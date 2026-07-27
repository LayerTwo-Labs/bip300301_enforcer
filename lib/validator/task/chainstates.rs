//! Querying the node's chainstates (`getchainstates`) to determine how far
//! block data is available during an assumeutxo background sync.

use jsonrpsee::core::client::ClientT;
use serde::Deserialize;

/// One chainstate entry from `getchainstates`. All fields are optional:
/// Bitcoin Core returns an empty object for a chainstate without a tip.
#[derive(Debug, Deserialize)]
pub(super) struct ChainstateInfo {
    /// Height of the chainstate's tip.
    #[serde(default)]
    blocks: Option<u32>,
    /// Set iff this chainstate was activated from an assumeutxo snapshot
    /// (`loadtxoutset`).
    #[serde(default)]
    snapshot_blockhash: Option<bitcoin::BlockHash>,
    /// Whether every block in this chainstate has been fully validated.
    #[serde(default)]
    validated: Option<bool>,
}

/// Response of the `getchainstates` RPC. Normally a single chainstate. Two
/// while the node is validating an assumeutxo snapshot in the background.
#[derive(Debug, Deserialize)]
pub(super) struct Chainstates {
    chainstates: Vec<ChainstateInfo>,
}

impl Chainstates {
    /// The highest block height the node guarantees full block data for,
    /// or `None` if every block on the active chain is available.
    pub(super) fn block_data_available_height(&self) -> Option<u32> {
        let unvalidated_snapshot = self.chainstates.iter().any(|chainstate| {
            chainstate.snapshot_blockhash.is_some() && chainstate.validated != Some(true)
        });
        if !unvalidated_snapshot {
            return None;
        }
        let background_height = self
            .chainstates
            .iter()
            .filter(|chainstate| chainstate.snapshot_blockhash.is_none())
            .filter_map(|chainstate| chainstate.blocks)
            .max()
            // A background chainstate that hasn't connected anything yet
            // (or reports an empty object) has served nothing but genesis.
            .unwrap_or(0);
        Some(background_height)
    }
}

/// Query `getchainstates`. Available on all supported Bitcoin Core versions.
pub(super) async fn get_chainstates<C>(
    client: &C,
) -> Result<Chainstates, jsonrpsee::core::client::Error>
where
    C: ClientT + Sync,
{
    client
        .request("getchainstates", jsonrpsee::rpc_params![])
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_block_data_available_on_a_normal_node() {
        let normal: Chainstates = serde_json::from_str(
            r#"{"headers":300,"chainstates":[{"blocks":300,"bestblockhash":"aa","validated":true}]}"#,
        )
        .unwrap();
        assert_eq!(normal.block_data_available_height(), None);
    }

    #[test]
    fn background_sync_limits_available_block_data() {
        let syncing: Chainstates = serde_json::from_str(
            r#"{"headers":299,"chainstates":[
                {"blocks":150,"validated":true},
                {"blocks":299,
                 "snapshot_blockhash":
                    "3bb7ce5eba0be48939b7a521ac1ba9316afee2c7bada3a0cca24188e6d7d96c0",
                 "validated":false}
            ]}"#,
        )
        .unwrap();
        assert_eq!(syncing.block_data_available_height(), Some(150));

        // Right after `loadtxoutset` the background chainstate can be an
        // empty object (no tip yet): nothing but genesis is available.
        let fresh: Chainstates = serde_json::from_str(
            r#"{"headers":299,"chainstates":[
                {},
                {"blocks":299,
                 "snapshot_blockhash":
                    "3bb7ce5eba0be48939b7a521ac1ba9316afee2c7bada3a0cca24188e6d7d96c0",
                 "validated":false}
            ]}"#,
        )
        .unwrap();
        assert_eq!(fresh.block_data_available_height(), Some(0));
    }

    #[test]
    fn validated_snapshot_chainstate_has_all_block_data() {
        // Once background validation completes, the remaining chainstate may
        // still carry its snapshot_blockhash, but is fully validated.
        let merged: Chainstates = serde_json::from_str(
            r#"{"headers":300,"chainstates":[
                {"blocks":300,
                 "snapshot_blockhash":
                    "3bb7ce5eba0be48939b7a521ac1ba9316afee2c7bada3a0cca24188e6d7d96c0",
                 "validated":true}
            ]}"#,
        )
        .unwrap();
        assert_eq!(merged.block_data_available_height(), None);
    }
}
