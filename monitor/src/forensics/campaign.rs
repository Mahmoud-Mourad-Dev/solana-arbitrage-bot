//! Persistence-campaign helpers (Prompt O): adaptive windowing, per-sweep
//! SOL/USD measurement, and class-level $/day — the testable core the
//! `observe-campaign` binary orchestrates.
//!
//! READ-ONLY. Nothing here constructs, signs, or submits a transaction.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use solana_client::rpc_client::{GetConfirmedSignaturesForAddress2Config, RpcClient};
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

use super::pipeline::{
    extract_price_points, fetch_accounting, scan_pair, PairOutcome, ScanOptions,
};
use super::schema::InputV2;

/// Threshold at which an event is "addressable" (lamports, net) — the same
/// 50k floor the batch verdict uses.
pub const ADDRESSABLE_THRESHOLD_LAMPORTS: i128 = 50_000;

/// Adaptive window ladder in hours: start at 3h and halve on a guard fire,
/// down to a 0.05h floor. Fixed sequence so the extrapolation factor of every
/// row is one of a known set.
pub const WINDOW_LADDER_HOURS: [f64; 6] = [3.0, 1.5, 0.75, 0.35, 0.15, 0.075];
pub const WINDOW_FLOOR_HOURS: f64 = 0.05;

/// Known SOL/USDC pools for the per-sweep price measurement — the three
/// Whirlpool SOL/USDC pools validated in S15B. Reused, not re-derived.
pub const SOL_USDC_POOLS: &[&str] = &[
    "83v8iPyZihDEjDdY8RdZddyZNyUtXngz69Lgo9Kt5d6d",
    "BSddxwYW73as8852ZTHRH13pbZEmZ96NBjayc5mSVtkZ",
    "Esvfxt3jMDdtTZqLF1fqRhDjzM8Bpr7fZxJMrK69PB7e",
];
const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

/// Minimum clean price points before a sweep's USD column is trusted; below
/// this the sweep is INCONCLUSIVE on USD rather than borrowing a prior price.
pub const MIN_PRICE_SWAPS: usize = 10;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolUsd {
    pub n: usize,
    pub median: f64,
    pub p25: f64,
    pub p75: f64,
    /// True when `n >= MIN_PRICE_SWAPS`; otherwise the USD column is INCONCLUSIVE.
    pub conclusive: bool,
}

/// Measure SOL/USD from recent clean two-sided SOL/USDC swaps. `sample_per_pool`
/// caps the getTransaction fan-out per pool.
pub fn measure_sol_usd(rpc: &RpcClient, sample_per_pool: usize) -> Result<SolUsd> {
    let mut usd_per_sol: Vec<f64> = Vec::new();
    for pool in SOL_USDC_POOLS {
        let Ok(pk) = Pubkey::from_str(pool) else {
            continue;
        };
        let cfg = GetConfirmedSignaturesForAddress2Config {
            before: None,
            until: None,
            limit: Some(sample_per_pool),
            commitment: Some(CommitmentConfig::confirmed()),
        };
        let Ok(sigs) = rpc.get_signatures_for_address_with_config(&pk, cfg) else {
            continue;
        };
        let mut accts = Vec::new();
        for s in sigs.iter().filter(|s| s.err.is_none()) {
            if let Ok(a) = fetch_accounting(rpc, &s.signature, s.slot) {
                accts.push(a);
            }
        }
        for p in extract_price_points(&accts, USDC_MINT) {
            // USDC has 6 decimals; USD/SOL = (units/1e6) / (lamports/1e9).
            let v = (p.quote_units as f64 / 1e6) / (p.lamports as f64 / 1e9);
            if (1.0..100_000.0).contains(&v) {
                usd_per_sol.push(v);
            }
        }
    }
    usd_per_sol.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = usd_per_sol.len();
    let pick = |q: f64| -> f64 {
        if n == 0 {
            return 0.0;
        }
        usd_per_sol[((n as f64 * q) as usize).min(n - 1)]
    };
    Ok(SolUsd {
        n,
        median: pick(0.5),
        p25: pick(0.25),
        p75: pick(0.75),
        conclusive: n >= MIN_PRICE_SWAPS,
    })
}

#[derive(Debug, Serialize)]
pub struct AdaptiveOutcome {
    /// The window actually used (hours). Carries the extrapolation factor.
    pub window_hours: f64,
    /// True if every ladder rung + the floor tripped a guard.
    pub guard_floor_exceeded: bool,
    /// Observed landed cross-tx rate (per hour) at the last attempt, for the
    /// GUARD_FLOOR_EXCEEDED report — informative even when unmeasured.
    pub observed_rate_per_hour: Option<f64>,
    pub outcome: Option<PairOutcome>,
}

/// Is this error one of the deliberate census guards (truncation / fetch cap)?
fn is_guard_error(e: &anyhow::Error) -> bool {
    let s = e.to_string();
    s.contains("TRUNCATED")
        || s.contains("exceeds the") && s.contains("fetch cap")
        || s.contains("exceeds the") && s.contains("-fetch cap")
}

