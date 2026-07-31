//! Startup fitness checks for the connected Bitcoin Core node, beyond the
//! version check in [`crate::version`].

use std::{collections::HashMap, time::Duration};

use jsonrpsee::core::client::ClientT;
use miette::Diagnostic;
use serde::Deserialize;
use thiserror::Error;

use crate::validator::main_rest_client::{MainRestClient, MainRestClientError};

/// Minimal subset of `getblockchaininfo` needed to detect pruning. The
/// upstream `bitcoin_jsonrpsee::client::BlockchainInfo` type doesn't expose
/// this field, so we decode it ourselves.
#[derive(Debug, Deserialize)]
struct BlockchainInfoPruning {
    /// Whether pruning is enabled
    pruned: bool,
}

#[derive(Debug, Diagnostic, Error)]
#[error("Bitcoin Core node has pruning enabled")]
#[diagnostic(code(bip300301_enforcer::pruned_node))]
pub struct PrunedNode {
    #[help]
    pub help: String,
}

#[derive(Debug, Diagnostic, Error)]
pub enum PruneCheckError {
    #[error("failed to call `getblockchaininfo` on Bitcoin Core")]
    #[diagnostic(code(bip300301_enforcer::getblockchaininfo_failed))]
    Rpc(#[source] jsonrpsee::core::client::Error),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Pruned(#[from] PrunedNode),
}

/// Refuse to run against a node with pruning enabled.
///
/// The enforcer validates BIP300/BIP301 rules from full block data, syncing
/// from its last processed block (genesis, on a first run). A pruned node
/// discards exactly that data, so even a node that has not pruned anything
/// yet is only one enforcer downtime away from becoming unusable. Failing
/// up front with a clear message beats dying mid-sync on a raw
/// `Block not available (pruned data)` JSON-RPC error.
pub async fn check_node_not_pruned<C>(client: &C) -> Result<(), PruneCheckError>
where
    C: ClientT + Sync,
{
    let info: BlockchainInfoPruning = client
        .request("getblockchaininfo", jsonrpsee::rpc_params![])
        .await
        .map_err(PruneCheckError::Rpc)?;
    if !info.pruned {
        return Ok(());
    }
    Err(PrunedNode {
        help: "the enforcer needs to read every block from its last processed \
               block onwards (from genesis, on a first run), and a pruned node \
               cannot reliably serve historical blocks. Restart Bitcoin Core \
               without the `prune` option, and reindex (`-reindex`) if the node \
               has already pruned block data."
            .to_owned(),
    }
    .into())
}

#[derive(Debug, Diagnostic, Error)]
pub enum RestServerCheckError {
    #[error(transparent)]
    #[diagnostic(
        code(bip300301_enforcer::rest_server_not_enabled),
        help("start Bitcoin Core with `-rest=1`")
    )]
    NotEnabled(MainRestClientError),
    #[error("mainchain REST server still unavailable after {retries} retries")]
    #[diagnostic(code(bip300301_enforcer::rest_server_unavailable))]
    StillWarmingUp {
        retries: u32,
        #[source]
        source: MainRestClientError,
    },
    #[error("unable to check availability of mainchain REST server")]
    #[diagnostic(code(bip300301_enforcer::rest_server_check_failed))]
    Failed(#[source] MainRestClientError),
}

/// Check that the node's REST server (`-rest`) is enabled and reachable,
/// tolerating the 503s Bitcoin Core serves while warming up. The enforcer
/// batch-fetches block headers over REST during sync.
pub async fn check_rest_server_available(
    rest_client: &MainRestClient,
) -> Result<(), RestServerCheckError> {
    const RETRY_DELAY: Duration = Duration::from_millis(250);
    const MAX_RETRIES: u32 = 5;

    let started = tokio::time::Instant::now();
    let mut retries = 0;
    loop {
        match rest_client.get_chain_info().await {
            Ok(_) => {
                tracing::info!(
                    "verified mainchain REST server is enabled in {:?}",
                    started.elapsed()
                );
                return Ok(());
            }
            Err(err @ MainRestClientError::RestServerNotEnabled) => {
                return Err(RestServerCheckError::NotEnabled(err));
            }
            // Bitcoin Core responds 503 on the REST interface while warming
            // up. Tolerate this, the same way we tolerate RPC_IN_WARMUP on
            // the JSON-RPC interface.
            Err(MainRestClientError::Http(err))
                if err.status() == Some(reqwest::StatusCode::SERVICE_UNAVAILABLE) =>
            {
                if retries >= MAX_RETRIES {
                    return Err(RestServerCheckError::StillWarmingUp {
                        retries: MAX_RETRIES,
                        source: MainRestClientError::Http(err),
                    });
                }
                retries += 1;
                tracing::debug!(
                    err = %err,
                    "Mainchain REST server not ready yet, retrying ({retries}/{MAX_RETRIES})...",
                );
                tokio::time::sleep(RETRY_DELAY).await;
            }
            Err(err) => return Err(RestServerCheckError::Failed(err)),
        }
    }
}

/// Minimal subset of a `getzmqnotifications` entry.
#[derive(Debug, Deserialize)]
struct ZmqNotification {
    #[serde(rename = "type")]
    notification_type: String,
    address: String,
}

