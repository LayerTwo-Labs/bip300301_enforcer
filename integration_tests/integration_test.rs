use std::{future::Future, panic::AssertUnwindSafe, time::Duration};

use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    proto::{
        self,
        common::{ConsensusHex, Hex},
        mainchain::{
            CreateDepositTransactionRequest, CreateDepositTransactionResponse,
            CreateNewAddressRequest, CreateSidechainProposalRequest, GetSidechainProposalsRequest,
            GetSidechainsRequest, ListSidechainDepositTransactionsRequest,
            ListUnspentOutputsRequest, SendTransactionRequest, block_info, withdrawal_bundle_event,
        },
    },
};
use bitcoin::Amount;
use futures::{FutureExt as _, StreamExt as _, channel::mpsc};
use tokio::time::sleep;
use tokio_stream::wrappers::IntervalStream;
use tracing::Instrument as _;

use crate::{
    mine::{mine, mine_check_block_events, mine_signet_check},
    setup::{
        Directories, DummySidechain, MiningMode, Mode, Network, PostSetup, PreSetup, Sidechain,
    },
    test_peer_bmm_request, test_unconfirmed_transactions,
    util::{AsyncTrial, BinPaths, FileDumpConfig, TestFailureCollector, TestFileRegistry},
};

type TestFuture = std::pin::Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>;
type TestTrial = AsyncTrial<TestFuture>;

struct TestSetupComponents {
    bin_paths: BinPaths,
    network: Network,
    mode: Mode,
    file_registry: TestFileRegistry,
    failure_collector: TestFailureCollector,
}

fn register_files(file_registry: &TestFileRegistry, name: &str, directories: &Directories) {
    // Register specific files with their own configurations
    file_registry.register_file(
        name,
        directories.bitcoin_dir.join("stdout.txt"),
        FileDumpConfig::new().with_label("Bitcoin Core stdout"),
    );

    file_registry.register_file(
        name,
        directories.bitcoin_dir.join("stderr.txt"),
        FileDumpConfig::new().with_label("Bitcoin Core stderr"),
    );

    file_registry.register_file(
        name,
        directories.enforcer_dir.join("stdout.txt"),
        FileDumpConfig::new().with_label("Enforcer stdout"),
    );

    file_registry.register_file(
        name,
        directories.enforcer_dir.join("stderr.txt"),
        FileDumpConfig::new().with_label("Enforcer stderr"),
    );
}

async fn catch_unwind<Fut>(test_future: Fut) -> anyhow::Result<()>
where
    Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    match AssertUnwindSafe(test_future).catch_unwind().await {
        Ok(result) => result,
        Err(panic_payload) => {
            // Convert panic to an error
            let panic_msg = if let Some(s) = panic_payload.downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = panic_payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "Unknown panic".to_string()
            };
            Err(anyhow::anyhow!("Test panicked: {panic_msg}"))
        }
    }
}

fn new_trial<F, Fut>(name: String, comps: TestSetupComponents, test_fn: F) -> TestTrial
where
    F: FnOnce(PreSetup) -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let file_registry = comps.file_registry.clone();
    AsyncTrial::new(
        name.clone(),
        Box::pin(async move {
            let pre_setup = PreSetup::new(comps.bin_paths.clone(), comps.network)?;

            register_files(&file_registry, &name, &pre_setup.directories);

            let test_future =
                test_fn(pre_setup).instrument(tracing::info_span!("test", name = %name));

            catch_unwind(test_future).await
        }),
        comps.file_registry,
        comps.failure_collector,
    )
}

fn new_trial_with_setup<F, Fut>(name: String, comps: TestSetupComponents, test_fn: F) -> TestTrial
where
    F: FnOnce(PostSetup) -> Fut + Send + 'static,
    Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let file_registry = comps.file_registry.clone();
    AsyncTrial::new(
        name.clone(),
        Box::pin(async move {
            let (res_tx, _) = mpsc::unbounded();
            let pre_setup = PreSetup::new(comps.bin_paths.clone(), comps.network)?;
            register_files(&file_registry, &name, &pre_setup.directories);
            let setup_opts: crate::setup::SetupOpts = Default::default();
            let post_setup = pre_setup.setup(comps.mode, setup_opts, res_tx).await?;

            let test_future =
                test_fn(post_setup).instrument(tracing::info_span!("test", name = %name));

            catch_unwind(test_future).await
        }) as TestFuture,
        comps.file_registry,
        comps.failure_collector,
    )
}

