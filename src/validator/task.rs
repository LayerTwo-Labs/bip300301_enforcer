use std::collections::HashSet;

use async_broadcast::{Sender, TrySendError};
use bip300301::{
    client::{GetBlockClient, U8Witness},
    jsonrpsee, MainClient,
};
use bip300301_messages::{
    bitcoin::{
        self, consensus::Encodable, hashes::Hash, opcodes::all::OP_RETURN, Amount, Block,
        BlockHash, OutPoint, Transaction, TxOut,
    },
    m6_to_id, parse_coinbase_script, parse_m8_bmm_request, parse_op_drivechain, sha256d,
    CoinbaseMessage, M4AckBundles, ABSTAIN_TWO_BYTES, ALARM_TWO_BYTES,
};
use bitcoin::Work;
use either::Either;
use fallible_iterator::FallibleIterator;
use futures::{TryFutureExt as _, TryStreamExt as _};
use hashlink::{LinkedHashMap, LinkedHashSet};
use heed::RoTxn;
use thiserror::Error;

use crate::{
    messages::CoinbaseBuilder,
    types::{
        BlockInfo, BmmCommitments, Ctip, Deposit, Event, Hash256, HeaderInfo, PendingM6id,
        Sidechain, SidechainNumber, SidechainProposal, TreasuryUtxo, WithdrawalBundleEvent,
        WithdrawalBundleEventKind,
    },
    zmq::SequenceMessage,
};

use super::dbs::{
    self, db_error, CommitWriteTxnError, Dbs, ReadTxnError, RwTxn, UnitKey, WriteTxnError,
};

const WITHDRAWAL_BUNDLE_MAX_AGE: u16 = 10;
const WITHDRAWAL_BUNDLE_INCLUSION_THRESHOLD: u16 = WITHDRAWAL_BUNDLE_MAX_AGE / 2; // 5

const USED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE: u16 = WITHDRAWAL_BUNDLE_MAX_AGE; // 5
const USED_SIDECHAIN_SLOT_ACTIVATION_THRESHOLD: u16 = USED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE / 2;

const UNUSED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE: u16 = 10;
const UNUSED_SIDECHAIN_SLOT_ACTIVATION_MAX_FAILS: u16 = 5;
const UNUSED_SIDECHAIN_SLOT_ACTIVATION_THRESHOLD: u16 =
    UNUSED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE - UNUSED_SIDECHAIN_SLOT_ACTIVATION_MAX_FAILS;

