//! Test-only DEX stand-in. NOT for deployment.
//!
//! The arbitrage program forwards a hop's account slice `[..]` (minus the
//! program at index 0) and a swap-data blob. This mock ignores the swap
//! semantics and simply performs ONE SPL token transfer of `amount` from a
//! faucet account to the destination, where `amount` is read from the swap
//! data the arbitrage program built:
//!
//! - Raydium form `[9, amount u64, min_out u64]` -> amount = data[1..9]
//! - Whirlpool form `[disc(8), amount u64, ...]`  -> amount = data[8..16]
//!
//! Forwarded accounts (the mock's own instruction metas), in order:
//!   0: SPL token program
//!   1: faucet token account   (source, writable; authority = signer below)
//!   2: destination token acct (writable)
//!   3: authority              (signer; owner of both token accounts)
//!
//! Because the authority is the outer transaction signer (propagated through
//! the arbitrage program's CPI), no PDA signing is needed: the mock credits
//! the destination, letting the arbitrage program observe a real balance
//! increase and run its profit check.

use solana_program::{
    account_info::AccountInfo,
    entrypoint,
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    program::invoke,
    program_error::ProgramError,
    pubkey::Pubkey,
};

const TOKEN_PROGRAM: Pubkey =
    solana_program::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

entrypoint!(process_instruction);

fn process_instruction(
    _program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    // Extract the swap amount from whichever form the caller used.
    let amount = if !data.is_empty() && data[0] == 9 && data.len() >= 9 {
        u64::from_le_bytes(data[1..9].try_into().unwrap())
    } else if data.len() >= 16 {
        u64::from_le_bytes(data[8..16].try_into().unwrap())
    } else {
        return Err(ProgramError::InvalidInstructionData);
    };

    let token_program = &accounts[0];
    let faucet = &accounts[1];
    let destination = &accounts[2];
    let authority = &accounts[3];
    if token_program.key != &TOKEN_PROGRAM {
        return Err(ProgramError::IncorrectProgramId);
    }

    // SPL Token `Transfer` (tag 3) + amount u64 LE.
    let mut ix_data = Vec::with_capacity(9);
    ix_data.push(3u8);
    ix_data.extend_from_slice(&amount.to_le_bytes());
    let ix = Instruction {
        program_id: TOKEN_PROGRAM,
        accounts: vec![
            AccountMeta::new(*faucet.key, false),
            AccountMeta::new(*destination.key, false),
            AccountMeta::new_readonly(*authority.key, true),
        ],
        data: ix_data,
    };
    invoke(
        &ix,
        &[faucet.clone(), destination.clone(), authority.clone()],
    )
}
