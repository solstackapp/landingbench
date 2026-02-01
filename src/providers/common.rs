use std::{collections::HashMap, sync::Arc};

use crossbeam_queue::ArrayQueue;
use tracing::{error, warn};

use crate::{
    backend::{SignatureEnvelope, SignatureObservation},
    utils::{Comparator, TransactionData},
};

#[derive(Default)]
pub struct TransactionAccumulator {
    entries: HashMap<String, TransactionData>,
}

impl TransactionAccumulator {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
        }
    }

    pub fn record(&mut self, signature: String, data: TransactionData) -> bool {
        use std::collections::hash_map::Entry;

        match self.entries.entry(signature) {
            Entry::Vacant(entry) => {
                entry.insert(data);
                true
            }
            Entry::Occupied(mut entry) => {
                if data.elapsed_since_start < entry.get().elapsed_since_start {
                    entry.insert(data);
                    true
                } else {
                    false
                }
            }
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn into_inner(self) -> HashMap<String, TransactionData> {
        self.entries
    }
}

pub fn fatal_connection_error(endpoint: &str, err: impl std::fmt::Display) -> ! {
    error!(endpoint = endpoint, error = %err, "Failed to connect to endpoint");
    eprintln!("Failed to connect to endpoint {}: {}", endpoint, err);
    std::process::exit(1);
}

pub fn build_signature_envelope(
    comparator: &Arc<Comparator>,
    endpoint: &str,
    signature: &str,
    data: TransactionData,
    total_producers: usize,
) -> Option<SignatureEnvelope> {
    comparator
        .record_observation(endpoint, signature, data, total_producers)
        .map(|observations| {
            let mut payload = observations
                .into_iter()
                .map(|(endpoint, tx_data)| SignatureObservation {
                    endpoint,
                    timestamp: tx_data.wallclock_secs,
                    backfilled: tx_data.wallclock_secs < tx_data.start_wallclock_secs,
                })
                .collect::<Vec<_>>();
            payload.sort_by(|lhs, rhs| lhs.endpoint.cmp(&rhs.endpoint));
            SignatureEnvelope {
                signature: signature.to_owned(),
                observations: payload,
            }
        })
}

pub fn enqueue_signature(
    sender: &Arc<ArrayQueue<SignatureEnvelope>>,
    endpoint: &str,
    signature: &str,
    envelope: SignatureEnvelope,
) {
    if sender.push(envelope).is_err() {
        warn!(endpoint = endpoint, signature = %signature, "Signature queue full; dropping observation");
    }
}
