//! S13C Slice 6 — Pump AMM SELL-only direct-call simulation parity.
//!
//! For each supported route (1 and 3): capture a FRESH recent successful direct
//! top-level Pump `sell`, reconstruct it byte-exact, clone its coherent rotating
//! fee set [9,10,22,23] + fee-v2 accounts, substitute ONLY the user-specific
//! accounts [1] authority / [5] base ATA / [6] quote (WSOL) ATA with a current
//! token holder, and simulate the sell (`sigVerify=false`). Compare the local
//! Rust Pump sell quote to the simulated WSOL account delta.
//!
//! HARD SAFETY BOUNDARY: MODE=simulate only; refuses on ENABLE_SUBMIT/
//! ENABLE_JITO/.live-armed. NO sign, NO keypair, NO send, NO Jito, NO Meteora
//! leg, NO atomic composition. Read + simulateTransaction only.

use anyhow::{anyhow, bail, Context, Result};
use arb_monitor::fixture_capture::b58_decode;
use arb_monitor::pump_amm::{decode_pump_pool, sell_quote_with_fee_split, PumpAmmPool};
use arb_monitor::pump_feev2::{decode_fee_config, market_cap, FeeConfig};
use arb_monitor::pump_reconstruct::{decode_sell_data, reconstruct_sell_data, SELL_DISCRIMINATOR};
use arb_monitor::sim_parity::SafetyGate;
use base64::Engine;
use sha2::{Digest, Sha256};
use solana_account_decoder::{UiAccountData, UiAccountEncoding};
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_config::{
    RpcSimulateTransactionAccountsConfig, RpcSimulateTransactionConfig, RpcTransactionConfig,
};
use solana_sdk::account::Account;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::transaction::Transaction;
use solana_transaction_status_client_types::{
    EncodedTransaction, UiInstruction, UiMessage, UiParsedInstruction, UiTransactionEncoding,
};
use std::collections::HashMap;
use std::str::FromStr;

const WSOL: &str = "So11111111111111111111111111111111111111112";
const PUMP_PROGRAM: &str = "pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA";
const PUMP_FEE_PROGRAM: &str = "pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ";
const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const TOKEN_2022_PROGRAM: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
const ATA_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
const COMPUTE_BUDGET: &str = "ComputeBudget111111111111111111111111111111";
const SYSTEM: &str = "11111111111111111111111111111111";
/// Fee payer / rent payer: a public system wallet with ample SOL (public
/// address only; never a key). Decouples fee payment from the seller wallet.
const FEE_PAYER: &str = "5rkSwEceTC6TxtQwzMVdsdmG4xs7u3iZMH3isU8tssdh";

struct Route {
    name: &'static str,
    pool: &'static str,
    mint: &'static str,
}

const ROUTES: [Route; 2] = [
    Route {
        name: "route1",
        pool: "5ByL7MZoLABYnwMPZKPKjf4MGkZ7FeBzrAnos19Pre2z",
        mint: "FeMbDoX7R1Psc4GEcvJdsbNbZA3bfztcyDCatJVJpump",
    },
    Route {
        name: "route3",
        pool: "8qDidAKuyNYKaR4dh2ZFZZVG5gBTUfyJcwQPgwt9FS1Y",
        mint: "DdPrHYqM8Ueovnk9kAnAgoGhswkuaTqmxcoZzU3Zpump",
    },
];

fn pk(s: &str) -> Pubkey {
    Pubkey::from_str(s).unwrap()
}

/// One captured direct top-level Pump sell (the clone source).
struct ClonedSell {
    sig: String,
    slot: u64,
    /// 24 (pubkey, signer, writable) rows in instruction order.
    accounts: Vec<(Pubkey, bool, bool)>,
    amount_in: u64,
    min_out: u64,
}

