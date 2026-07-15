//! `observe-narrow` — S13B fast-poll narrow observe experiment (corrected).
//!
//! Polls a small curated route set every few seconds. It streams poll AND
//! reconfirm EVENTS to JSONL and computes every metric through the pure
//! `narrow_report::aggregate_narrow`, so the live report and the offline
//! `rebuild-report` are byte-identical. Headline economics are CAUSAL
//! (value at detection / delayed reconfirmations); the in-episode maximum is
//! reported only as a labelled hindsight upper bound.
//!
//! **Never builds/signs/simulates/submits.** Read-only. Costs are MODELED.
//!
//! Usage: cargo run -p arb-monitor --bin observe-narrow --cache narrow-routes.json
//! Env: RPC_ENDPOINT (redacted), NARROW_INTERVAL_SECS (target PERIOD, 3),
//!      OBS_DURATION_SECS (86400), OBS_MAX_SOL (20), OBS_OUT_DIR (reports/narrow),
//!      NARROW_FROZEN_SECS (600), NARROW_FROZEN_CONTROLS (csv token mints).

use anyhow::{Context, Result};
use arb_common::cost::CostModel;
use arb_monitor::market_discovery::DiscoveryCache;
use arb_monitor::narrow_report::{aggregate_narrow, PollEvent};
use arb_monitor::observe_live::{
    cluster_time, env_u64, fetch_snapshot, git_commit, gzip, install_shutdown, now_ms, reconfirm,
    routes_for, secrets_from_env, Ctx,
};
use arb_monitor::observe_report::competitive_model;
use arb_monitor::observe_report::default_scenarios;
use arb_monitor::optimizer::{optimize, size_analysis, SizeGrid};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use std::collections::BTreeMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::info;

const FROZEN_PROBE_LAMPORTS: u64 = 100_000_000; // 0.1 SOL

/// An open episode awaiting reconfirmations.
struct Open {
    start_ms: u64,
    targets: Vec<u64>, // remaining delay milestones (ms)
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    let cache_path = args
        .iter()
        .position(|a| a == "--cache")
        .and_then(|i| args.get(i + 1).cloned())
        .unwrap_or_else(|| "narrow-routes.json".into());

