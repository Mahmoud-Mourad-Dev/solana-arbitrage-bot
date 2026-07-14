//! `rebuild-report` — reconstruct the wide observe report from a (possibly
//! partial) candidate JSONL, offline. Accepts v1 and v2 JSONL (field aliases).
//! Read-only; no network, no chain access.
//!
//! Usage: cargo run -p arb-monitor --bin rebuild-report -- <candidates.jsonl> [out.json]

use anyhow::{Context, Result};
use arb_monitor::observe_live::{git_commit, gzip};
use arb_monitor::observe_report::{
    aggregate, default_scenarios, sensitivity, wide_verdict, CandidateRecord,
};
use std::collections::BTreeMap;
use std::io::BufRead;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let jsonl_path = args
        .get(1)
        .context("usage: rebuild-report <candidates.jsonl> [out.json]")?;
    let out_path = args
        .get(2)
        .cloned()
        .unwrap_or_else(|| format!("{jsonl_path}.report.json"));

    let file = std::fs::File::open(jsonl_path).with_context(|| format!("open {jsonl_path}"))?;
    let mut records: Vec<CandidateRecord> = Vec::new();
    let (mut ok, mut bad) = (0u64, 0u64);
    for line in std::io::BufReader::new(file).lines() {
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
        "rebuilt {ok} records ({bad} skipped) → {out_path}(.gz)\n{} unique routes, {} episodes, {} confirmed sightings",
        agg.unique_routes, agg.unique_episodes, agg.sightings_confirmed_once
    );
    Ok(())
}
