//! Meteora DLMM (`LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo`) support:
//! LbPair + BinArray parsers and an exact-in quote that is a faithful port of
//! Meteora's own off-chain quoting code (`dlmm-sdk/commons/src/quote.rs` +
//! `extensions/{bin,lb_pair}.rs`), including:
//!
//! - **collect-fee-mode** (`InputOnly` / `OnlyY`): fee side depends on the
//!   pool AND the swap direction — never assumed.
//! - **per-bin LIMIT ORDER fills**: MM liquidity first, then processed
//!   orders, then open orders, all at the bin price.
//! - **bitmap-based array traversal**: empty gaps are skipped exactly like
//!   the program does; a needed-but-missing array is a structured error.
//! - volatility reference/accumulator updates per the on-chain rules.
//!
//! Verification status (see `docs/meteora-dlmm-layout.md`):
//! - Layouts: byte-verified against real mainnet accounts AND the official
//!   IDL (`idls/dlmm.json`, LbPair total 904, Bin 144).
//! - `price_from_id`: byte-identical to on-chain stored prices (140 bins,
//!   two pools, two bin steps).
//! - Quote path: ported 1:1 from the official source; must re-pass LIVE
//!   parity (zero overestimates) before it gates real sizing.
//!
//! Financial invariants: integer-only; conversion rounding DOWN, fee/capacity
//! rounding UP; missing data → structured error, never a fabricated quote.
//! Token-2022 transfer fees are NOT modelled here — discovery must screen out
//! mints with a non-zero transfer fee before this quote is trusted.

use ruint::aliases::U256;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;

/// Meteora DLMM program id (mainnet).
pub const DLMM_PROGRAM_ID: &str = "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo";

/// Anchor discriminators (sha256("account:<Name>")[..8]).
pub const LB_PAIR_DISCRIMINATOR: [u8; 8] = [0x21, 0x0b, 0x31, 0x62, 0xb5, 0x65, 0xb1, 0x0d];
pub const BIN_ARRAY_DISCRIMINATOR: [u8; 8] = [0x5c, 0x8e, 0x5c, 0xdc, 0x05, 0x94, 0x46, 0xb5];

pub const MAX_BIN_PER_ARRAY: i32 = 70;
pub const BASIS_POINT_MAX: u64 = 10_000;
/// Fee rates are expressed in 1e9 (the on-chain FEE_PRECISION).
pub const FEE_PRECISION: u64 = 1_000_000_000;
/// Hard cap on the total fee rate: 10%.
pub const MAX_FEE_RATE: u64 = 100_000_000;
/// Global bin id bounds (program constants, not per-pair).
pub const MIN_BIN_ID: i32 = -443_636;
pub const MAX_BIN_ID: i32 = 443_636;
/// The in-account bitmap covers array indices [-512, 511].
pub const BIN_ARRAY_BITMAP_SIZE: i32 = 512;

const ONE_Q64: u128 = 1u128 << 64;

// ─────────────────────────────── layouts ───────────────────────────────
// All offsets below match the official IDL (idls/dlmm.json) AND were
// byte-verified against live mainnet accounts (docs/meteora-dlmm-layout.md).

/// StaticParameters (32 bytes at offset 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticParameters {
    pub base_factor: u16,
    pub filter_period: u16,
    pub decay_period: u16,
    pub reduction_factor: u16,
    pub variable_fee_control: u32,
    pub max_volatility_accumulator: u32,
    pub min_bin_id: i32,
    pub max_bin_id: i32,
    pub protocol_share: u16,
    pub base_fee_power_factor: u8,
    /// 0=Undetermined, 1=LiquidityMining, 2=LimitOrder.
    pub function_type: u8,
    /// 0=InputOnly, 1=OnlyY.
    pub collect_fee_mode: u8,
}

/// VariableParameters (32 bytes at offset 40; timestamp at 56).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VariableParameters {
    pub volatility_accumulator: u32,
    pub volatility_reference: u32,
    pub index_reference: i32,
    pub last_update_timestamp: i64,
}

/// Decoded LbPair (the fields the quote path needs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LbPair {
    pub parameters: StaticParameters,
    pub v_parameters: VariableParameters,
    pub pair_type: u8,
    pub active_id: i32,
    pub bin_step: u16,
    pub status: u8,
    pub activation_type: u8,
    pub token_x_mint: Pubkey,
    pub token_y_mint: Pubkey,
    pub reserve_x: Pubkey,
    pub reserve_y: Pubkey,
    /// Reward mints (all-default ⇒ limit orders allowed for Undetermined).
    pub reward_mints: [Pubkey; 2],
    /// Which array indices ([-512,511] + 512 = bit) hold liquidity.
    pub bin_array_bitmap: [u64; 16],
    pub activation_point: u64,
    /// 0 = SPL Token, 1 = Token-2022.
    pub token_x_program_flag: u8,
    pub token_y_program_flag: u8,
}

impl LbPair {
    /// Limit orders participate in swaps for this pair?
    /// Port of `is_support_limit_order`.
    pub fn supports_limit_orders(&self) -> bool {
        match self.parameters.function_type {
            2 => true,  // LimitOrder
            1 => false, // LiquidityMining
            0 => self.reward_mints.iter().all(|m| *m == Pubkey::default()),
            _ => false,
        }
    }

    /// Is the trading fee charged on the input token for this direction?
    /// Port of `fee_on_input`: InputOnly ⇒ always; OnlyY ⇒ only when Y is
    /// the input (i.e. !swap_for_y). Unknown mode ⇒ true (matches upstream).
    pub fn fee_on_input(&self, swap_for_y: bool) -> bool {
        match self.parameters.collect_fee_mode {
            0 => true,
            1 => !swap_for_y,
            _ => true,
        }
    }

