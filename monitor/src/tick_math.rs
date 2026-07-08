//! Exact Orca Whirlpool (CLMM) swap math — tick-array aware.
//!
//! Replaces the single-tick approximation. Steps the swap through initialized
//! ticks, crossing `liquidity_net` at each boundary, so an input that would
//! cross ticks on-chain is priced the way the Whirlpool program prices it —
//! not with a flattering constant-liquidity assumption.
//!
//! Conventions (all sqrt prices are Q64.64):
//! - `a_to_b = true`  swaps token A in, price/sqrt DECREASES, tick decreases.
//! - `a_to_b = false` swaps token B in, price/sqrt INCREASES, tick increases.
//!
//! CONSERVATIVE by construction: input deltas round UP, output deltas round
//! DOWN, next-sqrt rounds against the trade. If the swap would step beyond the
//! loaded tick coverage (we can't see the liquidity there), the quote is
//! REJECTED (`None`) rather than estimated — never overestimate.

use ruint::aliases::{U256, U512};

pub const PPM: u128 = 1_000_000;
pub const Q64: u128 = 1 << 64;

/// Orca tradable tick bounds.
pub const MIN_TICK: i32 = -443_636;
pub const MAX_TICK: i32 = 443_636;
pub const MIN_SQRT_PRICE_X64: u128 = 4_295_048_016;
pub const MAX_SQRT_PRICE_X64: u128 = 79_226_673_515_401_279_992_447_579_055;

/// An initialized tick the swap may cross: its sqrt price and net liquidity.
#[derive(Debug, Clone, Copy)]
pub struct Crossing {
    pub sqrt_price: u128,
    pub liquidity_net: i128,
}

/// sqrt(1.0001^tick) in Q64.64, via the canonical Uniswap-V3 constant chain
/// (Q128.128 magic, `>> 64` to Q64.64). Rounds up like the reference.
pub fn sqrt_price_from_tick(tick: i32) -> u128 {
    let abs = tick.unsigned_abs() as u128;
    // Base ratio in Q128.128.
    let mut ratio: U256 = if abs & 0x1 != 0 {
        U256::from_str_radix("fffcb933bd6fad37aa2d162d1a594001", 16).unwrap()
    } else {
        U256::from(1u128) << 128
    };
    const M: &[(u128, &str)] = &[
        (0x2, "fff97272373d413259a46990580e213a"),
        (0x4, "fff2e50f5f656932ef12357cf3c7fdcc"),
        (0x8, "ffe5caca7e10e4e61c3624eaa0941cd0"),
        (0x10, "ffcb9843d60f6159c9db58835c926644"),
        (0x20, "ff973b41fa98c081472e6896dfb254c0"),
        (0x40, "ff2ea16466c96a3843ec78b326b52861"),
        (0x80, "fe5dee046a99a2a811c461f1969c3053"),
        (0x100, "fcbe86c7900a88aedcffc83b479aa3a4"),
        (0x200, "f987a7253ac413176f2b074cf7815e54"),
        (0x400, "f3392b0822b70005940c7a398e4b70f3"),
        (0x800, "e7159475a2c29b7443b29c7fa6e889d9"),
        (0x1000, "d097f3bdfd2022b8845ad8f792aa5825"),
        (0x2000, "a9f746462d870fdf8a65dc1f90e061e5"),
        (0x4000, "70d869a156d2a1b890bb3df62baf32f7"),
        (0x8000, "31be135f97d08fd981231505542fcfa6"),
        (0x10000, "9aa508b5b7a84e1c677de54f3e99bc9"),
        (0x20000, "5d6af8dedb81196699c329225ee604"),
        (0x40000, "2216e584f5fa1ea926041bedfe98"),
        (0x80000, "48a170391f7dc42444e8fa2"),
    ];
    for (bit, hex) in M {
        if abs & bit != 0 {
            let c = U256::from_str_radix(hex, 16).unwrap();
            ratio = (ratio * c) >> 128;
        }
    }
    if tick > 0 {
        ratio = U256::MAX / ratio;
    }
    // Q128.128 -> Q64.64, rounding up.
    let shifted = ratio >> 64;
    let rem = ratio - (shifted << 64);
    let mut out = shifted;
    if rem != U256::ZERO {
        out += U256::from(1u8);
    }
    u128::try_from(out).unwrap_or(MAX_SQRT_PRICE_X64)
}

