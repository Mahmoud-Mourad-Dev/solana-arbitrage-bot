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

    println!("=== S14B-2 Meteora↔Whirlpool discovery (quote-only, single-tick clamped) ===");
    println!("discovering routes…");
    let routes = discover(&rpc)?;
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
    let clamp_hits = events
        .iter()
        .filter_map(|e| e.xdex.as_ref())
        .filter(|x| x.no_tick_crossed)
        .count();
    println!(
        "\nsweeps={sweeps} events={} ok={ok} clamp-confirmed-no-cross={clamp_hits}",
        events.len()
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
    let (profitable, gross, net, size, xprov) = match optimize(route, wsol, cost, grid) {
        Some(c) => {
            // Re-evaluate once through the same route (spec §6).
            let (leg1_fee, leg2_fee) = route
                .evaluate(wsol, c.amount_in, cost)
                .map(|cc| (cc.leg1_fee, cc.leg2_fee))
                .unwrap_or((c.leg1_fee, c.leg2_fee));
            let no_cross = route.round_trip(wsol, c.amount_in).is_ok();
            let x = XdexProvenance {
                meteora_pair: r.meteora_pair.to_string(),
                whirlpool_pool: r.whirlpool.to_string(),
                direction: format!("WSOL->{}->WSOL", sym(&r.token)),
                amount_in: c.amount_in,
                token_mid: c.token_mid,
                wsol_out: c.wsol_out,
                meteora_fee: leg1_fee,
                whirlpool_fee: leg2_fee,
                whirlpool_tick_current: tick_cur,
                whirlpool_single_tick_capacity: cap,
                no_tick_crossed: no_cross,
                meteora_slot: met_slot,
                whirlpool_slot: wp_slot,
                meteora_pair_hash: met_hash,
                whirlpool_pool_hash: wp_hash,
            };
            (
                c.net_profit >= 0,
                c.gross_profit,
                c.net_profit,
                c.amount_in,
                Some(x),
            )
        }
        None => (
            false,
            0,
            0,
            0,
            Some(XdexProvenance {
                meteora_pair: r.meteora_pair.to_string(),
                whirlpool_pool: r.whirlpool.to_string(),
                direction: format!("WSOL->{}->WSOL", sym(&r.token)),
                amount_in: 0,
                token_mid: 0,
                wsol_out: 0,
                meteora_fee: 0,
                whirlpool_fee: 0,
                whirlpool_tick_current: tick_cur,
                whirlpool_single_tick_capacity: cap,
                no_tick_crossed: true,
                meteora_slot: met_slot,
                whirlpool_slot: wp_slot,
                meteora_pair_hash: met_hash,
                whirlpool_pool_hash: wp_hash,
            }),
        ),
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
