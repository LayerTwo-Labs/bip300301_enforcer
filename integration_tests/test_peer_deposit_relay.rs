//! A deposit broadcast on one node, relayed over p2p, and mined by another.
//!
//! Every other deposit test is single-node, so the transaction never goes on
//! the wire and nothing checks that a deposit is *relayable*. Here the sender
//! configures no `--p2p-broadcast-addr`, leaving Bitcoin Core's own relay as
//! the only way it can reach the miner.
//!
//! That makes this a standardness test: relay applies policy rules, not just
//! consensus, so an OP_DRIVECHAIN output or a CTIP-spending input that
//! consensus accepts but policy does not would confirm on one node and never
//! leave it.

use std::collections::HashMap;

use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    proto::{
        self,
        mainchain::{
            BlockHeaderInfo, BlockInfo, CreateDepositTransactionRequest,
            CreateDepositTransactionResponse, CreateNewAddressRequest, GetBalanceRequest,
            GetChainTipRequest, GetCtipRequest, GetCtipResponse,
            ListSidechainDepositTransactionsRequest, OutPoint, SendTransactionRequest,
            SendTransactionResponse, SubscribeEventsRequest, SubscribeEventsResponse,
            WalletTransaction, block_info, subscribe_events_response,
        },
    },
};
use connectrpc::ConnectError;
use futures::{StreamExt as _, channel::mpsc};
use tracing::Instrument as _;

use crate::{
    integration_test::{
        activate_sidechain, fund_enforcer, propose_sidechain, wait_for_wallet_sync,
    },
    mine,
    mine::MiningPolicy,
    setup::{
        BitcoindKind, DummySidechain, Mode, Network, PostSetup as NodeSetup, SetupOpts, Sidechain,
        WAIT_POLL_INTERVAL_SUBPROCESS, wait_for_port_free, wait_for_tx_in_mempool, wait_until,
        wait_until_every,
    },
    util::{self, BinPaths, TestFileRegistry},
};

pub const TEST_NAME: &str = "peer_deposit_relay";

pub fn trial_name(network: Network) -> String {
    format!("{TEST_NAME} (network: {network})")
}

const SENDER_FUNDING: bitcoin::Amount = bitcoin::Amount::from_sat(100_000_000);
const DEPOSIT_AMOUNT: bitcoin::Amount = bitcoin::Amount::from_sat(21_000_000);
const DEPOSIT_FEE: bitcoin::Amount = bitcoin::Amount::from_sat(1_000_000);

const SIDECHAIN_ADDRESS: &str = "peer relay sidechain address";

struct Directories<'a> {
    /// Mines the blocks
    miner: &'a crate::setup::Directories,
    /// Builds and broadcasts the deposit
    sender: &'a crate::setup::Directories,
}

impl Directories<'_> {
    fn register_files(&self, file_registry: &TestFileRegistry, test_name: &str) {
        self.miner
            .register_files(file_registry, test_name, Some("miner"));
        self.sender
            .register_files(file_registry, test_name, Some("sender"));
    }
}

struct PostSetup {
    miner: NodeSetup,
    sender: NodeSetup,
}

struct PreSetup {
    network: Network,
    miner: crate::setup::PreSetup,
    sender: crate::setup::PreSetup,
}

impl PreSetup {
    fn new(
        bin_paths: BinPaths,
        network: Network,
        file_registry: &TestFileRegistry,
    ) -> anyhow::Result<Self> {
        let miner = crate::setup::PreSetup::new(bin_paths.clone(), network)?;
        let sender = crate::setup::PreSetup::new(bin_paths, network)?;
        let directories = Directories {
            miner: &miner.directories,
            sender: &sender.directories,
        };
        directories.register_files(file_registry, &trial_name(network));
        Ok(Self {
            network,
            miner,
            sender,
        })
    }