#[derive(Debug, Diagnostic, Error)]
pub enum ZmqSequenceCheckError {
    #[error("failed to call `getzmqnotifications` on Bitcoin Core")]
    #[diagnostic(code(bip300301_enforcer::getzmqnotifications_failed))]
    Rpc(#[source] jsonrpsee::core::client::Error),
    #[error("unable to find ZMQ notification for `pubsequence` in `getzmqnotifications` response")]
    #[diagnostic(
        help(
            "Your Bitcoin Core instance is not configured to send ZMQ notifications for the `pubsequence` notification type"
        ),
        code(bip300301_enforcer::zmq_pubsequence_notification_missing),
        url("https://github.com/layerTwo-Labs/bip300301_enforcer?tab=readme-ov-file#requirements")
    )]
    Missing,
}

/// Discover the node's `zmqpubsequence` endpoint via `getzmqnotifications`.
/// The enforcer follows the node's chain through the sequence notifications.
pub async fn zmq_sequence_address<C>(client: &C) -> Result<String, ZmqSequenceCheckError>
where
    C: ClientT + Sync,
{
    let notifications: Vec<ZmqNotification> = client
        .request("getzmqnotifications", jsonrpsee::rpc_params![])
        .await
        .map_err(ZmqSequenceCheckError::Rpc)?;
    notifications
        .into_iter()
        .find(|notification| notification.notification_type == "pubsequence")
        .map(|notification| notification.address)
        .ok_or(ZmqSequenceCheckError::Missing)
}

/// Minimal subset of a `getindexinfo` entry.
#[derive(Debug, Deserialize)]
struct IndexInfo {
    synced: bool,
    best_block_height: u32,
}

#[derive(Debug, Diagnostic, Error)]
pub enum TxindexCheckError {
    #[error("failed to call `getindexinfo` on Bitcoin Core")]
    #[diagnostic(code(bip300301_enforcer::getindexinfo_failed))]
    Rpc(#[source] jsonrpsee::core::client::Error),
    #[error("`txindex` is not enabled on the mainchain client")]
    #[diagnostic(
        code(bip300301_enforcer::txindex_not_enabled),
        help(
            "restart Bitcoin Core with `-txindex=1`; the index is built in the \
             background, no reindex is required"
        )
    )]
    NotEnabled,
}

/// Check that the node has `txindex` enabled, waiting for the index to
/// finish building if it is still syncing: RPC requests against a partial
/// index fail in obscure ways later.
pub async fn check_txindex<C>(client: &C) -> Result<(), TxindexCheckError>
where
    C: ClientT + Sync,
{
    const POLL_INTERVAL: Duration = Duration::from_secs(5);
    /// Waiting-for-txindex polls between info-level progress logs; the polls
    /// in between log at debug.
    const POLLS_PER_INFO_LOG: u32 = 6;

    let mut polls: u32 = 0;
    loop {
        let indexes: HashMap<String, IndexInfo> = client
            .request("getindexinfo", jsonrpsee::rpc_params![])
            .await
            .map_err(TxindexCheckError::Rpc)?;
        let Some(txindex) = indexes.get("txindex") else {
            return Err(TxindexCheckError::NotEnabled);
        };
        if txindex.synced {
            return Ok(());
        }
        let wait_msg = format!(
            "waiting for the node's txindex to finish building (indexed up to \
             height {})",
            txindex.best_block_height
        );
        if polls.is_multiple_of(POLLS_PER_INFO_LOG) {
            tracing::info!("{wait_msg}");
        } else {
            tracing::debug!("{wait_msg}");
        }
        polls += 1;
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_pruned_info() {
        let unpruned: BlockchainInfoPruning =
            serde_json::from_str(r#"{"chain":"regtest","blocks":10,"pruned":false}"#).unwrap();
        assert!(!unpruned.pruned);

        let pruned: BlockchainInfoPruning =
            serde_json::from_str(r#"{"pruned":true,"pruneheight":230}"#).unwrap();
        assert!(pruned.pruned);
    }

    #[test]
    fn deserialize_zmq_notifications() {
        let notifications: Vec<ZmqNotification> = serde_json::from_str(
            r#"[
                {"type":"pubhashblock","address":"tcp://127.0.0.1:28332","hwm":1000},
                {"type":"pubsequence","address":"tcp://127.0.0.1:28333","hwm":1000}
            ]"#,
        )
        .unwrap();
        let sequence = notifications
            .into_iter()
            .find(|notification| notification.notification_type == "pubsequence")
            .unwrap();
        assert_eq!(sequence.address, "tcp://127.0.0.1:28333");
    }

    #[test]
    fn deserialize_index_info() {
        let indexes: HashMap<String, IndexInfo> = serde_json::from_str(
            r#"{"txindex":{"synced":false,"best_block_height":230},
                "basic block filter index":{"synced":true,"best_block_height":300}}"#,
        )
        .unwrap();
        let txindex = &indexes["txindex"];
        assert!(!txindex.synced);
        assert_eq!(txindex.best_block_height, 230);

        let no_txindex: HashMap<String, IndexInfo> = serde_json::from_str("{}").unwrap();
        assert!(!no_txindex.contains_key("txindex"));
    }
}
