use std::{
    borrow::Cow,
    cmp::Ordering,
    collections::{BTreeMap, HashMap, HashSet},
    future::Future,
    time::Instant,
};

use async_broadcast::{Sender, TrySendError};
use bitcoin::{
    Amount, Block, BlockHash, OutPoint, Transaction, Work,
    hashes::{Hash as _, sha256d},
};
use either::Either;
use fallible_iterator::FallibleIterator;
use fatality::Split as _;
use futures::FutureExt as _;
use jsonrpsee::core::{
    client::BatchResponse,
    params::{ArrayParams, BatchRequestBuilder},
};
use sneed::{RoTxn, RwTxn, db};

use super::main_rest_client::MainRestClient;
use crate::{
    messages::{
        CoinbaseMessage, CoinbaseMessages, M1ProposeSidechain, M2AckSidechain, M3ProposeBundle,
        M4AckBundles, M7BmmAccept, compute_m6id, parse_m8_tx, parse_op_drivechain,
    },
    proto::mainchain::HeaderSyncProgress,
    types::{
        BlockEvent, BlockInfo, BmmCommitments, Ctip, Deposit, Event, HeaderInfo, M6id,
        PendingM6idInfo, Sidechain, SidechainNumber, SidechainProposal, SidechainProposalId,
        SidechainProposalStatus, WITHDRAWAL_BUNDLE_INCLUSION_THRESHOLD, WITHDRAWAL_BUNDLE_MAX_AGE,
        WithdrawalBundleEvent, WithdrawalBundleEventKind,
    },
    validator::dbs::{
        ActiveSidechainDbs, Dbs,
        diff::{self, Diff},
    },
};

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
    rotxn: &RoTxn,
    dbs: &Dbs,
    proposal: SidechainProposal,
    proposal_height: u32,
) -> Result<Option<diff::NewSidechainProposal>, error::HandleM1ProposeSidechain> {
    let proposal_id = proposal.compute_id();
    // FIXME: check that the proposal was made in an ancestor block
    if dbs
        .proposal_id_to_sidechain
        .contains_key(rotxn, &proposal_id)?
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

    let diff = diff::NewSidechainProposal {
        id: proposal_id,
        sidechain,
    };
    Ok(Some(diff))
}

// See https://github.com/LayerTwo-Labs/bip300_bip301_specifications/blob/master/bip300.md#m2-1
fn handle_m2_ack_sidechain(
    rotxn: &RoTxn,
    dbs: &Dbs,
    height: u32,
    sidechain_number: SidechainNumber,
    description_hash: sha256d::Hash,
) -> Result<diff::AckSidechain, error::HandleM2AckSidechain> {
    let proposal_id = SidechainProposalId {
        sidechain_number,
        description_hash,
    };
    let sidechain = dbs.proposal_id_to_sidechain.try_get(rotxn, &proposal_id)?;
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
    let mut diff = diff::AckSidechain {
        id: proposal_id,
        activated: false,
    };

    let sidechain_proposal_age = height - sidechain.status.proposal_height;

    let sidechain_slot_is_used = dbs
        .active_sidechains
        .sidechain()
        .try_get(rotxn, &sidechain_number)?
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
        diff.activated = true;
    }
    Ok(diff)
}

fn handle_failed_sidechain_proposals(
    rotxn: &RoTxn,
    dbs: &Dbs,
    height: u32,
) -> Result<diff::FailedProposals, error::HandleFailedSidechainProposals> {
    let failed_proposals = dbs
        .proposal_id_to_sidechain
        .iter(rotxn)
        .map_err(db::Error::from)?
        .map_err(db::Error::from)
        .filter(|(_proposal_id, sidechain)| {
            // doing `height - sidechain.status.proposal_height` can panic if the enforcer has data
            // from a previous sync that is not in the active chain.
            let sidechain_proposal_age = height.saturating_sub(sidechain.status.proposal_height);
            let sidechain_slot_is_used = dbs
                .active_sidechains
                .sidechain()
                .contains_key(rotxn, &sidechain.proposal.sidechain_number)?;
            // FIXME: Do we need to check that the vote_count is below the threshold, or is it
            // enough to check that the max age was exceeded?
            let failed = sidechain_slot_is_used
                && sidechain_proposal_age > USED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE as u32
                || !sidechain_slot_is_used
                    && sidechain_proposal_age > UNUSED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE as u32;
            Ok(failed)
        })
        .collect()?;
    Ok(diff::FailedProposals(failed_proposals))
}

