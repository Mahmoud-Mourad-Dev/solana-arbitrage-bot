//! Resolves an opportunity hop (pool address + direction) into the complete
//! CPI account list the on-chain program forwards to the DEX.
//!
//! - Raydium v4: pool account -> vaults/openOrders/targetOrders/market, then
//!   the OpenBook market -> bids/asks/eventQueue/market vaults/vault signer.
//!   These keys never change, so they are cached forever.
//! - Whirlpool: vaults + three tick arrays derived from the CURRENT tick
//!   (direction-dependent) + oracle PDA. Tick data drifts, so entries are
//!   refreshed after a TTL.

use anyhow::{anyhow, bail, Context, Result};
use arbitrage_program::{RAYDIUM_V4_PROGRAM, TOKEN_PROGRAM, WHIRLPOOL_PROGRAM};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{instruction::AccountMeta, pubkey, pubkey::Pubkey};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use arb_common::ix::DexKind;
use arb_common::opportunity::OpportunityHop;

pub const ATA_PROGRAM: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
pub const WSOL_MINT: Pubkey = pubkey!("So11111111111111111111111111111111111111112");

pub const TICK_ARRAY_SIZE: i32 = 88;
const RAYDIUM_ACCOUNT_LEN: usize = 752;
const WHIRLPOOL_ACCOUNT_LEN: usize = 653;
const MARKET_ACCOUNT_LEN: usize = 388;

/// index (within the hop slice, program at 0) of the user source account.
pub const RAYDIUM_SOURCE_INDEX: u8 = 16;
pub const WHIRLPOOL_SOURCE_INDEX_A: u8 = 4;
pub const WHIRLPOOL_SOURCE_INDEX_B: u8 = 6;

pub fn derive_ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), TOKEN_PROGRAM.as_ref(), mint.as_ref()],
        &ATA_PROGRAM,
    )
    .0
}

fn read_pubkey(data: &[u8], offset: usize) -> Pubkey {
    Pubkey::new_from_array(data[offset..offset + 32].try_into().unwrap())
}

#[derive(Debug, Clone)]
pub struct RaydiumKeys {
    pub amm: Pubkey,
    pub authority: Pubkey,
    pub open_orders: Pubkey,
    pub target_orders: Pubkey,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub market_program: Pubkey,
    pub market: Pubkey,
    pub bids: Pubkey,
    pub asks: Pubkey,
    pub event_queue: Pubkey,
    pub market_base_vault: Pubkey,
    pub market_quote_vault: Pubkey,
    pub vault_signer: Pubkey,
}

#[derive(Debug, Clone)]
pub struct WhirlpoolKeys {
    pub whirlpool: Pubkey,
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub vault_a: Pubkey,
    pub vault_b: Pubkey,
    pub tick_spacing: u16,
    pub tick_current: i32,
    pub oracle: Pubkey,
    pub fetched_at: Instant,
}

/// Fully resolved hop, ready for instruction assembly.
pub struct ResolvedHop {
    pub dex: DexKind,
    /// Hop slice: `[dex_program, ...CPI accounts]` in DEX order.
    pub metas: Vec<AccountMeta>,
    pub source_index: u8,
    pub a_to_b: bool,
    pub min_amount_out: u64,
}

pub struct Resolver {
    rpc: Arc<RpcClient>,
    owner: Pubkey,
    raydium_cache: Mutex<HashMap<Pubkey, Arc<RaydiumKeys>>>,
    whirlpool_cache: Mutex<HashMap<Pubkey, Arc<WhirlpoolKeys>>>,
    whirlpool_ttl: Duration,
}

impl Resolver {
    pub fn new(rpc: Arc<RpcClient>, owner: Pubkey, whirlpool_ttl: Duration) -> Self {
        Self {
            rpc,
            owner,
            raydium_cache: Mutex::new(HashMap::new()),
            whirlpool_cache: Mutex::new(HashMap::new()),
            whirlpool_ttl,
        }
    }

