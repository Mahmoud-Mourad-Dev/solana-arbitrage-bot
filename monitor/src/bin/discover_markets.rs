//! `discover-markets` — S5 dynamic Pump∩Meteora discovery binary.
//!
//! Read-only: scans both programs for WSOL-paired pools, intersects the token
//! sets, validates structure with the SAME decoders the quote engines use,
//! applies a WSOL liquidity floor, screens mint safety, and writes a ranked
//! restart cache (`markets.generated.json`) plus a funnel report.
//!
//! Usage: cargo run -p arb-monitor --bin discover-markets [--out PATH]
//! Env: RPC_ENDPOINT, MIN_WSOL_RESERVE_LAMPORTS (default 2 SOL),
//!      MAX_MARKETS (default 200).

use anyhow::{Context, Result};
use arb_monitor::market_discovery::{
    intersect_tokens, parse_mint_safety, DiscoveredMarket, DiscoveryCache, DiscoveryStats,
    CACHE_VERSION, WSOL_MINT,
};
use arb_monitor::meteora_dlmm::{self, decode_lb_pair};
use arb_monitor::pump_amm::{self, decode_pump_pool};
use solana_account_decoder::UiAccountEncoding;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};
use solana_client::rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType};
use solana_sdk::account::Account;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

const TOKEN_2022_PROGRAM: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

/// GPA with a dataSlice: returns (pubkey, sliced bytes).
async fn gpa_slice(
    rpc: &RpcClient,
    program: &Pubkey,
    data_size: u64,
    memcmp_offset: usize,
    memcmp_pubkey: &str,
    slice_offset: usize,
    slice_len: usize,
) -> Result<Vec<(Pubkey, Vec<u8>)>> {
    let cfg = RpcProgramAccountsConfig {
        filters: Some(vec![
            RpcFilterType::DataSize(data_size),
            RpcFilterType::Memcmp(Memcmp::new(
                memcmp_offset,
                MemcmpEncodedBytes::Base58(memcmp_pubkey.to_string()),
            )),
        ]),
        account_config: RpcAccountInfoConfig {
            encoding: Some(UiAccountEncoding::Base64),
            data_slice: Some(solana_account_decoder::UiDataSliceConfig {
                offset: slice_offset,
                length: slice_len,
            }),
            commitment: Some(CommitmentConfig::confirmed()),
            min_context_slot: None,
        },
        with_context: None,
        sort_results: None,
    };
    let accounts = rpc.get_program_accounts_with_config(program, cfg).await?;
    Ok(accounts.into_iter().map(|(k, a)| (k, a.data)).collect())
}

/// Batched getMultipleAccounts (100 per call).
async fn get_many(rpc: &RpcClient, keys: &[Pubkey]) -> Result<Vec<Option<Account>>> {
    let mut out = Vec::with_capacity(keys.len());
    for chunk in keys.chunks(100) {
        let accs = rpc.get_multiple_accounts(chunk).await?;
        out.extend(accs);
    }
    Ok(out)
}

