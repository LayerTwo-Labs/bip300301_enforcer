//! Block diff that can be applied or undone when connecting / disconnecting
//! blocks.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use sneed::{RoTxn, RwTxn, db};
use thiserror::Error;
use transitive::Transitive;

use crate::{
    types::{Ctip, M6id, PendingM6idInfo, Sidechain, SidechainNumber, SidechainProposalId},
    validator::dbs,
};

pub(in crate::validator) trait Diff {
    /// The DBs that the diff applies to
    type Dbs;

    type ApplyError;
    type UndoError;

    fn apply(
        &self,
        rwtxn: &mut RwTxn,
        dbs: &Self::Dbs,
        height: u32,
    ) -> Result<(), Self::ApplyError>;

    fn undo(&self, rwtxn: &mut RwTxn, dbs: &Self::Dbs) -> Result<(), Self::UndoError>;
}

/// Errors that can occur when undoing a block diff.
///
/// A `VoteCountUnderflow` indicates the block being disconnected — or some
/// inconsistency between the block and the state — would have driven a
/// vote count below zero. Treat as an invalid block, not corrupt state.
#[derive(Debug, Error, Transitive)]
#[transitive(
    from(db::error::Delete, db::Error),
    from(db::error::Get, db::Error),
    from(db::error::Put, db::Error),
    from(db::error::TryGet, db::Error)
)]
pub(in crate::validator) enum UndoError {
    #[error(transparent)]
    Db(Box<db::Error>),
    #[error("vote count underflow on undo for sidechain proposal slot `{sidechain_number}`")]
    SidechainVoteCountUnderflow { sidechain_number: SidechainNumber },
    #[error("vote count underflow on undo for sidechain `{sidechain_number}` m6id `{m6id}`")]
    BundleVoteCountUnderflow {
        sidechain_number: SidechainNumber,
        m6id: M6id,
    },
}