pub async fn propose_sidechain<S>(post_setup: &mut PostSetup) -> anyhow::Result<()>
where
    S: Sidechain,
{
    tracing::info!("Proposing sidechain");
    let create_sidechain_proposal_request = {
        let v0 = proto::mainchain::sidechain_declaration::V0 {
            title: proto::wrap_string("sidechain"),
            description: proto::wrap_string("sidechain"),
            hash_id_1: buffa::MessageField::some(ConsensusHex::encode(&[0; 32])),
            hash_id_2: buffa::MessageField::some(Hex::encode(&[0u8; 20])),
        };
        let declaration = proto::mainchain::SidechainDeclaration {
            sidechain_declaration: Some(v0.into()),
        };
        CreateSidechainProposalRequest {
            sidechain_id: proto::wrap_u32(S::SIDECHAIN_NUMBER.0.into()),
            declaration: buffa::MessageField::some(declaration),
        }
    };
    let mut create_sidechain_proposal_resp = post_setup
        .wallet_service_client
        .create_sidechain_proposal(create_sidechain_proposal_request)
        .await?;
    // Wait before mining
    sleep(std::time::Duration::from_secs(1)).await;
    tracing::debug!("Mining 1 block");
    let () = mine::<S>(post_setup, 1, Some(true)).await?;
    let Some(_) = create_sidechain_proposal_resp.message().await? else {
        anyhow::bail!("Expected response when proposing sidechain");
    };
    tracing::debug!("Proposed sidechain");
    tracing::debug!("Checking sidechain proposals");
    let sidechain_proposals_resp = post_setup
        .validator_service_client
        .get_sidechain_proposals(GetSidechainProposalsRequest::default())
        .await?
        .into_owned();
    if sidechain_proposals_resp.sidechain_proposals.len() != 1 {
        anyhow::bail!("Expected 1 sidechain proposal")
    }
    Ok(())
}

pub async fn activate_sidechain<S>(post_setup: &mut PostSetup) -> anyhow::Result<()>
where
    S: Sidechain,
{
    tracing::info!("Activating sidechain");
    tracing::debug!("Checking that 0 sidechains are active");
    let sidechains_resp = post_setup
        .validator_service_client
        .get_sidechains(GetSidechainsRequest::default())
        .await?
        .into_owned();
    if !sidechains_resp.sidechains.is_empty() {
        anyhow::bail!("unexpected sidechains resp: `{sidechains_resp:?}`")
    };
    let blocks_to_mine = 6;
    tracing::debug!("Mining {blocks_to_mine} blocks");
    let _ = mine_check_block_events::<_, S>(post_setup, blocks_to_mine, Some(true), |_, _| Ok(()))
        .await?;
    tracing::debug!("Checking that exactly 1 sidechain is active");
    let sidechains_resp = post_setup
        .validator_service_client
        .get_sidechains(GetSidechainsRequest::default())
        .await?
        .into_owned();
    if sidechains_resp.sidechains.len() != 1 {
        anyhow::bail!("Expected 1 active sidechain")
    }
    Ok(())
}

pub async fn wait_for_wallet_sync() -> anyhow::Result<()> {
    // Wait 15s for a re-sync
    const WAIT: Duration = Duration::from_secs(15);
    let progress_bar = indicatif::ProgressBar::new(WAIT.as_secs()).with_style(
        indicatif::ProgressStyle::with_template(&format!(
            "[{{bar:15}}] {{elapsed}}/{}",
            indicatif::HumanDuration(WAIT)
        ))?
        .progress_chars("%%-"),
    );
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.tick().await;
    let () = progress_bar
        .wrap_stream(IntervalStream::new(interval))
        .map(|_| ())
        .take(WAIT.as_secs() as usize)
        .collect()
        .await;
    Ok(())
}

