//! Deposits that no longer spend the treasury UTXO (aggregated deposits): the
//! consolidation that spends them in the same block, and what it must not be
//! allowed to do.

use std::time::Duration;

use bip300301_enforcer_lib::{
    proto::{
        self,
        common::ConsensusHex,
        mainchain::{
            CreateDepositTransactionRequest, CreateDepositTransactionResponse, GetCtipRequest,
            GetCtipResponse, ListSidechainDepositTransactionsRequest, OutPoint as ProtoOutPoint,
            WalletTransaction, block_info, list_sidechain_deposit_transactions_response,
            withdrawal_bundle_event,
        },
    },
    types::{M6id, Thresholds},
};
use bitcoin::{
    Amount, BlockHash, OutPoint, ScriptBuf, Transaction, TxIn, TxOut, Txid,
    consensus::encode::serialize_hex, script::PushBytesBuf, transaction::Version,
};
use futures::channel::mpsc;
use jsonrpsee::{core::client::ClientT as _, rpc_params};
use serde::Deserialize;

use crate::{
    block_verdict::{
        Expect, assert_enforcer_verdict, wait_for_enforcer_height, wait_for_enforcer_tip_hash,
    },
    integration_test::{
        activate_sidechain, deposit, expect_withdrawal_bundle_event, fund_enforcer,
        propose_sidechain, wait_for_wallet_sync,
    },
    mine::{MiningPolicy, mine, mine_check_block_events},
    setup::{
        DummySidechain, MiningMode, PostSetup, Sidechain as _, best_block_hash, core_utxos,
        generate_empty_block, invalidate_block, op_drivechain, pending_bundle_count,
        read_enforcer_log, sign_with_core_wallet, wait_for_tx_in_mempool,
    },
    test_sidechain_ack_policy::bundle_vote_count,
};

const TREASURY: Amount = Amount::from_sat(1_000_000);
const DEPOSIT_FEE: Amount = Amount::from_sat(10_000);

/// Activate [`DummySidechain`] and give it a treasury of [`TREASURY`]. The
/// sidechain must be kept alive.
async fn activate_funded_sidechain(
    post_setup: &mut PostSetup,
) -> anyhow::Result<(DummySidechain, String)> {
    let (sidechain_res_tx, _sidechain_res_rx) = mpsc::unbounded();
    let mut sidechain = DummySidechain::setup((), post_setup, sidechain_res_tx).await?;
    let () = propose_sidechain::<DummySidechain>(post_setup).await?;
    let () = activate_sidechain::<DummySidechain>(post_setup).await?;
    let () = fund_enforcer::<DummySidechain>(post_setup).await?;
    let sidechain_address = sidechain.get_deposit_address().await?;
    let () = deposit(
        post_setup,
        &mut sidechain,
        &sidechain_address,
        TREASURY,
        DEPOSIT_FEE,
    )
    .await?;
    Ok((sidechain, sidechain_address))
}

/// Broadcast a deposit from the enforcer wallet without mining it.
async fn create_deposit(
    post_setup: &mut PostSetup,
    sidechain_address: &str,
    value: Amount,
) -> anyhow::Result<Txid> {
    let txid = post_setup
        .wallet_service_client
        .create_deposit_transaction(CreateDepositTransactionRequest {
            sidechain_id: proto::wrap_u32(DummySidechain::SIDECHAIN_NUMBER.0.into()),
            address: proto::wrap_string(sidechain_address),
            value_sats: proto::wrap_u64(value.to_sat()),
            fee_sats: proto::wrap_u64(DEPOSIT_FEE.to_sat()),
        })
        .await?
        .into_owned()
        .txid
        .ok_or_else(|| proto::Error::missing_field::<CreateDepositTransactionResponse>("txid"))?
        .decode::<CreateDepositTransactionResponse, _>("txid")?;
    let () = wait_for_tx_in_mempool(&post_setup.bitcoind_client, &txid).await?;
    Ok(txid)
}

/// The sidechain's treasury UTXO, per the validator.
async fn treasury_utxo(post_setup: &mut PostSetup) -> anyhow::Result<(OutPoint, Amount)> {
    let ctip = post_setup
        .validator_service_client
        .get_ctip(GetCtipRequest {
            sidechain_number: proto::wrap_u32(DummySidechain::SIDECHAIN_NUMBER.0.into()),
        })
        .await?
        .into_owned()
        .ctip
        .into_option()
        .ok_or_else(|| anyhow::anyhow!("the sidechain has no treasury UTXO"))?;
    let txid = ctip
        .txid
        .into_option()
        .ok_or_else(|| proto::Error::missing_field::<GetCtipResponse>("txid"))?
        .decode::<GetCtipResponse, Txid>("txid")?;
    let outpoint = OutPoint {
        txid,
        vout: ctip.vout,
    };
    Ok((outpoint, Amount::from_sat(ctip.value)))
}

/// Mine one block, returning its events.
async fn mine_one(
    post_setup: &mut PostSetup,
    policy: MiningPolicy,
) -> anyhow::Result<Vec<block_info::Event>> {
    let mut events = Vec::new();
    let () =
        mine_check_block_events::<_, DummySidechain>(post_setup, 1, policy, |_, block_info| {
            events.extend(block_info.events);
            Ok(())
        })
        .await?;
    Ok(events)
}

