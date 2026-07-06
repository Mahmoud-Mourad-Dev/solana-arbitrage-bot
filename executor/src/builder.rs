//! Assembles the atomic bundle transaction:
//!   1. compute budget (limit + price)
//!   2. the on-chain arbitrage program instruction (all hops)
//!   3. Jito tip transfer
//!
//! packed into a v0 message (address lookup tables optional but strongly
//! recommended for 3-4 hop routes).

use anyhow::{bail, Context, Result};
use arb_common::ix::{encode_instruction, HopParams, IxParams};
use solana_sdk::{
    address_lookup_table::AddressLookupTableAccount,
    compute_budget::ComputeBudgetInstruction,
    hash::Hash,
    instruction::{AccountMeta, Instruction},
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    transaction::VersionedTransaction,
};
use solana_system_interface::instruction as system_instruction;

use crate::resolver::ResolvedHop;

/// Solana packet size — a serialized tx beyond this can never land.
pub const MAX_TX_BYTES: usize = 1232;

pub struct BundleParams<'a> {
    pub program_id: Pubkey,
    pub payer: &'a Keypair,
    /// Profit-checked account (WSOL ATA for SOL-base cycles).
    pub base_token_account: Pubkey,
    pub hops: &'a [ResolvedHop],
    pub amount_in: u64,
    pub min_profit: u64,
    pub cu_limit: u32,
    pub cu_price_microlamports: u64,
    pub tip_account: Pubkey,
    pub tip_lamports: u64,
    pub blockhash: Hash,
    pub lookup_tables: &'a [AddressLookupTableAccount],
}

pub fn build_arb_instruction(
    program_id: Pubkey,
    payer: Pubkey,
    base_token_account: Pubkey,
    hops: &[ResolvedHop],
    amount_in: u64,
    min_profit: u64,
) -> Instruction {
    let params = IxParams {
        amount_in,
        min_profit,
        hops: hops
            .iter()
            .map(|h| HopParams {
                dex: h.dex,
                num_accounts: h.metas.len() as u8,
                source_index: h.source_index,
                a_to_b: h.a_to_b,
                min_amount_out: h.min_amount_out,
            })
            .collect(),
    };
    let mut accounts = Vec::with_capacity(2 + hops.iter().map(|h| h.metas.len()).sum::<usize>());
    accounts.push(AccountMeta::new_readonly(payer, true));
    accounts.push(AccountMeta::new(base_token_account, false));
    for hop in hops {
        accounts.extend(hop.metas.iter().cloned());
    }
    Instruction {
        program_id,
        accounts,
        data: encode_instruction(&params),
    }
}

