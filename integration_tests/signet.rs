use std::{future::Future, task::Poll, time::Duration};

use bip300301_enforcer_lib::{
    bins,
    proto::{
        self,
        common::{ConsensusHex, Hex, ReverseHex},
        mainchain::{
            block_info, validator_service_client::ValidatorServiceClient,
            wallet_service_client::WalletServiceClient, withdrawal_bundle_event,
            BroadcastWithdrawalBundleRequest, BroadcastWithdrawalBundleResponse,
            CreateDepositTransactionRequest, CreateSidechainProposalRequest,
            GetSidechainProposalsRequest, GetSidechainsRequest, SubscribeEventsRequest,
        },
    },
    types::{BlindedM6, M6id},
};
use bitcoin::{
    hashes::{Hash as _, HashEngine},
    Address, Transaction, TxOut,
};
use futures::{
    future::BoxFuture, poll, stream::FuturesUnordered, FutureExt as _, StreamExt as _,
    TryStreamExt as _,
};
use reserve_port::ReservedPort;
use temp_dir::TempDir;
use tokio::time::sleep;
use tokio_stream::wrappers::IntervalStream;

use crate::util::{self, drop_temp_dir, AsyncTrial, BinPaths, CommandExt as _};

#[derive(Debug)]
struct ReservedPorts {
    bitcoind_listen: ReservedPort,
    bitcoind_rpc: ReservedPort,
    bitcoind_zmq_sequence: ReservedPort,
    electrs_electrum_rpc: ReservedPort,
    electrs_monitoring: ReservedPort,
    enforcer_serve_grpc: ReservedPort,
    enforcer_serve_rpc: ReservedPort,
}

impl ReservedPorts {
    fn new() -> Result<Self, reserve_port::Error> {
        Ok(Self {
            bitcoind_listen: ReservedPort::random()?,
            bitcoind_rpc: ReservedPort::random()?,
            bitcoind_zmq_sequence: ReservedPort::random()?,
            electrs_electrum_rpc: ReservedPort::random()?,
            electrs_monitoring: ReservedPort::random()?,
            enforcer_serve_grpc: ReservedPort::random()?,
            enforcer_serve_rpc: ReservedPort::random()?,
        })
    }
}

type Transport = tonic::transport::Channel;

struct PostSetup {
    out_dir: TempDir,
    bitcoin_cli: bins::BitcoinCli,
    processes: FuturesUnordered<BoxFuture<'static, anyhow::Error>>,
    signet_miner: bins::SignetMiner,
    validator_service_client: ValidatorServiceClient<Transport>,
    wallet_service_client: WalletServiceClient<Transport>,
    mining_address: Address,
    receive_address: Address,
    // MUST be last in order to ensure that it is dropped last
    _reserved_ports: ReservedPorts,
}

