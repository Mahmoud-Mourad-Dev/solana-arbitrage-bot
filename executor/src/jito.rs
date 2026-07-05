//! Jito Block Engine client: sendBundle over JSON-RPC with bounded retries,
//! plus a best-effort status probe for observability.

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
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

/// sendBundle JSON-RPC body. Base64 encoding with an explicit
/// `{"encoding":"base64"}` param — faster to decode than base58 and the
/// form Jito now recommends (base58 is deprecated for bundles).
pub fn bundle_request_body(txs: &[VersionedTransaction]) -> Result<Value> {
    let encoded: Vec<String> = txs
        .iter()
        .map(|tx| Ok(BASE64.encode(bincode::serialize(tx)?)))
        .collect::<Result<_>>()?;
    Ok(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "sendBundle",
        "params": [encoded, { "encoding": "base64" }],
    }))
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

    /// Submit a bundle (base64-encoded signed transactions). Returns the
    /// bundle id assigned by the block engine.
    pub async fn send_bundle(&self, txs: &[VersionedTransaction]) -> Result<String> {
        let body = bundle_request_body(txs)?;

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

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::{
        hash::Hash,
        message::{v0, VersionedMessage},
        signature::Keypair,
        signer::Signer,
    };
    use solana_system_interface::instruction as system_instruction;

    fn dummy_tx() -> VersionedTransaction {
        let payer = Keypair::new();
        let ix = system_instruction::transfer(&payer.pubkey(), &JITO_TIP_ACCOUNTS[0], 1_000_000);
        let msg =
            v0::Message::try_compile(&payer.pubkey(), &[ix], &[], Hash::new_unique()).unwrap();
        VersionedTransaction::try_new(VersionedMessage::V0(msg), &[&payer]).unwrap()
    }

    #[test]
    fn bundle_body_is_base64_with_encoding_param() {
        let tx = dummy_tx();
        let body = bundle_request_body(std::slice::from_ref(&tx)).unwrap();

        assert_eq!(body["method"], "sendBundle");
        assert_eq!(body["params"][1]["encoding"], "base64");

        let encoded = body["params"][0][0].as_str().expect("tx entry is a string");
        // Round-trip: base64 -> bincode -> identical transaction.
        let raw = BASE64.decode(encoded).expect("valid base64");
        let decoded: VersionedTransaction = bincode::deserialize(&raw).expect("valid bincode");
        assert_eq!(decoded, tx);
        // Must NOT be valid base58 payload semantics: base64 alphabet chars
        // like '+' or '/' or '=' padding may appear; more importantly the
        // decoded bytes above already prove the encoding used.
        assert_eq!(body["params"][0].as_array().unwrap().len(), 1);
    }

    #[test]
    fn bundle_body_handles_multiple_txs() {
        let txs = [dummy_tx(), dummy_tx()];
        let body = bundle_request_body(&txs).unwrap();
        assert_eq!(body["params"][0].as_array().unwrap().len(), 2);
    }
}
