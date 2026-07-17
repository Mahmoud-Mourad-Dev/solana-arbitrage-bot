//! Offline repricing of historical observe economics under the corrected Pump
//! fee-v2 model (S13C slice 6B, step 7). PURE: no RPC.
//!
//! The observe/narrow economics were computed with the stale hardcoded 30 bps
//! Pump sell fee. On the `meteora->pump` routes the Pump leg is a SELL, so its
//! WSOL output was OVERSTATED by `(real_fee - 30)` bps of the Pump-leg notional.
//! This module recomputes each poll's competitive net under a corrected fee.
//!
//! HONESTY: without the historical fee-config account per observation we cannot
//! do EXACT historical repricing. We therefore apply a clearly-labelled
//! MEASURED-CURRENT-RATE sensitivity (Route 1 = 75 bps, Route 3 = 95 bps — the
//! rates measured in slice 6) and mark every corrected record as estimated.
//! Records on pools without a measured rate are NOT repriced.

/// Old (stale) Pump fee assumption the observe economics used.
pub const LEGACY_FEE_BPS: i128 = 30;

/// Measured-current-rate sensitivity per supported Pump pool (slice 6).
pub fn measured_fee_bps(pool: &str) -> Option<i128> {
    match pool {
        "5ByL7MZoLABYnwMPZKPKjf4MGkZ7FeBzrAnos19Pre2z" => Some(75), // route 1
        "8qDidAKuyNYKaR4dh2ZFZZVG5gBTUfyJcwQPgwt9FS1Y" => Some(95), // route 3
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepriceClass {
    /// Repriced with the measured current rate as a labelled sensitivity.
    EstimatedCurrentRate,
    /// No measured rate for this pool — left unchanged, flagged.
    NotRepriceable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepricedPoll {
    pub class: RepriceClass,
    pub old_net: i128,
    pub corrected_net: i128,
    pub extra_fee_lamports: i128,
}

/// The Pump-leg WSOL notional recovered on a `meteora->pump` round trip: you put
/// in `size` WSOL and receive back ≈ `size + gross` WSOL from the Pump sell.
fn pump_leg_notional(size_lamports: u64, gross_lamports: u64) -> i128 {
    size_lamports as i128 + gross_lamports as i128
}

/// Reprice one poll's competitive net under the corrected Pump fee for `pool`.
pub fn reprice_poll(
    pool: &str,
    old_net: i128,
    size_lamports: u64,
    gross_lamports: u64,
) -> RepricedPoll {
    match measured_fee_bps(pool) {
        Some(real_bps) => {
            let extra_bps = real_bps - LEGACY_FEE_BPS; // >0: previously under-charged
            let notional = pump_leg_notional(size_lamports, gross_lamports);
            // ceil the extra fee (matches on-chain per-component ceil direction).
            let extra_fee = (notional * extra_bps + 9_999) / 10_000;
            RepricedPoll {
                class: RepriceClass::EstimatedCurrentRate,
                old_net,
                corrected_net: old_net - extra_fee,
                extra_fee_lamports: extra_fee,
            }
        }
        None => RepricedPoll {
            class: RepriceClass::NotRepriceable,
            old_net,
            corrected_net: old_net,
            extra_fee_lamports: 0,
        },
    }
}

/// Aggregate repricing outcome over a set of polls.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepriceSummary {
    pub total_polls: usize,
    pub estimated: usize,
    pub not_repriceable: usize,
    /// Competitive-positive polls (net > 0) BEFORE correction, on measured pools.
    pub positive_before: usize,
    /// Competitive-positive polls AFTER correction, on measured pools.
    pub positive_after: usize,
    /// Sum of positive old nets (capturable value proxy) BEFORE, measured pools.
    pub capturable_before_lamports: i128,
    /// Sum of positive corrected nets AFTER, measured pools.
    pub capturable_after_lamports: i128,
}

pub fn summarize<'a>(
    polls: impl Iterator<Item = (&'a str, i128, u64, u64)>, // (pool, old_net, size, gross)
) -> RepriceSummary {
    let mut s = RepriceSummary::default();
    for (pool, old_net, size, gross) in polls {
        s.total_polls += 1;
        let r = reprice_poll(pool, old_net, size, gross);
        match r.class {
            RepriceClass::EstimatedCurrentRate => {
                s.estimated += 1;
                if r.old_net > 0 {
                    s.positive_before += 1;
                    s.capturable_before_lamports += r.old_net;
                }
                if r.corrected_net > 0 {
                    s.positive_after += 1;
                    s.capturable_after_lamports += r.corrected_net;
                }
            }
            RepriceClass::NotRepriceable => s.not_repriceable += 1,
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measured_rates_only_for_supported_pools() {
        assert_eq!(
            measured_fee_bps("5ByL7MZoLABYnwMPZKPKjf4MGkZ7FeBzrAnos19Pre2z"),
            Some(75)
        );
        assert_eq!(
            measured_fee_bps("8qDidAKuyNYKaR4dh2ZFZZVG5gBTUfyJcwQPgwt9FS1Y"),
            Some(95)
        );
        assert_eq!(measured_fee_bps("SomeOtherPool"), None);
    }

    #[test]
    fn corrected_net_drops_by_extra_fee() {
        // Route 1: extra 45 bps on a 1 SOL notional (size 1e9, gross 0).
        let r = reprice_poll(
            "5ByL7MZoLABYnwMPZKPKjf4MGkZ7FeBzrAnos19Pre2z",
            4_000_000,
            1_000_000_000,
            0,
        );
        assert_eq!(r.class, RepriceClass::EstimatedCurrentRate);
        // 45 bps of 1e9 = 4_500_000.
        assert_eq!(r.extra_fee_lamports, 4_500_000);
        assert_eq!(r.corrected_net, 4_000_000 - 4_500_000); // now NEGATIVE
        assert!(r.corrected_net < 0);
    }

    #[test]
    fn route3_uses_65bps_extra() {
        let r = reprice_poll(
            "8qDidAKuyNYKaR4dh2ZFZZVG5gBTUfyJcwQPgwt9FS1Y",
            10_000_000,
            1_000_000_000,
            0,
        );
        // (95-30)=65 bps of 1e9 = 6_500_000.
        assert_eq!(r.extra_fee_lamports, 6_500_000);
        assert_eq!(r.corrected_net, 3_500_000);
    }

    #[test]
    fn unknown_pool_not_repriced() {
        let r = reprice_poll("Xpool", 5_000_000, 1_000_000_000, 0);
        assert_eq!(r.class, RepriceClass::NotRepriceable);
        assert_eq!(r.corrected_net, r.old_net);
    }

    #[test]
    fn summary_counts_positive_flip() {
        let polls = vec![
            (
                "5ByL7MZoLABYnwMPZKPKjf4MGkZ7FeBzrAnos19Pre2z",
                4_000_000i128,
                1_000_000_000u64,
                0u64,
            ),
            (
                "8qDidAKuyNYKaR4dh2ZFZZVG5gBTUfyJcwQPgwt9FS1Y",
                10_000_000,
                1_000_000_000,
                0,
            ),
            ("Xpool", 9_000_000, 1_000_000_000, 0),
        ];
        let s = summarize(polls.into_iter());
        assert_eq!(s.total_polls, 3);
        assert_eq!(s.estimated, 2);
        assert_eq!(s.not_repriceable, 1);
        assert_eq!(s.positive_before, 2); // both measured pools were positive
        assert_eq!(s.positive_after, 1); // route1 flips negative, route3 stays positive
    }
}
