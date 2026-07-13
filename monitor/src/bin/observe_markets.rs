//! `observe-markets` — S13 observe mode with confirmation + evidence.
//!
//! Loads the discovery cache and, each cycle, snapshots every market's quote
//! inputs in ONE getMultipleAccounts (single slot), builds both
//! WSOL→token→WSOL routes, optimizes size, and for each candidate performs an
//! immediate fresh single-slot CONFIRMATION (optionally a second one) to test
//! whether the signal survives. Every candidate is streamed to JSONL with full
//! economics, cost breakdown, latencies, slot, and confirmation results. At the
//! end it writes an aggregated + sensitivity report.
//!
//! **It NEVER builds, signs, simulates, or submits a transaction.** It is a
//! monitor. A candidate is a signal, not a fill.
//!
//! Usage: cargo run -p arb-monitor --bin observe-markets [--cache PATH] [--once]
//! Env: RPC_ENDPOINT (secrets are redacted from all logs), MODE,
//!      OBS_MIN_NET_LAMPORTS (default 0), OBS_MAX_SOL (default 20),
//!      OBS_INTERVAL_SECS (default 15), OBS_DURATION_SECS (default 86400),
//!      OBS_DOUBLE_CONFIRM (default true), OBS_OUT_DIR (default reports/observe).

use anyhow::{Context, Result};
use arb_common::cost::{CostModel, ExecutionPayment};
use arb_common::mode::Mode;
use arb_monitor::market_discovery::{DiscoveredMarket, DiscoveryCache, WSOL_MINT};
use arb_monitor::meteora_dlmm::{self, decode_bin_array, decode_lb_pair, BinArray};
use arb_monitor::observe_report::{
    aggregate, default_scenarios, redact_secrets, sensitivity, CandidateRecord, Confirmation,
    CostBreakdown,
};
use arb_monitor::optimizer::{optimize, SizeGrid};
use arb_monitor::pump_amm::{self, decode_pump_pool};
use arb_monitor::route_engine::{Leg, Route, RouteReject};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::account::Account;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::sysvar::clock::ID as CLOCK_ID;
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
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

fn bin_array_pda(pair: &Pubkey, index: i64, program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"bin_array", pair.as_ref(), &index.to_le_bytes()],
        program,
    )
    .0
}

fn git_commit() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Programs + WSOL, resolved once.
struct Ctx {
    pump_prog: Pubkey,
    dlmm_prog: Pubkey,
    wsol: Pubkey,
}

/// The two venue legs for a market at a single slot.
struct Snapshot {
    slot: u64,
    pump_leg: Leg,
    dlmm_leg: Leg,
}

/// Fetch a market's quote inputs at ONE slot (pair-probe to pick the bin-array
/// window, then a single getMultipleAccounts). Returns None with a reason on
/// any missing/invalid state — never a guess.
async fn fetch_snapshot(
    rpc: &RpcClient,
    ctx: &Ctx,
    m: &DiscoveredMarket,
    now_unix: i64,
    secrets: &[&str],
) -> Result<Snapshot, &'static str> {
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
            warn!(error = %redact_secrets(&e.to_string(), secrets), "pair probe failed");
            return Err("pair_fetch");
        }
    };
    let aidx = (active_id as i64).div_euclid(70);
    let idxs: Vec<i64> = (aidx - 2..=aidx + 2).collect();
    let mut keys = vec![pool_k, bv, qv, pair_k];
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
            warn!(error = %redact_secrets(&e.to_string(), secrets), "snapshot fetch failed");
            return Err("snapshot_fetch");
        }
    };
    let slot = resp.context.slot;
    let v = resp.value;
    let (Some(pool_acc), Some(bv_acc), Some(qv_acc), Some(pair_acc)) = (&v[0], &v[1], &v[2], &v[3])
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
    let mut arrays: HashMap<i64, BinArray> = HashMap::new();
    for (i, acc) in idxs.iter().zip(&v[4..]) {
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
        pump_leg: Leg::Pump {
            pool,
            base_reserve,
            quote_reserve,
        },
        dlmm_leg: Leg::Meteora {
            pair,
            arrays,
            now_unix,
        },
    })
}

