//! Quote math — 1:1 port of the TypeScript monitor's `src/math.ts`.
//!
//! TS BigInt is arbitrary precision; Rust integers are not. Every formula
//! here is annotated with its worst-case bit width and computed in a type
//! that provably cannot overflow:
//!
//! - CPMM: `amount_in * (feeDen-feeNum) * reserve_out` ≤ 2^64·2^64·2^64 →
//!   U256 (192 bits worst case).
//! - Whirlpool A→B: `(L << 64) * S0` ≤ 2^128·2^64·2^128 → up to 2^320 →
//!   U512.
//! - All rounding goes AGAINST the trade (conservative), matching TS.
//!
//! TODO(whirlpool-exact): this is the same single-tick approximation as
//! the TS engine — quotes are clamped by `max_impact_bps` and NEVER
//! overestimate output, but they underestimate large trades that cross
//! ticks. Replace with exact tick-array walking (fetch + traverse
//! initialized ticks) in a later phase; keep the conservative clamp until
//! then.

use ruint::aliases::{U256, U512};

pub const PPM: u64 = 1_000_000;
pub const BPS: u64 = 10_000;

/// Constant product (x·y=k) exact-in quote with fee on input:
/// `out = (Δx·(1-f)·y) / (x + Δx·(1-f))`, floor division (on-chain match).
pub fn cpmm_amount_out(
    amount_in: u64,
    reserve_in: u64,
    reserve_out: u64,
    fee_numerator: u64,
    fee_denominator: u64,
) -> u64 {
    if amount_in == 0
        || reserve_in == 0
        || reserve_out == 0
        || fee_denominator == 0
        || fee_numerator >= fee_denominator
    {
        return 0;
    }
    // ≤ 2^64 * 2^64 = 2^128 — fits U256 with headroom for the next mul.
    let after_fee = U256::from(amount_in) * U256::from(fee_denominator - fee_numerator);
    let numerator = after_fee * U256::from(reserve_out); // ≤ 2^192
    let denominator = U256::from(reserve_in) * U256::from(fee_denominator) + after_fee;
    // out < reserve_out ≤ u64::MAX, so the cast is total.
    u64::try_from(numerator / denominator).unwrap_or(0)
}

/// Raydium v4 effective reserve for one side:
/// `vault + openOrders total − needTakePnl`. None ⇒ pool unquotable
/// (mid-migration / drained) — mirrors the TS `null` contract.
pub fn raydium_effective_reserve(
    vault_balance: u64,
    open_orders_total: u64,
    need_take_pnl: u64,
) -> Option<u64> {
    let gross = vault_balance as u128 + open_orders_total as u128; // ≤ 2^65
    let net = gross.checked_sub(need_take_pnl as u128)?;
    if net == 0 {
        return None;
    }
    u64::try_from(net).ok()
}

#[derive(Debug, Clone, Copy)]
pub struct WhirlpoolQuoteState {
    /// Q64.64 sqrt price.
    pub sqrt_price_x64: u128,
    /// Active in-range liquidity.
    pub liquidity: u128,
    /// Fee rate in parts-per-million (e.g. 3000 = 30 bps).
    pub fee_rate_ppm: u64,
}

/// Whirlpool (CLMM) exact-in quote within the current tick's liquidity.
///
/// A→B (price falls):  `S1 = ceil((L·2^64·S0) / (L·2^64 + ΔA·S0))`,
///                     `out = floor(L·(S0−S1) / 2^64)`
/// B→A (price rises):  `S1 = S0 + floor(ΔB·2^64 / L)`,
///                     `out = floor(L·(S1−S0)·2^64 / (S0·S1))`
///
/// Returns 0 when the implied sqrt-price move exceeds `max_impact_bps`
/// (tick-crossing risk — see module TODO) or the pool is unquotable.
pub fn whirlpool_amount_out(
    p: &WhirlpoolQuoteState,
    input_is_a: bool,
    amount_in: u64,
    max_impact_bps: u64,
) -> u64 {
    if amount_in == 0 || p.liquidity == 0 || p.sqrt_price_x64 == 0 || p.fee_rate_ppm >= PPM {
        return 0;
    }
    let after_fee = (amount_in as u128 * (PPM - p.fee_rate_ppm) as u128) / PPM as u128; // ≤ 2^84
    if after_fee == 0 {
        return 0;
    }

    let l = U512::from(p.liquidity);
    let s0 = U512::from(p.sqrt_price_x64);
    let d_in = U512::from(after_fee);

    let (s1, amount_out) = if input_is_a {
        // denom = L·2^64 + ΔA·S0 ≤ 2^192 + 2^212; num = L·2^64·S0 ≤ 2^320.
        let num = (l << 64) * s0;
        let den = (l << 64) + d_in * s0;
        let s1 = (num + den - U512::from(1u8)) / den; // ceil → against trade
        if s1 >= s0 {
            return 0;
        }
        let out = (l * (s0 - s1)) >> 64;
        (s1, out)
    } else {
        let s1 = s0 + (d_in << 64) / l; // floor → against trade
        if s1 <= s0 {
            return 0;
        }
        // num = L·(S1−S0)·2^64 ≤ 2^128·2^129·2^64 ≈ 2^321 — U512 ok.
        let out = ((l * (s1 - s0)) << 64) / (s0 * s1);
        (s1, out)
    };

    // Price-impact guard (bps of sqrt price move).
    let diff = if s1 > s0 { s1 - s0 } else { s0 - s1 };
    let impact_bps = diff * U512::from(BPS) / s0;
    if impact_bps > U512::from(max_impact_bps) {
        return 0;
    }
    u64::try_from(amount_out).unwrap_or(0)
}