fn paid_out(events: &[block_info::Event], m6id: M6id) -> anyhow::Result<bool> {
    let m6id_hex = ConsensusHex::encode(&m6id.0);
    for event in events {
        let Some(block_info::event::Event::WithdrawalBundle(_)) = &event.event else {
            continue;
        };
        let (event_m6id, event) = expect_withdrawal_bundle_event(event)?;
        if *event_m6id == m6id_hex
            && matches!(event, withdrawal_bundle_event::event::Event::Succeeded(_))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `(txid, sequence number)` of each deposit in `events`.
fn deposits(events: &[block_info::Event]) -> anyhow::Result<Vec<(Txid, u64)>> {
    events
        .iter()
        .filter_map(|event| match &event.event {
            Some(block_info::event::Event::Deposit(deposit)) => Some(deposit),
            _ => None,
        })
        .map(|deposit| {
            let txid = deposit
                .outpoint
                .as_option()
                .ok_or_else(|| {
                    proto::Error::missing_field::<proto::mainchain::Deposit>("outpoint")
                })?
                .txid
                .clone()
                .into_option()
                .ok_or_else(|| proto::Error::missing_field::<ProtoOutPoint>("txid"))?
                .decode::<ProtoOutPoint, Txid>("txid")?;
            let sequence_number =
                proto::unwrap_u64(deposit.sequence_number.clone()).ok_or_else(|| {
                    proto::Error::missing_field::<proto::mainchain::Deposit>("sequence_number")
                })?;
            Ok((txid, sequence_number))
        })
        .collect()
}

/// Broadcast a withdrawal bundle and vote it up to the point where the next
/// block pays it out.
async fn approve_withdrawal(
    post_setup: &mut PostSetup,
    sidechain: &mut DummySidechain,
    payout: Amount,
    fee: Amount,
) -> anyhow::Result<M6id> {
    let receive_address = post_setup.receive_address.clone();
    let m6id = sidechain
        .create_withdrawal(post_setup, &receive_address, payout, fee)
        .await?;
    // The M3 proposal, then one upvote per block.
    let () = mine::<DummySidechain>(post_setup, 1, MiningPolicy::SILENT).await?;
    let threshold = u32::from(Thresholds::SHORT.withdrawal_bundle_inclusion_threshold);
    for _ in 0..threshold + 1 {
        let () = mine::<DummySidechain>(post_setup, 1, MiningPolicy::VOTE).await?;
        if bundle_vote_count(post_setup, m6id.0).await? > threshold {
            return Ok(m6id);
        }
    }
    anyhow::bail!("the withdrawal bundle never passed the inclusion threshold")
}

async fn received_by_address(post_setup: &PostSetup) -> anyhow::Result<Amount> {
    let received_btc: f64 = post_setup
        .bitcoind_client
        .request(
            "getreceivedbyaddress",
            rpc_params![post_setup.receive_address.to_string()],
        )
        .await?;
    Ok(Amount::from_btc(received_btc)?)
}

/// An approved withdrawal is paid out in a block that also carries a deposit
/// into the same treasury. The M6 spends both the treasury UTXO and the
/// deposit, and it must still be recognised as the withdrawal: paid once, with
/// its bundle retired.
pub async fn test_payout_with_unconsolidated_deposit(
    mut post_setup: PostSetup,
) -> anyhow::Result<()> {
    const PAYOUT: Amount = Amount::from_sat(60_000);
    const PAYOUT_FEE: Amount = Amount::from_sat(1_000);
    let (mut sidechain, sidechain_address) = activate_funded_sidechain(&mut post_setup).await?;
    let m6id = approve_withdrawal(&mut post_setup, &mut sidechain, PAYOUT, PAYOUT_FEE).await?;

    // Larger than the payout, so the M6's treasury output is worth more than
    // the treasury UTXO it spends.
    let deposit_txid = create_deposit(&mut post_setup, &sidechain_address, TREASURY).await?;
    tracing::info!(%deposit_txid, "Deposit in the mempool, mining the payout block");
    let events = mine_one(&mut post_setup, MiningPolicy::VOTE).await?;
    let paid_in_deposit_block = paid_out(&events, m6id)?;
    let pending_after = pending_bundle_count(
        &post_setup.validator_service_client,
        DummySidechain::SIDECHAIN_NUMBER,
    )
    .await?;

    // Whatever the first block did, the bundle is still approved: if it is
    // still pending, this block pays it again.
    let _events = mine_one(&mut post_setup, MiningPolicy::VOTE).await?;
    let received = received_by_address(&post_setup).await?;
    anyhow::ensure!(
        received == PAYOUT,
        "the withdrawal paid out {received} in total, expected {PAYOUT}: the block with the \
         deposit {} it and left {pending_after} bundle(s) pending, so the next block paid it again",
        if paid_in_deposit_block {
            "paid"
        } else {
            "did not retire"
        }
    );
    anyhow::ensure!(
        paid_in_deposit_block,
        "the block carrying the deposit did not report the withdrawal as paid"
    );
    Ok(())
}

/// Mine blocks so bitcoind's wallet has a mature coinbase, and wait for the
/// enforcer to catch up on them.
async fn fund_core_wallet(post_setup: &PostSetup) -> anyhow::Result<()> {
    // Coinbase outputs need 100 confirmations.
    const FUNDING_BLOCKS: u32 = 101;
    let _block_hashes: Vec<BlockHash> = post_setup
        .bitcoind_client
        .request(
            "generatetoaddress",
            rpc_params![FUNDING_BLOCKS, post_setup.mining_address.to_string()],
        )
        .await?;
    let funded_height: u32 = post_setup
        .bitcoind_client
        .request("getblockcount", rpc_params![])
        .await?;
    wait_for_enforcer_height(post_setup, funded_height).await
}

/// A deposit of `value` into the sidechain from bitcoind's wallet: treasury
/// output, address, change.
async fn signed_core_deposit(post_setup: &PostSetup, value: Amount) -> anyhow::Result<Transaction> {
    const TX_FEE: Amount = Amount::from_sat(1_000);
    let (utxo, input_value) = core_utxos(&post_setup.bitcoind_client, value + TX_FEE, 1)
        .await?
        .remove(0);
    let unsigned = Transaction {
        version: Version::TWO,
        lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: utxo,
            ..TxIn::default()
        }],
        output: vec![
            TxOut {
                script_pubkey: op_drivechain()?.script(DummySidechain::SIDECHAIN_NUMBER),
                value,
            },
            TxOut {
                script_pubkey: ScriptBuf::new_op_return(PushBytesBuf::from(*b"sidechain address")),
                value: Amount::ZERO,
            },
            TxOut {
                script_pubkey: post_setup.mining_address.script_pubkey(),
                value: input_value - value - TX_FEE,
            },
        ],
    };
    sign_with_core_wallet(&post_setup.bitcoind_client, &unsigned).await
}

