//! PumpSwap AMM (`pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA`) support:
//! a verified Pool-account parser and an integer constant-product quote.
//!
//! Ground truth (layout, fees, discriminators) is documented and
//! chain-verified in `docs/pump-amm-layout.md`. Reserves are the live SPL
//! balances of the pool's two vaults (like Raydium), NOT stored in the struct.
//!
//! Quote status: **EXACT for creator-less pools** — the two-direction fee
//! formulas were reverse-engineered from and verified against 29/29 real
//! executed mainnet swaps (byte-exact, balance-delta method), with 8 of those
//! embedded below as regression vectors. Pools with a non-zero `coin_creator`
//! are REFUSED (`UnverifiedFeeSchedule`) until their schedule gets the same
//! treatment.

use crate::math::cpmm_amount_out;
use solana_sdk::pubkey::Pubkey;

/// PumpSwap AMM program id (mainnet + devnet).
pub const PUMP_AMM_PROGRAM_ID: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";

/// Anchor account discriminator for `Pool` (sha256("account:Pool")[..8]).
pub const POOL_DISCRIMINATOR: [u8; 8] = [0xf1, 0x9a, 0x6d, 0x04, 0x11, 0xb1, 0x6d, 0xbc];

/// Instruction discriminators (sha256("global:<name>")[..8]).
pub const IX_BUY_DISCRIMINATOR: [u8; 8] = [0x66, 0x06, 0x3d, 0x12, 0x01, 0xda, 0xeb, 0xea];
pub const IX_SELL_DISCRIMINATOR: [u8; 8] = [0x33, 0xe6, 0x85, 0xa4, 0x01, 0x7f, 0x83, 0xad];

/// Fee schedule, EMPIRICALLY PINNED against 29/29 real mainnet swaps on a
/// creator-less pool (see docs/pump-amm-layout.md, parity study 2026-07-12).
/// PumpSwap fees are always charged on the QUOTE side:
///
/// * base in → quote out: `out = gross − ceil(gross·25/10⁴)` where
///   `gross = floor(x·Rq/(Rb+x))` — 25 bps off the quote OUTPUT.
/// * quote in → base out: `C = floor(U·10⁴/(10⁴+30))`, `out = floor(C·Rb/(Rq+C))`
///   — a 30 bps ON-TOP markup divided out of the quote INPUT (25 bps retained
///   in the vault, 5 bps transferred out as 2 × 2.5 bps).
///
/// These constants are verified ONLY for pools with `coin_creator == default`.
/// Pools with a creator fee are REFUSED (`UnverifiedFeeSchedule`) until their
/// schedule is parity-verified the same way.
pub const QUOTE_OUT_FEE_BPS: u64 = 25;
pub const QUOTE_IN_MARKUP_BPS: u64 = 30;

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
    /// True when the empirically-verified fee schedule applies to this pool
    /// (no coin-creator fee). Pools with a creator are refused until their
    /// schedule is parity-verified — never guessed.
    pub fn fee_schedule_verified(&self) -> bool {
        self.coin_creator == Pubkey::default()
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
    /// Pool has a coin-creator fee whose schedule we have NOT parity-verified.
    /// Refuse rather than guess.
    UnverifiedFeeSchedule,
}

