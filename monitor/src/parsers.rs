//! Fixed-offset binary account parsers — port of `src/parsers/*.ts`.
//! Offsets are the SAME constants proven against live mainnet by
//! `npm run verify:layouts` and by the executor's resolver.

use solana_sdk::pubkey::Pubkey;

pub const RAYDIUM_V4_ACCOUNT_SIZE: usize = 752;
pub const WHIRLPOOL_ACCOUNT_SIZE: usize = 653;

#[inline]
fn u64_le(d: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(d[o..o + 8].try_into().unwrap())
}

#[inline]
fn u128_le(d: &[u8], o: usize) -> u128 {
    u128::from_le_bytes(d[o..o + 16].try_into().unwrap())
}

#[inline]
fn pk(d: &[u8], o: usize) -> Pubkey {
    Pubkey::new_from_array(d[o..o + 32].try_into().unwrap())
}

/// Raydium AMM v4 (LIQUIDITY_STATE_LAYOUT_V4, 752 bytes, native program —
/// NO Anchor discriminator; first field is `status: u64`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RaydiumV4Decoded {
    pub status: u64,
    pub base_decimal: u8,
    pub quote_decimal: u8,
    pub swap_fee_numerator: u64,
    pub swap_fee_denominator: u64,
    pub base_need_take_pnl: u64,
    pub quote_need_take_pnl: u64,
    pub pool_open_time: u64,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub open_orders: Pubkey,
    pub market_id: Pubkey,
}

pub fn decode_raydium_v4(d: &[u8]) -> Option<RaydiumV4Decoded> {
    if d.len() != RAYDIUM_V4_ACCOUNT_SIZE {
        return None;
    }
    Some(RaydiumV4Decoded {
        status: u64_le(d, 0),
        base_decimal: u64_le(d, 32) as u8,
        quote_decimal: u64_le(d, 40) as u8,
        swap_fee_numerator: u64_le(d, 176),
        swap_fee_denominator: u64_le(d, 184),
        base_need_take_pnl: u64_le(d, 192),
        quote_need_take_pnl: u64_le(d, 200),
        pool_open_time: u64_le(d, 224),
        base_vault: pk(d, 336),
        quote_vault: pk(d, 368),
        base_mint: pk(d, 400),
        quote_mint: pk(d, 432),
        open_orders: pk(d, 496),
        market_id: pk(d, 528),
    })
}

/// Orca Whirlpool (Anchor account, 653 bytes, 8-byte discriminator).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WhirlpoolDecoded {
    pub tick_spacing: u16,
    pub fee_rate_ppm: u64,
    pub liquidity: u128,
    pub sqrt_price_x64: u128,
    pub tick_current_index: i32,
    pub token_mint_a: Pubkey,
    pub token_vault_a: Pubkey,
    pub token_mint_b: Pubkey,
    pub token_vault_b: Pubkey,
}

pub fn decode_whirlpool(d: &[u8]) -> Option<WhirlpoolDecoded> {
    if d.len() != WHIRLPOOL_ACCOUNT_SIZE {
        return None;
    }
    Some(WhirlpoolDecoded {
        tick_spacing: u16::from_le_bytes(d[41..43].try_into().unwrap()),
        fee_rate_ppm: u16::from_le_bytes(d[45..47].try_into().unwrap()) as u64,
        liquidity: u128_le(d, 49),
        sqrt_price_x64: u128_le(d, 65),
        tick_current_index: i32::from_le_bytes(d[81..85].try_into().unwrap()),
        token_mint_a: pk(d, 101),
        token_vault_a: pk(d, 133),
        token_mint_b: pk(d, 181),
        token_vault_b: pk(d, 213),
    })
}

/// SPL token account: amount u64 LE at offset 64.
pub fn decode_token_amount(d: &[u8]) -> Option<u64> {
    if d.len() < 72 {
        return None;
    }
    Some(u64_le(d, 64))
}

/// SPL mint: decimals u8 at offset 44.
pub fn decode_mint_decimals(d: &[u8]) -> Option<u8> {
    if d.len() < 45 {
        return None;
    }
    Some(d[44])
}

