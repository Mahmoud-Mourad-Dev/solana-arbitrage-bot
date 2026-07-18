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
use arb_monitor::narrow_report::{aggregate_narrow, parse_narrow_jsonl};
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

    // Detect narrow vs wide: a narrow file starts with the run manifest
    // (post-repair) or contains narrow poll events; peek the first few lines.
    let peek = std::fs::read_to_string(jsonl_path).with_context(|| format!("open {jsonl_path}"))?;
    let is_narrow = peek
        .lines()
        .filter(|l| !l.trim().is_empty())
        .take(3)
        .any(|l| l.contains("\"manifest_version\"") || l.contains("\"profitable_competitive\""));
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
/// exact same aggregator as the live tool. The run manifest (first JSONL line,
/// S13C P7) supplies routes/controls/frozen-secs so NO external flags are
/// needed; flags remain as overrides for pre-manifest files only.
fn rebuild_narrow(body: &str, jsonl_path: &str, out_path: &str, args: &[String]) -> Result<()> {
    let (manifest, events, ok, bad) = parse_narrow_jsonl(body);

    // Manifest first; CLI flags override / backfill for legacy files.
    let mut controls: Vec<String> = manifest
        .as_ref()
        .map(|m| m.control_tokens.clone())
        .unwrap_or_default();
    if let Some(cli) = arg_val(args, "--controls") {
        controls = cli
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
    }
    let frozen_secs: u64 = arg_val(args, "--frozen-secs")
        .and_then(|v| v.parse().ok())
        .or(manifest.as_ref().map(|m| m.frozen_secs))
        .unwrap_or(600);
    let mut token_of: BTreeMap<String, String> = manifest
        .as_ref()
        .map(|m| m.token_of.clone())
        .unwrap_or_default();
    if let Some(p) = arg_val(args, "--routes") {
        if let Some(map) = std::fs::read_to_string(p).ok().and_then(|s| {
            arb_monitor::market_discovery::DiscoveryCache::from_json(&s).map(|c| {
                c.markets
                    .iter()
                    .map(|m| (m.pump_pool.clone(), m.token_mint.clone()))
                    .collect::<BTreeMap<_, _>>()
            })
        }) {
            token_of = map;
        }
    }

    let rpc_failure_events = events.iter().filter(|e| !e.valid_snapshot).count();
    let m = aggregate_narrow(&events, &token_of, &controls, frozen_secs);
    let report = serde_json::json!({
        "run": {
            "commit": git_commit(), "source_jsonl": jsonl_path,
            "events_parsed": ok, "events_skipped_malformed": bad,
            "reconstructed_offline": true, "format": "narrow",
            "manifest": manifest,
            "manifest_used": manifest.is_some(),
            "rpc_failure_events": rpc_failure_events,
            "cost_basis": "modeled competitive — no tx built or simulated",
            "note": "Rebuilt via the same aggregate_narrow used live; headline is CAUSAL. \
                     Config comes from the in-file run manifest; --routes/--controls are \
                     only needed for legacy pre-manifest JSONL.",
        },
        "metrics": m,
    });
    std::fs::write(out_path, serde_json::to_string_pretty(&report)?)?;
    gzip(out_path);
    println!(
        "rebuilt NARROW {ok} events ({bad} skipped, manifest={}) → {out_path}(.gz)\nepisodes={} active_routes={} causal_detect/day={} hindsight_UB/day={}",
        manifest.is_some(),
        m.episodes_total, m.independently_active_routes,
        m.causal_at_detection_per_day_lamports, m.hindsight_upper_bound_per_day_lamports
    );
    Ok(())
}
