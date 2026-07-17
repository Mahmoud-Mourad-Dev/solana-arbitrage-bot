//! Offline economics repricing under the corrected Pump fee-v2 model
//! (S13C slice 6B, step 7). PURE offline: reads observe/narrow JSONL, applies
//! the measured-current-rate fee sensitivity to the Pump SELL leg, and prints
//! corrected competitive-positive counts and capturable value. No RPC, no chain.
//!
//! Usage: `reprice-pump-fees <polls.jsonl> [more.jsonl ...]`

use anyhow::{bail, Context, Result};
use arb_monitor::pump_reprice::{reprice_poll, summarize, RepriceClass, LEGACY_FEE_BPS};
use serde::Deserialize;
use std::io::{BufRead, BufReader};

#[derive(Debug, Deserialize)]
struct Poll {
    route: String,
    #[serde(default)]
    profitable_competitive: bool,
    #[serde(default)]
    gross_lamports: u64,
    #[serde(default)]
    competitive_net_lamports: i128,
    #[serde(default)]
    size_lamports: u64,
}

fn pool_of(route: &str) -> &str {
    route.split('|').next().unwrap_or(route)
}

fn main() -> Result<()> {
    let paths: Vec<String> = std::env::args().skip(1).collect();
    if paths.is_empty() {
        bail!("usage: reprice-pump-fees <polls.jsonl> [more.jsonl ...]");
    }

    let mut polls: Vec<Poll> = Vec::new();
    for p in &paths {
        let f = std::fs::File::open(p).with_context(|| format!("open {p}"))?;
        for line in BufReader::new(f).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(poll) = serde_json::from_str::<Poll>(&line) {
                polls.push(poll);
            }
        }
    }

    println!("=== S13C SLICE 6B — historical economics repricing (Pump fee-v2) ===");
    println!("Legacy fee assumption: {LEGACY_FEE_BPS} bps. Correction is a MEASURED-");
    println!("CURRENT-RATE sensitivity (route1=75 bps, route3=95 bps), NOT exact");
    println!("historical repricing (no per-observation fee-config provenance).\n");
    println!("Files: {}", paths.join(", "));
    println!("Polls read: {}\n", polls.len());

    let summary = summarize(polls.iter().map(|p| {
        (
            pool_of(&p.route),
            p.competitive_net_lamports,
            p.size_lamports,
            p.gross_lamports,
        )
    }));

    println!("Repriceable (measured-rate pools): {}", summary.estimated);
    println!(
        "Not repriceable (other pools):     {}",
        summary.not_repriceable
    );
    println!(
        "Competitive-positive polls: before={} after={}",
        summary.positive_before, summary.positive_after
    );
    println!(
        "Capturable value (sum positive net): before={} lamports after={} lamports",
        summary.capturable_before_lamports, summary.capturable_after_lamports
    );
    let removed = summary.capturable_before_lamports - summary.capturable_after_lamports;
    println!(
        "Value removed by fee correction: {removed} lamports ({:.6} SOL)\n",
        removed as f64 / 1e9
    );

    // Per-poll detail for any competitive-positive record on a measured pool.
    let mut shown = 0;
    for p in &polls {
        if !(p.profitable_competitive || p.competitive_net_lamports > 0) {
            continue;
        }
        let r = reprice_poll(
            pool_of(&p.route),
            p.competitive_net_lamports,
            p.size_lamports,
            p.gross_lamports,
        );
        if r.class == RepriceClass::EstimatedCurrentRate {
            println!(
                "  {} size={} old_net={} extra_fee={} corrected_net={} {}",
                &pool_of(&p.route)[..8],
                p.size_lamports,
                r.old_net,
                r.extra_fee_lamports,
                r.corrected_net,
                if r.corrected_net > 0 {
                    "STILL+"
                } else {
                    "->NEG"
                }
            );
            shown += 1;
        }
    }
    if shown == 0 {
        println!("(No competitive-positive opportunity records present in these files —");
        println!(" corrected economics on this dataset is 0 capturable value. The prior");
        println!(" 0.1127 / 0.095 SOL/day figures came from a larger historical dataset");
        println!(" not present in the working tree; rerun against that JSONL to reprice.)");
    }

    Ok(())
}