#[derive(Debug, Error)]
enum HandleM1ProposeSidechainError {
    #[error(transparent)]
    DbPut(#[from] db_error::Put),
    #[error(transparent)]
    DbTryGet(#[from] db_error::TryGet),
}

#[allow(clippy::enum_variant_names)]
#[derive(Debug, Error)]
enum HandleM2AckSidechainError {
    #[error(transparent)]
    DbDelete(#[from] db_error::Delete),
    #[error(transparent)]
    DbPut(#[from] db_error::Put),
    #[error(transparent)]
    DbTryGet(#[from] db_error::TryGet),
}

#[allow(clippy::enum_variant_names)]
#[derive(Debug, Error)]
enum HandleFailedSidechainProposalsError {
    #[error(transparent)]
    DbDelete(#[from] db_error::Delete),
    #[error(transparent)]
    DbIter(#[from] db_error::Iter),
    #[error(transparent)]
    DbTryGet(#[from] db_error::TryGet),
}

#[derive(Debug, Error)]
enum HandleM3ProposeBundleError {
    #[error(transparent)]
    DbPut(#[from] db_error::Put),
    #[error(transparent)]
    DbTryGet(#[from] db_error::TryGet),
    #[error(
        "Cannot propose bundle; sidechain slot {} is inactive",
        .sidechain_number.0
    )]
    InactiveSidechain { sidechain_number: SidechainNumber },
}

#[derive(Debug, Error)]
enum HandleM4VotesError {
    #[error(transparent)]
    DbPut(#[from] db_error::Put),
    #[error(transparent)]
    DbTryGet(#[from] db_error::TryGet),
}

#[derive(Debug, Error)]
enum HandleM4AckBundlesError {
    #[error("Error handling M4 Votes")]
    Votes(#[from] HandleM4VotesError),
}

#[derive(Debug, Error)]
enum HandleFailedM6IdsError {
    #[error(transparent)]
    DbIter(#[from] db_error::Iter),
    #[error(transparent)]
    DbPut(#[from] db_error::Put),
}

#[derive(Debug, Error)]
enum HandleM5M6Error {
    #[error(transparent)]
    DbPut(#[from] db_error::Put),
    #[error(transparent)]
    DbTryGet(#[from] db_error::TryGet),
    #[error("Invalid M6")]
    InvalidM6,
    #[error("Old Ctip for sidechain {} is unspent", .sidechain_number.0)]
    OldCtipUnspent { sidechain_number: SidechainNumber },
}

#[derive(Debug, Error)]
enum HandleM8Error {
    #[error("BMM request expired")]
    BmmRequestExpired,
    #[error("Cannot include BMM request; not accepted by miners")]
    NotAcceptedByMiners,
}

#[derive(Debug, Error)]
enum ConnectBlockError {
    #[error(transparent)]
    PutBlockInfo(#[from] dbs::block_hash_dbs_error::PutBlockInfo),
    #[error(transparent)]
    DbDelete(#[from] db_error::Delete),
    #[error(transparent)]
    DbFirst(#[from] db_error::First),
    #[error(transparent)]
    DbGet(#[from] db_error::Get),
    #[error(transparent)]
    DbLen(#[from] db_error::Len),
    #[error(transparent)]
    DbPut(#[from] db_error::Put),
    #[error(transparent)]
    DbTryGet(#[from] db_error::TryGet),
    #[error("Error handling failed M6IDs")]
    FailedM6Ids(#[from] HandleFailedM6IdsError),
    #[error("Error handling failed sidechain proposals")]
    FailedSidechainProposals(#[from] HandleFailedSidechainProposalsError),
    #[error("Error handling M1 (propose sidechain)")]
    M1ProposeSidechain(#[from] HandleM1ProposeSidechainError),
    #[error("Error handling M2 (ack sidechain)")]
    M2AckSidechain(#[from] HandleM2AckSidechainError),
    #[error("Error handling M3 (propose bundle)")]
    M3ProposeBundle(#[from] HandleM3ProposeBundleError),
    #[error("Error handling M4 (ack bundles)")]
    M4AckBundles(#[from] HandleM4AckBundlesError),
    #[error("Error handling M5/M6")]
    M5M6(#[from] HandleM5M6Error),
    #[error("Error handling M8")]
    M8(#[from] HandleM8Error),
    #[error("Multiple blocks BMM'd in sidechain slot {}", .sidechain_number.0)]
    MultipleBmmBlocks { sidechain_number: SidechainNumber },
}

#[derive(Debug, Error)]
enum DisconnectBlockError {}

#[derive(Debug, Error)]
enum TxValidationError {}

#[derive(Debug, Error)]
enum SyncError {
    #[error(transparent)]
    CommitWriteTxn(#[from] CommitWriteTxnError),
    #[error("Failed to connect block")]
    ConnectBlock(#[from] ConnectBlockError),
    #[error(transparent)]
    DbGet(#[from] db_error::Get),
    #[error(transparent)]
    DbPut(#[from] db_error::Put),
    #[error(transparent)]
    DbTryGet(#[from] db_error::TryGet),
    #[error("JSON RPC error (`{method}`)")]
    JsonRpc {
        method: String,
        source: jsonrpsee::core::ClientError,
    },
    #[error(transparent)]
    ReadTxn(#[from] ReadTxnError),
    #[error(transparent)]
    WriteTxn(#[from] WriteTxnError),
}

// Returns the serialized sidechain proposal OP_RETURN output.
fn create_sidechain_proposal(
    sidechain_number: SidechainNumber,
    version: u8,
    title: String,
    description: String,
    hash_1: Vec<u8>,
    hash_2: Vec<u8>,
) -> anyhow::Result<Vec<u8>> {
    const HASH_1_LENGTH: usize = 32;
    const HASH_2_LENGTH: usize = 20;

    let hash_1: Vec<u8> = if hash_1.is_empty() {
        vec![0u8; HASH_1_LENGTH]
    } else if hash_1.len() == HASH_1_LENGTH {
        hash_1
    } else {
        return Err(anyhow::anyhow!(
            "hash_1 must be empty or {HASH_1_LENGTH} bytes"
        ));
    };

    let hash_2: Vec<u8> = if hash_2.is_empty() {
        vec![0u8; HASH_2_LENGTH]
    } else if hash_2.len() == HASH_2_LENGTH {
        hash_2
    } else {
        return Err(anyhow::anyhow!(
            "hash_2 must be empty or {HASH_2_LENGTH} bytes"
        ));
    };

    let mut data: Vec<u8> = vec![];
    data.push(sidechain_number.into());
    data.push(version);
    data.extend_from_slice(title.as_bytes());
    data.extend_from_slice(description.as_bytes());
    data.extend_from_slice(&hash_1);
    data.extend_from_slice(&hash_2);

    let builder = CoinbaseBuilder::new()
        .propose_sidechain(sidechain_number.into(), &data)
        .build();

    let tx_out = builder.first().unwrap();

    let mut buffer = Vec::new();
    match tx_out.consensus_encode(&mut buffer) {
        Ok(_) => (),
        Err(e) => {
            return Err(anyhow::anyhow!("Error encoding tx_out: {}", e));
        }
    }

    Ok(buffer)
}

// See https://github.com/LayerTwo-Labs/bip300_bip301_specifications/blob/master/bip300.md#m1-1
fn handle_m1_propose_sidechain(
    rwtxn: &mut RwTxn,
    dbs: &Dbs,
    proposal_height: u32,
    sidechain_number: SidechainNumber,
    data: Vec<u8>,
) -> Result<(), HandleM1ProposeSidechainError> {
    let data_hash: Hash256 = sha256d(&data);
    if dbs
        .data_hash_to_sidechain_proposal
        .try_get(rwtxn, &data_hash)?
        .is_some()
    {
        // If a proposal with the same data_hash already exists,
        // we ignore this M1.
        //
        // Having the same data_hash means that data is the same as well.
        //
        // Without this rule it would be possible for the miners to reset the vote count for
        // any sidechain proposal at any point.
        tracing::debug!("sidechain proposal already exists");
        return Ok(());
    }
    let sidechain_proposal = SidechainProposal {
        sidechain_number,
        data,
        vote_count: 0,
        proposal_height,
    };

    let () = dbs
        .data_hash_to_sidechain_proposal
        .put(rwtxn, &data_hash, &sidechain_proposal)?;

    tracing::info!("persisted new sidechain proposal: {sidechain_proposal:?}");
    Ok(())
}

// See https://github.com/LayerTwo-Labs/bip300_bip301_specifications/blob/master/bip300.md#m2-1
fn handle_m2_ack_sidechain(
    rwtxn: &mut RwTxn,
    dbs: &Dbs,
    height: u32,
    sidechain_number: SidechainNumber,
    data_hash: [u8; 32],
) -> Result<(), HandleM2AckSidechainError> {
    let sidechain_proposal = dbs
        .data_hash_to_sidechain_proposal
        .try_get(rwtxn, &data_hash)?;
    let Some(mut sidechain_proposal) = sidechain_proposal else {
        return Ok(());
    };
    if sidechain_proposal.sidechain_number != sidechain_number {
        return Ok(());
    }
    sidechain_proposal.vote_count += 1;
    dbs.data_hash_to_sidechain_proposal
        .put(rwtxn, &data_hash, &sidechain_proposal)?;

    let sidechain_proposal_age = height - sidechain_proposal.proposal_height;

    let sidechain_slot_is_used = dbs
        .sidechain_numbers
        .sidechain
        .try_get(rwtxn, &sidechain_number)?
        .is_some();

    let new_sidechain_activated = {
        sidechain_slot_is_used
            && sidechain_proposal.vote_count > USED_SIDECHAIN_SLOT_ACTIVATION_THRESHOLD
            && sidechain_proposal_age <= USED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE as u32
    } || {
        !sidechain_slot_is_used
            && sidechain_proposal.vote_count > UNUSED_SIDECHAIN_SLOT_ACTIVATION_THRESHOLD
            && sidechain_proposal_age <= UNUSED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE as u32
    };

    if new_sidechain_activated {
        tracing::info!(
            "sidechain {} in slot {} was activated",
            String::from_utf8_lossy(&sidechain_proposal.data),
            sidechain_number.0
        );
        let sidechain = Sidechain {
            sidechain_number,
            data: sidechain_proposal.data,
            proposal_height: sidechain_proposal.proposal_height,
            activation_height: height,
            vote_count: sidechain_proposal.vote_count,
        };
        dbs.sidechain_numbers
            .sidechain
            .put(rwtxn, &sidechain_number, &sidechain)?;
        dbs.data_hash_to_sidechain_proposal
            .delete(rwtxn, &data_hash)?;
    }
    Ok(())
}

fn handle_failed_sidechain_proposals(
    rwtxn: &mut RwTxn,
    dbs: &Dbs,
    height: u32,
) -> Result<(), HandleFailedSidechainProposalsError> {
    let failed_proposals: Vec<_> = dbs
        .data_hash_to_sidechain_proposal
        .iter(rwtxn)
        .map_err(db_error::Iter::from)?
        .map_err(|err| HandleFailedSidechainProposalsError::DbIter(err.into()))
        .filter_map(|(data_hash, sidechain_proposal)| {
            let sidechain_proposal_age = height - sidechain_proposal.proposal_height;
            let sidechain_slot_is_used = dbs
                .sidechain_numbers
                .sidechain
                .try_get(rwtxn, &sidechain_proposal.sidechain_number)?
                .is_some();
            // FIXME: Do we need to check that the vote_count is below the threshold, or is it
            // enough to check that the max age was exceeded?
            let failed = sidechain_slot_is_used
                && sidechain_proposal_age > USED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE as u32
                || !sidechain_slot_is_used
                    && sidechain_proposal_age > UNUSED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE as u32;
            if failed {
                Ok(Some(data_hash))
            } else {
                Ok(None)
            }
        })
        .collect()?;
    for failed_proposal_data_hash in &failed_proposals {
        dbs.data_hash_to_sidechain_proposal
            .delete(rwtxn, failed_proposal_data_hash)?;
    }
    Ok(())
}

fn handle_m3_propose_bundle(
    rwtxn: &mut RwTxn,
    dbs: &Dbs,
    sidechain_number: SidechainNumber,
    m6id: [u8; 32],
) -> Result<(), HandleM3ProposeBundleError> {
    if dbs
        .sidechain_numbers
        .sidechain
        .try_get(rwtxn, &sidechain_number)?
        .is_none()
    {
        return Err(HandleM3ProposeBundleError::InactiveSidechain { sidechain_number });
    }
    let pending_m6ids = dbs
        .sidechain_numbers
        .pending_m6ids
        .try_get(rwtxn, &sidechain_number)?;
    let mut pending_m6ids = pending_m6ids.unwrap_or_default();
    let pending_m6id = PendingM6id {
        m6id,
        vote_count: 0,
    };
    pending_m6ids.push(pending_m6id);
    let () = dbs
        .sidechain_numbers
        .pending_m6ids
        .put(rwtxn, &sidechain_number, &pending_m6ids)?;
    Ok(())
}

fn handle_m4_votes(
    rwtxn: &mut RwTxn,
    dbs: &Dbs,
    upvotes: &[u16],
) -> Result<(), HandleM4VotesError> {
    for (sidechain_number, vote) in upvotes.iter().enumerate() {
        let sidechain_number = (sidechain_number as u8).into();
        let vote = *vote;
        if vote == ABSTAIN_TWO_BYTES {
            continue;
        }
        let pending_m6ids = dbs
            .sidechain_numbers
            .pending_m6ids
            .try_get(rwtxn, &sidechain_number)?;
        let Some(mut pending_m6ids) = pending_m6ids else {
            continue;
        };
        if vote == ALARM_TWO_BYTES {
            for pending_m6id in &mut pending_m6ids {
                if pending_m6id.vote_count > 0 {
                    pending_m6id.vote_count -= 1;
                }
            }
        } else if let Some(pending_m6id) = pending_m6ids.get_mut(vote as usize) {
            pending_m6id.vote_count += 1;
        }
        let () =
            dbs.sidechain_numbers
                .pending_m6ids
                .put(rwtxn, &sidechain_number, &pending_m6ids)?;
    }
    Ok(())
}

fn handle_m4_ack_bundles(
    rwtxn: &mut RwTxn,
    dbs: &Dbs,
    m4: &M4AckBundles,
) -> Result<(), HandleM4AckBundlesError> {
    match m4 {
        M4AckBundles::LeadingBy50 => {
            todo!();
        }
        M4AckBundles::RepeatPrevious => {
            todo!();
        }
        M4AckBundles::OneByte { upvotes } => {
            let upvotes: Vec<u16> = upvotes.iter().map(|vote| *vote as u16).collect();
            handle_m4_votes(rwtxn, dbs, &upvotes).map_err(HandleM4AckBundlesError::from)
        }
        M4AckBundles::TwoBytes { upvotes } => {
            handle_m4_votes(rwtxn, dbs, upvotes).map_err(HandleM4AckBundlesError::from)
        }
    }
}

/// Returns failed M6IDs with sidechain numbers
fn handle_failed_m6ids(
    rwtxn: &mut RwTxn,
    dbs: &Dbs,
) -> Result<LinkedHashSet<(SidechainNumber, [u8; 32])>, HandleFailedM6IdsError> {
    let mut failed_m6ids = LinkedHashSet::new();
    let mut updated_slots = LinkedHashMap::new();
    let () = dbs
        .sidechain_numbers
        .pending_m6ids
        .iter(rwtxn)
        .map_err(db_error::Iter::from)?
        .map_err(db_error::Iter::from)
        .for_each(|(sidechain_number, pending_m6ids)| {
            for pending_m6id in &pending_m6ids {
                if pending_m6id.vote_count > WITHDRAWAL_BUNDLE_MAX_AGE {
                    failed_m6ids.insert((sidechain_number, pending_m6id.m6id));
                }
            }
            let pending_m6ids: Vec<_> = pending_m6ids
                .into_iter()
                .filter(|pending_m6id| {
                    !failed_m6ids.contains(&(sidechain_number, pending_m6id.m6id))
                })
                .collect();
            updated_slots.insert(sidechain_number, pending_m6ids);
            Ok(())
        })?;
    for (sidechain_number, pending_m6ids) in updated_slots {
        let () =
            dbs.sidechain_numbers
                .pending_m6ids
                .put(rwtxn, &sidechain_number, &pending_m6ids)?;
    }
    Ok(failed_m6ids)
}

/// Deposit or (sidechain_id, m6id)
type DepositOrSuccessfulWithdrawal = Either<Deposit, (SidechainNumber, [u8; 32])>;

fn handle_m5_m6(
    rwtxn: &mut RwTxn,
    dbs: &Dbs,
    transaction: &Transaction,
) -> Result<Option<DepositOrSuccessfulWithdrawal>, HandleM5M6Error> {
    let txid = transaction.compute_txid();
    // TODO: Check that there is only one OP_DRIVECHAIN per sidechain slot.
    let (sidechain_number, new_ctip, new_total_value) = {
        let output = &transaction.output[0];
        // If OP_DRIVECHAIN script is invalid,
        // for example if it is missing OP_TRUE at the end,
        // it will just be ignored.
        if let Ok((_input, sidechain_number)) =
            parse_op_drivechain(&output.script_pubkey.to_bytes())
        {
            let sidechain_number = sidechain_number.into();
            let new_ctip = OutPoint { txid, vout: 0 };
            let new_total_value = output.value;

            (sidechain_number, new_ctip, new_total_value)
        } else {
            return Ok(None);
        }
    };
    let address = {
        let output = &transaction.output[1];
        let script = output.script_pubkey.to_bytes();
        if script[0] == OP_RETURN.to_u8() {
            Some(script[1..].to_vec())
        } else {
            None
        }
    };
    let old_total_value = {
        if let Some(old_ctip) = dbs
            .sidechain_numbers
            .ctip
            .try_get(rwtxn, &sidechain_number)?
        {
            let old_ctip_found = transaction
                .input
                .iter()
                .any(|input| input.previous_output == old_ctip.outpoint);
            if !old_ctip_found {
                return Err(HandleM5M6Error::OldCtipUnspent { sidechain_number });
            }
            old_ctip.value
        } else {
            Amount::ZERO
        }
    };
    let treasury_utxo = TreasuryUtxo {
        outpoint: new_ctip,
        address: address.clone(),
        total_value: new_total_value,
        previous_total_value: old_total_value,
    };
    dbg!(&treasury_utxo);

    let mut res = None;
    // M6
    if new_total_value < old_total_value {
        let mut m6_valid = false;
        let m6id = m6_to_id(transaction, old_total_value.to_sat());
        if let Some(pending_m6ids) = dbs
            .sidechain_numbers
            .pending_m6ids
            .try_get(rwtxn, &sidechain_number)?
        {
            for pending_m6id in &pending_m6ids {
                if pending_m6id.m6id == m6id
                    && pending_m6id.vote_count > WITHDRAWAL_BUNDLE_INCLUSION_THRESHOLD
                {
                    m6_valid = true;
                }
            }
            if m6_valid {
                let pending_m6ids: Vec<_> = pending_m6ids
                    .into_iter()
                    .filter(|pending_m6id| pending_m6id.m6id != m6id)
                    .collect();
                dbs.sidechain_numbers.pending_m6ids.put(
                    rwtxn,
                    &sidechain_number,
                    &pending_m6ids,
                )?;
            }
        }
        if m6_valid {
            res = Some(Either::Right((sidechain_number, m6id)));
        } else {
            return Err(HandleM5M6Error::InvalidM6);
        }
    }
    let mut treasury_utxo_count = dbs
        .sidechain_numbers
        .treasury_utxo_count
        .try_get(rwtxn, &sidechain_number)?
        .unwrap_or(0);
    // Sequence numbers begin at 0, so the total number of treasury utxos in the database
    // gives us the *next* sequence number.
    let sequence_number = treasury_utxo_count;
    dbs.sidechain_number_sequence_number_to_treasury_utxo.put(
        rwtxn,
        &(sidechain_number, sequence_number),
        &treasury_utxo,
    )?;
    treasury_utxo_count += 1;
    dbs.sidechain_numbers.treasury_utxo_count.put(
        rwtxn,
        &sidechain_number,
        &treasury_utxo_count,
    )?;
    let new_ctip = Ctip {
        outpoint: new_ctip,
        value: new_total_value,
    };
    dbs.sidechain_numbers
        .ctip
        .put(rwtxn, &sidechain_number, &new_ctip)?;
    match address {
        Some(address) if new_total_value >= old_total_value && res.is_none() => {
            let deposit = Deposit {
                sequence_number,
                sidechain_id: sidechain_number,
                outpoint: OutPoint { txid, vout: 0 },
                output: TxOut {
                    value: new_total_value - old_total_value,
                    script_pubkey: address.into(),
                },
            };
            res = Some(Either::Left(deposit));
        }
        Some(_) | None => (),
    }
    Ok(res)
}

fn handle_m8(
    transaction: &Transaction,
    accepted_bmm_requests: &BmmCommitments,
    prev_mainchain_block_hash: &BlockHash,
) -> Result<(), HandleM8Error> {
    let output = &transaction.output[0];
    let script = output.script_pubkey.to_bytes();

    if let Ok((_input, bmm_request)) = parse_m8_bmm_request(&script) {
        if !accepted_bmm_requests
            .get(&bmm_request.sidechain_number.into())
            .is_some_and(|commitment| *commitment == bmm_request.sidechain_block_hash)
        {
            return Err(HandleM8Error::NotAcceptedByMiners);
        }
        if bmm_request.prev_mainchain_block_hash != prev_mainchain_block_hash.to_byte_array() {
            return Err(HandleM8Error::BmmRequestExpired);
        }
    }
    Ok(())
}

fn connect_block(
    rwtxn: &mut RwTxn,
    dbs: &Dbs,
    event_tx: &Sender<Event>,
    block: &Block,
    height: u32,
) -> Result<(), ConnectBlockError> {
    // TODO: Check that there are no duplicate M2s.
    let coinbase = &block.txdata[0];
    let mut bmmed_sidechain_slots = HashSet::new();
    let mut accepted_bmm_requests = BmmCommitments::new();
    let mut withdrawal_bundle_events = Vec::new();
    for output in &coinbase.output {
        let message = match parse_coinbase_script(&output.script_pubkey) {
            Ok((rest, message)) => {
                if !rest.is_empty() {
                    tracing::warn!("Extra data in coinbase script: {:?}", hex::encode(rest));
                }
                message
            }

            Err(err) => {
                // Happens all the time. Would be nice to differentiate between "this isn't a BIP300 message"
                // and "we failed real bad".
                tracing::trace!("Failed to parse coinbase script: {:?}", err);
                continue;
            }
        };

        match message {
            CoinbaseMessage::M1ProposeSidechain {
                sidechain_number,
                data,
            } => {
                tracing::info!(
                    "Propose sidechain number {sidechain_number} with data \"{}\"",
                    String::from_utf8_lossy(&data)
                );
                handle_m1_propose_sidechain(
                    rwtxn,
                    dbs,
                    height,
                    sidechain_number.into(),
                    data.clone(),
                )?;
            }
            CoinbaseMessage::M2AckSidechain {
                sidechain_number,
                data_hash,
            } => {
                tracing::info!(
                    "Ack sidechain number {sidechain_number} with hash {}",
                    hex::encode(data_hash)
                );
                handle_m2_ack_sidechain(rwtxn, dbs, height, sidechain_number.into(), data_hash)?;
            }
            CoinbaseMessage::M3ProposeBundle {
                sidechain_number,
                bundle_txid,
            } => {
                let sidechain_number = sidechain_number.into();
                let () = handle_m3_propose_bundle(rwtxn, dbs, sidechain_number, bundle_txid)?;
                let event = WithdrawalBundleEvent {
                    sidechain_id: sidechain_number,
                    m6id: bundle_txid,
                    kind: WithdrawalBundleEventKind::Submitted,
                };
                withdrawal_bundle_events.push(event);
            }
            CoinbaseMessage::M4AckBundles(m4) => {
                handle_m4_ack_bundles(rwtxn, dbs, &m4)?;
            }
            CoinbaseMessage::M7BmmAccept {
                sidechain_number,
                sidechain_block_hash,
            } => {
                let sidechain_number = sidechain_number.into();
                if bmmed_sidechain_slots.contains(&sidechain_number) {
                    return Err(ConnectBlockError::MultipleBmmBlocks { sidechain_number });
                }
                bmmed_sidechain_slots.insert(sidechain_number);
                accepted_bmm_requests.insert(sidechain_number, sidechain_block_hash);
            }
        }
    }

    let () = handle_failed_sidechain_proposals(rwtxn, dbs, height)?;
    let failed_m6ids = handle_failed_m6ids(rwtxn, dbs)?;

    let block_hash = block.header.block_hash();
    let prev_mainchain_block_hash = block.header.prev_blockhash;

    let mut deposits = Vec::new();
    withdrawal_bundle_events.extend(failed_m6ids.into_iter().map(|(sidechain_id, m6id)| {
        WithdrawalBundleEvent {
            m6id,
            sidechain_id,
            kind: WithdrawalBundleEventKind::Failed,
        }
    }));
    for transaction in &block.txdata[1..] {
        match handle_m5_m6(rwtxn, dbs, transaction)? {
            Some(Either::Left(deposit)) => deposits.push(deposit),
            Some(Either::Right((sidechain_id, m6id))) => {
                let withdrawal_bundle_event = WithdrawalBundleEvent {
                    m6id,
                    sidechain_id,
                    kind: WithdrawalBundleEventKind::Succeeded,
                };
                withdrawal_bundle_events.push(withdrawal_bundle_event);
            }
            None => (),
        };
        let () = handle_m8(
            transaction,
            &accepted_bmm_requests,
            &prev_mainchain_block_hash,
        )?;
    }
    let block_info = BlockInfo {
        deposits,
        withdrawal_bundle_events,
        bmm_commitments: accepted_bmm_requests.into_iter().collect(),
    };
    let () = dbs
        .block_hashes
        .put_block_info(rwtxn, &block_hash, &block_info)
        .map_err(ConnectBlockError::PutBlockInfo)?;
    // TODO: invalidate block
    let current_tip_cumulative_work: Option<Work> = 'work: {
        let Some(current_tip) = dbs.current_chain_tip.try_get(rwtxn, &UnitKey)? else {
            break 'work None;
        };
        Some(
            dbs.block_hashes
                .cumulative_work()
                .get(rwtxn, &current_tip)?,
        )
    };
    let cumulative_work = dbs.block_hashes.cumulative_work().get(rwtxn, &block_hash)?;
    if Some(cumulative_work) > current_tip_cumulative_work {
        dbs.current_chain_tip.put(rwtxn, &UnitKey, &block_hash)?;
        tracing::debug!("updated current chain tip to {block_hash}");
    }
    let event = {
        let header_info = HeaderInfo {
            block_hash,
            prev_block_hash: prev_mainchain_block_hash,
            height,
            work: block.header.work(),
        };
        Event::ConnectBlock {
            header_info,
            block_info,
        }
    };
    let _send_err: Result<Option<_>, TrySendError<_>> = event_tx.try_broadcast(event);
    Ok(())
}

// TODO: Add unit tests ensuring that `connect_block` and `disconnect_block` are inverse
// operations.
#[allow(unreachable_code, unused_variables)]
fn disconnect_block(
    _rwtxn: &mut RwTxn,
    _dbs: &Dbs,
    event_tx: &Sender<Event>,
    block_hash: BlockHash,
) -> Result<(), DisconnectBlockError> {
    // FIXME: implement
    todo!();
    let event = Event::DisconnectBlock { block_hash };
    let _send_err: Result<Option<_>, TrySendError<_>> = event_tx.try_broadcast(event);
    Ok(())
}

fn _is_transaction_valid(
    _rotxn: &mut RoTxn,
    _dbs: &Dbs,
    _transaction: &Transaction,
) -> Result<(), TxValidationError> {
    todo!();
}

async fn sync_headers(
    dbs: &Dbs,
    main_client: &jsonrpsee::http_client::HttpClient,
    main_tip: BlockHash,
) -> Result<(), SyncError> {
    let mut block_hash = main_tip;
    while let Some(latest_missing_header) = tokio::task::block_in_place(|| {
        let rotxn = dbs.read_txn()?;
        dbs.block_hashes
            .latest_missing_ancestor_header(&rotxn, block_hash)
            .map_err(SyncError::DbTryGet)
    })? {
        tracing::debug!("Syncing header `{latest_missing_header}` -> `{main_tip}`");
        let header = main_client
            .getblockheader(latest_missing_header)
            .map_err(|err| SyncError::JsonRpc {
                method: "getblockheader".to_owned(),
                source: err,
            })
            .await?;
        let height = header.height;
        let mut rwtxn = dbs.write_txn()?;
        dbs.block_hashes
            .put_header(&mut rwtxn, &header.into(), height)?;
        let () = rwtxn.commit()?;
        block_hash = latest_missing_header;
    }
    Ok(())
}

// MUST be called after `initial_sync_headers`.
async fn sync_blocks(
    dbs: &Dbs,
    event_tx: &Sender<Event>,
    main_client: &jsonrpsee::http_client::HttpClient,
    main_tip: BlockHash,
) -> Result<(), SyncError> {
    let missing_blocks: Vec<BlockHash> = tokio::task::block_in_place(|| {
        let rotxn = dbs.read_txn()?;
        dbs.block_hashes
            .ancestor_headers(&rotxn, main_tip)
            .map(|(block_hash, _header)| Ok(block_hash))
            .take_while(|block_hash| Ok(!dbs.block_hashes.contains_block(&rotxn, block_hash)?))
            .collect()
            .map_err(SyncError::from)
    })?;
    if missing_blocks.is_empty() {
        return Ok(());
    }
    for missing_block in missing_blocks.into_iter().rev() {
        tracing::debug!("Syncing block `{missing_block}` -> `{main_tip}`");
        let block = main_client
            .get_block(missing_block, U8Witness::<0>)
            .map_err(|err| SyncError::JsonRpc {
                method: "getblock".to_owned(),
                source: err,
            })
            .await?
            .0;
        let mut rwtxn = dbs.write_txn()?;
        let height = dbs.block_hashes.height().get(&rwtxn, &missing_block)?;
        let () = connect_block(&mut rwtxn, dbs, event_tx, &block, height)?;
        tracing::debug!("connected block at height {height}: {missing_block}");
        let () = rwtxn.commit()?;
    }
    Ok(())
}

async fn sync_to_tip(
    dbs: &Dbs,
    event_tx: &Sender<Event>,
    main_client: &jsonrpsee::http_client::HttpClient,
    main_tip: BlockHash,
) -> Result<(), SyncError> {
    let () = sync_headers(dbs, main_client, main_tip).await?;
    let () = sync_blocks(dbs, event_tx, main_client, main_tip).await?;
    Ok(())
}

async fn initial_sync(
    dbs: &Dbs,
    event_tx: &Sender<Event>,
    main_client: &jsonrpsee::http_client::HttpClient,
) -> Result<(), SyncError> {
    let main_tip: BlockHash = main_client
        .getbestblockhash()
        .map_err(|err| SyncError::JsonRpc {
            method: "getbestblockhash".to_owned(),
            source: err,
        })
        .await?;
    tracing::debug!("mainchain tip: `{main_tip}`");
    let () = sync_to_tip(dbs, event_tx, main_client, main_tip).await?;
    Ok(())
}

pub(super) async fn task(
    main_client: &jsonrpsee::http_client::HttpClient,
    zmq_addr_sequence: &str,
    dbs: &Dbs,
    event_tx: &Sender<Event>,
) -> anyhow::Result<()> {
    // FIXME: use this instead of polling
    let zmq_sequence = crate::zmq::subscribe_sequence(zmq_addr_sequence).await?;
    let () = initial_sync(dbs, event_tx, main_client).await?;
    zmq_sequence
        .err_into()
        .try_for_each(|msg| async move {
            match msg {
                SequenceMessage::BlockHashConnected(block_hash, _) => {
                    let () = sync_to_tip(dbs, event_tx, main_client, block_hash).await?;
                    Ok(())
                }
                SequenceMessage::BlockHashDisconnected(block_hash, _) => {
                    let mut rwtxn = dbs.write_txn()?;
                    let () = disconnect_block(&mut rwtxn, dbs, event_tx, block_hash)?;
                    Ok(())
                }
                SequenceMessage::TxHashAdded { .. } | SequenceMessage::TxHashRemoved { .. } => {
                    Ok(())
                }
            }
        })
        .await
}
#[cfg(test)]
mod tests {

    use super::*;
    use bip300301_messages::CoinbaseMessage;
    use bitcoin::{consensus::Decodable, io::Cursor};

    #[test]
    fn test_roundtrip() {
        let Ok(proposal) = create_sidechain_proposal(
            SidechainNumber::from(13),
            0,
            "title".into(),
            "description".into(),
            vec![0u8; 32],
            vec![0u8; 20],
        ) else {
            panic!("Failed to create sidechain proposal");
        };

        let tx_out = TxOut::consensus_decode(&mut Cursor::new(&proposal)).unwrap();

        let Ok((rest, message)) = parse_coinbase_script(&tx_out.script_pubkey) else {
            panic!("Failed to parse sidechain proposal");
        };

        assert!(rest.is_empty());

        let CoinbaseMessage::M1ProposeSidechain {
            sidechain_number,
            data: _,
        } = message
        else {
            panic!("Failed to parse sidechain proposal");
        };

        assert_eq!(sidechain_number, 13);
    }

    #[test]
    fn test_parse_m1_message() {
        let hex_encoded = "020000000001010000000000000000000000000000000000000000000000000000000000000000ffffffff03023939feffffff0300f2052a01000000160014a46fddeaf98f1dd3efca296ad19fec5b067dab3a00000000000000005e6ad5e0c4af0a0a0154657374636861696e41207265616c6c792073696d706c652073696465636861696e000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000986a24aa21a9ede2f61c3f71d1defd3fa999dfa36953755c690689799962b48bebd836974e8cf94c70ecc7daa2000247304402207084564296c763b6cfcaf3aeea421015559161d4ecc0feb5f9b6b3c85b8b67ae02207044d4b0650a8f3b63218ca17764d2eced822859809141aee2112ca8e3b5a2770121032b15624631d879829cf8d66b89965d264674b36d73682a69d739b9cf255b99500120000000000000000000000000000000000000000000000000000000000000000000000000";
        let mut bytes = hex::decode(hex_encoded).expect("Decoding failed");
        let transaction = Transaction::consensus_decode(&mut Cursor::new(&mut bytes))
            .expect("Deserialization failed");

        for output in &transaction.output {
            let Ok((_, message)) = parse_coinbase_script(&output.script_pubkey) else {
                continue;
            };

            match message {
                CoinbaseMessage::M1ProposeSidechain {
                    sidechain_number,
                    data: _,
                } => {
                    assert_eq!(sidechain_number, 10);

                    return;
                }
                _ => panic!("Parsed message is not an M1ProposeSidechain"),
            }
        }

        panic!("did not find M1ProposeSidechain");
    }
}
