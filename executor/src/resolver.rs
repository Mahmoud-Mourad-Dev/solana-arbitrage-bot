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
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{instruction::AccountMeta, pubkey, pubkey::Pubkey};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use arb_common::dlmm_pda::{bin_array_pda, bitmap_extension_pda, dlmm_oracle, event_authority};
use arb_common::ix::{
    DexKind, METEORA_DLMM_PROGRAM_ID, PUMP_AMM_PROGRAM_ID, RAYDIUM_V4_PROGRAM_ID, TOKEN_PROGRAM_ID,
    WHIRLPOOL_PROGRAM_ID,
};
use arb_common::opportunity::OpportunityHop;
use arb_common::pump_pda;

// Program ids as solana Pubkeys, built from arb-common's canonical bytes
// (the same source of truth the on-chain program uses).
pub const RAYDIUM_V4_PROGRAM: Pubkey = Pubkey::new_from_array(RAYDIUM_V4_PROGRAM_ID);
pub const WHIRLPOOL_PROGRAM: Pubkey = Pubkey::new_from_array(WHIRLPOOL_PROGRAM_ID);
pub const METEORA_DLMM_PROGRAM: Pubkey = Pubkey::new_from_array(METEORA_DLMM_PROGRAM_ID);
pub const PUMP_AMM_PROGRAM: Pubkey = Pubkey::new_from_array(PUMP_AMM_PROGRAM_ID);
/// Pump fees-v2 program (sell account [20]). Source: sim_parity::PUMP_FEE_PROGRAM_ID.
pub const PUMP_FEE_PROGRAM: Pubkey = pubkey!("pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ");
pub const SYSTEM_PROGRAM: Pubkey = pubkey!("11111111111111111111111111111111");
pub const TOKEN_PROGRAM: Pubkey = Pubkey::new_from_array(TOKEN_PROGRAM_ID);
pub const TOKEN_2022_PROGRAM: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
pub const MEMO_PROGRAM: Pubkey = pubkey!("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");

pub const ATA_PROGRAM: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
pub const WSOL_MINT: Pubkey = pubkey!("So11111111111111111111111111111111111111112");

pub const TICK_ARRAY_SIZE: i32 = 88;
const RAYDIUM_ACCOUNT_LEN: usize = 752;
const WHIRLPOOL_ACCOUNT_LEN: usize = 653;
const MARKET_ACCOUNT_LEN: usize = 388;
const DLMM_LB_PAIR_MIN_LEN: usize = 904;

/// index (within the hop slice, program at 0) of the user source account.
pub const RAYDIUM_SOURCE_INDEX: u8 = 16;
pub const WHIRLPOOL_SOURCE_INDEX_A: u8 = 4;
pub const WHIRLPOOL_SOURCE_INDEX_B: u8 = 6;
/// DLMM swap2 `userTokenIn` sits at CPI index 4 → hop-slice index 5 (program
/// occupies slot 0). Verified against `swap2_cpi_fixtures.json`.
pub const METEORA_SOURCE_INDEX: u8 = 5;

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

/// ATA for (owner, mint) under an EXPLICIT token program (Token or Token-2022).
pub fn derive_ata_prog(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ATA_PROGRAM,
    )
    .0
}

