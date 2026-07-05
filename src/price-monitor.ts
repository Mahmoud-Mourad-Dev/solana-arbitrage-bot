/**
 * price-monitor — Off-chain price monitor + cyclic arbitrage discovery for
 * Solana mainnet-beta.
 *
 * Pipeline:
 *   Yellowstone Geyser gRPC (processed commitment)
 *     -> binary parsers (Raydium v4 / Orca Whirlpool / SPL / OpenOrders)
 *     -> in-memory PoolRegistry (single-writer Map, BigInt state)
 *     -> RedisSink (pipelined HSET mirror of pool states)
 *     -> DiscoveryEngine (precomputed cycle index, dirty-pool evaluation)
 *     -> Redis LPUSH + PUBLISH on `arbitrage_opportunities`
 */
import { Connection, PublicKey } from "@solana/web3.js";
import bs58 from "bs58";
import { loadConfig, MonitorConfig, WatchedPoolConfig } from "./config";
import { DiscoveryEngine, PoolRegistry } from "./graph";
import { GeyserStreamManager } from "./geyser";
import { RedisSink } from "./redis-sink";
import { decodeRaydiumV4, RAYDIUM_V4_ACCOUNT_SIZE } from "./parsers/raydium";
import { decodeWhirlpool, WHIRLPOOL_ACCOUNT_SIZE } from "./parsers/whirlpool";
import {
  decodeMintDecimals,
  decodeOpenOrdersTotals,
  decodeTokenAccountAmount,
} from "./parsers/spl";
import type { PoolState, RaydiumPoolState, WhirlpoolState } from "./types";
import { formatUnits, Logger } from "./utils";

const STATS_INTERVAL_MS = 30_000;

async function getAccountsBatched(
  connection: Connection,
  addresses: string[],
): Promise<Map<string, Buffer>> {
  const out = new Map<string, Buffer>();
  for (let i = 0; i < addresses.length; i += 100) {
    const chunk = addresses.slice(i, i + 100);
    const infos = await connection.getMultipleAccountsInfo(
      chunk.map((a) => new PublicKey(a)),
      "confirmed",
    );
    infos.forEach((info, idx) => {
      if (info) out.set(chunk[idx]!, Buffer.from(info.data));
    });
  }
  return out;
}

/**
 * One-time registry hydration over plain RPC: resolves each configured pool
 * into its mints, vaults, fees and (for Raydium) OpenOrders, then seeds
 * initial balances so the engine can quote before the first Geyser packet.
 */
