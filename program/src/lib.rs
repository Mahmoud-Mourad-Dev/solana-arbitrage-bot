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
//! The instruction byte layout and account ordering are defined ONCE in
//! `arb-common` (`arb_common::ix`) and shared verbatim with the off-chain
//! executor. See that module for the full ABI documentation.
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

// Re-export the frozen ABI so existing downstream imports keep working.
pub use arb_common::ix::*;

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);

pub const RAYDIUM_V4_PROGRAM: Pubkey = pubkey!("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8");
pub const WHIRLPOOL_PROGRAM: Pubkey = pubkey!("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc");
pub const TOKEN_PROGRAM: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

/// `ArbError` lives in arb-common (no solana deps there), so the orphan
/// rule forbids `impl From` — a plain mapping fn keeps call sites terse.
#[inline]
fn arb_err(e: ArbError) -> ProgramError {
    ProgramError::Custom(e as u32)
}

/// Read the `amount` field of an SPL token account with ownership checks.
#[inline]
fn token_amount(account: &AccountInfo) -> Result<u64, ProgramError> {
    if account.owner != &TOKEN_PROGRAM {
        return Err(arb_err(ArbError::InvalidTokenAccount));
    }
    let data = account
        .try_borrow_data()
        .map_err(|_| arb_err(ArbError::InvalidTokenAccount))?;
    if data.len() < 72 {
        return Err(arb_err(ArbError::InvalidTokenAccount));
    }
    Ok(u64::from_le_bytes(data[64..72].try_into().unwrap()))
}

/// The base account must be a token account whose `owner` field is the
/// transaction authority — prevents checking profit against a foreign
/// account.
fn check_user_token_account(account: &AccountInfo, authority: &Pubkey) -> Result<(), ProgramError> {
    if account.owner != &TOKEN_PROGRAM {
        return Err(arb_err(ArbError::InvalidTokenAccount));
    }
    let data = account
        .try_borrow_data()
        .map_err(|_| arb_err(ArbError::InvalidTokenAccount))?;
    if data.len() < 72 {
        return Err(arb_err(ArbError::InvalidTokenAccount));
    }
    if data[32..64] != authority.to_bytes() {
        return Err(arb_err(ArbError::TokenAccountOwnerMismatch));
    }
    Ok(())
}

pub fn process_instruction(
    _program_id: &Pubkey,
    accounts: &[AccountInfo],
    instruction_data: &[u8],
) -> ProgramResult {
    let params = parse_instruction(instruction_data).map_err(arb_err)?;

    let (authority, base_token, hop_accounts) = match accounts {
        [authority, base_token, rest @ ..] => (authority, base_token, rest),
        _ => return Err(ProgramError::NotEnoughAccountKeys),
    };
    if !authority.is_signer {
        return Err(arb_err(ArbError::MissingSignature));
    }
    check_user_token_account(base_token, authority.key)?;

    let starting_balance = token_amount(base_token)?;

    let mut cursor = 0usize;
    for (hop_index, hop) in params.hops.iter().enumerate() {
        let n = hop.num_accounts as usize;
        let slice = hop_accounts
            .get(cursor..cursor + n)
            .ok_or(arb_err(ArbError::AccountSliceOutOfBounds))?;
        cursor += n;

        let dex_program = &slice[0];
        let expected = match hop.dex {
            DexKind::RaydiumV4 => &RAYDIUM_V4_PROGRAM,
            DexKind::OrcaWhirlpool => &WHIRLPOOL_PROGRAM,
        };
        if dex_program.key != expected || !dex_program.executable {
            return Err(arb_err(ArbError::InvalidDexProgram));
        }

        // First hop trades the caller-specified size; later hops sweep the
        // full output of the previous leg from the user's source account.
        let amount_in = if hop_index == 0 {
            params.amount_in
        } else {
            token_amount(&slice[hop.source_index as usize])?
        };
        if amount_in == 0 {
            return Err(arb_err(ArbError::ZeroAmount));
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
        return Err(arb_err(ArbError::AccountSliceOutOfBounds));
    }

    let final_balance = token_amount(base_token)?;
    let required = starting_balance
        .checked_add(params.min_profit)
        .ok_or(arb_err(ArbError::ArithmeticOverflow))?;
    if final_balance < required {
        return Err(arb_err(ArbError::ProfitNotMet));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The on-chain constants must match the base58 strings frozen in
    /// arb-common (which the executor uses to build its Pubkeys).
    #[test]
    fn program_ids_match_common() {
        assert_eq!(RAYDIUM_V4_PROGRAM.to_string(), RAYDIUM_V4_PROGRAM_STR);
        assert_eq!(WHIRLPOOL_PROGRAM.to_string(), WHIRLPOOL_PROGRAM_STR);
        assert_eq!(TOKEN_PROGRAM.to_string(), TOKEN_PROGRAM_STR);
    }
}
