//! Cross-bidder BMM competition: two *different* wallets (different UTXOs,
//! no shared inputs) bid for the same sidechain slot with different h* and
//! different fees. Standard BIP125 RBF can't resolve this -- it only
//! replaces a transaction that shares an input with the replacement -- so
//! this exercises the `cusf-enforcer-mempool` patched fork's BMM-specific
//! `conflicts_with`/`weight_tweak` conflict resolution, which this repo
//! feeds via `accept_tx` (`lib/validator/cusf_enforcer.rs`) but does not
//! itself implement.
//!
//! Three rounds prove the winner is determined by fee and not by which side
//! happens to be "the miner's own" or "the one relayed over p2p". The first
//! two swap which side bids higher. The third makes the miner's own bid the
//! losing one *and* places it through the miner's enforcer wallet, so it is
//! recorded as a tracked bid -- the only arrangement that reaches the
//! coinbase's M7 preload, and so the only one that can show the tracking
//! table overriding the auction.

use std::{str::FromStr as _, time::Duration};

use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    messages::M8BmmRequest,
    proto::{
        self,
        common::{ConsensusHex, ReverseHex},
        mainchain::{BlockHeaderInfo, CreateBmmCriticalDataTransactionRequest, GetChainTipRequest},
    },
    types::BmmCommitment,
};
use bitcoin::{
    Amount, OutPoint, ScriptBuf, Transaction, TxIn, TxOut, Txid, consensus::encode::serialize_hex,
    transaction::Version,
};
use buffa::MessageField;
use futures::{StreamExt as _, channel::mpsc};
use serde::Deserialize;
use tokio::time::sleep;
use tracing::Instrument as _;

use crate::{
    integration_test::{activate_sidechain, fund_enforcer, propose_sidechain},
    mine,
    setup::{BitcoindKind, DummySidechain, Mode, Network, PostSetup, SetupOpts, Sidechain},
    util::{self, BinPaths, FileDumpConfig, TestFileRegistry},
};

pub const TEST_NAME: &str = "bmm_cross_bidder_competition";

#[derive(Deserialize)]
struct Utxo {
    txid: String,
    vout: u32,
    amount: f64,
}

#[derive(Deserialize)]
struct SignResult {
    hex: String,
    complete: bool,
}

fn register_files(
    file_registry: &TestFileRegistry,
    label: &str,
    directories: &crate::setup::Directories,
) {
    file_registry.register_file(
        TEST_NAME,
        directories.bitcoin_dir.join("stdout.txt"),
        FileDumpConfig::new().with_label(format!("Bitcoin Core stdout ({label})")),
    );
    file_registry.register_file(
        TEST_NAME,
        directories.bitcoin_dir.join("stderr.txt"),
        FileDumpConfig::new().with_label(format!("Bitcoin Core stderr ({label})")),
    );
    file_registry.register_file(
        TEST_NAME,
        directories.enforcer_dir.join("stdout.txt"),
        FileDumpConfig::new().with_label(format!("Enforcer stdout ({label})")),
    );
    file_registry.register_file(
        TEST_NAME,
        directories.enforcer_dir.join("stderr.txt"),
        FileDumpConfig::new().with_label(format!("Enforcer stderr ({label})")),
    );
}

fn sidechain_block_hash(label: &str) -> [u8; 32] {
    use bitcoin::hashes::Hash;
    bitcoin::hashes::sha256::Hash::hash(format!("cross bidder {label}").as_bytes()).to_byte_array()
}

