use std::{future::Future, time::Duration};

use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    proto::{
        self,
        common::{ConsensusHex, Hex},
        mainchain::{
            CreateDepositTransactionRequest, CreateDepositTransactionResponse,
            CreateSidechainProposalRequest, GetSidechainProposalsRequest, GetSidechainsRequest,
            block_info, withdrawal_bundle_event,
        },
    },
};
use bitcoin::Amount;
use futures::{StreamExt as _, TryStreamExt as _, channel::mpsc};
use tokio::time::sleep;
use tokio_stream::wrappers::IntervalStream;

use crate::{
    mine::{
        mine, mine_check_block_events, mine_gbt_check, mine_generateblocks_check, mine_signet_check,
    },
    setup::{DummySidechain, MiningMode, Mode, Network, PostSetup, Sidechain, setup},
    test_unconfirmed_transactions,
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
            let post_setup = setup(&comps.bin_paths, comps.network, comps.mode, res_tx).await?;

            // Register specific files with their own configurations
            file_registry.register_file(
                &name,
                post_setup
                    .out_dir
                    .path()
                    .join("bitcoind")
                    .join("stdout.txt"),
                FileDumpConfig::new().with_label("Bitcoin Core stdout"),
            );

            file_registry.register_file(
                &name,
                post_setup
                    .out_dir
                    .path()
                    .join("bitcoind")
                    .join("stderr.txt"),
                FileDumpConfig::new().with_label("Bitcoin Core stderr"),
            );

            file_registry.register_file(
                &name,
                post_setup
                    .out_dir
                    .path()
                    .join("enforcer")
                    .join("stdout.txt"),
                FileDumpConfig::new().with_label("Enforcer stdout"),
            );

            file_registry.register_file(
                &name,
                post_setup
                    .out_dir
                    .path()
                    .join("enforcer")
                    .join("stderr.txt"),
                FileDumpConfig::new().with_label("Enforcer stderr"),
            );

            test_fn(post_setup).await
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
        let sidechain_declaration =
            proto::mainchain::sidechain_declaration::SidechainDeclaration::V0(
                proto::mainchain::sidechain_declaration::V0 {
                    title: Some("sidechain".to_owned()),
                    description: Some("sidechain".to_owned()),
                    hash_id_1: Some(ConsensusHex::encode(&[0; 32])),
                    hash_id_2: Some(Hex::encode(&[0u8; 20])),
                },
            );
        let declaration = proto::mainchain::SidechainDeclaration {
            sidechain_declaration: Some(sidechain_declaration),
        };
        CreateSidechainProposalRequest {
            sidechain_id: Some(S::SIDECHAIN_NUMBER.0.into()),
            declaration: Some(declaration),
        }
    };
    let mut create_sidechain_proposal_resp = post_setup
        .wallet_service_client
        .create_sidechain_proposal(create_sidechain_proposal_request)
        .await?
        .into_inner();
    // Wait before mining
    sleep(std::time::Duration::from_secs(1)).await;
    tracing::debug!("Mining 1 block");
    let () = mine::<S>(post_setup, 1, Some(true)).await?;
    let Some(_) = create_sidechain_proposal_resp.try_next().await? else {
        anyhow::bail!("Expected response when proposing sidechain");
    };
    tracing::debug!("Proposed sidechain");
    tracing::debug!("Checking sidechain proposals");
    let sidechain_proposals_resp = post_setup
        .validator_service_client
        .get_sidechain_proposals(GetSidechainProposalsRequest {})
        .await?
        .into_inner();
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
        .get_sidechains(GetSidechainsRequest {})
        .await?
        .into_inner();
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
        .get_sidechains(GetSidechainsRequest {})
        .await?
        .into_inner();
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
    let () = match (post_setup.network, post_setup.mode.mining_mode()) {
        (Network::Regtest, MiningMode::GenerateBlocks) => {
            mine_generateblocks_check(post_setup, BLOCKS, Some(false), |_| {
                progress_bar.inc(1);
                Ok::<_, Infallible>(())
            })
            .await?
        }
        (Network::Regtest, MiningMode::GetBlockTemplate) => {
            mine_gbt_check::<_, Infallible, S>(post_setup, BLOCKS, |_| {
                progress_bar.inc(1);
                Ok(())
            })
            .await?
        }
        (Network::Signet, MiningMode::GetBlockTemplate) => {
            mine_signet_check::<_, Infallible, S>(post_setup, BLOCKS, |_| {
                progress_bar.inc(1);
                Ok(())
            })
            .await?;
        }
        (Network::Signet, MiningMode::GenerateBlocks) => {
            anyhow::bail!("not implemented")
        }
    };
    tracing::debug!("Waiting for wallet sync...");
    let () = wait_for_wallet_sync().await?;
    Ok(())
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
            sidechain_id: Some(S::SIDECHAIN_NUMBER.0.into()),
            address: Some(sidechain_address.to_owned()),
            value_sats: Some(deposit_amount.to_sat()),
            fee_sats: Some(deposit_fee.to_sat()),
        })
        .await?
        .into_inner()
        .txid
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
            },
        ] => Ok(()),
        events => anyhow::bail!("Expected deposit event, found `{events:?}`"),
    })
    .await?;
    let () = sidechain
        .confirm_deposit(post_setup, sidechain_address, deposit_amount, deposit_txid)
        .await?;
    Ok(())
}

