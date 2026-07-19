//! `observe-xdex` — S14B-2 Meteora DLMM ↔ Orca Whirlpool cross-DEX discovery.
//!
//! Cycle (ONLY): `WSOL → token on Meteora DLMM → WSOL on Orca Whirlpool swapV2`,
//! token ∈ {USDC, USDT}. QUOTE-ONLY, read-only. NEVER builds/signs/simulates/
//! submits. Whirlpool leg is SINGLE-TICK-CLAMPED (real crossing parity unproven
//! — see docs/whirlpool-parity-verdict.md); the optimizer can never explore a
//! size that crosses a tick. Uses the shared route engine + optimizer +
//! aggregate_narrow so discovery/optimizer/report cannot diverge.
//!
//! Usage: observe-xdex [--capture <secs>]   (default 45s smoke)
//! Env: RPC_ENDPOINT, XDEX_INTERVAL_SECS(5), XDEX_MAX_SOL(5), XDEX_OUT_DIR.

use anyhow::{bail, Context, Result};
use arb_monitor::meteora_dlmm::{decode_bin_array, decode_lb_pair, BinArray, LbPair};
use arb_monitor::narrow_report::{
    aggregate_narrow, parse_narrow_jsonl, PollEvent, RunManifest, XdexProvenance,
};
use arb_monitor::observe_live::{atomic_write, git_commit, gzip, now_ms, sha256_hex};
use arb_monitor::observe_report::competitive_model;
use arb_monitor::optimizer::{optimize, SizeGrid};
use arb_monitor::parsers::decode_token_amount;
use arb_monitor::route_engine::{Leg, Route, WhirlpoolLegState};
use arb_monitor::types::{tick_array_pda, tick_array_starts_around};
use arb_monitor::whirlpool_parity::{
    validate_mint, validate_pool, validate_tick_array, validate_vault, WHIRLPOOL_PROGRAM_ID,
};
use solana_account_decoder::UiAccountEncoding;
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};
use solana_client::rpc_filter::{Memcmp, MemcmpEncodedBytes, RpcFilterType};
use solana_sdk::account::Account;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::sysvar::clock::ID as CLOCK_ID;
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::str::FromStr;
use std::time::{Duration, Instant};

const WSOL: &str = "So11111111111111111111111111111111111111112";
const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
const USDT: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";
const DLMM_PROGRAM: &str = "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo";
const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

fn pk(s: &str) -> Pubkey {
    Pubkey::from_str(s).unwrap()
}

/// A validated cross-DEX route: Meteora pair (WSOL→token) + Whirlpool (token→WSOL).
#[derive(Clone)]
struct XRoute {
    token: Pubkey, // USDC or USDT
    meteora_pair: Pubkey,
    dlmm_bv: Pubkey, // token X vault
    dlmm_qv: Pubkey, // token Y vault
    whirlpool: Pubkey,
    wp_vault_a: Pubkey,
    wp_vault_b: Pubkey,
    wp_mint_a: Pubkey,
    wp_mint_b: Pubkey,
}

fn discover(rpc: &RpcClient) -> Result<Vec<XRoute>> {
    let mut out = Vec::new();
    for (market, quote) in [("WSOL/USDC", USDC), ("WSOL/USDT", USDT)] {
        let met = discover_meteora(rpc, quote).with_context(|| format!("meteora {market}"))?;
        let whp = discover_whirlpool(rpc, quote).with_context(|| format!("whirlpool {market}"))?;
        let (Some(met), Some(whp)) = (met, whp) else {
            println!(
                "  {market}: missing a venue (meteora={} whirlpool={})",
                met.is_some(),
                whp.is_some()
            );
            continue;
        };
        println!(
            "  {market}: meteora {} + whirlpool {} (ts {})",
            &met.0.to_string()[..8],
            &whp.0.to_string()[..8],
            whp.5
        );
        out.push(XRoute {
            token: pk(quote),
            meteora_pair: met.0,
            dlmm_bv: met.1,
            dlmm_qv: met.2,
            whirlpool: whp.0,
            wp_vault_a: whp.1,
            wp_vault_b: whp.2,
            wp_mint_a: whp.3,
            wp_mint_b: whp.4,
        });
    }
    if out.is_empty() {
        bail!("no cross-DEX routes discovered");
    }
    Ok(out)
}

/// A candidate pool on one venue, keyed later by its non-WSOL token.
struct MetCand {
    pair: Pubkey,
    reserve_x: Pubkey,
    reserve_y: Pubkey,
    wsol_vault: Pubkey,
}
struct WhpCand {
    pool: Pubkey,
    va: Pubkey,
    vb: Pubkey,
    ma: Pubkey,
    mb: Pubkey,
    wsol_vault: Pubkey,
    liquidity: u128,
}

