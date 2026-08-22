use std::collections::{HashMap, HashSet};

use bitcoin::{Amount, BlockHash, OutPoint, Transaction, TxOut};
use fallible_iterator::{FallibleIterator as _, IteratorExt as _};

use crate::{
    block_producer::{BlockProducer, BundleProposals, error},
    messages::{CoinbaseBuilder, CoinbaseMessage, CoinbaseMessages, M4AckBundles},
    types::{
        AckAllProposalsPolicy, AmountUnderflowError, BlindedM6, Ctip, M6id, Sidechain,
        SidechainAck, SidechainNumber, SidechainProposal, SidechainProposalId, Thresholds,
        WithdrawalBundlePolicy, WithdrawalBundleVote,
    },
};

/// The M4 votes to cast, paired with the explicit bundle ACKs to delete.
type SelectedBundleVotes = (Vec<WithdrawalBundleVote>, Vec<(SidechainNumber, M6id)>);

impl BlockProducer {
    /// Bundle proposals we've stored, joined with the validator's view of each
    /// one. Proposals for a sidechain that isn't active -- whose slot is not in
    /// `used_slots` -- are dropped: per BIP300 M3, a bundle proposed for an
    /// inactive slot is just an ordinary script (a no-op), so there is nothing
    /// to gain by proposing it. Those can sneak in by activating a sidechain
    /// and then reorging it out of existence.
    pub(crate) async fn get_bundle_proposals(
        &self,
        used_slots: &HashSet<SidechainNumber>,
    ) -> Result<HashMap<SidechainNumber, BundleProposals>, error::GetBundleProposals> {
        let bundle_proposals = self.db().get_bundle_proposals().await?;
        let res = bundle_proposals
            .into_iter()
            .map(Ok::<_, error::GetBundleProposals>)
            .transpose_into_fallible()
            .filter_map(|(sidechain_id, m6ids)| {
                if !used_slots.contains(&sidechain_id) {
                    return Ok(None);
                }
                let pending_m6ids = self.validator().get_pending_withdrawals(&sidechain_id)?;
                let res: Vec<_> = m6ids
                    .into_iter()
                    .map(|(m6id, blinded_m6)| (m6id, blinded_m6, pending_m6ids.get(&m6id).copied()))
                    .collect();
                if res.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some((sidechain_id, res)))
                }
            })
            .collect()?;
        Ok(res)
    }

    /// The slots an active sidechain currently occupies. A proposal for one of
    /// these would evict the incumbent if it activated, rather than moving into
    /// an empty slot.
    fn used_slots(active_sidechains: &[Sidechain]) -> HashSet<SidechainNumber> {
        active_sidechains
            .iter()
            .map(|sidechain| sidechain.proposal.sidechain_number)
            .collect()
    }

    /// Sidechain proposals from the *validator*: already included in a block, and
    /// therefore votable.
    fn get_active_sidechain_proposals(
        &self,
    ) -> Result<HashMap<SidechainNumber, SidechainProposal>, crate::validator::GetSidechainsError>
    {
        let pending_proposals = self
            .validator()
            .get_sidechains()?
            .into_iter()
            .map(|(_, sidechain)| (sidechain.proposal.sidechain_number, sidechain.proposal))
            .collect();
        Ok(pending_proposals)
    }

    fn validate_sidechain_ack(
        ack: &SidechainAck,
        pending_proposals: &HashMap<SidechainNumber, SidechainProposal>,
    ) -> bool {
        let Some(sidechain_proposal) = pending_proposals.get(&ack.sidechain_number) else {
            tracing::error!(
                "Handle sidechain ACK: could not find proposal: {}",
                ack.sidechain_number
            );
            return false;
        };
        let description_hash = sidechain_proposal.description.sha256d_hash();
        if description_hash == ack.description_hash {
            true
        } else {
            tracing::error!(
                "Handle sidechain ACK: invalid actual hash vs. ACK hash: {} != {}",
                description_hash,
                ack.description_hash,
            );
            false
        }
    }

    /// An M4 carries exactly one vote per active sidechain, as seen *after* the
    /// M2s in the same coinbase have been applied. An ACK that activates an unused slot grows the active
    /// set by one, which would invalidate an M4 sized against the current one.
    /// Returns `true` if any M2 in this coinbase might activate an unused slot,
    /// in which case the M4 must be omitted.
    ///
    /// An M2 for a *used* slot cannot do this: activating it replaces the
    /// incumbent in place, leaving the active set the same size.
    ///
    /// `next_height` is the height of the block under construction, from
    /// which each proposal's age is computed, mirroring the validator's M2
    /// handling at connect time.
    fn m2s_may_activate_unused_slot(
        &self,
        coinbase_messages: &CoinbaseMessages,
        used_slots: &HashSet<SidechainNumber>,
        next_height: u32,
    ) -> Result<bool, crate::validator::GetSidechainsError> {
        if coinbase_messages.m2_ack_slots().is_empty() {
            return Ok(false);
        }

        let mut unused_slot_acks = HashSet::new();
        for (message, _vout) in coinbase_messages.iter() {
            let CoinbaseMessage::M2AckSidechain(m2) = message else {
                continue;
            };
            // Only ACKs for slots that are not already active can grow the
            // active set
            if used_slots.contains(&m2.sidechain_number) {
                continue;
            }
            unused_slot_acks.insert(SidechainProposalId {
                sidechain_number: m2.sidechain_number,
                description_hash: m2.description_hash,
            });
        }
        if unused_slot_acks.is_empty() {
            return Ok(false);
        }
        let thresholds = self.validator().network_params().thresholds;
        let res = self
            .validator()
            .get_sidechains()?
            .into_iter()
            .any(|(proposal_id, sidechain)| {
                let proposal_age = next_height.saturating_sub(sidechain.status.proposal_height);
                unused_slot_acks.contains(&proposal_id)
                    && thresholds.activates_unused_slot(
                        sidechain.status.vote_count.saturating_add(1),
                        proposal_age,
                    )
            });
        Ok(res)
    }

    /// Split the stored ACKs into the ones to emit and the stale ones to
    /// delete, adding an auto-ACK for every pending proposal `policy` covers
    /// and that is left without one. Stale rows are dropped before that, so a
    /// stale ACK for a slot cannot suppress the auto-ACK for the replacement
    /// proposal in the same slot.
    ///
    /// `used_slots` are the slots an active sidechain already occupies: a
    /// proposal for one of those would evict the incumbent if it activated,
    /// which [`AckAllProposalsPolicy::NewSlots`] leaves to an explicit ACK.
    fn select_sidechain_acks(
        stored_acks: Vec<SidechainAck>,
        pending_proposals: &HashMap<SidechainNumber, SidechainProposal>,
        used_slots: &HashSet<SidechainNumber>,
        policy: AckAllProposalsPolicy,
    ) -> (Vec<SidechainAck>, Vec<SidechainAck>) {
        let (mut acks, stale_acks): (Vec<_>, Vec<_>) = stored_acks
            .into_iter()
            .partition(|ack| Self::validate_sidechain_ack(ack, pending_proposals));
        for (sidechain_number, proposal) in pending_proposals {
            if !policy.auto_acks(used_slots.contains(sidechain_number)) {
                continue;
            }
            if acks
                .iter()
                .any(|ack| ack.sidechain_number == *sidechain_number)
            {
                continue;
            }
            tracing::debug!("Handle sidechain ACK: adding 'fake' ACK for {sidechain_number}");
            acks.push(SidechainAck {
                sidechain_number: *sidechain_number,
                description_hash: proposal.description.sha256d_hash(),
            });
        }
        (acks, stale_acks)
    }

    /// Extend coinbase txouts for a new block with our drivechain messages:
    /// M1 (propose), M2 (ack), M3 (bundle propose) and M4 (bundle votes).
    pub(crate) async fn extend_coinbase_txouts(
        &self,
        ack_policy: AckAllProposalsPolicy,
        bundle_policy: WithdrawalBundlePolicy,
        mainchain_tip: BlockHash,
        coinbase_txouts: &mut Vec<TxOut>,
    ) -> Result<(), error::GenerateCoinbaseTxouts> {
        // The height of the block under construction. Below the BIP300
        // activation height the validator ignores every drivechain message,
        // so there is nothing useful to add to the coinbase: in particular,
        // pending sidechain proposals (e.g. rows persisted by a binary
        // predating the RPC-level activation gate) are held back rather than
        // mined into coinbases they can never register from.
        let next_height = self
            .validator()
            .get_header_info(&mainchain_tip)?
            .height
            .saturating_add(1);
        let activation_height = self.validator().network_params().bip300_activation_height;

        if next_height < activation_height {
            tracing::debug!(
                "Skipping drivechain coinbase messages: BIP300 activates at block height \
                 {activation_height} (next block height: {next_height})"
            );
            return Ok(());
        }

        let mut coinbase_builder = CoinbaseBuilder::new(coinbase_txouts)?;
        tracing::debug!(
            ?ack_policy,
            ?bundle_policy,
            %mainchain_tip,
            "Extending coinbase txouts",
        );

        // Pending sidechain proposals from /our/ policy DB.
        let sidechain_proposals =
            self.db()
                .get_our_sidechain_proposals()
                .await
                .inspect_err(|err| {
                    tracing::error!(
                        "Failed to get sidechain proposals: {:#}",
                        crate::errors::ErrorChain::new(err)
                    );
                })?;

        if !sidechain_proposals.is_empty() {
            tracing::debug!(
                "found {} sidechain proposal(s) in our SQLite DB",
                sidechain_proposals.len()
            );
        }

        // Sidechain proposals that already exist in the chain,
        // or will already be proposed in coinbase txouts
        let proposed_sidechains = self
            .validator()
            .get_sidechains()?
            .into_iter()
            .map(|(sidechain_proposal_id, _)| sidechain_proposal_id)
            .chain(coinbase_builder.messages().m1_sidechain_proposal_ids())
            .collect::<HashSet<_>>();

        for sidechain_proposal in sidechain_proposals {
            if !proposed_sidechains.contains(&sidechain_proposal.compute_id()) {
                coinbase_builder.propose_sidechain(sidechain_proposal)?;
            }
        }

        let stored_acks = self.db().get_sidechain_acks().await?;

        // Pending sidechain proposals from the /validator/, i.e. proposals
        // broadcast by (potentially) someone else, and already active.
        let active_sidechain_proposals = self.get_active_sidechain_proposals()?;

        // One read of the active set, shared by the ACK policy, the bundle
        // proposals, the activation check and the M4 below: a slot counts as
        // used consistently throughout, and the M4 is sized against the same
        // active set the check ran against.
        let active_sidechains = self.validator().get_active_sidechains()?;
        let used_slots = Self::used_slots(&active_sidechains);

        if ack_policy.is_automatic() && !active_sidechain_proposals.is_empty() {
            tracing::info!(
                ?ack_policy,
                "Handle sidechain ACK: acking proposals per policy, on top of what the DB says"
            );
        }
        let (sidechain_acks, stale_acks) = Self::select_sidechain_acks(
            stored_acks,
            &active_sidechain_proposals,
            &used_slots,
            ack_policy,
        );
        for stale_ack in stale_acks {
            self.db().delete_sidechain_ack(&stale_ack).await?;
            tracing::info!(
                "Unable to handle sidechain ack, deleted: {}",
                stale_ack.sidechain_number
            );
        }

        for sidechain_ack in sidechain_acks {
            if coinbase_builder
                .messages()
                .m2_ack_slot_vout(&sidechain_ack.sidechain_number)
                .is_some()
            {
                continue;
            }

            tracing::debug!(
                "Adding ACK for sidechain {}",
                sidechain_ack.sidechain_number
            );

            coinbase_builder.ack_sidechain(
                sidechain_ack.sidechain_number,
                sidechain_ack.description_hash,
            )?;
        }

        let bundle_proposals = self.get_bundle_proposals(&used_slots).await?;
        for (sidechain_id, m6ids) in &bundle_proposals {
            for (m6id, _blinded_m6, m6id_info) in m6ids {
                if m6id_info.is_none() {
                    coinbase_builder.propose_bundle(*sidechain_id, *m6id)?;
                }
            }
        }

        // Ack bundles (BIP300 M4), one vote per active sidechain in the same
        // order the validator reads them back.
        let stored_bundle_acks = self.db().get_bundle_acks().await?;
        let (bundle_votes, stale_bundle_acks) = self.select_bundle_votes(
            stored_bundle_acks,
            &bundle_proposals,
            &active_sidechains,
            bundle_policy,
        )?;
        for (sidechain_number, m6id) in stale_bundle_acks {
            self.db().delete_bundle_ack(sidechain_number, m6id).await?;
            tracing::info!(%sidechain_number, %m6id, "dropped an ACK for a bundle that is no longer pending");
        }
        // Only emit an M4 that votes. An abstain-only one changes nothing, and
        // the validator already treats it and an absent M4 identically -- a
        // later block's RepeatPrevious reads both as "no prior M4". And an M4
        // sized against the current active set is invalid in a block whose own
        // M2s grow that set.
        let votes_something = bundle_votes
            .iter()
            .any(|vote| *vote != WithdrawalBundleVote::Abstain);
        if votes_something
            && !self.m2s_may_activate_unused_slot(
                coinbase_builder.messages(),
                &used_slots,
                next_height,
            )?
        {
            coinbase_builder.ack_bundles(M4AckBundles::from_votes(&bundle_votes))?;
        }
        let () = coinbase_builder.build()?;
        Ok(())
    }

    /// One M4 vote per active sidechain, in active-sidechain order, plus the
    /// explicit ACKs to delete: rows naming a bundle that is no longer pending
    /// for its sidechain, which can never be voted on again.
    ///
    /// `bundle_proposals` are the bundles this node stores, which is what
    /// [`WithdrawalBundlePolicy::Known`] means by a bundle being ours.
    fn select_bundle_votes(
        &self,
        stored_bundle_acks: Vec<(SidechainNumber, M6id)>,
        bundle_proposals: &HashMap<SidechainNumber, BundleProposals>,
        active_sidechains: &[Sidechain],
        policy: WithdrawalBundlePolicy,
    ) -> Result<SelectedBundleVotes, crate::validator::GetPendingWithdrawalsError> {
        let mut acked_by_slot: HashMap<SidechainNumber, HashSet<M6id>> = HashMap::new();
        for (sidechain_number, m6id) in &stored_bundle_acks {
            acked_by_slot
                .entry(*sidechain_number)
                .or_default()
                .insert(*m6id);
        }
        let empty = HashSet::new();

        let mut votes = Vec::with_capacity(active_sidechains.len());
        let mut pending_by_slot: HashMap<SidechainNumber, Vec<M6id>> = HashMap::new();
        for sidechain in active_sidechains {
            let slot = sidechain.proposal.sidechain_number;
            let pending: Vec<M6id> = self
                .validator()
                .get_pending_withdrawals(&slot)?
                .keys()
                .copied()
                .collect();
            let ours: HashSet<M6id> = bundle_proposals
                .get(&slot)
                .into_iter()
                .flatten()
                .map(|(m6id, _blinded_m6, _info)| *m6id)
                .collect();
            votes.push(WithdrawalBundleVote::resolve(
                policy,
                &pending,
                acked_by_slot.get(&slot).unwrap_or(&empty),
                &ours,
            ));
            pending_by_slot.insert(slot, pending);
        }

        // An ACK for a slot with no active sidechain is stale too: there is no
        // pending list for it, so `pending_by_slot` has no entry to match.
        let stale = stored_bundle_acks
            .into_iter()
            .filter(|(sidechain_number, m6id)| {
                !pending_by_slot
                    .get(sidechain_number)
                    .is_some_and(|pending| pending.contains(m6id))
            })
            .collect();
        Ok((votes, stale))
    }

    /// Treasury value remaining after a withdrawal bundle spends `fee` and
    /// `payout`, returning an error instead of underflowing when their sum
    /// exceeds the current treasury. Mirrors the checked subtraction in
    /// `BlindedM6::into_m6`.
    fn new_treasury_value(
        value: Amount,
        fee: Amount,
        payout: Amount,
    ) -> Result<Amount, AmountUnderflowError> {
        value
            .checked_sub(fee)
            .and_then(|value| value.checked_sub(payout))
            .ok_or(AmountUnderflowError)
    }

    /// Finalize a blinded M6 against `ctip` and return the transaction together
    /// with its successor CTIP. BIP300 requires the replacement treasury output
    /// at index zero.
    fn finalize_m6(
        sidechain_id: SidechainNumber,
        ctip: Ctip,
        blinded_m6: BlindedM6<'_>,
    ) -> Result<(Transaction, Ctip), AmountUnderflowError> {
        let new_value =
            Self::new_treasury_value(ctip.value, *blinded_m6.fee(), *blinded_m6.payout())?;
        let m6 = blinded_m6.into_m6(sidechain_id, ctip.outpoint, ctip.value)?;
        let successor = Ctip {
            outpoint: OutPoint {
                txid: m6.compute_txid(),
                vout: 0,
            },
            value: new_value,
        };
        Ok((m6, successor))
    }

    /// Generate the M6 suffix txs for a new block. These are fully determined by
    /// the approved bundle and the CTIP: treasury outputs are anyone-can-spend
    /// and consensus-gated, so an M6 is constructed, never signed.
    ///
    /// Fails only while *reading* the bundles to pay out. The payout rules
    /// themselves cannot fail: a bundle that cannot be paid is skipped, so that
    /// one unpayable bundle does not stop the node from producing blocks. See
    /// [`Self::suffix_txs_for_proposals`].
    pub(crate) async fn generate_suffix_txs(
        &self,
        ctips: &HashMap<SidechainNumber, Ctip>,
    ) -> Result<Vec<Transaction>, error::GetBundleProposals> {
        let thresholds = self.validator().network_params().thresholds;
        let used_slots = Self::used_slots(&self.validator().get_active_sidechains()?);
        let bundle_proposals = self.get_bundle_proposals(&used_slots).await?;
        Ok(Self::suffix_txs_for_proposals(
            bundle_proposals,
            ctips,
            &thresholds,
        ))
    }

    /// The M6 suffix implied by `bundle_proposals` and the treasury state the
    /// suffix will be built against. Split out from
    /// [`Self::generate_suffix_txs`] so the payout rules are exercisable without
    /// a validator or a DB.
    ///
    /// Infallible by construction: a bundle that cannot be paid out is skipped,
    /// never fatal. The suffix is the *rest* of a block that is being built, so
    /// refusing to produce one over a single unpayable bundle stops the node
    /// from producing blocks at all -- and only a connected block can retire the
    /// bundle that is blocking it.
    fn suffix_txs_for_proposals(
        bundle_proposals: HashMap<SidechainNumber, BundleProposals>,
        ctips: &HashMap<SidechainNumber, Ctip>,
        thresholds: &Thresholds,
    ) -> Vec<Transaction> {
        let mut res = Vec::new();
        for (sidechain_id, m6ids) in bundle_proposals {
            let mut ctip = None;
            for (m6id, blinded_m6, m6id_info) in m6ids {
                let Some(m6id_info) = m6id_info else { continue };
                if m6id_info.vote_count <= thresholds.withdrawal_bundle_inclusion_threshold {
                    continue;
                }
                let current_ctip = if let Some(ctip) = ctip {
                    ctip
                } else if let Some(ctip) = ctips.get(&sidechain_id) {
                    *ctip
                } else {
                    // An approved bundle for a sidechain with no treasury has
                    // nothing to spend, so it cannot be paid out. Skip the
                    // sidechain and let the bundle age out instead. Failing here
                    // would abort the entire suffix, so one unpayable bundle
                    // would take every other sidechain's payouts -- and this
                    // node's block production -- down with it, and no block
                    // could ever connect to retire the bundle.
                    tracing::warn!(
                        %sidechain_id,
                        %m6id,
                        "skipping an approved withdrawal bundle: sidechain has no CTIP"
                    );
                    break;
                };
                // A bundle that pays out more than the treasury holds can never
                // be included -- the only way finalizing it can fail. Skip just
                // this bundle and let it age out: a smaller one behind it is
                // still payable. (The missing-CTIP arm `break`s instead, but
                // only to warn once per sidechain -- with no treasury nothing
                // behind it could pay out either, so the output is the same.)
                let (fee, payout) = (*blinded_m6.fee(), *blinded_m6.payout());
                let Ok((m6, successor_ctip)) =
                    Self::finalize_m6(sidechain_id, current_ctip, blinded_m6)
                else {
                    tracing::warn!(
                        %sidechain_id,
                        %m6id,
                        %fee,
                        %payout,
                        treasury = %current_ctip.value,
                        "skipping an approved withdrawal bundle: payout and fee exceed the treasury"
                    );
                    continue;
                };
                ctip = Some(successor_ctip);
                res.push(m6);
            }
        }
        res
    }
}

