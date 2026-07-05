import type {
  ArbitrageCycle,
  CycleHop,
  CycleRoute,
  PoolState,
  TokenNode,
} from "./types";
import type { MonitorConfig } from "./config";
import { WSOL_MINT, KNOWN_SYMBOLS } from "./config";
import { decodeRaydiumV4, raydiumSwapEnabled } from "./parsers/raydium";
import { decodeWhirlpool } from "./parsers/whirlpool";
import { decodeOpenOrdersTotals, decodeTokenAccountAmount } from "./parsers/spl";
import { optimizeInputAmount, quotePool } from "./math";
import { Logger, shortHash } from "./utils";

/**
 * In-memory registry of every account we watch, plus the token graph.
 * Single-writer (the Geyser stream handler); reads are synchronous.
 */
export class PoolRegistry {
  readonly pools = new Map<string, PoolState>();
  readonly tokens = new Map<string, TokenNode>();
  /** vault token-account address -> owning pool + side. */
  readonly vaultToPool = new Map<string, { pool: string; side: "A" | "B" }>();
  /** Raydium OpenOrders address -> owning pool. */
  readonly openOrdersToPool = new Map<string, string>();
  /** mint -> pool addresses touching it (graph adjacency). */
  readonly adjacency = new Map<string, string[]>();
  /** Per-account last applied slot, to drop out-of-order Geyser packets. */
  private readonly accountSlots = new Map<string, bigint>();

  registerToken(mint: string, decimals: number): void {
    if (!this.tokens.has(mint)) {
      this.tokens.set(mint, { mint, decimals, symbol: KNOWN_SYMBOLS[mint] });
    }
  }

  addPool(state: PoolState): void {
    this.pools.set(state.address, state);
    this.vaultToPool.set(state.vaultA, { pool: state.address, side: "A" });
    this.vaultToPool.set(state.vaultB, { pool: state.address, side: "B" });
    if (state.dex === "raydium-v4") {
      this.openOrdersToPool.set(state.openOrders, state.address);
    }
    for (const mint of [state.mintA, state.mintB]) {
      const list = this.adjacency.get(mint);
      if (list) {
        if (!list.includes(state.address)) list.push(state.address);
      } else {
        this.adjacency.set(mint, [state.address]);
      }
    }
  }

  /** Every account address the Geyser subscription must include. */
  allWatchedAccounts(): string[] {
    const set = new Set<string>();
    for (const p of this.pools.values()) {
      set.add(p.address);
      set.add(p.vaultA);
      set.add(p.vaultB);
      if (p.dex === "raydium-v4") set.add(p.openOrders);
    }
    return [...set];
  }

  otherMint(pool: PoolState, mint: string): string {
    return pool.mintA === mint ? pool.mintB : pool.mintA;
  }

  /** Freshest ready pool connecting two mints (for gas-cost conversion). */
  findReferencePool(mintX: string, mintY: string): PoolState | null {
    let best: PoolState | null = null;
    for (const addr of this.adjacency.get(mintX) ?? []) {
      const p = this.pools.get(addr);
      if (!p || !p.ready) continue;
      const pair = p.mintA === mintX ? p.mintB : p.mintA;
      if (pair !== mintY) continue;
      if (!best || p.lastSlot > best.lastSlot) best = p;
    }
    return best;
  }

  private acceptSlot(pubkey: string, slot: bigint): boolean {
    const prev = this.accountSlots.get(pubkey);
    if (prev !== undefined && slot < prev) return false;
    this.accountSlots.set(pubkey, slot);
    return true;
  }

  /**
   * Route a raw Geyser account update into the registry.
   * Returns the affected pool when its quotable state changed.
   */
  applyAccountUpdate(pubkey: string, data: Buffer, slot: bigint): PoolState | null {
    if (!this.acceptSlot(pubkey, slot)) return null;

    const pool = this.pools.get(pubkey);
    if (pool) {
      return this.applyPoolAccount(pool, data, slot);
    }

    const vaultRef = this.vaultToPool.get(pubkey);
    if (vaultRef) {
      const p = this.pools.get(vaultRef.pool);
      if (!p) return null;
      const amount = decodeTokenAccountAmount(data);
      if (amount === null) return null;
      if (p.dex === "raydium-v4") {
        if (vaultRef.side === "A") p.vaultABalance = amount;
        else p.vaultBBalance = amount;
      }
      // Whirlpool vault balances are not needed for quoting (sqrtPrice +
      // liquidity carry the price); the update still bumps freshness.
      this.touch(p, slot);
      return p;
    }

    const ooPool = this.openOrdersToPool.get(pubkey);
    if (ooPool) {
      const p = this.pools.get(ooPool);
      if (!p || p.dex !== "raydium-v4") return null;
      const totals = decodeOpenOrdersTotals(data);
      if (!totals) return null;
      p.openOrdersBaseTotal = totals.baseTokenTotal;
      p.openOrdersQuoteTotal = totals.quoteTokenTotal;
      this.touch(p, slot);
      return p;
    }

    return null;
  }

