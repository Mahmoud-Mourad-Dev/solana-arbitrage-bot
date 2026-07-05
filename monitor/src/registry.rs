//! In-memory pool registry + token graph — port of the `PoolRegistry`
//! half of `src/graph.ts`. Single-writer (the Geyser handler); reads are
//! synchronous. Applies raw account updates by routing on pubkey.

use crate::parsers::{
    decode_open_orders_totals, decode_raydium_v4, decode_token_amount, decode_whirlpool,
};
use crate::types::{known_symbol, PoolState, Side, TokenNode};
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;

pub struct PoolRegistry {
    pub pools: HashMap<Pubkey, PoolState>,
    pub tokens: HashMap<Pubkey, TokenNode>,
    vault_to_pool: HashMap<Pubkey, (Pubkey, Side)>,
    open_orders_to_pool: HashMap<Pubkey, Pubkey>,
    /// mint -> pools touching it (graph adjacency).
    pub adjacency: HashMap<Pubkey, Vec<Pubkey>>,
    /// per-account last applied slot (drop out-of-order packets).
    account_slots: HashMap<Pubkey, u64>,
}

impl Default for PoolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl PoolRegistry {
    pub fn new() -> Self {
        Self {
            pools: HashMap::new(),
            tokens: HashMap::new(),
            vault_to_pool: HashMap::new(),
            open_orders_to_pool: HashMap::new(),
            adjacency: HashMap::new(),
            account_slots: HashMap::new(),
        }
    }

    pub fn register_token(&mut self, mint: Pubkey, decimals: u8) {
        self.tokens.entry(mint).or_insert_with(|| TokenNode {
            mint,
            decimals,
            symbol: known_symbol(&mint),
        });
    }

    pub fn add_pool(&mut self, state: PoolState) {
        let (addr, mint_a, mint_b, vault_a, vault_b) = {
            let c = state.common();
            (c.address, c.mint_a, c.mint_b, c.vault_a, c.vault_b)
        };
        self.vault_to_pool.insert(vault_a, (addr, Side::A));
        self.vault_to_pool.insert(vault_b, (addr, Side::B));
        if let PoolState::Raydium(p) = &state {
            self.open_orders_to_pool.insert(p.open_orders, addr);
        }
        for mint in [mint_a, mint_b] {
            let list = self.adjacency.entry(mint).or_default();
            if !list.contains(&addr) {
                list.push(addr);
            }
        }
        self.pools.insert(addr, state);
    }

    /// Every account address the Geyser subscription must include.
    pub fn all_watched_accounts(&self) -> Vec<Pubkey> {
        let mut set = Vec::new();
        for p in self.pools.values() {
            let c = p.common();
            set.push(c.address);
            set.push(c.vault_a);
            set.push(c.vault_b);
            if let PoolState::Raydium(r) = p {
                set.push(r.open_orders);
            }
        }
        set.sort();
        set.dedup();
        set
    }

    /// Freshest ready pool connecting two mints (gas-cost conversion).
    pub fn find_reference_pool(&self, mint_x: &Pubkey, mint_y: &Pubkey) -> Option<&PoolState> {
        let mut best: Option<&PoolState> = None;
        for addr in self.adjacency.get(mint_x)? {
            let Some(p) = self.pools.get(addr) else {
                continue;
            };
            let c = p.common();
            if !c.ready {
                continue;
            }
            let pair = if &c.mint_a == mint_x {
                c.mint_b
            } else {
                c.mint_a
            };
            if &pair != mint_y {
                continue;
            }
            if best.is_none_or(|b| c.last_slot > b.common().last_slot) {
                best = Some(p);
            }
        }
        best
    }

    fn accept_slot(&mut self, pubkey: Pubkey, slot: u64) -> bool {
        match self.account_slots.get(&pubkey) {
            Some(prev) if slot < *prev => false,
            _ => {
                self.account_slots.insert(pubkey, slot);
                true
            }
        }
    }

