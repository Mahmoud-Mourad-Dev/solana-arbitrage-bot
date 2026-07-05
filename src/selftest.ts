/**
 * Offline self-test: exercises the swap math, the optimizer and the
 * discovery engine end-to-end with synthetic pool states. No network, no
 * Redis. Exits non-zero on any failure — wire into CI.
 */
import assert from "assert";
import {
  getAmountOutCpmm,
  optimizeInputAmount,
  quoteWhirlpool,
  raydiumEffectiveReserves,
} from "./math";
import { DiscoveryEngine, PoolRegistry } from "./graph";
import { loadConfig, USDC_MINT, WSOL_MINT } from "./config";
import type { ArbitrageCycle, RaydiumPoolState, WhirlpoolState } from "./types";
import { Logger } from "./utils";

const Q64 = 1n << 64n;

function mkRaydium(
  address: string,
  mintA: string,
  mintB: string,
  reserveA: bigint,
  reserveB: bigint,
): RaydiumPoolState {
  return {
    address,
    dex: "raydium-v4",
    mintA,
    mintB,
    vaultA: `${address}-va`,
    vaultB: `${address}-vb`,
    decimalsA: 9,
    decimalsB: 6,
    lastSlot: 1n,
    lastUpdatedMs: Date.now(),
    ready: true,
    vaultABalance: reserveA,
    vaultBBalance: reserveB,
    openOrders: `${address}-oo`,
    openOrdersBaseTotal: 0n,
    openOrdersQuoteTotal: 0n,
    baseNeedTakePnl: 0n,
    quoteNeedTakePnl: 0n,
    swapFeeNumerator: 25n,
    swapFeeDenominator: 10_000n,
    status: 6n,
    poolOpenTime: 0n,
  };
}

function mkWhirlpool(
  address: string,
  mintA: string,
  mintB: string,
  sqrtPriceX64: bigint,
  liquidity: bigint,
): WhirlpoolState {
  return {
    address,
    dex: "orca-whirlpool",
    mintA,
    mintB,
    vaultA: `${address}-va`,
    vaultB: `${address}-vb`,
    decimalsA: 9,
    decimalsB: 6,
    lastSlot: 1n,
    lastUpdatedMs: Date.now(),
    ready: true,
    sqrtPriceX64,
    liquidity,
    tickCurrentIndex: 0,
    tickSpacing: 64,
    feeRatePpm: 3000n,
    protocolFeeRate: 300,
  };
}

// ── CPMM math ────────────────────────────────────────────────────────────────
{
  // 1000 SOL / 150_000 USDC pool, swap 1 SOL at 25 bps fee.
  const x = 1_000n * 10n ** 9n;
  const y = 150_000n * 10n ** 6n;
  const out = getAmountOutCpmm(10n ** 9n, x, y, 25n, 10_000n);
  // Ideal mid-price is 150 USDC; expect slightly less (fee + impact).
  assert(out > 149_000_000n && out < 150_000_000n, `CPMM out of range: ${out}`);

  // Zero-fee invariant: k never decreases.
  const outNoFee = getAmountOutCpmm(10n ** 9n, x, y, 0n, 10_000n);
  assert((x + 10n ** 9n) * (y - outNoFee) >= x * y, "CPMM violated x*y=k");

  // Monotonicity in input.
  assert(
    getAmountOutCpmm(2n * 10n ** 9n, x, y, 25n, 10_000n) > out,
    "CPMM not monotonic",
  );
  assert.equal(getAmountOutCpmm(0n, x, y, 25n, 10_000n), 0n);
}

// ── Raydium effective reserves ──────────────────────────────────────────────
{
  const p = mkRaydium("P", WSOL_MINT, USDC_MINT, 100n, 200n);
  p.openOrdersBaseTotal = 10n;
  p.baseNeedTakePnl = 5n;
  const r = raydiumEffectiveReserves(p)!;
  assert.equal(r.base, 105n);
  assert.equal(r.quote, 200n);
  p.baseNeedTakePnl = 1_000n; // drained -> unquotable
  assert.equal(raydiumEffectiveReserves(p), null);
}

