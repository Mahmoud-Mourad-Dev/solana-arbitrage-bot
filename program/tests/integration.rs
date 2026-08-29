//! Acceptance-contract integration tests (mollusk-svm).
//!
//! These run against the COMPILED `.so` and are the behavioral contract the
//! Pinocchio rewrite must preserve byte-for-byte: exact custom error codes on
//! every revert path, and a real CPI + profit-check success/failure using the
//! `mock-dex` stand-in (which performs one SPL token transfer of the swapped
//! amount into the profit-checked base account).
//!
//! Same file, unchanged, must pass against both the solana-program build and
//! the Pinocchio build — that is what proves the migration preserved the ABI
//! and semantics. It also prints the success-path compute-unit cost so the
//! before/after CU can be compared.
//!
//! Requires the ELFs in `program/tests/fixtures/`:
//!   arbitrage_program.so, mock_dex.so   (staged by the build/verify script)

use arb_common::ix::{
    encode_instruction, DexKind, HopParams, IxParams, RAYDIUM_V4_PROGRAM_STR, TOKEN_PROGRAM_STR,
};
use mollusk_svm::program::loader_keys::LOADER_V3;
use mollusk_svm::Mollusk;
use solana_account::Account;
use solana_instruction::{AccountMeta, Instruction};
use solana_instruction_error::InstructionError;
use solana_pubkey::Pubkey;
use solana_rent::Rent;
use std::str::FromStr;

const WSOL_STR: &str = "So11111111111111111111111111111111111111112";
const SYSTEM_PROGRAM_STR: &str = "11111111111111111111111111111111";

fn pk(s: &str) -> Pubkey {
    Pubkey::from_str(s).unwrap()
}
fn raydium_program() -> Pubkey {
    pk(RAYDIUM_V4_PROGRAM_STR)
}
fn token_program() -> Pubkey {
    pk(TOKEN_PROGRAM_STR)
}

const START_BASE: u64 = 5_000_000;
const FAUCET: u64 = 10_000_000;
const AMOUNT_IN: u64 = 1_000_000;

/// Build a packed 165-byte SPL token account owned by the token program.
fn spl_token_account(mint: Pubkey, owner: Pubkey, amount: u64) -> Account {
    let mut data = vec![0u8; 165];
    data[0..32].copy_from_slice(mint.as_ref());
    data[32..64].copy_from_slice(owner.as_ref());
    data[64..72].copy_from_slice(&amount.to_le_bytes());
    data[108] = 1; // AccountState::Initialized
    Account {
        lamports: Rent::default().minimum_balance(165),
        data,
        owner: token_program(),
        executable: false,
        rent_epoch: 0,
    }
}

fn system_account(lamports: u64) -> Account {
    Account {
        lamports,
        data: vec![],
        owner: pk(SYSTEM_PROGRAM_STR),
        executable: false,
        rent_epoch: 0,
    }
}

fn executable_program_account(owner: Pubkey) -> Account {
    Account {
        lamports: 1,
        data: vec![],
        owner,
        executable: true,
        rent_epoch: 0,
    }
}

struct Fixture {
    mollusk: Mollusk,
    arb_program: Pubkey,
    authority: Pubkey,
    base: Pubkey,
    faucet: Pubkey,
}

fn setup() -> Fixture {
    let arb_program = Pubkey::new_unique();
    let mut mollusk = Mollusk::new(&arb_program, "arbitrage_program");
    // Real SPL token program (for the mock's inner transfer CPI).
    mollusk_svm_programs_token::token::add_program(&mut mollusk);
    // Mock DEX registered AT the Raydium v4 id so the arb program's program-id
    // check passes and mollusk executes the mock ELF on the CPI.
    mollusk.add_program(&raydium_program(), "mock_dex");

    Fixture {
        mollusk,
        arb_program,
        authority: Pubkey::new_unique(),
        base: Pubkey::new_unique(),
        faucet: Pubkey::new_unique(),
    }
}

