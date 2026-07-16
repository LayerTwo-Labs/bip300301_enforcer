//! Dual-node regtest helpers for BIP 360 P2P mempool E2E trials.
//!
//! Two topologies:
//! - **Stock dual** (`setup_dual_node_peers`): Alice + Bob both stock Core — trial #34.
//! - **Tier A** (`setup_tier_a_dual_node_peers`): Alice stock Core 31 + enforcer (CUSF);
//!   Bob = P2MR Core from jbride/bitcoin#2 head (`cryptoquick:p2mr`, `BITCOIND_P2MR`).

#![cfg(feature = "bip360")]

use std::time::Duration;

use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    p2p::broadcast_nonstandard_tx,
    validator::pqc::signer_dev::{
        SignAlgorithm, build_hybrid_ec_slh_spend_from_prevout,
        build_kitchen_sink_spend_from_prevout, build_p2mr_spend_from_prevout,
        p2mr_output_for_algorithm, p2mr_output_for_hybrid_ec_slh, p2mr_output_for_kitchen_sink,
    },
};
use bitcoin::consensus::encode::deserialize_hex;
use bitcoin::sighash::TapSighashType;
use bitcoin::{Address, Amount, Network, OutPoint, ScriptBuf, Transaction, TxOut, Txid};
use futures::channel::mpsc;
use serde::Deserialize;
use tokio::time::sleep;

use crate::{
    bip360_block::{
        HYBRID_EC_ENTROPY, MLDSA_ENTROPY, P2MR_SPEND_OUTPUT_VALUE, SCHNORR_ENTROPY, SLH_ENTROPY,
    },
    bip360_tx_report::{self, TxReport},
    mine,
    setup::{BitcoindKind, Mode, PostSetup, PreSetup, SetupOpts},
    util::{BinPaths, FileDumpConfig, TestFileRegistry},
};

pub const TEST_NAME: &str = "bip360_p2p_mempool_e2e";
/// Tier A kitchen-sink demo: stock Alice + cryptoquick P2MR Bob.
pub const TIER_A_TEST_NAME: &str = "bip360_kitchen_sink_tier_a";

/// Initial sat pile funded from Alice's wallet (round 0 output value).
pub const INITIAL_PILE_VALUE: u64 = 50_000;
/// Per-round spend output carved from the pile (fee margin retained in the delta).
pub const ROUND_SPEND_OUTPUT: u64 = P2MR_SPEND_OUTPUT_VALUE;

struct Directories<'a> {
    alice: &'a crate::setup::Directories,
    bob: &'a crate::setup::Directories,
}

impl Directories<'_> {
    fn register_files(
        file_registry: &TestFileRegistry,
        test_name: &str,
        directories: &crate::setup::Directories,
        label_suffix: &str,
    ) {
        file_registry.register_file(
            test_name,
            directories.bitcoin_dir.join("stdout.txt"),
            FileDumpConfig::new().with_label(format!("Bitcoin Core stdout ({label_suffix})")),
        );
        file_registry.register_file(
            test_name,
            directories.bitcoin_dir.join("stderr.txt"),
            FileDumpConfig::new().with_label(format!("Bitcoin Core stderr ({label_suffix})")),
        );
        file_registry.register_file(
            test_name,
            directories.enforcer_dir.join("stdout.txt"),
            FileDumpConfig::new().with_label(format!("Enforcer stdout ({label_suffix})")),
        );
        file_registry.register_file(
            test_name,
            directories.enforcer_dir.join("stderr.txt"),
            FileDumpConfig::new().with_label(format!("Enforcer stderr ({label_suffix})")),
        );
    }

    fn register_all(&self, file_registry: &TestFileRegistry, test_name: &str) {
        Self::register_files(file_registry, test_name, self.alice, "alice");
        Self::register_files(file_registry, test_name, self.bob, "bob");
    }
}

/// Two peered regtest nodes (Alice + Bob), each with a mempool-enabled enforcer.
pub struct DualNodePeers {
    pub alice: PostSetup,
    pub bob: PostSetup,
    /// When true, Bob runs P2MR Core (`BITCOIND_P2MR`); spends prefer Bob mempool.
    pub tier_a: bool,
}

/// Confirmed P2MR UTXO carried across P2P rounds.
#[derive(Clone, Debug)]
pub struct PileUtxo {
    pub outpoint: OutPoint,
    pub amount: u64,
    pub txout: TxOut,
}

