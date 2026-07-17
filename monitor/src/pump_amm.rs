//! PumpSwap AMM (`pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA`) support:
//! a verified Pool-account parser and an integer constant-product quote.
//!
//! Ground truth (layout, fees, discriminators) is documented and
//! chain-verified in `docs/pump-amm-layout.md`. Reserves are the live SPL
//! balances of the pool's two vaults (like Raydium), NOT stored in the struct.
//!
//! Quote status (verified against real mainnet swap EVENTS — the on-chain
//! log carries explicit fee fields, so this is ground truth, not inference):
//! - **SELL (base in → quote out): EXACT for all pools** — 17/17 real swaps,
//!   creator and creator-less. Returns what the TRADER receives
//!   (`gross − lp − protocol − creator`), not the vault delta.
//! - **BUY (quote in → base out): EXACT for creator-less pools**; creator
//!   pools would overestimate (fee-inversion rounding unresolved) so they are
//!   REFUSED (`CreatorBuyUnverified`) — a creator pool is still usable as the
//!   SELL leg.
//!
//! Earlier note: an initial "balance-delta" study measured the quote-vault
//! delta and mistook it for the trader's receipt, over-counting by the
//! protocol fee (~5 bps). The event-based model here corrects that.

use crate::math::cpmm_amount_out;
use solana_sdk::pubkey::Pubkey;

/// PumpSwap AMM program id (mainnet + devnet).
pub const PUMP_AMM_PROGRAM_ID: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";

/// Anchor account discriminator for `Pool` (sha256("account:Pool")[..8]).
pub const POOL_DISCRIMINATOR: [u8; 8] = [0xf1, 0x9a, 0x6d, 0x04, 0x11, 0xb1, 0x6d, 0xbc];

/// Instruction discriminators (sha256("global:<name>")[..8]).
pub const IX_BUY_DISCRIMINATOR: [u8; 8] = [0x66, 0x06, 0x3d, 0x12, 0x01, 0xda, 0xeb, 0xea];
pub const IX_SELL_DISCRIMINATOR: [u8; 8] = [0x33, 0xe6, 0x85, 0xa4, 0x01, 0x7f, 0x83, 0xad];

/// Fee schedule, EMPIRICALLY PINNED against real mainnet swap EVENTS across
/// creator and creator-less pools (see docs/pump-amm-layout.md).
///
/// Total fee is ALWAYS 30 bps; only the split shifts by whether the pool has a
/// coin creator. Each component is charged on the QUOTE token and rounded UP
/// independently (summing three separate ceils, NOT one 30-bps ceil):
///
/// | pool          | lp | protocol | creator |
/// |---------------|----|----------|---------|
/// | no creator    | 25 |    5     |    0    |
/// | has creator   | 20 |    5     |    5    |
///
/// * SELL (base in → quote out): `g = floor(x·Rq/(Rb+x))`,
///   `out = g − Σⱼ ceil(g·bpsⱼ/10⁴)`. **17/17 real swaps exact (both types).**
/// * BUY (quote in → base out): find max `C` with
///   `C + Σⱼ ceil(C·bpsⱼ/10⁴) ≤ U`, then `out = floor(C·Rb/(Rq+C))`.
///   **Exact for creator-less pools; creator pools overestimate by a few
///   units (unresolved), so creator-pool BUY is REFUSED, never shipped.**
pub const PROTOCOL_FEE_BPS: u64 = 5;
pub const NO_CREATOR_LP_BPS: u64 = 25;
pub const CREATOR_LP_BPS: u64 = 20;
pub const CREATOR_FEE_BPS: u64 = 5;

/// The three fee components (lp, protocol, creator) in bps for a pool.
///
/// NOTE (S13C slice 6B): this LEGACY split is correct only for the top
/// market-cap tier (creator=5 → 30 bps total). Current Pump pools use the
/// DYNAMIC fee-v2 schedule read from the fee-program config — see
/// [`crate::pump_feev2`] and [`sell_quote_with_fee_split`]. Do not use this
/// legacy split to quote a fee-v2 pool; it under-charges the fee.
pub fn fee_split(has_creator: bool) -> [u64; 3] {
    if has_creator {
        [CREATOR_LP_BPS, PROTOCOL_FEE_BPS, CREATOR_FEE_BPS]
    } else {
        [NO_CREATOR_LP_BPS, PROTOCOL_FEE_BPS, 0]
    }
}

