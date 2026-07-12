//! Monitor configuration — same env vars as the TypeScript monitor so a
//! single `.env` drives either implementation.

use anyhow::{Context, Result};
use arb_common::cost::{CostModel, ExecutionPayment};
use arb_common::mode::Mode;
use serde::Deserialize;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::str::FromStr;

pub use crate::types::{USDC_MINT_STR, WSOL_MINT_STR};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum DexKindCfg {
    #[serde(rename = "raydium-v4")]
    RaydiumV4,
    #[serde(rename = "orca-whirlpool")]
    OrcaWhirlpool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WatchedPool {
    pub address: String,
    pub dex: DexKindCfg,
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PoolsFile {
    pools: Vec<WatchedPool>,
}

#[derive(Debug, Clone, Default)]
pub struct MonitorConfig {
    /// Execution mode. DEFAULT `observe`. The monitor never submits, but the
    /// mode is threaded through for consistency and reporting.
    pub mode: Mode,

    pub geyser_endpoint: String,
    pub geyser_x_token: Option<String>,
    pub rpc_endpoint: String,

    pub redis_url: String,
    pub opportunity_channel: String,
    pub opportunity_list: String,
    pub opportunity_list_max: isize,

    pub base_mints: Vec<Pubkey>,
    pub max_hops: usize,
    pub min_profit_bps: u64,
    pub slippage_bps: u64,
    pub max_clmm_impact_bps: u64,

    /// base mint -> (min, max) input bounds (raw units).
    pub trade_bounds: HashMap<Pubkey, (u64, u64)>,

    pub base_signature_fee_lamports: u64,
    pub priority_fee_lamports: u64,
    pub jito_tip_lamports: u64,

    // Shared cost-model inputs — MUST mirror the executor so both sides agree
    // on profitability (env vars are identical: MIN_TIP_LAMPORTS, etc.).
    pub min_tip_lamports: u64,
    pub max_tip_lamports: u64,
    pub cu_limit: u32,
    pub cu_price_microlamports: u64,
    pub profit_margin_lamports: u64,

    pub opportunity_cooldown_ms: u64,

    pub pools: Vec<WatchedPool>,
}

fn env_str(name: &str, default: &str) -> String {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => v,
        _ => default.to_string(),
    }
}

fn env_parse<T: FromStr>(name: &str, default: T) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => v
            .parse::<T>()
            .map_err(|e| anyhow::anyhow!("env {name}={v} invalid: {e}")),
        _ => Ok(default),
    }
}