pub async fn fund_enforcer<S>(post_setup: &mut PostSetup) -> anyhow::Result<()>
where
    S: Sidechain,
{
    use std::convert::Infallible;
    const BLOCKS: u32 = 100;
    let progress_bar = indicatif::ProgressBar::new(BLOCKS as u64).with_style(
        indicatif::ProgressStyle::with_template("[{bar:100}] {pos}/{len}")?.progress_chars("#>-"),
    );
    tracing::info!("Funding enforcer");
    let () = match post_setup.network {
        Network::Regtest => {
            let address = post_setup
                .wallet_service_client
                .create_new_address(CreateNewAddressRequest::default())
                .await?
                .into_owned()
                .address;

            post_setup
                .bitcoin_cli
                .command::<String, _, _, _, _>(
                    [],
                    "generatetoaddress",
                    [BLOCKS.to_string(), address],
                )
                .run_utf8()
                .await?;
        }
        Network::Signet => {
            mine_signet_check::<_, Infallible, S>(post_setup, BLOCKS, |_| {
                progress_bar.inc(1);
                Ok(())
            })
            .await?;
        }
    };
    tracing::debug!("Waiting for wallet sync...");
    let () = wait_for_wallet_sync().await?;
    Ok(())
}

/// Number of unspent outputs (confirmed + unconfirmed) in the enforcer wallet.
pub async fn unspent_output_count(post_setup: &mut PostSetup) -> anyhow::Result<usize> {
    let utxos = post_setup
        .wallet_service_client
        .list_unspent_outputs(ListUnspentOutputsRequest {})
        .await?
        .into_owned();
    Ok(utxos.outputs.len())
}

/// Mine `blocks` to Bitcoin Core's own address (not the enforcer wallet), so
/// the coinbases do not add new UTXOs to the enforcer wallet.
pub async fn mine_to_core(post_setup: &mut PostSetup, blocks: u32) -> anyhow::Result<()> {
    let core_address = post_setup.receive_address.to_string();
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "generatetoaddress", [blocks.to_string(), core_address])
        .run_utf8()
        .await?;
    Ok(())
}

/// Collapse the entire wallet into a single confirmed UTXO. With one funding
/// UTXO, two deposits created without a block in between are forced into a
/// parent→child chain: the second can only be funded from the first's change.
pub async fn consolidate_to_single_utxo(post_setup: &mut PostSetup) -> anyhow::Result<()> {
    // The funding coinbases are mostly immature (coinbase maturity is 100
    // blocks), so a drain can only sweep the few mature ones. Advance the
    // chain so every funding coinbase matures and a single drain can sweep the
    // whole wallet.
    let () = mine_to_core(post_setup, 100).await?;
    let () = wait_for_wallet_sync().await?;

    for _ in 0..6 {
        if unspent_output_count(post_setup).await? <= 1 {
            return Ok(());
        }
        let drain_address = post_setup
            .wallet_service_client
            .create_new_address(CreateNewAddressRequest {})
            .await?
            .into_owned()
            .address;
        let _drain = post_setup
            .wallet_service_client
            .send_transaction(SendTransactionRequest {
                drain_wallet_to: Some(drain_address),
                ..Default::default()
            })
            .await?;
        sleep(std::time::Duration::from_secs(1)).await;
        let () = mine_to_core(post_setup, 1).await?;
        // Poll for the enforcer wallet to ingest the confirmed drain.
        for _ in 0..10 {
            sleep(std::time::Duration::from_secs(2)).await;
            if unspent_output_count(post_setup).await? == 1 {
                return Ok(());
            }
        }
    }
    anyhow::bail!(
        "failed to consolidate wallet to a single UTXO, still have {}",
        unspent_output_count(post_setup).await?
    )
}

const DEPOSIT_AMOUNT: bitcoin::Amount = bitcoin::Amount::from_sat(21_000_000);
const DEPOSIT_FEE: bitcoin::Amount = bitcoin::Amount::from_sat(1_000_000);