    async fn setup(
        self,
        res_tx: mpsc::UnboundedSender<anyhow::Result<()>>,
    ) -> anyhow::Result<PostSetup> {
        // The nodes agree on the magic by being the same build; the enforcers
        // are told it only to keep their view of the chain consistent with
        // their node.
        let magic_args: Vec<String> = crate::setup::bitcoind_regtest_magic()
            .iter()
            .map(|magic| format!("--network-magic={magic}"))
            .collect();
        let setup_opts = || SetupOpts {
            bitcoind_args: vec!["-debug=mempool".to_owned(), "-debug=net".to_owned()],
            bitcoind_kind: BitcoindKind::Patched,
            enforcer_args: magic_args.clone(),
            ..Default::default()
        };
        // Signet is only mineable through the enforcer's template server, so
        // both networks run in that mode.
        let sender = self
            .sender
            .setup(Mode::GetBlockTemplate, setup_opts(), res_tx.clone())
            .await?;
        let mut miner = self
            .miner
            .setup(Mode::GetBlockTemplate, setup_opts(), res_tx)
            .await?;
        let _res: String = sender
            .bitcoin_cli
            .command::<String, _, _, _, _>(
                [],
                "addnode",
                [
                    format!("127.0.0.1:{}", miner.reserved_ports.bitcoind_listen.port()),
                    "add".to_owned(),
                ],
            )
            .run_utf8()
            .await?;
        // With no cached chain to restore, each node mined its own first block
        // during setup, leaving competing tips of equal work that Bitcoin Core
        // will not reorg away from. One more block from the miner breaks the
        // tie, and merely extends an already-shared restored chain.
        let () = mine::mine::<DummySidechain>(&mut miner, 1, MiningPolicy::SILENT).await?;
        let post_setup = PostSetup { miner, sender };
        let () = wait_for_nodes_in_sync(&post_setup, "after peering").await?;
        tracing::info!(network = %self.network, "Peered the two nodes");
        Ok(post_setup)
    }
}

/// Bitcoin Core's tip, as opposed to the validator's.
async fn node_tip(node: &NodeSetup) -> anyhow::Result<bitcoin::BlockHash> {
    Ok(node
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getbestblockhash", [])
        .run_utf8()
        .await?
        .trim()
        .parse()?)
}

/// Block until both nodes are on the same tip.
async fn wait_for_nodes_in_sync(post_setup: &PostSetup, when: &str) -> anyhow::Result<()> {
    wait_until_every(
        &format!("both nodes to agree on a tip ({when})"),
        WAIT_POLL_INTERVAL_SUBPROCESS,
        || async {
            let (miner_tip, sender_tip) =
                futures::try_join!(node_tip(&post_setup.miner), node_tip(&post_setup.sender))?;
            Ok(miner_tip == sender_tip)
        },
    )
    .await
}

/// The sidechain's treasury UTXO as `node`'s validator sees it.
async fn ctip(node: &NodeSetup) -> anyhow::Result<Option<(bitcoin::Txid, u64)>> {
    let resp = node
        .validator_service_client
        .get_ctip(GetCtipRequest {
            sidechain_number: proto::wrap_u32(DummySidechain::SIDECHAIN_NUMBER.0.into()),
        })
        .await?
        .into_owned();
    let Some(ctip) = resp.ctip.into_option() else {
        return Ok(None);
    };
    let txid = ctip
        .txid
        .into_option()
        .ok_or_else(|| proto::Error::missing_field::<GetCtipResponse>("ctip.txid"))?
        .decode::<GetCtipResponse, bitcoin::Txid>("ctip.txid")?;
    Ok(Some((txid, ctip.value)))
}

/// Check that `block_info` reports exactly one event, a deposit of
/// `deposit_txid`.
fn expect_deposit_event(block_info: BlockInfo, deposit_txid: &bitcoin::Txid) -> anyhow::Result<()> {
    let events = block_info.events;
    let [event] = events.as_slice() else {
        anyhow::bail!("Expected exactly one block event, found `{events:?}`")
    };
    let Some(block_info::event::Event::Deposit(deposit)) = &event.event else {
        anyhow::bail!("Expected a deposit event, found `{event:?}`")
    };
    let outpoint = deposit
        .outpoint
        .as_option()
        .ok_or_else(|| proto::Error::missing_field::<proto::mainchain::Deposit>("outpoint"))?;
    let txid = outpoint
        .txid
        .clone()
        .into_option()
        .ok_or_else(|| proto::Error::missing_field::<OutPoint>("txid"))?
        .decode::<OutPoint, bitcoin::Txid>("txid")?;
    anyhow::ensure!(
        txid == *deposit_txid,
        "Deposit event names tx {txid}, expected {deposit_txid}",
    );
    Ok(())
}

