//! arb-executor — listens to `arbitrage_opportunities` on Redis, prices a
//! dynamic Jito tip, builds the atomic on-chain-program transaction and
//! submits it as a Jito bundle.

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::{
    address_lookup_table::{state::AddressLookupTable, AddressLookupTableAccount},
    commitment_config::CommitmentConfig,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair},
    signer::Signer,
    transaction::Transaction,
};
use solana_system_interface::program as system_program;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Semaphore};
use tracing::{debug, info, warn};

use arb_common::opportunity::Opportunity;
use arb_executor::blockhash::BlockhashCache;
use arb_executor::builder::{build_bundle_transaction, BundleParams};
use arb_executor::config::Config;
use arb_executor::jito::{random_tip_account, JitoClient};
use arb_executor::resolver::{derive_ata, Resolver, ATA_PROGRAM, TOKEN_PROGRAM, WSOL_MINT};
use arb_executor::tip::compute_tip;

struct App {
    cfg: Config,
    rpc: Arc<RpcClient>,
    payer: Keypair,
    resolver: Resolver,
    jito: JitoClient,
    blockhash: BlockhashCache,
    lookup_tables: Vec<AddressLookupTableAccount>,
    /// cycle id -> last submission instant (resubmit throttle).
    recent: Mutex<HashMap<String, Instant>>,
    /// mints whose ATA existence has been ensured this run.
    atas_ready: Mutex<HashMap<Pubkey, ()>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cfg = Config::from_env()?;
    let payer = read_keypair_file(&cfg.keypair_path)
        .map_err(|e| anyhow::anyhow!("read keypair {}: {e}", cfg.keypair_path))?;
    info!(
        payer = %payer.pubkey(),
        program = %cfg.arb_program_id,
        dry_run = cfg.dry_run,
        enable_submit = cfg.enable_submit,
        enable_jito = cfg.enable_jito,
        "executor starting"
    );
    if !cfg.dry_run && cfg.enable_submit && cfg.enable_jito {
        warn!("SUBMISSION ARMED — live bundles will be sent to Jito");
    }

    let rpc = Arc::new(RpcClient::new_with_commitment(
        cfg.rpc_url.clone(),
        CommitmentConfig::processed(),
    ));
    let blockhash = BlockhashCache::start(rpc.clone()).await?;
    let lookup_tables = load_lookup_tables(&rpc, &cfg.lookup_tables).await?;
    if !lookup_tables.is_empty() {
        info!(count = lookup_tables.len(), "address lookup tables loaded");
    }

    let app = Arc::new(App {
        resolver: Resolver::new(
            rpc.clone(),
            payer.pubkey(),
            Duration::from_secs(cfg.whirlpool_ttl_secs),
        ),
        jito: JitoClient::new(cfg.jito_url.clone())?,
        blockhash,
        lookup_tables,
        recent: Mutex::new(HashMap::new()),
        atas_ready: Mutex::new(HashMap::new()),
        rpc,
        payer,
        cfg,
    });

    run_redis_loop(app).await
}

async fn load_lookup_tables(
    rpc: &RpcClient,
    addresses: &[Pubkey],
) -> Result<Vec<AddressLookupTableAccount>> {
    let mut out = Vec::with_capacity(addresses.len());
    for addr in addresses {
        let account = rpc
            .get_account(addr)
            .await
            .with_context(|| format!("lookup table {addr} not found"))?;
        let table = AddressLookupTable::deserialize(&account.data)
            .with_context(|| format!("lookup table {addr} deserialize"))?;
        out.push(AddressLookupTableAccount {
            key: *addr,
            addresses: table.addresses.to_vec(),
        });
    }
    Ok(out)
}

