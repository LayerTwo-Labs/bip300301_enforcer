use std::{str::FromStr as _, time::Duration};

use bip300301_enforcer_lib::{bins::CommandExt as _, proto::mainchain::GetChainTipRequest};
use bitcoin::BlockHash;
use tokio::time::sleep;

use crate::setup::PostSetup;

/// Expected outcome of submitting a block to the enforcer.
pub enum Expect {
    /// The enforcer accepts the block
    Accepted,
    /// The enforcer rejects the block, `invalidateblock`s it, and logs a
    /// rejection reason containing this substring.
    Rejected { log_contains: &'static str },
}

/// The enforcer's validated chain tip `(hash, height)`. Surfaces the gRPC error
/// (e.g. `Unavailable` while the validator is not yet synced) so poll-loop
/// callers can retry rather than abort.
async fn enforcer_tip(post_setup: &mut PostSetup) -> anyhow::Result<(BlockHash, u32)> {
    let info = post_setup
        .validator_service_client
        .get_chain_tip(GetChainTipRequest {})
        .await?
        .into_inner()
        .block_header_info
        .ok_or_else(|| anyhow::anyhow!("get_chain_tip: missing block_header_info"))?;

    let hash = info
        .block_hash
        .and_then(|h| h.hex)
        .ok_or_else(|| anyhow::anyhow!("get_chain_tip: missing block_hash"))?;
    Ok((BlockHash::from_str(&hash)?, info.height))
}

/// `Some(height)` if `block_hash` is on bitcoind's active chain (`getblock`
/// reports `confirmations >= 0`), or `None` if it is known but off the active
/// chain — e.g. after the enforcer `invalidateblock`d it (`confirmations == -1`).
async fn active_chain_height(
    post_setup: &PostSetup,
    block_hash: BlockHash,
) -> anyhow::Result<Option<u32>> {
    let json = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "getblock", [block_hash.to_string()])
        .run_utf8()
        .await?;
    let block: serde_json::Value = serde_json::from_str(&json)?;
    let confirmations = block
        .get("confirmations")
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(|| anyhow::anyhow!("getblock: missing confirmations"))?;
    if confirmations < 0 {
        return Ok(None);
    }
    let height = block
        .get("height")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("getblock: missing height"))?;
    Ok(Some(height as u32))
}

/// `getchaintips` status for `block_hash`, or `None` if the block is not (yet)
/// present in any chain tip.
async fn chaintip_status(
    post_setup: &PostSetup,
    block_hash: BlockHash,
) -> anyhow::Result<Option<String>> {
    let chaintips_json = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getchaintips", [])
        .run_utf8()
        .await?;

    let chaintips: Vec<serde_json::Value> = serde_json::from_str(&chaintips_json)?;
    let target = block_hash.to_string();
    Ok(chaintips
        .iter()
        .find(|tip| tip.get("hash").and_then(|h| h.as_str()) == Some(&target))
        .and_then(|tip| tip.get("status"))
        .and_then(|s| s.as_str())
        .map(str::to_owned))
}

/// Poll until the enforcer reaches a terminal verdict on `block_hash`, then
/// assert it matches `expect`.
pub async fn assert_enforcer_verdict(
    post_setup: &mut PostSetup,
    block_hash: BlockHash,
    expect: Expect,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        // The validator may transiently report `Unavailable` while syncing;
        // retry until the deadline rather than aborting the whole assertion.
        let (tip_hash, tip_height) = match enforcer_tip(post_setup).await {
            Ok(tip) => tip,
            Err(err) => {
                anyhow::ensure!(
                    tokio::time::Instant::now() < deadline,
                    "timed out waiting for enforcer chain tip for block `{block_hash}`: {err:#}"
                );
                sleep(Duration::from_millis(200)).await;
                continue;
            }
        };

        // Accepted: the block is still on the active chain and the enforcer has
        // validated past its height. Checking chain membership (not tip equality)
        // keeps this correct if later blocks are mined on top.
        if let Some(block_height) = active_chain_height(post_setup, block_hash).await?
            && tip_height >= block_height
        {
            match &expect {
                Expect::Accepted => return Ok(()),
                Expect::Rejected { .. } => {
                    anyhow::bail!("expected enforcer to reject block `{block_hash}`")
                }
            }
        }

        // Rejected: the enforcer `invalidateblock`s, so getchaintips shows it invalid.
        let status = chaintip_status(post_setup, block_hash).await?;
        if status.as_deref() == Some("invalid") {
            match &expect {
                Expect::Rejected { log_contains } => {
                    let stdout = std::fs::read_to_string(
                        post_setup.directories.enforcer_dir.join("stdout.txt"),
                    )?;
                    anyhow::ensure!(
                        stdout.contains(*log_contains),
                        "enforcer rejected block `{block_hash}` but its log did not contain \
                         `{log_contains}`",
                    );
                    return Ok(());
                }
                Expect::Accepted => {
                    anyhow::bail!("expected enforcer to accept block `{block_hash}`")
                }
            }
        }

        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for enforcer verdict on block `{block_hash}`: enforcer tip is \
             `{tip_hash}` (height {tip_height}), its getchaintips status is {status:?}",
        );
        sleep(Duration::from_millis(200)).await;
    }
}