/// Mine a block holding exactly `txs`. `generateblock` bypasses mempool policy
/// while still enforcing consensus rules.
async fn generate_block(post_setup: &PostSetup, txs: &[&Transaction]) -> anyhow::Result<BlockHash> {
    #[derive(Deserialize)]
    struct Generated {
        hash: BlockHash,
    }
    let generated: Generated = post_setup
        .bitcoind_client
        .request(
            "generateblock",
            rpc_params![
                post_setup.mining_address.to_string(),
                txs.iter().map(|tx| serialize_hex(*tx)).collect::<Vec<_>>()
            ],
        )
        .await?;
    Ok(generated.hash)
}

fn treasury_input(previous_output: OutPoint) -> TxIn {
    TxIn {
        previous_output,
        ..TxIn::default()
    }
}

fn deposit_address_output() -> TxOut {
    TxOut {
        script_pubkey: ScriptBuf::new_op_return(PushBytesBuf::from(*b"sidechain address")),
        value: Amount::ZERO,
    }
}

fn attacker_output(value: Amount) -> TxOut {
    let mut spk = vec![0x00, 0x14];
    spk.extend_from_slice(&[0x42u8; 20]);
    TxOut {
        script_pubkey: ScriptBuf::from_bytes(spk),
        value,
    }
}

/// A transaction that spends the treasury UTXO together with a deposit from
/// the same block, and returns less than their sum to the treasury, is a
/// withdrawal. Without an approved bundle behind it, the block is invalid.
pub async fn test_consolidation_theft(mut post_setup: PostSetup) -> anyhow::Result<()> {
    const STOLEN_DEPOSIT: Amount = Amount::from_sat(10_000);
    let (_sidechain, _sidechain_address) = activate_funded_sidechain(&mut post_setup).await?;
    let (treasury_outpoint, treasury) = treasury_utxo(&mut post_setup).await?;
    let () = fund_core_wallet(&post_setup).await?;
    let deposit_tx = signed_core_deposit(&post_setup, STOLEN_DEPOSIT).await?;
    // The "consolidation": pays the treasury one sat more than it held, and
    // the rest of the deposit to whoever wrote the transaction. Treasury
    // outputs are anyone-can-spend, so nothing needs signing.
    let theft_tx = Transaction {
        version: Version::TWO,
        lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
        input: vec![
            treasury_input(treasury_outpoint),
            treasury_input(OutPoint {
                txid: deposit_tx.compute_txid(),
                vout: 0,
            }),
        ],
        output: vec![
            TxOut {
                script_pubkey: op_drivechain()?.script(DummySidechain::SIDECHAIN_NUMBER),
                value: treasury + Amount::ONE_SAT,
            },
            attacker_output(STOLEN_DEPOSIT - Amount::ONE_SAT),
        ],
    };
    let stolen = theft_tx.output[1].value;
    let block_hash = generate_block(&post_setup, &[&deposit_tx, &theft_tx]).await?;
    tracing::info!(%block_hash, "Mined the theft block");

    if let Err(err) = assert_enforcer_verdict(
        &mut post_setup,
        block_hash,
        Expect::Rejected {
            log_contains: "rejecting block",
        },
        Duration::from_secs(10),
    )
    .await
    {
        let (_, treasury_after) = treasury_utxo(&mut post_setup).await?;
        anyhow::bail!(
            "{err:#}: the treasury went from {treasury} to {treasury_after} while the deposit \
             of {STOLEN_DEPOSIT} was consumed and {stolen} of it left to the attacker, without \
             an approved withdrawal bundle"
        );
    }
    let (_, treasury_after) = treasury_utxo(&mut post_setup).await?;
    anyhow::ensure!(
        treasury_after == treasury,
        "the rejected block changed the treasury from {treasury} to {treasury_after}"
    );
    Ok(())
}

