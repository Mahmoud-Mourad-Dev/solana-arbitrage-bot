use anyhow::{Context, Result};
use arb_common::cost::{CostModel, ExecutionPayment};
use arb_common::mode::{resolve_live, Mode};
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

#[derive(Debug, Clone)]
pub struct Config {
    /// Execution mode. DEFAULT `observe`. Only `live` (armed) may submit.
    pub mode: Mode,
    /// Path to the acceptance marker file that arms `MODE=live`.
    pub live_marker_path: String,
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
    /// FOURTH ARMING GATE (Raydium pivot, Phase 0). Submission additionally
    /// requires `STRATEGY=raydium-dual`, so an old `.env` that already carries
    /// DRY_RUN=false + ENABLE_SUBMIT=true + ENABLE_JITO=true can NEVER arm the
    /// new strategy path by accident. Default is the empty string (disarmed).
    pub strategy: String,
}

/// The only `STRATEGY` value that may arm the Raydium dual-venue path.
pub const STRATEGY_RAYDIUM_DUAL: &str = "raydium-dual";

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

        // Mode gate (S1): default observe. `MODE=live` is REFUSED unless it is
        // armed by BOTH the explicit submit flag AND the acceptance marker file.
        let requested_mode = env_str("MODE", "observe")
            .parse::<Mode>()
            .map_err(|e| anyhow::anyhow!("MODE invalid: {e}"))?;
        let enable_submit = env_parse("ENABLE_SUBMIT", false)?;
        let live_marker_path = env_str("LIVE_MARKER_PATH", ".live-armed");
        let mode = resolve_live(requested_mode, enable_submit, &live_marker_path)
            .map_err(|e| anyhow::anyhow!(e))?;

        Ok(Self {
            mode,
            live_marker_path,
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
            enable_submit,
            enable_jito: env_parse("ENABLE_JITO", false)?,
            min_net_profit_lamports: env_parse("MIN_NET_PROFIT_LAMPORTS", 100_000u64)?,
            strategy: env_str("STRATEGY", ""),
        })
    }

    /// Is the Raydium dual-venue strategy explicitly selected? This is an
    /// ADDITIONAL requirement for submission — it never relaxes the existing
    /// MODE/DRY_RUN/ENABLE_SUBMIT/ENABLE_JITO gates.
    pub fn strategy_armed(&self) -> bool {
        self.strategy == STRATEGY_RAYDIUM_DUAL
    }

    /// Total non-tip lamports a submission burns if it lands.
    pub fn fee_lamports(&self) -> u64 {
        5_000 + (self.cu_limit as u64 * self.cu_price_microlamports) / 1_000_000
    }

    /// Build the shared [`CostModel`] from this config. The monitor builds the
    /// same model from the same values, so both sides agree on profitability.
    pub fn cost_model(&self) -> CostModel {
        CostModel {
            signature_fee_lamports: 5_000,
            compute_unit_limit: self.cu_limit,
            compute_unit_price_micro: self.cu_price_microlamports,
            margin_lamports: self.profit_margin_lamports,
            required_net_lamports: self.min_net_profit_lamports,
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

    /// A config with every OTHER gate already open — only `strategy` varies.
    /// This is exactly the dangerous case: an old `.env` that was armed for the
    /// previous strategy must NOT arm the Raydium dual-venue path.
    fn armed_except_strategy(strategy: &str) -> Config {
        Config {
            mode: Mode::Observe,
            live_marker_path: ".live-armed".into(),
            rpc_url: String::new(),
            redis_url: String::new(),
            redis_channel: String::new(),
            keypair_path: String::new(),
            arb_program_id: Pubkey::new_unique(),
            jito_url: String::new(),
            min_tip_lamports: 0,
            max_tip_lamports: 0,
            cu_limit: 700_000,
            cu_price_microlamports: 0,
            profit_margin_lamports: 0,
            max_opportunity_age_ms: 750,
            max_inflight: 1,
            resubmit_cooldown_ms: 0,
            whirlpool_ttl_secs: 10,
            lookup_tables: vec![],
            dry_run: false,
            enable_submit: true,
            enable_jito: true,
            min_net_profit_lamports: 0,
            strategy: strategy.into(),
        }
    }

    #[test]
    fn strategy_gate_requires_exact_value() {
        assert!(armed_except_strategy(STRATEGY_RAYDIUM_DUAL).strategy_armed());
        // Every other value — including the legacy empty default — is disarmed.
        for s in [
            "",
            "raydium",
            "raydium_dual",
            "RAYDIUM-DUAL",
            "meteora-pump",
        ] {
            assert!(
                !armed_except_strategy(s).strategy_armed(),
                "STRATEGY={s:?} must NOT arm"
            );
        }
    }

    #[test]
    fn strategy_gate_is_additive_not_a_relaxation() {
        // With STRATEGY set but the OLD gates closed, submission is still off.
        let mut cfg = armed_except_strategy(STRATEGY_RAYDIUM_DUAL);
        cfg.dry_run = true;
        let armed = cfg.mode.allows_live_submission()
            && !cfg.dry_run
            && cfg.enable_submit
            && cfg.enable_jito
            && cfg.strategy_armed();
        assert!(!armed, "STRATEGY must never override DRY_RUN");

        let mut cfg = armed_except_strategy(STRATEGY_RAYDIUM_DUAL);
        cfg.enable_submit = false;
        let armed = cfg.mode.allows_live_submission()
            && !cfg.dry_run
            && cfg.enable_submit
            && cfg.enable_jito
            && cfg.strategy_armed();
        assert!(!armed, "STRATEGY must never override ENABLE_SUBMIT");
    }

    #[test]
    fn observe_mode_is_never_live_even_fully_armed() {
        // Mode::Observe does not allow live submission regardless of flags.
        let cfg = armed_except_strategy(STRATEGY_RAYDIUM_DUAL);
        assert!(!cfg.mode.allows_live_submission());
    }
}
