use std::{borrow::Cow, cmp::Ordering, collections::HashMap};

use async_broadcast::{Sender, TrySendError};
use bip300301::{
    client::{GetBlockClient, U8Witness},
    jsonrpsee, MainClient,
};
use bitcoin::{
    self,
    hashes::{sha256d, Hash as _},
    Amount, Block, BlockHash, OutPoint, Transaction, Work,
};
use either::Either;
use fallible_iterator::FallibleIterator;
use fatality::Split as _;
use futures::{TryFutureExt as _, TryStreamExt as _};
use hashlink::LinkedHashSet;
use heed::RoTxn;

use crate::{
    messages::{
        compute_m6id, parse_op_drivechain, CoinbaseMessage, CoinbaseMessages, M1ProposeSidechain,
        M2AckSidechain, M3ProposeBundle, M4AckBundles, M7BmmAccept, M8BmmRequest,
    },
    types::{
        BlockEvent, BlockInfo, BmmCommitments, Ctip, Deposit, Event, HeaderInfo, M6id, Sidechain,
        SidechainNumber, SidechainProposal, SidechainProposalId, SidechainProposalStatus,
        TreasuryUtxo, WithdrawalBundleEvent, WithdrawalBundleEventKind,
        WITHDRAWAL_BUNDLE_INCLUSION_THRESHOLD, WITHDRAWAL_BUNDLE_MAX_AGE,
    },
    validator::dbs::{db_error, Dbs, RwTxn, UnitKey},
    zmq::SequenceMessage,
};

mod error;

const USED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE: u16 = 10;
const USED_SIDECHAIN_SLOT_ACTIVATION_THRESHOLD: u16 = USED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE / 2;

const UNUSED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE: u16 = 10;
const UNUSED_SIDECHAIN_SLOT_ACTIVATION_MAX_FAILS: u16 = 5;
const UNUSED_SIDECHAIN_SLOT_ACTIVATION_THRESHOLD: u16 =
    UNUSED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE - UNUSED_SIDECHAIN_SLOT_ACTIVATION_MAX_FAILS;

/// Returns `Some` if the sidechain proposal does not already exist
// See https://github.com/LayerTwo-Labs/bip300_bip301_specifications/blob/master/bip300.md#m1-1
fn handle_m1_propose_sidechain(
    rwtxn: &mut RwTxn,
    dbs: &Dbs,
    proposal: SidechainProposal,
    proposal_height: u32,
) -> Result<Option<Sidechain>, error::HandleM1ProposeSidechain> {
    let proposal_id: SidechainProposalId = (&proposal).into();
    // FIXME: check that the proposal was made in an ancestor block
    if dbs
        .proposal_id_to_sidechain
        .contains_key(rwtxn, &proposal_id)?
    {
        // If a proposal with the same description_hash already exists,
        // we ignore this M1.
        //
        // Having the same description_hash means that data is the same as well.
        //
        // Without this rule it would be possible for the miners to reset the vote count for
        // any sidechain proposal at any point.
        tracing::debug!("sidechain proposal already exists");
        return Ok(None);
    }
    let sidechain = Sidechain {
        proposal,
        status: SidechainProposalStatus {
            vote_count: 0,
            proposal_height,
            activation_height: None,
        },
    };

    let () = dbs
        .proposal_id_to_sidechain
        .put(rwtxn, &proposal_id, &sidechain)?;

    tracing::info!("persisted new sidechain proposal: {}", sidechain.proposal);
    Ok(Some(sidechain))
}