/// Ternary search for the input maximizing `profit` on `[min, max]`.
/// Profit along a CPMM/CLMM chain is unimodal; every probe is tracked so
/// clipped regions (CLMM guard returning 0) can't produce a worse answer
/// than one already seen. Mirrors TS `optimizeInputAmount`.
pub fn optimize_input<F: Fn(u64) -> i128>(
    profit_at: F,
    min: u64,
    max: u64,
    max_iterations: u32,
) -> (u64, i128) {
    let mut lo = min;
    let mut hi = max;
    let mut best_amount = 0u64;
    let mut best_profit = i128::MIN;

    let probe = |x: u64, best_amount: &mut u64, best_profit: &mut i128| -> i128 {
        let p = profit_at(x);
        if p > *best_profit {
            *best_profit = p;
            *best_amount = x;
        }
        p
    };

    probe(lo, &mut best_amount, &mut best_profit);
    probe(hi, &mut best_amount, &mut best_profit);
    let mut i = 0;
    while i < max_iterations && hi - lo > 1 {
        let third = (hi - lo) / 3;
        let m1 = lo + third;
        let m2 = hi - third;
        let p1 = probe(m1, &mut best_amount, &mut best_profit);
        let p2 = probe(m2, &mut best_amount, &mut best_profit);
        if p1 < p2 {
            lo = m1 + 1;
        } else {
            hi = m2.saturating_sub(1);
        }
        i += 1;
    }
    (best_amount, best_profit)
}

#[cfg(test)]
mod tests {
    use super::*;

    const Q64: u128 = 1 << 64;

    /// Mirrors TS selftest: 1000 SOL / 150k USDC pool, 1 SOL in, 25 bps.
    #[test]
    fn cpmm_matches_ts_selftest_range() {
        let x = 1_000 * 10u64.pow(9);
        let y = 150_000 * 10u64.pow(6);
        let out = cpmm_amount_out(10u64.pow(9), x, y, 25, 10_000);
        assert!(
            out > 149_000_000 && out < 150_000_000,
            "CPMM out of range: {out}"
        );

        // Zero-fee invariant: k never decreases.
        let out_no_fee = cpmm_amount_out(10u64.pow(9), x, y, 0, 10_000);
        let k_before = x as u128 * y as u128;
        let k_after = (x as u128 + 10u128.pow(9)) * (y as u128 - out_no_fee as u128);
        assert!(k_after >= k_before, "x*y=k violated");

        // Monotonicity + zero cases.
        assert!(cpmm_amount_out(2 * 10u64.pow(9), x, y, 25, 10_000) > out);
        assert_eq!(cpmm_amount_out(0, x, y, 25, 10_000), 0);
        assert_eq!(cpmm_amount_out(1, 0, y, 25, 10_000), 0);
    }

    /// No-overflow proof at the extremes TS BigInt handled natively.
    #[test]
    fn cpmm_extreme_reserves_no_overflow() {
        let out = cpmm_amount_out(u64::MAX, u64::MAX, u64::MAX, 25, 10_000);
        assert!(out > 0 && out < u64::MAX);
    }

    /// Mirrors TS: reserves 100+10-5=105; drained pool -> None.
    #[test]
    fn raydium_reserve_matches_ts_selftest() {
        assert_eq!(raydium_effective_reserve(100, 10, 5), Some(105));
        assert_eq!(raydium_effective_reserve(100, 0, 1_000), None);
        assert_eq!(raydium_effective_reserve(5, 0, 5), None);
    }