  private applyPoolAccount(pool: PoolState, data: Buffer, slot: bigint): PoolState | null {
    if (pool.dex === "raydium-v4") {
      const d = decodeRaydiumV4(data);
      if (!d) return null;
      pool.baseNeedTakePnl = d.baseNeedTakePnl;
      pool.quoteNeedTakePnl = d.quoteNeedTakePnl;
      pool.swapFeeNumerator = d.swapFeeNumerator;
      pool.swapFeeDenominator = d.swapFeeDenominator;
      pool.status = d.status;
      pool.poolOpenTime = d.poolOpenTime;
      this.touch(pool, slot);
      return pool;
    }
    const d = decodeWhirlpool(data);
    if (!d) return null;
    pool.sqrtPriceX64 = d.sqrtPriceX64;
    pool.liquidity = d.liquidity;
    pool.tickCurrentIndex = d.tickCurrentIndex;
    pool.feeRatePpm = d.feeRatePpm;
    this.touch(pool, slot);
    return pool;
  }

  private touch(pool: PoolState, slot: bigint): void {
    if (slot > pool.lastSlot) pool.lastSlot = slot;
    pool.lastUpdatedMs = Date.now();
  }
}

export interface EngineStats {
  searches: number;
  routesEvaluated: number;
  opportunities: number;
  suppressedByCooldown: number;
}

/**
 * Cycle discovery engine.
 *
 * All routes up to maxHops are enumerated ONCE at startup (the pool set is
 * static for a run) and indexed by pool address. A Geyser update therefore
 * costs: dirty-set insert + O(routes touching that pool) simulations — no
 * graph traversal ever happens on the hot path. Evaluation is coalesced
 * onto setImmediate and chunked under a time budget so the event loop is
 * never blocked while packets keep arriving.
 */
export class DiscoveryEngine {
  private readonly routes: CycleRoute[] = [];
  private readonly routesByPool = new Map<string, CycleRoute[]>();
  private readonly dirty = new Set<string>();
  private scheduled = false;
  /** cycleKey -> last publish bookkeeping for cooldown throttling. */
  private readonly lastPublished = new Map<string, { atMs: number; profit: bigint }>();
  readonly stats: EngineStats = {
    searches: 0,
    routesEvaluated: 0,
    opportunities: 0,
    suppressedByCooldown: 0,
  };

  private static readonly TIME_BUDGET_MS = 4;

  constructor(
    private readonly registry: PoolRegistry,
    private readonly config: MonitorConfig,
    private readonly log: Logger,
    private readonly onOpportunity: (cycle: ArbitrageCycle) => void,
  ) {}

  /** Enumerate every base-anchored cycle route of length 2..maxHops. */
  buildCycleIndex(): void {
    const seen = new Set<string>();
    for (const base of this.config.baseMints) {
      if (!this.registry.adjacency.has(base)) continue;
      this.dfs(base, base, [], [], seen);
    }
    for (const route of this.routes) {
      for (const pool of route.pools) {
        const list = this.routesByPool.get(pool);
        if (list) list.push(route);
        else this.routesByPool.set(pool, [route]);
      }
    }
    this.log.info(
      `cycle index built: ${this.routes.length} routes across ${this.registry.pools.size} pools (maxHops=${this.config.maxHops})`,
    );
  }

  private dfs(
    base: string,
    currentMint: string,
    poolPath: string[],
    mintPath: string[],
    seen: Set<string>,
  ): void {
    for (const poolAddr of this.registry.adjacency.get(currentMint) ?? []) {
      if (poolPath.includes(poolAddr)) continue; // never reuse a pool
      const pool = this.registry.pools.get(poolAddr);
      if (!pool) continue;
      const next = this.registry.otherMint(pool, currentMint);

      if (next === base) {
        if (poolPath.length >= 1) {
          // poolPath + this pool closes a cycle of length >= 2
          const pools = [...poolPath, poolAddr];
          const key = pools.join(">");
          if (!seen.has(key)) {
            seen.add(key);
            // hop h swaps inputMints[h] -> next mint; mintPath already lists
            // every intermediate mint, so [base, ...mintPath] lines up 1:1.
            this.routes.push({ key, baseMint: base, pools, inputMints: [base, ...mintPath] });
          }
        }
        continue;
      }

      if (poolPath.length + 1 >= this.config.maxHops) continue; // would exceed depth
      if (mintPath.includes(next)) continue; // no revisiting intermediates
      this.dfs(base, next, [...poolPath, poolAddr], [...mintPath, next], seen);
    }
  }

  /** Hot-path entry: called for every relevant Geyser account update. */
  markDirty(poolAddress: string): void {
    if (!this.routesByPool.has(poolAddress)) return;
    this.dirty.add(poolAddress);
    if (!this.scheduled) {
      this.scheduled = true;
      setImmediate(() => this.runSearch());
    }
  }

  private runSearch(): void {
    this.scheduled = false;
    if (this.dirty.size === 0) return;
    this.stats.searches++;

    // Union of routes touching any dirty pool, deduped by route key.
    const candidates = new Map<string, CycleRoute>();
    for (const pool of this.dirty) {
      for (const route of this.routesByPool.get(pool) ?? []) {
        candidates.set(route.key, route);
      }
    }
    this.dirty.clear();
    this.evaluateChunk([...candidates.values()], 0);
  }

