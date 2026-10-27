//! A block template packed to the weight limit, holding a deposit whose
//! consolidation the mempool's weight reservation does not cover.
//!
//! A deposit into an existing treasury leaves the treasury UTXO unspent, so
//! block production appends a consolidation transaction that spends both. The
//! mempool reserves weight for it when it accepts the deposit, and the
//! template is filled up to what is left. A reservation smaller than the
//! consolidation that really gets built makes a template that is over the
//! limit, which bitcoind refuses.
//!
//! The trial fills the mempool with filler transactions, measures how much
//! room the template has left, and then adds one more filler that is too big
//! for that room by less than the shortfall. A sufficient reservation keeps
//! that filler out. An insufficient one lets it in and overflows the block.

use bip300301_enforcer_lib::{
    messages::M8BmmRequest,
    proto::{
        self,
        mainchain::{CreateDepositTransactionRequest, GetSidechainsRequest},
    },
    types::{BmmCommitment, SidechainNumber},
};
use bitcoin::{
    Amount, OutPoint, ScriptBuf, Transaction, TxIn, TxOut, Txid, consensus::encode::serialize_hex,
    script::PushBytesBuf, transaction::Version,
};
use bitcoin_jsonrpsee::client::{BlockTemplateRequest, CoinbaseTxnOrValue};
use cusf_enforcer_mempool::server::RpcClient as _;
use futures::channel::mpsc;
use jsonrpsee::{core::client::ClientT as _, rpc_params};

use crate::{
    block_verdict::wait_for_enforcer_height,
    integration_test::{
        activate_sidechain, deposit, fund_enforcer, propose_sidechain, propose_sidechain_for_slot,
    },
    mine::{MiningPolicy, mine, wait_for_tx_in_block_template},
    setup::{
        DummySidechain, PostSetup, Sidechain as _, best_block_hash, core_utxos, op_drivechain,
        sign_with_core_wallet, wait_for_tx_in_mempool,
    },
    util::expect_block_template,
};

const MAX_BLOCK_WEIGHT: u64 = 4_000_000;

/// Outputs in each of the large fillers that take up most of the block.
const LARGE_FILLER_OUTPUTS: usize = 3_000;
const LARGE_FILLERS: usize = 10;
/// Fee rates, in sat/vB, that order the filler groups: the canary is picked
/// first, then the large fillers, and the last filler goes in last.
const CANARY_RATE: u64 = 50;
const LARGE_FILLER_RATE: u64 = 10;
const LAST_FILLER_RATE: u64 = 2;
/// Weight of a P2WPKH output, which is what each filler output adds.
const OUTPUT_WEIGHT: u64 = 124;
/// How far past the room left in the template the last filler is sized. It
/// must be inside the smallest shortfall under test, and wider than the
/// weight jitter from signature lengths.
const OVERSHOOT: u64 = 180;

const DEPOSIT_VALUE: Amount = Amount::from_sat(10_000);
const DEPOSIT_TX_FEE: Amount = Amount::from_sat(40_000);

async fn mine_to_core(post_setup: &PostSetup, blocks: u32) -> anyhow::Result<()> {
    let _block_hashes: Vec<bitcoin::BlockHash> = post_setup
        .bitcoind_client
        .request(
            "generatetoaddress",
            rpc_params![blocks, post_setup.mining_address.to_string()],
        )
        .await?;
    let height: u32 = post_setup
        .bitcoind_client
        .request("getblockcount", rpc_params![])
        .await?;
    wait_for_enforcer_height(post_setup, height).await
}

/// Deposit from the enforcer wallet and mine it, so `slot` has a treasury.
async fn fund_treasury(post_setup: &mut PostSetup, slot: SidechainNumber) -> anyhow::Result<()> {
    let txid = post_setup
        .wallet_service_client
        .create_deposit_transaction(CreateDepositTransactionRequest {
            sidechain_id: proto::wrap_u32(slot.0.into()),
            address: proto::wrap_string("sidechain address"),
            value_sats: proto::wrap_u64(1_000_000),
            fee_sats: proto::wrap_u64(10_000),
        })
        .await?
        .into_owned()
        .txid
        .ok_or_else(|| {
            proto::Error::missing_field::<proto::mainchain::CreateDepositTransactionResponse>(
                "txid",
            )
        })?
        .decode::<proto::mainchain::CreateDepositTransactionResponse, Txid>("txid")?;
    let () = wait_for_tx_in_mempool(&post_setup.bitcoind_client, &txid).await?;
    let () = mine::<DummySidechain>(post_setup, 1, MiningPolicy::SILENT).await?;
    Ok(())
}

