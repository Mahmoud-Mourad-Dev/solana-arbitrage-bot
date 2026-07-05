//! Background-refreshed blockhash cache: the hot path never pays an RPC
//! round-trip for a hash that only changes every slot anyway.

use anyhow::{Context, Result};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::hash::Hash;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

const REFRESH_INTERVAL: Duration = Duration::from_millis(400);

pub struct BlockhashCache {
    latest: Arc<RwLock<Hash>>,
}

impl BlockhashCache {
    /// Fetches once synchronously (fail fast on a bad RPC), then keeps
    /// refreshing in a background task for the life of the process.
    pub async fn start(rpc: Arc<RpcClient>) -> Result<Self> {
        let initial = rpc
            .get_latest_blockhash()
            .await
            .context("initial blockhash fetch — is RPC_ENDPOINT reachable?")?;
        let latest = Arc::new(RwLock::new(initial));

        let shared = latest.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(REFRESH_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                match rpc.get_latest_blockhash().await {
                    Ok(hash) => *shared.write().await = hash,
                    Err(e) => tracing::warn!(error = %e, "blockhash refresh failed"),
                }
            }
        });
        Ok(Self { latest })
    }

    pub async fn get(&self) -> Hash {
        *self.latest.read().await
    }
}
