//! `capture-parity-fixtures` — S13C slice 2 fixture-capture command.
//!
//! Discovers real, successful, DIRECT Pump `sell` / Meteora `swap` instructions
//! for the configured routes and persists deterministic, versioned fixtures
//! with program-version provenance. Read-only: it receives only a
//! [`ChainReader`] (via `RpcReader`) and CANNOT simulate, send, sign, bundle,
//! deploy, or load a keypair.
//!
//! Paginates + retries with bounded backoff, resumes from a persisted cursor,
//! writes incrementally, dedups, redacts RPC secrets, and emits per-route
//! diagnostics. NO account substitution / mutation / simulation in this slice.
//!
//! Usage:
//!   cargo run -p arb-monitor --bin capture-parity-fixtures -- \
//!     [--route <id>|all] [--venue pump|meteora|both] [--target N] [--max-pages P]
//! Env: RPC_ENDPOINT (redacted in logs), CPF_OUT_DIR (reports/fixtures).

use anyhow::{Context, Result};
use arb_monitor::fixture_capture::{
    backoff_ms, classify, dedup_and_order, program_version_matches, CaptureDiagnostics, Fixture,
    ProgramProvenance, Venue,
};
use arb_monitor::observe_live::{git_commit, gzip, secrets_from_env};
use arb_monitor::observe_report::redact_secrets;
use arb_monitor::sim_client::{AccountData, ChainReader, RawIx, RawTx, SigInfo};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_client::GetConfirmedSignaturesForAddress2Config;
use solana_client::rpc_config::RpcTransactionConfig;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_transaction_status_client_types::{
    EncodedTransaction, UiInstruction, UiMessage, UiTransactionEncoding,
};
use std::collections::BTreeMap;
use std::io::Write;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Read-only RPC-backed [`ChainReader`]. Wraps `RpcClient` but the capture
/// logic only ever sees the `ChainReader` trait — no send method is reachable.
struct RpcReader {
    rpc: RpcClient,
    secrets: Vec<String>,
}

impl RpcReader {
    fn redact(&self, s: &str) -> String {
        redact_secrets(
            s,
            &self.secrets.iter().map(String::as_str).collect::<Vec<_>>(),
        )
    }
}

impl ChainReader for RpcReader {
    async fn get_account(&self, pk: &Pubkey) -> Result<Option<AccountData>> {
        match self.rpc.get_account(pk).await {
            Ok(a) => Ok(Some(AccountData {
                owner: a.owner,
                executable: a.executable,
                data: a.data,
            })),
            Err(_) => Ok(None),
        }
    }

    async fn get_multiple_accounts(
        &self,
        pks: &[Pubkey],
    ) -> Result<(u64, Vec<Option<AccountData>>)> {
        let resp = self
            .rpc
            .get_multiple_accounts_with_commitment(pks, CommitmentConfig::confirmed())
            .await
            .map_err(|e| anyhow::anyhow!(self.redact(&e.to_string())))?;
        let v = resp
            .value
            .into_iter()
            .map(|o| {
                o.map(|a| AccountData {
                    owner: a.owner,
                    executable: a.executable,
                    data: a.data,
                })
            })
            .collect();
        Ok((resp.context.slot, v))
    }