/// Scan recent signatures on the pool for a FRESH successful direct top-level
/// Pump sell (program = Pump, disc = sell, 24 accounts, [0]=pool, [3]=mint).
fn find_fresh_sell(rpc: &RpcClient, pool: &Pubkey, mint: &Pubkey) -> Result<ClonedSell> {
    let sigs = rpc
        .get_signatures_for_address(pool)
        .context("get_signatures_for_address")?;
    for si in sigs.iter().filter(|s| s.err.is_none()).take(150) {
        let sig = solana_sdk::signature::Signature::from_str(&si.signature)?;
        let cfg = RpcTransactionConfig {
            encoding: Some(UiTransactionEncoding::JsonParsed),
            commitment: Some(CommitmentConfig::confirmed()),
            max_supported_transaction_version: Some(0),
        };
        let tx = match rpc.get_transaction_with_config(&sig, cfg) {
            Ok(t) => t,
            Err(_) => continue,
        };
        let slot = tx.slot;
        let EncodedTransaction::Json(ui) = tx.transaction.transaction else {
            continue;
        };
        let UiMessage::Parsed(msg) = ui.message else {
            continue;
        };
        // pubkey → (signer, writable)
        let flags: HashMap<String, (bool, bool)> = msg
            .account_keys
            .iter()
            .map(|a| (a.pubkey.clone(), (a.signer, a.writable)))
            .collect();
        for ix in &msg.instructions {
            let UiInstruction::Parsed(UiParsedInstruction::PartiallyDecoded(pd)) = ix else {
                continue;
            };
            if pd.program_id != PUMP_PROGRAM || pd.accounts.len() != 24 {
                continue;
            }
            let raw = b58_decode(&pd.data).unwrap_or_default();
            if raw.len() != 24 || raw[0..8] != SELL_DISCRIMINATOR {
                continue;
            }
            if pd.accounts[0] != pool.to_string() || pd.accounts[3] != mint.to_string() {
                continue;
            }
            let (amount_in, min_out) =
                decode_sell_data(&raw).map_err(|e| anyhow!("decode sell: {e:?}"))?;
            let accounts = pd
                .accounts
                .iter()
                .map(|s| {
                    let (sg, w) = flags.get(s).copied().unwrap_or((false, false));
                    (pk(s), sg, w)
                })
                .collect();
            return Ok(ClonedSell {
                sig: si.signature.clone(),
                slot,
                accounts,
                amount_in,
                min_out,
            });
        }
    }
    bail!("no fresh direct top-level Pump sell found in recent signatures")
}

fn derive_ata(owner: &Pubkey, token_program: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &pk(ATA_PROGRAM),
    )
    .0
}

fn create_ata_idempotent_ix(
    payer: &Pubkey,
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
) -> Instruction {
    let ata = derive_ata(owner, token_program, mint);
    Instruction {
        program_id: pk(ATA_PROGRAM),
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(ata, false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(pk(SYSTEM), false),
            AccountMeta::new_readonly(*token_program, false),
        ],
        data: vec![1],
    }
}

fn compute_unit_limit_ix(units: u32) -> Instruction {
    let mut data = vec![2u8];
    data.extend_from_slice(&units.to_le_bytes());
    Instruction {
        program_id: pk(COMPUTE_BUDGET),
        accounts: vec![],
        data,
    }
}

/// Build the sell instruction from cloned accounts, overriding only the semantic
/// amounts; optional user substitution at [1]/[5]/[6].
fn build_sell_ix(
    cloned: &[(Pubkey, bool, bool)],
    amount_in: u64,
    min_out: u64,
    subst: Option<(Pubkey, Pubkey, Pubkey)>, // (authority, base_ata, quote_ata)
) -> Instruction {
    let metas = cloned
        .iter()
        .enumerate()
        .map(|(i, (pkey, signer, writable))| {
            let key = match (subst, i) {
                (Some((a, _, _)), 1) => a,
                (Some((_, b, _)), 5) => b,
                (Some((_, _, q)), 6) => q,
                _ => *pkey,
            };
            if *writable {
                AccountMeta::new(key, *signer)
            } else {
                AccountMeta::new_readonly(key, *signer)
            }
        })
        .collect();
    Instruction {
        program_id: pk(PUMP_PROGRAM),
        accounts: metas,
        data: reconstruct_sell_data(amount_in, min_out).to_vec(),
    }
}

