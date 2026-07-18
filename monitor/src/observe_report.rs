//! S13 observe evidence (schema v2): record schema with explicit units,
//! secret redaction, EPISODE-aware dedup/aggregation, profit distribution,
//! and fee/tip sensitivity analysis.
//!
//! Everything here is PURE and unit-tested. Binaries fill [`CandidateRecord`]s
//! from live scans and stream them to JSONL; [`aggregate`] + [`sensitivity`]
//! produce the final report (also reachable offline via `rebuild-report`).
//!
//! Honesty rules encoded here:
//! - All monetary fields carry a `_lamports` suffix; timings carry `_ms`.
//! - Costs are MODELED estimates (`cost_basis = "modeled"`) — no transaction
//!   was built or simulated to measure them.
//! - Persistence is reported as EPISODES (gap-split runs of sightings), never
//!   as first-to-last span; unique routes / unique episodes / raw sightings
//!   are separate counters.
//! - Nothing here claims an opportunity is executable.

use arb_common::cost::{CostModel, ExecutionPayment};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const LAMPORTS_PER_SOL: f64 = 1_000_000_000.0;

/// Human-readable SOL for summaries (display only — never used in math).
pub fn sol(lamports: i128) -> String {
    format!("{:.6} SOL", lamports as f64 / LAMPORTS_PER_SOL)
}

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

// ───────────────────────── record schema (v2) ─────────────────────────

/// Every cost-model component, in lamports. `cost_basis` is always
/// `"modeled"` in observe mode: these are ASSUMPTIONS, not measurements — no
/// transaction was built or simulated. Field aliases accept v1 JSONL.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostBreakdown {
    #[serde(alias = "signature_fee")]
    pub base_tx_fee_lamports: u64,
    pub compute_unit_limit: u32,
    pub compute_unit_price_micro: u64,
    #[serde(alias = "priority_lamports")]
    pub priority_fee_lamports: u64,
    #[serde(alias = "extra_priority_lamports")]
    pub extra_priority_fee_lamports: u64,
    #[serde(alias = "ata_lamports")]
    pub ata_rent_lamports: u64,
    pub rent_lamports: u64,
    #[serde(alias = "jito_tip")]
    pub jito_tip_lamports: u64,
    #[serde(alias = "margin")]
    pub margin_lamports: u64,
    /// fixed costs + jito tip + margin (everything subtracted from gross).
    #[serde(alias = "total_cost")]
    pub total_cost_lamports: u64,
    /// "modeled" — estimates from config, NOT measured on chain.
    #[serde(default = "modeled")]
    pub cost_basis: String,
}

fn modeled() -> String {
    "modeled".to_string()
}

impl CostBreakdown {
    pub fn from_model(cost: &CostModel, gross: u64) -> Self {
        let tip = cost.payment(gross);
        CostBreakdown {
            base_tx_fee_lamports: cost.signature_fee_lamports,
            compute_unit_limit: cost.compute_unit_limit,
            compute_unit_price_micro: cost.compute_unit_price_micro,
            priority_fee_lamports: cost.compute_priority_lamports(),
            extra_priority_fee_lamports: cost.extra_priority_lamports,
            ata_rent_lamports: cost.ata_lamports,
            rent_lamports: cost.rent_lamports,
            jito_tip_lamports: tip,
            margin_lamports: cost.margin_lamports,
            total_cost_lamports: cost.fixed_costs() + tip + cost.margin_lamports,
            cost_basis: modeled(),
        }
    }
}

/// One confirmation attempt (fresh single-slot re-fetch + re-quote).
/// `delay_ms` is measured from the ORIGINAL detection to this confirmation
/// completing. NOTE: back-to-back confirmations frequently land on the same
/// slot as detection and then only prove internal consistency, not temporal
/// survival — schedule them with a real delay to measure persistence.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Confirmation {
    pub survived: bool,
    pub context_slot: u64,
    #[serde(alias = "net_profit")]
    pub net_profit_lamports: i128,
    #[serde(alias = "gross_profit")]
    pub gross_profit_lamports: u64,
    #[serde(alias = "latency_ms")]
    pub delay_ms: u64,
    /// Whether the reconfirmation snapshot executed successfully. A failed
    /// reconfirm (false) must NEVER read as a zero-profit survivor (S13C P3).
    #[serde(default)]
    pub valid_snapshot: bool,
    /// Typed reason when the reconfirmation could not execute.
    #[serde(default)]
    pub reject_reason: Option<String>,
}