#[cfg(test)]
mod tests {
    use std::{
        borrow::Cow,
        collections::{HashMap, HashSet},
    };

    use bitcoin::{
        Amount, OutPoint, ScriptBuf, Transaction, TxOut, Txid, absolute::LockTime,
        hashes::Hash as _, opcodes::all::OP_RETURN, script::Builder, transaction::Version,
    };

    use crate::{
        block_producer::{BlockProducer, BundleProposals},
        types::{
            AckAllProposalsPolicy, AmountUnderflowError, BlindedM6, Ctip, PendingM6idInfo,
            SidechainAck, SidechainDescription, SidechainNumber, SidechainProposal, Thresholds,
            op_drivechain_script,
        },
    };

    fn test_proposal(sidechain_number: SidechainNumber, description: &[u8]) -> SidechainProposal {
        SidechainProposal {
            sidechain_number,
            description: SidechainDescription(description.to_vec()),
        }
    }

    const THRESHOLDS: Thresholds = Thresholds::SHORT;
    const TREASURY_VALUE: Amount = Amount::from_sat(1_000_000);
    const FEE_SATS: u64 = 1_000;
    const PAYOUT: Amount = Amount::from_sat(50_000);

    /// A single bundle proposal with enough votes to be paid out.
    fn approved_bundle() -> BundleProposals {
        approved_bundle_paying(PAYOUT)
    }

