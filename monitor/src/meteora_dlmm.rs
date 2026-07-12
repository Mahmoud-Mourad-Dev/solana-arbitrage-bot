//! Meteora DLMM (`LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo`) support:
//! LbPair + BinArray parsers, exact Q64.64 bin-price math, variable-fee
//! model, and an integer bin-traversal quote.
//!
//! Verification status (see `docs/meteora-dlmm-layout.md`):
//! - **LbPair / BinArray layouts: VERIFIED** against real mainnet accounts
//!   (parsed reserves resolve to token accounts of the parsed mints, owned by
//!   the pair; bin-array `lb_pair` back-pointer and index range check out).
//! - **`price_from_id`: EXACT** — byte-identical to the on-chain stored bin
//!   prices for 140 bins across two pools with different bin steps.
//! - **Swap traversal + fee application: NEAR-PARITY, not exact.** Live
//!   parity vs 3 real swaps (2026-07-12): two single-bin fills −1 unit
//!   (conservative), one bin-crossing fill **+679 (+0.0006%) OVERestimate**.
//!   Known gaps vs Meteora's current `commons/src/quote.rs`: per-bin limit
//!   order fills and collect-fee-mode (fee-on-input pools) are NOT modelled
//!   here. Until the full port (S4b) lands and re-passes live parity, treat
//!   this quote as approximate and do not use it for final go/no-go sizing.
//!
//! Financial invariants: integer-only; output rounding is always DOWN and fee
//! rounding always UP (never overestimate output); missing bin arrays produce
//! [`DlmmQuoteError::InsufficientBinCoverage`], never a partial fake quote.

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
/// Fee rates are expressed in 1e9 (like the on-chain FEE_PRECISION).
pub const FEE_PRECISION: u64 = 1_000_000_000;
/// Hard cap on the total fee rate: 10%.
pub const MAX_FEE_RATE: u64 = 100_000_000;

const ONE_Q64: u128 = 1u128 << 64;

// ─────────────────────────────── layouts ───────────────────────────────

/// StaticParameters (32 bytes at offset 8). Field offsets verified against a
/// live pair (base_factor 10000, bin_step 15 ⇒ 0.15% base fee — plausible and
/// cross-checked against the pair's advertised fee).
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
}

/// VariableParameters (32 bytes at offset 40). `last_update_timestamp` offset
/// (56) verified: parses to a unix time minutes before capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VariableParameters {
    pub volatility_accumulator: u32,
    pub volatility_reference: u32,
    pub index_reference: i32,
    pub last_update_timestamp: i64,
}

/// Decoded LbPair (the fields the strategy needs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LbPair {
    pub parameters: StaticParameters,
    pub v_parameters: VariableParameters,
    pub pair_type: u8,
    pub active_id: i32,
    pub bin_step: u16,
    pub status: u8,
    pub token_x_mint: Pubkey,
    pub token_y_mint: Pubkey,
    pub reserve_x: Pubkey,
    pub reserve_y: Pubkey,
}

/// One bin's swap-relevant state. `price` is Q64.64 (Y per X).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Bin {
    pub amount_x: u64,
    pub amount_y: u64,
    pub price: u128,
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

const LB_PAIR_MIN_LEN: usize = 216; // through reserve_y
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

