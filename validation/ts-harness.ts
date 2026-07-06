/**
 * TS side of the differential-parity harness. Loads validation/scenarios.json,
 * constructs pool states, runs the SAME DiscoveryEngine the production TS
 * monitor uses, and writes normalized opportunities to validation/out-ts.json.
 *
 * Run: npx tsx validation/ts-harness.ts
 */
import { readFileSync, writeFileSync } from "fs";
import { resolve } from "path";
import { PoolRegistry, DiscoveryEngine } from "../src/graph";
import type { MonitorConfig } from "../src/config";
import type { ArbitrageCycle, PoolState } from "../src/types";

interface Scenario {
  name: string;
  dirty: string[];
  pools: any[];
}
interface File {
  mints: Record<string, string>;
  addresses: Record<string, string>;
  config: any;
  scenarios: Scenario[];
}

const file: File = JSON.parse(
  readFileSync(resolve(__dirname, "scenarios.json"), "utf8"),
);
const sym = (m: Record<string, string>, k: string): string => m[k] ?? k;

function buildConfig(f: File): MonitorConfig {
  const c = f.config;
  const tradeBounds = new Map<string, { min: bigint; max: bigint }>();
  for (const [mintSym, b] of Object.entries<any>(c.tradeBounds)) {
    tradeBounds.set(sym(f.mints, mintSym), { min: BigInt(b.min), max: BigInt(b.max) });
  }
  return {
    geyserEndpoint: "",
    geyserXToken: undefined,
    rpcEndpoint: "",
    redisUrl: "",
    opportunityChannel: "",
    opportunityList: "",
    opportunityListMax: 1000,
    redisFlushIntervalMs: 20,
    redisFlushMaxCommands: 256,
    baseMints: c.baseMints.map((m: string) => sym(f.mints, m)),
    maxHops: c.maxHops,
    minProfitBps: c.minProfitBps,
    slippageBps: c.slippageBps,
    maxClmmImpactBps: c.maxClmmImpactBps,
    tradeBounds,
    baseSignatureFeeLamports: BigInt(c.baseSignatureFeeLamports),
    priorityFeeLamports: BigInt(c.priorityFeeLamports),
    jitoTipLamports: BigInt(c.jitoTipLamports),
    opportunityCooldownMs: c.opportunityCooldownMs,
    logLevel: "error",
    pools: [],
  } as MonitorConfig;
}

function buildPool(f: File, p: any): PoolState {
  const addr = sym(f.addresses, p.address);
  const mintA = sym(f.mints, p.mintA);
  const mintB = sym(f.mints, p.mintB);
  const base = {
    address: addr,
    label: p.address,
    mintA,
    mintB,
    vaultA: sym(f.addresses, p.vaultA),
    vaultB: sym(f.addresses, p.vaultB),
    decimalsA: p.decimalsA,
    decimalsB: p.decimalsB,
    lastSlot: BigInt(p.slot),
    lastUpdatedMs: 1,
    ready: true,
  };
  if (p.dex === "raydium-v4") {
    return {
      ...base,
      dex: "raydium-v4",
      vaultABalance: BigInt(p.reserveBase),
      vaultBBalance: BigInt(p.reserveQuote),
      openOrders: sym(f.addresses, p.openOrders),
      openOrdersBaseTotal: 0n,
      openOrdersQuoteTotal: 0n,
      baseNeedTakePnl: 0n,
      quoteNeedTakePnl: 0n,
      swapFeeNumerator: BigInt(p.feeNum),
      swapFeeDenominator: BigInt(p.feeDen),
      status: BigInt(p.status),
      poolOpenTime: BigInt(p.poolOpenTime),
    };
  }
  return {
    ...base,
    dex: "orca-whirlpool",
    sqrtPriceX64: BigInt(p.sqrtPriceX64),
    liquidity: BigInt(p.liquidity),
    tickCurrentIndex: 0,
    tickSpacing: 64,
    feeRatePpm: BigInt(p.feeRatePpm),
    protocolFeeRate: 300,
  };
}

/** Normalized, comparison-ready cycle (discoveredAtMs excluded). */
function normalize(c: ArbitrageCycle): unknown {
  return {
    id: c.id,
    baseMint: c.baseMint,
    baseSymbol: c.baseSymbol ?? null,
    amountIn: c.amountIn.toString(),
    expectedAmountOut: c.expectedAmountOut.toString(),
    grossProfit: c.grossProfit.toString(),
    estimatedCostInBase: c.estimatedCostInBase.toString(),
    netProfit: c.netProfit.toString(),
    netProfitBps: c.netProfitBps,
    slot: c.slot.toString(),
    hops: c.hops.map((h) => ({
      pool: h.pool,
      dex: h.dex,
      inputMint: h.inputMint,
      outputMint: h.outputMint,
      amountIn: h.amountIn.toString(),
      expectedAmountOut: h.expectedAmountOut.toString(),
      minAmountOut: h.minAmountOut.toString(),
    })),
  };
}

async function runScenario(f: File, s: Scenario): Promise<unknown[]> {
  const config = buildConfig(f);
  const registry = new PoolRegistry();
  for (const p of s.pools) {
    const state = buildPool(f, p);
    registry.registerToken(state.mintA, state.decimalsA);
    registry.registerToken(state.mintB, state.decimalsB);
    registry.addPool(state);
  }
  const found: ArbitrageCycle[] = [];
  const engine = new DiscoveryEngine(registry, config, { debug() {}, info() {}, warn() {}, error() {} } as any, (c) => found.push(c));
  engine.buildCycleIndex();
  for (const d of s.dirty) engine.markDirty(sym(f.addresses, d));
  // Drain setImmediate chunking.
  await new Promise((r) => setImmediate(() => setImmediate(() => setImmediate(r))));
  return found.map(normalize).sort((a: any, b: any) => a.id.localeCompare(b.id));
}

async function main(): Promise<void> {
  const out: Record<string, unknown[]> = {};
  for (const s of file.scenarios) {
    out[s.name] = await runScenario(file, s);
  }
  writeFileSync(resolve(__dirname, "out-ts.json"), JSON.stringify(out, null, 2) + "\n");
  const total = Object.values(out).reduce((n, a) => n + a.length, 0);
  console.log(`ts-harness: ${file.scenarios.length} scenarios, ${total} opportunities -> out-ts.json`);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