fn token_amount(acc: &Account) -> Option<u64> {
    (acc.data.len() >= 72).then(|| u64::from_le_bytes(acc.data[64..72].try_into().unwrap()))
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let out_path = {
        let args: Vec<String> = std::env::args().collect();
        args.iter()
            .position(|a| a == "--out")
            .and_then(|i| args.get(i + 1).cloned())
            .unwrap_or_else(|| "markets.generated.json".to_string())
    };
    let rpc_url = std::env::var("RPC_ENDPOINT").context("RPC_ENDPOINT required")?;
    let min_reserve: u64 = std::env::var("MIN_WSOL_RESERVE_LAMPORTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000_000_000); // 2 SOL
    let max_markets: usize = std::env::var("MAX_MARKETS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);

    let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());
    let pump_prog = Pubkey::from_str(pump_amm::PUMP_AMM_PROGRAM_ID)?;
    let dlmm_prog = Pubkey::from_str(meteora_dlmm::DLMM_PROGRAM_ID)?;
    let mut stats = DiscoveryStats::default();

    // 1) Universe scan (dataSlice = the non-WSOL mint only).
    info!("scanning PumpSwap pools (quote=WSOL, base=WSOL)…");
    let mut pump_by_token: HashMap<String, Vec<String>> = HashMap::new();
    for (memcmp_off, mint_off) in [(75usize, 43usize), (43, 75)] {
        let rows = gpa_slice(&rpc, &pump_prog, 301, memcmp_off, WSOL_MINT, mint_off, 32).await?;
        stats.pump_wsol_pools += rows.len();
        for (pk, data) in rows {
            if data.len() == 32 {
                let mint = Pubkey::new_from_array(data.try_into().unwrap()).to_string();
                pump_by_token.entry(mint).or_default().push(pk.to_string());
            }
        }
    }
    info!(
        pools = stats.pump_wsol_pools,
        tokens = pump_by_token.len(),
        "pump universe"
    );

    info!("scanning Meteora DLMM pairs (y=WSOL, x=WSOL)…");
    let mut dlmm_by_token: HashMap<String, Vec<String>> = HashMap::new();
    for (memcmp_off, mint_off) in [(120usize, 88usize), (88, 120)] {
        let rows = gpa_slice(&rpc, &dlmm_prog, 904, memcmp_off, WSOL_MINT, mint_off, 32).await?;
        stats.dlmm_wsol_pairs += rows.len();
        for (pk, data) in rows {
            if data.len() == 32 {
                let mint = Pubkey::new_from_array(data.try_into().unwrap()).to_string();
                dlmm_by_token.entry(mint).or_default().push(pk.to_string());
            }
        }
    }
    info!(
        pairs = stats.dlmm_wsol_pairs,
        tokens = dlmm_by_token.len(),
        "dlmm universe"
    );

    // 2) Intersection.
    let candidates = intersect_tokens(&pump_by_token, &dlmm_by_token);
    stats.tokens_intersecting = candidates.len();
    info!(tokens = candidates.len(), "tokens on BOTH venues");

    // 3) Hydrate + structurally validate with the real decoders.
    // token → (best pump pool, best dlmm pair) chosen later by WSOL reserve.
    let all_pump: Vec<Pubkey> = candidates
        .iter()
        .flat_map(|(_, p, _)| p.iter())
        .filter_map(|s| Pubkey::from_str(s).ok())
        .collect();
    let all_dlmm: Vec<Pubkey> = candidates
        .iter()
        .flat_map(|(_, _, d)| d.iter())
        .filter_map(|s| Pubkey::from_str(s).ok())
        .collect();
    info!(
        pump = all_pump.len(),
        dlmm = all_dlmm.len(),
        "hydrating pool accounts…"
    );
    let pump_accounts = get_many(&rpc, &all_pump).await?;
    let dlmm_accounts = get_many(&rpc, &all_dlmm).await?;

    struct PumpCand {
        pool: Pubkey,
        base_is_wsol: bool,
        base_vault: Pubkey,
        quote_vault: Pubkey,
        wsol_vault: Pubkey,
        fee_verified: bool,
    }
    struct DlmmCand {
        pair: Pubkey,
        x_is_wsol: bool,
        reserve_x: Pubkey,
        reserve_y: Pubkey,
        wsol_reserve_acc: Pubkey,
        bin_step: u16,
    }
    let wsol = Pubkey::from_str(WSOL_MINT)?;
    let mut pump_cands: HashMap<String, Vec<PumpCand>> = HashMap::new();
    for (pk, acc) in all_pump.iter().zip(&pump_accounts) {
        let Some(acc) = acc else { continue };
        if acc.owner != pump_prog {
            continue; // ownership check: never trust GPA blindly
        }
        let Ok(p) = decode_pump_pool(&acc.data) else {
            continue;
        };
        let base_is_wsol = p.base_mint == wsol;
        let token = if base_is_wsol {
            p.quote_mint
        } else {
            p.base_mint
        };
        if !base_is_wsol && p.quote_mint != wsol {
            continue;
        }
        if p.has_creator() {
            // Creator pools: SELL leg exact, BUY leg refused (see pump_amm).
            stats.pump_fee_unverified += 1;
        }
        pump_cands
            .entry(token.to_string())
            .or_default()
            .push(PumpCand {
                pool: *pk,
                base_is_wsol,
                base_vault: p.base_vault,
                quote_vault: p.quote_vault,
                wsol_vault: if base_is_wsol {
                    p.base_vault
                } else {
                    p.quote_vault
                },
                fee_verified: !p.has_creator(),
            });
    }
    let mut dlmm_cands: HashMap<String, Vec<DlmmCand>> = HashMap::new();
    for (pk, acc) in all_dlmm.iter().zip(&dlmm_accounts) {
        let Some(acc) = acc else { continue };
        if acc.owner != dlmm_prog {
            continue;
        }
        let Ok(d) = decode_lb_pair(&acc.data) else {
            continue;
        };
        // Same gating as the quote: enabled + permissionless types only.
        if d.status != 0 || d.pair_type == 1 || d.pair_type == 2 {
            continue;
        }
        let x_is_wsol = d.token_x_mint == wsol;
        if !x_is_wsol && d.token_y_mint != wsol {
            continue;
        }
        let token = if x_is_wsol {
            d.token_y_mint
        } else {
            d.token_x_mint
        };
        dlmm_cands
            .entry(token.to_string())
            .or_default()
            .push(DlmmCand {
                pair: *pk,
                x_is_wsol,
                reserve_x: d.reserve_x,
                reserve_y: d.reserve_y,
                wsol_reserve_acc: if x_is_wsol { d.reserve_x } else { d.reserve_y },
                bin_step: d.bin_step,
            });
    }
    let structurally_valid: Vec<String> = candidates
        .iter()
        .map(|(m, _, _)| m.clone())
        .filter(|m| pump_cands.contains_key(m) && dlmm_cands.contains_key(m))
        .collect();
    stats.structurally_valid = structurally_valid.len();
    info!(
        tokens = structurally_valid.len(),
        "structurally valid on both venues"
    );

    // 4) WSOL reserve balances (one account per candidate pool/pair).
    let mut reserve_keys: Vec<Pubkey> = Vec::new();
    for m in &structurally_valid {
        for c in &pump_cands[m] {
            reserve_keys.push(c.wsol_vault);
        }
        for c in &dlmm_cands[m] {
            reserve_keys.push(c.wsol_reserve_acc);
        }
    }
    info!(accounts = reserve_keys.len(), "fetching WSOL reserves…");
    let reserve_accounts = get_many(&rpc, &reserve_keys).await?;
    let reserves: HashMap<Pubkey, u64> = reserve_keys
        .iter()
        .zip(&reserve_accounts)
        .filter_map(|(k, a)| a.as_ref().and_then(token_amount).map(|v| (*k, v)))
        .collect();

    // Pick the deepest pool per venue per token; apply the liquidity floor.
    struct Survivor {
        token: String,
        pump: PumpCand,
        pump_wsol: u64,
        dlmm: DlmmCand,
        dlmm_wsol: u64,
    }
    let mut survivors: Vec<Survivor> = Vec::new();
    for m in structurally_valid {
        let pump_list = pump_cands.remove(&m).unwrap();
        let dlmm_list = dlmm_cands.remove(&m).unwrap();
        let best_pump = pump_list
            .into_iter()
            .map(|c| {
                let r = reserves.get(&c.wsol_vault).copied().unwrap_or(0);
                (r, c)
            })
            .max_by_key(|(r, _)| *r);
        let best_dlmm = dlmm_list
            .into_iter()
            .map(|c| {
                let r = reserves.get(&c.wsol_reserve_acc).copied().unwrap_or(0);
                (r, c)
            })
            .max_by_key(|(r, _)| *r);
        if let (Some((pr, pc)), Some((dr, dc))) = (best_pump, best_dlmm) {
            if pr >= min_reserve && dr >= min_reserve {
                survivors.push(Survivor {
                    token: m,
                    pump: pc,
                    pump_wsol: pr,
                    dlmm: dc,
                    dlmm_wsol: dr,
                });
            }
        }
    }
    stats.above_liquidity_floor = survivors.len();
    info!(
        tokens = survivors.len(),
        floor_sol = min_reserve as f64 / 1e9,
        "above liquidity floor on both venues"
    );

    // 5) Mint safety screen.
    let mint_keys: Vec<Pubkey> = survivors
        .iter()
        .filter_map(|s| Pubkey::from_str(&s.token).ok())
        .collect();
    let mint_accounts = get_many(&rpc, &mint_keys).await?;
    let t22 = Pubkey::from_str(TOKEN_2022_PROGRAM)?;
    let spl = Pubkey::from_str(TOKEN_PROGRAM)?;

    let mut markets: Vec<DiscoveredMarket> = Vec::new();
    for (s, mint_acc) in survivors.into_iter().zip(mint_accounts) {
        let Some(acc) = mint_acc else { continue };
        let token_2022 = if acc.owner == t22 {
            true
        } else if acc.owner == spl {
            false
        } else {
            warn!(token = %s.token, owner = %acc.owner, "mint owned by unknown program — rejected");
            continue;
        };
        let Ok(safety) = parse_mint_safety(&acc.data, token_2022) else {
            warn!(token = %s.token, "mint failed to parse — rejected");
            continue;
        };
        let safe = safety.is_safe();
        if safe {
            stats.safe += 1;
        } else {
            stats.rejected_unsafe += 1;
        }
        markets.push(DiscoveredMarket {
            token_mint: s.token,
            decimals: safety.decimals,
            token_2022,
            pump_pool: s.pump.pool.to_string(),
            pump_base_is_wsol: s.pump.base_is_wsol,
            pump_base_vault: s.pump.base_vault.to_string(),
            pump_quote_vault: s.pump.quote_vault.to_string(),
            pump_fee_verified: s.pump.fee_verified,
            pump_wsol_reserve: s.pump_wsol,
            dlmm_pair: s.dlmm.pair.to_string(),
            dlmm_x_is_wsol: s.dlmm.x_is_wsol,
            dlmm_reserve_x: s.dlmm.reserve_x.to_string(),
            dlmm_reserve_y: s.dlmm.reserve_y.to_string(),
            dlmm_bin_step: s.dlmm.bin_step,
            dlmm_wsol_reserve: s.dlmm_wsol,
            safety,
            safe,
            rank_lamports: s.pump_wsol.min(s.dlmm_wsol),
        });
    }

    // 6) Rank (thinner-side WSOL depth) and cap.
    markets.sort_by_key(|m| std::cmp::Reverse(m.rank_lamports));
    markets.truncate(max_markets);

    let slot = rpc.get_slot().await.unwrap_or(0);
    let cache = DiscoveryCache {
        version: CACHE_VERSION,
        generated_at_ms: now_ms(),
        rpc_slot: slot,
        stats: stats.clone(),
        markets,
    };
    std::fs::write(&out_path, cache.to_json())?;

    // 7) Funnel report.
    println!("\n══════════ DISCOVERY FUNNEL ══════════");
    println!("pump WSOL pools:            {}", stats.pump_wsol_pools);
    println!("dlmm WSOL pairs:            {}", stats.dlmm_wsol_pairs);
    println!("tokens on both venues:      {}", stats.tokens_intersecting);
    println!("structurally valid:         {}", stats.structurally_valid);
    println!(
        "pump pools w/ creator fee:  {} (quote refuses until verified)",
        stats.pump_fee_unverified
    );
    println!(
        "above {:.1} SOL floor both:  {}",
        min_reserve as f64 / 1e9,
        stats.above_liquidity_floor
    );
    println!("safe mints:                 {}", stats.safe);
    println!("rejected unsafe mints:      {}", stats.rejected_unsafe);
    println!(
        "cache written:              {out_path} (top {} by thinner-side depth)",
        cache.markets.len()
    );
    println!("\nTop 15 markets (rank = min(pump,dlmm) WSOL depth):");
    for m in cache.markets.iter().take(15) {
        println!(
            "  {} pump={:.2} SOL dlmm={:.2} SOL step={} feeV={} safe={}",
            &m.token_mint[..8],
            m.pump_wsol_reserve as f64 / 1e9,
            m.dlmm_wsol_reserve as f64 / 1e9,
            m.dlmm_bin_step,
            m.pump_fee_verified,
            m.safe
        );
    }
    Ok(())
}