// See https://github.com/LayerTwo-Labs/bip300_bip301_specifications/blob/master/bip300.md#m2-1
fn handle_m2_ack_sidechain(
    rwtxn: &mut RwTxn,
    dbs: &Dbs,
    height: u32,
    sidechain_number: SidechainNumber,
    description_hash: sha256d::Hash,
) -> Result<(), error::HandleM2AckSidechain> {
    let proposal_id = SidechainProposalId {
        sidechain_number,
        description_hash,
    };
    let sidechain = dbs.proposal_id_to_sidechain.try_get(rwtxn, &proposal_id)?;
    let Some(mut sidechain) = sidechain else {
        return Err(error::HandleM2AckSidechain::MissingProposal {
            sidechain_slot: sidechain_number,
            description_hash,
        });
    };
    sidechain.status.vote_count += 1;
    dbs.proposal_id_to_sidechain
        .put(rwtxn, &proposal_id, &sidechain)?;

    let sidechain_proposal_age = height - sidechain.status.proposal_height;

    let sidechain_slot_is_used = dbs
        .active_sidechains
        .sidechain()
        .try_get(rwtxn, &sidechain_number)?
        .is_some();

    let new_sidechain_activated = {
        sidechain_slot_is_used
            && sidechain.status.vote_count > USED_SIDECHAIN_SLOT_ACTIVATION_THRESHOLD
            && sidechain_proposal_age <= USED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE as u32
    } || {
        !sidechain_slot_is_used
            && sidechain.status.vote_count > UNUSED_SIDECHAIN_SLOT_ACTIVATION_THRESHOLD
            && sidechain_proposal_age <= UNUSED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE as u32
    };

    if new_sidechain_activated {
        tracing::info!(
            "sidechain {} in slot {} was activated",
            String::from_utf8_lossy(&sidechain.proposal.description.0),
            sidechain_number.0
        );
        sidechain.status.activation_height = Some(height);
        let () = dbs
            .active_sidechains
            .put_sidechain(rwtxn, &sidechain_number, &sidechain)?;
        dbs.proposal_id_to_sidechain.delete(rwtxn, &proposal_id)?;
    }
    Ok(())
}

fn handle_failed_sidechain_proposals(
    rwtxn: &mut RwTxn,
    dbs: &Dbs,
    height: u32,
) -> Result<(), error::HandleFailedSidechainProposals> {
    let failed_proposals: Vec<_> = dbs
        .proposal_id_to_sidechain
        .iter(rwtxn)
        .map_err(db_error::Iter::from)?
        .map_err(|err| error::HandleFailedSidechainProposals::DbIter(err.into()))
        .filter_map(|(proposal_id, sidechain)| {
            let sidechain_proposal_age = height - sidechain.status.proposal_height;
            let sidechain_slot_is_used = dbs
                .active_sidechains
                .sidechain()
                .contains_key(rwtxn, &sidechain.proposal.sidechain_number)?;
            // FIXME: Do we need to check that the vote_count is below the threshold, or is it
            // enough to check that the max age was exceeded?
            let failed = sidechain_slot_is_used
                && sidechain_proposal_age > USED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE as u32
                || !sidechain_slot_is_used
                    && sidechain_proposal_age > UNUSED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE as u32;
            if failed {
                Ok(Some(proposal_id))
            } else {
                Ok(None)
            }
        })
        .collect()?;
    for failed_proposal_id in &failed_proposals {
        dbs.proposal_id_to_sidechain
            .delete(rwtxn, failed_proposal_id)?;
    }
    Ok(())
}

fn handle_m3_propose_bundle(
    rwtxn: &mut RwTxn,
    dbs: &Dbs,
    sidechain_number: SidechainNumber,
    m6id: M6id,
    block_height: u32,
) -> Result<(), error::HandleM3ProposeBundle> {
    if dbs
        .active_sidechains
        .try_put_pending_m6id(rwtxn, &sidechain_number, m6id, block_height)?
    {
        Ok(())
    } else {
        Err(error::HandleM3ProposeBundle::InactiveSidechain { sidechain_number })
    }
}

fn handle_m4_votes(
    rwtxn: &mut RwTxn,
    dbs: &Dbs,
    upvotes: &[u16],
) -> Result<(), error::HandleM4Votes> {
    let active_sidechains: Vec<_> = dbs
        .active_sidechains
        .sidechain()
        .iter(rwtxn)
        .map_err(db_error::Iter::from)?
        .map(|(sidechain_number, _)| Ok(sidechain_number))
        .collect()
        .map_err(db_error::Iter::from)?;
    if upvotes.len() != active_sidechains.len() {
        return Err(error::HandleM4Votes::InvalidVotes {
            expected: active_sidechains.len(),
            len: upvotes.len(),
        });
    }
    for (idx, vote) in upvotes.iter().enumerate() {
        let sidechain_number = active_sidechains[idx];
        let vote = *vote;
        if vote == M4AckBundles::ABSTAIN_TWO_BYTES {
            continue;
        }
        if vote == M4AckBundles::ALARM_TWO_BYTES {
            let _: bool = dbs
                .active_sidechains
                .try_alarm_pending_m6ids(rwtxn, &sidechain_number)
                .map_err(error::HandleM4Votes::TryAlarmPendingM6ids)?;
        } else if !dbs.active_sidechains.try_upvote_pending_withdrawal(
            rwtxn,
            &sidechain_number,
            vote as usize,
        )? {
            return Err(error::HandleM4Votes::UpvoteFailed {
                sidechain_number,
                index: vote,
            });
        };
    }
    Ok(())
}

