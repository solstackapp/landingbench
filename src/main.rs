use anyhow::{Context, Result, anyhow};
use blake3;
use bs58;
use config::{ArgsCommitment, ConfigToml, EndpointKind, GrpcKind};
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_nonce::state::State;
use solana_pubkey::Pubkey;
use solana_rpc_client_nonce_utils::nonblocking::{
    data_from_state, state_from_account,
};
use solana_sdk::{
    commitment_config::{CommitmentConfig, CommitmentLevel},
    hash::Hash,
    instruction::Instruction,
    message::Message,
    signature::{Keypair, Signature, Signer},
    system_instruction,
    transaction::Transaction,
};
use std::{
    collections::HashMap,
    collections::HashSet,
    convert::TryFrom,
    env,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    signal::ctrl_c,
    sync::{Mutex, broadcast, mpsc, oneshot},
    time::{Duration as TokioDuration, sleep},
};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use url::Url;

mod config;
mod proto;
mod providers;
mod senders;
mod utils;

const DEFAULT_CONFIG_PATH: &str = "config.toml";
static RANDOM_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct SendRecord {
    provider: String,
    sent_elapsed: Duration,
    round_id: String,
}

#[derive(Debug, Default)]
struct ProviderMetrics {
    landed: usize,
    latencies_ms: Vec<f64>,
}

#[derive(Default)]
struct LandingBook {
    records: Mutex<LandingBookInner>,
}

#[derive(Default)]
struct LandingBookInner {
    sent: HashMap<String, SendRecord>,
    metrics: HashMap<String, ProviderMetrics>,
    rounds_attempted: usize,
    landed_rounds: HashSet<String>,
    waiters: HashMap<String, oneshot::Sender<()>>,
    round_notifiers: HashMap<String, tokio::sync::watch::Sender<bool>>,
}

impl LandingBook {
    async fn record_send(
        &self,
        signature: String,
        provider: String,
        sent_elapsed: Duration,
        round_id: String,
    ) -> (oneshot::Receiver<()>, tokio::sync::watch::Receiver<bool>) {
        let (tx, rx) = oneshot::channel();
        let mut guard = self.records.lock().await;
        let sig_key = signature.clone();
        let round_tx = guard
            .round_notifiers
            .entry(round_id.clone())
            .or_insert_with(|| {
                let (tx, _rx) = tokio::sync::watch::channel(false);
                tx
            })
            .clone();
        guard.sent.insert(
            sig_key.clone(),
            SendRecord {
                provider: provider.clone(),
                sent_elapsed,
                round_id,
            },
        );
        guard.metrics.entry(provider).or_default();
        guard.waiters.insert(sig_key, tx);
        (rx, round_tx.subscribe())
    }

    async fn start_round(&self, round_id: String) {
        let mut guard = self.records.lock().await;
        guard.rounds_attempted += 1;
        guard.round_notifiers.entry(round_id).or_insert_with(|| {
            let (tx, _rx) = tokio::sync::watch::channel(false);
            tx
        });
    }

    async fn record_landing(&self, obs: providers::yellowstone::MemoObservation) -> bool {
        let mut guard = self.records.lock().await;
        let Some(send) = guard.sent.remove(&obs.signature) else {
            return false;
        };

        let observed_provider = obs
            .memo
            .split_once('|')
            .map(|(provider, _)| provider)
            .unwrap_or(obs.memo.as_str());
        if observed_provider != send.provider {
            warn!(
                signature = %obs.signature,
                observed_memo = %obs.memo,
                expected_provider = %send.provider,
                "Memo provider mismatch; skipping"
            );
            return false;
        }

        let latency_ms = obs
            .elapsed_since_start
            .saturating_sub(send.sent_elapsed)
            .as_secs_f64()
            * 1_000.0;

        let entry = guard.metrics.entry(send.provider.clone()).or_default();
        entry.landed += 1;
        entry.latencies_ms.push(latency_ms);
        if guard.landed_rounds.insert(send.round_id.clone()) {
            // Only count a landed round once even if multiple endpoints report.
        }
        if let Some(waiter) = guard.waiters.remove(&obs.signature) {
            let _ = waiter.send(());
        }
        if let Some(round_tx) = guard.round_notifiers.remove(&send.round_id) {
            let _ = round_tx.send(true);
        }
        info!(
            provider = %send.provider,
            signature = %obs.signature,
            latency_ms = latency_ms,
            "Transaction landed"
        );
        true
    }

