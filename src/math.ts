import { ceilDiv } from "./utils";
import type { PoolState, RaydiumPoolState, WhirlpoolState } from "./types";

const Q64 = 1n << 64n;
const PPM = 1_000_000n;
const BPS = 10_000n;

/**
 * Raydium v4 effective reserves. The pool account itself holds no balances:
 *   reserve = vault balance + OpenOrders total − needTakePnl
 * Returns null when the state would be non-positive (mid-migration, drained
 * pool, or not yet hydrated) — callers must treat that pool as unquotable.
 */
export function raydiumEffectiveReserves(
  p: RaydiumPoolState,
): { base: bigint; quote: bigint } | null {
  const base = p.vaultABalance + p.openOrdersBaseTotal - p.baseNeedTakePnl;
  const quote = p.vaultBBalance + p.openOrdersQuoteTotal - p.quoteNeedTakePnl;
  if (base <= 0n || quote <= 0n) return null;
  return { base, quote };
}

/**
 * Constant product (x·y=k) exact-in quote with fee on input:
 *   out = (Δx·(1-f)·y) / (x + Δx·(1-f))
 * Integer math throughout; floor division matches on-chain behavior.
 */
export function getAmountOutCpmm(
  amountIn: bigint,
  reserveIn: bigint,
  reserveOut: bigint,
  feeNumerator: bigint,
  feeDenominator: bigint,
): bigint {
  if (amountIn <= 0n || reserveIn <= 0n || reserveOut <= 0n) return 0n;
  const amountInAfterFee = amountIn * (feeDenominator - feeNumerator);
  const numerator = amountInAfterFee * reserveOut;
  const denominator = reserveIn * feeDenominator + amountInAfterFee;
  if (denominator <= 0n) return 0n;
  return numerator / denominator;
}

export function quoteRaydium(p: RaydiumPoolState, inputMint: string, amountIn: bigint): bigint {
  const reserves = raydiumEffectiveReserves(p);
  if (!reserves) return 0n;
  // Canonical Raydium v4 swap fee is 25/10000; trust the on-chain fields,
  // fall back only if the account reports a zero denominator.
  const feeNum = p.swapFeeDenominator > 0n ? p.swapFeeNumerator : 25n;
  const feeDen = p.swapFeeDenominator > 0n ? p.swapFeeDenominator : 10_000n;
  if (inputMint === p.mintA) {
    return getAmountOutCpmm(amountIn, reserves.base, reserves.quote, feeNum, feeDen);
  }
  if (inputMint === p.mintB) {
    return getAmountOutCpmm(amountIn, reserves.quote, reserves.base, feeNum, feeDen);
  }
  return 0n;
}

/**
 * Whirlpool (CLMM) exact-in quote using the current tick's liquidity.
 * Q64.64 sqrt-price math, fee (ppm) charged on input:
 *
 *   A→B (price falls):  S1 = ceil( (L·2^64·S0) / (L·2^64 + ΔA·S0) )
 *                       out = floor( L·(S0−S1) / 2^64 )
 *   B→A (price rises):  S1 = S0 + floor( ΔB·2^64 / L )
 *                       out = floor( L·(S1−S0)·2^64 / (S0·S1) )
 *
 * Rounding always goes against the trade (conservative). If the implied
 * sqrt-price move exceeds `maxImpactBps` the quote is rejected (0n): the
 * single-tick approximation is invalid once ticks are crossed, and the
 * discovery layer must never overestimate output.
 */
export function quoteWhirlpool(
  p: WhirlpoolState,
  inputMint: string,
  amountIn: bigint,
  maxImpactBps: number,
): bigint {
  if (amountIn <= 0n || p.liquidity <= 0n || p.sqrtPriceX64 <= 0n) return 0n;
  const amountInAfterFee = (amountIn * (PPM - p.feeRatePpm)) / PPM;
  if (amountInAfterFee <= 0n) return 0n;

  const L = p.liquidity;
  const S0 = p.sqrtPriceX64;
  let S1: bigint;
  let amountOut: bigint;

  if (inputMint === p.mintA) {
    const denom = (L << 64n) + amountInAfterFee * S0;
    S1 = ceilDiv((L << 64n) * S0, denom);
    if (S1 >= S0) return 0n;
    amountOut = (L * (S0 - S1)) >> 64n;
  } else if (inputMint === p.mintB) {
    S1 = S0 + (amountInAfterFee << 64n) / L;
    if (S1 <= S0) return 0n;
    amountOut = (L * ((S1 - S0) << 64n)) / (S0 * S1);
  } else {
    return 0n;
  }

  const impactBps = ((S1 > S0 ? S1 - S0 : S0 - S1) * BPS) / S0;
  if (impactBps > BigInt(maxImpactBps)) return 0n;
  return amountOut;
}

/** Dispatch quote by pool kind. Returns 0n whenever the pool cannot fill. */
export function quotePool(
  pool: PoolState,
  inputMint: string,
  amountIn: bigint,
  maxClmmImpactBps: number,
): bigint {
  if (!pool.ready) return 0n;
  if (pool.dex === "raydium-v4") return quoteRaydium(pool, inputMint, amountIn);
  return quoteWhirlpool(pool, inputMint, amountIn, maxClmmImpactBps);
}

/**
 * Ternary search for the input amount maximizing profit on [min, max].
 * Profit along a CPMM/CLMM chain is unimodal in the input, so ternary
 * search converges; every probed point is tracked so a flat/clipped region
 * (e.g. CLMM impact guard returning 0) can never make us return a worse
 * point than one we already saw.
 */
export function optimizeInputAmount(
  profitAt: (amountIn: bigint) => bigint,
  min: bigint,
  max: bigint,
  maxIterations = 48,
): { amountIn: bigint; profit: bigint } {
  let lo = min;
  let hi = max;
  let bestAmount = 0n;
  let bestProfit = -(1n << 255n);

  const probe = (x: bigint): bigint => {
    const p = profitAt(x);
    if (p > bestProfit) {
      bestProfit = p;
      bestAmount = x;
    }
    return p;
  };

  probe(lo);
  probe(hi);
  for (let i = 0; i < maxIterations && hi - lo > 1n; i++) {
    const third = (hi - lo) / 3n;
    const m1 = lo + third;
    const m2 = hi - third;
    if (probe(m1) < probe(m2)) {
      lo = m1 + 1n;
    } else {
      hi = m2 - 1n;
    }
  }
  return { amountIn: bestAmount, profit: bestProfit };
}