fn handle_m3_propose_bundle(
    rotxn: &RoTxn,
    dbs: &Dbs,
    sidechain_number: SidechainNumber,
    m6id: M6id,
) -> Result<diff::ProposeBundle, error::HandleM3ProposeBundle> {
    if dbs
        .active_sidechains
        .sidechain()
        .contains_key(rotxn, &sidechain_number)?
    {
        let diff = diff::ProposeBundle {
            m6id,
            sidechain_number,
        };
        Ok(diff)
    } else {
        Err(error::HandleM3ProposeBundle::InactiveSidechain { sidechain_number })
    }
}

fn handle_m4_votes(
    rotxn: &RoTxn,
    dbs: &Dbs,
    upvotes: &[u16],
) -> Result<diff::AckBundles, error::HandleM4Votes> {
    let active_sidechains: Vec<_> = dbs
        .active_sidechains
        .sidechain()
        .iter(rotxn)?
        .map(|(sidechain_number, _)| Ok(sidechain_number))
        .collect()?;
    if upvotes.len() != active_sidechains.len() {
        return Err(error::HandleM4Votes::InvalidVotes {
            expected: active_sidechains.len(),
            len: upvotes.len(),
        });
    }
    let mut diff = diff::AckBundles::default();
    for (idx, vote) in upvotes.iter().enumerate() {
        let sidechain_number = active_sidechains[idx];
        let vote = *vote;
        if vote == M4AckBundles::ABSTAIN_TWO_BYTES {
            continue;
        }
        let pending_withdrawals = dbs
            .active_sidechains
            .pending_m6ids()
            .get(rotxn, &sidechain_number)?;
        if vote == M4AckBundles::ALARM_TWO_BYTES {
            let positive_votes_proposals = pending_withdrawals
                .into_iter()
                .filter_map(|(m6id, info)| {
                    if info.vote_count > 0 {
                        Some(m6id)
                    } else {
                        None
                    }
                })
                .collect::<HashSet<_>>();
            if !positive_votes_proposals.is_empty() {
                diff.0.insert(
                    sidechain_number,
                    diff::AckBundleAction::Alarm {
                        positive_votes_proposals,
                    },
                );
            }
        } else if let Some((m6id, info)) = pending_withdrawals.get_index(vote as usize) {
            if info.vote_count < u16::MAX {
                diff.0.insert(
                    sidechain_number,
                    diff::AckBundleAction::Upvote { m6id: *m6id },
                );
            }
        } else {
            return Err(error::HandleM4Votes::UpvoteFailed {
                sidechain_number,
                index: vote,
            });
        }
    }
    Ok(diff)
}

fn handle_m4_ack_bundles(
    rotxn: &RoTxn,
    dbs: &Dbs,
    m4: &M4AckBundles,
) -> Result<diff::AckBundles, error::HandleM4AckBundles> {
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
            handle_m4_votes(rotxn, dbs, &upvotes).map_err(error::HandleM4AckBundles::from)
        }
        M4AckBundles::TwoBytes { upvotes } => {
            handle_m4_votes(rotxn, dbs, upvotes).map_err(error::HandleM4AckBundles::from)
        }
    }
}

/// Returns failed M6IDs with sidechain numbers
fn handle_failed_m6ids(
    rotxn: &RoTxn,
    dbs: &Dbs,
    block_height: u32,
) -> Result<diff::FailedM6ids, error::HandleFailedM6Ids> {
    let active_sidechains: Vec<_> = dbs
        .active_sidechains
        .sidechain()
        .lazy_decode()
        .iter(rotxn)?
        .map(|(sidechain_number, _)| Ok(sidechain_number))
        .collect()?;

    let mut failed_m6ids = HashMap::<_, BTreeMap<_, _>>::new();

    for sidechain_number in active_sidechains {
        let pending_m6ids = dbs
            .active_sidechains
            .pending_m6ids()
            .try_get(rotxn, &sidechain_number)?
            .expect("active sidechain should exist as key");
        let failed = pending_m6ids
            .into_iter()
            .enumerate()
            .filter(|(_, (_, info))| {
                let age = block_height.saturating_sub(info.proposal_height);
                age > WITHDRAWAL_BUNDLE_MAX_AGE as u32
            })
            .collect::<BTreeMap<_, _>>();
        if !failed.is_empty() {
            failed_m6ids.insert(sidechain_number, failed);
        }
    }

    Ok(diff::FailedM6ids(failed_m6ids))
}

/// Deposit or (sidechain_id, m6id, sequence_number)
type DepositOrSuccessfulWithdrawal = Either<Deposit, (SidechainNumber, M6id, u64)>;

