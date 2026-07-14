//! `observe-markets` — S13 WIDE observe mode (all discovered markets).
//!
//! Each cycle: single-slot snapshot per market, both routes, size-optimize,
//! two near-immediate confirmations, stream to JSONL. Graceful SIGINT/SIGTERM
//! flush, hourly checkpoint reports. **Never builds/signs/simulates/submits.**
//! For sub-cycle survival evidence use `observe-narrow`.
//!
//! Usage: cargo run -p arb-monitor --bin observe-markets [--cache PATH] [--once]
//! Env: RPC_ENDPOINT (secrets redacted), OBS_MAX_SOL (20), OBS_INTERVAL_SECS
//!      (15), OBS_DURATION_SECS (86400), OBS_DOUBLE_CONFIRM (true),
//!      OBS_OUT_DIR (reports/observe).

use anyhow::{Context, Result};
use arb_common::cost::{CostModel, ExecutionPayment};
use arb_common::mode::Mode;
use arb_monitor::market_discovery::DiscoveryCache;
use arb_monitor::observe_live::{
    cluster_time, env_u64, fetch_snapshot, git_commit, gzip, install_shutdown, now_ms, reconfirm,
    routes_for, secrets_from_env, Ctx,
};
use arb_monitor::observe_report::{
    aggregate, default_scenarios, sensitivity, wide_verdict, CandidateRecord, CostBreakdown,
};
use arb_monitor::optimizer::{optimize, SizeGrid};
use arb_monitor::route_engine::RouteReject;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use std::collections::BTreeMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    let cache_path = arg_val(&args, "--cache").unwrap_or_else(|| "markets.generated.json".into());
    let once = args.iter().any(|a| a == "--once");

    let mode: Mode = std::env::var("MODE")
        .ok()
        .and_then(|m| m.parse().ok())
        .unwrap_or(Mode::Observe);
    let min_net = env_u64("OBS_MIN_NET_LAMPORTS", 0);
    let max_sol: f64 = std::env::var("OBS_MAX_SOL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20.0);
    let interval = Duration::from_secs(env_u64("OBS_INTERVAL_SECS", 15));
    let duration = Duration::from_secs(env_u64("OBS_DURATION_SECS", 86_400));
    let double_confirm = std::env::var("OBS_DOUBLE_CONFIRM")
        .map(|v| v != "false" && v != "0")
        .unwrap_or(true);
    let out_dir = std::env::var("OBS_OUT_DIR").unwrap_or_else(|_| "reports/observe".into());
    std::fs::create_dir_all(&out_dir).ok();

    let rpc_url = std::env::var("RPC_ENDPOINT").context("RPC_ENDPOINT required")?;
    let secrets_owned = secrets_from_env(&rpc_url);
    let secrets: Vec<&str> = secrets_owned.iter().map(|s| s.as_str()).collect();
    let rpc = RpcClient::new_with_commitment(rpc_url.clone(), CommitmentConfig::confirmed());
    let ctx = Ctx::new()?;

    let raw = std::fs::read_to_string(&cache_path)
        .with_context(|| format!("read discovery cache {cache_path}"))?;
    let cache = DiscoveryCache::from_json(&raw)
        .context("cache version mismatch — re-run discover-markets")?;

    let run_id = now_ms();
    let jsonl_path = format!("{out_dir}/candidates-{run_id}.jsonl");
    let report_path = format!("{out_dir}/report-{run_id}.json");
    let mut jsonl = std::fs::File::create(&jsonl_path)?;
    let shutdown = Arc::new(AtomicBool::new(false));
    install_shutdown(shutdown.clone());
    info!(markets = cache.markets.len(), mode = %mode, commit = %git_commit(), jsonl = %jsonl_path,
          "observe-markets (WIDE) starting — NEVER submits; SIGINT/SIGTERM flushes a partial report");

    let cost = base_cost(min_net);
    let grid = SizeGrid {
        min: 10_000_000,
        max: (max_sol * 1e9) as u64,
        ..Default::default()
    };

    let mut records: Vec<CandidateRecord> = Vec::new();
    let mut scan_secs: Vec<f64> = Vec::new();
    let mut reject_totals: BTreeMap<String, u64> = BTreeMap::new();
    let run_start = Instant::now();
    let mut last_checkpoint = Instant::now();

    loop {
        let cycle_start = Instant::now();
        let now_unix = cluster_time(&rpc).await;
        let mut cycle_candidates = 0usize;

        for m in &cache.markets {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            let mkt_start = Instant::now();
            let snap = match fetch_snapshot(&rpc, &ctx, m, now_unix, &secrets).await {
                Ok(s) => s,
                Err(reason) => {
                    *reject_totals
                        .entry(format!("no_state:{reason}"))
                        .or_default() += 1;
                    continue;
                }
            };
            let (rpc_latency_ms, context_slot) = (snap.rpc_latency_ms, snap.slot);

            for (label, route) in routes_for(snap) {
                if route.token_mint(&ctx.wsol).is_none() {
                    *reject_totals.entry("topology".into()).or_default() += 1;
                    continue;
                }
                let Some(c) = optimize(&route, &ctx.wsol, &cost, &grid) else {
                    let key = classify(&route.evaluate(&ctx.wsol, grid.max / 10, &cost));
                    *reject_totals.entry(key.into()).or_default() += 1;
                    continue;
                };
                cycle_candidates += 1;
                let detected_at_ms = now_ms();
                let (pump_fee, meteora_fee) = if label == "pump->meteora" {
                    (c.leg1_fee, c.leg2_fee)
                } else {
                    (c.leg2_fee, c.leg1_fee)
                };
                let confirm1 = reconfirm(
                    &rpc,
                    &ctx,
                    m,
                    now_unix,
                    &secrets,
                    label,
                    &cost,
                    &grid,
                    detected_at_ms,
                )
                .await;
                let confirm2 = if double_confirm {
                    Some(
                        reconfirm(
                            &rpc,
                            &ctx,
                            m,
                            now_unix,
                            &secrets,
                            label,
                            &cost,
                            &grid,
                            detected_at_ms,
                        )
                        .await,
                    )
                } else {
                    None
                };
                let rec = CandidateRecord {
                    detected_at_ms,
                    context_slot,
                    token_mint: m.token_mint.clone(),
                    pump_pool: m.pump_pool.clone(),
                    dlmm_pair: m.dlmm_pair.clone(),
                    direction: label.to_string(),
                    input_lamports: c.amount_in,
                    gross_profit_lamports: c.gross_profit,
                    pump_fee_lamports: pump_fee,
                    meteora_fee_lamports: meteora_fee,
                    cost: CostBreakdown::from_model(&cost, c.gross_profit),
                    net_profit_lamports: c.net_profit,
                    rpc_latency_ms,
                    scan_latency_ms: mkt_start.elapsed().as_millis() as u64,
                    total_candidate_age_ms: now_ms().saturating_sub(detected_at_ms),
                    confirm1_delay_ms: Some(confirm1.delay_ms),
                    confirm2_delay_ms: confirm2.as_ref().map(|c| c.delay_ms),
                    confirm1: Some(confirm1),
                    confirm2,
                };
                writeln!(jsonl, "{}", serde_json::to_string(&rec)?)?;
                jsonl.flush().ok();
                records.push(rec);
            }
        }

        scan_secs.push(cycle_start.elapsed().as_secs_f64());
        info!(
            cycle_secs = format!("{:.1}", scan_secs.last().unwrap()),
            candidates = cycle_candidates,
            total_records = records.len(),
            "cycle complete"
        );

        // Hourly checkpoint so a late crash keeps the accumulated analysis.
        if last_checkpoint.elapsed() >= Duration::from_secs(3600) {
            write_report(
                &report_path,
                run_id,
                &mode,
                &cache,
                &records,
                &scan_secs,
                &reject_totals,
                run_start.elapsed(),
                min_net,
                max_sol,
                double_confirm,
                &jsonl_path,
                true,
            )?;
            info!(report = %report_path, "hourly checkpoint written");
            last_checkpoint = Instant::now();
        }

        if once || shutdown.load(Ordering::Relaxed) || run_start.elapsed() >= duration {
            break;
        }
        // Interruptible sleep.
        let woke = Instant::now();
        while woke.elapsed() < interval {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    let partial = shutdown.load(Ordering::Relaxed);
    write_report(
        &report_path,
        run_id,
        &mode,
        &cache,
        &records,
        &scan_secs,
        &reject_totals,
        run_start.elapsed(),
        min_net,
        max_sol,
        double_confirm,
        &jsonl_path,
        partial,
    )?;
    gzip(&report_path);
    gzip(&jsonl_path);
    let agg = aggregate(&records, &scan_secs, reject_totals.clone());
    println!(
        "\n════ WIDE OBSERVE {} ════\ncycles={} raw_sightings={} unique_routes={} episodes={} conf1={} conf2={}\n{}\nreport: {report_path}(.gz)",
        if partial { "STOPPED (partial)" } else { "COMPLETE" },
        agg.scan_cycles, agg.raw_sightings, agg.unique_routes, agg.unique_episodes,
        agg.sightings_confirmed_once, agg.sightings_confirmed_twice, wide_verdict(&agg),
    );
    Ok(())
}

fn base_cost(min_net: u64) -> CostModel {
    CostModel {
        signature_fee_lamports: 5_000,
        compute_unit_limit: 600_000,
        compute_unit_price_micro: 10_000,
        margin_lamports: 10_000,
        required_net_lamports: min_net,
        payment: ExecutionPayment::JitoTip {
            min_lamports: 10_000,
            max_lamports: 100_000_000,
        },
        ..Default::default()
    }
}

fn classify(r: &Result<arb_monitor::route_engine::Candidate, RouteReject>) -> &'static str {
    match r {
        Err(RouteReject::Leg1(_)) => "leg1",
        Err(RouteReject::Leg2(_)) => "leg2",
        Err(RouteReject::NonPositiveGross) => "non_positive_gross",
        Err(RouteReject::BelowNet { .. }) | Ok(_) => "below_net",
        Err(RouteReject::TopologyMismatch) => "topology",
    }
}

fn arg_val(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1).cloned())
}