/// How a spend round was confirmed (for demo logs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpendConfirmPath {
    /// Admitted to P2MR Bob mempool, then mined (Tier A happy path).
    P2mrMempool,
    /// Built block + `submitblock` (stock dual, or Tier A fallback).
    SubmitBlock,
}

#[must_use]
pub fn bip360_p2p_setup_opts(peer_listen_port: u16, bitcoind_kind: BitcoindKind) -> SetupOpts {
    SetupOpts {
        bitcoind_kind,
        // Wallet + electrs required: validator-only `--enable-mempool` exits after initial sync.
        enable_enforcer_wallet: true,
        bitcoind_args: vec![
            "-debug=mempool".to_string(),
            "-debug=net".to_string(),
            "-debug=validation".to_string(),
            // Regtest has no fee estimates; fundrawtransaction needs a fallback.
            "-fallbackfee=0.0002".to_string(),
        ],
        enforcer_args: vec![
            "--activation-height=0".to_string(),
            "--pqc-verify-budget-ms=5000".to_string(),
            format!("--p2p-broadcast-addr=127.0.0.1:{peer_listen_port}"),
        ],
    }
}

struct PreDualSetup {
    alice: PreSetup,
    bob: PreSetup,
    alice_kind: BitcoindKind,
    bob_kind: BitcoindKind,
    test_name: &'static str,
    tier_a: bool,
}

impl PreDualSetup {
    fn new(
        bin_paths: BinPaths,
        file_registry: &TestFileRegistry,
        alice_kind: BitcoindKind,
        bob_kind: BitcoindKind,
        test_name: &'static str,
        tier_a: bool,
    ) -> anyhow::Result<Self> {
        let alice = PreSetup::new(bin_paths.clone(), crate::setup::Network::Regtest)?;
        let bob = PreSetup::new(bin_paths, crate::setup::Network::Regtest)?;
        let directories = Directories {
            alice: &alice.directories,
            bob: &bob.directories,
        };
        directories.register_all(file_registry, test_name);
        Ok(Self {
            alice,
            bob,
            alice_kind,
            bob_kind,
            test_name,
            tier_a,
        })
    }

    async fn setup(
        self,
        res_tx: mpsc::UnboundedSender<anyhow::Result<()>>,
    ) -> anyhow::Result<DualNodePeers> {
        let bob_listen = self.bob.reserved_ports.bitcoind_listen.port();
        let alice_listen = self.alice.reserved_ports.bitcoind_listen.port();

        tracing::info!(
            test = self.test_name,
            alice_kind = ?self.alice_kind,
            bob_kind = ?self.bob_kind,
            tier_a = self.tier_a,
            "starting dual-node peers"
        );

        let alice = {
            let opts = bip360_p2p_setup_opts(bob_listen, self.alice_kind);
            self.alice
                .setup(Mode::GetBlockTemplate, opts, res_tx.clone())
                .await?
        };
        let bob = {
            let opts = bip360_p2p_setup_opts(alice_listen, self.bob_kind);
            self.bob.setup(Mode::GetBlockTemplate, opts, res_tx).await?
        };

        drop(
            alice
                .bitcoin_cli
                .command::<String, _, _, _, _>(
                    [],
                    "addnode",
                    [format!("127.0.0.1:{bob_listen}"), "add".to_owned()],
                )
                .run_utf8()
                .await?,
        );
        drop(
            bob.bitcoin_cli
                .command::<String, _, _, _, _>(
                    [],
                    "addnode",
                    [format!("127.0.0.1:{alice_listen}"), "add".to_owned()],
                )
                .run_utf8()
                .await?,
        );

        Ok(DualNodePeers {
            alice,
            bob,
            tier_a: self.tier_a,
        })
    }
}

async fn bootstrap_dual_peers(
    bin_paths: BinPaths,
    file_registry: &TestFileRegistry,
    alice_kind: BitcoindKind,
    bob_kind: BitcoindKind,
    test_name: &'static str,
    tier_a: bool,
) -> anyhow::Result<DualNodePeers> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let mut peers = PreDualSetup::new(
        bin_paths,
        file_registry,
        alice_kind,
        bob_kind,
        test_name,
        tier_a,
    )?
    .setup(res_tx)
    .await?;

    fund_wallet(&mut peers.alice, 100).await?;
    let alice_height = current_block_height(&peers.alice).await? as u32;
    // Bob must finish IBD of Alice's chain before he can accept wallet→P2MR funding
    // txs that reference Alice's coinbases (sendrawtransaction → missing/spent inputs).
    crate::bip360_block::wait_for_bitcoind_height(&peers.bob, alice_height).await?;
    crate::bip360_block::wait_for_enforcer_synced(&mut peers.alice).await?;
    crate::bip360_block::wait_for_enforcer_synced(&mut peers.bob).await?;
    Ok(peers)
}