/// Craft, sign, and broadcast a competing M8 bid directly against `post_setup`'s
/// own bitcoind wallet -- entirely independent of any enforcer wallet, so it
/// shares no inputs with any enforcer-originated bid.
async fn broadcast_competing_bid(
    post_setup: &PostSetup,
    prev_mainchain_block_hash: bitcoin::BlockHash,
    h_star: [u8; 32],
    fee: Amount,
) -> anyhow::Result<Txid> {
    let utxos: Vec<Utxo> = {
        let json = post_setup
            .bitcoin_cli
            .command::<String, _, String, _, _>([], "listunspent", [])
            .run_utf8()
            .await?;
        serde_json::from_str(&json)?
    };
    let (utxo, input_value) = utxos
        .into_iter()
        .find_map(|u| {
            let amount = Amount::from_btc(u.amount).ok()?;
            (amount > fee).then_some((u, amount))
        })
        .ok_or_else(|| anyhow::anyhow!("no spendable UTXO in bitcoind wallet"))?;

    let change_address = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getnewaddress", [])
        .run_utf8()
        .await?;
    let change_address = bitcoin::Address::from_str(change_address.trim())?
        .require_network(post_setup.network.into())?;

    let script_pubkey = ScriptBuf::new_op_return(&M8BmmRequest::build(
        DummySidechain::SIDECHAIN_NUMBER,
        BmmCommitment(h_star),
        prev_mainchain_block_hash,
    )?);

    let unsigned_tx = Transaction {
        version: Version::TWO,
        lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint {
                txid: Txid::from_str(&utxo.txid)?,
                vout: utxo.vout,
            },
            ..TxIn::default()
        }],
        output: vec![
            TxOut {
                script_pubkey,
                value: Amount::ZERO,
            },
            TxOut {
                script_pubkey: change_address.script_pubkey(),
                value: input_value - fee,
            },
        ],
    };

    let signed_hex = {
        let json = post_setup
            .bitcoin_cli
            .command::<String, _, _, _, _>(
                [],
                "signrawtransactionwithwallet",
                [serialize_hex(&unsigned_tx)],
            )
            .run_utf8()
            .await?;
        let signed: SignResult = serde_json::from_str(&json)?;
        anyhow::ensure!(signed.complete, "signrawtransactionwithwallet incomplete");
        signed.hex
    };

    let txid_str = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "sendrawtransaction", [signed_hex, "0".to_owned()])
        .run_utf8()
        .await?;
    Ok(Txid::from_str(txid_str.trim())?)
}

async fn get_tip_info(
    post_setup: &mut PostSetup,
) -> anyhow::Result<(ReverseHex, u32, bitcoin::BlockHash)> {
    let BlockHeaderInfo {
        block_hash: tip_block_hash,
        height: tip_height,
        ..
    } = post_setup
        .validator_service_client
        .get_chain_tip(GetChainTipRequest::default())
        .await?
        .into_owned()
        .block_header_info
        .into_option()
        .ok_or_else(|| anyhow::anyhow!("Expected `block_header_info` in GetChainTipResponse"))?;
    let tip_block_hash_field = tip_block_hash
        .into_option()
        .ok_or_else(|| anyhow::anyhow!("Expected `block_hash` in BlockHeaderInfo"))?;
    let decoded: bitcoin::BlockHash = tip_block_hash_field
        .clone()
        .decode::<BlockHeaderInfo, bitcoin::BlockHash>("block_hash")?;
    Ok((tip_block_hash_field, tip_height, decoded))
}

async fn wait_for_sender_tip(
    post_setup: &mut PostSetup,
    expected: bitcoin::BlockHash,
) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let (_, _, tip) = get_tip_info(post_setup).await?;
        if tip == expected {
            return Ok(());
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "sender did not sync to tip {expected} in time"
        );
        sleep(Duration::from_millis(250)).await;
    }
}

struct Setups {
    miner: PostSetup,
    sender: PostSetup,
}