fn routes_for(snap: Snapshot) -> [(&'static str, Route); 2] {
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

    let mode: Mode = std::env::var("MODE")
        .ok()
        .and_then(|m| m.parse().ok())
        .unwrap_or(Mode::Observe);
    let min_net: u64 = env_u64("OBS_MIN_NET_LAMPORTS", 0);
    let max_sol: f64 = std::env::var("OBS_MAX_SOL")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20.0);
    let interval = Duration::from_secs(env_u64("OBS_INTERVAL_SECS", 15));
    let duration = Duration::from_secs(env_u64("OBS_DURATION_SECS", 86_400));
    let double_confirm = std::env::var("OBS_DOUBLE_CONFIRM")
        .map(|v| v != "false" && v != "0")
        .unwrap_or(true);
    let out_dir = std::env::var("OBS_OUT_DIR").unwrap_or_else(|_| "reports/observe".to_string());
    std::fs::create_dir_all(&out_dir).ok();

    let rpc_url = std::env::var("RPC_ENDPOINT").context("RPC_ENDPOINT required")?;
    let wss_url = std::env::var("RPC_WSS_ENDPOINT").unwrap_or_default();
    let secrets: Vec<&str> = [rpc_url.as_str(), wss_url.as_str()]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect();

    let rpc = RpcClient::new_with_commitment(rpc_url.clone(), CommitmentConfig::confirmed());
    let ctx = Ctx {
        pump_prog: Pubkey::from_str(pump_amm::PUMP_AMM_PROGRAM_ID)?,
        dlmm_prog: Pubkey::from_str(meteora_dlmm::DLMM_PROGRAM_ID)?,
        wsol: Pubkey::from_str(WSOL_MINT)?,
    };

    let raw = std::fs::read_to_string(&cache_path)
        .with_context(|| format!("read discovery cache {cache_path}"))?;
    let cache = DiscoveryCache::from_json(&raw)
        .context("cache version mismatch — re-run discover-markets")?;

    let run_id = now_ms();
    let jsonl_path = format!("{out_dir}/candidates-{run_id}.jsonl");
    let mut jsonl = std::fs::File::create(&jsonl_path)?;
    info!(
        markets = cache.markets.len(),
        mode = %mode,
        commit = %git_commit(),
        jsonl = %jsonl_path,
        "observe-markets S13 starting (NEVER submits)"
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

    let mut all_records: Vec<CandidateRecord> = Vec::new();
    let mut scan_secs: Vec<f64> = Vec::new();
    let mut reject_totals: BTreeMap<String, u64> = BTreeMap::new();
    let run_start = Instant::now();

    loop {
        let cycle_start = Instant::now();
        let mut cycle_candidates = 0usize;

        let now_unix = match rpc.get_account(&CLOCK_ID).await {
            Ok(a) if a.data.len() >= 40 => i64::from_le_bytes(a.data[32..40].try_into().unwrap()),
            _ => (now_ms() / 1000) as i64,
        };

        for m in &cache.markets {
            let mkt_start = Instant::now();
            let rpc_t0 = Instant::now();
            let snap = match fetch_snapshot(&rpc, &ctx, m, now_unix, &secrets).await {
                Ok(s) => s,
                Err(reason) => {
                    *reject_totals
                        .entry(format!("no_state:{reason}"))
                        .or_default() += 1;
                    continue;
                }
            };
            let rpc_latency_ms = rpc_t0.elapsed().as_millis() as u64;
            let context_slot = snap.slot;

            for (label, route) in routes_for(snap) {
                if route.token_mint(&ctx.wsol).is_none() {
                    *reject_totals.entry("topology".into()).or_default() += 1;
                    continue;
                }
                let Some(c) = optimize(&route, &ctx.wsol, &cost, &grid) else {
                    // Classify why (probe at a mid size).
                    let key = match route.evaluate(&ctx.wsol, grid.max / 10, &cost) {
                        Err(RouteReject::Leg1(_)) => "leg1",
                        Err(RouteReject::Leg2(_)) => "leg2",
                        Err(RouteReject::NonPositiveGross) => "non_positive_gross",
                        Err(RouteReject::BelowNet { .. }) => "below_net",
                        Err(RouteReject::TopologyMismatch) => "topology",
                        Ok(_) => "below_net", // optimize None but a size works ⇒ under floor
                    };
                    *reject_totals.entry(key.into()).or_default() += 1;
                    continue;
                };
                cycle_candidates += 1;
                let detected_at_ms = now_ms();

                // pump/meteora fee attribution depends on which leg is which.
                let (pump_fee, meteora_fee) = if label == "pump->meteora" {
                    (c.leg1_fee, c.leg2_fee)
                } else {
                    (c.leg2_fee, c.leg1_fee)
                };

                // Immediate fresh single-slot confirmation(s).
                let confirm1 = confirm(
                    &rpc,
                    &ctx,
                    m,
                    now_unix,
                    &secrets,
                    label,
                    &cost,
                    &grid,
                    detected_at_ms,
                )
                .await;
                let confirm2 = if double_confirm {
                    Some(
                        confirm(
                            &rpc,
                            &ctx,
                            m,
                            now_unix,
                            &secrets,
                            label,
                            &cost,
                            &grid,
                            detected_at_ms,
                        )
                        .await,
                    )
                } else {
                    None
                };

                let rec = CandidateRecord {
                    detected_at_ms,
                    context_slot,
                    token_mint: m.token_mint.clone(),
                    pump_pool: m.pump_pool.clone(),
                    dlmm_pair: m.dlmm_pair.clone(),
                    direction: label.to_string(),
                    amount_in: c.amount_in,
                    gross_profit: c.gross_profit,
                    pump_fee,
                    meteora_fee,
                    cost: CostBreakdown::from_model(&cost, c.gross_profit),
                    net_profit: c.net_profit,
                    rpc_latency_ms,
                    scan_latency_ms: mkt_start.elapsed().as_millis() as u64,
                    candidate_age_ms: now_ms().saturating_sub(detected_at_ms),
                    confirm1: Some(confirm1),
                    confirm2,
                };
                writeln!(jsonl, "{}", serde_json::to_string(&rec)?)?;
                jsonl.flush().ok();
                all_records.push(rec);
            }
        }

        let cycle_secs = cycle_start.elapsed().as_secs_f64();
        scan_secs.push(cycle_secs);
        let confirmed = all_records
            .iter()
            .rev()
            .take(cycle_candidates)
            .filter(|r| r.confirmed_once())
            .count();
        info!(
            cycle_secs = format!("{cycle_secs:.1}"),
            candidates = cycle_candidates,
            confirmed_this_cycle = confirmed,
            total_records = all_records.len(),
            "cycle complete"
        );

        if once || run_start.elapsed() >= duration {
            break;
        }
        tokio::time::sleep(interval).await;
    }

    // ── Final report ──────────────────────────────────────────────────
    let agg = aggregate(&all_records, &scan_secs, reject_totals.clone());
    let sens = sensitivity(&all_records, &default_scenarios());
    let scan_median = agg.scan_median_secs;
    let report = serde_json::json!({
        "run": {
            "id": run_id,
            "commit": git_commit(),
            "mode": mode.to_string(),
            "markets": cache.markets.len(),
            "duration_secs": run_start.elapsed().as_secs(),
            "cycles": scan_secs.len(),
            "min_net_lamports": min_net,
            "max_sol": max_sol,
            "double_confirm": double_confirm,
            "note": "observe-only; never builds/simulates/submits a transaction. \
                     Candidates are single-slot-consistent MONITOR signals.",
        },
        "aggregate": agg,
        "sensitivity": sens,
        "throughput": {
            "scan_median_secs": scan_median,
            "explanation": throughput_verdict(scan_median, agg.persistence_median_ms, agg.single_confirm_survivors),
        },
        "acceptance": {
            "question": "Do profitable signals survive fresh single-slot confirmation and persist long enough to justify building the execution layer?",
            "single_confirm_survivors": agg.single_confirm_survivors,
            "double_confirm_survivors": agg.double_confirm_survivors,
            "persistence_median_ms": agg.persistence_median_ms,
            "persistence_exceeds_cycle": agg.persistence_exceeds_cycle,
            "verdict": acceptance_verdict(&agg),
        },
        "jsonl_log": jsonl_path,
    });
    let report_path = format!("{out_dir}/report-{run_id}.json");
    std::fs::write(&report_path, serde_json::to_string_pretty(&report)?)?;

    // Compress the report + JSONL (best-effort; gzip is standard on the box).
    for p in [&report_path, &jsonl_path] {
        let _ = std::process::Command::new("gzip").args(["-kf", p]).status();
    }

    println!("\n════════ S13 OBSERVE RUN COMPLETE ════════");
    println!(
        "cycles={} raw_signals={} unique={} conf1={} conf2={} persist_median={}ms",
        agg.scan_cycles,
        agg.raw_signals,
        agg.unique_opportunities,
        agg.single_confirm_survivors,
        agg.double_confirm_survivors,
        agg.persistence_median_ms
    );
    println!(
        "scan median {:.1}s → {}",
        scan_median,
        throughput_verdict(
            scan_median,
            agg.persistence_median_ms,
            agg.single_confirm_survivors
        )
    );
    println!("sensitivity (confirmed survivors under rising cost):");
    for s in &sens {
        println!(
            "  {:12} survivors={} net_p50={} net_max={}",
            s.label, s.survivors, s.net_p50, s.net_max
        );
    }
    println!("ACCEPTANCE: {}", acceptance_verdict(&agg));
    println!("report: {report_path}(.gz)  jsonl: {jsonl_path}(.gz)");
    Ok(())
}

/// Fresh single-slot re-fetch + re-optimize for one direction; is it still a
/// candidate netting ≥ 0?
#[allow(clippy::too_many_arguments)]
async fn confirm(
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
                latency_ms: now_ms().saturating_sub(detected_at_ms),
                ..Default::default()
            }
        }
    };
    let slot = snap.slot;
    let route = routes_for(snap)
        .into_iter()
        .find(|(l, _)| *l == label)
        .map(|(_, r)| r)
        .unwrap();
    match optimize(&route, &ctx.wsol, cost, grid) {
        Some(c) if c.net_profit >= 0 => Confirmation {
            survived: true,
            context_slot: slot,
            net_profit: c.net_profit,
            gross_profit: c.gross_profit,
            latency_ms: now_ms().saturating_sub(detected_at_ms),
        },
        _ => Confirmation {
            survived: false,
            context_slot: slot,
            latency_ms: now_ms().saturating_sub(detected_at_ms),
            ..Default::default()
        },
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn throughput_verdict(scan_median: f64, persist_median_ms: u64, survivors: usize) -> String {
    if survivors == 0 {
        return "no confirmed survivors — throughput is moot until an edge exists".into();
    }
    let persist_s = persist_median_ms as f64 / 1000.0;
    if persist_s >= scan_median {
        format!(
            "median edge persists {persist_s:.0}s ≥ {scan_median:.0}s cycle — a next-cycle actor could plausibly catch it (execution latency still unmodeled)"
        )
    } else {
        format!(
            "median edge persists {persist_s:.0}s < {scan_median:.0}s cycle — a {scan_median:.0}s scan is too slow to act on the typical edge; faster ingestion (Geyser) would be required"
        )
    }
}

fn acceptance_verdict(agg: &arb_monitor::observe_report::Aggregate) -> String {
    if agg.single_confirm_survivors == 0 {
        "NO — zero signals survived fresh single-slot confirmation. Do NOT build the execution layer on this strategy yet.".into()
    } else if agg.double_confirm_survivors == 0 {
        "WEAK — some signals confirmed once but none twice; edges are fleeting. Execution layer not justified without faster ingestion.".into()
    } else if !agg.persistence_exceeds_cycle {
        "MARGINAL — signals confirm but do not persist a full scan cycle; a 75–98s poll cannot act on them. Reassess with faster ingestion before building execution.".into()
    } else {
        "PROMISING — signals survive double confirmation AND persist ≥ one cycle. Worth a deeper look, but still a monitor signal, not proven executable/profitable.".into()
    }
}
