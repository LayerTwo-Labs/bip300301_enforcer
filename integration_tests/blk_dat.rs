//! Shared helpers for e2e checks against Bitcoin Core on-disk `blk*.dat`.
//!
//! Used by BIP 300 (drivechain) and BIP 360 dual-node trials that assert a
//! mined block is stored unaltered on a peer's block files after tip acceptance.

use std::{path::Path, time::Duration};

use bip300301_enforcer_lib::{
    bins::CommandExt as _, validator::parse_block_files::BlockDirectoryParser,
};
use bitcoin::{
    Block, BlockHash, Network,
    consensus::encode::{deserialize_hex, serialize},
};
use tokio::time::sleep;

use crate::setup::PostSetup;

/// `datadir/regtest/blocks` (or mainnet/signet equivalent) for a harness node.
#[must_use]
pub fn blocks_dir(post_setup: &PostSetup) -> std::path::PathBuf {
    let network_dir = match post_setup.network {
        crate::setup::Network::Regtest => "regtest",
        crate::setup::Network::Signet => "signet",
    };
    post_setup
        .directories
        .bitcoin_dir
        .join(network_dir)
        .join("blocks")
}

/// Load `block_hash` from a Core blocks directory (`blk*.dat` + `xor.dat`).
pub fn load_block_from_blocks_dir(
    blocks_dir: &Path,
    block_hash: BlockHash,
    network: Network,
) -> anyhow::Result<Block> {
    let parser = BlockDirectoryParser::new(blocks_dir.to_path_buf(), network)
        .map_err(|e| anyhow::anyhow!("open blocks dir {}: {e}", blocks_dir.display()))?;

    for entry in parser {
        let parsed = entry.map_err(|e| anyhow::anyhow!("parse blk*.dat: {e}"))?;
        if parsed.header.block_hash() != block_hash {
            continue;
        }
        let txdata = parsed
            .parse_tx_data()
            .map_err(|e| anyhow::anyhow!("decode txdata for {block_hash}: {e}"))?;
        return Ok(Block {
            header: parsed.header,
            txdata,
        });
    }

    anyhow::bail!(
        "block {block_hash} not found in blocks dir {}",
        blocks_dir.display()
    )
}

/// Poll until `blocks_dir` contains `block_hash` (disk may lag RPC briefly).
pub async fn wait_for_block_on_disk(
    blocks_dir: &Path,
    block_hash: BlockHash,
    network: Network,
) -> anyhow::Result<Block> {
    const POLL: Duration = Duration::from_millis(200);
    const TIMEOUT: Duration = Duration::from_secs(45);
    let deadline = std::time::Instant::now() + TIMEOUT;
    loop {
        match load_block_from_blocks_dir(blocks_dir, block_hash, network) {
            Ok(block) => return Ok(block),
            Err(err) => {
                anyhow::ensure!(
                    std::time::Instant::now() < deadline,
                    "blk*.dat never contained {block_hash} within {TIMEOUT:?} under {}: {err:#}",
                    blocks_dir.display()
                );
                sleep(POLL).await;
            }
        }
    }
}

/// Assert bitcoind active tip equals `block_hash`.
pub async fn ensure_tip(
    node: &PostSetup,
    block_hash: BlockHash,
    label: &str,
) -> anyhow::Result<()> {
    let best = node
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getbestblockhash", [])
        .run_utf8()
        .await?;
    anyhow::ensure!(
        best.trim().eq_ignore_ascii_case(&block_hash.to_string()),
        "{label} tip {} != expected block {block_hash}",
        best.trim()
    );
    Ok(())
}

/// Full block via RPC `getblock <hash> 0` (hex → `Block`).
pub async fn rpc_get_block(node: &PostSetup, block_hash: BlockHash) -> anyhow::Result<Block> {
    let hex = node
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "getblock", [block_hash.to_string(), "0".to_string()])
        .run_utf8()
        .await?;
    Ok(deserialize_hex(hex.trim())?)
}

/// Poll until bitcoind height ≥ `min_height`.
pub async fn wait_for_height(node: &PostSetup, min_height: u32) -> anyhow::Result<()> {
    const POLL: Duration = Duration::from_millis(250);
    const TIMEOUT: Duration = Duration::from_secs(90);
    let deadline = std::time::Instant::now() + TIMEOUT;
    loop {
        let height: u32 = node
            .bitcoin_cli
            .command::<String, _, String, _, _>([], "getblockcount", [])
            .run_utf8()
            .await?
            .trim()
            .parse()?;
        if height >= min_height {
            return Ok(());
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "bitcoind did not reach height {min_height} within {TIMEOUT:?} (stuck at {height})"
        );
        sleep(POLL).await;
    }
}

/// Require two blocks consensus-serialize identically (header + all txs/witnesses).
pub fn assert_blocks_equal(
    label_a: &str,
    a: &Block,
    label_b: &str,
    b: &Block,
) -> anyhow::Result<()> {
    let ser_a = serialize(a);
    let ser_b = serialize(b);
    if ser_a == ser_b {
        return Ok(());
    }
    anyhow::bail!(
        "block mismatch: {label_a} vs {label_b}\n\
         {label_a}_hash={} {label_b}_hash={}\n\
         {label_a}_len={} {label_b}_len={}\n\
         {label_a}_txs={} {label_b}_txs={}",
        a.block_hash(),
        b.block_hash(),
        ser_a.len(),
        ser_b.len(),
        a.txdata.len(),
        b.txdata.len(),
    )
}