async fn setup(bin_paths: BinPaths, file_registry: &TestFileRegistry) -> anyhow::Result<Setups> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let miner_pre = crate::setup::PreSetup::new(bin_paths.clone(), Network::Regtest)?;
    let sender_pre = crate::setup::PreSetup::new(bin_paths, Network::Regtest)?;
    register_files(file_registry, "miner", &miner_pre.directories);
    register_files(file_registry, "sender", &sender_pre.directories);

    let sender = {
        let enforcer_args = vec![format!(
            "--p2p-broadcast-addr=127.0.0.1:{}",
            miner_pre.reserved_ports.bitcoind_listen.port()
        )];
        let setup_opts: SetupOpts = SetupOpts {
            bitcoind_args: Vec::new(),
            bitcoind_kind: BitcoindKind::Unpatched,
            enforcer_args,
            enforcer_wallet: Default::default(),
        };
        sender_pre
            .setup(Mode::GetBlockTemplate, setup_opts, res_tx.clone())
            .await?
    };
    let miner = {
        let bitcoind_args = vec!["-debug=mempool", "-debug=validation"];
        let setup_opts: SetupOpts<_> = SetupOpts {
            bitcoind_args,
            bitcoind_kind: BitcoindKind::Patched,
            enforcer_args: Vec::new(),
            enforcer_wallet: Default::default(),
        };
        miner_pre
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
    Ok(Setups { miner, sender })
}

/// How the miner's own competing bid gets into its mempool.
#[derive(Clone, Copy)]
enum MinerBid {
    /// Straight against bitcoind, so nothing is recorded in the producer's
    /// bid-tracking table.
    Raw,
    /// Through the miner's own enforcer wallet, which records it as a tracked
    /// bid -- the arrangement in which `extend_coinbase_txouts` preloads its
    /// M7 into the coinbase before any fee has been compared.
    EnforcerWallet,
}

async fn run_round(
    setups: &mut Setups,
    round: &str,
    sender_fee: u64,
    miner_fee: u64,
    miner_bid: MinerBid,
) -> anyhow::Result<()> {
    let (prev_bytes, height, prev_hash) = get_tip_info(&mut setups.sender).await?;
    let h_star_sender = sidechain_block_hash(&format!("{round}-sender"));
    let h_star_miner = sidechain_block_hash(&format!("{round}-miner"));

    let sender_txid = setups
        .sender
        .wallet_service_client
        .create_bmm_critical_data_transaction(CreateBmmCriticalDataTransactionRequest {
            sidechain_id: proto::wrap_u32(DummySidechain::SIDECHAIN_NUMBER.0.into()),
            value_sats: proto::wrap_u64(sender_fee),
            height: proto::wrap_u32(height),
            critical_hash: MessageField::some(ConsensusHex::encode(&h_star_sender)),
            prev_bytes: MessageField::some(prev_bytes.clone()),
        })
        .await?
        .into_owned()
        .txid
        .into_option()
        .and_then(|txid| proto::unwrap_string(txid.hex))
        .ok_or_else(|| anyhow::anyhow!("missing txid"))?;
    tracing::info!(round, %sender_txid, sender_fee, "Sender bid placed");

    let miner_txid = match miner_bid {
        MinerBid::Raw => broadcast_competing_bid(
            &setups.miner,
            prev_hash,
            h_star_miner,
            Amount::from_sat(miner_fee),
        )
        .await?
        .to_string(),
        MinerBid::EnforcerWallet => setups
            .miner
            .wallet_service_client
            .create_bmm_critical_data_transaction(CreateBmmCriticalDataTransactionRequest {
                sidechain_id: proto::wrap_u32(DummySidechain::SIDECHAIN_NUMBER.0.into()),
                value_sats: proto::wrap_u64(miner_fee),
                height: proto::wrap_u32(height),
                critical_hash: MessageField::some(ConsensusHex::encode(&h_star_miner)),
                prev_bytes: MessageField::some(prev_bytes),
            })
            .await?
            .into_owned()
            .txid
            .into_option()
            .and_then(|txid| proto::unwrap_string(txid.hex))
            .ok_or_else(|| anyhow::anyhow!("missing txid"))?,
    };
    tracing::info!(round, %miner_txid, miner_fee, "Miner-local competing bid placed");

    // Give the sender's bid time to reach the miner over p2p.
    sleep(Duration::from_secs(10)).await;

    let expect_sender_wins = sender_fee > miner_fee;
    let expected_h_star = if expect_sender_wins {
        h_star_sender
    } else {
        h_star_miner
    };
    tracing::info!(
        round,
        expect_sender_wins,
        "Mining and checking which bid the patched miner included"
    );
    let () = mine::mine_check_block_events::<_, DummySidechain>(
        &mut setups.miner,
        1,
        None,
        move |_, block_info| {
            let bmm_commitment = block_info
                .bmm_commitment
                .into_option()
                .ok_or_else(|| anyhow::anyhow!("Expected a BMM commitment"))?;
            let expected = ConsensusHex::encode(&expected_h_star);
            anyhow::ensure!(
                bmm_commitment == expected,
                "expected the higher-fee bid ({} sats) to win, got a different h*",
                sender_fee.max(miner_fee),
            );
            Ok(())
        },
    )
    .await?;
    tracing::info!(round, "Higher-fee bid won, as expected");

    let (_, _, new_tip) = get_tip_info(&mut setups.miner).await?;
    let () = wait_for_sender_tip(&mut setups.sender, new_tip).await?;
    Ok(())
}

