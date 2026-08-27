//! In-window market price of the quote token, derived from the population's
//! own clean two-asset swaps — never from a present-day feed and never from a
//! quoted/simulated price. Integer rational arithmetic throughout.
//!
//! A price point is the ratio (quote_units : lamports) observed in one swap
//! whose only balance deltas are WSOL/SOL and the quote mint, on opposite
//! sides. The local price at a slot is the median of the K nearest points by
//! slot — the window is historical and the rate drifts (73→78 USDC/SOL across
//! the S15B window), enough to swamp a thin margin if priced globally.

use serde::Serialize;

/// One observed exchange: `quote_units` of the quote mint moved against
/// `lamports` of SOL value, in the same transaction, opposite directions.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct PricePoint {
    pub slot: u64,
    pub quote_units: u128,
    pub lamports: u128,
}

impl PricePoint {
    /// Compare two ratios quote/lamports by cross-multiplication (no floats).
    /// Magnitudes: units ≤ ~1e15, lamports ≤ ~1e15 → product ≤ 1e30 < u128::MAX.
    fn ratio_cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.quote_units * other.lamports).cmp(&(other.quote_units * self.lamports))
    }
}

/// Median-by-ratio of a set of points. Returns `None` on empty input.
pub fn median_price(points: &[PricePoint]) -> Option<PricePoint> {
    if points.is_empty() {
        return None;
    }
    let mut v: Vec<PricePoint> = points.to_vec();
    v.sort_by(|a, b| a.ratio_cmp(b));
    Some(v[v.len() / 2])
}

/// The local price at `slot`: median of the `k` nearest points by slot.
/// `points` must be sorted by slot ascending. `None` if `points` is empty.
pub fn local_price(points: &[PricePoint], slot: u64, k: usize) -> Option<PricePoint> {
    if points.is_empty() {
        return None;
    }
    let i = points.partition_point(|p| p.slot < slot);
    let half = k / 2;
    let lo = i.saturating_sub(half);
    let hi = (lo + k).min(points.len());
    let lo = hi.saturating_sub(k);
    median_price(&points[lo..hi])
}

/// Convert a signed quote-token delta to lamports of SOL value at `price`.
/// Floor division toward zero; the ≤1-lamport rounding is immaterial against
/// every threshold in use and never flips a sign.
pub fn quote_to_lamports(d_quote: i128, price: &PricePoint) -> i128 {
    if price.quote_units == 0 {
        return 0; // guarded by callers; a zero-quote point is never stored
    }
    d_quote * price.lamports as i128 / price.quote_units as i128
}

/// Realized P&L of a transaction in lamports of SOL-equivalent value.
///
/// THE ACCOUNTING IDENTITY S15A GOT WRONG: `d_sol + d_wsol` alone is correct
/// only for a closed cycle. When the signer spends the quote token to acquire
/// SOL, that formula books the proceeds as profit and ignores what was paid —
/// every ordinary purchase of SOL reads as a large fake profit (all 45 fixture
/// transactions; see `docs/forensics-s15b.md`). The quote leg must be priced.
pub fn value_pnl(d_sol: i128, d_wsol: i128, d_quote: i128, price: &PricePoint) -> i128 {
    d_sol + d_wsol + quote_to_lamports(d_quote, price)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pp(slot: u64, q: u128, l: u128) -> PricePoint {
        PricePoint {
            slot,
            quote_units: q,
            lamports: l,
        }
    }

    /// The exact on-chain numbers from `5KTk2eUJya…`, one of the 45 fixture
    /// transactions S15A reported as +0.0097 SOL of arbitrage: 0.74 USDC spent
    /// across 5 pools for SOL at 74.61 USDC/SOL against a 73.25 market — a
    /// purchase ~1.9% OVER market, not profit.
    #[test]
    fn buying_sol_with_usdc_is_not_arbitrage_profit() {
        let (d_sol, d_wsol, d_usdc) = (9_747_693_i128, 0_i128, -740_000_i128);
        assert_eq!(d_sol + d_wsol, 9_747_693, "the number S15A reported");
        // market 73.25 USDC/SOL == 73_250_000 units per 1e9 lamports
        let market = pp(0, 73_250_000, 1_000_000_000);
        let v = value_pnl(d_sol, d_wsol, d_usdc, &market);
        assert!(
            v < 0,
            "a purchase above market must not read as profit: {v}"
        );
        assert!(
            (-400_000..-300_000).contains(&v),
            "expected ~-0.00035 SOL, got {v}"
        );
    }

    #[test]
    fn closed_cycle_is_unaffected_by_quote_pricing() {
        let market = pp(0, 74_950_000, 1_000_000_000);
        assert_eq!(value_pnl(50_000, 0, 0, &market), 50_000);
    }

    #[test]
    fn market_rate_swap_nets_about_zero() {
        // sell 0.01 SOL, receive 0.7495 USDC at 74.95
        let market = pp(0, 74_950_000, 1_000_000_000);
        let v = value_pnl(-10_000_000, 0, 749_500, &market);
        assert!(v.abs() < 10_000, "market-rate swap nets ~0, got {v}");
    }

    #[test]
    fn median_is_by_ratio_not_by_field() {
        // ratios: 2.0, 1.0, 4.0 → median 2.0 (the (200, 100) point)
        let pts = vec![pp(1, 200, 100), pp(2, 100, 100), pp(3, 400, 100)];
        let m = median_price(&pts).unwrap();
        assert_eq!((m.quote_units, m.lamports), (200, 100));
    }

    #[test]
    fn local_price_windows_by_slot() {
        // price 1.0 at low slots, 3.0 at high slots
        let mut pts: Vec<PricePoint> = (0..100).map(|i| pp(i, 100, 100)).collect();
        pts.extend((1000..1100).map(|i| pp(i, 300, 100)));
        let lo = local_price(&pts, 50, 51).unwrap();
        let hi = local_price(&pts, 1050, 51).unwrap();
        assert_eq!(lo.quote_units, 100);
        assert_eq!(hi.quote_units, 300);
    }

    #[test]
    fn empty_points_is_none_not_a_default() {
        assert!(median_price(&[]).is_none());
        assert!(local_price(&[], 5, 51).is_none());
    }
}
