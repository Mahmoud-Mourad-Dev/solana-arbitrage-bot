//! arb-monitor — standalone Rust price monitor.
//!
//! Bootstraps the pool registry, runs the Geyser -> discovery pipeline, and
//! publishes opportunities to Redis (the same `arbitrage_opportunities`
//! contract the TypeScript monitor uses). The pipeline itself lives in
//! `arb_monitor::pipeline` so the fused `arb-bot` binary can reuse it with an
//! in-process channel instead of Redis.

use anyhow::Result;
use arb_monitor::config::MonitorConfig;
use arb_monitor::pipeline::{run_pipeline, OpportunitySink};
use arb_monitor::redis_sink::RedisSink;
use tracing::info;

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

    let sink = RedisSink::connect(
        &cfg.redis_url,
        cfg.opportunity_channel.clone(),
        cfg.opportunity_list.clone(),
        cfg.opportunity_list_max,
    )
    .await?;
    info!(channel = %cfg.opportunity_channel, "redis publisher ready");

    run_pipeline(&cfg, OpportunitySink::Redis(sink)).await
}