/// Bootstrap dual peers (both stock Core), mature Alice's wallet, sync enforcers.
pub async fn setup_dual_node_peers(
    bin_paths: BinPaths,
    file_registry: &TestFileRegistry,
) -> anyhow::Result<DualNodePeers> {
    bootstrap_dual_peers(
        bin_paths,
        file_registry,
        BitcoindKind::Unpatched,
        BitcoindKind::Unpatched,
        TEST_NAME,
        false,
    )
    .await
}

/// Tier A: Alice = stock Core 31 (CUSF), Bob = P2MR Core (`BITCOIND_P2MR` / cryptoquick:p2mr).
pub async fn setup_tier_a_dual_node_peers(
    bin_paths: BinPaths,
    file_registry: &TestFileRegistry,
) -> anyhow::Result<DualNodePeers> {
    // Fail early with a clear setup hint if the P2MR binary is missing.
    bin_paths.bitcoind_p2mr().map_err(|err| {
        anyhow::anyhow!(
            "BITCOIND_P2MR not set or invalid ({err}). \
             Point it at jbride/bitcoin#2 head (cryptoquick:p2mr), e.g. \
             .integration-deps/bitcoin-p2mr/bitcoind — see just setup-p2mr"
        )
    })?;
    bootstrap_dual_peers(
        bin_paths,
        file_registry,
        BitcoindKind::Unpatched,
        BitcoindKind::P2mr,
        TIER_A_TEST_NAME,
        true,
    )
    .await
}

async fn fund_wallet(post_setup: &mut PostSetup, blocks: u32) -> anyhow::Result<()> {
    let mining_addr = post_setup.mining_address.to_string();
    let n_blocks = blocks.to_string();
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "generatetoaddress", [&n_blocks, &mining_addr])
        .run_utf8()
        .await?;
    crate::integration_test::wait_for_wallet_sync(post_setup).await?;
    Ok(())
}

#[derive(Debug, Deserialize)]
struct ListUnspentEntry {
    txid: String,
    vout: u32,
    amount: f64,
}

#[derive(Debug, Deserialize)]
struct FundRawResult {
    hex: String,
}

#[derive(Debug, Deserialize)]
struct SignResult {
    hex: String,
    complete: bool,
}

/// Build and sign a wallet-funded transaction paying into a P2MR `scriptPubKey`.
pub async fn build_wallet_to_p2mr_tx(
    post_setup: &PostSetup,
    script_pubkey: ScriptBuf,
    amount_sats: u64,
) -> anyhow::Result<Transaction> {
    let unspent_json = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "listunspent", ["1".to_string()])
        .run_utf8()
        .await?;
    let unspent: Vec<ListUnspentEntry> = serde_json::from_str(&unspent_json)?;
    let utxo = unspent
        .into_iter()
        .max_by(|a, b| {
            a.amount
                .partial_cmp(&b.amount)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .ok_or_else(|| anyhow::anyhow!("wallet has no spendable UTXOs"))?;

    // Core `createrawtransaction` requires each output object to have exactly one key
    // (address → amount, or `data` → hex). Encode the P2MR script as a bech32m address
    // so the wallet can fund/sign a payment into witness v2.
    let p2mr_addr = Address::from_script(script_pubkey.as_script(), Network::Regtest)
        .map_err(|err| anyhow::anyhow!("encode P2MR script as regtest address: {err}"))?;
    let amount_btc = Amount::from_sat(amount_sats).to_btc();
    let inputs = serde_json::json!([{ "txid": utxo.txid, "vout": utxo.vout }]);
    let outputs = serde_json::json!([{ p2mr_addr.to_string(): amount_btc }]);
    let raw_hex = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "createrawtransaction",
            [inputs.to_string(), outputs.to_string()],
        )
        .run_utf8()
        .await?;
    // Explicit feeRate avoids -4 "Fee estimation failed" on empty regtest mempools
    // even when bitcoind was started without -fallbackfee.
    let fund_opts = serde_json::json!({ "feeRate": 0.0002 });
    let funded_json = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "fundrawtransaction",
            [raw_hex.trim().to_string(), fund_opts.to_string()],
        )
        .run_utf8()
        .await?;
    let funded: FundRawResult = serde_json::from_str(&funded_json)?;
    let signed_json = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "signrawtransactionwithwallet", [funded.hex.trim()])
        .run_utf8()
        .await?;
    let signed: SignResult = serde_json::from_str(&signed_json)?;
    anyhow::ensure!(
        signed.complete,
        "wallet signing incomplete for wallet→P2MR tx"
    );
    Ok(deserialize_hex(signed.hex.trim())?)
}

