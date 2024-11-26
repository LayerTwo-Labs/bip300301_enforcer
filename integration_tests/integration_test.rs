#![allow(clippy::print_stderr, clippy::print_stdout)]

use std::{ffi::OsStr, future::Future, process::Stdio, task::Poll, time::Duration};

use bitcoin::{Address, Transaction, TxOut};
use futures::{
    future::BoxFuture, poll, stream::FuturesUnordered, FutureExt as _, StreamExt as _, TryStreamExt,
};
use temp_dir::TempDir;
use tokio::{process::Command, time::sleep};

use bip300301_enforcer_lib::{
    proto::{
        self,
        common::{ConsensusHex, Hex, ReverseHex},
        mainchain::{
            block_info, validator_service_client::ValidatorServiceClient,
            wallet_service_client::WalletServiceClient, withdrawal_bundle_event,
            BroadcastWithdrawalBundleRequest, BroadcastWithdrawalBundleResponse,
            CreateDepositTransactionRequest, CreateSidechainProposalRequest, GenerateBlocksRequest,
            GenerateBlocksResponse, GetSidechainProposalsRequest, GetSidechainsRequest,
            SubscribeEventsRequest,
        },
    },
    types::{BlindedM6, M6id},
};
use tokio_stream::wrappers::IntervalStream;

use crate::BinPaths;

trait CommandExt {
    async fn run(&mut self) -> anyhow::Result<Vec<u8>>;

    // capture as utf8
    async fn run_utf8(&mut self) -> anyhow::Result<String> {
        let bytes = self.run().await?;
        let mut res = String::from_utf8(bytes)?;
        res = res.trim().to_owned();
        Ok(res)
    }
}

impl CommandExt for Command {
    async fn run(&mut self) -> anyhow::Result<Vec<u8>> {
        let output = self.output().await?;
        if output.status.success() {
            return Ok(output.stdout);
        }
        match String::from_utf8(output.stderr) {
            Ok(err_msg) => Err(anyhow::anyhow!("Command failed with error: `{err_msg}`")),
            Err(err) => {
                let stderr_hex = hex::encode(err.into_bytes());
                Err(anyhow::anyhow!(
                    "Command failed with stderr hex: `{stderr_hex}`"
                ))
            }
        }
    }
}

fn bitcoin_cli_command<Cmd, CmdArg, Subcommand, SubcommandArg, CmdArgs, SubcommandArgs>(
    command: Cmd,
    command_args: CmdArgs,
    subcommand: Subcommand,
    subcommand_args: SubcommandArgs,
) -> Command
where
    Cmd: AsRef<OsStr>,
    CmdArg: AsRef<OsStr>,
    Subcommand: AsRef<OsStr>,
    SubcommandArg: AsRef<OsStr>,
    CmdArgs: IntoIterator<Item = CmdArg>,
    SubcommandArgs: IntoIterator<Item = SubcommandArg>,
{
    let mut command = Command::new(command);
    command.args([
        "-chain=regtest",
        "-rpcuser=drivechain",
        "-rpcpassword=integrationtesting",
        "-rpcport=18443",
    ]);
    command.args(command_args);
    command.arg(subcommand);
    command.args(subcommand_args);
    command
}

/// Run command with args, dumping stderr/stdout to `dir`` on exit,
/// and leaking `dir`
fn await_command_with_args<Cmd, Env, Arg, Envs, Args>(
    dir: TempDir,
    command: Cmd,
    envs: Envs,
    args: Args,
) -> impl Future<Output = anyhow::Error> + 'static
where
    Cmd: AsRef<OsStr>,
    Arg: AsRef<OsStr>,
    Env: AsRef<OsStr>,
    Envs: IntoIterator<Item = (Env, Env)>,
    Args: IntoIterator<Item = Arg>,
{
    let mut cmd = Command::new(command.as_ref());
    cmd.envs(envs);
    cmd.args(args);
    cmd.kill_on_drop(true);
    let command: String = command.as_ref().to_string_lossy().to_string();
    async move {
        let stderr_file = match std::fs::File::create_new(dir.path().join("stderr.txt")) {
            Ok(stderr_file) => stderr_file,
            Err(err) => {
                let err = anyhow::Error::from(err);
                return anyhow::anyhow!(
                    "error creating stderr file for command `{command}`: `{err:#}`"
                );
            }
        };
        cmd.stderr(Stdio::from(stderr_file));
        let stdout_file = match std::fs::File::create_new(dir.path().join("stdout.txt")) {
            Ok(stdout_file) => stdout_file,
            Err(err) => {
                let err = anyhow::Error::from(err);
                return anyhow::anyhow!(
                    "error creating stdout file for command `{command}`: `{err:#}`"
                );
            }
        };
        cmd.stdout(Stdio::from(stdout_file));
        let mut cmd = match cmd.spawn() {
            Ok(cmd) => cmd,
            Err(err) => {
                let err = anyhow::Error::from(err);
                return anyhow::anyhow!("Spawning command {command} failed: `{err:#}`");
            }
        };
        let exit_status = match cmd.wait().await {
            Ok(exit_status) => exit_status,
            Err(err) => {
                let err = anyhow::Error::from(err);
                return anyhow::anyhow!("Command {command} failed: `{err:#}`",);
            }
        };
        eprintln!(
            "Command `{command}` exited with status `{}`; Leaking dir `{}`",
            exit_status,
            dir.path().display(),
        );
        dir.leak();
        anyhow::anyhow!("Command `{command}` failed")
    }
}

