//! forensics-batch — run the Q1–Q4 pipeline over a directory of v2 inputs and
//! emit one comparison table ranking venue pairs by economic viability.
//!
//! READ-ONLY. No key loading, no transaction construction, Mode::Observe
//! semantics throughout (this binary cannot submit anything).

use anyhow::{Context, Result};
use arb_monitor::forensics::pipeline::{scan_pair, PairOutcome, ScanOptions};
use arb_monitor::forensics::schema::load_input;
use serde::Serialize;
use solana_client::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;

// ─────────────────────────── verdict policy ───────────────────────────
// Declared once, here, so a verdict can never drift from an undocumented
// number buried in a formatting string.

/// Cheapest Geyser tier covering ~90 accounts (6 pairs × [pool + bin/tick
/// arrays]) — the S15B infra floor.
const INFRA_FLOOR_USD_MONTH: f64 = 49.0;
/// Building is only justified when the WHOLE market's addressable pot clears
/// the floor with headroom, before any capture-rate discount.
const MIN_MONTHLY_USD_TO_BUILD: f64 = 5.0 * INFRA_FLOOR_USD_MONTH;
/// Below this event rate a bot cannot amortize its fixed costs even if every
/// event is won.
const MIN_EVENTS_PER_DAY: f64 = 50.0;
/// The threshold row used for "addressable" economics (net lamports).
const ADDRESSABLE_THRESHOLD_LAMPORTS: i128 = 50_000;

#[derive(Debug, Serialize)]
struct Row {
    input: String,
    venue_a: String,
    venue_b: String,
    events_per_day: f64,
    median_lamports: i128,
    pct_below_sig_floor: f64,
    events_over_threshold: usize,
    usd_month_all_participants: Option<f64>,
    distinct_signers: usize,
    top1_share_pct: f64,
    verdict: String,
    notes: Vec<String>,
}

