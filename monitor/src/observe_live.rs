//! Shared live-fetch helpers for the observe binaries (wide + narrow).
//!
//! Read-only: single-slot snapshots, route construction, and delayed
//! reconfirmation. NEVER builds, signs, simulates, or submits anything.

use crate::market_discovery::DiscoveredMarket;
use crate::meteora_dlmm::{self, decode_bin_array, decode_lb_pair, BinArray};
use crate::observe_report::{redact_secrets, Confirmation};
use crate::optimizer::{optimize, SizeGrid};
use crate::pump_amm::{self, decode_pump_pool};
use crate::route_engine::{Leg, Route};
use arb_common::cost::CostModel;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::account::Account;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::sysvar::clock::ID as CLOCK_ID;
use std::collections::HashMap;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// Parse a u64 env var with a default.
pub fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Short git commit hash of the running tree (for report provenance).
pub fn git_commit() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Best-effort gzip (keeps the original): produces `<path>.gz`.
pub fn gzip(path: &str) {
    let _ = std::process::Command::new("gzip")
        .args(["-kf", path])
        .status();
}

/// Secrets to redact from logs: the RPC URL plus optional WSS endpoint.
pub fn secrets_from_env(rpc_url: &str) -> Vec<String> {
    let mut v = vec![rpc_url.to_string()];
    if let Ok(w) = std::env::var("RPC_WSS_ENDPOINT") {
        if !w.is_empty() {
            v.push(w);
        }
    }
    v
}

/// Flip `flag` to true on SIGINT or SIGTERM so the run loop can flush a partial
/// report and exit gracefully instead of dying mid-write.
pub fn install_shutdown(flag: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    use std::sync::atomic::Ordering;
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
            let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
            tokio::select! {
                _ = term.recv() => {}
                _ = int.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        tracing::warn!("shutdown signal received — flushing a partial final report");
        flag.store(true, Ordering::Relaxed);
    });
}

fn token_amount(acc: &Account) -> Option<u64> {
    (acc.data.len() >= 72).then(|| u64::from_le_bytes(acc.data[64..72].try_into().unwrap()))
}

/// SPL / Token-2022 Mint `supply` (u64 @ offset 36).
fn mint_supply(acc: &Account) -> Option<u64> {
    (acc.data.len() >= 44).then(|| u64::from_le_bytes(acc.data[36..44].try_into().unwrap()))
}

/// The Pump fee-program GLOBAL config account ([19]) — one address for all
/// pools (proven identical across routes 1 & 3). Owned by the fee program;
/// its layout is validated on decode.
pub const PUMP_FEE_CONFIG_ADDR: &str = "5PHirr8joyTMp9JMm6nW7hNDVyEYdkzDqazxPD7RaTjx";

pub fn bin_array_pda(pair: &Pubkey, index: i64, program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"bin_array", pair.as_ref(), &index.to_le_bytes()],
        program,
    )
    .0
}

/// Programs + WSOL, resolved once.
pub struct Ctx {
    pub pump_prog: Pubkey,
    pub dlmm_prog: Pubkey,
    pub wsol: Pubkey,
}

impl Ctx {
    pub fn new() -> anyhow::Result<Self> {
        Ok(Ctx {
            pump_prog: Pubkey::from_str(pump_amm::PUMP_AMM_PROGRAM_ID)?,
            dlmm_prog: Pubkey::from_str(meteora_dlmm::DLMM_PROGRAM_ID)?,
            wsol: Pubkey::from_str(crate::market_discovery::WSOL_MINT)?,
        })
    }
}

/// The two venue legs for a market at a single slot, plus measured latency.
pub struct Snapshot {
    pub slot: u64,
    pub rpc_latency_ms: u64,
    pub pump_leg: Leg,
    pub dlmm_leg: Leg,
}

/// Cluster time (unix) from the Clock sysvar, for DLMM volatility decay.
pub async fn cluster_time(rpc: &RpcClient) -> i64 {
    match rpc.get_account(&CLOCK_ID).await {
        Ok(a) if a.data.len() >= 40 => i64::from_le_bytes(a.data[32..40].try_into().unwrap()),
        _ => (now_ms() / 1000) as i64,
    }
}

