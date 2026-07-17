//! S13C Slice 5 — Route 1 Meteora-only DIRECT-CALL simulation smoke test.
//!
//! Answers: can the CPI-observed `swap2` instruction be invoked DIRECTLY as a
//! top-level instruction (no Jupiter / no caller program)? Uses a public
//! simulation user that self-wraps native SOL (the spec's "enough native SOL to
//! create it in simulation" path), builds a direct top-level `swap2`, and runs
//! `simulateTransaction` with `sigVerify=false`, `replaceRecentBlockhash=true`,
//! and `minContextSlot` = the single-slot snapshot slot.
//!
//! HARD SAFETY BOUNDARY: MODE=simulate only; refuses if ENABLE_SUBMIT/
//! ENABLE_JITO/.live-armed. NO signing, NO keypair load, NO send, NO Jito, NO
//! Pump instruction, NO atomic composition. Read + simulate only.

use anyhow::{anyhow, Context, Result};
use arb_monitor::meteora_direct_call::{
    audit_fixture, builder_vs_source_diff, verdict, DirectCallVerdict,
};
use arb_monitor::meteora_dlmm::{decode_bin_array, decode_lb_pair, BinArray, LbPair};
use arb_monitor::meteora_reconstruct::load as load_fixtures;
use arb_monitor::sim_parity::{
    bitmap_extension_pda, build_dlmm_swap2_ix, dlmm_oracle, dlmm_program, DlmmSwapAccounts,
    SafetyGate,
};
use base64::Engine;
use sha2::{Digest, Sha256};
use solana_account_decoder::{UiAccountData, UiAccountEncoding};
use solana_client::rpc_client::RpcClient;
use solana_client::rpc_config::{
    RpcSimulateTransactionAccountsConfig, RpcSimulateTransactionConfig,
};
use solana_sdk::account::Account;
use solana_sdk::commitment_config::CommitmentConfig;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::transaction::Transaction;
use solana_transaction_status_client_types::UiTransactionEncoding;
use std::collections::HashMap;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

const PAIR: &str = "CnK82s8exdsK9nwqQ55kd9wcxoA22NwTchZJCBdu8LDa";
const WSOL: &str = "So11111111111111111111111111111111111111112";
const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const TOKEN_2022_PROGRAM: &str = "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb";
const ATA_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
const COMPUTE_BUDGET: &str = "ComputeBudget111111111111111111111111111111";
/// Public simulation user: a plain system wallet with ample native SOL, taken
/// from a recent successful swapper on this pair. Public address only — no key.
const SIM_USER: &str = "5rkSwEceTC6TxtQwzMVdsdmG4xs7u3iZMH3isU8tssdh";

fn pk(s: &str) -> Pubkey {
    Pubkey::from_str(s).unwrap()
}

fn derive_ata(owner: &Pubkey, token_program: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &pk(ATA_PROGRAM),
    )
    .0
}

/// Associated-token-account `CreateIdempotent` (instruction tag 1).
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
            AccountMeta::new_readonly(pk("11111111111111111111111111111111"), false),
            AccountMeta::new_readonly(*token_program, false),
        ],
        data: vec![1],
    }
}

/// SPL-Token `SyncNative` (instruction tag 17) — reconciles a wSOL account's
/// token amount with its lamport balance after a native transfer.
fn sync_native_ix(account: &Pubkey) -> Instruction {
    Instruction {
        program_id: pk(TOKEN_PROGRAM),
        accounts: vec![AccountMeta::new(*account, false)],
        data: vec![17],
    }
}

/// System-program `Transfer` (enum variant 2) — fund the wSOL account with
/// native lamports before `SyncNative`. Hand-built to avoid the deprecated
/// `solana_sdk::system_instruction` module.
fn system_transfer_ix(from: &Pubkey, to: &Pubkey, lamports: u64) -> Instruction {
    let mut data = vec![2u8, 0, 0, 0];
    data.extend_from_slice(&lamports.to_le_bytes());
    Instruction {
        program_id: pk("11111111111111111111111111111111"),
        accounts: vec![AccountMeta::new(*from, true), AccountMeta::new(*to, false)],
        data,
    }
}