/// A transaction that spends only a deposit from the same block, and returns
/// less than it to the treasury, takes value out of the treasury without
/// spending the treasury UTXO. It is not a valid withdrawal, and the enforcer
/// must reject the block and carry on, not fall over on it.
pub async fn test_withdrawal_without_treasury_input(
    mut post_setup: PostSetup,
) -> anyhow::Result<()> {
    const DEPOSIT: Amount = Amount::from_sat(10_000);
    let (_sidechain, _sidechain_address) = activate_funded_sidechain(&mut post_setup).await?;
    let (_, treasury) = treasury_utxo(&mut post_setup).await?;
    let () = fund_core_wallet(&post_setup).await?;
    let deposit_tx = signed_core_deposit(&post_setup, DEPOSIT).await?;
    let skim_tx = Transaction {
        version: Version::TWO,
        lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
        input: vec![treasury_input(OutPoint {
            txid: deposit_tx.compute_txid(),
            vout: 0,
        })],
        output: vec![
            TxOut {
                script_pubkey: op_drivechain()?.script(DummySidechain::SIDECHAIN_NUMBER),
                value: DEPOSIT - Amount::from_sat(5_000),
            },
            attacker_output(Amount::from_sat(5_000)),
        ],
    };
    let block_hash = generate_block(&post_setup, &[&deposit_tx, &skim_tx]).await?;
    tracing::info!(%block_hash, "Mined the block spending only the deposit");

    assert_enforcer_verdict(
        &mut post_setup,
        block_hash,
        Expect::Rejected {
            log_contains: "does not spend the consolidated treasury UTXO",
        },
        Duration::from_secs(10),
    )
    .await?;
    // A crashed enforcer cannot answer.
    let (_, treasury_after) = treasury_utxo(&mut post_setup)
        .await
        .map_err(|err| anyhow::anyhow!("the enforcer is not serving after the block: {err:#}"))?;
    anyhow::ensure!(
        treasury_after == treasury,
        "the rejected block changed the treasury from {treasury} to {treasury_after}"
    );
    Ok(())
}

/// Two deposits into the same treasury, in one block: each gets its own
/// sequence number, and both show up in the wallet's deposit history.
pub async fn test_deposit_sequence_numbers(mut post_setup: PostSetup) -> anyhow::Result<()> {
    let (_sidechain, sidechain_address) = activate_funded_sidechain(&mut post_setup).await?;
    let deposit_txid_1 = create_deposit(&mut post_setup, &sidechain_address, TREASURY).await?;
    let deposit_txid_2 = create_deposit(&mut post_setup, &sidechain_address, TREASURY).await?;
    tracing::info!(%deposit_txid_1, %deposit_txid_2, "Mining both deposits in one block");
    let events = mine_one(&mut post_setup, MiningPolicy::SILENT).await?;
    let deposits = deposits(&events)?;
    let [(txid_1, seq_1), (txid_2, seq_2)] = deposits.as_slice() else {
        anyhow::bail!("expected 2 deposit events, got {deposits:?}");
    };
    anyhow::ensure!(
        [*txid_1, *txid_2].contains(&deposit_txid_1)
            && [*txid_1, *txid_2].contains(&deposit_txid_2),
        "unexpected deposits {deposits:?}"
    );
    anyhow::ensure!(
        seq_1 != seq_2,
        "both deposits got sequence number {seq_1}; sidechains tell deposits apart by it"
    );

    let listed = post_setup
        .wallet_service_client
        .list_sidechain_deposit_transactions(ListSidechainDepositTransactionsRequest {})
        .await?
        .into_owned()
        .transactions
        .len();
    anyhow::ensure!(
        listed == 3,
        "the wallet lists {listed} deposit(s), expected the 3 it made"
    );
    Ok(())
}

