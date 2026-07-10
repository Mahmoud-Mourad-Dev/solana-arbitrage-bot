//! arb-preview — long-running dry-run of the discovery engine against LIVE
//! mainnet via plain RPC polling (no Yellowstone Geyser).
//!
//! Feasibility probe, NOT a trading loop: no keypair, no submit, no Jito, no
//! Redis, nothing is ever sent. Runs the SAME registry + DiscoveryEngine +
//! exact tick-array Whirlpool quoting as production, sourcing pool state by
//! periodic `getMultipleAccounts`.
//!
//! Consistency guards (P0 fixes) so multi-hour runs never fabricate profit:
//! - each account is stamped with ITS OWN chunk's slot (no max-slot inflation);
//! - a poll whose accounts span too many slots is applied but discovery is
//!   SKIPPED (cross-slot snapshots can invent arbitrage);
//! - discovery rejects any cycle touching a pool older than a freshness floor;
//! - a wall-clock gap (laptop sleep) triggers a full rehydrate and the first
//!   post-gap poll's discovery is discarded;
//! - the tick-array refresh only re-registers PDAs (the main poll fetches them
//!   with real slots) — it never stamps a wall-clock ms as a slot;
//! - stop + duration are wall-clock based (sleep-proof) with a clean Ctrl-C.
//!
//! Env: RPC_ENDPOINT, POOLS_FILE, PREVIEW_DURATION_SECS (0=until Ctrl-C),
//! PREVIEW_POLL_INTERVAL_MS, PREVIEW_MIN_PROFIT_BPS, PREVIEW_MAX_POOLS,
//! PREVIEW_TICK_REFRESH_SECS, PREVIEW_REPORT_FILE.

use anyhow::{Context, Result};
use arb_monitor::bootstrap::bootstrap_registry;
use arb_monitor::config::MonitorConfig;
use arb_monitor::consistency::{
    fresh_floor, is_sleep_gap, slot_spread_ok, DEFAULT_MAX_POOL_SLOT_LAG, DEFAULT_MAX_SLOT_SPREAD,
};
use arb_monitor::discovery::DiscoveryEngine;
use arb_monitor::quote::{quote_pool_detailed, QuoteOutcome};
use arb_monitor::registry::{now_ms, PoolRegistry};
use arb_monitor::types::PoolState;
use serde_json::json;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// One discovered cycle's lifetime stats over the run.
#[derive(Clone)]
struct OppRecord {
    base: String,
    hops: usize,
    count: u64,
    best_net: u64,
    best_bps: u64,
    first_ms: u64,
    last_ms: u64,
}

#[derive(Default)]
struct Report {
    polls_ok: u64,
    polls_failed: u64,
    /// polls where discovery was skipped because chunk slots diverged too far.
    polls_skipped_inconsistent: u64,
    /// wall-clock gaps (laptop sleep) detected; first post-gap poll discarded.
    sleep_gaps: u64,
    /// largest slot spread seen within a single poll.
    max_slot_spread: u64,
    /// raw candidates surfaced by discovery (before the confirmation gate).
    candidates_raw: u64,
    /// candidates dropped because a single-slot re-fetch wasn't possible.
    confirm_rejected_inconsistent: u64,
    /// candidates that vanished on the consistent single-slot re-quote
    /// (cross-slot / stale artifacts — the phantom class).
    confirm_rejected_profit: u64,
    /// confirmation re-fetches that errored (RPC).
    confirm_errors: u64,
    /// CONFIRMED cycles (survived single-slot re-quote) keyed by id.
    opps: HashMap<String, OppRecord>,
    hourly: Vec<u64>,
}

/// A consistent view of one poll: every account carries the slot of the chunk
/// it came from, plus the min/max slot across chunks.
struct PollSnapshot {
    min_slot: u64,
    max_slot: u64,
    accounts: Vec<(Pubkey, Vec<u8>, u64)>,
}