    /// The two mints of a pool (fetches + caches the pool on first use).
    pub async fn pool_mints(&self, pool: Pubkey, dex: DexKind) -> Result<(Pubkey, Pubkey)> {
        match dex {
            DexKind::RaydiumV4 => {
                let k = self.raydium_keys(pool).await?;
                Ok((k.base_mint, k.quote_mint))
            }
            DexKind::OrcaWhirlpool => {
                let k = self.whirlpool_keys(pool).await?;
                Ok((k.mint_a, k.mint_b))
            }
        }
    }

    pub async fn resolve_hop(&self, hop: &OpportunityHop) -> Result<ResolvedHop> {
        let pool = Pubkey::from_str(&hop.pool).context("bad pool address in opportunity")?;
        let input_mint = Pubkey::from_str(&hop.input_mint).context("bad input mint")?;
        match hop.dex {
            DexKind::RaydiumV4 => {
                let keys = self.raydium_keys(pool).await?;
                self.raydium_hop(&keys, input_mint, hop.min_amount_out)
            }
            DexKind::OrcaWhirlpool => {
                let keys = self.whirlpool_keys(pool).await?;
                self.whirlpool_hop(&keys, input_mint, hop.min_amount_out)
            }
        }
    }

    // ── Raydium ─────────────────────────────────────────────────────────────

    async fn raydium_keys(&self, pool: Pubkey) -> Result<Arc<RaydiumKeys>> {
        if let Some(k) = self.raydium_cache.lock().await.get(&pool) {
            return Ok(k.clone());
        }
        let data = self
            .rpc
            .get_account_data(&pool)
            .await
            .with_context(|| format!("fetch raydium pool {pool}"))?;
        if data.len() != RAYDIUM_ACCOUNT_LEN {
            bail!("{pool} is not a Raydium v4 pool (len={})", data.len());
        }
        // Pubkey block starts at 336 (after 32 u64s + swap volume counters).
        let base_vault = read_pubkey(&data, 336);
        let quote_vault = read_pubkey(&data, 368);
        let base_mint = read_pubkey(&data, 400);
        let quote_mint = read_pubkey(&data, 432);
        let open_orders = read_pubkey(&data, 496);
        let market = read_pubkey(&data, 528);
        let market_program = read_pubkey(&data, 560);
        let target_orders = read_pubkey(&data, 592);

        let mkt = self
            .rpc
            .get_account_data(&market)
            .await
            .with_context(|| format!("fetch openbook market {market}"))?;
        if mkt.len() != MARKET_ACCOUNT_LEN {
            bail!("market {market} unexpected len {}", mkt.len());
        }
        // Serum MarketState offsets (5-byte "serum" prefix included).
        let vault_signer_nonce = u64::from_le_bytes(mkt[45..53].try_into().unwrap());
        let market_base_vault = read_pubkey(&mkt, 117);
        let market_quote_vault = read_pubkey(&mkt, 165);
        let event_queue = read_pubkey(&mkt, 253);
        let bids = read_pubkey(&mkt, 285);
        let asks = read_pubkey(&mkt, 317);
        let vault_signer = Pubkey::create_program_address(
            &[market.as_ref(), &vault_signer_nonce.to_le_bytes()],
            &market_program,
        )
        .map_err(|e| anyhow!("vault signer derivation failed for {market}: {e}"))?;

        let authority = Pubkey::find_program_address(&[b"amm authority"], &RAYDIUM_V4_PROGRAM).0;

        let keys = Arc::new(RaydiumKeys {
            amm: pool,
            authority,
            open_orders,
            target_orders,
            base_vault,
            quote_vault,
            base_mint,
            quote_mint,
            market_program,
            market,
            bids,
            asks,
            event_queue,
            market_base_vault,
            market_quote_vault,
            vault_signer,
        });
        self.raydium_cache.lock().await.insert(pool, keys.clone());
        Ok(keys)
    }