/// SELL quote (base in → quote out) with an EXPLICIT fee split `[lp, protocol,
/// creator]` in bps — the caller supplies the current fee-v2 tier (from
/// [`crate::pump_feev2`]). Returns the net output and the total DEX fee (quote
/// units). The fee-less CPMM gross is identical to the legacy path; only the fee
/// rate is caller-provided. This is the fee-v2-correct SELL quote.
pub fn sell_quote_with_fee_split(
    amount_in: u64,
    base_reserve: u64,
    quote_reserve: u64,
    fee_split_bps: [u64; 3],
) -> Result<PumpQuoteDetail, PumpQuoteError> {
    if base_reserve == 0 || quote_reserve == 0 {
        return Err(PumpQuoteError::EmptyReserves);
    }
    if amount_in == 0 {
        return Err(PumpQuoteError::NoFill);
    }
    let gross = crate::math::cpmm_amount_out(amount_in, base_reserve, quote_reserve, 0, 10_000);
    let fee = total_fee(gross, fee_split_bps);
    let out = gross.saturating_sub(fee);
    if out == 0 {
        return Err(PumpQuoteError::NoFill);
    }
    Ok(PumpQuoteDetail { out, fee })
}

fn ceil_div(a: u128, b: u128) -> u128 {
    a.div_ceil(b)
}

/// Sum of the independently-ceiled fee components on `amount` (quote units).
fn total_fee(amount: u64, split: [u64; 3]) -> u64 {
    split
        .iter()
        .map(|&bps| ceil_div(amount as u128 * bps as u128, 10_000) as u64)
        .sum()
}

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
    /// Whether the pool charges a coin-creator fee (affects the fee split and,
    /// for now, whether BUY is exact).
    pub fn has_creator(&self) -> bool {
        self.coin_creator != Pubkey::default()
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
    /// BUY (quote in → base out) on a coin-creator pool: our inversion
    /// overestimates by a few units, which would fabricate profit. Refused
    /// until the exact on-chain rounding is pinned. Such a pool can still be
    /// used as the SELL leg (base in → quote out), which IS exact.
    CreatorBuyUnverified,
    /// The dynamic fee-v2 rate could not be resolved from the snapshot — the
    /// quote is REFUSED (never an optimistic 30 bps fallback). See
    /// [`crate::pump_feev2::FeeV2Error`].
    FeeV2(crate::pump_feev2::FeeV2Error),
}

/// The quote-token input `C` that actually enters the pool for a BUY of
/// user-paid `u_in`: the largest `C` with `C + Σ ceil(C·bpsⱼ/10⁴) ≤ u_in`.
/// Matches the on-chain `buy_exact_quote_in` fee inversion exactly.
fn effective_buy_input(u_in: u64, split: [u64; 3]) -> u64 {
    let total: u64 = split.iter().sum();
    let fits = |c: u64| c as u128 + total_fee(c, split) as u128 <= u_in as u128;
    // Start from the closed-form estimate and correct the ±1 ceil boundary.
    let mut c = ((u_in as u128 * 10_000) / (10_000 + total as u128)) as u64;
    while c > 0 && !fits(c) {
        c -= 1;
    }
    while fits(c + 1) {
        c += 1;
    }
    c
}

/// Exact-in PumpSwap quote from real live vault reserves. Direction is derived
/// from `input_mint` vs the pool's base/quote mints (WSOL can be EITHER side —
/// never assume).
///
/// * base in → quote out (SELL): exact for all pools (17/17 real swaps).
/// * quote in → base out (BUY): exact for creator-less pools; creator pools
///   are refused (`CreatorBuyUnverified`) to preserve never-overestimate.
pub fn pump_quote(
    pool: &PumpAmmPool,
    input_mint: &Pubkey,
    amount_in: u64,
    base_reserve: u64,
    quote_reserve: u64,
) -> Result<u64, PumpQuoteError> {
    pump_quote_detailed(pool, input_mint, amount_in, base_reserve, quote_reserve).map(|q| q.out)
}