/// Activate [`DummySidechain`] and give it a treasury. The sidechain must be
/// kept alive.
async fn activate_funded_sidechain(post_setup: &mut PostSetup) -> anyhow::Result<DummySidechain> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let mut sidechain = DummySidechain::setup((), post_setup, res_tx).await?;
    let () = propose_sidechain::<DummySidechain>(post_setup).await?;
    let () = activate_sidechain::<DummySidechain>(post_setup).await?;
    let () = fund_enforcer::<DummySidechain>(post_setup).await?;
    let address = sidechain.get_deposit_address().await?;
    let () = deposit(
        post_setup,
        &mut sidechain,
        &address,
        Amount::from_sat(1_000_000),
        Amount::from_sat(10_000),
    )
    .await?;
    Ok(sidechain)
}

/// Activate a second sidechain in `slot` and give it a treasury.
async fn activate_second_funded_sidechain(
    post_setup: &mut PostSetup,
    slot: SidechainNumber,
) -> anyhow::Result<()> {
    let () =
        propose_sidechain_for_slot::<DummySidechain>(post_setup, slot, "second sidechain").await?;
    // Past the activation threshold of an unused slot.
    let () = mine::<DummySidechain>(post_setup, 7, MiningPolicy::VOTE).await?;
    let active = post_setup
        .validator_service_client
        .get_sidechains(GetSidechainsRequest::default())
        .await?
        .into_owned()
        .sidechains
        .len();
    anyhow::ensure!(active == 2, "expected 2 active sidechains, got {active}");
    fund_treasury(post_setup, slot).await
}

fn deposit_address_output() -> TxOut {
    TxOut {
        script_pubkey: ScriptBuf::new_op_return(PushBytesBuf::from(*b"sidechain address")),
        value: Amount::ZERO,
    }
}

/// A deposit into each of `slots`, funded by `utxo`, with `first_output` ahead
/// of them if given.
async fn signed_deposit_tx(
    post_setup: &PostSetup,
    utxo: (OutPoint, Amount),
    first_output: Option<TxOut>,
    slots: &[SidechainNumber],
) -> anyhow::Result<Transaction> {
    let op_drivechain = op_drivechain()?;
    let (previous_output, value) = utxo;
    let mut output: Vec<TxOut> = first_output.into_iter().collect();
    for slot in slots {
        output.push(TxOut {
            script_pubkey: op_drivechain.script(*slot),
            value: DEPOSIT_VALUE,
        });
        output.push(deposit_address_output());
    }
    output.push(TxOut {
        script_pubkey: post_setup.mining_address.script_pubkey(),
        value: value - DEPOSIT_VALUE * slots.len() as u64 - DEPOSIT_TX_FEE,
    });
    let unsigned = Transaction {
        version: Version::TWO,
        lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output,
            ..TxIn::default()
        }],
        output,
    };
    sign_with_core_wallet(&post_setup.bitcoind_client, &unsigned).await
}

async fn broadcast(post_setup: &PostSetup, tx: &Transaction) -> anyhow::Result<Txid> {
    Ok(post_setup
        .bitcoind_client
        .request("sendrawtransaction", rpc_params![serialize_hex(tx), 0])
        .await?)
}

/// A signed transaction spending `utxo` into `n_outputs` small P2WPKH outputs
/// and change, paying `sat_per_vb`. Pays a little more than that, as the fee
/// is set from an upper bound on the size.
async fn signed_filler(
    post_setup: &PostSetup,
    utxo: (OutPoint, Amount),
    n_outputs: usize,
    sat_per_vb: u64,
) -> anyhow::Result<Transaction> {
    const FILLER_OUTPUT_VALUE: Amount = Amount::from_sat(1_000);
    let (previous_output, value) = utxo;
    let script_pubkey = post_setup.mining_address.script_pubkey();
    let fee = Amount::from_sat(sat_per_vb * (n_outputs as u64 * 31 + 300));
    let change = value - FILLER_OUTPUT_VALUE * n_outputs as u64 - fee;
    let output = std::iter::repeat_n(
        TxOut {
            script_pubkey: script_pubkey.clone(),
            value: FILLER_OUTPUT_VALUE,
        },
        n_outputs,
    )
    .chain([TxOut {
        script_pubkey,
        value: change,
    }])
    .collect();
    let unsigned = Transaction {
        version: Version::TWO,
        lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output,
            ..TxIn::default()
        }],
        output,
    };
    sign_with_core_wallet(&post_setup.bitcoind_client, &unsigned).await
}

