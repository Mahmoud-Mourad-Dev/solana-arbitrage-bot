//! Quote dispatch: PoolState + input mint + amount -> output. Bridges the
//! registry's typed pools to the pure math in [`crate::math`], applying the
//! Raydium effective-reserve rule and the Whirlpool direction. Mirrors the
//! TS `quotePool`.

use crate::math::{
    cpmm_amount_out, raydium_effective_reserve, whirlpool_amount_out, WhirlpoolQuoteState,
};
use crate::types::PoolState;
use solana_sdk::pubkey::Pubkey;

pub fn quote_pool(
    pool: &PoolState,
    input_mint: &Pubkey,
    amount_in: u64,
    max_clmm_impact_bps: u64,
) -> u64 {
    if !pool.common().ready {
        return 0;
    }
    match pool {
        PoolState::Raydium(p) => {
            let base = match raydium_effective_reserve(
                p.vault_a_balance,
                p.open_orders_base_total,
                p.base_need_take_pnl,
            ) {
                Some(v) => v,
                None => return 0,
            };
            let quote = match raydium_effective_reserve(
                p.vault_b_balance,
                p.open_orders_quote_total,
                p.quote_need_take_pnl,
            ) {
                Some(v) => v,
                None => return 0,
            };
            // Trust on-chain fee fields; fall back to canonical 25/10000.
            let (fee_num, fee_den) = if p.swap_fee_denominator > 0 {
                (p.swap_fee_numerator, p.swap_fee_denominator)
            } else {
                (25, 10_000)
            };
            if input_mint == &p.common.mint_a {
                cpmm_amount_out(amount_in, base, quote, fee_num, fee_den)
            } else if input_mint == &p.common.mint_b {
                cpmm_amount_out(amount_in, quote, base, fee_num, fee_den)
            } else {
                0
            }
        }
        PoolState::Whirlpool(p) => {
            let input_is_a = if input_mint == &p.common.mint_a {
                true
            } else if input_mint == &p.common.mint_b {
                false
            } else {
                return 0;
            };
            whirlpool_amount_out(
                &WhirlpoolQuoteState {
                    sqrt_price_x64: p.sqrt_price_x64,
                    liquidity: p.liquidity,
                    fee_rate_ppm: p.fee_rate_ppm,
                },
                input_is_a,
                amount_in,
                max_clmm_impact_bps,
            )
        }
    }
}