/// Wait for the deposit to cross the wire into the miner node's mempool.
///
/// Relay rejections are silent on the receiving side, so on timeout ask the
/// miner node what it makes of the transaction rather than reporting only that
/// nothing arrived.
async fn wait_for_deposit_relay(
    post_setup: &PostSetup,
    deposit_txid: &bitcoin::Txid,
) -> anyhow::Result<()> {
    let Err(wait_err) = wait_for_tx_in_mempool(&post_setup.miner.bitcoin_cli, deposit_txid).await
    else {
        return Ok(());
    };
    let raw_tx = post_setup
        .sender
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "getrawtransaction", [deposit_txid.to_string()])
        .run_utf8()
        .await?;
    let verdict = post_setup
        .miner
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "testmempoolaccept",
            [serde_json::json!([raw_tx.trim()]).to_string()],
        )
        .run_utf8()
        .await?;
    // `allowed: false` names the policy rule that turned the deposit away;
    // `allowed: true` means it simply never arrived.
    anyhow::bail!(
        "Deposit {deposit_txid} was accepted by the node that built it, but never reached \
         the miner node. `testmempoolaccept` on the miner node says: {}. Underlying wait \
         error: {wait_err:#}",
        verdict.trim(),
    )
}

/// Read the sender's event stream until the block the miner mined connects,
/// and check that the sender derives the deposit from it.
async fn expect_sender_deposit_event(
    events: &mut tokio::sync::mpsc::UnboundedReceiver<
        Result<SubscribeEventsResponse, ConnectError>,
    >,
    mined_block_hash: bitcoin::BlockHash,
    deposit_txid: &bitcoin::Txid,
) -> anyhow::Result<()> {
    // Two hops: the block has to reach the sender over p2p, then be connected.
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
    let read_events = async {
        loop {
            let Some(resp) = events.recv().await.transpose()? else {
                anyhow::bail!("Sender's event stream closed before block {mined_block_hash}")
            };
            let Some(event) = resp.event.into_option().and_then(|inner| inner.event) else {
                anyhow::bail!("Expected an event in the sender's event stream")
            };
            let subscribe_events_response::event::Event::ConnectBlock(connect_block) = event else {
                anyhow::bail!("Unexpected block disconnect on the sender: `{event:?}`")
            };
            let block_hash = connect_block
                .header_info
                .into_option()
                .ok_or_else(|| {
                    proto::Error::missing_field::<subscribe_events_response::event::ConnectBlock>(
                        "header_info",
                    )
                })?
                .block_hash
                .into_option()
                .ok_or_else(|| proto::Error::missing_field::<BlockHeaderInfo>("block_hash"))?
                .decode::<BlockHeaderInfo, bitcoin::BlockHash>("block_hash")?;
            if block_hash != mined_block_hash {
                tracing::debug!(%block_hash, "Skipping an earlier connect event on the sender");
                continue;
            }
            let block_info = connect_block.block_info.into_option().ok_or_else(|| {
                proto::Error::missing_field::<subscribe_events_response::event::ConnectBlock>(
                    "block_info",
                )
            })?;
            return expect_deposit_event(block_info, deposit_txid);
        }
    };
    match tokio::time::timeout(TIMEOUT, read_events).await {
        Ok(res) => res,
        Err(_) => anyhow::bail!(
            "Timed out waiting for the sender to connect block {mined_block_hash} and \
             report the deposit in it",
        ),
    }
}