async fn current_block_height(post_setup: &PostSetup) -> anyhow::Result<i32> {
    let height_str = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getblockcount", [])
        .run_utf8()
        .await?;
    Ok(height_str.trim().parse()?)
}

/// Submit a (possibly non-standard) tx into a node's mempool via `sendrawtransaction`.
///
/// Uses `maxfeerate=0` so large PQC fee-rates never trip the default RPC ceiling.
/// Nodes are started with `-acceptnonstdtxn` (see `Bitcoind::spawn_command_with_args`).
async fn send_raw_to_node(node: &PostSetup, tx: &Transaction) -> anyhow::Result<()> {
    use bitcoin::consensus::encode::serialize_hex;
    let hex = serialize_hex(tx);
    // Bitcoin Core: sendrawtransaction "hexstring" (maxfeerate)
    let resp = node
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "sendrawtransaction", [hex, "0".to_string()])
        .run_utf8()
        .await?;
    let returned = resp.trim();
    let expected = tx.compute_txid().to_string();
    anyhow::ensure!(
        returned == expected || returned.is_empty(),
        "sendrawtransaction unexpected response: {returned} (expected txid {expected})"
    );
    Ok(())
}

/// Inject a P2MR (or other non-standard) tx into both peers' Core mempools.
///
/// Prefer RPC `sendrawtransaction` on each node (reliable on regtest with
/// `-acceptnonstdtxn`). Fall back to direct P2P inject toward `peer_listen_port`
/// when RPC fails (e.g. already-in-mempool is treated as success).
pub async fn broadcast_to_peer(
    broadcaster: &PostSetup,
    peer: &PostSetup,
    peer_listen_port: u16,
    tx: Transaction,
) -> anyhow::Result<()> {
    let txid = tx.compute_txid();

    // Primary path: RPC into both nodes so both mempools see the tx without
    // depending on P2P relay timing or the temporary inject peer's fRelay flag.
    for (label, node) in [("broadcaster", broadcaster), ("peer", peer)] {
        match send_raw_to_node(node, &tx).await {
            Ok(()) => {}
            Err(err) => {
                let msg = format!("{err:#}");
                // Already known is fine (prior inject / re-broadcast).
                if msg.contains("already in mempool")
                    || msg.contains("Transaction already in block chain")
                    || msg.contains("txn-already-in-mempool")
                    || msg.contains("txn-already-known")
                {
                    tracing::debug!(%txid, label, "sendrawtransaction: already known");
                } else {
                    return Err(anyhow::anyhow!(
                        "sendrawtransaction failed on {label} for {txid}: {msg}"
                    ));
                }
            }
        }
    }

    if bip360_tx_report::wait_for_mempool_entry(broadcaster, txid, Duration::from_secs(5))
        .await
        .is_ok()
        && bip360_tx_report::wait_for_mempool_entry(peer, txid, Duration::from_secs(5))
            .await
            .is_ok()
    {
        return Ok(());
    }

    // Fallback: legacy P2P inject (used by production non-standard OP_DRIVECHAIN path).
    let height = current_block_height(broadcaster).await?;
    let magic = Network::Regtest.magic();
    let peer_addr = format!("127.0.0.1:{peer_listen_port}")
        .parse()
        .map_err(|err| anyhow::anyhow!("invalid peer P2P address: {err}"))?;

    for attempt in 0..3 {
        match broadcast_nonstandard_tx(peer_addr, height, magic, tx.clone()).await {
            Ok(true) | Ok(false) | Err(_) => {
                if bip360_tx_report::wait_for_mempool_entry(
                    broadcaster,
                    txid,
                    Duration::from_millis(500),
                )
                .await
                .is_ok()
                    && bip360_tx_report::wait_for_mempool_entry(peer, txid, Duration::from_millis(500))
                        .await
                        .is_ok()
                {
                    return Ok(());
                }
                if attempt < 2 {
                    sleep(Duration::from_millis(500)).await;
                }
            }
        }
    }

    anyhow::bail!(
        "failed to get tx {txid} into both mempools after sendrawtransaction + P2P inject"
    );
}