/// A full candidate observation (v2 field names carry units; v1 aliases
/// accepted so `rebuild-report` can read older JSONL).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateRecord {
    pub detected_at_ms: u64,
    pub context_slot: u64,
    pub token_mint: String,
    pub pump_pool: String,
    pub dlmm_pair: String,
    pub direction: String,
    #[serde(alias = "amount_in")]
    pub input_lamports: u64,
    #[serde(alias = "gross_profit")]
    pub gross_profit_lamports: u64,
    #[serde(alias = "pump_fee")]
    pub pump_fee_lamports: u64,
    #[serde(alias = "meteora_fee")]
    pub meteora_fee_lamports: u64,
    pub cost: CostBreakdown,
    #[serde(alias = "net_profit")]
    pub net_profit_lamports: i128,
    pub rpc_latency_ms: u64,
    pub scan_latency_ms: u64,
    /// Detection → record finalized (includes confirmations).
    #[serde(alias = "candidate_age_ms")]
    pub total_candidate_age_ms: u64,
    /// Detection → confirm1 done (None if confirm1 absent).
    #[serde(default)]
    pub confirm1_delay_ms: Option<u64>,
    /// Detection → confirm2 done (None if confirm2 absent).
    #[serde(default)]
    pub confirm2_delay_ms: Option<u64>,
    pub confirm1: Option<Confirmation>,
    pub confirm2: Option<Confirmation>,
}

impl CandidateRecord {
    /// Stable identity of the underlying opportunity (for dedup / episodes).
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

// ───────────────────────── episodes ─────────────────────────

/// A run of consecutive sightings of one route, split when the gap between
/// sightings exceeds `gap_ms`. THIS is the honest persistence unit: a route
/// seen at 09:00 and again at 20:00 is two episodes, not an 11-hour edge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Episode {
    pub route_key: String,
    pub start_ms: u64,
    pub end_ms: u64,
    pub sightings: usize,
    pub max_gross_lamports: u64,
    pub distinct_gross_values: usize,
}

impl Episode {
    pub fn duration_ms(&self) -> u64 {
        self.end_ms - self.start_ms
    }
}

/// Split each route's (time-sorted) sightings into episodes.
pub fn episodes(records: &[CandidateRecord], gap_ms: u64) -> Vec<Episode> {
    let mut by_route: BTreeMap<String, Vec<&CandidateRecord>> = BTreeMap::new();
    for r in records {
        by_route.entry(r.opportunity_key()).or_default().push(r);
    }
    let mut out = Vec::new();
    for (key, mut v) in by_route {
        v.sort_by_key(|r| r.detected_at_ms);
        let mut cur: Vec<&CandidateRecord> = vec![v[0]];
        for r in v.into_iter().skip(1) {
            if r.detected_at_ms - cur.last().unwrap().detected_at_ms > gap_ms {
                out.push(mk_episode(&key, &cur));
                cur = vec![r];
            } else {
                cur.push(r);
            }
        }
        out.push(mk_episode(&key, &cur));
    }
    out
}

fn mk_episode(key: &str, sightings: &[&CandidateRecord]) -> Episode {
    let grosses: Vec<u64> = sightings.iter().map(|r| r.gross_profit_lamports).collect();
    let mut distinct = grosses.clone();
    distinct.sort_unstable();
    distinct.dedup();
    Episode {
        route_key: key.to_string(),
        start_ms: sightings[0].detected_at_ms,
        end_ms: sightings.last().unwrap().detected_at_ms,
        sightings: sightings.len(),
        max_gross_lamports: grosses.iter().copied().max().unwrap_or(0),
        distinct_gross_values: distinct.len(),
    }
}

// ───────────────────────── aggregation ─────────────────────────

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Aggregate {
    /// Raw JSONL records (repeated sightings of the same route count each).
    pub raw_sightings: usize,
    /// Distinct (pump, dlmm, direction) routes.
    pub unique_routes: usize,
    /// Gap-split episodes across all routes — the honest persistence unit.
    pub unique_episodes: usize,
    /// Sightings whose confirm1/confirm2 survived (NOT unique routes).
    pub sightings_confirmed_once: usize,
    pub sightings_confirmed_twice: usize,
    /// Routes with at least one confirmed sighting.
    pub routes_with_confirmation: usize,
    /// Episode duration stats (ms). Single-sighting episodes have duration 0.
    pub episode_duration_p50_ms: u64,
    pub episode_duration_p90_ms: u64,
    pub episode_duration_max_ms: u64,
    pub single_sighting_episodes: usize,
    /// Net-profit percentiles (lamports) over confirmed SIGHTINGS.
    pub net_p50_lamports: i128,
    pub net_p90_lamports: i128,
    pub net_max_lamports: i128,
    pub by_direction: BTreeMap<String, usize>,
    pub reject_totals: BTreeMap<String, u64>,
    pub scan_cycles: usize,
    pub scan_median_secs: f64,
    pub scan_max_secs: f64,
}