type Transport = tonic::transport::Channel;

struct PostSetup {
    processes: FuturesUnordered<BoxFuture<'static, anyhow::Error>>,
    validator_service_client: ValidatorServiceClient<Transport>,
    wallet_service_client: WalletServiceClient<Transport>,
    receive_address: Address,
}

async fn setup(bin_paths: &BinPaths) -> anyhow::Result<PostSetup> {
    println!("Running setup");
    let bitcoin_dir = TempDir::new()?;
    println!("Bitcoin dir: {}", bitcoin_dir.path().display());
    let electrs_dir = TempDir::new()?;
    let electrs_conf = electrs_dir.path().join("config.toml");
    std::fs::write(&electrs_conf, "auth = \"drivechain:integrationtesting\"")?;
    println!("Electrs dir: {}", electrs_dir.path().display());
    let enforcer_dir = TempDir::new()?;
    println!("Enforcer dir: {}", enforcer_dir.path().display());
    println!("Starting bitcoin node");
    let bitcoin_datadir_arg = format!("-datadir={}", bitcoin_dir.path().display());
    let electrs_daemon_dir = format!("{}", bitcoin_dir.path().join("path").display());
    let bitcoind = await_command_with_args::<_, String, _, _, _>(
        bitcoin_dir,
        &bin_paths.bitcoind,
        [],
        [
            "-acceptnonstdtxn",
            "-chain=regtest",
            &bitcoin_datadir_arg,
            "-rpcuser=drivechain",
            "-rpcpassword=integrationtesting",
            "-rpcport=18443",
            "-server",
            "-zmqpubsequence=tcp://127.0.0.1:28332",
        ],
    );
    let mut processes = FuturesUnordered::new();
    processes.push(bitcoind.boxed());
    if let Poll::Ready(Some(err)) = poll!(processes.next()) {
        return Err(err);
    }
    // wait for startup
    sleep(std::time::Duration::from_secs(1)).await;
    if let Poll::Ready(Some(err)) = poll!(processes.next()) {
        return Err(err);
    }
    // Create a wallet and get an address
    println!("Creating wallet");
    let _create_wallet_output = bitcoin_cli_command::<_, String, _, _, _, _>(
        &bin_paths.bitcoin_cli,
        [],
        "createwallet",
        ["integration-test"],
    )
    .run_utf8()
    .await?;
    if let Poll::Ready(Some(err)) = poll!(processes.next()) {
        return Err(err);
    }
    println!("Generating mining address");
    let mining_addr = bitcoin_cli_command::<_, _, _, String, _, _>(
        &bin_paths.bitcoin_cli,
        ["-rpcwallet=integration-test"],
        "getnewaddress",
        [],
    )
    .run_utf8()
    .await?;
    println!("Mining address: {mining_addr}");
    println!("Generating receiving address");
    let receive_address = bitcoin_cli_command::<_, _, _, String, _, _>(
        &bin_paths.bitcoin_cli,
        ["-rpcwallet=integration-test"],
        "getnewaddress",
        [],
    )
    .run_utf8()
    .await?;
    println!("Receiving address: {receive_address}");
    let receive_address = receive_address
        .parse::<Address<_>>()?
        .require_network(bitcoin::Network::Regtest)?;
    if let Poll::Ready(Some(err)) = poll!(processes.next()) {
        return Err(err);
    }
    // Start electrs
    println!("Starting electrs");
    let electrs_db_dir = format!("{}", electrs_dir.path().display());
    let electrs_conf_path = format!("{}", electrs_conf.display());
    let electrs = await_command_with_args::<_, String, _, _, _>(
        electrs_dir,
        &bin_paths.electrs,
        [],
        [
            "--db-dir",
            &electrs_db_dir,
            "--daemon-dir",
            &electrs_daemon_dir,
            "--network",
            "regtest",
            "--conf",
            &electrs_conf_path,
            "--log-filters",
            "\"DEBUG\"",
        ],
    );
    processes.push(electrs.boxed());
    if let Poll::Ready(Some(err)) = poll!(processes.next()) {
        return Err(err);
    }
    // wait for electrs to start
    sleep(std::time::Duration::from_secs(1)).await;
    if let Poll::Ready(Some(err)) = poll!(processes.next()) {
        return Err(err);
    }
    // Mine 1 block
    println!("Mining 1 block");
    let _output = bitcoin_cli_command(
        &bin_paths.bitcoin_cli,
        ["-rpcwallet=integration-test"],
        "generatetoaddress",
        ["1", &mining_addr],
    )
    .run_utf8()
    .await?;
    if let Poll::Ready(Some(err)) = poll!(processes.next()) {
        return Err(err);
    }
    // Start BIP300301 Enforcer
    println!("Starting bip300301_enforcer");
    let enforcer_data_dir = format!("{}", enforcer_dir.path().display());
    let enforcer = await_command_with_args(
        enforcer_dir,
        &bin_paths.bip300301_enforcer,
        [(
            "RUST_LOG",
            "h2=info,hyper_util=info,jsonrpsee-client=debug,jsonrpsee-http=debug,tonic=debug,trace",
        )],
        [
            "--data-dir",
            &enforcer_data_dir,
            "--node-rpc-user",
            "drivechain",
            "--node-rpc-pass",
            "integrationtesting",
            "--node-zmq-addr-sequence",
            "tcp://127.0.0.1:28332",
            "--enable-wallet",
            "--log-level",
            "trace",
            "--wallet-electrum-host",
            "127.0.0.1",
            "--wallet-electrum-port",
            "60401",
        ],
    );
    processes.push(enforcer.boxed());
    if let Poll::Ready(Some(err)) = poll!(processes.next()) {
        return Err(err);
    }
    // wait for enforcer to start
    sleep(std::time::Duration::from_secs(1)).await;
    if let Poll::Ready(Some(err)) = poll!(processes.next()) {
        return Err(err);
    }
    let validator_service_client =
        ValidatorServiceClient::connect("http://127.0.0.1:50051").await?;
    let wallet_service_client = WalletServiceClient::connect("http://127.0.0.1:50051").await?;
    Ok(PostSetup {
        processes,
        validator_service_client,
        wallet_service_client,
        receive_address,
    })
}