    async fn finalize(&self) -> LandingSummary {
        let guard = self.records.lock().await;
        let mut providers = Vec::new();
        let mut overall_latencies = Vec::new();
        for (name, metrics) in guard.metrics.iter() {
            let summary = ProviderSummary::from_metrics(name.clone(), metrics);
            overall_latencies.extend_from_slice(&summary.latencies_ms);
            providers.push(summary);
        }
        providers.sort_by(|a, b| a.name.cmp(&b.name));

        LandingSummary {
            providers,
            total_landed: guard.landed_rounds.len(),
            overall: compute_overall(&overall_latencies),
            rounds: guard.rounds_attempted,
        }
    }
}

#[derive(Debug, Clone)]
struct ProviderSummary {
    name: String,
    landed: usize,
    latencies_ms: Vec<f64>,
}

impl ProviderSummary {
    fn from_metrics(name: String, metrics: &ProviderMetrics) -> Self {
        let mut latencies = metrics.latencies_ms.clone();
        latencies.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        Self {
            name,
            landed: metrics.landed,
            latencies_ms: latencies,
        }
    }
}

#[derive(Debug, Clone)]
struct LandingSummary {
    providers: Vec<ProviderSummary>,
    total_landed: usize,
    overall: OverallLatency,
    rounds: usize,
}

#[derive(Debug, Clone, Default)]
struct OverallLatency {
    min_ms: Option<f64>,
    max_ms: Option<f64>,
    avg_ms: Option<f64>,
    p50_ms: Option<f64>,
    p90_ms: Option<f64>,
    p95_ms: Option<f64>,
    p99_ms: Option<f64>,
}

fn compute_overall(latencies: &[f64]) -> OverallLatency {
    if latencies.is_empty() {
        return OverallLatency::default();
    }
    let mut sorted = latencies.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let sum: f64 = sorted.iter().copied().sum();
    OverallLatency {
        min_ms: sorted.first().copied(),
        max_ms: sorted.last().copied(),
        avg_ms: Some(sum / sorted.len() as f64),
        p50_ms: Some(utils::percentile(&sorted, 0.50)),
        p90_ms: Some(utils::percentile(&sorted, 0.90)),
        p95_ms: Some(utils::percentile(&sorted, 0.95)),
        p99_ms: Some(utils::percentile(&sorted, 0.99)),
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct CliArgs {
    config_path: Option<String>,
}

impl CliArgs {
    fn parse() -> Self {
        let mut args = env::args().skip(1);
        let mut parsed = CliArgs { config_path: None };

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--config" => {
                    let value = args.next().unwrap_or_else(|| {
                        eprintln!("Missing value for --config");
                        print_usage();
                        std::process::exit(1);
                    });
                    parsed.config_path = Some(value);
                }
                "--help" | "-h" => {
                    print_usage();
                    std::process::exit(0);
                }
                other => {
                    eprintln!("Unknown argument: {}", other);
                    print_usage();
                    std::process::exit(1);
                }
            }
        }

        parsed
    }
}

fn print_usage() {
    eprintln!("Usage: landingbench [--config <PATH>]");
}