    async fn get_signatures(
        &self,
        address: &Pubkey,
        before: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SigInfo>> {
        let cfg = GetConfirmedSignaturesForAddress2Config {
            before: before.and_then(|s| solana_sdk::signature::Signature::from_str(s).ok()),
            until: None,
            limit: Some(limit),
            commitment: Some(CommitmentConfig::confirmed()),
        };
        let sigs = self
            .rpc
            .get_signatures_for_address_with_config(address, cfg)
            .await
            .map_err(|e| anyhow::anyhow!(self.redact(&e.to_string())))?;
        Ok(sigs
            .into_iter()
            .map(|s| SigInfo {
                signature: s.signature,
                slot: s.slot,
                err: s.err.is_some(),
                block_time: s.block_time,
            })
            .collect())
    }

    async fn get_transaction(&self, signature: &str) -> Result<Option<RawTx>> {
        let sig = solana_sdk::signature::Signature::from_str(signature)?;
        let cfg = RpcTransactionConfig {
            encoding: Some(UiTransactionEncoding::Json),
            commitment: Some(CommitmentConfig::confirmed()),
            max_supported_transaction_version: Some(0),
        };
        let tx = match self.rpc.get_transaction_with_config(&sig, cfg).await {
            Ok(t) => t,
            Err(_) => return Ok(None),
        };
        Ok(decode_raw_tx(tx))
    }
}

/// Map a solana JSON transaction into our minimal [`RawTx`].
fn decode_raw_tx(
    tx: solana_transaction_status_client_types::EncodedConfirmedTransactionWithStatusMeta,
) -> Option<RawTx> {
    let slot = tx.slot;
    let block_time = tx.block_time;
    // `TransactionVersion` is not public; read it via serde ("legacy" or N).
    let version = match serde_json::to_value(&tx.transaction.version) {
        Ok(serde_json::Value::Number(n)) => n.to_string(),
        Ok(serde_json::Value::String(s)) => s,
        _ => "legacy".into(),
    };
    let meta = tx.transaction.meta.as_ref()?;
    let err = meta.err.as_ref().map(|e| format!("{e:?}"));

    let EncodedTransaction::Json(ui) = &tx.transaction.transaction else {
        return None;
    };
    let UiMessage::Raw(msg) = &ui.message else {
        return None;
    };
    let header = (
        msg.header.num_required_signatures,
        msg.header.num_readonly_signed_accounts,
        msg.header.num_readonly_unsigned_accounts,
    );
    let (loaded_writable, loaded_readonly) = match &meta.loaded_addresses {
        solana_transaction_status_client_types::option_serializer::OptionSerializer::Some(la) => {
            (la.writable.clone(), la.readonly.clone())
        }
        _ => (vec![], vec![]),
    };
    let top_level = msg
        .instructions
        .iter()
        .map(|ix| RawIx {
            program_id_index: ix.program_id_index as usize,
            account_indices: ix.accounts.iter().map(|&a| a as usize).collect(),
            data_b58: ix.data.clone(),
        })
        .collect();
    // Inner program ids: resolve program_id_index against the full key list.
    let mut all_keys = msg.account_keys.clone();
    all_keys.extend(loaded_writable.clone());
    all_keys.extend(loaded_readonly.clone());
    let mut inner_program_ids = Vec::new();
    if let solana_transaction_status_client_types::option_serializer::OptionSerializer::Some(
        inners,
    ) = &meta.inner_instructions
    {
        for group in inners {
            for ix in &group.instructions {
                if let UiInstruction::Compiled(c) = ix {
                    if let Some(pk) = all_keys.get(c.program_id_index as usize) {
                        inner_program_ids.push(pk.clone());
                    }
                }
            }
        }
    }
    Some(RawTx {
        slot,
        block_time,
        version,
        err,
        header,
        static_keys: msg.account_keys.clone(),
        loaded_writable,
        loaded_readonly,
        top_level,
        inner_program_ids,
    })
}

/// Route config: id → (pool, token mint). Mirrors narrow-routes but scoped to
/// the three parity targets + investigation extras.
fn routes() -> BTreeMap<&'static str, (&'static str, &'static str)> {
    BTreeMap::from([
        (
            "route1",
            (
                "5ByL7MZoLABYnwMPZKPKjf4MGkZ7FeBzrAnos19Pre2z",
                "FeMbDoX7R1Psc4GEcvJdsbNbZA3bfztcyDCatJVJpump",
            ),
        ),
        (
            "route2",
            (
                "ETMhxtENfkMK85TAcveEbZdBv9htziWzDSddmShRP2wB",
                "33eum82LaAhtDtjFGdMDBS4KWMWEDuNZDPtxbAF3pump",
            ),
        ),
        (
            "route3",
            (
                "8qDidAKuyNYKaR4dh2ZFZZVG5gBTUfyJcwQPgwt9FS1Y",
                "DdPrHYqM8Ueovnk9kAnAgoGhswkuaTqmxcoZzU3Zpump",
            ),
        ),
    ])
}

