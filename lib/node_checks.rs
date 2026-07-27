//! Startup fitness checks for the connected Bitcoin Core node, beyond the
//! version check in [`crate::version`].

use jsonrpsee::core::client::ClientT;
use miette::Diagnostic;
use serde::Deserialize;
use thiserror::Error;

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
}