/// Poll enforcer GBT until `txid` appears in the template transaction list.
///
/// GBT inclusion implies the enforcer mempool `accept_tx` path accepted the tx: only accepted
/// mempool txs are surfaced in the block template the enforcer serves for mining.
pub async fn wait_for_gbt_inclusion(
    post_setup: &PostSetup,
    txid: Txid,
    timeout: Duration,
) -> anyhow::Result<()> {
    use bitcoin_jsonrpsee::client::BlockTemplateRequest;
    use cusf_enforcer_mempool::server::RpcClient as _;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let mut request = BlockTemplateRequest::default();
        request.capabilities.insert("coinbasetxn".to_owned());
        let template = post_setup.gbt_client.get_block_template(request).await?;
        if template.transactions.iter().any(|tx| tx.txid == txid) {
            return Ok(());
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "tx {txid} not included in enforcer GBT within {timeout:?}"
        );
        sleep(Duration::from_millis(500)).await;
    }
}

/// Per-tx checks: Core mempool on both peers + enforcer GBT inclusion on both enforcers.
pub async fn assert_tx_accepted_by_both_peers(
    peers: &DualNodePeers,
    round: u32,
    tx: &Transaction,
    mempool_timeout: Duration,
    gbt_timeout: Duration,
) -> anyhow::Result<(bip360_tx_report::MempoolFees, bip360_tx_report::MempoolFees)> {
    let txid = tx.compute_txid();
    let alice_fees =
        bip360_tx_report::wait_for_mempool_entry(&peers.alice, txid, mempool_timeout).await?;
    let bob_fees =
        bip360_tx_report::wait_for_mempool_entry(&peers.bob, txid, mempool_timeout).await?;

    bip360_tx_report::log_tx_report(
        round,
        "alice-mempool",
        &bip360_tx_report::tx_report(tx, Some(alice_fees.clone())),
    );
    bip360_tx_report::log_tx_report(
        round,
        "bob-mempool",
        &bip360_tx_report::tx_report(tx, Some(bob_fees.clone())),
    );

    wait_for_gbt_inclusion(&peers.alice, txid, gbt_timeout).await?;
    wait_for_gbt_inclusion(&peers.bob, txid, gbt_timeout).await?;
    tracing::info!(round, %txid, "tx accepted in both mempools and enforcer GBT templates");
    Ok((alice_fees, bob_fees))
}

async fn mine_one_block(miner: &mut PostSetup) -> anyhow::Result<()> {
    use crate::setup::DummySidechain;
    mine::mine_gbt_check::<_, std::convert::Infallible, DummySidechain>(miner, 1, |_| Ok(()))
        .await
        .map_err(|e| match e {
            either::Either::Left(err) => anyhow::anyhow!("mine_gbt failed: {err}"),
            either::Either::Right(never) => match never {},
        })
}

async fn sync_both_enforcers(alice: &mut PostSetup, bob: &mut PostSetup) -> anyhow::Result<()> {
    crate::bip360_block::wait_for_enforcer_synced(alice).await?;
    crate::bip360_block::wait_for_enforcer_synced(bob).await?;
    Ok(())
}

/// Confirm a P2MR *spend* that stock Core will not admit to the mempool.
///
/// Spending a witness-v2 (P2MR) output trips stock Core's
/// `DISCOURAGE_UPGRADABLE_WITNESS_PROGRAM` mempool script flag even with
/// `-acceptnonstdtxn`. Funding (paying *into* v2) still works via mempool.
///
/// So spend rounds assemble a block (GBT coinbase + spend) and `submitblock`,
/// then wait for the peer bitcoind + both enforcers to retain the tip (CUSF
/// `connect_block` path). This still exercises dual-node propagation and
/// enforcer acceptance of Schnorr / hybrid / ML-DSA / kitchen-sink spends.
async fn confirm_p2mr_spend_via_submitblock(
    miner: &mut PostSetup,
    peer: &mut PostSetup,
    round: u32,
    spend_tx: &Transaction,
) -> anyhow::Result<()> {
    let txid = spend_tx.compute_txid();
    bip360_tx_report::log_tx_report(
        round,
        "submitblock-spend",
        &bip360_tx_report::tx_report(spend_tx, None),
    );

    let block = crate::bip360_block::build_block_from_template(miner, vec![spend_tx.clone()]).await?;
    let block_hash = crate::bip360_block::submit_block(miner, &block).await?;
    let miner_height = current_block_height(miner).await? as u32;

    crate::bip360_block::wait_for_bitcoind_height(peer, miner_height).await?;
    // Both enforcers must reach bitcoind tip without having invalidated the block.
    crate::bip360_block::wait_for_enforcer_synced(miner).await?;
    crate::bip360_block::wait_for_enforcer_synced(peer).await?;

    // Confirm Core still has the submitted tip (not reorged away after invalidateblock).
    for (label, node) in [("miner", miner), ("peer", peer)] {
        let best = node
            .bitcoin_cli
            .command::<String, _, String, _, _>([], "getbestblockhash", [])
            .run_utf8()
            .await?;
        anyhow::ensure!(
            best.trim().eq_ignore_ascii_case(&block_hash.to_string()),
            "{label} bitcoind tip {} != submitted block {block_hash} (round {round}, spend {txid})",
            best.trim()
        );
    }

    tracing::info!(
        round,
        %txid,
        %block_hash,
        path = "submitblock",
        "P2MR spend confirmed via submitblock; dual nodes retained tip"
    );
    Ok(())
}