/// Returns (sidechain_id, m6id, info) for the withdrawal
fn handle_m6(
    rotxn: &RoTxn,
    dbs: &ActiveSidechainDbs,
    transaction: Transaction,
    old_treasury_value: Amount,
) -> Result<(M6id, SidechainNumber, PendingM6idInfo), error::HandleM5M6> {
    let (m6id, sidechain_number) = compute_m6id(transaction, old_treasury_value)?;

    let pending_m6ids = dbs
        .pending_m6ids()
        .try_get(rotxn, &sidechain_number)?
        .ok_or(error::HandleM5M6::InvalidM6)?;
    let info = pending_m6ids
        .get(&m6id)
        .ok_or(error::HandleM5M6::InvalidM6)?;
    if info.vote_count > WITHDRAWAL_BUNDLE_INCLUSION_THRESHOLD {
        Ok((m6id, sidechain_number, *info))
    } else {
        Err(error::HandleM5M6::InvalidM6)
    }
}

fn handle_m5_m6(
    rotxn: &RoTxn,
    dbs: &ActiveSidechainDbs,
    transaction: Cow<'_, Transaction>,
) -> Result<Option<(DepositOrSuccessfulWithdrawal, diff::Tx)>, error::HandleM5M6> {
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
        if let Some(old_ctip) = dbs.ctip().try_get(rotxn, &sidechain_number)? {
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
            let (m6id, sidechain_number_, info) =
                handle_m6(rotxn, dbs, transaction.into_owned(), old_treasury_value)?;
            assert_eq!(sidechain_number, sidechain_number_);
            let sequence_number = dbs
                .treasury_utxo_count
                .try_get(rotxn, &sidechain_number)?
                .unwrap_or(0);
            let diff = diff::Tx {
                sidechain_number,
                new_ctip,
                removed_pending_withdrawal: Some((m6id, info)),
            };
            let res = Either::Right((sidechain_number, m6id, sequence_number));
            Ok(Some((res, diff)))
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
            let sequence_number = dbs
                .treasury_utxo_count
                .try_get(rotxn, &sidechain_number)?
                .unwrap_or(0);
            let deposit = Deposit {
                sequence_number,
                sidechain_id: sidechain_number,
                outpoint: new_ctip.outpoint,
                address,
                value: new_ctip.value - old_treasury_value,
            };
            let res = Either::Left(deposit);
            let diff = diff::Tx {
                sidechain_number,
                new_ctip,
                removed_pending_withdrawal: None,
            };
            Ok(Some((res, diff)))
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
    accepted_bmm_requests: Option<&BmmCommitments>,
    prev_mainchain_block_hash: &BlockHash,
) -> Result<bool, error::HandleM8> {
    let Some(bmm_request) = parse_m8_tx(transaction) else {
        return Ok(false);
    };
    if let Some(accepted_bmm_requests) = accepted_bmm_requests
        && accepted_bmm_requests
            .get(&bmm_request.sidechain_number)
            .is_none_or(|commitment| *commitment != bmm_request.sidechain_block_hash)
    {
        Err(error::HandleM8::NotAcceptedByMiners)
    } else if bmm_request.prev_mainchain_block_hash != *prev_mainchain_block_hash {
        Err(error::HandleM8::BmmRequestExpired)
    } else {
        Ok(true)
    }
}

#[derive(Debug)]
enum CoinbaseMessageEvent {
    NewSidechainProposal { sidechain: Sidechain },
    WithdrawalBundle(WithdrawalBundleEvent),
}

fn handle_coinbase_message(
    rotxn: &RoTxn,
    dbs: &Dbs,
    height: u32,
    accepted_bmm_requests: &mut BmmCommitments,
    message: CoinbaseMessage,
) -> Result<(Option<CoinbaseMessageEvent>, Option<diff::CoinbaseMsg>), error::ConnectBlock> {
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
            if let Some(diff) = handle_m1_propose_sidechain(rotxn, dbs, sidechain_proposal, height)?
            {
                let event = CoinbaseMessageEvent::NewSidechainProposal {
                    sidechain: diff.sidechain.clone(),
                };
                let diff = diff::CoinbaseMsg::NewSidechainProposal(diff);
                Ok((Some(event), Some(diff)))
            } else {
                Ok((None, None))
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
            let diff =
                handle_m2_ack_sidechain(rotxn, dbs, height, sidechain_number, description_hash)?;
            Ok((None, Some(diff::CoinbaseMsg::AckSidechain(diff))))
        }
        CoinbaseMessage::M3ProposeBundle(M3ProposeBundle {
            sidechain_number,
            bundle_txid,
        }) => {
            let diff = handle_m3_propose_bundle(rotxn, dbs, sidechain_number, bundle_txid.into())?;
            let event = CoinbaseMessageEvent::WithdrawalBundle(WithdrawalBundleEvent {
                sidechain_id: sidechain_number,
                m6id: bundle_txid.into(),
                kind: WithdrawalBundleEventKind::Submitted,
            });
            Ok((Some(event), Some(diff::CoinbaseMsg::ProposeBundle(diff))))
        }
        CoinbaseMessage::M4AckBundles(m4) => {
            let diff = handle_m4_ack_bundles(rotxn, dbs, &m4)?;
            let diff = if diff.0.is_empty() {
                None
            } else {
                Some(diff::CoinbaseMsg::AckBundles(diff))
            };
            Ok((None, diff))
        }
        CoinbaseMessage::M7BmmAccept(M7BmmAccept {
            sidechain_number,
            sidechain_block_hash,
        }) => {
            if accepted_bmm_requests.contains_key(&sidechain_number) {
                return Err(error::ConnectBlock::MultipleBmmBlocks { sidechain_number });
            }
            accepted_bmm_requests.insert(sidechain_number, sidechain_block_hash);
            Ok((None, None))
        }
    }
}

