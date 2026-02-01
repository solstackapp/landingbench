use anyhow::Result;
use solana_client::nonblocking::rpc_client::RpcClient;
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
    super::rpc::send_transaction(client, tx, commitment).await
}
