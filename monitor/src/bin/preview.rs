//! arb-preview — long-running dry-run of the discovery engine against LIVE
//! mainnet via plain RPC polling (no Yellowstone Geyser).
//!
//! Feasibility probe, NOT a trading loop: no keypair, no submit, no Jito, no
//! Redis, nothing is ever sent. It runs the SAME registry + DiscoveryEngine +
//! exact tick-array Whirlpool quoting as production, sourcing pool state by
//! periodic `getMultipleAccounts`. Use it to see, over hours, whether a pool
//! set produces (exact-math-valid) opportunities before paying for Geyser.
//!
//! Suitable for multi-hour runs: auto-stops after `PREVIEW_DURATION_SECS`,
//! refreshes tick-array coverage as prices drift, survives RPC rate limits
//! with backoff, and writes a cumulative JSON + human report at the end (and
//! on Ctrl-C).
//!
//! Env:
//!   RPC_ENDPOINT              use a private/paid RPC for long runs — the
//!                             public one WILL rate-limit across hours
//!   POOLS_FILE                pools.generated.json etc (default pools.json)
//!   PREVIEW_DURATION_SECS     auto-stop after N seconds (0 = until Ctrl-C)
//!   PREVIEW_POLL_INTERVAL_MS  default 3000 (raise for public RPC)
//!   PREVIEW_MIN_PROFIT_BPS    override discovery threshold
//!   PREVIEW_MAX_POOLS         cap pools loaded
//!   PREVIEW_TICK_REFRESH_SECS re-fetch tick arrays every N s (default 300)
//!   PREVIEW_REPORT_FILE       JSON report path (default preview-report.json)

use anyhow::{Context, Result};
use arb_monitor::bootstrap::bootstrap_registry;
use arb_monitor::config::MonitorConfig;
use arb_monitor::discovery::DiscoveryEngine;
use arb_monitor::quote::{quote_pool_detailed, QuoteOutcome};
use arb_monitor::registry::{now_ms, PoolRegistry};
use arb_monitor::types::PoolState;
use serde_json::json;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
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
    opps: HashMap<String, OppRecord>,
    /// opportunities emitted per wall-clock hour bucket since start.
    hourly: Vec<u64>,
}

