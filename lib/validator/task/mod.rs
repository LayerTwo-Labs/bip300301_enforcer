use std::{borrow::Cow, cmp::Ordering, collections::HashMap, time::Instant};

use async_broadcast::{Sender, TrySendError};
use bip300301::client::{GetBlockClient, U8Witness};
use bitcoin::{
    self,
    hashes::{sha256d, Hash as _},
    Amount, Block, BlockHash, OutPoint, Transaction, Work,
};
use either::Either;
use fallible_iterator::FallibleIterator;
use fatality::Split as _;
use futures::TryFutureExt as _;
use hashlink::LinkedHashSet;

use crate::{
    messages::{
        compute_m6id, parse_op_drivechain, CoinbaseMessage, CoinbaseMessages, M1ProposeSidechain,
        M2AckSidechain, M3ProposeBundle, M4AckBundles, M7BmmAccept, M8BmmRequest,
    },
    proto::mainchain::HeaderSyncProgress,
    types::{
        BlockEvent, BlockInfo, BmmCommitments, Ctip, Deposit, Event, HeaderInfo, M6id, Sidechain,
        SidechainNumber, SidechainProposal, SidechainProposalId, SidechainProposalStatus,
        WithdrawalBundleEvent, WithdrawalBundleEventKind, WITHDRAWAL_BUNDLE_INCLUSION_THRESHOLD,
        WITHDRAWAL_BUNDLE_MAX_AGE,
    },
    validator::dbs::{db_error, Dbs, RwTxn, UnitKey},
};

use super::main_rest_client::MainRestClient;