fn handle_m4_ack_bundles(
    rwtxn: &mut RwTxn,
    dbs: &Dbs,
    m4: &M4AckBundles,
) -> Result<(), error::HandleM4AckBundles> {
    match m4 {
        M4AckBundles::LeadingBy50 => {
            todo!();
        }
        M4AckBundles::RepeatPrevious => {
            todo!();
        }
        M4AckBundles::OneByte { upvotes } => {
            let upvotes: Vec<u16> = upvotes
                .iter()
                .map(|vote| match *vote {
                    M4AckBundles::ABSTAIN_ONE_BYTE => M4AckBundles::ABSTAIN_TWO_BYTES,
                    M4AckBundles::ALARM_ONE_BYTE => M4AckBundles::ALARM_TWO_BYTES,
                    vote => vote as u16,
                })
                .collect();
            handle_m4_votes(rwtxn, dbs, &upvotes).map_err(error::HandleM4AckBundles::from)
        }
        M4AckBundles::TwoBytes { upvotes } => {
            handle_m4_votes(rwtxn, dbs, upvotes).map_err(error::HandleM4AckBundles::from)
        }
    }
}

/// Returns failed M6IDs with sidechain numbers
fn handle_failed_m6ids(
    rwtxn: &mut RwTxn,
    dbs: &Dbs,
    block_height: u32,
) -> Result<LinkedHashSet<(SidechainNumber, M6id)>, error::HandleFailedM6Ids> {
    let mut failed_m6ids = LinkedHashSet::new();

    dbs.active_sidechains
        .retain_pending_withdrawals(rwtxn, |sidechain_number, m6id, info| {
            let age = block_height.saturating_sub(info.proposal_height);
            if age > WITHDRAWAL_BUNDLE_MAX_AGE as u32 {
                failed_m6ids.replace((sidechain_number, *m6id));
                false
            } else {
                true
            }
        })?;
    Ok(failed_m6ids)
}

/// Deposit or (sidechain_id, m6id, sequence_number)
type DepositOrSuccessfulWithdrawal = Either<Deposit, (SidechainNumber, M6id, u64)>;

/// Returns (sidechain_id, m6id)
fn handle_m6(
    rwtxn: &mut RwTxn,
    dbs: &Dbs,
    transaction: Transaction,
    old_treasury_value: Amount,
) -> Result<(M6id, SidechainNumber), error::HandleM5M6> {
    let (m6id, sidechain_number) = compute_m6id(transaction, old_treasury_value)?;

    if dbs
        .active_sidechains
        .try_with_pending_withdrawal_entry(rwtxn, &sidechain_number, m6id, |entry| match entry {
            ordermap::map::Entry::Occupied(entry)
                if entry.get().vote_count > WITHDRAWAL_BUNDLE_INCLUSION_THRESHOLD =>
            {
                entry.remove();
                true
            }
            _ => false,
        })?
        .is_some_and(|ok| ok)
    {
        Ok((m6id, sidechain_number))
    } else {
        Err(error::HandleM5M6::InvalidM6)
    }
}