    /// [`approved_bundle`], for a chosen payout. Distinct payouts give distinct
    /// m6ids, so several can be pending for one sidechain at once.
    fn approved_bundle_paying(payout: Amount) -> BundleProposals {
        let fee_output = TxOut {
            value: Amount::ZERO,
            script_pubkey: Builder::new()
                .push_opcode(OP_RETURN)
                .push_slice(FEE_SATS.to_be_bytes())
                .into_script(),
        };
        let payout_output = TxOut {
            value: payout,
            script_pubkey: ScriptBuf::from_bytes([vec![0x00, 0x14], vec![0x11; 20]].concat()),
        };
        let tx = bitcoin::Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: Vec::new(), // a blinded M6 has no inputs
            output: vec![fee_output, payout_output],
        };
        let blinded_m6 = BlindedM6::try_from(Cow::Owned(tx))
            .expect("test fixture is not a valid blinded M6")
            .into_owned();
        let info = PendingM6idInfo {
            vote_count: THRESHOLDS.withdrawal_bundle_inclusion_threshold + 1,
            proposal_height: 1,
        };
        vec![(blinded_m6.compute_m6id(), blinded_m6, Some(info))]
    }

    fn ctip(byte: u8) -> Ctip {
        Ctip {
            outpoint: OutPoint {
                txid: Txid::from_byte_array([byte; 32]),
                vout: 0,
            },
            value: TREASURY_VALUE,
        }
    }

    /// A bundle whose sidechain has no treasury cannot be paid out, but it must
    /// not stop the block: erroring out here aborts the whole suffix, which
    /// wedges block production entirely (and then no block can connect to retire
    /// the offending bundle).
    #[test]
    fn suffix_txs_skip_a_sidechain_without_a_ctip() {
        const NO_CTIP: SidechainNumber = SidechainNumber(0);
        const WITH_CTIP: SidechainNumber = SidechainNumber(1);
        let bundle_proposals =
            HashMap::from_iter([(NO_CTIP, approved_bundle()), (WITH_CTIP, approved_bundle())]);
        let ctips = HashMap::from_iter([(WITH_CTIP, ctip(1))]);

        let suffix_txs =
            BlockProducer::suffix_txs_for_proposals(bundle_proposals, &ctips, &THRESHOLDS);

        // The payable sidechain still gets its M6, spending its own CTIP.
        assert_eq!(suffix_txs.len(), 1);
        assert_eq!(
            suffix_txs[0]
                .input
                .first()
                .expect("an M6 spends the treasury")
                .previous_output,
            ctip(1).outpoint
        );
    }

    /// The same, with nothing left to pay out: an empty suffix, not an error.
    #[test]
    fn suffix_txs_are_empty_when_no_sidechain_has_a_ctip() {
        let bundle_proposals = HashMap::from_iter([(SidechainNumber(0), approved_bundle())]);
        let suffix_txs =
            BlockProducer::suffix_txs_for_proposals(bundle_proposals, &HashMap::new(), &THRESHOLDS);
        assert!(suffix_txs.is_empty());
    }

    /// A bundle whose payout and fee exceed the treasury can never be included
    /// -- an M6 spending more than the CTIP is consensus-invalid -- so it is
    /// skipped *alone*, rather than aborting the suffix: the payable bundles
    /// around it still go through, and the one behind it chains onto the CTIP
    /// the one before it advanced.
    #[test]
    fn suffix_txs_skip_only_the_bundle_exceeding_the_treasury() {
        const SIDECHAIN: SidechainNumber = SidechainNumber(0);
        const SECOND_PAYOUT: Amount = Amount::from_sat(30_000);
        // Payable against the treasury the block starts with, but not against
        // what remains of it once the first bundle is paid: skipping has to be
        // judged against the advanced CTIP, not the initial one.
        let overdrawing_payout = TREASURY_VALUE - PAYOUT;
        let mut bundles = approved_bundle_paying(PAYOUT);
        bundles.extend(approved_bundle_paying(overdrawing_payout));
        bundles.extend(approved_bundle_paying(SECOND_PAYOUT));
        let bundle_proposals = HashMap::from_iter([(SIDECHAIN, bundles)]);
        let ctips = HashMap::from_iter([(SIDECHAIN, ctip(0))]);

        let suffix_txs =
            BlockProducer::suffix_txs_for_proposals(bundle_proposals, &ctips, &THRESHOLDS);

        let [first_m6, second_m6] = suffix_txs.as_slice() else {
            panic!("expected exactly the two payable M6s, got {suffix_txs:?}");
        };
        let fee = Amount::from_sat(FEE_SATS);
        // The first payable bundle spends the initial CTIP and leaves the
        // reduced treasury at vout 0, as BIP300 M6 requires.
        assert_eq!(
            first_m6
                .input
                .first()
                .expect("an M6 spends the treasury")
                .previous_output,
            ctip(0).outpoint
        );
        let advanced_treasury = TREASURY_VALUE - PAYOUT - fee;
        assert_eq!(first_m6.output[0].value, advanced_treasury);
        assert!(
            first_m6.output.iter().any(|output| output.value == PAYOUT),
            "the first surviving M6 is not the first payable bundle: {first_m6:?}"
        );
        // The skip leaves the advanced CTIP intact: the second payable bundle
        // chains directly onto the first M6's treasury output, untouched by the
        // overdrawing bundle between them.
        assert_eq!(
            second_m6
                .input
                .first()
                .expect("an M6 spends the treasury")
                .previous_output,
            OutPoint {
                txid: first_m6.compute_txid(),
                vout: 0,
            }
        );
        assert_eq!(
            second_m6.output[0].value,
            advanced_treasury - SECOND_PAYOUT - fee
        );
        assert!(
            second_m6
                .output
                .iter()
                .any(|output| output.value == SECOND_PAYOUT),
            "the second surviving M6 is not the second payable bundle: {second_m6:?}"
        );
    }

    #[test]
    fn new_treasury_value_rejects_bundle_exceeding_treasury() {
        // A withdrawal bundle whose payout plus fee is larger than the current
        // treasury must error rather than underflow the treasury subtraction.
        let value = Amount::from_sat(100_000);
        let fee = Amount::from_sat(1_000);
        let payout = Amount::from_sat(200_000);
        assert!(matches!(
            BlockProducer::new_treasury_value(value, fee, payout),
            Err(AmountUnderflowError)
        ));
    }

    #[test]
    fn new_treasury_value_subtracts_fee_and_payout() {
        let value = Amount::from_sat(100_000);
        let fee = Amount::from_sat(1_000);
        let payout = Amount::from_sat(60_000);
        assert_eq!(
            BlockProducer::new_treasury_value(value, fee, payout).unwrap(),
            Amount::from_sat(39_000)
        );
    }

    fn test_blinded_m6(fee: u64, payout: u64) -> BlindedM6<'static> {
        let tx = Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: Vec::new(),
            output: vec![
                TxOut {
                    value: Amount::ZERO,
                    script_pubkey: Builder::new()
                        .push_opcode(OP_RETURN)
                        .push_slice(fee.to_be_bytes())
                        .into_script(),
                },
                TxOut {
                    value: Amount::from_sat(payout),
                    script_pubkey: ScriptBuf::new(),
                },
            ],
        };
        BlindedM6::try_from(Cow::Owned(tx)).unwrap()
    }

    #[test]
    fn successive_m6s_spend_predecessor_treasury_output_zero() {
        let sidechain_id = SidechainNumber(7);
        let initial_ctip = Ctip {
            outpoint: OutPoint {
                txid: Txid::all_zeros(),
                vout: 3,
            },
            value: Amount::from_sat(200_000),
        };

        let (first_m6, first_successor) =
            BlockProducer::finalize_m6(sidechain_id, initial_ctip, test_blinded_m6(1_000, 50_000))
                .unwrap();
        let first_txid = first_m6.compute_txid();
        assert_eq!(
            first_m6.output[0].script_pubkey,
            op_drivechain_script(sidechain_id)
        );
        assert_eq!(
            first_successor.outpoint,
            OutPoint {
                txid: first_txid,
                vout: 0
            }
        );
        assert_eq!(first_successor.value, Amount::from_sat(149_000));

        let (second_m6, second_successor) = BlockProducer::finalize_m6(
            sidechain_id,
            first_successor,
            test_blinded_m6(2_000, 25_000),
        )
        .unwrap();
        assert_eq!(second_m6.input[0].previous_output, first_successor.outpoint);
        assert_eq!(
            second_successor.outpoint,
            OutPoint {
                txid: second_m6.compute_txid(),
                vout: 0,
            }
        );
        assert_eq!(second_successor.value, Amount::from_sat(122_000));
    }

    #[test]
    fn stale_same_slot_ack_does_not_suppress_replacement_auto_ack() {
        let slot = SidechainNumber(7);
        let replacement = test_proposal(slot, b"new");
        let replacement_hash = replacement.description.sha256d_hash();
        let stale = SidechainAck {
            sidechain_number: slot,
            description_hash: test_proposal(slot, b"old").description.sha256d_hash(),
        };
        let pending = HashMap::from_iter([(slot, replacement)]);

        let (acks, stale_acks) = BlockProducer::select_sidechain_acks(
            vec![stale],
            &pending,
            &HashSet::new(),
            AckAllProposalsPolicy::NewSlots,
        );

        assert_eq!(acks.len(), 1);
        assert_eq!(acks[0].sidechain_number, slot);
        assert_eq!(acks[0].description_hash, replacement_hash);
        assert_eq!(stale_acks.len(), 1);
    }

    #[test]
    fn matching_stored_ack_is_kept_and_not_duplicated() {
        let slot = SidechainNumber(7);
        let proposal = test_proposal(slot, b"desc");
        let description_hash = proposal.description.sha256d_hash();
        let ack = SidechainAck {
            sidechain_number: slot,
            description_hash,
        };
        let pending = HashMap::from_iter([(slot, proposal)]);

        let (acks, stale_acks) = BlockProducer::select_sidechain_acks(
            vec![ack],
            &pending,
            &HashSet::new(),
            AckAllProposalsPolicy::NewSlots,
        );

        assert_eq!(acks.len(), 1);
        assert_eq!(acks[0].description_hash, description_hash);
        assert!(stale_acks.is_empty());
    }

    #[test]
    fn stale_ack_is_deleted_without_auto_ack() {
        let slot = SidechainNumber(7);
        let stale = SidechainAck {
            sidechain_number: slot,
            description_hash: test_proposal(slot, b"old").description.sha256d_hash(),
        };

        let (acks, stale_acks) = BlockProducer::select_sidechain_acks(
            vec![stale],
            &HashMap::new(),
            &HashSet::new(),
            AckAllProposalsPolicy::None,
        );

        assert!(acks.is_empty());
        assert_eq!(stale_acks.len(), 1);
    }

    /// A proposal for an occupied slot would evict the sidechain sitting
    /// there if it activated. `NewSlots` auto-ACKs the empty slot next to it
    /// and leaves the replacement alone; `All` votes for both.
    #[test]
    fn new_slots_policy_auto_acks_new_slots_but_not_replacements() {
        const EMPTY: SidechainNumber = SidechainNumber(3);
        const OCCUPIED: SidechainNumber = SidechainNumber(7);
        let pending = HashMap::from_iter([
            (EMPTY, test_proposal(EMPTY, b"fresh")),
            (OCCUPIED, test_proposal(OCCUPIED, b"replacement")),
        ]);
        let used_slots = HashSet::from_iter([OCCUPIED]);

        let (acks, stale_acks) = BlockProducer::select_sidechain_acks(
            Vec::new(),
            &pending,
            &used_slots,
            AckAllProposalsPolicy::NewSlots,
        );
        assert!(stale_acks.is_empty());
        let slots: Vec<_> = acks.iter().map(|ack| ack.sidechain_number).collect();
        assert_eq!(slots, vec![EMPTY], "a replacement was auto-ACKed");

        let (acks, _) = BlockProducer::select_sidechain_acks(
            Vec::new(),
            &pending,
            &used_slots,
            AckAllProposalsPolicy::All,
        );
        let mut slots: Vec<_> = acks.iter().map(|ack| ack.sidechain_number).collect();
        slots.sort_unstable();
        assert_eq!(slots, vec![EMPTY, OCCUPIED]);
    }

    /// `NewSlots` withholds only the *automatic* vote: an operator who has
    /// deliberately ACKed a replacement still gets their M2 emitted, since
    /// that explicit ACK is the only way to vote for one under this policy.
    #[test]
    fn new_slots_policy_still_emits_an_explicit_replacement_ack() {
        const OCCUPIED: SidechainNumber = SidechainNumber(7);
        let replacement = test_proposal(OCCUPIED, b"replacement");
        let description_hash = replacement.description.sha256d_hash();
        let ack = SidechainAck {
            sidechain_number: OCCUPIED,
            description_hash,
        };
        let pending = HashMap::from_iter([(OCCUPIED, replacement)]);

        let (acks, stale_acks) = BlockProducer::select_sidechain_acks(
            vec![ack],
            &pending,
            &HashSet::from_iter([OCCUPIED]),
            AckAllProposalsPolicy::NewSlots,
        );

        assert!(stale_acks.is_empty());
        assert_eq!(acks.len(), 1);
        assert_eq!(acks[0].sidechain_number, OCCUPIED);
        assert_eq!(acks[0].description_hash, description_hash);
    }

    /// `None` emits nothing automatic, whether or not the slot is occupied.
    #[test]
    fn none_policy_auto_acks_nothing() {
        const EMPTY: SidechainNumber = SidechainNumber(3);
        const OCCUPIED: SidechainNumber = SidechainNumber(7);
        let pending = HashMap::from_iter([
            (EMPTY, test_proposal(EMPTY, b"fresh")),
            (OCCUPIED, test_proposal(OCCUPIED, b"replacement")),
        ]);

        let (acks, stale_acks) = BlockProducer::select_sidechain_acks(
            Vec::new(),
            &pending,
            &HashSet::from_iter([OCCUPIED]),
            AckAllProposalsPolicy::None,
        );

        assert!(acks.is_empty());
        assert!(stale_acks.is_empty());
    }
}