/// S14B-3 WIDE discovery: enumerate ALL WSOL-paired Meteora DLMM + Orca
/// Whirlpool pools, join strictly by exact token mint, screen the token mint
/// (classic SPL, no mint/freeze authority, no extensions), gate liquidity via
/// WSOL-vault balance on BOTH legs, and enumerate up to ~50 validated route
/// combinations (top pools per venue per token — not only the single deepest).
/// Returns (routes, rejection tally, stats line).
fn discover_wide(
    rpc: &RpcClient,
    max_routes: usize,
) -> Result<(Vec<XRoute>, BTreeMap<String, usize>, String)> {
    let wsol = pk(WSOL);
    let mut reject: BTreeMap<String, usize> = BTreeMap::new();
    let mut bump = |k: &str| *reject.entry(k.to_string()).or_default() += 1;

    // ── enumerate Meteora WSOL pairs (token_x@88 / token_y@120). ──
    let mut met_by_token: HashMap<Pubkey, Vec<MetCand>> = HashMap::new();
    let mut met_pools = 0usize;
    for (off_wsol, wsol_is_x) in [(88u64, true), (120u64, false)] {
        for (addr, acc) in gpa(rpc, DLMM_PROGRAM, 904, off_wsol, WSOL)? {
            let Ok(p) = decode_lb_pair(&acc.data) else {
                bump("meteora_decode");
                continue;
            };
            if p.status != 0 {
                bump("meteora_disabled");
                continue;
            }
            met_pools += 1;
            let token = if wsol_is_x {
                p.token_y_mint
            } else {
                p.token_x_mint
            };
            if token == wsol {
                continue;
            }
            let wsol_vault = if wsol_is_x { p.reserve_x } else { p.reserve_y };
            met_by_token.entry(token).or_default().push(MetCand {
                pair: addr,
                reserve_x: p.reserve_x,
                reserve_y: p.reserve_y,
                wsol_vault,
            });
        }
    }

    // ── enumerate Whirlpool WSOL pools (mint_a@101 / mint_b@181). ──
    let mut whp_by_token: HashMap<Pubkey, Vec<WhpCand>> = HashMap::new();
    let mut whp_pools = 0usize;
    for (off_wsol, wsol_is_a) in [(101u64, true), (181u64, false)] {
        for (addr, acc) in gpa(rpc, WHIRLPOOL_PROGRAM_ID, 653, off_wsol, WSOL)? {
            if acc.owner != pk(WHIRLPOOL_PROGRAM_ID) {
                continue;
            }
            let Some(d) = arb_monitor::parsers::decode_whirlpool(&acc.data) else {
                bump("whirlpool_decode");
                continue;
            };
            if d.liquidity == 0 {
                bump("whirlpool_empty");
                continue;
            }
            whp_pools += 1;
            let token = if wsol_is_a {
                d.token_mint_b
            } else {
                d.token_mint_a
            };
            if token == wsol {
                continue;
            }
            let wsol_vault = if wsol_is_a {
                d.token_vault_a
            } else {
                d.token_vault_b
            };
            whp_by_token.entry(token).or_default().push(WhpCand {
                pool: addr,
                va: d.token_vault_a,
                vb: d.token_vault_b,
                ma: d.token_mint_a,
                mb: d.token_mint_b,
                wsol_vault,
                liquidity: d.liquidity,
            });
        }
    }

    // ── join by exact token mint. ──
    let shared: Vec<Pubkey> = met_by_token
        .keys()
        .filter(|t| whp_by_token.contains_key(*t))
        .cloned()
        .collect();

    // ── screen each shared token mint (batched). ──
    let mint_keys: Vec<Pubkey> = shared.clone();
    let mint_accs = get_multi(rpc, &mint_keys)?;
    let mut safe_tokens: Vec<Pubkey> = Vec::new();
    for (t, acc) in shared.iter().zip(&mint_accs) {
        let Some(acc) = acc else {
            bump("mint_missing");
            continue;
        };
        match arb_monitor::mint_safety::screen_mint(&acc.owner.to_string(), &acc.data) {
            Ok(()) => safe_tokens.push(*t),
            Err(e) => bump(&format!("mint:{e:?}").replace(char::is_whitespace, "")),
        }
    }

    // ── liquidity gate + rank: rank whirlpool cands by on-pool liquidity; for
    // Meteora rank by WSOL vault balance (batched). Keep top-2 per venue. ──
    // Fetch WSOL vault balances for all cand pools of safe tokens.
    let mut vault_keys: Vec<Pubkey> = Vec::new();
    for t in &safe_tokens {
        for m in &met_by_token[t] {
            vault_keys.push(m.wsol_vault);
        }
        for w in &whp_by_token[t] {
            vault_keys.push(w.wsol_vault);
        }
    }
    let vault_accs = get_multi(rpc, &vault_keys)?;
    let bal: HashMap<Pubkey, u64> = vault_keys
        .iter()
        .zip(&vault_accs)
        .filter_map(|(k, a)| {
            a.as_ref()
                .and_then(|a| decode_token_amount(&a.data))
                .map(|b| (*k, b))
        })
        .collect();
    const MIN_WSOL: u64 = 2_000_000_000; // 2 SOL executable depth on each leg

    let mut routes: Vec<XRoute> = Vec::new();
    for t in &safe_tokens {
        let mut mets: Vec<&MetCand> = met_by_token[t]
            .iter()
            .filter(|m| bal.get(&m.wsol_vault).copied().unwrap_or(0) >= MIN_WSOL)
            .collect();
        mets.sort_by_key(|m| std::cmp::Reverse(bal.get(&m.wsol_vault).copied().unwrap_or(0)));
        let mut whps: Vec<&WhpCand> = whp_by_token[t]
            .iter()
            .filter(|w| bal.get(&w.wsol_vault).copied().unwrap_or(0) >= MIN_WSOL)
            .collect();
        whps.sort_by_key(|w| std::cmp::Reverse(w.liquidity));
        if mets.is_empty() {
            bump("meteora_thin");
        }
        if whps.is_empty() {
            bump("whirlpool_thin");
        }
        // Combinations: top-2 × top-2 per token (not only the single deepest).
        for m in mets.iter().take(2) {
            for w in whps.iter().take(2) {
                routes.push(XRoute {
                    token: *t,
                    meteora_pair: m.pair,
                    dlmm_bv: m.reserve_x,
                    dlmm_qv: m.reserve_y,
                    whirlpool: w.pool,
                    wp_vault_a: w.va,
                    wp_vault_b: w.vb,
                    wp_mint_a: w.ma,
                    wp_mint_b: w.mb,
                });
                if routes.len() >= max_routes {
                    break;
                }
            }
            if routes.len() >= max_routes {
                break;
            }
        }
        if routes.len() >= max_routes {
            break;
        }
    }

    let stats = format!(
        "meteora_wsol_pools={met_pools} whirlpool_wsol_pools={whp_pools} shared_tokens={} safe_tokens={} routes={}",
        shared.len(),
        safe_tokens.len(),
        routes.len()
    );
    Ok((routes, reject, stats))
}

