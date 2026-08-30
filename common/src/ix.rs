//! On-chain instruction ABI — FROZEN. Any change here is a breaking
//! protocol change between executor and program and must be versioned.
//!
//! ```text
//! header (17 bytes, little-endian):
//!   [0]      num_hops: u8            (1..=4)
//!   [1..9]   amount_in: u64          (raw units, first hop input)
//!   [9..17]  min_profit: u64         (raw units of the base token)
//! per hop (12 bytes each):
//!   [0]      dex: u8                 (0 = Raydium v4, 1 = Orca Whirlpool)
//!   [1]      num_accounts: u8        (hop slice length INCLUDING the dex
//!                                     program at index 0)
//!   [2]      source_index: u8        (index within the hop slice of the
//!                                     user's SOURCE token account)
//!   [3]      flags: u8               (bit0 = a_to_b, Whirlpool only)
//!   [4..12]  min_amount_out: u64     (per-hop floor, forwarded to the DEX)
//! ```
//!
//! No Borsh, no Anchor discriminator for OUR program. The Whirlpool Anchor
//! discriminator below belongs to the EXTERNAL Whirlpool program's `swap`.

use alloc::vec::Vec;

pub const MAX_HOPS: usize = 4;
pub const HEADER_LEN: usize = 17;
pub const HOP_LEN: usize = 12;

/// Whirlpool swap sqrt-price bounds (Q64.64). Passing the extreme in the
/// trade direction means "no price limit"; the per-hop min_amount_out and
/// the final profit check are the real guards.
pub const MIN_SQRT_PRICE_X64: u128 = 4_295_048_016;
pub const MAX_SQRT_PRICE_X64: u128 = 79_226_673_515_401_279_992_447_579_055;

/// Anchor sighash("global", "swap") of the EXTERNAL Whirlpool program.
pub const WHIRLPOOL_SWAP_DISCRIMINATOR: [u8; 8] = [248, 198, 158, 145, 225, 117, 135, 200];
/// Raydium AMM v4 SwapBaseIn single-byte discriminator.
pub const RAYDIUM_SWAP_BASE_IN_TAG: u8 = 9;
/// Meteora DLMM `swap2` discriminator. Source of truth:
/// `monitor/src/meteora_reconstruct.rs::SWAP2_DISCRIMINATOR` and
/// `monitor/fixtures/meteora/swap2_cpi_fixtures.json` ("414b3f4ceb5b5b88"),
/// captured from live mainnet CPIs and byte-exact-verified in Slice 5.
/// A monitor-side test guards this constant against that source.
pub const METEORA_SWAP2_DISCRIMINATOR: [u8; 8] = [0x41, 0x4b, 0x3f, 0x4c, 0xeb, 0x5b, 0x5b, 0x88];
/// PumpSwap AMM instruction discriminators, `sha256("global:<name>")[..8]`.
/// Source of truth: `monitor/src/pump_amm.rs` (`IX_SELL_DISCRIMINATOR` /
/// `IX_BUY_DISCRIMINATOR`) and `monitor/src/pump_reconstruct.rs`
/// (`SELL_DISCRIMINATOR`), both proven byte-exact against captured mainnet
/// sells (S1–S13). A monitor-side test guards these against that source.
/// SELL = base in → quote out (exact for all pools); BUY = quote in → base out
/// (exact only for creator-less pools — the quote refuses creator-pool BUY).
pub const PUMP_SELL_DISCRIMINATOR: [u8; 8] = [0x33, 0xe6, 0x85, 0xa4, 0x01, 0x7f, 0x83, 0xad];
pub const PUMP_BUY_DISCRIMINATOR: [u8; 8] = [0x66, 0x06, 0x3d, 0x12, 0x01, 0xda, 0xeb, 0xea];

/// Program ids as raw bytes-agnostic base58 strings (each crate converts to
/// its own Pubkey/Address type; keeping strings avoids a Solana dependency).
pub const RAYDIUM_V4_PROGRAM_STR: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";
pub const WHIRLPOOL_PROGRAM_STR: &str = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";
pub const TOKEN_PROGRAM_STR: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
/// Source of truth: `monitor/src/meteora_dlmm.rs::DLMM_PROGRAM_ID` (mainnet
/// program behind the 6/6 live-exact quote parity); guarded by a monitor test.
pub const METEORA_DLMM_PROGRAM_STR: &str = "LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo";
/// Source of truth: `monitor/src/pump_amm.rs::PUMP_AMM_PROGRAM_ID`; guarded by
/// a monitor test.
pub const PUMP_AMM_PROGRAM_STR: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";