#[allow(clippy::too_many_arguments)]
async fn capture_route<R: ChainReader>(
    reader: &R,
    route_id: &str,
    pool: &str,
    mint: &str,
    venue: Venue,
    target: usize,
    max_pages: usize,
    resume_before: Option<String>,
) -> (Vec<Fixture>, CaptureDiagnostics, Option<String>) {
    let mut diag = CaptureDiagnostics::default();
    let mut fixtures: Vec<Fixture> = Vec::new();
    let pool_pk = Pubkey::from_str(pool).unwrap();
    let mut before = resume_before;

    'pages: for _ in 0..max_pages {
        let sigs = fetch_with_retry(reader, &pool_pk, before.as_deref(), &mut diag).await;
        let Some(sigs) = sigs else { break };
        if sigs.is_empty() {
            break;
        }
        before = Some(sigs.last().unwrap().signature.clone());
        for si in &sigs {
            diag.signatures_scanned += 1;
            diag.first_scanned_slot.get_or_insert(si.slot);
            diag.last_scanned_slot = Some(si.slot);
            if si.err {
                diag.failed_transactions += 1;
                continue;
            }
            let tx = match reader.get_transaction(&si.signature).await {
                Ok(Some(t)) => t,
                Ok(None) => {
                    diag.missing_metadata += 1;
                    continue;
                }
                Err(_) => {
                    diag.rpc_failures += 1;
                    continue;
                }
            };
            diag.transactions_fetched += 1;
            diag.successful_transactions += 1;
            let (cls, fx) = classify(&tx, venue, route_id, pool, mint, true, now_ms());
            use arb_monitor::fixture_capture::TxClass::*;
            match cls {
                DirectTopLevel => {
                    diag.direct_target += 1;
                    if let Some(mut f) = fx {
                        f.signature = si.signature.clone();
                        fixtures.push(f);
                        diag.accepted_fixtures += 1;
                        if fixtures.len() >= target {
                            break 'pages;
                        }
                    }
                }
                InnerCpi | AggregatorRouted => diag.cpi_or_aggregator += 1,
                PoolMintMismatch => diag.pool_mint_mismatch += 1,
                UnsupportedVersion => diag.unsupported_version += 1,
                Failed => diag.failed_transactions += 1,
                MissingMetadata => diag.missing_metadata += 1,
                NoTargetInstruction => {}
            }
        }
    }
    let before_cursor = before;
    (dedup_and_order(fixtures), diag, before_cursor)
}

async fn fetch_with_retry<R: ChainReader>(
    reader: &R,
    pool: &Pubkey,
    before: Option<&str>,
    diag: &mut CaptureDiagnostics,
) -> Option<Vec<SigInfo>> {
    for attempt in 0..5u32 {
        match reader.get_signatures(pool, before, 100).await {
            Ok(v) => return Some(v),
            Err(_) => {
                diag.rpc_retries += 1;
                tokio::time::sleep(Duration::from_millis(backoff_ms(attempt, 250, 8000))).await;
            }
        }
    }
    diag.rpc_failures += 1;
    None
}