async fn setup(bin_paths: &BinPaths) -> anyhow::Result<PostSetup> {
    tracing::info!("Running setup");
    let reserved_ports = ReservedPorts::new()?;
    let secret_key = bitcoin::PrivateKey::generate(bitcoin::NetworkKind::Test);
    let (cpk, signet_challenge) = {
        let cpk = bitcoin::CompressedPublicKey::from_private_key(
            &bitcoin::secp256k1::Secp256k1::new(),
            &secret_key,
        )?;
        let signet_challenge = bitcoin::Script::builder()
            .push_opcode(bitcoin::opcodes::all::OP_PUSHNUM_1)
            .push_slice(cpk.to_bytes())
            .push_opcode(bitcoin::opcodes::all::OP_PUSHNUM_1)
            .push_opcode(bitcoin::opcodes::all::OP_CHECKMULTISIG)
            .into_script();
        (cpk, signet_challenge)
    };
    let signet_challenge_addr =
        bitcoin::Address::from_script(&cpk.p2wpkh_script_code(), &bitcoin::params::SIGNET)?;
    let mining_address =
        bitcoin::Address::from_script(&cpk.p2wpkh_script_code(), &bitcoin::params::SIGNET)?;
    let signet_magic: [u8; 4] = {
        let mut hasher = bitcoin::hashes::sha256d::Hash::engine();
        hasher.input(&[0x25]);
        hasher.input(signet_challenge.as_bytes());
        let hash = bitcoin::hashes::sha256d::Hash::from_engine(hasher);
        hash[..=3].try_into()?
    };
    tracing::info!(
        signet_challenge = %hex::encode(signet_challenge.as_bytes()),
        signet_magic = %hex::encode(signet_magic),
        %mining_address
    );
    let out_dir = TempDir::new()?;
    // leak unless explicitly allowed to cleanup
    out_dir.leak();
    tracing::info!("Output dir: {}", out_dir.path().display());
    let bitcoin_dir = out_dir.path().join("bitcoind");
    std::fs::create_dir(&bitcoin_dir)?;
    tracing::info!("Bitcoin dir: {}", bitcoin_dir.display());
    let electrs_dir = out_dir.path().join("electrs");
    std::fs::create_dir(&electrs_dir)?;
    tracing::info!("Electrs dir: {}", electrs_dir.display());
    let electrs_conf = electrs_dir.join("config.toml");
    std::fs::write(&electrs_conf, "auth = \"drivechain:integrationtesting\"")?;
    let enforcer_dir = out_dir.path().join("enforcer");
    std::fs::create_dir(&enforcer_dir)?;
    tracing::info!("Enforcer dir: {}", enforcer_dir.display());
    tracing::debug!("Starting bitcoin node");
    let bitcoind = bins::Bitcoind {
        path: bin_paths.bitcoind.clone(),
        data_dir: bitcoin_dir,
        listen_port: reserved_ports.bitcoind_listen.port(),
        network: bitcoin::Network::Signet,
        onion_ports: None,
        rpc_user: "drivechain".to_owned(),
        rpc_pass: "integrationtesting".to_owned(),
        rpc_port: reserved_ports.bitcoind_rpc.port(),
        signet_challenge: Some(signet_challenge),
        txindex: true,
        zmq_sequence_port: reserved_ports.bitcoind_zmq_sequence.port(),
    };
    let bitcoind_proc = bitcoind.await_command_with_args::<String, String, _, _>([], []);
    let mut processes = FuturesUnordered::new();
    processes.push(bitcoind_proc.boxed());
    if let Poll::Ready(Some(err)) = poll!(processes.next()) {
        return Err(err);
    }
    // wait for startup
    sleep(std::time::Duration::from_secs(1)).await;
    if let Poll::Ready(Some(err)) = poll!(processes.next()) {
        return Err(err);
    }
    // Create a wallet and get a receiving address
    let mut bitcoin_cli = bins::BitcoinCli {
        path: bin_paths.bitcoin_cli.clone(),
        network: bitcoind.network,
        rpc_user: bitcoind.rpc_user.clone(),
        rpc_pass: bitcoind.rpc_pass.clone(),
        rpc_port: bitcoind.rpc_port,
        rpc_wallet: None,
    };
    tracing::debug!("Creating wallet");
    let _create_wallet_output = bitcoin_cli
        .command::<String, _, _, _, _>([], "createwallet", ["integration-test"])
        .run_utf8()
        .await?;
    if let Poll::Ready(Some(err)) = poll!(processes.next()) {
        return Err(err);
    }
    bitcoin_cli.rpc_wallet = Some("integration-test".to_owned());
    tracing::debug!("Importing secret key");
    let mining_descriptor = {
        use miniscript;
        let descriptor = bdk_wallet::descriptor!(wpkh(secret_key))?;
        descriptor.0.to_string_with_secret(&descriptor.1)
    };
    let multisig_descriptor = {
        let descriptor = bdk_wallet::descriptor!(bare(multi(1, secret_key)))?;
        descriptor.0.to_string_with_secret(&descriptor.1)
    };
    let import_descriptors_output = bitcoin_cli
        .command::<String, _, String, _, _>(
            [],
            "importdescriptors",
            [serde_json::json!([
                {
                    "desc": mining_descriptor,
                    "timestamp": "now",
                    "active": false,
                },
                {
                    "desc": multisig_descriptor,
                    "timestamp": "now",
                    "active": false,
                },
            ])
            .to_string()],
        )
        .run_utf8()
        .await?;
    let expected_import_descriptors_output = serde_json::json!([
        { "success": true }, { "success": true }
    ]);
    if serde_json::from_str::<serde_json::Value>(&import_descriptors_output)?
        != expected_import_descriptors_output
    {
        anyhow::bail!("Importing descriptors failed: `{import_descriptors_output}`")
    }
    tracing::debug!(
        %signet_challenge_addr,
        "Checking that the signet challenge addr is loaded"
    );
    let getaddressinfo_output = bitcoin_cli
        .command::<String, _, _, _, _>([], "getaddressinfo", [signet_challenge_addr.to_string()])
        .run_utf8()
        .await?;
    tracing::debug!(%getaddressinfo_output);
    tracing::debug!("Generating receiving address");
    let receive_address = bitcoin_cli
        .command::<String, _, String, _, _>([], "getnewaddress", [])
        .run_utf8()
        .await?;
    tracing::debug!("Receiving address: {receive_address}");
    let receive_address = receive_address
        .parse::<Address<_>>()?
        .require_network(bitcoind.network)?;
    if let Poll::Ready(Some(err)) = poll!(processes.next()) {
        return Err(err);
    }
    tracing::debug!("Calibrating signet");
    let mut signet_miner = bins::SignetMiner {
        path: bin_paths.signet_miner.clone(),
        bitcoin_cli: Some(bitcoin_cli.display_without_chain()),
        bitcoin_util: Some(bin_paths.bitcoin_util.clone()),
        nbits: None,
        getblocktemplate_command: None,
        coinbasetxn: false,
        block_interval: None,
        debug: false,
    };
    let calibrate_output = signet_miner
        .command("calibrate", vec!["--seconds=1"])
        .run_utf8()
        .await?;
    let nbits_hex = {
        calibrate_output
            .strip_prefix("nbits=")
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Missing nbits prefix from calibration output: `{calibrate_output}`",
                )
            })?
            .split_once(" ")
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Missing nbits suffix from calibration output: `{calibrate_output}`",
                )
            })?
            .0
            .to_owned()
    };
    signet_miner.nbits = Some(hex::FromHex::from_hex(&nbits_hex)?);
    tracing::debug!(%mining_address, %nbits_hex, "Mining 1 block");
    let mine_output = signet_miner
        .command("generate", vec!["--address", &mining_address.to_string()])
        .run_utf8()
        .await?;
    tracing::debug!("Checking that block was mined successfully");
    let blocks: u32 = bitcoin_cli
        .command::<String, _, String, _, _>([], "getblockcount", [])
        .run_utf8()
        .await?
        .parse()?;
    anyhow::ensure!(blocks == 1);
    tracing::debug!("Mined 1 block: `{mine_output}`");
    if let Poll::Ready(Some(err)) = poll!(processes.next()) {
        return Err(err);
    }
    // Start electrs
    tracing::debug!("Starting electrs");
    let electrs = util::Electrs {
        path: bin_paths.electrs.clone(),
        db_dir: electrs_dir,
        config: electrs_conf,
        daemon_dir: bitcoind.data_dir.join("path"),
        daemon_p2p_port: bitcoind.listen_port,
        daemon_rpc_port: bitcoind.rpc_port,
        electrum_rpc_port: reserved_ports.electrs_electrum_rpc.port(),
        monitoring_port: reserved_ports.electrs_monitoring.port(),
        network: bitcoind.network,
        signet_magic: Some(signet_magic),
    };
    let electrs_proc = electrs.await_command_with_args::<String, String, _, _>([], []);
    processes.push(electrs_proc.boxed());
    if let Poll::Ready(Some(err)) = poll!(processes.next()) {
        return Err(err);
    }
    // wait for electrs to start
    sleep(std::time::Duration::from_secs(1)).await;
    if let Poll::Ready(Some(err)) = poll!(processes.next()) {
        return Err(err);
    }
    // Start BIP300301 Enforcer
    tracing::debug!("Starting bip300301_enforcer");
    let enforcer = util::Enforcer {
        path: bin_paths.bip300301_enforcer.clone(),
        data_dir: enforcer_dir,
        enable_mempool: true,
        node_rpc_user: bitcoind.rpc_user,
        node_rpc_pass: bitcoind.rpc_pass,
        node_rpc_port: bitcoind.rpc_port,
        node_zmq_sequence_port: bitcoind.zmq_sequence_port,
        serve_grpc_port: reserved_ports.enforcer_serve_grpc.port(),
        serve_rpc_port: reserved_ports.enforcer_serve_rpc.port(),
        wallet_electrum_port: electrs.electrum_rpc_port,
    };
    let enforcer_proc = enforcer.await_command_with_args::<_, String, _, _>(
        [(
            "RUST_LOG",
            "h2=info,hyper_util=info,jsonrpsee-client=debug,jsonrpsee-http=debug,tonic=debug,trace",
        )],
        [],
    );
    processes.push(enforcer_proc.boxed());
    if let Poll::Ready(Some(err)) = poll!(processes.next()) {
        return Err(err);
    }
    // wait for enforcer to start
    sleep(std::time::Duration::from_secs(1)).await;
    if let Poll::Ready(Some(err)) = poll!(processes.next()) {
        return Err(err);
    }
    // Set signet miner to use enforcer's GBT server
    {
        let gbt_script_file = out_dir.path().join("gbt-script.sh");
        tracing::info!("GBT script: {}", gbt_script_file.display());
        let gbt_script = format!(
            r#"#!/bin/sh
            REQUEST='{{"jsonrpc":"2.0","id":0,"method":"getblocktemplate","params":['$1']}}'
            RESPONSE=$(curl 127.0.0.1:{} --no-progress-meter -H "Content-Type: application/json" --data-binary "${{REQUEST}}")
            RESULT=$(echo "${{RESPONSE}}" | jq '.result')
            echo "${{RESULT}}""#,
            enforcer.serve_rpc_port
        );
        std::fs::write(&gbt_script_file, gbt_script)?;
        cfg_if::cfg_if! {
            if #[cfg(target_family = "unix")] {
                use std::os::unix::fs::PermissionsExt as _;
                let mut perms = std::fs::metadata(&gbt_script_file)?.permissions();
                // Add execute permission (equivalent to chmod +x)
                perms.set_mode(perms.mode() | 0o111);
                std::fs::set_permissions(&gbt_script_file, perms)?;
            }
        }
        signet_miner.coinbasetxn = true;
        signet_miner.getblocktemplate_command = Some(format!("{}", gbt_script_file.display()));
    }
    let validator_service_client =
        ValidatorServiceClient::connect(format!("http://127.0.0.1:{}", enforcer.serve_grpc_port))
            .await?;
    let wallet_service_client =
        WalletServiceClient::connect(format!("http://127.0.0.1:{}", enforcer.serve_grpc_port))
            .await?;
    Ok(PostSetup {
        out_dir,
        bitcoin_cli,
        processes,
        signet_miner,
        validator_service_client,
        wallet_service_client,
        mining_address,
        receive_address,
        _reserved_ports: reserved_ports,
    })
}