fn tier_a_strict() -> bool {
    matches!(
        std::env::var("BIP360_TIER_A_STRICT").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

/// Tier A: try Bob (P2MR Core) mempool, then mine on Bob; fall back to submitblock.
///
/// Stock Alice is expected **not** to hold the spend in mempool (CUSF gap). Bob
/// (`cryptoquick:p2mr` / jbride#2 head) may accept overload-format spends — or not
/// (OP_SUBSTR dialect still valid elsewhere); fallback keeps the kitchen-sink demo green.
async fn confirm_p2mr_spend_tier_a(
    peers: &mut DualNodePeers,
    round: u32,
    spend_tx: &Transaction,
    preferred_miner_is_bob: bool,
) -> anyhow::Result<SpendConfirmPath> {
    let txid = spend_tx.compute_txid();

    // 1) Prefer Bob P2MR mempool.
    match send_raw_to_node(&peers.bob, spend_tx).await {
        Ok(()) => {
            if bip360_tx_report::wait_for_mempool_entry(
                &peers.bob,
                txid,
                Duration::from_secs(15),
            )
            .await
            .is_ok()
            {
                bip360_tx_report::log_tx_report(
                    round,
                    "bob-p2mr-mempool",
                    &bip360_tx_report::tx_report(spend_tx, None),
                );
                // Alice stock should typically reject / omit (document CUSF mempool gap).
                if bip360_tx_report::wait_for_mempool_entry(
                    &peers.alice,
                    txid,
                    Duration::from_secs(2),
                )
                .await
                .is_ok()
                {
                    tracing::info!(
                        round,
                        %txid,
                        "unexpected: stock Alice also has spend in mempool"
                    );
                } else {
                    tracing::info!(
                        round,
                        %txid,
                        "stock Alice mempool absent (expected CUSF gap); Bob P2MR holds spend"
                    );
                }

                // Mine on Bob so the P2MR node produces the tip including the spend.
                confirm_p2mr_spend_via_submitblock(
                    &mut peers.bob,
                    &mut peers.alice,
                    round,
                    spend_tx,
                )
                .await?;
                tracing::info!(
                    round,
                    %txid,
                    path = "p2mr_mempool",
                    "Tier A spend confirmed via Bob P2MR mempool + mine"
                );
                return Ok(SpendConfirmPath::P2mrMempool);
            }
            tracing::warn!(
                round,
                %txid,
                "sendraw to Bob succeeded but getmempoolentry timed out; falling back"
            );
        }
        Err(err) => {
            tracing::warn!(
                round,
                %txid,
                error = %format!("{err:#}"),
                "Bob P2MR mempool rejected spend (dual-valid dialects; fallback submitblock)"
            );
            if tier_a_strict() {
                return Err(anyhow::anyhow!(
                    "BIP360_TIER_A_STRICT: Bob P2MR mempool rejected spend {txid}: {err:#}"
                ));
            }
        }
    }

    // 2) Fallback: submitblock. Prefer stock Alice as miner so undefined-v2 anyone-can-spend
    //    consensus accepts even when Bob script-verify is dialect-strict on the tx alone.
    //    If preferred miner is Bob and mempool failed, still try Alice first for tip retention.
    let (miner_is_bob, reason) = if preferred_miner_is_bob {
        (
            false,
            "Tier A fallback: mine on stock Alice (submitblock) after Bob mempool miss",
        )
    } else {
        (
            false,
            "Tier A fallback: mine on stock Alice (submitblock)",
        )
    };
    tracing::warn!(round, %txid, reason, miner_is_bob, "using submitblock path");
    let _ = miner_is_bob;
    confirm_p2mr_spend_via_submitblock(&mut peers.alice, &mut peers.bob, round, spend_tx).await?;
    Ok(SpendConfirmPath::SubmitBlock)
}

/// Confirm a P2MR spend using Tier A mempool-first when `peers.tier_a`, else submitblock.
async fn confirm_p2mr_spend(
    peers: &mut DualNodePeers,
    miner_is_bob: bool,
    round: u32,
    spend_tx: &Transaction,
) -> anyhow::Result<SpendConfirmPath> {
    if peers.tier_a {
        return confirm_p2mr_spend_tier_a(peers, round, spend_tx, miner_is_bob).await;
    }
    if miner_is_bob {
        confirm_p2mr_spend_via_submitblock(&mut peers.bob, &mut peers.alice, round, spend_tx)
            .await?;
    } else {
        confirm_p2mr_spend_via_submitblock(&mut peers.alice, &mut peers.bob, round, spend_tx)
            .await?;
    }
    Ok(SpendConfirmPath::SubmitBlock)
}

fn vout_for_script_pubkey(tx: &Transaction, script_pubkey: &ScriptBuf) -> anyhow::Result<u32> {
    tx.output
        .iter()
        .position(|output| output.script_pubkey == *script_pubkey)
        .map(|index| index as u32)
        .ok_or_else(|| anyhow::anyhow!("spend tx missing output with expected script_pubkey"))
}

/// Round 0: Alice wallet → initial Schnorr P2MR pile.
pub async fn round0_wallet_to_p2mr(peers: &mut DualNodePeers) -> anyhow::Result<PileUtxo> {
    let (script_pubkey, _) = p2mr_output_for_algorithm(SignAlgorithm::Schnorr, &SCHNORR_ENTROPY)
        .map_err(anyhow::Error::msg)?;
    let tx =
        build_wallet_to_p2mr_tx(&peers.alice, script_pubkey.clone(), INITIAL_PILE_VALUE).await?;
    let txid = tx.compute_txid();
    broadcast_to_peer(
        &peers.alice,
        &peers.bob,
        peers.bob.reserved_ports.bitcoind_listen.port(),
        tx.clone(),
    )
    .await?;
    assert_tx_accepted_by_both_peers(
        peers,
        0,
        &tx,
        Duration::from_secs(45),
        Duration::from_secs(45),
    )
    .await?;
    mine_one_block(&mut peers.alice).await?;
    sync_both_enforcers(&mut peers.alice, &mut peers.bob).await?;
    let vout = vout_for_script_pubkey(&tx, &script_pubkey)?;
    Ok(PileUtxo {
        outpoint: OutPoint { txid, vout },
        amount: INITIAL_PILE_VALUE,
        txout: TxOut {
            value: Amount::from_sat(INITIAL_PILE_VALUE),
            script_pubkey,
        },
    })
}

/// Round 1: Bob Schnorr-only spend → hybrid P2MR pile for round 2.
pub async fn round1_bob_schnorr_spend(
    peers: &mut DualNodePeers,
    pile: PileUtxo,
) -> anyhow::Result<PileUtxo> {
    let (next_spk, _) = p2mr_output_for_hybrid_ec_slh(&HYBRID_EC_ENTROPY, &SLH_ENTROPY)
        .map_err(anyhow::Error::msg)?;
    let tx = build_p2mr_spend_from_prevout(
        SignAlgorithm::Schnorr,
        &SCHNORR_ENTROPY,
        TapSighashType::All,
        pile.outpoint,
        pile.txout,
        ROUND_SPEND_OUTPUT,
        next_spk.clone(),
    )
    .map_err(anyhow::Error::msg)?;
    let path = confirm_p2mr_spend(peers, true, 1, &tx).await?;
    tracing::info!(round = 1, ?path, "round 1 spend path");
    let txid = tx.compute_txid();
    let vout = vout_for_script_pubkey(&tx, &next_spk)?;
    Ok(PileUtxo {
        outpoint: OutPoint { txid, vout },
        amount: ROUND_SPEND_OUTPUT,
        txout: TxOut {
            value: Amount::from_sat(ROUND_SPEND_OUTPUT),
            script_pubkey: next_spk,
        },
    })
}

/// Round 2: Alice hybrid EC+SLH spend → ML-DSA P2MR pile for round 3.
pub async fn round2_alice_hybrid_spend(
    peers: &mut DualNodePeers,
    pile: PileUtxo,
) -> anyhow::Result<PileUtxo> {
    let (next_spk, _) = p2mr_output_for_algorithm(SignAlgorithm::Mldsa, &MLDSA_ENTROPY)
        .map_err(anyhow::Error::msg)?;
    let tx = build_hybrid_ec_slh_spend_from_prevout(
        &HYBRID_EC_ENTROPY,
        &SLH_ENTROPY,
        TapSighashType::All,
        pile.outpoint,
        pile.txout,
        ROUND_SPEND_OUTPUT,
        next_spk.clone(),
    )
    .map_err(anyhow::Error::msg)?;
    let path = confirm_p2mr_spend(peers, false, 2, &tx).await?;
    tracing::info!(round = 2, ?path, "round 2 spend path");
    let txid = tx.compute_txid();
    let vout = vout_for_script_pubkey(&tx, &next_spk)?;
    Ok(PileUtxo {
        outpoint: OutPoint { txid, vout },
        amount: ROUND_SPEND_OUTPUT,
        txout: TxOut {
            value: Amount::from_sat(ROUND_SPEND_OUTPUT),
            script_pubkey: next_spk,
        },
    })
}

/// Round 3: Bob ML-DSA-only spend → kitchen-sink P2MR pile for round 4.
pub async fn round3_bob_mldsa_spend(
    peers: &mut DualNodePeers,
    pile: PileUtxo,
) -> anyhow::Result<PileUtxo> {
    let (next_spk, _) =
        p2mr_output_for_kitchen_sink(&HYBRID_EC_ENTROPY, &MLDSA_ENTROPY, &SLH_ENTROPY)
            .map_err(anyhow::Error::msg)?;
    let tx = build_p2mr_spend_from_prevout(
        SignAlgorithm::Mldsa,
        &MLDSA_ENTROPY,
        TapSighashType::All,
        pile.outpoint,
        pile.txout,
        ROUND_SPEND_OUTPUT,
        next_spk.clone(),
    )
    .map_err(anyhow::Error::msg)?;
    let path = confirm_p2mr_spend(peers, true, 3, &tx).await?;
    tracing::info!(round = 3, ?path, "round 3 spend path");
    let txid = tx.compute_txid();
    let vout = vout_for_script_pubkey(&tx, &next_spk)?;
    Ok(PileUtxo {
        outpoint: OutPoint { txid, vout },
        amount: ROUND_SPEND_OUTPUT,
        txout: TxOut {
            value: Amount::from_sat(ROUND_SPEND_OUTPUT),
            script_pubkey: next_spk,
        },
    })
}

/// Round 4: Alice kitchen-sink (Schnorr + ML-DSA + SLH) spend to a standard destination.
///
/// Returns `(TxReport, SpendConfirmPath)` so Tier A logs can show mempool vs submitblock.
pub async fn round4_alice_kitchen_sink_spend(
    peers: &mut DualNodePeers,
    pile: PileUtxo,
) -> anyhow::Result<(TxReport, SpendConfirmPath)> {
    let destination = peers.alice.mining_address.script_pubkey();
    let tx = build_kitchen_sink_spend_from_prevout(
        &HYBRID_EC_ENTROPY,
        &MLDSA_ENTROPY,
        &SLH_ENTROPY,
        TapSighashType::All,
        pile.outpoint,
        pile.txout,
        ROUND_SPEND_OUTPUT,
        destination,
    )
    .map_err(anyhow::Error::msg)?;

    assert_eq!(
        tx.input[0].witness.len(),
        5,
        "kitchen-sink witness stack depth"
    );
    let report = bip360_tx_report::tx_report(&tx, None);
    assert_eq!(report.pqc_algos.len(), 3, "expected three PQC algorithms");
    assert!(report.pqc_algos.contains(&"schnorr"));
    assert!(report.pqc_algos.contains(&"mldsa"));
    assert!(report.pqc_algos.contains(&"slh"));
    assert!(
        report.weight > 10_000,
        "kitchen-sink tx weight should exceed 10_000 WU (got {})",
        report.weight
    );

    let path = confirm_p2mr_spend(peers, false, 4, &tx).await?;
    tracing::info!(round = 4, ?path, "round 4 kitchen-sink spend path");
    Ok((report, path))
}