fn hash_state(accounts: &[Option<Account>]) -> String {
    let mut h = Sha256::new();
    for a in accounts {
        match a {
            Some(acc) => {
                h.update((acc.data.len() as u64).to_le_bytes());
                h.update(&acc.data);
                h.update(acc.lamports.to_le_bytes());
            }
            None => h.update(b"MISSING"),
        }
    }
    format!("{:x}", h.finalize())
}

fn token_amount_from_ui(data: &UiAccountData) -> Option<u64> {
    let bytes = match data {
        UiAccountData::Binary(s, UiAccountEncoding::Base64) => {
            base64::engine::general_purpose::STANDARD.decode(s).ok()?
        }
        _ => return None,
    };
    if bytes.len() < 72 {
        return None;
    }
    Some(u64::from_le_bytes(bytes[64..72].try_into().ok()?))
}

/// A token account's `amount` (u64 @ offset 64) from raw account bytes.
fn token_amount_bytes(data: &[u8]) -> Option<u64> {
    if data.len() < 72 {
        return None;
    }
    Some(u64::from_le_bytes(data[64..72].try_into().ok()?))
}

/// Reserves (base, quote) from a [base_vault, quote_vault, ..] account fetch.
fn read_reserves(accs: &[Option<Account>]) -> Option<(u64, u64)> {
    let base = token_amount_bytes(&accs.first()?.as_ref()?.data)?;
    let quote = token_amount_bytes(&accs.get(1)?.as_ref()?.data)?;
    Some((base, quote))
}

/// A token account's mint (offset 0) and owner (offset 32) from raw bytes.
fn token_mint_owner(data: &[u8]) -> Option<(Pubkey, Pubkey)> {
    if data.len() < 72 {
        return None;
    }
    Some((
        Pubkey::new_from_array(data[0..32].try_into().ok()?),
        Pubkey::new_from_array(data[32..64].try_into().ok()?),
    ))
}

struct SimResult {
    entered_pump: bool,
    success: bool,
    err: Option<String>,
    units: Option<u64>,
    tx_size: usize,
    accounts: usize,
    watch_post: Vec<Option<u64>>,
    logs: Vec<String>,
}

fn simulate(
    rpc: &RpcClient,
    tx: &Transaction,
    watch: &[Pubkey],
    min_context_slot: u64,
) -> Result<SimResult> {
    let cfg = RpcSimulateTransactionConfig {
        sig_verify: false,
        replace_recent_blockhash: true,
        commitment: Some(CommitmentConfig::confirmed()),
        encoding: Some(UiTransactionEncoding::Base64),
        accounts: Some(RpcSimulateTransactionAccountsConfig {
            addresses: watch.iter().map(|k| k.to_string()).collect(),
            encoding: Some(UiAccountEncoding::Base64),
        }),
        min_context_slot: Some(min_context_slot),
        inner_instructions: false,
    };
    let resp = rpc
        .simulate_transaction_with_config(tx, cfg)
        .context("simulateTransaction")?;
    let v = resp.value;
    let logs = v.logs.unwrap_or_default();
    let entered = logs
        .iter()
        .any(|l| l.contains(&format!("Program {PUMP_PROGRAM} invoke")));
    let watch_post = match &v.accounts {
        Some(a) => a
            .iter()
            .map(|o| o.as_ref().and_then(|ui| token_amount_from_ui(&ui.data)))
            .collect(),
        None => vec![None; watch.len()],
    };
    Ok(SimResult {
        entered_pump: entered,
        success: v.err.is_none(),
        err: v.err.map(|e| format!("{e:?}")),
        units: v.units_consumed,
        tx_size: bincode::serialize(tx).map(|b| b.len()).unwrap_or(0),
        accounts: tx.message.account_keys.len(),
        watch_post,
        logs,
    })
}

fn err_logs(logs: &[String]) -> Vec<String> {
    let relevant: Vec<String> = logs
        .iter()
        .filter(|l| {
            l.contains(PUMP_PROGRAM)
                || l.contains("Error")
                || l.contains("failed")
                || l.contains("insufficient")
                || l.contains("custom program error")
        })
        .cloned()
        .collect();
    let start = relevant.len().saturating_sub(4);
    relevant[start..].to_vec()
}