    /// Nearest array index with liquidity per the in-account bitmap, walking
    /// down (`swap_for_y`) or up from `start` inclusive. `None` = nothing in
    /// that direction (or `start` outside the core bitmap range, which would
    /// need the bitmap-extension account we don't model).
    fn next_array_with_liquidity(&self, swap_for_y: bool, start: i64) -> Option<i64> {
        if !((-BIN_ARRAY_BITMAP_SIZE as i64)..(BIN_ARRAY_BITMAP_SIZE as i64)).contains(&start) {
            return None;
        }
        let bit_set = |idx: i64| {
            let bit = (idx + BIN_ARRAY_BITMAP_SIZE as i64) as usize;
            (self.bin_array_bitmap[bit / 64] >> (bit % 64)) & 1 == 1
        };
        if swap_for_y {
            (-(BIN_ARRAY_BITMAP_SIZE as i64)..=start)
                .rev()
                .find(|&i| bit_set(i))
        } else {
            (start..BIN_ARRAY_BITMAP_SIZE as i64).find(|&i| bit_set(i))
        }
    }
}

/// One bin's swap-relevant state (Bin is 144 bytes; offsets from the IDL).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Bin {
    pub amount_x: u64,
    pub amount_y: u64,
    /// Q64.64, Y per X. 0 ⇒ not initialised (recompute from id).
    pub price: u128,
    /// Open limit-order amount (fills after MM + processed layers).
    pub open_order_amount: u64,
    /// Remaining amount on processed limit orders (fills after MM).
    pub processed_order_remaining_amount: u64,
    /// Non-zero ⇒ this bin's limit orders are on the ask side.
    pub limit_order_ask_side: u8,
}

impl Bin {
    /// MM liquidity on the out side.
    fn mm_amount_out(&self, swap_for_y: bool) -> u64 {
        if swap_for_y {
            self.amount_y
        } else {
            self.amount_x
        }
    }

    /// Port of `get_limit_order_amounts_by_direction`.
    fn limit_order_amounts(&self, swap_for_y: bool) -> (u64, u64) {
        let is_ask = self.limit_order_ask_side != 0;
        if (swap_for_y && !is_ask) || (!swap_for_y && is_ask) {
            (
                self.open_order_amount,
                self.processed_order_remaining_amount,
            )
        } else {
            (0, 0)
        }
    }

    /// Port of `get_max_amount_out_with_limit_orders`.
    fn max_amount_out(&self, swap_for_y: bool, support_limit_order: bool) -> u64 {
        let mm = self.mm_amount_out(swap_for_y);
        if !support_limit_order {
            return mm;
        }
        let (open, processed) = self.limit_order_amounts(swap_for_y);
        mm.saturating_add(open).saturating_add(processed)
    }
}

/// Decoded BinArray: 70 consecutive bins starting at `index * 70`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinArray {
    pub index: i64,
    pub lb_pair: Pubkey,
    pub bins: Vec<Bin>, // always 70
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DlmmDecodeError {
    TooShort { len: usize, need: usize },
    BadDiscriminator,
}

const LB_PAIR_LEN: usize = 904; // full struct per IDL
const BIN_ARRAY_LEN: usize = 8 + 8 + 8 + 32 + 70 * 144; // 10_136 (verified)
const BIN_SIZE: usize = 144;
const BINS_OFFSET: usize = 56;

fn read_pubkey(data: &[u8], off: usize) -> Pubkey {
    let mut b = [0u8; 32];
    b.copy_from_slice(&data[off..off + 32]);
    Pubkey::new_from_array(b)
}

fn u16le(d: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([d[o], d[o + 1]])
}
fn u32le(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(d[o..o + 4].try_into().unwrap())
}
fn i32le(d: &[u8], o: usize) -> i32 {
    i32::from_le_bytes(d[o..o + 4].try_into().unwrap())
}
fn u64le(d: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(d[o..o + 8].try_into().unwrap())
}
fn i64le(d: &[u8], o: usize) -> i64 {
    i64::from_le_bytes(d[o..o + 8].try_into().unwrap())
}
fn u128le(d: &[u8], o: usize) -> u128 {
    u128::from_le_bytes(d[o..o + 16].try_into().unwrap())
}

/// Decode an LbPair account (discriminator + full length checked).
pub fn decode_lb_pair(data: &[u8]) -> Result<LbPair, DlmmDecodeError> {
    if data.len() < LB_PAIR_LEN {
        return Err(DlmmDecodeError::TooShort {
            len: data.len(),
            need: LB_PAIR_LEN,
        });
    }
    if data[0..8] != LB_PAIR_DISCRIMINATOR {
        return Err(DlmmDecodeError::BadDiscriminator);
    }
    let mut bitmap = [0u64; 16];
    for (i, limb) in bitmap.iter_mut().enumerate() {
        *limb = u64le(data, 584 + i * 8);
    }
    Ok(LbPair {
        parameters: StaticParameters {
            base_factor: u16le(data, 8),
            filter_period: u16le(data, 10),
            decay_period: u16le(data, 12),
            reduction_factor: u16le(data, 14),
            variable_fee_control: u32le(data, 16),
            max_volatility_accumulator: u32le(data, 20),
            min_bin_id: i32le(data, 24),
            max_bin_id: i32le(data, 28),
            protocol_share: u16le(data, 32),
            base_fee_power_factor: data[34],
            function_type: data[35],
            collect_fee_mode: data[36],
        },
        v_parameters: VariableParameters {
            volatility_accumulator: u32le(data, 40),
            volatility_reference: u32le(data, 44),
            index_reference: i32le(data, 48),
            last_update_timestamp: i64le(data, 56),
        },
        pair_type: data[75],
        active_id: i32le(data, 76),
        bin_step: u16le(data, 80),
        status: data[82],
        activation_type: data[86],
        token_x_mint: read_pubkey(data, 88),
        token_y_mint: read_pubkey(data, 120),
        reserve_x: read_pubkey(data, 152),
        reserve_y: read_pubkey(data, 184),
        reward_mints: [read_pubkey(data, 264), read_pubkey(data, 264 + 144)],
        bin_array_bitmap: bitmap,
        activation_point: u64le(data, 816),
        token_x_program_flag: data[880],
        token_y_program_flag: data[881],
    })
}