fn handle_m5_m6(
    rwtxn: &mut RwTxn,
    dbs: &Dbs,
    transaction: Cow<'_, Transaction>,
) -> Result<Option<DepositOrSuccessfulWithdrawal>, error::HandleM5M6> {
    let txid = transaction.compute_txid();
    // TODO: Check that there is only one OP_DRIVECHAIN per sidechain slot.
    let (sidechain_number, new_ctip, new_treasury_value) = {
        let Some(treasury_output) = transaction.output.first() else {
            return Ok(None);
        };
        // If OP_DRIVECHAIN script is invalid,
        // for example if it is missing OP_TRUE at the end,
        // it will just be ignored.
        if let Ok((_input, sidechain_number)) =
            parse_op_drivechain(&treasury_output.script_pubkey.to_bytes())
        {
            let new_ctip = OutPoint { txid, vout: 0 };
            (sidechain_number, new_ctip, treasury_output.value)
        } else {
            return Ok(None);
        }
    };
    let old_treasury_value = {
        if let Some(old_ctip) = dbs
            .active_sidechains
            .ctip()
            .try_get(rwtxn, &sidechain_number)?
        {
            let old_ctip_found = transaction
                .input
                .iter()
                .any(|input| input.previous_output == old_ctip.outpoint);
            if !old_ctip_found {
                return Err(error::HandleM5M6::OldCtipUnspent { sidechain_number });
            }
            old_ctip.value
        } else {
            Amount::ZERO
        }
    };
    let treasury_utxo = TreasuryUtxo {
        sidechain_number,
        outpoint: new_ctip,
        total_value: new_treasury_value,
        previous_total_value: old_treasury_value,
    };

    let treasury_utxo_count = dbs
        .active_sidechains
        .treasury_utxo_count
        .try_get(rwtxn, &sidechain_number)?
        .unwrap_or(0);
    // Sequence numbers begin at 0, so the total number of treasury utxos in the database
    // gives us the *next* sequence number.
    let sequence_number = treasury_utxo_count;
    let res = match new_treasury_value.cmp(&old_treasury_value) {
        // M6
        Ordering::Less => {
            if transaction.input.len() != 1 {
                return Ok(None);
            }
            let (m6id, sidechain_number_) =
                handle_m6(rwtxn, dbs, transaction.into_owned(), old_treasury_value)?;
            assert_eq!(sidechain_number, sidechain_number_);
            Either::Right((sidechain_number, m6id, sequence_number))
        }
        // M5
        Ordering::Greater => {
            let Some(address_output) = transaction.output.get(1) else {
                return Ok(None);
            };
            let Some(address) =
                crate::messages::try_parse_op_return_address(&address_output.script_pubkey)
            else {
                return Ok(None);
            };
            let deposit = Deposit {
                sequence_number,
                sidechain_id: sidechain_number,
                outpoint: new_ctip,
                address,
                value: new_treasury_value - old_treasury_value,
            };
            Either::Left(deposit)
        }
        Ordering::Equal => {
            // FIXME: is it ok to treat this as a regular TX?
            return Ok(None);
        }
    };
    dbs.active_sidechains.slot_sequence_to_treasury_utxo.put(
        rwtxn,
        &(sidechain_number, sequence_number),
        &treasury_utxo,
    )?;
    let new_treasury_utxo_count = treasury_utxo_count + 1;
    dbs.active_sidechains.treasury_utxo_count.put(
        rwtxn,
        &sidechain_number,
        &new_treasury_utxo_count,
    )?;
    let new_ctip = Ctip {
        outpoint: new_ctip,
        value: new_treasury_value,
    };
    dbs.active_sidechains
        .put_ctip(rwtxn, sidechain_number, &new_ctip)?;
    Ok(Some(res))
}

/// Handles a (potential) M8 BMM request.
/// Returns `true` if this is a valid BMM request, `HandleM8Error::Jfyi` if
/// this is an invalid BMM request, and `false` if this is not a BMM request.
fn handle_m8(
    transaction: &Transaction,
    accepted_bmm_requests: &BmmCommitments,
    prev_mainchain_block_hash: &BlockHash,
) -> Result<bool, error::HandleM8> {
    let output = &transaction.output[0];
    let script = output.script_pubkey.to_bytes();

    if let Ok((_input, bmm_request)) = M8BmmRequest::parse(&script) {
        if accepted_bmm_requests
            .get(&bmm_request.sidechain_number)
            .is_none_or(|commitment| *commitment != bmm_request.sidechain_block_hash)
        {
            Err(error::HandleM8::NotAcceptedByMiners)
        } else if bmm_request.prev_mainchain_block_hash != prev_mainchain_block_hash.to_byte_array()
        {
            Err(error::HandleM8::BmmRequestExpired)
        } else {
            Ok(true)
        }
    } else {
        Ok(false)
    }
}

#[derive(Debug)]
enum CoinbaseMessageEvent {
    NewSidechainProposal { sidechain: Sidechain },
    WithdrawalBundle(WithdrawalBundleEvent),
}

