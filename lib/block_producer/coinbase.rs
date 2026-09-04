use std::collections::{HashMap, HashSet};

use bitcoin::{Amount, BlockHash, OutPoint, Transaction, TxOut};
use fallible_iterator::{FallibleIterator as _, IteratorExt as _};

use crate::{
    block_producer::{BlockProducer, BundleProposals, error},
    messages::{CoinbaseBuilder, CoinbaseMessage, CoinbaseMessages, M4AckBundles},
    types::{
        AmountUnderflowError, BlindedM6, Ctip, Sidechain, SidechainAck, SidechainNumber,
        SidechainProposal, SidechainProposalId, Thresholds,
    },
    validator::PendingM6ids,
};

impl BlockProducer {
    /// Bundle proposals we've stored, joined with the validator's view of each
    /// one. Proposals for a sidechain that isn't active are dropped: per BIP300
    /// M3, a bundle proposed for an inactive slot is just an ordinary script (a
    /// no-op), so there is nothing to gain by proposing it. Those can sneak in by
    /// activating a sidechain and then reorging it out of existence.
    pub(crate) async fn get_bundle_proposals(
        &self,
    ) -> Result<HashMap<SidechainNumber, BundleProposals>, error::GetBundleProposals> {
        let bundle_proposals = self.db().get_bundle_proposals().await?;
        let active_sidechain_numbers: HashSet<SidechainNumber> = self
            .validator()
            .get_active_sidechains()?
            .into_iter()
            .map(|sidechain| sidechain.proposal.sidechain_number)
            .collect();
        let res = bundle_proposals
            .into_iter()
            .map(Ok::<_, error::GetBundleProposals>)
            .transpose_into_fallible()
            .filter_map(|(sidechain_id, m6ids)| {
                if !active_sidechain_numbers.contains(&sidechain_id) {
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
    /// `next_height` is the height of the block under construction, from
    /// which each proposal's age is computed, mirroring the validator's M2
    /// handling at connect time.
    fn m2s_may_activate_unused_slot(
        &self,
        coinbase_messages: &CoinbaseMessages,
        active_sidechains: &[Sidechain],
        next_height: u32,
    ) -> Result<bool, crate::validator::GetSidechainsError> {
        if coinbase_messages.m2_ack_slots().is_empty() {
            return Ok(false);
        }
        let active_slots: HashSet<SidechainNumber> = active_sidechains
            .iter()
            .map(|sidechain| sidechain.proposal.sidechain_number)
            .collect();

        let mut unused_slot_acks = HashSet::new();
        for (message, _vout) in coinbase_messages.iter() {
            let CoinbaseMessage::M2AckSidechain(m2) = message else {
                continue;
            };
            // Only ACKs for slots that are not already active can grow the
            // active set
            if active_slots.contains(&m2.sidechain_number) {
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
    /// delete, adding an auto-ACK for every pending proposal left without one
    /// when `ack_all_proposals` is set. Stale rows are dropped before that,
    /// so a stale ACK for a slot cannot suppress the auto-ACK for the
    /// replacement proposal in the same slot.
    fn select_sidechain_acks(
        stored_acks: Vec<SidechainAck>,
        pending_proposals: &HashMap<SidechainNumber, SidechainProposal>,
        ack_all_proposals: bool,
    ) -> (Vec<SidechainAck>, Vec<SidechainAck>) {
        let (mut acks, stale_acks): (Vec<_>, Vec<_>) = stored_acks
            .into_iter()
            .partition(|ack| Self::validate_sidechain_ack(ack, pending_proposals));
        if ack_all_proposals {
            for (sidechain_number, proposal) in pending_proposals {
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
        }
        (acks, stale_acks)
    }

    /// The M4 vote for one sidechain: the position, in the pending set the
    /// validator indexes votes against, of the first bundle this producer both
    /// knows and could actually pay out against `ctip`.
    ///
    /// Voting for index 0 regardless upvotes whichever bundle happens to be
    /// first, including one that [`Self::suffix_txs_for_proposals`] would skip
    /// as unpayable. Since an upvote downvotes every other pending bundle,
    /// that starves a payable withdrawal queued behind an unpayable one into
    /// `Failed`, without the unpayable one ever being paid out either.
    ///
    /// Returns [`M4AckBundles::ABSTAIN_ONE_BYTE`] when no pending bundle
    /// qualifies, including when the one that does sits at an index a one-byte
    /// vote cannot express.
    fn select_m4_upvote(
        pending_m6ids: &PendingM6ids,
        bundle_proposals: Option<&BundleProposals>,
        ctip: Option<Ctip>,
    ) -> u8 {
        // With no treasury to spend, no bundle for this sidechain is payable.
        let Some(ctip) = ctip else {
            return M4AckBundles::ABSTAIN_ONE_BYTE;
        };
        for (index, (m6id, _info)) in pending_m6ids.iter().enumerate() {
            // Only bundles we hold ourselves: without the blinded M6 we can
            // neither tell whether it fits the treasury, nor build the M6 the
            // upvote is meant to lead to.
            let proposed = bundle_proposals
                .into_iter()
                .flatten()
                .find(|(proposed_m6id, _, _)| proposed_m6id == m6id);
            let Some((_, blinded_m6, _)) = proposed else {
                continue;
            };
            // The same predicate [`Self::suffix_txs_for_proposals`] applies:
            // a bundle spending more than the treasury holds can never be
            // included, so upvoting it only downvotes the ones that can.
            let (fee, payout) = (*blinded_m6.fee(), *blinded_m6.payout());
            if Self::new_treasury_value(ctip.value, fee, payout).is_err() {
                continue;
            }
            // 0xFE and 0xFF are the one-byte ALARM and ABSTAIN sentinels, so a
            // bundle from there on cannot be voted for -- and every bundle
            // after it sits at a higher index still.
            return match u8::try_from(index) {
                Ok(index) if index < M4AckBundles::ALARM_ONE_BYTE => index,
                _ => M4AckBundles::ABSTAIN_ONE_BYTE,
            };
        }
        M4AckBundles::ABSTAIN_ONE_BYTE
    }

    /// Extend coinbase txouts for a new block with our drivechain messages:
    /// M1 (propose), M2 (ack), M3 (bundle propose) and M4 (bundle votes).
    pub(crate) async fn extend_coinbase_txouts(
        &self,
        ack_all_proposals: bool,
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
            ack_all_proposals,
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

        if ack_all_proposals && !active_sidechain_proposals.is_empty() {
            tracing::info!(
                "Handle sidechain ACK: acking all sidechains regardless of what DB says"
            );
        }
        let (sidechain_acks, stale_acks) = Self::select_sidechain_acks(
            stored_acks,
            &active_sidechain_proposals,
            ack_all_proposals,
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

        let bundle_proposals = self.get_bundle_proposals().await?;
        for (sidechain_id, m6ids) in &bundle_proposals {
            for (m6id, _blinded_m6, m6id_info) in m6ids {
                if m6id_info.is_none() {
                    coinbase_builder.propose_bundle(*sidechain_id, *m6id)?;
                }
            }
        }
        // One read of the active set, shared by the activation check and the
        // M4 below, so that the M4 is sized against the same active set the
        // check ran against.
        let active_sidechains = self.validator().get_active_sidechains()?;
        // Ack bundles
        if ack_all_proposals
            && !self.m2s_may_activate_unused_slot(
                coinbase_builder.messages(),
                &active_sidechains,
                next_height,
            )?
        {
            let upvotes = active_sidechains
                .iter()
                .map(|sidechain| {
                    let sidechain_number = sidechain.proposal.sidechain_number;
                    let pending_m6ids = self
                        .validator()
                        .get_pending_withdrawals(&sidechain_number)?;
                    let ctip = self.validator().try_get_ctip(sidechain_number)?;
                    Ok(Self::select_m4_upvote(
                        &pending_m6ids,
                        bundle_proposals.get(&sidechain_number),
                        ctip,
                    ))
                })
                .collect::<Result<_, error::GenerateCoinbaseTxouts>>()?;
            coinbase_builder.ack_bundles(M4AckBundles::OneByte { upvotes })?;
        }
        let () = coinbase_builder.build()?;
        Ok(())
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
        let bundle_proposals = self.get_bundle_proposals().await?;
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
    use std::{borrow::Cow, collections::HashMap};

    use bitcoin::{
        Amount, OutPoint, ScriptBuf, Transaction, TxOut, Txid, absolute::LockTime,
        hashes::Hash as _, opcodes::all::OP_RETURN, script::Builder, transaction::Version,
    };

    use crate::{
        block_producer::{BlockProducer, BundleProposals},
        messages::M4AckBundles,
        types::{
            AmountUnderflowError, BlindedM6, Ctip, PendingM6idInfo, SidechainAck,
            SidechainDescription, SidechainNumber, SidechainProposal, Thresholds,
            op_drivechain_script,
        },
        validator::PendingM6ids,
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

    /// The validator's pending set for `bundles`, in the order it indexes M4
    /// votes against.
    fn pending_m6ids(bundles: &BundleProposals) -> PendingM6ids {
        let mut pending = PendingM6ids::new();
        for (m6id, _blinded_m6, info) in bundles {
            pending.insert(*m6id, info.expect("test fixture is a pending bundle"));
        }
        pending
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

    /// The ordinary case: the bundle at index 0 is ours and payable, so it
    /// takes the vote.
    #[test]
    fn m4_upvotes_the_first_payable_known_bundle() {
        let bundles = approved_bundle_paying(PAYOUT);
        let pending = pending_m6ids(&bundles);

        let upvote = BlockProducer::select_m4_upvote(&pending, Some(&bundles), Some(ctip(0)));

        assert_eq!(upvote, 0);
    }

    /// A bundle that cannot be paid out must not take the vote. An upvote
    /// downvotes every *other* pending bundle, so voting for index 0 blindly
    /// starves the payable withdrawal queued behind an unpayable one into
    /// `Failed`, while the unpayable one is never paid out either.
    #[test]
    fn m4_upvote_skips_an_unpayable_bundle_ahead_of_a_payable_one() {
        let mut bundles = approved_bundle_paying(TREASURY_VALUE);
        bundles.extend(approved_bundle_paying(PAYOUT));
        let pending = pending_m6ids(&bundles);

        let upvote = BlockProducer::select_m4_upvote(&pending, Some(&bundles), Some(ctip(0)));

        assert_eq!(upvote, 1);
    }

    /// A bundle we don't hold ourselves can be neither checked for payability
    /// nor turned into the M6 the upvote is meant to lead to, so it is not
    /// voted for.
    #[test]
    fn m4_abstains_when_the_pending_bundle_is_unknown_to_us() {
        let pending = pending_m6ids(&approved_bundle_paying(PAYOUT));
        let unknown_to_us = BundleProposals::new();

        assert_eq!(
            BlockProducer::select_m4_upvote(&pending, None, Some(ctip(0))),
            M4AckBundles::ABSTAIN_ONE_BYTE
        );
        assert_eq!(
            BlockProducer::select_m4_upvote(&pending, Some(&unknown_to_us), Some(ctip(0))),
            M4AckBundles::ABSTAIN_ONE_BYTE
        );
    }

    /// Nothing payable pending -- whether because the only bundle overdraws
    /// the treasury, or because there is no treasury at all -- is an
    /// abstention, not a vote for a bundle that can never be included.
    #[test]
    fn m4_abstains_when_no_pending_bundle_is_payable() {
        let overdrawing = approved_bundle_paying(TREASURY_VALUE);
        let pending = pending_m6ids(&overdrawing);
        assert_eq!(
            BlockProducer::select_m4_upvote(&pending, Some(&overdrawing), Some(ctip(0))),
            M4AckBundles::ABSTAIN_ONE_BYTE
        );

        // The same for a sidechain with no treasury: nothing to spend.
        let payable = approved_bundle_paying(PAYOUT);
        let pending = pending_m6ids(&payable);
        assert_eq!(
            BlockProducer::select_m4_upvote(&pending, Some(&payable), None),
            M4AckBundles::ABSTAIN_ONE_BYTE
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

        let (acks, stale_acks) = BlockProducer::select_sidechain_acks(vec![stale], &pending, true);

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

        let (acks, stale_acks) = BlockProducer::select_sidechain_acks(vec![ack], &pending, true);

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

        let (acks, stale_acks) =
            BlockProducer::select_sidechain_acks(vec![stale], &HashMap::new(), false);

        assert!(acks.is_empty());
        assert_eq!(stale_acks.len(), 1);
    }
}
