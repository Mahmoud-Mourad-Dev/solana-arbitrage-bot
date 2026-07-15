//! `rebuild-report` — reconstruct an observe report from a (possibly partial)
//! JSONL, offline. Auto-detects format:
//!
//! - WIDE candidate JSONL (`CandidateRecord`, v1/v2 aliases), OR
//! - NARROW poll JSONL (`narrow_report::PollEvent`, poll + reconfirm events).
//!
//! Uses the SAME aggregators as the live tools, so metrics match exactly.
//! Read-only; no network, no chain access.
//!
//! Usage: cargo run -p arb-monitor --bin rebuild-report -- <jsonl> [out.json]
//!        [--controls tok1,tok2] [--frozen-secs 600] [--routes narrow-routes.json]

use anyhow::{Context, Result};
use arb_monitor::narrow_report::{aggregate_narrow, PollEvent};
use arb_monitor::observe_live::{git_commit, gzip};
use arb_monitor::observe_report::{
    aggregate, default_scenarios, sensitivity, wide_verdict, CandidateRecord,
};
use std::collections::BTreeMap;
use std::io::BufRead;

fn arg_val(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1).cloned())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let jsonl_path = args
        .get(1)
        .filter(|s| !s.starts_with("--"))
        .context("usage: rebuild-report <jsonl> [out.json] [--controls ..] [--routes ..]")?;
    let out_path = args
        .get(2)
        .filter(|s| !s.starts_with("--"))
        .cloned()
        .unwrap_or_else(|| format!("{jsonl_path}.report.json"));

    // Peek the first non-empty line to detect narrow vs wide.
    let peek = std::fs::read_to_string(jsonl_path).with_context(|| format!("open {jsonl_path}"))?;
    let first = peek.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let is_narrow = first.contains("\"profitable_competitive\"");
    if is_narrow {
        return rebuild_narrow(&peek, jsonl_path, &out_path, &args);
    }

    let mut records: Vec<CandidateRecord> = Vec::new();
    let (mut ok, mut bad) = (0u64, 0u64);
    for line in std::io::BufReader::new(peek.as_bytes()).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<CandidateRecord>(&line) {
            Ok(r) => {
                records.push(r);
                ok += 1;
            }
            // A partial JSONL may end mid-line after a crash — skip, don't fail.
            Err(_) => bad += 1,
        }
    }

    // Scan durations aren't in the JSONL; estimate cycle count from distinct
    // detection-time buckets is unreliable, so we report cycles as unknown and
    // pass an empty scan vector (persistence/episodes don't need it).
    let reject_totals: BTreeMap<String, u64> = BTreeMap::new();
    let agg = aggregate(&records, &[], reject_totals);
    let sens = sensitivity(&records, &default_scenarios());

    let report = serde_json::json!({
        "run": {
            "commit": git_commit(),
            "source_jsonl": jsonl_path,
            "records_parsed": ok,
            "records_skipped_malformed": bad,
            "reconstructed_offline": true,
            "cost_basis": "modeled — no transaction was built or simulated",
            "note": "Rebuilt from JSONL; scan-cycle durations and reject totals are \
                     not in the log, so throughput fields are omitted. Episodes, \
                     unique routes, confirmation survivors and sensitivity are exact.",
        },
        "aggregate": agg,
        "sensitivity": sens,
        "verdict": wide_verdict(&agg),
    });
    std::fs::write(&out_path, serde_json::to_string_pretty(&report)?)?;
    gzip(&out_path);
    println!(
        "rebuilt WIDE {ok} records ({bad} skipped) → {out_path}(.gz)\n{} unique routes, {} episodes, {} confirmed sightings",
        agg.unique_routes, agg.unique_episodes, agg.sightings_confirmed_once
    );
    Ok(())
}

/// Rebuild the corrected NARROW report from a poll+reconfirm JSONL, using the
/// exact same aggregator as the live tool.
fn rebuild_narrow(body: &str, jsonl_path: &str, out_path: &str, args: &[String]) -> Result<()> {
    let controls: Vec<String> = arg_val(args, "--controls")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let frozen_secs: u64 = arg_val(args, "--frozen-secs")
        .and_then(|v| v.parse().ok())
        .unwrap_or(600);
    // token_of map from an optional routes cache (pump_pool → token).
    let token_of: BTreeMap<String, String> = arg_val(args, "--routes")
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| {
            arb_monitor::market_discovery::DiscoveryCache::from_json(&s).map(|c| {
                c.markets
                    .iter()
                    .map(|m| (m.pump_pool.clone(), m.token_mint.clone()))
                    .collect()
            })
        })
        .unwrap_or_default();

    let mut events: Vec<PollEvent> = Vec::new();
    let (mut ok, mut bad) = (0u64, 0u64);
    for line in body.lines() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<PollEvent>(line) {
            Ok(e) => {
                events.push(e);
                ok += 1;
            }
            Err(_) => bad += 1, // tolerate a truncated last line
        }
    }
    let m = aggregate_narrow(&events, &token_of, &controls, frozen_secs);
    let report = serde_json::json!({
        "run": {
            "commit": git_commit(), "source_jsonl": jsonl_path,
            "events_parsed": ok, "events_skipped_malformed": bad,
            "reconstructed_offline": true, "format": "narrow",
            "cost_basis": "modeled competitive — no tx built or simulated",
            "note": "Rebuilt via the same aggregate_narrow used live; headline is CAUSAL. \
                     Pass --routes narrow-routes.json to resolve token mints, --controls to \
                     exclude frozen controls.",
        },
        "metrics": m,
    });
    std::fs::write(out_path, serde_json::to_string_pretty(&report)?)?;
    gzip(out_path);
    println!(
        "rebuilt NARROW {ok} events ({bad} skipped) → {out_path}(.gz)\nepisodes={} active_routes={} causal_detect/day={} hindsight_UB/day={}",
        m.episodes_total, m.independently_active_routes,
        m.causal_at_detection_per_day_lamports, m.hindsight_upper_bound_per_day_lamports
    );
    Ok(())
}