pub async fn deposit<S>(
    post_setup: &mut PostSetup,
    sidechain: &mut S,
    sidechain_address: &str,
    deposit_amount: bitcoin::Amount,
    deposit_fee: bitcoin::Amount,
) -> anyhow::Result<()>
where
    S: Sidechain,
{
    tracing::info!(
        deposit_amount = %deposit_amount.display_dynamic(),
        deposit_fee = %deposit_fee.display_dynamic(),
        "Creating deposit",
    );
    let deposit_txid: bitcoin::Txid = post_setup
        .wallet_service_client
        .create_deposit_transaction(CreateDepositTransactionRequest {
            sidechain_id: proto::wrap_u32(S::SIDECHAIN_NUMBER.0.into()),
            address: proto::wrap_string(sidechain_address.to_owned()),
            value_sats: proto::wrap_u64(deposit_amount.to_sat()),
            fee_sats: proto::wrap_u64(deposit_fee.to_sat()),
        })
        .await?
        .into_owned()
        .txid
        .into_option()
        .ok_or_else(|| proto::Error::missing_field::<CreateDepositTransactionResponse>("txid"))?
        .decode::<CreateDepositTransactionResponse, _>("txid")?;
    tracing::debug!("Deposit TXID: {deposit_txid}");
    // Wait for deposit tx to enter mempool
    sleep(std::time::Duration::from_secs(1)).await;
    tracing::debug!("Mining 1 sidechain block");
    let () = mine_check_block_events::<_, S>(post_setup, 1, None, |_, block_info| match block_info
        .events
        .as_slice()
    {
        [
            block_info::Event {
                event: Some(block_info::event::Event::Deposit(_)),
                ..
            },
        ] => Ok(()),
        events => anyhow::bail!("Expected deposit event, found `{events:?}`"),
    })
    .await?;
    let () = sidechain
        .confirm_deposit(post_setup, sidechain_address, deposit_amount, deposit_txid)
        .await?;
    // Listing deposits must succeed even for a sidechain's first deposit, whose
    // treasury UTXO is stored at sequence number 0. This is a regression test
    // for the `seq - 1` underflow that made this RPC fail with
    // "Missing value from db active_sidechain_slot_sequence_to_treasury_utxo".
    let deposits = post_setup
        .wallet_service_client
        .list_sidechain_deposit_transactions(ListSidechainDepositTransactionsRequest {})
        .await?
        .into_owned()
        .transactions;
    anyhow::ensure!(
        !deposits.is_empty(),
        "expected the deposit just made to be listed, found none",
    );
    Ok(())
}

// returns M6id and event
fn expect_withdrawal_bundle_event(
    event: &block_info::Event,
) -> anyhow::Result<(&ConsensusHex, &withdrawal_bundle_event::event::Event)> {
    let block_info::event::Event::WithdrawalBundle(wbe) = event
        .event
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("block_info::Event missing inner event"))?
    else {
        anyhow::bail!("Expected withdrawal bundle event");
    };
    let event_m6id = wbe
        .m6id
        .as_option()
        .ok_or_else(|| anyhow::anyhow!("withdrawal bundle event missing m6id"))?;
    let inner_event = wbe
        .event
        .as_option()
        .ok_or_else(|| anyhow::anyhow!("withdrawal bundle event missing event wrapper"))?
        .event
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("withdrawal bundle event missing oneof"))?;
    Ok((event_m6id, inner_event))
}

const WITHDRAW_AMOUNT_0: Amount = Amount::from_sat(18_000_000);
const WITHDRAW_FEE_0: Amount = Amount::from_sat(1_000_000);
const WITHDRAW_AMOUNT_1: Amount = Amount::from_sat(18_000_000);
const WITHDRAW_FEE_1: Amount = Amount::from_sat(1_000_000);