impl MonitorConfig {
    pub fn from_env(require_geyser: bool) -> Result<Self> {
        let wsol = Pubkey::from_str(WSOL_MINT_STR).unwrap();
        let usdc = Pubkey::from_str(USDC_MINT_STR).unwrap();

        let geyser_endpoint = if require_geyser {
            std::env::var("GEYSER_ENDPOINT").context("GEYSER_ENDPOINT required")?
        } else {
            env_str("GEYSER_ENDPOINT", "")
        };

        let base_mints = env_str("BASE_MINTS", WSOL_MINT_STR)
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(Pubkey::from_str)
            .collect::<Result<Vec<_>, _>>()
            .context("BASE_MINTS contains an invalid mint")?;

        let mut trade_bounds = HashMap::new();
        trade_bounds.insert(
            wsol,
            (
                env_parse("TRADE_MIN_WSOL", 50_000_000u64)?,
                env_parse("TRADE_MAX_WSOL", 10_000_000_000u64)?,
            ),
        );
        trade_bounds.insert(
            usdc,
            (
                env_parse("TRADE_MIN_USDC", 5_000_000u64)?,
                env_parse("TRADE_MAX_USDC", 1_500_000_000u64)?,
            ),
        );

        // POOLS_FILE takes precedence over POOLS_PATH (both accepted).
        let pools_path = match std::env::var("POOLS_FILE") {
            Ok(v) if !v.is_empty() => v,
            _ => env_str("POOLS_PATH", "pools.json"),
        };
        let raw =
            std::fs::read_to_string(&pools_path).with_context(|| format!("read {pools_path}"))?;
        let pools_file: PoolsFile = serde_json::from_str(&raw).context("parse pools.json")?;
        let pools: Vec<WatchedPool> = pools_file
            .pools
            .into_iter()
            .filter(|p| Pubkey::from_str(&p.address).is_ok())
            .collect();

        let max_hops = env_parse::<usize>("MAX_HOPS", 4)?.clamp(2, 4);

        let mode = env_str("MODE", "observe")
            .parse::<Mode>()
            .map_err(|e| anyhow::anyhow!("MODE invalid: {e}"))?;

        Ok(Self {
            mode,
            geyser_endpoint,
            geyser_x_token: std::env::var("GEYSER_X_TOKEN")
                .ok()
                .filter(|s| !s.is_empty()),
            rpc_endpoint: env_str("RPC_ENDPOINT", "https://api.mainnet-beta.solana.com"),
            redis_url: env_str("REDIS_URL", "redis://127.0.0.1:6379"),
            opportunity_channel: env_str("REDIS_OPPORTUNITY_CHANNEL", "arbitrage_opportunities"),
            opportunity_list: env_str("REDIS_OPPORTUNITY_LIST", "arbitrage_opportunities"),
            opportunity_list_max: env_parse("REDIS_OPPORTUNITY_LIST_MAX", 1000isize)?,
            base_mints,
            max_hops,
            min_profit_bps: env_parse("MIN_PROFIT_BPS", 10u64)?,
            slippage_bps: env_parse("SLIPPAGE_BPS", 20u64)?,
            max_clmm_impact_bps: env_parse("MAX_CLMM_IMPACT_BPS", 100u64)?,
            trade_bounds,
            base_signature_fee_lamports: env_parse("BASE_SIGNATURE_FEE_LAMPORTS", 5_000u64)?,
            priority_fee_lamports: env_parse("PRIORITY_FEE_LAMPORTS", 100_000u64)?,
            jito_tip_lamports: env_parse("JITO_TIP_LAMPORTS", 1_000_000u64)?,
            min_tip_lamports: env_parse("MIN_TIP_LAMPORTS", 10_000u64)?,
            max_tip_lamports: env_parse("MAX_TIP_LAMPORTS", 100_000_000u64)?,
            cu_limit: env_parse("CU_LIMIT", 700_000u32)?,
            cu_price_microlamports: env_parse("CU_PRICE_MICROLAMPORTS", 10_000u64)?,
            profit_margin_lamports: env_parse("PROFIT_MARGIN_LAMPORTS", 10_000u64)?,
            opportunity_cooldown_ms: env_parse("OPPORTUNITY_COOLDOWN_MS", 500u64)?,
            pools,
        })
    }

    /// Build the shared [`CostModel`] — identical construction to the
    /// executor's `Config::cost_model`, so a candidate the monitor accepts is
    /// evaluated by the executor with the exact same economics.
    pub fn cost_model(&self) -> CostModel {
        CostModel {
            signature_fee_lamports: self.base_signature_fee_lamports,
            compute_unit_limit: self.cu_limit,
            compute_unit_price_micro: self.cu_price_microlamports,
            margin_lamports: self.profit_margin_lamports,
            required_net_lamports: 0,
            payment: ExecutionPayment::JitoTip {
                min_lamports: self.min_tip_lamports,
                max_lamports: self.max_tip_lamports,
            },
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point of S2: the monitor and executor, from identical inputs,
    /// derive the identical net. This mirrors the executor's `cost_model()`
    /// construction and the executor's on-chain floor formula. If someone edits
    /// one side's mapping and not the other, this fails.
    #[test]
    fn monitor_cost_model_matches_executor_formula() {
        // Executor defaults (see executor/src/config.rs).
        let cfg = MonitorConfig {
            base_signature_fee_lamports: 5_000,
            cu_limit: 700_000,
            cu_price_microlamports: 10_000,
            profit_margin_lamports: 10_000,
            min_tip_lamports: 10_000,
            max_tip_lamports: 100_000_000,
            ..Default::default()
        };
        let model = cfg.cost_model();
        for gross in [50_000u64, 1_000_000, 10_000_000, 1_000_000_000] {
            // Executor: fees = 5_000 + cu_limit*cu_price/1e6; tip = jito_tip;
            //           min_profit = tip + fees + margin; net = gross - min_profit.
            let fees = 5_000 + (700_000u64 * 10_000) / 1_000_000;
            let tip = arb_common::cost::jito_tip(gross, 10_000, 100_000_000);
            let executor_min_profit = tip + fees + 10_000;
            let executor_net = gross as i128 - executor_min_profit as i128;
            assert_eq!(
                model.net(gross),
                executor_net,
                "monitor net must equal executor net (gross={gross})"
            );
            assert_eq!(model.fixed_costs(), fees);
            assert_eq!(model.payment(gross), tip);
        }
    }
}