async function bootstrapRegistry(
  config: MonitorConfig,
  registry: PoolRegistry,
  log: Logger,
): Promise<void> {
  const connection = new Connection(config.rpcEndpoint, "confirmed");
  const poolAccounts = await getAccountsBatched(
    connection,
    config.pools.map((p) => p.address),
  );

  const pending: Array<{ cfg: WatchedPoolConfig; state: PoolState }> = [];
  for (const cfg of config.pools) {
    const data = poolAccounts.get(cfg.address);
    if (!data) {
      log.warn(`[bootstrap] pool account not found, skipping: ${cfg.address} (${cfg.label})`);
      continue;
    }
    if (cfg.dex === "raydium-v4") {
      if (data.length !== RAYDIUM_V4_ACCOUNT_SIZE) {
        log.warn(`[bootstrap] ${cfg.address} is not a Raydium v4 account, skipping`);
        continue;
      }
      const d = decodeRaydiumV4(data)!;
      const state: RaydiumPoolState = {
        address: cfg.address,
        dex: "raydium-v4",
        label: cfg.label,
        mintA: d.baseMint,
        mintB: d.quoteMint,
        vaultA: d.baseVault,
        vaultB: d.quoteVault,
        decimalsA: d.baseDecimal,
        decimalsB: d.quoteDecimal,
        lastSlot: 0n,
        lastUpdatedMs: 0,
        ready: false,
        vaultABalance: 0n,
        vaultBBalance: 0n,
        openOrders: d.openOrders,
        openOrdersBaseTotal: 0n,
        openOrdersQuoteTotal: 0n,
        baseNeedTakePnl: d.baseNeedTakePnl,
        quoteNeedTakePnl: d.quoteNeedTakePnl,
        swapFeeNumerator: d.swapFeeNumerator,
        swapFeeDenominator: d.swapFeeDenominator,
        status: d.status,
        poolOpenTime: d.poolOpenTime,
      };
      pending.push({ cfg, state });
    } else {
      if (data.length !== WHIRLPOOL_ACCOUNT_SIZE) {
        log.warn(`[bootstrap] ${cfg.address} is not a Whirlpool account, skipping`);
        continue;
      }
      const d = decodeWhirlpool(data)!;
      const state: WhirlpoolState = {
        address: cfg.address,
        dex: "orca-whirlpool",
        label: cfg.label,
        mintA: d.tokenMintA,
        mintB: d.tokenMintB,
        vaultA: d.tokenVaultA,
        vaultB: d.tokenVaultB,
        decimalsA: 0, // hydrated from mint accounts below
        decimalsB: 0,
        lastSlot: 0n,
        lastUpdatedMs: 0,
        ready: false,
        sqrtPriceX64: d.sqrtPriceX64,
        liquidity: d.liquidity,
        tickCurrentIndex: d.tickCurrentIndex,
        tickSpacing: d.tickSpacing,
        feeRatePpm: d.feeRatePpm,
        protocolFeeRate: d.protocolFeeRate,
      };
      pending.push({ cfg, state });
    }
  }

  // Second pass: vault balances, OpenOrders totals, mint decimals.
  const secondary = new Set<string>();
  for (const { state } of pending) {
    secondary.add(state.vaultA);
    secondary.add(state.vaultB);
    secondary.add(state.mintA);
    secondary.add(state.mintB);
    if (state.dex === "raydium-v4") secondary.add(state.openOrders);
  }
  const secondaryAccounts = await getAccountsBatched(connection, [...secondary]);

  for (const { cfg, state } of pending) {
    const mintAData = secondaryAccounts.get(state.mintA);
    const mintBData = secondaryAccounts.get(state.mintB);
    const decimalsA = mintAData ? decodeMintDecimals(mintAData) : null;
    const decimalsB = mintBData ? decodeMintDecimals(mintBData) : null;
    if (decimalsA === null || decimalsB === null) {
      log.warn(`[bootstrap] missing mint metadata for ${cfg.address}, skipping`);
      continue;
    }
    state.decimalsA = decimalsA;
    state.decimalsB = decimalsB;

    if (state.dex === "raydium-v4") {
      const va = secondaryAccounts.get(state.vaultA);
      const vb = secondaryAccounts.get(state.vaultB);
      const oo = secondaryAccounts.get(state.openOrders);
      const aBal = va ? decodeTokenAccountAmount(va) : null;
      const bBal = vb ? decodeTokenAccountAmount(vb) : null;
      if (aBal === null || bBal === null) {
        log.warn(`[bootstrap] missing vault balances for ${cfg.address}, skipping`);
        continue;
      }
      state.vaultABalance = aBal;
      state.vaultBBalance = bBal;
      const totals = oo ? decodeOpenOrdersTotals(oo) : null;
      if (totals) {
        state.openOrdersBaseTotal = totals.baseTokenTotal;
        state.openOrdersQuoteTotal = totals.quoteTokenTotal;
      }
    }

    state.ready = true;
    state.lastUpdatedMs = Date.now();
    registry.registerToken(state.mintA, state.decimalsA);
    registry.registerToken(state.mintB, state.decimalsB);
    registry.addPool(state);
    log.info(
      `[bootstrap] ${cfg.label ?? cfg.address} ready ` +
        `(${registry.tokens.get(state.mintA)?.symbol ?? state.mintA.slice(0, 4)}/` +
        `${registry.tokens.get(state.mintB)?.symbol ?? state.mintB.slice(0, 4)})`,
    );
  }

  if (registry.pools.size === 0) {
    throw new Error("bootstrap produced zero usable pools — check pools.json / RPC endpoint");
  }
}

async function main(): Promise<void> {
  const config = loadConfig();
  const log = new Logger(config.logLevel);
  log.info(`price-monitor starting: ${config.pools.length} configured pools`);

  const registry = new PoolRegistry();
  await bootstrapRegistry(config, registry, log);

  const sink = new RedisSink(config, log);
  const engine = new DiscoveryEngine(registry, config, log, (cycle) => {
    sink.publishOpportunity(cycle);
    const dec = registry.tokens.get(cycle.baseMint)?.decimals ?? 0;
    log.info(
      `OPPORTUNITY ${cycle.id} ${cycle.baseSymbol ?? cycle.baseMint.slice(0, 4)} ` +
        `${cycle.hops.length}-hop net=+${formatUnits(cycle.netProfit, dec)} ` +
        `(${cycle.netProfitBps}bps) in=${formatUnits(cycle.amountIn, dec)} slot=${cycle.slot}`,
    );
  });
  engine.buildCycleIndex();

  let updatesReceived = 0;
  const geyser = new GeyserStreamManager(
    config.geyserEndpoint,
    config.geyserXToken,
    log,
    (update) => {
      updatesReceived++;
      const pubkey = bs58.encode(update.pubkey);
      const pool = registry.applyAccountUpdate(pubkey, update.data, update.slot);
      if (pool) {
        sink.queuePoolState(pool);
        engine.markDirty(pool.address);
      }
    },
  );
  await geyser.start(registry.allWatchedAccounts());

  const statsTimer = setInterval(() => {
    const s = engine.stats;
    log.info(
      `stats: updates=${updatesReceived} searches=${s.searches} ` +
        `routesEvaluated=${s.routesEvaluated} opportunities=${s.opportunities} ` +
        `cooldownSuppressed=${s.suppressedByCooldown}`,
    );
  }, STATS_INTERVAL_MS);

  const shutdown = async (signal: string): Promise<void> => {
    log.info(`${signal} received, shutting down`);
    clearInterval(statsTimer);
    geyser.stop();
    await sink.close();
    process.exit(0);
  };
  process.on("SIGINT", () => void shutdown("SIGINT"));
  process.on("SIGTERM", () => void shutdown("SIGTERM"));
}

main().catch((err) => {
  // eslint-disable-next-line no-console
  console.error("fatal:", err);
  process.exit(1);
});