pub mod error;

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
    let proposal_id = proposal.compute_id();
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
    tracing::debug!(
        %sidechain_number,
        %description_hash,
        vote_count = %sidechain.status.vote_count,
        "ACK'd sidechain proposal");
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
            // doing `height - sidechain.status.proposal_height` can panic if the enforcer has data
            // from a previous sync that is not in the active chain.
            let sidechain_proposal_age = height.saturating_sub(sidechain.status.proposal_height);
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
    let (sidechain_number, new_ctip) = {
        let Some(treasury_output) = transaction.output.first() else {
            return Ok(None);
        };
        // If OP_DRIVECHAIN script is invalid,
        // for example if it is missing OP_TRUE at the end,
        // it will just be ignored.
        if let Ok((_input, sidechain_number)) =
            parse_op_drivechain(&treasury_output.script_pubkey.to_bytes())
        {
            let new_ctip = Ctip {
                outpoint: OutPoint { txid, vout: 0 },
                value: treasury_output.value,
            };
            (sidechain_number, new_ctip)
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
    match new_ctip.value.cmp(&old_treasury_value) {
        // M6
        Ordering::Less => {
            if transaction.input.len() != 1 {
                return Ok(None);
            }
            let (m6id, sidechain_number_) =
                handle_m6(rwtxn, dbs, transaction.into_owned(), old_treasury_value)?;
            assert_eq!(sidechain_number, sidechain_number_);
            let sequence_number =
                dbs.active_sidechains
                    .put_ctip(rwtxn, sidechain_number, &new_ctip)?;
            let res = Either::Right((sidechain_number, m6id, sequence_number));
            Ok(Some(res))
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
            let sequence_number =
                dbs.active_sidechains
                    .put_ctip(rwtxn, sidechain_number, &new_ctip)?;
            let deposit = Deposit {
                sequence_number,
                sidechain_id: sidechain_number,
                outpoint: new_ctip.outpoint,
                address,
                value: new_ctip.value - old_treasury_value,
            };
            Ok(Some(Either::Left(deposit)))
        }
        Ordering::Equal => {
            // FIXME: is it ok to treat this as a regular TX?
            Ok(None)
        }
    }
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
) -> Result<Option<TransactionEvent>, error::HandleTransaction> {
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
    .map_err(|err| match err.split() {
        Ok(just_for_info) => {
            tracing::warn!("Non-fatal error handling M8: {just_for_info:#}");
            error::HandleTransaction::M8(just_for_info.into())
        }
        Err(err) => error::HandleTransaction::M8(err.into()),
    })? {
        tracing::trace!(
            "Handled valid M8 BMM request in tx `{}`",
            transaction.compute_txid()
        );
    };
    Ok(res)
}

/// Check if a tx is valid against the current tip.
pub fn validate_tx(
    dbs: &Dbs,
    transaction: &Transaction,
) -> Result<bool, error::ValidateTransaction> {
    let mut rwtxn = dbs.write_txn()?;
    let tip_hash = dbs
        .current_chain_tip
        .try_get(&rwtxn, &UnitKey)?
        .ok_or(error::ValidateTransactionInner::NoChainTip)?;
    match handle_transaction(
        &mut rwtxn,
        dbs,
        &mut BmmCommitments::new(),
        &tip_hash,
        transaction,
    ) {
        Ok(_) => Ok(true),
        Err(err) => match err.split() {
            Ok(_jfyi) => Ok(false),
            Err(err) => Err(err.into()),
        },
    }
}

/// Block header should be stored before calling this.
#[tracing::instrument(skip_all)]
pub(in crate::validator) fn connect_block(
    rwtxn: &mut RwTxn,
    dbs: &Dbs,
    block: &Block,
) -> Result<Event, error::ConnectBlock> {
    let parent = block.header.prev_blockhash;

    tracing::trace!("verifying chain tip is block parent");
    // Check that current chain tip is block parent
    match dbs.current_chain_tip.try_get(rwtxn, &UnitKey)? {
        Some(tip) if parent == tip => (),
        Some(tip) => {
            let tip_height = dbs
                .block_hashes
                .height()
                .get(rwtxn, &tip)
                .unwrap_or_default();
            tracing::error!(
                chain_tip = %tip,
                incoming_block_parent = %parent,
                "unable to connect block: chain tip is not parent of incoming block"
            );
            return Err(error::ConnectBlock::BlockParent {
                parent,
                tip,
                tip_height,
            });
        }
        None if block.header.prev_blockhash == BlockHash::all_zeros() => (),
        None => {
            return Err(error::ConnectBlock::BlockParent {
                parent,
                tip: BlockHash::all_zeros(),
                tip_height: 0,
            })
        }
    }

    tracing::trace!("starting block processing");
    let height = dbs.block_hashes.height().get(rwtxn, &block.block_hash())?;
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
                tracing::trace!(
                    script = hex::encode(output.script_pubkey.to_bytes()),
                    "Failed to parse coinbase script: {:?}",
                    err
                );
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
    tracing::trace!("Handled coinbase tx, handling other txs...");
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
    tracing::trace!("Handled block txs");
    let block_info = BlockInfo {
        bmm_commitments: accepted_bmm_requests.into_iter().collect(),
        coinbase_txid: coinbase.compute_txid(),
        events,
    };
    tracing::trace!("Storing block info");
    let () = dbs
        .block_hashes
        .put_block_info(rwtxn, &block_hash, &block_info)
        .map_err(error::ConnectBlock::PutBlockInfo)?;
    tracing::trace!("Stored block info");
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
        tracing::debug!("updated current chain tip: {}", height);
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
    Ok(event)
}

// TODO: Add unit tests ensuring that `connect_block` and `disconnect_block` are inverse
// operations.
pub(in crate::validator) fn disconnect_block(
    _rwtxn: &mut RwTxn,
    _dbs: &Dbs,
    event_tx: &Sender<Event>,
    block_hash: BlockHash,
) -> Result<(), error::DisconnectBlock> {
    // FIXME: implement
    let event = Event::DisconnectBlock { block_hash };
    let _send_err: Result<Option<_>, TrySendError<_>> = event_tx.try_broadcast(event);
    todo!();
}

fn fetch_chain_tip_enforcer_db(dbs: &Dbs) -> Result<Option<(BlockHash, u32)>, Box<error::Sync>> {
    let rotxn = dbs
        .read_txn()
        .map_err(|err| Box::new(error::Sync::from(err)))?;

    let current_enforcer_tip_opt = dbs
        .current_chain_tip
        .try_get(&rotxn, &UnitKey)
        .map_err(|err| Box::new(error::Sync::from(err)))?;

    let tip = match current_enforcer_tip_opt {
        Some(tip) => tip,
        None => {
            return Ok(None);
        }
    };

    let tip_height = dbs
        .block_hashes
        .height()
        .get(&rotxn, &tip)
        .map_err(|err| Box::new(error::Sync::from(err)))?;

    Ok(Some((tip, tip_height)))
}

// Find the latest chain point both in the enforcer chain and the active chain.
// Returns hash and height of the best common block.
// This returns a boxed error to satisfy a Clippy complaint.
async fn fetch_common_chain_point<MainRpcClient>(
    dbs: &Dbs,
    mainchain: &MainRpcClient,
    chain_tip_enforcer_db: Option<(BlockHash, u32)>, // The currently stored enforcer tip
) -> Result<(BlockHash, u32), Box<error::Sync>>
where
    MainRpcClient: bip300301::client::MainClient + Sync,
{
    let mut max_height = mainchain
        .getblockcount()
        .await
        .map(|count| count as u32)
        .map_err(|err| {
            Box::new(error::Sync::JsonRpc {
                method: "getblockcount".to_owned(),
                source: err,
            })
        })?;

    let (enforcer_tip, mut tip_height) = match chain_tip_enforcer_db {
        Some((tip, tip_height)) => (tip, tip_height),
        None => {
            let genesis_hash =
                mainchain
                    .getblockhash(0)
                    .await
                    .map_err(|err| error::Sync::JsonRpc {
                        method: "getblockhash".to_owned(),
                        source: err,
                    })?;

            // Break out early, we're at genesis!
            return Ok((genesis_hash, 0));
        }
    };

    // The earliest point where we know that the enforcer tip is in the active chain.
    let mut known_good_height = 0; // Always good at genesis!

    let increase_tip_height =
        |tip_height: u32, max_height: u32| std::cmp::min(tip_height * 2, max_height);

    let decrease_tip_height = |tip_height: u32, known_good_height: u32| {
        let diff = tip_height - known_good_height;
        tip_height - diff / 2
    };

    // When now need to verify that the enforcer tip is actually in
    // the active chain. If this is not the case, we do a binary search
    // to find the latest enforcer block that is in the active chain, and
    // return that.

    tracing::debug!(
        enforcer_tip = ?enforcer_tip,
        enforcer_tip_height = tip_height,
        "Verifying enforcer tip is in active chain"
    );
    loop {
        let mut best_common_tip = enforcer_tip;
        let mut best_common_tip_height = tip_height;
        (best_common_tip, best_common_tip_height) = match mainchain
            .getblockhash(tip_height as usize)
            .await
        {
            Ok(active_chain) => {
                let header_info = {
                    let rotxn = dbs
                        .read_txn()
                        .map_err(|err| Box::new(error::Sync::from(err)))?;

                    dbs.block_hashes
                        .try_get_header_info(&rotxn, &active_chain)
                        .map_err(|err| Box::new(error::Sync::from(err)))
                };

                let enforcer_header = match header_info {
                    Ok(Some(header)) => header,
                    Ok(None) => {
                        if tip_height == known_good_height + 1 {
                            tracing::debug!(
                                enforcer_chain = ?enforcer_tip,
                                block_height = best_common_tip_height,
                                "Missing header known_good_height + 1, returning"
                            );
                            return Ok((best_common_tip, best_common_tip_height));
                        } else {
                            tracing::debug!(
                                active_chain = ?active_chain,
                                "Enforcer does not have header at height {tip_height}, decreasing"
                            );
                            if tip_height == max_height {
                                tracing::trace!(
                                    "Decreasing max_height {max_height} -> {}",
                                    max_height - 1
                                );
                                max_height -= 1;
                            }
                            tip_height = decrease_tip_height(tip_height, known_good_height);
                            continue;
                        }
                    }
                    Err(err) => return Err(err),
                };

                // We've reached the end of our search!
                if enforcer_header.block_hash == active_chain
                    && (tip_height == known_good_height || tip_height == max_height)
                {
                    tracing::debug!(
                        enforcer_chain = ?enforcer_tip,
                        "Enforcer and active chain agree at height {tip_height}, returning"
                    );
                    Ok((enforcer_header.block_hash, enforcer_header.height))
                // The enforcer and the active chain are in agreement,
                // at least at this point.
                } else if enforcer_header.block_hash == active_chain {
                    tracing::debug!(
                        active_chain = ?active_chain,
                        enforcer_chain = ?enforcer_header.block_hash,
                        "Enforcer and active chain agree at height {tip_height}, increasing"
                    );

                    known_good_height = tip_height;
                    tip_height = increase_tip_height(tip_height, max_height);
                    continue;
                } else {
                    tracing::debug!(
                        active_chain = ?active_chain,
                        enforcer_chain = ?enforcer_tip,
                        "Enforcer and active chain disagree at height {tip_height}, decreasing"
                    );
                    if tip_height == max_height {
                        tracing::trace!("Decreasing max_height {max_height} -> {}", max_height - 1);
                        max_height -= 1;
                    }
                    tip_height = decrease_tip_height(tip_height, known_good_height);
                    continue;
                }
            }
            // https://github.com/bitcoin/bitcoin/blob/9a4c92eb9ac29204df3d826f5ae526ab16b8ad65/src/rpc/protocol.h#L44
            // -8 is the error code for an invalid argument, i.e. an out of range block hash
            Err(jsonrpsee::core::ClientError::Call(err))
                if err.code() == -8 &&
                // Take care to only hit this branch if we're actually able to go lower. Edge case, 
                // but if mainchain is on a freshly wiped regtest network while the enforcer is on
                // a network with blocks, we'll end up looping at block height 1 without this check 
                tip_height > known_good_height + 1 =>
            {
                tracing::debug!(
                    known_good_height = known_good_height,
                    tip_height = tip_height,
                    "Block hash out of range at height {tip_height}, decreasing"
                );
                if tip_height == max_height {
                    tracing::trace!("Decreasing max_height {max_height} -> {}", max_height - 1);
                    max_height -= 1;
                }
                tip_height = decrease_tip_height(tip_height, known_good_height);
                continue;
            }
            Err(err) => Err(Box::new(error::Sync::JsonRpc {
                method: "getblockhash".to_owned(),
                source: err,
            })),
        }?;

        return Ok((best_common_tip, best_common_tip_height));
    }
}

#[tracing::instrument(skip_all)]
async fn sync_headers<MainRpcClient>(
    dbs: &Dbs,
    main_rest_client: &MainRestClient,
    main_rpc_client: &MainRpcClient,
    main_tip: BlockHash,
    progress_tx: &tokio::sync::watch::Sender<HeaderSyncProgress>,
) -> Result<(), error::Sync>
where
    MainRpcClient: bip300301::client::MainClient + Sync,
{
    let start = Instant::now();

    let chain_tip_enforcer_db = fetch_chain_tip_enforcer_db(dbs).map_err(|boxed| *boxed)?;

    let (common_chain_tip, common_chain_tip_height) =
        fetch_common_chain_point(dbs, main_rpc_client, chain_tip_enforcer_db)
            .await
            .map_err(|boxed| *boxed)?;

    // If the enforcer tip is /ahead/ of the active chain, we need to move the enforcer tip back
    // to the common chain tip.
    match chain_tip_enforcer_db {
        Some((block_hash, height)) => {
            let mut rwtxn = dbs.write_txn()?;

            let mut header = dbs.block_hashes.get_header_info(&rwtxn, &block_hash)?;

            // Figure out which header we should move the enforcer tip back
            // to (if any).

            while !dbs
                .block_hashes
                .contains_block(&rwtxn, &header.block_hash)?
                || (header.height > common_chain_tip_height + 1)
                // If we're at the same height we can still be on the wrong chain
                || (header.height == common_chain_tip_height && header.block_hash != common_chain_tip)
            {
                header = dbs
                    .block_hashes
                    .get_header_info(&rwtxn, &header.prev_block_hash)?;
            }

            // The tip should be updated if:
            // 1. We went through at least one iteration of the loop, this means the tip
            // 2. The enforcer tip is ahead of the active chain.
            if header.block_hash != block_hash || height > common_chain_tip_height {
                tracing::warn!(
                    "Enforcer tip (#{height}) is ahead of the active chain (#{common_chain_tip_height}), moving back to #{} `{}`",
                    header.height-   1, header.prev_block_hash
                );
                let () =
                    dbs.current_chain_tip
                        .put(&mut rwtxn, &UnitKey, &header.prev_block_hash)?;
                let () = rwtxn.commit()?;
            } else {
                tracing::trace!(
                    "Enforcer tip (#{height}) is already at the common chain tip (#{common_chain_tip_height}), nothing to do"
                );
            }
        }
        None => {
            tracing::trace!("no enforcer tip stored");
        }
    }

    if common_chain_tip == main_tip {
        tracing::debug!(main_tip = %main_tip, "already have all headers up to #{common_chain_tip_height}!");
        return Ok(());
    }

    let mut current_block_hash = common_chain_tip;
    let mut current_height = common_chain_tip_height;

    // Fetch headers in batches for efficiency
    // 2000 is max allowed by Bitcoin Core
    const HEADER_FETCH_BATCH_SIZE: usize = 2000;

    tracing::debug!(
        "Syncing headers starting from #{current_height} `{current_block_hash}` up to `{main_tip}`"
    );

    // The requested block header is the /first/ header in the batch. This means we
    // need to loop from our current tip, until we find the requested main tip.
    loop {
        tracing::debug!(
            "Fetching batch of headers starting from #{current_height} `{current_block_hash}`"
        );

        // Fetch a batch of headers
        let headers = main_rest_client
            .get_block_headers(&current_block_hash, HEADER_FETCH_BATCH_SIZE)
            .map_err(error::Sync::Rest)
            .await?;

        // This will be empty if the requested block hash is not in the current active chain,
        // i.e. if we're dealing with a reorg (or was given an invalid block hash to begin with).
        if headers.is_empty() {
            // Syncing headers is a reasonably quick operation. We therefore deal with it by erroring out
            // here, and then retrying the sync from further out in the call stack.
            //
            // This branch only hits if we're dealing with a reorg /while/ we're syncing headers.
            // If we've reorged before starting the sync, it is picked up at the beginning. It is
            // such an edge case that we don't bother implementing it (for now, at least)
            return Err(error::Sync::BlockNotInActiveChain {
                block_hash: current_block_hash,
            });
        }

        // Update the block_hash to the oldest header fetched in this batch to continue the loop
        let (_, last_block_hash, last_block_height) = headers.last().unwrap();
        current_block_hash = *last_block_hash;
        current_height = *last_block_height;

        // Store the fetched headers
        let mut rwtxn = dbs.write_txn()?;
        dbs.block_hashes.put_headers(
            &mut rwtxn,
            &headers
                .iter()
                .map(|(header, _, height)| (*header, *height))
                .collect::<Vec<_>>(),
        )?;
        let () = rwtxn.commit()?;

        // Send progress update
        let progress = HeaderSyncProgress {
            current_height: Some(current_height),
        };
        if let Err(err) = progress_tx.send(progress) {
            tracing::warn!("Failed to send header sync progress: {err:#}");
        }

        // Important: break out here /after/ storing the last header.
        if current_block_hash == main_tip {
            tracing::info!(main_tip = %main_tip, "Reached main tip at height {current_height}!");
            break;
        }
    }

    // After finishing the loop, current_height is the height of the active chain.
    // If the active chain /regressed/ (i.e. `invalidateblock`), we can end up
    // in a situation where the current enforcer tip is /ahead/ of the active chain,
    // on an invalid fork.
    let mut rwtxn = dbs.write_txn()?;
    let current_enforcer_tip_height = dbs.block_hashes.height().get(&rwtxn, &common_chain_tip)?;

    if current_enforcer_tip_height > current_height {
        tracing::warn!("Enforcer tip (#{current_enforcer_tip_height}) is ahead of the active chain (#{current_height}), moving back to `{current_block_hash}`");
        let () = dbs
            .current_chain_tip
            .put(&mut rwtxn, &UnitKey, &current_block_hash)?;
        let () = rwtxn.commit()?;
    }

    tracing::info!(
        main_tip = ?main_tip,
        "Synced {} headers in {:?}",
        current_height - current_enforcer_tip_height, start.elapsed()
    );
    Ok(())
}

// MUST be called after `sync_headers`.
#[tracing::instrument(skip_all)]
async fn sync_blocks<MainRpcClient>(
    dbs: &Dbs,
    event_tx: &Sender<Event>,
    main_rpc_client: &MainRpcClient,
    main_tip: BlockHash,
) -> Result<(), error::Sync>
where
    MainRpcClient: bip300301::client::MainClient + Sync,
{
    let start = Instant::now();
    let missing_blocks = tokio::task::block_in_place(|| {
        let rotxn = dbs.read_txn()?;

        dbs.block_hashes
            .ancestor_headers(&rotxn, main_tip)
            .map(|(block_hash, _)| Ok(block_hash))
            .take_while(|block_hash| Ok(!dbs.block_hashes.contains_block(&rotxn, block_hash)?))
            .collect::<Vec<_>>()
            .map_err(error::Sync::from)
    })?;

    if missing_blocks.is_empty() {
        tracing::info!("No missing blocks, skipping sync");
        return Ok(());
    }

    tracing::info!(
        "identified {} missing blocks in {:?}, starting sync",
        missing_blocks.len(),
        start.elapsed()
    );

    let mut total_blocks_fetched = 0;
    for missing_block in missing_blocks.into_iter().rev() {
        let block = main_rpc_client
            .get_block(missing_block, U8Witness::<0>)
            .map_err(|err| error::Sync::JsonRpc {
                method: "getblock".to_owned(),
                source: err,
            })
            .await?
            .0;
        total_blocks_fetched += 1;

        let mut rwtxn = dbs.write_txn()?;

        let height = dbs.block_hashes.height().get(&rwtxn, &missing_block)?;

        tracing::debug!("Syncing block #{height} `{missing_block}` -> `{main_tip}`",);

        // We should not call out to `invalidateblock` in case of failures here,
        // as that is handled by the cusf-enforcer-mempool crate.
        // FIXME: handle disconnects
        let event = connect_block(&mut rwtxn, dbs, &block)?;
        tracing::trace!("connected block at height {height}: {missing_block}");
        let () = rwtxn.commit()?;
        // Events should only ever be sent after committing DB txs, see
        // https://github.com/LayerTwo-Labs/bip300301_enforcer/pull/185
        let _send_err: Result<Option<_>, TrySendError<_>> = event_tx.try_broadcast(event);
    }
    tracing::info!(
        "Synced {total_blocks_fetched} blocks in {:?}",
        start.elapsed()
    );
    Ok(())
}

pub(in crate::validator) async fn sync_to_tip<MainClient>(
    dbs: &Dbs,
    event_tx: &Sender<Event>,
    header_sync_progress_tx: &tokio::sync::watch::Sender<HeaderSyncProgress>,
    main_rpc_client: &MainClient,
    main_rest_client: &MainRestClient,
    main_tip: BlockHash,
) -> Result<(), error::Sync>
where
    MainClient: bip300301::client::MainClient + Sync,
{
    let () = sync_headers(
        dbs,
        main_rest_client,
        main_rpc_client,
        main_tip,
        header_sync_progress_tx,
    )
    .await?;
    let () = sync_blocks(dbs, event_tx, main_rpc_client, main_tip).await?;
    Ok(())
}