fn ceil_div_u512(num: U512, den: U512) -> U512 {
    if den == U512::ZERO {
        return U512::ZERO;
    }
    (num + den - U512::from(1u8)) / den
}

/// Δ of token A between two sqrt prices (lower < upper). Round up for input.
pub fn get_amount_a_delta(
    sqrt_lower: u128,
    sqrt_upper: u128,
    liquidity: u128,
    round_up: bool,
) -> u128 {
    if liquidity == 0 || sqrt_lower >= sqrt_upper {
        return 0;
    }
    let diff = U512::from(sqrt_upper - sqrt_lower);
    let numerator = (U512::from(liquidity) << 64) * diff; // L * 2^64 * diff
    let denominator = U512::from(sqrt_lower) * U512::from(sqrt_upper);
    let q = if round_up {
        ceil_div_u512(numerator, denominator)
    } else {
        numerator / denominator
    };
    u128::try_from(q).unwrap_or(u128::MAX)
}

/// Δ of token B between two sqrt prices (lower < upper). Round up for input.
pub fn get_amount_b_delta(
    sqrt_lower: u128,
    sqrt_upper: u128,
    liquidity: u128,
    round_up: bool,
) -> u128 {
    if liquidity == 0 || sqrt_lower >= sqrt_upper {
        return 0;
    }
    let product = U256::from(liquidity) * U256::from(sqrt_upper - sqrt_lower); // Q64.64 scaled
    let mut result = product >> 64;
    if round_up && (product - (result << 64)) != U256::ZERO {
        result += U256::from(1u8);
    }
    u128::try_from(result).unwrap_or(u128::MAX)
}

/// a_to_b: new sqrt after adding `amount` of token A. Rounds up (price stays
/// higher → less output → conservative).
fn next_sqrt_from_a_in(sqrt: u128, liquidity: u128, amount: u128) -> u128 {
    if amount == 0 {
        return sqrt;
    }
    let l = U512::from(liquidity);
    let numerator = (l << 64) * U512::from(sqrt); // L * 2^64 * sqrt
    let product = U512::from(amount) * U512::from(sqrt);
    let denominator = (l << 64) + product;
    u128::try_from(ceil_div_u512(numerator, denominator)).unwrap_or(MIN_SQRT_PRICE_X64)
}

/// b_to_a: new sqrt after adding `amount` of token B. Rounds down (price stays
/// lower → less output → conservative).
fn next_sqrt_from_b_in(sqrt: u128, liquidity: u128, amount: u128) -> u128 {
    if amount == 0 {
        return sqrt;
    }
    let quotient = (U512::from(amount) << 64) / U512::from(liquidity);
    let next = U512::from(sqrt) + quotient;
    u128::try_from(next).unwrap_or(MAX_SQRT_PRICE_X64)
}

struct Step {
    amount_in: u128,
    amount_out: u128,
    next_sqrt: u128,
    fee: u128,
}