    let interval = Duration::from_secs(env_u64("NARROW_INTERVAL_SECS", 3));
    let duration = Duration::from_secs(env_u64("OBS_DURATION_SECS", 86_400));
    let max_sol: f64 = std::env::var("OBS_MAX_SOL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20.0);
    let frozen_secs = env_u64("NARROW_FROZEN_SECS", 600);
    let out_dir = std::env::var("OBS_OUT_DIR").unwrap_or_else(|_| "reports/narrow".into());
    std::fs::create_dir_all(&out_dir).ok();

    let rpc_url = std::env::var("RPC_ENDPOINT").context("RPC_ENDPOINT required")?;
    let secrets_owned = secrets_from_env(&rpc_url);
    let secrets: Vec<&str> = secrets_owned.iter().map(|s| s.as_str()).collect();
    let rpc = RpcClient::new_with_commitment(rpc_url.clone(), CommitmentConfig::confirmed());
    let ctx = Ctx::new()?;

    let raw = std::fs::read_to_string(&cache_path)
        .with_context(|| format!("read narrow route set {cache_path}"))?;
    let cache = DiscoveryCache::from_json(&raw).context("narrow-routes cache version mismatch")?;
    let token_of: BTreeMap<String, String> = cache
        .markets
        .iter()
        .map(|m| (m.pump_pool.clone(), m.token_mint.clone()))
        .collect();
    let control_tokens: Vec<String> = std::env::var("NARROW_FROZEN_CONTROLS")
        .unwrap_or_else(|_| "4kKa5c1RSvE6eHc3YvxgNqqgsyg39cwguXkjTPYXpump".into())
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let run_id = now_ms();
    let jsonl_path = format!("{out_dir}/polls-{run_id}.jsonl");
    let report_path = format!("{out_dir}/report-{run_id}.json");
    let mut jsonl = std::fs::File::create(&jsonl_path)?;
    let shutdown = Arc::new(AtomicBool::new(false));
    install_shutdown(shutdown.clone());
    info!(routes = cache.markets.len(), commit = %git_commit(),
          target_period_s = interval.as_secs(), "observe-narrow starting — fast poll, NEVER submits");

    let cost = competitive_model(); // episodes defined by COMPETITIVE net ≥ 0
    let grid = SizeGrid {
        min: 10_000_000,
        max: (max_sol * 1e9) as u64,
        ..Default::default()
    };
    let scenarios: Vec<(String, CostModel)> = default_scenarios()
        .into_iter()
        .map(|s| (s.label.clone(), s.model()))
        .collect();

    let mut events: Vec<PollEvent> = Vec::new();
    let mut open: BTreeMap<String, Open> = BTreeMap::new();
    // Interval semantics: sweep + sleep should target `interval` PERIOD.
    let mut sweep_samples: Vec<u64> = Vec::new();
    let mut sleep_samples: Vec<u64> = Vec::new();
    let mut tip_tier_shaped = 0u64;
    let run_start = Instant::now();
    let mut last_checkpoint = Instant::now();
    let mut poll_count = 0u64;
    let mut rpc_failures = 0u64;

    loop {
        let sweep_start = Instant::now();
        let now_unix = cluster_time(&rpc).await;
        for m in &cache.markets {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            poll_count += 1;
            let snap = match fetch_snapshot(&rpc, &ctx, m, now_unix, &secrets).await {
                Ok(s) => s,
                Err(_) => {
                    rpc_failures += 1;
                    open.remove(&route_key(m, "meteora->pump")); // unavailable ⇒ close
                    continue;
                }
            };
            let latency = snap.rpc_latency_ms;
            let slot = snap.slot;
            // Only the live direction (pump-first is a creator-BUY, refused).
            let route = match routes_for(snap)
                .into_iter()
                .find(|(l, _)| *l == "meteora->pump")
            {
                Some((_, r)) if r.token_mint(&ctx.wsol).is_some() => r,
                _ => continue,
            };
            let key = route_key(m, "meteora->pump");
            let at_ms = now_ms();

            let fingerprint = route
                .round_trip(&ctx.wsol, FROZEN_PROBE_LAMPORTS)
                .map(|(_, out)| out as i128 - FROZEN_PROBE_LAMPORTS as i128)
                .unwrap_or(i128::MIN);
            let (profitable, gross, net, size) = match optimize(&route, &ctx.wsol, &cost, &grid) {
                Some(c) => (c.net_profit >= 0, c.gross_profit, c.net_profit, c.amount_in),
                None => (false, 0, 0, 0),
            };
            let ev = PollEvent {
                route: key.clone(),
                at_ms,
                slot,
                kind: "poll".into(),
                profitable_competitive: profitable,
                gross_lamports: gross,
                competitive_net_lamports: net,
                size_lamports: size,
                fingerprint,
                snapshot_latency_ms: latency,
                reconfirm_delay_ms: None,
                episode_start_ms: None,
            };
            writeln!(jsonl, "{}", serde_json::to_string(&ev)?)?;
            events.push(ev);

            // Episode transitions (drive reconfirm scheduling only).
            match (open.contains_key(&key), profitable) {
                (false, true) => {
                    // New episode — record its size analysis once (optimizer
                    // correction / tip-tier check).
                    if let Some(sa) = size_analysis(&route, &ctx.wsol, &grid, &scenarios) {
                        if sa.tip_tier_shaped {
                            tip_tier_shaped += 1;
                        }
                    }
                    open.insert(
                        key.clone(),
                        Open {
                            start_ms: at_ms,
                            targets: vec![2_000, 10_000, 30_000],
                        },
                    );
                }
                (true, false) => {
                    open.remove(&key);
                }
                _ => {}
            }

            // Due reconfirmations for this route's open episode.
            let due: Vec<u64> = open
                .get(&key)
                .map(|o| {
                    o.targets
                        .iter()
                        .copied()
                        .filter(|&t| now_ms().saturating_sub(o.start_ms) >= t)
                        .collect()
                })
                .unwrap_or_default();
            for target in due {
                let start = open.get(&key).map(|o| o.start_ms).unwrap_or(at_ms);
                let cf = reconfirm(
                    &rpc,
                    &ctx,
                    m,
                    now_unix,
                    &secrets,
                    "meteora->pump",
                    &cost,
                    &grid,
                    start,
                )
                .await;
                let ev = PollEvent {
                    route: key.clone(),
                    at_ms: now_ms(),
                    slot: cf.context_slot,
                    kind: "reconfirm".into(),
                    profitable_competitive: cf.survived,
                    gross_lamports: cf.gross_profit_lamports,
                    competitive_net_lamports: cf.net_profit_lamports,
                    size_lamports: 0,
                    fingerprint: 0,
                    snapshot_latency_ms: 0,
                    reconfirm_delay_ms: Some(cf.delay_ms),
                    episode_start_ms: Some(start),
                };
                writeln!(jsonl, "{}", serde_json::to_string(&ev)?)?;
                events.push(ev);
                if let Some(o) = open.get_mut(&key) {
                    o.targets.retain(|&t| t != target);
                }
            }
        }
        jsonl.flush().ok();

        let sweep_ms = sweep_start.elapsed().as_millis() as u64;
        sweep_samples.push(sweep_ms);
        // Interval fix: `interval` is the target PERIOD. Sleep the remainder.
        let sleep_ms = (interval.as_millis() as u64).saturating_sub(sweep_ms);
        sleep_samples.push(sleep_ms);

        if last_checkpoint.elapsed() >= Duration::from_secs(3600) {
            write_report(
                &report_path,
                run_id,
                &events,
                &token_of,
                &control_tokens,
                frozen_secs,
                poll_count,
                rpc_failures,
                run_start.elapsed(),
                interval,
                &sweep_samples,
                &sleep_samples,
                tip_tier_shaped,
                true,
            )?;
            info!(report = %report_path, "hourly checkpoint written");
            last_checkpoint = Instant::now();
        }
        if shutdown.load(Ordering::Relaxed) || run_start.elapsed() >= duration {
            break;
        }
        let woke = Instant::now();
        while (woke.elapsed().as_millis() as u64) < sleep_ms {
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
        &events,
        &token_of,
        &control_tokens,
        frozen_secs,
        poll_count,
        rpc_failures,
        run_start.elapsed(),
        interval,
        &sweep_samples,
        &sleep_samples,
        tip_tier_shaped,
        partial,
    )?;
    gzip(&report_path);
    gzip(&jsonl_path);

    let m = aggregate_narrow(&events, &token_of, &control_tokens, frozen_secs);
    println!(
        "\n════ NARROW OBSERVE {} ════",
        if partial {
            "STOPPED (partial)"
        } else {
            "COMPLETE"
        }
    );
    println!(
        "episodes={} /day={:.0} | active_routes={} class(A/Fl/Fr/N)={}/{}/{}/{}",
        m.episodes_total,
        m.episodes_per_day,
        m.independently_active_routes,
        m.class_active,
        m.class_flicker,
        m.class_frozen_spread,
        m.class_never_profitable
    );
    println!(
        "CAUSAL/day (lamports): detect={} +2s={} +10s={} +30s={} | hindsight_UB={}",
        m.causal_at_detection_per_day_lamports,
        m.causal_plus2s_per_day_lamports,
        m.causal_plus10s_per_day_lamports,
        m.causal_plus30s_per_day_lamports,
        m.hindsight_upper_bound_per_day_lamports
    );
    println!(
        "top3 causal share={}% | report: {report_path}(.gz)",
        m.top3_causal_share_pct
    );
    Ok(())
}

fn route_key(m: &arb_monitor::market_discovery::DiscoveredMarket, dir: &str) -> String {
    format!("{}|{}|{}", m.pump_pool, m.dlmm_pair, dir)
}

fn mean(v: &[u64]) -> u64 {
    if v.is_empty() {
        0
    } else {
        v.iter().sum::<u64>() / v.len() as u64
    }
}

#[allow(clippy::too_many_arguments)]
fn write_report(
    path: &str,
    run_id: u64,
    events: &[PollEvent],
    token_of: &BTreeMap<String, String>,
    controls: &[String],
    frozen_secs: u64,
    poll_count: u64,
    rpc_failures: u64,
    elapsed: Duration,
    interval: Duration,
    sweep: &[u64],
    sleep: &[u64],
    tip_tier_shaped: u64,
    partial: bool,
) -> Result<()> {
    let m = aggregate_narrow(events, token_of, controls, frozen_secs);
    let sweep_ms = mean(sweep);
    let sleep_ms = mean(sleep);
    let report = serde_json::json!({
        "run": {
            "id": run_id, "commit": git_commit(), "partial": partial,
            "duration_secs": elapsed.as_secs(), "target_period_secs": interval.as_secs(),
            "frozen_control_tokens": controls, "frozen_secs": frozen_secs,
            "cost_basis": "modeled (competitive scenario) — no tx built or simulated",
        },
        "cadence": {
            "target_period_ms": interval.as_millis() as u64,
            "mean_sweep_ms": sweep_ms,
            "mean_sleep_ms": sleep_ms,
            "effective_period_ms": sweep_ms + sleep_ms,
            "note": "period = sweep(work over all routes incl. due reconfirms) + sleep(remainder of target)",
            "poll_count": poll_count,
            "rpc_failures": rpc_failures,
            "rpc_failure_rate": rpc_failures as f64 / poll_count.max(1) as f64,
        },
        "headline_causal": {
            "primary": "value at detection and delayed reconfirmations — the only capturable figures",
            "at_detection_per_day_lamports": m.causal_at_detection_per_day_lamports,
            "plus2s_per_day_lamports": m.causal_plus2s_per_day_lamports,
            "plus10s_per_day_lamports": m.causal_plus10s_per_day_lamports,
            "plus30s_per_day_lamports": m.causal_plus30s_per_day_lamports,
            "hindsight_upper_bound_per_day_lamports": m.hindsight_upper_bound_per_day_lamports,
            "hindsight_note": "NOT causally capturable — the best net seen mid-episode; upper bound only",
        },
        "metrics": m,
        "optimizer_correction": {
            "tip_tier_shaped_episodes": tip_tier_shaped,
            "note": "episodes whose competitive net-optimal size sits just below a Jito-tip tier boundary",
        },
        "provisional_gates": {
            "consider_simulation_parity_if": "≥10 competitive-positive episodes/day AND meaningful +10s survival AND multiple active routes AND ≥~0.1 SOL/day CAUSAL competitive value",
            "note": "modeled costs; a candidate is a monitor signal, not a fill",
        },
    });
    std::fs::write(path, serde_json::to_string_pretty(&report)?)?;
    Ok(())
}