/// getProgramAccounts with a dataSize + single memcmp filter (base58 pubkey).
fn gpa(
    rpc: &RpcClient,
    program: &str,
    data_size: u64,
    offset: u64,
    memcmp_b58: &str,
) -> Result<Vec<(Pubkey, Account)>> {
    let cfg = RpcProgramAccountsConfig {
        filters: Some(vec![
            RpcFilterType::DataSize(data_size),
            RpcFilterType::Memcmp(Memcmp::new(
                offset as usize,
                MemcmpEncodedBytes::Base58(memcmp_b58.to_string()),
            )),
        ]),
        account_config: RpcAccountInfoConfig {
            encoding: Some(UiAccountEncoding::Base64),
            commitment: Some(CommitmentConfig::confirmed()),
            ..Default::default()
        },
        ..Default::default()
    };
    Ok(rpc.get_program_accounts_with_config(&pk(program), cfg)?)
}

/// Batched getMultipleAccounts in chunks of 100.
fn get_multi(rpc: &RpcClient, keys: &[Pubkey]) -> Result<Vec<Option<Account>>> {
    let mut out = Vec::with_capacity(keys.len());
    for chunk in keys.chunks(100) {
        out.extend(rpc.get_multiple_accounts(chunk)?);
    }
    Ok(out)
}

