use anyhow::{Result, anyhow};
use solana_client::{nonblocking::rpc_client::RpcClient, rpc_config::RpcSendTransactionConfig};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    signature::Signature,
    transaction::Transaction,
};

pub async fn send_transaction(
    client: &RpcClient,
    tx: &Transaction,
    commitment: CommitmentConfig,
) -> Result<Signature> {
    let cfg = RpcSendTransactionConfig {
        skip_preflight: true,
        preflight_commitment: Some(commitment.commitment),
        max_retries: Some(0),
        ..RpcSendTransactionConfig::default()
    };
    let signature = client
        .send_transaction_with_config(tx, cfg)
        .await
        .map_err(|err| anyhow!("RPC send failure: {err}"))?;
    Ok(signature)
}
