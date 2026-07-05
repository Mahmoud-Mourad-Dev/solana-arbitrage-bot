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

use thiserror::Error;

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

/// Program ids as raw bytes-agnostic base58 strings (each crate converts to
/// its own Pubkey/Address type; keeping strings avoids a Solana dependency).
pub const RAYDIUM_V4_PROGRAM_STR: &str = "675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8";
pub const WHIRLPOOL_PROGRAM_STR: &str = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";
pub const TOKEN_PROGRAM_STR: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

/// Stable custom error codes surfaced as `ProgramError::Custom(code)`.
/// Codes are FROZEN — executors match on them for landed-tx forensics.
#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ArbError {
    #[error("malformed instruction data")]
    MalformedInstruction = 0,
    #[error("hop count must be 1..=4")]
    BadHopCount = 1,
    #[error("unknown dex tag")]
    UnknownDex = 2,
    #[error("hop account slice out of bounds")]
    AccountSliceOutOfBounds = 3,
    #[error("hop program id does not match declared dex")]
    InvalidDexProgram = 4,
    #[error("account is not a valid SPL token account")]
    InvalidTokenAccount = 5,
    #[error("token account not owned by authority")]
    TokenAccountOwnerMismatch = 6,
    #[error("arithmetic overflow")]
    ArithmeticOverflow = 7,
    #[error("cycle finished below required profit — reverting")]
    ProfitNotMet = 8,
    #[error("authority signature missing")]
    MissingSignature = 9,
    #[error("hop input amount is zero")]
    ZeroAmount = 10,
}

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
    /// executors match on Custom(code) for landed-tx forensics.
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
    }
}
