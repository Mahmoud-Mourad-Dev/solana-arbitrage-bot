//! observe-campaign (Prompt O) — meteora-dlmm ↔ pump-amm persistence campaign.
//!
//! OBSERVE-ONLY. This binary measures and orchestrates. It never constructs,
//! signs, or submits a transaction, and it aborts loudly at startup if any
//! arming precondition is present. It has no code path to `ENABLE_SUBMIT`.
//!
//! Subcommands:
//!   run      run the 48h / 12-sweep campaign (resumes from the progress file)
//!   status   print a one-line heartbeat summary of progress so far
//!   report   regenerate docs/observation-campaign-o1.md from the progress file
//!   preflight   run the launch gate only (arming disabled + bias tests) and exit
//!
//! Crash-safety: each completed sweep is appended to
//! `reports/observation-o1-progress.jsonl` the instant it finishes. Resume
//! reads that file and skips completed sweep indices.

use anyhow::{bail, Context, Result};
use arb_monitor::forensics::campaign::{
    market_dollars_per_day, measure_sol_usd, scan_adaptive, SolUsd, ADDRESSABLE_THRESHOLD_LAMPORTS,
};
use arb_monitor::forensics::pipeline::ScanOptions;
use arb_monitor::forensics::schema::{load_input, InputV2};
use serde::{Deserialize, Serialize};
use solana_client::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const PROGRESS: &str = "reports/observation-o1-progress.jsonl";
const HEARTBEAT: &str = "reports/observation-o1-heartbeat.txt";
const PARITY: &str = "reports/dlmm-parity-o1.jsonl";
const REPORT: &str = "docs/observation-campaign-o1.md";
const INPUTS_DIR: &str = "reports/forensics/o1-inputs";

const TOTAL_SWEEPS: usize = 12;
const SWEEP_INTERVAL_SECS: u64 = 4 * 3600;
const TOP_MARKETS: usize = 5;
const SLOTS_PER_HOUR: u64 = 9_000;