/// Deepest Meteora DLMM WSOL/quote pair → (pair, base_vault, quote_vault).
fn discover_meteora(rpc: &RpcClient, quote: &str) -> Result<Option<(Pubkey, Pubkey, Pubkey)>> {
    let program = pk(DLMM_PROGRAM);
    let mut best: Option<(u64, Pubkey, Pubkey, Pubkey)> = None;
    for (mx, my) in [(WSOL, quote), (quote, WSOL)] {
        let filters = vec![
            RpcFilterType::DataSize(904),
            RpcFilterType::Memcmp(Memcmp::new(88, MemcmpEncodedBytes::Base58(mx.to_string()))),
            RpcFilterType::Memcmp(Memcmp::new(120, MemcmpEncodedBytes::Base58(my.to_string()))),
        ];
        let cfg = RpcProgramAccountsConfig {
            filters: Some(filters),
            account_config: RpcAccountInfoConfig {
                encoding: Some(UiAccountEncoding::Base64),
                commitment: Some(CommitmentConfig::confirmed()),
                ..Default::default()
            },
            ..Default::default()
        };
        for (addr, acc) in rpc.get_program_accounts_with_config(&program, cfg)? {
            let Ok(p) = decode_lb_pair(&acc.data) else {
                continue;
            };
            if p.status != 0 {
                continue;
            }
            let wsol_vault = if mx == WSOL { p.reserve_x } else { p.reserve_y };
            let bal = rpc
                .get_account(&wsol_vault)
                .ok()
                .and_then(|a| decode_token_amount(&a.data))
                .unwrap_or(0);
            if bal < 5_000_000_000 {
                continue; // < 5 SOL depth
            }
            if best.as_ref().map(|b| bal > b.0).unwrap_or(true) {
                best = Some((bal, addr, p.reserve_x, p.reserve_y));
            }
        }
    }
    Ok(best.map(|(_, a, bv, qv)| (a, bv, qv)))
}

/// Deepest Whirlpool WSOL/quote pool → (pool, vaultA, vaultB, mintA, mintB, ts).
#[allow(clippy::type_complexity)]
fn discover_whirlpool(
    rpc: &RpcClient,
    quote: &str,
) -> Result<Option<(Pubkey, Pubkey, Pubkey, Pubkey, Pubkey, u16)>> {
    let program = pk(WHIRLPOOL_PROGRAM_ID);
    let mut best: Option<(u64, Pubkey, Pubkey, Pubkey, Pubkey, Pubkey, u16)> = None;
    for (ma, mb) in [(WSOL, quote), (quote, WSOL)] {
        let filters = vec![
            RpcFilterType::DataSize(653),
            RpcFilterType::Memcmp(Memcmp::new(101, MemcmpEncodedBytes::Base58(ma.to_string()))),
            RpcFilterType::Memcmp(Memcmp::new(181, MemcmpEncodedBytes::Base58(mb.to_string()))),
        ];
        let cfg = RpcProgramAccountsConfig {
            filters: Some(filters),
            account_config: RpcAccountInfoConfig {
                encoding: Some(UiAccountEncoding::Base64),
                commitment: Some(CommitmentConfig::confirmed()),
                ..Default::default()
            },
            ..Default::default()
        };
        for (addr, acc) in rpc.get_program_accounts_with_config(&program, cfg)? {
            let Ok(d) = validate_pool(&acc.owner, &acc.data, &pk(ma), &pk(mb)) else {
                continue;
            };
            if d.liquidity == 0 {
                continue;
            }
            // classic-SPL only.
            let (Ok(a_acc), Ok(b_acc)) = (
                rpc.get_account(&d.token_mint_a),
                rpc.get_account(&d.token_mint_b),
            ) else {
                continue;
            };
            if validate_mint(&a_acc.owner, a_acc.data.len()).is_err()
                || validate_mint(&b_acc.owner, b_acc.data.len()).is_err()
                || a_acc.owner != pk(TOKEN_PROGRAM)
                || b_acc.owner != pk(TOKEN_PROGRAM)
            {
                continue;
            }
            let wsol_vault = if ma == WSOL {
                d.token_vault_a
            } else {
                d.token_vault_b
            };
            let bal = rpc
                .get_account(&wsol_vault)
                .ok()
                .and_then(|a| decode_token_amount(&a.data))
                .unwrap_or(0);
            if bal < 5_000_000_000 {
                continue;
            }
            if best.as_ref().map(|b| bal > b.0).unwrap_or(true) {
                best = Some((
                    bal,
                    addr,
                    d.token_vault_a,
                    d.token_vault_b,
                    d.token_mint_a,
                    d.token_mint_b,
                    d.tick_spacing,
                ));
            }
        }
    }
    Ok(best.map(|(_, a, va, vb, ma, mb, ts)| (a, va, vb, ma, mb, ts)))
}