/// One exact-input swap step from `sqrt_current` toward `sqrt_target`.
fn compute_swap_step(
    amount_remaining: u128,
    fee_ppm: u128,
    liquidity: u128,
    sqrt_current: u128,
    sqrt_target: u128,
    a_to_b: bool,
) -> Step {
    let amount_after_fee = amount_remaining * (PPM - fee_ppm) / PPM;

    let amount_to_target = if a_to_b {
        get_amount_a_delta(sqrt_target, sqrt_current, liquidity, true)
    } else {
        get_amount_b_delta(sqrt_current, sqrt_target, liquidity, true)
    };

    let (next_sqrt, reached_target) = if amount_after_fee >= amount_to_target {
        (sqrt_target, true)
    } else if a_to_b {
        (
            next_sqrt_from_a_in(sqrt_current, liquidity, amount_after_fee),
            false,
        )
    } else {
        (
            next_sqrt_from_b_in(sqrt_current, liquidity, amount_after_fee),
            false,
        )
    };

    let (amount_in, amount_out) = if a_to_b {
        (
            get_amount_a_delta(next_sqrt, sqrt_current, liquidity, true),
            get_amount_b_delta(next_sqrt, sqrt_current, liquidity, false),
        )
    } else {
        (
            get_amount_b_delta(sqrt_current, next_sqrt, liquidity, true),
            get_amount_a_delta(sqrt_current, next_sqrt, liquidity, false),
        )
    };

    let fee = if reached_target {
        // ceil(amount_in * fee / (PPM - fee))
        let num = amount_in * fee_ppm;
        let den = PPM - fee_ppm;
        num.div_ceil(den)
    } else {
        // consumed all remaining input; leftover after amount_in is the fee.
        amount_remaining.saturating_sub(amount_in)
    };

    Step {
        amount_in,
        amount_out,
        next_sqrt,
        fee,
    }
}

fn apply_liquidity_net(liq: u128, net: i128, a_to_b: bool) -> u128 {
    // liquidity_net is added when the tick is crossed left-to-right (price up).
    let add = if a_to_b { -net } else { net };
    if add >= 0 {
        liq.saturating_add(add as u128)
    } else {
        liq.saturating_sub(add.unsigned_abs())
    }
}