/// A quote plus its fee attribution, for reporting. `fee` is the total DEX fee
/// charged on this leg, always denominated in the QUOTE token (PumpSwap charges
/// on the quote side in both directions).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PumpQuoteDetail {
    pub out: u64,
    pub fee: u64,
}

/// Like [`pump_quote`] but also returns the DEX fee (in quote-token units).
pub fn pump_quote_detailed(
    pool: &PumpAmmPool,
    input_mint: &Pubkey,
    amount_in: u64,
    base_reserve: u64,
    quote_reserve: u64,
) -> Result<PumpQuoteDetail, PumpQuoteError> {
    if base_reserve == 0 || quote_reserve == 0 {
        return Err(PumpQuoteError::EmptyReserves);
    }
    if amount_in == 0 {
        return Err(PumpQuoteError::NoFill);
    }
    let split = fee_split(pool.has_creator());
    let (out, fee) = if input_mint == &pool.base_mint {
        // SELL: fee-less CPMM gross, then subtract independently-ceiled fees.
        let gross = cpmm_amount_out(amount_in, base_reserve, quote_reserve, 0, 10_000);
        let fee = total_fee(gross, split);
        (gross.saturating_sub(fee), fee)
    } else if input_mint == &pool.quote_mint {
        // BUY: exact inversion of the input fee, then fee-less CPMM.
        if pool.has_creator() {
            return Err(PumpQuoteError::CreatorBuyUnverified);
        }
        let effective = effective_buy_input(amount_in, split);
        let fee = amount_in.saturating_sub(effective); // on-top fee, quote units
        (
            cpmm_amount_out(effective, quote_reserve, base_reserve, 0, 10_000),
            fee,
        )
    } else {
        return Err(PumpQuoteError::WrongMint);
    };
    if out == 0 {
        return Err(PumpQuoteError::NoFill);
    }
    Ok(PumpQuoteDetail { out, fee })
}

/// The fee-v2 rate + provenance selected for a quote (for recording).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PumpFeeV2Detail {
    pub market_cap: u128,
    pub tier_index: usize,
    pub lp_bps: u64,
    pub protocol_bps: u64,
    pub creator_bps: u64,
    pub total_bps: u64,
}

/// Resolve the dynamic fee-v2 tier for a pool from a single-slot snapshot
/// (reserves + base-mint supply + decoded fee config). Typed error, never a
/// fallback rate.
pub fn resolve_fee_v2(
    base_reserve: u64,
    quote_reserve: u64,
    base_mint_supply: u64,
    fee_config: &crate::pump_feev2::FeeConfig,
) -> Result<PumpFeeV2Detail, PumpQuoteError> {
    let mc = crate::pump_feev2::market_cap(base_mint_supply, base_reserve, quote_reserve)
        .map_err(PumpQuoteError::FeeV2)?;
    let idx = fee_config
        .tiers
        .iter()
        .rposition(|t| (t.market_cap_threshold as u128) <= mc)
        .unwrap_or(0);
    let t = &fee_config.tiers[idx];
    Ok(PumpFeeV2Detail {
        market_cap: mc,
        tier_index: idx,
        lp_bps: t.lp_bps,
        protocol_bps: t.protocol_bps,
        creator_bps: t.creator_bps,
        total_bps: t.total_bps(),
    })
}