// Mine blocks, running a check after each block
async fn mine_check<F>(
    post_setup: &mut PostSetup,
    blocks: u32,
    ack_all_proposals: Option<bool>,
    mut check: F,
) -> anyhow::Result<()>
where
    F: FnMut(ReverseHex) -> anyhow::Result<()>,
{
    let request = GenerateBlocksRequest {
        blocks: Some(blocks),
        ack_all_proposals: ack_all_proposals.unwrap_or(false),
    };
    let mut stream = post_setup
        .wallet_service_client
        .generate_blocks(request)
        .await?
        .into_inner();
    if let Poll::Ready(Some(err)) = poll!(post_setup.processes.next()) {
        return Err(err);
    }
    while let Some(resp) = stream.try_next().await? {
        if let Poll::Ready(Some(err)) = poll!(post_setup.processes.next()) {
            return Err(err);
        }
        let GenerateBlocksResponse {
            block_hash: Some(block_hash),
        } = resp
        else {
            anyhow::bail!("Expected block hash")
        };
        let () = check(block_hash)?;
    }
    Ok(())
}

async fn mine(
    post_setup: &mut PostSetup,
    blocks: u32,
    ack_all_proposals: Option<bool>,
) -> anyhow::Result<()> {
    mine_check(post_setup, blocks, ack_all_proposals, |_| Ok(())).await
}