/// ComputeBudget `SetComputeUnitLimit` (tag 2).
fn compute_unit_limit_ix(units: u32) -> Instruction {
    let mut data = vec![2u8];
    data.extend_from_slice(&units.to_le_bytes());
    Instruction {
        program_id: pk(COMPUTE_BUDGET),
        accounts: vec![],
        data,
    }
}

/// SHA-256 over the (slot-consistent) data of the mutable market accounts, in a
/// fixed order. Used by the same-state guard.
fn hash_state(accounts: &[Option<Account>]) -> String {
    let mut h = Sha256::new();
    for a in accounts {
        match a {
            Some(acc) => {
                h.update((acc.data.len() as u64).to_le_bytes());
                h.update(&acc.data);
            }
            None => h.update(b"MISSING"),
        }
    }
    format!("{:x}", h.finalize())
}

/// Read a token account's `amount` (u64 @ offset 64) from a simulated UiAccount.
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

struct Snapshot {
    slot: u64,
    hash: String,
    pair: LbPair,
    bin_arrays: HashMap<i64, BinArray>,
    /// Keys hashed (for re-fetch), in order.
    keys: Vec<Pubkey>,
}

struct SimOutcome {
    label: String,
    entered_meteora: bool,
    success: bool,
    err: Option<String>,
    units: Option<u64>,
    tx_size: usize,
    static_accounts: usize,
    bin_array_count: usize,
    wsol_post: Option<u64>,
    dest_post: Option<u64>,
    logs: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
fn build_swap_tx(
    user: &Pubkey,
    pair_k: &Pubkey,
    pair: &LbPair,
    wsol_ata: &Pubkey,
    dest_ata: &Pubkey,
    dest_mint: &Pubkey,
    bin_arrays: &[Pubkey],
    bitmap_ext: Option<Pubkey>,
    amount_in: u64,
    min_out: u64,
) -> Transaction {
    // Setup: high CU limit, create wSOL ATA, wrap `amount_in` SOL, sync, create
    // the Token-2022 destination ATA — all owned by / paid by the sim user.
    let wsol = pk(WSOL);
    let spl = pk(TOKEN_PROGRAM);
    let t22 = pk(TOKEN_2022_PROGRAM);
    let mut ixs = vec![
        compute_unit_limit_ix(600_000),
        create_ata_idempotent_ix(user, user, &wsol, &spl),
        system_transfer_ix(user, wsol_ata, amount_in),
        sync_native_ix(wsol_ata),
        create_ata_idempotent_ix(user, user, dest_mint, &t22),
    ];

    let acct = DlmmSwapAccounts {
        lb_pair: *pair_k,
        reserve_x: pair.reserve_x,
        reserve_y: pair.reserve_y,
        token_x_mint: pair.token_x_mint,
        token_y_mint: pair.token_y_mint,
        token_x_2022: pair.token_x_program_flag == 1,
        token_y_2022: pair.token_y_program_flag == 1,
        user: *user,
        user_token_in: *wsol_ata,  // WSOL in (token_y)
        user_token_out: *dest_ata, // memecoin out (token_x)
        bin_arrays: bin_arrays.to_vec(),
    };
    ixs.push(build_dlmm_swap2_ix(&acct, bitmap_ext, amount_in, min_out));

    Transaction::new_unsigned(Message::new(&ixs, Some(user)))
}

fn simulate(
    rpc: &RpcClient,
    label: &str,
    tx: &Transaction,
    watch: &[Pubkey],
    min_context_slot: u64,
) -> Result<SimOutcome> {
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
        .context("simulateTransaction RPC failed")?;
    let v = resp.value;
    let logs = v.logs.unwrap_or_default();
    let dlmm = dlmm_program().to_string();
    let entered = logs
        .iter()
        .any(|l| l.contains(&format!("Program {dlmm} invoke")));
    let (wsol_post, dest_post) = match &v.accounts {
        Some(a) => (
            a.first()
                .and_then(|o| o.as_ref())
                .and_then(|ui| token_amount_from_ui(&ui.data)),
            a.get(1)
                .and_then(|o| o.as_ref())
                .and_then(|ui| token_amount_from_ui(&ui.data)),
        ),
        None => (None, None),
    };
    let tx_size = bincode::serialize(tx).map(|b| b.len()).unwrap_or(0);
    Ok(SimOutcome {
        label: label.to_string(),
        entered_meteora: entered,
        success: v.err.is_none(),
        err: v.err.map(|e| format!("{e:?}")),
        units: v.units_consumed,
        tx_size,
        static_accounts: tx.message.account_keys.len(),
        bin_array_count: 0,
        wsol_post,
        dest_post,
        logs,
    })
}

fn snapshot(rpc: &RpcClient, pair_k: &Pubkey, bin_idxs: &[i64]) -> Result<Snapshot> {
    let prog = dlmm_program();
    let oracle = dlmm_oracle(pair_k);
    // First read the pair to get reserves.
    let pair_acc = rpc.get_account(pair_k).context("fetch pair")?;
    let pair = decode_lb_pair(&pair_acc.data).map_err(|e| anyhow!("decode pair: {e:?}"))?;

    let bin_keys: Vec<Pubkey> = bin_idxs
        .iter()
        .map(|&i| {
            Pubkey::find_program_address(&[b"bin_array", pair_k.as_ref(), &i.to_le_bytes()], &prog)
                .0
        })
        .collect();

    let mut keys = vec![*pair_k, pair.reserve_x, pair.reserve_y, oracle];
    keys.extend(bin_keys.iter().cloned());

    let resp = rpc
        .get_multiple_accounts_with_commitment(&keys, CommitmentConfig::confirmed())
        .context("snapshot getMultipleAccounts")?;
    let slot = resp.context.slot;
    let accs = resp.value;
    let hash = hash_state(&accs);

    let mut bin_arrays = HashMap::new();
    for (n, &idx) in bin_idxs.iter().enumerate() {
        if let Some(Some(acc)) = accs.get(4 + n) {
            if acc.owner == prog {
                if let Ok(ba) = decode_bin_array(&acc.data) {
                    bin_arrays.insert(idx, ba);
                }
            }
        }
    }
    Ok(Snapshot {
        slot,
        hash,
        pair,
        bin_arrays,
        keys,
    })
}

fn refetch_hash(rpc: &RpcClient, keys: &[Pubkey]) -> Result<String> {
    let resp = rpc
        .get_multiple_accounts_with_commitment(keys, CommitmentConfig::confirmed())
        .context("re-fetch getMultipleAccounts")?;
    Ok(hash_state(&resp.value))
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn main() -> Result<()> {
    // ── Safety gate first (never proceed unless MODE=simulate & unarmed). ──
    SafetyGate::verify_env()?;
    let rpc_url = std::env::var("RPC_ENDPOINT").context("RPC_ENDPOINT required")?;
    let rpc = RpcClient::new_with_commitment(rpc_url, CommitmentConfig::confirmed());

    let pair_k = pk(PAIR);
    let user = pk(SIM_USER);
    let wsol = pk(WSOL);
    let t22 = pk(TOKEN_2022_PROGRAM);
    let spl = pk(TOKEN_PROGRAM);

    println!("=== S13C SLICE 5 — Route 1 Meteora direct-call simulation ===\n");

    // ── Stage 5.0 privilege audit (pure, from fixtures). ──
    let route1 = load_fixtures()
        .routes
        .into_values()
        .find(|r| r.route == "route1")
        .ok_or_else(|| anyhow!("route1 fixtures missing"))?;
    let vdt = verdict(&route1);
    println!("[5.0] Direct-call privilege verdict: {vdt:?}");
    let fx0 = &route1.cpi_fixtures[0];
    println!("[5.0] Privilege table (fixture {}):", &fx0.sig[..16]);
    println!("   idx  role                       idl(s/w)  src(s/w)  sentinel  callerPDA  replace");
    for row in audit_fixture(fx0) {
        println!(
            "   {:>3}  {:<26} {}/{}       {}/{}       {:<8} {:<9} {}",
            row.index,
            format!("{:?}", row.role),
            row.idl_signer as u8,
            row.idl_writable as u8,
            row.source_signer as u8,
            row.source_writable as u8,
            row.is_none_sentinel,
            row.caller_pda_signing,
            row.safe_to_replace,
        );
    }
    for fx in &route1.cpi_fixtures {
        let diffs = builder_vs_source_diff(fx);
        if !diffs.is_empty() {
            println!(
                "[5.0] builder↔source privilege deltas in {}:",
                &fx.sig[..16]
            );
            for d in diffs {
                println!(
                    "        [{}] {:?}.{} builder={} source={}",
                    d.index, d.role, d.field, d.builder, d.source
                );
            }
        }
    }
    if vdt != DirectCallVerdict::PrivilegesResolvedViable {
        println!("\nFINAL VERDICT: DIRECT CALL PRIVILEGES UNRESOLVED");
        return Ok(());
    }

    // ── Stage 5.2 public sim-user discovery. ──
    let user_acc = rpc.get_account(&user).context("fetch sim user")?;
    println!("\n[5.2] Sim user {SIM_USER}");
    println!(
        "        system-owned={} lamports={} (self-wraps native SOL for wSOL source)",
        user_acc.owner == pk("11111111111111111111111111111111"),
        user_acc.lamports
    );
    let wsol_ata = derive_ata(&user, &spl, &wsol);

    // ── Snapshot (single slot) for the active region: walk UP from aidx. ──
    // WSOL→token is Y→X (walk up), so include the active array and the next few
    // ascending arrays. Read the pair once to learn the active array index.
    let pair_probe = rpc.get_account(&pair_k).context("probe pair")?;
    let active_aidx = (decode_lb_pair(&pair_probe.data)
        .map_err(|e| anyhow!("decode pair: {e:?}"))?
        .active_id as i64)
        .div_euclid(70);
    let bin_idxs: Vec<i64> = (active_aidx..=active_aidx + 3).collect();
    let snap = snapshot(&rpc, &pair_k, &bin_idxs)?;
    let dest_mint = snap.pair.token_x_mint;
    let dest_ata = derive_ata(&user, &t22, &dest_mint);
    let bitmap_pda = bitmap_extension_pda(&pair_k);
    let bitmap_ext_exists = rpc
        .get_account_with_commitment(&bitmap_pda, CommitmentConfig::confirmed())
        .map(|r| r.value.is_some())
        .unwrap_or(false);
    let bitmap_ext = if bitmap_ext_exists {
        Some(bitmap_pda)
    } else {
        None
    };

    println!(
        "\n[5.2] Snapshot slot={} active_id={} aidx={} bin_arrays_present={:?} bitmap_ext={}",
        snap.slot,
        snap.pair.active_id,
        active_aidx,
        snap.bin_arrays.keys().collect::<Vec<_>>(),
        bitmap_ext
            .map(|k| k.to_string())
            .unwrap_or_else(|| "None-sentinel".into())
    );
    println!("        wSOL source ATA (to create+wrap): {wsol_ata}");
    println!("        dest Token-2022 ATA (to create):  {dest_ata}");
    println!("        state hash: {}", &snap.hash[..16]);

    // Bin arrays for the instruction: ascending existing arrays (walk-up order).
    let bin_keys_asc: Vec<Pubkey> = bin_idxs
        .iter()
        .filter(|i| snap.bin_arrays.contains_key(i))
        .map(|&i| {
            Pubkey::find_program_address(
                &[b"bin_array", pair_k.as_ref(), &i.to_le_bytes()],
                &dlmm_program(),
            )
            .0
        })
        .collect();

    // ── Local quote (Rust engine) for the tested input. ──
    let amount_in: u64 = 100_000_000; // 0.1 SOL (Control B)
    let local_quote = arb_monitor::meteora_dlmm::dlmm_quote_exact_in_detailed(
        &snap.pair,
        &snap.bin_arrays,
        false, // Y in → X out
        amount_in,
        now_unix(),
    );
    println!("\n[5.3] Local Rust quote for {amount_in} WSOL in: {local_quote:?}");

    let watch = vec![wsol_ata, dest_ata];

    // ── Control B: fresh semantic instruction, 0.1 SOL, min_out=0. ──
    let tx_b = build_swap_tx(
        &user,
        &pair_k,
        &snap.pair,
        &wsol_ata,
        &dest_ata,
        &dest_mint,
        &bin_keys_asc,
        bitmap_ext,
        amount_in,
        0,
    );
    let mut out_b = simulate(&rpc, "Control B (0.1 SOL, min=0)", &tx_b, &watch, snap.slot)?;
    out_b.bin_array_count = bin_keys_asc.len();
    print_outcome(&out_b);

    // ── Control A: smaller fresh amount (0.05 SOL). ──
    let amount_a: u64 = 50_000_000;
    let tx_a = build_swap_tx(
        &user,
        &pair_k,
        &snap.pair,
        &wsol_ata,
        &dest_ata,
        &dest_mint,
        &bin_keys_asc,
        bitmap_ext,
        amount_a,
        0,
    );
    let mut out_a = simulate(
        &rpc,
        "Control A (0.05 SOL, min=0)",
        &tx_a,
        &watch,
        snap.slot,
    )?;
    out_a.bin_array_count = bin_keys_asc.len();
    print_outcome(&out_a);

    // ── Negative controls (must fail for their own reason). ──
    println!("\n[5.3] Negative controls (each must FAIL for its own reason):");

    // N1: foreign bin arrays — derive the same indices for a DIFFERENT pair, so
    // none belong to THIS pool. Proves pair-membership enforcement.
    let foreign_pair = Pubkey::new_unique();
    let foreign_bins: Vec<Pubkey> = bin_idxs
        .iter()
        .map(|&i| {
            Pubkey::find_program_address(
                &[b"bin_array", foreign_pair.as_ref(), &i.to_le_bytes()],
                &dlmm_program(),
            )
            .0
        })
        .collect();
    let tx_n1 = build_swap_tx(
        &user,
        &pair_k,
        &snap.pair,
        &wsol_ata,
        &dest_ata,
        &dest_mint,
        &foreign_bins,
        bitmap_ext,
        amount_in,
        0,
    );
    let n1 = simulate(
        &rpc,
        "N1 foreign-pair bin arrays",
        &tx_n1,
        &watch,
        snap.slot,
    )?;
    print_negative(&n1);

    // N2: missing required (active) bin array — drop the first ascending array.
    let missing: Vec<Pubkey> = bin_keys_asc.iter().skip(1).cloned().collect();
    let tx_n2 = build_swap_tx(
        &user, &pair_k, &snap.pair, &wsol_ata, &dest_ata, &dest_mint, &missing, bitmap_ext,
        amount_in, 0,
    );
    let n2 = simulate(
        &rpc,
        "N2 missing active bin array",
        &tx_n2,
        &watch,
        snap.slot,
    )?;
    print_negative(&n2);

    // N3: wrong bitmap-extension account (bogus account where a sentinel/PDA is
    // required).
    let bogus = Pubkey::new_unique();
    let tx_n3 = build_swap_tx(
        &user,
        &pair_k,
        &snap.pair,
        &wsol_ata,
        &dest_ata,
        &dest_mint,
        &bin_keys_asc,
        Some(bogus),
        amount_in,
        0,
    );
    let n3 = simulate(
        &rpc,
        "N3 wrong bitmap-extension account",
        &tx_n3,
        &watch,
        snap.slot,
    )?;
    print_negative(&n3);

    // N4: impossible minimum output.
    let tx_n4 = build_swap_tx(
        &user,
        &pair_k,
        &snap.pair,
        &wsol_ata,
        &dest_ata,
        &dest_mint,
        &bin_keys_asc,
        bitmap_ext,
        amount_in,
        u64::MAX,
    );
    let n4 = simulate(
        &rpc,
        "N4 impossible min-out (u64::MAX)",
        &tx_n4,
        &watch,
        snap.slot,
    )?;
    print_negative(&n4);

    // ── Informational probe (NOT a gate): reordered valid arrays. ──
    // The DLMM program searches the remaining accounts for the array holding the
    // active bin, so a reversed (descending) order of VALID arrays is accepted.
    // The monotonic order seen in the slice-4 fixtures is a caller convention,
    // not a program requirement — recorded here, not counted as a must-fail.
    let mut rev = bin_keys_asc.clone();
    rev.reverse();
    let tx_ord = build_swap_tx(
        &user, &pair_k, &snap.pair, &wsol_ata, &dest_ata, &dest_mint, &rev, bitmap_ext, amount_in,
        0,
    );
    let ord = simulate(
        &rpc,
        "Probe: reordered valid arrays",
        &tx_ord,
        &watch,
        snap.slot,
    )?;
    println!(
        "\n[5.3] Order-tolerance probe: reversed valid arrays → success={} entered={} (informational; program is order-agnostic)",
        ord.success, ord.entered_meteora
    );

    // ── Same-state guard. ──
    let post_hash = refetch_hash(&rpc, &snap.keys)?;
    let same_state = post_hash == snap.hash;
    println!(
        "\n[5.3] Same-state guard: pre={} post={} unchanged={}",
        &snap.hash[..16],
        &post_hash[..16],
        same_state
    );

    // ── Output accounting for Control B. ──
    println!("\n[5.3] Output accounting (Control B, 0.1 SOL in):");
    if out_b.success {
        let dest_delta = out_b.dest_post.unwrap_or(0); // pre = 0 (fresh ATA)
        let wsol_post = out_b.wsol_post.unwrap_or(0);
        let wsol_consumed = amount_in.saturating_sub(wsol_post);
        println!("        wSOL wrapped (pre-swap) : {amount_in}");
        println!("        wSOL post-swap balance   : {wsol_post}  (consumed {wsol_consumed})");
        println!("        dest token delta (net)   : {dest_delta}");
        match &local_quote {
            Ok((q, fee)) => {
                let diff = dest_delta as i128 - *q as i128;
                let bps = if *q > 0 {
                    (diff.abs() * 10_000) / *q as i128
                } else {
                    0
                };
                println!("        local quote out          : {q} (dex fee {fee})");
                println!("        abs diff                 : {diff}");
                println!("        bps diff                 : {bps}");
            }
            Err(e) => println!("        local quote unavailable: {e:?}"),
        }
    } else {
        println!("        (Control B did not succeed — see logs above)");
    }

    // ── Final verdict. ──
    let verdict_str = final_verdict(
        &out_b,
        &out_a,
        &[&n1, &n2, &n3, &n4],
        same_state,
        &local_quote,
    );
    println!("\nFINAL VERDICT: {verdict_str}");
    Ok(())
}

fn print_outcome(o: &SimOutcome) {
    println!("\n[5.3] {}", o.label);
    println!(
        "        entered_meteora={} success={} units={:?} tx_size={}B accounts={} bins={}",
        o.entered_meteora, o.success, o.units, o.tx_size, o.static_accounts, o.bin_array_count
    );
    if let Some(e) = &o.err {
        println!("        err: {e}");
        for l in redacted_logs(&o.logs) {
            println!("        log: {l}");
        }
    }
}

fn print_negative(o: &SimOutcome) {
    println!(
        "   {} → success={} (expect false) err={}",
        o.label,
        o.success,
        o.err.as_deref().unwrap_or("<none>")
    );
    if o.success {
        println!("        !! UNEXPECTED SUCCESS — negative control did not fail");
    } else {
        // Show the DLMM/error-relevant log lines so the failure reason is
        // auditable (not the setup-instruction noise).
        let dlmm = dlmm_program().to_string();
        let relevant: Vec<&String> = o
            .logs
            .iter()
            .filter(|l| {
                l.contains(&dlmm)
                    || l.contains("Error")
                    || l.contains("failed")
                    || l.contains("custom program error")
            })
            .collect();
        for l in relevant.iter().rev().take(4).rev() {
            println!("        log: {l}");
        }
    }
}

/// Program logs with the RPC endpoint / api-key never present (logs are program
/// output only, but keep the guard explicit); truncate to a sane size.
fn redacted_logs(logs: &[String]) -> Vec<String> {
    logs.iter().take(40).cloned().collect()
}

fn final_verdict(
    b: &SimOutcome,
    a: &SimOutcome,
    negs: &[&SimOutcome],
    same_state: bool,
    local_quote: &Result<(u64, u64), arb_monitor::meteora_dlmm::DlmmQuoteError>,
) -> &'static str {
    if !b.entered_meteora && !a.entered_meteora {
        return "DIRECT TOP-LEVEL CALL NOT VIABLE";
    }
    if !b.success {
        return "INSUFFICIENT VALID SAMPLES";
    }
    if !same_state {
        return "INSUFFICIENT VALID SAMPLES";
    }
    // Every negative control must have failed.
    if negs.iter().any(|n| n.success) {
        return "INSUFFICIENT VALID SAMPLES";
    }
    match (b.dest_post, local_quote) {
        (Some(dest), Ok((q, _))) => {
            let diff = (dest as i128 - *q as i128).abs();
            let bps = if *q > 0 {
                (diff * 10_000) / *q as i128
            } else {
                i128::MAX
            };
            if bps <= 1 {
                "METEORA DIRECT PARITY PROVEN"
            } else {
                "METEORA QUOTE MISMATCH"
            }
        }
        _ => "INSUFFICIENT VALID SAMPLES",
    }
}
