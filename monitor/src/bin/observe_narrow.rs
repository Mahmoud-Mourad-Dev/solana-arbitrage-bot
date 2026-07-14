//! `observe-narrow` — S13B fast-poll narrow observe experiment.
//!
//! Polls a SMALL curated route set (~10) every few seconds to measure real
//! edge birth/lifetime at near-actionable resolution. An EPISODE is a run of
//! competitive-cost-positive polls: it starts on non-profitable→positive and
//! ends on positive→(non-profitable/invalid/stale). Each new episode is
//! reconfirmed at ~+2s/+10s/+30s. Frozen/dead routes (unchanged slot + byte-
//! identical gross) are detected and excluded from the economic total but kept
//! as controls.
//!
//! **Never builds/signs/simulates/submits.** Read-only. Costs are MODELED.
//!
//! Usage: cargo run -p arb-monitor --bin observe-narrow --cache narrow-routes.json
//! Env: RPC_ENDPOINT (redacted), NARROW_INTERVAL_SECS (3), OBS_DURATION_SECS
//!      (86400), OBS_MAX_SOL (20), OBS_OUT_DIR (reports/narrow),
//!      NARROW_FROZEN_SECS (600 — min age to call a route frozen).

use anyhow::{Context, Result};
use arb_common::cost::CostModel;
use arb_monitor::market_discovery::{DiscoveredMarket, DiscoveryCache};
use arb_monitor::observe_live::{
    cluster_time, env_u64, fetch_snapshot, git_commit, gzip, install_shutdown, now_ms, reconfirm,
    routes_for, secrets_from_env, Ctx,
};
use arb_monitor::observe_report::{competitive_model, default_scenarios, sol, Confirmation};
use arb_monitor::optimizer::{optimize, size_analysis, SizeAnalysis, SizeGrid};
use serde::Serialize;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use std::collections::BTreeMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::info;

/// A poll observation of one route+direction under the competitive model.
struct Poll {
    at_ms: u64,
    slot: u64,
    profitable: bool, // competitive net ≥ 0
    gross: u64,
    net: i128,
    size: u64,
}

/// Classification of a route's behaviour over the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
enum RouteClass {
    /// Active market: profitable episodes with varying gross.
    Active,
    /// Frozen/dead: unchanged slot + byte-identical gross for a long time.
    FrozenSpread,
    /// Only ever a single-poll flicker.
    Flicker,
    /// Never profitable under competitive costs.
    NeverProfitable,
}

/// One competitive-positive episode.
#[derive(Debug, Clone, Serialize)]
struct EpisodeRecord {
    route_key: String,
    token_mint: String,
    pump_pool: String,
    dlmm_pair: String,
    direction: String,
    start_ms: u64,
    end_ms: u64,
    duration_ms: u64,
    sightings: usize,
    max_gross_lamports: u64,
    median_gross_lamports: u64,
    distinct_gross_values: usize,
    max_competitive_net_lamports: i128,
    median_competitive_net_lamports: i128,
    best_input_lamports: u64,
    start_slot: u64,
    end_slot: u64,
    max_snapshot_latency_ms: u64,
    /// +2s / +10s / +30s reconfirmations of THIS episode.
    reconfirms: Vec<Confirmation>,
    /// Optimizer correction: raw curve + net-optimal size per scenario +
    /// whether the competitive optimum is shaped by a tip-tier boundary.
    size_analysis: Option<SizeAnalysis>,
}

/// Mutable builder for an in-progress episode.
struct Building {
    start_ms: u64,
    start_slot: u64,
    grosses: Vec<u64>,
    nets: Vec<i128>,
    sizes: Vec<u64>,
    slots: Vec<u64>,
    max_lat: u64,
    reconfirm_targets: Vec<u64>, // remaining wall-clock targets (ms since epoch)
    reconfirms: Vec<Confirmation>,
    size_analysis: Option<SizeAnalysis>,
}

/// Per-route rolling state.
#[derive(Default)]
struct RouteState {
    building: Option<Building>,
    episodes: Vec<EpisodeRecord>,
    // Frozen detection uses a STALENESS FINGERPRINT — the round-trip gross at a
    // fixed probe size — NOT the RPC context slot (which always advances). A
    // dead pool yields a byte-identical fingerprint every poll. This is
    // independent of competitive profitability, so a frozen-but-unprofitable
    // control is still detected.
    last_fingerprint: Option<i128>,
    identical_since_ms: Option<u64>,
    identical_span_ms: u64,
    ever_profitable: bool,
    total_polls: usize,
}