/// Decode a BinArray account (discriminator + exact length checked).
pub fn decode_bin_array(data: &[u8]) -> Result<BinArray, DlmmDecodeError> {
    if data.len() < BIN_ARRAY_LEN {
        return Err(DlmmDecodeError::TooShort {
            len: data.len(),
            need: BIN_ARRAY_LEN,
        });
    }
    if data[0..8] != BIN_ARRAY_DISCRIMINATOR {
        return Err(DlmmDecodeError::BadDiscriminator);
    }
    let mut bins = Vec::with_capacity(70);
    for b in 0..70 {
        let o = BINS_OFFSET + b * BIN_SIZE;
        bins.push(Bin {
            amount_x: u64le(data, o),
            amount_y: u64le(data, o + 8),
            price: u128le(data, o + 16),
            open_order_amount: u64le(data, o + 112),
            processed_order_remaining_amount: u64le(data, o + 128),
            limit_order_ask_side: data[o + 140],
        });
    }
    Ok(BinArray {
        index: i64le(data, 8),
        lb_pair: read_pubkey(data, 24),
        bins,
    })
}

// ─────────────────────────── price math (EXACT) ───────────────────────────

/// Floor division toward −∞ (bin ids are signed).
pub fn bin_array_index(bin_id: i32) -> i64 {
    (bin_id as i64).div_euclid(MAX_BIN_PER_ARRAY as i64)
}

/// Position of `bin_id` inside its array (0..70).
pub fn bin_offset_in_array(bin_id: i32) -> usize {
    (bin_id as i64).rem_euclid(MAX_BIN_PER_ARRAY as i64) as usize
}

/// Q64.64 price of a bin: `(1 + bin_step/10000)^bin_id`, using Meteora's exact
/// pow algorithm (inverse-base trick + floor muls). **Byte-identical to the
/// on-chain stored bin prices for 140 bins across two live pools.**
pub fn price_from_id(bin_id: i32, bin_step: u16) -> Option<u128> {
    let base = ONE_Q64 + ((bin_step as u128) << 64) / BASIS_POINT_MAX as u128;
    pow_q64(base, bin_id)
}

fn pow_q64(base: u128, exp: i32) -> Option<u128> {
    if exp == 0 {
        return Some(ONE_Q64);
    }
    let mut invert = exp < 0;
    let mut n = exp.unsigned_abs();
    let mut sq = base;
    let mut result = ONE_Q64;
    if sq >= result {
        sq = u128::MAX.checked_div(sq)?;
        invert = !invert;
    }
    while n > 0 {
        if n & 1 == 1 {
            result = mul_shr_floor(result, sq)?;
        }
        sq = mul_shr_floor(sq, sq)?;
        n >>= 1;
    }
    if result == 0 {
        return None;
    }
    if invert {
        result = u128::MAX.checked_div(result)?;
    }
    Some(result)
}

fn mul_shr_floor(a: u128, b: u128) -> Option<u128> {
    let p = U256::from(a) * U256::from(b);
    let r: U256 = p >> 64;
    (r <= U256::from(u128::MAX)).then(|| r.to::<u128>())
}

fn mul_shr_ceil(a: u128, b: u128) -> Option<u128> {
    let p = U256::from(a) * U256::from(b);
    let mask = (U256::from(1u8) << 64) - U256::from(1u8);
    let r: U256 = (p >> 64)
        + if p & mask > U256::ZERO {
            U256::from(1u8)
        } else {
            U256::ZERO
        };
    (r <= U256::from(u128::MAX)).then(|| r.to::<u128>())
}

fn shl_div_floor(a: u128, price: u128) -> Option<u128> {
    if price == 0 {
        return None;
    }
    let r: U256 = (U256::from(a) << 64) / U256::from(price);
    (r <= U256::from(u128::MAX)).then(|| r.to::<u128>())
}

fn shl_div_ceil(a: u128, price: u128) -> Option<u128> {
    if price == 0 {
        return None;
    }
    let n = U256::from(a) << 64;
    let d = U256::from(price);
    let r: U256 = (n + d - U256::from(1u8)) / d;
    (r <= U256::from(u128::MAX)).then(|| r.to::<u128>())
}

/// Port of `Bin::get_amount_out` (conversion of input to output at price).
fn amount_out_at_price(amount_in: u64, price: u128, swap_for_y: bool, ceil: bool) -> Option<u64> {
    let v = if swap_for_y {
        if ceil {
            mul_shr_ceil(price, amount_in as u128)?
        } else {
            mul_shr_floor(price, amount_in as u128)?
        }
    } else if ceil {
        shl_div_ceil(amount_in as u128, price)?
    } else {
        shl_div_floor(amount_in as u128, price)?
    };
    u64::try_from(v).ok()
}