async fn poll_accounts(rpc: &RpcClient, watched: &[Pubkey]) -> Result<PollSnapshot> {
    // Fetch the 100-key chunks concurrently; keep each chunk's context slot so
    // accounts are stamped with their TRUE slot (never the poll-wide max).
    let futures = watched.chunks(100).map(|chunk| async move {
        let resp = rpc
            .get_multiple_accounts_with_commitment(chunk, CommitmentConfig::processed())
            .await
            .context("getMultipleAccounts")?;
        Ok::<_, anyhow::Error>((chunk, resp))
    });
    let results = futures::future::try_join_all(futures).await?;
    let mut min_slot = u64::MAX;
    let mut max_slot = 0u64;
    let mut accounts = Vec::with_capacity(watched.len());
    for (chunk, resp) in results {
        let s = resp.context.slot;
        min_slot = min_slot.min(s);
        max_slot = max_slot.max(s);
        for (pk, acc) in chunk.iter().zip(resp.value) {
            if let Some(acc) = acc {
                accounts.push((*pk, acc.data, s));
            }
        }
    }
    if accounts.is_empty() {
        min_slot = 0;
    }
    Ok(PollSnapshot {
        min_slot,
        max_slot,
        accounts,
    })
}

fn whirlpool_health(registry: &PoolRegistry) -> (u32, u32, u32) {
    let (mut ok, mut missing, mut beyond) = (0u32, 0u32, 0u32);
    let probe = 10u64.pow(9);
    for pool in registry.pools.values() {
        if let PoolState::Whirlpool(w) = pool {
            for mint in [w.common.mint_a, w.common.mint_b] {
                match quote_pool_detailed(pool, &mint, probe) {
                    QuoteOutcome::Ok(_) => ok += 1,
                    QuoteOutcome::WhirlpoolMissingTicks => missing += 1,
                    QuoteOutcome::WhirlpoolBeyondCoverage => beyond += 1,
                    _ => {}
                }
            }
        }
    }
    (ok, missing, beyond)
}