/// Fixed probe size for the staleness fingerprint (0.1 SOL).
const FROZEN_PROBE_LAMPORTS: u64 = 100_000_000;

fn median_u64(v: &[u64]) -> u64 {
    if v.is_empty() {
        return 0;
    }
    let mut s = v.to_vec();
    s.sort_unstable();
    s[s.len() / 2]
}
fn median_i128(v: &[i128]) -> i128 {
    if v.is_empty() {
        return 0;
    }
    let mut s = v.to_vec();
    s.sort_unstable();
    s[s.len() / 2]
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

    // Roles: which tokens are frozen controls (excluded from economic total).
    let frozen_controls: Vec<String> = std::env::var("NARROW_FROZEN_CONTROLS")
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
    info!(routes = cache.markets.len(), commit = %git_commit(), interval_s = interval.as_secs(),
          "observe-narrow starting — fast poll, NEVER submits");

    let cost = competitive_model(); // episodes are defined by COMPETITIVE net ≥ 0
    let grid = SizeGrid {
        min: 10_000_000,
        max: (max_sol * 1e9) as u64,
        ..Default::default()
    };
    let scenarios: Vec<(String, CostModel)> = default_scenarios()
        .into_iter()
        .map(|s| (s.label.clone(), s.model()))
        .collect();

    // Only meteora->pump is live (pump-first BUY is creator-refused); poll both
    // but state is tracked per (route, direction).
    let mut state: BTreeMap<String, RouteState> = BTreeMap::new();
    let run_start = Instant::now();
    let mut last_checkpoint = Instant::now();
    let mut poll_count = 0u64;
    let mut rpc_failures = 0u64;

    loop {
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
                    // Unavailable ⇒ end any open episode on the live direction.
                    end_episode(&mut state, &route_key(m, "meteora->pump"), &cost, now_ms());
                    continue;
                }
            };
            let latency = snap.rpc_latency_ms;
            let slot = snap.slot;
            for (label, route) in routes_for(snap) {
                // Only meteora->pump is a live direction: pump-first (WSOL→token
                // on PumpSwap) is a creator-pool BUY, which is refused. Skip it
                // rather than pollute the report with structurally-dead entries.
                if label != "meteora->pump" || route.token_mint(&ctx.wsol).is_none() {
                    continue;
                }
                let key = route_key(m, label);
                // Staleness fingerprint: round-trip gross at a fixed probe size,
                // independent of the competitive gate (detects frozen pools even
                // when they are competitively unprofitable, e.g. the control).
                let fingerprint = route
                    .round_trip(&ctx.wsol, FROZEN_PROBE_LAMPORTS)
                    .map(|(_, out)| out as i128 - FROZEN_PROBE_LAMPORTS as i128)
                    .unwrap_or(i128::MIN);
                let poll = match optimize(&route, &ctx.wsol, &cost, &grid) {
                    Some(c) => Poll {
                        at_ms: now_ms(),
                        slot,
                        profitable: c.net_profit >= 0,
                        gross: c.gross_profit,
                        net: c.net_profit,
                        size: c.amount_in,
                    },
                    None => Poll {
                        at_ms: now_ms(),
                        slot,
                        profitable: false,
                        gross: 0,
                        net: 0,
                        size: 0,
                    },
                };
                writeln!(
                    jsonl,
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "route": key, "at_ms": poll.at_ms, "slot": poll.slot,
                        "profitable_competitive": poll.profitable,
                        "gross_lamports": poll.gross, "competitive_net_lamports": poll.net,
                        "size_lamports": poll.size, "snapshot_latency_ms": latency,
                    }))?
                )?;

                let st = state.entry(key.clone()).or_default();
                st.total_polls += 1;
                if poll.profitable {
                    st.ever_profitable = true;
                }
                // Frozen detection: identical staleness fingerprint over time.
                // An uncomputable probe (too thin for 0.1 SOL) is NOT treated as
                // frozen — that is capacity-limited, not a dead spread.
                let identical =
                    fingerprint != i128::MIN && st.last_fingerprint == Some(fingerprint);
                if identical {
                    let since = st.identical_since_ms.get_or_insert(poll.at_ms);
                    st.identical_span_ms = poll.at_ms.saturating_sub(*since);
                } else {
                    st.identical_since_ms = Some(poll.at_ms);
                    st.identical_span_ms = 0;
                }
                st.last_fingerprint = Some(fingerprint);

                // Episode state machine.
                match (&mut st.building, poll.profitable) {
                    (None, true) => {
                        // New episode.
                        let sa = size_analysis(&route, &ctx.wsol, &grid, &scenarios);
                        st.building = Some(Building {
                            start_ms: poll.at_ms,
                            start_slot: poll.slot,
                            grosses: vec![poll.gross],
                            nets: vec![poll.net],
                            sizes: vec![poll.size],
                            slots: vec![poll.slot],
                            max_lat: latency,
                            reconfirm_targets: vec![
                                poll.at_ms + 2_000,
                                poll.at_ms + 10_000,
                                poll.at_ms + 30_000,
                            ],
                            reconfirms: Vec::new(),
                            size_analysis: sa,
                        });
                    }
                    (Some(b), true) => {
                        b.grosses.push(poll.gross);
                        b.nets.push(poll.net);
                        b.sizes.push(poll.size);
                        b.slots.push(poll.slot);
                        b.max_lat = b.max_lat.max(latency);
                    }
                    (Some(_), false) => {
                        end_episode(&mut state, &key, &cost, poll.at_ms);
                    }
                    (None, false) => {}
                }

                // Due reconfirmations for the open episode on this route.
                let due: Vec<u64> = state
                    .get(&key)
                    .and_then(|s| s.building.as_ref())
                    .map(|b| {
                        b.reconfirm_targets
                            .iter()
                            .copied()
                            .filter(|&t| now_ms() >= t)
                            .collect()
                    })
                    .unwrap_or_default();
                for target in due {
                    let started = state
                        .get(&key)
                        .and_then(|s| s.building.as_ref())
                        .map(|b| b.start_ms)
                        .unwrap_or(now_ms());
                    let cf = reconfirm(
                        &rpc, &ctx, m, now_unix, &secrets, label, &cost, &grid, started,
                    )
                    .await;
                    if let Some(b) = state.get_mut(&key).and_then(|s| s.building.as_mut()) {
                        b.reconfirm_targets.retain(|&t| t != target);
                        b.reconfirms.push(cf);
                    }
                }
            }
        }

        // Frozen classification is applied at report time; nothing to do here.
        if last_checkpoint.elapsed() >= Duration::from_secs(3600) {
            let rep = build_report(
                run_id,
                &cache,
                &state,
                &frozen_controls,
                frozen_secs,
                poll_count,
                rpc_failures,
                run_start.elapsed(),
                interval,
                true,
            );
            std::fs::write(&report_path, serde_json::to_string_pretty(&rep)?)?;
            info!(report = %report_path, "hourly checkpoint written");
            last_checkpoint = Instant::now();
        }

        if shutdown.load(Ordering::Relaxed) || run_start.elapsed() >= duration {
            break;
        }
        let woke = Instant::now();
        while woke.elapsed() < interval {
            if shutdown.load(Ordering::Relaxed) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    // Close any open episodes at end of run.
    let keys: Vec<String> = state.keys().cloned().collect();
    for k in keys {
        end_episode(&mut state, &k, &cost, now_ms());
    }

    let partial = shutdown.load(Ordering::Relaxed);
    let rep = build_report(
        run_id,
        &cache,
        &state,
        &frozen_controls,
        frozen_secs,
        poll_count,
        rpc_failures,
        run_start.elapsed(),
        interval,
        partial,
    );
    std::fs::write(&report_path, serde_json::to_string_pretty(&rep)?)?;
    gzip(&report_path);
    gzip(&jsonl_path);

    println!(
        "\n════ NARROW OBSERVE {} ════",
        if partial {
            "STOPPED (partial)"
        } else {
            "COMPLETE"
        }
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&rep["decision_metrics"])?
    );
    println!("report: {report_path}(.gz)  polls: {jsonl_path}(.gz)");
    Ok(())
}