// returns M6id and event
fn expect_withdrawal_bundle_event(
    event: &block_info::Event,
) -> anyhow::Result<(&ConsensusHex, &withdrawal_bundle_event::event::Event)> {
    match event {
        block_info::Event {
            event:
                Some(block_info::event::Event::WithdrawalBundle(
                    proto::mainchain::WithdrawalBundleEvent {
                        m6id: Some(event_m6id),
                        event:
                            Some(proto::mainchain::withdrawal_bundle_event::Event {
                                event: Some(event),
                            }),
                    },
                )),
        } => Ok((event_m6id, event)),
        _ => anyhow::bail!("Expected withdrawal bundle event"),
    }
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
            let withdrawal_bundle_event::event::Event::Submitted(
                withdrawal_bundle_event::event::Submitted {},
            ) = event
            else {
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
                let withdrawal_bundle_event::event::Event::Failed(
                    withdrawal_bundle_event::event::Failed {},
                ) = event
                else {
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
            let withdrawal_bundle_event::event::Event::Submitted(
                withdrawal_bundle_event::event::Submitted {},
            ) = event
            else {
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
    let () = mine_check_block_events::<_, S>(post_setup, 7, Some(true), |seq, block_info| {
        match (seq, block_info.events.as_slice()) {
            (6, [event]) => {
                let (event_m6id, event) = expect_withdrawal_bundle_event(event)?;
                let withdrawal_bundle_event::event::Event::Succeeded(
                    withdrawal_bundle_event::event::Succeeded {
                        sequence_number: _,
                        transaction: _,
                    },
                ) = event
                else {
                    anyhow::bail!("Expected withdrawal bundle success event, found `{event:?}`")
                };
                anyhow::ensure!(*event_m6id == ConsensusHex::encode(&m6id.0));
                Ok(())
            }
            (6, events) => {
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

#[allow(clippy::significant_drop_tightening, reason = "false positive")]
async fn deposit_withdraw_roundtrip_task<S>(
    post_setup: &mut PostSetup,
    res_tx: mpsc::UnboundedSender<anyhow::Result<()>>,
    sidechain_init: S::Init,
) -> anyhow::Result<()>
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
    drop(sidechain);
    Ok(())
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
    deposit_withdraw_roundtrip_task::<S>(&mut post_setup, res_tx, sidechain_init).await
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

    let peer_bmm_request_tests =
        [(Network::Regtest, Mode::Mempool)]
            .iter()
            .map(|(network, mode)| {
                new_trial_with_setup(
                    format!("peer_bmm_request (mode: {mode}, network: {network})"),
                    TestSetupComponents {
                        bin_paths: bin_paths.clone(),
                        network: *network,
                        mode: *mode,
                        file_registry: file_registry.clone(),
                        failure_collector: failure_collector.clone(),
                    },
                    crate::test_peer_bmm_request::test_peer_bmm_request,
                )
            });
    let mut async_trials = vec![];

    async_trials.extend(deposit_withdraw_roundtrip_tests);
    async_trials.extend(unconfirmed_transactions_tests);
    async_trials.extend(peer_bmm_request_tests);

    async_trials
}