// ── Whirlpool Q64.64 math ────────────────────────────────────────────────────
{
  // Price 1.0 (sqrtPrice = 2^64), deep liquidity.
  const wp = mkWhirlpool("W", WSOL_MINT, USDC_MINT, Q64, 10n ** 15n);
  const amountIn = 10n ** 9n;

  const outAB = quoteWhirlpool(wp, WSOL_MINT, amountIn, 10_000);
  const outBA = quoteWhirlpool(wp, USDC_MINT, amountIn, 10_000);
  // At price 1 with 30 bps fee, out ≈ in * 0.997 minus tiny impact.
  const expected = (amountIn * 997n) / 1000n;
  for (const out of [outAB, outBA]) {
    assert(out > 0n && out <= expected, `whirlpool out of range: ${out}`);
    assert(expected - out < expected / 1000n, `whirlpool impact too large: ${out}`);
  }

  // Impact guard: a trade ~10% of virtual depth must be rejected at 100 bps cap.
  const huge = quoteWhirlpool(wp, WSOL_MINT, 10n ** 14n, 100);
  assert.equal(huge, 0n, "CLMM impact guard failed to reject");

  // Empty liquidity is unquotable.
  const dead = mkWhirlpool("W2", WSOL_MINT, USDC_MINT, Q64, 0n);
  assert.equal(quoteWhirlpool(dead, WSOL_MINT, amountIn, 10_000), 0n);
}

// ── Optimizer ────────────────────────────────────────────────────────────────
{
  // profit(x) peaks at x = 6000 on a synthetic concave curve.
  const peak = 6_000n;
  const { amountIn, profit } = optimizeInputAmount(
    (x) => -((x - peak) * (x - peak)) / 1000n + 500n,
    1n,
    1_000_000n,
  );
  assert(profit > 490n, `optimizer missed peak: profit=${profit}`);
  assert(
    amountIn > peak - 200n && amountIn < peak + 200n,
    `optimizer far from peak: ${amountIn}`,
  );
}

// ── Discovery engine end-to-end ──────────────────────────────────────────────
{
  process.env.GEYSER_ENDPOINT = process.env.GEYSER_ENDPOINT || "http://selftest.invalid";
  const config = loadConfig(false);
  config.baseMints = [WSOL_MINT];
  config.minProfitBps = 5;

  const registry = new PoolRegistry();
  registry.registerToken(WSOL_MINT, 9);
  registry.registerToken(USDC_MINT, 6);

  // Two SOL/USDC venues with a deliberate ~2% price discrepancy:
  // Raydium prices SOL at 150 USDC, Whirlpool at ~153 USDC.
  const ray = mkRaydium("RAY_POOL", WSOL_MINT, USDC_MINT, 5_000n * 10n ** 9n, 750_000n * 10n ** 6n);
  // sqrtPrice for price(B/A in raw units) = 153e6/1e9 = 0.153: sqrt(0.153) * 2^64
  const sqrtP = 7_216_072_408_257_405_000n; // ≈ sqrt(0.153) * 2^64
  const orca = mkWhirlpool("ORCA_POOL", WSOL_MINT, USDC_MINT, sqrtP, 10n ** 16n);
  registry.addPool(ray);
  registry.addPool(orca);

  const found: ArbitrageCycle[] = [];
  const engine = new DiscoveryEngine(registry, config, new Logger("error"), (c) => found.push(c));
  engine.buildCycleIndex();

  engine.markDirty("RAY_POOL");
  engine.markDirty("ORCA_POOL");

  // Discovery runs on setImmediate — drain the event loop, then assert.
  setImmediate(() =>
    setImmediate(() => {
      assert(found.length > 0, "engine found no cycle in a 2% discrepancy");
      const best = found.reduce((a, b) => (a.netProfit > b.netProfit ? a : b));
      assert.equal(best.baseMint, WSOL_MINT);
      assert.equal(best.hops.length, 2);
      assert(best.netProfit > 0n, "published cycle has non-positive net profit");
      assert(
        best.hops[0]!.outputMint === USDC_MINT && best.hops[1]!.outputMint === WSOL_MINT,
        "route mints malformed",
      );
      assert(
        best.hops[1]!.amountIn === best.hops[0]!.expectedAmountOut,
        "hop chaining broken",
      );
      assert(
        best.hops[0]!.minAmountOut < best.hops[0]!.expectedAmountOut,
        "slippage floor missing",
      );
      // The profitable direction must buy SOL cheap on Raydium (150) and
      // sell dear on Orca (153): first hop through RAY_POOL is SOL->USDC? No —
      // buy USDC where SOL is expensive (Orca), buy SOL back where cheap (Raydium).
      assert.equal(best.hops[0]!.pool, "ORCA_POOL", "picked the losing direction");
      // eslint-disable-next-line no-console
      console.log(
        `selftest OK — best cycle net=+${best.netProfit} lamports ` +
          `(${best.netProfitBps} bps) on ${best.amountIn} in, routes=${found.length}`,
      );
    }),
  );
}