fn percentile_i128(sorted: &[i128], p: f64) -> i128 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn percentile_u64(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn median_f64(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

/// Gap used to split sightings into episodes: 2 nominal scan cycles.
pub const EPISODE_GAP_MS: u64 = 182_000;

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
    let eps = episodes(records, EPISODE_GAP_MS);
    let mut durs: Vec<u64> = eps.iter().map(|e| e.duration_ms()).collect();
    durs.sort_unstable();

    let mut confirmed_nets: Vec<i128> = Vec::new();
    let (mut c1, mut c2) = (0usize, 0usize);
    let mut confirmed_routes: std::collections::BTreeSet<String> = Default::default();
    for r in records {
        if r.confirmed_once() {
            c1 += 1;
            confirmed_nets.push(r.confirm1.as_ref().unwrap().net_profit_lamports);
            confirmed_routes.insert(r.opportunity_key());
        }
        if r.confirmed_twice() {
            c2 += 1;
        }
    }
    confirmed_nets.sort_unstable();
    let unique: std::collections::BTreeSet<String> =
        records.iter().map(|r| r.opportunity_key()).collect();

    Aggregate {
        raw_sightings: records.len(),
        unique_routes: unique.len(),
        unique_episodes: eps.len(),
        sightings_confirmed_once: c1,
        sightings_confirmed_twice: c2,
        routes_with_confirmation: confirmed_routes.len(),
        episode_duration_p50_ms: percentile_u64(&durs, 0.50),
        episode_duration_p90_ms: percentile_u64(&durs, 0.90),
        episode_duration_max_ms: durs.last().copied().unwrap_or(0),
        single_sighting_episodes: eps.iter().filter(|e| e.sightings == 1).count(),
        net_p50_lamports: percentile_i128(&confirmed_nets, 0.50),
        net_p90_lamports: percentile_i128(&confirmed_nets, 0.90),
        net_max_lamports: confirmed_nets.last().copied().unwrap_or(0),
        by_direction: by_dir,
        reject_totals,
        scan_cycles: scan_secs.len(),
        scan_median_secs: median_f64(scan_secs.to_vec()),
        scan_max_secs: scan_secs.iter().copied().fold(0.0_f64, f64::max),
    }
}

/// Honest verdict for the WIDE scanner. Never says "promising" off raw
/// sighting counts; states the measurement limits explicitly.
pub fn wide_verdict(agg: &Aggregate) -> String {
    if agg.sightings_confirmed_once == 0 {
        return "NO SIGNAL — zero sightings survived fresh single-slot confirmation.".into();
    }
    let cadence = if agg.scan_median_secs > 0.0 {
        format!("a wide {}s-cycle scan", agg.scan_median_secs.round() as u64)
    } else {
        "the wide scan".to_string()
    };
    format!(
        "{} unique routes / {} episodes from {} raw sightings. {} cannot measure \
         sub-cycle edge survival, and same-instant confirmations mostly prove internal \
         consistency only. Use the narrow fast-poll experiment for executability \
         evidence; costs here are modeled, not measured.",
        agg.unique_routes, agg.unique_episodes, agg.raw_sightings, cadence
    )
}

// ───────────────────────── sensitivity ─────────────────────────

/// A realistic cost scenario (priority fee + Jito tip assumptions).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeeScenario {
    pub label: String,
    pub cu_limit: u32,
    pub cu_price_micro: u64,
    pub extra_priority_lamports: u64,
    pub jito_min_lamports: u64,
    pub jito_max_lamports: u64,
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
                min_lamports: self.jito_min_lamports,
                max_lamports: self.jito_max_lamports,
            },
            ..Default::default()
        }
    }
}

