//! PumpSwap AMM (`pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA`) support:
//! a verified Pool-account parser and an integer constant-product quote.
//!
//! Ground truth (layout, fees, discriminators) is documented and
//! chain-verified in `docs/pump-amm-layout.md`. Reserves are the live SPL
//! balances of the pool's two vaults (like Raydium), NOT stored in the struct.
//!
//! Quote status: the constant-product math is standard fee-on-input and reuses
//! [`crate::math::cpmm_amount_out`] (U256, never overestimates). Whether
//! PumpSwap's protocol/creator fees exactly match a fee-on-input reduction is
//! **PROVISIONAL until the S9 simulation-parity harness reconciles it against a
//! real swap.** Until then this must not be called "exact".

use crate::math::cpmm_amount_out;
use solana_sdk::pubkey::Pubkey;

/// PumpSwap AMM program id (mainnet + devnet).
pub const PUMP_AMM_PROGRAM_ID: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";

/// Anchor account discriminator for `Pool` (sha256("account:Pool")[..8]).
pub const POOL_DISCRIMINATOR: [u8; 8] = [0xf1, 0x9a, 0x6d, 0x04, 0x11, 0xb1, 0x6d, 0xbc];

/// Instruction discriminators (sha256("global:<name>")[..8]).
pub const IX_BUY_DISCRIMINATOR: [u8; 8] = [0x66, 0x06, 0x3d, 0x12, 0x01, 0xda, 0xeb, 0xea];
pub const IX_SELL_DISCRIMINATOR: [u8; 8] = [0x33, 0xe6, 0x85, 0xa4, 0x01, 0x7f, 0x83, 0xad];

/// Verified fee schedule (from on-chain GlobalConfig): lp 20 + protocol 5.
pub const LP_FEE_BPS: u64 = 20;
pub const PROTOCOL_FEE_BPS: u64 = 5;
/// Total fee (bps) for a pool with no coin-creator fee.
pub const BASE_TOTAL_FEE_BPS: u64 = LP_FEE_BPS + PROTOCOL_FEE_BPS;

/// Minimum size of a Pool account (through `lp_supply`). Real accounts observed
/// at 301 bytes; we require at least the fields we parse.
const POOL_MIN_LEN: usize = 243; // disc..=coin_creator

/// Decoded PumpSwap Pool account. Reserves are fetched separately from the
/// vault token accounts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PumpAmmPool {
    pub bump: u8,
    pub index: u16,
    pub creator: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub lp_mint: Pubkey,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
    pub lp_supply: u64,
    /// All-zero ⇒ no coin-creator fee. PROVISIONAL offset (see docs).
    pub coin_creator: Pubkey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PumpDecodeError {
    TooShort { len: usize, need: usize },
    BadDiscriminator,
}

impl PumpAmmPool {
    /// Total swap fee (bps): 25 base, plus the creator fee only when the pool
    /// actually has a coin-creator set. `creator_fee_bps` comes from config /
    /// GlobalConfig (it is dynamic on-chain); callers pass the current value.
    pub fn total_fee_bps(&self, creator_fee_bps: u64) -> u64 {
        if self.coin_creator == Pubkey::default() {
            BASE_TOTAL_FEE_BPS
        } else {
            BASE_TOTAL_FEE_BPS + creator_fee_bps
        }
    }

