//! Orca Whirlpool real-swap parity: fixture schema, account provenance
//! validation, and offline replay (S14B-1). PURE: no RPC in this module.
//!
//! Evidence standard: the ONLY accepted parity proof is
//! `local swap_exact_in output == observed on-chain vault/destination delta`
//! on a snapshot whose freshness is proven (EXACT_SLOT / PRE_SLOT_MATCH).
//! Rust-vs-TypeScript equality is NOT evidence. Ambiguous snapshots are
//! rejected, never forced.

use crate::parsers::{decode_tick_array, decode_whirlpool, WhirlpoolDecoded};
use crate::tick_math::{sqrt_price_from_tick, swap_exact_in_detailed, Crossing, SwapDetail};
use crate::types::{tick_array_pda, tick_array_span, WHIRLPOOL_PROGRAM};
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

pub const WHIRLPOOL_PROGRAM_ID: &str = "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc";
pub const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const TOKEN_2022_PROGRAM: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
pub const FIXTURE_SCHEMA_VERSION: u32 = 1;

/// Oracle PDA: seeds ["oracle", whirlpool].
pub fn oracle_pda(whirlpool: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"oracle", whirlpool.as_ref()], &WHIRLPOOL_PROGRAM).0
}

// ─────────────────────────── fixture schema ───────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolRecord {
    pub address: String,
    pub program: String,
    pub whirlpools_config: String,
    pub token_mint_a: String,
    pub token_mint_b: String,
    pub vault_a: String,
    pub vault_b: String,
    pub tick_spacing: u16,
    pub fee_rate_ppm: u64,
    pub protocol_fee_rate: u16,
    pub oracle: String,
    pub token_program_a: String,
    pub token_program_b: String,
    pub any_token_2022: bool,
    pub transfer_fee_or_hook: bool,
    /// Market label, e.g. "WSOL/USDC".
    pub market: String,
}

