use std::{collections::HashSet, time::Duration};

use async_broadcast::{Sender, TrySendError};
use bip300301_messages::{
    bitcoin::{
        self, hashes::Hash, opcodes::all::OP_RETURN, Amount, Block, BlockHash, OutPoint,
        Transaction, TxOut,
    },
    m6_to_id, parse_coinbase_script, parse_m8_bmm_request, parse_op_drivechain, sha256d,
    CoinbaseMessage, M4AckBundles, ABSTAIN_TWO_BYTES, ALARM_TWO_BYTES,
};
use either::Either;
use fallible_iterator::FallibleIterator;
use futures::{StreamExt as _, TryStreamExt as _};
use hashlink::{LinkedHashMap, LinkedHashSet};
use heed::RoTxn;
use miette::{miette, IntoDiagnostic};
use thiserror::Error;
use tokio::time::{interval, Instant};
use tokio_stream::wrappers::IntervalStream;
use ureq_jsonrpc::json;

use crate::{
    rpc_client::RpcClient,
    types::{
        BlockInfo, BmmCommitments, Ctip, Deposit, Event, Hash256, HeaderInfo, PendingM6id,
        Sidechain, SidechainNumber, SidechainProposal, TreasuryUtxo, WithdrawalBundleEvent,
        WithdrawalBundleEventKind,
    },
};