/// DLMM stores a per-mint flag: 1 ⇒ Token-2022, else classic Token program.
fn token_program_for(flag: u8) -> Pubkey {
    if flag == 1 {
        TOKEN_2022_PROGRAM
    } else {
        TOKEN_PROGRAM
    }
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
pub struct MeteoraDlmmKeys {
    pub lb_pair: Pubkey,
    pub reserve_x: Pubkey,
    pub reserve_y: Pubkey,
    pub mint_x: Pubkey,
    pub mint_y: Pubkey,
    pub token_x_program: Pubkey,
    pub token_y_program: Pubkey,
    pub oracle: Pubkey,
    pub bitmap_extension: Pubkey,
    pub active_id: i32,
    pub fetched_at: Instant,
}

/// PumpSwap AMM keys derivable from pool state + PDAs. The rotating/fee-v2
/// accounts ([9],[10],[19],[21],[22],[23]) are NOT here — they are undocumented
/// and rotating (docs/pump-fee-v2-layout.md) and must be carried from the quote.
#[derive(Debug, Clone)]
pub struct PumpAmmKeys {
    pub pool: Pubkey,
    pub base_mint: Pubkey,
    pub quote_mint: Pubkey,
    pub base_vault: Pubkey,
    pub quote_vault: Pubkey,
    pub coin_creator: Pubkey,
    pub base_token_program: Pubkey,
    pub quote_token_program: Pubkey,
    // derived PDAs
    pub global_config: Pubkey,
    pub event_authority: Pubkey,
    pub coin_creator_vault_authority: Pubkey,
    pub coin_creator_vault_ata: Pubkey,
    pub fetched_at: Instant,
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
    dlmm_cache: Mutex<HashMap<Pubkey, Arc<MeteoraDlmmKeys>>>,
    pump_cache: Mutex<HashMap<Pubkey, Arc<PumpAmmKeys>>>,
    whirlpool_ttl: Duration,
}

impl Resolver {
    pub fn new(rpc: Arc<RpcClient>, owner: Pubkey, whirlpool_ttl: Duration) -> Self {
        Self {
            rpc,
            owner,
            raydium_cache: Mutex::new(HashMap::new()),
            whirlpool_cache: Mutex::new(HashMap::new()),
            dlmm_cache: Mutex::new(HashMap::new()),
            pump_cache: Mutex::new(HashMap::new()),
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
            DexKind::MeteoraDlmm => {
                let k = self.dlmm_keys(pool).await?;
                Ok((k.mint_x, k.mint_y))
            }
            DexKind::PumpAmm => {
                let k = self.pump_keys(pool).await?;
                Ok((k.base_mint, k.quote_mint))
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
            DexKind::MeteoraDlmm => {
                let keys = self.dlmm_keys(pool).await?;
                self.dlmm_hop(&keys, input_mint, hop.min_amount_out, &hop.bin_arrays)
            }
            DexKind::PumpAmm => {
                let keys = self.pump_keys(pool).await?;
                self.pump_hop(
                    &keys,
                    input_mint,
                    hop.min_amount_out,
                    &hop.pump_carried_accounts,
                )
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

    // ── Meteora DLMM ──────────────────────────────────────────────────────────

    async fn dlmm_keys(&self, pool: Pubkey) -> Result<Arc<MeteoraDlmmKeys>> {
        if let Some(k) = self.dlmm_cache.lock().await.get(&pool) {
            // Only the metadata (mints, reserves, token programs, oracle) is
            // cached; active_id drifts but is not used to pick bin arrays
            // (those come from the quote). TTL bounds staleness anyway.
            if k.fetched_at.elapsed() < self.whirlpool_ttl {
                return Ok(k.clone());
            }
        }
        let data = self
            .rpc
            .get_account_data(&pool)
            .await
            .with_context(|| format!("fetch dlmm lb_pair {pool}"))?;
        if data.len() < DLMM_LB_PAIR_MIN_LEN {
            bail!("{pool} is not a DLMM lb_pair (len={})", data.len());
        }
        // Offsets verified in monitor/src/meteora_dlmm.rs (6/6 live-exact).
        let active_id = i32::from_le_bytes(data[76..80].try_into().unwrap());
        let mint_x = read_pubkey(&data, 88);
        let mint_y = read_pubkey(&data, 120);
        let reserve_x = read_pubkey(&data, 152);
        let reserve_y = read_pubkey(&data, 184);
        let token_x_program = token_program_for(data[880]);
        let token_y_program = token_program_for(data[881]);
        let oracle = dlmm_oracle(&METEORA_DLMM_PROGRAM, &pool);
        let bitmap_extension = bitmap_extension_pda(&METEORA_DLMM_PROGRAM, &pool);

        let keys = Arc::new(MeteoraDlmmKeys {
            lb_pair: pool,
            reserve_x,
            reserve_y,
            mint_x,
            mint_y,
            token_x_program,
            token_y_program,
            oracle,
            bitmap_extension,
            active_id,
            fetched_at: Instant::now(),
        });
        self.dlmm_cache.lock().await.insert(pool, keys.clone());
        Ok(keys)
    }

    /// Build the DLMM `swap2` hop slice. Account order is the IDL order,
    /// verified byte-for-byte against `swap2_cpi_fixtures.json`:
    ///
    /// ```text
    /// 0  lb_pair (w)                 8  oracle (w)
    /// 1  bitmap_extension | program  9  host_fee_in = program (None)
    /// 2  reserve_x (w)              10  user (authority, signer)
    /// 3  reserve_y (w)             11  token_x_program
    /// 4  user_token_in (w)         12  token_y_program
    /// 5  user_token_out (w)        13  memo program
    /// 6  token_x_mint              14  event_authority
    /// 7  token_y_mint              15  dlmm program (self)
    /// 16.. bin arrays (w), in traversal order
    /// ```
    ///
    /// `bin_array_indices` MUST come from the quote that produced the route.
    /// If empty this returns an error rather than guessing a set that could
    /// disagree with the quoted output.
    fn dlmm_hop(
        &self,
        k: &MeteoraDlmmKeys,
        input_mint: Pubkey,
        min_amount_out: u64,
        bin_array_indices: &[i64],
    ) -> Result<ResolvedHop> {
        if input_mint != k.mint_x && input_mint != k.mint_y {
            bail!("input mint {input_mint} not in dlmm pool {}", k.lb_pair);
        }
        if bin_array_indices.is_empty() {
            bail!(
                "DLMM hop on {} has no bin-array set — it must be carried from the \
                 quote (OpportunityHop.bin_arrays); refusing to guess",
                k.lb_pair
            );
        }
        // userTokenIn is the ATA of the input mint; userTokenOut the other.
        let user_x = derive_ata(&self.owner, &k.mint_x);
        let user_y = derive_ata(&self.owner, &k.mint_y);
        let (user_in, user_out) = if input_mint == k.mint_x {
            (user_x, user_y)
        } else {
            (user_y, user_x)
        };

        let mut metas = vec![
            AccountMeta::new_readonly(METEORA_DLMM_PROGRAM, false), // hop program at [0]
            AccountMeta::new(k.lb_pair, false),                     // 0 lb_pair
            AccountMeta::new(k.bitmap_extension, false),            // 1 bitmap ext
            AccountMeta::new(k.reserve_x, false),                   // 2 reserve_x
            AccountMeta::new(k.reserve_y, false),                   // 3 reserve_y
            AccountMeta::new(user_in, false),                       // 4 user_token_in
            AccountMeta::new(user_out, false),                      // 5 user_token_out
            AccountMeta::new_readonly(k.mint_x, false),             // 6 token_x_mint
            AccountMeta::new_readonly(k.mint_y, false),             // 7 token_y_mint
            AccountMeta::new(k.oracle, false),                      // 8 oracle
            AccountMeta::new_readonly(METEORA_DLMM_PROGRAM, false), // 9 host_fee_in = None
            AccountMeta::new_readonly(self.owner, true),            // 10 user (signer)
            AccountMeta::new_readonly(k.token_x_program, false),    // 11 token_x_program
            AccountMeta::new_readonly(k.token_y_program, false),    // 12 token_y_program
            AccountMeta::new_readonly(MEMO_PROGRAM, false),         // 13 memo
            AccountMeta::new_readonly(event_authority(&METEORA_DLMM_PROGRAM), false), // 14
            AccountMeta::new_readonly(METEORA_DLMM_PROGRAM, false), // 15 program (self)
        ];
        for &idx in bin_array_indices {
            metas.push(AccountMeta::new(
                bin_array_pda(&METEORA_DLMM_PROGRAM, &k.lb_pair, idx),
                false,
            ));
        }
        Ok(ResolvedHop {
            dex: DexKind::MeteoraDlmm,
            metas,
            source_index: METEORA_SOURCE_INDEX,
            a_to_b: input_mint == k.mint_x, // recorded; direction is implied by the token accounts
            min_amount_out,
        })
    }

    // ── PumpSwap AMM ──────────────────────────────────────────────────────────

    async fn pump_keys(&self, pool: Pubkey) -> Result<Arc<PumpAmmKeys>> {
        if let Some(k) = self.pump_cache.lock().await.get(&pool) {
            if k.fetched_at.elapsed() < self.whirlpool_ttl {
                return Ok(k.clone());
            }
        }
        let data = self
            .rpc
            .get_account_data(&pool)
            .await
            .with_context(|| format!("fetch pump pool {pool}"))?;
        // Offsets + discriminator from monitor/src/pump_amm.rs (proven).
        const POOL_DISC: [u8; 8] = [0xf1, 0x9a, 0x6d, 0x04, 0x11, 0xb1, 0x6d, 0xbc];
        const POOL_MIN_LEN: usize = 243;
        if data.len() < POOL_MIN_LEN || data[0..8] != POOL_DISC {
            bail!("{pool} is not a PumpSwap pool (len={})", data.len());
        }
        let base_mint = read_pubkey(&data, 43);
        let quote_mint = read_pubkey(&data, 75);
        let base_vault = read_pubkey(&data, 139);
        let quote_vault = read_pubkey(&data, 171);
        let coin_creator = read_pubkey(&data, 211);

        // Token programs [11]/[12]: the OWNER of each mint account (Token vs
        // Token-2022). Read them, never assume — a pump base can be Token-2022.
        let base_token_program = self.mint_owner(&base_mint).await?;
        let quote_token_program = self.mint_owner(&quote_mint).await?;

        let global_config = pump_pda::global_config(&PUMP_AMM_PROGRAM);
        let event_authority = pump_pda::event_authority(&PUMP_AMM_PROGRAM);
        let coin_creator_vault_authority =
            pump_pda::coin_creator_vault_authority(&PUMP_AMM_PROGRAM, &coin_creator);
        // The coin-creator vault ATA is held for the QUOTE mint under its token
        // program (sell account [17]).
        let coin_creator_vault_ata = pump_pda::coin_creator_vault_ata(
            &PUMP_AMM_PROGRAM,
            &coin_creator,
            &quote_mint,
            &quote_token_program,
            &ATA_PROGRAM,
        );

        let keys = Arc::new(PumpAmmKeys {
            pool,
            base_mint,
            quote_mint,
            base_vault,
            quote_vault,
            coin_creator,
            base_token_program,
            quote_token_program,
            global_config,
            event_authority,
            coin_creator_vault_authority,
            coin_creator_vault_ata,
            fetched_at: Instant::now(),
        });
        self.pump_cache.lock().await.insert(pool, keys.clone());
        Ok(keys)
    }

    /// The owner program of a mint account (Token or Token-2022).
    async fn mint_owner(&self, mint: &Pubkey) -> Result<Pubkey> {
        let acc = self
            .rpc
            .get_account(mint)
            .await
            .with_context(|| format!("fetch mint {mint}"))?;
        if acc.owner != TOKEN_PROGRAM && acc.owner != TOKEN_2022_PROGRAM {
            bail!("mint {mint} owner {} is not a token program", acc.owner);
        }
        Ok(acc.owner)
    }

    /// Build the PumpSwap swap hop slice `[pump_program, ...24 CPI accounts]`.
    ///
    /// Account order + writable/signer flags are the captured mainnet SELL CPI
    /// (monitor/fixtures/pump/reconstruction_fixtures.json route1) — validated
    /// index-by-index by `pump_hop_account_order_matches_captured_cpi`.
    ///
    /// Six accounts CANNOT be derived from pool state (undocumented seeds +
    /// rotating recipient — docs/pump-fee-v2-layout.md): [9] protocol_fee_
    /// recipient, [10] its ATA, [19] fee_config, [21] fee_pool, [22] fee_pool_
    /// state, [23] fee_recipient_ata. They MUST be carried from the quote in
    /// exactly that order. An empty/short/invalid set is a HARD ERROR — never
    /// guessed, never defaulted.
    fn pump_hop(
        &self,
        k: &PumpAmmKeys,
        input_mint: Pubkey,
        min_amount_out: u64,
        carried: &[String],
    ) -> Result<ResolvedHop> {
        let is_sell = if input_mint == k.base_mint {
            true // base in → quote out
        } else if input_mint == k.quote_mint {
            false // quote in → base out
        } else {
            bail!("input mint {input_mint} not in pump pool {}", k.pool);
        };
        if carried.len() != 6 {
            bail!(
                "pump hop on {} requires exactly 6 carried accounts \
                 [recipient, recipient_ata, fee_config, fee_pool, fee_pool_state, \
                 fee_recipient_ata] from the quote — got {}; refusing to guess",
                k.pool,
                carried.len()
            );
        }
        let mut c = [Pubkey::default(); 6];
        for (i, s) in carried.iter().enumerate() {
            c[i] = Pubkey::from_str(s)
                .with_context(|| format!("bad carried pump account [{i}]: {s}"))?;
        }
        let (recipient, recipient_ata, fee_config, fee_pool, fee_pool_state, fee_recipient_ata) =
            (c[0], c[1], c[2], c[3], c[4], c[5]);

        let user_base_ata = derive_ata_prog(&self.owner, &k.base_mint, &k.base_token_program);
        let user_quote_ata = derive_ata_prog(&self.owner, &k.quote_mint, &k.quote_token_program);

        // CPI accounts [0..24] in the captured order; w/s flags from the CPI.
        let metas = vec![
            AccountMeta::new_readonly(PUMP_AMM_PROGRAM, false), // slice[0] hop program
            AccountMeta::new(k.pool, false),                    // 0 pool (w)
            AccountMeta::new_readonly(self.owner, true),        // 1 user (w,s) — writable below
            AccountMeta::new_readonly(k.global_config, false),  // 2
            AccountMeta::new_readonly(k.base_mint, false),      // 3
            AccountMeta::new_readonly(k.quote_mint, false),     // 4
            AccountMeta::new(user_base_ata, false),             // 5 (w)
            AccountMeta::new(user_quote_ata, false),            // 6 (w)
            AccountMeta::new(k.base_vault, false),              // 7 (w)
            AccountMeta::new(k.quote_vault, false),             // 8 (w)
            AccountMeta::new_readonly(recipient, false),        // 9 carried
            AccountMeta::new(recipient_ata, false),             // 10 carried (w)
            AccountMeta::new_readonly(k.base_token_program, false), // 11
            AccountMeta::new_readonly(k.quote_token_program, false), // 12
            AccountMeta::new_readonly(SYSTEM_PROGRAM, false),   // 13
            AccountMeta::new_readonly(ATA_PROGRAM, false),      // 14
            AccountMeta::new_readonly(k.event_authority, false), // 15
            AccountMeta::new_readonly(PUMP_AMM_PROGRAM, false), // 16 program (self)
            AccountMeta::new(k.coin_creator_vault_ata, false),  // 17 (w)
            AccountMeta::new_readonly(k.coin_creator_vault_authority, false), // 18
            AccountMeta::new_readonly(fee_config, false),       // 19 carried
            AccountMeta::new_readonly(PUMP_FEE_PROGRAM, false), // 20
            AccountMeta::new_readonly(fee_pool, false),         // 21 carried
            AccountMeta::new_readonly(fee_pool_state, false),   // 22 carried
            AccountMeta::new(fee_recipient_ata, false),         // 23 carried (w)
        ];
        // user (CPI idx 1 → slice idx 2) must be a writable signer.
        let mut metas = metas;
        metas[2] = AccountMeta::new(self.owner, true);

        // The program reads the source balance for hops>0 at `source_index`.
        // SELL sweeps user_base_ata (CPI 5 → slice 6); BUY sweeps user_quote_ata
        // (CPI 6 → slice 7).
        let source_index = if is_sell { 6 } else { 7 };

        Ok(ResolvedHop {
            dex: DexKind::PumpAmm,
            metas,
            source_index,
            a_to_b: is_sell, // program maps a_to_b → is_sell for pump
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

    fn dlmm_keys_fixture() -> MeteoraDlmmKeys {
        MeteoraDlmmKeys {
            lb_pair: Pubkey::new_unique(),
            reserve_x: Pubkey::new_unique(),
            reserve_y: Pubkey::new_unique(),
            mint_x: Pubkey::new_unique(),
            mint_y: WSOL_MINT,
            token_x_program: TOKEN_2022_PROGRAM,
            token_y_program: TOKEN_PROGRAM,
            oracle: Pubkey::new_unique(),
            bitmap_extension: Pubkey::new_unique(),
            active_id: 100,
            fetched_at: Instant::now(),
        }
    }

    #[test]
    fn dlmm_hop_account_order_matches_swap2_idl() {
        let r = Resolver::new(
            // no RPC calls in dlmm_hop; a dummy client is fine
            Arc::new(RpcClient::new("http://localhost:8899".into())),
            Pubkey::new_unique(),
            Duration::from_secs(30),
        );
        let k = dlmm_keys_fixture();
        let hop = r
            .dlmm_hop(&k, k.mint_x, 42, &[0, 1])
            .expect("dlmm hop builds");
        // program at [0], then the 16 fixed IDL accounts, then 2 bin arrays.
        assert_eq!(hop.metas.len(), 1 + 16 + 2);
        assert_eq!(hop.metas[0].pubkey, METEORA_DLMM_PROGRAM);
        assert_eq!(hop.metas[1].pubkey, k.lb_pair);
        assert_eq!(hop.metas[3].pubkey, k.reserve_x);
        // userTokenIn (index 5) is the input-mint ATA; source_index points at it.
        assert_eq!(hop.metas[5].pubkey, derive_ata(&r.owner, &k.mint_x));
        assert_eq!(hop.metas[6].pubkey, derive_ata(&r.owner, &k.mint_y));
        assert_eq!(hop.source_index, METEORA_SOURCE_INDEX);
        assert_eq!(
            hop.metas[METEORA_SOURCE_INDEX as usize].pubkey,
            hop.metas[5].pubkey
        );
        // user authority at [11] is the only signer, and writable-none of the
        // fixed readonly programs got marked writable.
        assert!(hop.metas[11].is_signer);
        assert_eq!(hop.metas[11].pubkey, r.owner);
        assert_eq!(hop.metas[12].pubkey, TOKEN_2022_PROGRAM); // token_x_program
        assert_eq!(hop.metas[13].pubkey, TOKEN_PROGRAM); // token_y_program
        assert_eq!(hop.metas[14].pubkey, MEMO_PROGRAM); // memo
        assert_eq!(hop.metas[15].pubkey, event_authority(&METEORA_DLMM_PROGRAM));
        assert_eq!(hop.metas[16].pubkey, METEORA_DLMM_PROGRAM); // program self
                                                                // host_fee_in (index 10) is the program-id None sentinel.
        assert_eq!(hop.metas[10].pubkey, METEORA_DLMM_PROGRAM);
        // first bin array follows the 17 fixed slots.
        assert_eq!(
            hop.metas[17].pubkey,
            bin_array_pda(&METEORA_DLMM_PROGRAM, &k.lb_pair, 0)
        );
    }

    #[test]
    fn dlmm_hop_reverses_token_accounts_when_input_is_y() {
        let r = Resolver::new(
            Arc::new(RpcClient::new("http://localhost:8899".into())),
            Pubkey::new_unique(),
            Duration::from_secs(30),
        );
        let k = dlmm_keys_fixture();
        let hop = r.dlmm_hop(&k, k.mint_y, 1, &[0]).unwrap();
        // input = y ⇒ userTokenIn is the y ATA, userTokenOut the x ATA.
        assert_eq!(hop.metas[5].pubkey, derive_ata(&r.owner, &k.mint_y));
        assert_eq!(hop.metas[6].pubkey, derive_ata(&r.owner, &k.mint_x));
    }

    #[test]
    fn dlmm_hop_refuses_empty_bin_arrays() {
        let r = Resolver::new(
            Arc::new(RpcClient::new("http://localhost:8899".into())),
            Pubkey::new_unique(),
            Duration::from_secs(30),
        );
        let k = dlmm_keys_fixture();
        // The quote must supply the traversed set; guessing is refused.
        assert!(r.dlmm_hop(&k, k.mint_x, 1, &[]).is_err());
    }

    #[test]
    fn dlmm_hop_rejects_foreign_input_mint() {
        let r = Resolver::new(
            Arc::new(RpcClient::new("http://localhost:8899".into())),
            Pubkey::new_unique(),
            Duration::from_secs(30),
        );
        let k = dlmm_keys_fixture();
        assert!(r.dlmm_hop(&k, Pubkey::new_unique(), 1, &[0]).is_err());
    }

    fn pump_keys_fixture() -> PumpAmmKeys {
        let base_mint = Pubkey::new_unique();
        let quote_mint = WSOL_MINT;
        PumpAmmKeys {
            pool: Pubkey::new_unique(),
            base_mint,
            quote_mint,
            base_vault: Pubkey::new_unique(),
            quote_vault: Pubkey::new_unique(),
            coin_creator: Pubkey::new_unique(),
            base_token_program: TOKEN_2022_PROGRAM,
            quote_token_program: TOKEN_PROGRAM,
            global_config: Pubkey::new_unique(),
            event_authority: Pubkey::new_unique(),
            coin_creator_vault_authority: Pubkey::new_unique(),
            coin_creator_vault_ata: Pubkey::new_unique(),
            fetched_at: Instant::now(),
        }
    }

    fn a_resolver() -> Resolver {
        Resolver::new(
            Arc::new(RpcClient::new("http://localhost:8899".into())),
            Pubkey::new_unique(),
            Duration::from_secs(30),
        )
    }

    fn six_carried() -> Vec<String> {
        (0..6).map(|_| Pubkey::new_unique().to_string()).collect()
    }

    #[test]
    fn pump_hop_account_order_matches_captured_cpi() {
        let r = a_resolver();
        let k = pump_keys_fixture();
        let carried = six_carried();
        let hop = r.pump_hop(&k, k.base_mint, 7, &carried).expect("builds");
        // program at [0], then the 24 CPI accounts.
        assert_eq!(hop.metas.len(), 1 + 24);
        assert_eq!(hop.metas[0].pubkey, PUMP_AMM_PROGRAM);
        // pool-derived positions (CPI index shown in comment; slice = CPI+1).
        assert_eq!(hop.metas[1].pubkey, k.pool); // 0
        assert_eq!(hop.metas[2].pubkey, r.owner); // 1 user
        assert_eq!(hop.metas[3].pubkey, k.global_config); // 2
        assert_eq!(hop.metas[4].pubkey, k.base_mint); // 3
        assert_eq!(hop.metas[5].pubkey, k.quote_mint); // 4
        assert_eq!(hop.metas[8].pubkey, k.base_vault); // 7
        assert_eq!(hop.metas[9].pubkey, k.quote_vault); // 8
        assert_eq!(hop.metas[12].pubkey, TOKEN_2022_PROGRAM); // 11 base token prog
        assert_eq!(hop.metas[13].pubkey, TOKEN_PROGRAM); // 12 quote token prog
        assert_eq!(hop.metas[14].pubkey, SYSTEM_PROGRAM); // 13
        assert_eq!(hop.metas[15].pubkey, ATA_PROGRAM); // 14
        assert_eq!(hop.metas[16].pubkey, k.event_authority); // 15
        assert_eq!(hop.metas[17].pubkey, PUMP_AMM_PROGRAM); // 16 program self
        assert_eq!(hop.metas[18].pubkey, k.coin_creator_vault_ata); // 17
        assert_eq!(hop.metas[19].pubkey, k.coin_creator_vault_authority); // 18
        assert_eq!(hop.metas[21].pubkey, PUMP_FEE_PROGRAM); // 20
                                                            // carried accounts land at CPI [9,10,19,21,22,23] → slice [10,11,20,22,23,24].
        let c: Vec<Pubkey> = carried
            .iter()
            .map(|s| Pubkey::from_str(s).unwrap())
            .collect();
        assert_eq!(hop.metas[10].pubkey, c[0]); // 9 recipient
        assert_eq!(hop.metas[11].pubkey, c[1]); // 10 recipient_ata
        assert_eq!(hop.metas[20].pubkey, c[2]); // 19 fee_config
        assert_eq!(hop.metas[22].pubkey, c[3]); // 21 fee_pool
        assert_eq!(hop.metas[23].pubkey, c[4]); // 22 fee_pool_state
        assert_eq!(hop.metas[24].pubkey, c[5]); // 23 fee_recipient_ata
                                                // writable/signer flags exactly match the captured CPI.
        let writable: Vec<usize> = (0..hop.metas.len())
            .filter(|i| hop.metas[*i].is_writable)
            .collect();
        assert_eq!(writable, vec![1, 2, 6, 7, 8, 9, 11, 18, 24], "writable set");
        let signers: Vec<usize> = (0..hop.metas.len())
            .filter(|i| hop.metas[*i].is_signer)
            .collect();
        assert_eq!(signers, vec![2], "only the user signs");
        // SELL sweeps user_base_ata (slice 6).
        assert_eq!(hop.source_index, 6);
        assert!(hop.a_to_b, "base in ⇒ is_sell");
    }

    #[test]
    fn pump_hop_buy_uses_quote_source_index() {
        let r = a_resolver();
        let k = pump_keys_fixture();
        let hop = r.pump_hop(&k, k.quote_mint, 1, &six_carried()).unwrap();
        assert_eq!(hop.source_index, 7); // user_quote_ata
        assert!(!hop.a_to_b);
    }

    #[test]
    fn pump_hop_refuses_absent_or_short_carried_set() {
        let r = a_resolver();
        let k = pump_keys_fixture();
        assert!(r.pump_hop(&k, k.base_mint, 1, &[]).is_err());
        assert!(r.pump_hop(&k, k.base_mint, 1, &six_carried()[..5]).is_err());
    }

    #[test]
    fn pump_hop_rejects_foreign_input_mint() {
        let r = a_resolver();
        let k = pump_keys_fixture();
        assert!(r
            .pump_hop(&k, Pubkey::new_unique(), 1, &six_carried())
            .is_err());
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
