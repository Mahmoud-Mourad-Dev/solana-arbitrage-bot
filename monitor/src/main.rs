//! arb-monitor — Rust price monitor (Phase B).
//!
//! Geyser account stream -> parsers -> registry -> discovery -> Redis
//! PUBLISH `arbitrage_opportunities`, emitting the SAME JSON the TypeScript
//! monitor emits. Single-writer registry on the stream task keeps the hot
//! path lock-free.

use anyhow::Result;
use arb_monitor::bootstrap::bootstrap_registry;
use arb_monitor::config::MonitorConfig;
use arb_monitor::discovery::DiscoveryEngine;
use arb_monitor::geyser::{extract_account_update, open_stream};
use arb_monitor::redis_sink::RedisSink;
use arb_monitor::registry::PoolRegistry;
use futures::StreamExt;
use solana_sdk::pubkey::Pubkey;
use std::time::Duration;
use tracing::{error, info, warn};

const STATS_INTERVAL: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cfg = MonitorConfig::from_env(true)?;
    info!(
        pools = cfg.pools.len(),
        max_hops = cfg.max_hops,
        "arb-monitor starting"
    );

    // 1) Hydrate the registry over plain RPC.
    let mut registry = PoolRegistry::new();
    bootstrap_registry(&cfg, &mut registry).await?;
    info!(
        pools = registry.pools.len(),
        tokens = registry.tokens.len(),
        "registry hydrated"
    );

    // 2) Precompute the cycle index.
    let mut engine = DiscoveryEngine::new(cfg.opportunity_cooldown_ms);
    engine.build_cycle_index(&registry, &cfg);
    info!(routes = engine.route_count(), "cycle index built");
    if engine.route_count() == 0 {
        warn!("no cycles found across configured pools — nothing to monitor");
    }

    // 3) Connect Redis publisher.
    let mut sink = RedisSink::connect(
        &cfg.redis_url,
        cfg.opportunity_channel.clone(),
        cfg.opportunity_list.clone(),
        cfg.opportunity_list_max,
    )
    .await?;
    info!(channel = %cfg.opportunity_channel, "redis publisher ready");

    // 4) Open the Geyser stream (library handles reconnect/dedup).
    let watched: Vec<String> = registry
        .all_watched_accounts()
        .iter()
        .map(Pubkey::to_string)
        .collect();
    let mut stream =
        open_stream(&cfg.geyser_endpoint, cfg.geyser_x_token.as_deref(), watched).await?;
    info!("geyser stream open @ processed commitment");

    let mut updates_received: u64 = 0;
    let mut last_stats = std::time::Instant::now();

    while let Some(item) = stream.next().await {
        let update = match item {
            Ok(u) => u,
            Err(e) => {
                error!(error = %e, "geyser stream error");
                continue;
            }
        };
        let Some(acc) = extract_account_update(update) else {
            continue;
        };
        let Ok(pubkey) = Pubkey::try_from(acc.pubkey.as_slice()) else {
            continue;
        };

        updates_received += 1;
        if let Some(pool) = registry.apply_account_update(pubkey, &acc.data, acc.slot) {
            if engine.mark_dirty(pool) {
                for opp in engine.run_search(&registry, &cfg) {
                    let dec = registry
                        .tokens
                        .get(&Pubkey::try_from(opp.base_mint.as_str()).unwrap_or_default())
                        .map(|t| t.decimals)
                        .unwrap_or(0);
                    info!(
                        id = %opp.id,
                        base = %opp.base_symbol.clone().unwrap_or_default(),
                        hops = opp.hops.len(),
                        net = opp.net_profit,
                        bps = opp.net_profit_bps as u64,
                        amount_in = opp.amount_in,
                        slot = opp.slot,
                        decimals = dec,
                        "OPPORTUNITY"
                    );
                    if let Err(e) = sink.publish_opportunity(&opp).await {
                        warn!(error = %e, id = %opp.id, "publish failed");
                    }
                }
            }
        }

        if last_stats.elapsed() >= STATS_INTERVAL {
            let s = &engine.stats;
            info!(
                updates = updates_received,
                searches = s.searches,
                routes_evaluated = s.routes_evaluated,
                opportunities = s.opportunities,
                cooldown_suppressed = s.suppressed_by_cooldown,
                "stats"
            );
            last_stats = std::time::Instant::now();
        }
    }

    warn!("geyser stream ended");
    Ok(())
}
