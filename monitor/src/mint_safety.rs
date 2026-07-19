//! SPL mint safety screening (S14B-3). PURE: decodes a mint account and applies
//! the strict cross-DEX safety filter — classic SPL only (no Token-2022), no
//! mint authority, no freeze authority, base 82-byte layout (no extensions).
//!
//! SPL Mint (82 bytes): mint_authority COption<Pubkey> @0 (4-byte tag + 32),
//! supply u64 @36, decimals u8 @44, is_initialized @45, freeze_authority
//! COption<Pubkey> @46 (4-byte tag + 32).

pub const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const TOKEN_2022_PROGRAM: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
pub const MINT_LEN: usize = 82;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MintReject {
    /// Owner is Token-2022 (extensions out of scope for this strategy).
    Token2022,
    /// Owner is not a recognized token program.
    UnknownOwner,
    /// Not exactly the 82-byte base layout (carries extensions / malformed).
    NotBaseLayout { len: usize },
    /// A mint authority is set (mintable — unsafe).
    HasMintAuthority,
    /// A freeze authority is set (freezable — unsafe).
    HasFreezeAuthority,
    /// Not initialized.
    Uninitialized,
}

/// Screen a mint. `owner_is_token2022` is the account owner program id classify;
/// `data` is the raw mint account bytes. Ok ⇒ safe classic SPL.
pub fn screen_mint(owner: &str, data: &[u8]) -> Result<(), MintReject> {
    match owner {
        o if o == TOKEN_2022_PROGRAM => return Err(MintReject::Token2022),
        o if o == TOKEN_PROGRAM => {}
        _ => return Err(MintReject::UnknownOwner),
    }
    if data.len() != MINT_LEN {
        return Err(MintReject::NotBaseLayout { len: data.len() });
    }
    if data[45] == 0 {
        return Err(MintReject::Uninitialized);
    }
    let mint_auth_tag = u32::from_le_bytes(data[0..4].try_into().unwrap());
    if mint_auth_tag != 0 {
        return Err(MintReject::HasMintAuthority);
    }
    let freeze_auth_tag = u32::from_le_bytes(data[46..50].try_into().unwrap());
    if freeze_auth_tag != 0 {
        return Err(MintReject::HasFreezeAuthority);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_mint(mint_auth: bool, freeze_auth: bool, init: bool) -> Vec<u8> {
        let mut d = vec![0u8; MINT_LEN];
        if mint_auth {
            d[0..4].copy_from_slice(&1u32.to_le_bytes());
        }
        d[44] = 6; // decimals
        d[45] = init as u8;
        if freeze_auth {
            d[46..50].copy_from_slice(&1u32.to_le_bytes());
        }
        d
    }

    #[test]
    fn clean_classic_spl_passes() {
        assert_eq!(
            screen_mint(TOKEN_PROGRAM, &base_mint(false, false, true)),
            Ok(())
        );
    }

    #[test]
    fn token2022_rejected() {
        assert_eq!(
            screen_mint(TOKEN_2022_PROGRAM, &base_mint(false, false, true)),
            Err(MintReject::Token2022)
        );
    }

    #[test]
    fn unknown_owner_rejected() {
        assert_eq!(
            screen_mint(
                "SomeOtherProgram1111111111111111111111111111",
                &base_mint(false, false, true)
            ),
            Err(MintReject::UnknownOwner)
        );
    }

    #[test]
    fn mint_authority_rejected() {
        assert_eq!(
            screen_mint(TOKEN_PROGRAM, &base_mint(true, false, true)),
            Err(MintReject::HasMintAuthority)
        );
    }

    #[test]
    fn freeze_authority_rejected() {
        assert_eq!(
            screen_mint(TOKEN_PROGRAM, &base_mint(false, true, true)),
            Err(MintReject::HasFreezeAuthority)
        );
    }

    #[test]
    fn extension_length_rejected() {
        let mut d = base_mint(false, false, true);
        d.extend_from_slice(&[0u8; 40]); // extensions → not base layout
        assert_eq!(
            screen_mint(TOKEN_PROGRAM, &d),
            Err(MintReject::NotBaseLayout { len: 122 })
        );
    }

    #[test]
    fn uninitialized_rejected() {
        assert_eq!(
            screen_mint(TOKEN_PROGRAM, &base_mint(false, false, false)),
            Err(MintReject::Uninitialized)
        );
    }

    #[test]
    fn known_safe_mints_would_pass_shape() {
        // USDC/USDT are classic SPL with no mint/freeze authority disabled?
        // (USDC HAS a mint+freeze authority — so it must be REJECTED by this
        // strict filter. This documents that the wide filter is stricter than
        // the S14B-2 hardcoded USDC/USDT set.)
        let usdc_like = base_mint(true, true, true);
        assert_eq!(
            screen_mint(TOKEN_PROGRAM, &usdc_like),
            Err(MintReject::HasMintAuthority)
        );
    }
}