#[tokio::main]
async fn main() -> Result<()> {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .compact()
        .try_init()
        .map_err(|err| anyhow!(err))?;

    let cli = CliArgs::parse();
    let config_path = cli.config_path.as_deref().unwrap_or(DEFAULT_CONFIG_PATH);
    let config = ConfigToml::load_or_create(config_path)?;
    validate_config(&config)?;

    info!(config_path, "Loaded configuration");

    let keypair = Arc::new(decode_private_key(&config.config.private_key)?);
    let nonce_account: Pubkey = config
        .config
        .nonce_account
        .parse()
        .context("Invalid nonce account pubkey")?;
    let start_instant = Instant::now();

    let commitment_config = commitment_from_args(config.config.commitment);
    let utility_client = Arc::new(RpcClient::new(config.config.utility_rpc.clone()));
    let (shutdown_tx, mut shutdown_rx) = broadcast::channel::<()>(1);
    let shutdown_flag = Arc::new(AtomicBool::new(false));
    let (memo_tx, mut memo_rx) = mpsc::channel::<providers::yellowstone::MemoObservation>(1024);
    let (grpc_ready_tx, grpc_ready_rx) = oneshot::channel::<()>();

    let listener = providers::yellowstone::spawn_memo_listener(
        config.grpc.clone(),
        config.config.commitment,
        &config.config.nonce_account,
        start_instant,
        shutdown_tx.subscribe(),
        memo_tx,
        grpc_ready_tx,
    )
    .map_err(|err| anyhow!("{}", err))?;

    grpc_ready_rx
        .await
        .context("gRPC listener failed to become ready")?;
    info!("gRPC listener ready, starting benchmark");

    let book = Arc::new(LandingBook::default());
    let endpoints = config.endpoint.clone();
    let clients: Vec<Arc<RpcClient>> = endpoints
        .iter()
        .map(|ep| Arc::new(RpcClient::new(ep.url.clone())))
        .collect();

    // Initialize Soyas QUIC clients for any Soyas endpoints
    let mut soyas_clients: HashMap<String, Arc<senders::SoyasClient>> = HashMap::new();
    for endpoint in &endpoints {
        if endpoint.kind == EndpointKind::Soyas {
            let api_key = endpoint
                .api_key
                .as_ref()
                .ok_or_else(|| anyhow!("Soyas endpoint {} requires api_key", endpoint.name))?;
            let client = senders::SoyasClient::connect(&endpoint.url, api_key)
                .await
                .with_context(|| {
                    format!("Failed to connect to Soyas endpoint {}", endpoint.name)
                })?;
            info!(endpoint = %endpoint.name, url = %endpoint.url, "Connected to Soyas QUIC endpoint");
            soyas_clients.insert(endpoint.name.clone(), Arc::new(client));
        }
    }
    let soyas_clients = Arc::new(soyas_clients);

    let round_id = format!("{}", (utils::get_current_timestamp() * 1000.0) as i64);
    let mut previous_nonce: Option<Hash> = None;

    tokio::spawn({
        let shutdown_tx = shutdown_tx.clone();
        let shutdown_flag = Arc::clone(&shutdown_flag);
        async move {
            if ctrl_c().await.is_ok() {
                shutdown_flag.store(true, Ordering::SeqCst);
                let _ = shutdown_tx.send(());
            }
        }
    });

    // Observation processing in background
    let book_for_obs = Arc::clone(&book);
    let memo_task = tokio::spawn(async move {
        while let Some(obs) = memo_rx.recv().await {
            book_for_obs.record_landing(obs).await;
        }
    });

    for _round in 0..config.config.transactions {
        if shutdown_rx.try_recv().is_ok() {
            shutdown_flag.store(true, Ordering::SeqCst);
            break;
        }

        let nonce_hash = fetch_fresh_nonce_hash(
            utility_client.as_ref(),
            &nonce_account,
            previous_nonce,
            commitment_config.clone(),
            config.config.landing_grace_ms,
        )
        .await?;
        previous_nonce = Some(nonce_hash);
        let round_id = bs58::encode(nonce_hash).into_string();
        book.start_round(round_id.clone()).await;
        let max_in_flight = endpoints.len().max(1);
        let _send_results: Vec<_> = stream::iter(endpoints.iter().enumerate())
            .map(|(idx, endpoint)| {
                let client = clients
                    .get(idx)
                    .cloned()
                    .unwrap_or_else(|| Arc::new(RpcClient::new(endpoint.url.clone())));
                let soyas_client = soyas_clients.get(&endpoint.name).cloned();
                let endpoint = endpoint.clone();
                let keypair = Arc::clone(&keypair);
                let book = Arc::clone(&book);
                let shutdown_rx = shutdown_tx.subscribe();
                let commitment = commitment_config.clone();
                let round_id = round_id.clone();
                async move {
                    send_single_tx(
                        endpoint,
                        client,
                        soyas_client,
                        keypair,
                        nonce_account,
                        nonce_hash,
                        config.config.prio_fee,
                        config.config.compute_unit_limit,
                        config.config.landing_grace_ms,
                        commitment,
                        start_instant,
                        shutdown_rx,
                        book,
                        round_id,
                    )
                    .await
                }
            })
            .buffer_unordered(max_in_flight)
            .collect()
            .await;
        sleep(Duration::from_millis(config.config.delay_ms)).await;
    }

    // Allow extra time for landings after sends complete unless shutdown was requested
    if !shutdown_flag.load(Ordering::SeqCst) {
        sleep(Duration::from_millis(config.config.landing_grace_ms)).await;
    }
    let _ = shutdown_tx.send(());
    let mut listener = listener;
    tokio::select! {
        _ = &mut listener => {}
        _ = sleep(TokioDuration::from_secs(2)) => {
            warn!("gRPC listener shutdown timed out; aborting");
            listener.abort();
            let _ = listener.await;
        }
    }
    let _ = memo_task.await;

    let summary = book.finalize().await;
    render_summary(&summary, &config, &round_id);

    Ok(())
}