/// Base58-decoded program ids. These are the on-chain source of truth (the
/// Pinocchio program builds its `Address` constants from them, avoiding a
/// base58 decoder on-chain). `program_id_bytes_match_str` guards that they
/// equal the canonical `*_STR` above.
pub const RAYDIUM_V4_PROGRAM_ID: [u8; 32] = [
    75, 217, 73, 196, 54, 2, 195, 63, 32, 119, 144, 237, 22, 163, 82, 76, 161, 185, 151, 92, 241,
    33, 162, 169, 12, 255, 236, 125, 248, 182, 138, 205,
];
pub const WHIRLPOOL_PROGRAM_ID: [u8; 32] = [
    14, 3, 104, 95, 142, 144, 144, 83, 228, 88, 18, 28, 102, 245, 167, 106, 237, 199, 112, 106,
    161, 28, 130, 248, 170, 149, 42, 143, 43, 120, 121, 169,
];
pub const TOKEN_PROGRAM_ID: [u8; 32] = [
    6, 221, 246, 225, 215, 101, 161, 147, 217, 203, 225, 70, 206, 235, 121, 172, 28, 180, 133, 237,
    95, 91, 55, 145, 58, 140, 245, 133, 126, 255, 0, 169,
];
pub const METEORA_DLMM_PROGRAM_ID: [u8; 32] = [
    4, 233, 225, 47, 188, 132, 232, 38, 201, 50, 204, 233, 226, 100, 12, 206, 21, 89, 12, 28, 98,
    115, 176, 146, 87, 8, 186, 59, 133, 32, 176, 188,
];
pub const PUMP_AMM_PROGRAM_ID: [u8; 32] = [
    12, 20, 222, 252, 130, 94, 198, 118, 148, 37, 8, 24, 187, 101, 64, 101, 244, 41, 141, 49, 86,
    213, 113, 180, 212, 248, 9, 12, 24, 233, 168, 99,
];

/// Stable custom error codes surfaced as `ProgramError::Custom(code)`.
/// Codes are FROZEN — executors match on them for landed-tx forensics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ArbError {
    MalformedInstruction = 0,
    BadHopCount = 1,
    UnknownDex = 2,
    AccountSliceOutOfBounds = 3,
    InvalidDexProgram = 4,
    InvalidTokenAccount = 5,
    TokenAccountOwnerMismatch = 6,
    ArithmeticOverflow = 7,
    ProfitNotMet = 8,
    MissingSignature = 9,
    ZeroAmount = 10,
}

impl ArbError {
    pub const fn message(self) -> &'static str {
        match self {
            ArbError::MalformedInstruction => "malformed instruction data",
            ArbError::BadHopCount => "hop count must be 1..=4",
            ArbError::UnknownDex => "unknown dex tag",
            ArbError::AccountSliceOutOfBounds => "hop account slice out of bounds",
            ArbError::InvalidDexProgram => "hop program id does not match declared dex",
            ArbError::InvalidTokenAccount => "account is not a valid SPL token account",
            ArbError::TokenAccountOwnerMismatch => "token account not owned by authority",
            ArbError::ArithmeticOverflow => "arithmetic overflow",
            ArbError::ProfitNotMet => "cycle finished below required profit — reverting",
            ArbError::MissingSignature => "authority signature missing",
            ArbError::ZeroAmount => "hop input amount is zero",
        }
    }
}

impl core::fmt::Display for ArbError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.message())
    }
}

#[cfg(feature = "std")]
impl std::error::Error for ArbError {}