/// Result of an exact-input swap: `None` means REJECTED (would step beyond
/// loaded tick coverage, or liquidity is exhausted) — never overestimate.
pub fn swap_exact_in(
    sqrt_price_current: u128,
    liquidity: u128,
    fee_ppm: u128,
    a_to_b: bool,
    amount_in: u64,
    // Ordered in the swap direction (a_to_b: descending sqrt; else ascending),
    // strictly beyond the current price. Only initialized ticks within loaded
    // tick-array coverage.
    crossings: &[Crossing],
    // Hard edge of loaded coverage (a_to_b: lowest; else highest sqrt seen).
    coverage_limit_sqrt: u128,
) -> Option<u64> {
    if amount_in == 0 || sqrt_price_current == 0 || fee_ppm >= PPM {
        return None;
    }
    let mut remaining = amount_in as u128;
    let mut out: u128 = 0;
    let mut sqrt = sqrt_price_current;
    let mut liq = liquidity;

    for c in crossings {
        // Sanity: crossing must be in the swap direction relative to `sqrt`.
        if a_to_b && c.sqrt_price >= sqrt {
            continue;
        }
        if !a_to_b && c.sqrt_price <= sqrt {
            continue;
        }
        let step = compute_swap_step(remaining, fee_ppm, liq, sqrt, c.sqrt_price, a_to_b);
        remaining = remaining.saturating_sub(step.amount_in + step.fee);
        out += step.amount_out;
        sqrt = step.next_sqrt;
        if remaining == 0 || sqrt != c.sqrt_price {
            return u64::try_from(out).ok();
        }
        // Reached the tick boundary exactly — cross it.
        liq = apply_liquidity_net(liq, c.liquidity_net, a_to_b);
    }

    if remaining == 0 {
        return u64::try_from(out).ok();
    }

    // Input remains after all known crossings. Try to finish within the last
    // covered region; if it still doesn't fit, we'd need liquidity we can't
    // see — REJECT.
    let past_limit = if a_to_b {
        coverage_limit_sqrt >= sqrt
    } else {
        coverage_limit_sqrt <= sqrt
    };
    if past_limit || liq == 0 {
        return None;
    }
    let step = compute_swap_step(remaining, fee_ppm, liq, sqrt, coverage_limit_sqrt, a_to_b);
    remaining = remaining.saturating_sub(step.amount_in + step.fee);
    out += step.amount_out;
    if remaining > 0 {
        // Would step past loaded coverage — unknown liquidity beyond. Reject.
        return None;
    }
    u64::try_from(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::{whirlpool_amount_out, WhirlpoolQuoteState};

    fn far_limit(a_to_b: bool) -> u128 {
        if a_to_b {
            MIN_SQRT_PRICE_X64
        } else {
            MAX_SQRT_PRICE_X64
        }
    }

    #[test]
    fn sqrt_price_from_tick_anchors() {
        // tick 0 -> price 1.0 -> 2^64.
        assert_eq!(sqrt_price_from_tick(0), Q64);
        // monotonic
        assert!(sqrt_price_from_tick(1) > sqrt_price_from_tick(0));
        assert!(sqrt_price_from_tick(-1) < sqrt_price_from_tick(0));
        // symmetry: p(t) * p(-t) ≈ 2^128 (within rounding).
        let p = sqrt_price_from_tick(25130) as f64 / Q64 as f64;
        // 1.0001^(25130/2) ≈ e^(25130/2 * ln 1.0001)
        let expect = (25130.0 / 2.0 * 1.0001_f64.ln()).exp();
        assert!((p / expect - 1.0).abs() < 1e-6, "p={p} expect={expect}");
        // live SOL/USDC tick from verify-layouts (-25130) round-trips near price.
        let pn = sqrt_price_from_tick(-25130) as f64 / Q64 as f64;
        let en = (-25130.0 / 2.0 * 1.0001_f64.ln()).exp();
        assert!((pn / en - 1.0).abs() < 1e-6, "pn={pn} en={en}");
    }

    /// DEFINITIVE: for real on-chain pools, the exact sqrt price must fall in
    /// `[p(tick), p(tick+1))`. Pairs captured live from the Orca API — proves
    /// the tick constants are byte-correct, not just float-close.
    #[test]
    fn sqrt_price_from_tick_brackets_live_pools() {
        // (tickCurrentIndex, sqrtPrice) from api.orca.so/v2/solana/pools
        let live: &[(i32, u128)] = &[
            (-25606, 5_127_895_571_055_230_691),
            (0, 18_447_648_878_060_539_960),
            (64325, 459_895_625_220_235_268_296),
            (-451, 18_036_345_499_896_374_654),
        ];
        for &(tick, sqrt_price) in live {
            let lo = sqrt_price_from_tick(tick);
            let hi = sqrt_price_from_tick(tick + 1);
            assert!(
                lo <= sqrt_price && sqrt_price < hi,
                "tick {tick}: p={sqrt_price} not in [{lo}, {hi})"
            );
        }
    }

    /// ANCHOR: within a single tick (no crossings), exact math must equal the
    /// old single-tick formula — that formula is exact for constant liquidity.
    #[test]
    fn matches_single_tick_when_no_crossing() {
        let sqrt = Q64; // price 1.0
        let liq = 10u128.pow(15);
        let fee = 3000u128;
        for &amt in &[10u64.pow(6), 10u64.pow(8), 10u64.pow(9)] {
            for a_to_b in [true, false] {
                // No crossings, huge coverage → stays within the tick.
                let exact =
                    swap_exact_in(sqrt, liq, fee, a_to_b, amt, &[], far_limit(a_to_b)).unwrap();
                let single = whirlpool_amount_out(
                    &WhirlpoolQuoteState {
                        sqrt_price_x64: sqrt,
                        liquidity: liq,
                        fee_rate_ppm: fee as u64,
                    },
                    a_to_b,
                    amt,
                    10_000,
                );
                // identical to the constant-liquidity closed form (±1 rounding).
                let d = exact.abs_diff(single);
                assert!(
                    d <= 1,
                    "exact={exact} single={single} amt={amt} a_to_b={a_to_b}"
                );
            }
        }
    }

    #[test]
    fn crossing_one_tick_reduces_output_vs_flat() {
        let sqrt = Q64;
        let liq = 10u128.pow(12);
        let fee = 3000u128;
        // Large enough to clear the boundary at tick -30 (needs ~1.5e9) and
        // keep swapping with reduced liquidity afterward.
        let amt = 50 * 10u64.pow(9);
        let cross = Crossing {
            sqrt_price: sqrt_price_from_tick(-30),
            liquidity_net: (liq / 2) as i128, // a_to_b subtracts net → liq halves
        };
        let with_cross = swap_exact_in(sqrt, liq, fee, true, amt, &[cross], MIN_SQRT_PRICE_X64);
        let flat = swap_exact_in(sqrt, liq, fee, true, amt, &[], far_limit(true)).unwrap();
        let crossed = with_cross.expect("should fill within coverage");
        assert!(
            crossed < flat,
            "crossed={crossed} flat={flat} — crossing must cut output"
        );
    }

    #[test]
    fn crossing_multiple_ticks() {
        let sqrt = Q64;
        let liq = 10u128.pow(12);
        let fee = 3000u128;
        let amt = 8 * 10u64.pow(9);
        let crossings: Vec<Crossing> = (1..=3)
            .map(|k| Crossing {
                sqrt_price: sqrt_price_from_tick(-400 * k),
                liquidity_net: (liq / 4) as i128,
            })
            .collect();
        let out = swap_exact_in(sqrt, liq, fee, true, amt, &crossings, MIN_SQRT_PRICE_X64);
        assert!(out.is_some(), "should fill across three tick arrays");
        // Fewer crossings (more assumed depth) yields >= output.
        let out_one = swap_exact_in(
            sqrt,
            liq,
            fee,
            true,
            amt,
            &crossings[..1],
            MIN_SQRT_PRICE_X64,
        );
        if let (Some(a), Some(b)) = (out, out_one) {
            assert!(b >= a, "more crossings must not increase output");
        }
    }

    #[test]
    fn rejects_when_input_exceeds_coverage() {
        // Thin liquidity, no crossings, and a TIGHT coverage limit just below
        // current: a large input can't be filled within coverage → reject.
        let sqrt = Q64;
        let liq = 10u128.pow(9); // thin
        let fee = 3000u128;
        let amt = 10u64.pow(12); // huge relative to depth
        let coverage = sqrt_price_from_tick(-2); // barely below current
        let out = swap_exact_in(sqrt, liq, fee, true, amt, &[], coverage);
        assert!(
            out.is_none(),
            "must reject rather than overestimate: {out:?}"
        );
    }

    #[test]
    fn rejects_on_exhausted_liquidity() {
        let sqrt = Q64;
        let liq = 10u128.pow(10);
        let fee = 3000u128;
        let amt = 10u64.pow(12);
        // One crossing that removes ALL liquidity, then more input remains and
        // no further coverage → reject.
        let cross = Crossing {
            sqrt_price: sqrt_price_from_tick(-10),
            liquidity_net: liq as i128, // a_to_b subtracts → liq -> 0
        };
        let out = swap_exact_in(
            sqrt,
            liq,
            fee,
            true,
            amt,
            &[cross],
            sqrt_price_from_tick(-12),
        );
        assert!(out.is_none(), "exhausted liquidity must reject: {out:?}");
    }

    #[test]
    fn never_overestimates_vs_flat_upper_bound() {
        // The flat (single-tick, no liquidity loss) quote is an UPPER bound on
        // any real swap that only loses liquidity. Exact with non-negative
        // a_to_b crossings must never exceed it.
        let sqrt = Q64;
        let liq = 10u128.pow(13);
        let fee = 3000u128;
        for amt in [10u64.pow(9), 10u64.pow(10), 5 * 10u64.pow(10)] {
            let flat = swap_exact_in(sqrt, liq, fee, true, amt, &[], far_limit(true)).unwrap();
            let cross = Crossing {
                sqrt_price: sqrt_price_from_tick(-50),
                liquidity_net: (liq / 3) as i128,
            };
            if let Some(exact) =
                swap_exact_in(sqrt, liq, fee, true, amt, &[cross], MIN_SQRT_PRICE_X64)
            {
                assert!(exact <= flat, "exact {exact} > flat upper bound {flat}");
            }
        }
    }
}