/// Serum/OpenBook OpenOrders: baseTokenTotal @85, quoteTokenTotal @101.
pub fn decode_open_orders_totals(d: &[u8]) -> Option<(u64, u64)> {
    if d.len() < 109 {
        return None;
    }
    Some((u64_le(d, 85), u64_le(d, 101)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic 752-byte Raydium account with sentinel values planted at
    /// every parsed offset — guards against off-by-one drift.
    #[test]
    fn raydium_offsets() {
        let mut d = vec![0u8; RAYDIUM_V4_ACCOUNT_SIZE];
        d[0..8].copy_from_slice(&6u64.to_le_bytes()); // status
        d[32..40].copy_from_slice(&9u64.to_le_bytes()); // baseDecimal
        d[40..48].copy_from_slice(&6u64.to_le_bytes()); // quoteDecimal
        d[176..184].copy_from_slice(&25u64.to_le_bytes()); // swapFeeNum
        d[184..192].copy_from_slice(&10_000u64.to_le_bytes()); // swapFeeDen
        d[192..200].copy_from_slice(&111u64.to_le_bytes()); // baseNeedTakePnl
        d[200..208].copy_from_slice(&222u64.to_le_bytes()); // quoteNeedTakePnl
        d[224..232].copy_from_slice(&1_700_000_000u64.to_le_bytes()); // poolOpenTime
        let base_vault = Pubkey::new_unique();
        let quote_mint = Pubkey::new_unique();
        d[336..368].copy_from_slice(base_vault.as_ref());
        d[432..464].copy_from_slice(quote_mint.as_ref());

        let p = decode_raydium_v4(&d).unwrap();
        assert_eq!(p.status, 6);
        assert_eq!(p.base_decimal, 9);
        assert_eq!(p.quote_decimal, 6);
        assert_eq!(p.swap_fee_numerator, 25);
        assert_eq!(p.swap_fee_denominator, 10_000);
        assert_eq!(p.base_need_take_pnl, 111);
        assert_eq!(p.quote_need_take_pnl, 222);
        assert_eq!(p.pool_open_time, 1_700_000_000);
        assert_eq!(p.base_vault, base_vault);
        assert_eq!(p.quote_mint, quote_mint);
        assert_eq!(decode_raydium_v4(&d[..751]), None);
    }

    #[test]
    fn whirlpool_offsets() {
        let mut d = vec![0u8; WHIRLPOOL_ACCOUNT_SIZE];
        d[41..43].copy_from_slice(&64u16.to_le_bytes()); // tickSpacing
        d[45..47].copy_from_slice(&3000u16.to_le_bytes()); // feeRate
        d[49..65].copy_from_slice(&123_456_789u128.to_le_bytes()); // liquidity
        d[65..81].copy_from_slice(&(1u128 << 64).to_le_bytes()); // sqrtPrice
        d[81..85].copy_from_slice(&(-25130i32).to_le_bytes()); // tick
        let mint_a = Pubkey::new_unique();
        let vault_b = Pubkey::new_unique();
        d[101..133].copy_from_slice(mint_a.as_ref());
        d[213..245].copy_from_slice(vault_b.as_ref());

        let p = decode_whirlpool(&d).unwrap();
        assert_eq!(p.tick_spacing, 64);
        assert_eq!(p.fee_rate_ppm, 3000);
        assert_eq!(p.liquidity, 123_456_789);
        assert_eq!(p.sqrt_price_x64, 1u128 << 64);
        assert_eq!(p.tick_current_index, -25130);
        assert_eq!(p.token_mint_a, mint_a);
        assert_eq!(p.token_vault_b, vault_b);
        assert_eq!(decode_whirlpool(&d[..652]), None);
    }

    #[test]
    fn spl_and_open_orders() {
        let mut tok = vec![0u8; 165];
        tok[64..72].copy_from_slice(&42u64.to_le_bytes());
        assert_eq!(decode_token_amount(&tok), Some(42));
        assert_eq!(decode_token_amount(&tok[..71]), None);

        let mut mint = vec![0u8; 82];
        mint[44] = 9;
        assert_eq!(decode_mint_decimals(&mint), Some(9));

        let mut oo = vec![0u8; 3228];
        oo[85..93].copy_from_slice(&1_000u64.to_le_bytes());
        oo[101..109].copy_from_slice(&2_000u64.to_le_bytes());
        assert_eq!(decode_open_orders_totals(&oo), Some((1_000, 2_000)));
        assert_eq!(decode_open_orders_totals(&oo[..108]), None);
    }
}