fn commitment_from_args(commitment: ArgsCommitment) -> CommitmentConfig {
    let level = match commitment {
        ArgsCommitment::Processed => CommitmentLevel::Processed,
        ArgsCommitment::Confirmed => CommitmentLevel::Confirmed,
        ArgsCommitment::Finalized => CommitmentLevel::Finalized,
    };
    CommitmentConfig { commitment: level }
}

async fn send_single_tx(
    endpoint: config::Endpoint,
    rpc_client: Arc<RpcClient>,
    soyas_client: Option<Arc<senders::SoyasClient>>,
    keypair: Arc<Keypair>,
    nonce_account: Pubkey,
    nonce_hash: Hash,
    prio_fee: f64,
    cu_limit: u32,
    landing_wait_ms: u64,
    commitment: CommitmentConfig,
    start_instant: Instant,
    mut shutdown_rx: broadcast::Receiver<()>,
    book: Arc<LandingBook>,
    round_id: String,
) -> Result<()> {
    if shutdown_rx.try_recv().is_ok() {
        return Ok(());
    }

    let authority = keypair.pubkey();
    let instructions = build_instructions(
        &endpoint,
        &nonce_account,
        &authority,
        &endpoint.name,
        prio_fee,
        cu_limit,
    )?;
    let message = Message::new(&instructions, Some(&authority));
    let tx = Transaction::new(&[keypair.as_ref()], message, nonce_hash);

    let send_elapsed = start_instant.elapsed();
    let send_result: Result<Signature> = match endpoint.kind {
        EndpointKind::Soyas => {
            let client = soyas_client
                .as_ref()
                .ok_or_else(|| anyhow!("Soyas client not initialized for {}", endpoint.name))?;
            client
                .send_transaction(&tx)
                .await
                .map(|_| tx.signatures[0])
                .map_err(|e| anyhow!("Soyas send failure: {e}"))
        }
        EndpointKind::Rpc => senders::rpc::send_transaction(&rpc_client, &tx, commitment).await,
        EndpointKind::Custom => {
            senders::custom::send_transaction(&rpc_client, &tx, commitment).await
        }
    };

    match send_result {
        Ok(signature) => {
            info!(endpoint = %endpoint.name, signature = %signature, "Sent transaction");
            let (ack, mut round_rx) = book
                .record_send(
                    signature.to_string(),
                    endpoint.name.clone(),
                    send_elapsed,
                    round_id,
                )
                .await;
            let waited = tokio::time::timeout(Duration::from_millis(landing_wait_ms), async {
                tokio::select! {
                    _ = ack => Ok(WaitOutcome::SelfLanded),
                    res = round_rx.changed() => res.map(|_| WaitOutcome::OtherLanded),
                }
            })
            .await;

            match waited {
                Ok(Ok(WaitOutcome::SelfLanded)) => {}
                Ok(Ok(WaitOutcome::OtherLanded)) => {
                    info!(endpoint = %endpoint.name, "Round landed by another endpoint; skipping wait");
                }
                Ok(Err(_)) => {
                    warn!(endpoint = %endpoint.name, "Landing waiter dropped before completion");
                }
                Err(_) => {
                    warn!(endpoint = %endpoint.name, "Timed out waiting for landing; continuing");
                }
            }
        }
        Err(err) => {
            warn!(endpoint = %endpoint.name, error = ?err, "Failed to send transaction");
        }
    }

    Ok(())
}

enum WaitOutcome {
    SelfLanded,
    OtherLanded,
}