/// PumpSwap quote using the DYNAMIC fee-v2 model (S13C slice 6C). The fee tier
/// is computed from the SAME single-slot snapshot (pre-trade market cap —
/// proven by simulation to match on-chain; identical across sizes in a
/// snapshot). SELL is exact; BUY on a fee-bearing pool is refused (creator bps
/// are always > 0 under fee-v2, and creator-pool BUY inversion is unverified).
/// NEVER falls back to the legacy 30 bps.
pub fn pump_quote_detailed_v2(
    pool: &PumpAmmPool,
    input_mint: &Pubkey,
    amount_in: u64,
    base_reserve: u64,
    quote_reserve: u64,
    base_mint_supply: u64,
    fee_config: &crate::pump_feev2::FeeConfig,
) -> Result<PumpQuoteDetail, PumpQuoteError> {
    if base_reserve == 0 || quote_reserve == 0 {
        return Err(PumpQuoteError::EmptyReserves);
    }
    if amount_in == 0 {
        return Err(PumpQuoteError::NoFill);
    }
    let fee_v2 = resolve_fee_v2(base_reserve, quote_reserve, base_mint_supply, fee_config)?;
    let split = [fee_v2.lp_bps, fee_v2.protocol_bps, fee_v2.creator_bps];
    if input_mint == &pool.base_mint {
        // SELL (base in → quote out): exact.
        sell_quote_with_fee_split(amount_in, base_reserve, quote_reserve, split)
    } else if input_mint == &pool.quote_mint {
        // BUY: creator bps > 0 under fee-v2 → inversion unverified, refuse.
        Err(PumpQuoteError::CreatorBuyUnverified)
    } else {
        Err(PumpQuoteError::WrongMint)
    }
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
        assert!(!p.has_creator());
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

    /// REAL creator-less swaps on the fixture pool. `out` is the amount the
    /// TRADER receives (event field f104 = gross − lp − proto), NOT the vault
    /// delta (which also carries the protocol fee away to a third party). The
    /// first row's out is event-confirmed on jwCm9JJR (66_386_586_546); the
    /// rest use the same event-validated formula on their real reserves.
    /// Direction A: base(WSOL) in → quote out.
    const REAL_SWAPS_BASE_IN: &[(u64, u64, u64, u64)] = &[
        (
            501_037_669_936,
            36_137_094_094_035,
            924_918_154,
            66_386_586_546,
        ), // jwCm9JJR
        (
            500_974_990_791,
            36_140_885_655_287,
            2_058_411_074,
            147_444_723_518,
        ), // 5okh17ZA
        (
            498_476_080_737,
            36_321_608_291_587,
            2_498_910_054,
            180_632_048_512,
        ), // 5zsKkAg1
        (
            495_067_428_118,
            36_570_986_090_308,
            3_613_243_728,
            264_184_023_758,
        ), // 2yMUVGHi
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

    // Real creator-pool swaps (FFcYgSSg / 4w2cysot, coin_creator set):
    // SELL (base in → quote out): (Rb, Rq, x, out).
    const REAL_CREATOR_SELLS: &[(u64, u64, u64, u64)] = &[
        (
            26_041_566_079_395,
            18_076_600_971_954,
            331_078_229,
            229_123_655,
        ), // 66pAEPX2
        (
            26_051_170_950_238,
            18_069_922_681_817,
            93_290_000,
            64_514_558,
        ), // 4A22ggkV
        (
            26_050_218_587_527,
            18_070_581_973_829,
            952_362_711,
            658_631_398,
        ), // Nq832d3q
        (
            26_047_288_410_402,
            18_072_610_749_077,
            2_930_177_125,
            2_026_742_406,
        ), // 67JffNr9
    ];
    // Real creator-pool BUY (quote in → base out): (Rb, Rq, U, out).
    const REAL_CREATOR_BUYS: &[(u64, u64, u64, u64)] = &[
        (
            29_201_030_158_845,
            4_127_335_851_343,
            6_686_600_000,
            47_090_342_993,
        ), // 22otDMKy
        (
            29_209_285_190_721,
            4_126_167_065_044,
            1_169_952_753,
            8_255_031_876,
        ), // 5tkZPG1k
        (
            29_216_298_642_157,
            4_125_174_586_861,
            50_561_831,
            357_025_623,
        ), // 2j1YdKJ5
    ];

    /// A synthetic pool with a set coin creator (real fee split 20/5/5).
    fn creator_pool() -> PumpAmmPool {
        let mut p = decode_real();
        p.coin_creator = Pubkey::new_unique();
        p
    }

    #[test]
    fn sell_matches_real_swaps_exactly_creatorless_and_creator() {
        let p = decode_real();
        for &(rb, rq, x, expected) in REAL_SWAPS_BASE_IN {
            assert_eq!(pump_quote(&p, &p.base_mint, x, rb, rq).unwrap(), expected);
        }
        let c = creator_pool();
        for &(rb, rq, x, expected) in REAL_CREATOR_SELLS {
            assert_eq!(
                pump_quote(&c, &c.base_mint, x, rb, rq).unwrap(),
                expected,
                "creator-pool SELL must be byte-exact"
            );
        }
    }

    #[test]
    fn buy_matches_real_swaps_exactly_creatorless() {
        let p = decode_real();
        for &(rb, rq, u, expected) in REAL_SWAPS_QUOTE_IN {
            assert_eq!(pump_quote(&p, &p.quote_mint, u, rb, rq).unwrap(), expected);
        }
    }

    #[test]
    fn creator_buy_is_refused_not_overestimated() {
        // We KNOW the exact answers; our inversion would overestimate them, so
        // the quote must refuse rather than fabricate profit.
        let c = creator_pool();
        for &(rb, rq, u, actual) in REAL_CREATOR_BUYS {
            assert_eq!(
                pump_quote(&c, &c.quote_mint, u, rb, rq),
                Err(PumpQuoteError::CreatorBuyUnverified)
            );
            // Sanity: the naive inversion really is ≥ the true output (unsafe).
            let naive = cpmm_amount_out(effective_buy_input(u, fee_split(true)), rq, rb, 0, 10_000);
            assert!(naive >= actual, "inversion must be the overestimating side");
        }
    }

    #[test]
    fn quote_rejects_wrong_mint_and_empty_reserves() {
        let p = decode_real();
        assert_eq!(
            pump_quote(&p, &Pubkey::default(), 1_000, 10, 10),
            Err(PumpQuoteError::WrongMint)
        );
        assert_eq!(
            pump_quote(&p, &p.base_mint, 1_000, 0, 10),
            Err(PumpQuoteError::EmptyReserves)
        );
    }

    #[test]
    fn effective_buy_input_is_max_feasible() {
        // The inversion must be the LARGEST C whose grossed-up cost fits u_in.
        let split = fee_split(true);
        for u in [10_000u64, 1_000_000, 6_686_600_000] {
            let c = effective_buy_input(u, split);
            let cost = |x: u64| x as u128 + total_fee(x, split) as u128;
            assert!(cost(c) <= u as u128);
            assert!(cost(c + 1) > u as u128);
        }
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

    // ── Slice 6C: dynamic fee-v2 integration regression tests. ──
    use crate::pump_feev2::{decode_fee_config, FeeConfig};

    const FEE_CFG_BYTES: &[u8] = include_bytes!("../fixtures/pump/fee_config_5PHirr8.bin");

    fn feev2_pool(base_mint: Pubkey, quote_mint: Pubkey) -> PumpAmmPool {
        PumpAmmPool {
            bump: 0,
            index: 0,
            creator: Pubkey::default(),
            base_mint,
            quote_mint,
            lp_mint: Pubkey::default(),
            base_vault: Pubkey::default(),
            quote_vault: Pubkey::default(),
            lp_supply: 0,
            coin_creator: Pubkey::default(),
        }
    }

    #[test]
    fn route1_captured_state_resolves_75bps_and_sells_exact() {
        let cfg = decode_fee_config(FEE_CFG_BYTES).unwrap();
        // Route 1 captured single-slot state.
        let (base_res, quote_res, supply) = (
            52_559_268_744_521u64,
            1_722_520_916_860u64,
            999_678_618_479_009u64,
        );
        let fee = resolve_fee_v2(base_res, quote_res, supply, &cfg).unwrap();
        assert_eq!(
            (fee.lp_bps, fee.protocol_bps, fee.creator_bps, fee.total_bps),
            (20, 5, 50, 75)
        );
        // SELL quote nets exactly gross − Σ ceil(gross·bps/1e4).
        let base_mint = Pubkey::new_unique();
        let pool = feev2_pool(base_mint, Pubkey::new_unique());
        let amt = 44_114_312u64;
        let q = pump_quote_detailed_v2(&pool, &base_mint, amt, base_res, quote_res, supply, &cfg)
            .unwrap();
        let gross = crate::math::cpmm_amount_out(amt, base_res, quote_res, 0, 10_000);
        let expect = sell_quote_with_fee_split(amt, base_res, quote_res, [20, 5, 50]).unwrap();
        assert_eq!(q, expect);
        assert_eq!(q.out, gross - q.fee);
    }

    #[test]
    fn route3_captured_state_resolves_expected_tier() {
        let cfg = decode_fee_config(FEE_CFG_BYTES).unwrap();
        // Route 3 captured state (≈13.7e12 mcap → creator 70 → 95 bps).
        let fee = resolve_fee_v2(
            58_271_548_974_899,
            801_671_310_462,
            998_934_621_420_585,
            &cfg,
        )
        .unwrap();
        assert_eq!(fee.total_bps, 95);
        // A higher-market-cap state (lower fee) selects a cheaper tier — dynamic.
        let fee_hi = resolve_fee_v2(
            56_210_225_348_742,
            831_147_255_886,
            998_934_621_420_585,
            &cfg,
        )
        .unwrap();
        assert!(fee_hi.total_bps <= 95);
    }

    #[test]
    fn all_sizes_in_one_snapshot_use_the_same_pretrade_tier() {
        let cfg = decode_fee_config(FEE_CFG_BYTES).unwrap();
        let (base_res, quote_res, supply) = (
            52_559_268_744_521u64,
            1_722_520_916_860u64,
            999_678_618_479_009u64,
        );
        let base_mint = Pubkey::new_unique();
        let pool = feev2_pool(base_mint, Pubkey::new_unique());
        // Fee bps is a property of the snapshot (pre-trade mcap), size-independent.
        for amt in [1_000_000u64, 11_638_009, 44_114_312, 500_000_000] {
            let q =
                pump_quote_detailed_v2(&pool, &base_mint, amt, base_res, quote_res, supply, &cfg)
                    .unwrap();
            let expect = sell_quote_with_fee_split(amt, base_res, quote_res, [20, 5, 50]).unwrap();
            assert_eq!(q, expect, "size {amt}");
        }
    }

    #[test]
    fn no_optimistic_fallback_on_bad_inputs() {
        let cfg = decode_fee_config(FEE_CFG_BYTES).unwrap();
        let base_mint = Pubkey::new_unique();
        let pool = feev2_pool(base_mint, Pubkey::new_unique());
        // Zero supply → typed FeeV2 error, NOT a 30 bps fallback.
        let e = pump_quote_detailed_v2(&pool, &base_mint, 1_000, 1_000, 1_000, 0, &cfg);
        assert!(matches!(e, Err(PumpQuoteError::FeeV2(_))));
        // Zero reserve → EmptyReserves (before fee resolution).
        assert_eq!(
            pump_quote_detailed_v2(&pool, &base_mint, 1_000, 0, 1_000, 1, &cfg),
            Err(PumpQuoteError::EmptyReserves)
        );
    }

    #[test]
    fn monitor_and_optimizer_share_one_pump_fee_impl() {
        // The route-engine Leg and the direct quote MUST return identical output
        // for the same snapshot — they call the same fee-v2 implementation.
        use crate::route_engine::Leg;
        let cfg = decode_fee_config(FEE_CFG_BYTES).unwrap();
        let (base_res, quote_res, supply) = (
            52_559_268_744_521u64,
            1_722_520_916_860u64,
            999_678_618_479_009u64,
        );
        let base_mint = Pubkey::new_unique();
        let quote_mint = Pubkey::new_unique();
        let pool = feev2_pool(base_mint, quote_mint);
        let amt = 9_000_000u64;
        let leg = Leg::Pump {
            pool: pool.clone(),
            base_reserve: base_res,
            quote_reserve: quote_res,
            base_mint_supply: supply,
            fee_config: cfg.clone(),
        };
        let via_leg = leg.quote_detailed(&base_mint, amt).unwrap();
        let direct =
            pump_quote_detailed_v2(&pool, &base_mint, amt, base_res, quote_res, supply, &cfg)
                .unwrap();
        assert_eq!(via_leg, (direct.out, direct.fee));
    }

    #[test]
    fn flat_config_reproduces_legacy_30bps_split() {
        // A positively-identified legacy-flat pool still charges 30 bps exactly.
        let base_mint = Pubkey::new_unique();
        let pool = feev2_pool(base_mint, Pubkey::new_unique());
        let flat = FeeConfig::flat(25, 5, 0);
        let (br, qr) = (1_000_000_000_000u64, 1_000_000_000_000u64);
        let v2 = pump_quote_detailed_v2(&pool, &base_mint, 1_000_000, br, qr, 1_000_000_000, &flat)
            .unwrap();
        let legacy = pump_quote_detailed(&pool, &base_mint, 1_000_000, br, qr).unwrap();
        assert_eq!(v2, legacy);
    }
}