async fn test_peer_deposit_relay_task(mut post_setup: PostSetup) -> anyhow::Result<()> {
    tracing::info!("Setup successfully");
    // All on the miner, so the sender learns of the sidechain the way any
    // other node would: from blocks.
    let () = propose_sidechain::<DummySidechain>(&mut post_setup.miner).await?;
    tracing::info!("Proposed sidechain successfully");
    let () = activate_sidechain::<DummySidechain>(&mut post_setup.miner).await?;
    tracing::info!("Activated sidechain successfully");
    let () = fund_enforcer::<DummySidechain>(&mut post_setup.miner).await?;
    tracing::info!("Funded enforcer successfully (miner)");

    // From the miner, not the sender's own node: on signet both restore the
    // same cached chain, so their Core wallets hold the *same* coins.
    let sender_addr = post_setup
        .sender
        .wallet_service_client
        .create_new_address(CreateNewAddressRequest::default())
        .await?
        .into_owned()
        .address;
    let funding_txid = post_setup
        .miner
        .wallet_service_client
        .send_transaction(SendTransactionRequest {
            destinations: HashMap::from([(sender_addr, SENDER_FUNDING.to_sat())])
                .into_iter()
                .collect(),
            ..Default::default()
        })
        .await?
        .into_owned()
        .txid
        .into_option()
        .ok_or_else(|| proto::Error::missing_field::<SendTransactionResponse>("txid"))?
        .decode::<SendTransactionResponse, bitcoin::Txid>("txid")?;
    // The miner builds blocks from the enforcer's mempool mirror, so mining
    // the moment `SendTransaction` returns races that mirror.
    let () = mine::wait_for_tx_in_block_template(&post_setup.miner, &funding_txid).await?;
    let () =
        crate::mine::mine::<DummySidechain>(&mut post_setup.miner, 1, MiningPolicy::SILENT).await?;
    let () = wait_until("sender wallet to see the funding tx confirmed", || async {
        let balance = post_setup
            .sender
            .wallet_service_client
            .get_balance(GetBalanceRequest::default())
            .await?
            .into_owned();
        Ok(balance.confirmed_sats >= SENDER_FUNDING.to_sat())
    })
    .await?;
    tracing::info!("Funded enforcer successfully (sender)");

    // The deposit is built against the sidechain's CTIP, so the sender has to
    // have caught up to the activation first.
    let () = wait_for_nodes_in_sync(&post_setup, "before the deposit").await?;
    anyhow::ensure!(
        ctip(&post_setup.sender).await?.is_none(),
        "Expected the sidechain to have no treasury UTXO before its first deposit",
    );

    // Subscribe before the deposit exists, so its connect event cannot be
    // missed. Pumped into a channel as `DummySidechain` does, since the stream
    // borrows the client the deposit below needs.
    let (mut sender_events, _sender_events_pump) = {
        let mut stream = post_setup
            .sender
            .validator_service_client
            .subscribe_events(SubscribeEventsRequest {
                sidechain_id: proto::wrap_u32(DummySidechain::SIDECHAIN_NUMBER.0.into()),
            })
            .await?;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let pump: util::AbortOnDrop<()> = tokio::spawn(async move {
            loop {
                match stream.message().await {
                    Ok(Some(view)) => {
                        if tx.send(Ok(view.to_owned_message())).is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(err) => {
                        drop(tx.send(Err(err)));
                        break;
                    }
                }
            }
        })
        .into();
        (rx, pump)
    };

    let deposit_txid = post_setup
        .sender
        .wallet_service_client
        .create_deposit_transaction(CreateDepositTransactionRequest {
            sidechain_id: proto::wrap_u32(DummySidechain::SIDECHAIN_NUMBER.0.into()),
            address: proto::wrap_string(SIDECHAIN_ADDRESS),
            value_sats: proto::wrap_u64(DEPOSIT_AMOUNT.to_sat()),
            fee_sats: proto::wrap_u64(DEPOSIT_FEE.to_sat()),
        })
        .await?
        .into_owned()
        .txid
        .into_option()
        .ok_or_else(|| proto::Error::missing_field::<CreateDepositTransactionResponse>("txid"))?
        .decode::<CreateDepositTransactionResponse, bitcoin::Txid>("txid")?;
    tracing::info!(%deposit_txid, "Created deposit on the sender");

    // `sendrawtransaction` applies the same policy check relay does, so a
    // deposit missing here was never going to relay either.
    let () = wait_for_tx_in_mempool(&post_setup.sender.bitcoin_cli, &deposit_txid).await?;
    tracing::info!("Deposit accepted by the sender's own node");

    let () = wait_for_deposit_relay(&post_setup, &deposit_txid).await?;
    tracing::info!("Deposit relayed to the miner node over p2p");

    // Mined by a node whose wallet never built the deposit.
    let () = mine::wait_for_tx_in_block_template(&post_setup.miner, &deposit_txid).await?;
    let () = mine::mine_check_block_events::<_, DummySidechain>(
        &mut post_setup.miner,
        1,
        MiningPolicy::SILENT,
        |_, block_info| expect_deposit_event(block_info, &deposit_txid),
    )
    .await?;
    let mined_block_hash = post_setup
        .miner
        .validator_service_client
        .get_chain_tip(GetChainTipRequest::default())
        .await?
        .into_owned()
        .block_header_info
        .into_option()
        .ok_or_else(|| proto::Error::missing_field::<GetChainTipRequest>("block_header_info"))?
        .block_hash
        .into_option()
        .ok_or_else(|| proto::Error::missing_field::<BlockHeaderInfo>("block_hash"))?
        .decode::<BlockHeaderInfo, bitcoin::BlockHash>("block_hash")?;
    tracing::info!(%mined_block_hash, "Miner mined the deposit");

    // The other direction: the sender derives the same deposit from a block it
    // received rather than produced.
    let () =
        expect_sender_deposit_event(&mut sender_events, mined_block_hash, &deposit_txid).await?;
    tracing::info!("Sender reported the deposit from the received block");

    let () = wait_for_nodes_in_sync(&post_setup, "after mining the deposit").await?;
    let miner_ctip = ctip(&post_setup.miner).await?;
    let sender_ctip = ctip(&post_setup.sender).await?;
    anyhow::ensure!(
        miner_ctip == sender_ctip,
        "Nodes disagree on the treasury UTXO: miner {miner_ctip:?}, sender {sender_ctip:?}",
    );
    anyhow::ensure!(
        miner_ctip == Some((deposit_txid, DEPOSIT_AMOUNT.to_sat())),
        "Expected the treasury UTXO to be {} sats of {deposit_txid}, found {miner_ctip:?}",
        DEPOSIT_AMOUNT.to_sat(),
    );

    // The wallet side, fed by electrs rather than the validator, has to reach
    // the same conclusion.
    let () = wait_for_wallet_sync(&mut post_setup.sender).await?;
    let () = wait_until("sender's wallet to list the deposit", || async {
        let deposits = post_setup
            .sender
            .wallet_service_client
            .list_sidechain_deposit_transactions(ListSidechainDepositTransactionsRequest {})
            .await?
            .into_owned()
            .transactions;
        for deposit in deposits {
            let Some(tx) = deposit.tx.into_option() else {
                continue;
            };
            let Some(txid) = tx.txid.into_option() else {
                continue;
            };
            if txid.decode::<WalletTransaction, bitcoin::Txid>("txid")? == deposit_txid {
                return Ok(true);
            }
        }
        Ok(false)
    })
    .await?;
    tracing::info!("Sender's wallet lists the deposit");

    tracing::info!(
        "Removing {}, {}",
        post_setup.miner.directories.base_dir.path().display(),
        post_setup.sender.directories.base_dir.path().display()
    );
    // The child processes hold their data dirs open, and aborting a task only
    // schedules cancellation -- a freed port is proof it finished.
    let teardown_ports: Vec<u16> = [&post_setup.miner, &post_setup.sender]
        .into_iter()
        .flat_map(|setup| {
            [
                setup.reserved_ports.bitcoind_rpc.port(),
                setup.reserved_ports.enforcer_serve_grpc.port(),
            ]
        })
        .collect();
    drop(post_setup.miner.tasks);
    drop(post_setup.sender.tasks);
    for port in teardown_ports {
        wait_for_port_free("127.0.0.1", port, std::time::Duration::from_secs(30)).await?;
    }
    post_setup.miner.directories.base_dir.cleanup()?;
    post_setup.sender.directories.base_dir.cleanup()?;
    Ok(())
}

/// Test that a deposit relays and confirms on a node other than the one that
/// broadcast it
/// * Miner proposes and activates a sidechain, and funds Sender's wallet
/// * Sender creates a deposit, which reaches Miner only by Bitcoin Core relay
/// * Miner mines it, and both enforcers report the same deposit
pub async fn test_peer_deposit_relay(
    bin_paths: BinPaths,
    network: Network,
    file_registry: TestFileRegistry,
) -> anyhow::Result<()> {
    let (res_tx, mut res_rx) = mpsc::unbounded();
    let post_setup = PreSetup::new(bin_paths, network, &file_registry)?
        .setup(res_tx.clone())
        .await?;
    let _test_task: util::AbortOnDrop<()> = tokio::task::spawn({
        async move {
            let res = test_peer_deposit_relay_task(post_setup).await;
            let _send_err: Result<(), _> = res_tx.unbounded_send(res);
        }
        .in_current_span()
    })
    .into();
    res_rx
        .next()
        .await
        .ok_or_else(|| anyhow::anyhow!("Unexpected end of test task result stream"))?
}