fn build_instructions(
    endpoint: &config::Endpoint,
    nonce_account: &Pubkey,
    authority: &Pubkey,
    memo_text: &str,
    prio_fee: f64,
    cu_limit: u32,
) -> Result<Vec<Instruction>> {
    let mut ix = Vec::new();
    ix.push(system_instruction::advance_nonce_account(
        nonce_account,
        authority,
    ));
    if prio_fee > 0.0 {
        let micro_fee = (prio_fee * 1_000_000.0)
            .max(0.0)
            .min(u64::MAX as f64)
            .round() as u64;
        ix.push(
            solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(micro_fee),
        );
        ix.push(
            solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(cu_limit),
        );
    }
    let memo_program = Pubkey::from_str("Fa1CoNWTFZkQaJCuGRLshjJ3n9t6fYsARK3GjdAGjnXx")
        .map_err(|err| anyhow!("Invalid memo program id: {err}"))?;
    ix.push(Instruction {
        program_id: memo_program,
        accounts: vec![],
        data: random_ix_payload(memo_text),
    });

    if endpoint.kind == config::EndpointKind::Custom || endpoint.kind == config::EndpointKind::Soyas
    {
        if let Some(lamports) = endpoint.tip_lamports {
            if lamports > 0 {
                let wallet = endpoint.tip_wallet.as_ref().ok_or_else(|| {
                    anyhow!(
                        "{:?} endpoint {} missing tip_wallet",
                        endpoint.kind,
                        endpoint.name
                    )
                })?;
                let wallet_pk: Pubkey = wallet
                    .parse()
                    .map_err(|err| anyhow!("Invalid tip_wallet for {}: {err}", endpoint.name))?;
                ix.push(system_instruction::transfer(
                    authority, &wallet_pk, lamports,
                ));
            }
        }
    }
    Ok(ix)
}

fn random_ix_payload(provider: &str) -> Vec<u8> {
    let count = RANDOM_COUNTER.fetch_add(1, Ordering::Relaxed);
    let now = utils::get_current_timestamp();
    let seed = format!("{provider}|{now:.6}|{count}");
    let hash = blake3::hash(seed.as_bytes());
    let random = bs58::encode(hash.as_bytes()).into_string();
    format!("{provider}|{random}").into_bytes()
}

async fn fetch_fresh_nonce_hash(
    client: &RpcClient,
    nonce_account: &Pubkey,
    previous: Option<Hash>,
    commitment: CommitmentConfig,
    wait_ms: u64,
) -> Result<Hash> {
    let deadline = Instant::now() + Duration::from_millis(wait_ms.max(100));
    loop {
        let account = client
            .get_account_with_commitment(nonce_account, commitment)
            .await
            .context("Failed to fetch nonce account with commitment")?
            .value
            .ok_or_else(|| anyhow!("Nonce account not found"))?;
        let state: State = state_from_account(&account).context("Failed to parse nonce state")?;
        let data = data_from_state(&state).context("Nonce account uninitialized")?;
        let current = data.blockhash();

        if previous.map(|p| p != current).unwrap_or(true) {
            return Ok(current);
        }

        if Instant::now() >= deadline {
            return Err(anyhow!(
                "Nonce hash did not advance before timeout; hash still {}",
                bs58::encode(current).into_string()
            ));
        }

        sleep(TokioDuration::from_millis(100)).await;
    }
}

fn decode_private_key(key: &str) -> Result<Keypair> {
    let bytes = bs58::decode(key.trim())
        .into_vec()
        .context("Failed to decode private key")?;
    Keypair::try_from(bytes.as_slice()).context("Invalid private key bytes")
}

fn validate_config(config: &ConfigToml) -> Result<()> {
    if config.endpoint.is_empty() {
        return Err(anyhow!("At least one RPC endpoint is required"));
    }
    if config.grpc.kind != GrpcKind::Yellowstone {
        return Err(anyhow!(
            "Only a single Yellowstone gRPC listener is supported for now"
        ));
    }
    for endpoint in &config.endpoint {
        match endpoint.kind {
            EndpointKind::Rpc => {}
            EndpointKind::Custom => {
                if endpoint.tip_lamports.unwrap_or(0) == 0 {
                    return Err(anyhow!(
                        "Custom endpoint {} must set tip_lamports > 0",
                        endpoint.name
                    ));
                }
                if endpoint.tip_wallet.is_none() {
                    return Err(anyhow!(
                        "Custom endpoint {} must set tip_wallet",
                        endpoint.name
                    ));
                }
            }
            EndpointKind::Soyas => {
                if endpoint.api_key.is_none() {
                    return Err(anyhow!("Soyas endpoint {} must set api_key", endpoint.name));
                }
                if endpoint.tip_lamports.unwrap_or(0) == 0 {
                    return Err(anyhow!(
                        "Soyas endpoint {} must set tip_lamports > 0",
                        endpoint.name
                    ));
                }
                if endpoint.tip_wallet.is_none() {
                    return Err(anyhow!(
                        "Soyas endpoint {} must set tip_wallet",
                        endpoint.name
                    ));
                }
            }
        }
    }
    Ok(())
}