  /** Evaluate routes in slices bounded by TIME_BUDGET_MS, yielding between. */
  private evaluateChunk(routes: CycleRoute[], start: number): void {
    const t0 = performance.now();
    let i = start;
    for (; i < routes.length; i++) {
      const route = routes[i]!;
      this.stats.routesEvaluated++;
      const cycle = this.evaluateRoute(route);
      if (cycle) this.publish(route, cycle);
      if (performance.now() - t0 > DiscoveryEngine.TIME_BUDGET_MS && i + 1 < routes.length) {
        setImmediate(() => this.evaluateChunk(routes, i + 1));
        return;
      }
    }
  }

  private evaluateRoute(route: CycleRoute): ArbitrageCycle | null {
    const pools: PoolState[] = [];
    const nowSec = BigInt(Math.floor(Date.now() / 1000));
    let maxSlot = 0n;
    for (const addr of route.pools) {
      const p = this.registry.pools.get(addr);
      if (!p || !p.ready) return null;
      if (p.dex === "raydium-v4" && !raydiumSwapEnabled(p.status, p.poolOpenTime, nowSec)) {
        return null;
      }
      if (p.lastSlot > maxSlot) maxSlot = p.lastSlot;
      pools.push(p);
    }

    const bounds = this.config.tradeBounds.get(route.baseMint);
    if (!bounds) return null;

    const simulate = (amountIn: bigint): bigint => {
      let amount = amountIn;
      for (let h = 0; h < pools.length; h++) {
        amount = quotePool(pools[h]!, route.inputMints[h]!, amount, this.config.maxClmmImpactBps);
        if (amount <= 0n) return 0n;
      }
      return amount;
    };

    const { amountIn, profit: grossProfit } = optimizeInputAmount(
      (x) => simulate(x) - x,
      bounds.min,
      bounds.max,
    );
    if (grossProfit <= 0n || amountIn <= 0n) return null;

    const cost = this.executionCostInBase(route.baseMint);
    if (cost === null) return null; // cannot price gas in this base — skip
    const netProfit = grossProfit - cost;
    if (netProfit <= 0n) return null;
    const netProfitBps = Number((netProfit * 10_000n) / amountIn);
    if (netProfitBps < this.config.minProfitBps) return null;

    // Re-walk the winning amount to capture exact per-hop in/out legs.
    const hops: CycleHop[] = [];
    let amount = amountIn;
    for (let h = 0; h < pools.length; h++) {
      const pool = pools[h]!;
      const inputMint = route.inputMints[h]!;
      const out = quotePool(pool, inputMint, amount, this.config.maxClmmImpactBps);
      if (out <= 0n) return null;
      hops.push({
        pool: pool.address,
        dex: pool.dex,
        inputMint,
        outputMint: this.registry.otherMint(pool, inputMint),
        amountIn: amount,
        expectedAmountOut: out,
        minAmountOut: (out * (10_000n - BigInt(this.config.slippageBps))) / 10_000n,
      });
      amount = out;
    }

    return {
      id: shortHash(route.key),
      baseMint: route.baseMint,
      baseSymbol: this.registry.tokens.get(route.baseMint)?.symbol,
      hops,
      amountIn,
      expectedAmountOut: amount,
      grossProfit: amount - amountIn,
      estimatedCostInBase: cost,
      netProfit: amount - amountIn - cost,
      netProfitBps,
      slot: maxSlot,
      discoveredAtMs: Date.now(),
    };
  }

  /**
   * Total execution cost (signature + priority fee + Jito tip) expressed in
   * the cycle's base mint. For non-SOL bases the lamport cost is priced
   * through the freshest WSOL/base pool; without one we return null and the
   * cycle is skipped rather than published with an unpriced cost.
   */
  private executionCostInBase(baseMint: string): bigint | null {
    const lamports =
      this.config.baseSignatureFeeLamports +
      this.config.priorityFeeLamports +
      this.config.jitoTipLamports;
    if (baseMint === WSOL_MINT) return lamports;
    const ref = this.registry.findReferencePool(WSOL_MINT, baseMint);
    if (!ref) return null;
    const converted = quotePool(ref, WSOL_MINT, lamports, this.config.maxClmmImpactBps);
    return converted > 0n ? converted : null;
  }

  private publish(route: CycleRoute, cycle: ArbitrageCycle): void {
    const prev = this.lastPublished.get(route.key);
    const now = cycle.discoveredAtMs;
    if (prev && now - prev.atMs < this.config.opportunityCooldownMs) {
      // Within cooldown: only let through a materially better quote (>5%).
      if (cycle.netProfit <= (prev.profit * 105n) / 100n) {
        this.stats.suppressedByCooldown++;
        return;
      }
    }
    this.lastPublished.set(route.key, { atMs: now, profit: cycle.netProfit });
    this.stats.opportunities++;
    this.onOpportunity(cycle);
  }
}
