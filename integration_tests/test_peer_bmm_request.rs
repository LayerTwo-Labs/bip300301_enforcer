use std::collections::HashMap;

use bip300301_enforcer_lib::{
    bins::CommandExt,
    proto::{
        common::ConsensusHex,
        mainchain::{
            BlockHeaderInfo, CreateBmmCriticalDataTransactionRequest, CreateNewAddressRequest,
            GetBalanceRequest, GetChainTipRequest, SendTransactionRequest,
        },
    },
};
use futures::{StreamExt as _, channel::mpsc};
use tokio::time::sleep;
use tracing::Instrument as _;

use crate::{
    integration_test::{activate_sidechain, fund_enforcer, propose_sidechain},
    mine,
    setup::{BitcoindKind, DummySidechain, Mode, Network, SetupOpts, Sidechain},
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
            let enforcer_args = vec![format!(
                "--p2p-broadcast-addr=127.0.0.1:{}",
                self.miner.reserved_ports.bitcoind_listen.port()
            )];
            let setup_opts: SetupOpts = SetupOpts {
                bitcoind_args: Vec::new(),
                bitcoind_kind: BitcoindKind::Unpatched,
                enforcer_args,
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
        .create_new_address(CreateNewAddressRequest {})
        .await?
        .into_inner()
        .address;
    if post_setup
        .miner
        .wallet_service_client
        .send_transaction(SendTransactionRequest {
            destinations: HashMap::from([(sender_addr, 123_456_u64)]),
            ..Default::default()
        })
        .await?
        .into_inner()
        .txid
        .is_none()
    {
        anyhow::bail!("Failed to create a tx to fund sender wallet")
    };
    let () = crate::mine::mine::<DummySidechain>(&mut post_setup.miner, 1, None).await?;
    // Wait for sender to receive block
    sleep(std::time::Duration::from_secs(1)).await;
    let sender_balance = post_setup
        .sender
        .wallet_service_client
        .get_balance(GetBalanceRequest {})
        .await?
        .into_inner();
    anyhow::ensure!(sender_balance.confirmed_sats > 0);
    tracing::info!("Funded enforcer successfully (sender)");

    let BlockHeaderInfo {
        block_hash: tip_block_hash,
        prev_block_hash: _,
        height: tip_height,
        work: _,
        timestamp: _,
    } = post_setup
        .sender
        .validator_service_client
        .get_chain_tip(GetChainTipRequest {})
        .await?
        .into_inner()
        .block_header_info
        .ok_or_else(|| {
            anyhow::anyhow!("Expected `block_header_info field` in GetChainInfoResponse")
        })?;
    let tip_block_hash = tip_block_hash
        .ok_or_else(|| anyhow::anyhow!("Expected `block_hash field` in BlockHeaderInfo"))?;
    let sidechain_block_hash: [u8; 32] = {
        use bitcoin::hashes::Hash;
        bitcoin::hashes::sha256::Hash::hash(b"dummy sidechain block").to_byte_array()
    };
    let Some(bmm_request_txid) = post_setup
        .sender
        .wallet_service_client
        .create_bmm_critical_data_transaction(CreateBmmCriticalDataTransactionRequest {
            sidechain_id: Some(DummySidechain::SIDECHAIN_NUMBER.0.into()),
            value_sats: Some(10_000),
            height: Some(tip_height),
            critical_hash: Some(ConsensusHex::encode(&sidechain_block_hash)),
            prev_bytes: Some(tip_block_hash),
        })
        .await?
        .into_inner()
        .txid
        .and_then(|txid| txid.hex)
    else {
        anyhow::bail!("Failed to create BMM critical data tx")
    };
    tracing::info!(%bmm_request_txid, "Created BMM request tx successfully");
    // Wait for mempool inclusion / p2p broadcast.
    // This can take >5s sometimes, for unknown reasons.
    sleep(std::time::Duration::from_secs(10)).await;
    let mempool_entry = post_setup
        .miner
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "getmempoolentry", [bmm_request_txid])
        .run_utf8()
        .await?;
    tracing::debug!(%mempool_entry);
    // Mine a block and check that the BMM request worked
    let () = mine::mine_check_block_events::<_, DummySidechain>(
        &mut post_setup.miner,
        1,
        None,
        |_, block_info| {
            let bmm_commitment = block_info
                .bmm_commitment
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
    drop(post_setup.miner.tasks);
    drop(post_setup.sender.tasks);
    // Wait for tasks to die
    sleep(std::time::Duration::from_secs(1)).await;
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