fn handle_coinbase_message(
    rwtxn: &mut RwTxn,
    dbs: &Dbs,
    height: u32,
    accepted_bmm_requests: &mut BmmCommitments,
    message: CoinbaseMessage,
) -> Result<Option<CoinbaseMessageEvent>, error::ConnectBlock> {
    match message {
        CoinbaseMessage::M1ProposeSidechain(M1ProposeSidechain {
            sidechain_number,
            description,
        }) => {
            tracing::info!(
                "Propose sidechain number {sidechain_number} with data \"{}\"",
                String::from_utf8_lossy(&description.0)
            );
            let sidechain_proposal = SidechainProposal {
                sidechain_number,
                description,
            };
            if let Some(sidechain) =
                handle_m1_propose_sidechain(rwtxn, dbs, sidechain_proposal, height)?
            {
                let event = CoinbaseMessageEvent::NewSidechainProposal { sidechain };
                Ok(Some(event))
            } else {
                Ok(None)
            }
        }
        CoinbaseMessage::M2AckSidechain(M2AckSidechain {
            sidechain_number,
            description_hash,
        }) => {
            tracing::info!(
                "Ack sidechain number {sidechain_number} with proposal description hash {}",
                hex::encode(description_hash)
            );
            handle_m2_ack_sidechain(rwtxn, dbs, height, sidechain_number, description_hash)?;
            Ok(None)
        }
        CoinbaseMessage::M3ProposeBundle(M3ProposeBundle {
            sidechain_number,
            bundle_txid,
        }) => {
            let () =
                handle_m3_propose_bundle(rwtxn, dbs, sidechain_number, bundle_txid.into(), height)?;
            let event = CoinbaseMessageEvent::WithdrawalBundle(WithdrawalBundleEvent {
                sidechain_id: sidechain_number,
                m6id: bundle_txid.into(),
                kind: WithdrawalBundleEventKind::Submitted,
            });
            Ok(Some(event))
        }
        CoinbaseMessage::M4AckBundles(m4) => {
            let () = handle_m4_ack_bundles(rwtxn, dbs, &m4)?;
            Ok(None)
        }
        CoinbaseMessage::M7BmmAccept(M7BmmAccept {
            sidechain_number,
            sidechain_block_hash,
        }) => {
            if accepted_bmm_requests.contains_key(&sidechain_number) {
                return Err(error::ConnectBlock::MultipleBmmBlocks { sidechain_number });
            }
            accepted_bmm_requests.insert(sidechain_number, sidechain_block_hash);
            Ok(None)
        }
    }
}

#[derive(Debug)]
enum TransactionEvent {
    Deposit(Deposit),
    WithdrawalBundle(WithdrawalBundleEvent),
}

fn handle_transaction(
    rwtxn: &mut RwTxn,
    dbs: &Dbs,
    accepted_bmm_requests: &mut BmmCommitments,
    prev_mainchain_block_hash: &BlockHash,
    transaction: &Transaction,
) -> Result<Option<TransactionEvent>, error::ConnectBlock> {
    let mut res = None;
    match handle_m5_m6(rwtxn, dbs, Cow::Borrowed(transaction))? {
        Some(Either::Left(deposit)) => {
            res = Some(TransactionEvent::Deposit(deposit));
        }
        Some(Either::Right((sidechain_id, m6id, sequence_number))) => {
            let withdrawal_bundle_event = WithdrawalBundleEvent {
                m6id,
                sidechain_id,
                kind: WithdrawalBundleEventKind::Succeeded {
                    sequence_number,
                    transaction: transaction.clone(),
                },
            };
            res = Some(TransactionEvent::WithdrawalBundle(withdrawal_bundle_event));
        }
        None => (),
    };
    if handle_m8(
        transaction,
        accepted_bmm_requests,
        prev_mainchain_block_hash,
    )
    // We need to differentiate fatal and non-fatal errors. Non-fatal
    // errors should not cause the initial sync to exit! We therefore must take
    // care to not use the ? operator to exit from connect_block with an error
    .or_else(|err| match err.split() {
        Ok(just_for_info) => {
            tracing::warn!("Non-fatal error handling M8: {just_for_info:#}");
            Ok(false)
        }
        Err(err) => Err(error::ConnectBlock::M8(err.into())),
    })? {
        tracing::trace!(
            "Handled valid M8 BMM request in tx `{}`",
            transaction.compute_txid()
        );
    };
    Ok(res)
}