/// DEX tag: byte value is the wire encoding; serde names match the
/// TypeScript monitor's JSON (`"raydium-v4"` / `"orca-whirlpool"`).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum DexKind {
    #[cfg_attr(feature = "serde", serde(rename = "raydium-v4"))]
    RaydiumV4 = 0,
    #[cfg_attr(feature = "serde", serde(rename = "orca-whirlpool"))]
    OrcaWhirlpool = 1,
    #[cfg_attr(feature = "serde", serde(rename = "meteora-dlmm"))]
    MeteoraDlmm = 2,
    #[cfg_attr(feature = "serde", serde(rename = "pump-amm"))]
    PumpAmm = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HopParams {
    pub dex: DexKind,
    pub num_accounts: u8,
    pub source_index: u8,
    pub a_to_b: bool,
    pub min_amount_out: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IxParams {
    pub amount_in: u64,
    pub min_profit: u64,
    pub hops: Vec<HopParams>,
}

#[inline]
fn read_u64_le(data: &[u8], offset: usize) -> u64 {
    // callers guarantee bounds
    u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap())
}

/// Parse instruction data (used on-chain — must stay allocation-light).
pub fn parse_instruction(data: &[u8]) -> Result<IxParams, ArbError> {
    if data.len() < HEADER_LEN {
        return Err(ArbError::MalformedInstruction);
    }
    let num_hops = data[0] as usize;
    if num_hops == 0 || num_hops > MAX_HOPS {
        return Err(ArbError::BadHopCount);
    }
    if data.len() != HEADER_LEN + num_hops * HOP_LEN {
        return Err(ArbError::MalformedInstruction);
    }
    let amount_in = read_u64_le(data, 1);
    let min_profit = read_u64_le(data, 9);

    let mut hops = Vec::with_capacity(num_hops);
    for i in 0..num_hops {
        let o = HEADER_LEN + i * HOP_LEN;
        let dex = match data[o] {
            0 => DexKind::RaydiumV4,
            1 => DexKind::OrcaWhirlpool,
            2 => DexKind::MeteoraDlmm,
            3 => DexKind::PumpAmm,
            _ => return Err(ArbError::UnknownDex),
        };
        let num_accounts = data[o + 1];
        let source_index = data[o + 2];
        if num_accounts < 2 || source_index >= num_accounts {
            return Err(ArbError::MalformedInstruction);
        }
        hops.push(HopParams {
            dex,
            num_accounts,
            source_index,
            a_to_b: data[o + 3] & 1 == 1,
            min_amount_out: read_u64_le(data, o + 4),
        });
    }
    Ok(IxParams {
        amount_in,
        min_profit,
        hops,
    })
}

/// Exact inverse of [`parse_instruction`]; used by the off-chain executor.
pub fn encode_instruction(params: &IxParams) -> Vec<u8> {
    let mut data = Vec::with_capacity(HEADER_LEN + params.hops.len() * HOP_LEN);
    data.push(params.hops.len() as u8);
    data.extend_from_slice(&params.amount_in.to_le_bytes());
    data.extend_from_slice(&params.min_profit.to_le_bytes());
    for hop in &params.hops {
        data.push(hop.dex as u8);
        data.push(hop.num_accounts);
        data.push(hop.source_index);
        data.push(hop.a_to_b as u8);
        data.extend_from_slice(&hop.min_amount_out.to_le_bytes());
    }
    data
}

/// Raydium v4 SwapBaseIn: `[9, amount_in u64, minimum_amount_out u64]`.
pub fn build_raydium_swap_data(amount_in: u64, min_amount_out: u64) -> Vec<u8> {
    let mut data = Vec::with_capacity(17);
    data.push(RAYDIUM_SWAP_BASE_IN_TAG);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_amount_out.to_le_bytes());
    data
}

/// Whirlpool `swap`: discriminator + amount + other_amount_threshold +
/// sqrt_price_limit + amount_specified_is_input + a_to_b.
pub fn build_whirlpool_swap_data(amount_in: u64, min_amount_out: u64, a_to_b: bool) -> Vec<u8> {
    let sqrt_price_limit = if a_to_b {
        MIN_SQRT_PRICE_X64 + 1
    } else {
        MAX_SQRT_PRICE_X64 - 1
    };
    let mut data = Vec::with_capacity(42);
    data.extend_from_slice(&WHIRLPOOL_SWAP_DISCRIMINATOR);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_amount_out.to_le_bytes());
    data.extend_from_slice(&sqrt_price_limit.to_le_bytes());
    data.push(1); // amount_specified_is_input = true (exact-in)
    data.push(a_to_b as u8);
    data
}