async fn poll_accounts(
    rpc: &RpcClient,
    watched: &[Pubkey],
) -> Result<(u64, Vec<(Pubkey, Vec<u8>)>)> {
    // Fetch the 100-key chunks concurrently — on a slow/throttled RPC this
    // cuts poll latency roughly by the number of chunks (critical for a
    // multi-hour run with hundreds of watched accounts).
    let futures = watched.chunks(100).map(|chunk| async move {
        let resp = rpc
            .get_multiple_accounts_with_commitment(chunk, CommitmentConfig::processed())
            .await
            .context("getMultipleAccounts")?;
        Ok::<_, anyhow::Error>((chunk, resp))
    });
    let results = futures::future::try_join_all(futures).await?;
    let mut slot = 0u64;
    let mut out = Vec::with_capacity(watched.len());
    for (chunk, resp) in results {
        slot = slot.max(resp.context.slot);
        for (pk, acc) in chunk.iter().zip(resp.value) {
            if let Some(acc) = acc {
                out.push((*pk, acc.data));
            }
        }
    }
    Ok((slot, out))
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

    // Most persistent cycles (high count over long span = likely still a
    // pricing quirk to investigate, NOT a real repeatable profit).
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
        "  pools / routes:     {} pools, {} tokens",
        registry.pools.len(),
        registry.tokens.len()
    );
    println!(
        "  polls:              {} ok, {} failed ({poll_success:.1}% success)",
        report.polls_ok, report.polls_failed
    );
    println!("  whirlpool quotes:   {ok} exact-quotable, {missing} missing-ticks, {beyond} beyond-coverage");
    println!("  min profit gate:    {} bps", cfg.min_profit_bps);
    println!("  unique cycles seen: {}", report.opps.len());
    let total_opps: u64 = report.opps.values().map(|r| r.count).sum();
    println!("  total opportunity emissions: {total_opps}");

    if report.opps.is_empty() {
        println!("\n  NO opportunities surfaced. With exact quoting this is the expected");
        println!("  result at poll latency on efficient pools — real cyclic arb is");
        println!("  sub-second. It means: no fake persistent edges, and nothing this");
        println!("  pool set + poll cadence can catch. Geyser speed is what would let");
        println!("  you race for genuine fleeting edges (if any exist here).");
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
        println!("\n  NOTE: a cycle that persisted for many seconds/minutes is almost");
        println!("  certainly NOT a real repeatable profit — validate any candidate with");
        println!("  on-chain simulateTransaction before trusting it.");
    }

    let json = json!({
        "runtime_secs": runtime_s,
        "pools": registry.pools.len(),
        "tokens": registry.tokens.len(),
        "polls_ok": report.polls_ok,
        "polls_failed": report.polls_failed,
        "poll_success_pct": poll_success,
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
    let interval = Duration::from_millis(env_u64(
        "PREVIEW_POLL_INTERVAL_MS",
        env_u64("POLL_INTERVAL_MS", 3000),
    ));
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
        warn!("public RPC + long run: expect heavy rate-limiting. Use a private RPC (Helius/Triton/QuickNode free tier) and/or raise PREVIEW_POLL_INTERVAL_MS.");
    }
    info!(
        pools = cfg.pools.len(),
        poll_ms = interval.as_millis() as u64,
        duration_secs,
        tick_refresh_secs = tick_refresh.as_secs(),
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

    let started_ms = now_ms();
    let mut report = Report {
        hourly: vec![0],
        ..Default::default()
    };
    let mut ticker = tokio::time::interval(interval);
    let mut refresh_ticker = tokio::time::interval(tick_refresh);
    refresh_ticker.tick().await; // consume immediate first tick
    let mut stats_ticker = tokio::time::interval(Duration::from_secs(60));
    stats_ticker.tick().await;
    let deadline = if duration_secs > 0 {
        Some(tokio::time::Instant::now() + Duration::from_secs(duration_secs))
    } else {
        None
    };
    let mut backoff_until: Option<tokio::time::Instant> = None;

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if let Some(b) = backoff_until {
                    if tokio::time::Instant::now() < b { continue; }
                    backoff_until = None;
                }
                let (slot, accounts) = match poll_accounts(&rpc, &watched).await {
                    Ok(v) => { report.polls_ok += 1; v }
                    Err(e) => {
                        report.polls_failed += 1;
                        warn!(error = %e, "poll failed — backing off 10s (rate limit? use a private RPC)");
                        backoff_until = Some(tokio::time::Instant::now() + Duration::from_secs(10));
                        continue;
                    }
                };
                for (pk, data) in &accounts {
                    registry.apply_account_update(*pk, data, slot);
                }
                for addr in registry.pools.keys().copied().collect::<Vec<_>>() {
                    engine.mark_dirty(addr);
                }
                let now = now_ms();
                let hour = ((now - started_ms) / 3_600_000) as usize;
                while report.hourly.len() <= hour { report.hourly.push(0); }
                for opp in engine.run_search(&registry, &cfg) {
                    report.hourly[hour] += 1;
                    let base = opp.base_symbol.clone().unwrap_or_default();
                    let bps = opp.net_profit_bps as u64;
                    let rec = report.opps.entry(opp.id.clone()).or_insert(OppRecord {
                        base: base.clone(), hops: opp.hops.len(), count: 0,
                        best_net: 0, best_bps: 0, first_ms: now, last_ms: now,
                    });
                    if rec.count == 0 {
                        info!(id = %opp.id, base = %base, hops = opp.hops.len(), net = opp.net_profit, bps, "NEW cycle candidate");
                    }
                    rec.count += 1;
                    rec.last_ms = now;
                    rec.best_net = rec.best_net.max(opp.net_profit);
                    rec.best_bps = rec.best_bps.max(bps);
                    let _ = slot;
                }
            }
            _ = refresh_ticker.tick() => {
                let tas = registry.rebuild_whirlpool_tick_arrays();
                match poll_accounts(&rpc, &tas).await {
                    Ok((_, accts)) => {
                        for (pk, data) in &accts { registry.apply_account_update(*pk, data, now_ms()); }
                        watched = registry.all_watched_accounts();
                        let (ok, missing, beyond) = whirlpool_health(&registry);
                        info!(accounts = watched.len(), quotable = ok, missing, beyond, "tick-array coverage refreshed");
                    }
                    Err(e) => warn!(error = %e, "tick refresh fetch failed"),
                }
            }
            _ = stats_ticker.tick() => {
                let rt = (now_ms() - started_ms) / 1000;
                info!(
                    runtime_s = rt, polls_ok = report.polls_ok, polls_failed = report.polls_failed,
                    unique_cycles = report.opps.len(),
                    total = report.opps.values().map(|r| r.count).sum::<u64>(),
                    "progress"
                );
            }
            _ = tokio::signal::ctrl_c() => { info!("Ctrl-C — finalizing report"); break; }
            _ = async { if let Some(d) = deadline { tokio::time::sleep_until(d).await } else { std::future::pending().await } } => {
                info!("duration reached — finalizing report");
                break;
            }
        }
    }

    print_and_write_report(&report, &registry, &cfg, started_ms, &report_file);
    Ok(())
}