// Create a withdrawal, and let it expire
async fn withdraw_expire<S>(
    post_setup: &mut PostSetup,
    sidechain: &mut S,
    withdraw_amount: Amount,
    withdraw_fee: Amount,
) -> anyhow::Result<()>
where
    S: Sidechain,
{
    tracing::info!(
        value = %withdraw_amount.display_dynamic(),
        fee = %withdraw_fee.display_dynamic(),
        "Creating expiring withdrawal"
    );
    let receive_address = post_setup.receive_address.clone();
    let m6id = sidechain
        .create_withdrawal(
            post_setup,
            &receive_address,
            WITHDRAW_AMOUNT_0,
            WITHDRAW_FEE_0,
        )
        .await?;
    tracing::debug!("Mining 1 block to include M3 for withdrawal bundle");
    let () = mine_check_block_events::<_, S>(post_setup, 1, None, |_, block_info| match block_info
        .events
        .as_slice()
    {
        [event] => {
            let (event_m6id, event) = expect_withdrawal_bundle_event(event)?;
            let withdrawal_bundle_event::event::Event::Submitted(_) = event else {
                anyhow::bail!("Expected withdrawal bundle submitted event, found `{event:?}`")
            };
            anyhow::ensure!(*event_m6id == ConsensusHex::encode(&m6id.0));
            Ok(())
        }
        events => {
            anyhow::bail!("Expected withdrawal bundle submitted event, found `{events:?}`")
        }
    })
    .await?;
    tracing::debug!("Mining blocks until withdrawal bundle failure due to expiry");
    let () = mine_check_block_events::<_, S>(post_setup, 11, None, |seq, block_info| {
        match (seq, block_info.events.as_slice()) {
            (10, [event]) => {
                let (event_m6id, event) = expect_withdrawal_bundle_event(event)?;
                let withdrawal_bundle_event::event::Event::Failed(_) = event else {
                    anyhow::bail!("Expected withdrawal bundle failed event, found `{event:?}`")
                };
                anyhow::ensure!(*event_m6id == ConsensusHex::encode(&m6id.0));
                Ok(())
            }
            (10, events) => {
                anyhow::bail!("Expected withdrawal bundle failed event, found `{events:?}`")
            }
            (_, []) => Ok(()),
            (_, events) => anyhow::bail!("Expected no events, found `{events:?}`"),
        }
    })
    .await?;
    Ok(())
}

// Upvote the next withdrawal bundle so that it succeeds
pub async fn withdraw_succeed<S>(
    post_setup: &mut PostSetup,
    sidechain: &mut S,
    withdraw_amount: Amount,
    withdraw_fee: Amount,
    pending_withdrawal_value: Amount,
) -> anyhow::Result<()>
where
    S: Sidechain,
{
    tracing::info!(
        value = %withdraw_amount.display_dynamic(),
        fee = %withdraw_fee.display_dynamic(),
        "Creating withdrawal"
    );
    let receive_address = post_setup.receive_address.clone();
    let m6id = sidechain
        .create_withdrawal(post_setup, &receive_address, withdraw_amount, withdraw_fee)
        .await?;
    tracing::debug!("Mining 1 block to include M3 for withdrawal bundle");
    let () = mine_check_block_events::<_, S>(post_setup, 1, None, |_, block_info| match block_info
        .events
        .as_slice()
    {
        [event] => {
            let (event_m6id, event) = expect_withdrawal_bundle_event(event)?;
            let withdrawal_bundle_event::event::Event::Submitted(_) = event else {
                anyhow::bail!("Expected withdrawal bundle submitted event, found `{event:?}`")
            };
            anyhow::ensure!(*event_m6id == ConsensusHex::encode(&m6id.0));
            Ok(())
        }
        events => {
            anyhow::bail!("Expected withdrawal bundle submitted event, found `{events:?}`")
        }
    })
    .await?;
    tracing::debug!("Checking receive address balance is 0");
    let receive_addr_balance_str = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "getreceivedbyaddress",
            [post_setup.receive_address.to_string()],
        )
        .run_utf8()
        .await?;
    let receive_addr_balance =
        bitcoin::Amount::from_str_in(&receive_addr_balance_str, bitcoin::Denomination::Bitcoin)?;
    anyhow::ensure!(receive_addr_balance == bitcoin::Amount::ZERO);
    tracing::debug!("Mining blocks until withdrawal success");
    let () = mine_check_block_events::<_, S>(post_setup, 6, Some(true), |seq, block_info| {
        match (seq, block_info.events.as_slice()) {
            (5, [event]) => {
                let (event_m6id, event) = expect_withdrawal_bundle_event(event)?;
                let withdrawal_bundle_event::event::Event::Succeeded(_) = event else {
                    anyhow::bail!("Expected withdrawal bundle success event, found `{event:?}`")
                };
                anyhow::ensure!(*event_m6id == ConsensusHex::encode(&m6id.0));
                Ok(())
            }
            (5, events) => {
                anyhow::bail!("Expected withdrawal bundle success event, found `{events:?}`")
            }
            (_, []) => Ok(()),
            (_, events) => anyhow::bail!("Expected no events, found `{events:?}`"),
        }
    })
    .await?;
    let expected_withdrawal_value = pending_withdrawal_value + withdraw_amount;
    tracing::debug!(
        expected = %expected_withdrawal_value.display_dynamic(),
        "Checking receive address balance"
    );
    let receive_addr_balance_str = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "getreceivedbyaddress",
            [post_setup.receive_address.to_string()],
        )
        .run_utf8()
        .await?;
    let receive_addr_balance =
        bitcoin::Amount::from_str_in(&receive_addr_balance_str, bitcoin::Denomination::Bitcoin)?;
    anyhow::ensure!(receive_addr_balance == expected_withdrawal_value);
    Ok(())
}

