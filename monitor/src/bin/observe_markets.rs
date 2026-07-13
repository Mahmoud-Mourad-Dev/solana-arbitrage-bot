//! `observe-markets` — S12 observe mode.
//!
//! Loads the discovery cache, snapshots live state for each market at a single
//! slot, builds both WSOL→token→WSOL routes (Pump-first and Meteora-first),
//! optimizes the size with the two-stage optimizer, and reports candidates +
//! a full rejection taxonomy. **It NEVER builds, signs, or submits anything.**
//!
//! Usage: cargo run -p arb-monitor --bin observe-markets [--cache PATH] [--once]
//! Env: RPC_ENDPOINT, MODE (must not be `live` here — this binary can't submit
//!      regardless), OBS_MIN_NET_LAMPORTS (default 0), OBS_MAX_SOL (default 20),
//!      OBS_INTERVAL_SECS (default 15).

use anyhow::{Context, Result};
use arb_common::cost::{CostModel, ExecutionPayment};
use arb_common::mode::Mode;
use arb_monitor::market_discovery::{DiscoveryCache, WSOL_MINT};
use arb_monitor::meteora_dlmm::{self, decode_bin_array, decode_lb_pair, BinArray};
use arb_monitor::optimizer::{optimize, SizeGrid};
use arb_monitor::pump_amm::{self, decode_pump_pool};
use arb_monitor::route_engine::{Candidate, Leg, Route, RouteReject};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::account::Account;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::sysvar::clock::ID as CLOCK_ID;
use std::collections::HashMap;
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn token_amount(acc: &Account) -> Option<u64> {
    (acc.data.len() >= 72).then(|| u64::from_le_bytes(acc.data[64..72].try_into().unwrap()))
}

/// Bin-array PDA: seeds [b"bin_array", pair, index_le_i64].
fn bin_array_pda(pair: &Pubkey, index: i64, program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"bin_array", pair.as_ref(), &index.to_le_bytes()],
        program,
    )
    .0
}

async fn get_many(rpc: &RpcClient, keys: &[Pubkey]) -> Result<Vec<Option<Account>>> {
    let mut out = Vec::with_capacity(keys.len());
    for chunk in keys.chunks(100) {
        out.extend(rpc.get_multiple_accounts(chunk).await?);
    }
    Ok(out)
}

#[derive(Default)]
struct RejectTally {
    topology: u64,
    leg1: u64,
    leg2: u64,
    non_positive: u64,
    below_net: u64,
    no_state: u64,
    dead_route: u64,
}

