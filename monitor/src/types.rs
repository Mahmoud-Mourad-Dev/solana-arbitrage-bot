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
    /// Loaded tick arrays keyed by `start_tick_index`. Populated at bootstrap
    /// and refreshed by account updates; empty means "can't quote exactly".
    pub tick_arrays: std::collections::HashMap<i32, Vec<crate::parsers::TickInfo>>,
}

pub const WHIRLPOOL_PROGRAM: Pubkey = Pubkey::new_from_array(arb_common::ix::WHIRLPOOL_PROGRAM_ID);

/// Ticks per array times spacing — the tick span one TickArray covers.
pub fn tick_array_span(tick_spacing: u16) -> i32 {
    tick_spacing as i32 * crate::parsers::TICKS_PER_ARRAY as i32
}

/// start_tick_index of the array containing `tick` (floor semantics).
pub fn tick_array_start(tick: i32, tick_spacing: u16) -> i32 {
    let span = tick_array_span(tick_spacing);
    tick.div_euclid(span) * span
}

/// The 5 tick-array starts around the current tick (±2 arrays) — enough for
/// both swap directions.
pub fn tick_array_starts_around(tick: i32, tick_spacing: u16) -> [i32; 5] {
    let span = tick_array_span(tick_spacing);
    let s = tick_array_start(tick, tick_spacing);
    [s - 2 * span, s - span, s, s + span, s + 2 * span]
}

/// PDA of a tick array: seeds ["tick_array", whirlpool, start_index_ascii].
pub fn tick_array_pda(whirlpool: &Pubkey, start_tick_index: i32) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"tick_array",
            whirlpool.as_ref(),
            start_tick_index.to_string().as_bytes(),
        ],
        &WHIRLPOOL_PROGRAM,
    )
    .0
}

impl WhirlpoolPool {
    /// Build the ordered crossings for a swap direction plus the coverage-edge
    /// sqrt price, from loaded tick arrays. `None` if no arrays are loaded
    /// (→ the quote must be rejected, never estimated).
    pub fn build_crossings(&self, a_to_b: bool) -> Option<(Vec<crate::tick_math::Crossing>, u128)> {
        if self.tick_arrays.is_empty() {
            return None;
        }
        let spacing = self.tick_spacing;
        let mut lowest_start = i32::MAX;
        let mut highest_start = i32::MIN;
        let mut items: Vec<(i32, i128)> = Vec::new();
        for (start, ticks) in &self.tick_arrays {
            lowest_start = lowest_start.min(*start);
            highest_start = highest_start.max(*start);
            for (i, t) in ticks.iter().enumerate() {
                if !t.initialized {
                    continue;
                }
                let tick_index = start + i as i32 * spacing as i32;
                items.push((tick_index, t.liquidity_net));
            }
        }
        let cur = self.tick_current_index;
        let span = tick_array_span(spacing);
        let (mut selected, coverage_limit): (Vec<(i32, i128)>, u128) = if a_to_b {
            let sel: Vec<_> = items.into_iter().filter(|(ti, _)| *ti <= cur).collect();
            (sel, crate::tick_math::sqrt_price_from_tick(lowest_start))
        } else {
            let sel: Vec<_> = items.into_iter().filter(|(ti, _)| *ti > cur).collect();
            (
                sel,
                crate::tick_math::sqrt_price_from_tick(highest_start + span),
            )
        };
        // Order in the swap direction.
        if a_to_b {
            selected.sort_by_key(|&(ti, _)| std::cmp::Reverse(ti)); // descending tick
        } else {
            selected.sort_by_key(|&(ti, _)| ti); // ascending tick
        }
        let crossings = selected
            .into_iter()
            .map(|(ti, net)| crate::tick_math::Crossing {
                sqrt_price: crate::tick_math::sqrt_price_from_tick(ti),
                liquidity_net: net,
            })
            .collect();
        Some((crossings, coverage_limit))
    }
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
