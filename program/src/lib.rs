//! Atomic cyclic-arbitrage execution program.
//!
//! Receives an ordered list of swap hops (Raydium AMM v4 / Orca Whirlpool),
//! executes them via CPI, and enforces the profit constraint:
//!
//! ```text
//! balance(base_token_account) after all hops
//!     >= balance before + min_profit
//! ```
//!
//! otherwise the whole transaction reverts — zero inventory risk, and any
//! in-transaction Jito tip reverts with it.
//!
//! ## Instruction data layout (little-endian, no Borsh — CU-lean)
//!
//! ```text
//! header (17 bytes):
//!   [0]      num_hops: u8            (1..=4)
//!   [1..9]   amount_in: u64          (raw units, first hop input)
//!   [9..17]  min_profit: u64         (raw units of the base token)
//! per hop (12 bytes each):
//!   [0]      dex: u8                 (0 = Raydium v4, 1 = Orca Whirlpool)
//!   [1]      num_accounts: u8        (length of this hop's account slice,
//!                                     INCLUDING the dex program at index 0)
//!   [2]      source_index: u8        (index within the hop slice of the
//!                                     user's SOURCE token account)
//!   [3]      flags: u8               (bit0 = a_to_b, Whirlpool only)
//!   [4..12]  min_amount_out: u64     (per-hop floor, forwarded to the DEX)
//! ```
//!
//! ## Account order
//!
//! ```text
//! 0: authority            (signer; owner of every user token account)
//! 1: base token account   (profit-checked SPL account, e.g. WSOL ATA)
//! 2..: hop slices, concatenated. Each slice = [dex_program, ...CPI accounts
//!      in the exact order the DEX expects].
//! ```
//!
//! Hops after the first swap the FULL balance of their source account, so
//! intermediate legs never strand tokens (intermediate ATAs start empty).

use solana_program::{
    account_info::AccountInfo,
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    program::invoke,
    program_error::ProgramError,
    pubkey,
    pubkey::Pubkey,
};
use thiserror::Error;

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);

pub const RAYDIUM_V4_PROGRAM: Pubkey = pubkey!("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8");
pub const WHIRLPOOL_PROGRAM: Pubkey = pubkey!("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc");
pub const TOKEN_PROGRAM: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

/// Whirlpool swap sqrt-price bounds (Q64.64). Passing the extreme in the
/// trade direction means "no price limit"; the per-hop min_amount_out and
/// the final profit check are the real guards.
pub const MIN_SQRT_PRICE_X64: u128 = 4_295_048_016;
pub const MAX_SQRT_PRICE_X64: u128 = 79_226_673_515_401_279_992_447_579_055;

/// Anchor sighash("global", "swap") for the Whirlpool program.
pub const WHIRLPOOL_SWAP_DISCRIMINATOR: [u8; 8] = [248, 198, 158, 145, 225, 117, 135, 200];
/// Raydium AMM v4 SwapBaseIn single-byte discriminator.
pub const RAYDIUM_SWAP_BASE_IN_TAG: u8 = 9;

pub const MAX_HOPS: usize = 4;
pub const HEADER_LEN: usize = 17;
pub const HOP_LEN: usize = 12;

#[derive(Error, Debug, Clone, Copy, PartialEq, Eq)]
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