fn tally(t: &mut RejectTally, r: &RouteReject) {
    match r {
        RouteReject::TopologyMismatch => t.topology += 1,
        RouteReject::Leg1(_) => t.leg1 += 1,
        RouteReject::Leg2(_) => t.leg2 += 1,
        RouteReject::NonPositiveGross => t.non_positive += 1,
        RouteReject::BelowNet { .. } => t.below_net += 1,
    }
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
    let cache_path = args
        .iter()
        .position(|a| a == "--cache")
        .and_then(|i| args.get(i + 1).cloned())
        .unwrap_or_else(|| "markets.generated.json".to_string());
    let once = args.iter().any(|a| a == "--once");

    // Mode is informational here; observe-markets physically cannot submit.
    let mode: Mode = std::env::var("MODE")
        .ok()
        .and_then(|m| m.parse().ok())
        .unwrap_or(Mode::Observe);
    let min_net: u64 = std::env::var("OBS_MIN_NET_LAMPORTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let max_sol: f64 = std::env::var("OBS_MAX_SOL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20.0);
    let interval = Duration::from_secs(
        std::env::var("OBS_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(15),
    );

    let rpc_url = std::env::var("RPC_ENDPOINT").context("RPC_ENDPOINT required")?;
    let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());
    let dlmm_prog = Pubkey::from_str(meteora_dlmm::DLMM_PROGRAM_ID)?;
    let pump_prog = Pubkey::from_str(pump_amm::PUMP_AMM_PROGRAM_ID)?;
    let wsol = Pubkey::from_str(WSOL_MINT)?;

    let raw = std::fs::read_to_string(&cache_path)
        .with_context(|| format!("read discovery cache {cache_path}"))?;
    let cache = DiscoveryCache::from_json(&raw)
        .context("cache version mismatch — re-run discover-markets")?;
    info!(
        markets = cache.markets.len(),
        mode = %mode,
        "observe-markets starting (NEVER submits)"
    );

    let cost = CostModel {
        signature_fee_lamports: 5_000,
        compute_unit_limit: 600_000,
        compute_unit_price_micro: 10_000,
        margin_lamports: 10_000,
        required_net_lamports: min_net,
        payment: ExecutionPayment::JitoTip {
            min_lamports: 10_000,
            max_lamports: 100_000_000,
        },
        ..Default::default()
    };
    let grid = SizeGrid {
        min: 10_000_000,
        max: (max_sol * 1e9) as u64,
        ..Default::default()
    };

    loop {
        let started = std::time::Instant::now();
        let mut candidates: Vec<(String, &'static str, Candidate)> = Vec::new();
        let mut rejects = RejectTally::default();
        let mut best_below: Option<(String, i128, u64)> = None; // token, net, gross

        // Cluster time for DLMM volatility decay.
        let now_unix = match rpc.get_account(&CLOCK_ID).await {
            Ok(a) if a.data.len() >= 40 => i64::from_le_bytes(a.data[32..40].try_into().unwrap()),
            _ => (now_ms() / 1000) as i64,
        };

        for m in &cache.markets {
            // Phase 1: fetch pump pool + vaults, dlmm pair.
            let (Ok(pump_pool_k), Ok(pump_bv), Ok(pump_qv), Ok(dlmm_pair_k)) = (
                Pubkey::from_str(&m.pump_pool),
                Pubkey::from_str(&m.pump_base_vault),
                Pubkey::from_str(&m.pump_quote_vault),
                Pubkey::from_str(&m.dlmm_pair),
            ) else {
                rejects.no_state += 1;
                continue;
            };
            let phase1 = match get_many(&rpc, &[pump_pool_k, pump_bv, pump_qv, dlmm_pair_k]).await {
                Ok(v) => v,
                Err(e) => {
                    warn!(token = %m.token_mint, error = %e, "phase1 fetch failed");
                    rejects.no_state += 1;
                    continue;
                }
            };
            let (Some(pool_acc), Some(bv_acc), Some(qv_acc), Some(pair_acc)) =
                (&phase1[0], &phase1[1], &phase1[2], &phase1[3])
            else {
                rejects.no_state += 1;
                continue;
            };
            if pool_acc.owner != pump_prog || pair_acc.owner != dlmm_prog {
                rejects.no_state += 1;
                continue;
            }
            let (Ok(pool), Ok(pair)) = (
                decode_pump_pool(&pool_acc.data),
                decode_lb_pair(&pair_acc.data),
            ) else {
                rejects.no_state += 1;
                continue;
            };
            let (Some(base_reserve), Some(quote_reserve)) =
                (token_amount(bv_acc), token_amount(qv_acc))
            else {
                rejects.no_state += 1;
                continue;
            };

            // Phase 2: bin arrays around the (fresh) active id.
            let aidx = (pair.active_id as i64).div_euclid(70);
            let idxs: Vec<i64> = (aidx - 2..=aidx + 2).collect();
            let arr_keys: Vec<Pubkey> = idxs
                .iter()
                .map(|&i| bin_array_pda(&dlmm_pair_k, i, &dlmm_prog))
                .collect();
            let arr_accs = match get_many(&rpc, &arr_keys).await {
                Ok(v) => v,
                Err(_) => {
                    rejects.no_state += 1;
                    continue;
                }
            };
            let mut arrays: HashMap<i64, BinArray> = HashMap::new();
            for (i, acc) in idxs.iter().zip(&arr_accs) {
                if let Some(acc) = acc {
                    if acc.owner == dlmm_prog {
                        if let Ok(ba) = decode_bin_array(&acc.data) {
                            if ba.lb_pair == dlmm_pair_k {
                                arrays.insert(*i, ba);
                            }
                        }
                    }
                }
            }

            let pump_leg = Leg::Pump {
                pool: pool.clone(),
                base_reserve,
                quote_reserve,
            };
            let dlmm_leg = Leg::Meteora {
                pair: pair.clone(),
                arrays,
                now_unix,
            };

            // Both directions.
            let routes = [
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
            ];
            for (label, route) in routes {
                if route.token_mint(&wsol).is_none() {
                    rejects.topology += 1;
                    continue;
                }
                match optimize(&route, &wsol, &cost, &grid) {
                    Some(c) => candidates.push((m.token_mint.clone(), label, c)),
                    None => {
                        // Probe once at a mid size to classify WHY.
                        match route.evaluate(&wsol, grid.max / 10, &cost) {
                            Err(r) => {
                                if matches!(r, RouteReject::Leg1(_) | RouteReject::Leg2(_)) {
                                    // structural vs capacity already handled in
                                    // optimize; count as leg reject / dead.
                                    rejects.dead_route += 1;
                                }
                                tally(&mut rejects, &r);
                            }
                            Ok(c) => {
                                // optimize returned None but a size works — must
                                // be below the net floor at the probe size.
                                if best_below
                                    .as_ref()
                                    .map(|b| c.net_profit > b.1)
                                    .unwrap_or(true)
                                {
                                    best_below =
                                        Some((m.token_mint.clone(), c.net_profit, c.gross_profit));
                                }
                            }
                        }
                    }
                }
            }
        }

        candidates.sort_by_key(|(_, _, c)| std::cmp::Reverse(c.net_profit));
        let report = serde_json::json!({
            "generated_at_ms": now_ms(),
            "mode": mode.to_string(),
            "markets_scanned": cache.markets.len(),
            "cluster_time": now_unix,
            "candidates": candidates.iter().map(|(tok, dir, c)| serde_json::json!({
                "token": tok, "direction": dir,
                "amount_in_sol": c.amount_in as f64 / 1e9,
                "gross_profit_lamports": c.gross_profit,
                "net_profit_lamports": c.net_profit,
                "wsol_out": c.wsol_out,
                "token_mid": c.token_mid,
                "tip_lamports": c.payment,
            })).collect::<Vec<_>>(),
            "rejects": {
                "topology": rejects.topology,
                "leg1": rejects.leg1,
                "leg2": rejects.leg2,
                "non_positive_gross": rejects.non_positive,
                "below_net_floor": rejects.below_net,
                "no_live_state": rejects.no_state,
                "dead_route": rejects.dead_route,
            },
            "best_near_miss": best_below.as_ref().map(|(t, net, gross)| serde_json::json!({
                "token": t, "net_profit_lamports": net, "gross_profit_lamports": gross,
            })),
            "scan_secs": started.elapsed().as_secs_f64(),
            "caveat": "UNCONFIRMED: state is snapshotted across two RPC rounds \
                       (pool/pair then bin arrays) so candidates may be cross-slot \
                       artifacts. A single-slot confirmation gate is required before \
                       any candidate is trusted. observe-markets never submits.",
        });
        let path = format!("observe-report-{}.json", now_ms());
        std::fs::write(&path, serde_json::to_string_pretty(&report)?)?;

        println!(
            "\n════════ OBSERVE SCAN ({:.1}s) ════════",
            started.elapsed().as_secs_f64()
        );
        println!("markets scanned:     {}", cache.markets.len());
        println!(
            "CANDIDATES (net ≥ {} lamports): {}",
            min_net,
            candidates.len()
        );
        for (tok, dir, c) in candidates.iter().take(20) {
            println!(
                "  ✅ {} {} in={:.3} SOL gross={} net={} tip={}",
                &tok[..8.min(tok.len())],
                dir,
                c.amount_in as f64 / 1e9,
                c.gross_profit,
                c.net_profit,
                c.payment,
            );
        }
        if let Some((t, net, gross)) = &best_below {
            println!(
                "best near-miss (below floor): {} net={net} gross={gross}",
                &t[..8.min(t.len())]
            );
        }
        println!(
            "rejects: topo={} leg1={} leg2={} nonPos={} belowNet={} noState={} dead={}",
            rejects.topology,
            rejects.leg1,
            rejects.leg2,
            rejects.non_positive,
            rejects.below_net,
            rejects.no_state,
            rejects.dead_route
        );
        println!("report: {path}");
        if !candidates.is_empty() {
            println!(
                "⚠ candidates are UNCONFIRMED (cross-slot snapshot); need a \
                 single-slot re-quote before they mean anything. Never submitted."
            );
        }

        if once {
            break;
        }
        tokio::time::sleep(interval).await;
    }
    Ok(())
}
