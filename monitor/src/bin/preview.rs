//! arb-preview — dry-run the discovery engine against LIVE mainnet using
//! plain RPC polling instead of Yellowstone Geyser.
//!
//! Purpose: before paying for a Geyser subscription, find out whether your
//! configured pool set (pools.json) actually produces profitable cycles at
//! all, and see the real numbers. It runs the SAME registry + DiscoveryEngine
//! the production monitor uses — only the data source differs (periodic
//! `getMultipleAccounts` vs a gRPC stream). No Redis, no keypair, no
//! executor, nothing is ever submitted.
//!
//! IMPORTANT: polling latency (seconds) means anything found here is NOT
//! executable — it would be stale and contested. This is a feasibility probe,
//! not a trading loop: it tells you if the opportunity space is non-empty and
//! roughly how big/frequent, so you can decide whether Geyser is worth it.
//! Quotes stay conservative (single-tick CLMM, fees included), so real edge is
//! >= what you see, not less.
//!
//! Usage:
//!   RPC_ENDPOINT=... cargo run -p arb-monitor --bin preview
//! Env: RPC_ENDPOINT, POLL_INTERVAL_MS (default 3000), plus the usual
//! BASE_MINTS / MIN_PROFIT_BPS / trade-bound / cost vars from .env.

use anyhow::{Context, Result};
use arb_monitor::bootstrap::bootstrap_registry;
use arb_monitor::config::MonitorConfig;
use arb_monitor::discovery::DiscoveryEngine;
use arb_monitor::registry::PoolRegistry;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use std::time::Duration;
use tracing::{info, warn};

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Fetch all watched accounts in one batch; returns (slot, [(pubkey, data)]).
async fn poll_accounts(
    rpc: &RpcClient,
    watched: &[Pubkey],
) -> Result<(u64, Vec<(Pubkey, Vec<u8>)>)> {
    let mut slot = 0u64;
    let mut out = Vec::with_capacity(watched.len());
    for chunk in watched.chunks(100) {
        let resp = rpc
            .get_multiple_accounts_with_commitment(chunk, CommitmentConfig::processed())
            .await
            .context("getMultipleAccounts")?;
        slot = slot.max(resp.context.slot);
        for (pk, acc) in chunk.iter().zip(resp.value) {
            if let Some(acc) = acc {
                out.push((*pk, acc.data));
            }
        }
    }
    Ok((slot, out))
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // Geyser not required for the preview.
    let mut cfg = MonitorConfig::from_env(false)?;
    // PREVIEW_* knobs override the shared config for feasibility runs.
    let interval = Duration::from_millis(env_u64(
        "PREVIEW_POLL_INTERVAL_MS",
        env_u64("POLL_INTERVAL_MS", 3000),
    ));
    if let Ok(v) = std::env::var("PREVIEW_MIN_PROFIT_BPS") {
        if let Ok(bps) = v.parse() {
            cfg.min_profit_bps = bps;
        }
    }
    let max_pools = env_u64("PREVIEW_MAX_POOLS", 0) as usize;
    if max_pools > 0 && cfg.pools.len() > max_pools {
        cfg.pools.truncate(max_pools);
    }

    info!(
        pools = cfg.pools.len(),
        rpc = %cfg.rpc_endpoint,
        poll_ms = interval.as_millis() as u64,
        min_profit_bps = cfg.min_profit_bps,
        "arb-preview starting (RPC polling — NO Geyser, NO submission)"
    );

    let mut registry = PoolRegistry::new();
    bootstrap_registry(&cfg, &mut registry).await?;
    info!(
        pools = registry.pools.len(),
        tokens = registry.tokens.len(),
        "registry hydrated from chain"
    );

    let mut engine = DiscoveryEngine::new(cfg.opportunity_cooldown_ms);
    engine.build_cycle_index(&registry, &cfg);
    let routes = engine.route_count();
    info!(routes, "cycle index built");
    if routes == 0 {
        warn!("no cycles across configured pools — add more pools that share tokens, or check pools.json");
        return Ok(());
    }
    for base in &cfg.base_mints {
        let sym = registry
            .tokens
            .get(base)
            .and_then(|t| t.symbol)
            .unwrap_or("?");
        info!(base = %sym, "cycle base token");
    }

    let rpc =
        RpcClient::new_with_commitment(cfg.rpc_endpoint.clone(), CommitmentConfig::processed());
    let watched = registry.all_watched_accounts();
    info!(
        accounts = watched.len(),
        "polling these accounts each cycle"
    );

    let mut ticker = tokio::time::interval(interval);
    let mut poll_n: u64 = 0;
    let mut total_found: u64 = 0;

    loop {
        ticker.tick().await;
        poll_n += 1;

        let (slot, accounts) = match poll_accounts(&rpc, &watched).await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "poll failed (RPC rate limit? try a paid RPC or raise POLL_INTERVAL_MS)");
                continue;
            }
        };

        // Feed every polled account through the same update path, then
        // re-evaluate every route (polling is not the hot path).
        for (pk, data) in &accounts {
            registry.apply_account_update(*pk, data, slot);
        }
        for addr in registry.pools.keys().copied().collect::<Vec<_>>() {
            engine.mark_dirty(addr);
        }

        let opps = engine.run_search(&registry, &cfg);
        total_found += opps.len() as u64;

        if opps.is_empty() {
            info!(poll = poll_n, slot, "no profitable cycle this poll");
        } else {
            for opp in &opps {
                let dec = registry
                    .tokens
                    .get(&Pubkey::try_from(opp.base_mint.as_str()).unwrap_or_default())
                    .map(|t| t.decimals)
                    .unwrap_or(9);
                let scale = 10f64.powi(dec as i32);
                info!(
                    poll = poll_n,
                    slot,
                    base = %opp.base_symbol.clone().unwrap_or_default(),
                    hops = opp.hops.len(),
                    "💰 OPPORTUNITY"
                );
                info!(
                    net_bps = opp.net_profit_bps as u64,
                    amount_in = format!("{:.4}", opp.amount_in as f64 / scale),
                    net_profit = format!("{:.6}", opp.net_profit as f64 / scale),
                    gross_profit = format!("{:.6}", opp.gross_profit as f64 / scale),
                    "  economics (base-token units)"
                );
                for (i, h) in opp.hops.iter().enumerate() {
                    let dex = match h.dex {
                        arb_common::ix::DexKind::RaydiumV4 => "raydium",
                        arb_common::ix::DexKind::OrcaWhirlpool => "orca",
                    };
                    info!(hop = i, %dex, pool = %h.pool, "  route leg");
                }
            }
        }

        if poll_n.is_multiple_of(20) {
            info!(
                polls = poll_n,
                opportunities_found = total_found,
                routes_evaluated = engine.stats.routes_evaluated,
                "── preview summary ──"
            );
        }
    }
}