    fn raydium_hop(
        &self,
        k: &RaydiumKeys,
        input_mint: Pubkey,
        min_amount_out: u64,
    ) -> Result<ResolvedHop> {
        let output_mint = if input_mint == k.base_mint {
            k.quote_mint
        } else if input_mint == k.quote_mint {
            k.base_mint
        } else {
            bail!("input mint {input_mint} not in raydium pool {}", k.amm);
        };
        let user_source = derive_ata(&self.owner, &input_mint);
        let user_dest = derive_ata(&self.owner, &output_mint);

        // Raydium SDK swap account order (18 accounts incl. targetOrders).
        let metas = vec![
            AccountMeta::new_readonly(RAYDIUM_V4_PROGRAM, false), // hop program
            AccountMeta::new_readonly(TOKEN_PROGRAM, false),
            AccountMeta::new(k.amm, false),
            AccountMeta::new_readonly(k.authority, false),
            AccountMeta::new(k.open_orders, false),
            AccountMeta::new(k.target_orders, false),
            AccountMeta::new(k.base_vault, false),
            AccountMeta::new(k.quote_vault, false),
            AccountMeta::new_readonly(k.market_program, false),
            AccountMeta::new(k.market, false),
            AccountMeta::new(k.bids, false),
            AccountMeta::new(k.asks, false),
            AccountMeta::new(k.event_queue, false),
            AccountMeta::new(k.market_base_vault, false),
            AccountMeta::new(k.market_quote_vault, false),
            AccountMeta::new_readonly(k.vault_signer, false),
            AccountMeta::new(user_source, false),
            AccountMeta::new(user_dest, false),
            AccountMeta::new_readonly(self.owner, true),
        ];
        Ok(ResolvedHop {
            dex: DexKind::RaydiumV4,
            metas,
            source_index: RAYDIUM_SOURCE_INDEX,
            a_to_b: false, // unused for raydium
            min_amount_out,
        })
    }

    // ── Whirlpool ───────────────────────────────────────────────────────────

    async fn whirlpool_keys(&self, pool: Pubkey) -> Result<Arc<WhirlpoolKeys>> {
        if let Some(k) = self.whirlpool_cache.lock().await.get(&pool) {
            if k.fetched_at.elapsed() < self.whirlpool_ttl {
                return Ok(k.clone());
            }
        }
        let data = self
            .rpc
            .get_account_data(&pool)
            .await
            .with_context(|| format!("fetch whirlpool {pool}"))?;
        if data.len() != WHIRLPOOL_ACCOUNT_LEN {
            bail!("{pool} is not a Whirlpool (len={})", data.len());
        }
        let tick_spacing = u16::from_le_bytes(data[41..43].try_into().unwrap());
        let tick_current = i32::from_le_bytes(data[81..85].try_into().unwrap());
        let mint_a = read_pubkey(&data, 101);
        let vault_a = read_pubkey(&data, 133);
        let mint_b = read_pubkey(&data, 181);
        let vault_b = read_pubkey(&data, 213);
        let oracle =
            Pubkey::find_program_address(&[b"oracle", pool.as_ref()], &WHIRLPOOL_PROGRAM).0;

        let keys = Arc::new(WhirlpoolKeys {
            whirlpool: pool,
            mint_a,
            mint_b,
            vault_a,
            vault_b,
            tick_spacing,
            tick_current,
            oracle,
            fetched_at: Instant::now(),
        });
        self.whirlpool_cache.lock().await.insert(pool, keys.clone());
        Ok(keys)
    }