/// One-hop instruction: base(WSOL) --mock swap--> base(WSOL), crediting
/// AMOUNT_IN into the base account. `dex_program` overrides the hop's program
/// (for the wrong-program test); `authority_signs` toggles the signer flag;
/// `base_owner` overrides the base token account's owner field.
fn build_case(
    f: &Fixture,
    ix_data: Vec<u8>,
    dex_program: Pubkey,
    authority_signs: bool,
    base_owner: Pubkey,
) -> (Instruction, Vec<(Pubkey, Account)>) {
    let (token_id, token_acct) = mollusk_svm_programs_token::token::keyed_account();

    // Hop slice: [dex_program, token_program, faucet, base(dest), authority].
    let metas = vec![
        AccountMeta::new_readonly(f.authority, authority_signs), // 0 authority
        AccountMeta::new(f.base, false),                         // 1 base (profit-checked)
        AccountMeta::new_readonly(dex_program, false),           // hop[0] dex program
        AccountMeta::new_readonly(token_id, false),              // hop[1] token program
        AccountMeta::new(f.faucet, false),                       // hop[2] faucet (source)
        AccountMeta::new(f.base, false),                         // hop[3] dest = base
        AccountMeta::new_readonly(f.authority, authority_signs), // hop[4] authority
    ];
    let ix = Instruction {
        program_id: f.arb_program,
        accounts: metas,
        data: ix_data,
    };

    let accounts = vec![
        (f.authority, system_account(1_000_000_000)),
        (
            f.base,
            spl_token_account(pk(WSOL_STR), base_owner, START_BASE),
        ),
        (dex_program, executable_program_account(LOADER_V3)),
        (token_id, token_acct),
        (
            f.faucet,
            spl_token_account(pk(WSOL_STR), f.authority, FAUCET),
        ),
    ];
    (ix, accounts)
}

/// Standard 1-hop Raydium-form data with the given profit floor.
fn one_hop_data(min_profit: u64) -> Vec<u8> {
    encode_instruction(&IxParams {
        amount_in: AMOUNT_IN,
        min_profit,
        hops: vec![HopParams {
            dex: DexKind::RaydiumV4,
            num_accounts: 5,
            source_index: 3,
            a_to_b: false,
            min_amount_out: 0,
        }],
    })
}

fn assert_custom(result: &mollusk_svm::result::InstructionResult, code: u32) {
    match &result.raw_result {
        Err(InstructionError::Custom(c)) => assert_eq!(*c, code, "wrong custom error code"),
        other => panic!("expected Custom({code}), got {other:?}"),
    }
}

// ── Success + profit enforcement (real CPI) ─────────────────────────────────

#[test]
fn successful_cycle_meets_profit_and_reports_cu() {
    let f = setup();
    let (ix, accounts) = build_case(
        &f,
        one_hop_data(AMOUNT_IN / 2),
        raydium_program(),
        true,
        f.authority,
    );
    let result = f.mollusk.process_instruction(&ix, &accounts);
    assert!(
        result.raw_result.is_ok(),
        "expected success, got {:?}",
        result.raw_result
    );
    // Base account must have grown by exactly AMOUNT_IN.
    let base = result
        .resulting_accounts
        .iter()
        .find(|(k, _)| *k == f.base)
        .expect("base account present");
    let final_amount = u64::from_le_bytes(base.1.data[64..72].try_into().unwrap());
    assert_eq!(final_amount, START_BASE + AMOUNT_IN);
    println!(
        "SUCCESS-PATH COMPUTE UNITS: {}",
        result.compute_units_consumed
    );
}

#[test]
fn profit_not_met_reverts() {
    let f = setup();
    // Demand more profit than the mock delivers (AMOUNT_IN) -> revert.
    let (ix, accounts) = build_case(
        &f,
        one_hop_data(AMOUNT_IN * 2),
        raydium_program(),
        true,
        f.authority,
    );
    let result = f.mollusk.process_instruction(&ix, &accounts);
    assert_custom(&result, 8); // ProfitNotMet
}

// ── B3: intermediate-minimum check (gap-analysis §F) ────────────────────────

