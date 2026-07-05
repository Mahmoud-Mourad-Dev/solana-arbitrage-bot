//! One-time registry hydration over plain RPC — port of the TS
//! `bootstrapRegistry`. Resolves each configured pool into mints, vaults,
//! fees and (Raydium) OpenOrders, then seeds balances so the engine can
//! quote before the first Geyser packet.

use anyhow::{bail, Context, Result};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::str::FromStr;
use tracing::{info, warn};

use crate::config::{DexKindCfg, MonitorConfig};
use crate::parsers::{
    decode_mint_decimals, decode_open_orders_totals, decode_raydium_v4, decode_token_amount,
    decode_whirlpool,
};
use crate::registry::{now_ms, PoolRegistry};
use crate::types::{PoolCommon, PoolState, RaydiumPool, WhirlpoolPool};

async fn get_accounts_batched(
    rpc: &RpcClient,
    addresses: &[Pubkey],
) -> Result<HashMap<Pubkey, Vec<u8>>> {
    let mut out = HashMap::new();
    for chunk in addresses.chunks(100) {
        let infos = rpc
            .get_multiple_accounts(chunk)
            .await
            .context("getMultipleAccounts")?;
        for (addr, info) in chunk.iter().zip(infos) {
            if let Some(acc) = info {
                out.insert(*addr, acc.data);
            }
        }
    }
    Ok(out)
}

pub async fn bootstrap_registry(cfg: &MonitorConfig, registry: &mut PoolRegistry) -> Result<()> {
    let rpc =
        RpcClient::new_with_commitment(cfg.rpc_endpoint.clone(), CommitmentConfig::confirmed());

    let pool_addrs: Vec<Pubkey> = cfg
        .pools
        .iter()
        .filter_map(|p| Pubkey::from_str(&p.address).ok())
        .collect();
    let pool_accounts = get_accounts_batched(&rpc, &pool_addrs).await?;

    // First pass: build skeleton pool states from the pool accounts.
    let mut pending: Vec<PoolState> = Vec::new();
    for cfg_pool in &cfg.pools {
        let Ok(addr) = Pubkey::from_str(&cfg_pool.address) else {
            continue;
        };
        let Some(data) = pool_accounts.get(&addr) else {
            warn!(pool = %addr, label = ?cfg_pool.label, "pool account not found, skipping");
            continue;
        };
        match cfg_pool.dex {
            DexKindCfg::RaydiumV4 => {
                let Some(d) = decode_raydium_v4(data) else {
                    warn!(pool = %addr, "not a Raydium v4 account, skipping");
                    continue;
                };
                pending.push(PoolState::Raydium(RaydiumPool {
                    common: PoolCommon {
                        address: addr,
                        label: cfg_pool.label.clone(),
                        mint_a: d.base_mint,
                        mint_b: d.quote_mint,
                        vault_a: d.base_vault,
                        vault_b: d.quote_vault,
                        decimals_a: d.base_decimal,
                        decimals_b: d.quote_decimal,
                        last_slot: 0,
                        last_updated_ms: 0,
                        ready: false,
                    },
                    vault_a_balance: 0,
                    vault_b_balance: 0,
                    open_orders: d.open_orders,
                    open_orders_base_total: 0,
                    open_orders_quote_total: 0,
                    base_need_take_pnl: d.base_need_take_pnl,
                    quote_need_take_pnl: d.quote_need_take_pnl,
                    swap_fee_numerator: d.swap_fee_numerator,
                    swap_fee_denominator: d.swap_fee_denominator,
                    status: d.status,
                    pool_open_time: d.pool_open_time,
                }));
            }
            DexKindCfg::OrcaWhirlpool => {
                let Some(d) = decode_whirlpool(data) else {
                    warn!(pool = %addr, "not a Whirlpool account, skipping");
                    continue;
                };
                pending.push(PoolState::Whirlpool(WhirlpoolPool {
                    common: PoolCommon {
                        address: addr,
                        label: cfg_pool.label.clone(),
                        mint_a: d.token_mint_a,
                        mint_b: d.token_mint_b,
                        vault_a: d.token_vault_a,
                        vault_b: d.token_vault_b,
                        decimals_a: 0,
                        decimals_b: 0,
                        last_slot: 0,
                        last_updated_ms: 0,
                        ready: false,
                    },
                    sqrt_price_x64: d.sqrt_price_x64,
                    liquidity: d.liquidity,
                    tick_current_index: d.tick_current_index,
                    tick_spacing: d.tick_spacing,
                    fee_rate_ppm: d.fee_rate_ppm,
                }));
            }
        }
    }

    // Second pass: vaults, mints, OpenOrders.
    let mut secondary: Vec<Pubkey> = Vec::new();
    for p in &pending {
        let c = p.common();
        secondary.extend([c.vault_a, c.vault_b, c.mint_a, c.mint_b]);
        if let PoolState::Raydium(r) = p {
            secondary.push(r.open_orders);
        }
    }
    secondary.sort();
    secondary.dedup();
    let secondary_accounts = get_accounts_batched(&rpc, &secondary).await?;

    for mut pool in pending {
        let (mint_a, mint_b) = {
            let c = pool.common();
            (c.mint_a, c.mint_b)
        };
        let dec_a = secondary_accounts
            .get(&mint_a)
            .and_then(|d| decode_mint_decimals(d));
        let dec_b = secondary_accounts
            .get(&mint_b)
            .and_then(|d| decode_mint_decimals(d));
        let (Some(dec_a), Some(dec_b)) = (dec_a, dec_b) else {
            warn!(pool = %pool.common().address, "missing mint metadata, skipping");
            continue;
        };
        pool.common_mut().decimals_a = dec_a;
        pool.common_mut().decimals_b = dec_b;

        if let PoolState::Raydium(r) = &mut pool {
            let a_bal = secondary_accounts
                .get(&r.common.vault_a)
                .and_then(|d| decode_token_amount(d));
            let b_bal = secondary_accounts
                .get(&r.common.vault_b)
                .and_then(|d| decode_token_amount(d));
            let (Some(a_bal), Some(b_bal)) = (a_bal, b_bal) else {
                warn!(pool = %r.common.address, "missing vault balances, skipping");
                continue;
            };
            r.vault_a_balance = a_bal;
            r.vault_b_balance = b_bal;
            if let Some((base, quote)) = secondary_accounts
                .get(&r.open_orders)
                .and_then(|d| decode_open_orders_totals(d))
            {
                r.open_orders_base_total = base;
                r.open_orders_quote_total = quote;
            }
        }

        pool.common_mut().ready = true;
        pool.common_mut().last_updated_ms = now_ms();
        registry.register_token(mint_a, dec_a);
        registry.register_token(mint_b, dec_b);
        let label = pool
            .common()
            .label
            .clone()
            .unwrap_or_else(|| pool.common().address.to_string());
        registry.add_pool(pool);
        info!(pool = %label, "bootstrapped");
    }

    if registry.pools.is_empty() {
        bail!("bootstrap produced zero usable pools — check pools.json / RPC endpoint");
    }
    Ok(())
}