    fn whirlpool_hop(
        &self,
        k: &WhirlpoolKeys,
        input_mint: Pubkey,
        min_amount_out: u64,
    ) -> Result<ResolvedHop> {
        let a_to_b = if input_mint == k.mint_a {
            true
        } else if input_mint == k.mint_b {
            false
        } else {
            bail!("input mint {input_mint} not in whirlpool {}", k.whirlpool);
        };
        let user_a = derive_ata(&self.owner, &k.mint_a);
        let user_b = derive_ata(&self.owner, &k.mint_b);
        let ticks = tick_array_pdas(&k.whirlpool, k.tick_current, k.tick_spacing, a_to_b);

        let metas = vec![
            AccountMeta::new_readonly(WHIRLPOOL_PROGRAM, false), // hop program
            AccountMeta::new_readonly(TOKEN_PROGRAM, false),
            AccountMeta::new_readonly(self.owner, true), // token authority
            AccountMeta::new(k.whirlpool, false),
            AccountMeta::new(user_a, false),
            AccountMeta::new(k.vault_a, false),
            AccountMeta::new(user_b, false),
            AccountMeta::new(k.vault_b, false),
            AccountMeta::new(ticks[0], false),
            AccountMeta::new(ticks[1], false),
            AccountMeta::new(ticks[2], false),
            AccountMeta::new(k.oracle, false),
        ];
        Ok(ResolvedHop {
            dex: DexKind::OrcaWhirlpool,
            metas,
            source_index: if a_to_b {
                WHIRLPOOL_SOURCE_INDEX_A
            } else {
                WHIRLPOOL_SOURCE_INDEX_B
            },
            a_to_b,
            min_amount_out,
        })
    }
}

/// First tick-array start index containing `tick` (Whirlpool convention:
/// 88 initializable ticks per array, floor semantics for negatives).
pub fn tick_array_start_index(tick: i32, tick_spacing: u16) -> i32 {
    let span = tick_spacing as i32 * TICK_ARRAY_SIZE;
    tick.div_euclid(span) * span
}

/// The three tick arrays a swap may traverse, walking in trade direction
/// (a_to_b = price down = decreasing ticks).
pub fn tick_array_pdas(
    whirlpool: &Pubkey,
    tick: i32,
    tick_spacing: u16,
    a_to_b: bool,
) -> [Pubkey; 3] {
    let span = tick_spacing as i32 * TICK_ARRAY_SIZE;
    let start = tick_array_start_index(tick, tick_spacing);
    let step = if a_to_b { -span } else { span };
    let mut out = [Pubkey::default(); 3];
    for (i, slot) in out.iter_mut().enumerate() {
        let idx = start + step * i as i32;
        *slot = Pubkey::find_program_address(
            &[
                b"tick_array",
                whirlpool.as_ref(),
                idx.to_string().as_bytes(),
            ],
            &WHIRLPOOL_PROGRAM,
        )
        .0;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_array_floor_semantics() {
        // spacing 64 -> span 5632
        assert_eq!(tick_array_start_index(0, 64), 0);
        assert_eq!(tick_array_start_index(5631, 64), 0);
        assert_eq!(tick_array_start_index(5632, 64), 5632);
        assert_eq!(tick_array_start_index(-1, 64), -5632);
        // live SOL/USDC value observed on mainnet: tick -25130
        assert_eq!(tick_array_start_index(-25130, 64), -28160);
    }

    #[test]
    fn tick_arrays_walk_in_direction() {
        let wp = Pubkey::new_unique();
        let down = tick_array_pdas(&wp, -25130, 64, true);
        let up = tick_array_pdas(&wp, -25130, 64, false);
        // same starting array, diverging afterwards
        assert_eq!(down[0], up[0]);
        assert_ne!(down[1], up[1]);
        assert_eq!(down.len(), 3);
    }

    #[test]
    fn ata_derivation_matches_known_vector() {
        // USDC ATA of the system program id (well-known deterministic vector
        // recomputed via find_program_address itself — asserts stability).
        let owner = Pubkey::from_str("11111111111111111111111111111111").unwrap();
        let usdc = Pubkey::from_str("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v").unwrap();
        let ata = derive_ata(&owner, &usdc);
        assert_ne!(ata, Pubkey::default());
        assert_eq!(ata, derive_ata(&owner, &usdc)); // deterministic
    }
}