/// Two mock hops: faucet --hop0--> intermediate --hop1--> base. Both hops run
/// the mock at the Raydium id. `hop0_min_out` is the independent floor B3
/// enforces on the intermediate account after hop 0.
fn build_two_hop(
    f: &Fixture,
    hop0_min_out: u64,
    min_profit: u64,
) -> (Instruction, Vec<(Pubkey, Account)>) {
    let (token_id, token_acct) = mollusk_svm_programs_token::token::keyed_account();
    let intermediate = Pubkey::new_unique();

    let data = encode_instruction(&IxParams {
        amount_in: AMOUNT_IN,
        min_profit,
        hops: vec![
            HopParams {
                dex: DexKind::RaydiumV4,
                num_accounts: 5,
                source_index: 3,
                a_to_b: false,
                min_amount_out: hop0_min_out,
            },
            HopParams {
                dex: DexKind::RaydiumV4,
                num_accounts: 5,
                source_index: 2, // intermediate within hop1 slice
                a_to_b: false,
                min_amount_out: 0,
            },
        ],
    });

    let metas = vec![
        AccountMeta::new_readonly(f.authority, true), // 0 authority
        AccountMeta::new(f.base, false),              // 1 base (profit-checked)
        // hop0 slice: faucet -> intermediate
        AccountMeta::new_readonly(raydium_program(), false),
        AccountMeta::new_readonly(token_id, false),
        AccountMeta::new(f.faucet, false),
        AccountMeta::new(intermediate, false),
        AccountMeta::new_readonly(f.authority, true),
        // hop1 slice: intermediate -> base
        AccountMeta::new_readonly(raydium_program(), false),
        AccountMeta::new_readonly(token_id, false),
        AccountMeta::new(intermediate, false),
        AccountMeta::new(f.base, false),
        AccountMeta::new_readonly(f.authority, true),
    ];
    let ix = Instruction {
        program_id: f.arb_program,
        accounts: metas,
        data,
    };
    let accounts = vec![
        (f.authority, system_account(1_000_000_000)),
        (
            f.base,
            spl_token_account(pk(WSOL_STR), f.authority, START_BASE),
        ),
        (raydium_program(), executable_program_account(LOADER_V3)),
        (token_id, token_acct),
        (
            f.faucet,
            spl_token_account(pk(WSOL_STR), f.authority, FAUCET),
        ),
        (
            intermediate,
            spl_token_account(pk(WSOL_STR), f.authority, 0),
        ),
    ];
    (ix, accounts)
}

#[test]
fn two_hop_passes_when_intermediate_clears_min() {
    let f = setup();
    // hop0 credits AMOUNT_IN into the intermediate; floor is met.
    let (ix, accounts) = build_two_hop(&f, AMOUNT_IN, 0);
    let result = f.mollusk.process_instruction(&ix, &accounts);
    assert!(
        result.raw_result.is_ok(),
        "expected success, got {:?}",
        result.raw_result
    );
    let base = result
        .resulting_accounts
        .iter()
        .find(|(k, _)| *k == f.base)
        .expect("base present");
    let final_amount = u64::from_le_bytes(base.1.data[64..72].try_into().unwrap());
    assert_eq!(final_amount, START_BASE + AMOUNT_IN);
}

#[test]
fn two_hop_reverts_when_intermediate_below_min() {
    let f = setup();
    // Demand more out of hop0 than the mock delivered (AMOUNT_IN). The FINAL
    // profit check would still pass (base grows by AMOUNT_IN, min_profit 0);
    // only the B3 intermediate check can catch this.
    let (ix, accounts) = build_two_hop(&f, AMOUNT_IN * 2, 0);
    let result = f.mollusk.process_instruction(&ix, &accounts);
    assert_custom(&result, 8); // ProfitNotMet, raised by the intermediate check
}

// ── Validation / revert paths (no CPI reached) ──────────────────────────────

#[test]
fn missing_signer_reverts() {
    let f = setup();
    let (ix, accounts) = build_case(&f, one_hop_data(0), raydium_program(), false, f.authority);
    let result = f.mollusk.process_instruction(&ix, &accounts);
    assert_custom(&result, 9); // MissingSignature
}

#[test]
fn token_owner_mismatch_reverts() {
    let f = setup();
    let stranger = Pubkey::new_unique();
    let (ix, accounts) = build_case(&f, one_hop_data(0), raydium_program(), true, stranger);
    let result = f.mollusk.process_instruction(&ix, &accounts);
    assert_custom(&result, 6); // TokenAccountOwnerMismatch
}

#[test]
fn wrong_dex_program_reverts() {
    let f = setup();
    let not_a_dex = Pubkey::new_unique();
    let (ix, accounts) = build_case(&f, one_hop_data(0), not_a_dex, true, f.authority);
    let result = f.mollusk.process_instruction(&ix, &accounts);
    assert_custom(&result, 4); // InvalidDexProgram
}

#[test]
fn malformed_instruction_reverts() {
    let f = setup();
    let (ix, accounts) = build_case(&f, vec![0u8; 5], raydium_program(), true, f.authority);
    let result = f.mollusk.process_instruction(&ix, &accounts);
    assert_custom(&result, 0); // MalformedInstruction
}

#[test]
fn bad_hop_count_reverts() {
    let f = setup();
    let mut data = vec![0u8; 17];
    data[0] = 5; // > MAX_HOPS
    let (ix, accounts) = build_case(&f, data, raydium_program(), true, f.authority);
    let result = f.mollusk.process_instruction(&ix, &accounts);
    assert_custom(&result, 1); // BadHopCount
}