pub async fn deposit_withdraw_roundtrip_task<S>(
    post_setup: &mut PostSetup,
    res_tx: mpsc::UnboundedSender<anyhow::Result<()>>,
    sidechain_init: S::Init,
) -> anyhow::Result<S>
where
    S: Sidechain,
{
    let mut sidechain = S::setup(sidechain_init, post_setup, res_tx).await?;
    tracing::info!("Setup successfully");
    let () = propose_sidechain::<S>(post_setup).await?;
    tracing::info!("Proposed sidechain successfully");
    let () = activate_sidechain::<S>(post_setup).await?;
    tracing::info!("Activated sidechain successfully");
    let () = fund_enforcer::<S>(post_setup).await?;
    tracing::info!("Funded enforcer successfully");
    let deposit_address = sidechain.get_deposit_address().await?;
    let () = deposit(
        post_setup,
        &mut sidechain,
        &deposit_address,
        DEPOSIT_AMOUNT,
        DEPOSIT_FEE,
    )
    .await?;
    tracing::info!("Deposited to sidechain successfully");
    // Wait for mempool to catch up before attempting second deposit
    tracing::debug!("Waiting for wallet sync...");
    let () = wait_for_wallet_sync().await?;
    tracing::info!("Attempting second deposit");
    let () = deposit(
        post_setup,
        &mut sidechain,
        &deposit_address,
        DEPOSIT_AMOUNT,
        DEPOSIT_FEE,
    )
    .await?;
    tracing::info!("Deposited to sidechain successfully");
    let pending_withdrawal_value = match post_setup.mode.mining_mode() {
        MiningMode::GenerateBlocks => {
            let () = withdraw_expire(
                post_setup,
                &mut sidechain,
                WITHDRAW_AMOUNT_0,
                WITHDRAW_FEE_0,
            )
            .await?;
            tracing::info!("Withdrawal expired successfully");
            WITHDRAW_AMOUNT_0
        }
        MiningMode::GetBlockTemplate => Amount::ZERO,
    };
    let () = withdraw_succeed(
        post_setup,
        &mut sidechain,
        WITHDRAW_AMOUNT_1,
        WITHDRAW_FEE_1,
        pending_withdrawal_value,
    )
    .await?;
    tracing::info!("Withdrawal succeeded");
    Ok(sidechain)
}

/// Test a deposit-withdraw round-trip.
/// * Proposes and activates a sidechain
/// * Creates two deposits
/// * If mode is not GBT, creates a withdrawal that will be allowed to expire
/// * Creates and handles a withdrawal
pub async fn deposit_withdraw_roundtrip<S>(
    mut post_setup: PostSetup,
    sidechain_init: S::Init,
) -> anyhow::Result<()>
where
    S: Sidechain + Send,
    S::Init: Send + 'static,
{
    let (res_tx, _) = mpsc::unbounded();
    let _sidechain: S =
        deposit_withdraw_roundtrip_task::<S>(&mut post_setup, res_tx, sidechain_init).await?;
    Ok(())
}