async fn test_bmm_cross_bidder_competition_task(mut setups: Setups) -> anyhow::Result<()> {
    let () = propose_sidechain::<DummySidechain>(&mut setups.miner).await?;
    let () = activate_sidechain::<DummySidechain>(&mut setups.miner).await?;
    let () = fund_enforcer::<DummySidechain>(&mut setups.miner).await?;

    let sender_addr = setups
        .sender
        .wallet_service_client
        .create_new_address(proto::mainchain::CreateNewAddressRequest::default())
        .await?
        .into_owned()
        .address;
    let () = setups
        .miner
        .wallet_service_client
        .send_transaction(proto::mainchain::SendTransactionRequest {
            destinations: std::collections::HashMap::from([(sender_addr, 10_000_000_u64)])
                .into_iter()
                .collect(),
            ..Default::default()
        })
        .await
        .map(drop)?;
    let () = mine::mine::<DummySidechain>(&mut setups.miner, 1, None).await?;
    sleep(Duration::from_secs(1)).await;
    let (_, _, tip) = get_tip_info(&mut setups.miner).await?;
    let () = wait_for_sender_tip(&mut setups.sender, tip).await?;

    // Round 1: the miner-local bid has the higher fee.
    let () = run_round(&mut setups, "round1", 5_000, 50_000, MinerBid::Raw).await?;
    // Round 2: swap which side has the higher fee, to prove the winner is
    // determined by fee, not by which side happens to be "local" or
    // "p2p-relayed".
    let () = run_round(&mut setups, "round2", 50_000, 5_000, MinerBid::Raw).await?;
    // Round 3: the losing side is the miner's own, and this time it is placed
    // through the miner's enforcer wallet rather than straight against
    // bitcoind. Rounds 1 and 2 never record a bid in the miner's producer DB,
    // so they never reach the path where `extend_coinbase_txouts` claims the
    // sidechain's slot with the miner's own h* before a single fee has been
    // looked at -- and where the template pass then excludes every competing
    // h*, the 50,000-sat one included. "Whose bid it is" must not decide the
    // auction; the fee must.
    let () = run_round(
        &mut setups,
        "round3",
        50_000,
        5_000,
        MinerBid::EnforcerWallet,
    )
    .await?;

    drop(setups.miner.tasks);
    drop(setups.sender.tasks);
    Ok(())
}

pub async fn test_bmm_cross_bidder_competition(
    bin_paths: BinPaths,
    file_registry: TestFileRegistry,
) -> anyhow::Result<()> {
    let (res_tx, mut res_rx) = mpsc::unbounded();
    let setups = setup(bin_paths, &file_registry).await?;
    let _test_task: util::AbortOnDrop<()> = tokio::task::spawn(
        {
            let res_tx = res_tx.clone();
            async move {
                let res = test_bmm_cross_bidder_competition_task(setups).await;
                let _send_err: Result<(), _> = res_tx.unbounded_send(res);
            }
        }
        .in_current_span(),
    )
    .into();
    res_rx
        .next()
        .await
        .ok_or_else(|| anyhow::anyhow!("Unexpected end of test task result stream"))?
}