/// Optimistic → hot ladder. "competitive" is the reference realistic case.
pub fn default_scenarios() -> Vec<FeeScenario> {
    let base = |label: &str, cu_price: u64, extra: u64, jmin: u64, jmax: u64| FeeScenario {
        label: label.to_string(),
        cu_limit: 600_000,
        cu_price_micro: cu_price,
        extra_priority_lamports: extra,
        jito_min_lamports: jmin,
        jito_max_lamports: jmax,
        margin_lamports: 10_000,
    };
    vec![
        base("optimistic", 10_000, 0, 10_000, 100_000_000),
        base("typical", 50_000, 50_000, 100_000, 100_000_000),
        base("competitive", 200_000, 200_000, 1_000_000, 500_000_000),
        base("hot", 1_000_000, 1_000_000, 5_000_000, 1_000_000_000),
    ]
}

/// The competitive reference model (used by the narrow experiment's episode
/// definition).
pub fn competitive_model() -> CostModel {
    default_scenarios()
        .into_iter()
        .find(|s| s.label == "competitive")
        .unwrap()
        .model()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioResult {
    pub label: String,
    /// UNIQUE ROUTES whose median confirmed gross still nets ≥ 0.
    pub unique_route_survivors: usize,
    /// Raw confirmed sightings netting ≥ 0 (inflated by repeats; shown for
    /// comparison only).
    pub sighting_survivors: usize,
    pub net_p50_lamports: i128,
    pub net_max_lamports: i128,
}

/// Re-cost confirmed records under each scenario. Route-level numbers use the
/// MEDIAN confirmed gross per route (dedup — one frozen route repeated 949×
/// must count once).
pub fn sensitivity(records: &[CandidateRecord], scenarios: &[FeeScenario]) -> Vec<ScenarioResult> {
    let mut per_route: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut sighting_grosses: Vec<u64> = Vec::new();
    for r in records {
        if r.confirmed_once() {
            let g = r.confirm1.as_ref().unwrap().gross_profit_lamports;
            per_route.entry(r.opportunity_key()).or_default().push(g);
            sighting_grosses.push(g);
        }
    }
    let route_medians: Vec<u64> = per_route
        .values()
        .map(|v| {
            let mut v = v.clone();
            v.sort_unstable();
            v[v.len() / 2]
        })
        .collect();
    scenarios
        .iter()
        .map(|sc| {
            let model = sc.model();
            let mut route_nets: Vec<i128> = route_medians.iter().map(|&g| model.net(g)).collect();
            route_nets.sort_unstable();
            ScenarioResult {
                label: sc.label.clone(),
                unique_route_survivors: route_nets.iter().filter(|&&n| n >= 0).count(),
                sighting_survivors: sighting_grosses
                    .iter()
                    .filter(|&&g| model.net(g) >= 0)
                    .count(),
                net_p50_lamports: percentile_i128(&route_nets, 0.50),
                net_max_lamports: route_nets.last().copied().unwrap_or(0),
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
        assert_eq!(
            redact_secrets("token api_key=abc done", &[]),
            "token <redacted-url> done"
        );
    }

    fn rec(pool: &str, detected: u64, gross: u64, c1: bool, c2: bool) -> CandidateRecord {
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
            pump_pool: pool.into(),
            dlmm_pair: "pair".into(),
            direction: "meteora->pump".into(),
            input_lamports: 1_000_000_000,
            gross_profit_lamports: gross,
            pump_fee_lamports: 1000,
            meteora_fee_lamports: 2000,
            cost: CostBreakdown::from_model(&cost, gross),
            net_profit_lamports: cost.net(gross),
            rpc_latency_ms: 40,
            scan_latency_ms: 80_000,
            total_candidate_age_ms: 200,
            confirm1_delay_ms: c1.then_some(120),
            confirm2_delay_ms: c2.then_some(240),
            confirm1: c1.then(|| Confirmation {
                survived: true,
                context_slot: 101,
                net_profit_lamports: cost.net(gross),
                gross_profit_lamports: gross,
                delay_ms: 120,
                valid_snapshot: true,
                reject_reason: None,
            }),
            confirm2: c2.then(|| Confirmation {
                survived: true,
                context_slot: 102,
                net_profit_lamports: cost.net(gross),
                gross_profit_lamports: gross,
                delay_ms: 240,
                valid_snapshot: true,
                reject_reason: None,
            }),
        }
    }

    #[test]
    fn v1_jsonl_field_names_still_parse() {
        // A v1-era record (pre-units rename) must deserialize via aliases so
        // rebuild-report can read old VPS logs.
        let v1 = r#"{"detected_at_ms":1,"context_slot":2,"token_mint":"t",
            "pump_pool":"p","dlmm_pair":"d","direction":"meteora->pump",
            "amount_in":100,"gross_profit":50,"pump_fee":1,"meteora_fee":2,
            "cost":{"signature_fee":5000,"compute_unit_limit":600000,
              "compute_unit_price_micro":10000,"priority_lamports":6000,
              "extra_priority_lamports":0,"ata_lamports":0,"rent_lamports":0,
              "jito_tip":10,"margin":5,"total_cost":11015},
            "net_profit":39,"rpc_latency_ms":9,"scan_latency_ms":10,
            "candidate_age_ms":11,
            "confirm1":{"survived":true,"context_slot":3,"net_profit":39,
              "gross_profit":50,"latency_ms":100},
            "confirm2":null}"#;
        let r: CandidateRecord = serde_json::from_str(v1).unwrap();
        assert_eq!(r.input_lamports, 100);
        assert_eq!(r.gross_profit_lamports, 50);
        assert_eq!(r.cost.base_tx_fee_lamports, 5000);
        assert_eq!(r.cost.jito_tip_lamports, 10);
        assert_eq!(r.cost.cost_basis, "modeled");
        assert_eq!(r.total_candidate_age_ms, 11);
        assert_eq!(r.confirm1.as_ref().unwrap().delay_ms, 100);
        assert!(r.confirmed_once() && !r.confirmed_twice());
    }

    #[test]
    fn episodes_split_on_gaps_not_first_to_last() {
        // Route A: sightings at 0, 60s, 120s (one episode), then 8h later two
        // more (second episode). First-to-last would claim 8h persistence; the
        // episode view must say two episodes of 120s and 30s.
        let h8 = 8 * 3600 * 1000u64;
        let recs = vec![
            rec("A", 0, 100, true, false),
            rec("A", 60_000, 100, true, false),
            rec("A", 120_000, 100, true, false),
            rec("A", h8, 100, true, false),
            rec("A", h8 + 30_000, 100, true, false),
        ];
        let eps = episodes(&recs, EPISODE_GAP_MS);
        assert_eq!(eps.len(), 2);
        assert_eq!(eps[0].duration_ms(), 120_000);
        assert_eq!(eps[1].duration_ms(), 30_000);
        let agg = aggregate(&recs, &[76.0], BTreeMap::new());
        assert_eq!(agg.unique_routes, 1);
        assert_eq!(agg.unique_episodes, 2);
        assert_eq!(agg.raw_sightings, 5);
        assert_eq!(agg.episode_duration_max_ms, 120_000);
        // No first-to-last field exists any more; the verdict must not claim
        // persistence.
        let v = wide_verdict(&agg);
        assert!(v.contains("cannot measure"), "{v}");
    }

    #[test]
    fn sensitivity_dedups_routes_and_shrinks_with_costs() {
        // Frozen-style route A repeated 100×, live route B once. Route-level
        // survivor counts must treat A as ONE route.
        let mut recs: Vec<CandidateRecord> = (0..100)
            .map(|i| rec("A", i * 91_000, 5_000_000, true, false))
            .collect();
        recs.push(rec("B", 0, 800_000, true, false));
        let res = sensitivity(&recs, &default_scenarios());
        let opt = res.iter().find(|r| r.label == "optimistic").unwrap();
        let hot = res.iter().find(|r| r.label == "hot").unwrap();
        assert_eq!(opt.unique_route_survivors, 2);
        assert_eq!(opt.sighting_survivors, 101); // inflated view, kept for contrast
        assert!(hot.unique_route_survivors <= opt.unique_route_survivors);
        assert_eq!(hot.unique_route_survivors, 0); // both under hot costs
    }

    #[test]
    fn aggregate_counts_confirmed_sightings_and_routes_separately() {
        let recs = vec![
            rec("A", 0, 5_000_000, true, true),
            rec("A", 91_000, 5_000_000, true, false),
            rec("B", 10_000, 3_000_000, false, false),
        ];
        let agg = aggregate(&recs, &[80.0, 82.0], BTreeMap::new());
        assert_eq!(agg.sightings_confirmed_once, 2);
        assert_eq!(agg.sightings_confirmed_twice, 1);
        assert_eq!(agg.routes_with_confirmation, 1); // only A
        assert_eq!(agg.unique_routes, 2);
        assert_eq!(agg.scan_cycles, 2);
    }

    #[test]
    fn sol_formatting_is_display_only() {
        assert_eq!(sol(1_500_000_000), "1.500000 SOL");
        assert_eq!(sol(-500_000), "-0.000500 SOL");
    }
}