/// An approved withdrawal that the treasury UTXO alone cannot cover is paid
/// out as soon as a deposit in the same block makes up the difference.
pub async fn test_payout_funded_by_unconsolidated_deposit(
    mut post_setup: PostSetup,
) -> anyhow::Result<()> {
    const PAYOUT: Amount = Amount::from_sat(1_400_000);
    const PAYOUT_FEE: Amount = Amount::from_sat(100_000);
    let (mut sidechain, sidechain_address) = activate_funded_sidechain(&mut post_setup).await?;
    let m6id = approve_withdrawal(&mut post_setup, &mut sidechain, PAYOUT, PAYOUT_FEE).await?;

    let deposit_txid = create_deposit(&mut post_setup, &sidechain_address, TREASURY).await?;
    tracing::info!(%deposit_txid, "Deposit in the mempool, mining the payout block");
    let events = mine_one(&mut post_setup, MiningPolicy::VOTE).await?;
    if !paid_out(&events, m6id)? {
        let skipped: Vec<String> = read_enforcer_log(&post_setup.directories.enforcer_dir)?
            .lines()
            .filter(|line| line.contains("skipping an approved withdrawal bundle"))
            .map(str::to_owned)
            .collect();
        anyhow::bail!(
            "the withdrawal of {} was not paid in the block whose deposit brought the treasury \
             to {}; the producer logged: {skipped:#?}",
            PAYOUT + PAYOUT_FEE,
            TREASURY + TREASURY,
        );
    }
    Ok(())
}

/// The txids of the sidechain deposits the wallet lists.
async fn listed_deposits(post_setup: &PostSetup) -> anyhow::Result<Vec<Txid>> {
    post_setup
        .wallet_service_client
        .list_sidechain_deposit_transactions(ListSidechainDepositTransactionsRequest {})
        .await?
        .into_owned()
        .transactions
        .into_iter()
        .map(|listed| {
            listed
                .tx
                .into_option()
                .ok_or_else(|| {
                    proto::Error::missing_field::<
                        list_sidechain_deposit_transactions_response::SidechainDepositTransaction,
                    >("tx")
                })?
                .txid
                .into_option()
                .ok_or_else(|| proto::Error::missing_field::<WalletTransaction>("txid"))?
                .decode::<WalletTransaction, Txid>("txid")
                .map_err(anyhow::Error::from)
        })
        .collect()
}

/// What `expected` has that the wallet's deposit history lacks.
fn missing_from_history(listed: &[Txid], expected: &[Txid]) -> Vec<Txid> {
    expected
        .iter()
        .copied()
        .filter(|txid| !listed.contains(txid))
        .collect()
}

/// Deposits the wallet makes into a sidechain that already has a treasury
/// show up in its deposit history, both while unconfirmed and once mined.
pub async fn test_wallet_deposit_history(mut post_setup: PostSetup) -> anyhow::Result<()> {
    let (_sidechain, sidechain_address) = activate_funded_sidechain(&mut post_setup).await?;
    let listed = listed_deposits(&post_setup).await?;
    anyhow::ensure!(
        listed.len() == 1,
        "expected just the funding deposit in the history, got {listed:?}"
    );
    let deposit_txid_1 = create_deposit(&mut post_setup, &sidechain_address, TREASURY).await?;
    let deposit_txid_2 = create_deposit(&mut post_setup, &sidechain_address, TREASURY).await?;
    let expected = [deposit_txid_1, deposit_txid_2];

    let unconfirmed_missing = missing_from_history(&listed_deposits(&post_setup).await?, &expected);
    let () = mine_one(&mut post_setup, MiningPolicy::SILENT)
        .await
        .map(drop)?;
    let () = wait_for_wallet_sync(&mut post_setup).await?;
    let listed = listed_deposits(&post_setup).await?;
    let mined_missing = missing_from_history(&listed, &expected);

    anyhow::ensure!(
        unconfirmed_missing.is_empty() && mined_missing.is_empty(),
        "the wallet's deposit history lacks deposits it made into an existing treasury. \
         Unconfirmed, missing {unconfirmed_missing:?}. After being mined, missing \
         {mined_missing:?}, and it lists {} deposit(s) in total.",
        listed.len()
    );
    Ok(())
}

/// What a transaction that spends a pending deposit's treasury output does with
/// the money.
#[derive(Clone, Copy)]
enum SpendOfPendingDeposit {
    /// Returns most of it to the treasury and keeps the rest.
    ReturnsLess,
    /// Takes all of it back, creating no treasury output. This costs the
    /// attacker one transaction fee: deposit, then spend the deposit.
    TakesItBack,
}

