//! S13 observe evidence: record schema, secret redaction, dedup/aggregation,
//! persistence stats, profit distribution, and fee/tip sensitivity analysis.
//!
//! Everything here is PURE and unit-tested. The `observe-markets` binary fills
//! [`CandidateRecord`]s from live scans (with single-slot confirmation) and
//! streams them to JSONL; at the end it calls [`aggregate`] and
//! [`sensitivity`] to produce the final report.
//!
//! Nothing here claims an opportunity is executable. A record is a MONITOR
//! signal; the acceptance question is whether such signals SURVIVE fresh
//! single-slot confirmation and PERSIST across cycles.

use arb_common::cost::{CostModel, ExecutionPayment};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ───────────────────────── secret redaction ─────────────────────────

/// Scrub RPC endpoints, API keys, and query strings from a string before it is
/// logged or written to a report. Replaces any known secret verbatim, then
/// drops any whitespace token that looks like a URL or carries a key.
pub fn redact_secrets(input: &str, secrets: &[&str]) -> String {
    let mut s = input.to_string();
    for secret in secrets {
        if !secret.is_empty() {
            s = s.replace(secret, "<redacted>");
        }
    }
    s.split_whitespace()
        .map(|tok| {
            let low = tok.to_ascii_lowercase();
            if low.contains("://")
                || low.contains("api-key")
                || low.contains("api_key")
                || low.contains("?api")
                || low.contains("access-token")
            {
                "<redacted-url>"
            } else {
                tok
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ───────────────────────── record schema ─────────────────────────

/// Every cost-model component, in lamports, recorded per candidate so the
/// report is fully reproducible and re-costable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostBreakdown {
    pub signature_fee: u64,
    pub compute_unit_limit: u32,
    pub compute_unit_price_micro: u64,
    pub priority_lamports: u64,
    pub extra_priority_lamports: u64,
    pub ata_lamports: u64,
    pub rent_lamports: u64,
    pub jito_tip: u64,
    pub margin: u64,
    /// fixed_costs + jito_tip + margin (everything subtracted from gross).
    pub total_cost: u64,
}

impl CostBreakdown {
    pub fn from_model(cost: &CostModel, gross: u64) -> Self {
        let priority = cost.compute_priority_lamports();
        let tip = cost.payment(gross);
        CostBreakdown {
            signature_fee: cost.signature_fee_lamports,
            compute_unit_limit: cost.compute_unit_limit,
            compute_unit_price_micro: cost.compute_unit_price_micro,
            priority_lamports: priority,
            extra_priority_lamports: cost.extra_priority_lamports,
            ata_lamports: cost.ata_lamports,
            rent_lamports: cost.rent_lamports,
            jito_tip: tip,
            margin: cost.margin_lamports,
            total_cost: cost.fixed_costs() + tip + cost.margin_lamports,
        }
    }
}

/// One immediate confirmation attempt (fresh single-slot re-fetch + re-quote).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Confirmation {
    pub survived: bool,
    pub context_slot: u64,
    pub net_profit: i128,
    pub gross_profit: u64,
    /// ms from the original detection to this confirmation completing.
    pub latency_ms: u64,
}

/// A full candidate observation. `direction` is "pump->meteora" or
/// "meteora->pump". Fees are DEX fees on each venue (in that leg's fee token;
/// for meteora->pump both are WSOL). All economic figures are integer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateRecord {
    pub detected_at_ms: u64,
    pub context_slot: u64,
    pub token_mint: String,
    pub pump_pool: String,
    pub dlmm_pair: String,
    pub direction: String,
    pub amount_in: u64,
    pub gross_profit: u64,
    pub pump_fee: u64,
    pub meteora_fee: u64,
    pub cost: CostBreakdown,
    pub net_profit: i128,
    pub rpc_latency_ms: u64,
    pub scan_latency_ms: u64,
    /// ms from detection to when this record was finalized (post-confirmation).
    pub candidate_age_ms: u64,
    pub confirm1: Option<Confirmation>,
    pub confirm2: Option<Confirmation>,
}

impl CandidateRecord {
    /// Stable identity of the underlying opportunity (for dedup / persistence).
    pub fn opportunity_key(&self) -> String {
        format!("{}|{}|{}", self.pump_pool, self.dlmm_pair, self.direction)
    }
    pub fn confirmed_once(&self) -> bool {
        self.confirm1.as_ref().map(|c| c.survived).unwrap_or(false)
    }
    pub fn confirmed_twice(&self) -> bool {
        self.confirmed_once() && self.confirm2.as_ref().map(|c| c.survived).unwrap_or(false)
    }
}

// ───────────────────────── aggregation ─────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Aggregate {
    pub raw_signals: usize,
    pub unique_opportunities: usize,
    pub single_confirm_survivors: usize,
    pub double_confirm_survivors: usize,
    /// Over opportunities that were confirmed at least once: how long (ms) the
    /// SAME opportunity kept re-appearing as a survivor across cycles.
    pub persistence_median_ms: u64,
    pub persistence_max_ms: u64,
    /// Net-profit percentiles (lamports) over CONFIRMED records.
    pub net_p50: i128,
    pub net_p90: i128,
    pub net_max: i128,
    pub by_direction: BTreeMap<String, usize>,
    pub reject_totals: BTreeMap<String, u64>,
    /// Throughput.
    pub scan_cycles: usize,
    pub scan_median_secs: f64,
    pub scan_max_secs: f64,
    /// The core verdict input: does the median confirmed opportunity persist at
    /// least one scan cycle (so a next-cycle actor could plausibly catch it)?
    pub persistence_exceeds_cycle: bool,
}

