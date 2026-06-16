use std::{
    borrow::Cow,
    cmp::Ordering,
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
    time::Instant,
};

use async_broadcast::{Sender, TrySendError};
use bitcoin::{
    Amount, Block, BlockHash, Network, OutPoint, Transaction, Work,
    hashes::{Hash as _, sha256d},
};
use error_fatality::{Fatality as _, Split as _};
use fallible_iterator::{FallibleIterator, IteratorExt};
use futures::FutureExt as _;
use jsonrpsee::core::{
    client::BatchResponse,
    params::{ArrayParams, BatchRequestBuilder},
};
use sneed::{RoTxn, RwTxn, db};
use tokio_util::sync::CancellationToken;

use crate::{
    messages::{
        CoinbaseMessage, CoinbaseMessages, M1ProposeSidechain, M2AckSidechain, M3ProposeBundle,
        M4AckBundles, M7BmmAccept, compute_m6id, parse_m8_tx, parse_op_drivechain,
    },
    proto::mainchain::HeaderSyncProgress,
    types::{
        BlockEvent, BlockInfo, BmmCommitments, Ctip, Deposit, Event, HeaderInfo, M6id,
        PendingM6idInfo, Sidechain, SidechainNumber, SidechainProposal, SidechainProposalId,
        SidechainProposalStatus, Thresholds, WithdrawalBundleEvent, WithdrawalBundleEventKind,
    },
    validator::{
        dbs::{
            Dbs, PendingM6ids,
            diff::{self, Diff},
        },
        main_rest_client::MainRestClient,
    },
};

mod block_files;
pub mod error;

/// Bundles the consensus inputs that every BIP 300/301 handler needs: a
/// reference to the validator's databases, the Bitcoin network we're running
/// against, and the matching voting/aging [`Thresholds`].
#[derive(Clone, Copy)]
pub(in crate::validator) struct BlockHandler<'a> {
    pub(super) dbs: &'a Dbs,
    pub(super) network: Network,
    pub(super) thresholds: Thresholds,
}

impl<'a> BlockHandler<'a> {
    pub(in crate::validator) fn new(dbs: &'a Dbs, network: Network) -> Self {
        Self {
            dbs,
            network,
            thresholds: Thresholds::for_network(network),
        }
    }
}

/// Vote margin for the M4 `LeadingBy50` encoding: a bundle must be
/// leading its rivals by at least this many votes to be upvoted.
const LEADING_BY_50_MARGIN: u16 = 50;

#[derive(Debug)]
enum DepositsOrSuccessfulWithdrawal {
    M5Deposits {
        deposits: HashMap<SidechainNumber, Deposit>,
        diff: diff::M5,
    },
    M6Withdrawal {
        sidechain_id: SidechainNumber,
        m6id: M6id,
        sequence_number: u64,
        diff: diff::M6,
    },
}

#[derive(Debug)]
enum CoinbaseMessageEvent {
    NewSidechainProposal { sidechain: Sidechain },
    WithdrawalBundle(WithdrawalBundleEvent),
}

#[derive(Debug)]
enum TransactionEvent {
    Deposits(HashMap<SidechainNumber, Deposit>),
    WithdrawalBundle(WithdrawalBundleEvent),
}

fn downvoted_others_for_upvote(pending: &PendingM6ids, upvote_target: M6id) -> Vec<M6id> {
    pending
        .iter()
        .filter_map(|(m6id, info)| (*m6id != upvote_target && info.vote_count > 0).then_some(*m6id))
        .collect()
}

fn positive_votes_proposals_for_alarm(pending: &PendingM6ids) -> HashSet<M6id> {
    pending
        .iter()
        .filter_map(|(m6id, info)| (info.vote_count > 0).then_some(*m6id))
        .collect()
}

/// BIP 301 M8 (BMM Request). Validates that an M8 transaction matches an
/// accepted BMM commitment (via M7) and references the current mainchain
/// tip as its `prev_mainchain_block_hash`.
///
/// Returns `true` if the tx is a valid BMM request, a non-fatal
/// `HandleM8` error if invalid, and `false` if the tx is not an M8 at all.
///
/// <https://github.com/LayerTwo-Labs/bip300_bip301_specifications/blob/master/bip301.md#m8-bmm-request>
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

