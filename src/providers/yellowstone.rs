use std::{error::Error, str::FromStr, time::Instant};

use futures_util::{sink::SinkExt, stream::StreamExt};
use solana_pubkey::Pubkey;
use tokio::{
    sync::{broadcast, oneshot},
    task,
};
use tonic::transport::ClientTlsConfig;
use tracing::{error, info, warn};

use crate::{
    config::{ArgsCommitment, GrpcEndpoint, GrpcKind},
};

use super::yellowstone_client::GeyserGrpcClient;
use crate::proto::geyser::{
    CommitmentLevel, SubscribeRequest, SubscribeRequestFilterTransactions, SubscribeRequestPing,
    subscribe_update::UpdateOneof,
};

#[derive(Debug, Clone)]
pub struct MemoObservation {
    pub signature: String,
    pub memo: String,
    pub elapsed_since_start: std::time::Duration,
}

pub fn spawn_memo_listener(
    endpoint: GrpcEndpoint,
    commitment: ArgsCommitment,
    observed_nonce_account: &str,
    start_instant: Instant,
    mut shutdown_rx: broadcast::Receiver<()>,
    memo_tx: tokio::sync::mpsc::Sender<MemoObservation>,
    ready_tx: oneshot::Sender<()>,
) -> Result<task::JoinHandle<()>, Box<dyn Error + Send + Sync>> {
    if endpoint.kind != GrpcKind::Yellowstone {
        return Err("Only yellowstone gRPC endpoints are supported for now".into());
    }

    let endpoint_name = endpoint.name.clone();
    let endpoint_url = endpoint.url.clone();
    let endpoint_token = endpoint
        .x_token
        .clone()
        .filter(|token| !token.trim().is_empty());

    let commitment: CommitmentLevel = commitment.into();
    let observed_nonce = observed_nonce_account.to_string();

    let join = task::spawn(async move {
        info!(endpoint = %endpoint_name, url = %endpoint_url, "Connecting to gRPC stream");
        let builder = match GeyserGrpcClient::build_from_shared(endpoint_url.clone()) {
            Ok(b) => b,
            Err(err) => {
                error!(endpoint = %endpoint_name, error = %err, "Failed to build gRPC client");
                return;
            }
        };
        let builder = if let Some(token) = endpoint_token {
            match builder.x_token(Some(token)) {
                Ok(b) => b,
                Err(err) => {
                    error!(endpoint = %endpoint_name, error = %err, "Failed to attach x-token");
                    return;
                }
            }
        } else {
            builder
        };
        let builder = if endpoint_url.starts_with("https") {
            match builder.tls_config(ClientTlsConfig::new().with_native_roots()) {
                Ok(b) => b,
                Err(err) => {
                    error!(endpoint = %endpoint_name, error = %err, "Failed to set TLS config");
                    return;
                }
            }
        } else {
            builder
        };

        let mut client = match builder.connect().await {
            Ok(c) => c,
            Err(err) => {
                error!(endpoint = %endpoint_name, error = %err, "Failed to connect to gRPC endpoint");
                return;
            }
        };

        let Ok((mut subscribe_tx, mut stream)) = client.subscribe().await else {
            error!(endpoint = %endpoint_name, "Failed to subscribe");
            return;
        };

        let mut transactions = std::collections::HashMap::new();
        transactions.insert(
            "nonce-filter".to_string(),
            SubscribeRequestFilterTransactions {
                account_include: vec![observed_nonce],
                account_exclude: vec![],
                account_required: vec![],
                ..Default::default()
            },
        );

        if let Err(err) = subscribe_tx
            .send(SubscribeRequest {
                slots: Default::default(),
                accounts: Default::default(),
                transactions,
                transactions_status: Default::default(),
                entry: Default::default(),
                blocks: Default::default(),
                blocks_meta: Default::default(),
                commitment: Some(commitment as i32),
                accounts_data_slice: Vec::default(),
                ping: None,
                from_slot: None,
            })
            .await
        {
            error!(endpoint = %endpoint_name, error = %err, "Failed to send subscribe request");
            return;
        }

        match stream.next().await {
            Some(Ok(msg)) => {
                if msg.update_oneof.is_some() {
                    info!(endpoint = %endpoint_name, "gRPC subscription confirmed");
                    let _ = ready_tx.send(());
                }
            }
            Some(Err(err)) => {
                error!(endpoint = %endpoint_name, error = %err, "Failed to confirm subscription");
                return;
            }
            None => {
                error!(endpoint = %endpoint_name, "Stream closed before subscription confirmed");
                return;
            }
        }

        let memo_program =
            Pubkey::from_str("Fa1CoNWTFZkQaJCuGRLshjJ3n9t6fYsARK3GjdAGjnXx").expect("valid memo id");

        loop {
            tokio::select! { biased;
                _ = shutdown_rx.recv() => {
                    info!(endpoint = %endpoint_name, "Stopping gRPC listener (shutdown)");
                    break;
                }
                message = stream.next() => {
                    match message {
                        Some(Ok(msg)) => {
                            if let Some(UpdateOneof::Transaction(tx_msg)) = msg.update_oneof {
                                if let Some((signature, memo)) = extract_memo(&tx_msg, memo_program) {
                                    let obs = MemoObservation {
                                        signature,
                                        memo,
                                        elapsed_since_start: start_instant.elapsed(),
                                    };
                                    if memo_tx.send(obs).await.is_err() {
                                        warn!(endpoint = %endpoint_name, "Receiver dropped; stopping listener");
                                        break;
                                    }
                                }
                            } else if let Some(UpdateOneof::Ping(_)) = msg.update_oneof {
                                if let Err(err) = subscribe_tx
                                    .send(SubscribeRequest {
                                        ping: Some(SubscribeRequestPing { id: 1 }),
                                        ..Default::default()
                                    })
                                    .await
                                {
                                    error!(endpoint = %endpoint_name, error = ?err, "Ping reply failed");
                                    break;
                                }
                            }
                        }
                        Some(Err(err)) => {
                            error!(endpoint = %endpoint_name, error = ?err, "Stream error");
                            break;
                        }
                        None => {
                            info!(endpoint = %endpoint_name, "Stream closed by server");
                            break;
                        }
                    }
                }
            }
        }
    });

    Ok(join)
}

fn extract_memo(
    tx_msg: &crate::proto::geyser::SubscribeUpdateTransaction,
    memo_program: Pubkey,
) -> Option<(String, String)> {
    let signature_bytes = tx_msg
        .transaction
        .as_ref()?
        .transaction
        .as_ref()?
        .signatures
        .first()
        .cloned()?;
    let signature = bs58::encode(&signature_bytes).into_string();
    let message = tx_msg
        .transaction
        .as_ref()?
        .transaction
        .as_ref()?
        .message
        .as_ref()?;

    let program_id =
        |idx: usize| -> Option<&[u8]> { message.account_keys.get(idx).map(|k| k.as_slice()) };

    for inst in &message.instructions {
        let Some(pid_bytes) = program_id(inst.program_id_index as usize) else {
            continue;
        };
        if pid_bytes == memo_program.as_ref() {
            if let Ok(memo) = String::from_utf8(inst.data.clone()) {
                return Some((signature, memo));
            }
        }
    }

    None
}
