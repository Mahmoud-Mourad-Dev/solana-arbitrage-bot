//! Pure decision helpers for slot/freshness/sleep consistency in the polling
//! path. Extracted so the rules are unit-testable without RPC or a full run.
//!
//! Why these exist: an RPC poll fetches many accounts in several concurrent
//! chunks, each answered at a possibly different slot. Mixing accounts from
//! different slots — or mixing stale (pre-sleep) state with fresh — can
//! fabricate arbitrage that never existed on-chain. These helpers let the
//! polling loop (a) reject a poll whose accounts span too many slots, (b)
//! discard cycles that touch a stale pool, and (c) detect a sleep/gap and
//! rehydrate instead of trusting mixed state.

/// Max slot spread within one poll before the snapshot is considered
/// inconsistent (discovery is skipped for that poll).
pub const DEFAULT_MAX_SLOT_SPREAD: u64 = 3;

/// A pool older than `poll_slot - DEFAULT_MAX_POOL_SLOT_LAG` is treated as
/// stale and any cycle touching it is rejected.
pub const DEFAULT_MAX_POOL_SLOT_LAG: u64 = 4;

/// True when the accounts fetched in one poll are close enough in slot to
/// trust cross-pool comparisons.
pub fn slot_spread_ok(min_slot: u64, max_slot: u64, max_spread: u64) -> bool {
    max_slot >= min_slot && max_slot - min_slot <= max_spread
}

/// The slot floor below which a pool is considered stale for this poll.
pub fn fresh_floor(poll_slot: u64, max_lag: u64) -> u64 {
    poll_slot.saturating_sub(max_lag)
}

/// True when the wall-clock gap between two polls indicates the process was
/// suspended (laptop sleep, pause) rather than a normal cadence. Triggers on
/// a gap larger than 4 poll intervals or an absolute 30s, whichever is bigger
/// — so normal jitter never trips it but a real sleep always does.
pub fn is_sleep_gap(gap_ms: u64, poll_interval_ms: u64) -> bool {
    let threshold = poll_interval_ms.saturating_mul(4).max(30_000);
    gap_ms > threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spread_gate() {
        assert!(slot_spread_ok(100, 100, 3)); // identical
        assert!(slot_spread_ok(100, 103, 3)); // at the edge
        assert!(!slot_spread_ok(100, 104, 3)); // over
        assert!(!slot_spread_ok(104, 100, 3)); // inverted (shouldn't happen) -> reject
    }

    #[test]
    fn freshness_floor() {
        assert_eq!(fresh_floor(1000, 4), 996);
        assert_eq!(fresh_floor(2, 4), 0); // saturates, never underflows
    }

    /// A post-sleep poll (huge wall gap) must be flagged; normal 3s cadence
    /// with jitter must not.
    #[test]
    fn sleep_gap_detection() {
        // 3s interval: normal polls (even a slow 10s one) are NOT a gap.
        assert!(!is_sleep_gap(3_100, 3_000));
        assert!(!is_sleep_gap(10_000, 3_000));
        // A 2.4-hour sleep (your run) IS a gap.
        assert!(is_sleep_gap(8_791_000, 3_000));
        // Absolute floor: even with a tiny interval, <30s is not a gap.
        assert!(!is_sleep_gap(20_000, 1_000));
        assert!(is_sleep_gap(31_000, 1_000));
    }
}