impl BlockHandler<'_> {
    /// BIP 300 M1 (Propose New Sidechain). Returns `Some` if the proposal is
    /// novel; re-proposals of an existing `(slot, description_hash)` are ignored
    /// so miners cannot reset vote counts.
    ///
    /// <https://github.com/LayerTwo-Labs/bip300_bip301_specifications/blob/master/bip300.md#m1-propose-new-sidechain>
    fn handle_m1_propose_sidechain(
        &self,
        rotxn: &RoTxn,
        proposal: SidechainProposal,
        proposal_height: u32,
    ) -> Result<Option<diff::NewSidechainProposal>, error::HandleM1ProposeSidechain> {
        let dbs = self.dbs;
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

    /// BIP 300 M2 (ACK Proposal). Increments the proposal's vote count and
    /// activates it if it has crossed the threshold within the age window.
    ///
    /// An M2 whose `description_hash` doesn't match any prior M1's
    /// `sha256d(D)` MUST be ignored, in that case returns `Ok(None)`
    ///
    /// <https://github.com/LayerTwo-Labs/bip300_bip301_specifications/blob/master/bip300.md#m2-ack-proposal>
    fn handle_m2_ack_sidechain(
        &self,
        rotxn: &RoTxn,
        height: u32,
        sidechain_number: SidechainNumber,
        description_hash: sha256d::Hash,
    ) -> Result<Option<diff::AckSidechain>, error::HandleM2AckSidechain> {
        let dbs = self.dbs;
        let thresholds = self.thresholds;
        let proposal_id = SidechainProposalId {
            sidechain_number,
            description_hash,
        };
        let sidechain = dbs.proposal_id_to_sidechain.try_get(rotxn, &proposal_id)?;
        let Some(mut sidechain) = sidechain else {
            tracing::debug!(
                %sidechain_number,
                %description_hash,
                "ignoring M2 ack: no matching M1 proposal for this description hash"
            );
            return Ok(None);
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
                && sidechain.status.vote_count > thresholds.used_sidechain_slot_activation_threshold
                && sidechain_proposal_age <= thresholds.used_sidechain_slot_proposal_max_age as u32
        } || {
            !sidechain_slot_is_used
                && sidechain.status.vote_count
                    > thresholds.unused_sidechain_slot_activation_threshold
                && sidechain_proposal_age
                    <= thresholds.unused_sidechain_slot_proposal_max_age as u32
        };

        if new_sidechain_activated {
            diff.activated = true;
        }
        Ok(Some(diff))
    }

    /// BIP 300 M2 failure path: removes sidechain proposals whose age has
    /// exceeded the max proposal age.
    ///
    /// <https://github.com/LayerTwo-Labs/bip300_bip301_specifications/blob/master/bip300.md#m2-ack-proposal>
    fn handle_failed_sidechain_proposals(
        &self,
        rotxn: &RoTxn,
        height: u32,
    ) -> Result<diff::FailedProposals, error::HandleFailedSidechainProposals> {
        let dbs = self.dbs;
        let thresholds = self.thresholds;
        let failed_proposals = dbs
            .proposal_id_to_sidechain
            .iter(rotxn)
            .map_err(db::Error::from)?
            .map_err(db::Error::from)
            .filter(|(_proposal_id, sidechain)| {
                // doing `height - sidechain.status.proposal_height` can panic if the enforcer has data
                // from a previous sync that is not in the active chain.
                let age = height.saturating_sub(sidechain.status.proposal_height);
                let sidechain_slot_is_used = dbs
                    .active_sidechains
                    .sidechain()
                    .contains_key(rotxn, &sidechain.proposal.sidechain_number)?;
                let (max_age, threshold) = if sidechain_slot_is_used {
                    (
                        thresholds.used_sidechain_slot_proposal_max_age as u32,
                        thresholds.used_sidechain_slot_activation_threshold as u32,
                    )
                } else {
                    (
                        thresholds.unused_sidechain_slot_proposal_max_age as u32,
                        thresholds.unused_sidechain_slot_activation_threshold as u32,
                    )
                };
                // BIP 300: a proposal fails once its age exceeds the max age, or
                // once it has accumulated enough non-ack blocks that it can no
                // longer reach the activation threshold within the remaining
                // window (max_fails = max_age - threshold).
                let max_fails = max_age.saturating_sub(threshold);
                let fails = age.saturating_sub(sidechain.status.vote_count as u32);
                let failed = age > max_age || (age > max_fails && fails >= max_fails);
                Ok(failed)
            })
            .collect()?;
        Ok(diff::FailedProposals(failed_proposals))
    }

    /// BIP 300 M3 (Propose Bundle). Adds a new pending withdrawal bundle for an
    /// active sidechain; rejects proposals for inactive slots.
    ///
    /// <https://github.com/LayerTwo-Labs/bip300_bip301_specifications/blob/master/bip300.md#m3-propose-bundle>
    fn handle_m3_propose_bundle(
        &self,
        rotxn: &RoTxn,
        sidechain_number: SidechainNumber,
        m6id: M6id,
    ) -> Result<diff::ProposeBundle, error::HandleM3ProposeBundle> {
        let dbs = self.dbs;
        if !dbs
            .active_sidechains
            .sidechain()
            .contains_key(rotxn, &sidechain_number)?
        {
            return Err(error::HandleM3ProposeBundle::InactiveSidechain { sidechain_number });
        }
        // BIP 300: re-proposing an M6ID that is already pending (proposed in a
        // previous block and not yet paid out) would reset its ack count and
        // age. Reject such a block instead.
        let pending = dbs
            .active_sidechains
            .pending_m6ids()
            .get(rotxn, &sidechain_number)?;
        if pending.contains_key(&m6id) {
            return Err(error::HandleM3ProposeBundle::BundleAlreadyPending {
                sidechain_number,
                m6id,
            });
        }
        Ok(diff::ProposeBundle {
            m6id,
            sidechain_number,
        })
    }

    /// Core M4 upvote resolver: for each active sidechain, interprets the vote
    /// value (index into pending bundles, ABSTAIN `0xFFFF`, or ALARM `0xFFFE`).
    ///
    /// <https://github.com/LayerTwo-Labs/bip300_bip301_specifications/blob/master/bip300.md#m4-ack-bundle>
    fn handle_m4_votes(
        &self,
        rotxn: &RoTxn,
        upvotes: &[u16],
    ) -> Result<diff::AckBundles, error::HandleM4Votes> {
        let dbs = self.dbs;
        let active_sidechains = dbs.active_sidechains.numbers(rotxn)?;
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
                let positive_votes_proposals =
                    positive_votes_proposals_for_alarm(&pending_withdrawals);
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
                    let upvote_target = *m6id;
                    diff.0.insert(
                        sidechain_number,
                        diff::AckBundleAction::Upvote {
                            m6id: upvote_target,
                            downvoted_others: downvoted_others_for_upvote(
                                &pending_withdrawals,
                                upvote_target,
                            ),
                        },
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

    /// BIP 300 M4 version 0x03 (LeadingBy50): for each active sidechain, upvote
    /// the single bundle whose `vote_count` exceeds the next-highest rival's by
    /// at least `LEADING_BY_50_MARGIN`. A sidechain's sole bundle counts as
    /// leading an implicit zero-vote rival.
    ///
    /// <https://github.com/LayerTwo-Labs/bip300_bip301_specifications/blob/master/bip300.md#m4-ack-bundle>
    fn handle_m4_leading_by_50(
        &self,
        rotxn: &RoTxn,
    ) -> Result<diff::AckBundles, error::HandleM4Votes> {
        let dbs = self.dbs;
        let active_sidechains = dbs.active_sidechains.numbers(rotxn)?;

        let mut diff = diff::AckBundles::default();
        for sidechain_number in active_sidechains {
            let pending = dbs
                .active_sidechains
                .pending_m6ids()
                .get(rotxn, &sidechain_number)?;

            let mut by_votes: Vec<(M6id, u16)> =
                pending.iter().map(|(m, i)| (*m, i.vote_count)).collect();
            by_votes.sort_unstable_by_key(|(_, vote_count)| std::cmp::Reverse(*vote_count));

            let Some(&(leader_m6id, lead_votes)) = by_votes.first() else {
                continue;
            };
            let second_highest = by_votes.get(1).map_or(0, |(_, v)| *v);

            if lead_votes.saturating_sub(second_highest) >= LEADING_BY_50_MARGIN
                && lead_votes < u16::MAX
            {
                diff.0.insert(
                    sidechain_number,
                    diff::AckBundleAction::Upvote {
                        m6id: leader_m6id,
                        downvoted_others: downvoted_others_for_upvote(&pending, leader_m6id),
                    },
                );
            }
        }
        Ok(diff)
    }

    /// BIP 300 M4 version 0x00 (RepeatPrevious): "sets this block's M4 equal to
    /// the previous block's M4". Re-derived against the *current* pending state
    /// (not cloned verbatim); see
    /// [`diff::AckBundleAction::Upvote::downvoted_others`] and
    /// [`diff::AckBundleAction::Alarm::positive_votes_proposals`] for why the
    /// auxiliary sets must be recomputed.
    ///
    /// <https://github.com/LayerTwo-Labs/bip300_bip301_specifications/blob/master/bip300.md#m4-ack-bundle>
    fn handle_m4_repeat_previous(
        &self,
        rotxn: &RoTxn,
        prev_block_hash: BlockHash,
    ) -> Result<diff::AckBundles, error::HandleM4AckBundles> {
        let dbs = self.dbs;
        // First block post-genesis: `connect_block` accepts `parent == all_zeros`,
        // and there is no stored diff to repeat. Treat as "no prior M4" — empty diff.
        if prev_block_hash == BlockHash::all_zeros() {
            return Ok(diff::AckBundles::default());
        }

        let prev_diff = dbs.block_hashes.diff().get(rotxn, &prev_block_hash)?;

        // At most one AckBundles entry can exist per block (enforced by
        // `CoinbaseMessages::push` rejecting duplicate M4s).
        let Some(prev_ack_bundles) = prev_diff.coinbase.msgs.iter().find_map(|msg| match msg {
            diff::CoinbaseMsg::AckBundles(ab) => Some(ab),
            _ => None,
        }) else {
            // Previous block had no effective M4 (either absent, or abstain-only).
            // Repeating that means no actions for this block.
            return Ok(diff::AckBundles::default());
        };

        let mut rebuilt = diff::AckBundles::default();
        for (sidechain_number, action) in &prev_ack_bundles.0 {
            let pending = dbs
                .active_sidechains
                .pending_m6ids()
                .get(rotxn, sidechain_number)?;
            match action {
                diff::AckBundleAction::Upvote { m6id, .. } => {
                    let target = *m6id;
                    let Some(info) = pending.get(&target) else {
                        return Err(
                            error::HandleM4AckBundles::RepeatPreviousUpvotesMissingBundle {
                                sidechain_number: *sidechain_number,
                                m6id: target,
                            },
                        );
                    };
                    // Mirror `handle_m4_votes`: a saturated target silently
                    // yields no upvote rather than overflowing on apply.
                    if info.vote_count == u16::MAX {
                        continue;
                    }
                    rebuilt.0.insert(
                        *sidechain_number,
                        diff::AckBundleAction::Upvote {
                            m6id: target,
                            downvoted_others: downvoted_others_for_upvote(&pending, target),
                        },
                    );
                }
                diff::AckBundleAction::Alarm { .. } => {
                    // Alarm applies to all pending in the sidechain; per-m6id
                    // existence doesn't matter. Recompute the positive-vote set
                    // against current state.
                    let positive_votes_proposals = positive_votes_proposals_for_alarm(&pending);
                    if !positive_votes_proposals.is_empty() {
                        rebuilt.0.insert(
                            *sidechain_number,
                            diff::AckBundleAction::Alarm {
                                positive_votes_proposals,
                            },
                        );
                    }
                }
            }
        }
        Ok(rebuilt)
    }

    /// BIP 300 M4 (ACK Bundle) dispatcher across the four encoding versions:
    /// OneByte (0x01), TwoBytes (0x02), LeadingBy50 (0x03), RepeatPrevious
    /// (0x00).
    ///
    /// <https://github.com/LayerTwo-Labs/bip300_bip301_specifications/blob/master/bip300.md#m4-ack-bundle>
    fn handle_m4_ack_bundles(
        &self,
        rotxn: &RoTxn,
        prev_block_hash: BlockHash,
        m4: &M4AckBundles,
    ) -> Result<diff::AckBundles, error::HandleM4AckBundles> {
        match m4 {
            M4AckBundles::LeadingBy50 => self
                .handle_m4_leading_by_50(rotxn)
                .map_err(error::HandleM4AckBundles::from),
            M4AckBundles::RepeatPrevious => self.handle_m4_repeat_previous(rotxn, prev_block_hash),
            M4AckBundles::OneByte { upvotes } => {
                let upvotes: Vec<u16> = upvotes
                    .iter()
                    .map(|vote| match *vote {
                        M4AckBundles::ABSTAIN_ONE_BYTE => M4AckBundles::ABSTAIN_TWO_BYTES,
                        M4AckBundles::ALARM_ONE_BYTE => M4AckBundles::ALARM_TWO_BYTES,
                        vote => vote as u16,
                    })
                    .collect();
                self.handle_m4_votes(rotxn, &upvotes)
                    .map_err(error::HandleM4AckBundles::from)
            }
            M4AckBundles::TwoBytes { upvotes } => {
                // BIP 300 M4: TwoBytes is only allowed if at least one element
                // exceeds the one-byte range.
                if upvotes.iter().all(|v| *v <= 253) {
                    return Err(error::HandleM4AckBundles::TwoBytesWithinByteRange);
                }
                self.handle_m4_votes(rotxn, upvotes)
                    .map_err(error::HandleM4AckBundles::from)
            }
        }
    }

    /// BIP 300 M6 failure path: removes pending withdrawal bundles whose age
    /// has exceeded `withdrawal_bundle_max_age`.
    ///
    /// <https://github.com/LayerTwo-Labs/bip300_bip301_specifications/blob/master/bip300.md#m6-withdrawal-bundle>
    fn handle_failed_m6ids(
        &self,
        rotxn: &RoTxn,
        block_height: u32,
    ) -> Result<diff::FailedM6ids, error::HandleFailedM6Ids> {
        let dbs = self.dbs;
        let thresholds = self.thresholds;
        let active_sidechains = dbs.active_sidechains.numbers(rotxn)?;

        let mut failed_m6ids = HashMap::<_, BTreeMap<_, _>>::new();

        for sidechain_number in active_sidechains {
            // Invariant: `put_sidechain` always creates a corresponding
            // `pending_m6ids` entry, so every active sidechain must have one.
            // A missing entry indicates DB corruption and is unrecoverable without resyncing.
            let pending_m6ids = dbs
                .active_sidechains
                .pending_m6ids()
                .get(rotxn, &sidechain_number)?;
            let failed = pending_m6ids
                .into_iter()
                .enumerate()
                .filter(|(_, (_, info))| {
                    let age = block_height.saturating_sub(info.proposal_height);
                    age > thresholds.withdrawal_bundle_max_age as u32
                })
                .collect::<BTreeMap<_, _>>();
            if !failed.is_empty() {
                failed_m6ids.insert(sidechain_number, failed);
            }
        }

        Ok(diff::FailedM6ids(failed_m6ids))
    }

    /// BIP 300 M6 (Withdrawal Bundle): validates that the tx matches an approved
    /// pending bundle with sufficient vote count and returns its metadata.
    ///
    /// <https://github.com/LayerTwo-Labs/bip300_bip301_specifications/blob/master/bip300.md#m6-withdrawal-bundle>
    fn handle_m6(
        &self,
        rotxn: &RoTxn,
        transaction: Transaction,
        old_treasury_value: Amount,
    ) -> Result<(M6id, SidechainNumber, usize, PendingM6idInfo), error::HandleM5M6> {
        let (m6id, sidechain_number) = compute_m6id(transaction, old_treasury_value)?;

        let pending_m6ids = self
            .dbs
            .active_sidechains
            .pending_m6ids()
            .try_get(rotxn, &sidechain_number)?
            .ok_or(error::InvalidM6::MissingPendingWithdrawal {
                m6id,
                sidechain_number,
            })?;
        // Capture the bundle's position in the chronological list so a
        // disconnect (undo) can restore it at the same index.
        let (index, _, info) =
            pending_m6ids
                .get_full(&m6id)
                .ok_or(error::InvalidM6::MissingPendingWithdrawal {
                    m6id,
                    sidechain_number,
                })?;
        if info.vote_count > self.thresholds.withdrawal_bundle_inclusion_threshold {
            Ok((m6id, sidechain_number, index, *info))
        } else {
            let err = error::InvalidM6::InsufficientVoteCount {
                m6id,
                threshold: self.thresholds.withdrawal_bundle_inclusion_threshold,
                vote_count: info.vote_count,
            };
            Err(err.into())
        }
    }

    /// BIP 300 M5 (Deposit) / M6 (Withdrawal Bundle) dispatcher. A tx whose
    /// first output is an OP_DRIVECHAIN treasury UTXO is classified by
    /// comparing the new treasury value to the previous one: a larger value
    /// is an M5 deposit, a smaller value is an M6 withdrawal bundle.
    ///
    /// <https://github.com/LayerTwo-Labs/bip300_bip301_specifications/blob/master/bip300.md#m5-deposit>
    /// <https://github.com/LayerTwo-Labs/bip300_bip301_specifications/blob/master/bip300.md#m6-withdrawal-bundle>
    fn handle_m5_m6(
        &self,
        rotxn: &RoTxn,
        transaction: Cow<'_, Transaction>,
    ) -> Result<Option<DepositsOrSuccessfulWithdrawal>, error::HandleM5M6> {
        let dbs = &self.dbs.active_sidechains;
        let txid = transaction.compute_txid();
        let ctip_spends: HashMap<SidechainNumber, bitcoin::Amount> = transaction
            .input
            .iter()
            .map(Result::<_, error::HandleM5M6>::Ok)
            .transpose_into_fallible()
            .filter_map(|input| {
                dbs.ctip_outpoint_to_value_seq()
                    .try_get(rotxn, &input.previous_output)?
                    .map(|(sidechain_number, amount, _)| Ok((sidechain_number, amount)))
                    .transpose()
            })
            .collect()?;
        let new_ctips = {
            let mut new_ctips = HashMap::<SidechainNumber, Ctip>::new();
            for (vout, output) in transaction.output.iter().enumerate() {
                if let Ok((_input, sidechain_number)) =
                    parse_op_drivechain(output.script_pubkey.as_bytes())
                {
                    // An OP_DRIVECHAIN output only designates a treasury UTXO
                    // for an *active* sidechain slot. On the mainchain
                    // OP_DRIVECHAIN is anyone-can-spend, so for an inactive slot
                    // the output is an ordinary output and the block is valid.
                    // Without this guard we'd treat it as a CTIP with a
                    // nonexistent treasury (old value zero), then either reject
                    // a valid block (`ZeroDiff` for a zero-value output) or
                    // fabricate a deposit into a sidechain that doesn't exist.
                    if !dbs.sidechain().contains_key(rotxn, &sidechain_number)? {
                        continue;
                    }
                    let new_ctip = Ctip {
                        outpoint: OutPoint {
                            txid,
                            vout: vout as u32,
                        },
                        value: output.value,
                    };
                    if new_ctips.insert(sidechain_number, new_ctip).is_some() {
                        return Err(error::HandleM5M6::MultipleOpDrivechainOutputs(
                            sidechain_number,
                        ));
                    }
                }
            }
            new_ctips
        };
        let n_new_ctips = new_ctips.len();
        // BIP 300: a tx that spends a treasury UTXO must also create a new
        // treasury UTXO for that sidechain. Without this check, a spend of the
        // anyone-can-spend OP_DRIVECHAIN treasury that creates no replacement
        // output is silently ignored and drains the slot.
        for sidechain_number in ctip_spends.keys() {
            if !new_ctips.contains_key(sidechain_number) {
                return Err(error::HandleM5M6::TreasurySpentWithoutNewCtip {
                    sidechain_number: *sidechain_number,
                });
            }
        }
        // Check that old ctips are spent
        let mut res = Option::<DepositsOrSuccessfulWithdrawal>::None;
        for (sidechain_number, new_ctip) in new_ctips {
            let old_treasury_value = if dbs.ctip().contains_key(rotxn, &sidechain_number)? {
                *ctip_spends
                    .get(&sidechain_number)
                    .ok_or_else(|| error::HandleM5M6::OldCtipUnspent { sidechain_number })?
            } else if ctip_spends.contains_key(&sidechain_number) {
                return Err(error::HandleM5M6::CtipDbsInconsistent {
                    sidechain: sidechain_number,
                    db_exists_in: dbs.ctip_outpoint_to_value_seq().name().to_owned(),
                    db_missing_in: dbs.ctip().name().to_owned(),
                });
            } else {
                Amount::ZERO
            };
            match new_ctip.value.cmp(&old_treasury_value) {
                // M6
                Ordering::Less => {
                    if transaction.input.len() != 1 {
                        let err = error::InvalidM6::InputCount {
                            n_inputs: transaction.input.len(),
                        };
                        return Err(err.into());
                    }
                    if new_ctip.outpoint.vout != 0 {
                        let err = error::InvalidM6::TreasuryOutputIndex {
                            treasury_vout: new_ctip.outpoint.vout,
                        };
                        return Err(err.into());
                    }
                    let (m6id, sidechain_number_, removed_index, info) = self.handle_m6(
                        rotxn,
                        transaction.clone().into_owned(),
                        old_treasury_value,
                    )?;
                    // `handle_m6` → `compute_m6id` parses the same first-output script
                    // that we already parsed above. Mismatch would mean the parser is
                    // non-deterministic (impossible) or the transaction was mutated
                    // between the two parses — both indicate a serious invariant
                    // violation.
                    assert_eq!(
                        sidechain_number, sidechain_number_,
                        "invariant violation: parse_op_drivechain returned different \
                        sidechain numbers for the same output",
                    );
                    let sequence_number = dbs
                        .treasury_utxo_count
                        .try_get(rotxn, &sidechain_number)?
                        .unwrap_or(0);
                    let diff = diff::M6 {
                        sidechain_number,
                        new_ctip,
                        removed_pending_withdrawal: m6id,
                        removed_pending_withdrawal_index: removed_index,
                        removed_pending_withdrawal_info: info,
                    };
                    match res {
                        None => {
                            res = Some(DepositsOrSuccessfulWithdrawal::M6Withdrawal {
                                sidechain_id: sidechain_number,
                                m6id,
                                sequence_number,
                                diff,
                            });
                        }
                        Some(DepositsOrSuccessfulWithdrawal::M5Deposits { .. }) => {
                            return Err(error::HandleM5M6::Ambiguous);
                        }
                        Some(DepositsOrSuccessfulWithdrawal::M6Withdrawal { .. }) => {
                            let err = error::InvalidM6::TreasuryOutputCount {
                                n_treasury_outputs: n_new_ctips,
                            };
                            return Err(err.into());
                        }
                    }
                }
                // M5
                Ordering::Greater => {
                    let address = if let Some(address_output) =
                        transaction.output.get(new_ctip.outpoint.vout as usize + 1)
                    {
                        crate::messages::try_parse_op_return_address(&address_output.script_pubkey)
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    let sequence_number = dbs
                        .treasury_utxo_count
                        .try_get(rotxn, &sidechain_number)?
                        .unwrap_or(0);
                    let deposit = Deposit {
                        sequence_number,
                        outpoint: new_ctip.outpoint,
                        address,
                        value: new_ctip.value - old_treasury_value,
                    };
                    match res.as_mut() {
                        None => {
                            let diff = diff::M5 {
                                new_ctips: HashMap::from_iter([(sidechain_number, new_ctip)]),
                            };
                            let deposits = HashMap::from_iter([(sidechain_number, deposit)]);
                            res =
                                Some(DepositsOrSuccessfulWithdrawal::M5Deposits { deposits, diff });
                        }
                        Some(DepositsOrSuccessfulWithdrawal::M5Deposits { deposits, diff }) => {
                            deposits.insert(sidechain_number, deposit);
                            diff.new_ctips.insert(sidechain_number, new_ctip);
                        }
                        Some(DepositsOrSuccessfulWithdrawal::M6Withdrawal { .. }) => {
                            return Err(error::HandleM5M6::Ambiguous);
                        }
                    }
                }
                Ordering::Equal => return Err(error::HandleM5M6::ZeroDiff),
            }
        }
        Ok(res)
    }

    /// Dispatches a single coinbase OP_RETURN message to its handler. M1/M2/M3/M4
    /// are BIP 300; M7 is BIP 301.
    ///
    /// <https://github.com/LayerTwo-Labs/bip300_bip301_specifications/blob/master/bip300.md>
    /// <https://github.com/LayerTwo-Labs/bip300_bip301_specifications/blob/master/bip301.md#m7-bmm-accept>
    #[expect(clippy::result_large_err)]
    fn handle_coinbase_message(
        &self,
        rotxn: &RoTxn,
        height: u32,
        prev_block_hash: BlockHash,
        accepted_bmm_requests: &mut BmmCommitments,
        message: CoinbaseMessage,
    ) -> Result<(Option<CoinbaseMessageEvent>, Option<diff::CoinbaseMsg>), error::ConnectBlock>
    {
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
                if let Some(diff) =
                    self.handle_m1_propose_sidechain(rotxn, sidechain_proposal, height)?
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
                let diff = self
                    .handle_m2_ack_sidechain(rotxn, height, sidechain_number, description_hash)?
                    .map(diff::CoinbaseMsg::AckSidechain);
                Ok((None, diff))
            }
            CoinbaseMessage::M3ProposeBundle(M3ProposeBundle {
                sidechain_number,
                bundle_txid,
            }) => {
                let diff =
                    self.handle_m3_propose_bundle(rotxn, sidechain_number, bundle_txid.into())?;
                let event = CoinbaseMessageEvent::WithdrawalBundle(WithdrawalBundleEvent {
                    sidechain_id: sidechain_number,
                    m6id: bundle_txid.into(),
                    kind: WithdrawalBundleEventKind::Submitted,
                });
                Ok((Some(event), Some(diff::CoinbaseMsg::ProposeBundle(diff))))
            }
            CoinbaseMessage::M4AckBundles(m4) => {
                let diff = self.handle_m4_ack_bundles(rotxn, prev_block_hash, &m4)?;
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

    fn handle_transaction(
        &self,
        rotxn: &RoTxn,
        accepted_bmm_requests: Option<&BmmCommitments>,
        prev_mainchain_block_hash: &BlockHash,
        transaction: &Transaction,
    ) -> Result<Option<(TransactionEvent, diff::Tx)>, error::HandleTransaction> {
        let mut res = None;
        match self.handle_m5_m6(rotxn, Cow::Borrowed(transaction))? {
            Some(DepositsOrSuccessfulWithdrawal::M5Deposits { deposits, diff }) => {
                res = Some((TransactionEvent::Deposits(deposits), diff::Tx::M5(diff)));
            }
            Some(DepositsOrSuccessfulWithdrawal::M6Withdrawal {
                sidechain_id,
                m6id,
                sequence_number,
                diff,
            }) => {
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
                    diff::Tx::M6(diff),
                ));
            }
            None => (),
        };
        if handle_m8(
            transaction,
            accepted_bmm_requests,
            prev_mainchain_block_hash,
        )
        .map_err(error::HandleTransaction::M8)?
        {
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
    pub(in crate::validator) fn validate_tx(
        &self,
        parent_rwtxn: &mut RwTxn,
        transaction: &Transaction,
    ) -> Result<bool, error::ValidateTransaction> {
        let dbs = self.dbs;
        let mut child_rwtxn = dbs.nested_write_txn(parent_rwtxn)?;
        let tip_hash = dbs
            .current_chain_tip
            .try_get(&child_rwtxn, &())?
            .ok_or(error::ValidateTransactionInner::NoChainTip)?;
        let tip_height = dbs.block_hashes.height().get(&child_rwtxn, &tip_hash)?;
        match self.handle_transaction(&child_rwtxn, None, &tip_hash, transaction) {
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
    #[expect(clippy::result_large_err)]
    pub(in crate::validator) fn connect_block(
        &self,
        rwtxn: &mut RwTxn,
        block: &Block,
    ) -> Result<Event, error::ConnectBlock> {
        let dbs = self.dbs;
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
            let (event, diff) = coinbase_msg_diffs.rotxn(|rotxn, _dbs| {
                self.handle_coinbase_message(
                    rotxn,
                    height,
                    parent,
                    &mut accepted_bmm_requests,
                    message,
                )
            })?;
            match event {
                Some(CoinbaseMessageEvent::NewSidechainProposal { sidechain }) => {
                    let proposal = sidechain.proposal;
                    let description_hash = proposal.description.sha256d_hash();
                    let index =
                        m1_sidechain_proposals[&(proposal.sidechain_number, description_hash)];
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
                let upvotes =
                    vec![M4AckBundles::ABSTAIN_ONE_BYTE; active_sidechains_count as usize];
                let m4 = M4AckBundles::OneByte { upvotes };
                let diff = self.handle_m4_ack_bundles(rotxn, parent, &m4)?;
                Ok::<_, error::ConnectBlock>(diff)
            })?;
            if !diff.0.is_empty() {
                let () = coinbase_msg_diffs.apply(diff::CoinbaseMsg::AckBundles(diff))?;
            }
        }
        // Coinbase msg diffs are already applied
        let coinbase_msg_diffs = coinbase_msg_diffs.diffs();
        let failed_sidechain_proposals_diff =
            self.handle_failed_sidechain_proposals(rwtxn, height)?;
        let () =
            failed_sidechain_proposals_diff.apply(rwtxn, &dbs.proposal_id_to_sidechain, height)?;
        let failed_m6ids_diff = self.handle_failed_m6ids(rwtxn, height)?;
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
            let Some((tx_event, diff)) = tx_diffs
                .rotxn(|rotxn, _dbs| {
                    self.handle_transaction(
                        rotxn,
                        Some(&accepted_bmm_requests),
                        &prev_mainchain_block_hash,
                        transaction,
                    )
                })
                .map_err(|source| error::ConnectBlock::Transaction {
                    txid: transaction.compute_txid(),
                    block_hash,
                    source,
                })?
            else {
                continue 'connect_txs;
            };
            let () = tx_diffs.apply(diff)?;
            match tx_event {
                TransactionEvent::Deposits(deposits) => {
                    events.push(BlockEvent::Deposits(deposits));
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
        &self,
        rwtxn: &mut RwTxn,
        event_tx: &Sender<Event>,
        block_hash: BlockHash,
    ) -> Result<(), error::DisconnectBlock> {
        let dbs = self.dbs;
        // Absence of a stored diff means we rejected the block. Nothing do to on our end!
        let Some(diff) = dbs.block_hashes.diff().try_get(rwtxn, &block_hash)? else {
            tracing::trace!(
                %block_hash,
                "disconnect_block: block was rejected, treating as no-op"
            );
            return Ok(());
        };

        if let Some(tip_hash) = dbs.current_chain_tip.try_get(rwtxn, &())?
            && tip_hash != block_hash
        {
            return Err(error::DisconnectBlock::TipHash {
                block_hash,
                tip_hash,
            });
        }
        let header_info = dbs.block_hashes.get_header_info(rwtxn, &block_hash)?;
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
async fn sync_headers<MainRpcClient>(
    dbs: &Dbs,
    main_rest_client: &MainRestClient,
    main_rpc_client: &MainRpcClient,
    main_tip: BlockHash,
    progress_tx: &tokio::sync::watch::Sender<HeaderSyncProgress>,
    cancel: CancellationToken,
) -> Result<(), error::Sync>
where
    MainRpcClient: bitcoin_jsonrpsee::client::MainClient + Sync,
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
    loop {
        if cancel.is_cancelled() {
            tracing::warn!("Header sync interrupted");
            return Err(error::Sync::Shutdown);
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
        jiff::SignedDuration::try_from(start.elapsed()).unwrap_or_default()
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

    let fetch_duration = start.elapsed();

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

    let deserialization_duration = start.elapsed() - fetch_duration;

    tracing::debug!(
        "Fetched ({:?}) and deserialized ({:?}) batch of {} block(s) with {} total transactions",
        fetch_duration,
        deserialization_duration,
        blocks.len(),
        blocks.iter().map(|block| block.txdata.len()).sum::<usize>(),
    );

    Ok(blocks)
}

impl BlockHandler<'_> {
    pub(in crate::validator) fn handle_block_batch<'a>(
        &self,
        rwtxn: &mut RwTxn<'a>,
        blocks: &[Block],
        event_tx: &Sender<Event>,
    ) -> Result<(), error::Sync> {
        let dbs = self.dbs;
        let start = Instant::now();

        let mut total_txs = 0;
        let mut total_bmm_commitments = 0;
        let mut total_events = 0;

        // Process blocks sequentially to maintain ordering and database consistency
        for block in blocks {
            let block_hash = block.block_hash();

            tracing::trace!("Syncing block #{} `{block_hash}`", {
                // Do the data fetch within the macro, to avoid the cost on higher
                // log levels
                dbs.block_hashes.height().get(rwtxn, &block_hash)?
            });

            let start_block = Instant::now();
            // FIXME: handle disconnects
            //
            // The old comment here said `invalidateblock` is handled by the
            // cusf-enforcer-mempool crate. That only holds for the live path; during
            // sync the crate just propagates the error, so a non-fatal (consensus-
            // invalid) block halted the enforcer. Surface it with the block hash so
            // the sync loop can invalidate it. Fatal (infra/DB) errors still abort.
            let event = match self.connect_block(rwtxn, block) {
                Ok(event) => event,
                Err(err) if !err.is_fatal() => {
                    return Err(error::Sync::BlockRejected {
                        block_hash,
                        source: Box::new(err),
                    });
                }
                Err(err) => return Err(err.into()),
            };

            let connect_block_duration =
                jiff::SignedDuration::try_from(start_block.elapsed()).unwrap_or_default();

            // Create dynamic fields using a HashMap for structured logging
            match &event {
                Event::ConnectBlock {
                    header_info,
                    block_info,
                } => {
                    // Keep all the blocks at info level in the beginning,
                    // and then taper off into less log noise
                    let log_interval = match header_info.height {
                        0..=999 => 1,
                        1000..=9999 => 10,
                        10_000..=99_999 => 100,
                        100_000.. => 1000,
                    };

                    total_txs += block.txdata.len();
                    total_bmm_commitments += block_info.bmm_commitments.len();
                    total_events += block_info.events.len();

                    // Apparently it isn't possible to do dynamic levels? wtf
                    // https://github.com/tokio-rs/tracing/issues/2730
                    if header_info.height % log_interval == 0 {
                        tracing::info!(
                            total_txs = block.txdata.len(),
                            bmm_commitments = block_info.bmm_commitments.len(),
                            sc_events = block_info.events.len(),
                            "Synced block #{}: `{}` in {connect_block_duration}",
                            header_info.height,
                            header_info.block_hash,
                        );
                    } else {
                        tracing::debug!(
                            total_txs = block.txdata.len(),
                            bmm_commitments = block_info.bmm_commitments.len(),
                            sc_events = block_info.events.len(),
                            "Synced block #{}: `{}` in {connect_block_duration}",
                            header_info.height,
                            header_info.block_hash,
                        );
                    };
                }
                Event::DisconnectBlock { block_hash } => {
                    tracing::debug!(
                        "Disconnected block: `{block_hash}` in {connect_block_duration}",
                    );
                }
            }
            // Events should only ever be sent after committing DB txs, see
            // https://github.com/LayerTwo-Labs/bip300301_enforcer/pull/185
            let _send_err: Result<Option<_>, TrySendError<_>> = event_tx.try_broadcast(event);
        }

        tracing::info!(
            total_txs = total_txs,
            total_bmm_commitments = total_bmm_commitments,
            total_events = total_events,
            "Synced batch of {} blocks in {}",
            blocks.len(),
            jiff::SignedDuration::try_from(start.elapsed()).unwrap_or_default(),
        );
        Ok(())
    }

    // MUST be called after `sync_headers`.
    #[tracing::instrument(skip_all)]
    async fn sync_blocks<MainRpcClient>(
        &self,
        event_tx: &Sender<Event>,
        main_rpc_client: &MainRpcClient,
        main_blocks_dir: Option<PathBuf>,
        main_tip: BlockHash,
        cancel: CancellationToken,
    ) -> Result<(), error::Sync>
    where
        MainRpcClient: bitcoin_jsonrpsee::client::MainClient + Sync,
    {
        // Batch size for concurrent block fetching
        // It's hard to know what a good size here is, without
        // further benchmarking.
        const BLOCK_FETCH_BATCH_SIZE: usize = 50;

        let dbs = self.dbs;
        let start = Instant::now();
        let mut missing_blocks = tokio::task::block_in_place(|| {
            let current_enforcer_tip = {
                let mut rwtxn = dbs.write_txn()?;
                let mut current_enforcer_tip = dbs
                    .current_chain_tip
                    .try_get(&rwtxn, &())?
                    .unwrap_or_else(BlockHash::all_zeros);
                let last_common_ancestor = dbs.block_hashes.last_common_ancestor(
                    &rwtxn,
                    current_enforcer_tip,
                    main_tip,
                )?;
                if current_enforcer_tip != last_common_ancestor {
                    tracing::info!(
                        "Disconnecting tip {current_enforcer_tip} -> {last_common_ancestor}"
                    );
                    while current_enforcer_tip != last_common_ancestor {
                        let () =
                            self.disconnect_block(&mut rwtxn, event_tx, current_enforcer_tip)?;
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

        let mut total_blocks_fetched: usize = 0;

        if let Some(main_blocks_dir) = main_blocks_dir {
            let start = Instant::now();
            tracing::debug!(
                network = %self.network,
                "syncing blocks from blocks dir: {}",
                main_blocks_dir.display()
            );

            match block_files::sync_from_directory(
                self,
                event_tx,
                &mut missing_blocks,
                main_blocks_dir,
                cancel.clone(),
            ) {
                Ok(total_handled_blocks) => {
                    total_blocks_fetched += total_handled_blocks as usize;
                    tracing::info!(
                        "Synced {total_handled_blocks} blocks from blocks dir in {:?}",
                        start.elapsed()
                    );
                }
                Err(e) => {
                    tracing::error!("Error syncing blocks from blocks dir: {e:#}");
                }
            }
        }

        // Process blocks in batches for better network efficiency
        let missing_blocks_rev: Vec<_> = missing_blocks.into_iter().rev().collect();
        for chunk in missing_blocks_rev.chunks(BLOCK_FETCH_BATCH_SIZE) {
            if cancel.is_cancelled() {
                tracing::warn!("Block sync interrupted");
                return Err(error::Sync::Shutdown);
            }

            let blocks = fetch_blocks_batch(main_rpc_client, chunk).await?;
            total_blocks_fetched += blocks.len();

            let mut rwtxn = dbs.write_txn()?;
            match self.handle_block_batch(&mut rwtxn, &blocks, event_tx) {
                Ok(()) => {
                    rwtxn.commit()?;
                }
                // A consensus-invalid block in the active chain must be invalidated
                // on the node so it reorgs away, not treated as a fatal sync error
                // that halts the enforcer. Without this, a bootstrapping or
                // catching-up enforcer halts forever on such a block (e.g. an
                // anyone-can-spend treasury UTXO spent without a replacement CTIP).
                // This mirrors the live `ConnectBlockAction::Reject` handling.
                Err(error::Sync::BlockRejected { block_hash, source }) => {
                    // Drop the write txn (discard the partial batch) before awaiting.
                    drop(rwtxn);
                    tracing::warn!(
                        %block_hash,
                        "invalidating consensus-invalid block during sync and re-syncing: {:#}",
                        crate::errors::ErrorChain::new(source.as_ref()),
                    );
                    main_rpc_client
                        .invalidate_block(block_hash)
                        .await
                        .map_err(|err| error::Sync::JsonRpc {
                            method: "invalidateblock".to_owned(),
                            source: err,
                        })?;
                    // The node has reorged away from the rejected block; stop here
                    // and let the caller re-sync to the new tip.
                    return Ok(());
                }
                Err(err) => return Err(err),
            }
        }

        tracing::info!(
            "Synced {total_blocks_fetched} blocks in {:?}",
            start.elapsed()
        );
        Ok(())
    }
}

// Is this a good name? "Signal" in this context means both
// signal receiver and signal sender
pub struct SyncSignals {
    pub cancel: CancellationToken,
    pub header_sync_progress_tx: tokio::sync::watch::Sender<HeaderSyncProgress>,
    pub event_tx: Sender<Event>,
}

impl BlockHandler<'_> {
    pub(in crate::validator) async fn sync_to_tip<MainClient>(
        &self,
        main_rpc_client: &MainClient,
        main_rest_client: &MainRestClient,
        main_blocks_dir: Option<PathBuf>,
        main_tip: BlockHash,
        signals: SyncSignals,
    ) -> Result<(), error::Sync>
    where
        MainClient: bitcoin_jsonrpsee::client::MainClient + Sync,
    {
        let () = sync_headers(
            self.dbs,
            main_rest_client,
            main_rpc_client,
            main_tip,
            &signals.header_sync_progress_tx,
            signals.cancel.clone(),
        )
        .await?;
        let () = self
            .sync_blocks(
                &signals.event_tx,
                main_rpc_client,
                main_blocks_dir,
                main_tip,
                signals.cancel.clone(),
            )
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use bitcoin::{
        Amount, BlockHash, OutPoint, ScriptBuf, Transaction, TxIn, TxOut, Txid, hashes::Hash as _,
    };
    use error_fatality::Fatality as _;
    use hashlink::LinkedHashMap;
    use miette::{IntoDiagnostic, Result, bail};

    use super::*;
    use crate::{
        messages::{M4AckBundles, M8BmmRequest, create_m5_deposit_output},
        types::{
            BmmCommitment, BmmCommitments, Ctip, M6id, SidechainDescription, SidechainNumber,
            SidechainProposal,
        },
        validator::test_utils::{create_test_dbs, test_block_header, test_m6id, test_sidechain},
    };

    /// `BlockHandler` bound to a test `Dbs`. Regtest gets [`Thresholds::SHORT`].
    fn test_handler(dbs: &Dbs) -> BlockHandler<'_> {
        BlockHandler::new(dbs, bitcoin::Network::Regtest)
    }

    fn build_m8_tx(
        sidechain_number: SidechainNumber,
        sidechain_block_hash: [u8; 32],
        prev_mainchain_block_hash: BlockHash,
    ) -> Transaction {
        let script_pubkey = M8BmmRequest::script_pubkey(
            sidechain_number,
            BmmCommitment(sidechain_block_hash),
            prev_mainchain_block_hash,
        )
        .expect("failed to build M8 script");
        Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![TxIn::default()],
            output: vec![TxOut {
                script_pubkey,
                value: Amount::ZERO,
            }],
        }
    }

    fn build_plain_tx() -> Transaction {
        Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![TxIn::default()],
            output: vec![TxOut {
                script_pubkey: ScriptBuf::new(),
                value: Amount::from_sat(1000),
            }],
        }
    }

    fn build_m5_deposit_tx(
        sidechain_number: SidechainNumber,
        old_ctip_outpoint: OutPoint,
        old_ctip_value: Amount,
        deposit_amount: Amount,
    ) -> Transaction {
        let treasury_output =
            create_m5_deposit_output(sidechain_number, old_ctip_value, deposit_amount);
        let address_output = TxOut {
            script_pubkey: ScriptBuf::new_op_return(
                bitcoin::script::PushBytesBuf::try_from(b"sidechain_address".to_vec()).unwrap(),
            ),
            value: Amount::ZERO,
        };
        Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: old_ctip_outpoint,
                ..TxIn::default()
            }],
            output: vec![treasury_output, address_output],
        }
    }

    /// A transaction with a single output scripted exactly like a treasury
    /// UTXO — `OP_DRIVECHAIN <slot> OP_TRUE` — for an arbitrary slot and value,
    /// spending `input_outpoint`. Used to exercise outputs that *look* like
    /// CTIPs but target an inactive sidechain slot.
    fn build_drivechain_output_tx(
        slot: SidechainNumber,
        value: Amount,
        input_outpoint: OutPoint,
    ) -> Transaction {
        Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: input_outpoint,
                ..TxIn::default()
            }],
            output: vec![create_m5_deposit_output(slot, Amount::ZERO, value)],
        }
    }

    fn dummy_block_hash(byte: u8) -> BlockHash {
        BlockHash::from_byte_array([byte; 32])
    }

    // ── handle_m8 ──

    #[test]
    fn handle_m8_non_m8_tx_returns_false() -> Result<()> {
        let prev_hash = dummy_block_hash(0xAA);
        assert!(!handle_m8(&build_plain_tx(), None, &prev_hash).into_diagnostic()?);
        Ok(())
    }

    #[test]
    fn handle_m8_valid_request_accepted() -> Result<()> {
        let sc = SidechainNumber(1);
        let prev_hash = dummy_block_hash(0xAA);
        let tx = build_m8_tx(sc, [0x42; 32], prev_hash);

        let mut accepted: BmmCommitments = LinkedHashMap::new();
        accepted.insert(sc, BmmCommitment([0x42; 32]));

        assert!(handle_m8(&tx, Some(&accepted), &prev_hash).into_diagnostic()?);
        Ok(())
    }

    #[test]
    fn handle_m8_rejection_cases_are_non_fatal() {
        let sc = SidechainNumber(1);
        let prev_hash = dummy_block_hash(0xAA);
        let tx = build_m8_tx(sc, [0x42; 32], prev_hash);

        let empty: BmmCommitments = LinkedHashMap::new();
        let err =
            handle_m8(&tx, Some(&empty), &prev_hash).expect_err("empty accepted list must reject");
        assert!(matches!(err, error::HandleM8::NotAcceptedByMiners));
        assert!(!err.is_fatal());

        let mut wrong: BmmCommitments = LinkedHashMap::new();
        wrong.insert(sc, BmmCommitment([0xFF; 32]));
        let err =
            handle_m8(&tx, Some(&wrong), &prev_hash).expect_err("wrong commitment must reject");
        assert!(matches!(err, error::HandleM8::NotAcceptedByMiners));

        let err =
            handle_m8(&tx, None, &dummy_block_hash(0xBB)).expect_err("wrong prev_hash must reject");
        assert!(matches!(err, error::HandleM8::BmmRequestExpired));
        assert!(!err.is_fatal());
    }

    #[test]
    fn handle_m8_none_accepted_only_checks_expiry() -> Result<()> {
        let sc = SidechainNumber(1);
        let prev_hash = dummy_block_hash(0xAA);
        let tx = build_m8_tx(sc, [0x42; 32], prev_hash);

        assert!(handle_m8(&tx, None, &prev_hash).into_diagnostic()?);
        assert!(handle_m8(&tx, None, &dummy_block_hash(0xBB)).is_err());
        Ok(())
    }

    // ── handle_transaction ──

    #[test]
    fn handle_transaction_m8_errors_propagate_as_non_fatal() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let rotxn = dbs.read_txn().into_diagnostic()?;
        let sc = SidechainNumber(1);
        let prev_hash = dummy_block_hash(0xAA);
        let tx = build_m8_tx(sc, [0x42; 32], prev_hash);

        let handler = test_handler(&dbs);
        let empty: BmmCommitments = LinkedHashMap::new();
        let err = handler
            .handle_transaction(&rotxn, Some(&empty), &prev_hash, &tx)
            .expect_err("not-accepted M8 must error");
        assert!(!err.is_fatal());

        let err = handler
            .handle_transaction(&rotxn, None, &dummy_block_hash(0xBB), &tx)
            .expect_err("expired M8 must error");
        assert!(!err.is_fatal());
        Ok(())
    }

    #[test]
    fn handle_transaction_valid_m8_returns_none() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let rotxn = dbs.read_txn().into_diagnostic()?;
        let sc = SidechainNumber(1);
        let prev_hash = dummy_block_hash(0xAA);
        let tx = build_m8_tx(sc, [0x42; 32], prev_hash);

        let mut accepted: BmmCommitments = LinkedHashMap::new();
        accepted.insert(sc, BmmCommitment([0x42; 32]));

        let result = test_handler(&dbs)
            .handle_transaction(&rotxn, Some(&accepted), &prev_hash, &tx)
            .into_diagnostic()?;
        assert!(result.is_none(), "valid M8 should not produce a tx event");
        Ok(())
    }

    // ── connect_block ──

    #[derive(Default)]
    struct TestBlockParts {
        extra_coinbase_outputs: Vec<TxOut>,
        extra_txs: Vec<Transaction>,
    }

    fn build_test_block(prev_hash: BlockHash, parts: TestBlockParts) -> Block {
        let mut coinbase_outputs = vec![TxOut {
            script_pubkey: ScriptBuf::new(),
            value: Amount::from_sat(50_0000_0000),
        }];
        coinbase_outputs.extend(parts.extra_coinbase_outputs);
        let coinbase_tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: Txid::all_zeros(),
                    vout: 0xFFFFFFFF,
                },
                ..TxIn::default()
            }],
            output: coinbase_outputs,
        };
        let mut txdata = vec![coinbase_tx];
        txdata.extend(parts.extra_txs);
        Block {
            header: bitcoin::block::Header {
                version: bitcoin::block::Version::TWO,
                prev_blockhash: prev_hash,
                merkle_root: bitcoin::TxMerkleNode::all_zeros(),
                time: 0,
                bits: bitcoin::CompactTarget::from_consensus(0x2000_0000),
                nonce: 0,
            },
            txdata,
        }
    }

    #[test]
    fn connect_block_does_not_skip_non_fatal_tx_errors() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let prev_hash = BlockHash::all_zeros();

        // M8 request with no matching M7 in coinbase → non-fatal error
        let m8_tx = build_m8_tx(SidechainNumber(1), [0x42; 32], prev_hash);
        let block = build_test_block(
            prev_hash,
            TestBlockParts {
                extra_txs: vec![m8_tx],
                ..Default::default()
            },
        );

        dbs.block_hashes
            .put_headers(&mut rwtxn, &[(block.header, 0)])
            .into_diagnostic()?;

        assert!(
            test_handler(&dbs)
                .connect_block(&mut rwtxn, &block)
                .is_err(),
            "connect_block should not succeed due to non-fatal M8 error"
        );
        Ok(())
    }

    // Regression test for the sync-path DoS: a block the mainchain node accepts
    // but that is consensus-invalid per BIP300/301 (here an M8 BMM request with no
    // matching M7 accept; the motivating real case is a treasury UTXO spent
    // without a replacement CTIP) must be surfaced by `handle_block_batch` as the
    // *recoverable* `Sync::BlockRejected` carrying the offending block hash — so
    // the sync loop can `invalidateblock` it and re-sync — NOT propagated as a
    // fatal error that halts a bootstrapping/catching-up enforcer.
    #[test]
    fn handle_block_batch_rejects_consensus_invalid_block_without_halting() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let prev_hash = BlockHash::all_zeros();

        let m8_tx = build_m8_tx(SidechainNumber(1), [0x42; 32], prev_hash);
        let block = build_test_block(
            prev_hash,
            TestBlockParts {
                extra_txs: vec![m8_tx],
                ..Default::default()
            },
        );
        let block_hash = block.block_hash();

        dbs.block_hashes
            .put_headers(&mut rwtxn, &[(block.header, 0)])
            .into_diagnostic()?;

        let (event_tx, _event_rx) = async_broadcast::broadcast(16);
        let err = test_handler(&dbs)
            .handle_block_batch(&mut rwtxn, std::slice::from_ref(&block), &event_tx)
            .expect_err("consensus-invalid block must be rejected");

        // Must be recoverable (so the sync loop invalidates + re-syncs), not fatal.
        assert!(!err.is_fatal(), "a consensus rejection must not be fatal");
        match err {
            error::Sync::BlockRejected {
                block_hash: rejected,
                ..
            } => assert_eq!(
                rejected, block_hash,
                "BlockRejected must carry the offending block hash for invalidateblock"
            ),
            other => panic!("expected Sync::BlockRejected, got: {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn connect_then_disconnect_restores_db_state() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let prev_hash = BlockHash::all_zeros();
        let block = build_test_block(prev_hash, TestBlockParts::default());
        let block_hash = block.header.block_hash();

        dbs.block_hashes
            .put_headers(&mut rwtxn, &[(block.header, 0)])
            .into_diagnostic()?;
        assert!(
            dbs.current_chain_tip
                .try_get(&rwtxn, &())
                .into_diagnostic()?
                .is_none()
        );

        let (event_tx, _) = async_broadcast::broadcast(16);
        let handler = test_handler(&dbs);
        let _event = handler
            .connect_block(&mut rwtxn, &block)
            .into_diagnostic()?;
        assert_eq!(
            dbs.current_chain_tip
                .try_get(&rwtxn, &())
                .into_diagnostic()?,
            Some(block_hash)
        );

        handler
            .disconnect_block(&mut rwtxn, &event_tx, block_hash)
            .into_diagnostic()?;
        assert!(
            dbs.current_chain_tip
                .try_get(&rwtxn, &())
                .into_diagnostic()?
                .is_none()
        );
        Ok(())
    }

    // ── handle_m5_m6 ──

    #[test]
    fn handle_m5_m6_plain_tx_returns_none() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let rotxn = dbs.read_txn().into_diagnostic()?;
        assert!(
            test_handler(&dbs)
                .handle_m5_m6(&rotxn, Cow::Borrowed(&build_plain_tx()))
                .into_diagnostic()?
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn handle_m5_m6_deposit_no_existing_ctip() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let sc = SidechainNumber(1);
        // A deposit can only target an *active* sidechain.
        dbs.active_sidechains
            .put_sidechain(&mut rwtxn, &sc, &test_sidechain(1, 0))
            .into_diagnostic()?;
        let deposit_amount = Amount::from_sat(10_000);
        let tx = build_m5_deposit_tx(sc, OutPoint::default(), Amount::ZERO, deposit_amount);

        let result = test_handler(&dbs)
            .handle_m5_m6(&rwtxn, Cow::Borrowed(&tx))
            .into_diagnostic()?;
        let Some(DepositsOrSuccessfulWithdrawal::M5Deposits { deposits, diff }) = result else {
            panic!("expected M5 deposit");
        };
        assert_eq!(deposits.len(), 1);
        assert!(deposits.contains_key(&sc));
        assert_eq!(deposits[&sc].value, deposit_amount);
        assert_eq!(deposits[&sc].address, b"sidechain_address");
        assert!(diff.new_ctips.contains_key(&sc));
        assert_eq!(diff.new_ctips[&sc].value, deposit_amount);
        Ok(())
    }

    #[test]
    fn handle_m5_m6_deposit_with_existing_ctip() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let sc = SidechainNumber(1);
        dbs.active_sidechains
            .put_sidechain(&mut rwtxn, &sc, &test_sidechain(1, 0))
            .into_diagnostic()?;
        let old_outpoint = OutPoint {
            txid: Txid::from_byte_array([0x11; 32]),
            vout: 0,
        };
        let old_value = Amount::from_sat(5_000);
        dbs.active_sidechains
            .put_ctip(
                &mut rwtxn,
                sc,
                &Ctip {
                    outpoint: old_outpoint,
                    value: old_value,
                },
            )
            .into_diagnostic()?;

        let deposit_amount = Amount::from_sat(3_000);
        let tx = build_m5_deposit_tx(sc, old_outpoint, old_value, deposit_amount);

        let Some(DepositsOrSuccessfulWithdrawal::M5Deposits { deposits, diff }) =
            test_handler(&dbs)
                .handle_m5_m6(&rwtxn, Cow::Borrowed(&tx))
                .into_diagnostic()?
        else {
            panic!("expected M5 deposit");
        };
        assert!(deposits.contains_key(&sc));
        assert_eq!(deposits[&sc].value, deposit_amount);
        assert!(diff.new_ctips.contains_key(&sc));
        assert_eq!(diff.new_ctips[&sc].value, old_value + deposit_amount);
        Ok(())
    }

    #[test]
    fn handle_m5_m6_old_ctip_unspent_is_not_fatal() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let sc = SidechainNumber(1);
        dbs.active_sidechains
            .put_sidechain(&mut rwtxn, &sc, &test_sidechain(1, 0))
            .into_diagnostic()?;
        dbs.active_sidechains
            .put_ctip(
                &mut rwtxn,
                sc,
                &Ctip {
                    outpoint: OutPoint {
                        txid: Txid::from_byte_array([0x11; 32]),
                        vout: 0,
                    },
                    value: Amount::from_sat(5_000),
                },
            )
            .into_diagnostic()?;

        let tx = build_m5_deposit_tx(
            sc,
            OutPoint::default(),
            Amount::from_sat(5_000),
            Amount::from_sat(1_000),
        );
        let err = test_handler(&dbs)
            .handle_m5_m6(&rwtxn, Cow::Borrowed(&tx))
            .expect_err("spending wrong outpoint must error");
        assert!(matches!(err, error::HandleM5M6::OldCtipUnspent { .. }));
        assert!(!err.is_fatal());
        Ok(())
    }

    /// BIP 300: a tx that spends a treasury UTXO without creating a new
    /// treasury UTXO is invalid. OP_DRIVECHAIN is anyone-can-spend, so without
    /// this rule anyone could drain the treasury.
    #[test]
    fn handle_m5_m6_treasury_spent_without_new_ctip_is_invalid() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let sc = SidechainNumber(1);
        let treasury_outpoint = OutPoint {
            txid: Txid::from_byte_array([0x11; 32]),
            vout: 0,
        };
        dbs.active_sidechains
            .put_ctip(
                &mut rwtxn,
                sc,
                &Ctip {
                    outpoint: treasury_outpoint,
                    value: Amount::from_sat(5_000),
                },
            )
            .into_diagnostic()?;

        // Spend the treasury UTXO and pay it all to a non-treasury output.
        let theft_tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: treasury_outpoint,
                ..TxIn::default()
            }],
            output: vec![TxOut {
                script_pubkey: ScriptBuf::new(),
                value: Amount::from_sat(5_000),
            }],
        };

        let err = test_handler(&dbs)
            .handle_m5_m6(&rwtxn, Cow::Borrowed(&theft_tx))
            .expect_err("spending a treasury without creating a new one must be invalid");
        assert!(matches!(
            err,
            error::HandleM5M6::TreasurySpentWithoutNewCtip { sidechain_number }
                if sidechain_number == sc
        ));
        assert!(!err.is_fatal());
        Ok(())
    }

    #[test]
    fn handle_m5_m6_inactive_slot_drivechain_output_is_ignored() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let rotxn = dbs.read_txn().into_diagnostic()?;
        // Slot 42 is not an active sidechain. `b4 01 2a 51` =
        // OP_DRIVECHAIN OP_PUSHBYTES_1 <42> OP_TRUE, value 0.
        let inactive = SidechainNumber(42);
        let tx = build_drivechain_output_tx(inactive, Amount::ZERO, OutPoint::default());

        // Sanity check: the output really does parse as a treasury UTXO.
        assert_eq!(
            parse_op_drivechain(tx.output[0].script_pubkey.as_bytes())
                .expect("output should parse as OP_DRIVECHAIN")
                .1,
            inactive,
        );
        assert!(
            test_handler(&dbs)
                .handle_m5_m6(&rotxn, Cow::Borrowed(&tx))
                .into_diagnostic()?
                .is_none(),
            "inactive-slot OP_DRIVECHAIN output must be ignored, not treated as a CTIP",
        );
        Ok(())
    }

    #[test]
    fn connect_block_accepts_zero_value_inactive_slot_drivechain_output() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let prev_hash = BlockHash::all_zeros();

        let tx = build_drivechain_output_tx(SidechainNumber(42), Amount::ZERO, OutPoint::default());
        let block = build_test_block(
            prev_hash,
            TestBlockParts {
                extra_txs: vec![tx],
                ..Default::default()
            },
        );
        dbs.block_hashes
            .put_headers(&mut rwtxn, &[(block.header, 0)])
            .into_diagnostic()?;

        test_handler(&dbs)
            .connect_block(&mut rwtxn, &block)
            .into_diagnostic()?;
        Ok(())
    }

    #[test]
    fn connect_block_accepts_inactive_slot_drivechain_output_spent_in_same_block() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let prev_hash = BlockHash::all_zeros();

        let create_tx = build_drivechain_output_tx(
            SidechainNumber(42),
            Amount::from_sat(1_000),
            OutPoint::default(),
        );
        let spend_tx = Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::locktime::absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint {
                    txid: create_tx.compute_txid(),
                    vout: 0,
                },
                ..TxIn::default()
            }],
            output: vec![TxOut {
                script_pubkey: ScriptBuf::new(),
                value: Amount::from_sat(1_000),
            }],
        };
        let block = build_test_block(
            prev_hash,
            TestBlockParts {
                extra_txs: vec![create_tx, spend_tx],
                ..Default::default()
            },
        );
        dbs.block_hashes
            .put_headers(&mut rwtxn, &[(block.header, 0)])
            .into_diagnostic()?;

        test_handler(&dbs)
            .connect_block(&mut rwtxn, &block)
            .into_diagnostic()?;
        Ok(())
    }

    // ── handle_m1 ──

    #[test]
    fn handle_m1_new_and_duplicate() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;

        let proposal = SidechainProposal {
            sidechain_number: SidechainNumber(1),
            description: SidechainDescription(vec![0x00, 0x01, b'x']),
        };

        let handler = test_handler(&dbs);
        let diff = handler
            .handle_m1_propose_sidechain(&rwtxn, proposal.clone(), 100)
            .into_diagnostic()?
            .expect("new proposal should produce a diff");
        assert_eq!(diff.sidechain.proposal, proposal);
        assert_eq!(diff.sidechain.status.proposal_height, 100);

        dbs.proposal_id_to_sidechain
            .put(&mut rwtxn, &diff.id, &diff.sidechain)
            .into_diagnostic()?;
        let result = handler
            .handle_m1_propose_sidechain(&rwtxn, proposal, 200)
            .into_diagnostic()?;
        assert!(result.is_none(), "duplicate proposal should be ignored");
        Ok(())
    }

    // ── handle_m2 ──

    #[test]
    fn handle_m2_activation_thresholds() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let handler = test_handler(&dbs);
        let max_age = handler.thresholds.unused_sidechain_slot_proposal_max_age as u32;
        let activation_threshold = handler
            .thresholds
            .unused_sidechain_slot_activation_threshold;

        let proposal_height: u32 = 100;
        let mut sidechain = test_sidechain(1, proposal_height);
        let proposal_id = sidechain.proposal.compute_id();

        dbs.proposal_id_to_sidechain
            .put(&mut rwtxn, &proposal_id, &sidechain)
            .into_diagnostic()?;
        let diff = handler
            .handle_m2_ack_sidechain(
                &rwtxn,
                proposal_height + 1,
                proposal_id.sidechain_number,
                proposal_id.description_hash,
            )
            .into_diagnostic()?
            .expect("M2 for known proposal must produce a diff");
        assert!(!diff.activated, "1 vote should not activate");

        sidechain.status.vote_count = activation_threshold;
        dbs.proposal_id_to_sidechain
            .put(&mut rwtxn, &proposal_id, &sidechain)
            .into_diagnostic()?;
        let diff = handler
            .handle_m2_ack_sidechain(
                &rwtxn,
                proposal_height + max_age,
                proposal_id.sidechain_number,
                proposal_id.description_hash,
            )
            .into_diagnostic()?
            .expect("M2 for known proposal must produce a diff");
        assert!(
            diff.activated,
            "vote_count > threshold within max age should activate"
        );

        dbs.proposal_id_to_sidechain
            .put(&mut rwtxn, &proposal_id, &sidechain)
            .into_diagnostic()?;
        let diff = handler
            .handle_m2_ack_sidechain(
                &rwtxn,
                proposal_height + max_age + 1,
                proposal_id.sidechain_number,
                proposal_id.description_hash,
            )
            .into_diagnostic()?
            .expect("M2 for known proposal must produce a diff");
        assert!(
            !diff.activated,
            "proposal exceeding max age should not activate"
        );
        Ok(())
    }

    /// BIP 300 M2: an M2 whose `description_hash` doesn't match any prior
    /// M1's `sha256d(D)` MUST be ignored ("interpreted as an ordinary
    /// script"). The containing block must NOT be rejected.
    #[test]
    fn connect_block_accepts_m2_for_unknown_proposal() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let prev_hash = BlockHash::all_zeros();

        let m2_script: ScriptBuf = M2AckSidechain {
            sidechain_number: SidechainNumber(99),
            description_hash: bitcoin::hashes::sha256d::Hash::all_zeros(),
        }
        .try_into()
        .into_diagnostic()?;
        let block = build_test_block(
            prev_hash,
            TestBlockParts {
                extra_coinbase_outputs: vec![TxOut {
                    script_pubkey: m2_script,
                    value: Amount::ZERO,
                }],
                ..Default::default()
            },
        );

        dbs.block_hashes
            .put_headers(&mut rwtxn, &[(block.header, 0)])
            .into_diagnostic()?;

        test_handler(&dbs)
            .connect_block(&mut rwtxn, &block)
            .into_diagnostic()
            .map_err(|e| {
                miette::miette!(
                    "M2 referencing unknown proposal must be ignored, \
                     not reject the block: {e:#}"
                )
            })?;
        Ok(())
    }

    /// BIP 300: a proposal must fail once it has accumulated enough non-ack
    /// blocks that it can no longer reach the activation threshold, even before
    /// its max age. Regtest uses max_age=10, threshold=5, so max_fails=5.
    #[test]
    fn handle_failed_sidechain_proposals_early_fails_doomed_proposal() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let handler = test_handler(&dbs);

        // Doomed (unused slot, never acked): at height 6, age=6 > max_fails=5
        // and fails=6 >= 5, but age <= max_age=10, so only the early-failure
        // rule can catch it.
        let doomed = test_sidechain(1, 0);
        let doomed_id = doomed.proposal.compute_id();
        dbs.proposal_id_to_sidechain
            .put(&mut rwtxn, &doomed_id, &doomed)
            .into_diagnostic()?;

        // Healthy (acked every block, fails=0): must not fail.
        let mut healthy = test_sidechain(2, 0);
        healthy.status.vote_count = 6;
        let healthy_id = healthy.proposal.compute_id();
        dbs.proposal_id_to_sidechain
            .put(&mut rwtxn, &healthy_id, &healthy)
            .into_diagnostic()?;

        let failed = handler
            .handle_failed_sidechain_proposals(&rwtxn, 6)
            .into_diagnostic()?;
        assert!(
            failed.0.contains_key(&doomed_id),
            "a proposal that can no longer reach the threshold must fail early"
        );
        assert!(
            !failed.0.contains_key(&healthy_id),
            "a fully-acked proposal must not fail"
        );
        Ok(())
    }

    // ── handle_m3 ──

    /// BIP 300 M3: a newly proposed bundle starts with an ACK score of 1
    /// (the proposer's implicit vote), not 0.
    #[test]
    fn handle_m3_propose_starts_vote_count_at_one() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let sc = SidechainNumber(1);
        dbs.active_sidechains
            .put_sidechain(&mut rwtxn, &sc, &test_sidechain(1, 0))
            .into_diagnostic()?;

        let m6id = test_m6id(0xAA);
        let diff = test_handler(&dbs)
            .handle_m3_propose_bundle(&rwtxn, sc, m6id)
            .into_diagnostic()?;
        diff.apply(&mut rwtxn, &dbs.active_sidechains, 0)
            .into_diagnostic()?;

        let pending = dbs
            .active_sidechains
            .pending_m6ids()
            .get(&rwtxn, &sc)
            .into_diagnostic()?;
        assert_eq!(pending[&m6id].vote_count, 1);
        Ok(())
    }

    /// BIP 300 M3: re-proposing a withdrawal bundle that is already pending
    /// must be rejected, otherwise a miner could reset its ack count and age.
    #[test]
    fn handle_m3_rejects_already_pending_bundle() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let sc = SidechainNumber(1);
        dbs.active_sidechains
            .put_sidechain(&mut rwtxn, &sc, &test_sidechain(1, 0))
            .into_diagnostic()?;

        // Propose a bundle and apply it, so the m6id is pending.
        let m6id = test_m6id(0xAA);
        let diff = test_handler(&dbs)
            .handle_m3_propose_bundle(&rwtxn, sc, m6id)
            .into_diagnostic()?;
        diff.apply(&mut rwtxn, &dbs.active_sidechains, 0)
            .into_diagnostic()?;

        // Re-proposing the same m6id is rejected.
        let err = test_handler(&dbs)
            .handle_m3_propose_bundle(&rwtxn, sc, m6id)
            .expect_err("re-proposing a pending bundle must be rejected");
        assert!(matches!(
            err,
            error::HandleM3ProposeBundle::BundleAlreadyPending { sidechain_number, m6id: e }
                if sidechain_number == sc && e == m6id
        ));
        assert!(!err.is_fatal());

        // A different, not-yet-pending m6id is still accepted.
        assert!(
            test_handler(&dbs)
                .handle_m3_propose_bundle(&rwtxn, sc, test_m6id(0xBB))
                .is_ok(),
            "a not-yet-pending bundle should be accepted"
        );
        Ok(())
    }

    // ── handle_m4_votes ──

    #[test]
    fn handle_m4_votes_abstain_and_upvote() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let sc = SidechainNumber(1);
        dbs.active_sidechains
            .put_sidechain(&mut rwtxn, &sc, &test_sidechain(1, 0))
            .into_diagnostic()?;

        let diff = test_handler(&dbs)
            .handle_m4_votes(&rwtxn, &[M4AckBundles::ABSTAIN_TWO_BYTES])
            .into_diagnostic()?;
        assert!(diff.0.is_empty());

        let m6id = test_m6id(0xAA);
        dbs.active_sidechains
            .put_pending_m6id(&mut rwtxn, &sc, m6id, 0)
            .into_diagnostic()?;
        let diff = test_handler(&dbs)
            .handle_m4_votes(&rwtxn, &[0])
            .into_diagnostic()?;
        assert!(diff.0.contains_key(&sc));
        Ok(())
    }

    #[test]
    fn handle_m4_votes_alarm() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let sc = SidechainNumber(1);
        dbs.active_sidechains
            .put_sidechain(&mut rwtxn, &sc, &test_sidechain(1, 0))
            .into_diagnostic()?;

        let m6id = test_m6id(0xAA);
        dbs.active_sidechains
            .put_pending_m6id(&mut rwtxn, &sc, m6id, 0)
            .into_diagnostic()?;
        dbs.active_sidechains
            .with_pending_withdrawal_entry(&mut rwtxn, &sc, m6id, |entry| {
                if let ordermap::map::Entry::Occupied(mut e) = entry {
                    e.get_mut().vote_count = 3;
                }
            })
            .into_diagnostic()?;

        let diff = test_handler(&dbs)
            .handle_m4_votes(&rwtxn, &[M4AckBundles::ALARM_TWO_BYTES])
            .into_diagnostic()?;
        assert!(diff.0.contains_key(&sc));
        Ok(())
    }

    #[test]
    fn handle_m4_votes_errors() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let sc = SidechainNumber(1);
        dbs.active_sidechains
            .put_sidechain(&mut rwtxn, &sc, &test_sidechain(1, 0))
            .into_diagnostic()?;

        let err = test_handler(&dbs)
            .handle_m4_votes(&rwtxn, &[0, 0])
            .expect_err("wrong vote count must error");
        assert!(matches!(
            err,
            error::HandleM4Votes::InvalidVotes {
                expected: 1,
                len: 2
            }
        ));

        let err = test_handler(&dbs)
            .handle_m4_votes(&rwtxn, &[0])
            .expect_err("upvote with no pending must error");
        assert!(matches!(err, error::HandleM4Votes::UpvoteFailed { .. }));
        Ok(())
    }

    /// BIP 300: bundles in the same sidechain that didn't receive a vote get
    /// -1. Verifies the diff records non-leader bundles with vote_count > 0
    /// in `downvoted_others`, and apply/undo are symmetric.
    #[test]
    fn handle_m4_votes_upvote_downvotes_other_positive_bundles() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let sc = SidechainNumber(1);
        dbs.active_sidechains
            .put_sidechain(&mut rwtxn, &sc, &test_sidechain(1, 0))
            .into_diagnostic()?;

        let target = test_m6id(0xAA);
        let other_positive = test_m6id(0xBB);
        let other_zero = test_m6id(0xCC);
        for m6id in [target, other_positive, other_zero] {
            dbs.active_sidechains
                .put_pending_m6id(&mut rwtxn, &sc, m6id, 0)
                .into_diagnostic()?;
        }
        set_vote_count(&mut rwtxn, &dbs, &sc, target, 2);
        set_vote_count(&mut rwtxn, &dbs, &sc, other_positive, 3);
        set_vote_count(&mut rwtxn, &dbs, &sc, other_zero, 0);

        // Upvote target (index 0). Spec: target +1; other_positive -1;
        // other_zero unchanged (saturating at 0).
        let diff = test_handler(&dbs)
            .handle_m4_votes(&rwtxn, &[0])
            .into_diagnostic()?;
        let Some(diff::AckBundleAction::Upvote {
            m6id,
            downvoted_others,
        }) = diff.0.get(&sc)
        else {
            bail!("expected Upvote action");
        };
        assert_eq!(*m6id, target);
        assert_eq!(downvoted_others, &vec![other_positive]);

        diff.apply(&mut rwtxn, &dbs.active_sidechains, 1)
            .into_diagnostic()?;
        let pending = dbs
            .active_sidechains
            .pending_m6ids()
            .get(&rwtxn, &sc)
            .into_diagnostic()?;
        assert_eq!(pending[&target].vote_count, 3);
        assert_eq!(pending[&other_positive].vote_count, 2);
        assert_eq!(pending[&other_zero].vote_count, 0);

        // Undo restores original state.
        diff.undo(&mut rwtxn, &dbs.active_sidechains)
            .into_diagnostic()?;
        let pending = dbs
            .active_sidechains
            .pending_m6ids()
            .get(&rwtxn, &sc)
            .into_diagnostic()?;
        assert_eq!(pending[&target].vote_count, 2);
        assert_eq!(pending[&other_positive].vote_count, 3);
        assert_eq!(pending[&other_zero].vote_count, 0);
        Ok(())
    }

    // ── handle_m4_ack_bundles: TwoBytes encoding rules ──

    /// BIP 300 M4 encoding: a `VOTES_TWO_BYTE` M4 with no element `> 253`
    /// MUST invalidate the block — the encoding wastes a byte per element
    /// when every value would fit in one byte.
    #[test]
    fn handle_m4_ack_bundles_rejects_twobytes_within_byte_range() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let sc = SidechainNumber(1);
        dbs.active_sidechains
            .put_sidechain(&mut rwtxn, &sc, &test_sidechain(1, 0))
            .into_diagnostic()?;

        // The single vote fits in one byte (≤ 253), so TwoBytes is wasteful.
        let m4 = M4AckBundles::TwoBytes { upvotes: vec![253] };
        let err = test_handler(&dbs)
            .handle_m4_ack_bundles(&rwtxn, BlockHash::all_zeros(), &m4)
            .expect_err("TwoBytes with all elements ≤ 253 must be rejected");
        assert!(matches!(
            err,
            error::HandleM4AckBundles::TwoBytesWithinByteRange
        ));
        assert!(
            !err.is_fatal(),
            "encoding rule violation is block-invalidation, not DB corruption"
        );
        Ok(())
    }

    /// Sanity: TwoBytes that does need two-byte encoding (e.g. a vote > 253,
    /// or a TwoByte-only sentinel like ALARM_TWO_BYTES = 0xFFFE) is allowed.
    #[test]
    fn handle_m4_ack_bundles_accepts_twobytes_with_value_over_byte_range() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let sc = SidechainNumber(1);
        dbs.active_sidechains
            .put_sidechain(&mut rwtxn, &sc, &test_sidechain(1, 0))
            .into_diagnostic()?;

        let m4 = M4AckBundles::TwoBytes {
            upvotes: vec![M4AckBundles::ALARM_TWO_BYTES],
        };
        let _diff = test_handler(&dbs)
            .handle_m4_ack_bundles(&rwtxn, BlockHash::all_zeros(), &m4)
            .into_diagnostic()?;
        Ok(())
    }

    // ── handle_m4_ack_bundles: LeadingBy50 & RepeatPrevious ──

    fn set_vote_count(rwtxn: &mut RwTxn, dbs: &Dbs, sc: &SidechainNumber, m6id: M6id, count: u16) {
        dbs.active_sidechains
            .with_pending_withdrawal_entry(rwtxn, sc, m6id, |entry| {
                let ordermap::map::Entry::Occupied(mut e) = entry else {
                    panic!("expected pending m6id");
                };
                e.get_mut().vote_count = count;
            })
            .unwrap();
    }

    #[test]
    fn handle_m4_leading_by_50_margin() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let sc = SidechainNumber(1);
        dbs.active_sidechains
            .put_sidechain(&mut rwtxn, &sc, &test_sidechain(1, 0))
            .into_diagnostic()?;

        let leader = test_m6id(0xAA);
        let rival = test_m6id(0xBB);
        dbs.active_sidechains
            .put_pending_m6id(&mut rwtxn, &sc, leader, 0)
            .into_diagnostic()?;
        dbs.active_sidechains
            .put_pending_m6id(&mut rwtxn, &sc, rival, 0)
            .into_diagnostic()?;

        // Lead of 49 → no upvote
        set_vote_count(&mut rwtxn, &dbs, &sc, leader, 60);
        set_vote_count(&mut rwtxn, &dbs, &sc, rival, 11);
        let diff = test_handler(&dbs)
            .handle_m4_ack_bundles(&rwtxn, BlockHash::all_zeros(), &M4AckBundles::LeadingBy50)
            .into_diagnostic()?;
        assert!(diff.0.is_empty(), "lead of 49 should not trigger upvote");

        // Lead of exactly 50 → upvote leader
        set_vote_count(&mut rwtxn, &dbs, &sc, rival, 10);
        let diff = test_handler(&dbs)
            .handle_m4_ack_bundles(&rwtxn, BlockHash::all_zeros(), &M4AckBundles::LeadingBy50)
            .into_diagnostic()?;
        assert!(matches!(
            diff.0.get(&sc),
            Some(diff::AckBundleAction::Upvote { m6id, .. }) if *m6id == leader
        ));

        // Tied top → no upvote
        set_vote_count(&mut rwtxn, &dbs, &sc, leader, 100);
        set_vote_count(&mut rwtxn, &dbs, &sc, rival, 100);
        let diff = test_handler(&dbs)
            .handle_m4_ack_bundles(&rwtxn, BlockHash::all_zeros(), &M4AckBundles::LeadingBy50)
            .into_diagnostic()?;
        assert!(diff.0.is_empty(), "ties should not trigger upvote");
        Ok(())
    }

    #[test]
    fn handle_m4_leading_by_50_single_bundle_treats_no_rival_as_zero() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let sc = SidechainNumber(1);
        dbs.active_sidechains
            .put_sidechain(&mut rwtxn, &sc, &test_sidechain(1, 0))
            .into_diagnostic()?;
        let m6id = test_m6id(0xAA);
        dbs.active_sidechains
            .put_pending_m6id(&mut rwtxn, &sc, m6id, 0)
            .into_diagnostic()?;

        // vote_count 49, no rival → lead = 49 (vs implicit 0) → no upvote
        set_vote_count(&mut rwtxn, &dbs, &sc, m6id, 49);
        let diff = test_handler(&dbs)
            .handle_m4_ack_bundles(&rwtxn, BlockHash::all_zeros(), &M4AckBundles::LeadingBy50)
            .into_diagnostic()?;
        assert!(diff.0.is_empty());

        // vote_count 50 → lead = 50 → upvote
        set_vote_count(&mut rwtxn, &dbs, &sc, m6id, 50);
        let diff = test_handler(&dbs)
            .handle_m4_ack_bundles(&rwtxn, BlockHash::all_zeros(), &M4AckBundles::LeadingBy50)
            .into_diagnostic()?;
        assert!(matches!(
            diff.0.get(&sc),
            Some(diff::AckBundleAction::Upvote { .. })
        ));
        Ok(())
    }

    #[test]
    fn handle_m4_leading_by_50_empty_cases() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;

        // No sidechains → empty diff
        let diff = test_handler(&dbs)
            .handle_m4_ack_bundles(&rwtxn, BlockHash::all_zeros(), &M4AckBundles::LeadingBy50)
            .into_diagnostic()?;
        assert!(diff.0.is_empty());

        // Active sidechain but no pending m6ids → no action for that sidechain
        let sc = SidechainNumber(1);
        dbs.active_sidechains
            .put_sidechain(&mut rwtxn, &sc, &test_sidechain(1, 0))
            .into_diagnostic()?;
        let diff = test_handler(&dbs)
            .handle_m4_ack_bundles(&rwtxn, BlockHash::all_zeros(), &M4AckBundles::LeadingBy50)
            .into_diagnostic()?;
        assert!(diff.0.is_empty());
        Ok(())
    }

    #[test]
    fn handle_m4_leading_by_50_skips_saturated_leader() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let sc = SidechainNumber(1);
        dbs.active_sidechains
            .put_sidechain(&mut rwtxn, &sc, &test_sidechain(1, 0))
            .into_diagnostic()?;
        let m6id = test_m6id(0xAA);
        dbs.active_sidechains
            .put_pending_m6id(&mut rwtxn, &sc, m6id, 0)
            .into_diagnostic()?;
        set_vote_count(&mut rwtxn, &dbs, &sc, m6id, u16::MAX);

        // Leader already at u16::MAX → no further upvote
        let diff = test_handler(&dbs)
            .handle_m4_ack_bundles(&rwtxn, BlockHash::all_zeros(), &M4AckBundles::LeadingBy50)
            .into_diagnostic()?;
        assert!(diff.0.is_empty());
        Ok(())
    }

    // ── handle_m4_ack_bundles: RepeatPrevious (end-to-end) ──

    /// Store a fabricated block diff (with `prev_blockhash` as its parent)
    /// so RepeatPrevious resolution can find it. Returns the resulting block
    /// hash, since it's derived from the header contents.
    fn store_block_diff(
        rwtxn: &mut RwTxn,
        dbs: &Dbs,
        prev_blockhash: BlockHash,
        ack_bundles: Option<diff::AckBundles>,
    ) -> BlockHash {
        let header = test_block_header(prev_blockhash);
        let block_hash = header.block_hash();
        dbs.block_hashes.put_headers(rwtxn, &[(header, 0)]).unwrap();

        let msgs: Vec<diff::CoinbaseMsg> = ack_bundles
            .map(|ab| vec![diff::CoinbaseMsg::AckBundles(ab)])
            .unwrap_or_default();
        let block_diff = diff::Block {
            coinbase: diff::Coinbase {
                msgs,
                failed_proposals: diff::FailedProposals(HashMap::new()),
                failed_m6ids: diff::FailedM6ids(HashMap::new()),
            },
            txs: Vec::new(),
        };
        let block_info = BlockInfo {
            bmm_commitments: LinkedHashMap::new(),
            coinbase_txid: Txid::all_zeros(),
            events: Vec::new(),
        };
        dbs.block_hashes
            .put_block_info(rwtxn, &block_hash, &block_info, &block_diff)
            .unwrap();
        block_hash
    }

    #[test]
    fn handle_m4_repeat_previous_copies_prior_upvote() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let sc = SidechainNumber(1);
        dbs.active_sidechains
            .put_sidechain(&mut rwtxn, &sc, &test_sidechain(1, 0))
            .into_diagnostic()?;
        let m6id = test_m6id(0xAA);
        dbs.active_sidechains
            .put_pending_m6id(&mut rwtxn, &sc, m6id, 0)
            .into_diagnostic()?;

        // Store a previous block whose AckBundles upvoted m6id
        let prior = diff::AckBundles({
            let mut map = HashMap::new();
            map.insert(
                sc,
                diff::AckBundleAction::Upvote {
                    m6id,
                    downvoted_others: Vec::new(),
                },
            );
            map
        });
        let prev_hash = store_block_diff(&mut rwtxn, &dbs, BlockHash::all_zeros(), Some(prior));

        let diff = test_handler(&dbs)
            .handle_m4_ack_bundles(&rwtxn, prev_hash, &M4AckBundles::RepeatPrevious)
            .into_diagnostic()?;
        assert!(matches!(
            diff.0.get(&sc),
            Some(diff::AckBundleAction::Upvote { m6id: m, .. }) if *m == m6id
        ));
        Ok(())
    }

    #[test]
    fn handle_m4_repeat_previous_empty_when_no_prior_m4() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;

        // Previous block has no AckBundles stored
        let prev_hash = store_block_diff(&mut rwtxn, &dbs, BlockHash::all_zeros(), None);

        let diff = test_handler(&dbs)
            .handle_m4_ack_bundles(&rwtxn, prev_hash, &M4AckBundles::RepeatPrevious)
            .into_diagnostic()?;
        assert!(
            diff.0.is_empty(),
            "no prior M4 (or all-abstain) should resolve to empty diff"
        );
        Ok(())
    }

    #[test]
    fn handle_m4_repeat_previous_rejects_when_bundle_gone() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let sc = SidechainNumber(1);
        dbs.active_sidechains
            .put_sidechain(&mut rwtxn, &sc, &test_sidechain(1, 0))
            .into_diagnostic()?;
        // Note: m6id is *not* put as pending — simulating the case where the
        // bundle was executed/failed between blocks.
        let missing_m6id = test_m6id(0xCC);

        let prior = diff::AckBundles({
            let mut map = HashMap::new();
            map.insert(
                sc,
                diff::AckBundleAction::Upvote {
                    m6id: missing_m6id,
                    downvoted_others: Vec::new(),
                },
            );
            map
        });
        let prev_hash = store_block_diff(&mut rwtxn, &dbs, BlockHash::all_zeros(), Some(prior));

        let err = test_handler(&dbs)
            .handle_m4_ack_bundles(&rwtxn, prev_hash, &M4AckBundles::RepeatPrevious)
            .expect_err("missing bundle must error");
        assert!(matches!(
            err,
            error::HandleM4AckBundles::RepeatPreviousUpvotesMissingBundle { .. }
        ));
        assert!(!err.is_fatal(),);
        Ok(())
    }

    /// regression guard: RepeatPrevious must **not** clone
    /// `downvoted_others` verbatim from the prior block's diff. That set is
    /// "bundles with vote_count > 0 at apply time" — recorded for the prior
    /// block's state. If a rival bundle was decremented to 0 by the prior
    /// block, apply's `saturating_sub(1)` would no-op at 0 while undo's
    /// `+= 1` would spuriously lift it to 1, breaking apply/undo symmetry.
    ///
    /// The fix: recompute `downvoted_others` against current pending state.
    /// This test verifies the rebuilt set matches the current state, not
    /// the stored set.
    #[test]
    fn handle_m4_repeat_previous_rebuilds_downvoted_others_against_current_state() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let sc = SidechainNumber(1);
        dbs.active_sidechains
            .put_sidechain(&mut rwtxn, &sc, &test_sidechain(1, 0))
            .into_diagnostic()?;
        let target = test_m6id(0xAA);
        let rival_now_zero = test_m6id(0xBB);
        let rival_still_positive = test_m6id(0xCC);
        for m6id in [target, rival_now_zero, rival_still_positive] {
            dbs.active_sidechains
                .put_pending_m6id(&mut rwtxn, &sc, m6id, 0)
                .into_diagnostic()?;
        }
        // Current state: target=1, rival_now_zero=0, rival_still_positive=2.
        set_vote_count(&mut rwtxn, &dbs, &sc, target, 1);
        set_vote_count(&mut rwtxn, &dbs, &sc, rival_now_zero, 0);
        set_vote_count(&mut rwtxn, &dbs, &sc, rival_still_positive, 2);

        // Prior diff listed BOTH rivals in downvoted_others (simulating the
        // state before rival_now_zero was decremented to 0).
        let stale = diff::AckBundles({
            let mut map = HashMap::new();
            map.insert(
                sc,
                diff::AckBundleAction::Upvote {
                    m6id: target,
                    downvoted_others: vec![rival_now_zero, rival_still_positive],
                },
            );
            map
        });
        let prev_hash = store_block_diff(&mut rwtxn, &dbs, BlockHash::all_zeros(), Some(stale));

        let rebuilt = test_handler(&dbs)
            .handle_m4_ack_bundles(&rwtxn, prev_hash, &M4AckBundles::RepeatPrevious)
            .into_diagnostic()?;
        let Some(diff::AckBundleAction::Upvote {
            m6id,
            downvoted_others,
        }) = rebuilt.0.get(&sc)
        else {
            bail!("expected Upvote action");
        };
        assert_eq!(*m6id, target);
        assert_eq!(
            downvoted_others,
            &vec![rival_still_positive],
            "stale rival_now_zero must be excluded; only currently-positive rivals survive"
        );
        Ok(())
    }

    /// regression guard: prior block's Alarm
    /// recorded `positive_votes_proposals` matching its pre-apply state.
    /// On the current block's RepeatPrevious, the set must be recomputed
    /// against current pending, otherwise Alarm apply/undo desynchronizes
    /// (apply no-ops on 0-voted bundles, undo bumps them to 1).
    #[test]
    fn handle_m4_repeat_previous_rebuilds_positive_votes_against_current_state() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let sc = SidechainNumber(1);
        dbs.active_sidechains
            .put_sidechain(&mut rwtxn, &sc, &test_sidechain(1, 0))
            .into_diagnostic()?;
        let a = test_m6id(0xAA);
        let b = test_m6id(0xBB);
        for m6id in [a, b] {
            dbs.active_sidechains
                .put_pending_m6id(&mut rwtxn, &sc, m6id, 0)
                .into_diagnostic()?;
        }
        // Current state: a=0, b=3. Stale prior set claimed both were positive.
        set_vote_count(&mut rwtxn, &dbs, &sc, a, 0);
        set_vote_count(&mut rwtxn, &dbs, &sc, b, 3);
        let stale = diff::AckBundles({
            let mut map = HashMap::new();
            map.insert(
                sc,
                diff::AckBundleAction::Alarm {
                    positive_votes_proposals: HashSet::from([a, b]),
                },
            );
            map
        });
        let prev_hash = store_block_diff(&mut rwtxn, &dbs, BlockHash::all_zeros(), Some(stale));

        let rebuilt = test_handler(&dbs)
            .handle_m4_ack_bundles(&rwtxn, prev_hash, &M4AckBundles::RepeatPrevious)
            .into_diagnostic()?;
        let Some(diff::AckBundleAction::Alarm {
            positive_votes_proposals,
        }) = rebuilt.0.get(&sc)
        else {
            bail!("expected Alarm action");
        };
        assert_eq!(
            positive_votes_proposals,
            &HashSet::from([b]),
            "stale zero-voted bundle must be excluded from rebuilt Alarm set"
        );
        Ok(())
    }

    /// Liveness guard: the first block post-genesis has
    /// `prev_block_hash == all_zeros()` with no stored diff. A miner who
    /// uses `RepeatPrevious` there is spec-compliant, so the handler must
    /// return an empty diff, not panic.
    #[test]
    fn handle_m4_repeat_previous_genesis_parent_yields_empty_diff() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let rwtxn = dbs.write_txn().into_diagnostic()?;

        let diff = test_handler(&dbs)
            .handle_m4_ack_bundles(
                &rwtxn,
                BlockHash::all_zeros(),
                &M4AckBundles::RepeatPrevious,
            )
            .into_diagnostic()?;
        assert!(diff.0.is_empty());
        Ok(())
    }

    /// Overflow guard: if the prior block upvoted a bundle that is now at
    /// u16::MAX in the current pending state, RepeatPrevious must skip the
    /// upvote instead of overflowing at apply time (`vote_count += 1`).
    /// Mirrors `handle_m4_votes`'s `if info.vote_count < u16::MAX` gate.
    #[test]
    fn handle_m4_repeat_previous_skips_saturated_target() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let sc = SidechainNumber(1);
        dbs.active_sidechains
            .put_sidechain(&mut rwtxn, &sc, &test_sidechain(1, 0))
            .into_diagnostic()?;
        let m6id = test_m6id(0xAA);
        dbs.active_sidechains
            .put_pending_m6id(&mut rwtxn, &sc, m6id, 0)
            .into_diagnostic()?;
        set_vote_count(&mut rwtxn, &dbs, &sc, m6id, u16::MAX);

        let prior = diff::AckBundles({
            let mut map = HashMap::new();
            map.insert(
                sc,
                diff::AckBundleAction::Upvote {
                    m6id,
                    downvoted_others: Vec::new(),
                },
            );
            map
        });
        let prev_hash = store_block_diff(&mut rwtxn, &dbs, BlockHash::all_zeros(), Some(prior));

        let diff = test_handler(&dbs)
            .handle_m4_ack_bundles(&rwtxn, prev_hash, &M4AckBundles::RepeatPrevious)
            .into_diagnostic()?;
        assert!(
            diff.0.is_empty(),
            "saturated target must not produce an overflow-prone Upvote diff"
        );
        Ok(())
    }

    /// Two bundles share second place while a third leads by 50 over them
    /// — spec threshold is "leading by ≥ 50 vs next-highest rival", so the
    /// leader wins regardless of ties behind it.
    #[test]
    fn handle_m4_leading_by_50_leader_wins_over_tied_second_place() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;
        let sc = SidechainNumber(1);
        dbs.active_sidechains
            .put_sidechain(&mut rwtxn, &sc, &test_sidechain(1, 0))
            .into_diagnostic()?;
        let leader = test_m6id(0xAA);
        let tied_a = test_m6id(0xBB);
        let tied_b = test_m6id(0xCC);
        for m in [leader, tied_a, tied_b] {
            dbs.active_sidechains
                .put_pending_m6id(&mut rwtxn, &sc, m, 0)
                .into_diagnostic()?;
        }
        set_vote_count(&mut rwtxn, &dbs, &sc, leader, 120);
        set_vote_count(&mut rwtxn, &dbs, &sc, tied_a, 60);
        set_vote_count(&mut rwtxn, &dbs, &sc, tied_b, 60);

        let diff = test_handler(&dbs)
            .handle_m4_ack_bundles(&rwtxn, BlockHash::all_zeros(), &M4AckBundles::LeadingBy50)
            .into_diagnostic()?;
        assert!(matches!(
            diff.0.get(&sc),
            Some(diff::AckBundleAction::Upvote { m6id, .. }) if *m6id == leader
        ));
        Ok(())
    }
}