#[allow(clippy::too_many_arguments)]
fn write_report(
    path: &str,
    run_id: u64,
    mode: &Mode,
    cache: &DiscoveryCache,
    records: &[CandidateRecord],
    scan_secs: &[f64],
    reject_totals: &BTreeMap<String, u64>,
    elapsed: Duration,
    min_net: u64,
    max_sol: f64,
    double_confirm: bool,
    jsonl_path: &str,
    partial: bool,
) -> Result<()> {
    let agg = aggregate(records, scan_secs, reject_totals.clone());
    let sens = sensitivity(records, &default_scenarios());
    let report = serde_json::json!({
        "run": {
            "id": run_id, "commit": git_commit(), "mode": mode.to_string(),
            "markets": cache.markets.len(), "duration_secs": elapsed.as_secs(),
            "cycles": scan_secs.len(), "min_net_lamports": min_net, "max_sol": max_sol,
            "double_confirm": double_confirm, "partial": partial,
            "cost_basis": "modeled — no transaction was built or simulated; priority fee, \
                           Jito tip, compute units and rent are ASSUMPTIONS",
            "note": "observe-only; never builds/simulates/submits. Candidates are \
                     single-slot-consistent MONITOR signals.",
        },
        "aggregate": agg,
        "sensitivity": sens,
        "verdict": wide_verdict(&agg),
        "jsonl_log": jsonl_path,
    });
    std::fs::write(path, serde_json::to_string_pretty(&report)?)?;
    Ok(())
}
