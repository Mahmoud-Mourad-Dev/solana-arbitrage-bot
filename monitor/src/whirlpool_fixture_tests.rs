//! S14B-1 fixture-driven Whirlpool parity tests. The ONLY parity evidence
//! accepted here is: local exact math == observed on-chain vault delta, on
//! captured real-swap fixtures (`fixtures/whirlpool/real_swaps.json`).
//!
//! These tests are DETERMINISTIC — they replay committed fixtures offline.

#[cfg(test)]
mod tests {
    use crate::types::tick_array_pda;
    use crate::whirlpool_parity::*;
    use solana_sdk::pubkey::Pubkey;
    use std::str::FromStr;

    const FIXTURES: &str = include_str!("../fixtures/whirlpool/real_swaps.json");
    const DISCREPANCIES: &str = include_str!("../fixtures/whirlpool/discrepancies.json");
    const WSOL: &str = "So11111111111111111111111111111111111111112";

    fn load() -> WhirlpoolFixtureFile {
        serde_json::from_str(FIXTURES).expect("real_swaps.json parses")
    }
    fn load_disc() -> WhirlpoolFixtureFile {
        serde_json::from_str(DISCREPANCIES).expect("discrepancies.json parses")
    }
    fn variant(fx: &SwapFixture) -> &'static str {
        match fx.data_hex.get(0..16).unwrap_or("") {
            "f8c69e91e17587c8" => "swap",
            "2b04ed0b1ac91e62" => "swapV2",
            _ => "other",
        }
    }

    fn pool_of<'a>(f: &'a WhirlpoolFixtureFile, fx: &SwapFixture) -> &'a PoolRecord {
        f.pools.iter().find(|p| p.address == fx.pool).unwrap()
    }

    /// Is this fixture's input the non-WSOL token (the strategic direction)?
    fn is_token_to_wsol(f: &WhirlpoolFixtureFile, fx: &SwapFixture) -> bool {
        let p = pool_of(f, fx);
        let in_mint = if fx.a_to_b {
            &p.token_mint_a
        } else {
            &p.token_mint_b
        };
        in_mint != WSOL
    }

    #[test]
    fn evidence_set_meets_slice_requirements() {
        let f = load();
        assert_eq!(f.schema_version, FIXTURE_SCHEMA_VERSION);
        assert!(f.pools.len() >= 2, "≥2 pools required");
        assert!(f.swaps.len() >= 6, "≥6 usable swaps required");
        let t2w = f.swaps.iter().filter(|fx| is_token_to_wsol(&f, fx)).count();
        assert!(t2w >= 3, "≥3 Token→WSOL swaps required, got {t2w}");
        // Distinct pools actually exercised.
        let pools: std::collections::BTreeSet<&str> =
            f.swaps.iter().map(|s| s.pool.as_str()).collect();
        assert!(pools.len() >= 2, "swaps must cover ≥2 pools");
        // Multiple input sizes (at least 3 distinct magnitudes).
        let mut sizes: Vec<u64> = f.swaps.iter().map(|s| s.amount_in).collect();
        sizes.sort_unstable();
        sizes.dedup();
        assert!(sizes.len() >= 3, "multiple distinct input sizes required");
        // No ambiguous fixtures were ever written.
        for fx in &f.swaps {
            assert!(
                fx.freshness == "EXACT_SLOT" || fx.freshness == "PRE_SLOT_MATCH",
                "{}: {}",
                fx.sig,
                fx.freshness
            );
        }
    }

    /// THE parity assertion: every accepted fixture replays to the EXACT
    /// observed output. No tolerance.
    #[test]
    fn every_fixture_replays_exactly() {
        let f = load();
        for fx in &f.swaps {
            let d = replay_fixture(fx).unwrap_or_else(|| panic!("{} rejected by replay", fx.sig));
            assert_eq!(
                d.amount_out,
                fx.observed_out,
                "{} {} in={} local={} observed={} diff={}",
                fx.sig,
                fx.direction,
                fx.amount_in,
                d.amount_out,
                fx.observed_out,
                d.amount_out as i128 - fx.observed_out as i128
            );
        }
    }

    /// HONEST SCOPE: the proven fixture set reproduces real swaps that stay
    /// within a single tick. On-chain tick-CROSSING parity is NOT proven here —
    /// the only real crossing captured landed in the discrepancy set (legacy
    /// direct swap-v1). Tick-crossing MATH is covered by `tick_math` unit tests
    /// (`crossing_one_tick_reduces_output_vs_flat`, `crossing_multiple_ticks`).
    #[test]
    fn proven_set_is_single_tick_crossing_is_unit_tested_only() {
        let f = load();
        for fx in &f.swaps {
            let d = replay_fixture(fx).unwrap();
            assert_eq!(
                d.ticks_crossed, 0,
                "{} unexpectedly crossed a tick in the proven set",
                fx.sig
            );
        }
    }

    /// The discrepancy set is fully accounted for: every entry is the LEGACY
    /// `swap` (v1) instruction invoked DIRECTLY (not via CPI), and every diff is
    /// sub-bps and tiny. swapV2 and CPI-routed swaps never appear here. This
    /// documents the open edge WITHOUT masking it with a tolerance in the
    /// proven path.
    #[test]
    fn discrepancy_set_is_bounded_legacy_swap_v1_direct() {
        let d = load_disc();
        assert!(!d.swaps.is_empty(), "discrepancy set should not be empty");
        for fx in &d.swaps {
            assert_eq!(
                variant(fx),
                "swap",
                "{}: only legacy swap-v1 may differ",
                fx.sig
            );
            assert!(!fx.cpi, "{}: CPI swaps must be exact, never here", fx.sig);
            let local = replay_fixture(fx).expect("replays (just not exactly)");
            let diff = (local.amount_out as i128 - fx.observed_out as i128).unsigned_abs();
            // Documented bound: observed sub-bps drift on legacy direct swaps.
            assert!(
                diff > 0,
                "{}: exact fixtures belong in the proven set",
                fx.sig
            );
            assert!(
                diff <= 64,
                "{}: diff {diff} exceeds documented bound",
                fx.sig
            );
            // The drift is a vanishing fraction of the output (< 1 bps).
            let bps = diff.saturating_mul(10_000) / (fx.observed_out.max(1) as u128);
            assert_eq!(bps, 0, "{}: drift must be < 1 bps, got {bps}", fx.sig);
        }
    }

    #[test]
    fn token_to_wsol_direction_replays_exactly() {
        let f = load();
        let mut n = 0;
        for fx in f.swaps.iter().filter(|fx| is_token_to_wsol(&f, fx)) {
            let d = replay_fixture(fx).unwrap();
            assert_eq!(d.amount_out, fx.observed_out, "{}", fx.sig);
            n += 1;
        }
        assert!(n >= 3, "strategic direction needs ≥3 exact fixtures");
    }

    // ── negative controls (each must fail for its typed reason) ──

    #[test]
    fn negative_stale_sqrt_price_breaks_parity() {
        let f = load();
        let mut fx = f.swaps[0].clone();
        // Perturb the snapshot: a stale/wrong sqrt price must NOT reproduce the
        // observed output (otherwise the comparison would prove nothing).
        fx.pool_state.sqrt_price_x64 = fx.pool_state.sqrt_price_x64 / 100 * 101;
        if let Some(d) = replay_fixture(&fx) {
            assert_ne!(
                d.amount_out, fx.observed_out,
                "stale state must not replay exactly"
            );
        }
    }

    #[test]
    fn negative_missing_tick_arrays_rejects_or_reprices_a_crossing_swap() {
        // Use the ONE real crossing swap (captured in the discrepancy set):
        // stripping its tick arrays must change the result — a crossing swap
        // can never be priced correctly without the arrays it traverses.
        let d = load_disc();
        let crossing = d.swaps.iter().find(|s| {
            replay_fixture(s)
                .map(|r| r.ticks_crossed > 0)
                .unwrap_or(false)
        });
        let Some(fx0) = crossing else {
            return; // no real crossing captured this run — nothing to assert
        };
        let with = replay_fixture(fx0).unwrap();
        let mut stripped = fx0.clone();
        stripped.tick_arrays = vec![];
        let without = replay_fixture(&stripped);
        assert!(
            without.is_none() || without.unwrap().amount_out != with.amount_out,
            "a crossing swap must not price identically without its tick arrays"
        );
    }

    #[test]
    fn negative_wrong_pool_tick_array_rejected_by_provenance() {
        let f = load();
        let fx = &f.swaps[0];
        let real_pool = Pubkey::from_str(&fx.pool).unwrap();
        let other_pool = Pubkey::new_unique();
        // A tick array PDA derived for ANOTHER pool must fail validation.
        let start = fx.tick_arrays[0].start_tick_index;
        let foreign = tick_array_pda(&other_pool, start);
        // Build a minimal valid-shaped account whose back-pointer is the other
        // pool: both the PDA check and the back-pointer check must reject.
        let mut data = vec![0u8; crate::parsers::TICK_ARRAY_ACCOUNT_SIZE];
        data[8..12].copy_from_slice(&start.to_le_bytes());
        data[crate::parsers::TICK_ARRAY_ACCOUNT_SIZE - 32..].copy_from_slice(other_pool.as_ref());
        let r = validate_tick_array(0, &foreign, &data, &real_pool, fx.pool_state.tick_spacing);
        assert!(
            matches!(r, Err(WhirlpoolReject::TickArrayWrongPool { .. })),
            "{r:?}"
        );
    }

    #[test]
    fn negative_bad_start_alignment_rejected() {
        let f = load();
        let fx = &f.swaps[0];
        let pool = Pubkey::from_str(&fx.pool).unwrap();
        let bad_start = fx.tick_arrays[0].start_tick_index + 1; // misaligned
        let mut data = vec![0u8; crate::parsers::TICK_ARRAY_ACCOUNT_SIZE];
        data[8..12].copy_from_slice(&bad_start.to_le_bytes());
        data[crate::parsers::TICK_ARRAY_ACCOUNT_SIZE - 32..].copy_from_slice(pool.as_ref());
        let r = validate_tick_array(
            0,
            &tick_array_pda(&pool, bad_start),
            &data,
            &pool,
            fx.pool_state.tick_spacing,
        );
        assert!(
            matches!(r, Err(WhirlpoolReject::TickArrayBadStart { .. })),
            "{r:?}"
        );
    }

    #[test]
    fn negative_malformed_tick_array_rejected() {
        let pool = Pubkey::new_unique();
        let r = validate_tick_array(0, &Pubkey::new_unique(), &[0u8; 100], &pool, 64);
        assert!(matches!(
            r,
            Err(WhirlpoolReject::TickArrayUndecodable { .. })
        ));
    }

    #[test]
    fn negative_wrong_direction_does_not_match_observed() {
        let f = load();
        // Flipping the direction on a real fixture must not reproduce the
        // observed output (sanity that direction is inferred, not assumed).
        let fx0 = &f.swaps[0];
        let mut fx = fx0.clone();
        fx.a_to_b = !fx.a_to_b;
        if let Some(d) = replay_fixture(&fx) {
            assert_ne!(d.amount_out, fx.observed_out, "{}", fx.sig);
        }
    }

    #[test]
    fn negative_excessive_amount_rejects_not_overestimates() {
        let f = load();
        let mut fx = f.swaps[0].clone();
        // An input far beyond loaded coverage must be REJECTED (None), never
        // silently extrapolated.
        fx.amount_in = u64::MAX / 4;
        assert!(
            replay_fixture(&fx).is_none(),
            "excessive input must reject, not extrapolate"
        );
    }

    #[test]
    fn fixture_pools_are_clean_spl_markets() {
        // This slice only accepts classic-SPL (or extensionless) mints: the
        // observed vault delta then equals the destination credit exactly.
        let f = load();
        for p in &f.pools {
            assert!(!p.transfer_fee_or_hook, "{} has extensions", p.address);
        }
    }
}