/// Port of `Bin::get_amount_in` (input needed for a given output at price).
fn amount_in_for_out(amount_out: u64, price: u128, swap_for_y: bool, ceil: bool) -> Option<u64> {
    let v = if swap_for_y {
        if ceil {
            shl_div_ceil(amount_out as u128, price)?
        } else {
            shl_div_floor(amount_out as u128, price)?
        }
    } else if ceil {
        mul_shr_ceil(price, amount_out as u128)?
    } else {
        mul_shr_floor(price, amount_out as u128)?
    };
    u64::try_from(v).ok()
}

// ─────────────────────────────── fees ───────────────────────────────
// Ports of the LbPairExtension fee functions (u128 rates in 1e9 scale).

pub fn base_fee_rate(p: &StaticParameters, bin_step: u16) -> u128 {
    (p.base_factor as u128)
        * (bin_step as u128)
        * 10u128
        * 10u128.pow(p.base_fee_power_factor as u32)
}

pub fn variable_fee_rate(p: &StaticParameters, bin_step: u16, volatility_accumulator: u32) -> u128 {
    if p.variable_fee_control == 0 {
        return 0;
    }
    let square = (volatility_accumulator as u128 * bin_step as u128).pow(2);
    (square * p.variable_fee_control as u128).div_ceil(100_000_000_000)
}

pub fn total_fee_rate(p: &StaticParameters, bin_step: u16, volatility_accumulator: u32) -> u128 {
    (base_fee_rate(p, bin_step) + variable_fee_rate(p, bin_step, volatility_accumulator))
        .min(MAX_FEE_RATE as u128)
}

/// Fee ON TOP of a net amount: `ceil(amount·rate/(1e9−rate))`.
fn compute_fee(amount: u64, rate: u128) -> Option<u64> {
    let denominator = (FEE_PRECISION as u128).checked_sub(rate)?;
    if denominator == 0 {
        return None;
    }
    let fee = (amount as u128)
        .checked_mul(rate)?
        .checked_add(denominator)?
        - 1;
    u64::try_from(fee / denominator).ok()
}

/// Fee taken FROM a gross amount: `ceil(amount·rate/1e9)`.
fn compute_fee_from_amount(amount_with_fees: u64, rate: u128) -> Option<u64> {
    let fee = (amount_with_fees as u128)
        .checked_mul(rate)?
        .checked_add(FEE_PRECISION as u128 - 1)?;
    u64::try_from(fee / FEE_PRECISION as u128).ok()
}

// ───────────────────────── volatility updates ─────────────────────────
// Ports of `update_references` / `update_volatility_accumulator`; they act on
// a WORKING COPY of VariableParameters during a quote.

pub fn update_references(
    p: &StaticParameters,
    v: &mut VariableParameters,
    active_id: i32,
    now_unix: i64,
) {
    let elapsed = now_unix.saturating_sub(v.last_update_timestamp);
    if elapsed >= p.filter_period as i64 {
        v.index_reference = active_id;
        if elapsed < p.decay_period as i64 {
            v.volatility_reference = ((v.volatility_accumulator as u64 * p.reduction_factor as u64)
                / BASIS_POINT_MAX) as u32;
        } else {
            v.volatility_reference = 0;
        }
    }
}

pub fn update_volatility_accumulator(
    p: &StaticParameters,
    v: &mut VariableParameters,
    active_id: i32,
) {
    let delta = (v.index_reference as i64 - active_id as i64).unsigned_abs();
    let va = v.volatility_reference as u64 + delta * BASIS_POINT_MAX;
    v.volatility_accumulator = va.min(p.max_volatility_accumulator as u64) as u32;
}

// ─────────────────────────────── quote ───────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DlmmQuoteError {
    /// Pair disabled, unsupported pair type, or unknown mode byte.
    PairNotSupported { reason: &'static str },
    /// The traversal needed a bin array we don't hold — refuse to guess.
    InsufficientBinCoverage { missing_array_index: i64 },
    /// No liquidity left in this direction (bitmap exhausted / id bounds).
    ExhaustedLiquidity,
    /// Zero input or output.
    NoFill,
    /// Integer overflow / bad price data.
    MathOverflow,
}

impl DlmmQuoteError {
    /// True for errors that mean "this size is too big for the liquidity we
    /// hold" (as opposed to a structural problem) — the optimizer treats these
    /// as an upper bound to search under, not a route-wide failure.
    pub fn is_capacity(&self) -> bool {
        matches!(
            self,
            DlmmQuoteError::InsufficientBinCoverage { .. } | DlmmQuoteError::ExhaustedLiquidity
        )
    }
}

struct FillResult {
    amount_in: u64,
    amount_left: u64,
    out_amount: u64,
}

/// Port of `calculate_exact_in_fill_amount`: fill `amount` against one
/// liquidity layer of `max_amount_out` at `price`.
fn fill_layer(
    amount: u64,
    max_amount_out: u64,
    price: u128,
    swap_for_y: bool,
) -> Result<FillResult, DlmmQuoteError> {
    if max_amount_out == 0 {
        return Ok(FillResult {
            amount_in: 0,
            amount_left: amount,
            out_amount: 0,
        });
    }
    let max_amount_in = amount_in_for_out(max_amount_out, price, swap_for_y, true)
        .ok_or(DlmmQuoteError::MathOverflow)?;
    if amount >= max_amount_in {
        Ok(FillResult {
            amount_in: max_amount_in,
            amount_left: amount - max_amount_in,
            out_amount: max_amount_out,
        })
    } else {
        Ok(FillResult {
            amount_in: amount,
            amount_left: 0,
            out_amount: amount_out_at_price(amount, price, swap_for_y, false)
                .ok_or(DlmmQuoteError::MathOverflow)?,
        })
    }
}