fn connect_block(
    rwtxn: &mut RwTxn,
    dbs: &Dbs,
    event_tx: &Sender<Event>,
    block: &Block,
    height: u32,
) -> Result<(), error::ConnectBlock> {
    let coinbase = &block.txdata[0];
    let mut coinbase_messages = CoinbaseMessages::new();
    // map of sidechain proposals to first vout
    let mut m1_sidechain_proposals = HashMap::new();
    for (vout, output) in coinbase.output.iter().enumerate() {
        let message = match CoinbaseMessage::parse(&output.script_pubkey) {
            Ok((rest, message)) => {
                if !rest.is_empty() {
                    tracing::warn!("Extra data in coinbase script: {:?}", hex::encode(rest));
                }
                message
            }
            Err(err) => {
                // Happens all the time. Would be nice to differentiate between "this isn't a BIP300 message"
                // and "we failed real bad".
                // TODO: format script as hex in error
                tracing::trace!("Failed to parse coinbase script: {:?}", err);
                continue;
            }
        };
        if let CoinbaseMessage::M1ProposeSidechain(M1ProposeSidechain {
            sidechain_number,
            description,
        }) = &message
        {
            let description_hash = description.sha256d_hash();
            m1_sidechain_proposals
                .entry((*sidechain_number, description_hash))
                .or_insert(vout as u32);
        }
        coinbase_messages.push(message)?;
    }
    let mut accepted_bmm_requests = BmmCommitments::new();
    let mut events = Vec::<BlockEvent>::new();
    let m4_exists = coinbase_messages.m4_exists();
    for message in coinbase_messages {
        match handle_coinbase_message(rwtxn, dbs, height, &mut accepted_bmm_requests, message)? {
            Some(CoinbaseMessageEvent::NewSidechainProposal { sidechain }) => {
                let proposal = sidechain.proposal;
                let description_hash = proposal.description.sha256d_hash();
                let index = m1_sidechain_proposals[&(proposal.sidechain_number, description_hash)];
                events.push(BlockEvent::SidechainProposal {
                    vout: index,
                    proposal,
                })
            }
            Some(CoinbaseMessageEvent::WithdrawalBundle(withdrawal_bundle_event)) => {
                events.push(withdrawal_bundle_event.into());
            }
            None => (),
        };
    }
    if !m4_exists {
        let active_sidechains_count = dbs.active_sidechains.sidechain().len(rwtxn)?;
        let upvotes = vec![M4AckBundles::ABSTAIN_ONE_BYTE; active_sidechains_count as usize];
        let m4 = M4AckBundles::OneByte { upvotes };
        let () = handle_m4_ack_bundles(rwtxn, dbs, &m4)?;
    }

    let () = handle_failed_sidechain_proposals(rwtxn, dbs, height)?;
    let failed_m6ids = handle_failed_m6ids(rwtxn, dbs, height)?;
    events.extend(failed_m6ids.into_iter().map(|(sidechain_id, m6id)| {
        WithdrawalBundleEvent {
            m6id,
            sidechain_id,
            kind: WithdrawalBundleEventKind::Failed,
        }
        .into()
    }));

    let block_hash = block.header.block_hash();
    let prev_mainchain_block_hash = block.header.prev_blockhash;

    for transaction in &block.txdata[1..] {
        match handle_transaction(
            rwtxn,
            dbs,
            &mut accepted_bmm_requests,
            &prev_mainchain_block_hash,
            transaction,
        )? {
            Some(TransactionEvent::Deposit(deposit)) => {
                events.push(deposit.into());
            }
            Some(TransactionEvent::WithdrawalBundle(withdrawal_bundle_event)) => {
                events.push(withdrawal_bundle_event.into());
            }
            None => (),
        }
    }

    let block_info = BlockInfo {
        bmm_commitments: accepted_bmm_requests.into_iter().collect(),
        coinbase_txid: coinbase.compute_txid(),
        events,
    };
    let () = dbs
        .block_hashes
        .put_block_info(rwtxn, &block_hash, &block_info)
        .map_err(error::ConnectBlock::PutBlockInfo)?;
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
) -> Result<(), error::DisconnectBlock> {
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
) -> Result<(), error::TxValidation> {
    todo!();
}

