use std::collections::HashMap;

use bip300301_enforcer_lib::{
    bins::CommandExt,
    proto::{
        self,
        common::ConsensusHex,
        mainchain::{
            BlockHeaderInfo, CreateBmmCriticalDataTransactionRequest, CreateNewAddressRequest,
            GetBalanceRequest, GetChainTipRequest, SendTransactionRequest, SendTransactionResponse,
        },
    },
};
use buffa::MessageField;
use futures::{StreamExt as _, channel::mpsc};
use tracing::Instrument as _;

use crate::{
    integration_test::{activate_sidechain, fund_enforcer, propose_sidechain},
    mine,
    setup::{
        BitcoindKind, DummySidechain, Mode, Network, SetupOpts, Sidechain,
        WAIT_POLL_INTERVAL_SUBPROCESS, wait_for_port_free, wait_for_tx_in_mempool, wait_until,
        wait_until_every,
    },
    util::{self, BinPaths, FileDumpConfig, TestFileRegistry},
};

struct Directories<'a> {
    /// The sidechain instance that will be mining blocks
    miner: &'a crate::setup::Directories,
    /// Sidechain process that will be sending the BMM request
    sender: &'a crate::setup::Directories,
}

impl Directories<'_> {
    fn register_files_label_suffix(
        file_registry: &TestFileRegistry,
        test_name: &str,
        directories: &crate::setup::Directories,
        label_suffix: &str,
    ) {
        // Register specific files with their own configurations
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

    fn register_files(&self, file_registry: &TestFileRegistry, test_name: &str) {
        Self::register_files_label_suffix(file_registry, test_name, self.miner, "miner");
        Self::register_files_label_suffix(file_registry, test_name, self.sender, "sender");
    }
}

pub const TEST_NAME: &str = "peer_bmm_request";

struct PostSetup {
    /// The sidechain instance that will be mining blocks
    miner: crate::setup::PostSetup,
    /// Sidechain process that will be sending the BMM request
    sender: crate::setup::PostSetup,
}

struct PreSetup {
    miner: crate::setup::PreSetup,
    sender: crate::setup::PreSetup,
}

impl PreSetup {
    fn new(bin_paths: BinPaths, file_registry: &TestFileRegistry) -> anyhow::Result<Self> {
        let miner = crate::setup::PreSetup::new(bin_paths.clone(), Network::Regtest)?;
        let sender = crate::setup::PreSetup::new(bin_paths, Network::Regtest)?;
        let directories = Directories {
            miner: &miner.directories,
            sender: &sender.directories,
        };
        directories.register_files(file_registry, TEST_NAME);
        Ok(Self { miner, sender })
    }

    async fn setup(
        self,
        res_tx: mpsc::UnboundedSender<anyhow::Result<()>>,
    ) -> anyhow::Result<PostSetup> {
        let sender = {
            // Use a hostname rather than an IP to exercise DNS resolution of
            // p2p broadcast addresses
            let enforcer_args = vec![format!(
                "--p2p-broadcast-addr=localhost:{}",
                self.miner.reserved_ports.bitcoind_listen.port()
            )];
            let setup_opts: SetupOpts = SetupOpts {
                bitcoind_args: Vec::new(),
                bitcoind_kind: BitcoindKind::Unpatched,
                enforcer_args,
                ..Default::default()
            };
            self.sender
                .setup(Mode::GetBlockTemplate, setup_opts, res_tx.clone())
                .await?
        };
        let miner = {
            let bitcoind_args = vec![
                "-debug=mempool",
                "-debug=net",
                "-debug=validation",
                "-loglevelalways=1",
                "-logtimemicros=1",
            ];
            let setup_opts: SetupOpts<_> = SetupOpts {
                bitcoind_args,
                bitcoind_kind: BitcoindKind::Patched,
                enforcer_args: Vec::new(),
                ..Default::default()
            };
            self.miner
                .setup(Mode::GetBlockTemplate, setup_opts, res_tx)
                .await?
        };
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
        Ok(PostSetup { miner, sender })
    }
}