async fn propose_sidechain(post_setup: &mut PostSetup) -> anyhow::Result<()> {
    println!("Proposing sidechain");
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
            sidechain_id: Some(0),
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
    if let Poll::Ready(Some(err)) = poll!(post_setup.processes.next()) {
        return Err(err);
    }
    let () = mine(post_setup, 1, Some(true)).await?;
    if let Poll::Ready(Some(err)) = poll!(post_setup.processes.next()) {
        return Err(err);
    }
    let Some(_) = create_sidechain_proposal_resp.try_next().await? else {
        anyhow::bail!("Expected response when proposing sidechain");
    };
    println!("Proposed sidechain");
    println!("Checking sidechain proposals");
    let sidechain_proposals_resp = post_setup
        .validator_service_client
        .get_sidechain_proposals(GetSidechainProposalsRequest {})
        .await?
        .into_inner();
    if let Poll::Ready(Some(err)) = poll!(post_setup.processes.next()) {
        return Err(err);
    }
    if sidechain_proposals_resp.sidechain_proposals.len() != 1 {
        anyhow::bail!("Expected 1 sidechain proposal")
    }
    Ok(())
}

async fn activate_sidechain(post_setup: &mut PostSetup) -> anyhow::Result<()> {
    println!("Activating sidechain");
    println!("Checking that 0 sidechains are active");
    let sidechains_resp = post_setup
        .validator_service_client
        .get_sidechains(GetSidechainsRequest {})
        .await?
        .into_inner();
    if !sidechains_resp.sidechains.is_empty() {
        anyhow::bail!("unexpected sidechains resp: `{sidechains_resp:?}`")
    };
    if let Poll::Ready(Some(err)) = poll!(post_setup.processes.next()) {
        return Err(err);
    }
    let blocks_to_mine = 6;
    println!("Mining {blocks_to_mine} blocks");
    let _ = mine(post_setup, blocks_to_mine, Some(true)).await?;
    if let Poll::Ready(Some(err)) = poll!(post_setup.processes.next()) {
        return Err(err);
    }
    println!("Checking that exactly 1 sidechain is active");
    let sidechains_resp = post_setup
        .validator_service_client
        .get_sidechains(GetSidechainsRequest {})
        .await?
        .into_inner();
    if sidechains_resp.sidechains.len() != 1 {
        anyhow::bail!("Expected 1 active sidechain")
    }
    if let Poll::Ready(Some(err)) = poll!(post_setup.processes.next()) {
        return Err(err);
    }
    Ok(())
}