fn route_key(m: &DiscoveredMarket, dir: &str) -> String {
    format!("{}|{}|{}", m.pump_pool, m.dlmm_pair, dir)
}

/// Finalize an open episode (if any) into a completed record.
fn end_episode(
    state: &mut BTreeMap<String, RouteState>,
    key: &str,
    _cost: &CostModel,
    end_ms: u64,
) {
    let Some(st) = state.get_mut(key) else { return };
    let Some(b) = st.building.take() else { return };
    let parts: Vec<&str> = key.split('|').collect();
    let rec = EpisodeRecord {
        route_key: key.to_string(),
        token_mint: String::new(), // filled at report time from cache
        pump_pool: parts.first().unwrap_or(&"").to_string(),
        dlmm_pair: parts.get(1).unwrap_or(&"").to_string(),
        direction: parts.get(2).unwrap_or(&"").to_string(),
        start_ms: b.start_ms,
        end_ms,
        duration_ms: end_ms.saturating_sub(b.start_ms),
        sightings: b.grosses.len(),
        max_gross_lamports: b.grosses.iter().copied().max().unwrap_or(0),
        median_gross_lamports: median_u64(&b.grosses),
        distinct_gross_values: {
            let mut g = b.grosses.clone();
            g.sort_unstable();
            g.dedup();
            g.len()
        },
        max_competitive_net_lamports: b.nets.iter().copied().max().unwrap_or(0),
        median_competitive_net_lamports: median_i128(&b.nets),
        best_input_lamports: b
            .nets
            .iter()
            .zip(&b.sizes)
            .max_by_key(|(n, _)| **n)
            .map(|(_, s)| *s)
            .unwrap_or(0),
        start_slot: b.start_slot,
        end_slot: b.slots.last().copied().unwrap_or(b.start_slot),
        max_snapshot_latency_ms: b.max_lat,
        reconfirms: b.reconfirms,
        size_analysis: b.size_analysis,
    };
    st.episodes.push(rec);
}