/// A signed filler that weighs `target_wu`, give or take half an output.
async fn signed_filler_of_weight(
    post_setup: &PostSetup,
    utxo: (OutPoint, Amount),
    target_wu: u64,
    sat_per_vb: u64,
) -> anyhow::Result<Transaction> {
    let mut n_outputs = (target_wu / OUTPUT_WEIGHT).saturating_sub(3) as i64;
    for _ in 0..5 {
        let tx = signed_filler(post_setup, utxo, n_outputs as usize, sat_per_vb).await?;
        let off = target_wu as i64 - tx.weight().to_wu() as i64;
        let outputs_off = (off as f64 / OUTPUT_WEIGHT as f64).round() as i64;
        if outputs_off == 0 {
            return Ok(tx);
        }
        n_outputs += outputs_off;
    }
    anyhow::bail!("could not size a filler to {target_wu} WU")
}

struct EnforcerTemplate {
    coinbase: Transaction,
    txs: Vec<Transaction>,
}

impl EnforcerTemplate {
    fn weight(&self) -> u64 {
        let compact_size_len = |n: u64| match n {
            0..=0xfc => 1,
            0xfd..=0xffff => 3,
            _ => 5,
        };
        let header_and_count =
            (bitcoin::block::Header::SIZE as u64 + compact_size_len(self.txs.len() as u64 + 1)) * 4;
        header_and_count
            + self.coinbase.weight().to_wu()
            + self.txs.iter().map(|tx| tx.weight().to_wu()).sum::<u64>()
    }

    fn contains(&self, txid: Txid) -> bool {
        self.txs.iter().any(|tx| tx.compute_txid() == txid)
    }
}

async fn enforcer_template(post_setup: &PostSetup) -> anyhow::Result<EnforcerTemplate> {
    let mut request = BlockTemplateRequest::default();
    request.capabilities.insert("coinbasetxn".to_owned());
    let template = expect_block_template(post_setup.gbt_client.get_block_template(request).await?)?;
    let CoinbaseTxnOrValue::Txn(coinbase) = template.coinbase_txn_or_value else {
        anyhow::bail!("the template has no coinbase transaction");
    };
    let txs = template
        .transactions
        .iter()
        .map(|tx| bitcoin::consensus::deserialize(&tx.data))
        .collect::<Result<_, _>>()?;
    Ok(EnforcerTemplate {
        coinbase: bitcoin::consensus::deserialize(&coinbase.data)?,
        txs,
    })
}