/// Anyone can relay a transaction that spends a pending deposit's treasury
/// output. The mempool checks each transaction on its own, where the deposit is
/// just another unknown input, so it accepts the spend. A block holding both
/// is invalid. Block production must still work with the spend in the mempool,
/// and still mine the deposit.
async fn check_block_production_survives_spend_of_pending_deposit(
    mut post_setup: PostSetup,
    spend: SpendOfPendingDeposit,
) -> anyhow::Result<()> {
    use bitcoin_jsonrpsee::client::BlockTemplateRequest;
    use cusf_enforcer_mempool::server::RpcClient as _;

    const SKIM: Amount = Amount::from_sat(5_000);
    const SPEND_TX_FEE: Amount = Amount::from_sat(2_000);
    let (_sidechain, sidechain_address) = activate_funded_sidechain(&mut post_setup).await?;
    let (_, treasury_before) = treasury_utxo(&mut post_setup).await?;
    let () = fund_core_wallet(&post_setup).await?;
    let op_drivechain = op_drivechain()?;

    let deposit_txid = create_deposit(&mut post_setup, &sidechain_address, TREASURY).await?;
    let deposit_tx: Transaction = {
        let hex: String = post_setup
            .bitcoind_client
            .request("getrawtransaction", rpc_params![deposit_txid])
            .await?;
        bitcoin::consensus::encode::deserialize_hex(&hex)?
    };
    let (treasury_vout, treasury_output) = deposit_tx
        .output
        .iter()
        .enumerate()
        .find(|(_, output)| op_drivechain.parse(output.script_pubkey.as_bytes()).is_ok())
        .ok_or_else(|| anyhow::anyhow!("the deposit has no treasury output"))?;
    let output = match spend {
        SpendOfPendingDeposit::ReturnsLess => vec![
            TxOut {
                script_pubkey: op_drivechain.script(DummySidechain::SIDECHAIN_NUMBER),
                value: treasury_output.value - SKIM - SPEND_TX_FEE,
            },
            deposit_address_output(),
            attacker_output(SKIM),
        ],
        SpendOfPendingDeposit::TakesItBack => {
            vec![attacker_output(treasury_output.value - SPEND_TX_FEE)]
        }
    };
    let spend_tx = Transaction {
        version: Version::TWO,
        lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
        input: vec![treasury_input(OutPoint {
            txid: deposit_txid,
            vout: treasury_vout as u32,
        })],
        output,
    };
    let spend_txid: Txid = post_setup
        .bitcoind_client
        .request(
            "sendrawtransaction",
            rpc_params![serialize_hex(&spend_tx), 0],
        )
        .await
        .map_err(|err| anyhow::anyhow!("bitcoind refused the spend of the deposit: {err:#}"))?;
    tracing::info!(%spend_txid, "Relayed a spend of the pending deposit");

    if matches!(post_setup.mode.mining_mode(), MiningMode::GetBlockTemplate) {
        // Transactions reach the enforcer's mirror in the order bitcoind
        // announces them, so a high-paying transaction sent last shows when it
        // has seen the spend.
        let canary = {
            let (utxo, value) = core_utxos(&post_setup.bitcoind_client, Amount::ONE_BTC, 1)
                .await?
                .remove(0);
            let unsigned = Transaction {
                version: Version::TWO,
                lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
                input: vec![treasury_input(utxo)],
                output: vec![TxOut {
                    script_pubkey: post_setup.mining_address.script_pubkey(),
                    value: value - Amount::from_sat(10_000),
                }],
            };
            sign_with_core_wallet(&post_setup.bitcoind_client, &unsigned).await?
        };
        let canary_txid: Txid = post_setup
            .bitcoind_client
            .request("sendrawtransaction", rpc_params![serialize_hex(&canary), 0])
            .await?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let mut request = BlockTemplateRequest::default();
            request.capabilities.insert("coinbasetxn".to_owned());
            let template = post_setup
                .gbt_client
                .get_block_template(request)
                .await
                .map_err(|err| {
                    anyhow::anyhow!(
                        "the enforcer cannot build a block template while the spend of the \
                         pending deposit {spend_txid} is in its mempool: {err:#}"
                    )
                })?
                .into_template()
                .ok_or_else(|| anyhow::anyhow!("expected a block template"))?;
            if template
                .transactions
                .iter()
                .any(|tx| tx.txid == canary_txid)
            {
                anyhow::ensure!(
                    template.transactions.iter().all(|tx| tx.txid != spend_txid),
                    "the template includes the spend of the pending deposit, an invalid block"
                );
                break;
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "the template never included the canary {canary_txid}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    } else {
        // Self-mining has no template to poll, so give the mirror time to see
        // the spend.
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    let () = mine::<DummySidechain>(&mut post_setup, 1, MiningPolicy::SILENT)
        .await
        .map_err(|err| {
            anyhow::anyhow!(
                "the enforcer cannot mine a block while the spend of the pending deposit \
                 {spend_txid} is in its mempool: {err:#}"
            )
        })?;
    let (_, treasury_after) = treasury_utxo(&mut post_setup).await?;
    anyhow::ensure!(
        treasury_after == treasury_before + TREASURY,
        "treasury {treasury_after}, expected the deposit to be mined for {}",
        treasury_before + TREASURY
    );
    Ok(())
}

pub async fn test_mempool_spend_of_pending_deposit(post_setup: PostSetup) -> anyhow::Result<()> {
    check_block_production_survives_spend_of_pending_deposit(
        post_setup,
        SpendOfPendingDeposit::ReturnsLess,
    )
    .await
}

pub async fn test_mempool_take_back_of_pending_deposit(
    post_setup: PostSetup,
) -> anyhow::Result<()> {
    check_block_production_survives_spend_of_pending_deposit(
        post_setup,
        SpendOfPendingDeposit::TakesItBack,
    )
    .await
}

/// The same, mining with the enforcer's own miner instead of a block template.
pub async fn test_self_mining_with_take_back_of_pending_deposit(
    post_setup: PostSetup,
) -> anyhow::Result<()> {
    check_block_production_survives_spend_of_pending_deposit(
        post_setup,
        SpendOfPendingDeposit::TakesItBack,
    )
    .await
}

/// A block holding a deposit and the transaction consolidating it is reorged
/// out. The treasury returns to what it was, and the deposit, back in the
/// mempool, can be mined again with a fresh consolidation.
pub async fn test_reorg_of_consolidated_deposit(mut post_setup: PostSetup) -> anyhow::Result<()> {
    let (_sidechain, sidechain_address) = activate_funded_sidechain(&mut post_setup).await?;
    let (_, treasury_before) = treasury_utxo(&mut post_setup).await?;

    let deposit_txid = create_deposit(&mut post_setup, &sidechain_address, TREASURY).await?;
    let () = crate::mine::wait_for_tx_in_block_template(&post_setup, &deposit_txid).await?;
    let () = mine::<DummySidechain>(&mut post_setup, 1, MiningPolicy::SILENT).await?;
    let (_, treasury_after) = treasury_utxo(&mut post_setup).await?;
    anyhow::ensure!(
        treasury_after == treasury_before + TREASURY,
        "treasury {treasury_after} after the deposit, expected {}",
        treasury_before + TREASURY
    );

    // Replace the block with a different one, as the template would rebuild
    // the block just invalidated, which bitcoind refuses as `duplicate-invalid`.
    let mined = best_block_hash(&post_setup.bitcoind_client).await?;
    let () = invalidate_block(&post_setup.bitcoind_client, mined).await?;
    let replacement = generate_empty_block(
        &post_setup.bitcoind_client,
        &post_setup.mining_address.to_string(),
    )
    .await?;
    let () = wait_for_enforcer_tip_hash(&post_setup, replacement).await?;
    let (_, treasury_reorged) = treasury_utxo(&mut post_setup).await?;
    anyhow::ensure!(
        treasury_reorged == treasury_before,
        "treasury {treasury_reorged} after the reorg, expected {treasury_before}"
    );

    // The deposit is back in the mempool and the enforcer can mine it again.
    let () = crate::mine::wait_for_tx_in_block_template(&post_setup, &deposit_txid)
        .await
        .map_err(|err| anyhow::anyhow!("the deposit did not return to the template: {err:#}"))?;
    let () = mine::<DummySidechain>(&mut post_setup, 1, MiningPolicy::SILENT).await?;
    let (_, treasury_remined) = treasury_utxo(&mut post_setup).await?;
    anyhow::ensure!(
        treasury_remined == treasury_before + TREASURY,
        "treasury {treasury_remined} after mining the deposit again, expected {}",
        treasury_before + TREASURY
    );
    Ok(())
}

/// The rule that makes aggregated deposits safe: a block that deposits into an
/// existing treasury and does not consolidate the deposit is invalid.
pub async fn test_block_without_consolidation_is_rejected(
    mut post_setup: PostSetup,
) -> anyhow::Result<()> {
    let (_sidechain, _sidechain_address) = activate_funded_sidechain(&mut post_setup).await?;
    let (_, treasury) = treasury_utxo(&mut post_setup).await?;
    let () = fund_core_wallet(&post_setup).await?;
    let deposit_tx = signed_core_deposit(&post_setup, Amount::from_sat(10_000)).await?;
    let block_hash = generate_block(&post_setup, &[&deposit_tx]).await?;
    assert_enforcer_verdict(
        &mut post_setup,
        block_hash,
        Expect::Rejected {
            log_contains: "unconsolidated ctip",
        },
        Duration::from_secs(10),
    )
    .await?;
    let (_, treasury_after) = treasury_utxo(&mut post_setup).await?;
    anyhow::ensure!(
        treasury_after == treasury,
        "the rejected block changed the treasury from {treasury} to {treasury_after}"
    );
    Ok(())
}

/// A deposit waiting in the mempool when the enforcer restarts is accepted
/// again and mined with its consolidation.
pub async fn test_restart_with_pending_deposit(mut post_setup: PostSetup) -> anyhow::Result<()> {
    let (_sidechain, sidechain_address) = activate_funded_sidechain(&mut post_setup).await?;
    let (_, treasury_before) = treasury_utxo(&mut post_setup).await?;
    let deposit_txid = create_deposit(&mut post_setup, &sidechain_address, TREASURY).await?;
    let () = crate::mine::wait_for_tx_in_block_template(&post_setup, &deposit_txid).await?;

    let (res_tx, _res_rx) = mpsc::unbounded();
    let () = post_setup
        .restart_enforcer(&crate::util::BinPaths::new(), Vec::<String>::new(), res_tx)
        .await?;
    let () = crate::mine::wait_for_tx_in_block_template(&post_setup, &deposit_txid)
        .await
        .map_err(|err| {
            anyhow::anyhow!("the deposit is not in the template after a restart: {err:#}")
        })?;
    let () = mine::<DummySidechain>(&mut post_setup, 1, MiningPolicy::SILENT).await?;
    let (_, treasury_after) = treasury_utxo(&mut post_setup).await?;
    anyhow::ensure!(
        treasury_after == treasury_before + TREASURY,
        "treasury {treasury_after} after mining the deposit, expected {}",
        treasury_before + TREASURY
    );
    Ok(())
}

/// A small deposit into a sidechain that already has a treasury. The treasury
/// output used to hold the old treasury plus the deposit, which is far above
/// the dust limit whatever the deposit. It now holds the deposit alone.
pub async fn test_small_deposit_into_existing_treasury(
    mut post_setup: PostSetup,
) -> anyhow::Result<()> {
    const SMALL: Amount = Amount::from_sat(300);
    let (_sidechain, sidechain_address) = activate_funded_sidechain(&mut post_setup).await?;
    let (_, treasury_before) = treasury_utxo(&mut post_setup).await?;
    let deposit_txid = post_setup
        .wallet_service_client
        .create_deposit_transaction(CreateDepositTransactionRequest {
            sidechain_id: proto::wrap_u32(DummySidechain::SIDECHAIN_NUMBER.0.into()),
            address: proto::wrap_string(&sidechain_address),
            value_sats: proto::wrap_u64(SMALL.to_sat()),
            fee_sats: proto::wrap_u64(DEPOSIT_FEE.to_sat()),
        })
        .await
        .map_err(|err| {
            anyhow::anyhow!(
                "a deposit of {SMALL} into a sidechain with a treasury of {treasury_before} was \
                 refused: {err:#}"
            )
        })?
        .into_owned()
        .txid
        .ok_or_else(|| proto::Error::missing_field::<CreateDepositTransactionResponse>("txid"))?
        .decode::<CreateDepositTransactionResponse, Txid>("txid")?;
    let () = wait_for_tx_in_mempool(&post_setup.bitcoind_client, &deposit_txid).await?;
    Ok(())
}

/// A payout returned to the mempool by a reorg is the very transaction block
/// production rebuilds for the still-approved bundle, so the template holds it
/// once. With a deposit pending, the rebuilt payout also spends the deposit,
/// so it is a different transaction paying the same bundle from the same
/// treasury UTXO.
pub async fn test_returned_payout_with_pending_deposit(
    mut post_setup: PostSetup,
) -> anyhow::Result<()> {
    const PAYOUT: Amount = Amount::from_sat(60_000);
    const PAYOUT_FEE: Amount = Amount::from_sat(1_000);
    let (mut sidechain, sidechain_address) = activate_funded_sidechain(&mut post_setup).await?;
    let m6id = approve_withdrawal(&mut post_setup, &mut sidechain, PAYOUT, PAYOUT_FEE).await?;

    let parent = best_block_hash(&post_setup.bitcoind_client).await?;
    let events = mine_one(&mut post_setup, MiningPolicy::VOTE).await?;
    anyhow::ensure!(
        paid_out(&events, m6id)?,
        "the payout block did not pay the bundle"
    );
    let paid_in = best_block_hash(&post_setup.bitcoind_client).await?;
    let paid_block: serde_json::Value = post_setup
        .bitcoind_client
        .request("getblock", rpc_params![paid_in, 1])
        .await?;
    let m6_txids: Vec<Txid> = paid_block["tx"]
        .as_array()
        .into_iter()
        .flatten()
        .skip(1)
        .filter_map(|txid| txid.as_str()?.parse().ok())
        .collect();
    let [m6_txid] = m6_txids.as_slice() else {
        anyhow::bail!("expected just the payout in the block, got {m6_txids:?}");
    };

    // The payout returns to the mempool and the bundle is payable again.
    let () = invalidate_block(&post_setup.bitcoind_client, paid_in).await?;
    let replacement = generate_empty_block(
        &post_setup.bitcoind_client,
        &post_setup.mining_address.to_string(),
    )
    .await?;
    let () = wait_for_enforcer_tip_hash(&post_setup, replacement).await?;
    anyhow::ensure!(
        pending_bundle_count(
            &post_setup.validator_service_client,
            DummySidechain::SIDECHAIN_NUMBER
        )
        .await?
            == 1,
        "the bundle should be pending again after the reorg (parent {parent})"
    );
    let m6_txid = *m6_txid;
    let () = crate::setup::wait_until("the payout to return to the mempool", || async {
        let mempool: Vec<Txid> = post_setup
            .bitcoind_client
            .request("getrawmempool", rpc_params![])
            .await?;
        Ok(mempool.contains(&m6_txid))
    })
    .await?;

    let deposit_txid = create_deposit(&mut post_setup, &sidechain_address, TREASURY).await?;
    let () = crate::mine::wait_for_tx_in_block_template(&post_setup, &deposit_txid)
        .await
        .map_err(|err| anyhow::anyhow!("the deposit never reached the template: {err:#}"))?;
    let events = mine_one(&mut post_setup, MiningPolicy::VOTE)
        .await
        .map_err(|err| {
            anyhow::anyhow!(
                "the enforcer cannot mine the returned payout with a pending deposit: {err:#}"
            )
        })?;
    anyhow::ensure!(paid_out(&events, m6id)?, "the bundle was not paid");
    anyhow::ensure!(
        received_by_address(&post_setup).await? == PAYOUT,
        "the bundle paid out other than once"
    );
    Ok(())
}