async fn sync_headers(
    dbs: &Dbs,
    main_client: &jsonrpsee::http_client::HttpClient,
    main_tip: BlockHash,
) -> Result<(), error::Sync> {
    let mut block_hash = main_tip;
    while let Some((latest_missing_header, latest_missing_header_height)) =
        tokio::task::block_in_place(|| {
            let rotxn = dbs.read_txn()?;
            match dbs
                .block_hashes
                .latest_missing_ancestor_header(&rotxn, block_hash)
                .map_err(error::Sync::DbTryGet)?
            {
                Some(latest_missing_header) => {
                    let height = dbs
                        .block_hashes
                        .height()
                        .try_get(&rotxn, &latest_missing_header)?;
                    Ok::<_, error::Sync>(Some((latest_missing_header, height)))
                }
                None => Ok(None),
            }
        })?
    {
        if let Some(latest_missing_header_height) = latest_missing_header_height {
            tracing::debug!("Syncing header #{latest_missing_header_height} `{latest_missing_header}` -> `{main_tip}`");
        } else {
            tracing::debug!("Syncing header `{latest_missing_header}` -> `{main_tip}`");
        }
        let header = main_client
            .getblockheader(latest_missing_header)
            .map_err(|err| error::Sync::JsonRpc {
                method: "getblockheader".to_owned(),
                source: err,
            })
            .await?;
        latest_missing_header_height.inspect(|height| assert_eq!(*height, header.height));
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
) -> Result<(), error::Sync> {
    let missing_blocks: Vec<BlockHash> = tokio::task::block_in_place(|| {
        let rotxn = dbs.read_txn()?;
        dbs.block_hashes
            .ancestor_headers(&rotxn, main_tip)
            .map(|(block_hash, _header)| Ok(block_hash))
            .take_while(|block_hash| Ok(!dbs.block_hashes.contains_block(&rotxn, block_hash)?))
            .collect()
            .map_err(error::Sync::from)
    })?;
    if missing_blocks.is_empty() {
        return Ok(());
    }
    for missing_block in missing_blocks.into_iter().rev() {
        tracing::debug!("Syncing block `{missing_block}` -> `{main_tip}`");
        let block = main_client
            .get_block(missing_block, U8Witness::<0>)
            .map_err(|err| error::Sync::JsonRpc {
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
) -> Result<(), error::Sync> {
    let () = sync_headers(dbs, main_client, main_tip).await?;
    let () = sync_blocks(dbs, event_tx, main_client, main_tip).await?;
    Ok(())
}

async fn initial_sync(
    dbs: &Dbs,
    event_tx: &Sender<Event>,
    main_client: &jsonrpsee::http_client::HttpClient,
) -> Result<(), error::Sync> {
    let main_tip: BlockHash = main_client
        .getbestblockhash()
        .map_err(|err| error::Sync::JsonRpc {
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
) -> Result<(), error::Fatal> {
    let zmq_sequence = crate::zmq::subscribe_sequence(zmq_addr_sequence)
        .await
        .map_err(error::Fatal::from)?;
    let () = initial_sync(dbs, event_tx, main_client)
        .await
        .or_else(|err| {
            let non_fatal: <error::Sync as fatality::Split>::Jfyi = err.split()?;
            let non_fatal = anyhow::Error::from(non_fatal);

            // In a way, this doesn't make sense. The initial sync exits, at
            // this point. We'd need to restart it?
            tracing::warn!("Non-fatal error during initial sync: {non_fatal:#}");
            Ok::<(), error::Fatal>(())
        })?;
    zmq_sequence
        .err_into::<error::Fatal>()
        .try_for_each(|msg| async move {
            match msg {
                SequenceMessage::BlockHashConnected(block_hash, _) => {
                    let () = sync_to_tip(dbs, event_tx, main_client, block_hash)
                        .await
                        .or_else(|err| {
                            let non_fatal: <error::Sync as fatality::Split>::Jfyi = err.split()?;
                            let non_fatal = anyhow::Error::from(non_fatal);
                            tracing::warn!("Error during sync to {block_hash}: {non_fatal:#}");
                            Ok::<(), error::Fatal>(())
                        })?;
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
        .map_err(error::Fatal::from)
}