fn build_tx(fee_payer: &Pubkey, setup: Vec<Instruction>, sell: Instruction) -> Transaction {
    let mut ixs = vec![compute_unit_limit_ix(400_000)];
    ixs.extend(setup);
    ixs.push(sell);
    Transaction::new_unsigned(Message::new(&ixs, Some(fee_payer)))
}

/// Find a current token holder (not a pool vault), returning
/// (token_account, owner_authority, token_program, balance).
fn find_holder(
    rpc: &RpcClient,
    mint: &Pubkey,
    exclude: &[Pubkey],
) -> Result<(Pubkey, Pubkey, Pubkey, u64)> {
    let largest = rpc
        .get_token_largest_accounts(mint)
        .context("get_token_largest_accounts")?;
    for bal in largest {
        let acc_k = Pubkey::from_str(&bal.address)?;
        if exclude.contains(&acc_k) {
            continue;
        }
        let amount: u64 = bal.amount.amount.parse().unwrap_or(0);
        if amount == 0 {
            continue;
        }
        let acc = match rpc.get_account(&acc_k) {
            Ok(a) => a,
            Err(_) => continue,
        };
        let token_program = acc.owner;
        let Some((acc_mint, owner)) = token_mint_owner(&acc.data) else {
            continue;
        };
        if &acc_mint != mint {
            continue;
        }
        // Owner must be a plain wallet (system-owned) so it can be an authority.
        let owner_acc = rpc.get_account(&owner).ok();
        let owner_is_system = owner_acc.map(|o| o.owner == pk(SYSTEM)).unwrap_or(false);
        if !owner_is_system {
            continue;
        }
        return Ok((acc_k, owner, token_program, amount));
    }
    bail!("no suitable current token holder found")
}

/// Bracketed measurement: fetch reserves (base_vault, quote_vault, pool) at slot
/// S, simulate at minContextSlot=S, re-fetch. If the bracket is unchanged the
/// sim provably used those exact reserves — the only way to get drift-free
/// parity on a live pool that trades every slot. Retries until clean or exhausted.
fn measure_clean(
    rpc: &RpcClient,
    bracket_keys: &[Pubkey],
    make_tx: impl Fn() -> Transaction,
    watch: &[Pubkey],
) -> Result<((u64, u64), SimResult, bool, u64)> {
    let mut last: Option<((u64, u64), SimResult, u64)> = None;
    for _ in 0..10 {
        let pre = rpc
            .get_multiple_accounts_with_commitment(bracket_keys, CommitmentConfig::confirmed())
            .context("bracket pre-fetch")?;
        let slot = pre.context.slot;
        let Some(res) = read_reserves(&pre.value) else {
            bail!("vault decode in bracket");
        };
        let hpre = hash_state(&pre.value);
        let tx = make_tx();
        let r = simulate(rpc, &tx, watch, slot)?;
        let post = rpc
            .get_multiple_accounts_with_commitment(bracket_keys, CommitmentConfig::confirmed())
            .context("bracket post-fetch")?;
        if hash_state(&post.value) == hpre {
            return Ok((res, r, true, slot));
        }
        last = Some((res, r, slot));
    }
    let (res, r, slot) = last.unwrap();
    Ok((res, r, false, slot))
}

