//! Pool/token domain types — port of `src/types.ts` (PoolState union).

use solana_sdk::pubkey::Pubkey;

pub const WSOL_MINT_STR: &str = "So11111111111111111111111111111111111111112";
pub const USDC_MINT_STR: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

pub fn known_symbol(mint: &Pubkey) -> Option<&'static str> {
    match mint.to_string().as_str() {
        WSOL_MINT_STR => Some("SOL"),
        USDC_MINT_STR => Some("USDC"),
        "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R" => Some("RAY"),
        "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB" => Some("USDT"),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    A,
    B,
}

#[derive(Debug, Clone)]
pub struct TokenNode {
    pub mint: Pubkey,
    pub decimals: u8,
    pub symbol: Option<&'static str>,
}

/// Fields shared by both pool kinds (TS `PoolStateBase`).
#[derive(Debug, Clone)]
pub struct PoolCommon {
    pub address: Pubkey,
    pub label: Option<String>,
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub vault_a: Pubkey,
    pub vault_b: Pubkey,
    pub decimals_a: u8,
    pub decimals_b: u8,
    pub last_slot: u64,
    pub last_updated_ms: u64,
    pub ready: bool,
}

#[derive(Debug, Clone)]
pub struct RaydiumPool {
    pub common: PoolCommon,
    pub vault_a_balance: u64,
    pub vault_b_balance: u64,
    pub open_orders: Pubkey,
    pub open_orders_base_total: u64,
    pub open_orders_quote_total: u64,
    pub base_need_take_pnl: u64,
    pub quote_need_take_pnl: u64,
    pub swap_fee_numerator: u64,
    pub swap_fee_denominator: u64,
    pub status: u64,
    pub pool_open_time: u64,
}

#[derive(Debug, Clone)]
pub struct WhirlpoolPool {
    pub common: PoolCommon,
    pub sqrt_price_x64: u128,
    pub liquidity: u128,
    pub tick_current_index: i32,
    pub tick_spacing: u16,
    pub fee_rate_ppm: u64,
}

#[derive(Debug, Clone)]
pub enum PoolState {
    Raydium(RaydiumPool),
    Whirlpool(WhirlpoolPool),
}

impl PoolState {
    pub fn common(&self) -> &PoolCommon {
        match self {
            PoolState::Raydium(p) => &p.common,
            PoolState::Whirlpool(p) => &p.common,
        }
    }

    pub fn common_mut(&mut self) -> &mut PoolCommon {
        match self {
            PoolState::Raydium(p) => &mut p.common,
            PoolState::Whirlpool(p) => &mut p.common,
        }
    }

    pub fn dex(&self) -> arb_common::ix::DexKind {
        match self {
            PoolState::Raydium(_) => arb_common::ix::DexKind::RaydiumV4,
            PoolState::Whirlpool(_) => arb_common::ix::DexKind::OrcaWhirlpool,
        }
    }

    pub fn other_mint(&self, mint: &Pubkey) -> Pubkey {
        let c = self.common();
        if &c.mint_a == mint {
            c.mint_b
        } else {
            c.mint_a
        }
    }

    /// Raydium AmmStatus gate — port of TS `raydiumSwapEnabled`.
    pub fn swap_enabled(&self, now_sec: u64) -> bool {
        match self {
            PoolState::Whirlpool(_) => true,
            PoolState::Raydium(p) => match p.status {
                1 | 6 => true,                    // Initialized, SwapOnly
                7 => now_sec >= p.pool_open_time, // WaitingTrade
                _ => false,
            },
        }
    }
}
