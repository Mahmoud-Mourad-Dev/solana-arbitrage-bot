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

/// MAJOR-ASSET ALLOWLIST (S15A fix 1).
///
/// The authority checks below exist to reject **memecoin rug risk**: an unknown
/// mint whose deployer can mint supply or freeze our account can strand or
/// devalue our inventory mid-cycle. That reasoning does NOT apply to established
/// stablecoins, whose mint authority is the issuer's normal supply mechanism
/// (Circle for USDC, Tether for USDT) and is not a counterparty risk to a
/// two-hop arbitrage that holds the asset for one transaction.
///
/// S14B-3 excluded **every USDC route** because of this, which is precisely
/// where the observed profitable Meteora↔Whirlpool arbitrage happens (USDC
/// appears in 45/45 of the reconstructed transactions — see
/// `docs/forensic-route-recon-s15a.md`).
///
/// This list is EXPLICIT and deliberately tiny. It is not a general relaxation:
/// every mint not named here still faces the full screen. Adding to it is a
/// decision about issuer trust, not a technical convenience.
pub const MAJOR_ASSETS: &[(&str, &str)] = &[
    ("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v", "USDC"),
    ("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB", "USDT"),
];

/// Is this mint on the explicit major-asset allowlist?
pub fn major_asset(mint: &str) -> Option<&'static str> {
    MAJOR_ASSETS
        .iter()
        .find(|(m, _)| *m == mint)
        .map(|(_, sym)| *sym)
}

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

/// Screen a mint by its raw account bytes. `owner` is the account's owner
/// program id. Ok ⇒ safe to trade. This is the STRICT screen: it knows nothing
/// about allowlists and rejects any mint that can be minted or frozen.
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

/// Screen a mint for TRADING eligibility (S15A fix 1).
///
/// Identical to [`screen_mint`] except that a mint on the explicit
/// [`MAJOR_ASSETS`] allowlist is accepted despite carrying an issuer mint/freeze
/// authority. Structural checks are NEVER waived — even an allowlisted mint must
/// be a classic-SPL, initialized, base-layout mint owned by the Token program.
/// A Token-2022 or malformed account is rejected regardless of the allowlist.
pub fn screen_mint_for_trading(mint: &str, owner: &str, data: &[u8]) -> Result<(), MintReject> {
    match screen_mint(owner, data) {
        // Only the two ISSUER-AUTHORITY rejections may be waived, and only for
        // an explicitly allowlisted major asset.
        Err(MintReject::HasMintAuthority) | Err(MintReject::HasFreezeAuthority)
            if major_asset(mint).is_some() =>
        {
            Ok(())
        }
        other => other,
    }
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

    // ── S15A fix 1: major-asset allowlist (narrow, explicit, structural checks
    // never waived). ──

    const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    const USDT: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB";

    #[test]
    fn allowlist_is_exactly_usdc_and_usdt() {
        assert_eq!(major_asset(USDC), Some("USDC"));
        assert_eq!(major_asset(USDT), Some("USDT"));
        assert_eq!(
            MAJOR_ASSETS.len(),
            2,
            "allowlist must stay tiny and explicit"
        );
        assert_eq!(
            major_asset("So11111111111111111111111111111111111111112"),
            None
        );
        assert_eq!(
            major_asset("9BB6NFEcjBCtnNLFko2FqVQBq8HHM13kCyYcdQbgpump"),
            None
        );
    }

    #[test]
    fn usdc_shaped_mint_passes_trading_screen_but_fails_strict() {
        // USDC carries BOTH a mint and a freeze authority (Circle).
        let d = base_mint(true, true, true);
        assert_eq!(
            screen_mint(TOKEN_PROGRAM, &d),
            Err(MintReject::HasMintAuthority)
        );
        assert_eq!(screen_mint_for_trading(USDC, TOKEN_PROGRAM, &d), Ok(()));
        assert_eq!(screen_mint_for_trading(USDT, TOKEN_PROGRAM, &d), Ok(()));
    }

    #[test]
    fn allowlist_does_not_leak_to_other_mints() {
        // The SAME dangerous shape on a non-allowlisted mint is still rejected.
        let d = base_mint(true, true, true);
        assert_eq!(
            screen_mint_for_trading(
                "9BB6NFEcjBCtnNLFko2FqVQBq8HHM13kCyYcdQbgpump",
                TOKEN_PROGRAM,
                &d
            ),
            Err(MintReject::HasMintAuthority)
        );
    }

    #[test]
    fn allowlist_never_waives_structural_checks() {
        // Token-2022 stays rejected even for an allowlisted address.
        let d = base_mint(false, false, true);
        assert_eq!(
            screen_mint_for_trading(USDC, TOKEN_2022_PROGRAM, &d),
            Err(MintReject::Token2022)
        );
        // Extension-bearing layout stays rejected.
        let mut ext = base_mint(true, true, true);
        ext.extend_from_slice(&[0u8; 40]);
        assert!(matches!(
            screen_mint_for_trading(USDC, TOKEN_PROGRAM, &ext),
            Err(MintReject::NotBaseLayout { .. })
        ));
        // Uninitialized stays rejected.
        assert_eq!(
            screen_mint_for_trading(USDC, TOKEN_PROGRAM, &base_mint(true, true, false)),
            Err(MintReject::Uninitialized)
        );
        // Unknown owner program stays rejected.
        assert_eq!(
            screen_mint_for_trading(USDC, "SomeOtherProgram1111111111111111111111111111", &d),
            Err(MintReject::UnknownOwner)
        );
    }

    #[test]
    fn clean_mint_passes_both_screens_identically() {
        let d = base_mint(false, false, true);
        assert_eq!(screen_mint(TOKEN_PROGRAM, &d), Ok(()));
        assert_eq!(
            screen_mint_for_trading("AnyUnknownMint", TOKEN_PROGRAM, &d),
            Ok(())
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