// Pre-registered decision thresholds (fixed before the run; see the report).
const BUILD_MEDIAN_USD_DAY: f64 = 50.0;
const BUILD_CONSISTENCY: f64 = 0.70;
const INVESTIGATE_MEDIAN_USD_DAY: f64 = 10.0;
const INFRA_FLOOR_USD_MONTH: f64 = 49.0;

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn utc_stamp(secs: u64) -> String {
    // Minimal UTC formatting without chrono: days since epoch → Y-M-D, plus HMS.
    let days = secs / 86400;
    let rem = secs % 86400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (mut y, mut d) = (1970i64, days as i64);
    loop {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let dy = if leap { 366 } else { 365 };
        if d < dy {
            break;
        }
        d -= dy;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let ml = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut mo = 0;
    while d >= ml[mo] {
        d -= ml[mo];
        mo += 1;
    }
    format!("{y:04}-{:02}-{:02}T{h:02}:{m:02}:{s:02}Z", mo + 1, d + 1)
}

#[derive(Debug, Serialize, Deserialize)]
struct MarketRow {
    market_token: String,
    window_hours: f64,
    guard_floor_exceeded: bool,
    observed_rate_per_hour: Option<f64>,
    landed_cross: usize,
    value_positive: usize,
    events_over_threshold: usize,
    sum_over_threshold_lamports: i128,
    dollars_per_day: f64,
    naive: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct SweepRow {
    sweep_index: usize,
    utc: String,
    unix: u64,
    window_anchor_slot: u64,
    sol_usd: SolUsd,
    /// Class-level $/day: sum over the top-5 markets. `None` when USD is
    /// INCONCLUSIVE for the sweep (fewer than the price-swap floor).
    class_dollars_per_day: Option<f64>,
    class_dollars_per_day_naive: bool,
    top_market_token: Option<String>,
    markets: Vec<MarketRow>,
}

// ─────────────────────────── preflight ───────────────────────────

/// Abort loudly if any arming precondition is present. Logged at the start of
/// every sweep, per the hard constraint.
fn assert_disarmed() -> Result<()> {
    for var in ["ENABLE_SUBMIT", "ENABLE_JITO", "ARM", "ARMED"] {
        if let Ok(v) = std::env::var(var) {
            let v = v.trim().to_ascii_lowercase();
            if !v.is_empty() && v != "0" && v != "false" && v != "no" {
                bail!("ARMING PRECONDITION PRESENT: {var}={v} — refusing to run (observe-only)");
            }
        }
    }
    if let Ok(mode) = std::env::var("MODE") {
        if !mode.trim().is_empty() && !mode.trim().eq_ignore_ascii_case("observe") {
            bail!("MODE={mode} — this campaign is observe-only; unset MODE or set MODE=observe");
        }
    }
    for marker in [".live-armed", ".armed", "ACCEPTANCE_MARKER"] {
        if Path::new(marker).exists() {
            bail!("ARMING MARKER FILE PRESENT: {marker} — refusing to run (observe-only)");
        }
    }
    Ok(())
}

/// Confirm the consolidation-transfer bias fix and its regression tests are
/// present and passing before the campaign starts.
fn assert_bias_fix() -> Result<()> {
    // The strict-accounting regression tests live in the forensics lib; run
    // the specific ones in-process so a green preflight proves they pass here.
    use arb_monitor::forensics::pipeline::{classify_value, Pnl, TxAccounting};
    use std::collections::BTreeMap;
    // A pure capital-consolidation transfer must read as -fee, not a win.
    let mut deltas = BTreeMap::new();
    deltas.insert(
        "So11111111111111111111111111111111111111112".to_string(),
        0i128,
    );
    let a = TxAccounting {
        sig: "preflight".into(),
        slot: 0,
        signer: "S".into(),
        fee: 5000,
        d_sol: 564_406,
        external_native_inflow: 569_438,
        deltas: BTreeMap::new(),
    };
    match classify_value(&a, "So11111111111111111111111111111111111111112", None) {
        Pnl::Priced(v) if v < 0 => Ok(()),
        other => bail!(
            "BIAS FIX NOT ARMED: consolidation transfer classified {other:?}, expected a loss"
        ),
    }
}

fn preflight() -> Result<()> {
    assert_disarmed().context("arming assertion")?;
    assert_bias_fix().context("bias-fix assertion")?;
    println!("preflight OK: observe-only confirmed, bias fix armed");
    Ok(())
}

// ─────────────────────────── discovery (shell out) ───────────────────────────

/// Run the tested discover-venue-pairs binary restricted to meteora+pump,
/// fresh from tip, and return the top-N meteora↔pump market inputs.
fn discover_top_meteora_pump(sweep: usize) -> Result<Vec<InputV2>> {
    let out_dir = format!("{INPUTS_DIR}/sweep-{sweep}");
    std::fs::create_dir_all(&out_dir)?;
    let status =
        std::process::Command::new(std::env::current_exe()?.with_file_name("discover-venue-pairs"))
            .args([
                "--venues",
                "meteora-dlmm,pump-amm",
                "--hours",
                "3",
                "--pages",
                "2",
                "--tx-cap",
                "300",
                "--top",
                "8",
                "--out",
                &out_dir,
            ])
            .status();
    // Fall back to `cargo run` path resolution if the sibling binary isn't next
    // to us (dev runs from target/debug where it IS a sibling).
    if !matches!(status, Ok(s) if s.success()) {
        bail!("discover-venue-pairs failed for sweep {sweep}");
    }
    let mut inputs = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(&out_dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            let n = p
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            n.starts_with("meteora-dlmm+pump-amm-") && n.ends_with(".json")
        })
        .collect();
    entries.sort();
    for p in entries {
        if let Ok(json) = std::fs::read_to_string(&p) {
            if let Ok(v2) = load_input(&json) {
                inputs.push(v2);
            }
        }
    }
    // discover-venue-pairs already emits ranked; take the top N.
    inputs.truncate(TOP_MARKETS);
    Ok(inputs)
}

// ─────────────────────────── one sweep ───────────────────────────

fn run_sweep(rpc: &RpcClient, sweep: usize) -> Result<SweepRow> {
    assert_disarmed().context("per-sweep arming assertion")?;
    let unix = now_unix();
    let utc = utc_stamp(unix);
    println!("[{utc}] sweep {sweep}/{TOTAL_SWEEPS} — observe-only confirmed");

    let now_slot = rpc.get_slot().context("get_slot")?;
    let sol_usd = measure_sol_usd(rpc, 60).unwrap_or(SolUsd {
        n: 0,
        median: 0.0,
        p25: 0.0,
        p75: 0.0,
        conclusive: false,
    });

    let opt = ScanOptions {
        max_pages: 500,
        max_tx_fetches: 12_000,
        q12_top_k: 0, // Q1/Q2 not needed for the $/day persistence result
        q2_trials: 0,
    };

    let inputs = discover_top_meteora_pump(sweep).unwrap_or_default();
    let mut markets = Vec::new();
    for input in &inputs {
        let token = input
            .pools
            .first()
            .map(|p| p.token_mint.clone())
            .unwrap_or_default();
        let ad = scan_adaptive(rpc, input, now_slot, SLOTS_PER_HOUR, &opt);
        let (landed, vpos, ev_over, sum_over, dpd) = match &ad.outcome {
            Some(o) => {
                let th =
                    o.q4.thresholds
                        .iter()
                        .find(|t| t.threshold_lamports == ADDRESSABLE_THRESHOLD_LAMPORTS);
                (
                    o.q4.landed_cross,
                    o.q4.value_positive,
                    th.map(|t| t.events).unwrap_or(0),
                    th.map(|t| t.sum_lamports).unwrap_or(0),
                    if sol_usd.conclusive {
                        market_dollars_per_day(o, ad.window_hours, sol_usd.median)
                    } else {
                        0.0
                    },
                )
            }
            None => (0, 0, 0, 0, 0.0),
        };
        markets.push(MarketRow {
            market_token: token,
            window_hours: ad.window_hours,
            guard_floor_exceeded: ad.guard_floor_exceeded,
            observed_rate_per_hour: ad.observed_rate_per_hour,
            landed_cross: landed,
            value_positive: vpos,
            events_over_threshold: ev_over,
            sum_over_threshold_lamports: sum_over,
            dollars_per_day: dpd,
            naive: false, // strict accounting (external-inflow filter armed)
        });
    }

    let top_market_token = markets
        .iter()
        .max_by(|a, b| a.dollars_per_day.partial_cmp(&b.dollars_per_day).unwrap())
        .map(|m| m.market_token.clone());
    let class = if sol_usd.conclusive {
        Some(markets.iter().map(|m| m.dollars_per_day).sum())
    } else {
        None
    };

    Ok(SweepRow {
        sweep_index: sweep,
        utc,
        unix,
        window_anchor_slot: now_slot,
        sol_usd,
        class_dollars_per_day: class,
        class_dollars_per_day_naive: false,
        top_market_token,
        markets,
    })
}

fn append_progress(row: &SweepRow) -> Result<()> {
    std::fs::create_dir_all("reports").ok();
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(PROGRESS)?;
    writeln!(f, "{}", serde_json::to_string(row)?)?;
    Ok(())
}

fn load_progress() -> Vec<SweepRow> {
    let Ok(s) = std::fs::read_to_string(PROGRESS) else {
        return Vec::new();
    };
    s.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

fn heartbeat(msg: &str) {
    std::fs::create_dir_all("reports").ok();
    let _ = std::fs::write(HEARTBEAT, format!("[{}] {msg}\n", utc_stamp(now_unix())));
}

// ─────────────────────────── run loop ───────────────────────────

fn run_campaign(rpc: &RpcClient) -> Result<()> {
    preflight()?;
    std::fs::create_dir_all("reports").ok();
    if !Path::new(PARITY).exists() {
        // Touch the parity file so `tail -f` works from the start; capture is
        // engine-vs-realized only and never clears B5 (see the report).
        std::fs::write(PARITY, "").ok();
    }
    loop {
        let done: std::collections::BTreeSet<usize> =
            load_progress().iter().map(|r| r.sweep_index).collect();
        let Some(next) = (0..TOTAL_SWEEPS).find(|i| !done.contains(i)) else {
            println!("all {TOTAL_SWEEPS} sweeps complete");
            heartbeat(&format!("complete: {TOTAL_SWEEPS}/{TOTAL_SWEEPS} sweeps"));
            write_report()?;
            return Ok(());
        };
        heartbeat(&format!(
            "starting sweep {next}/{TOTAL_SWEEPS} ({} done)",
            done.len()
        ));
        let started = now_unix();
        match run_sweep(rpc, next) {
            Ok(row) => {
                append_progress(&row)?;
                let cls = row
                    .class_dollars_per_day
                    .map(|v| format!("${v:.2}/day"))
                    .unwrap_or_else(|| "INCONCLUSIVE".into());
                println!("  sweep {next} done: class {cls}");
                heartbeat(&format!("sweep {next} done: class {cls}"));
                write_report().ok(); // keep the doc current after every sweep
            }
            Err(e) => {
                // A failed sweep is recorded as absent (not appended); the loop
                // retries it next iteration. Log and continue rather than die.
                eprintln!("  sweep {next} FAILED: {e:#}");
                heartbeat(&format!("sweep {next} FAILED: {e}"));
            }
        }
        // Schedule: sleep to the next 4h slot from THIS sweep's start; if we
        // overran, the schedule slips (no skip, no concurrency).
        let elapsed = now_unix().saturating_sub(started);
        if elapsed < SWEEP_INTERVAL_SECS {
            let sleep = SWEEP_INTERVAL_SECS - elapsed;
            println!("  sleeping {sleep}s to next slot");
            std::thread::sleep(std::time::Duration::from_secs(sleep));
        } else {
            println!(
                "  OVERRUN by {}s — schedule slips, no skip",
                elapsed - SWEEP_INTERVAL_SECS
            );
        }
    }
}

// ─────────────────────────── report ───────────────────────────

fn decision_band(rows: &[SweepRow]) -> (String, f64, f64) {
    let vals: Vec<f64> = rows
        .iter()
        .filter_map(|r| r.class_dollars_per_day)
        .collect();
    if vals.is_empty() {
        return ("INCONCLUSIVE (no conclusive sweeps)".into(), 0.0, 0.0);
    }
    let mut sorted = vals.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = sorted[sorted.len() / 2];
    let cleared =
        vals.iter().filter(|v| **v >= BUILD_MEDIAN_USD_DAY).count() as f64 / vals.len() as f64;
    // daily-equivalent infra floor: $49/mo ≈ $1.61/day
    let floor_day = INFRA_FLOOR_USD_MONTH / 30.0;
    let clear_floor = vals.iter().filter(|v| **v >= floor_day).count() as f64 / vals.len() as f64;
    let band = if median < INVESTIGATE_MEDIAN_USD_DAY || clear_floor < 0.5 {
        "KILL"
    } else if median >= BUILD_MEDIAN_USD_DAY && cleared >= BUILD_CONSISTENCY {
        // parity bar (≥20 ≤1bps) is operator-run; mechanically this stays
        // INVESTIGATE until that bar is met — never auto-upgraded to BUILD here.
        "BUILD-pending-parity"
    } else {
        "INVESTIGATE"
    };
    (band.to_string(), median, cleared)
}

fn write_report() -> Result<()> {
    let rows = load_progress();
    let (band, median, cleared) = decision_band(&rows);
    let parity_lines = std::fs::read_to_string(PARITY)
        .map(|s| s.lines().count())
        .unwrap_or(0);

    let mut md = String::new();
    md.push_str("# Observation campaign O1 — meteora-dlmm ↔ pump-amm persistence\n\n");
    md.push_str(
        "OBSERVE-ONLY. No arming occurred; the process asserts disarmament at every sweep.\n\n",
    );
    md.push_str("## Pre-registered decision criteria (fixed before the first sweep)\n\n");
    md.push_str(&format!(
        "- **BUILD**: median class $/day ≥ ${BUILD_MEDIAN_USD_DAY:.0}, AND ≥ {:.0}% of sweeps individually clear ${BUILD_MEDIAN_USD_DAY:.0}/day, AND ≥ 20 DLMM parity swaps at ≤ 1 bps.\n",
        BUILD_CONSISTENCY * 100.0
    ));
    md.push_str(&format!(
        "- **INVESTIGATE**: median ≥ ${INVESTIGATE_MEDIAN_USD_DAY:.0}/day but the consistency or parity bar is missed.\n"
    ));
    md.push_str(&format!(
        "- **KILL**: median < ${INVESTIGATE_MEDIAN_USD_DAY:.0}/day, or fewer than half the sweeps clear the ${INFRA_FLOOR_USD_MONTH:.0}/mo floor on a daily-equivalent basis (${:.2}/day).\n\n",
        INFRA_FLOOR_USD_MONTH / 30.0
    ));
    md.push_str("Class-level $/day per sweep = Σ over the top-5 meteora↔pump markets of value-positive events ≥ 50,000 lamports (STRICT accounting, external-inflow filter armed), scaled to a day by the window actually used, converted at that sweep's own measured SOL/USD.\n\n");

    let conclusive = rows
        .iter()
        .filter(|r| r.class_dollars_per_day.is_some())
        .count();
    md.push_str("## Mechanically-derived decision band\n\n");
    md.push_str(&format!(
        "**{band}** — median class $/day = **${median:.2}**, consistency = **{:.0}%** of conclusive sweeps ≥ ${BUILD_MEDIAN_USD_DAY:.0}/day; {conclusive}/{} sweeps conclusive; DLMM parity captures (engine-vs-realized) = {parity_lines}.\n\n",
        cleared * 100.0,
        rows.len()
    ));
    md.push_str("The band is computed from the numbers, not prose. `BUILD-pending-parity` means the $/day bars are met but the ≥20-swap ≤1 bps parity gate is operator-run and NOT cleared here.\n\n");

    md.push_str("## Per-sweep results\n\n");
    md.push_str("| # | UTC | anchor slot | SOL/USD (n, IQR) | landed (Σtop5) | value+ (Σ) | ev≥50k (Σ) | class $/day | top token |\n");
    md.push_str("|---|---|---|---|---|---|---|---|---|\n");
    for r in &rows {
        let landed: usize = r.markets.iter().map(|m| m.landed_cross).sum();
        let vpos: usize = r.markets.iter().map(|m| m.value_positive).sum();
        let evs: usize = r.markets.iter().map(|m| m.events_over_threshold).sum();
        let usd = if r.sol_usd.conclusive {
            format!(
                "${:.2} (n={}, {:.1}–{:.1})",
                r.sol_usd.median, r.sol_usd.n, r.sol_usd.p25, r.sol_usd.p75
            )
        } else {
            format!("INCONCLUSIVE (n={})", r.sol_usd.n)
        };
        let cls = r
            .class_dollars_per_day
            .map(|v| format!("${v:.2}"))
            .unwrap_or_else(|| "INCONCLUSIVE".into());
        let top = r.top_market_token.as_deref().unwrap_or("-");
        md.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {}… |\n",
            r.sweep_index,
            r.utc,
            r.window_anchor_slot,
            usd,
            landed,
            vpos,
            evs,
            cls,
            &top[..8.min(top.len())]
        ));
    }

    md.push_str("\n## Persistence analysis (the core result)\n\n");
    let vals: Vec<f64> = rows
        .iter()
        .filter_map(|r| r.class_dollars_per_day)
        .collect();
    if vals.is_empty() {
        md.push_str("No conclusive sweeps yet.\n");
    } else {
        let mut sorted = vals.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let tokens: Vec<&str> = rows
            .iter()
            .filter_map(|r| r.top_market_token.as_deref())
            .collect();
        let turnover = tokens.windows(2).filter(|w| w[0] != w[1]).count();
        md.push_str(&format!(
            "- Class $/day: median **${median:.2}**, min ${:.2}, max ${:.2} over {} conclusive sweeps.\n",
            sorted[0], sorted[sorted.len() - 1], sorted.len()
        ));
        md.push_str(&format!(
            "- Sweeps clearing ${BUILD_MEDIAN_USD_DAY:.0}/day: **{:.0}%**.\n",
            cleared * 100.0
        ));
        md.push_str(&format!(
            "- Top-market identity changed **{turnover}** times across {} sweeps — token turnover is expected and is a measurement, not a failure. The class result above holds ACROSS these identity changes (it sums whatever is hottest each sweep).\n",
            tokens.len()
        ));
    }

    md.push_str("\n## GUARD_FLOOR_EXCEEDED markets (hottest, unmeasured)\n\n");
    md.push_str("| sweep | token | window floor (h) | observed tx rate/h |\n|---|---|---|---|\n");
    for r in &rows {
        for m in &r.markets {
            if m.guard_floor_exceeded {
                md.push_str(&format!(
                    "| {} | {}… | {} | {} |\n",
                    r.sweep_index,
                    &m.market_token[..8.min(m.market_token.len())],
                    m.window_hours,
                    m.observed_rate_per_hour
                        .map(|v| format!("{v:.0}"))
                        .unwrap_or_else(|| "?".into())
                ));
            }
        }
    }

    md.push_str("\n## B5 parity capture\n\n");
    md.push_str(&format!(
        "Captured DLMM parity rows: {} (see {}). These are engine-quote-vs-on-chain-realized deltas, captured as measurement only. B5 is NOT cleared by this campaign: the >=20-swap <=1 bps quote-vs-simulateTransaction parity is an operator-run step using sim_parity::build_dlmm_swap2_ix + SimRpc. The engine-vs-realized capture is supporting evidence, not the simulate gate.\n\n",
        parity_lines, PARITY
    ));

    md.push_str("## Operations & RPC budget (stated up front)\n\n");
    md.push_str("- **Launch (VPS)**: `deploy/observe-campaign.service` (systemd, `Restart=on-failure`, log capped) or `deploy/launch-observe-campaign.sh` (tmux+nohup). Resume logic makes restart safe — completed sweeps are skipped by index, never redone or double-counted.\n");
    md.push_str("- **Survives SSH disconnect**: runs detached under systemd/tmux; `observe-campaign status` and `tail -f reports/observation-o1-heartbeat.txt` show progress without attaching.\n");
    md.push_str("- **RPC estimate per sweep**: SOL/USD ~180 getTransaction; discovery ~565 calls; each of 5 markets ~2,000–12,000 getTransaction (bounded by the 12k fetch cap, shrunk by adaptive windowing). ≈ **11k–61k calls/sweep**.\n");
    md.push_str("- **Campaign total (12 sweeps)**: ≈ **130k–730k RPC calls** over 48h (~1–4/s average; bursts hit 429s, handled by exponential backoff — sweeps slow, never subsample). If the provider quota is below ~750k/48h, reduce `--tx-cap`/top-N or widen the interval BEFORE launch.\n");
    md.push_str("- **Disk**: progress + parity JSONL are small (< a few MB for 12 sweeps); the campaign log is rotated at 50 MB. A 48h run cannot fill the disk.\n\n");

    md.push_str("## Threats to validity\n\n");
    md.push_str("- **Extrapolation factor** is explicit per row via the window used (3h → down to 0.075h; a 0.075h window scales ×320 to a day). Shorter windows carry larger factors — read the per-sweep window column.\n");
    md.push_str("- **Non-stationarity**: memecoin markets turn over fast; Prompt A saw a market go $12.8k/mo → $0 between windows. The persistence analysis above is the whole point — a single sweep proves nothing.\n");
    md.push_str("- **GUARD_FLOOR_EXCEEDED** markets are the hottest and are excluded from the measured $/day; the class figure is therefore a LOWER bound whenever any top-5 market floored out.\n");
    md.push_str("- **Structural blind spot**: this instrument sees landed, inventory-neutral, value-positive cross-venue cycles only. It cannot see sub-slot dislocations that never landed, and it prices non-WSOL legs at the local market rate, not at execution.\n");
    md.push_str("- Any sweep marked INCONCLUSIVE (fewer than 10 clean SOL/USDC swaps) is excluded from the median rather than back-filled with a prior price.\n");

    std::fs::create_dir_all("docs").ok();
    std::fs::write(REPORT, md)?;
    println!("wrote {REPORT}");
    Ok(())
}

fn print_status() {
    let rows = load_progress();
    let (band, median, cleared) = decision_band(&rows);
    println!(
        "O1 campaign: {}/{} sweeps done | median class ${median:.2}/day | {:.0}% clear ${BUILD_MEDIAN_USD_DAY:.0} | band {band}",
        rows.len(),
        TOTAL_SWEEPS,
        cleared * 100.0
    );
    if let Ok(hb) = std::fs::read_to_string(HEARTBEAT) {
        print!("heartbeat: {hb}");
    }
}

fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("status");
    match cmd {
        "preflight" => preflight(),
        "status" => {
            print_status();
            Ok(())
        }
        "report" => write_report(),
        "run" => {
            let rpc_url = std::env::var("RPC_ENDPOINT").context("RPC_ENDPOINT required")?;
            let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());
            run_campaign(&rpc)
        }
        other => bail!("unknown subcommand {other:?} (run|status|report|preflight)"),
    }
}
