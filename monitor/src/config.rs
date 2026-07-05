//! Monitor configuration — same env vars as the TypeScript monitor so a
//! single `.env` drives either implementation.

use anyhow::{Context, Result};
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

#[derive(Debug, Clone)]
pub struct MonitorConfig {
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

        let pools_path = env_str("POOLS_PATH", "pools.json");
        let raw =
            std::fs::read_to_string(&pools_path).with_context(|| format!("read {pools_path}"))?;
        let pools_file: PoolsFile = serde_json::from_str(&raw).context("parse pools.json")?;
        let pools: Vec<WatchedPool> = pools_file
            .pools
            .into_iter()
            .filter(|p| Pubkey::from_str(&p.address).is_ok())
            .collect();

        let max_hops = env_parse::<usize>("MAX_HOPS", 4)?.clamp(2, 4);

        Ok(Self {
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
            opportunity_cooldown_ms: env_parse("OPPORTUNITY_COOLDOWN_MS", 500u64)?,
            pools,
        })
    }
}