struct ExactInFill {
    amount_left: u64,
    out_amount: u64,
}

/// Port of `get_exact_in_fill_amount_result`: MM layer, then processed limit
/// orders, then open limit orders.
fn fill_bin(
    bin: &Bin,
    amount_in: u64,
    price: u128,
    swap_for_y: bool,
    support_limit_order: bool,
) -> Result<ExactInFill, DlmmQuoteError> {
    let mm = fill_layer(amount_in, bin.mm_amount_out(swap_for_y), price, swap_for_y)?;
    if !support_limit_order {
        return Ok(ExactInFill {
            amount_left: mm.amount_left,
            out_amount: mm.out_amount,
        });
    }
    let mut total_in = mm.amount_in;
    let mut total_out = mm.out_amount;
    if mm.amount_left > 0 {
        let (open, processed) = bin.limit_order_amounts(swap_for_y);
        let pf = fill_layer(mm.amount_left, processed, price, swap_for_y)?;
        total_in += pf.amount_in;
        total_out += pf.out_amount;
        if pf.amount_left > 0 {
            let of = fill_layer(pf.amount_left, open, price, swap_for_y)?;
            total_in += of.amount_in;
            total_out += of.out_amount;
        }
    }
    Ok(ExactInFill {
        amount_left: amount_in - total_in,
        out_amount: total_out,
    })
}

struct BinQuote {
    amount_in: u64,
    amount_out: u64,
    /// Trading fee charged at this bin (input-token units when fee_on_input,
    /// else output-token units).
    fee: u64,
}

/// Port of `swap_exact_in_quote_at_bin` (fee-on-input vs fee-on-output).
#[allow(clippy::too_many_arguments)]
fn quote_at_bin(
    bin: &Bin,
    p: &StaticParameters,
    bin_step: u16,
    volatility_accumulator: u32,
    in_amount: u64,
    price: u128,
    swap_for_y: bool,
    support_limit_order: bool,
    fee_on_input: bool,
) -> Result<BinQuote, DlmmQuoteError> {
    let rate = total_fee_rate(p, bin_step, volatility_accumulator);
    let mut excluded_fee_amount_in = in_amount;
    if fee_on_input {
        let fee = compute_fee_from_amount(in_amount, rate).ok_or(DlmmQuoteError::MathOverflow)?;
        excluded_fee_amount_in = in_amount
            .checked_sub(fee)
            .ok_or(DlmmQuoteError::MathOverflow)?;
    }

    let fill = fill_bin(
        bin,
        excluded_fee_amount_in,
        price,
        swap_for_y,
        support_limit_order,
    )?;

    let mut included_fee_amount_in = in_amount;
    if fill.amount_left > 0 {
        excluded_fee_amount_in = excluded_fee_amount_in
            .checked_sub(fill.amount_left)
            .ok_or(DlmmQuoteError::MathOverflow)?;
        if fee_on_input {
            let fee =
                compute_fee(excluded_fee_amount_in, rate).ok_or(DlmmQuoteError::MathOverflow)?;
            included_fee_amount_in = excluded_fee_amount_in
                .checked_add(fee)
                .ok_or(DlmmQuoteError::MathOverflow)?;
        } else {
            included_fee_amount_in = excluded_fee_amount_in;
        }
    }

    let mut excluded_fee_amount_out = fill.out_amount;
    if !fee_on_input {
        let fee =
            compute_fee_from_amount(fill.out_amount, rate).ok_or(DlmmQuoteError::MathOverflow)?;
        excluded_fee_amount_out = fill
            .out_amount
            .checked_sub(fee)
            .ok_or(DlmmQuoteError::MathOverflow)?;
    }

    let fee = if fee_on_input {
        included_fee_amount_in.saturating_sub(excluded_fee_amount_in)
    } else {
        fill.out_amount.saturating_sub(excluded_fee_amount_out)
    };
    Ok(BinQuote {
        amount_in: included_fee_amount_in,
        amount_out: excluded_fee_amount_out,
        fee,
    })
}

/// Exact-in DLMM quote — faithful port of Meteora's `quote_exact_in`.
///
/// * `swap_for_y = true`: X in → Y out (walk DOWN); false: Y in → X out (UP).
/// * `bin_arrays`: decoded arrays by index. The traversal follows the pair's
///   liquidity bitmap (skipping empty gaps exactly like the program); an
///   array the bitmap demands but the map lacks ⇒ `InsufficientBinCoverage`.
/// * `now_unix` drives the volatility reference decay.
///
/// NOT modelled (callers must screen): Token-2022 transfer fees on either
/// mint; pairs of type Permission/CustomizablePermissionless (activation
/// gating) are refused.
pub fn dlmm_quote_exact_in(
    pair: &LbPair,
    bin_arrays: &HashMap<i64, BinArray>,
    swap_for_y: bool,
    amount_in: u64,
    now_unix: i64,
) -> Result<u64, DlmmQuoteError> {
    dlmm_quote_exact_in_detailed(pair, bin_arrays, swap_for_y, amount_in, now_unix).map(|(o, _)| o)
}