/// Program provenance for a program id (executable + program-data slot).
async fn provenance<R: ChainReader>(
    reader: &R,
    program_id: &str,
    fixture_slot: u64,
) -> ProgramProvenance {
    let pk = Pubkey::from_str(program_id).ok();
    let mut pv = ProgramProvenance {
        program_id: program_id.to_string(),
        executable: false,
        program_data_address: None,
        fixture_slot,
        program_data_slot: None,
        current_matches_fixture: None,
    };
    let Some(pk) = pk else { return pv };
    if let Ok(Some(acc)) = reader.get_account(&pk).await {
        pv.executable = acc.executable;
        // Upgradeable programs: data = [4-byte tag][32-byte ProgramData addr].
        if acc.data.len() >= 36 {
            let pda = Pubkey::new_from_array(acc.data[4..36].try_into().unwrap());
            pv.program_data_address = Some(pda.to_string());
            if let Ok(Some(pd)) = reader.get_account(&pda).await {
                // ProgramData: [4-byte tag][8-byte slot]...
                if pd.data.len() >= 12 {
                    let slot = u64::from_le_bytes(pd.data[4..12].try_into().unwrap());
                    pv.program_data_slot = Some(slot);
                    pv.current_matches_fixture = program_version_matches(fixture_slot, Some(slot));
                }
            }
        }
    }
    pv
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args: Vec<String> = std::env::args().collect();
    let arg = |k: &str| {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1).cloned())
    };
    let route_sel = arg("--route").unwrap_or_else(|| "all".into());
    let venue_sel = arg("--venue").unwrap_or_else(|| "pump".into());
    let target: usize = arg("--target").and_then(|v| v.parse().ok()).unwrap_or(3);
    let max_pages: usize = arg("--max-pages")
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let out_dir = std::env::var("CPF_OUT_DIR").unwrap_or_else(|_| "reports/fixtures".into());
    std::fs::create_dir_all(&out_dir).ok();

    let rpc_url = std::env::var("RPC_ENDPOINT").context("RPC_ENDPOINT required")?;
    let reader = RpcReader {
        rpc: RpcClient::new_with_commitment(rpc_url.clone(), CommitmentConfig::confirmed()),
        secrets: secrets_from_env(&rpc_url),
    };
    let venues: Vec<Venue> = match venue_sel.as_str() {
        "meteora" => vec![Venue::MeteoraSwap],
        "both" => vec![Venue::PumpSell, Venue::MeteoraSwap],
        _ => vec![Venue::PumpSell],
    };

    let all = routes();
    let selected: Vec<(&str, (&str, &str))> = all
        .iter()
        .filter(|(id, _)| route_sel == "all" || route_sel == **id)
        .map(|(id, v)| (*id, *v))
        .collect();

    // Resume cursors.
    let cursor_path = format!("{out_dir}/cursors.json");
    let mut cursors: BTreeMap<String, String> = std::fs::read_to_string(&cursor_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let run_id = now_ms();
    let out_path = format!("{out_dir}/fixtures-{run_id}.json");
    let mut report = serde_json::Map::new();
    report.insert("schema_version".into(), serde_json::json!(1));
    report.insert("commit".into(), serde_json::json!(git_commit()));
    report.insert("generated_at_ms".into(), serde_json::json!(run_id));

    for venue in venues {
        for (id, (pool, mint)) in &selected {
            info!(route = id, ?venue, "capturing");
            let key = format!("{id}:{venue:?}");
            let (mut fx, diag, cursor) = capture_route(
                &reader,
                id,
                pool,
                mint,
                venue,
                target,
                max_pages,
                cursors.get(&key).cloned(),
            )
            .await;
            if let Some(c) = cursor {
                cursors.insert(key.clone(), c);
            }
            // Program-version provenance for accepted fixtures.
            for f in fx.iter_mut() {
                let mut pvs = vec![provenance(&reader, &f.program_id, f.slot).await];
                if venue == Venue::PumpSell {
                    pvs.push(
                        provenance(
                            &reader,
                            arb_monitor::sim_parity::PUMP_FEE_PROGRAM_ID,
                            f.slot,
                        )
                        .await,
                    );
                }
                f.program_provenance = Some(pvs);
            }
            let status = if fx.len() >= target {
                "SUFFICIENT"
            } else {
                "UNDER_EVIDENCED"
            };
            let deficit = target.saturating_sub(fx.len());
            warn_if_under(id, status, fx.len(), target, &diag);
            report.insert(
                key,
                serde_json::json!({
                    "route": id, "pool": pool, "mint": mint, "venue": format!("{venue:?}"),
                    "status": status, "accepted": fx.len(), "target": target, "deficit": deficit,
                    "diagnostics": diag, "fixtures": fx,
                }),
            );
            // Incremental persist after each route.
            std::fs::write(&out_path, serde_json::to_string_pretty(&report)?)?;
            std::fs::write(&cursor_path, serde_json::to_string_pretty(&cursors)?)?;
        }
    }
    gzip(&out_path);
    println!("\n════ FIXTURE CAPTURE (slice 2) ════");
    for (k, v) in &report {
        if let Some(o) = v.as_object() {
            if let (Some(st), Some(ac)) = (o.get("status"), o.get("accepted")) {
                println!("  {k}: {} accepted={ac}", st.as_str().unwrap_or(""));
            }
        }
    }
    println!("artifact: {out_path}(.gz)  cursors: {cursor_path}");
    println!("NOTE: capture only — no substitution/simulation/parity performed.");
    Ok(())
}

fn warn_if_under(id: &str, status: &str, got: usize, target: usize, diag: &CaptureDiagnostics) {
    if status == "UNDER_EVIDENCED" {
        warn!(
            route = id,
            got,
            target,
            scanned = diag.signatures_scanned,
            direct = diag.direct_target,
            cpi_agg = diag.cpi_or_aggregator,
            failed = diag.failed_transactions,
            "route UNDER_EVIDENCED — deeper search or mark unsupported (do NOT lower the bar)"
        );
    }
}

// Silence unused Write import if incremental writer changes.
#[allow(dead_code)]
fn _touch(f: &mut std::fs::File) -> std::io::Result<()> {
    f.write_all(b"")
}