fn cluster_time(rpc: &RpcClient) -> i64 {
    rpc.get_account(&CLOCK_ID)
        .ok()
        .filter(|a| a.data.len() >= 40)
        .map(|a| i64::from_le_bytes(a.data[32..40].try_into().unwrap()))
        .unwrap_or_else(|| (now_ms() / 1000) as i64)
}

/// Build the two-leg Route for a route at one slot each, with full provenance.
/// Returns (Route, XdexProvenance-scaffold) or a typed reason string.
#[allow(clippy::type_complexity)]
fn snapshot_route(
    rpc: &RpcClient,
    r: &XRoute,
    now_unix: i64,
) -> Result<(Route, u64, u64, String, String, i32, u16), String> {
    let wsol = pk(WSOL);
    // ── Meteora leg (WSOL→token). Fetch pair + bin arrays around active. ──
    let met_pair_acc = rpc
        .get_account(&r.meteora_pair)
        .map_err(|_| "meteora_fetch")?;
    if met_pair_acc.owner != pk(DLMM_PROGRAM) {
        return Err("meteora_owner".into());
    }
    let pair: LbPair = decode_lb_pair(&met_pair_acc.data).map_err(|_| "meteora_decode")?;
    // provenance: pair sides are exactly {WSOL, token}, vaults match decode.
    let sides = [pair.token_x_mint, pair.token_y_mint];
    if !(sides.contains(&wsol) && sides.contains(&r.token)) {
        return Err("meteora_mint_mismatch".into());
    }
    if pair.reserve_x != r.dlmm_bv || pair.reserve_y != r.dlmm_qv {
        return Err("meteora_vault_identity".into());
    }
    let met_slot = rpc.get_slot().unwrap_or(0);
    let aidx = (pair.active_id as i64).div_euclid(70);
    let idxs: Vec<i64> = (aidx - 2..=aidx + 2).collect();
    let bin_keys: Vec<Pubkey> = idxs
        .iter()
        .map(|&i| {
            Pubkey::find_program_address(
                &[b"bin_array", r.meteora_pair.as_ref(), &i.to_le_bytes()],
                &pk(DLMM_PROGRAM),
            )
            .0
        })
        .collect();
    let bin_accs = rpc
        .get_multiple_accounts(&bin_keys)
        .map_err(|_| "meteora_bins_fetch")?;
    let mut arrays: HashMap<i64, BinArray> = HashMap::new();
    for (i, acc) in idxs.iter().zip(&bin_accs) {
        if let Some(acc) = acc {
            if acc.owner == pk(DLMM_PROGRAM) {
                if let Ok(ba) = decode_bin_array(&acc.data) {
                    if ba.lb_pair == r.meteora_pair {
                        arrays.insert(*i, ba);
                    }
                }
            }
        }
    }
    let met_hash = sha256_hex(&met_pair_acc.data);
    let leg1 = Leg::Meteora {
        pair,
        arrays,
        now_unix,
    };

    // ── Whirlpool leg (token→WSOL), single-tick clamped. ──
    let pool_k = r.whirlpool;
    let pool_acc = rpc.get_account(&pool_k).map_err(|_| "whirlpool_fetch")?;
    let d = validate_pool(&pool_acc.owner, &pool_acc.data, &r.wp_mint_a, &r.wp_mint_b)
        .map_err(|e| format!("whirlpool_pool:{e:?}"))?;
    let wp_slot = rpc.get_slot().unwrap_or(0);
    let starts = tick_array_starts_around(d.tick_current_index, d.tick_spacing);
    let ta_keys: Vec<Pubkey> = starts.iter().map(|&s| tick_array_pda(&pool_k, s)).collect();
    let mut keys = vec![r.wp_vault_a, r.wp_vault_b];
    keys.extend(ta_keys.iter().cloned());
    let accs = rpc
        .get_multiple_accounts(&keys)
        .map_err(|_| "whirlpool_accs_fetch")?;
    let (Some(va), Some(vb)) = (&accs[0], &accs[1]) else {
        return Err("whirlpool_vault_missing".into());
    };
    validate_vault(
        &r.wp_vault_a,
        &d.token_vault_a,
        &va.owner,
        &va.data,
        &d.token_mint_a,
        &pool_k,
    )
    .map_err(|e| format!("whirlpool_vault_a:{e:?}"))?;
    validate_vault(
        &r.wp_vault_b,
        &d.token_vault_b,
        &vb.owner,
        &vb.data,
        &d.token_mint_b,
        &pool_k,
    )
    .map_err(|e| format!("whirlpool_vault_b:{e:?}"))?;
    let mut initialized_ticks: Vec<(i32, i128)> = Vec::new();
    let (mut lowest, mut highest) = (i32::MAX, i32::MIN);
    for (n, acc) in accs[2..].iter().enumerate() {
        let Some(acc) = acc else { continue };
        let ta = validate_tick_array(n, &ta_keys[n], &acc.data, &pool_k, d.tick_spacing)
            .map_err(|e| format!("whirlpool_tick_array:{e:?}"))?;
        lowest = lowest.min(ta.start_tick_index);
        highest = highest.max(ta.start_tick_index);
        for (i, t) in ta.ticks.iter().enumerate() {
            if t.initialized {
                initialized_ticks.push((
                    ta.start_tick_index + i as i32 * d.tick_spacing as i32,
                    t.liquidity_net,
                ));
            }
        }
    }
    if lowest == i32::MAX {
        return Err("whirlpool_no_tick_arrays".into());
    }
    let wp_hash = sha256_hex(&pool_acc.data);
    let tick_cur = d.tick_current_index;
    let ts = d.tick_spacing;
    let leg2 = Leg::Whirlpool(WhirlpoolLegState {
        mint_a: d.token_mint_a,
        mint_b: d.token_mint_b,
        sqrt_price_x64: d.sqrt_price_x64,
        liquidity: d.liquidity,
        tick_current_index: d.tick_current_index,
        tick_spacing: d.tick_spacing,
        fee_ppm: d.fee_rate_ppm,
        initialized_ticks,
        lowest_start: lowest,
        highest_start: highest,
    });

    Ok((
        Route { leg1, leg2 },
        met_slot,
        wp_slot,
        met_hash,
        wp_hash,
        tick_cur,
        ts,
    ))
}

fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt().with_env_filter("info").init();
    let rpc_url = std::env::var("RPC_ENDPOINT").context("RPC_ENDPOINT required")?;
    let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());

    let args: Vec<String> = std::env::args().collect();
    let secs: u64 = args
        .iter()
        .position(|a| a == "--capture")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(45);
    let interval = Duration::from_secs(
        std::env::var("XDEX_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5),
    );
    let max_sol: f64 = std::env::var("XDEX_MAX_SOL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5.0);
    let out_dir = std::env::var("XDEX_OUT_DIR").unwrap_or_else(|_| "reports/xdex".into());
    std::fs::create_dir_all(&out_dir).ok();

    let wide = args.iter().any(|a| a == "--wide");
    let max_routes: usize = std::env::var("XDEX_MAX_ROUTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);
    let routes = if wide {
        println!(
            "=== S14B-3 WIDE Meteora↔Whirlpool discovery (quote-only, single-tick clamped) ==="
        );
        println!("enumerating all WSOL-paired pools on both venues…");
        let (routes, reject, stats) = discover_wide(&rpc, max_routes)?;
        println!("  {stats}");
        println!("  rejections (typed): {reject:?}");
        if routes.is_empty() {
            bail!("wide discovery produced no validated routes");
        }
        for r in &routes {
            println!(
                "  route token {} : meteora {} × whirlpool {}",
                &r.token.to_string()[..8],
                &r.meteora_pair.to_string()[..8],
                &r.whirlpool.to_string()[..8]
            );
        }
        routes
    } else {
        println!("=== S14B-2 Meteora↔Whirlpool discovery (quote-only, single-tick clamped) ===");
        println!("discovering routes…");
        discover(&rpc)?
    };
    let wsol = pk(WSOL);
    let cost = competitive_model();
    let grid = SizeGrid {
        min: 5_000_000,
        max: (max_sol * 1e9) as u64,
        ..Default::default()
    };

    let run_id = now_ms();
    let jsonl_path = format!("{out_dir}/xdex-{run_id}.jsonl");
    let report_path = format!("{out_dir}/xdex-report-{run_id}.json");
    let mut jsonl = std::fs::File::create(&jsonl_path)?;
    let token_of: BTreeMap<String, String> = routes
        .iter()
        .map(|r| (r.meteora_pair.to_string(), r.token.to_string()))
        .collect();
    let manifest = RunManifest {
        manifest_version: 1,
        report_version: 2,
        commit: git_commit(),
        run_id,
        started_at_ms: run_id,
        target_period_ms: interval.as_millis() as u64,
        frozen_secs: 600,
        control_tokens: vec![],
        token_of: token_of.clone(),
        excluded_unsafe: vec![],
        fee_schema: "xdex-meteora-whirlpool-v1".into(),
    };
    writeln!(jsonl, "{}", serde_json::to_string(&manifest)?)?;

    let mut events: Vec<PollEvent> = Vec::new();
    let start = Instant::now();
    let mut sweeps = 0u64;
    while start.elapsed() < Duration::from_secs(secs) {
        let now_unix = cluster_time(&rpc);
        for r in &routes {
            let key = format!(
                "{}|{}|WSOL->{}->WSOL",
                r.meteora_pair,
                r.whirlpool,
                sym(&r.token)
            );
            let ev = match snapshot_route(&rpc, r, now_unix) {
                Ok((route, met_slot, wp_slot, met_hash, wp_hash, tick_cur, _ts)) => build_event(
                    &key, r, &route, &wsol, &cost, &grid, met_slot, wp_slot, met_hash, wp_hash,
                    tick_cur,
                ),
                Err(reason) => fail_event(&key, &reason),
            };
            writeln!(jsonl, "{}", serde_json::to_string(&ev)?)?;
            events.push(ev);
        }
        jsonl.flush().ok();
        sweeps += 1;
        std::thread::sleep(interval);
    }

    let m = aggregate_narrow(&events, &token_of, &[], 600);
    let report = serde_json::json!({
        "run": { "id": run_id, "commit": git_commit(), "kind": "xdex-discovery-smoke",
                 "duration_secs": start.elapsed().as_secs(), "sweeps": sweeps,
                 "cost_basis": "modeled competitive — quote-only, single-tick clamped, no tx built" },
        "metrics": m,
    });
    atomic_write(&report_path, &serde_json::to_string_pretty(&report)?)?;
    gzip(&report_path);
    gzip(&jsonl_path);

    // Offline equivalence self-check (manifest-driven, no flags).
    let body = std::fs::read_to_string(&jsonl_path).unwrap_or_default();
    let (_pm, pe, _ok, _bad) = parse_narrow_jsonl(&body);
    let off = aggregate_narrow(&pe, &token_of, &[], 600);
    let equiv = serde_json::to_value(&m).unwrap() == serde_json::to_value(&off).unwrap();

    let ok = events.iter().filter(|e| e.is_ok()).count();
    let xs: Vec<&XdexProvenance> = events.iter().filter_map(|e| e.xdex.as_ref()).collect();
    let gross_positive = xs.iter().filter(|x| x.best_gross_lamports > 0).count();
    let competitive_positive = events
        .iter()
        .filter(|e| e.is_ok() && e.profitable_competitive)
        .count();
    let clamp_binding = xs.iter().filter(|x| x.clamp_binding).count();
    let best_gross = xs.iter().map(|x| x.best_gross_lamports).max().unwrap_or(0);
    let best_gross_route = xs.iter().max_by_key(|x| x.best_gross_lamports);
    let best_net = events
        .iter()
        .map(|e| e.competitive_net_lamports)
        .max()
        .unwrap_or(0);
    println!(
        "\nsweeps={sweeps} events={} ok={ok} valid_polls={ok}",
        events.len()
    );
    println!(
        "gross-positive candidates={gross_positive}  competitive-positive candidates={competitive_positive}"
    );
    println!(
        "best gross edge={best_gross} lamports ({:.6} SOL){}",
        best_gross as f64 / 1e9,
        best_gross_route
            .filter(|x| x.best_gross_lamports > 0)
            .map(|x| format!("  @size {} on {}", x.best_gross_size, &x.meteora_pair[..8]))
            .unwrap_or_default()
    );
    println!("best competitive net={best_net} lamports");
    println!(
        "single-tick clamp BINDING on {}/{} ok polls",
        clamp_binding, ok
    );
    println!(
        "episodes={} competitive-positive-polls: routes classed A/Fl/Fr/N = {}/{}/{}/{}",
        m.episodes_total,
        m.class_active,
        m.class_flicker,
        m.class_frozen_spread,
        m.class_never_profitable
    );
    println!(
        "causal detect/day = {} lamports",
        m.causal_at_detection_per_day_lamports
    );
    println!("offline==live metrics: {equiv}");
    println!("report: {report_path}(.gz)  jsonl: {jsonl_path}(.gz)");
    Ok(())
}

fn sym(mint: &Pubkey) -> &'static str {
    match mint.to_string().as_str() {
        USDC => "USDC",
        USDT => "USDT",
        _ => "TOK",
    }
}

/// Best fee-less round-trip edge (wsol_out - amount_in) over a coarse log grid,
/// ignoring costs. round_trip already refuses a Whirlpool tick crossing, so this
/// stays within the single-tick clamp. Returns (best_edge, size_at_best).
fn best_gross_edge(route: &Route, wsol: &Pubkey, grid: &SizeGrid) -> (i128, u64) {
    let mut best = (i128::MIN, 0u64);
    let n = 24u32;
    let (lmin, lmax) = ((grid.min.max(1) as f64).ln(), (grid.max.max(2) as f64).ln());
    for i in 0..n {
        let t = i as f64 / (n - 1) as f64;
        let size = (lmin + (lmax - lmin) * t).exp().round() as u64;
        if let Ok((_mid, out)) = route.round_trip(wsol, size) {
            let edge = out as i128 - size as i128;
            if edge > best.0 {
                best = (edge, size);
            }
        }
    }
    if best.0 == i128::MIN {
        (0, 0)
    } else {
        best
    }
}

#[allow(clippy::too_many_arguments)]
fn build_event(
    key: &str,
    r: &XRoute,
    route: &Route,
    wsol: &Pubkey,
    cost: &arb_common::cost::CostModel,
    grid: &SizeGrid,
    met_slot: u64,
    wp_slot: u64,
    met_hash: String,
    wp_hash: String,
    tick_cur: i32,
) -> PollEvent {
    let cap = match &route.leg2 {
        Leg::Whirlpool(w) => w.single_tick_capacity_for(&r.token),
        _ => 0,
    };
    let clamp_binding = cap < grid.max;
    // GROSS-positive signal: best fee-less round-trip edge over a coarse grid,
    // ignoring costs and clamped to within-tick sizes (round_trip rejects a
    // crossing). Separate from the competitive (cost-gated) optimum.
    let (best_gross, best_gross_size) = best_gross_edge(route, wsol, grid);
    // Fee provenance / competitive optimum.
    let opt = optimize(route, wsol, cost, grid);
    let (amount_in, token_mid, wsol_out, meteora_fee, whirlpool_fee) = match &opt {
        Some(c) => (c.amount_in, c.token_mid, c.wsol_out, c.leg1_fee, c.leg2_fee),
        None => (0, 0, 0, 0, 0),
    };
    let no_cross = if amount_in > 0 {
        route.round_trip(wsol, amount_in).is_ok()
    } else {
        true
    };
    let x = XdexProvenance {
        meteora_pair: r.meteora_pair.to_string(),
        whirlpool_pool: r.whirlpool.to_string(),
        direction: format!("WSOL->{}->WSOL", sym(&r.token)),
        amount_in,
        token_mid,
        wsol_out,
        meteora_fee,
        whirlpool_fee,
        whirlpool_tick_current: tick_cur,
        whirlpool_single_tick_capacity: cap,
        no_tick_crossed: no_cross,
        best_gross_lamports: best_gross,
        best_gross_size,
        clamp_binding,
        meteora_slot: met_slot,
        whirlpool_slot: wp_slot,
        meteora_pair_hash: met_hash,
        whirlpool_pool_hash: wp_hash,
    };
    let (profitable, gross, net, size, xprov) = match &opt {
        Some(c) => (
            c.net_profit >= 0,
            c.gross_profit,
            c.net_profit,
            c.amount_in,
            Some(x),
        ),
        None => (false, 0, 0, 0, Some(x)),
    };
    PollEvent {
        route: key.to_string(),
        at_ms: now_ms(),
        slot: wp_slot,
        kind: "poll".into(),
        profitable_competitive: profitable,
        gross_lamports: gross,
        competitive_net_lamports: net,
        size_lamports: size,
        fingerprint: i128::MIN,
        snapshot_latency_ms: 0,
        reconfirm_delay_ms: None,
        episode_start_ms: None,
        valid_snapshot: true,
        poll_status: "ok".into(),
        reject_reason: None,
        rpc_error: None,
        fee_v2: None,
        xdex: xprov,
    }
}

fn fail_event(key: &str, reason: &str) -> PollEvent {
    let transient = reason.contains("fetch");
    PollEvent {
        route: key.to_string(),
        at_ms: now_ms(),
        slot: 0,
        kind: "poll".into(),
        profitable_competitive: false,
        gross_lamports: 0,
        competitive_net_lamports: 0,
        size_lamports: 0,
        fingerprint: i128::MIN,
        snapshot_latency_ms: 0,
        reconfirm_delay_ms: None,
        episode_start_ms: None,
        valid_snapshot: false,
        poll_status: if transient {
            "rpc_error".into()
        } else {
            "snapshot_invalid".into()
        },
        reject_reason: Some(reason.to_string()),
        rpc_error: transient.then(|| reason.to_string()),
        fee_v2: None,
        xdex: None,
    }
}
