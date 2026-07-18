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

/// The two venue legs for a market at a single slot, plus measured latency and
/// the fee-v2 provenance resolved from the SAME snapshot.
pub struct Snapshot {
    pub slot: u64,
    pub rpc_latency_ms: u64,
    pub fee_v2: crate::narrow_report::FeeV2Provenance,
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
    // ── Account provenance (S13C P4): the cached identities must MATCH the
    // decoded on-chain pool/pair, and every account must be owned by the
    // expected program — typed rejects, never a quote on mismatched state.
    if pool.base_vault != bv || pool.quote_vault != qv {
        return Err("vault_identity_mismatch");
    }
    if pool.base_mint != base_mint_k {
        return Err("pool_base_mint_mismatch");
    }
    if pool.quote_mint != ctx.wsol {
        return Err("pool_quote_not_wsol");
    }
    let token_prog = Pubkey::from_str(crate::sim_parity::TOKEN_PROGRAM).unwrap();
    let token22_prog = Pubkey::from_str(crate::sim_parity::TOKEN_2022_PROGRAM).unwrap();
    let is_token_prog = |k: &Pubkey| *k == token_prog || *k == token22_prog;
    if !is_token_prog(&bv_acc.owner) || !is_token_prog(&qv_acc.owner) {
        return Err("vault_owner_not_token_program");
    }
    // SPL/2022 token-account layout: mint at [0..32].
    let acct_mint = |a: &Account| -> Option<Pubkey> {
        (a.data.len() >= 72).then(|| Pubkey::new_from_array(a.data[0..32].try_into().unwrap()))
    };
    if acct_mint(bv_acc) != Some(pool.base_mint) || acct_mint(qv_acc) != Some(pool.quote_mint) {
        return Err("vault_mint_mismatch");
    }
    if !is_token_prog(&mint_acc.owner) {
        return Err("mint_owner_not_token_program");
    }
    let fee_prog = Pubkey::from_str(crate::sim_parity::PUMP_FEE_PROGRAM_ID).unwrap();
    if fee_cfg_acc.owner != fee_prog {
        return Err("fee_config_owner_mismatch");
    }
    // Meteora pair sides must be exactly {token, WSOL}.
    let pair_mints = [pair.token_x_mint, pair.token_y_mint];
    if !(pair_mints.contains(&ctx.wsol) && pair_mints.contains(&base_mint_k)) {
        return Err("pair_mint_mismatch");
    }
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
    // Fee-v2 provenance (S13C P5) — every value from THIS snapshot.
    let Ok(tier) =
        crate::pump_amm::resolve_fee_v2(base_reserve, quote_reserve, base_mint_supply, &fee_config)
    else {
        return Err("fee_tier_unresolved");
    };
    let fee_v2 = crate::narrow_report::FeeV2Provenance {
        market_cap_lamports: tier.market_cap,
        tier_index: tier.tier_index,
        lp_bps: tier.lp_bps,
        protocol_bps: tier.protocol_bps,
        creator_bps: tier.creator_bps,
        total_bps: tier.total_bps,
        lp_fee_lamports: 0, // filled by the caller at the optimized size
        protocol_fee_lamports: 0,
        creator_fee_lamports: 0,
        fee_config_address: PUMP_FEE_CONFIG_ADDR.to_string(),
        fee_config_owner: fee_cfg_acc.owner.to_string(),
        fee_config_hash: sha256_hex(&fee_cfg_acc.data),
        schema_version: crate::pump_feev2::FEE_SCHEMA_VERSION.to_string(),
        base_mint_supply,
        base_reserve,
        quote_reserve,
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
        fee_v2,
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

/// SHA-256 hex of account data (fee-config provenance hash).
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

/// Bounded retry backoff schedule (ms) for snapshot fetches: attempt 0 → no
/// wait, then 250ms, then 500ms; hard-capped — never unbounded.
pub fn retry_backoff_ms(attempt: u32) -> u64 {
    match attempt {
        0 => 0,
        1 => 250,
        _ => 500,
    }
}

/// Per-attempt RPC timeout for a snapshot fetch.
pub const SNAPSHOT_TIMEOUT_SECS: u64 = 10;
/// Total snapshot attempts (1 initial + bounded retries).
pub const SNAPSHOT_ATTEMPTS: u32 = 2;

/// `fetch_snapshot` with a per-attempt timeout and bounded retry/backoff
/// (S13C P8). Distinguishes a timeout ("rpc_timeout") from other rejects.
pub async fn fetch_snapshot_retry(
    rpc: &RpcClient,
    ctx: &Ctx,
    m: &DiscoveredMarket,
    now_unix: i64,
    secrets: &[&str],
) -> Result<Snapshot, &'static str> {
    let mut last: &'static str = "rpc_timeout";
    for attempt in 0..SNAPSHOT_ATTEMPTS {
        let wait = retry_backoff_ms(attempt);
        if wait > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
        }
        match tokio::time::timeout(
            std::time::Duration::from_secs(SNAPSHOT_TIMEOUT_SECS),
            fetch_snapshot(rpc, ctx, m, now_unix, secrets),
        )
        .await
        {
            Ok(Ok(s)) => return Ok(s),
            // Provenance/decode rejects are deterministic — retrying is
            // pointless; return immediately.
            Ok(Err(e)) if !is_transient(e) => return Err(e),
            Ok(Err(e)) => last = e,
            Err(_) => last = "rpc_timeout",
        }
    }
    Err(last)
}