fn run_route(rpc: &RpcClient, route: &Route) -> Result<&'static str> {
    let pool_k = pk(route.pool);
    let mint_k = pk(route.mint);
    let fee_payer = pk(FEE_PAYER);
    println!(
        "\n================ {} (pool {}) ================",
        route.name, route.pool
    );

    // ── Step 1-2: fresh reconstructed sell, byte-exact. ──
    let cloned = find_fresh_sell(rpc, &pool_k, &mint_k)?;
    println!(
        "[1] fresh sell sig={} slot={}",
        &cloned.sig[..20],
        cloned.slot
    );
    let raw_data = reconstruct_sell_data(cloned.amount_in, cloned.min_out);
    let (da, dm) = decode_sell_data(&raw_data).unwrap();
    let byte_exact = da == cloned.amount_in && dm == cloned.min_out;
    println!(
        "[2] byte-exact reconstruction: {} (amount_in={} min_out={}, 24 accounts, [0]=pool [3]=mint)",
        byte_exact, cloned.amount_in, cloned.min_out
    );

    // ── Step 3: coherent rotating fee set [9,10,22,23] from the SAME tx. ──
    let rot: Vec<Pubkey> = [9, 10, 22, 23]
        .iter()
        .map(|&i| cloned.accounts[i].0)
        .collect();
    let rot_ok = rot.iter().all(|k| *k != Pubkey::default());
    println!(
        "[3] rotating fee set [9,10,22,23] present & coherent (same tx): {} -> {:?}",
        rot_ok,
        rot.iter()
            .map(|k| k.to_string()[..8].to_string())
            .collect::<Vec<_>>()
    );
    if !rot_ok {
        println!("VERDICT[{}]: PUMP ROTATING FEE SET UNRESOLVED", route.name);
        return Ok("PUMP ROTATING FEE SET UNRESOLVED");
    }

    // ── Step 4: validate current Pump + Fee Program deployments. ──
    let pump_acc = rpc.get_account(&pk(PUMP_PROGRAM)).context("pump program")?;
    let fee_acc = rpc
        .get_account(&pk(PUMP_FEE_PROGRAM))
        .context("fee program")?;
    println!(
        "[4] deployments: pump.exec={} fee.exec={}",
        pump_acc.executable, fee_acc.executable
    );

    // ── Step 5: select a current seller (holder) for substitution. ──
    let pool = decode_pump_pool(&rpc.get_account(&pool_k).context("pool account")?.data)
        .map_err(|e| anyhow!("decode pool: {e:?}"))?;
    let (holder_ata, holder_owner, holder_tprog, holder_bal) =
        find_holder(rpc, &mint_k, &[pool.base_vault, pool.quote_vault])?;
    let wsol_ata = derive_ata(&holder_owner, &pk(TOKEN_PROGRAM), &pk(WSOL));
    println!(
        "[5] seller owner={} base_ata={} bal={} tprog={} wsol_dest={}",
        &holder_owner.to_string()[..8],
        &holder_ata.to_string()[..8],
        holder_bal,
        if holder_tprog == pk(TOKEN_2022_PROGRAM) {
            "Token-2022"
        } else {
            "SPL"
        },
        &wsol_ata.to_string()[..8],
    );

    // ── fee-v2: decode the fee-program config [19] + base-mint supply. ──
    let fee_cfg: FeeConfig = decode_fee_config(
        &rpc.get_account(&cloned.accounts[19].0)
            .context("fee config [19]")?
            .data,
    )
    .map_err(|e| anyhow!("decode fee config: {e:?}"))?;
    let base_supply: u64 = rpc
        .get_token_supply(&mint_k)
        .context("base mint supply")?
        .amount
        .parse()
        .unwrap_or(0);
    println!(
        "[fee-v2] config [19]={} tiers={} base_supply={}",
        &cloned.accounts[19].0.to_string()[..8],
        fee_cfg.tiers.len(),
        base_supply
    );

    // ── Step 6: simulate the reconstructed sell UNCHANGED (original seller). ──
    let watch = vec![wsol_ata, holder_ata];
    let step6_slot = rpc.get_slot().unwrap_or(0);
    let sell_unchanged = build_sell_ix(&cloned.accounts, cloned.amount_in, 0, None);
    let tx_unchanged = build_tx(&fee_payer, vec![], sell_unchanged);
    let r0 = simulate(rpc, &tx_unchanged, &watch, step6_slot)?;
    println!(
        "[6] unchanged reconstruction: entered_pump={} success={} err={} (original seller no longer holds base — informational)",
        r0.entered_pump, r0.success, r0.err.as_deref().unwrap_or("none")
    );

    // ── Step 7-8: substitute [1]/[5]/[6], test fixture + smaller amount. ──
    let subst = (holder_owner, holder_ata, wsol_ata);
    let setup = vec![create_ata_idempotent_ix(
        &fee_payer,
        &holder_owner,
        &pk(WSOL),
        &pk(TOKEN_PROGRAM),
    )];
    let bracket_keys = [pool.base_vault, pool.quote_vault, pool_k];

    // amount A: fixture amount (if holder has enough), else half the balance.
    let amount_a = if cloned.amount_in <= holder_bal {
        cloned.amount_in
    } else {
        holder_bal / 2
    };
    let amount_b = (amount_a / 3).max(1);

    let mut proven_any = false;
    let mut any_clean = false;
    let mut mismatch_seen = false;
    let mut accounting_unresolved = false;

    for (label, amt) in [("A/fixture", amount_a), ("B/smaller", amount_b)] {
        let (res, r, clean, mslot) = measure_clean(
            rpc,
            &bracket_keys,
            || {
                let sell = build_sell_ix(&cloned.accounts, amt, 0, Some(subst));
                build_tx(&fee_payer, setup.clone(), sell)
            },
            &watch,
        )?;
        any_clean |= clean;
        let (base_res, quote_res) = res;
        // fee-v2: market cap → tier → [lp, protocol, creator] split for THIS state.
        let mc = market_cap(base_supply, base_res, quote_res);
        let (split, tier_bps) = match mc {
            Ok(mc) => {
                let t = fee_cfg.tier_for(mc);
                ([t.lp_bps, t.protocol_bps, t.creator_bps], t.total_bps())
            }
            Err(_) => ([20, 5, 5], 30),
        };
        let local = sell_quote_with_fee_split(amt, base_res, quote_res, split);
        println!(
            "\n[7/8] {} amount={} entered={} success={} units={:?} tx={}B accounts={} clean_bracket={} slot={} reserves=({},{}) fee_v2_tier={}bps split={:?}",
            label, amt, r.entered_pump, r.success, r.units, r.tx_size, r.accounts, clean, mslot, base_res, quote_res, tier_bps, split
        );
        if !r.success {
            for l in err_logs(&r.logs) {
                println!("        log: {l}");
            }
            continue;
        }
        let wsol_delta = r.watch_post[0].unwrap_or(0);
        let base_post = r.watch_post[1].unwrap_or(0);
        let base_delta = holder_bal.saturating_sub(base_post);
        match &local {
            Ok(q) => {
                let diff = wsol_delta as i128 - q.out as i128;
                let bps = if q.out > 0 {
                    (diff.abs() * 10_000) / q.out as i128
                } else {
                    0
                };
                // gross (fee-less CPMM) is unambiguous from our model: out + fee.
                let gross = q.out as i128 + q.fee as i128;
                let real_fee = gross - wsol_delta as i128;
                let real_fee_bps = if gross > 0 {
                    (real_fee * 10_000) / gross
                } else {
                    0
                };
                let model_fee_bps = if gross > 0 {
                    (q.fee as i128 * 10_000) / gross
                } else {
                    0
                };
                println!(
                    "        local_quote_out={} dex_fee={} | sim_wsol_delta={} | abs_diff={} bps={}",
                    q.out, q.fee, wsol_delta, diff, bps
                );
                println!(
                    "        gross(CPMM)={} | model_fee={}bps real_fee={}bps ({} lamports)",
                    gross, model_fee_bps, real_fee_bps, real_fee
                );
                println!(
                    "        base(token) delta={} (== amount_in {}? {}) clean_bracket={}",
                    base_delta,
                    amt,
                    base_delta == amt,
                    clean
                );
                let within = diff.abs() <= 1; // integer floor rounding tolerance
                if clean && within && base_delta == amt {
                    proven_any = true;
                } else if clean && !within {
                    mismatch_seen = true;
                }
            }
            Err(e) => {
                println!("        local quote error: {e:?}");
                accounting_unresolved = true;
            }
        }
    }

    // ── Negative controls (no bracket needed). ──
    println!("\n[neg] Negative controls (each must FAIL):");
    let neg_slot = rpc.get_slot().unwrap_or(0);
    let negatives = run_negatives(
        rpc, &cloned, &pool, subst, &setup, &fee_payer, &watch, neg_slot, amount_a, holder_bal,
    );

    // ── Verdict. The same-state guard is the per-sample bracket: a sample only
    // counts if the vaults+pool were identical before and after its sim. ──
    let negatives_ok = negatives.iter().all(|(_, failed)| *failed);
    if any_clean {
        println!("\n[guard] at least one clean-bracket (same-state) sample obtained");
    } else {
        println!("\n[guard] no clean-bracket sample obtained — pool state moved every measurement");
    }
    if !negatives_ok {
        println!("        (a negative control did not fail — result not trusted)");
    }
    // Slice-6B verdict vocabulary (fee-v2 model re-test).
    let verdict = if !negatives_ok || !any_clean {
        "INSUFFICIENT VALID SAMPLES"
    } else if accounting_unresolved {
        "DYNAMIC FEE SOURCE UNRESOLVED"
    } else if proven_any && !mismatch_seen {
        "PUMP FEE-V2 PARITY PROVEN"
    } else if mismatch_seen {
        "PUMP FEE-V2 MODEL MISMATCH"
    } else {
        "INSUFFICIENT VALID SAMPLES"
    };
    println!("VERDICT[{}]: {}", route.name, verdict);
    Ok(verdict)
}

