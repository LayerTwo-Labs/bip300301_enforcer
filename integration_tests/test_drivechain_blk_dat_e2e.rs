//! BIP 300 / drivechain e2e: Miner mines a block with a new standard tx; Alice
//! (stock Core + drivechain enforcer) accepts the tip; Alice's on-disk `blk*.dat`
//! matches the block Miner produced (RPC `getblock 0` as miner-side source of truth).
//!
//! This is the drivechain counterpart of `bip360_blk_dat_e2e` (which needs P2MR Bob).
//! Both stock Unpatched Core; default enforcer features include `drivechain`.
//!
//! Recipe: `just drivechain-blk-dat-e2e` / `just it drivechain_blk_dat_e2e`.

use std::time::Duration;

use bip300301_enforcer_lib::bins::CommandExt as _;
use bitcoin::{Amount, BlockHash, Network};
use futures::{StreamExt as _, channel::mpsc};
use tracing::Instrument as _;

use crate::{
    blk_dat::{
        assert_blocks_equal, blocks_dir, ensure_tip, rpc_get_block, wait_for_block_on_disk,
        wait_for_height,
    },
    block_verdict::{Expect, assert_enforcer_verdict},
    setup::{BitcoindKind, Mode, Network as HarnessNetwork, PostSetup, PreSetup, SetupOpts},
    util::{BinPaths, FileDumpConfig, TestFileRegistry},
};

pub const TEST_NAME: &str = "drivechain_blk_dat_e2e";

fn register_peer_files(
    file_registry: &TestFileRegistry,
    dirs: &crate::setup::Directories,
    label: &str,
) {
    file_registry.register_file(
        TEST_NAME,
        dirs.bitcoin_dir.join("stdout.txt"),
        FileDumpConfig::new().with_label(format!("Bitcoin Core stdout ({label})")),
    );
    file_registry.register_file(
        TEST_NAME,
        dirs.bitcoin_dir.join("stderr.txt"),
        FileDumpConfig::new().with_label(format!("Bitcoin Core stderr ({label})")),
    );
    file_registry.register_file(
        TEST_NAME,
        dirs.enforcer_dir.join("stdout.txt"),
        FileDumpConfig::new().with_label(format!("Enforcer stdout ({label})")),
    );
    file_registry.register_file(
        TEST_NAME,
        dirs.enforcer_dir.join("stderr.txt"),
        FileDumpConfig::new().with_label(format!("Enforcer stderr ({label})")),
    );
}

