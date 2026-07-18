use std::collections::{HashMap, HashSet};

use bitcoin::{Amount, BlockHash, OutPoint, Transaction, TxOut};
use fallible_iterator::{FallibleIterator as _, IteratorExt as _};

use crate::{
    block_producer::{BlockProducer, BundleProposals, error},
    messages::{CoinbaseBuilder, M4AckBundles},
    types::{AmountUnderflowError, Ctip, SidechainAck, SidechainNumber, SidechainProposal},
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
        &self,
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

        let mut sidechain_acks = self.db().get_sidechain_acks().await?;

        // Pending sidechain proposals from the /validator/, i.e. proposals
        // broadcast by (potentially) someone else, and already active.
        let active_sidechain_proposals = self.get_active_sidechain_proposals()?;

        if ack_all_proposals && !active_sidechain_proposals.is_empty() {
            tracing::info!(
                "Handle sidechain ACK: acking all sidechains regardless of what DB says"
            );

            for (sidechain_number, sidechain_proposal) in &active_sidechain_proposals {
                let sidechain_number = *sidechain_number;

                if !sidechain_acks
                    .iter()
                    .any(|ack| ack.sidechain_number == sidechain_number)
                {
                    tracing::debug!(
                        "Handle sidechain ACK: adding 'fake' ACK for {}",
                        sidechain_number
                    );
                    sidechain_acks.push(SidechainAck {
                        sidechain_number,
                        description_hash: sidechain_proposal.description.sha256d_hash(),
                    });
                }
            }
        }

        for sidechain_ack in sidechain_acks {
            if !self.validate_sidechain_ack(&sidechain_ack, &active_sidechain_proposals) {
                self.db().delete_sidechain_ack(&sidechain_ack).await?;
                tracing::info!(
                    "Unable to handle sidechain ack, deleted: {}",
                    sidechain_ack.sidechain_number
                );
                continue;
            }
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
        // Ack bundles
        // TODO: Exclusively ack bundles that are known to us
        // TODO: ack bundles when M2 messages are present
        if ack_all_proposals && coinbase_builder.messages().m2_ack_slots().is_empty() {
            let active_sidechains = self.validator().get_active_sidechains()?;
            let upvotes = active_sidechains
                .into_iter()
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
                    let Ctip { outpoint, value } = if let Some(ctip) = ctip {
                        ctip
                    } else {
                        *ctips
                            .get(&sidechain_id)
                            .ok_or_else(|| error::GenerateSuffixTxs::MissingCtip { sidechain_id })?
                    };
                    let new_value =
                        Self::new_treasury_value(value, *blinded_m6.fee(), *blinded_m6.payout())?;
                    let m6 = blinded_m6.into_m6(sidechain_id, outpoint, value)?;
                    ctip = Some(Ctip {
                        outpoint: OutPoint {
                            txid: m6.compute_txid(),
                            vout: (m6.output.len() - 1) as u32,
                        },
                        value: new_value,
                    });
                    res.push(m6);
                }
            }
        }
        Ok(res)
    }
}

#[cfg(test)]
mod tests {
    use bitcoin::Amount;

    use crate::{block_producer::BlockProducer, types::AmountUnderflowError};

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
}
