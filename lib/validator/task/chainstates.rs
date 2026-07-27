//! Querying the node's chainstates (`getchainstates`) to determine which
//! blocks lack block data during an assumeutxo background sync.

use jsonrpsee::core::client::ClientT;
use serde::Deserialize;
use thiserror::Error;

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

/// An assumeutxo snapshot (`loadtxoutset`) that the node is still validating
/// in the background.
#[derive(Clone, Copy, Debug)]
pub(super) struct BackgroundSync {
    /// Hash of the snapshot base block.
    pub snapshot_blockhash: bitcoin::BlockHash,
    /// Tip height of the background chainstate. Block data is available at
    /// and below this height.
    pub background_height: u32,
}

#[derive(Debug, Error)]
#[error(
    "`getchainstates` reports an unvalidated snapshot chainstate, but no background chainstate"
)]
pub(in crate::validator) struct MissingBackgroundChainstate;

impl Chainstates {
    /// The assumeutxo background sync in progress, or `None` if every block
    /// on the active chain has block data.
    pub(super) fn background_sync(
        &self,
    ) -> Result<Option<BackgroundSync>, MissingBackgroundChainstate> {
        let Some(snapshot_blockhash) = self.chainstates.iter().find_map(|chainstate| {
            chainstate
                .snapshot_blockhash
                .filter(|_| chainstate.validated != Some(true))
        }) else {
            return Ok(None);
        };
        let background_height = self
            .chainstates
            .iter()
            .filter(|chainstate| chainstate.snapshot_blockhash.is_none())
            .map(|chainstate| {
                // A background chainstate that hasn't connected anything
                // yet reports an empty object
                chainstate.blocks.unwrap_or(0)
            })
            .max()
            .ok_or(MissingBackgroundChainstate)?;
        Ok(Some(BackgroundSync {
            snapshot_blockhash,
            background_height,
        }))
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
    //! Only the response shapes Bitcoin Core does not produce, so the
    //! integration tests (`test_node_requirements`) cannot cover them.

    use super::*;

    const SNAPSHOT_BLOCKHASH: &str =
        "3bb7ce5eba0be48939b7a521ac1ba9316afee2c7bada3a0cca24188e6d7d96c0";

    #[test]
    fn empty_background_chainstate_has_nothing_but_genesis() {
        // A chainstate without a tip is reported as an empty object.
        let fresh: Chainstates = serde_json::from_str(&format!(
            r#"{{"headers":299,"chainstates":[
                {{}},
                {{"blocks":299,
                  "snapshot_blockhash":"{SNAPSHOT_BLOCKHASH}",
                  "validated":false}}
            ]}}"#
        ))
        .unwrap();
        assert_eq!(
            fresh
                .background_sync()
                .unwrap()
                .map(|sync| sync.background_height),
            Some(0)
        );
    }

    #[test]
    fn unvalidated_snapshot_without_background_chainstate_is_an_error() {
        // Must surface as an error rather than be read as "nothing but
        // genesis is available", which `sync_blocks` would wait on forever.
        let malformed: Chainstates = serde_json::from_str(&format!(
            r#"{{"headers":299,"chainstates":[
                {{"blocks":299,
                  "snapshot_blockhash":"{SNAPSHOT_BLOCKHASH}",
                  "validated":false}}
            ]}}"#
        ))
        .unwrap();
        assert!(malformed.background_sync().is_err());
    }
}