async fn fund_enforcer(post_setup: &mut PostSetup) -> anyhow::Result<()> {
    const BLOCKS: u32 = 100;
    let progress_bar = indicatif::ProgressBar::new(BLOCKS as u64).with_style(
        indicatif::ProgressStyle::with_template("[{bar:100}] {pos}/{len}")?.progress_chars("#>-"),
    );
    println!("Funding enforcer");
    let _ = mine_check(post_setup, 100, Some(false), |_| {
        progress_bar.inc(1);
        Ok(())
    })
    .await?;
    if let Poll::Ready(Some(err)) = poll!(post_setup.processes.next()) {
        return Err(err);
    }
    println!("Waiting for wallet sync...");
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

/// Mine blocks, and check the events for each block
async fn mine_check_block_events<F>(
    post_setup: &mut PostSetup,
    blocks: u32,
    ack_all_proposals: Option<bool>,
    mut check: F,
) -> anyhow::Result<()>
where
    F: FnMut(u32, proto::mainchain::BlockInfo) -> anyhow::Result<()>,
{
    println!("Mining {blocks} block(s)");
    let mut events = post_setup
        .validator_service_client
        .subscribe_events(SubscribeEventsRequest {
            sidechain_id: Some(0),
        })
        .await?
        .into_inner();
    if let Poll::Ready(Some(err)) = poll!(post_setup.processes.next()) {
        return Err(err);
    }
    for blocks_mined in 0..blocks {
        if let Poll::Ready(Some(err)) = poll!(post_setup.processes.next()) {
            return Err(err);
        }
        let () = mine(post_setup, 1, ack_all_proposals).await?;
        if let Poll::Ready(Some(err)) = poll!(post_setup.processes.next()) {
            return Err(err);
        }
        let Some(event) = events
            .try_next()
            .await?
            .and_then(|event| event.event)
            .and_then(|event| event.event)
        else {
            anyhow::bail!("Expected a block event")
        };
        let proto::mainchain::subscribe_events_response::event::Event::ConnectBlock(connect_block) =
            event
        else {
            anyhow::bail!("Expected connect block event")
        };
        let Some(block_info) = connect_block.block_info else {
            anyhow::bail!("Expected block info")
        };
        let () = check(blocks_mined, block_info)?;
    }
    Ok(())
}

async fn deposit(post_setup: &mut PostSetup) -> anyhow::Result<()> {
    const DEPOSIT_AMOUNT_SATS: u64 = 21_000_000;
    const DEPOSIT_FEE_SATS: u64 = 1_000_000;
    println!(
        "Creating deposit for `{}` sats, with `{}` sats fee",
        DEPOSIT_AMOUNT_SATS, DEPOSIT_FEE_SATS
    );
    let Some(deposit_txid) = post_setup
        .wallet_service_client
        .create_deposit_transaction(CreateDepositTransactionRequest {
            sidechain_id: Some(0),
            address: Some(Hex::encode(&[69; 32])),
            value_sats: Some(DEPOSIT_AMOUNT_SATS),
            fee_sats: Some(DEPOSIT_FEE_SATS),
        })
        .await?
        .into_inner()
        .txid
        .and_then(|txid| txid.hex)
    else {
        anyhow::bail!("Expected a deposit txid")
    };
    println!("Deposit TXID: {deposit_txid}");
    if let Poll::Ready(Some(err)) = poll!(post_setup.processes.next()) {
        return Err(err);
    }
    println!("Mining 1 sidechain block");
    let () = mine_check_block_events(post_setup, 1, None, |_, block_info| {
        match block_info.events.as_slice() {
            [block_info::Event {
                event: Some(block_info::event::Event::Deposit(_)),
            }] => Ok(()),
            events => anyhow::bail!("Expected deposit event, found `{events:?}`"),
        }
    })
    .await?;
    Ok(())
}

/// Construct a blinded M6 tx
fn blinded_m6<Payouts>(fee_sats: u64, payouts: Payouts) -> anyhow::Result<(Transaction, M6id)>
where
    Payouts: IntoIterator<Item = TxOut>,
{
    let fee_txout = {
        let script_pubkey = bitcoin::script::Builder::new()
            .push_opcode(bitcoin::opcodes::all::OP_RETURN)
            .push_slice(fee_sats.to_be_bytes())
            .into_script();
        bitcoin::TxOut {
            value: bitcoin::Amount::ZERO,
            script_pubkey,
        }
    };
    let outputs = Vec::from_iter(std::iter::once(fee_txout).chain(payouts));
    let tx = Transaction {
        version: bitcoin::transaction::Version::TWO,
        lock_time: bitcoin::blockdata::locktime::absolute::LockTime::ZERO,
        input: Vec::new(),
        output: outputs,
    };
    let m6id = BlindedM6::try_from(std::borrow::Cow::Borrowed(&tx))?.compute_m6id();
    Ok((tx, m6id))
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

// Create a withdrawal, and let it expire
async fn withdraw_expire(post_setup: &mut PostSetup) -> anyhow::Result<()> {
    const WITHDRAW_AMOUNT_SATS: u64 = 18_000_000;
    const WITHDRAW_FEE_SATS: u64 = 1_000_000;
    println!(
        "Creating expiring withdrawal for `{}` sats, with `{}` sats fee",
        WITHDRAW_AMOUNT_SATS, WITHDRAW_FEE_SATS,
    );
    let (withdrawal_bundle_tx, m6id) = blinded_m6(
        WITHDRAW_FEE_SATS,
        [TxOut {
            script_pubkey: post_setup.receive_address.script_pubkey(),
            value: bitcoin::Amount::from_sat(WITHDRAW_AMOUNT_SATS),
        }],
    )?;
    println!("Withdrawal M6id: {m6id}");
    let BroadcastWithdrawalBundleResponse {} = post_setup
        .wallet_service_client
        .broadcast_withdrawal_bundle(BroadcastWithdrawalBundleRequest {
            sidechain_id: Some(0),
            transaction: Some(bitcoin::consensus::serialize(&withdrawal_bundle_tx)),
        })
        .await?
        .into_inner();
    if let Poll::Ready(Some(err)) = poll!(post_setup.processes.next()) {
        return Err(err);
    }
    println!("Mining 1 block to include M3 for withdrawal bundle");
    let () = mine_check_block_events(post_setup, 1, None, |_, block_info| {
        match block_info.events.as_slice() {
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
        }
    })
    .await?;
    println!("Mining blocks until withdrawal bundle failure due to expiry");
    let () = mine_check_block_events(post_setup, 11, None, |seq, block_info| {
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
    if let Poll::Ready(Some(err)) = poll!(post_setup.processes.next()) {
        return Err(err);
    }
    Ok(())
}

// Upvote the next withdrawal bundle so that it succeeds
async fn withdraw_succeed(bin_paths: &BinPaths, post_setup: &mut PostSetup) -> anyhow::Result<()> {
    const WITHDRAW_AMOUNT_SATS: u64 = 18_000_000;
    const WITHDRAW_FEE_SATS: u64 = 1_000_000;
    println!(
        "Creating withdrawal for `{}` sats, with `{}` sats fee",
        WITHDRAW_AMOUNT_SATS, WITHDRAW_FEE_SATS,
    );
    let (withdrawal_bundle_tx, m6id) = blinded_m6(
        WITHDRAW_FEE_SATS,
        [TxOut {
            script_pubkey: post_setup.receive_address.script_pubkey(),
            value: bitcoin::Amount::from_sat(WITHDRAW_AMOUNT_SATS),
        }],
    )?;
    println!("Withdrawal M6id: {m6id}");
    let BroadcastWithdrawalBundleResponse {} = post_setup
        .wallet_service_client
        .broadcast_withdrawal_bundle(BroadcastWithdrawalBundleRequest {
            sidechain_id: Some(0),
            transaction: Some(bitcoin::consensus::serialize(&withdrawal_bundle_tx)),
        })
        .await?
        .into_inner();
    if let Poll::Ready(Some(err)) = poll!(post_setup.processes.next()) {
        return Err(err);
    }
    println!("Mining 1 block to include M3 for withdrawal bundle");
    let () = mine_check_block_events(post_setup, 1, None, |_, block_info| {
        match block_info.events.as_slice() {
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
        }
    })
    .await?;
    println!("Checking receive address balance is 0");
    let receive_addr_balance_str = bitcoin_cli_command::<_, String, _, _, _, _>(
        &bin_paths.bitcoin_cli,
        [],
        "getreceivedbyaddress",
        [post_setup.receive_address.to_string()],
    )
    .run_utf8()
    .await?;
    let receive_addr_balance =
        bitcoin::Amount::from_str_in(&receive_addr_balance_str, bitcoin::Denomination::Bitcoin)?;
    anyhow::ensure!(receive_addr_balance == bitcoin::Amount::ZERO);
    println!("Mining blocks until withdrawal success");
    let () = mine_check_block_events(post_setup, 7, Some(true), |seq, block_info| {
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
    println!("Checking receive address balance is {WITHDRAW_AMOUNT_SATS}");
    let receive_addr_balance_str = bitcoin_cli_command::<_, String, _, _, _, _>(
        &bin_paths.bitcoin_cli,
        [],
        "getreceivedbyaddress",
        [post_setup.receive_address.to_string()],
    )
    .run_utf8()
    .await?;
    let receive_addr_balance =
        bitcoin::Amount::from_str_in(&receive_addr_balance_str, bitcoin::Denomination::Bitcoin)?;
    anyhow::ensure!(receive_addr_balance.to_sat() == WITHDRAW_AMOUNT_SATS);
    Ok(())
}

pub async fn test(bin_paths: &BinPaths) -> anyhow::Result<()> {
    let mut post_setup = setup(bin_paths).await?;
    println!("Setup successfully");
    let () = propose_sidechain(&mut post_setup).await?;
    println!("Proposed sidechain successfully");
    let () = activate_sidechain(&mut post_setup).await?;
    println!("Activated sidechain successfully");
    let () = fund_enforcer(&mut post_setup).await?;
    println!("Funded enforcer successfully");
    let () = deposit(&mut post_setup).await?;
    println!("Deposited to sidechain successfully");
    let () = withdraw_expire(&mut post_setup).await?;
    println!("Withdrawal expired successfully");
    let () = withdraw_succeed(bin_paths, &mut post_setup).await?;
    println!("Withdrawal succeeded");
    Ok(())
}
