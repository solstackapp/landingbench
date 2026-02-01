use crate::proto::geyser::CommitmentLevel;
use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

#[derive(Debug, Deserialize, Serialize)]
pub struct ConfigToml {
    pub config: Config,
    pub grpc: GrpcEndpoint,
    pub endpoint: Vec<Endpoint>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Config {
    #[serde(default = "default_utility_rpc")]
    pub utility_rpc: String,
    pub transactions: usize,
    pub delay_ms: u64,
    pub commitment: ArgsCommitment,
    pub nonce_account: String,
    pub private_key: String,
    #[serde(default = "default_landing_grace_ms")]
    pub landing_grace_ms: u64,
    #[serde(default = "default_priority_fee")]
    pub prio_fee: f64,
    #[serde(default = "default_compute_unit_limit")]
    pub compute_unit_limit: u32,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct GrpcEndpoint {
    pub name: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x_token: Option<String>,
    pub kind: GrpcKind,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct Endpoint {
    pub name: String,
    pub url: String,
    pub kind: EndpointKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tip_lamports: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tip_wallet: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum EndpointKind {
    Rpc,
    Custom,
    Soyas,
}

#[derive(Debug, Deserialize, Serialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum GrpcKind {
    Yellowstone,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ArgsCommitment {
    #[default]
    Processed,
    Confirmed,
    Finalized,
}

impl From<ArgsCommitment> for CommitmentLevel {
    fn from(commitment: ArgsCommitment) -> Self {
        match commitment {
            ArgsCommitment::Processed => CommitmentLevel::Processed,
            ArgsCommitment::Confirmed => CommitmentLevel::Confirmed,
            ArgsCommitment::Finalized => CommitmentLevel::Finalized,
        }
    }
}

impl ArgsCommitment {
    pub fn as_str(&self) -> &'static str {
        match self {
            ArgsCommitment::Processed => "processed",
            ArgsCommitment::Confirmed => "confirmed",
            ArgsCommitment::Finalized => "finalized",
        }
    }
}

impl EndpointKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EndpointKind::Rpc => "rpc",
            EndpointKind::Custom => "custom",
            EndpointKind::Soyas => "soyas",
        }
    }
}

impl GrpcKind {
}

fn default_landing_grace_ms() -> u64 {
    10_000
}

fn default_priority_fee() -> f64 {
    0.0
}

fn default_compute_unit_limit() -> u32 {
    1_000_000
}

fn default_utility_rpc() -> String {
    "https://api.mainnet-beta.solana.com".to_string()
}

impl ConfigToml {
    pub fn load(path: &str) -> Result<Self> {
        let content =
            fs::read_to_string(path).with_context(|| format!("Failed to read config {}", path))?;
        let config = toml::from_str(&content).map_err(|err| anyhow!(err))?;
        Ok(config)
    }

    pub fn create_default(path: &str) -> Result<Self> {
        let default_config = ConfigToml {
            config: Config {
                utility_rpc: default_utility_rpc(),
                transactions: 100,
                delay_ms: 50,
                commitment: ArgsCommitment::Processed,
                nonce_account: "nonceaccountgoeshere".to_string(),
                private_key: "pkgoeshere".to_string(),
                landing_grace_ms: default_landing_grace_ms(),
                prio_fee: default_priority_fee(),
                compute_unit_limit: default_compute_unit_limit(),
            },
            grpc: GrpcEndpoint {
                name: "listener".to_string(),
                url: "http://fra.corvus-labs.io:10101".to_string(),
                x_token: None,
                kind: GrpcKind::Yellowstone,
            },
            endpoint: vec![
                Endpoint {
                    name: "rpc-a".to_string(),
                    url: "https://api.mainnet-beta.solana.com".to_string(),
                    kind: EndpointKind::Rpc,
                    tip_lamports: None,
                    tip_wallet: None,
                    api_key: None,
                },
                Endpoint {
                    name: "rpc-b".to_string(),
                    url: "https://ssc-dao.genesysgo.net".to_string(),
                    kind: EndpointKind::Rpc,
                    tip_lamports: None,
                    tip_wallet: None,
                    api_key: None,
                },
            ],
        };

        let toml_string = toml::to_string_pretty(&default_config)
            .context("Failed to serialize default config")?;
        fs::write(path, toml_string)
            .with_context(|| format!("Failed to write default config {}", path))?;

        Ok(default_config)
    }

    pub fn load_or_create(path: &str) -> Result<Self> {
        if Path::new(path).exists() {
            Self::load(path)
        } else {
            Self::create_default(path)
        }
    }
}