fn percentile_i128(sorted: &[i128], p: f64) -> i128 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn median_u64(mut v: Vec<u64>) -> u64 {
    if v.is_empty() {
        return 0;
    }
    v.sort_unstable();
    v[v.len() / 2]
}

fn median_f64(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

/// Aggregate a run's records + per-cycle scan durations + reject totals.
pub fn aggregate(
    records: &[CandidateRecord],
    scan_secs: &[f64],
    reject_totals: BTreeMap<String, u64>,
) -> Aggregate {
    let mut by_dir: BTreeMap<String, usize> = BTreeMap::new();
    for r in records {
        *by_dir.entry(r.direction.clone()).or_default() += 1;
    }

    // Per-opportunity survivor timestamps (for persistence).
    let mut survivor_times: BTreeMap<String, (u64, u64)> = BTreeMap::new(); // key -> (min,max)
    let mut confirmed_nets: Vec<i128> = Vec::new();
    let (mut c1, mut c2) = (0usize, 0usize);
    for r in records {
        if r.confirmed_once() {
            c1 += 1;
            confirmed_nets.push(r.confirm1.as_ref().unwrap().net_profit);
            let e = survivor_times
                .entry(r.opportunity_key())
                .or_insert((r.detected_at_ms, r.detected_at_ms));
            e.0 = e.0.min(r.detected_at_ms);
            e.1 = e.1.max(r.detected_at_ms);
        }
        if r.confirmed_twice() {
            c2 += 1;
        }
    }
    let persistences: Vec<u64> = survivor_times.values().map(|(lo, hi)| hi - lo).collect();
    let persistence_median = median_u64(persistences.clone());
    let persistence_max = persistences.iter().copied().max().unwrap_or(0);

    confirmed_nets.sort_unstable();
    let scan_median = median_f64(scan_secs.to_vec());
    let scan_max = scan_secs.iter().copied().fold(0.0_f64, f64::max);

    let unique: std::collections::BTreeSet<String> =
        records.iter().map(|r| r.opportunity_key()).collect();

    Aggregate {
        raw_signals: records.len(),
        unique_opportunities: unique.len(),
        single_confirm_survivors: c1,
        double_confirm_survivors: c2,
        persistence_median_ms: persistence_median,
        persistence_max_ms: persistence_max,
        net_p50: percentile_i128(&confirmed_nets, 0.50),
        net_p90: percentile_i128(&confirmed_nets, 0.90),
        net_max: confirmed_nets.last().copied().unwrap_or(0),
        by_direction: by_dir,
        reject_totals,
        scan_cycles: scan_secs.len(),
        scan_median_secs: scan_median,
        scan_max_secs: scan_max,
        persistence_exceeds_cycle: persistence_median as f64 / 1000.0 >= scan_median,
    }
}

// ───────────────────────── sensitivity ─────────────────────────

/// A realistic cost scenario (priority fee + Jito tip assumptions), replacing a
/// single optimistic value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeeScenario {
    pub label: String,
    pub cu_limit: u32,
    pub cu_price_micro: u64,
    pub extra_priority_lamports: u64,
    pub jito_min: u64,
    pub jito_max: u64,
    pub margin_lamports: u64,
}