async fn mine_single(
    signet_miner: &bins::SignetMiner,
    mining_address: &Address,
) -> anyhow::Result<()> {
    let _mine_output = signet_miner
        .command(
            "generate",
            vec![
                "--address",
                &mining_address.to_string(),
                "--block-interval",
                "1",
            ],
        )
        .run_utf8()
        .await?;
    Ok(())
}

// Mine blocks, running a check after each block
async fn mine_check<F>(post_setup: &mut PostSetup, blocks: u32, mut check: F) -> anyhow::Result<()>
where
    F: FnMut(ReverseHex) -> anyhow::Result<()>,
{
    use proto::mainchain::subscribe_events_response::event::Event;
    let mut stream = post_setup
        .validator_service_client
        .subscribe_events(SubscribeEventsRequest {
            sidechain_id: Some(0),
        })
        .await?
        .into_inner();
    for _ in 0..blocks {
        let () = mine_single(&post_setup.signet_miner, &post_setup.mining_address).await?;
        let Some(resp) = stream.try_next().await? else {
            anyhow::bail!("Expected block event")
        };
        let Some(resp_event) = resp.event.and_then(|event| event.event) else {
            anyhow::bail!("Expected block event to be Some(_)")
        };
        match resp_event {
            Event::ConnectBlock(connect_block) => {
                let Some(header_info) = connect_block.header_info else {
                    anyhow::bail!("Missing header info")
                };
                let Some(block_hash) = header_info.block_hash else {
                    anyhow::bail!("Missing block hash")
                };
                check(block_hash)?
            }
            Event::DisconnectBlock(_) => anyhow::bail!("Unexpected block disconnect"),
        };
    }
    Ok(())
}