fn print_and_write_report(
    report: &Report,
    registry: &PoolRegistry,
    cfg: &MonitorConfig,
    started_ms: u64,
    report_file: &str,
) {
    let runtime_s = (now_ms().saturating_sub(started_ms)) / 1000;
    let total_polls = report.polls_ok + report.polls_failed;
    let poll_success = if total_polls > 0 {
        report.polls_ok as f64 / total_polls as f64 * 100.0
    } else {
        0.0
    };
    let (ok, missing, beyond) = whirlpool_health(registry);

    let mut by_persist: Vec<&OppRecord> = report.opps.values().collect();
    by_persist.sort_by_key(|r| std::cmp::Reverse(r.count));
    let mut by_profit: Vec<&OppRecord> = report.opps.values().collect();
    by_profit.sort_by_key(|r| std::cmp::Reverse(r.best_net));

    println!("\n═══════════════ arb-preview report ═══════════════");
    println!(
        "  runtime:            {}h {}m {}s",
        runtime_s / 3600,
        (runtime_s % 3600) / 60,
        runtime_s % 60
    );
    println!(
        "  pools / tokens:     {} pools, {} tokens",
        registry.pools.len(),
        registry.tokens.len()
    );
    println!(
        "  polls:              {} ok, {} failed ({poll_success:.1}% success)",
        report.polls_ok, report.polls_failed
    );
    println!(
        "  consistency:        {} polls skipped (slot spread), max spread {} slots",
        report.polls_skipped_inconsistent, report.max_slot_spread
    );
    println!(
        "  sleep/gaps:         {} detected (first post-gap poll discarded each)",
        report.sleep_gaps
    );
    println!("  whirlpool quotes:   {ok} exact-quotable, {missing} missing-ticks, {beyond} beyond-coverage");
    println!("  min profit gate:    {} bps", cfg.min_profit_bps);
    println!(
        "  confirmation gate:  {} raw candidates -> {} CONFIRMED, {} vanished on single-slot re-quote, {} unconfirmable ({} fetch errors)",
        report.candidates_raw,
        report.opps.len(),
        report.confirm_rejected_profit,
        report.confirm_rejected_inconsistent,
        report.confirm_errors,
    );
    println!("  unique CONFIRMED cycles: {}", report.opps.len());
    let total_opps: u64 = report.opps.values().map(|r| r.count).sum();
    println!("  total confirmed emissions:  {total_opps}");

    if report.opps.is_empty() {
        println!("\n  NO opportunities surfaced. With exact quoting + consistency guards this");
        println!("  is the expected result at poll latency on efficient pools — real cyclic");
        println!("  arb is sub-second. It means: no fake edges, and nothing this pool set +");
        println!("  cadence can catch. That is trustworthy evidence, not a failure.");
    } else {
        println!("\n  ── most PERSISTENT cycles (long-lived = investigate, likely quirk) ──");
        for r in by_persist.iter().take(5) {
            let span_s = (r.last_ms.saturating_sub(r.first_ms)) / 1000;
            println!(
                "    {}x  {} {}-hop  best={} bps  persisted {}s",
                r.count, r.base, r.hops, r.best_bps, span_s
            );
        }
        println!("  ── best net profit seen (base-token raw units) ──");
        for r in by_profit.iter().take(5) {
            println!(
                "    {} {}-hop  net={} ({} bps)  seen {}x",
                r.base, r.hops, r.best_net, r.best_bps, r.count
            );
        }
        println!("\n  NOTE: a persistent cycle is almost certainly NOT repeatable profit —");
        println!("  validate any candidate with on-chain simulateTransaction before trusting.");
    }

    let json = json!({
        "runtime_secs": runtime_s,
        "pools": registry.pools.len(),
        "tokens": registry.tokens.len(),
        "polls_ok": report.polls_ok,
        "polls_failed": report.polls_failed,
        "poll_success_pct": poll_success,
        "polls_skipped_inconsistent": report.polls_skipped_inconsistent,
        "max_slot_spread": report.max_slot_spread,
        "sleep_gaps_detected": report.sleep_gaps,
        "candidates_raw": report.candidates_raw,
        "confirmed_cycles": report.opps.len(),
        "confirm_rejected_profit": report.confirm_rejected_profit,
        "confirm_rejected_inconsistent": report.confirm_rejected_inconsistent,
        "confirm_errors": report.confirm_errors,
        "min_profit_bps": cfg.min_profit_bps,
        "whirlpool_quotable": ok,
        "whirlpool_missing_ticks": missing,
        "whirlpool_beyond_coverage": beyond,
        "unique_cycles": report.opps.len(),
        "total_emissions": total_opps,
        "hourly_opportunities": report.hourly,
        "cycles": report.opps.iter().map(|(id, r)| json!({
            "id": id, "base": r.base, "hops": r.hops, "count": r.count,
            "best_net": r.best_net, "best_bps": r.best_bps,
            "first_ms": r.first_ms, "last_ms": r.last_ms,
        })).collect::<Vec<_>>(),
    });
    match std::fs::write(
        report_file,
        serde_json::to_string_pretty(&json).unwrap_or_default() + "\n",
    ) {
        Ok(()) => println!("\n  wrote {report_file}"),
        Err(e) => warn!(error = %e, "failed to write report file"),
    }
    println!("═══════════════════════════════════════════════════\n");
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let mut cfg = MonitorConfig::from_env(false)?;
    let interval_ms = env_u64(
        "PREVIEW_POLL_INTERVAL_MS",
        env_u64("POLL_INTERVAL_MS", 3000),
    );
    let interval = Duration::from_millis(interval_ms);
    let duration_secs = env_u64("PREVIEW_DURATION_SECS", 0);
    let tick_refresh = Duration::from_secs(env_u64("PREVIEW_TICK_REFRESH_SECS", 300).max(30));
    let report_file =
        std::env::var("PREVIEW_REPORT_FILE").unwrap_or_else(|_| "preview-report.json".to_string());
    if let Ok(v) = std::env::var("PREVIEW_MIN_PROFIT_BPS") {
        if let Ok(bps) = v.parse() {
            cfg.min_profit_bps = bps;
        }
    }
    let max_pools = env_u64("PREVIEW_MAX_POOLS", 0) as usize;
    if max_pools > 0 && cfg.pools.len() > max_pools {
        cfg.pools.truncate(max_pools);
    }

    if cfg.rpc_endpoint.contains("api.mainnet-beta.solana.com") && duration_secs > 600 {
        warn!("public RPC + long run: expect rate-limiting. Use a private RPC and/or raise PREVIEW_POLL_INTERVAL_MS.");
    }
    info!(
        pools = cfg.pools.len(),
        poll_ms = interval_ms,
        duration_secs,
        max_slot_spread = DEFAULT_MAX_SLOT_SPREAD,
        max_pool_lag = DEFAULT_MAX_POOL_SLOT_LAG,
        min_profit_bps = cfg.min_profit_bps,
        "arb-preview starting (RPC polling — NO Geyser, NO submission)"
    );

    let mut registry = PoolRegistry::new();
    bootstrap_registry(&cfg, &mut registry).await?;
    info!(
        pools = registry.pools.len(),
        tokens = registry.tokens.len(),
        "registry hydrated from chain"
    );

    let mut engine = DiscoveryEngine::new(cfg.opportunity_cooldown_ms);
    engine.build_cycle_index(&registry, &cfg);
    let routes = engine.route_count();
    info!(routes, "cycle index built");
    if routes == 0 {
        warn!("no cycles across configured pools — pool set too small/disconnected");
        return Ok(());
    }
    let (ok, missing, beyond) = whirlpool_health(&registry);
    info!(
        quotable = ok,
        rejected_missing_ticks = missing,
        rejected_beyond_coverage = beyond,
        "whirlpool exact-quote health"
    );

    let rpc =
        RpcClient::new_with_commitment(cfg.rpc_endpoint.clone(), CommitmentConfig::processed());
    let mut watched = registry.all_watched_accounts();
    info!(
        accounts = watched.len(),
        "polling these accounts each cycle"
    );

    // Clean stop: a Ctrl-C task flips a flag the loop checks (wall-clock based,
    // so it works even after the process was suspended).
    let stop = Arc::new(AtomicBool::new(false));
    {
        let s = stop.clone();
        tokio::spawn(async move {
            let _ = tokio::signal::ctrl_c().await;
            s.store(true, Ordering::SeqCst);
        });
    }

    let started_ms = now_ms();
    let mut last_poll_wall = started_ms;
    let mut report = Report {
        hourly: vec![0],
        ..Default::default()
    };
    let mut ticker = tokio::time::interval(interval);
    let mut refresh_ticker = tokio::time::interval(tick_refresh);
    refresh_ticker.tick().await;
    let mut stats_ticker = tokio::time::interval(Duration::from_secs(60));
    stats_ticker.tick().await;
    let mut poll_n: u64 = 0;
    let mut backoff_until: Option<tokio::time::Instant> = None;

    loop {
        // Wall-clock stop + duration (sleep-proof).
        if stop.load(Ordering::SeqCst) {
            info!("Ctrl-C received — finalizing report");
            break;
        }
        if duration_secs > 0 && now_ms().saturating_sub(started_ms) >= duration_secs * 1000 {
            info!("duration reached (wall clock) — finalizing report");
            break;
        }

        tokio::select! {
            _ = ticker.tick() => {
                if let Some(b) = backoff_until {
                    if tokio::time::Instant::now() < b { continue; }
                    backoff_until = None;
                }

                // Sleep/gap detection BEFORE polling: rehydrate + discard.
                let now = now_ms();
                let gap = now.saturating_sub(last_poll_wall);
                last_poll_wall = now;
                let sleep_gap = poll_n > 0 && is_sleep_gap(gap, interval_ms);
                if sleep_gap {
                    report.sleep_gaps += 1;
                    warn!(gap_ms = gap, "sleep/gap detected — full rehydrate, discarding this poll's discovery");
                    registry.reset_slot_tracking();
                    let _ = registry.rebuild_whirlpool_tick_arrays();
                    watched = registry.all_watched_accounts();
                }

                let snap = match poll_accounts(&rpc, &watched).await {
                    Ok(s) => { report.polls_ok += 1; s }
                    Err(e) => {
                        report.polls_failed += 1;
                        warn!(error = %e, "poll failed — backing off 10s");
                        backoff_until = Some(tokio::time::Instant::now() + Duration::from_secs(10));
                        continue;
                    }
                };
                let spread = snap.max_slot.saturating_sub(snap.min_slot);
                report.max_slot_spread = report.max_slot_spread.max(spread);

                // Apply each account with ITS OWN chunk slot (no max inflation).
                for (pk, data, slot) in &snap.accounts {
                    registry.apply_account_update(*pk, data, *slot);
                }
                poll_n += 1;

                // Discard discovery on the first post-gap poll (state may be
                // partially stale until the next clean poll).
                if sleep_gap { continue; }

                // Cross-slot consistency: don't trust cross-pool cycles when
                // the snapshot spans too many slots.
                if !slot_spread_ok(snap.min_slot, snap.max_slot, DEFAULT_MAX_SLOT_SPREAD) {
                    report.polls_skipped_inconsistent += 1;
                    continue;
                }

                // Freshness floor: reject cycles touching a stale pool.
                let floor = fresh_floor(snap.max_slot, DEFAULT_MAX_POOL_SLOT_LAG);
                for addr in registry.pools.keys().copied().collect::<Vec<_>>() {
                    engine.mark_dirty(addr);
                }
                let hour = ((now - started_ms) / 3_600_000) as usize;
                while report.hourly.len() <= hour { report.hourly.push(0); }

                // Raw candidates from the (already consistency-gated) poll.
                let candidates = engine.run_search(&registry, &cfg, Some(floor));
                for raw in candidates {
                    report.candidates_raw += 1;

                    // ── P1 CONFIRMATION GATE ──────────────────────────────
                    // Re-fetch EXACTLY this cycle's accounts in one call (one
                    // slot, zero cross-slot risk) and re-quote. A candidate
                    // that no longer clears every gate on that consistent
                    // single-slot snapshot was an artifact — drop it.
                    let Some(cycle_pools) = engine.route_pools(&raw.id) else { continue; };
                    let accts = registry.accounts_for_pools(&cycle_pools);
                    let csnap = match poll_accounts(&rpc, &accts).await {
                        Ok(s) => s,
                        Err(e) => { warn!(error = %e, id = %raw.id, "confirm fetch failed"); report.confirm_errors += 1; continue; }
                    };
                    if csnap.max_slot.saturating_sub(csnap.min_slot) > 0 {
                        // Couldn't get a single-slot snapshot — refuse to confirm.
                        report.confirm_rejected_inconsistent += 1;
                        continue;
                    }
                    for (pk, data, slot) in &csnap.accounts {
                        registry.apply_account_update(*pk, data, *slot);
                    }
                    let cfloor = fresh_floor(csnap.max_slot, DEFAULT_MAX_POOL_SLOT_LAG);
                    let confirmed = match engine.evaluate_by_id(&raw.id, &registry, &cfg, Some(cfloor)) {
                        Some(c) => c,
                        None => { report.confirm_rejected_profit += 1; continue; }
                    };

                    // Survived single-slot confirmation → record it.
                    report.hourly[hour] += 1;
                    let base = confirmed.base_symbol.clone().unwrap_or_default();
                    let bps = confirmed.net_profit_bps as u64;
                    let rec = report.opps.entry(confirmed.id.clone()).or_insert(OppRecord {
                        base: base.clone(), hops: confirmed.hops.len(), count: 0,
                        best_net: 0, best_bps: 0, first_ms: now, last_ms: now,
                    });
                    if rec.count == 0 {
                        info!(id = %confirmed.id, base = %base, hops = confirmed.hops.len(), net = confirmed.net_profit, bps, slot = confirmed.slot, "CONFIRMED cycle (single-slot)");
                    }
                    rec.count += 1;
                    rec.last_ms = now;
                    rec.best_net = rec.best_net.max(confirmed.net_profit);
                    rec.best_bps = rec.best_bps.max(bps);
                }
            }
            _ = refresh_ticker.tick() => {
                // Re-derive tick-array PDAs around drifted ticks; the MAIN poll
                // fetches them with real slots (no now_ms poisoning).
                let _ = registry.rebuild_whirlpool_tick_arrays();
                watched = registry.all_watched_accounts();
                let (ok, missing, beyond) = whirlpool_health(&registry);
                info!(accounts = watched.len(), quotable = ok, missing, beyond, "tick-array coverage refreshed");
            }
            _ = stats_ticker.tick() => {
                let rt = (now_ms() - started_ms) / 1000;
                info!(
                    runtime_s = rt, polls_ok = report.polls_ok, polls_failed = report.polls_failed,
                    skipped_inconsistent = report.polls_skipped_inconsistent, sleep_gaps = report.sleep_gaps,
                    unique_cycles = report.opps.len(),
                    total = report.opps.values().map(|r| r.count).sum::<u64>(),
                    "progress"
                );
            }
        }
    }

    print_and_write_report(&report, &registry, &cfg, started_ms, &report_file);
    Ok(())
}
