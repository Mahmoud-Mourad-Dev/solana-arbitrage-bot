//! Jito Block Engine client: sendBundle over JSON-RPC with bounded retries,
//! plus a best-effort status probe for observability.

use anyhow::{anyhow, bail, Context, Result};
use rand::seq::SliceRandom;
use serde_json::{json, Value};
use solana_sdk::{pubkey, pubkey::Pubkey, transaction::VersionedTransaction};
use std::time::Duration;

/// Official mainnet Jito tip accounts (any one works; rotate randomly to
/// spread write locks across bundles).
pub const JITO_TIP_ACCOUNTS: [Pubkey; 8] = [
    pubkey!("96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5"),
    pubkey!("HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe"),
    pubkey!("Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY"),
    pubkey!("ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49"),
    pubkey!("DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh"),
    pubkey!("ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt"),
    pubkey!("DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL"),
    pubkey!("3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT"),
];

pub fn random_tip_account() -> Pubkey {
    *JITO_TIP_ACCOUNTS
        .choose(&mut rand::thread_rng())
        .expect("tip account list is non-empty")
}

pub struct JitoClient {
    http: reqwest::Client,
    url: String,
    max_attempts: u32,
}

impl JitoClient {
    pub fn new(url: String) -> Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(3))
                .pool_idle_timeout(Duration::from_secs(60))
                .build()
                .context("build http client")?,
            url,
            max_attempts: 3,
        })
    }

    /// Submit a bundle (base58-encoded signed transactions). Returns the
    /// bundle id assigned by the block engine.
    pub async fn send_bundle(&self, txs: &[VersionedTransaction]) -> Result<String> {
        let encoded: Vec<String> = txs
            .iter()
            .map(|tx| Ok(bs58::encode(bincode::serialize(tx)?).into_string()))
            .collect::<Result<_>>()?;
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "sendBundle",
            "params": [encoded],
        });

        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 1..=self.max_attempts {
            match self.post(&body).await {
                Ok(bundle_id) => return Ok(bundle_id),
                Err(e) => {
                    tracing::warn!(attempt, error = %e, "sendBundle attempt failed");
                    last_err = Some(e);
                    if attempt < self.max_attempts {
                        // Short backoff: an opportunity is worthless in
                        // seconds; don't wait longer than it can live.
                        tokio::time::sleep(Duration::from_millis(50 * attempt as u64)).await;
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("sendBundle failed with no error recorded")))
    }

    async fn post(&self, body: &Value) -> Result<String> {
        let resp = self
            .http
            .post(&self.url)
            .json(body)
            .send()
            .await
            .context("http send")?;
        let status = resp.status();
        let payload: Value = resp.json().await.context("decode block engine response")?;
        if let Some(err) = payload.get("error") {
            bail!("block engine error (http {status}): {err}");
        }
        payload
            .get("result")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("no result field in response: {payload}"))
    }

    /// Best-effort landing probe (observability only, never on hot path).
    pub async fn bundle_status(&self, bundle_id: &str) -> Result<Value> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "getBundleStatuses",
            "params": [[bundle_id]],
        });
        let resp = self.http.post(&self.url).json(&body).send().await?;
        let payload: Value = resp.json().await?;
        Ok(payload
            .get("result")
            .and_then(|r| r.get("value"))
            .and_then(|v| v.get(0))
            .cloned()
            .unwrap_or(Value::Null))
    }
}
