//! Dual-node regtest helpers for BIP 360 P2P mempool E2E trials.

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
use bitcoin::{Amount, Network, OutPoint, ScriptBuf, Transaction, TxOut, Txid};
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

/// Two peered regtest nodes (Alice + Bob), each with stock Core and a mempool-enabled enforcer.
pub struct DualNodePeers {
    pub alice: PostSetup,
    pub bob: PostSetup,
}

/// Confirmed P2MR UTXO carried across P2P rounds.
#[derive(Clone, Debug)]
pub struct PileUtxo {
    pub outpoint: OutPoint,
    pub amount: u64,
    pub txout: TxOut,
}

#[must_use]
pub fn bip360_p2p_setup_opts(peer_listen_port: u16) -> SetupOpts {
    SetupOpts {
        bitcoind_kind: BitcoindKind::Unpatched,
        // Wallet + electrs required: validator-only `--enable-mempool` exits after initial sync.
        enable_enforcer_wallet: true,
        bitcoind_args: vec![
            "-debug=mempool".to_string(),
            "-debug=net".to_string(),
            "-debug=validation".to_string(),
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
}

impl PreDualSetup {
    fn new(bin_paths: BinPaths, file_registry: &TestFileRegistry) -> anyhow::Result<Self> {
        let alice = PreSetup::new(bin_paths.clone(), crate::setup::Network::Regtest)?;
        let bob = PreSetup::new(bin_paths, crate::setup::Network::Regtest)?;
        let directories = Directories {
            alice: &alice.directories,
            bob: &bob.directories,
        };
        directories.register_all(file_registry, TEST_NAME);
        Ok(Self { alice, bob })
    }

    async fn setup(
        self,
        res_tx: mpsc::UnboundedSender<anyhow::Result<()>>,
    ) -> anyhow::Result<DualNodePeers> {
        let bob_listen = self.bob.reserved_ports.bitcoind_listen.port();
        let alice_listen = self.alice.reserved_ports.bitcoind_listen.port();

        let alice = {
            let opts = bip360_p2p_setup_opts(bob_listen);
            self.alice
                .setup(Mode::GetBlockTemplate, opts, res_tx.clone())
                .await?
        };
        let bob = {
            let opts = bip360_p2p_setup_opts(alice_listen);
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

        Ok(DualNodePeers { alice, bob })
    }
}

/// Bootstrap dual peers, mature Alice's wallet, and sync both enforcers to bitcoind.
pub async fn setup_dual_node_peers(
    bin_paths: BinPaths,
    file_registry: &TestFileRegistry,
) -> anyhow::Result<DualNodePeers> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let mut peers = PreDualSetup::new(bin_paths, file_registry)?
        .setup(res_tx)
        .await?;

    fund_wallet(&mut peers.alice, 100).await?;
    crate::bip360_block::wait_for_enforcer_synced(&mut peers.alice).await?;
    crate::bip360_block::wait_for_enforcer_synced(&mut peers.bob).await?;
    Ok(peers)
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

    let amount_btc = Amount::from_sat(amount_sats).to_btc();
    let inputs = serde_json::json!([{ "txid": utxo.txid, "vout": utxo.vout }]);
    let outputs = serde_json::json!([{
        "scriptPubKey": hex::encode(script_pubkey.as_bytes()),
        "amount": amount_btc,
    }]);
    let raw_hex = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "createrawtransaction",
            [inputs.to_string(), outputs.to_string()],
        )
        .run_utf8()
        .await?;
    let funded_json = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "fundrawtransaction", [raw_hex.trim()])
        .run_utf8()
        .await?;
    let funded: SignResult = serde_json::from_str(&funded_json)?;
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

/// Broadcast a non-standard P2MR tx directly to a peer's bitcoind P2P port.
///
/// Retries on timeout and treats success when the tx is already in the broadcaster's mempool
/// (common when a prior inject succeeded but the P2P handshake timed out).
pub async fn broadcast_to_peer(
    broadcaster: &PostSetup,
    peer_listen_port: u16,
    tx: Transaction,
) -> anyhow::Result<()> {
    let txid = tx.compute_txid();
    if bip360_tx_report::wait_for_mempool_entry(broadcaster, txid, Duration::from_millis(500))
        .await
        .is_ok()
    {
        return Ok(());
    }

    let height = current_block_height(broadcaster).await?;
    let magic = Network::Regtest.magic();
    let peer_addr = format!("127.0.0.1:{peer_listen_port}")
        .parse()
        .map_err(|err| anyhow::anyhow!("invalid peer P2P address: {err}"))?;

    for attempt in 0..3 {
        match broadcast_nonstandard_tx(peer_addr, height, magic, tx.clone()).await {
            Ok(true) => return Ok(()),
            Ok(false) | Err(_) => {
                if bip360_tx_report::wait_for_mempool_entry(
                    broadcaster,
                    txid,
                    Duration::from_millis(500),
                )
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

    anyhow::bail!("broadcast_nonstandard_tx failed after retries and tx {txid} not in mempool");
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
    broadcast_to_peer(
        &peers.bob,
        peers.alice.reserved_ports.bitcoind_listen.port(),
        tx.clone(),
    )
    .await?;
    assert_tx_accepted_by_both_peers(
        peers,
        1,
        &tx,
        Duration::from_secs(45),
        Duration::from_secs(45),
    )
    .await?;
    mine_one_block(&mut peers.bob).await?;
    sync_both_enforcers(&mut peers.alice, &mut peers.bob).await?;
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
    broadcast_to_peer(
        &peers.alice,
        peers.bob.reserved_ports.bitcoind_listen.port(),
        tx.clone(),
    )
    .await?;
    assert_tx_accepted_by_both_peers(
        peers,
        2,
        &tx,
        Duration::from_secs(45),
        Duration::from_secs(45),
    )
    .await?;
    mine_one_block(&mut peers.alice).await?;
    sync_both_enforcers(&mut peers.alice, &mut peers.bob).await?;
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
    broadcast_to_peer(
        &peers.bob,
        peers.alice.reserved_ports.bitcoind_listen.port(),
        tx.clone(),
    )
    .await?;
    assert_tx_accepted_by_both_peers(
        peers,
        3,
        &tx,
        Duration::from_secs(45),
        Duration::from_secs(45),
    )
    .await?;
    mine_one_block(&mut peers.bob).await?;
    sync_both_enforcers(&mut peers.alice, &mut peers.bob).await?;
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
pub async fn round4_alice_kitchen_sink_spend(
    peers: &mut DualNodePeers,
    pile: PileUtxo,
) -> anyhow::Result<TxReport> {
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
    broadcast_to_peer(
        &peers.alice,
        peers.bob.reserved_ports.bitcoind_listen.port(),
        tx.clone(),
    )
    .await?;
    let (alice_fees, _bob_fees) = assert_tx_accepted_by_both_peers(
        peers,
        4,
        &tx,
        Duration::from_secs(60),
        Duration::from_secs(60),
    )
    .await?;

    assert_eq!(
        tx.input[0].witness.len(),
        5,
        "kitchen-sink witness stack depth"
    );
    let report = bip360_tx_report::tx_report(&tx, Some(alice_fees));
    assert_eq!(report.pqc_algos.len(), 3, "expected three PQC algorithms");
    assert!(report.pqc_algos.contains(&"schnorr"));
    assert!(report.pqc_algos.contains(&"mldsa"));
    assert!(report.pqc_algos.contains(&"slh"));
    assert!(
        report.weight > 10_000,
        "kitchen-sink tx weight should exceed 10_000 WU (got {})",
        report.weight
    );

    mine_one_block(&mut peers.alice).await?;
    sync_both_enforcers(&mut peers.alice, &mut peers.bob).await?;
    Ok(report)
}