/// Whether a snapshot reject is transport-level (worth one bounded retry).
pub fn is_transient(reason: &str) -> bool {
    matches!(reason, "pair_fetch" | "snapshot_fetch" | "rpc_timeout")
}

/// Atomic file write: temp file in the same directory + rename, so a crash
/// mid-write can never leave a truncated report/checkpoint (S13C P8).
pub fn atomic_write(path: &str, contents: &str) -> std::io::Result<()> {
    let tmp = format!("{path}.tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

/// Split a curated market list into (safe, unsafe-with-reason) (S13C P1).
pub fn partition_safe(
    markets: &[DiscoveredMarket],
) -> (Vec<DiscoveredMarket>, Vec<(String, String)>) {
    let mut safe = Vec::new();
    let mut unsafe_routes = Vec::new();
    for m in markets {
        if m.safe {
            safe.push(m.clone());
        } else {
            unsafe_routes.push((
                m.pump_pool.clone(),
                format!("safe=false (mint {} failed screening)", m.token_mint),
            ));
        }
    }
    (safe, unsafe_routes)
}

/// Startup gate: with `allow_diagnostic=false` (default), ANY unsafe route in
/// the curated file refuses startup, listing each offender. With the explicit
/// observe-only diagnostic flag, unsafe routes are EXCLUDED (never polled,
/// never in episodes/controls/economics) and the safe remainder runs.
/// (safe markets, excluded-unsafe `(pool, reason)` pairs).
pub type SafePartition = (Vec<DiscoveredMarket>, Vec<(String, String)>);

pub fn startup_route_check(
    markets: &[DiscoveredMarket],
    allow_diagnostic: bool,
) -> Result<SafePartition, String> {
    let (safe, unsafe_routes) = partition_safe(markets);
    if !unsafe_routes.is_empty() && !allow_diagnostic {
        let list: Vec<String> = unsafe_routes
            .iter()
            .map(|(p, r)| format!("{p}: {r}"))
            .collect();
        return Err(format!(
            "curated file contains {} unsafe route(s) — refusing startup \
             (set NARROW_UNSAFE_DIAGNOSTIC=true to exclude them and continue): {}",
            unsafe_routes.len(),
            list.join("; ")
        ));
    }
    if safe.is_empty() {
        return Err("no safe routes to poll".into());
    }
    Ok((safe, unsafe_routes))
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
    let snap = match fetch_snapshot_retry(rpc, ctx, m, now_unix, secrets).await {
        Ok(s) => s,
        Err(reason) => {
            // FAILED reconfirmation: valid_snapshot=false + typed reason so the
            // aggregator can NEVER mistake it for a zero-profit survivor.
            return Confirmation {
                survived: false,
                valid_snapshot: false,
                reject_reason: Some(reason.to_string()),
                delay_ms: now_ms().saturating_sub(detected_at_ms),
                ..Default::default()
            };
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
            valid_snapshot: true,
            reject_reason: None,
            context_slot: slot,
            net_profit_lamports: c.net_profit,
            gross_profit_lamports: c.gross_profit,
            delay_ms: now_ms().saturating_sub(detected_at_ms),
        },
        _ => Confirmation {
            survived: false,
            valid_snapshot: true,
            context_slot: slot,
            delay_ms: now_ms().saturating_sub(detected_at_ms),
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn market(pool: &str, safe: bool) -> DiscoveredMarket {
        // Build via serde so we don't depend on field-by-field construction.
        let mut v = serde_json::json!({
            "token_mint": format!("TOK{pool}"),
            "decimals": 6,
            "token_2022": true,
            "pump_pool": pool,
            "pump_base_is_wsol": false,
            "pump_base_vault": "BV",
            "pump_quote_vault": "QV",
            "pump_fee_verified": false,
            "pump_wsol_reserve": 1u64,
            "dlmm_pair": "PAIR",
            "dlmm_x_is_wsol": false,
            "dlmm_reserve_x": "RX",
            "dlmm_reserve_y": "RY",
            "dlmm_bin_step": 80,
            "dlmm_wsol_reserve": 1u64,
            "safety": serde_json::to_value(crate::market_discovery::MintSafety::default()).unwrap(),
            "safe": safe,
        });
        // Tolerate extra required fields with defaults if schema grows.
        if let Some(obj) = v.as_object_mut() {
            obj.entry("min_wsol_reserve")
                .or_insert(serde_json::json!(1u64));
            obj.entry("rank_lamports")
                .or_insert(serde_json::json!(1u64));
        }
        serde_json::from_value(v).expect("test market builds")
    }

    // ── S13C P1: unsafe routes can never reach polls or economics. ──

    #[test]
    fn startup_refuses_unsafe_routes_by_default() {
        let ms = vec![market("SAFE1", true), market("BAD1", false)];
        let err = startup_route_check(&ms, false).unwrap_err();
        assert!(err.contains("refusing startup"), "{err}");
        assert!(err.contains("BAD1"), "must name the offending route: {err}");
    }

    #[test]
    fn diagnostic_flag_excludes_unsafe_but_runs_safe_remainder() {
        let ms = vec![market("SAFE1", true), market("BAD1", false)];
        let (safe, excluded) = startup_route_check(&ms, true).unwrap();
        assert_eq!(safe.len(), 1);
        assert_eq!(safe[0].pump_pool, "SAFE1");
        assert_eq!(excluded.len(), 1);
        assert_eq!(excluded[0].0, "BAD1");
        assert!(excluded[0].1.contains("safe=false"));
    }

    #[test]
    fn all_unsafe_refuses_even_with_flag() {
        let ms = vec![market("BAD1", false)];
        assert!(startup_route_check(&ms, true).is_err(), "no safe routes");
    }

    #[test]
    fn unsafe_route_cannot_affect_economics() {
        // The partition is the ONLY source of pollable routes: an unsafe route
        // never yields events, so aggregate_narrow over the safe set cannot
        // contain it. Prove the partition drops it entirely.
        let ms = vec![market("SAFE1", true), market("BAD1", false)];
        let (safe, _) = partition_safe(&ms);
        assert!(safe.iter().all(|m| m.pump_pool != "BAD1"));
        // And a poll stream built from the safe set has no BAD1 route key.
        let keys: Vec<String> = safe
            .iter()
            .map(|m| format!("{}|{}|meteora->pump", m.pump_pool, m.dlmm_pair))
            .collect();
        assert!(keys.iter().all(|k| !k.contains("BAD1")));
    }

    // ── S13C P8: bounded backoff, transient classification, atomic writes. ──

    #[test]
    fn retry_backoff_is_bounded() {
        assert_eq!(retry_backoff_ms(0), 0);
        assert_eq!(retry_backoff_ms(1), 250);
        assert_eq!(retry_backoff_ms(2), 500);
        assert_eq!(retry_backoff_ms(99), 500, "hard cap — never unbounded");
    }

    #[test]
    fn transient_vs_deterministic_rejects() {
        assert!(is_transient("snapshot_fetch"));
        assert!(is_transient("pair_fetch"));
        assert!(is_transient("rpc_timeout"));
        // Provenance/decode failures are deterministic — no retry.
        assert!(!is_transient("vault_identity_mismatch"));
        assert!(!is_transient("fee_config_owner_mismatch"));
        assert!(!is_transient("fee_config_decode"));
        assert!(!is_transient("pool_base_mint_mismatch"));
    }

    #[test]
    fn atomic_write_replaces_whole_file_and_leaves_no_tmp() {
        let dir = std::env::temp_dir().join(format!("narrow-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("report.json");
        let path_s = path.to_str().unwrap();
        atomic_write(path_s, "{\"v\":1}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"v\":1}");
        atomic_write(path_s, "{\"v\":2}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"v\":2}");
        assert!(
            !std::path::Path::new(&format!("{path_s}.tmp")).exists(),
            "tmp must be renamed away"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sha256_hex_is_deterministic() {
        assert_eq!(sha256_hex(b""), sha256_hex(b""));
        assert_ne!(sha256_hex(b"a"), sha256_hex(b"b"));
        assert_eq!(sha256_hex(b"abc").len(), 64);
    }
}
