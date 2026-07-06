//! arb-executor — standalone execution bot. Subscribes to the Redis
//! `arbitrage_opportunities` channel, prices a dynamic Jito tip, builds the
//! atomic on-chain-program transaction and submits it as a Jito bundle.
//!
//! The trading core lives in `arb_executor::app` so the fused `arb-bot`
//! binary can reuse it with an in-process channel instead of Redis.

use anyhow::Result;
use arb_executor::app::{run_redis_loop, App};
use arb_executor::config::Config;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cfg = Config::from_env()?;
    let app = App::from_config(cfg).await?;
    run_redis_loop(app).await
}
