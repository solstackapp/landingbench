use anyhow::Context as _;
use arc_swap::ArcSwap;
use quinn::{
    ClientConfig, Connection, Endpoint, IdleTimeout, TransportConfig,
    crypto::rustls::QuicClientConfig,
};
use solana_sdk::{signature::Keypair, transaction::Transaction};
use solana_tls_utils::{SkipServerVerification, new_dummy_x509_certificate};
use std::{
    net::{SocketAddr, ToSocketAddrs as _},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;
use tracing::warn;

const ALPN_TPU_PROTOCOL_ID: &[u8] = b"solana-tpu";
const SOYAS_SERVER: &str = "soyas-landing";
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(25);
const MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

pub struct SoyasClient {
    endpoint: Endpoint,
    client_config: ClientConfig,
    addr: SocketAddr,
    connection: ArcSwap<Connection>,
    reconnect: Mutex<()>,
}

impl SoyasClient {
    pub async fn connect(endpoint_addr: &str, api_key: &str) -> anyhow::Result<Self> {
        let keypair = Keypair::from_base58_string(api_key);
        let (cert, key) = new_dummy_x509_certificate(&keypair);
        let mut crypto = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(SkipServerVerification::new())
            .with_client_auth_cert(vec![cert], key)
            .context("failed to configure client certificate")?;

        crypto.alpn_protocols = vec![ALPN_TPU_PROTOCOL_ID.to_vec()];

        let client_crypto = QuicClientConfig::try_from(crypto)
            .context("failed to convert rustls config into quinn crypto config")?;
        let mut client_config = ClientConfig::new(Arc::new(client_crypto));
        let mut transport = TransportConfig::default();
        transport.keep_alive_interval(Some(KEEP_ALIVE_INTERVAL));
        transport.max_idle_timeout(Some(IdleTimeout::try_from(MAX_IDLE_TIMEOUT)?));
        client_config.transport_config(Arc::new(transport));

        let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?)?;
        endpoint.set_default_client_config(client_config.clone());
        let addr = endpoint_addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| anyhow::anyhow!("Address not resolved"))?;
        let connection = endpoint.connect(addr, SOYAS_SERVER)?.await?;

        Ok(Self {
            endpoint,
            client_config,
            addr,
            connection: ArcSwap::from_pointee(connection),
            reconnect: Mutex::new(()),
        })
    }

    pub async fn send_transaction(&self, transaction: &Transaction) -> anyhow::Result<()> {
        let serialized_tx = bincode::serialize(transaction)?;
        let connection = self.connection.load_full();
        if Self::try_send_bytes(&connection, &serialized_tx)
            .await
            .is_ok()
        {
            return Ok(());
        }
        warn!("Failed to send payload to Soyas QUIC; reconnecting");
        self.reconnect().await?;
        let connection = self.connection.load_full();
        Self::try_send_bytes(&connection, &serialized_tx).await
    }

    async fn reconnect(&self) -> anyhow::Result<()> {
        let _guard = self.reconnect.try_lock()?;
        let connection = self
            .endpoint
            .connect_with(self.client_config.clone(), self.addr, SOYAS_SERVER)?
            .await?;
        self.connection.store(Arc::new(connection));
        Ok(())
    }

    async fn try_send_bytes(connection: &Connection, payload: &[u8]) -> anyhow::Result<()> {
        let mut stream = connection.open_uni().await?;
        stream.write_all(payload).await?;
        stream.finish()?;
        Ok(())
    }
}