#[derive(Debug)]
enum TransactionEvent {
    Deposit(Deposit),
    WithdrawalBundle(WithdrawalBundleEvent),
}

fn handle_transaction(
    rotxn: &RoTxn,
    dbs: &ActiveSidechainDbs,
    accepted_bmm_requests: Option<&BmmCommitments>,
    prev_mainchain_block_hash: &BlockHash,
    transaction: &Transaction,
) -> Result<Option<(TransactionEvent, diff::Tx)>, error::HandleTransaction> {
    let mut res = None;
    match handle_m5_m6(rotxn, dbs, Cow::Borrowed(transaction))? {
        Some((Either::Left(deposit), diff)) => {
            res = Some((TransactionEvent::Deposit(deposit), diff));
        }
        Some((Either::Right((sidechain_id, m6id, sequence_number)), diff)) => {
            let withdrawal_bundle_event = WithdrawalBundleEvent {
                m6id,
                sidechain_id,
                kind: WithdrawalBundleEventKind::Succeeded {
                    sequence_number,
                    transaction: transaction.clone(),
                },
            };
            res = Some((
                TransactionEvent::WithdrawalBundle(withdrawal_bundle_event),
                diff,
            ));
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
/// Although a rwtxn is required, it is only used to create a child rwtxn,
/// which will always be aborted.
pub fn validate_tx(
    dbs: &Dbs,
    parent_rwtxn: &mut RwTxn,
    transaction: &Transaction,
) -> Result<bool, error::ValidateTransaction> {
    let mut child_rwtxn = dbs.nested_write_txn(parent_rwtxn)?;
    let tip_hash = dbs
        .current_chain_tip
        .try_get(&child_rwtxn, &())?
        .ok_or(error::ValidateTransactionInner::NoChainTip)?;
    let tip_height = dbs.block_hashes.height().get(&child_rwtxn, &tip_hash)?;
    match handle_transaction(
        &child_rwtxn,
        &dbs.active_sidechains,
        None,
        &tip_hash,
        transaction,
    ) {
        Ok(None) => Ok(true),
        Ok(Some((_, diff))) => {
            let () = diff.apply(&mut child_rwtxn, &dbs.active_sidechains, tip_height)?;
            Ok(true)
        }
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
    match dbs.current_chain_tip.try_get(rwtxn, &())? {
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
            });
        }
    }

    tracing::trace!("starting block processing");
    let height = dbs.block_hashes.height().get(rwtxn, &block.block_hash())?;
    let coinbase = &block.txdata[0];
    let mut coinbase_messages = CoinbaseMessages::default();
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
        coinbase_messages.push(message, vout)?;
    }
    let mut accepted_bmm_requests = BmmCommitments::new();
    let mut events = Vec::<BlockEvent>::new();
    let mut coinbase_msg_diffs = diff::DiffBuilder::new(rwtxn, dbs, height);
    let m4_exists = coinbase_messages.m4_exists();
    for (message, _vout) in coinbase_messages {
        let (event, diff) = coinbase_msg_diffs.rotxn(|rotxn, dbs| {
            handle_coinbase_message(rotxn, dbs, height, &mut accepted_bmm_requests, message)
        })?;
        match event {
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
        if let Some(diff) = diff {
            let () = coinbase_msg_diffs.apply(diff)?;
        }
    }
    if !m4_exists {
        let diff = coinbase_msg_diffs.rotxn(|rotxn, dbs| {
            let active_sidechains_count = dbs.active_sidechains.sidechain().len(rotxn)?;
            let upvotes = vec![M4AckBundles::ABSTAIN_ONE_BYTE; active_sidechains_count as usize];
            let m4 = M4AckBundles::OneByte { upvotes };
            let diff = handle_m4_ack_bundles(rotxn, dbs, &m4)?;
            Ok::<_, error::ConnectBlock>(diff)
        })?;
        if !diff.0.is_empty() {
            let () = coinbase_msg_diffs.apply(diff::CoinbaseMsg::AckBundles(diff))?;
        }
    }
    // Coinbase msg diffs are already applied
    let coinbase_msg_diffs = coinbase_msg_diffs.diffs();
    let failed_sidechain_proposals_diff = handle_failed_sidechain_proposals(rwtxn, dbs, height)?;
    let () = failed_sidechain_proposals_diff.apply(rwtxn, &dbs.proposal_id_to_sidechain, height)?;
    let failed_m6ids_diff = handle_failed_m6ids(rwtxn, dbs, height)?;
    let () = failed_m6ids_diff.apply(rwtxn, &dbs.active_sidechains, height)?;
    events.extend(
        failed_m6ids_diff
            .0
            .iter()
            .flat_map(|(sidechain_id, failed_m6ids)| {
                failed_m6ids.values().map(|(m6id, _)| {
                    WithdrawalBundleEvent {
                        m6id: *m6id,
                        sidechain_id: *sidechain_id,
                        kind: WithdrawalBundleEventKind::Failed,
                    }
                    .into()
                })
            }),
    );
    let coinbase_diff = diff::Coinbase {
        msgs: coinbase_msg_diffs,
        failed_proposals: failed_sidechain_proposals_diff,
        failed_m6ids: failed_m6ids_diff,
    };
    tracing::trace!("Handled coinbase tx, handling other txs...");
    let block_hash = block.header.block_hash();
    let prev_mainchain_block_hash = block.header.prev_blockhash;
    let mut tx_diffs = diff::DiffBuilder::new(rwtxn, &dbs.active_sidechains, height);
    'connect_txs: for transaction in &block.txdata[1..] {
        let Some((tx_event, diff)) = tx_diffs.rotxn(|rotxn, dbs| {
            handle_transaction(
                rotxn,
                dbs,
                Some(&accepted_bmm_requests),
                &prev_mainchain_block_hash,
                transaction,
            )
        })?
        else {
            continue 'connect_txs;
        };
        let () = tx_diffs.apply(diff)?;
        match tx_event {
            TransactionEvent::Deposit(deposit) => {
                events.push(deposit.into());
            }
            TransactionEvent::WithdrawalBundle(withdrawal_bundle_event) => {
                events.push(withdrawal_bundle_event.into());
            }
        }
    }
    // Tx diffs are already applied
    let tx_diffs = tx_diffs.diffs();
    tracing::trace!("Handled block txs");
    let block_info = BlockInfo {
        bmm_commitments: accepted_bmm_requests.into_iter().collect(),
        coinbase_txid: coinbase.compute_txid(),
        events,
    };
    let block_diff = diff::Block {
        coinbase: coinbase_diff,
        txs: tx_diffs,
    };
    tracing::trace!("Storing block info");
    let () = dbs
        .block_hashes
        .put_block_info(rwtxn, &block_hash, &block_info, &block_diff)
        .map_err(error::ConnectBlock::PutBlockInfo)?;
    tracing::trace!("Stored block info");
    let current_tip_cumulative_work: Option<Work> = 'work: {
        let Some(current_tip) = dbs.current_chain_tip.try_get(rwtxn, &())? else {
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
        dbs.current_chain_tip.put(rwtxn, &(), &block_hash)?;
        tracing::trace!("updated current chain tip: {}", height);
    }
    let event = {
        let header_info = HeaderInfo {
            block_hash,
            prev_block_hash: prev_mainchain_block_hash,
            height,
            work: block.header.work(),
            timestamp: block.header.time,
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
    rwtxn: &mut RwTxn,
    dbs: &Dbs,
    event_tx: &Sender<Event>,
    block_hash: BlockHash,
) -> Result<(), error::DisconnectBlock> {
    if let Some(tip_hash) = dbs.current_chain_tip.try_get(rwtxn, &())?
        && tip_hash != block_hash
    {
        return Err(error::DisconnectBlock::TipHash {
            block_hash,
            tip_hash,
        });
    }
    let header_info = dbs.block_hashes.get_header_info(rwtxn, &block_hash)?;
    let diff = dbs.block_hashes.diff().get(rwtxn, &block_hash)?;
    let () = diff.undo(rwtxn, dbs)?;
    if header_info.prev_block_hash != BlockHash::all_zeros() {
        dbs.current_chain_tip
            .put(rwtxn, &(), &header_info.prev_block_hash)?;
    } else {
        dbs.current_chain_tip.delete(rwtxn, &())?;
    }
    let event = Event::DisconnectBlock { block_hash };
    let _send_err: Result<Option<_>, TrySendError<_>> = event_tx.try_broadcast(event);
    Ok(())
}

// Find the best ancestor of the node's tip that the enforcer has
// Returns hash and height of the best ancestor.
async fn fetch_best_ancestor<MainRpcClient>(
    dbs: &Dbs,
    mainchain: &MainRpcClient,
    node_tip: BlockHash,
    node_tip_height: u32,
) -> Result<Option<(BlockHash, u32)>, error::Sync>
where
    MainRpcClient: bitcoin_jsonrpsee::client::MainClient + Sync,
{
    if node_tip == BlockHash::all_zeros() {
        return Ok(None);
    }

    // Check if enforcer already has the node tip
    {
        let rotxn = dbs.read_txn()?;
        if dbs.block_hashes.contains_header(&rotxn, &node_tip)? {
            return Ok(Some((node_tip, node_tip_height)));
        }
    }

    // Find best ancestor via binary search
    let mut best_known_ancestor: Option<(BlockHash, u32)> = None;
    let mut oldest_known_missing_ancestor_height = node_tip_height;

    loop {
        let best_known_ancestor_height = best_known_ancestor.map(|(_, height)| height);
        let midpoint_height =
            oldest_known_missing_ancestor_height.midpoint(best_known_ancestor_height.unwrap_or(0));
        if Some(midpoint_height) == best_known_ancestor_height
            || midpoint_height == oldest_known_missing_ancestor_height
        {
            return Ok(best_known_ancestor);
        }

        let midpoint = mainchain
            .getblockhash(midpoint_height as usize)
            .await
            .map_err(|err| error::Sync::JsonRpc {
                method: "getblockhash".to_owned(),
                source: err,
            })?;
        let rotxn = dbs.read_txn()?;
        if dbs.block_hashes.contains_header(&rotxn, &midpoint)? {
            best_known_ancestor = Some((midpoint, midpoint_height));
        } else {
            oldest_known_missing_ancestor_height = midpoint_height;
        }
    }
}

#[tracing::instrument(skip_all)]
async fn sync_headers<MainRpcClient, Signal>(
    dbs: &Dbs,
    main_rest_client: &MainRestClient,
    main_rpc_client: &MainRpcClient,
    main_tip: BlockHash,
    progress_tx: &tokio::sync::watch::Sender<HeaderSyncProgress>,
    shutdown_signal: Signal,
) -> Result<(), error::Sync>
where
    MainRpcClient: bitcoin_jsonrpsee::client::MainClient + Sync,
    Signal: Future<Output = ()> + Send,
{
    let start = Instant::now();

    if main_tip == BlockHash::all_zeros() {
        return Ok(());
    }

    let main_tip_height = main_rpc_client
        .getblockheader(main_tip)
        .await
        .map_err(|err| error::Sync::JsonRpc {
            method: "getblockheader".to_owned(),
            source: err,
        })?
        .height;

    let best_ancestor: Option<(BlockHash, u32)> =
        fetch_best_ancestor(dbs, main_rpc_client, main_tip, main_tip_height).await?;

    // Return early if all headers are already available.
    let (mut current_block_hash, mut current_height, new_headers_needed) = match best_ancestor {
        Some((ancestor, height)) => {
            if ancestor == main_tip {
                return Ok(());
            } else {
                (ancestor, height, main_tip_height - height)
            }
        }
        None => {
            let genesis_block_hash =
                main_rpc_client
                    .getblockhash(0)
                    .await
                    .map_err(|err| error::Sync::JsonRpc {
                        method: "getblockhash".to_owned(),
                        source: err,
                    })?;
            (genesis_block_hash, 0, main_tip_height + 1)
        }
    };

    // Fetch headers in batches for efficiency
    // 2000 is max allowed by Bitcoin Core
    const HEADER_FETCH_BATCH_SIZE: usize = 2000;

    tracing::debug!(
        "Syncing headers starting from #{current_height} `{current_block_hash}` up to `{main_tip}`"
    );

    // The requested block header is the /first/ header in the batch. This means we
    // need to loop from our current tip, until we find the requested main tip.
    let shutdown_signal = shutdown_signal.shared();
    loop {
        tokio::select! {
            biased;

            _ = shutdown_signal.clone() => {
                tracing::warn!("Header sync interrupted");
                return Err(error::Sync::Shutdown);
            }
            _ = futures::future::ready(()) => {}
        }

        tracing::debug!(
            "Fetching batch of headers starting from #{current_height} `{current_block_hash}`"
        );

        // It is possible that the first header is not new
        let headers_needed = (main_tip_height - current_height) + 1;
        let batch_size = std::cmp::min(HEADER_FETCH_BATCH_SIZE, headers_needed as usize);
        // Fetch a batch of headers
        let headers = main_rest_client
            .get_block_headers(&current_block_hash, batch_size)
            .await?;

        match headers.last() {
            Some((_, last_block_hash, last_block_height)) => {
                // Update the block_hash to the latest header fetched in this
                // batch to continue the loop
                current_block_hash = *last_block_hash;
                current_height = *last_block_height;
            }
            None => {
                // This will be empty if the requested block hash is not in the
                // current active chain, i.e. if we're dealing with a reorg,
                // or the provided mainchain tip was invalid.

                // Syncing headers is a reasonably quick operation.
                // We therefore deal with it by erroring out here, and then
                // retrying the sync from further out in the call stack.
                //
                // This branch only hits if we're dealing with a reorg /while/
                // we're syncing headers.
                // If we've reorged before starting the sync, it is picked up
                // at the beginning. It is such an edge case that we don't
                // bother implementing it (for now, at least)
                return Err(error::Sync::BlockNotInActiveChain {
                    block_hash: current_block_hash,
                });
            }
        }

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
        } else if current_height == main_tip_height {
            return Err(error::Sync::BlockNotInActiveChain {
                block_hash: main_tip,
            });
        }
    }

    tracing::info!(
        main_tip = ?main_tip,
        "Synced {new_headers_needed} headers in {}",
        jiff::SignedDuration::try_from(start.elapsed()).unwrap()
    );
    Ok(())
}

async fn fetch_blocks_batch<MainRpcClient>(
    main_rpc_client: &MainRpcClient,
    block_hashes: &[BlockHash],
) -> Result<Vec<Block>, error::Sync>
where
    MainRpcClient: bitcoin_jsonrpsee::client::MainClient + Sync,
{
    let mut get_block_batch = BatchRequestBuilder::new();
    for block_hash in block_hashes {
        // By default (1) a deserialized block is returned. We want to do this ourselves!
        let verbosity = 0;

        let mut params = ArrayParams::new();
        params
            .insert(block_hash.to_string())
            .expect("failed to insert block hash");

        params
            .insert(verbosity)
            .expect("failed to insert verbosity");

        get_block_batch
            .insert("getblock", params)
            .map_err(error::Sync::JsonSerialize)?;
    }

    let start = Instant::now();

    let batch_response: BatchResponse<String> = main_rpc_client
        .batch_request(get_block_batch)
        .boxed() // IMPORTANT: omitting this box leads to cryptic a lifetime error
        .await
        .map_err(|err| error::Sync::JsonRpc {
            method: "getblock (batched)".to_owned(),
            source: err,
        })?;

    tracing::debug!(
        "Fetched batch of {} block(s) in {:?}",
        batch_response.len(),
        start.elapsed(),
    );

    let blocks = match batch_response.ok() {
        Ok(blocks) => blocks
            .map(|block| {
                let bytes = hex::decode(block)?;
                let block: bitcoin::Block = bitcoin::consensus::deserialize(&bytes)?;

                Ok::<_, error::Sync>(block)
            })
            .collect::<Result<Vec<_>, _>>()?,
        Err(errors) => {
            return Err(error::Sync::BatchJsonRpc {
                errors: errors.map(|e| e.message().to_string()).collect(),
            });
        }
    };

    Ok(blocks)
}

// MUST be called after `sync_headers`.
#[tracing::instrument(skip_all)]
async fn sync_blocks<MainRpcClient, Signal>(
    dbs: &Dbs,
    event_tx: &Sender<Event>,
    main_rpc_client: &MainRpcClient,
    main_tip: BlockHash,
    shutdown_signal: Signal,
) -> Result<(), error::Sync>
where
    MainRpcClient: bitcoin_jsonrpsee::client::MainClient + Sync,
    Signal: Future<Output = ()> + Send,
{
    // Batch size for concurrent block fetching
    // It's hard to know what a good size here is, without
    // further benchmarking.
    const BLOCK_FETCH_BATCH_SIZE: usize = 50;

    let start = Instant::now();
    let missing_blocks = tokio::task::block_in_place(|| {
        let current_enforcer_tip = {
            let mut rwtxn = dbs.write_txn()?;
            let mut current_enforcer_tip = dbs
                .current_chain_tip
                .try_get(&rwtxn, &())?
                .unwrap_or_else(BlockHash::all_zeros);
            let last_common_ancestor =
                dbs.block_hashes
                    .last_common_ancestor(&rwtxn, current_enforcer_tip, main_tip)?;
            if current_enforcer_tip != last_common_ancestor {
                tracing::info!(
                    "Disconnecting tip {current_enforcer_tip} -> {last_common_ancestor}"
                );
                while current_enforcer_tip != last_common_ancestor {
                    let () = disconnect_block(&mut rwtxn, dbs, event_tx, current_enforcer_tip)?;
                    current_enforcer_tip = dbs
                        .current_chain_tip
                        .try_get(&rwtxn, &())?
                        .unwrap_or_else(BlockHash::all_zeros);
                }
                rwtxn.commit()?;
            } else {
                rwtxn.abort();
            }
            current_enforcer_tip
        };
        let rotxn = dbs.read_txn()?;
        let missing_blocks = dbs
            .block_hashes
            .ancestor_headers(&rotxn, main_tip)
            .map(|(block_hash, _)| Ok(block_hash))
            .take_while(|block_hash| Ok(*block_hash != current_enforcer_tip))
            .collect::<Vec<_>>()
            .map_err(error::Sync::from)?;
        Ok::<_, error::Sync>(missing_blocks)
    })?;

    if missing_blocks.is_empty() {
        tracing::info!("No missing blocks, skipping sync");
        return Ok(());
    }

    tracing::info!(
        "identified {} missing blocks in {:?}, starting batched sync",
        missing_blocks.len(),
        start.elapsed()
    );

    let shutdown_signal = shutdown_signal.shared();
    let mut total_blocks_fetched = 0;

    // Process blocks in batches for better network efficiency
    let missing_blocks_rev: Vec<_> = missing_blocks.into_iter().rev().collect();
    for chunk in missing_blocks_rev.chunks(BLOCK_FETCH_BATCH_SIZE) {
        tokio::select! {
            biased;

            _ = shutdown_signal.clone() => {
                tracing::warn!("Block sync interrupted");
                return Err(error::Sync::Shutdown);
            }
            _ = futures::future::ready(()) => {}
        }

        let blocks = fetch_blocks_batch(main_rpc_client, chunk).await?;
        total_blocks_fetched += blocks.len();

        // Do a single DB transaction for the entire batch. DB commits are a big part
        // of the sync time, so we want to reduce the number of times we do this.
        let mut rwtxn = dbs.write_txn()?;

        // Process blocks sequentially to maintain ordering and database consistency
        for block in blocks {
            let block_hash = block.block_hash();
            let height = dbs.block_hashes.height().get(&rwtxn, &block_hash)?;

            tracing::debug!("Syncing block #{height} `{block_hash}` -> `{main_tip}`",);

            // We should not call out to `invalidateblock` in case of failures here,
            // as that is handled by the cusf-enforcer-mempool crate.
            // FIXME: handle disconnects
            let event = connect_block(&mut rwtxn, dbs, &block)?;
            tracing::trace!("connected block at height {height}: {block_hash}");
            // Events should only ever be sent after committing DB txs, see
            // https://github.com/LayerTwo-Labs/bip300301_enforcer/pull/185
            let _send_err: Result<Option<_>, TrySendError<_>> = event_tx.try_broadcast(event);
        }

        let () = rwtxn.commit()?;
    }

    tracing::info!(
        "Synced {total_blocks_fetched} blocks in {:?}",
        start.elapsed()
    );
    Ok(())
}

pub(in crate::validator) async fn sync_to_tip<MainClient, Signal>(
    dbs: &Dbs,
    event_tx: &Sender<Event>,
    header_sync_progress_tx: &tokio::sync::watch::Sender<HeaderSyncProgress>,
    main_rpc_client: &MainClient,
    main_rest_client: &MainRestClient,
    main_tip: BlockHash,
    shutdown_signal: Signal,
) -> Result<(), error::Sync>
where
    MainClient: bitcoin_jsonrpsee::client::MainClient + Sync,
    Signal: Future<Output = ()> + Send,
{
    use futures::FutureExt as _;
    let shutdown_signal = shutdown_signal.shared();

    let () = sync_headers(
        dbs,
        main_rest_client,
        main_rpc_client,
        main_tip,
        header_sync_progress_tx,
        shutdown_signal.clone(),
    )
    .await?;
    let () = sync_blocks(
        dbs,
        event_tx,
        main_rpc_client,
        main_tip,
        shutdown_signal.clone(),
    )
    .await?;
    Ok(())
}