async fn test_peer_bmm_request_task(mut post_setup: PostSetup) -> anyhow::Result<()> {
    tracing::info!("Setup successfully");
    let () = propose_sidechain::<DummySidechain>(&mut post_setup.miner).await?;
    tracing::info!("Proposed sidechain successfully");
    let () = activate_sidechain::<DummySidechain>(&mut post_setup.miner).await?;
    tracing::info!("Activated sidechain successfully");
    let () = fund_enforcer::<DummySidechain>(&mut post_setup.miner).await?;
    tracing::info!("Funded enforcer successfully (miner)");

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
            destinations: HashMap::from([(sender_addr, 123_456_u64)])
                .into_iter()
                .collect(),
            ..Default::default()
        })
        .await?
        .into_owned()
        .txid
        .into_option()
        .ok_or_else(|| anyhow::anyhow!("Failed to create a tx to fund sender wallet"))?
        .decode::<SendTransactionResponse, bitcoin::Txid>("txid")?;
    // The block below has to *contain* this transaction, and the miner builds
    // it from the enforcer's mempool mirror rather than from bitcoind's
    // mempool. Mining the moment `SendTransaction` returns races the mirror,
    // which is milliseconds behind: the block goes out with only its coinbase,
    // the sender's balance never moves, and the wait below burns its whole
    // budget for nothing.
    let () = mine::wait_for_tx_in_block_template(&post_setup.miner, &funding_txid).await?;
    let () = crate::mine::mine::<DummySidechain>(&mut post_setup.miner, 1, None).await?;
    // Wait for the sender to receive the block over p2p and credit the funds.
    let () = wait_until("sender wallet to see the funding tx confirmed", || async {
        let balance = post_setup
            .sender
            .wallet_service_client
            .get_balance(GetBalanceRequest::default())
            .await?
            .into_owned();
        Ok(balance.confirmed_sats > 0)
    })
    .await?;
    tracing::info!("Funded enforcer successfully (sender)");

    let BlockHeaderInfo {
        block_hash: tip_block_hash,
        prev_block_hash: _,
        height: tip_height,
        work: _,
        timestamp: _,
        ..
    } = post_setup
        .sender
        .validator_service_client
        .get_chain_tip(GetChainTipRequest::default())
        .await?
        .into_owned()
        .block_header_info
        .into_option()
        .ok_or_else(|| {
            anyhow::anyhow!("Expected `block_header_info field` in GetChainInfoResponse")
        })?;
    let tip_block_hash = tip_block_hash
        .into_option()
        .ok_or_else(|| anyhow::anyhow!("Expected `block_hash field` in BlockHeaderInfo"))?;
    let sidechain_block_hash: [u8; 32] = {
        use bitcoin::hashes::Hash;
        bitcoin::hashes::sha256::Hash::hash(b"dummy sidechain block").to_byte_array()
    };
    let Some(bmm_request_txid) = post_setup
        .sender
        .wallet_service_client
        .create_bmm_critical_data_transaction(CreateBmmCriticalDataTransactionRequest {
            sidechain_id: proto::wrap_u32(DummySidechain::SIDECHAIN_NUMBER.0.into()),
            value_sats: proto::wrap_u64(10_000),
            height: proto::wrap_u32(tip_height),
            critical_hash: MessageField::some(ConsensusHex::encode(&sidechain_block_hash)),
            prev_bytes: MessageField::some(tip_block_hash),
        })
        .await?
        .into_owned()
        .txid
        .into_option()
        .and_then(|txid| proto::unwrap_string(txid.hex))
    else {
        anyhow::bail!("Failed to create BMM critical data tx")
    };
    tracing::info!(%bmm_request_txid, "Created BMM request tx successfully");
    // In addition to the p2p broadcast, the enforcer submits the BMM request
    // to its own node via `sendrawtransaction`, so it should be in the
    // sender node's mempool immediately
    let sender_mempool_entry = post_setup
        .sender
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "getmempoolentry", [bmm_request_txid.clone()])
        .run_utf8()
        .await?;
    tracing::debug!(%sender_mempool_entry);
    // Wait for the BMM request to reach the miner node's mempool over p2p.
    let () = wait_for_tx_in_mempool(
        &post_setup.miner.bitcoin_cli,
        &bmm_request_txid.parse::<bitcoin::Txid>()?,
    )
    .await?;
    // Check that the tx entered the sender node's mempool via RPC broadcast,
    // rather than via p2p relay from the miner node. The enforcer logs this
    // asynchronously, so poll rather than reading the log once.
    const RPC_BROADCAST_LOG_LINE: &str = "Broadcast BMM request transaction via RPC to own node";
    let sender_enforcer_stdout_path = post_setup
        .sender
        .directories
        .enforcer_dir
        .join("stdout.txt");
    // The enforcer log is megabytes of trace output, so re-reading it in full
    // is not a cheap check -- poll it at the slower interval.
    let () = wait_until_every(
        "sender enforcer to log the BMM request RPC broadcast to its own node",
        WAIT_POLL_INTERVAL_SUBPROCESS,
        || async {
            let stdout = std::fs::read_to_string(&sender_enforcer_stdout_path)?;
            Ok(stdout.contains(RPC_BROADCAST_LOG_LINE))
        },
    )
    .await?;
    // Mine a block and check that the BMM request worked
    let () = mine::mine_check_block_events::<_, DummySidechain>(
        &mut post_setup.miner,
        1,
        None,
        |_, block_info| {
            let bmm_commitment = block_info
                .bmm_commitment
                .into_option()
                .ok_or_else(|| anyhow::anyhow!("Expected a BMM commitment"))?;
            let expected_bmm_commitment = ConsensusHex::encode(&sidechain_block_hash);
            anyhow::ensure!(bmm_commitment == expected_bmm_commitment);
            Ok(())
        },
    )
    .await?;
    tracing::info!("Included BMM request tx successfully");
    tracing::info!(
        "Removing {}, {}",
        post_setup.miner.directories.base_dir.path().display(),
        post_setup.sender.directories.base_dir.path().display()
    );
    // The child processes hold their data dirs open, so wait for them to
    // actually exit before removing the dirs out from under them. Aborting the
    // task only schedules cancellation; a freed port is proof it finished.
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

/// Test broadcasting and receiving a peer's BMM request
/// * Miner proposes and activates a sidechain
/// * Miner funds Sender's wallet
/// * Sender creates a BMM request, and broadcasts it to Miner node
pub async fn test_peer_bmm_request(
    bin_paths: BinPaths,
    file_registry: TestFileRegistry,
) -> anyhow::Result<()> {
    let (res_tx, mut res_rx) = mpsc::unbounded();
    let post_setup = PreSetup::new(bin_paths, &file_registry)?
        .setup(res_tx.clone())
        .await?;
    let _test_task: util::AbortOnDrop<()> = tokio::task::spawn({
        async move {
            let res = test_peer_bmm_request_task(post_setup).await;
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