impl From<ArbError> for ProgramError {
    fn from(e: ArbError) -> Self {
        ProgramError::Custom(e as u32)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DexKind {
    RaydiumV4 = 0,
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

/// Parse instruction data. Shared with the off-chain executor via
/// [`encode_instruction`] so both sides can never drift.
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

/// Read the `amount` field of an SPL token account with ownership checks.
#[inline]
fn token_amount(account: &AccountInfo) -> Result<u64, ArbError> {
    if account.owner != &TOKEN_PROGRAM {
        return Err(ArbError::InvalidTokenAccount);
    }
    let data = account
        .try_borrow_data()
        .map_err(|_| ArbError::InvalidTokenAccount)?;
    if data.len() < 72 {
        return Err(ArbError::InvalidTokenAccount);
    }
    Ok(u64::from_le_bytes(data[64..72].try_into().unwrap()))
}

/// The base account must be a token account whose `owner` field is the
/// transaction authority — prevents checking profit against a foreign
/// account.
fn check_user_token_account(account: &AccountInfo, authority: &Pubkey) -> Result<(), ArbError> {
    if account.owner != &TOKEN_PROGRAM {
        return Err(ArbError::InvalidTokenAccount);
    }
    let data = account
        .try_borrow_data()
        .map_err(|_| ArbError::InvalidTokenAccount)?;
    if data.len() < 72 {
        return Err(ArbError::InvalidTokenAccount);
    }
    if data[32..64] != authority.to_bytes() {
        return Err(ArbError::TokenAccountOwnerMismatch);
    }
    Ok(())
}

pub fn process_instruction(
    _program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    let params = parse_instruction(instruction_data)?;

    let (authority, base_token, hop_accounts) = match accounts {
        [authority, base_token, rest @ ..] => (authority, base_token, rest),
        _ => return Err(ProgramError::NotEnoughAccountKeys),
    };
    if !authority.is_signer {
        return Err(ArbError::MissingSignature.into());
    }
    check_user_token_account(base_token, authority.key)?;

    let starting_balance = token_amount(base_token)?;

    let mut cursor = 0usize;
    for (hop_index, hop) in params.hops.iter().enumerate() {
        let n = hop.num_accounts as usize;
        let slice = hop_accounts
            .get(cursor..cursor + n)
            .ok_or(ArbError::AccountSliceOutOfBounds)?;
        cursor += n;

        let dex_program = &slice[0];
        let expected = match hop.dex {
            DexKind::RaydiumV4 => &RAYDIUM_V4_PROGRAM,
            DexKind::OrcaWhirlpool => &WHIRLPOOL_PROGRAM,
        };
        if dex_program.key != expected || !dex_program.executable {
            return Err(ArbError::InvalidDexProgram.into());
        }

        // First hop trades the caller-specified size; later hops sweep the
        // full output of the previous leg from the user's source account.
        let amount_in = if hop_index == 0 {
            params.amount_in
        } else {
            token_amount(&slice[hop.source_index as usize])?
        };
        if amount_in == 0 {
            return Err(ArbError::ZeroAmount.into());
        }

        let data = match hop.dex {
            DexKind::RaydiumV4 => build_raydium_swap_data(amount_in, hop.min_amount_out),
            DexKind::OrcaWhirlpool => {
                build_whirlpool_swap_data(amount_in, hop.min_amount_out, hop.a_to_b)
            }
        };
        // Privileges are inherited verbatim from the outer transaction.
        let metas: Vec<AccountMeta> = slice[1..]
            .iter()
            .map(|a| AccountMeta {
                pubkey: *a.key,
                is_signer: a.is_signer,
                is_writable: a.is_writable,
            })
            .collect();
        invoke(
            &Instruction {
                program_id: *dex_program.key,
                accounts: metas,
                data,
            },
            slice,
        )?;
    }
    if cursor != hop_accounts.len() {
        // Trailing unconsumed accounts signal a malformed client — refuse.
        return Err(ArbError::AccountSliceOutOfBounds.into());
    }

    let final_balance = token_amount(base_token)?;
    let required = starting_balance
        .checked_add(params.min_profit)
        .ok_or(ArbError::ArithmeticOverflow)?;
    if final_balance < required {
        return Err(ArbError::ProfitNotMet.into());
    }
    Ok(())
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
        // zero hops
        let mut zero = vec![0u8; HEADER_LEN];
        zero[0] = 0;
        assert_eq!(parse_instruction(&zero), Err(ArbError::BadHopCount));
        // five hops
        let mut five = vec![0u8; HEADER_LEN + 5 * HOP_LEN];
        five[0] = 5;
        assert_eq!(parse_instruction(&five), Err(ArbError::BadHopCount));
        // truncated hop section
        let mut trunc = encode_instruction(&sample_params());
        trunc.pop();
        assert_eq!(
            parse_instruction(&trunc),
            Err(ArbError::MalformedInstruction)
        );
        // unknown dex tag
        let mut bad_dex = encode_instruction(&sample_params());
        bad_dex[HEADER_LEN] = 7;
        assert_eq!(parse_instruction(&bad_dex), Err(ArbError::UnknownDex));
        // source_index outside the slice
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
        assert_eq!(d[40], 1); // exact-in
        assert_eq!(d[41], 1); // a_to_b
        let d2 = build_whirlpool_swap_data(111, 222, false);
        assert_eq!(
            u128::from_le_bytes(d2[24..40].try_into().unwrap()),
            MAX_SQRT_PRICE_X64 - 1
        );
        assert_eq!(d2[41], 0);
    }
}