/// Fetch a market's quote inputs at ONE slot (pair-probe to pick the bin-array
/// window, then a single getMultipleAccounts). Returns a structured reason on
/// any missing/invalid state — never a guess.
pub async fn fetch_snapshot(
    rpc: &RpcClient,
    ctx: &Ctx,
    m: &DiscoveredMarket,
    now_unix: i64,
    secrets: &[&str],
) -> Result<Snapshot, &'static str> {
    let t0 = std::time::Instant::now();
    let (Ok(pool_k), Ok(bv), Ok(qv), Ok(pair_k)) = (
        Pubkey::from_str(&m.pump_pool),
        Pubkey::from_str(&m.pump_base_vault),
        Pubkey::from_str(&m.pump_quote_vault),
        Pubkey::from_str(&m.dlmm_pair),
    ) else {
        return Err("bad_key");
    };
    let active_id = match rpc.get_account(&pair_k).await {
        Ok(a) if a.owner == ctx.dlmm_prog => match decode_lb_pair(&a.data) {
            Ok(p) => p.active_id,
            Err(_) => return Err("pair_decode"),
        },
        Ok(_) => return Err("pair_owner"),
        Err(e) => {
            tracing::warn!(error = %redact_secrets(&e.to_string(), secrets), "pair probe failed");
            return Err("pair_fetch");
        }
    };
    let (Ok(fee_cfg_k), Ok(base_mint_k)) = (
        Pubkey::from_str(PUMP_FEE_CONFIG_ADDR),
        Pubkey::from_str(&m.token_mint),
    ) else {
        return Err("bad_key");
    };
    let aidx = (active_id as i64).div_euclid(70);
    let idxs: Vec<i64> = (aidx - 2..=aidx + 2).collect();
    // Fixed slots: [0]=pool [1]=base_vault [2]=quote_vault [3]=pair
    // [4]=fee_config [5]=base_mint, then bin arrays. All one single-slot snapshot.
    let mut keys = vec![pool_k, bv, qv, pair_k, fee_cfg_k, base_mint_k];
    keys.extend(
        idxs.iter()
            .map(|&i| bin_array_pda(&pair_k, i, &ctx.dlmm_prog)),
    );

    let resp = match rpc
        .get_multiple_accounts_with_commitment(&keys, CommitmentConfig::confirmed())
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %redact_secrets(&e.to_string(), secrets), "snapshot fetch failed");
            return Err("snapshot_fetch");
        }
    };
    let slot = resp.context.slot;
    let v = resp.value;
    let (
        Some(pool_acc),
        Some(bv_acc),
        Some(qv_acc),
        Some(pair_acc),
        Some(fee_cfg_acc),
        Some(mint_acc),
    ) = (&v[0], &v[1], &v[2], &v[3], &v[4], &v[5])
    else {
        return Err("missing_account");
    };
    if pool_acc.owner != ctx.pump_prog || pair_acc.owner != ctx.dlmm_prog {
        return Err("wrong_owner");
    }
    let (Ok(pool), Ok(pair)) = (
        decode_pump_pool(&pool_acc.data),
        decode_lb_pair(&pair_acc.data),
    ) else {
        return Err("decode");
    };
    let (Some(base_reserve), Some(quote_reserve)) = (token_amount(bv_acc), token_amount(qv_acc))
    else {
        return Err("vault_decode");
    };
    // fee-v2: decode the fee config + base-mint supply from the SAME snapshot.
    // No optimistic fallback — a bad config fails the snapshot.
    let Ok(fee_config) = crate::pump_feev2::decode_fee_config(&fee_cfg_acc.data) else {
        return Err("fee_config_decode");
    };
    let Some(base_mint_supply) = mint_supply(mint_acc) else {
        return Err("mint_supply_decode");
    };
    let mut arrays: HashMap<i64, BinArray> = HashMap::new();
    for (i, acc) in idxs.iter().zip(&v[6..]) {
        if let Some(acc) = acc {
            if acc.owner == ctx.dlmm_prog {
                if let Ok(ba) = decode_bin_array(&acc.data) {
                    if ba.lb_pair == pair_k {
                        arrays.insert(*i, ba);
                    }
                }
            }
        }
    }
    Ok(Snapshot {
        slot,
        rpc_latency_ms: t0.elapsed().as_millis() as u64,
        pump_leg: Leg::Pump {
            pool,
            base_reserve,
            quote_reserve,
            base_mint_supply,
            fee_config,
        },
        dlmm_leg: Leg::Meteora {
            pair,
            arrays,
            now_unix,
        },
    })
}

/// Both directions from a snapshot.
pub fn routes_for(snap: Snapshot) -> [(&'static str, Route); 2] {
    let Snapshot {
        pump_leg, dlmm_leg, ..
    } = snap;
    [
        (
            "pump->meteora",
            Route {
                leg1: pump_leg.clone(),
                leg2: dlmm_leg.clone(),
            },
        ),
        (
            "meteora->pump",
            Route {
                leg1: dlmm_leg,
                leg2: pump_leg,
            },
        ),
    ]
}

/// Fresh single-slot re-fetch + re-optimize for one direction under `cost`; a
/// survivor is a candidate netting ≥ 0. `delay_ms` is measured from
/// `detected_at_ms`. The caller is responsible for any sleep before calling
/// (so it can schedule +2s/+10s/+30s reconfirmations precisely).
#[allow(clippy::too_many_arguments)]
pub async fn reconfirm(
    rpc: &RpcClient,
    ctx: &Ctx,
    m: &DiscoveredMarket,
    now_unix: i64,
    secrets: &[&str],
    label: &str,
    cost: &CostModel,
    grid: &SizeGrid,
    detected_at_ms: u64,
) -> Confirmation {
    let snap = match fetch_snapshot(rpc, ctx, m, now_unix, secrets).await {
        Ok(s) => s,
        Err(_) => {
            return Confirmation {
                survived: false,
                delay_ms: now_ms().saturating_sub(detected_at_ms),
                ..Default::default()
            }
        }
    };
    let slot = snap.slot;
    let route = routes_for(snap)
        .into_iter()
        .find(|(l, _)| *l == label)
        .map(|(_, r)| r)
        .expect("label is one of the two directions");
    match optimize(&route, &ctx.wsol, cost, grid) {
        Some(c) if c.net_profit >= 0 => Confirmation {
            survived: true,
            context_slot: slot,
            net_profit_lamports: c.net_profit,
            gross_profit_lamports: c.gross_profit,
            delay_ms: now_ms().saturating_sub(detected_at_ms),
        },
        _ => Confirmation {
            survived: false,
            context_slot: slot,
            delay_ms: now_ms().saturating_sub(detected_at_ms),
            ..Default::default()
        },
    }
}