async fn mine(post_setup: &mut PostSetup, blocks: u32) -> anyhow::Result<()> {
    mine_check(post_setup, blocks, |_| Ok(())).await
}

async fn propose_sidechain(post_setup: &mut PostSetup) -> anyhow::Result<()> {
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
    let () = mine(post_setup, 1).await?;
    if let Poll::Ready(Some(err)) = poll!(post_setup.processes.next()) {
        return Err(err);
    }
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
    if let Poll::Ready(Some(err)) = poll!(post_setup.processes.next()) {
        return Err(err);
    }
    if sidechain_proposals_resp.sidechain_proposals.len() != 1 {
        anyhow::bail!("Expected 1 sidechain proposal")
    }
    Ok(())
}

async fn activate_sidechain(post_setup: &mut PostSetup) -> anyhow::Result<()> {
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
    if let Poll::Ready(Some(err)) = poll!(post_setup.processes.next()) {
        return Err(err);
    }
    let blocks_to_mine = 6;
    tracing::debug!("Mining {blocks_to_mine} blocks");
    let _ = mine(post_setup, blocks_to_mine).await?;
    if let Poll::Ready(Some(err)) = poll!(post_setup.processes.next()) {
        return Err(err);
    }
    tracing::debug!("Checking that exactly 1 sidechain is active");
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
    tracing::info!("Funding enforcer");
    let () = mine_check(post_setup, BLOCKS, |_| {
        progress_bar.inc(1);
        Ok(())
    })
    .await?;
    let _ = if let Poll::Ready(Some(err)) = poll!(post_setup.processes.next()) {
        return Err(err);
    };
    tracing::debug!("Waiting for wallet sync...");
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
    mut check: F,
) -> anyhow::Result<()>
where
    F: FnMut(u32, proto::mainchain::BlockInfo) -> anyhow::Result<()>,
{
    tracing::debug!("Mining {blocks} block(s)");
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
        let () = mine(post_setup, 1).await?;
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
    tracing::info!(
        "Creating deposit for `{}` sats, with `{}` sats fee",
        DEPOSIT_AMOUNT_SATS,
        DEPOSIT_FEE_SATS
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
    tracing::debug!("Deposit TXID: {deposit_txid}");
    if let Poll::Ready(Some(err)) = poll!(post_setup.processes.next()) {
        return Err(err);
    }
    tracing::debug!("Mining 1 sidechain block");
    let () = mine_check_block_events(post_setup, 1, |_, block_info| {
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

// Upvote the next withdrawal bundle so that it succeeds
async fn withdraw_succeed(post_setup: &mut PostSetup) -> anyhow::Result<()> {
    const WITHDRAW_AMOUNT_SATS: u64 = 18_000_000;
    const WITHDRAW_FEE_SATS: u64 = 1_000_000;
    tracing::info!(
        "Creating withdrawal for `{}` sats, with `{}` sats fee",
        WITHDRAW_AMOUNT_SATS,
        WITHDRAW_FEE_SATS,
    );
    let (withdrawal_bundle_tx, m6id) = blinded_m6(
        WITHDRAW_FEE_SATS,
        [TxOut {
            script_pubkey: post_setup.receive_address.script_pubkey(),
            value: bitcoin::Amount::from_sat(WITHDRAW_AMOUNT_SATS),
        }],
    )?;
    tracing::debug!("Withdrawal M6id: {m6id}");
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
    tracing::debug!("Mining 1 block to include M3 for withdrawal bundle");
    let () = mine_check_block_events(post_setup, 1, |_, block_info| {
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
    let () = mine_check_block_events(post_setup, 7, |seq, block_info| {
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
    tracing::debug!("Checking receive address balance is {WITHDRAW_AMOUNT_SATS}");
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
    anyhow::ensure!(receive_addr_balance.to_sat() == WITHDRAW_AMOUNT_SATS);
    Ok(())
}

async fn test(bin_paths: &BinPaths) -> anyhow::Result<()> {
    let mut post_setup = setup(bin_paths).await?;
    tracing::info!("Setup successfully");
    let () = propose_sidechain(&mut post_setup).await?;
    tracing::info!("Proposed sidechain successfully");
    let () = activate_sidechain(&mut post_setup).await?;
    tracing::info!("Activated sidechain successfully");
    let () = fund_enforcer(&mut post_setup).await?;
    tracing::info!("Funded enforcer successfully");
    let () = deposit(&mut post_setup).await?;
    tracing::info!("Deposited to sidechain successfully");
    let () = withdraw_succeed(&mut post_setup).await?;
    tracing::info!("Withdrawal succeeded");
    tracing::info!("Removing {}", post_setup.out_dir.path().display());
    drop_temp_dir(&post_setup.out_dir)?;
    drop(post_setup);
    Ok(())
}

pub fn tests(bin_paths: &BinPaths) -> [AsyncTrial<impl Future<Output = anyhow::Result<()>>>; 1] {
    let bin_paths = bin_paths.clone();
    [AsyncTrial::new(
        "signet",
        async move { test(&bin_paths).await },
    )]
}
