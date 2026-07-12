//! Executor application core — the `App` (RPC, payer, resolver, Jito client,
//! blockhash cache, lookup tables) plus the per-opportunity handler and the
//! two drive loops:
//!
//! - [`run_redis_loop`]  — standalone executor: consume the Redis channel.
//! - [`run_channel_loop`] — fused `arb-bot`: consume an in-process mpsc
//!   channel fed directly by the monitor pipeline (no Redis on the hot path).
//!
//! Both share [`App::handle_opportunity`], so the trading logic (staleness /
//! economics gates, resolve, build, simulate/submit) is identical regardless
//! of how opportunities arrive.

use anyhow::{bail, Context, Result};
use arb_common::opportunity::Opportunity;
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
use tokio::sync::{mpsc, Mutex, Semaphore};
use tracing::{debug, info, warn};

use crate::blockhash::BlockhashCache;
use crate::builder::{build_bundle_transaction, BundleParams};
use crate::config::Config;
use crate::jito::{random_tip_account, JitoClient};
use crate::resolver::{derive_ata, Resolver, ATA_PROGRAM, TOKEN_PROGRAM, WSOL_MINT};

pub struct App {
    pub cfg: Config,
    pub rpc: Arc<RpcClient>,
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

impl App {
    /// Wire up the executor from config: keypair, RPC, background blockhash
    /// cache, resolver, Jito client and any address lookup tables.
    pub async fn from_config(cfg: Config) -> Result<Arc<Self>> {
        let payer = read_keypair_file(&cfg.keypair_path)
            .map_err(|e| anyhow::anyhow!("read keypair {}: {e}", cfg.keypair_path))?;
        info!(
            payer = %payer.pubkey(),
            program = %cfg.arb_program_id,
            mode = %cfg.mode,
            dry_run = cfg.dry_run,
            enable_submit = cfg.enable_submit,
            enable_jito = cfg.enable_jito,
            "executor ready"
        );
        if cfg.mode.allows_live_submission() && !cfg.dry_run && cfg.enable_submit && cfg.enable_jito
        {
            warn!("SUBMISSION ARMED — MODE=live, live bundles will be sent to Jito");
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
        let resolver = Resolver::new(
            rpc.clone(),
            payer.pubkey(),
            Duration::from_secs(cfg.whirlpool_ttl_secs),
        );
        let jito = JitoClient::new(cfg.jito_url.clone())?;

        Ok(Arc::new(Self {
            resolver,
            jito,
            blockhash,
            lookup_tables,
            recent: Mutex::new(HashMap::new()),
            atas_ready: Mutex::new(HashMap::new()),
            rpc,
            payer,
            cfg,
        }))
    }

    /// Full per-opportunity pipeline: gates -> resolve -> build -> simulate or
    /// submit. Errors are non-fatal (logged by the caller as a skip).
    pub async fn handle_opportunity(&self, opp: Opportunity) -> Result<()> {
        // 1) Staleness: pool states move every slot; old quotes are poison.
        let age = opp.age_ms();
        if age > self.cfg.max_opportunity_age_ms {
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
            let mut recent = self.recent.lock().await;
            let now = Instant::now();
            recent.retain(|_, t| now.duration_since(*t) < Duration::from_secs(30));
            if let Some(last) = recent.get(&opp.id) {
                if now.duration_since(*last) < Duration::from_millis(self.cfg.resubmit_cooldown_ms)
                {
                    bail!("cooldown (id={})", opp.id);
                }
            }
            recent.insert(opp.id.clone(), now);
        }

        // 4) Economics via the SHARED cost model (identical to the monitor's).
        //    Tip scales off GROSS profit; `min_profit` is the on-chain floor
        //    (fixed costs + tip + margin) the program enforces or reverts.
        let cost = self.cfg.cost_model();
        let gross = opp.gross_profit;
        let tip = cost.payment(gross);
        let fees = cost.fixed_costs();
        let min_profit = cost
            .total_burn(gross)
            .checked_add(cost.margin_lamports)
            .context("cost overflow")?;
        if gross <= min_profit {
            bail!(
                "unprofitable after costs: gross={gross} tip={tip} fees={fees} (id={})",
                opp.id
            );
        }
        let projected_net = gross - min_profit;
        if projected_net < self.cfg.min_net_profit_lamports {
            bail!(
                "below MIN_NET_PROFIT_LAMPORTS: net={projected_net} < {} (id={})",
                self.cfg.min_net_profit_lamports,
                opp.id
            );
        }

        // 5) Resolve every hop into its full CPI account list.
        let mut hops = Vec::with_capacity(opp.hops.len());
        for hop in &opp.hops {
            hops.push(self.resolver.resolve_hop(hop).await?);
            self.ensure_ata(&Pubkey::from_str(&hop.input_mint)?).await?;
            self.ensure_ata(&Pubkey::from_str(&hop.output_mint)?)
                .await?;
        }

        // 6) Build + sign the atomic transaction.
        let tip_account = random_tip_account();
        let tx = build_bundle_transaction(&BundleParams {
            program_id: self.cfg.arb_program_id,
            payer: &self.payer,
            base_token_account: derive_ata(&self.payer.pubkey(), &WSOL_MINT),
            hops: &hops,
            amount_in: opp.amount_in,
            min_profit,
            cu_limit: self.cfg.cu_limit,
            cu_price_microlamports: self.cfg.cu_price_microlamports,
            tip_account,
            tip_lamports: tip,
            blockhash: self.blockhash.get().await,
            lookup_tables: &self.lookup_tables,
        })?;

        // 7) Submission gate: real submits require MODE=live (armed) AND
        //    DRY_RUN=false AND ENABLE_SUBMIT=true AND ENABLE_JITO=true. In any
        //    other mode (observe/replay/simulate) we simulate and never send.
        let submit_armed = self.cfg.mode.allows_live_submission()
            && !self.cfg.dry_run
            && self.cfg.enable_submit
            && self.cfg.enable_jito;
        if !submit_armed {
            let sim = self.rpc.simulate_transaction(&tx).await?;
            info!(
                id = %opp.id,
                err = ?sim.value.err,
                cu = ?sim.value.units_consumed,
                tip,
                projected_net,
                dry_run = self.cfg.dry_run,
                enable_submit = self.cfg.enable_submit,
                enable_jito = self.cfg.enable_jito,
                "SIMULATION ONLY (submission disarmed)"
            );
            return Ok(());
        }

        // 8) Fire the bundle.
        let bundle_id = self.jito.send_bundle(std::slice::from_ref(&tx)).await?;
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
        match self.jito.bundle_status(&bundle_id).await {
            Ok(status) if !status.is_null() => info!(bundle = %bundle_id, %status, "bundle status"),
            Ok(_) => debug!(bundle = %bundle_id, "bundle not yet visible"),
            Err(e) => debug!(error = %e, "status probe failed"),
        }
        Ok(())
    }

    /// Guarantee the payer's ATA for `mint` exists (idempotent, once per mint
    /// per run). Swaps land into these accounts; a missing one reverts.
    async fn ensure_ata(&self, mint: &Pubkey) -> Result<()> {
        if self.atas_ready.lock().await.contains_key(mint) {
            return Ok(());
        }
        let owner = self.payer.pubkey();
        let ata = derive_ata(&owner, mint);
        if self.rpc.get_account(&ata).await.is_err() {
            info!(%mint, %ata, "creating missing ATA");
            let ix = create_ata_idempotent_ix(&owner, &owner, mint);
            let blockhash = self.rpc.get_latest_blockhash().await?;
            let tx =
                Transaction::new_signed_with_payer(&[ix], Some(&owner), &[&self.payer], blockhash);
            self.rpc
                .send_and_confirm_transaction(&tx)
                .await
                .with_context(|| format!("create ATA for {mint}"))?;
        }
        self.atas_ready.lock().await.insert(*mint, ());
        Ok(())
    }
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

/// Spawn a bounded worker for one opportunity. Returns false if the inflight
/// cap is saturated (opportunity dropped — it would be stale by the time a
/// slot frees anyway).
fn spawn_handler(app: &Arc<App>, semaphore: &Arc<Semaphore>, opp: Opportunity) -> bool {
    let Ok(permit) = semaphore.clone().try_acquire_owned() else {
        debug!(id = %opp.id, "at max inflight, dropping opportunity");
        return false;
    };
    let app = app.clone();
    tokio::spawn(async move {
        let _permit = permit;
        if let Err(e) = app.handle_opportunity(opp).await {
            debug!(error = %e, "opportunity skipped");
        }
    });
    true
}

/// Standalone executor: subscribe to the Redis opportunity channel (auto
/// reconnect with backoff) and dispatch each message to a bounded worker.
pub async fn run_redis_loop(app: Arc<App>) -> Result<()> {
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
        spawn_handler(app, semaphore, opp);
    }
    bail!("redis message stream ended")
}

/// Fused binary: consume opportunities from the in-process channel fed by the
/// monitor pipeline. Same bounded-worker dispatch as the Redis loop.
pub async fn run_channel_loop(app: Arc<App>, mut rx: mpsc::Receiver<Opportunity>) -> Result<()> {
    let semaphore = Arc::new(Semaphore::new(app.cfg.max_inflight));
    info!("executor consuming in-process opportunity channel");
    while let Some(opp) = rx.recv().await {
        spawn_handler(&app, &semaphore, opp);
    }
    bail!("opportunity channel closed")
}