impl FeeScenario {
    pub fn model(&self) -> CostModel {
        CostModel {
            signature_fee_lamports: 5_000,
            compute_unit_limit: self.cu_limit,
            compute_unit_price_micro: self.cu_price_micro,
            extra_priority_lamports: self.extra_priority_lamports,
            margin_lamports: self.margin_lamports,
            required_net_lamports: 0,
            payment: ExecutionPayment::JitoTip {
                min_lamports: self.jito_min,
                max_lamports: self.jito_max,
            },
            ..Default::default()
        }
    }
}

/// A realistic default ladder from optimistic to competitive.
pub fn default_scenarios() -> Vec<FeeScenario> {
    let base = |label: &str, cu_price: u64, extra: u64, jmin: u64, jmax: u64| FeeScenario {
        label: label.to_string(),
        cu_limit: 600_000,
        cu_price_micro: cu_price,
        extra_priority_lamports: extra,
        jito_min: jmin,
        jito_max: jmax,
        margin_lamports: 10_000,
    };
    vec![
        base("optimistic", 10_000, 0, 10_000, 100_000_000),
        base("typical", 50_000, 50_000, 100_000, 100_000_000),
        base("competitive", 200_000, 200_000, 1_000_000, 500_000_000),
        base("hot", 1_000_000, 1_000_000, 5_000_000, 1_000_000_000),
    ]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioResult {
    pub label: String,
    /// Confirmed records that STILL net ≥ 0 under this scenario.
    pub survivors: usize,
    pub net_p50: i128,
    pub net_max: i128,
}

/// Re-cost every once-confirmed record under each scenario using its recorded
/// gross profit (the DEX-level edge is fixed; only our costs change).
pub fn sensitivity(records: &[CandidateRecord], scenarios: &[FeeScenario]) -> Vec<ScenarioResult> {
    let grosses: Vec<u64> = records
        .iter()
        .filter(|r| r.confirmed_once())
        .map(|r| r.confirm1.as_ref().unwrap().gross_profit)
        .collect();
    scenarios
        .iter()
        .map(|sc| {
            let model = sc.model();
            let mut nets: Vec<i128> = grosses.iter().map(|&g| model.net(g)).collect();
            let survivors = nets.iter().filter(|&&n| n >= 0).count();
            nets.sort_unstable();
            ScenarioResult {
                label: sc.label.clone(),
                survivors,
                net_p50: percentile_i128(&nets, 0.50),
                net_max: nets.last().copied().unwrap_or(0),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_scrubs_urls_keys_and_known_secrets() {
        let rpc = "https://x.solana.quiknode.pro/deadbeef/";
        let e = format!("error sending request for url ({rpc}?api-key=SECRET): timeout");
        let out = redact_secrets(&e, &[rpc, "wss://x/"]);
        assert!(!out.contains("quiknode"), "{out}");
        assert!(!out.contains("SECRET"), "{out}");
        assert!(!out.contains("deadbeef"), "{out}");
        assert!(out.contains("error sending request"));
        // Bare api-key token with no scheme is still scrubbed.
        assert_eq!(
            redact_secrets("token api_key=abc done", &[]),
            "token <redacted-url> done"
        );
    }

    fn rec(
        key_pool: &str,
        dir: &str,
        detected: u64,
        gross: u64,
        c1: bool,
        c2: bool,
    ) -> CandidateRecord {
        let cost = CostModel {
            signature_fee_lamports: 5_000,
            payment: ExecutionPayment::JitoTip {
                min_lamports: 10_000,
                max_lamports: 100_000_000,
            },
            ..Default::default()
        };
        CandidateRecord {
            detected_at_ms: detected,
            context_slot: 100,
            token_mint: "tok".into(),
            pump_pool: key_pool.into(),
            dlmm_pair: "pair".into(),
            direction: dir.into(),
            amount_in: 1_000_000_000,
            gross_profit: gross,
            pump_fee: 1000,
            meteora_fee: 2000,
            cost: CostBreakdown::from_model(&cost, gross),
            net_profit: cost.net(gross),
            rpc_latency_ms: 40,
            scan_latency_ms: 80_000,
            candidate_age_ms: 200,
            confirm1: c1.then(|| Confirmation {
                survived: true,
                context_slot: 101,
                net_profit: cost.net(gross),
                gross_profit: gross,
                latency_ms: 120,
            }),
            confirm2: c2.then(|| Confirmation {
                survived: true,
                context_slot: 102,
                net_profit: cost.net(gross),
                gross_profit: gross,
                latency_ms: 240,
            }),
        }
    }

    #[test]
    fn aggregate_counts_dedup_survivors_and_persistence() {
        // Same opportunity (pool A) seen at t=0 and t=90_000, both confirmed →
        // one unique opportunity, persistence 90s. Pool B once, not confirmed.
        let recs = vec![
            rec("A", "meteora->pump", 0, 5_000_000, true, true),
            rec("A", "meteora->pump", 90_000, 5_000_000, true, false),
            rec("B", "meteora->pump", 10_000, 3_000_000, false, false),
        ];
        let mut rejects = BTreeMap::new();
        rejects.insert("leg1".to_string(), 230u64);
        let agg = aggregate(&recs, &[80.0, 82.0, 78.0], rejects);
        assert_eq!(agg.raw_signals, 3);
        assert_eq!(agg.unique_opportunities, 2); // A, B
        assert_eq!(agg.single_confirm_survivors, 2); // two A records
        assert_eq!(agg.double_confirm_survivors, 1);
        assert_eq!(agg.persistence_max_ms, 90_000);
        assert_eq!(agg.persistence_median_ms, 90_000);
        assert_eq!(agg.by_direction.get("meteora->pump"), Some(&3));
        assert_eq!(agg.reject_totals.get("leg1"), Some(&230));
        assert_eq!(agg.scan_cycles, 3);
        // 90s persistence ≥ ~80s median cycle.
        assert!(agg.persistence_exceeds_cycle);
    }

    #[test]
    fn short_lived_signal_does_not_exceed_cycle() {
        // A confirmed opportunity seen only once ⇒ persistence 0 < cycle.
        let recs = vec![rec("A", "meteora->pump", 0, 5_000_000, true, false)];
        let agg = aggregate(&recs, &[80.0], BTreeMap::new());
        assert_eq!(agg.persistence_median_ms, 0);
        assert!(!agg.persistence_exceeds_cycle);
    }

    #[test]
    fn sensitivity_shrinks_survivors_as_costs_rise() {
        // Gross ~ 0.005 SOL: profitable when optimistic, wiped out when hot.
        let recs = vec![
            rec("A", "meteora->pump", 0, 5_000_000, true, false),
            rec("B", "meteora->pump", 1, 800_000, true, false),
        ];
        let res = sensitivity(&recs, &default_scenarios());
        let opt = res.iter().find(|r| r.label == "optimistic").unwrap();
        let hot = res.iter().find(|r| r.label == "hot").unwrap();
        assert!(
            opt.survivors >= hot.survivors,
            "costs rising must not add survivors"
        );
        // Under 'hot' (≥0.005 SOL tip floor), a 0.0008 SOL gross can't survive.
        assert!(hot.survivors < 2);
    }
}