/// Like [`dlmm_quote_exact_in`] but also returns the total DEX fee (in the
/// fee's charged-token units: input token when the pool collects on input for
/// this direction, else output token).
pub fn dlmm_quote_exact_in_detailed(
    pair: &LbPair,
    bin_arrays: &HashMap<i64, BinArray>,
    swap_for_y: bool,
    amount_in: u64,
    now_unix: i64,
) -> Result<(u64, u64), DlmmQuoteError> {
    if amount_in == 0 {
        return Err(DlmmQuoteError::NoFill);
    }
    if pair.status != 0 {
        return Err(DlmmQuoteError::PairNotSupported {
            reason: "pair disabled",
        });
    }
    // Permission (1) / CustomizablePermissionless (2) need activation-point
    // checks against slot/time we don't carry here — refuse, don't guess.
    if pair.pair_type == 1 || pair.pair_type == 2 {
        return Err(DlmmQuoteError::PairNotSupported {
            reason: "permissioned pair type",
        });
    }

    let p = pair.parameters;
    let support_limit_order = pair.supports_limit_orders();
    let fee_on_input = pair.fee_on_input(swap_for_y);

    // Working copies (the quote simulates the program's state evolution).
    let mut v = pair.v_parameters;
    let mut active_id = pair.active_id;
    update_references(&p, &mut v, active_id, now_unix);

    let mut amount_left = amount_in;
    let mut total_out: u64 = 0;
    let mut total_fee: u64 = 0;

    while amount_left > 0 {
        // Next array with liquidity per the bitmap (skips empty gaps).
        let start = bin_array_index(active_id);
        let arr_idx = pair
            .next_array_with_liquidity(swap_for_y, start)
            .ok_or(DlmmQuoteError::ExhaustedLiquidity)?;
        let arr = bin_arrays
            .get(&arr_idx)
            .ok_or(DlmmQuoteError::InsufficientBinCoverage {
                missing_array_index: arr_idx,
            })?;

        // Port of `shift_active_bin_if_empty_gap`: jump across skipped gap.
        if arr_idx != start {
            active_id = if swap_for_y {
                (arr_idx * 70 + 69) as i32 // upper bin of the array
            } else {
                (arr_idx * 70) as i32 // lower bin of the array
            };
        }

        // Inner loop: consume bins inside this array.
        while bin_array_index(active_id) == arr_idx && amount_left > 0 {
            let bin = arr.bins[bin_offset_in_array(active_id)];
            let price = if bin.price != 0 {
                bin.price
            } else {
                price_from_id(active_id, pair.bin_step).ok_or(DlmmQuoteError::MathOverflow)?
            };

            if bin.max_amount_out(swap_for_y, support_limit_order) > 0 {
                update_volatility_accumulator(&p, &mut v, active_id);
                let r = quote_at_bin(
                    &bin,
                    &p,
                    pair.bin_step,
                    v.volatility_accumulator,
                    amount_left,
                    price,
                    swap_for_y,
                    support_limit_order,
                    fee_on_input,
                )?;
                if r.amount_in > 0 {
                    amount_left = amount_left
                        .checked_sub(r.amount_in)
                        .ok_or(DlmmQuoteError::MathOverflow)?;
                    total_out = total_out
                        .checked_add(r.amount_out)
                        .ok_or(DlmmQuoteError::MathOverflow)?;
                    total_fee = total_fee.saturating_add(r.fee);
                }
            }

            if amount_left > 0 {
                // Port of `advance_active_bin` (global id bounds).
                active_id += if swap_for_y { -1 } else { 1 };
                if !(MIN_BIN_ID..=MAX_BIN_ID).contains(&active_id) {
                    return Err(DlmmQuoteError::ExhaustedLiquidity);
                }
            }
        }
    }

    if total_out == 0 {
        return Err(DlmmQuoteError::NoFill);
    }
    Ok((total_out, total_fee))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    // Real mainnet fixtures captured 2026-07-12 (docs/meteora-dlmm-layout.md):
    // pair J4cGfY61ZMaBD2niXcfaUD7KsNZiDnjMnJsPJficos8J — pump-token/WSOL,
    // bin_step 15, active_id 643 at capture, function_type=LimitOrder,
    // collect_fee_mode=OnlyY, token X is Token-2022.
    const LB_PAIR_BYTES: &[u8] = include_bytes!("../fixtures/meteora/lbpair_J4cGfY61.bin");
    const BIN_ARRAY_9: &[u8] = include_bytes!("../fixtures/meteora/binarray_idx9_J4cGfY61.bin");
    // A DIFFERENT pool's array (bin_step 20) — cross-pool price validation.
    const BIN_ARRAY_OTHER: &[u8] =
        include_bytes!("../fixtures/meteora/binarray_idx6_step20_other.bin");

    fn pair() -> LbPair {
        decode_lb_pair(LB_PAIR_BYTES).unwrap()
    }
    fn array9() -> BinArray {
        decode_bin_array(BIN_ARRAY_9).unwrap()
    }
    fn arrays() -> HashMap<i64, BinArray> {
        let mut m = HashMap::new();
        m.insert(9, array9());
        m
    }

    #[test]
    fn decodes_real_lb_pair_including_v2_fields() {
        let p = pair();
        assert_eq!(
            p.token_x_mint,
            Pubkey::from_str("9cRCn9rGT8V2imeM2BaKs13yhMEais3ruM3rPvTGpump").unwrap()
        );
        // WSOL is token Y here — side must be derived, never assumed.
        assert_eq!(
            p.token_y_mint,
            Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap()
        );
        assert_eq!(
            p.reserve_x,
            Pubkey::from_str("FXnrNMBqkt8moyeRXZ5nrDEUiRaRndZng9EZy5WbNTiF").unwrap()
        );
        assert_eq!(p.bin_step, 15);
        assert_eq!(p.active_id, 643);
        assert_eq!(p.status, 0);
        assert_eq!(p.pair_type, 3); // PermissionlessV2
        assert_eq!(p.parameters.base_factor, 10_000);
        assert_eq!(p.parameters.protocol_share, 1_000);
        // v2 fields (verified against the official IDL + this account):
        assert_eq!(p.parameters.function_type, 2); // LimitOrder
        assert_eq!(p.parameters.collect_fee_mode, 1); // OnlyY
        assert!(p.supports_limit_orders());
        assert!(!p.fee_on_input(true)); // X in ⇒ fee on Y OUTPUT
        assert!(p.fee_on_input(false)); // Y in ⇒ fee on Y INPUT
        assert_eq!(p.token_x_program_flag, 1); // Token-2022!
        assert_eq!(p.token_y_program_flag, 0);
        assert_eq!(p.reward_mints, [Pubkey::default(); 2]);
        let ts = p.v_parameters.last_update_timestamp;
        assert!((1_577_836_800..4_102_444_800).contains(&ts), "ts={ts}");
        // Bitmap sanity (full coverage in `bitmap_navigation_matches_study`).
        assert_eq!(p.next_array_with_liquidity(false, 9), Some(9));
    }

    #[test]
    fn bitmap_navigation_matches_study() {
        let p = pair();
        // Liquidity in [-6, 20] (from the captured bitmap).
        assert_eq!(p.next_array_with_liquidity(false, -10), Some(-6));
        assert_eq!(p.next_array_with_liquidity(false, 0), Some(0));
        assert_eq!(p.next_array_with_liquidity(false, 20), Some(20));
        assert_eq!(p.next_array_with_liquidity(false, 21), None);
        assert_eq!(p.next_array_with_liquidity(true, 25), Some(20));
        assert_eq!(p.next_array_with_liquidity(true, -6), Some(-6));
        assert_eq!(p.next_array_with_liquidity(true, -7), None);
        // Outside the core bitmap → None (bitmap extension unsupported).
        assert_eq!(p.next_array_with_liquidity(false, 600), None);
    }

    #[test]
    fn decodes_real_bin_array_with_limit_orders() {
        let a = array9();
        assert_eq!(a.index, 9);
        assert_eq!(
            a.lb_pair,
            Pubkey::from_str("J4cGfY61ZMaBD2niXcfaUD7KsNZiDnjMnJsPJficos8J").unwrap()
        );
        assert_eq!(a.bins.len(), 70);
        assert_eq!(a.bins[0].amount_y, 13_189_812_598); // bin 630
        assert_eq!(a.bins[68].amount_x, 3_863_113_477); // bin 698
                                                        // The capture really contains open limit orders (4 bins).
        let lo_bins = a
            .bins
            .iter()
            .filter(|b| b.open_order_amount > 0 || b.processed_order_remaining_amount > 0)
            .count();
        assert_eq!(lo_bins, 4);
    }

    #[test]
    fn rejects_wrong_discriminator_and_short() {
        let mut d = LB_PAIR_BYTES.to_vec();
        d[0] ^= 0xff;
        assert_eq!(decode_lb_pair(&d), Err(DlmmDecodeError::BadDiscriminator));
        assert!(matches!(
            decode_lb_pair(&LB_PAIR_BYTES[..100]),
            Err(DlmmDecodeError::TooShort { .. })
        ));
        let mut b = BIN_ARRAY_9.to_vec();
        b[3] ^= 0x01;
        assert_eq!(decode_bin_array(&b), Err(DlmmDecodeError::BadDiscriminator));
    }

    /// THE key exactness test: our price function must reproduce the on-chain
    /// stored price of every initialised bin, byte-for-byte, in two different
    /// pools with different bin steps (140 bins total).
    #[test]
    fn price_from_id_is_byte_exact_vs_chain() {
        let a9 = array9();
        for (i, bin) in a9.bins.iter().enumerate() {
            if bin.price == 0 {
                continue;
            }
            let id = (a9.index * 70) as i32 + i as i32;
            assert_eq!(price_from_id(id, 15).unwrap(), bin.price, "bin {id}");
        }
        let other = decode_bin_array(BIN_ARRAY_OTHER).unwrap();
        for (i, bin) in other.bins.iter().enumerate() {
            if bin.price == 0 {
                continue;
            }
            let id = (other.index * 70) as i32 + i as i32;
            assert_eq!(price_from_id(id, 20).unwrap(), bin.price, "bin {id}");
        }
    }

    #[test]
    fn negative_ids_and_array_index_math() {
        assert_eq!(bin_array_index(0), 0);
        assert_eq!(bin_array_index(69), 0);
        assert_eq!(bin_array_index(70), 1);
        assert_eq!(bin_array_index(-1), -1);
        assert_eq!(bin_array_index(-70), -1);
        assert_eq!(bin_array_index(-71), -2);
        assert_eq!(bin_offset_in_array(-1), 69);
        assert_eq!(bin_offset_in_array(643), 13);
        let p = price_from_id(500, 15).unwrap();
        let n = price_from_id(-500, 15).unwrap();
        let prod = U256::from(p) * U256::from(n);
        let one = U256::from(1u8) << 128;
        let diff = if prod > one { prod - one } else { one - prod };
        assert!(diff < (one >> 40), "inverse identity too loose: {diff}");
    }

    #[test]
    fn base_fee_matches_live_pair() {
        let p = pair();
        // base_factor 10000 * bin_step 15 * 10 = 1_500_000 / 1e9 = 0.15%
        assert_eq!(base_fee_rate(&p.parameters, p.bin_step), 1_500_000);
        assert_eq!(variable_fee_rate(&p.parameters, p.bin_step, 0), 0);
        assert!(total_fee_rate(&p.parameters, p.bin_step, u32::MAX) <= MAX_FEE_RATE as u128);
    }

    #[test]
    fn quote_small_swap_both_directions() {
        let p = pair();
        let now = p.v_parameters.last_update_timestamp + 5;
        let out_x = dlmm_quote_exact_in(&p, &arrays(), false, 1_000_000_000, now).unwrap();
        assert!(out_x > 0);
        let out_y = dlmm_quote_exact_in(&p, &arrays(), true, 1_000_000_000, now).unwrap();
        assert!(out_y > 0);
    }

    #[test]
    fn quote_never_overestimates_vs_feeless_spot() {
        let p = pair();
        let now = p.v_parameters.last_update_timestamp + 5;
        let amt: u64 = 2_000_000_000;
        let out = dlmm_quote_exact_in(&p, &arrays(), false, amt, now).unwrap() as u128;
        let price = price_from_id(p.active_id, p.bin_step).unwrap();
        let ub = shl_div_floor(amt as u128, price).unwrap();
        assert!(out < ub, "quote {out} must be below feeless spot {ub}");
    }

    #[test]
    fn quote_is_monotonic_in_input() {
        let p = pair();
        let now = p.v_parameters.last_update_timestamp + 5;
        let mut prev = 0u64;
        for amt in [100_000_000u64, 500_000_000, 1_000_000_000, 3_000_000_000] {
            let out = dlmm_quote_exact_in(&p, &arrays(), false, amt, now).unwrap();
            assert!(out >= prev, "output must not shrink as input grows");
            prev = out;
        }
    }

    #[test]
    fn quote_rejects_when_bins_missing_never_fakes() {
        let p = pair();
        let now = p.v_parameters.last_update_timestamp + 5;
        // Exhausting array 9 upward demands array 10 (bitmap says it has
        // liquidity) — we don't hold it, so the quote REFUSES.
        let err = dlmm_quote_exact_in(&p, &arrays(), false, u64::MAX / 4, now).unwrap_err();
        assert_eq!(
            err,
            DlmmQuoteError::InsufficientBinCoverage {
                missing_array_index: 10
            }
        );
        // Downward: array 8.
        let err = dlmm_quote_exact_in(&p, &arrays(), true, u64::MAX / 4, now).unwrap_err();
        assert_eq!(
            err,
            DlmmQuoteError::InsufficientBinCoverage {
                missing_array_index: 8
            }
        );
        assert_eq!(
            dlmm_quote_exact_in(&p, &arrays(), false, 0, now),
            Err(DlmmQuoteError::NoFill)
        );
    }

    #[test]
    fn disabled_or_permissioned_pairs_are_refused() {
        let now = pair().v_parameters.last_update_timestamp + 5;
        let mut disabled = pair();
        disabled.status = 1;
        assert!(matches!(
            dlmm_quote_exact_in(&disabled, &arrays(), false, 1_000, now),
            Err(DlmmQuoteError::PairNotSupported { .. })
        ));
        let mut permissioned = pair();
        permissioned.pair_type = 1;
        assert!(matches!(
            dlmm_quote_exact_in(&permissioned, &arrays(), false, 1_000, now),
            Err(DlmmQuoteError::PairNotSupported { .. })
        ));
    }

    #[test]
    fn higher_volatility_reduces_output() {
        let p = pair();
        let calm = p.v_parameters.last_update_timestamp + 5;
        let out_calm = dlmm_quote_exact_in(&p, &arrays(), false, 1_000_000_000, calm).unwrap();
        let mut hot = pair();
        hot.v_parameters.volatility_accumulator = hot.parameters.max_volatility_accumulator;
        hot.v_parameters.volatility_reference = hot.parameters.max_volatility_accumulator;
        hot.v_parameters.index_reference = hot.active_id;
        let out_hot = dlmm_quote_exact_in(&hot, &arrays(), false, 1_000_000_000, calm).unwrap();
        assert!(
            out_hot < out_calm,
            "variable fee must cut output: hot={out_hot} calm={out_calm}"
        );
    }

    #[test]
    fn decay_reduces_fee_over_time() {
        let p = pair();
        let mut v = p.v_parameters;
        v.volatility_accumulator = 100_000;
        v.volatility_reference = 100_000;
        v.index_reference = p.active_id;

        // Within filter period: references kept.
        let mut v0 = v;
        update_references(
            &p.parameters,
            &mut v0,
            p.active_id,
            v.last_update_timestamp + 1,
        );
        assert_eq!(v0.volatility_reference, 100_000);
        // Between filter and decay: reduced.
        let mut v1 = v;
        update_references(
            &p.parameters,
            &mut v1,
            p.active_id,
            v.last_update_timestamp + p.parameters.filter_period as i64 + 1,
        );
        assert_eq!(
            v1.volatility_reference,
            (100_000u64 * p.parameters.reduction_factor as u64 / BASIS_POINT_MAX) as u32
        );
        // Past decay: zeroed.
        let mut v2 = v;
        update_references(
            &p.parameters,
            &mut v2,
            p.active_id,
            v.last_update_timestamp + p.parameters.decay_period as i64 + 1,
        );
        assert_eq!(v2.volatility_reference, 0);
    }
}