pub fn tests(
    bin_paths: &BinPaths,
    file_registry: TestFileRegistry,
    failure_collector: TestFailureCollector,
) -> Vec<TestTrial> {
    let deposit_withdraw_roundtrip_tests = [
        (Network::Regtest, Mode::GetBlockTemplate),
        (Network::Regtest, Mode::Mempool),
        (Network::Regtest, Mode::NoMempool),
        (Network::Signet, Mode::GetBlockTemplate),
    ]
    .iter()
    .map(|(network, mode)| {
        new_trial_with_setup(
            format!("deposit_withdraw_roundtrip (mode: {mode}, network: {network})"),
            TestSetupComponents {
                bin_paths: bin_paths.clone(),
                network: *network,
                mode: *mode,
                file_registry: file_registry.clone(),
                failure_collector: failure_collector.clone(),
            },
            |post_setup| deposit_withdraw_roundtrip::<DummySidechain>(post_setup, ()),
        )
    });

    // TODO: add a signet test here?
    let unconfirmed_transactions_tests =
        [(Network::Regtest, Mode::Mempool)]
            .iter()
            .map(|(network, mode)| {
                new_trial_with_setup(
                    format!("unconfirmed_transactions (mode: {mode}, network: {network})"),
                    TestSetupComponents {
                        bin_paths: bin_paths.clone(),
                        network: *network,
                        mode: *mode,
                        file_registry: file_registry.clone(),
                        failure_collector: failure_collector.clone(),
                    },
                    test_unconfirmed_transactions::test_unconfirmed_transactions,
                )
            });

    let peer_bmm_request_trial: TestTrial = {
        let name = test_peer_bmm_request::TEST_NAME;
        AsyncTrial::new(
            name,
            Box::pin({
                let bin_paths = bin_paths.clone();
                let file_registry = file_registry.clone();
                async move {
                    let test_future =
                        test_peer_bmm_request::test_peer_bmm_request(bin_paths, file_registry)
                            .instrument(tracing::info_span!("test", name = %name));
                    catch_unwind(test_future).await
                }
            }),
            file_registry.clone(),
            failure_collector.clone(),
        )
    };
    let mut async_trials = vec![];

    async_trials.extend(deposit_withdraw_roundtrip_tests);
    async_trials.extend(unconfirmed_transactions_tests);
    async_trials.push(peer_bmm_request_trial);
    async_trials.extend([new_trial(
        "file_based_block_parser".to_string(),
        TestSetupComponents {
            bin_paths: bin_paths.clone(),
            network: Network::Regtest,
            mode: Mode::Mempool,
            file_registry: file_registry.clone(),
            failure_collector: failure_collector.clone(),
        },
        crate::test_file_based_block_parser::test_file_based_block_parser,
    )]);
    async_trials.push(new_trial_with_setup(
        "invalid_block".to_string(),
        TestSetupComponents {
            bin_paths: bin_paths.clone(),
            network: Network::Regtest,
            mode: Mode::NoMempool,
            file_registry: file_registry.clone(),
            failure_collector: failure_collector.clone(),
        },
        crate::test_invalid_block::test_invalid_block,
    ));
    async_trials.push(new_trial_with_setup(
        "inactive_slot_drivechain_output".to_string(),
        TestSetupComponents {
            bin_paths: bin_paths.clone(),
            network: Network::Regtest,
            mode: Mode::NoMempool,
            file_registry: file_registry.clone(),
            failure_collector: failure_collector.clone(),
        },
        crate::test_inactive_drivechain_output::test_inactive_slot_drivechain_output,
    ));
    async_trials.push(new_trial_with_setup(
        "competing_unconfirmed_deposits".to_string(),
        TestSetupComponents {
            bin_paths: bin_paths.clone(),
            network: Network::Regtest,
            mode: Mode::GetBlockTemplate,
            file_registry: file_registry.clone(),
            failure_collector: failure_collector.clone(),
        },
        crate::test_competing_deposits::test_competing_unconfirmed_deposits,
    ));
    async_trials.push(new_trial_with_setup(
        "consecutive_deposits".to_string(),
        TestSetupComponents {
            bin_paths: bin_paths.clone(),
            network: Network::Regtest,
            mode: Mode::NoMempool,
            file_registry: file_registry.clone(),
            failure_collector: failure_collector.clone(),
        },
        crate::test_consecutive_deposits::test_consecutive_deposits,
    ));

    async_trials
}
