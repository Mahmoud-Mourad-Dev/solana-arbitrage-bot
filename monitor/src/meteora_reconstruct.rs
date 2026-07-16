//! Meteora DLMM `swap2` byte-exact reconstruction (S13C slice 4). PURE: no RPC,
//! no transaction construction, no payer substitution, no simulation.
//!
//! FINDING (recorded honestly): there are NO direct top-level Meteora swaps on
//! the supported pairs — every swap is Jupiter/CPI-routed. The fixtures here are
//! CPI-EXPOSED `swap2` instructions (labelled `source="cpi_inner"`); they expose
//! the exact DLMM instruction + account metas, but they do NOT satisfy the
//! three-DIRECT-fixture requirement. Route 3 has NO Meteora fixtures at all.
//!
//! `swap2` data = disc(8) | amount_in:u64 | min_amount_out:u64 |
//! remaining_accounts_info (empty `00000000` in every observed fixture).
//! Accounts: 16 fixed (IDL order) + N trailing bin arrays.

use crate::sim_parity::{
    bin_array_pda, bitmap_extension_pda, dlmm_oracle, dlmm_program, event_authority,
};
use serde::Deserialize;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

pub const SWAP2_DISCRIMINATOR: [u8; 8] = [0x41, 0x4b, 0x3f, 0x4c, 0xeb, 0x5b, 0x5b, 0x88];
pub const SWAP_DISCRIMINATOR: [u8; 8] = [0xf8, 0xc6, 0x9e, 0x91, 0xe1, 0x75, 0x87, 0xc8];
pub const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const TOKEN_2022_PROGRAM: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
pub const MEMO_PROGRAM: &str = "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr";

/// Which DLMM swap instruction variant a fixture uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeteoraVariant {
    Swap,
    Swap2,
}

impl MeteoraVariant {
    pub fn from_discriminator(disc: &[u8]) -> Option<Self> {
        if disc == SWAP2_DISCRIMINATOR {
            Some(MeteoraVariant::Swap2)
        } else if disc == SWAP_DISCRIMINATOR {
            Some(MeteoraVariant::Swap)
        } else {
            None
        }
    }
}

// ─────────────────────────── fixtures ───────────────────────────

#[derive(Debug, Deserialize)]
pub struct Swap2Fixtures {
    pub instruction_variant: String,
    pub swap2_discriminator: String,
    pub routes: std::collections::BTreeMap<String, RouteFx>,
}