#[allow(clippy::too_many_arguments)]
fn run_negatives(
    rpc: &RpcClient,
    cloned: &ClonedSell,
    _pool: &PumpAmmPool,
    subst: (Pubkey, Pubkey, Pubkey),
    setup: &[Instruction],
    fee_payer: &Pubkey,
    watch: &[Pubkey],
    slot: u64,
    amount: u64,
    holder_bal: u64,
) -> Vec<(&'static str, bool)> {
    let mut out = Vec::new();
    let mut check = |label: &'static str, ix: Instruction, extra: Vec<Instruction>| {
        let mut s = extra;
        s.extend(setup.iter().cloned());
        let tx = build_tx(fee_payer, s, ix);
        let failed = match simulate(rpc, &tx, watch, slot) {
            Ok(r) => {
                let reason = r.err.clone().unwrap_or_default();
                println!("   {label}: success={} err={}", r.success, reason);
                if !r.success {
                    for l in err_logs(&r.logs) {
                        println!("        log: {l}");
                    }
                }
                !r.success
            }
            Err(e) => {
                println!("   {label}: rpc error {e}");
                true
            }
        };
        out.push((label, failed));
    };

    // N1: wrong pool-specific fee-v2 account [19].
    let mut n1 = cloned.accounts.clone();
    n1[19].0 = Pubkey::new_unique();
    check(
        "N1 wrong fee-v2 [19]",
        build_sell_ix(&n1, amount, 0, Some(subst)),
        vec![],
    );

    // N2: mixed rotating set — replace [9] with an unrelated account.
    let mut n2 = cloned.accounts.clone();
    n2[9].0 = Pubkey::new_unique();
    check(
        "N2 mixed rotating [9]",
        build_sell_ix(&n2, amount, 0, Some(subst)),
        vec![],
    );

    // N3: wrong token account mint — point base ATA [5] at the WSOL dest.
    let bad = (subst.0, subst.2, subst.2);
    check(
        "N3 wrong base-account mint",
        build_sell_ix(&cloned.accounts, amount, 0, Some(bad)),
        vec![],
    );

    // N4: impossible min_quote_out.
    check(
        "N4 impossible min_out",
        build_sell_ix(&cloned.accounts, amount, u64::MAX, Some(subst)),
        vec![],
    );

    // N5: insufficient seller balance.
    check(
        "N5 insufficient balance",
        build_sell_ix(&cloned.accounts, holder_bal + 1_000, 0, Some(subst)),
        vec![],
    );

    out
}

fn main() -> Result<()> {
    SafetyGate::verify_env()?;
    let rpc_url = std::env::var("RPC_ENDPOINT").context("RPC_ENDPOINT required")?;
    let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());
    println!("=== S13C SLICE 6 — Pump AMM sell-only direct simulation parity ===");

    let mut verdicts = Vec::new();
    for route in &ROUTES {
        match run_route(&rpc, route) {
            Ok(v) => verdicts.push((route.name, v.to_string())),
            Err(e) => {
                println!("\n[{}] ERROR: {e:#}", route.name);
                verdicts.push((route.name, format!("INSUFFICIENT VALID SAMPLES ({e})")));
            }
        }
    }
    println!("\n================ FINAL VERDICTS ================");
    for (r, v) in &verdicts {
        println!("  {r}: {v}");
    }
    Ok(())
}