    /// Mirrors TS selftest: price 1.0, deep liquidity, 30 bps fee.
    #[test]
    fn whirlpool_matches_ts_selftest() {
        let p = WhirlpoolQuoteState {
            sqrt_price_x64: Q64,
            liquidity: 10u128.pow(15),
            fee_rate_ppm: 3_000,
        };
        let amount_in = 10u64.pow(9);
        let expected = amount_in * 997 / 1000;

        for input_is_a in [true, false] {
            let out = whirlpool_amount_out(&p, input_is_a, amount_in, 10_000);
            assert!(out > 0 && out <= expected, "out of range: {out}");
            assert!(
                expected - out < expected / 1000,
                "impact too large: {out} vs {expected}"
            );
        }

        // Impact guard: ~10% of virtual depth rejected at 100 bps cap.
        assert_eq!(whirlpool_amount_out(&p, true, 10u64.pow(14), 100), 0);
        // Empty liquidity unquotable.
        let dead = WhirlpoolQuoteState { liquidity: 0, ..p };
        assert_eq!(whirlpool_amount_out(&dead, true, amount_in, 10_000), 0);
    }

    /// The U512 headroom case that would overflow u128 math: max liquidity,
    /// high sqrt price. TS BigInt handled it silently; we must not panic
    /// and must stay conservative (0 is acceptable, panic is not).
    #[test]
    fn whirlpool_extreme_values_no_panic() {
        let p = WhirlpoolQuoteState {
            sqrt_price_x64: u128::MAX / 2,
            liquidity: u128::MAX / 2,
            fee_rate_ppm: 3_000,
        };
        let _ = whirlpool_amount_out(&p, true, u64::MAX, 10_000);
        let _ = whirlpool_amount_out(&p, false, u64::MAX, 10_000);
    }

    /// DIFFERENTIAL PARITY: exact outputs captured from the compiled
    /// TypeScript math (`dist/math.js`) via `node -e`. If the Rust port
    /// diverges from TS by a single lamport, these break. This is the
    /// contract that lets monitor-rs replace the TS monitor.
    #[test]
    fn cpmm_bitexact_vs_typescript() {
        // (amount_in, reserve_in, reserve_out, fee_num, fee_den) => TS out
        assert_eq!(
            cpmm_amount_out(
                1_000_000_000,
                1_000_000_000_000,
                150_000_000_000,
                25,
                10_000
            ),
            149_475_897
        );
        assert_eq!(
            cpmm_amount_out(123_456_789, 987_654_321_000, 555_555_555, 25, 10_000),
            69_262
        );
        assert_eq!(
            cpmm_amount_out(
                5_000_000_000,
                66_380_043_210_987,
                5_379_679_801_234,
                25,
                10_000
            ),
            404_174_747
        );
        assert_eq!(cpmm_amount_out(1, 1_000, 1_000, 30, 10_000), 0);
        assert_eq!(
            cpmm_amount_out(u64::MAX, u64::MAX, u64::MAX, 25, 10_000),
            9_211_828_392_252_955_061
        );
    }

    #[test]
    fn whirlpool_bitexact_vs_typescript() {
        // price 1.0, deep liquidity, 30 bps — both directions.
        let p1 = WhirlpoolQuoteState {
            sqrt_price_x64: 18_446_744_073_709_551_616,
            liquidity: 1_000_000_000_000_000,
            fee_rate_ppm: 3_000,
        };
        assert_eq!(
            whirlpool_amount_out(&p1, true, 1_000_000_000, 10_000),
            996_999_005
        );
        assert_eq!(
            whirlpool_amount_out(&p1, false, 1_000_000_000, 10_000),
            996_999_005
        );

        // live-like SOL/USDC sqrtPrice, both directions.
        let p2 = WhirlpoolQuoteState {
            sqrt_price_x64: 7_216_072_408_257_405_000,
            liquidity: 10_000_000_000_000_000,
            fee_rate_ppm: 3_000,
        };
        assert_eq!(
            whirlpool_amount_out(&p2, true, 10_000_000_000, 10_000),
            1_525_658_414
        );
        assert_eq!(
            whirlpool_amount_out(&p2, false, 1_529_000_000, 10_000),
            9_961_853_591
        );

        // low fee (4 bps), tight impact cap 100 bps — still passes.
        let p3 = WhirlpoolQuoteState {
            sqrt_price_x64: 7_216_072_408_257_405_000,
            liquidity: 10_000_000_000_000_000,
            fee_rate_ppm: 400,
        };
        assert_eq!(
            whirlpool_amount_out(&p3, true, 999_999_999, 100),
            152_963_759
        );
    }

    /// Mirrors TS: synthetic concave profit peaking at x=6000.
    #[test]
    fn optimizer_matches_ts_selftest() {
        let peak: i128 = 6_000;
        let (amount, profit) = optimize_input(
            |x| -((x as i128 - peak) * (x as i128 - peak)) / 1_000 + 500,
            1,
            1_000_000,
            48,
        );
        assert!(profit > 490, "optimizer missed peak: {profit}");
        assert!(
            (amount as i128 - peak).abs() < 200,
            "far from peak: {amount}"
        );
    }
}
