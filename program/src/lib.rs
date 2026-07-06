//! Atomic cyclic-arbitrage execution program — Pinocchio (no_std) build.
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
//! The instruction byte layout, account ordering and custom error codes are
//! defined ONCE in `arb-common` (`arb_common::ix`) and shared verbatim with
//! the off-chain executor — the ABI is unchanged from the previous
//! `solana-program` build. This module is a behaviour-preserving port to
//! Pinocchio: no Anchor, no `solana-program`, `no_std` on-chain.
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

#![cfg_attr(target_os = "solana", no_std)]

extern crate alloc;

use alloc::vec::Vec;

use arb_common::ix::{
    build_raydium_swap_data, build_whirlpool_swap_data, parse_instruction, ArbError, DexKind,
    RAYDIUM_V4_PROGRAM_ID, TOKEN_PROGRAM_ID, WHIRLPOOL_PROGRAM_ID,
};
use pinocchio::account::AccountView;
use pinocchio::address::Address;
use pinocchio::cpi::{invoke_signed_with_slice, Signer};
use pinocchio::error::{ProgramError, ProgramResult};
use pinocchio::instruction::{InstructionAccount, InstructionView};

// Program ids built from arb-common's canonical bytes (single source of
// truth, base58-guarded there) — no base58 decoder needed on-chain.
pub const RAYDIUM_V4_PROGRAM: Address = Address::new_from_array(RAYDIUM_V4_PROGRAM_ID);
pub const WHIRLPOOL_PROGRAM: Address = Address::new_from_array(WHIRLPOOL_PROGRAM_ID);
pub const TOKEN_PROGRAM: Address = Address::new_from_array(TOKEN_PROGRAM_ID);

// On-chain entrypoint only. We install the pieces explicitly rather than via
// `entrypoint!` because that macro's `default_panic_handler!` assumes a
// std-linked toolchain; this crate is genuinely `no_std` on-chain, so it uses
// `nostd_panic_handler!` (a real `#[panic_handler]`) plus the bump allocator
// (needed for the swap-data / CPI-metas Vecs).
#[cfg(all(target_os = "solana", not(feature = "no-entrypoint")))]
mod program_entry {
    pinocchio::program_entrypoint!(super::process_instruction);
    pinocchio::default_allocator!();
    pinocchio::nostd_panic_handler!();
}

#[inline(always)]
fn err(e: ArbError) -> ProgramError {
    ProgramError::Custom(e as u32)
}

/// Read the `amount` field of an SPL token account with ownership checks.
#[inline]
fn token_amount(account: &AccountView) -> Result<u64, ProgramError> {
    if !account.owned_by(&TOKEN_PROGRAM) {
        return Err(err(ArbError::InvalidTokenAccount));
    }
    let data = account
        .try_borrow()
        .map_err(|_| err(ArbError::InvalidTokenAccount))?;
    if data.len() < 72 {
        return Err(err(ArbError::InvalidTokenAccount));
    }
    Ok(u64::from_le_bytes(data[64..72].try_into().unwrap()))
}

/// The base account must be a token account whose `owner` field is the
/// transaction authority — prevents checking profit against a foreign
/// account.
fn check_user_token_account(
    account: &AccountView,
    authority: &Address,
) -> Result<(), ProgramError> {
    if !account.owned_by(&TOKEN_PROGRAM) {
        return Err(err(ArbError::InvalidTokenAccount));
    }
    let data = account
        .try_borrow()
        .map_err(|_| err(ArbError::InvalidTokenAccount))?;
    if data.len() < 72 {
        return Err(err(ArbError::InvalidTokenAccount));
    }
    if data[32..64] != authority.as_array()[..] {
        return Err(err(ArbError::TokenAccountOwnerMismatch));
    }
    Ok(())
}

pub fn process_instruction(
    _program_id: &Address,
    accounts: &mut [AccountView],
    instruction_data: &[u8],
) -> ProgramResult {
    let params = parse_instruction(instruction_data).map_err(err)?;

    let accounts: &[AccountView] = accounts;
    let (authority, base_token, hop_accounts) = match accounts {
        [authority, base_token, rest @ ..] => (authority, base_token, rest),
        _ => return Err(ProgramError::NotEnoughAccountKeys),
    };
    if !authority.is_signer() {
        return Err(err(ArbError::MissingSignature));
    }
    check_user_token_account(base_token, authority.address())?;

    let starting_balance = token_amount(base_token)?;

    let no_signers: &[Signer] = &[];
    let mut cursor = 0usize;
    for (hop_index, hop) in params.hops.iter().enumerate() {
        let n = hop.num_accounts as usize;
        let slice = hop_accounts
            .get(cursor..cursor + n)
            .ok_or(err(ArbError::AccountSliceOutOfBounds))?;
        cursor += n;

        let dex_program = &slice[0];
        let expected = match hop.dex {
            DexKind::RaydiumV4 => &RAYDIUM_V4_PROGRAM,
            DexKind::OrcaWhirlpool => &WHIRLPOOL_PROGRAM,
        };
        if dex_program.address() != expected || !dex_program.executable() {
            return Err(err(ArbError::InvalidDexProgram));
        }

        // First hop trades the caller-specified size; later hops sweep the
        // full output of the previous leg from the user's source account.
        let amount_in = if hop_index == 0 {
            params.amount_in
        } else {
            token_amount(&slice[hop.source_index as usize])?
        };
        if amount_in == 0 {
            return Err(err(ArbError::ZeroAmount));
        }

        let data = match hop.dex {
            DexKind::RaydiumV4 => build_raydium_swap_data(amount_in, hop.min_amount_out),
            DexKind::OrcaWhirlpool => {
                build_whirlpool_swap_data(amount_in, hop.min_amount_out, hop.a_to_b)
            }
        };

        // Privileges inherited verbatim from the outer transaction.
        let cpi_accounts = &slice[1..];
        let mut metas: Vec<InstructionAccount> = Vec::with_capacity(cpi_accounts.len());
        for a in cpi_accounts {
            metas.push(InstructionAccount::new(
                a.address(),
                a.is_writable(),
                a.is_signer(),
            ));
        }
        let ix = InstructionView {
            program_id: dex_program.address(),
            data: &data,
            accounts: &metas,
        };
        invoke_signed_with_slice(&ix, cpi_accounts, no_signers)?;
    }
    if cursor != hop_accounts.len() {
        // Trailing unconsumed accounts signal a malformed client — refuse.
        return Err(err(ArbError::AccountSliceOutOfBounds));
    }

    let final_balance = token_amount(base_token)?;
    let required = starting_balance
        .checked_add(params.min_profit)
        .ok_or(err(ArbError::ArithmeticOverflow))?;
    if final_balance < required {
        return Err(err(ArbError::ProfitNotMet));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Address constants must equal the canonical arb-common bytes (which
    /// are themselves base58-guarded in arb-common's own test suite).
    #[test]
    fn program_ids_match_common() {
        assert_eq!(RAYDIUM_V4_PROGRAM.as_array(), &RAYDIUM_V4_PROGRAM_ID);
        assert_eq!(WHIRLPOOL_PROGRAM.as_array(), &WHIRLPOOL_PROGRAM_ID);
        assert_eq!(TOKEN_PROGRAM.as_array(), &TOKEN_PROGRAM_ID);
    }
}