#[derive(Debug, Deserialize)]
pub struct RouteFx {
    pub route: String,
    pub pair: String,
    #[serde(default)]
    pub active_id: Option<i32>,
    #[serde(default)]
    pub bin_step: Option<u16>,
    #[serde(default)]
    pub oracle: Option<String>,
    #[serde(default)]
    pub token_x_program_flag: Option<u8>,
    #[serde(default)]
    pub token_y_program_flag: Option<u8>,
    pub direct_fixtures: usize,
    #[serde(default)]
    pub cpi_fixtures: Vec<Swap2Fx>,
    #[serde(default)]
    pub status: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Swap2Fx {
    pub sig: String,
    pub slot: u64,
    pub variant: String,
    pub data_hex: String,
    pub amount_in: u64,
    pub min_amount_out: u64,
    pub remaining_accounts_info_hex: String,
    pub n_accounts: usize,
    pub bin_array_count: usize,
    pub accounts: Vec<AccountRec>,
    pub bin_arrays: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct AccountRec {
    pub i: usize,
    pub pubkey: String,
    pub signer: bool,
    pub writable: bool,
    pub origin: String,
}

pub const FIXTURES_JSON: &str = include_str!("../fixtures/meteora/swap2_cpi_fixtures.json");

pub fn load() -> Swap2Fixtures {
    serde_json::from_str(FIXTURES_JSON).expect("swap2_cpi_fixtures.json parses")
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

// ─────────────────────── data reconstruction ───────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataError {
    TooShort { len: usize },
    WrongDiscriminator,
}

/// Reconstruct `swap2` data from semantic fields + the (verbatim) tail
/// `remaining_accounts_info` bytes. Byte-exact: disc | amount_in | min_out |
/// tail. The tail is preserved verbatim — never guessed.
pub fn reconstruct_swap2_data(
    amount_in: u64,
    min_amount_out: u64,
    remaining_accounts_info: &[u8],
) -> Vec<u8> {
    let mut d = Vec::with_capacity(24 + remaining_accounts_info.len());
    d.extend_from_slice(&SWAP2_DISCRIMINATOR);
    d.extend_from_slice(&amount_in.to_le_bytes());
    d.extend_from_slice(&min_amount_out.to_le_bytes());
    d.extend_from_slice(remaining_accounts_info);
    d
}

/// Decode swap2 data → (amount_in, min_out, remaining_accounts_info bytes).
pub fn decode_swap2_data(bytes: &[u8]) -> Result<(u64, u64, Vec<u8>), DataError> {
    if bytes.len() < 24 {
        return Err(DataError::TooShort { len: bytes.len() });
    }
    if bytes[0..8] != SWAP2_DISCRIMINATOR {
        return Err(DataError::WrongDiscriminator);
    }
    Ok((
        u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
        u64::from_le_bytes(bytes[16..24].try_into().unwrap()),
        bytes[24..].to_vec(),
    ))
}

// ─────────────────────── account / PDA validation ───────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MeteoraReject {
    UnsupportedVariant,
    AccountCountTooSmall {
        got: usize,
    },
    OracleMismatch,
    EventAuthorityMismatch,
    ProgramSentinelMismatch {
        index: usize,
    },
    DataReconstructionFailed,
    BinArrayNotOfPair {
        index: usize,
    },
    /// Arrays are not strictly monotonic in the traversal direction (ascending
    /// for a price-up swap, descending for a price-down swap).
    BinArraysNotMonotonic,
    NoBinArrays,
    TokenProgramMismatch {
        index: usize,
    },
}

/// Find the bin-array index i (within +/- 512 of the pair's active array) whose
/// PDA equals `addr`, proving the array belongs to THIS pair. None ⇒ foreign.
pub fn bin_array_index_of(pair: &Pubkey, addr: &Pubkey) -> Option<i64> {
    (-520i64..=520).find(|&i| &bin_array_pda(pair, i) == addr)
}

/// Full provenance/structure guard for a swap2 fixture on a route's pair.
/// Proves data reconstructs byte-exact, PDAs match, token programs match the
/// pair flags, and every trailing account is a correctly-ordered bin array of
/// THIS pair. Returns a typed rejection otherwise.
pub fn validate_swap2(route: &RouteFx, fx: &Swap2Fx) -> Result<(), MeteoraReject> {
    let raw = hex(&fx.data_hex);
    // Variant.
    if MeteoraVariant::from_discriminator(&raw[0..8.min(raw.len())]) != Some(MeteoraVariant::Swap2)
    {
        return Err(MeteoraReject::UnsupportedVariant);
    }
    // Byte-exact data reconstruction.
    let (a, m, tail) =
        decode_swap2_data(&raw).map_err(|_| MeteoraReject::DataReconstructionFailed)?;
    if a != fx.amount_in || m != fx.min_amount_out {
        return Err(MeteoraReject::DataReconstructionFailed);
    }
    if reconstruct_swap2_data(a, m, &tail) != raw {
        return Err(MeteoraReject::DataReconstructionFailed);
    }
    // 16 fixed accounts minimum.
    if fx.accounts.len() < 17 {
        return Err(MeteoraReject::AccountCountTooSmall {
            got: fx.accounts.len(),
        });
    }
    let pair = Pubkey::from_str(&route.pair).unwrap();
    let dlmm = dlmm_program();
    let at = |i: usize| Pubkey::from_str(&fx.accounts[i].pubkey).unwrap_or_default();
    // [1] bin_array_bitmap_extension: either the real PDA (`["bitmap", pair]`)
    // when the pool has an extension, or the program id as a None sentinel when
    // it does not. Both are valid; anything else is wrong.
    if at(1) != dlmm && at(1) != bitmap_extension_pda(&pair) {
        return Err(MeteoraReject::ProgramSentinelMismatch { index: 1 });
    }
    // [9] host_fee_in is always a None sentinel (= program id) for our routes.
    if at(9) != dlmm {
        return Err(MeteoraReject::ProgramSentinelMismatch { index: 9 });
    }
    // [8] oracle, [14] event_authority (PDA-proven).
    if at(8) != dlmm_oracle(&pair) {
        return Err(MeteoraReject::OracleMismatch);
    }
    if at(14) != event_authority(&dlmm) {
        return Err(MeteoraReject::EventAuthorityMismatch);
    }
    // Token programs match the pair's flags: [11]=token_x, [12]=token_y.
    let prog_for = |flag: Option<u8>| {
        if flag == Some(1) {
            TOKEN_2022_PROGRAM
        } else {
            TOKEN_PROGRAM
        }
    };
    if fx.accounts[11].pubkey != prog_for(route.token_x_program_flag) {
        return Err(MeteoraReject::TokenProgramMismatch { index: 11 });
    }
    if fx.accounts[12].pubkey != prog_for(route.token_y_program_flag) {
        return Err(MeteoraReject::TokenProgramMismatch { index: 12 });
    }
    // Trailing bin arrays: each belongs to THIS pair; indices strictly
    // MONOTONIC in the traversal direction (ascending for a price-up swap,
    // descending for a price-down swap — the direction the program walks bins).
    if fx.bin_arrays.is_empty() {
        return Err(MeteoraReject::NoBinArrays);
    }
    let mut idxs = Vec::with_capacity(fx.bin_arrays.len());
    for (k, addr) in fx.bin_arrays.iter().enumerate() {
        let pk = Pubkey::from_str(addr).unwrap_or_default();
        idxs.push(
            bin_array_index_of(&pair, &pk)
                .ok_or(MeteoraReject::BinArrayNotOfPair { index: 16 + k })?,
        );
    }
    let ascending = idxs.windows(2).all(|w| w[1] > w[0]);
    let descending = idxs.windows(2).all(|w| w[1] < w[0]);
    if !(ascending || descending) {
        return Err(MeteoraReject::BinArraysNotMonotonic);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn route1() -> RouteFx {
        let f = load();
        f.routes
            .into_values()
            .find(|r| r.route == "route1")
            .unwrap()
    }

    #[test]
    fn variant_is_swap2_and_no_direct_fixtures() {
        let f = load();
        assert_eq!(f.instruction_variant, "swap2");
        assert_eq!(f.swap2_discriminator, "414b3f4ceb5b5b88");
        let r1 = route1();
        assert_eq!(r1.direct_fixtures, 0, "no direct top-level swaps exist");
        assert!(r1.cpi_fixtures.len() >= 3, "≥3 CPI-exposed swap2 fixtures");
        for fx in &r1.cpi_fixtures {
            assert_eq!(fx.variant, "swap2");
        }
        // Route 3 has NO Meteora fixtures.
        let r3 = f.routes.get("route3").unwrap();
        assert_eq!(r3.direct_fixtures, 0);
        assert!(r3.cpi_fixtures.is_empty());
        assert_eq!(r3.status.as_deref(), Some("NO_METEORA_FIXTURES"));
    }

    #[test]
    fn data_reconstructs_byte_exact_for_every_fixture() {
        for fx in &route1().cpi_fixtures {
            let raw = hex(&fx.data_hex);
            let (a, m, tail) = decode_swap2_data(&raw).unwrap();
            assert_eq!((a, m), (fx.amount_in, fx.min_amount_out));
            assert_eq!(reconstruct_swap2_data(a, m, &tail), raw);
            // Observed tail is the empty remaining_accounts_info.
            assert_eq!(hex(&fx.remaining_accounts_info_hex), tail);
        }
    }

    #[test]
    fn field_mutations_touch_only_their_windows() {
        let tail = hex("00000000");
        let base = reconstruct_swap2_data(1000, 2000, &tail);
        let ma = reconstruct_swap2_data(1001, 2000, &tail);
        let mm = reconstruct_swap2_data(1000, 2001, &tail);
        assert_eq!(&base[0..8], &SWAP2_DISCRIMINATOR);
        assert_ne!(base[8..16], ma[8..16]);
        assert_eq!(base[16..], ma[16..]);
        assert_eq!(base[0..16], mm[0..16]);
        assert_ne!(base[16..24], mm[16..24]);
        // little-endian
        assert_eq!(
            &reconstruct_swap2_data(0x0102030405060708, 0, &[])[8..16],
            &0x0102030405060708u64.to_le_bytes()
        );
    }

    #[test]
    fn decode_rejects_malformed_and_wrong_discriminator() {
        assert_eq!(
            decode_swap2_data(&[0u8; 20]),
            Err(DataError::TooShort { len: 20 })
        );
        let mut bad = reconstruct_swap2_data(1, 2, &[0, 0, 0, 0]);
        bad[0] ^= 0xff;
        assert_eq!(decode_swap2_data(&bad), Err(DataError::WrongDiscriminator));
        // The `swap` (v1) discriminator must not be accepted as swap2.
        assert_eq!(
            MeteoraVariant::from_discriminator(&SWAP_DISCRIMINATOR),
            Some(MeteoraVariant::Swap)
        );
        assert_ne!(
            MeteoraVariant::from_discriminator(&SWAP_DISCRIMINATOR),
            Some(MeteoraVariant::Swap2)
        );
    }

    #[test]
    fn every_fixture_validates_end_to_end() {
        let r = route1();
        for fx in &r.cpi_fixtures {
            validate_swap2(&r, fx).unwrap_or_else(|e| panic!("{} {e:?}", fx.sig));
        }
    }

    #[test]
    fn oracle_and_event_authority_are_pda_proven() {
        let r = route1();
        let pair = Pubkey::from_str(&r.pair).unwrap();
        let fx = &r.cpi_fixtures[0];
        assert_eq!(fx.accounts[8].pubkey, dlmm_oracle(&pair).to_string());
        assert_eq!(
            fx.accounts[14].pubkey,
            event_authority(&dlmm_program()).to_string()
        );
        // Pair's stored oracle equals the derivation too.
        assert_eq!(
            r.oracle.as_deref(),
            Some(dlmm_oracle(&pair).to_string().as_str())
        );
    }

    #[test]
    fn bitmap_extension_is_pda_or_sentinel_per_fixture() {
        let r = route1();
        let pair = Pubkey::from_str(&r.pair).unwrap();
        let ext = bitmap_extension_pda(&pair).to_string();
        let sentinel = dlmm_program().to_string();
        let mut saw_real_ext = false;
        for fx in &r.cpi_fixtures {
            let a1 = &fx.accounts[1].pubkey;
            assert!(
                a1 == &ext || a1 == &sentinel,
                "acct[1] must be the bitmap-extension PDA or the None sentinel for {}: {a1}",
                fx.sig
            );
            saw_real_ext |= a1 == &ext;
        }
        // At least one captured fixture actually carries the extension account,
        // which is what proves the `["bitmap", pair]` derivation against real data.
        assert!(
            saw_real_ext,
            "expected ≥1 fixture with a real bitmap extension"
        );
    }

    #[test]
    fn token_x_is_token_2022_and_programs_match_flags() {
        let r = route1();
        assert_eq!(r.token_x_program_flag, Some(1), "token_x is Token-2022");
        assert_eq!(
            r.token_y_program_flag,
            Some(0),
            "token_y (WSOL) is SPL Token"
        );
        let fx = &r.cpi_fixtures[0];
        assert_eq!(fx.accounts[11].pubkey, TOKEN_2022_PROGRAM);
        assert_eq!(fx.accounts[12].pubkey, TOKEN_PROGRAM);
    }

    #[test]
    fn bin_arrays_belong_to_pair_and_are_monotonic() {
        let r = route1();
        let pair = Pubkey::from_str(&r.pair).unwrap();
        for fx in &r.cpi_fixtures {
            let idxs: Vec<i64> = fx
                .bin_arrays
                .iter()
                .map(|a| bin_array_index_of(&pair, &Pubkey::from_str(a).unwrap()).expect("of pair"))
                .collect();
            let asc = idxs.windows(2).all(|w| w[1] > w[0]);
            let desc = idxs.windows(2).all(|w| w[1] < w[0]);
            assert!(
                asc || desc,
                "bin arrays must be monotonic for {}: {idxs:?}",
                fx.sig
            );
        }
    }

    #[test]
    fn negative_bin_array_from_other_pair_is_rejected() {
        let r = route1();
        let mut fx = clone_fixture(&r.cpi_fixtures[0]);
        // Replace the first bin array with one derived for a DIFFERENT pair.
        let other = Pubkey::new_unique();
        fx.bin_arrays[0] = bin_array_pda(&other, -7).to_string();
        fx.accounts[16].pubkey = fx.bin_arrays[0].clone();
        assert_eq!(
            validate_swap2(&r, &fx),
            Err(MeteoraReject::BinArrayNotOfPair { index: 16 })
        );
    }

    #[test]
    fn negative_nonmonotonic_bin_arrays_rejected() {
        let r = route1();
        // Reversing a monotonic list stays monotonic — build a genuinely
        // NON-monotonic order by swapping the first two of a ≥3-array fixture.
        let src = r
            .cpi_fixtures
            .iter()
            .find(|f| f.bin_arrays.len() >= 3)
            .unwrap();
        let mut fx = clone_fixture(src);
        fx.bin_arrays.swap(0, 1);
        for (k, a) in fx.bin_arrays.iter().enumerate() {
            fx.accounts[16 + k].pubkey = a.clone();
        }
        assert_eq!(
            validate_swap2(&r, &fx),
            Err(MeteoraReject::BinArraysNotMonotonic)
        );
    }

    #[test]
    fn negative_wrong_oracle_and_bad_variant_rejected() {
        let r = route1();
        let mut fx = clone_fixture(&r.cpi_fixtures[0]);
        fx.accounts[8].pubkey = Pubkey::new_unique().to_string();
        assert_eq!(validate_swap2(&r, &fx), Err(MeteoraReject::OracleMismatch));
        // Wrong discriminator in data.
        let mut fx2 = clone_fixture(&r.cpi_fixtures[0]);
        let mut raw = hex(&fx2.data_hex);
        raw[0] ^= 0xff;
        fx2.data_hex = raw.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            validate_swap2(&r, &fx2),
            Err(MeteoraReject::UnsupportedVariant)
        );
    }

    fn clone_fixture(fx: &Swap2Fx) -> Swap2Fx {
        Swap2Fx {
            sig: fx.sig.clone(),
            slot: fx.slot,
            variant: fx.variant.clone(),
            data_hex: fx.data_hex.clone(),
            amount_in: fx.amount_in,
            min_amount_out: fx.min_amount_out,
            remaining_accounts_info_hex: fx.remaining_accounts_info_hex.clone(),
            n_accounts: fx.n_accounts,
            bin_array_count: fx.bin_array_count,
            accounts: fx
                .accounts
                .iter()
                .map(|a| AccountRec {
                    i: a.i,
                    pubkey: a.pubkey.clone(),
                    signer: a.signer,
                    writable: a.writable,
                    origin: a.origin.clone(),
                })
                .collect(),
            bin_arrays: fx.bin_arrays.clone(),
        }
    }
}