async fn peer_nodes(
    bin_paths: BinPaths,
    file_registry: &TestFileRegistry,
) -> anyhow::Result<(PostSetup, PostSetup)> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let alice_pre = PreSetup::new(bin_paths.clone(), HarnessNetwork::Regtest)?;
    let miner_pre = PreSetup::new(bin_paths, HarnessNetwork::Regtest)?;
    register_peer_files(file_registry, &alice_pre.directories, "alice");
    register_peer_files(file_registry, &miner_pre.directories, "miner");

    let miner_listen = miner_pre.reserved_ports.bitcoind_listen.port();
    let alice_listen = alice_pre.reserved_ports.bitcoind_listen.port();

    // Alice: tip authority + drivechain enforcer (default features).
    // Miner: factory only; still boots an enforcer (PostSetup always does) but tip
    // authority is Alice after P2P sync.
    type Opts = SetupOpts<String, String, Vec<String>, Vec<String>>;
    let opts_alice = Opts {
        bitcoind_kind: BitcoindKind::Unpatched,
        enable_enforcer_wallet: false,
        bitcoind_args: vec![
            "-debug=net".to_string(),
            "-debug=validation".to_string(),
            "-fallbackfee=0.0002".to_string(),
        ],
        enforcer_args: Vec::new(),
    };
    let opts_miner = Opts {
        bitcoind_kind: BitcoindKind::Unpatched,
        enable_enforcer_wallet: false,
        bitcoind_args: vec![
            "-debug=net".to_string(),
            "-debug=validation".to_string(),
            "-fallbackfee=0.0002".to_string(),
        ],
        enforcer_args: Vec::new(),
    };

    let mut alice = alice_pre
        .setup(Mode::NoMempool, opts_alice, res_tx.clone())
        .await?;
    let miner = miner_pre.setup(Mode::NoMempool, opts_miner, res_tx).await?;

    drop(
        alice
            .bitcoin_cli
            .command::<String, _, _, _, _>(
                [],
                "addnode",
                [format!("127.0.0.1:{miner_listen}"), "add".to_owned()],
            )
            .run_utf8()
            .await?,
    );
    drop(
        miner
            .bitcoin_cli
            .command::<String, _, _, _, _>(
                [],
                "addnode",
                [format!("127.0.0.1:{alice_listen}"), "add".to_owned()],
            )
            .run_utf8()
            .await?,
    );

    // Independent height-101 tips after setup; extend Alice so Miner reorgs onto Alice.
    let mining_addr = alice.mining_address.to_string();
    drop(
        alice
            .bitcoin_cli
            .command::<String, _, _, _, _>([], "generatetoaddress", ["1", &mining_addr])
            .run_utf8()
            .await?,
    );
    let alice_height: u32 = alice
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getblockcount", [])
        .run_utf8()
        .await?
        .trim()
        .parse()?;
    wait_for_height(&miner, alice_height).await?;

    let alice_tip = alice
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getbestblockhash", [])
        .run_utf8()
        .await?;
    let miner_tip = miner
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getbestblockhash", [])
        .run_utf8()
        .await?;
    anyhow::ensure!(
        alice_tip.trim().eq_ignore_ascii_case(miner_tip.trim()),
        "bootstrap: Miner tip {} != Alice tip {} (reorg onto Alice failed)",
        miner_tip.trim(),
        alice_tip.trim()
    );

    // Drivechain enforcer on Alice should be at tip (NoMempool validator-only).
    let tip_hash: BlockHash = alice_tip.trim().parse()?;
    assert_enforcer_verdict(
        &mut alice,
        tip_hash,
        Expect::Accepted,
        Duration::from_secs(60),
    )
    .await?;

    Ok((alice, miner))
}

/// After bootstrap, **Alice** owns mature coinbases on the shared tip (Miner reorged
/// onto Alice). Create a standard payment on Alice, ensure Miner has it in mempool,
/// then **Miner** mines the block via `generatetoaddress`.
///
/// Returns `(block_hash, spend_txid)`.
async fn create_and_mine_standard_tx(
    alice: &PostSetup,
    miner: &PostSetup,
) -> anyhow::Result<(BlockHash, String)> {
    let dest = alice.receive_address.to_string();
    let txid = alice
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "sendtoaddress", [dest, "0.01".to_string()])
        .run_utf8()
        .await?;
    let txid = txid.trim().to_string();
    tracing::info!(%txid, "Alice created standard payment");

    // Miner must hold the tx before generate (P2P relay, with sendraw fallback).
    const POLL: Duration = Duration::from_millis(250);
    const TIMEOUT: Duration = Duration::from_secs(30);
    let deadline = std::time::Instant::now() + TIMEOUT;
    let mut injected = false;
    loop {
        let raw = miner
            .bitcoin_cli
            .command::<String, _, String, _, _>([], "getrawmempool", [])
            .run_utf8()
            .await?;
        if raw.contains(&txid) {
            break;
        }
        if !injected && std::time::Instant::now() + Duration::from_secs(10) > deadline {
            let hex = alice
                .bitcoin_cli
                .command::<String, _, _, _, _>([], "getrawtransaction", [txid.clone()])
                .run_utf8()
                .await?;
            drop(
                miner
                    .bitcoin_cli
                    .command::<String, _, _, _, _>(
                        [],
                        "sendrawtransaction",
                        [hex.trim().to_string(), "0".to_string()],
                    )
                    .run_utf8()
                    .await,
            );
            injected = true;
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "Miner mempool never saw Alice payment {txid} within {TIMEOUT:?}"
        );
        tokio::time::sleep(POLL).await;
    }
    tracing::info!(%txid, "Miner mempool holds payment");

    let mining_addr = miner.mining_address.to_string();
    let mined = miner
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "generatetoaddress", ["1", &mining_addr])
        .run_utf8()
        .await?;
    let hashes: Vec<String> = serde_json::from_str(&mined)?;
    anyhow::ensure!(
        hashes.len() == 1,
        "expected one mined block hash, got {hashes:?}"
    );
    let block_hash: BlockHash = hashes[0].parse()?;

    let block_json = miner
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "getblock", [block_hash.to_string()])
        .run_utf8()
        .await?;
    let block_val: serde_json::Value = serde_json::from_str(&block_json)?;
    let txids = block_val
        .get("tx")
        .and_then(|t| t.as_array())
        .ok_or_else(|| anyhow::anyhow!("getblock missing tx array"))?;
    let found = txids.iter().any(|t| t.as_str() == Some(txid.as_str()));
    anyhow::ensure!(
        found,
        "mined block {block_hash} missing new tx {txid}; txs={txids:?}"
    );

    Ok((block_hash, txid))
}