/// Decode an LbPair account (discriminator + length checked).
pub fn decode_lb_pair(data: &[u8]) -> Result<LbPair, DlmmDecodeError> {
    if data.len() < LB_PAIR_MIN_LEN {
        return Err(DlmmDecodeError::TooShort {
            len: data.len(),
            need: LB_PAIR_MIN_LEN,
        });
    }
    if data[0..8] != LB_PAIR_DISCRIMINATOR {
        return Err(DlmmDecodeError::BadDiscriminator);
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
        token_x_mint: read_pubkey(data, 88),
        token_y_mint: read_pubkey(data, 120),
        reserve_x: read_pubkey(data, 152),
        reserve_y: read_pubkey(data, 184),
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

/// Meteora `u128x128_math::pow`: Q64.64 base, signed exponent. When the base
/// is ≥ 1.0 it works with `u128::MAX / base` (≈ the Q64.64 inverse) and flips
/// the invert flag; every multiply is floor(`>> 64`).
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

// ─────────────────────────── fees (PROVISIONAL) ───────────────────────────

/// Base fee rate in 1e9 units: `base_factor * bin_step * 10 * 10^power`.
pub fn base_fee_rate(p: &StaticParameters, bin_step: u16) -> u64 {
    let r = (p.base_factor as u128)
        * (bin_step as u128)
        * 10u128
        * 10u128.pow(p.base_fee_power_factor as u32);
    r.min(u64::MAX as u128) as u64
}

/// Variable fee rate in 1e9 units for a given volatility accumulator:
/// `ceil((va * bin_step)^2 * vfc / 1e11)`.
pub fn variable_fee_rate(p: &StaticParameters, bin_step: u16, volatility_accumulator: u32) -> u64 {
    if p.variable_fee_control == 0 {
        return 0;
    }
    let square = (volatility_accumulator as u128 * bin_step as u128).pow(2);
    let v = square * p.variable_fee_control as u128;
    v.div_ceil(100_000_000_000).min(u64::MAX as u128) as u64
}

/// Total fee rate, capped at 10%.
pub fn total_fee_rate(p: &StaticParameters, bin_step: u16, volatility_accumulator: u32) -> u64 {
    (base_fee_rate(p, bin_step).saturating_add(variable_fee_rate(
        p,
        bin_step,
        volatility_accumulator,
    )))
    .min(MAX_FEE_RATE)
}

/// Fee ON TOP of a net amount (used when a bin is fully consumed):
/// `ceil(amount * rate / (1e9 − rate))`.
fn compute_fee(amount: u128, rate: u64) -> Option<u128> {
    let denom = (FEE_PRECISION as u128).checked_sub(rate as u128)?;
    if denom == 0 {
        return None;
    }
    let num = amount.checked_mul(rate as u128)?;
    Some(num.div_ceil(denom))
}

/// Fee taken FROM a gross amount (partial-bin case):
/// `ceil(amount * rate / 1e9)`.
fn compute_fee_from_amount(amount: u128, rate: u64) -> Option<u128> {
    let num = amount.checked_mul(rate as u128)?;
    Some(num.div_ceil(FEE_PRECISION as u128))
}

/// Volatility state used to compute the per-bin variable fee during a quote.
/// Mirrors the on-chain reference/accumulator update rules.
#[derive(Debug, Clone, Copy)]
pub struct VolatilityTracker {
    pub volatility_reference: u32,
    pub index_reference: i32,
}

impl VolatilityTracker {
    /// Apply the time-decay reference update the program performs at swap
    /// start. `now_unix` should be the current cluster time; if it is older
    /// than `last_update_timestamp` we conservatively skip decay (higher fee ⇒
    /// lower quoted output — errs against the trade, never for it).
    pub fn at_swap_start(
        p: &StaticParameters,
        v: &VariableParameters,
        active_id: i32,
        now_unix: i64,
    ) -> Self {
        let elapsed = now_unix.saturating_sub(v.last_update_timestamp);
        if elapsed >= p.filter_period as i64 {
            let vr = if elapsed < p.decay_period as i64 {
                ((v.volatility_accumulator as u64 * p.reduction_factor as u64) / BASIS_POINT_MAX)
                    as u32
            } else {
                0
            };
            VolatilityTracker {
                volatility_reference: vr,
                index_reference: active_id,
            }
        } else {
            VolatilityTracker {
                volatility_reference: v.volatility_reference,
                index_reference: v.index_reference,
            }
        }
    }

    /// Volatility accumulator when the swap is crossing `bin_id`.
    pub fn accumulator_for_bin(&self, p: &StaticParameters, bin_id: i32) -> u32 {
        let delta = (bin_id as i64 - self.index_reference as i64).unsigned_abs();
        let va = self.volatility_reference as u64 + delta * BASIS_POINT_MAX;
        va.min(p.max_volatility_accumulator as u64) as u32
    }
}

// ─────────────────────────── swap (PROVISIONAL) ───────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DlmmQuoteError {
    /// The traversal needed a bin array we don't hold — refuse to guess.
    InsufficientBinCoverage { missing_array_index: i64 },
    /// Ran past the pool's min/max bin id with input remaining.
    ExhaustedLiquidity,
    /// Zero input or output.
    NoFill,
    /// Integer overflow / bad price data.
    MathOverflow,
}

/// Exact-in DLMM quote by bin traversal.
///
/// * `swap_for_y = true`: token X in → token Y out (price falls, walk DOWN).
/// * `swap_for_y = false`: token Y in → token X out (price rises, walk UP).
///
/// `bin_arrays` maps array index → decoded array; it must cover every bin the
/// traversal touches or the quote fails with `InsufficientBinCoverage` (we
/// never fabricate liquidity for bins we don't hold). `now_unix` drives fee
/// decay (see [`VolatilityTracker::at_swap_start`]).
///
/// PROVISIONAL: traversal + fee application not yet reconciled against
/// `simulateTransaction` (S9). Price math itself is exact.
pub fn dlmm_quote_exact_in(
    pair: &LbPair,
    bin_arrays: &HashMap<i64, BinArray>,
    swap_for_y: bool,
    amount_in: u64,
    now_unix: i64,
) -> Result<u64, DlmmQuoteError> {
    if amount_in == 0 {
        return Err(DlmmQuoteError::NoFill);
    }
    let p = &pair.parameters;
    let vt = VolatilityTracker::at_swap_start(p, &pair.v_parameters, pair.active_id, now_unix);

    let mut remaining: u128 = amount_in as u128;
    let mut total_out: u128 = 0;
    let mut bin_id = pair.active_id;

    loop {
        if bin_id < p.min_bin_id || bin_id > p.max_bin_id {
            return Err(DlmmQuoteError::ExhaustedLiquidity);
        }
        let arr_idx = bin_array_index(bin_id);
        let Some(arr) = bin_arrays.get(&arr_idx) else {
            return Err(DlmmQuoteError::InsufficientBinCoverage {
                missing_array_index: arr_idx,
            });
        };
        let bin = arr.bins[bin_offset_in_array(bin_id)];
        // On-chain stores price per bin; an uninitialised bin has price 0 —
        // recompute from the id instead of trusting a zero.
        let price = if bin.price != 0 {
            bin.price
        } else {
            price_from_id(bin_id, pair.bin_step).ok_or(DlmmQuoteError::MathOverflow)?
        };

        let out_side_liquidity: u128 = if swap_for_y {
            bin.amount_y as u128
        } else {
            bin.amount_x as u128
        };

        if out_side_liquidity > 0 {
            let rate = total_fee_rate(p, pair.bin_step, vt.accumulator_for_bin(p, bin_id));
            // Max input (before fee) this bin absorbs to emit ALL its out-side.
            let max_in_raw = if swap_for_y {
                // x needed to buy all y: ceil(y / price)
                shl_div_ceil(out_side_liquidity, price).ok_or(DlmmQuoteError::MathOverflow)?
            } else {
                // y needed to buy all x: ceil(x * price)
                mul_shr_ceil(price, out_side_liquidity).ok_or(DlmmQuoteError::MathOverflow)?
            };
            let max_fee = compute_fee(max_in_raw, rate).ok_or(DlmmQuoteError::MathOverflow)?;
            let max_in_with_fee = max_in_raw
                .checked_add(max_fee)
                .ok_or(DlmmQuoteError::MathOverflow)?;

            if remaining > max_in_with_fee {
                // Drain the bin completely (strict >, matching Meteora's
                // Bin::swap: at exact equality the partial path is taken).
                remaining -= max_in_with_fee;
                total_out += out_side_liquidity;
            } else {
                // Partial fill inside this bin.
                let fee =
                    compute_fee_from_amount(remaining, rate).ok_or(DlmmQuoteError::MathOverflow)?;
                let into_bin = remaining - fee;
                let out = if swap_for_y {
                    mul_shr_floor(price, into_bin).ok_or(DlmmQuoteError::MathOverflow)?
                } else {
                    shl_div_floor(into_bin, price).ok_or(DlmmQuoteError::MathOverflow)?
                };
                // Never emit more than the bin actually holds.
                total_out += out.min(out_side_liquidity);
                remaining = 0;
            }
        }

        if remaining == 0 {
            break;
        }
        bin_id += if swap_for_y { -1 } else { 1 };
    }

    let out64 = u64::try_from(total_out).map_err(|_| DlmmQuoteError::MathOverflow)?;
    if out64 == 0 {
        return Err(DlmmQuoteError::NoFill);
    }
    Ok(out64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    // Real mainnet fixtures captured 2026-07-12 (docs/meteora-dlmm-layout.md):
    // pair J4cGfY61ZMaBD2niXcfaUD7KsNZiDnjMnJsPJficos8J — pump-token/WSOL,
    // bin_step 15, active_id 643 at capture.
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

    #[test]
    fn decodes_real_lb_pair() {
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
        assert_eq!(
            p.reserve_y,
            Pubkey::from_str("HpR5R42Naputg4zLKXZRykZfjahVZK6WvSYqtcFWAbUo").unwrap()
        );
        assert_eq!(p.bin_step, 15);
        assert_eq!(p.active_id, 643);
        assert_eq!(p.status, 0);
        assert_eq!(p.parameters.base_factor, 10_000);
        assert_eq!(p.parameters.filter_period, 30);
        assert_eq!(p.parameters.decay_period, 600);
        assert_eq!(p.parameters.reduction_factor, 5_000);
        assert_eq!(p.parameters.variable_fee_control, 30_000);
        assert_eq!(p.parameters.max_volatility_accumulator, 350_000);
        assert_eq!(p.parameters.protocol_share, 1_000);
        // Timestamp parsed from the verified offset must be a sane unix time.
        let ts = p.v_parameters.last_update_timestamp;
        assert!((1_577_836_800..4_102_444_800).contains(&ts), "ts={ts}");
    }

    #[test]
    fn decodes_real_bin_array_and_back_pointer() {
        let a = array9();
        assert_eq!(a.index, 9); // bins 630..700 — contains active_id 643
        assert_eq!(
            a.lb_pair,
            Pubkey::from_str("J4cGfY61ZMaBD2niXcfaUD7KsNZiDnjMnJsPJficos8J").unwrap()
        );
        assert_eq!(a.bins.len(), 70);
        // Spot-check real captured liquidity values.
        assert_eq!(a.bins[0].amount_y, 13_189_812_598); // bin 630
        assert_eq!(a.bins[68].amount_x, 3_863_113_477); // bin 698
                                                        // Below active: Y side; above active: X side (price = Y per X).
        assert!(a.bins[bin_offset_in_array(640)].amount_y > 0);
        assert!(a.bins[bin_offset_in_array(660)].amount_x > 0);
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
            assert_eq!(
                price_from_id(id, 15).unwrap(),
                bin.price,
                "bin {id} price mismatch (step 15)"
            );
        }
        let other = decode_bin_array(BIN_ARRAY_OTHER).unwrap();
        for (i, bin) in other.bins.iter().enumerate() {
            if bin.price == 0 {
                continue;
            }
            let id = (other.index * 70) as i32 + i as i32;
            assert_eq!(
                price_from_id(id, 20).unwrap(),
                bin.price,
                "bin {id} price mismatch (step 20)"
            );
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
        // price(-i) ≈ 1/price(i): product within a few ulps of 2^128.
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
        // va = 0 ⇒ no variable fee; cap respected.
        assert_eq!(variable_fee_rate(&p.parameters, p.bin_step, 0), 0);
        assert!(total_fee_rate(&p.parameters, p.bin_step, u32::MAX) <= MAX_FEE_RATE);
    }

    fn arrays() -> HashMap<i64, BinArray> {
        let mut m = HashMap::new();
        m.insert(9, array9());
        m
    }

    #[test]
    fn quote_small_swap_both_directions() {
        let p = pair();
        let now = p.v_parameters.last_update_timestamp + 5;
        // Y in (WSOL) -> X out: walk UP through X-side bins.
        let out_x = dlmm_quote_exact_in(&p, &arrays(), false, 1_000_000_000, now).unwrap();
        assert!(out_x > 0);
        // X in -> Y out (WSOL): walk DOWN through Y-side bins.
        let out_y = dlmm_quote_exact_in(&p, &arrays(), true, 1_000_000_000, now).unwrap();
        assert!(out_y > 0);
    }

    #[test]
    fn quote_never_overestimates_vs_feeless_spot() {
        let p = pair();
        let now = p.v_parameters.last_update_timestamp + 5;
        let amt: u64 = 2_000_000_000;
        let out = dlmm_quote_exact_in(&p, &arrays(), false, amt, now).unwrap() as u128;
        // Upper bound: whole input converted at the ACTIVE bin price with no
        // fee and no traversal to worse bins: x_ub = in << 64 / price(active).
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
        // Total X in array 9 ≈ 249e9; a giant WSOL input must exhaust it and
        // hit the missing array 10 — the quote REFUSES rather than guessing.
        let err = dlmm_quote_exact_in(&p, &arrays(), false, u64::MAX / 4, now).unwrap_err();
        assert_eq!(
            err,
            DlmmQuoteError::InsufficientBinCoverage {
                missing_array_index: 10
            }
        );
        // Same going down: drain all Y and hit array 8.
        let err = dlmm_quote_exact_in(&p, &arrays(), true, u64::MAX / 4, now).unwrap_err();
        assert_eq!(
            err,
            DlmmQuoteError::InsufficientBinCoverage {
                missing_array_index: 8
            }
        );
        // Zero input: NoFill, not a zero-quote success.
        assert_eq!(
            dlmm_quote_exact_in(&p, &arrays(), false, 0, now),
            Err(DlmmQuoteError::NoFill)
        );
    }

    #[test]
    fn higher_volatility_reduces_output() {
        let p = pair();
        let calm = p.v_parameters.last_update_timestamp + 5;
        let out_calm = dlmm_quote_exact_in(&p, &arrays(), false, 1_000_000_000, calm).unwrap();

        // Force max volatility: same pair but with a huge accumulator and a
        // fresh timestamp (no decay).
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
        let mut v = p.parameters;
        v.variable_fee_control = 30_000;
        let mut vparams = p.v_parameters;
        vparams.volatility_accumulator = 100_000;
        vparams.volatility_reference = 100_000;
        vparams.index_reference = p.active_id;

        // Within filter period: references kept (max fee).
        let t0 = VolatilityTracker::at_swap_start(
            &v,
            &vparams,
            p.active_id,
            vparams.last_update_timestamp + 1,
        );
        // After decay period: reference zeroed.
        let t2 = VolatilityTracker::at_swap_start(
            &v,
            &vparams,
            p.active_id,
            vparams.last_update_timestamp + v.decay_period as i64 + 1,
        );
        let va0 = t0.accumulator_for_bin(&v, p.active_id);
        let va2 = t2.accumulator_for_bin(&v, p.active_id);
        assert!(va0 >= va2, "decay must not increase the accumulator");
        assert_eq!(va2, 0);
    }
}