/// Scan one market, halving the window on each guard fire. Never subsamples;
/// records the window used. `now_slot` anchors every attempt at the tip.
pub fn scan_adaptive(
    rpc: &RpcClient,
    base: &InputV2,
    now_slot: u64,
    slots_per_hour: u64,
    opt: &ScanOptions,
) -> AdaptiveOutcome {
    let mut ladder: Vec<f64> = WINDOW_LADDER_HOURS.to_vec();
    ladder.push(WINDOW_FLOOR_HOURS);
    let mut last_rate = None;
    for (i, &hours) in ladder.iter().enumerate() {
        let mut input = base.clone();
        input.slot_max = now_slot;
        input.slot_min = now_slot.saturating_sub((hours * slots_per_hour as f64) as u64);
        input.window_hours = hours;
        match scan_pair(rpc, &input, opt) {
            Ok(o) => {
                return AdaptiveOutcome {
                    window_hours: hours,
                    guard_floor_exceeded: false,
                    observed_rate_per_hour: None,
                    outcome: Some(o),
                };
            }
            Err(e) => {
                // Estimate the rate from the fetch-cap error if present, so the
                // GUARD_FLOOR_EXCEEDED row still carries a tx rate.
                if let Some(n) = parse_landed_from_error(&e) {
                    last_rate = Some(n as f64 / hours);
                }
                if is_guard_error(&e) {
                    if i + 1 < ladder.len() {
                        continue; // halve and retry
                    }
                    return AdaptiveOutcome {
                        window_hours: hours,
                        guard_floor_exceeded: true,
                        observed_rate_per_hour: last_rate,
                        outcome: None,
                    };
                }
                // Non-guard error (RPC etc.): report as no outcome, not a floor.
                return AdaptiveOutcome {
                    window_hours: hours,
                    guard_floor_exceeded: false,
                    observed_rate_per_hour: last_rate,
                    outcome: None,
                };
            }
        }
    }
    AdaptiveOutcome {
        window_hours: WINDOW_FLOOR_HOURS,
        guard_floor_exceeded: true,
        observed_rate_per_hour: last_rate,
        outcome: None,
    }
}

/// Pull "N landed cross-venue txs" out of the fetch-cap error message.
fn parse_landed_from_error(e: &anyhow::Error) -> Option<usize> {
    let s = e.to_string();
    let idx = s.find(" landed cross-venue txs")?;
    let prefix = &s[..idx];
    prefix
        .rsplit(|c: char| !c.is_ascii_digit())
        .find(|t| !t.is_empty())?
        .parse()
        .ok()
}

/// $/day contributed by one market outcome: the ≥50k-threshold sum, scaled to
/// a day and converted at `sol_usd`. Returns 0.0 for an unmeasured market.
pub fn market_dollars_per_day(o: &PairOutcome, window_hours: f64, sol_usd: f64) -> f64 {
    let sum_over =
        o.q4.thresholds
            .iter()
            .find(|t| t.threshold_lamports == ADDRESSABLE_THRESHOLD_LAMPORTS)
            .map(|t| t.sum_lamports)
            .unwrap_or(0);
    if window_hours <= 0.0 {
        return 0.0;
    }
    (sum_over as f64 / 1e9) / window_hours * 24.0 * sol_usd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_ladder_is_the_pre_registered_sequence() {
        // The exact rungs fixed in Prompt O (approximate halving, rounded).
        assert_eq!(WINDOW_LADDER_HOURS, [3.0, 1.5, 0.75, 0.35, 0.15, 0.075]);
        // Strictly decreasing, each roughly half the previous (0.4x..0.55x).
        for w in WINDOW_LADDER_HOURS.windows(2) {
            let ratio = w[1] / w[0];
            assert!((0.4..=0.55).contains(&ratio), "rung {w:?} not ~half");
        }
        assert!(*WINDOW_LADDER_HOURS.last().unwrap() > WINDOW_FLOOR_HOURS);
    }

    #[test]
    fn guard_error_detection() {
        let trunc = anyhow::anyhow!(
            "TRUNCATED: pool X pagination hit the 300-page cap before reaching slot 1"
        );
        assert!(is_guard_error(&trunc));
        let cap = anyhow::anyhow!(
            "39097 landed cross-venue txs exceeds the 12000-fetch cap; shrink the slot window"
        );
        assert!(is_guard_error(&cap));
        let rpc = anyhow::anyhow!("getTransaction: connection reset");
        assert!(!is_guard_error(&rpc));
    }

    #[test]
    fn rate_parsed_from_fetch_cap_error() {
        let e = anyhow::anyhow!(
            "39097 landed cross-venue txs exceeds the 12000-fetch cap; shrink the slot window"
        );
        assert_eq!(parse_landed_from_error(&e), Some(39097));
        let none = anyhow::anyhow!("TRUNCATED: pool X");
        assert_eq!(parse_landed_from_error(&none), None);
    }

    #[test]
    fn sol_usd_inconclusive_below_floor() {
        let s = SolUsd {
            n: 4,
            median: 100.0,
            p25: 98.0,
            p75: 102.0,
            conclusive: 4 >= MIN_PRICE_SWAPS,
        };
        assert!(!s.conclusive);
    }
}