fn render_summary(summary: &LandingSummary, config: &ConfigToml, test_id: &str) {
    let rpc_list = config
        .endpoint
        .iter()
        .map(|ep| {
            let mut base = format!(
                "{} ({}) [{}]",
                ep.name,
                display_endpoint_host(&ep.url),
                ep.kind.as_str()
            );
            if ep.kind == EndpointKind::Custom {
                if let (Some(tip), Some(wallet)) = (ep.tip_lamports, ep.tip_wallet.as_ref()) {
                    base.push_str(&format!(" tip={} to {}", tip, wallet));
                }
            }
            base
        })
        .collect::<Vec<_>>()
        .join(", ");
    let prio = config.config.prio_fee;
    let overall = &summary.overall;

    println!("\nFinished Test ID      : {}", test_id);
    println!(
        "Utility RPC Host      : {}",
        display_endpoint_host(&config.config.utility_rpc)
    );
    println!(
        "gRPC Endpoint         : {}",
        display_endpoint_host(&config.grpc.url)
    );
    println!("RPC Send Endpoints    : {}", rpc_list);
    println!("Nonce Account         : {}", config.config.nonce_account);
    println!("Transactions Count    : {}", summary.rounds);
    println!("Send Interval         : {}ms", config.config.delay_ms);
    println!(
        "Compute Unit Limit    : {}",
        config.config.compute_unit_limit
    );
    println!(
        "Priority Fee/CU       : {:.6} Lamports",
        prio
    );
    let landed_pct = if summary.rounds > 0 {
        (summary.total_landed as f64 / summary.rounds as f64) * 100.0
    } else {
        0.0
    };
    println!(
        "Transactions Landed   : {}/{} ({:.1}%)",
        summary.total_landed, summary.rounds, landed_pct
    );
    println!("Min Tx Landing Time   : {}", format_opt_ms(overall.min_ms));
    println!("Max Tx Landing Time   : {}", format_opt_ms(overall.max_ms));
    println!("Avg Tx Landing Time   : {}", format_opt_ms(overall.avg_ms));
    println!("Median Tx Landing Time: {}", format_opt_ms(overall.p50_ms));
    println!("P90 Tx Landing Time   : {}", format_opt_ms(overall.p90_ms));
    println!("P95 Tx Landing Time   : {}", format_opt_ms(overall.p95_ms));
    println!("P99 Tx Landing Time   : {}", format_opt_ms(overall.p99_ms));

    println!("\nEndpoint Landing Results:");
    for provider in &summary.providers {
        let pct = if summary.total_landed > 0 {
            (provider.landed as f64 / summary.total_landed as f64) * 100.0
        } else {
            0.0
        };
        println!(
            "  {} : {} wins ({:.1}%) [{}]",
            provider.name,
            provider.landed,
            pct,
            lookup_url(config, &provider.name)
        );
    }
}

fn lookup_url(config: &ConfigToml, name: &str) -> String {
    config
        .endpoint
        .iter()
        .find(|ep| ep.name == name)
        .map(|ep| display_endpoint_host(&ep.url))
        .unwrap_or_else(|| "-".to_string())
}

fn format_opt_ms(value: Option<f64>) -> String {
    value
        .map(|v| format!("{:.0} ms", v))
        .unwrap_or_else(|| "-".to_string())
}

fn display_endpoint_host(raw_url: &str) -> String {
    if let Ok(parsed) = Url::parse(raw_url) {
        let host = parsed.host_str().unwrap_or("-");
        if let Some(port) = parsed.port() {
            return format!("{host}:{port}");
        }
        return host.to_string();
    }

    let mut trimmed = raw_url;
    if let Some(pos) = trimmed.find('?') {
        trimmed = &trimmed[..pos];
    }
    if let Some(pos) = trimmed.find('#') {
        trimmed = &trimmed[..pos];
    }

    let host_part = if let Some(pos) = trimmed.find("://") {
        &trimmed[(pos + 3)..]
    } else {
        trimmed
    };

    let host_part = host_part.rsplit('@').next().unwrap_or(host_part);
    let host = host_part.split('/').next().unwrap_or(host_part);
    if host.is_empty() {
        "-".to_string()
    } else {
        host.to_string()
    }
}