/// Exact-in PumpSwap quote — the EXACT integer formulas reverse-engineered
/// from real executed swaps (29/29 byte-exact; see module fee docs).
///
/// Reserves are the live vault balances pre-swap. Direction is derived from
/// `input_mint` relative to the pool's base/quote mints (WSOL can be EITHER
/// side — never assume).
pub fn pump_quote(
    pool: &PumpAmmPool,
    input_mint: &Pubkey,
    amount_in: u64,
    base_reserve: u64,
    quote_reserve: u64,
) -> Result<u64, PumpQuoteError> {
    if !pool.fee_schedule_verified() {
        return Err(PumpQuoteError::UnverifiedFeeSchedule);
    }
    if base_reserve == 0 || quote_reserve == 0 {
        return Err(PumpQuoteError::EmptyReserves);
    }
    if amount_in == 0 {
        return Err(PumpQuoteError::NoFill);
    }
    let out = if input_mint == &pool.base_mint {
        // base in → quote out: fee-less CPMM, then 25 bps (ceil) OFF the output.
        let gross = cpmm_amount_out(amount_in, base_reserve, quote_reserve, 0, 10_000);
        let fee = (gross as u128 * QUOTE_OUT_FEE_BPS as u128).div_ceil(10_000) as u64;
        gross.saturating_sub(fee)
    } else if input_mint == &pool.quote_mint {
        // quote in → base out: divide the 30 bps on-top markup out of the
        // input (floor), then fee-less CPMM (floor).
        let effective =
            ((amount_in as u128 * 10_000) / (10_000 + QUOTE_IN_MARKUP_BPS as u128)) as u64;
        cpmm_amount_out(effective, quote_reserve, base_reserve, 0, 10_000)
    } else {
        return Err(PumpQuoteError::WrongMint);
    };
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
        assert!(p.fee_schedule_verified());
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

    /// REAL executed mainnet swaps on the fixture pool (2026-07-12), extracted
    /// via the balance-delta method: pre-swap vault reserves, user input,
    /// actual user output. The quote must reproduce every output EXACTLY.
    /// Direction A: base(WSOL) in → quote out.
    const REAL_SWAPS_BASE_IN: &[(u64, u64, u64, u64)] = &[
        (
            501_037_669_936,
            36_137_094_094_035,
            924_918_154,
            66_419_879_719,
        ), // jwCm9JJR8x2XHZg6
        (
            500_974_990_791,
            36_140_885_655_287,
            2_058_411_074,
            147_518_667_713,
        ), // 5okh17ZAZ6Vx7HCf
        (
            498_476_080_737,
            36_321_608_291_587,
            2_498_910_054,
            180_722_636_300,
        ), // 5zsKkAg1semv75G7
        (
            495_067_428_118,
            36_570_986_090_308,
            3_613_243_728,
            264_316_513_239,
        ), // 2yMUVGHiZbrRNDBx
    ];
    /// Direction B: quote in → base(WSOL) out (input = user-paid amount).
    const REAL_SWAPS_QUOTE_IN: &[(u64, u64, u64, u64)] = &[
        (
            503_033_401_865,
            35_993_366_987_574,
            143_798_790_804,
            1_995_731_929,
        ), // 3kkoznSysy2yJPWk
        (
            498_680_671_846,
            36_306_669_577_069,
            14_946_165_249,
            204_591_109,
        ), // 3hJNWmbWLrDNYeo1
        (
            497_566_348_172,
            36_386_859_309_007,
            184_218_615_108,
            2_498_920_054,
        ), // 5K1x88FtnKYbWFpU
        (
            500_118_171_012,
            36_200_350_223_881,
            264_233_946_107,
            3_613_253_728,
        ), // 4P3hju5oZ1pW1W2p
    ];

    #[test]
    fn quote_matches_real_swaps_exactly_base_in() {
        let p = decode_real();
        for &(rb, rq, x, expected) in REAL_SWAPS_BASE_IN {
            let out = pump_quote(&p, &p.base_mint, x, rb, rq).unwrap();
            assert_eq!(out, expected, "base-in swap must be byte-exact");
        }
    }

    #[test]
    fn quote_matches_real_swaps_exactly_quote_in() {
        let p = decode_real();
        for &(rb, rq, u, expected) in REAL_SWAPS_QUOTE_IN {
            let out = pump_quote(&p, &p.quote_mint, u, rb, rq).unwrap();
            assert_eq!(out, expected, "quote-in swap must be byte-exact");
        }
    }

    #[test]
    fn quote_rejects_wrong_mint_empty_reserves_and_unverified_creator() {
        let p = decode_real();
        assert_eq!(
            pump_quote(&p, &Pubkey::default(), 1_000, 10, 10),
            Err(PumpQuoteError::WrongMint)
        );
        assert_eq!(
            pump_quote(&p, &p.base_mint, 1_000, 0, 10),
            Err(PumpQuoteError::EmptyReserves)
        );
        // A pool WITH a creator fee must refuse until parity-verified.
        let mut with_creator = p.clone();
        with_creator.coin_creator = Pubkey::new_unique();
        assert_eq!(
            pump_quote(&with_creator, &with_creator.base_mint, 1_000, 10, 10),
            Err(PumpQuoteError::UnverifiedFeeSchedule)
        );
    }

    #[test]
    fn quote_never_overestimates_output() {
        let p = decode_real();
        let (rb, rq) = (501_037_669_936u64, 36_137_094_094_035u64);
        // Base in: below feeless spot.
        let amt = 5_000_000_000u64;
        let out = pump_quote(&p, &p.base_mint, amt, rb, rq).unwrap();
        let ideal = (amt as u128 * rq as u128 / rb as u128) as u64;
        assert!(out < ideal, "fee+slippage must reduce output below spot");
        // Quote in: below feeless spot too.
        let amt_q = 100_000_000_000u64;
        let out_b = pump_quote(&p, &p.quote_mint, amt_q, rb, rq).unwrap();
        let ideal_b = (amt_q as u128 * rb as u128 / rq as u128) as u64;
        assert!(out_b < ideal_b);
    }
}