use super::dbs::{db_error, CommitWriteTxnError, Dbs, RwTxn, UnitKey, WriteTxnError};

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
    DbDelete(#[from] db_error::Delete),
    #[error(transparent)]
    DbFirst(#[from] db_error::First),
    #[error(transparent)]
    DbLen(#[from] db_error::Len),
    #[error(transparent)]
    DbPut(#[from] db_error::Put),
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
enum InitialSyncError {
    #[error(transparent)]
    CommitWriteTxn(#[from] CommitWriteTxnError),
    #[error("Failed to connect block")]
    ConnectBlock(#[from] ConnectBlockError),
    #[error(transparent)]
    DbPut(#[from] db_error::Put),
    #[error(transparent)]
    DbTryGet(#[from] db_error::TryGet),
    #[error("Failed to decode block hash hex: `{block_hash_hex}`")]
    DecodeBlockHashHex {
        block_hash_hex: String,
        source: hex::FromHexError,
    },
    #[error("Failed to get block `{block_hash}`")]
    GetBlock { block_hash: String },
    #[error("Failed to get block count")]
    GetBlockCount,
    #[error("Failed to get block hash for height `{height}`")]
    GetBlockHash { height: u32 },
    #[error("RPC error: `{method}`")]
    Rpc {
        method: String,
        source: Box<ureq_jsonrpc::Error>,
    },
    #[error(transparent)]
    WriteTxn(#[from] WriteTxnError),
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
        .sidechain_number_to_sidechain
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
        dbs.sidechain_number_to_sidechain
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
                .sidechain_number_to_sidechain
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
        .sidechain_number_to_sidechain
        .try_get(rwtxn, &sidechain_number)?
        .is_none()
    {
        return Err(HandleM3ProposeBundleError::InactiveSidechain { sidechain_number });
    }
    let pending_m6ids = dbs
        .sidechain_number_to_pending_m6ids
        .try_get(rwtxn, &sidechain_number)?;
    let mut pending_m6ids = pending_m6ids.unwrap_or_default();
    let pending_m6id = PendingM6id {
        m6id,
        vote_count: 0,
    };
    pending_m6ids.push(pending_m6id);
    let () = dbs
        .sidechain_number_to_pending_m6ids
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
            .sidechain_number_to_pending_m6ids
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
            dbs.sidechain_number_to_pending_m6ids
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
        .sidechain_number_to_pending_m6ids
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
            dbs.sidechain_number_to_pending_m6ids
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
            .sidechain_number_to_ctip
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
            .sidechain_number_to_pending_m6ids
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
                dbs.sidechain_number_to_pending_m6ids.put(
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
        .sidechain_number_to_treasury_utxo_count
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
    dbs.sidechain_number_to_treasury_utxo_count.put(
        rwtxn,
        &sidechain_number,
        &treasury_utxo_count,
    )?;
    let new_ctip = Ctip {
        outpoint: new_ctip,
        value: new_total_value,
    };
    dbs.sidechain_number_to_ctip
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
        let Ok((_, message)) = parse_coinbase_script(&output.script_pubkey) else {
            continue;
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

    {
        let accepted_bmm_block_hashes: Vec<_> = accepted_bmm_requests
            .iter()
            .map(|(_sidechain_number, hash)| *hash)
            .collect();
        dbs.block_height_to_accepted_bmm_block_hashes.put(
            rwtxn,
            &height,
            &accepted_bmm_block_hashes,
        )?;
        const MAX_BMM_BLOCK_DEPTH: u64 = 6 * 24 * 7; // 1008 blocks = ~1 week of time
        if dbs.block_height_to_accepted_bmm_block_hashes.len(rwtxn)? > MAX_BMM_BLOCK_DEPTH {
            let (block_height, _) = dbs
                .block_height_to_accepted_bmm_block_hashes
                .first(rwtxn)?
                .unwrap();
            dbs.block_height_to_accepted_bmm_block_hashes
                .delete(rwtxn, &block_height)?;
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
    let () = dbs
        .block_hash_to_bmm_commitments
        .put(rwtxn, &block_hash, &accepted_bmm_requests)?;
    let () = dbs
        .block_hash_to_deposits
        .put(rwtxn, &block_hash, &deposits)?;
    let () = dbs
        .block_hash_to_header
        .put(rwtxn, &block_hash, &block.header)?;
    let () = dbs.block_hash_to_height.put(rwtxn, &block_hash, &height)?;
    let event = {
        let header_info = HeaderInfo {
            block_hash,
            prev_block_hash: prev_mainchain_block_hash,
            height,
            work: block.header.work().to_le_bytes(),
        };
        let block_info = BlockInfo {
            deposits,
            withdrawal_bundle_events,
            bmm_commitments: accepted_bmm_requests.into_iter().collect(),
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
fn _disconnect_block(
    _rwtxn: &mut RwTxn,
    _dbs: &Dbs,
    event_tx: &Sender<Event>,
    block_hash: Hash256,
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

fn initial_sync(
    dbs: &Dbs,
    event_tx: &Sender<Event>,
    main_client: &RpcClient,
) -> Result<(), InitialSyncError> {
    let mut rwtxn = dbs.write_txn()?;
    let mut height = dbs
        .current_block_height
        .try_get(&rwtxn, &UnitKey)?
        .unwrap_or(0);
    let main_block_height: u32 = main_client
        .send_request("getblockcount", &[])
        .map_err(|err| InitialSyncError::Rpc {
            method: "getblockcount".to_owned(),
            source: Box::new(err),
        })?
        .ok_or(InitialSyncError::GetBlockCount)?;
    tracing::debug!("mainchain block height: {main_block_height}");

    if height < main_block_height {
        tracing::debug!("syncing to block height: {height} -> {main_block_height}");
    } else {
        tracing::debug!("already synced to block height {height}");
    }

    while height < main_block_height {
        let block_hash: String = main_client
            .send_request("getblockhash", &[json!(height)])
            .map_err(|err| InitialSyncError::Rpc {
                method: "getblockhash".to_owned(),
                source: Box::new(err),
            })?
            .ok_or(InitialSyncError::GetBlockHash { height })?;

        tracing::trace!("fetched block hash at height {height}: {block_hash}");

        let block: String = main_client
            .send_request("getblock", &[json!(block_hash), json!(0)])
            .map_err(|err| InitialSyncError::Rpc {
                method: "getblock".to_owned(),
                source: Box::new(err),
            })?
            .ok_or_else(|| InitialSyncError::GetBlock {
                block_hash: block_hash.clone(),
            })?;

        let block = bitcoin::consensus::encode::deserialize_hex(&block).unwrap();
        match connect_block(&mut rwtxn, dbs, event_tx, &block, height) {
            Ok(_) => tracing::trace!("connected block at height {height}: {block_hash}"),
            Err(err) => {
                /*
                main_client
                    .send_request("invalidateblock", &[json!(block_hash)])
                    .into_diagnostic()?
                    .ok_or(miette!("failed to invalidate block"))?;
                */
                return Err(InitialSyncError::ConnectBlock(err));
            }
        }
        height += 1;
        let block_hash = {
            match hex::decode(&block_hash) {
                Ok(block_hash) => block_hash,
                Err(err) => {
                    return Err(InitialSyncError::DecodeBlockHashHex {
                        block_hash_hex: block_hash,
                        source: err,
                    })
                }
            }
            .try_into()
            .map(BlockHash::from_byte_array)
            .unwrap()
        };

        dbs.current_chain_tip
            .put(&mut rwtxn, &UnitKey, &block_hash)?;

        tracing::trace!("updated current chain tip to {block_hash}");
    }

    dbs.current_block_height
        .put(&mut rwtxn, &UnitKey, &height)?;
    let () = rwtxn.commit()?;
    Ok(())
}

// FIXME: Rewrite all of this to be more readable.
/// Single iteration of the task loop
fn task_loop_once(
    dbs: &Dbs,
    event_tx: &Sender<Event>,
    main_client: &RpcClient,
) -> Result<(), miette::Report> {
    let mut txn = dbs.write_txn().into_diagnostic()?;
    let mut height = dbs
        .current_block_height
        .try_get(&txn, &UnitKey)
        .into_diagnostic()?
        .unwrap_or(0);
    let main_block_height: u32 = main_client
        .send_request("getblockcount", &[])
        .into_diagnostic()?
        .ok_or(miette!("failed to get block count"))?;
    if main_block_height == height {
        return Ok(());
    }
    tracing::debug!("Block height: {main_block_height}");

    while height < main_block_height {
        let block_hash: String = main_client
            .send_request("getblockhash", &[json!(height)])
            .into_diagnostic()?
            .ok_or(miette!("failed to get block hash"))?;
        let prev_blockhash =
            bitcoin::consensus::encode::deserialize_hex::<BlockHash>(&block_hash).unwrap();
        // FIXME: This looks sus
        tracing::debug!("Mainchain tip: {prev_blockhash}");

        let block: String = main_client
            .send_request("getblock", &[json!(block_hash), json!(0)])
            .into_diagnostic()?
            .ok_or(miette!("failed to get block"))?;
        let block = bitcoin::consensus::encode::deserialize_hex(&block).unwrap();

        if connect_block(&mut txn, dbs, event_tx, &block, height).is_err() {
            /*
            main_client
                .send_request("invalidateblock", &[json!(block_hash)])
                .into_diagnostic()?
                .ok_or(miette!("failed to invalidate block"))?;
            */
        }

        // check for new block
        // validate block
        // if invalid invalidate
        // if valid connect
        // wait 1 second
        height += 1;
        let block_hash = hex::decode(block_hash)
            .into_diagnostic()?
            .try_into()
            .map(BlockHash::from_byte_array)
            .unwrap();
        dbs.current_chain_tip
            .put(&mut txn, &UnitKey, &block_hash)
            .into_diagnostic()?;
    }
    dbs.current_block_height
        .put(&mut txn, &UnitKey, &height)
        .into_diagnostic()?;
    txn.commit().into_diagnostic()?;
    Ok(())
}

pub(super) async fn task(
    main_client: &RpcClient,
    zmq_addr_sequence: &str,
    dbs: &Dbs,
    event_tx: &Sender<Event>,
) -> Result<(), miette::Report> {
    // FIXME: use this instead of polling
    let _zmq_sequence = crate::zmq::subscribe_sequence(zmq_addr_sequence)
        .await
        .into_diagnostic()?;
    let () = initial_sync(dbs, event_tx, main_client).into_diagnostic()?;
    let interval = interval(Duration::from_secs(1));
    IntervalStream::new(interval)
        .map(Ok)
        .try_for_each(move |_: Instant| async move { task_loop_once(dbs, event_tx, main_client) })
        .await
}