fn classify(st: &RouteState, frozen_secs: u64) -> RouteClass {
    // Frozen FIRST and independent of profitability: an unchanging staleness
    // fingerprint for a long stretch means the pool is dead (a structural
    // spread), whether or not it clears competitive costs.
    if st.identical_span_ms >= frozen_secs * 1000 {
        return RouteClass::FrozenSpread;
    }
    if !st.ever_profitable {
        return RouteClass::NeverProfitable;
    }
    let multi = st.episodes.iter().filter(|e| e.sightings > 1).count();
    if multi == 0 {
        RouteClass::Flicker
    } else {
        RouteClass::Active
    }
}

#[allow(clippy::too_many_arguments)]
fn build_report(
    run_id: u64,
    cache: &DiscoveryCache,
    state: &BTreeMap<String, RouteState>,
    frozen_controls: &[String],
    frozen_secs: u64,
    poll_count: u64,
    rpc_failures: u64,
    elapsed: Duration,
    interval: Duration,
    partial: bool,
) -> serde_json::Value {
    let token_of: BTreeMap<String, String> = cache
        .markets
        .iter()
        .map(|m| (m.pump_pool.clone(), m.token_mint.clone()))
        .collect();
    let is_control = |token: &str| frozen_controls.iter().any(|c| c == token);

    // Flatten episodes, attach token, classify routes.
    let mut episodes: Vec<EpisodeRecord> = Vec::new();
    let mut classes: BTreeMap<String, RouteClass> = BTreeMap::new();
    for (key, st) in state {
        let pump = key.split('|').next().unwrap_or("");
        let token = token_of.get(pump).cloned().unwrap_or_default();
        classes.insert(key.clone(), classify(st, frozen_secs));
        for e in &st.episodes {
            let mut e = e.clone();
            e.token_mint = token.clone();
            episodes.push(e);
        }
    }

    let hours = (elapsed.as_secs_f64() / 3600.0).max(1e-9);
    // Survival at each reconfirm milestone (nearest to 2/10/30 s).
    let survived_at = |lo: u64, hi: u64| -> usize {
        episodes
            .iter()
            .filter(|e| {
                e.reconfirms
                    .iter()
                    .any(|c| c.delay_ms >= lo && c.delay_ms < hi && c.survived)
            })
            .count()
    };
    let mut lifetimes: Vec<u64> = episodes.iter().map(|e| e.duration_ms).collect();
    lifetimes.sort_unstable();
    let pctl = |v: &[u64], p: f64| -> u64 {
        if v.is_empty() {
            0
        } else {
            v[(((v.len() - 1) as f64) * p).round() as usize]
        }
    };

    // Economics: one capture per episode, EXCLUDING frozen controls, only
    // competitive-positive episodes on active routes.
    let mut capturable: i128 = 0;
    let mut positive_episodes = 0usize;
    let mut active_routes: std::collections::BTreeSet<String> = Default::default();
    for e in &episodes {
        if is_control(&e.token_mint) {
            continue;
        }
        if e.max_competitive_net_lamports > 0 {
            capturable += e.max_competitive_net_lamports;
            positive_episodes += 1;
            active_routes.insert(e.route_key.clone());
        }
    }
    let per_day = capturable as f64 / hours * 24.0;

    let class_counts = |want: RouteClass| classes.values().filter(|&&c| c == want).count();
    let tier_shaped = episodes
        .iter()
        .filter(|e| {
            e.size_analysis
                .as_ref()
                .map(|s| s.tip_tier_shaped)
                .unwrap_or(false)
        })
        .count();

    serde_json::json!({
        "run": {
            "id": run_id, "commit": git_commit(), "partial": partial,
            "routes": cache.markets.len(), "duration_secs": elapsed.as_secs(),
            "poll_interval_secs": interval.as_secs(),
            "frozen_control_tokens": frozen_controls,
            "cost_basis": "modeled (competitive scenario) — no tx built or simulated",
        },
        "route_classification": classes.iter().map(|(k,c)| {
            let pump = k.split('|').next().unwrap_or("");
            serde_json::json!({"route": k, "token": token_of.get(pump), "class": c,
                               "is_frozen_control": is_control(token_of.get(pump).map(|s|s.as_str()).unwrap_or(""))})
        }).collect::<Vec<_>>(),
        "class_totals": {
            "active": class_counts(RouteClass::Active),
            "frozen_spread": class_counts(RouteClass::FrozenSpread),
            "flicker": class_counts(RouteClass::Flicker),
            "never_profitable": class_counts(RouteClass::NeverProfitable),
        },
        "decision_metrics": {
            "unique_episodes_per_day": episodes.len() as f64 / hours * 24.0,
            "episodes_total": episodes.len(),
            "episodes_survived_plus2s": survived_at(1_000, 6_000),
            "episodes_survived_plus10s": survived_at(6_000, 20_000),
            "episodes_survived_plus30s": survived_at(20_000, 120_000),
            "episode_lifetime_p50_ms": pctl(&lifetimes, 0.5),
            "episode_lifetime_p90_ms": pctl(&lifetimes, 0.9),
            "episode_lifetime_max_ms": lifetimes.last().copied().unwrap_or(0),
            "one_capture_per_episode_competitive_lamports_total": capturable,
            "one_capture_per_episode_competitive_per_day_lamports": per_day as i128,
            "one_capture_per_episode_competitive_per_day_sol": sol(per_day as i128),
            "positive_episodes_excl_controls": positive_episodes,
            "independently_active_profitable_routes": active_routes.len(),
            "tip_tier_shaped_episodes": tier_shaped,
            "poll_count": poll_count,
            "rpc_failures": rpc_failures,
            "rpc_failure_rate": rpc_failures as f64 / poll_count.max(1) as f64,
            "polls_per_sec": poll_count as f64 / elapsed.as_secs_f64().max(1e-9),
        },
        "provisional_gates": {
            "consider_simulation_parity_if": "≥10 competitive-positive episodes/day AND meaningful +10s survival AND multiple active routes AND ≥~0.1 SOL/day one-capture competitive value",
            "execution_layer_not_justified_below": "~0.3–0.5 SOL/day after realistic MEASURED costs (requires later simulation evidence)",
            "note": "These figures are MODELED, not measured. A candidate is a monitor signal, not a fill.",
        },
        "episodes": episodes,
    })
}