/// Redis subscribe loop with reconnect backoff. Each message is handled on
/// its own task, bounded by a semaphore — a stuck RPC call can never dam
/// the stream.
async fn run_redis_loop(app: Arc<App>) -> Result<()> {
    let semaphore = Arc::new(Semaphore::new(app.cfg.max_inflight));
    let mut backoff = Duration::from_millis(500);
    loop {
        match subscribe_and_consume(&app, &semaphore).await {
            Ok(()) => backoff = Duration::from_millis(500),
            Err(e) => {
                warn!(error = %e, "redis subscription dropped, reconnecting in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(15));
            }
        }
    }
}

async fn subscribe_and_consume(app: &Arc<App>, semaphore: &Arc<Semaphore>) -> Result<()> {
    let client = redis::Client::open(app.cfg.redis_url.as_str()).context("redis url")?;
    let mut pubsub = client.get_async_pubsub().await.context("redis connect")?;
    pubsub
        .subscribe(&app.cfg.redis_channel)
        .await
        .context("redis subscribe")?;
    info!(channel = %app.cfg.redis_channel, "subscribed to opportunity feed");

    let mut stream = pubsub.on_message();
    while let Some(msg) = stream.next().await {
        let payload: String = match msg.get_payload() {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "non-utf8 payload dropped");
                continue;
            }
        };
        let opp: Opportunity = match serde_json::from_str(&payload) {
            Ok(o) => o,
            Err(e) => {
                warn!(error = %e, "unparseable opportunity dropped");
                continue;
            }
        };

        let Ok(permit) = semaphore.clone().try_acquire_owned() else {
            debug!(id = %opp.id, "at max inflight, dropping opportunity");
            continue;
        };
        let app = app.clone();
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(e) = handle_opportunity(&app, opp).await {
                debug!(error = %e, "opportunity skipped");
            }
        });
    }
    bail!("redis message stream ended")
}

async fn handle_opportunity(app: &App, opp: Opportunity) -> Result<()> {
    // 1) Staleness: pool states move every slot; old quotes are poison.
    let age = opp.age_ms();
    if age > app.cfg.max_opportunity_age_ms {
        bail!("stale: {age}ms old (id={})", opp.id);
    }

    // 2) Only SOL-base cycles can be tipped without a price oracle.
    let base_mint = Pubkey::from_str(&opp.base_mint).context("bad base mint")?;
    if base_mint != WSOL_MINT {
        bail!(
            "non-WSOL base {} unsupported for tipping (id={})",
            opp.base_mint,
            opp.id
        );
    }

    // 3) Resubmit throttle per cycle id.
    {
        let mut recent = app.recent.lock().await;
        let now = Instant::now();
        recent.retain(|_, t| now.duration_since(*t) < Duration::from_secs(30));
        if let Some(last) = recent.get(&opp.id) {
            if now.duration_since(*last) < Duration::from_millis(app.cfg.resubmit_cooldown_ms) {
                bail!("cooldown (id={})", opp.id);
            }
        }
        recent.insert(opp.id.clone(), now);
    }

    // 4) Economics: tip scales off GROSS profit (monitor already estimated
    //    its own costs into netProfit; we recompute with the real tip).
    let tip = compute_tip(
        opp.gross_profit,
        app.cfg.min_tip_lamports,
        app.cfg.max_tip_lamports,
    );
    let fees = app.cfg.fee_lamports();
    let min_profit = tip
        .checked_add(fees)
        .and_then(|v| v.checked_add(app.cfg.profit_margin_lamports))
        .context("cost overflow")?;
    if opp.gross_profit <= min_profit {
        bail!(
            "unprofitable after costs: gross={} tip={tip} fees={fees} (id={})",
            opp.gross_profit,
            opp.id
        );
    }
    let projected_net = opp.gross_profit - min_profit;
    if projected_net < app.cfg.min_net_profit_lamports {
        bail!(
            "below MIN_NET_PROFIT_LAMPORTS: net={projected_net} < {} (id={})",
            app.cfg.min_net_profit_lamports,
            opp.id
        );
    }

    // 5) Resolve every hop into its full CPI account list.
    let mut hops = Vec::with_capacity(opp.hops.len());
    for hop in &opp.hops {
        hops.push(app.resolver.resolve_hop(hop).await?);
        ensure_ata(app, &Pubkey::from_str(&hop.input_mint)?).await?;
        ensure_ata(app, &Pubkey::from_str(&hop.output_mint)?).await?;
    }

    // 6) Build + sign the atomic transaction.
    let tip_account = random_tip_account();
    let tx = build_bundle_transaction(&BundleParams {
        program_id: app.cfg.arb_program_id,
        payer: &app.payer,
        base_token_account: derive_ata(&app.payer.pubkey(), &WSOL_MINT),
        hops: &hops,
        amount_in: opp.amount_in,
        min_profit,
        cu_limit: app.cfg.cu_limit,
        cu_price_microlamports: app.cfg.cu_price_microlamports,
        tip_account,
        tip_lamports: tip,
        blockhash: app.blockhash.get().await,
        lookup_tables: &app.lookup_tables,
    })?;

    // 7) Submission gate: real submits need DRY_RUN=false AND explicit
    //    ENABLE_SUBMIT=true AND ENABLE_JITO=true. Anything less simulates.
    let submit_armed = !app.cfg.dry_run && app.cfg.enable_submit && app.cfg.enable_jito;
    if !submit_armed {
        let sim = app.rpc.simulate_transaction(&tx).await?;
        info!(
            id = %opp.id,
            err = ?sim.value.err,
            cu = ?sim.value.units_consumed,
            tip,
            projected_net,
            dry_run = app.cfg.dry_run,
            enable_submit = app.cfg.enable_submit,
            enable_jito = app.cfg.enable_jito,
            "SIMULATION ONLY (submission disarmed)"
        );
        return Ok(());
    }

    // 8) Fire the bundle.
    let bundle_id = app.jito.send_bundle(std::slice::from_ref(&tx)).await?;
    info!(
        id = %opp.id,
        bundle = %bundle_id,
        gross = opp.gross_profit,
        tip,
        min_profit,
        age_ms = age,
        hops = opp.hops.len(),
        "bundle submitted"
    );

    // 9) Best-effort landing probe for the logs (off the hot path).
    tokio::time::sleep(Duration::from_secs(2)).await;
    match app.jito.bundle_status(&bundle_id).await {
        Ok(status) if !status.is_null() => info!(bundle = %bundle_id, %status, "bundle status"),
        Ok(_) => debug!(bundle = %bundle_id, "bundle not yet visible"),
        Err(e) => debug!(error = %e, "status probe failed"),
    }
    Ok(())
}