/// Verdict rules, in order:
/// - `INCONCLUSIVE` if monthly USD cannot be computed honestly (quote is not a
///   USD stable and no explicit `--sol-usd` assumption was given).
/// - `KILL` if the whole market's addressable pot (< threshold row, ALL
///   participants combined) is below the infra floor.
/// - `BUILD` if the pot clears 5× the floor AND the event rate clears
///   MIN_EVENTS_PER_DAY.
/// - `INVESTIGATE` otherwise.
fn verdict(usd_month: Option<f64>, events_per_day: f64) -> String {
    match usd_month {
        None => "INCONCLUSIVE".into(),
        Some(u) if u < INFRA_FLOOR_USD_MONTH => "KILL".into(),
        Some(u) if u >= MIN_MONTHLY_USD_TO_BUILD && events_per_day >= MIN_EVENTS_PER_DAY => {
            "BUILD".into()
        }
        Some(_) => "INVESTIGATE".into(),
    }
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn row_from_outcome(input: &str, o: &PairOutcome, sol_usd_override: Option<f64>) -> Row {
    let th =
        o.q4.thresholds
            .iter()
            .find(|t| t.threshold_lamports == ADDRESSABLE_THRESHOLD_LAMPORTS);
    let (events_over, sum_over) = th.map(|t| (t.events, t.sum_lamports)).unwrap_or((0, 0));
    let mut notes = o.notes.clone();
    let usd_per_sol = match (o.q4.usd_per_sol, sol_usd_override) {
        (Some(u), _) => Some(u), // in-window stable-derived price wins
        (None, Some(u)) => {
            notes.push(format!(
                "ASSUMPTION: quote is not a USD stable; USD figures use --sol-usd {u} (optimistic if SOL fell in-window)"
            ));
            Some(u)
        }
        (None, None) => None,
    };
    let sol_month_over = sum_over as f64 / 1e9 / o.window_hours * 24.0 * 30.0;
    let usd_month = usd_per_sol.map(|u| sol_month_over * u);
    let t0 =
        o.q4.thresholds
            .iter()
            .find(|t| t.threshold_lamports == 0)
            .map(|t| t.events_per_day)
            .unwrap_or(0.0);
    let pct_below = if o.q4.value_positive > 0 {
        o.q4.below_sig_fee_floor as f64 * 100.0 / o.q4.value_positive as f64
    } else {
        0.0
    };
    Row {
        input: input.to_string(),
        venue_a: o.venue_a.clone(),
        venue_b: o.venue_b.clone(),
        events_per_day: t0,
        median_lamports: o.q4.median_positive_lamports,
        pct_below_sig_floor: pct_below,
        events_over_threshold: events_over,
        usd_month_all_participants: usd_month,
        distinct_signers: o.q4.distinct_positive_signers,
        top1_share_pct: o.q4.top1_signer_share_permille as f64 / 10.0,
        verdict: verdict(usd_month, t0),
        notes,
    }
}

fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let args: Vec<String> = std::env::args().collect();
    let rpc_url = std::env::var("RPC_ENDPOINT").context("RPC_ENDPOINT required")?;
    let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());

    let dir = arg(&args, "--dir").unwrap_or_else(|| "reports/forensics/inputs".into());
    let sol_usd: Option<f64> = arg(&args, "--sol-usd").map(|s| s.parse()).transpose()?;
    let opt = ScanOptions {
        max_pages: arg(&args, "--max-pages")
            .map(|s| s.parse())
            .transpose()?
            .unwrap_or(120),
        max_tx_fetches: arg(&args, "--max-tx")
            .map(|s| s.parse())
            .transpose()?
            .unwrap_or(12_000),
        q12_top_k: arg(&args, "--q12-k")
            .map(|s| s.parse())
            .transpose()?
            .unwrap_or(12),
        q2_trials: 10_000,
    };

    let mut inputs: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
        .with_context(|| format!("read dir {dir}"))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension().is_some_and(|x| x == "json")
                && !p
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().contains("manifest"))
        })
        .collect();
    inputs.sort();
    println!(
        "=== forensics-batch (READ-ONLY) — {} inputs from {dir} ===",
        inputs.len()
    );
    println!(
        "policy: floor ${INFRA_FLOOR_USD_MONTH}/mo · build ≥ ${MIN_MONTHLY_USD_TO_BUILD}/mo \
         and ≥ {MIN_EVENTS_PER_DAY} events/day · threshold {ADDRESSABLE_THRESHOLD_LAMPORTS} lamports"
    );

    let mut rows: Vec<Row> = Vec::new();
    let mut outcomes: Vec<(String, Option<PairOutcome>)> = Vec::new();
    for path in &inputs {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        println!("\n── {name} ──");
        let run = || -> Result<PairOutcome> {
            let json = std::fs::read_to_string(path)?;
            let input = load_input(&json)?;
            scan_pair(&rpc, &input, &opt)
        };
        match run() {
            Ok(o) => {
                let r = row_from_outcome(&name, &o, sol_usd);
                println!(
                    "  {} + {}: {} cross txs ({} landed), {} value-positive, verdict {}",
                    o.venue_a,
                    o.venue_b,
                    o.q3.attempts,
                    o.q3.landed,
                    o.q4.value_positive,
                    r.verdict
                );
                rows.push(r);
                outcomes.push((name, Some(o)));
            }
            Err(e) => {
                // Errors (incl. loud truncation) become visible rows, never
                // silently skipped inputs.
                println!("  ERROR: {e:#}");
                rows.push(Row {
                    input: name.clone(),
                    venue_a: "?".into(),
                    venue_b: "?".into(),
                    events_per_day: 0.0,
                    median_lamports: 0,
                    pct_below_sig_floor: 0.0,
                    events_over_threshold: 0,
                    usd_month_all_participants: None,
                    distinct_signers: 0,
                    top1_share_pct: 0.0,
                    verdict: format!("ERROR: {e}"),
                    notes: vec![],
                });
                outcomes.push((name, None));
            }
        }
    }

    // Rank: BUILD first, then INVESTIGATE, by usd/month desc.
    rows.sort_by(|a, b| {
        let rank = |r: &Row| match r.verdict.as_str() {
            "BUILD" => 0,
            "INVESTIGATE" => 1,
            "INCONCLUSIVE" => 2,
            "KILL" => 3,
            _ => 4,
        };
        rank(a).cmp(&rank(b)).then(
            b.usd_month_all_participants
                .unwrap_or(0.0)
                .partial_cmp(&a.usd_month_all_participants.unwrap_or(0.0))
                .unwrap(),
        )
    });

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::create_dir_all("reports")?;
    let json_path = format!("reports/forensics-batch-{ts}.json");
    std::fs::write(
        &json_path,
        serde_json::to_string_pretty(&serde_json::json!({
            "policy": {
                "infra_floor_usd_month": INFRA_FLOOR_USD_MONTH,
                "min_monthly_usd_to_build": MIN_MONTHLY_USD_TO_BUILD,
                "min_events_per_day": MIN_EVENTS_PER_DAY,
                "addressable_threshold_lamports": ADDRESSABLE_THRESHOLD_LAMPORTS,
            },
            "rows": rows,
            "outcomes": outcomes.iter().map(|(n, o)| serde_json::json!({"input": n, "outcome": o})).collect::<Vec<_>>(),
        }))?,
    )?;

    let mut md = String::new();
    md.push_str("# Venue-pair forensics — comparison\n\n");
    md.push_str(&format!(
        "Policy: infra floor **${INFRA_FLOOR_USD_MONTH}/mo**, BUILD needs ≥ \
         **${MIN_MONTHLY_USD_TO_BUILD}/mo** (all participants, ≥{ADDRESSABLE_THRESHOLD_LAMPORTS} \
         lamports net) **and** ≥ **{MIN_EVENTS_PER_DAY} events/day**.\n\n"
    ));
    md.push_str("| venue_a | venue_b | events/day | median lamports | % below sig-fee floor | events > threshold | $/month (all participants) | distinct signers | top-1 share | verdict |\n");
    md.push_str("|---|---|---|---|---|---|---|---|---|---|\n");
    for r in &rows {
        md.push_str(&format!(
            "| {} | {} | {:.1} | {} | {:.0}% | {} | {} | {} | {:.1}% | {} |\n",
            r.venue_a,
            r.venue_b,
            r.events_per_day,
            r.median_lamports,
            r.pct_below_sig_floor,
            r.events_over_threshold,
            r.usd_month_all_participants
                .map(|u| format!("${u:.2}"))
                .unwrap_or_else(|| "Unsupported".into()),
            r.distinct_signers,
            r.top1_share_pct,
            r.verdict
        ));
    }
    md.push_str("\nNotes per input:\n");
    for r in &rows {
        if !r.notes.is_empty() {
            md.push_str(&format!("- **{}**: {}\n", r.input, r.notes.join("; ")));
        }
    }
    let md_path = format!("reports/forensics-batch-{ts}.md");
    std::fs::write(&md_path, &md)?;
    println!("\n{md}");
    println!("wrote {json_path}\nwrote {md_path}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::verdict;

    #[test]
    fn verdict_policy_matches_declared_constants() {
        assert_eq!(verdict(None, 1000.0), "INCONCLUSIVE");
        assert_eq!(
            verdict(Some(9.76), 289.8),
            "KILL",
            "the S15B pair dies here"
        );
        assert_eq!(verdict(Some(48.99), 1000.0), "KILL");
        assert_eq!(verdict(Some(49.0), 10.0), "INVESTIGATE");
        assert_eq!(
            verdict(Some(300.0), 49.9),
            "INVESTIGATE",
            "rate bar not met"
        );
        assert_eq!(
            verdict(Some(244.9), 100.0),
            "INVESTIGATE",
            "pot bar not met"
        );
        assert_eq!(verdict(Some(245.0), 50.0), "BUILD");
    }
}