    /// True when WSOL is one side of the pair (our WSOL-anchored strategy).
    pub fn wsol_side(&self, wsol: &Pubkey) -> Option<WsolSide> {
        if &self.base_mint == wsol {
            Some(WsolSide::Base)
        } else if &self.quote_mint == wsol {
            Some(WsolSide::Quote)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsolSide {
    Base,
    Quote,
}

fn read_pubkey(data: &[u8], off: usize) -> Pubkey {
    let mut b = [0u8; 32];
    b.copy_from_slice(&data[off..off + 32]);
    Pubkey::new_from_array(b)
}

/// Decode a PumpSwap Pool account. Verifies the Anchor discriminator and length
/// — a wrong-owner / wrong-type account is rejected, never guessed.
pub fn decode_pump_pool(data: &[u8]) -> Result<PumpAmmPool, PumpDecodeError> {
    if data.len() < POOL_MIN_LEN {
        return Err(PumpDecodeError::TooShort {
            len: data.len(),
            need: POOL_MIN_LEN,
        });
    }
    if data[0..8] != POOL_DISCRIMINATOR {
        return Err(PumpDecodeError::BadDiscriminator);
    }
    Ok(PumpAmmPool {
        bump: data[8],
        index: u16::from_le_bytes([data[9], data[10]]),
        creator: read_pubkey(data, 11),
        base_mint: read_pubkey(data, 43),
        quote_mint: read_pubkey(data, 75),
        lp_mint: read_pubkey(data, 107),
        base_vault: read_pubkey(data, 139),
        quote_vault: read_pubkey(data, 171),
        lp_supply: u64::from_le_bytes(data[203..211].try_into().unwrap()),
        coin_creator: read_pubkey(data, 211),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PumpQuoteError {
    /// `input_mint` is neither side of the pool.
    WrongMint,
    /// A reserve is zero (pool drained / not initialised) — no honest quote.
    EmptyReserves,
    /// The trade produces no output at this size.
    NoFill,
}

/// Integer constant-product quote for one PumpSwap leg.
///
/// Reserves are the live vault balances (`base_reserve` = balance of
/// `base_vault`, `quote_reserve` = balance of `quote_vault`). Direction is
/// derived from `input_mint`. Fee is applied on the input via
/// [`cpmm_amount_out`] (floor rounding, never overestimates).
///
/// PROVISIONAL: see module docs — not validated against `simulateTransaction`
/// yet, so callers must not treat the result as exact.
pub fn pump_quote(
    pool: &PumpAmmPool,
    input_mint: &Pubkey,
    amount_in: u64,
    base_reserve: u64,
    quote_reserve: u64,
    total_fee_bps: u64,
) -> Result<u64, PumpQuoteError> {
    if base_reserve == 0 || quote_reserve == 0 {
        return Err(PumpQuoteError::EmptyReserves);
    }
    let (reserve_in, reserve_out) = if input_mint == &pool.base_mint {
        (base_reserve, quote_reserve)
    } else if input_mint == &pool.quote_mint {
        (quote_reserve, base_reserve)
    } else {
        return Err(PumpQuoteError::WrongMint);
    };
    // fee-on-input CPMM: numerator = fee bps, denominator = 10_000.
    let out = cpmm_amount_out(amount_in, reserve_in, reserve_out, total_fee_bps, 10_000);
    if out == 0 {
        return Err(PumpQuoteError::NoFill);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine};
    use std::str::FromStr;

    // Real mainnet Pool `HM4BKerYkMLoPjwMv2CkHjkuac3Ajj5hGzCsd19vW84J`
    // (301 bytes) captured 2026-07-12. This is a real on-chain fixture, not a
    // fabricated one — see docs/pump-amm-layout.md.
    const REAL_POOL_B64: &str = "8ZptBBGxbbz/AAC28HeTnUlU91cpK6+AiuLYVpzvuKZUKBFl1ND9Q/fyQgabiFf+q4GE+2h/Y0YYwDXaxDncGus7VZig8AAAAAABHGw/3rfGk8Zm60BAKRPZ2Ln75Wn9gHuiwe7P685ZJmjZUZAXdBUpx5jCiHBAr0wSpBsnvPSf4wk6jWmcn6sdzvAYQt34XVOndJ7EHh8s+J36NuxqgSy+Doqmo6sNND6xAARqVwJQR/asuQJnMvc9EgCUm6TFpXtAVikE18eUs6RJoNVyhQMAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==";

    fn decode_real() -> PumpAmmPool {
        let raw = STANDARD.decode(REAL_POOL_B64).unwrap();
        assert_eq!(raw.len(), 301, "fixture must be the real 301-byte account");
        decode_pump_pool(&raw).unwrap()
    }

    #[test]
    fn decodes_real_mainnet_pool() {
        let p = decode_real();
        // Verified against chain: base=WSOL, quote=memecoin, vault mints match.
        assert_eq!(
            p.base_mint,
            Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap()
        );
        assert_eq!(
            p.quote_mint,
            Pubkey::from_str("2ux9p7iiPQSYWdtMW4iWQ9SC2SmNkrvpQrAfQHtBtPwV").unwrap()
        );
        assert_eq!(
            p.base_vault,
            Pubkey::from_str("HAEJeJDqctvbB8nx9hQciZN8nqjHMbPKtR6W3jwZurVN").unwrap()
        );
        assert_eq!(
            p.quote_vault,
            Pubkey::from_str("14uVQ5aY5sv26ufFfWYqYqvgH4h3pSZYiFKgz5rUU2P").unwrap()
        );
        assert_eq!(p.index, 0);
        assert_eq!(p.bump, 255);
        assert_eq!(p.lp_supply, 3_871_692_136_521);
        // No coin-creator on this pool ⇒ base 25 bps fee.
        assert_eq!(p.coin_creator, Pubkey::default());
        assert_eq!(p.total_fee_bps(30), BASE_TOTAL_FEE_BPS);
    }

    #[test]
    fn wsol_side_detected_from_mints_not_assumed() {
        let p = decode_real();
        let wsol = Pubkey::from_str("So11111111111111111111111111111111111111112").unwrap();
        assert_eq!(p.wsol_side(&wsol), Some(WsolSide::Base));
        assert_eq!(p.wsol_side(&Pubkey::default()), None);
    }

    #[test]
    fn rejects_wrong_discriminator_and_short() {
        let mut raw = STANDARD.decode(REAL_POOL_B64).unwrap();
        raw[0] ^= 0xff;
        assert_eq!(
            decode_pump_pool(&raw),
            Err(PumpDecodeError::BadDiscriminator)
        );
        assert!(matches!(
            decode_pump_pool(&raw[..100]),
            Err(PumpDecodeError::TooShort { .. })
        ));
    }

    #[test]
    fn quote_both_directions_match_cpmm() {
        let p = decode_real();
        let (rb, rq) = (250_722_271_472u64, 66_578_440_584_105u64); // real reserves
                                                                    // base (WSOL) in -> quote out
        let out_bq = pump_quote(&p, &p.base_mint, 1_000_000_000, rb, rq, 25).unwrap();
        assert_eq!(out_bq, cpmm_amount_out(1_000_000_000, rb, rq, 25, 10_000));
        // quote in -> base (WSOL) out
        let out_qb = pump_quote(&p, &p.quote_mint, 1_000_000_000, rb, rq, 25).unwrap();
        assert_eq!(out_qb, cpmm_amount_out(1_000_000_000, rq, rb, 25, 10_000));
        assert!(out_bq > 0 && out_qb > 0);
    }

    #[test]
    fn quote_rejects_wrong_mint_and_empty_reserves() {
        let p = decode_real();
        assert_eq!(
            pump_quote(&p, &Pubkey::default(), 1_000, 10, 10, 25),
            Err(PumpQuoteError::WrongMint)
        );
        assert_eq!(
            pump_quote(&p, &p.base_mint, 1_000, 0, 10, 25),
            Err(PumpQuoteError::EmptyReserves)
        );
    }

    #[test]
    fn quote_never_overestimates_output() {
        let p = decode_real();
        let (rb, rq) = (250_722_271_472u64, 66_578_440_584_105u64);
        let amt = 5_000_000_000u64;
        let out = pump_quote(&p, &p.base_mint, amt, rb, rq, 25).unwrap();
        // Upper bound: zero-fee, zero-slippage spot = amt * rq / rb.
        let ideal = (amt as u128 * rq as u128 / rb as u128) as u64;
        assert!(out < ideal, "fee+slippage must reduce output below spot");
    }
}