pub fn build_bundle_transaction(p: &BundleParams<'_>) -> Result<VersionedTransaction> {
    let payer_pk = p.payer.pubkey();
    let instructions = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(p.cu_limit),
        ComputeBudgetInstruction::set_compute_unit_price(p.cu_price_microlamports),
        build_arb_instruction(
            p.program_id,
            payer_pk,
            p.base_token_account,
            p.hops,
            p.amount_in,
            p.min_profit,
        ),
        // Tip lives INSIDE the atomic tx: if the profit check reverts, the
        // tip reverts with it and the failed attempt costs only the tx fee.
        system_instruction::transfer(&payer_pk, &p.tip_account, p.tip_lamports),
    ];

    let message = v0::Message::try_compile(&payer_pk, &instructions, p.lookup_tables, p.blockhash)
        .context("message compilation failed (too many accounts? add lookup tables)")?;
    let tx = VersionedTransaction::try_new(VersionedMessage::V0(message), &[p.payer])
        .context("signing failed")?;

    let size = bincode::serialize(&tx)
        .context("serialize for size check")?
        .len();
    if size > MAX_TX_BYTES {
        bail!(
            "transaction is {size} bytes (max {MAX_TX_BYTES}) — configure LOOKUP_TABLES \
             (run the create-alt binary) or reduce hops"
        );
    }
    Ok(tx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arb_common::ix::{parse_instruction, DexKind};

    /// Realistic hop shape: unique writable accounts, with the user owner
    /// (the payer) as the only signer meta — exactly what the resolver emits.
    fn fake_hop(dex: DexKind, n_metas: usize, source_index: u8, owner: Pubkey) -> ResolvedHop {
        let mut metas: Vec<AccountMeta> = (0..n_metas - 1)
            .map(|_| AccountMeta::new(Pubkey::new_unique(), false))
            .collect();
        metas.push(AccountMeta::new_readonly(owner, true));
        ResolvedHop {
            dex,
            metas,
            source_index,
            a_to_b: true,
            min_amount_out: 42,
        }
    }

    /// The instruction the builder emits must parse cleanly with the
    /// ON-CHAIN parser — cross-crate drift is a landed-then-reverted tx.
    #[test]
    fn builder_output_parses_on_chain() {
        let owner = Pubkey::new_unique();
        let hops = vec![
            fake_hop(DexKind::OrcaWhirlpool, 12, 4, owner),
            fake_hop(DexKind::RaydiumV4, 19, 16, owner),
        ];
        let ix = build_arb_instruction(
            Pubkey::new_unique(),
            owner,
            Pubkey::new_unique(),
            &hops,
            1_000_000_000,
            1_205_000,
        );
        let parsed = parse_instruction(&ix.data).expect("on-chain parser rejected builder output");
        assert_eq!(parsed.amount_in, 1_000_000_000);
        assert_eq!(parsed.min_profit, 1_205_000);
        assert_eq!(parsed.hops.len(), 2);
        assert_eq!(parsed.hops[0].dex, DexKind::OrcaWhirlpool);
        assert_eq!(parsed.hops[0].num_accounts, 12);
        assert_eq!(parsed.hops[0].source_index, 4);
        assert!(parsed.hops[0].a_to_b);
        assert_eq!(parsed.hops[1].num_accounts, 19);
        assert_eq!(parsed.hops[1].min_amount_out, 42);
        // account list: authority + base + hop slices
        assert_eq!(ix.accounts.len(), 2 + 12 + 19);
        assert!(ix.accounts[0].is_signer);
    }

    /// Real SOL->USDC->SOL topology: the token program, owner and the two
    /// user ATAs are SHARED between hops (hop 0's destination is hop 1's
    /// source) — that dedup is what lets a 2-hop route fit in one packet
    /// without lookup tables.
    #[test]
    fn realistic_two_hop_transaction_fits() {
        use crate::resolver::{RAYDIUM_V4_PROGRAM, TOKEN_PROGRAM, WHIRLPOOL_PROGRAM};
        let payer = Keypair::new();
        let owner = payer.pubkey();
        let wsol_ata = Pubkey::new_unique();
        let usdc_ata = Pubkey::new_unique();

        // Whirlpool a_to_b (WSOL -> USDC): program + 11 accounts.
        let mut wp = vec![
            AccountMeta::new_readonly(WHIRLPOOL_PROGRAM, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM, false),
            AccountMeta::new_readonly(owner, true),
        ];
        wp.push(AccountMeta::new(Pubkey::new_unique(), false)); // whirlpool
        wp.push(AccountMeta::new(wsol_ata, false));
        wp.push(AccountMeta::new(Pubkey::new_unique(), false)); // vault A
        wp.push(AccountMeta::new(usdc_ata, false));
        wp.push(AccountMeta::new(Pubkey::new_unique(), false)); // vault B
        for _ in 0..3 {
            wp.push(AccountMeta::new(Pubkey::new_unique(), false)); // ticks
        }
        wp.push(AccountMeta::new(Pubkey::new_unique(), false)); // oracle

        // Raydium (USDC -> WSOL): program + 18 accounts.
        let mut ray = vec![
            AccountMeta::new_readonly(RAYDIUM_V4_PROGRAM, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM, false),
        ];
        for _ in 0..13 {
            ray.push(AccountMeta::new(Pubkey::new_unique(), false)); // amm+market set
        }
        ray.push(AccountMeta::new_readonly(Pubkey::new_unique(), false)); // vault signer
        ray.push(AccountMeta::new(usdc_ata, false)); // user source
        ray.push(AccountMeta::new(wsol_ata, false)); // user dest
        ray.push(AccountMeta::new_readonly(owner, true));

        let hops = vec![
            ResolvedHop {
                dex: DexKind::OrcaWhirlpool,
                metas: wp,
                source_index: 4,
                a_to_b: true,
                min_amount_out: 1,
            },
            ResolvedHop {
                dex: DexKind::RaydiumV4,
                metas: ray,
                source_index: 16,
                a_to_b: false,
                min_amount_out: 1,
            },
        ];
        let tx = build_bundle_transaction(&BundleParams {
            program_id: Pubkey::new_unique(),
            payer: &payer,
            base_token_account: wsol_ata,
            hops: &hops,
            amount_in: 5_000_000_000,
            min_profit: 2_000_000,
            cu_limit: 700_000,
            cu_price_microlamports: 10_000,
            tip_account: Pubkey::new_unique(),
            tip_lamports: 1_000_000,
            blockhash: Hash::new_unique(),
            lookup_tables: &[],
        })
        .expect("realistic 2-hop tx must fit without lookup tables");
        assert_eq!(tx.signatures.len(), 1);
        let size = bincode::serialize(&tx).unwrap().len();
        assert!(size <= MAX_TX_BYTES, "tx {size} bytes exceeds packet");
    }

    /// Worst case (zero account sharing) must be REJECTED with the
    /// actionable lookup-table error, never silently submitted.
    #[test]
    fn oversized_transaction_is_rejected() {
        let payer = Keypair::new();
        let hops = vec![
            fake_hop(DexKind::OrcaWhirlpool, 12, 4, payer.pubkey()),
            fake_hop(DexKind::RaydiumV4, 19, 16, payer.pubkey()),
        ];
        let err = build_bundle_transaction(&BundleParams {
            program_id: Pubkey::new_unique(),
            payer: &payer,
            base_token_account: Pubkey::new_unique(),
            hops: &hops,
            amount_in: 5_000_000_000,
            min_profit: 2_000_000,
            cu_limit: 700_000,
            cu_price_microlamports: 10_000,
            tip_account: Pubkey::new_unique(),
            tip_lamports: 1_000_000,
            blockhash: Hash::new_unique(),
            lookup_tables: &[],
        })
        .expect_err("all-unique 31-account tx cannot fit in one packet");
        assert!(
            err.to_string().contains("LOOKUP_TABLES"),
            "unhelpful error: {err}"
        );
    }
}
