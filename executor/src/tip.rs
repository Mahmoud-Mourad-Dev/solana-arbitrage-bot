//! Dynamic Jito tip engine.
//!
//! tip = clamp(min_tip, gross_profit * tier_bps / 10_000, max_tip)
//!
//! The share of profit surrendered as tip scales UP with profitability:
//! fat opportunities attract more competing bundles, and Jito runs a
//! pay-for-priority auction per bundle group — winning a big one at 80%
//! beats losing it at 50%. Tiers operate on GROSS profit because the
//! monitor's own tip estimate is already baked into its netProfit (using
//! net would double-count).

/// Thin wrapper over the shared [`arb_common::cost::jito_tip`] so the executor
/// and the monitor compute the SAME tip from the same schedule — the split
/// cost model was exactly this drifting apart.
pub fn compute_tip(gross_profit_lamports: u64, min_tip: u64, max_tip: u64) -> u64 {
    arb_common::cost::jito_tip(gross_profit_lamports, min_tip, max_tip)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAMPORTS_PER_SOL: u64 = 1_000_000_000;

    #[test]
    fn flat_floor_applies_to_dust() {
        // 0.0001 SOL profit * 50% = 50k... below floor? floor 10k < 50k.
        assert_eq!(compute_tip(100_000, 10_000, u64::MAX), 50_000);
        // truly tiny profit hits the flat floor
        assert_eq!(compute_tip(10_000, 10_000, u64::MAX), 10_000);
    }

    #[test]
    fn tiers_scale_with_profit() {
        let sol = LAMPORTS_PER_SOL;
        assert_eq!(compute_tip(sol / 1000, 0, u64::MAX), sol / 1000 / 2); // 50%
        assert_eq!(compute_tip(sol / 100, 0, u64::MAX), sol / 100 * 6 / 10); // 60%
        assert_eq!(compute_tip(sol / 10, 0, u64::MAX), sol / 10 * 7 / 10); // 70%
        assert_eq!(compute_tip(sol, 0, u64::MAX), sol * 8 / 10); // 80%
    }

    #[test]
    fn cap_applies() {
        assert_eq!(
            compute_tip(10 * LAMPORTS_PER_SOL, 0, 100_000_000),
            100_000_000
        );
    }

    #[test]
    fn no_overflow_at_extremes() {
        assert_eq!(
            compute_tip(u64::MAX, 0, u64::MAX),
            (u64::MAX as u128 * 8 / 10) as u64
        );
    }
}