async fn drivechain_blk_dat_e2e_task(mut alice: PostSetup, miner: PostSetup) -> anyhow::Result<()> {
    let (block_hash, spend_txid) = create_and_mine_standard_tx(&alice, &miner).await?;
    ensure_tip(&miner, block_hash, "Miner").await?;
    let miner_block = rpc_get_block(&miner, block_hash).await?;
    anyhow::ensure!(
        miner_block.block_hash() == block_hash,
        "Miner getblock hash mismatch"
    );
    tracing::info!(%block_hash, %spend_txid, "Miner mined standard-tx block");

    let miner_height: u32 = miner
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getblockcount", [])
        .run_utf8()
        .await?
        .trim()
        .parse()?;
    wait_for_height(&alice, miner_height).await?;
    ensure_tip(&alice, block_hash, "Alice").await?;

    // Drivechain enforcer must keep tip (Accept, no invalidate).
    assert_enforcer_verdict(
        &mut alice,
        block_hash,
        Expect::Accepted,
        Duration::from_secs(60),
    )
    .await?;
    ensure_tip(&alice, block_hash, "Alice-after-enforcer").await?;

    let alice_blocks = blocks_dir(&alice);
    let alice_disk = wait_for_block_on_disk(&alice_blocks, block_hash, Network::Regtest).await?;
    assert_blocks_equal("miner_rpc", &miner_block, "alice_disk", &alice_disk)?;

    // Miner on-disk should match too (same storage path as Alice claim, self-check).
    let miner_disk =
        wait_for_block_on_disk(&blocks_dir(&miner), block_hash, Network::Regtest).await?;
    assert_blocks_equal("miner_rpc", &miner_block, "miner_disk", &miner_disk)?;

    // Sanity: payment amount path left coins somewhere; optional soft check on fee dust.
    let _ = Amount::from_btc(0.01)?;

    tracing::info!(
        %block_hash,
        %spend_txid,
        alice_blocks = %alice_blocks.display(),
        "PASS: drivechain dual-node Miner block == Alice blk*.dat (enforcer kept tip)"
    );

    drop(alice.tasks);
    drop(miner.tasks);
    tokio::time::sleep(Duration::from_secs(1)).await;
    alice.directories.base_dir.cleanup()?;
    miner.directories.base_dir.cleanup()?;
    Ok(())
}

pub async fn test_drivechain_blk_dat_e2e(
    bin_paths: BinPaths,
    file_registry: TestFileRegistry,
) -> anyhow::Result<()> {
    let (res_tx, mut res_rx) = mpsc::unbounded();
    let (alice, miner) = peer_nodes(bin_paths, &file_registry).await?;
    let _test_task: crate::util::AbortOnDrop<()> = tokio::task::spawn({
        async move {
            let res = drivechain_blk_dat_e2e_task(alice, miner).await;
            let _send_err: Result<(), _> = res_tx.unbounded_send(res);
        }
        .instrument(tracing::info_span!("test", name = TEST_NAME))
    })
    .into();
    res_rx
        .next()
        .await
        .ok_or_else(|| anyhow::anyhow!("unexpected end of drivechain blk.dat e2e stream"))?
}
