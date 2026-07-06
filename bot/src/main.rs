//! arb-bot — the whole pipeline in one process.
//!
//! ```text
//! Geyser -> monitor pipeline -> tokio::mpsc -> executor -> Jito
//!                                    ^
//!                     Redis is NOT on this path.
//! ```
//!
//! The monitor's `DiscoveryEngine` feeds opportunities straight into an
//! in-process bounded channel that the executor drains — no serialization,
//! no Redis round-trip, no cross-process hop on the latency-critical path.
//! Redis is optional: set `MONITOR_REDIS_MIRROR=true` to also publish
//! opportunities for external observability (off the hot path).
//!
//! Safety posture is unchanged: the executor still simulates unless
//! `DRY_RUN=false` AND `ENABLE_SUBMIT=true` AND `ENABLE_JITO=true`.

use anyhow::Result;
use arb_executor::app::{run_channel_loop, App};
use arb_executor::config::Config as ExecConfig;
use arb_monitor::config::MonitorConfig;
use arb_monitor::pipeline::{run_pipeline, OpportunitySink};
use arb_monitor::redis_sink::RedisSink;
use tokio::sync::mpsc;
use tracing::{info, warn};

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn env_bool(name: &str) -> bool {
    matches!(std::env::var(name).as_deref(), Ok("true" | "1"))
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let monitor_cfg = MonitorConfig::from_env(true)?;
    let exec_cfg = ExecConfig::from_env()?;
    let channel_cap = env_usize("INTERNAL_CHANNEL_CAP", 256);
    let redis_mirror = env_bool("MONITOR_REDIS_MIRROR");

    info!(
        pools = monitor_cfg.pools.len(),
        max_hops = monitor_cfg.max_hops,
        channel_cap,
        redis_mirror,
        "arb-bot starting (fused monitor + executor)"
    );

    // Executor core (RPC, payer, resolver, Jito, blockhash cache, ALTs).
    let app = App::from_config(exec_cfg).await?;

    // In-process opportunity channel. Bounded + try_send in the pipeline
    // means a slow executor drops opportunities rather than stalling Geyser.
    let (tx, rx) = mpsc::channel(channel_cap);

    let sink = if redis_mirror {
        let redis = RedisSink::connect(
            &monitor_cfg.redis_url,
            monitor_cfg.opportunity_channel.clone(),
            monitor_cfg.opportunity_list.clone(),
            monitor_cfg.opportunity_list_max,
        )
        .await?;
        info!(channel = %monitor_cfg.opportunity_channel, "redis observability mirror enabled");
        OpportunitySink::ChannelWithRedis(tx, redis)
    } else {
        OpportunitySink::Channel(tx)
    };

    // Run both halves; whichever finishes first (stream ended, channel
    // closed, or a fatal error) brings the process down.
    let monitor_cfg_owned = monitor_cfg;
    let pipeline = tokio::spawn(async move { run_pipeline(&monitor_cfg_owned, sink).await });
    let executor = tokio::spawn(async move { run_channel_loop(app, rx).await });

    tokio::select! {
        r = pipeline => {
            warn!("monitor pipeline exited");
            r.map_err(anyhow::Error::from).and_then(|x| x)
        }
        r = executor => {
            warn!("executor loop exited");
            r.map_err(anyhow::Error::from).and_then(|x| x)
        }
    }
}