/// Guarantee the payer's ATA for `mint` exists (idempotent create, once per
/// mint per run). Swaps land into these accounts; a missing one reverts the
/// whole cycle.
async fn ensure_ata(app: &App, mint: &Pubkey) -> Result<()> {
    {
        let ready = app.atas_ready.lock().await;
        if ready.contains_key(mint) {
            return Ok(());
        }
    }
    let owner = app.payer.pubkey();
    let ata = derive_ata(&owner, mint);
    if app.rpc.get_account(&ata).await.is_err() {
        info!(%mint, %ata, "creating missing ATA");
        let ix = create_ata_idempotent_ix(&owner, &owner, mint);
        let blockhash = app.rpc.get_latest_blockhash().await?;
        let tx = Transaction::new_signed_with_payer(&[ix], Some(&owner), &[&app.payer], blockhash);
        app.rpc
            .send_and_confirm_transaction(&tx)
            .await
            .with_context(|| format!("create ATA for {mint}"))?;
    }
    app.atas_ready.lock().await.insert(*mint, ());
    Ok(())
}

/// AssociatedTokenAccount::CreateIdempotent (discriminant 1), built by hand
/// to avoid dragging in the full spl crates.
fn create_ata_idempotent_ix(payer: &Pubkey, owner: &Pubkey, mint: &Pubkey) -> Instruction {
    Instruction {
        program_id: ATA_PROGRAM,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(derive_ata(owner, mint), false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(system_program::id(), false),
            AccountMeta::new_readonly(TOKEN_PROGRAM, false),
        ],
        data: vec![1],
    }
}