/// Pack the enforcer's template to within the shortfall of the weight limit
/// with `deposit_txid` in it, and check that the block it makes is valid.
async fn check_packed_template_fits(
    post_setup: &mut PostSetup,
    deposit_txid: Txid,
    utxos: Vec<(OutPoint, Amount)>,
) -> anyhow::Result<()> {
    let mut utxos = utxos.into_iter();
    let mut next_utxo = || utxos.next().ok_or_else(|| anyhow::anyhow!("out of UTXOs"));

    // Most of the block, in transactions that fit whatever the reservation is.
    let mut last_large_filler = None;
    for _ in 0..LARGE_FILLERS {
        let tx = signed_filler(
            post_setup,
            next_utxo()?,
            LARGE_FILLER_OUTPUTS,
            LARGE_FILLER_RATE,
        )
        .await?;
        last_large_filler = Some(broadcast(post_setup, &tx).await?);
    }
    let last_large_filler = last_large_filler.expect("at least one large filler");
    let () = wait_for_tx_in_block_template(post_setup, &deposit_txid).await?;
    let () = wait_for_tx_in_block_template(post_setup, &last_large_filler).await?;

    // The room left, measured on the real block with its real consolidation.
    let template = enforcer_template(post_setup).await?;
    anyhow::ensure!(
        template.txs.len() > LARGE_FILLERS + 1,
        "the template holds {} transactions, expected the deposit, its \
         consolidation and {LARGE_FILLERS} fillers",
        template.txs.len()
    );
    anyhow::ensure!(
        template.txs.iter().any(|tx| tx
            .input
            .iter()
            .any(|input| input.previous_output.txid == deposit_txid)),
        "the template has no transaction consolidating the deposit, so this would test nothing"
    );
    let room = MAX_BLOCK_WEIGHT
        .checked_sub(template.weight())
        .ok_or_else(|| anyhow::anyhow!("the template is already over the limit"))?;
    tracing::info!(room, "Room left in the template before the last filler");
    anyhow::ensure!(
        (2_000..380_000).contains(&room),
        "room {room} cannot be filled by a single filler"
    );

    // The canary goes in ahead of everything, and after the last filler in
    // the order bitcoind announces them, so seeing it in the template means
    // the mirror has seen the last filler too.
    let canary = signed_filler(post_setup, next_utxo()?, 0, CANARY_RATE).await?;
    let canary_weight = canary.weight().to_wu();
    let last_filler = signed_filler_of_weight(
        post_setup,
        next_utxo()?,
        room - canary_weight + OVERSHOOT,
        LAST_FILLER_RATE,
    )
    .await?;
    let last_filler_weight = last_filler.weight().to_wu();
    anyhow::ensure!(
        (room - canary_weight + 30..room - canary_weight + 340).contains(&last_filler_weight),
        "the last filler weighs {last_filler_weight}, out of reach of room {room} past the canary"
    );
    let last_filler_txid = broadcast(post_setup, &last_filler).await?;
    let canary_txid = broadcast(post_setup, &canary).await?;
    let () = wait_for_tx_in_block_template(post_setup, &canary_txid).await?;

    let template = enforcer_template(post_setup).await?;
    let weight = template.weight();
    let included = template.contains(last_filler_txid);
    tracing::info!(weight, included, "Template with the last filler offered");
    let mined = mine::<DummySidechain>(post_setup, 1, MiningPolicy::SILENT).await;
    anyhow::ensure!(
        weight <= MAX_BLOCK_WEIGHT && mined.is_ok(),
        "the template weighs {weight}, {} over the {MAX_BLOCK_WEIGHT} limit, because the last \
         filler ({last_filler_weight} WU, offered {} WU of room) was let in on a reservation \
         for the consolidation that is too small. bitcoind's verdict on the block: {mined:?}",
        weight.saturating_sub(MAX_BLOCK_WEIGHT),
        room - canary_weight,
    );
    Ok(())
}

/// A transaction that is both a BMM request and a deposit into an existing
/// treasury gets the weight reserved for the BMM accept, and nothing for the
/// consolidation.
pub async fn test_m8_deposit_template_weight(mut post_setup: PostSetup) -> anyhow::Result<()> {
    let _sidechain = activate_funded_sidechain(&mut post_setup).await?;
    let () = mine_to_core(&post_setup, 120).await?;
    let mut utxos = core_utxos(&post_setup.bitcoind_client, Amount::ONE_BTC, 13).await?;

    let tip = best_block_hash(&post_setup.bitcoind_client).await?;
    let bmm_request = TxOut {
        script_pubkey: M8BmmRequest::script_pubkey(
            DummySidechain::SIDECHAIN_NUMBER,
            BmmCommitment([0xD8; 32]),
            tip,
        )?,
        value: Amount::ZERO,
    };
    let deposit_tx = signed_deposit_tx(
        &post_setup,
        utxos.remove(0),
        Some(bmm_request),
        &[DummySidechain::SIDECHAIN_NUMBER],
    )
    .await?;
    let deposit_txid = broadcast(&post_setup, &deposit_tx).await?;
    check_packed_template_fits(&mut post_setup, deposit_txid, utxos).await
}

/// One transaction depositing into two existing treasuries needs a
/// consolidation with four inputs and two outputs, and gets the reservation
/// for two inputs and one output.
pub async fn test_two_treasury_deposit_template_weight(
    mut post_setup: PostSetup,
) -> anyhow::Result<()> {
    const SECOND_SLOT: SidechainNumber = SidechainNumber(1);
    let _sidechain = activate_funded_sidechain(&mut post_setup).await?;
    let () = activate_second_funded_sidechain(&mut post_setup, SECOND_SLOT).await?;
    let () = mine_to_core(&post_setup, 120).await?;
    let mut utxos = core_utxos(&post_setup.bitcoind_client, Amount::ONE_BTC, 13).await?;

    let deposit_tx = signed_deposit_tx(
        &post_setup,
        utxos.remove(0),
        None,
        &[DummySidechain::SIDECHAIN_NUMBER, SECOND_SLOT],
    )
    .await?;
    let deposit_txid = broadcast(&post_setup, &deposit_tx).await?;
    check_packed_template_fits(&mut post_setup, deposit_txid, utxos).await
}