/// Meteora DLMM `swap2`: discriminator + amount_in + min_amount_out +
/// empty `remaining_accounts_info` (`00000000`). The traversed bin arrays are
/// passed as trailing CPI accounts, not encoded in the data — which is why
/// DLMM's variable account count fits the existing per-hop `num_accounts: u8`
/// without any wire-format change (see `dlmm_hop_fits_existing_wire_format`).
/// Format source: `monitor/fixtures/meteora/swap2_cpi_fixtures.json`
/// (`disc(8)|amount_in:u64|min_amount_out:u64|remaining_accounts_info`).
pub fn build_meteora_swap_data(amount_in: u64, min_amount_out: u64) -> Vec<u8> {
    let mut data = Vec::with_capacity(28);
    data.extend_from_slice(&METEORA_SWAP2_DISCRIMINATOR);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_amount_out.to_le_bytes());
    data.extend_from_slice(&[0, 0, 0, 0]); // remaining_accounts_info: empty vec
    data
}

/// PumpSwap AMM swap data: `disc(8) | amount_in:u64 | min_out:u64` = 24 bytes.
/// `is_sell` selects SELL (base in → quote out) vs BUY (quote in → base out).
/// Layout source: `monitor/src/pump_reconstruct.rs::reconstruct_sell_data`
/// (proven byte-exact against captured mainnet sells). For a pump hop the
/// per-hop `a_to_b` flag carries `is_sell` (there is no sqrt-price bound to
/// encode), so the wire format is unchanged — `HOP_LEN` stays 12.
pub fn build_pump_swap_data(amount_in: u64, min_out: u64, is_sell: bool) -> Vec<u8> {
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(if is_sell {
        &PUMP_SELL_DISCRIMINATOR
    } else {
        &PUMP_BUY_DISCRIMINATOR
    });
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_out.to_le_bytes());
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_params() -> IxParams {
        IxParams {
            amount_in: 1_000_000_000,
            min_profit: 1_205_000,
            hops: vec![
                HopParams {
                    dex: DexKind::OrcaWhirlpool,
                    num_accounts: 12,
                    source_index: 4,
                    a_to_b: true,
                    min_amount_out: 152_000_000,
                },
                HopParams {
                    dex: DexKind::RaydiumV4,
                    num_accounts: 19,
                    source_index: 16,
                    a_to_b: false,
                    min_amount_out: 1_001_000_000,
                },
            ],
        }
    }

    #[test]
    fn encode_parse_roundtrip() {
        let params = sample_params();
        let encoded = encode_instruction(&params);
        assert_eq!(encoded.len(), HEADER_LEN + 2 * HOP_LEN);
        assert_eq!(parse_instruction(&encoded).unwrap(), params);
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(parse_instruction(&[]), Err(ArbError::MalformedInstruction));
        let mut zero = vec![0u8; HEADER_LEN];
        zero[0] = 0;
        assert_eq!(parse_instruction(&zero), Err(ArbError::BadHopCount));
        let mut five = vec![0u8; HEADER_LEN + 5 * HOP_LEN];
        five[0] = 5;
        assert_eq!(parse_instruction(&five), Err(ArbError::BadHopCount));
        let mut trunc = encode_instruction(&sample_params());
        trunc.pop();
        assert_eq!(
            parse_instruction(&trunc),
            Err(ArbError::MalformedInstruction)
        );
        let mut bad_dex = encode_instruction(&sample_params());
        bad_dex[HEADER_LEN] = 7;
        assert_eq!(parse_instruction(&bad_dex), Err(ArbError::UnknownDex));
        let mut bad_src = encode_instruction(&sample_params());
        bad_src[HEADER_LEN + 2] = 200;
        assert_eq!(
            parse_instruction(&bad_src),
            Err(ArbError::MalformedInstruction)
        );
    }

    #[test]
    fn raydium_swap_data_layout() {
        let d = build_raydium_swap_data(123, 456);
        assert_eq!(d.len(), 17);
        assert_eq!(d[0], RAYDIUM_SWAP_BASE_IN_TAG);
        assert_eq!(u64::from_le_bytes(d[1..9].try_into().unwrap()), 123);
        assert_eq!(u64::from_le_bytes(d[9..17].try_into().unwrap()), 456);
    }

    #[test]
    fn whirlpool_swap_data_layout() {
        let d = build_whirlpool_swap_data(111, 222, true);
        assert_eq!(d.len(), 42);
        assert_eq!(&d[0..8], &WHIRLPOOL_SWAP_DISCRIMINATOR);
        assert_eq!(u64::from_le_bytes(d[8..16].try_into().unwrap()), 111);
        assert_eq!(u64::from_le_bytes(d[16..24].try_into().unwrap()), 222);
        assert_eq!(
            u128::from_le_bytes(d[24..40].try_into().unwrap()),
            MIN_SQRT_PRICE_X64 + 1
        );
        assert_eq!(d[40], 1);
        assert_eq!(d[41], 1);
        let d2 = build_whirlpool_swap_data(111, 222, false);
        assert_eq!(
            u128::from_le_bytes(d2[24..40].try_into().unwrap()),
            MAX_SQRT_PRICE_X64 - 1
        );
        assert_eq!(d2[41], 0);
    }

    /// ABI freeze: error codes and layout constants must never drift —
    /// executors match on Custom(code) for landed-tx forensics. Adding
    /// MeteoraDlmm=2 is strictly additive; HEADER_LEN/HOP_LEN are unchanged.
    #[test]
    fn abi_frozen() {
        assert_eq!(HEADER_LEN, 17);
        assert_eq!(HOP_LEN, 12);
        assert_eq!(MAX_HOPS, 4);
        assert_eq!(ArbError::MalformedInstruction as u32, 0);
        assert_eq!(ArbError::InvalidDexProgram as u32, 4);
        assert_eq!(ArbError::ProfitNotMet as u32, 8);
        assert_eq!(ArbError::ZeroAmount as u32, 10);
        assert_eq!(DexKind::RaydiumV4 as u8, 0);
        assert_eq!(DexKind::OrcaWhirlpool as u8, 1);
        assert_eq!(DexKind::MeteoraDlmm as u8, 2);
        assert_eq!(DexKind::PumpAmm as u8, 3);
    }

    #[test]
    fn pump_swap_data_layout() {
        let sell = build_pump_swap_data(123, 456, true);
        assert_eq!(sell.len(), 24);
        assert_eq!(&sell[0..8], &PUMP_SELL_DISCRIMINATOR);
        assert_eq!(u64::from_le_bytes(sell[8..16].try_into().unwrap()), 123);
        assert_eq!(u64::from_le_bytes(sell[16..24].try_into().unwrap()), 456);
        let buy = build_pump_swap_data(1, 2, false);
        assert_eq!(&buy[0..8], &PUMP_BUY_DISCRIMINATOR);
    }

    /// A mixed MeteoraDlmm → PumpAmm route round-trips through the UNCHANGED
    /// wire format (the WSOL→token→WSOL 2-hop the surviving strategy needs).
    #[test]
    fn mixed_dlmm_pump_route_roundtrips() {
        let params = IxParams {
            amount_in: 500_000_000,
            min_profit: 100_000,
            hops: vec![
                HopParams {
                    dex: DexKind::MeteoraDlmm,
                    num_accounts: 20,
                    source_index: 5,
                    a_to_b: false,
                    min_amount_out: 42_000_000,
                },
                HopParams {
                    dex: DexKind::PumpAmm,
                    num_accounts: 24, // the pump 24-account swap slice + program
                    source_index: 5,  // user_base_ata at CPI idx 5 → slice idx 6? resolver sets it
                    a_to_b: true,     // pump: a_to_b = is_sell (base in → quote out)
                    min_amount_out: 500_100_000,
                },
            ],
        };
        let encoded = encode_instruction(&params);
        assert_eq!(
            encoded.len(),
            HEADER_LEN + 2 * HOP_LEN,
            "wire format unchanged"
        );
        assert_eq!(parse_instruction(&encoded).unwrap(), params);
    }

    /// Backward compatibility: a route serialized BEFORE PumpAmm existed (only
    /// Raydium/Whirlpool/Meteora tags) still decodes byte-identically.
    #[test]
    fn pre_pump_route_still_decodes() {
        let legacy = IxParams {
            amount_in: 1_000_000_000,
            min_profit: 1_205_000,
            hops: vec![
                HopParams {
                    dex: DexKind::OrcaWhirlpool,
                    num_accounts: 12,
                    source_index: 4,
                    a_to_b: true,
                    min_amount_out: 152_000_000,
                },
                HopParams {
                    dex: DexKind::MeteoraDlmm,
                    num_accounts: 20,
                    source_index: 5,
                    a_to_b: false,
                    min_amount_out: 1_001_000_000,
                },
            ],
        };
        let encoded = encode_instruction(&legacy);
        assert_eq!(parse_instruction(&encoded).unwrap(), legacy);
    }

    #[test]
    fn meteora_swap_data_layout() {
        let d = build_meteora_swap_data(123, 456);
        assert_eq!(d.len(), 28);
        assert_eq!(&d[0..8], &METEORA_SWAP2_DISCRIMINATOR);
        assert_eq!(u64::from_le_bytes(d[8..16].try_into().unwrap()), 123);
        assert_eq!(u64::from_le_bytes(d[16..24].try_into().unwrap()), 456);
        assert_eq!(&d[24..28], &[0, 0, 0, 0], "empty remaining_accounts_info");
    }

    /// Mixed 2-hop route (MeteoraDlmm → OrcaWhirlpool) round-trips through
    /// the UNCHANGED wire format. A real DLMM hop slice is
    /// [dlmm_program + 16 fixed accounts + N bin arrays]; with the captured
    /// maximum of 3 bin arrays that is 20 accounts — comfortably u8, so no
    /// ABI widening is needed (the point of this test).
    #[test]
    fn dlmm_hop_fits_existing_wire_format() {
        let params = IxParams {
            amount_in: 500_000_000,
            min_profit: 100_000,
            hops: vec![
                HopParams {
                    dex: DexKind::MeteoraDlmm,
                    num_accounts: 20, // program + 16 fixed + 3 bin arrays
                    source_index: 5,  // user_token_in at swap2 index 4, +1 for program at 0
                    a_to_b: false,    // unused for DLMM (direction is implied by token accounts)
                    min_amount_out: 42_000_000,
                },
                HopParams {
                    dex: DexKind::OrcaWhirlpool,
                    num_accounts: 12,
                    source_index: 4,
                    a_to_b: true,
                    min_amount_out: 500_100_000,
                },
            ],
        };
        let encoded = encode_instruction(&params);
        assert_eq!(
            encoded.len(),
            HEADER_LEN + 2 * HOP_LEN,
            "wire format unchanged"
        );
        assert_eq!(parse_instruction(&encoded).unwrap(), params);
    }

    /// The hardcoded on-chain program-id bytes MUST equal the base58 ids.
    #[test]
    fn program_id_bytes_match_str() {
        assert_eq!(
            bs58::decode(RAYDIUM_V4_PROGRAM_STR).into_vec().unwrap(),
            RAYDIUM_V4_PROGRAM_ID
        );
        assert_eq!(
            bs58::decode(WHIRLPOOL_PROGRAM_STR).into_vec().unwrap(),
            WHIRLPOOL_PROGRAM_ID
        );
        assert_eq!(
            bs58::decode(TOKEN_PROGRAM_STR).into_vec().unwrap(),
            TOKEN_PROGRAM_ID
        );
        assert_eq!(
            bs58::decode(METEORA_DLMM_PROGRAM_STR).into_vec().unwrap(),
            METEORA_DLMM_PROGRAM_ID
        );
        assert_eq!(
            bs58::decode(PUMP_AMM_PROGRAM_STR).into_vec().unwrap(),
            PUMP_AMM_PROGRAM_ID
        );
    }

    #[cfg(feature = "serde")]
    #[test]
    fn dexkind_serde_matches_monitor_json() {
        assert_eq!(
            serde_json::from_str::<DexKind>("\"raydium-v4\"").unwrap(),
            DexKind::RaydiumV4
        );
        assert_eq!(
            serde_json::from_str::<DexKind>("\"orca-whirlpool\"").unwrap(),
            DexKind::OrcaWhirlpool
        );
        assert_eq!(
            serde_json::from_str::<DexKind>("\"meteora-dlmm\"").unwrap(),
            DexKind::MeteoraDlmm
        );
        assert_eq!(
            serde_json::from_str::<DexKind>("\"pump-amm\"").unwrap(),
            DexKind::PumpAmm
        );
    }
}