/// One tick array's initialized ticks — enough to rebuild crossings exactly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TickArrayFx {
    pub pubkey: String,
    pub start_tick_index: i32,
    /// SHA-256 of the raw 9988-byte account at the snapshot.
    pub sha256: String,
    /// (tick_index, liquidity_net) for every initialized tick.
    pub initialized: Vec<(i32, i128)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolStateFx {
    pub sqrt_price_x64: u128,
    pub liquidity: u128,
    pub tick_current_index: i32,
    pub tick_spacing: u16,
    pub fee_rate_ppm: u64,
    /// SHA-256 of the raw 653-byte pool account at the snapshot.
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapFixture {
    pub sig: String,
    pub slot: u64,
    pub block_time: Option<i64>,
    /// "outer:<i>" or "inner:<outer>.<i>" — where the whirlpool ix sits.
    pub ix_location: String,
    pub cpi: bool,
    pub pool: String,
    /// true = token A in (price down); false = token B in (price up).
    pub a_to_b: bool,
    /// Human direction label, e.g. "USDC->WSOL".
    pub direction: String,
    pub amount_in: u64,
    /// Observed output = |vault-out delta| == destination token-account credit
    /// (both mints classic SPL, no transfer fee — asserted at capture).
    pub observed_out: u64,
    pub vault_a_pre: u64,
    pub vault_b_pre: u64,
    pub vault_a_post: u64,
    pub vault_b_post: u64,
    pub snapshot_slot: u64,
    pub slot_distance: u64,
    /// EXACT_SLOT | PRE_SLOT_MATCH (ambiguous fixtures are never written).
    pub freshness: String,
    pub pool_state: PoolStateFx,
    pub tick_arrays: Vec<TickArrayFx>,
    /// Whirlpool instruction account list (base58, in order).
    pub accounts: Vec<String>,
    /// Raw instruction data, hex.
    pub data_hex: String,
    pub compute_units: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhirlpoolFixtureFile {
    pub schema_version: u32,
    pub program: String,
    pub captured_at_commit: String,
    pub pools: Vec<PoolRecord>,
    pub swaps: Vec<SwapFixture>,
}

// ─────────────────────────── typed rejects ───────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WhirlpoolReject {
    PoolOwnerMismatch,
    PoolLayoutUndecodable,
    MintMismatch,
    VaultIdentityMismatch,
    VaultMintMismatch,
    VaultOwnerNotTokenProgram,
    VaultAuthorityMismatch,
    TickArrayWrongPool { index: usize },
    TickArrayBadStart { start: i32 },
    TickArrayUndecodable { index: usize },
    OracleMismatch,
    MintOwnerUnexpected,
    Token2022Unsupported,
    WrongDirectionMints,
    SnapshotStale,
    CoverageExceeded,
}

// ─────────────────────── provenance validation ───────────────────────

/// Validate the pool account itself: owner, decodability, expected mints.
pub fn validate_pool(
    pool_owner: &Pubkey,
    pool_data: &[u8],
    expect_mint_a: &Pubkey,
    expect_mint_b: &Pubkey,
) -> Result<WhirlpoolDecoded, WhirlpoolReject> {
    if *pool_owner != WHIRLPOOL_PROGRAM {
        return Err(WhirlpoolReject::PoolOwnerMismatch);
    }
    let d = decode_whirlpool(pool_data).ok_or(WhirlpoolReject::PoolLayoutUndecodable)?;
    if d.token_mint_a != *expect_mint_a || d.token_mint_b != *expect_mint_b {
        return Err(WhirlpoolReject::MintMismatch);
    }
    Ok(d)
}

/// Validate a vault token account against the decoded pool: identity equality
/// (cached vs decoded), token-program ownership, mint field, and that the
/// vault's authority is the whirlpool itself.
pub fn validate_vault(
    cached_vault: &Pubkey,
    decoded_vault: &Pubkey,
    vault_acc_owner: &Pubkey,
    vault_acc_data: &[u8],
    expect_mint: &Pubkey,
    whirlpool: &Pubkey,
) -> Result<(), WhirlpoolReject> {
    if cached_vault != decoded_vault {
        return Err(WhirlpoolReject::VaultIdentityMismatch);
    }
    let tok = Pubkey::from_str(TOKEN_PROGRAM).unwrap();
    let tok22 = Pubkey::from_str(TOKEN_2022_PROGRAM).unwrap();
    if *vault_acc_owner != tok && *vault_acc_owner != tok22 {
        return Err(WhirlpoolReject::VaultOwnerNotTokenProgram);
    }
    if vault_acc_data.len() < 72 {
        return Err(WhirlpoolReject::VaultMintMismatch);
    }
    let mint = Pubkey::new_from_array(vault_acc_data[0..32].try_into().unwrap());
    if mint != *expect_mint {
        return Err(WhirlpoolReject::VaultMintMismatch);
    }
    let auth = Pubkey::new_from_array(vault_acc_data[32..64].try_into().unwrap());
    if auth != *whirlpool {
        return Err(WhirlpoolReject::VaultAuthorityMismatch);
    }
    Ok(())
}

/// Validate that a tick-array account belongs to THIS whirlpool: PDA equality
/// for its decoded start index, valid start alignment, back-pointer match.
pub fn validate_tick_array(
    index: usize,
    array_pubkey: &Pubkey,
    array_data: &[u8],
    whirlpool: &Pubkey,
    tick_spacing: u16,
) -> Result<crate::parsers::TickArrayDecoded, WhirlpoolReject> {
    let d = decode_tick_array(array_data).ok_or(WhirlpoolReject::TickArrayUndecodable { index })?;
    let span = tick_array_span(tick_spacing);
    if d.start_tick_index % span != 0 {
        return Err(WhirlpoolReject::TickArrayBadStart {
            start: d.start_tick_index,
        });
    }
    // PDA + embedded back-pointer (last 32 bytes) must both match.
    if tick_array_pda(whirlpool, d.start_tick_index) != *array_pubkey {
        return Err(WhirlpoolReject::TickArrayWrongPool { index });
    }
    let back = Pubkey::new_from_array(
        array_data[crate::parsers::TICK_ARRAY_ACCOUNT_SIZE - 32..]
            .try_into()
            .unwrap(),
    );
    if back != *whirlpool {
        return Err(WhirlpoolReject::TickArrayWrongPool { index });
    }
    Ok(d)
}

/// Mint account owner check; Token-2022 with ANY extension is rejected in this
/// slice (transfer-fee/hook behavior would need separate proof).
pub fn validate_mint(mint_acc_owner: &Pubkey, mint_acc_len: usize) -> Result<(), WhirlpoolReject> {
    let tok = Pubkey::from_str(TOKEN_PROGRAM).unwrap();
    let tok22 = Pubkey::from_str(TOKEN_2022_PROGRAM).unwrap();
    if *mint_acc_owner == tok {
        return Ok(());
    }
    if *mint_acc_owner == tok22 {
        // Base mint length is 82; anything longer carries extensions.
        if mint_acc_len > 82 {
            return Err(WhirlpoolReject::Token2022Unsupported);
        }
        return Ok(());
    }
    Err(WhirlpoolReject::MintOwnerUnexpected)
}

// ─────────────────────────── offline replay ───────────────────────────

/// Rebuild the direction-ordered crossings + coverage limit from fixture tick
/// arrays (same rules as `WhirlpoolPool::build_crossings`).
pub fn crossings_from_fixture(
    fx_arrays: &[TickArrayFx],
    tick_current: i32,
    tick_spacing: u16,
    a_to_b: bool,
) -> (Vec<Crossing>, u128) {
    let span = tick_array_span(tick_spacing);
    let mut lowest = i32::MAX;
    let mut highest = i32::MIN;
    let mut items: Vec<(i32, i128)> = Vec::new();
    for ta in fx_arrays {
        lowest = lowest.min(ta.start_tick_index);
        highest = highest.max(ta.start_tick_index);
        for &(ti, net) in &ta.initialized {
            items.push((ti, net));
        }
    }
    let (mut sel, limit): (Vec<(i32, i128)>, u128) = if a_to_b {
        (
            items
                .into_iter()
                .filter(|(ti, _)| *ti <= tick_current)
                .collect(),
            sqrt_price_from_tick(lowest),
        )
    } else {
        (
            items
                .into_iter()
                .filter(|(ti, _)| *ti > tick_current)
                .collect(),
            sqrt_price_from_tick(highest + span),
        )
    };
    if a_to_b {
        sel.sort_by_key(|&(ti, _)| std::cmp::Reverse(ti));
    } else {
        sel.sort_by_key(|&(ti, _)| ti);
    }
    let crossings = sel
        .into_iter()
        .map(|(ti, net)| Crossing {
            sqrt_price: sqrt_price_from_tick(ti),
            liquidity_net: net,
        })
        .collect();
    (crossings, limit)
}

/// Replay a fixture through the exact local math. Returns the local detail;
/// the caller asserts `detail.amount_out == fx.observed_out`.
pub fn replay_fixture(fx: &SwapFixture) -> Option<SwapDetail> {
    let (crossings, limit) = crossings_from_fixture(
        &fx.tick_arrays,
        fx.pool_state.tick_current_index,
        fx.pool_state.tick_spacing,
        fx.a_to_b,
    );
    swap_exact_in_detailed(
        fx.pool_state.sqrt_price_x64,
        fx.pool_state.liquidity,
        fx.pool_state.fee_rate_ppm as u128,
        fx.a_to_b,
        fx.amount_in,
        &crossings,
        limit,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tick_math::{swap_exact_in, Q64};

    #[test]
    fn detailed_swap_matches_plain_swap() {
        let liq = 10u128.pow(14);
        for a_to_b in [true, false] {
            for amt in [10u64.pow(7), 10u64.pow(9)] {
                let cross = Crossing {
                    sqrt_price: sqrt_price_from_tick(if a_to_b { -40 } else { 40 }),
                    liquidity_net: (liq / 5) as i128,
                };
                let limit = sqrt_price_from_tick(if a_to_b { -8800 } else { 8800 });
                let plain = swap_exact_in(Q64, liq, 3000, a_to_b, amt, &[cross], limit);
                let det = swap_exact_in_detailed(Q64, liq, 3000, a_to_b, amt, &[cross], limit);
                assert_eq!(
                    plain,
                    det.map(|d| d.amount_out),
                    "amt={amt} a_to_b={a_to_b}"
                );
            }
        }
    }

    #[test]
    fn oracle_and_tick_array_pdas_are_deterministic() {
        let pool = Pubkey::new_unique();
        assert_eq!(oracle_pda(&pool), oracle_pda(&pool));
        assert_ne!(oracle_pda(&pool), oracle_pda(&Pubkey::new_unique()));
        assert_eq!(tick_array_pda(&pool, -22528), tick_array_pda(&pool, -22528));
    }

    #[test]
    fn vault_validation_rejects_each_defect() {
        let pool = Pubkey::new_unique();
        let mint = Pubkey::new_unique();
        let vault = Pubkey::new_unique();
        let tok = Pubkey::from_str(TOKEN_PROGRAM).unwrap();
        let mut data = vec![0u8; 165];
        data[0..32].copy_from_slice(mint.as_ref());
        data[32..64].copy_from_slice(pool.as_ref());
        // Valid.
        assert!(validate_vault(&vault, &vault, &tok, &data, &mint, &pool).is_ok());
        // Cached != decoded.
        assert_eq!(
            validate_vault(&Pubkey::new_unique(), &vault, &tok, &data, &mint, &pool),
            Err(WhirlpoolReject::VaultIdentityMismatch)
        );
        // Wrong owner program.
        assert_eq!(
            validate_vault(&vault, &vault, &Pubkey::new_unique(), &data, &mint, &pool),
            Err(WhirlpoolReject::VaultOwnerNotTokenProgram)
        );
        // Wrong mint.
        assert_eq!(
            validate_vault(&vault, &vault, &tok, &data, &Pubkey::new_unique(), &pool),
            Err(WhirlpoolReject::VaultMintMismatch)
        );
        // Wrong authority.
        assert_eq!(
            validate_vault(&vault, &vault, &tok, &data, &mint, &Pubkey::new_unique()),
            Err(WhirlpoolReject::VaultAuthorityMismatch)
        );
    }

    #[test]
    fn mint_validation_rejects_2022_with_extensions() {
        let tok = Pubkey::from_str(TOKEN_PROGRAM).unwrap();
        let tok22 = Pubkey::from_str(TOKEN_2022_PROGRAM).unwrap();
        assert!(validate_mint(&tok, 82).is_ok());
        assert!(validate_mint(&tok22, 82).is_ok());
        assert_eq!(
            validate_mint(&tok22, 300),
            Err(WhirlpoolReject::Token2022Unsupported)
        );
        assert_eq!(
            validate_mint(&Pubkey::new_unique(), 82),
            Err(WhirlpoolReject::MintOwnerUnexpected)
        );
    }
}