impl From<db::Error> for UndoError {
    fn from(err: db::Error) -> Self {
        Self::Db(Box::new(err))
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[must_use]
pub struct NewSidechainProposal {
    pub id: SidechainProposalId,
    pub sidechain: Sidechain,
}

impl Diff for NewSidechainProposal {
    type Dbs = dbs::ProposalIdToSidechain;
    type ApplyError = db::Error;
    type UndoError = db::Error;

    fn apply(&self, rwtxn: &mut RwTxn, dbs: &Self::Dbs, _height: u32) -> Result<(), db::Error> {
        dbs.put(rwtxn, &self.id, &self.sidechain)?;
        tracing::info!(
            "persisted new sidechain proposal: {}",
            self.sidechain.proposal
        );
        Ok(())
    }

    fn undo(&self, rwtxn: &mut RwTxn, dbs: &Self::Dbs) -> Result<(), db::Error> {
        let _ = dbs.delete(rwtxn, &self.id)?;
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[must_use]
pub struct AckSidechain {
    pub id: SidechainProposalId,
    /// true IFF the sidechain should be activated due to this ack causing the
    /// proposal to reach the vote threshold
    pub activated: bool,
}

impl Diff for AckSidechain {
    type Dbs = dbs::Dbs;
    type ApplyError = db::Error;
    type UndoError = UndoError;

    fn apply(&self, rwtxn: &mut RwTxn, dbs: &Self::Dbs, height: u32) -> Result<(), db::Error> {
        let mut sidechain = dbs.proposal_id_to_sidechain.get(rwtxn, &self.id)?;
        sidechain.status.vote_count += 1;
        if self.activated {
            sidechain.status.activation_height = Some(height);
            tracing::info!(
                "sidechain {} in slot {} was activated",
                String::from_utf8_lossy(&sidechain.proposal.description.0),
                self.id.sidechain_number.0
            );
            let () = dbs.active_sidechains.put_sidechain(
                rwtxn,
                &self.id.sidechain_number,
                &sidechain,
            )?;
            let _ = dbs.proposal_id_to_sidechain.delete(rwtxn, &self.id)?;
        } else {
            dbs.proposal_id_to_sidechain
                .put(rwtxn, &self.id, &sidechain)?;
        }
        Ok(())
    }

    fn undo(&self, rwtxn: &mut RwTxn, dbs: &Self::Dbs) -> Result<(), UndoError> {
        let mut sidechain = if self.activated {
            let mut sidechain = dbs
                .active_sidechains
                .sidechain()
                .get(rwtxn, &self.id.sidechain_number)?;
            let () = dbs
                .active_sidechains
                .delete_sidechain(rwtxn, &self.id.sidechain_number)?;
            sidechain.status.activation_height = None;
            sidechain
        } else {
            dbs.proposal_id_to_sidechain.get(rwtxn, &self.id)?
        };
        sidechain.status.vote_count = sidechain.status.vote_count.checked_sub(1).ok_or(
            UndoError::SidechainVoteCountUnderflow {
                sidechain_number: self.id.sidechain_number,
            },
        )?;
        dbs.proposal_id_to_sidechain
            .put(rwtxn, &self.id, &sidechain)?;
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[must_use]
pub struct ProposeBundle {
    pub sidechain_number: SidechainNumber,
    pub m6id: M6id,
}

impl Diff for ProposeBundle {
    type Dbs = dbs::ActiveSidechainDbs;
    type ApplyError = db::Error;
    type UndoError = db::Error;

    fn apply(&self, rwtxn: &mut RwTxn, dbs: &Self::Dbs, height: u32) -> Result<(), db::Error> {
        let () = dbs.put_pending_m6id(rwtxn, &self.sidechain_number, self.m6id, height)?;
        Ok(())
    }

    fn undo(&self, rwtxn: &mut RwTxn, dbs: &Self::Dbs) -> Result<(), db::Error> {
        let () = dbs.delete_pending_withdrawal(rwtxn, &self.sidechain_number, self.m6id)?;
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub enum AckBundleAction {
    Alarm {
        positive_votes_proposals: HashSet<M6id>,
    },
    Upvote {
        /// MUST not have max votes before applying
        m6id: M6id,
    },
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[must_use]
#[repr(transparent)]
pub struct AckBundles(pub HashMap<SidechainNumber, AckBundleAction>);

impl Diff for AckBundles {
    type Dbs = dbs::ActiveSidechainDbs;
    type ApplyError = db::Error;
    type UndoError = UndoError;

    fn apply(&self, rwtxn: &mut RwTxn, dbs: &Self::Dbs, _height: u32) -> Result<(), db::Error> {
        for (sidechain_number, bundle_action) in &self.0 {
            match bundle_action {
                AckBundleAction::Alarm {
                    positive_votes_proposals: _,
                } => {
                    let () = dbs.alarm_pending_m6ds(rwtxn, sidechain_number)?;
                }
                AckBundleAction::Upvote { m6id } => {
                    let () = dbs.with_pending_withdrawals(
                        rwtxn,
                        sidechain_number,
                        |pending_withdrawals| {
                            pending_withdrawals[m6id].vote_count += 1;
                        },
                    )?;
                }
            }
        }
        Ok(())
    }

    fn undo(&self, rwtxn: &mut RwTxn, dbs: &Self::Dbs) -> Result<(), UndoError> {
        for (sidechain_number, bundle_action) in &self.0 {
            match bundle_action {
                AckBundleAction::Alarm {
                    positive_votes_proposals,
                } => {
                    let () = dbs.with_pending_withdrawals(
                        rwtxn,
                        sidechain_number,
                        |pending_withdrawals| {
                            pending_withdrawals.iter_mut().for_each(|(m6id, info)| {
                                if positive_votes_proposals.contains(m6id) {
                                    info.vote_count += 1;
                                }
                            });
                        },
                    )?;
                }
                AckBundleAction::Upvote { m6id } => {
                    let () = dbs.with_pending_withdrawals(
                        rwtxn,
                        sidechain_number,
                        |pending_withdrawals| -> Result<(), UndoError> {
                            let entry = &mut pending_withdrawals[m6id];
                            entry.vote_count = entry.vote_count.checked_sub(1).ok_or(
                                UndoError::BundleVoteCountUnderflow {
                                    sidechain_number: *sidechain_number,
                                    m6id: *m6id,
                                },
                            )?;
                            Ok(())
                        },
                    )??;
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[must_use]
pub enum CoinbaseMsg {
    AckBundles(AckBundles),
    AckSidechain(AckSidechain),
    NewSidechainProposal(NewSidechainProposal),
    ProposeBundle(ProposeBundle),
}

impl Diff for CoinbaseMsg {
    type Dbs = dbs::Dbs;
    type ApplyError = db::Error;
    type UndoError = UndoError;

    fn apply(&self, rwtxn: &mut RwTxn, dbs: &Self::Dbs, height: u32) -> Result<(), db::Error> {
        match self {
            Self::AckBundles(diff) => {
                let () = diff.apply(rwtxn, &dbs.active_sidechains, height)?;
            }
            Self::AckSidechain(diff) => {
                let () = diff.apply(rwtxn, dbs, height)?;
            }
            Self::NewSidechainProposal(diff) => {
                let () = diff.apply(rwtxn, &dbs.proposal_id_to_sidechain, height)?;
            }
            Self::ProposeBundle(diff) => {
                let () = diff.apply(rwtxn, &dbs.active_sidechains, height)?;
            }
        }
        Ok(())
    }

    fn undo(&self, rwtxn: &mut RwTxn, dbs: &Self::Dbs) -> Result<(), UndoError> {
        match self {
            Self::AckBundles(diff) => {
                let () = diff.undo(rwtxn, &dbs.active_sidechains)?;
            }
            Self::AckSidechain(diff) => {
                let () = diff.undo(rwtxn, dbs)?;
            }
            Self::NewSidechainProposal(diff) => {
                let () = diff.undo(rwtxn, &dbs.proposal_id_to_sidechain)?;
            }
            Self::ProposeBundle(diff) => {
                let () = diff.undo(rwtxn, &dbs.active_sidechains)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[must_use]
#[repr(transparent)]
pub struct FailedProposals(pub HashMap<SidechainProposalId, Sidechain>);

impl Diff for FailedProposals {
    type Dbs = dbs::ProposalIdToSidechain;
    type ApplyError = db::Error;
    type UndoError = db::Error;

    fn apply(&self, rwtxn: &mut RwTxn, dbs: &Self::Dbs, _height: u32) -> Result<(), db::Error> {
        for failed_proposal_id in self.0.keys() {
            let _ = dbs.delete(rwtxn, failed_proposal_id)?;
        }
        Ok(())
    }

    fn undo(&self, rwtxn: &mut RwTxn, dbs: &Self::Dbs) -> Result<(), db::Error> {
        for (failed_proposal_id, proposal) in &self.0 {
            dbs.put(rwtxn, failed_proposal_id, proposal)?;
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[must_use]
#[repr(transparent)]
pub struct FailedM6ids(pub HashMap<SidechainNumber, BTreeMap<usize, (M6id, PendingM6idInfo)>>);

impl Diff for FailedM6ids {
    type Dbs = dbs::ActiveSidechainDbs;
    type ApplyError = db::Error;
    type UndoError = db::Error;

    fn apply(&self, rwtxn: &mut RwTxn, dbs: &Self::Dbs, _height: u32) -> Result<(), db::Error> {
        fn remove_failed_withdrawals(
            pending_withdrawals: &mut ordermap::OrderMap<M6id, PendingM6idInfo>,
            failed_m6ids: &BTreeMap<usize, (M6id, PendingM6idInfo)>,
        ) {
            let (non_failed, mut pending_withdrawals) = {
                let non_failed_capacity = pending_withdrawals.len() - failed_m6ids.len();
                let non_failed = pending_withdrawals;
                let pending_withdrawals = std::mem::replace(
                    non_failed,
                    ordermap::OrderMap::with_capacity(non_failed_capacity),
                )
                .into_iter();
                (non_failed, pending_withdrawals)
            };
            let mut failed_m6ids = failed_m6ids.keys().copied().peekable();
            let mut idx = 0;
            loop {
                if failed_m6ids
                    .next_if(|failed_idx| *failed_idx == idx)
                    .is_some()
                {
                    let Some(_) = pending_withdrawals.next() else {
                        return;
                    };
                    idx += 1;
                } else if let Some((m6id, info)) = pending_withdrawals.next() {
                    non_failed.insert(m6id, info);
                    idx += 1;
                } else {
                    return;
                }
            }
        }
        for (sidechain_number, failed_m6ids) in &self.0 {
            let () =
                dbs.with_pending_withdrawals(rwtxn, sidechain_number, |pending_withdrawals| {
                    remove_failed_withdrawals(pending_withdrawals, failed_m6ids)
                })?;
        }
        Ok(())
    }

    fn undo(&self, rwtxn: &mut RwTxn, dbs: &Self::Dbs) -> Result<(), db::Error> {
        fn refill_pending_withdrawals(
            pending_withdrawals: &mut ordermap::OrderMap<M6id, PendingM6idInfo>,
            failed_m6ids: &BTreeMap<usize, (M6id, PendingM6idInfo)>,
        ) {
            let mut non_failed = std::mem::replace(
                pending_withdrawals,
                ordermap::OrderMap::with_capacity(pending_withdrawals.len() + failed_m6ids.len()),
            )
            .into_iter();
            let mut failed_m6ids = failed_m6ids.iter().peekable();
            let mut idx = 0;
            loop {
                if let Some((_, (m6id, info))) =
                    failed_m6ids.next_if(|(failed_idx, _)| **failed_idx == idx)
                {
                    pending_withdrawals.insert(*m6id, *info);
                    idx += 1;
                } else if let Some((m6id, info)) = non_failed.next() {
                    pending_withdrawals.insert(m6id, info);
                    idx += 1;
                } else {
                    return;
                }
            }
        }
        for (sidechain_number, failed_m6ids) in &self.0 {
            let () =
                dbs.with_pending_withdrawals(rwtxn, sidechain_number, |pending_withdrawals| {
                    refill_pending_withdrawals(pending_withdrawals, failed_m6ids)
                })?;
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[must_use]
pub struct Coinbase {
    pub msgs: Vec<CoinbaseMsg>,
    pub failed_proposals: FailedProposals,
    pub failed_m6ids: FailedM6ids,
}

impl Diff for Coinbase {
    type Dbs = dbs::Dbs;
    type ApplyError = db::Error;
    type UndoError = UndoError;

    fn apply(&self, rwtxn: &mut RwTxn, dbs: &Self::Dbs, height: u32) -> Result<(), db::Error> {
        let Self {
            msgs,
            failed_proposals,
            failed_m6ids,
        } = self;
        for msg in msgs {
            let () = msg.apply(rwtxn, dbs, height)?;
        }
        let () = failed_proposals.apply(rwtxn, &dbs.proposal_id_to_sidechain, height)?;
        let () = failed_m6ids.apply(rwtxn, &dbs.active_sidechains, height)?;
        Ok(())
    }

    fn undo(&self, rwtxn: &mut RwTxn, dbs: &Self::Dbs) -> Result<(), UndoError> {
        let Self {
            msgs,
            failed_proposals,
            failed_m6ids,
        } = self;
        let () = failed_m6ids.undo(rwtxn, &dbs.active_sidechains)?;
        let () = failed_proposals.undo(rwtxn, &dbs.proposal_id_to_sidechain)?;
        for msg in msgs.iter().rev() {
            let () = msg.undo(rwtxn, dbs)?;
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[must_use]
pub struct Tx {
    pub sidechain_number: SidechainNumber,
    pub new_ctip: Ctip,
    pub removed_pending_withdrawal: Option<(M6id, PendingM6idInfo)>,
}

impl Diff for Tx {
    type Dbs = dbs::ActiveSidechainDbs;
    type ApplyError = db::Error;
    type UndoError = db::Error;

    fn apply(&self, rwtxn: &mut RwTxn, dbs: &Self::Dbs, _height: u32) -> Result<(), db::Error> {
        let Self {
            sidechain_number,
            new_ctip,
            removed_pending_withdrawal,
        } = self;
        let _: u64 = dbs.put_ctip(rwtxn, *sidechain_number, new_ctip)?;
        if let Some((m6id, _)) = removed_pending_withdrawal {
            let () = dbs.delete_pending_withdrawal(rwtxn, sidechain_number, *m6id)?;
        }
        Ok(())
    }

    fn undo(&self, rwtxn: &mut RwTxn, dbs: &Self::Dbs) -> Result<(), db::Error> {
        let Self {
            sidechain_number,
            new_ctip: _,
            removed_pending_withdrawal,
        } = self;
        let () = dbs.delete_ctip(rwtxn, *sidechain_number)?;
        if let Some((m6id, info)) = removed_pending_withdrawal {
            let () =
                dbs.with_pending_withdrawal_entry(rwtxn, sidechain_number, *m6id, |entry| {
                    match entry {
                        ordermap::map::Entry::Occupied(mut entry) => _ = entry.insert(*info),
                        ordermap::map::Entry::Vacant(entry) => _ = entry.insert(*info),
                    }
                })?;
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[must_use]
pub struct Block {
    pub coinbase: Coinbase,
    pub txs: Vec<Tx>,
}

impl Diff for Block {
    type Dbs = dbs::Dbs;
    type ApplyError = db::Error;
    type UndoError = UndoError;

    fn apply(&self, rwtxn: &mut RwTxn, dbs: &Self::Dbs, height: u32) -> Result<(), db::Error> {
        let Self { coinbase, txs } = self;
        let () = coinbase.apply(rwtxn, dbs, height)?;
        for tx in txs {
            let () = tx.apply(rwtxn, &dbs.active_sidechains, height)?;
        }
        Ok(())
    }

    fn undo(&self, rwtxn: &mut RwTxn, dbs: &Self::Dbs) -> Result<(), UndoError> {
        let Self { coinbase, txs } = self;
        for tx in txs.iter().rev() {
            let () = tx.undo(rwtxn, &dbs.active_sidechains)?;
        }
        let () = coinbase.undo(rwtxn, dbs)?;
        Ok(())
    }
}

/// Build a vec of diffs while applying diffs
#[must_use]
pub struct DiffBuilder<'a, 'txn, D>
where
    D: Diff,
{
    rwtxn: &'a mut RwTxn<'txn>,
    dbs: &'a D::Dbs,
    height: u32,
    diffs: Vec<D>,
}

impl<'a, 'txn, D> DiffBuilder<'a, 'txn, D>
where
    D: Diff,
{
    pub fn new(rwtxn: &'a mut RwTxn<'txn>, dbs: &'a D::Dbs, height: u32) -> Self {
        Self {
            rwtxn,
            dbs,
            height,
            diffs: Vec::new(),
        }
    }

    /// Use the txn and dbs
    pub fn rotxn<'b, F, T>(&'b self, f: F) -> T
    where
        F: FnOnce(&'b RoTxn<'txn>, &'b D::Dbs) -> T,
    {
        f(self.rwtxn, self.dbs)
    }

    /// Get the applied diffs
    pub fn diffs(self) -> Vec<D> {
        self.diffs
    }

    /// Apply a diff
    pub fn apply(&mut self, diff: D) -> Result<(), D::ApplyError> {
        let () = diff.apply(self.rwtxn, self.dbs, self.height)?;
        self.diffs.push(diff);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};

    use bitcoin::{Txid, hashes::Hash as _};
    use miette::{IntoDiagnostic, Result};

    use super::*;
    use crate::{
        types::{M6id, SidechainNumber},
        validator::test_utils::{create_test_dbs, test_sidechain},
    };

    fn test_m6id(byte: u8) -> M6id {
        M6id(Txid::from_byte_array([byte; 32]))
    }

    #[test]
    fn ack_sidechain_apply_undo_roundtrip_and_underflow_error() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;

        let sidechain = test_sidechain(1, 100);
        let proposal_id = sidechain.proposal.compute_id();
        dbs.proposal_id_to_sidechain
            .put(&mut rwtxn, &proposal_id, &sidechain)
            .into_diagnostic()?;

        let diff = AckSidechain {
            id: proposal_id,
            activated: false,
        };

        diff.apply(&mut rwtxn, &dbs, 101).into_diagnostic()?;
        let s = dbs
            .proposal_id_to_sidechain
            .get(&rwtxn, &proposal_id)
            .into_diagnostic()?;
        assert_eq!(s.status.vote_count, 1);

        diff.undo(&mut rwtxn, &dbs).into_diagnostic()?;
        let s = dbs
            .proposal_id_to_sidechain
            .get(&rwtxn, &proposal_id)
            .into_diagnostic()?;
        assert_eq!(s.status.vote_count, 0);

        let err = diff
            .undo(&mut rwtxn, &dbs)
            .expect_err("undo at vote_count=0 must error");
        assert!(matches!(
            err,
            UndoError::SidechainVoteCountUnderflow { sidechain_number }
                if sidechain_number == proposal_id.sidechain_number
        ));
        Ok(())
    }

    #[test]
    fn ack_bundles_upvote_apply_undo_roundtrip() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;

        let sc = SidechainNumber(1);
        dbs.active_sidechains
            .put_sidechain(&mut rwtxn, &sc, &test_sidechain(1, 0))
            .into_diagnostic()?;

        let m6id = test_m6id(0xBB);
        dbs.active_sidechains
            .put_pending_m6id(&mut rwtxn, &sc, m6id, 0)
            .into_diagnostic()?;

        let diff = AckBundles({
            let mut map = HashMap::new();
            map.insert(sc, AckBundleAction::Upvote { m6id });
            map
        });

        diff.apply(&mut rwtxn, &dbs.active_sidechains, 1)
            .into_diagnostic()?;
        let pending = dbs
            .active_sidechains
            .pending_m6ids()
            .get(&rwtxn, &sc)
            .into_diagnostic()?;
        assert_eq!(pending[&m6id].vote_count, 1);

        diff.undo(&mut rwtxn, &dbs.active_sidechains)
            .into_diagnostic()?;
        let pending = dbs
            .active_sidechains
            .pending_m6ids()
            .get(&rwtxn, &sc)
            .into_diagnostic()?;
        assert_eq!(pending[&m6id].vote_count, 0);
        Ok(())
    }

    #[test]
    fn ack_bundles_alarm_apply_undo_roundtrip() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;

        let sc = SidechainNumber(1);
        dbs.active_sidechains
            .put_sidechain(&mut rwtxn, &sc, &test_sidechain(1, 0))
            .into_diagnostic()?;

        let m6id_a = test_m6id(0xAA);
        let m6id_b = test_m6id(0xBB);
        dbs.active_sidechains
            .put_pending_m6id(&mut rwtxn, &sc, m6id_a, 0)
            .into_diagnostic()?;
        dbs.active_sidechains
            .put_pending_m6id(&mut rwtxn, &sc, m6id_b, 0)
            .into_diagnostic()?;

        for (m6id, count) in [(m6id_a, 3u16), (m6id_b, 1)] {
            dbs.active_sidechains
                .with_pending_withdrawal_entry(&mut rwtxn, &sc, m6id, |entry| {
                    if let ordermap::map::Entry::Occupied(mut e) = entry {
                        e.get_mut().vote_count = count;
                    }
                })
                .into_diagnostic()?;
        }

        let diff = AckBundles({
            let mut map = HashMap::new();
            map.insert(
                sc,
                AckBundleAction::Alarm {
                    positive_votes_proposals: HashSet::from([m6id_a, m6id_b]),
                },
            );
            map
        });

        diff.apply(&mut rwtxn, &dbs.active_sidechains, 1)
            .into_diagnostic()?;
        let p = dbs
            .active_sidechains
            .pending_m6ids()
            .get(&rwtxn, &sc)
            .into_diagnostic()?;
        assert_eq!(p[&m6id_a].vote_count, 2);
        assert_eq!(p[&m6id_b].vote_count, 0);

        diff.undo(&mut rwtxn, &dbs.active_sidechains)
            .into_diagnostic()?;
        let p = dbs
            .active_sidechains
            .pending_m6ids()
            .get(&rwtxn, &sc)
            .into_diagnostic()?;
        assert_eq!(p[&m6id_a].vote_count, 3);
        assert_eq!(p[&m6id_b].vote_count, 1);
        Ok(())
    }

    #[test]
    fn upvote_then_alarm_then_undo_upvote_errors_on_underflow() -> Result<()> {
        let (_dir, dbs) = create_test_dbs()?;
        let mut rwtxn = dbs.write_txn().into_diagnostic()?;

        let sc = SidechainNumber(1);
        dbs.active_sidechains
            .put_sidechain(&mut rwtxn, &sc, &test_sidechain(1, 0))
            .into_diagnostic()?;

        let m6id = test_m6id(0xCC);
        dbs.active_sidechains
            .put_pending_m6id(&mut rwtxn, &sc, m6id, 0)
            .into_diagnostic()?;

        let upvote_diff = AckBundles({
            let mut map = HashMap::new();
            map.insert(sc, AckBundleAction::Upvote { m6id });
            map
        });
        upvote_diff
            .apply(&mut rwtxn, &dbs.active_sidechains, 1)
            .into_diagnostic()?;
        assert_eq!(
            dbs.active_sidechains
                .pending_m6ids()
                .get(&rwtxn, &sc)
                .into_diagnostic()?[&m6id]
                .vote_count,
            1
        );

        let alarm_diff = AckBundles({
            let mut map = HashMap::new();
            map.insert(
                sc,
                AckBundleAction::Alarm {
                    positive_votes_proposals: HashSet::from([m6id]),
                },
            );
            map
        });
        alarm_diff
            .apply(&mut rwtxn, &dbs.active_sidechains, 2)
            .into_diagnostic()?;
        assert_eq!(
            dbs.active_sidechains
                .pending_m6ids()
                .get(&rwtxn, &sc)
                .into_diagnostic()?[&m6id]
                .vote_count,
            0
        );

        let err = upvote_diff
            .undo(&mut rwtxn, &dbs.active_sidechains)
            .expect_err("undo at vote_count=0 must error");
        assert!(matches!(
            err,
            UndoError::BundleVoteCountUnderflow {
                sidechain_number,
                m6id: err_m6id,
            } if sidechain_number == sc && err_m6id == m6id
        ));
        Ok(())
    }
}
