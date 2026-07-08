//! Quote dispatch: PoolState + input mint + amount -> output. Bridges the
//! registry's typed pools to the swap math, applying the Raydium
//! effective-reserve rule and EXACT tick-array Whirlpool quoting.
//!
//! Whirlpool quotes are exact (tick-array aware) and conservative: if the
//! required tick arrays are missing, or the swap would step beyond loaded
//! coverage, the quote is REJECTED (returns 0 with a reason) rather than
//! estimated — this is what removes the single-tick phantom-profit class.

use crate::math::{cpmm_amount_out, raydium_effective_reserve};
use crate::tick_math::swap_exact_in;
use crate::types::PoolState;
use solana_sdk::pubkey::Pubkey;

/// Why a quote produced no output — surfaced for observability. `Ok` carries
/// the output amount (which may be 0 for a genuinely tiny trade).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuoteOutcome {
    Ok(u64),
    NotReady,
    WrongMint,
    RaydiumDrained,
    /// Whirlpool: no tick arrays loaded for this pool.
    WhirlpoolMissingTicks,
    /// Whirlpool: swap would step beyond loaded coverage / exhaust liquidity.
    WhirlpoolBeyondCoverage,
}

impl QuoteOutcome {
    pub fn amount(self) -> u64 {
        match self {
            QuoteOutcome::Ok(v) => v,
            _ => 0,
        }
    }
}

/// Detailed quote (for callers that want the rejection reason).
pub fn quote_pool_detailed(pool: &PoolState, input_mint: &Pubkey, amount_in: u64) -> QuoteOutcome {
    if !pool.common().ready {
        return QuoteOutcome::NotReady;
    }
    match pool {
        PoolState::Raydium(p) => {
            let (Some(base), Some(quote)) = (
                raydium_effective_reserve(
                    p.vault_a_balance,
                    p.open_orders_base_total,
                    p.base_need_take_pnl,
                ),
                raydium_effective_reserve(
                    p.vault_b_balance,
                    p.open_orders_quote_total,
                    p.quote_need_take_pnl,
                ),
            ) else {
                return QuoteOutcome::RaydiumDrained;
            };
            let (fee_num, fee_den) = if p.swap_fee_denominator > 0 {
                (p.swap_fee_numerator, p.swap_fee_denominator)
            } else {
                (25, 10_000)
            };
            let out = if input_mint == &p.common.mint_a {
                cpmm_amount_out(amount_in, base, quote, fee_num, fee_den)
            } else if input_mint == &p.common.mint_b {
                cpmm_amount_out(amount_in, quote, base, fee_num, fee_den)
            } else {
                return QuoteOutcome::WrongMint;
            };
            QuoteOutcome::Ok(out)
        }
        PoolState::Whirlpool(p) => {
            let a_to_b = if input_mint == &p.common.mint_a {
                true
            } else if input_mint == &p.common.mint_b {
                false
            } else {
                return QuoteOutcome::WrongMint;
            };
            let Some((crossings, coverage_limit)) = p.build_crossings(a_to_b) else {
                return QuoteOutcome::WhirlpoolMissingTicks;
            };
            match swap_exact_in(
                p.sqrt_price_x64,
                p.liquidity,
                p.fee_rate_ppm as u128,
                a_to_b,
                amount_in,
                &crossings,
                coverage_limit,
            ) {
                Some(out) => QuoteOutcome::Ok(out),
                None => QuoteOutcome::WhirlpoolBeyondCoverage,
            }
        }
    }
}

/// Output amount only (0 = no fill / rejected). Hot-path entry for discovery.
/// `_max_clmm_impact_bps` is retained for signature compatibility but unused —
/// exact quoting supersedes the impact-guard approximation.
pub fn quote_pool(
    pool: &PoolState,
    input_mint: &Pubkey,
    amount_in: u64,
    _max_clmm_impact_bps: u64,
) -> u64 {
    quote_pool_detailed(pool, input_mint, amount_in).amount()
}