    /// Route a raw account update into the registry. Returns the affected
    /// pool address when its quotable state changed.
    pub fn apply_account_update(
        &mut self,
        pubkey: Pubkey,
        data: &[u8],
        slot: u64,
    ) -> Option<Pubkey> {
        if !self.accept_slot(pubkey, slot) {
            return None;
        }

        if self.pools.contains_key(&pubkey) {
            return self.apply_pool_account(pubkey, data, slot);
        }
        if let Some((pool_addr, side)) = self.vault_to_pool.get(&pubkey).copied() {
            let amount = decode_token_amount(data)?;
            let pool = self.pools.get_mut(&pool_addr)?;
            if let PoolState::Raydium(r) = pool {
                match side {
                    Side::A => r.vault_a_balance = amount,
                    Side::B => r.vault_b_balance = amount,
                }
            }
            Self::touch(pool.common_mut(), slot);
            return Some(pool_addr);
        }
        if let Some(pool_addr) = self.open_orders_to_pool.get(&pubkey).copied() {
            let (base, quote) = decode_open_orders_totals(data)?;
            let pool = self.pools.get_mut(&pool_addr)?;
            if let PoolState::Raydium(r) = pool {
                r.open_orders_base_total = base;
                r.open_orders_quote_total = quote;
                Self::touch(&mut r.common, slot);
                return Some(pool_addr);
            }
        }
        None
    }

    fn apply_pool_account(&mut self, addr: Pubkey, data: &[u8], slot: u64) -> Option<Pubkey> {
        let pool = self.pools.get_mut(&addr)?;
        match pool {
            PoolState::Raydium(r) => {
                let d = decode_raydium_v4(data)?;
                r.base_need_take_pnl = d.base_need_take_pnl;
                r.quote_need_take_pnl = d.quote_need_take_pnl;
                r.swap_fee_numerator = d.swap_fee_numerator;
                r.swap_fee_denominator = d.swap_fee_denominator;
                r.status = d.status;
                r.pool_open_time = d.pool_open_time;
                Self::touch(&mut r.common, slot);
            }
            PoolState::Whirlpool(w) => {
                let d = decode_whirlpool(data)?;
                w.sqrt_price_x64 = d.sqrt_price_x64;
                w.liquidity = d.liquidity;
                w.tick_current_index = d.tick_current_index;
                w.fee_rate_ppm = d.fee_rate_ppm;
                Self::touch(&mut w.common, slot);
            }
        }
        Some(addr)
    }

    fn touch(common: &mut crate::types::PoolCommon, slot: u64) {
        if slot > common.last_slot {
            common.last_slot = slot;
        }
        common.last_updated_ms = now_ms();
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{PoolCommon, RaydiumPool};

    fn ray(addr: Pubkey, a: Pubkey, b: Pubkey, va: Pubkey, vb: Pubkey, oo: Pubkey) -> PoolState {
        PoolState::Raydium(RaydiumPool {
            common: PoolCommon {
                address: addr,
                label: None,
                mint_a: a,
                mint_b: b,
                vault_a: va,
                vault_b: vb,
                decimals_a: 9,
                decimals_b: 6,
                last_slot: 0,
                last_updated_ms: 0,
                ready: true,
            },
            vault_a_balance: 0,
            vault_b_balance: 0,
            open_orders: oo,
            open_orders_base_total: 0,
            open_orders_quote_total: 0,
            base_need_take_pnl: 0,
            quote_need_take_pnl: 0,
            swap_fee_numerator: 25,
            swap_fee_denominator: 10_000,
            status: 6,
            pool_open_time: 0,
        })
    }

    #[test]
    fn vault_update_routes_to_pool_and_bumps_slot() {
        let (addr, a, b, va, vb, oo) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let mut reg = PoolRegistry::new();
        reg.add_pool(ray(addr, a, b, va, vb, oo));

        let mut tok = vec![0u8; 165];
        tok[64..72].copy_from_slice(&500u64.to_le_bytes());
        assert_eq!(reg.apply_account_update(va, &tok, 10), Some(addr));
        if let Some(PoolState::Raydium(r)) = reg.pools.get(&addr) {
            assert_eq!(r.vault_a_balance, 500);
            assert_eq!(r.common.last_slot, 10);
        } else {
            panic!("pool missing");
        }

        // out-of-order (older slot) is dropped.
        assert_eq!(reg.apply_account_update(va, &tok, 5), None);
    }

    #[test]
    fn adjacency_and_watched_accounts() {
        let (addr, a, b, va, vb, oo) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let mut reg = PoolRegistry::new();
        reg.add_pool(ray(addr, a, b, va, vb, oo));
        assert_eq!(reg.adjacency.get(&a).unwrap(), &vec![addr]);
        // pool + 2 vaults + open orders = 4 unique accounts.
        assert_eq!(reg.all_watched_accounts().len(), 4);
    }
}
