use std::collections::{HashMap, HashSet};

use bitcoin::{Amount, BlockHash, OutPoint, Transaction, TxOut};
use fallible_iterator::{FallibleIterator as _, IteratorExt as _};

use crate::{
    block_producer::{BlockProducer, BundleProposals, error},
    messages::{CoinbaseBuilder, CoinbaseMessage, CoinbaseMessages, M4AckBundles},
    types::{
        AmountUnderflowError, BlindedM6, Ctip, Sidechain, SidechainAck, SidechainNumber,
        SidechainProposal, SidechainProposalId,
    },
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

    /// Extend coinbase txouts for a new block with our drivechain messages:
    /// M1 (propose), M2 (ack), M3 (bundle propose), M4 (bundle votes) and
    /// M7 (BMM accept).
    pub(crate) async fn extend_coinbase_txouts(
        &self,
        ack_all_proposals: bool,
        mainchain_tip: BlockHash,
        coinbase_txouts: &mut Vec<TxOut>,
    ) -> Result<(), error::GenerateCoinbaseTxouts> {
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

        let bmm_hashes = self.db().get_bmm_requests(&mainchain_tip).await?;
        for (sidechain_number, bmm_hash) in bmm_hashes {
            if coinbase_builder
                .messages()
                .m7_bmm_accept_slot_vout(&sidechain_number)
                .is_some()
            {
                continue;
            }
            tracing::info!(
                "Adding BMM accept for SC {} with hash: {}",
                sidechain_number,
                bmm_hash
            );
            coinbase_builder.bmm_accept(sidechain_number, bmm_hash)?;
        }
        for (sidechain_id, m6ids) in self.get_bundle_proposals().await? {
            for (m6id, _blinded_m6, m6id_info) in m6ids {
                if m6id_info.is_none() {
                    coinbase_builder.propose_bundle(sidechain_id, m6id)?;
                }
            }
        }
        // One read of the active set, shared by the activation check and the
        // M4 below, so that the M4 is sized against the same active set the
        // check ran against.
        let active_sidechains = self.validator().get_active_sidechains()?;
        // The height of the block under construction, for the proposal age
        // window in the activation check below.
        let next_height = self
            .validator()
            .get_header_info(&mainchain_tip)?
            .height
            .saturating_add(1);
        // Ack bundles
        // TODO: Exclusively ack bundles that are known to us
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
                    if self
                        .validator()
                        .get_pending_withdrawals(&sidechain.proposal.sidechain_number)?
                        .is_empty()
                    {
                        Ok(M4AckBundles::ABSTAIN_ONE_BYTE)
                    } else {
                        Ok(0)
                    }
                })
                .collect::<Result<_, crate::validator::GetPendingWithdrawalsError>>()?;
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
    pub(crate) async fn generate_suffix_txs(
        &self,
        ctips: &HashMap<SidechainNumber, Ctip>,
    ) -> Result<Vec<Transaction>, error::GenerateSuffixTxs> {
        let thresholds = self.validator().network_params().thresholds;
        let mut res = Vec::new();
        for (sidechain_id, m6ids) in self.get_bundle_proposals().await? {
            let mut ctip = None;
            for (_m6id, blinded_m6, m6id_info) in m6ids {
                let Some(m6id_info) = m6id_info else { continue };
                if m6id_info.vote_count > thresholds.withdrawal_bundle_inclusion_threshold {
                    let current_ctip = if let Some(ctip) = ctip {
                        ctip
                    } else {
                        *ctips
                            .get(&sidechain_id)
                            .ok_or_else(|| error::GenerateSuffixTxs::MissingCtip { sidechain_id })?
                    };
                    let (m6, successor_ctip) =
                        Self::finalize_m6(sidechain_id, current_ctip, blinded_m6)?;
                    ctip = Some(successor_ctip);
                    res.push(m6);
                }
            }
        }
        Ok(res)
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
        block_producer::BlockProducer,
        types::{
            AmountUnderflowError, BlindedM6, Ctip, SidechainAck, SidechainDescription,
            SidechainNumber, SidechainProposal, op_drivechain_script,
        },
    };

    fn test_proposal(sidechain_number: SidechainNumber, description: &[u8]) -> SidechainProposal {
        SidechainProposal {
            sidechain_number,
            description: SidechainDescription(description.to_vec()),
        }
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
