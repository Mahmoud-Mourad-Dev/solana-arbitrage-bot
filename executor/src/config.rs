use anyhow::{Context, Result};
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

#[derive(Debug, Clone)]
pub struct Config {
    pub rpc_url: String,
    pub redis_url: String,
    pub redis_channel: String,
    pub keypair_path: String,
    /// Deployed address of the on-chain arbitrage program.
    pub arb_program_id: Pubkey,
    pub jito_url: String,

    pub min_tip_lamports: u64,
    pub max_tip_lamports: u64,

    pub cu_limit: u32,
    pub cu_price_microlamports: u64,
    /// Extra lamports of profit demanded on-chain beyond tip + fees.
    pub profit_margin_lamports: u64,

    /// Opportunities older than this are stale — discard.
    pub max_opportunity_age_ms: u64,
    pub max_inflight: usize,
    /// Per-cycle-id cooldown between submissions.
    pub resubmit_cooldown_ms: u64,
    /// Whirlpool tick data is refetched after this many seconds.
    pub whirlpool_ttl_secs: u64,

    /// Address lookup tables to compress transactions (comma separated).
    pub lookup_tables: Vec<Pubkey>,
    /// Build + simulate but never submit.
    pub dry_run: bool,
    /// Master submission switch. DEFAULT FALSE: without an explicit
    /// ENABLE_SUBMIT=true in the environment, nothing ever leaves the box.
    pub enable_submit: bool,
    /// Jito path switch, also default false. Both flags must be true (and
    /// DRY_RUN false) for a bundle to be sent.
    pub enable_jito: bool,
    /// Opportunities whose projected net (gross - tip - fees - margin)
    /// falls below this are rejected before any RPC work.
    pub min_net_profit_lamports: u64,
}

fn env_str(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn env_parse<T: FromStr>(name: &str, default: T) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    match std::env::var(name) {
        Err(_) => Ok(default),
        Ok(v) if v.is_empty() => Ok(default),
        Ok(v) => v
            .parse::<T>()
            .map_err(|e| anyhow::anyhow!("env {name}={v} invalid: {e}")),
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let arb_program_id = std::env::var("ARB_PROGRAM_ID")
            .context("ARB_PROGRAM_ID is required (deploy the program first)")?;
        let lookup_tables = env_str("LOOKUP_TABLES", "")
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(Pubkey::from_str)
            .collect::<Result<Vec<_>, _>>()
            .context("LOOKUP_TABLES contains an invalid pubkey")?;

        Ok(Self {
            rpc_url: env_str("RPC_ENDPOINT", "https://api.mainnet-beta.solana.com"),
            redis_url: env_str("REDIS_URL", "redis://127.0.0.1:6379"),
            redis_channel: env_str("REDIS_OPPORTUNITY_CHANNEL", "arbitrage_opportunities"),
            keypair_path: std::env::var("KEYPAIR_PATH").context("KEYPAIR_PATH is required")?,
            arb_program_id: Pubkey::from_str(&arb_program_id).context("bad ARB_PROGRAM_ID")?,
            jito_url: env_str(
                "JITO_BLOCK_ENGINE_URL",
                "https://mainnet.block-engine.jito.wtf/api/v1/bundles",
            ),
            min_tip_lamports: env_parse("MIN_TIP_LAMPORTS", 10_000u64)?,
            max_tip_lamports: env_parse("MAX_TIP_LAMPORTS", 100_000_000u64)?,
            cu_limit: env_parse("CU_LIMIT", 700_000u32)?,
            cu_price_microlamports: env_parse("CU_PRICE_MICROLAMPORTS", 10_000u64)?,
            profit_margin_lamports: env_parse("PROFIT_MARGIN_LAMPORTS", 10_000u64)?,
            max_opportunity_age_ms: env_parse("MAX_OPPORTUNITY_AGE_MS", 750u64)?,
            max_inflight: env_parse("MAX_INFLIGHT", 4usize)?,
            resubmit_cooldown_ms: env_parse("RESUBMIT_COOLDOWN_MS", 400u64)?,
            whirlpool_ttl_secs: env_parse("WHIRLPOOL_TTL_SECS", 10u64)?,
            lookup_tables,
            dry_run: env_parse("DRY_RUN", true)?,
            enable_submit: env_parse("ENABLE_SUBMIT", false)?,
            enable_jito: env_parse("ENABLE_JITO", false)?,
            min_net_profit_lamports: env_parse("MIN_NET_PROFIT_LAMPORTS", 100_000u64)?,
        })
    }

    /// Total non-tip lamports a submission burns if it lands.
    pub fn fee_lamports(&self) -> u64 {
        5_000 + (self.cu_limit as u64 * self.cu_price_microlamports) / 1_000_000
    }
}
