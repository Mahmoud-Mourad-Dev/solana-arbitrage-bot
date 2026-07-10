//! The monitor pipeline, extracted so it can drive either the standalone
//! `arb-monitor` binary (publishing to Redis) or the fused `arb-bot` binary
//! (feeding an in-process channel, Redis optional for observability).
//!
//! Geyser account stream -> parsers -> registry -> discovery -> [`OpportunitySink`].

use anyhow::Result;
use arb_common::opportunity::Opportunity;
use futures::StreamExt;
use solana_sdk::pubkey::Pubkey;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::bootstrap::bootstrap_registry;
use crate::config::MonitorConfig;
use crate::discovery::DiscoveryEngine;
use crate::geyser::{extract_account_update, open_stream};
use crate::redis_sink::RedisSink;
use crate::registry::PoolRegistry;

const STATS_INTERVAL: Duration = Duration::from_secs(30);

/// Where discovered opportunities go. The hot path never blocks: the channel
/// variants use `try_send`, dropping under backpressure (a stale opportunity
/// is worthless anyway) rather than stalling Geyser ingestion.
pub enum OpportunitySink {
    /// Standalone monitor: publish to Redis (LPUSH + PUBLISH).
    Redis(RedisSink),
    /// Fused binary: hand off in-process to the executor task.
    Channel(mpsc::Sender<Opportunity>),
    /// Fused binary with Redis mirror for observability (off hot path).
    ChannelWithRedis(mpsc::Sender<Opportunity>, RedisSink),
}

impl OpportunitySink {
    async fn emit(&mut self, opp: Opportunity) {
        match self {
            OpportunitySink::Redis(sink) => {
                if let Err(e) = sink.publish_opportunity(&opp).await {
                    warn!(error = %e, id = %opp.id, "redis publish failed");
                }
            }
            OpportunitySink::Channel(tx) => Self::try_channel(tx, opp),
            OpportunitySink::ChannelWithRedis(tx, sink) => {
                // Mirror to Redis first (clone), then hand off to executor.
                if let Err(e) = sink.publish_opportunity(&opp).await {
                    warn!(error = %e, id = %opp.id, "redis mirror failed");
                }
                Self::try_channel(tx, opp);
            }
        }
    }

    fn try_channel(tx: &mpsc::Sender<Opportunity>, opp: Opportunity) {
        use mpsc::error::TrySendError;
        match tx.try_send(opp) {
            Ok(()) => {}
            Err(TrySendError::Full(o)) => {
                debug!(id = %o.id, "executor channel full, dropping opportunity")
            }
            Err(TrySendError::Closed(o)) => {
                warn!(id = %o.id, "executor channel closed")
            }
        }
    }
}

/// Bootstrap, build the cycle index, open the Geyser stream and run discovery
/// until the stream ends, emitting each opportunity to `sink`.
pub async fn run_pipeline(cfg: &MonitorConfig, mut sink: OpportunitySink) -> Result<()> {
    let mut registry = PoolRegistry::new();
    bootstrap_registry(cfg, &mut registry).await?;
    info!(
        pools = registry.pools.len(),
        tokens = registry.tokens.len(),
        "registry hydrated"
    );

    let mut engine = DiscoveryEngine::new(cfg.opportunity_cooldown_ms);
    engine.build_cycle_index(&registry, cfg);
    info!(routes = engine.route_count(), "cycle index built");
    if engine.route_count() == 0 {
        warn!("no cycles found across configured pools — nothing to monitor");
    }

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
                for opp in engine.run_search(&registry, cfg, None) {
                    info!(
                        id = %opp.id,
                        base = %opp.base_symbol.clone().unwrap_or_default(),
                        hops = opp.hops.len(),
                        net = opp.net_profit,
                        bps = opp.net_profit_bps as u64,
                        amount_in = opp.amount_in,
                        slot = opp.slot,
                        "OPPORTUNITY"
                    );
                    sink.emit(opp).await;
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
