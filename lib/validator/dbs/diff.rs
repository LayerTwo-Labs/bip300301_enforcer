//! Block diff that can be applied or undone when connecting / disconnecting
//! blocks.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use sneed::{RoTxn, RwTxn, db};

use crate::{
    types::{Ctip, M6id, PendingM6idInfo, Sidechain, SidechainNumber, SidechainProposalId},
    validator::dbs,
};

pub(in crate::validator) trait Diff {
    /// The DBs that the diff applies to
    type Dbs;

    fn apply(&self, rwtxn: &mut RwTxn, dbs: &Self::Dbs, height: u32) -> Result<(), db::Error>;

    fn undo(&self, rwtxn: &mut RwTxn, dbs: &Self::Dbs) -> Result<(), db::Error>;
}

#[derive(Debug, Deserialize, Serialize)]
#[must_use]
pub struct NewSidechainProposal {
    pub id: SidechainProposalId,
    pub sidechain: Sidechain,
}

impl Diff for NewSidechainProposal {
    type Dbs = dbs::ProposalIdToSidechain;

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

    fn undo(&self, rwtxn: &mut RwTxn, dbs: &Self::Dbs) -> Result<(), db::Error> {
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
        sidechain.status.vote_count -= 1;
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

    fn undo(&self, rwtxn: &mut RwTxn, dbs: &Self::Dbs) -> Result<(), db::Error> {
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
                        |pending_withdrawals| {
                            pending_withdrawals[m6id].vote_count -= 1;
                        },
                    )?;
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

    fn undo(&self, rwtxn: &mut RwTxn, dbs: &Self::Dbs) -> Result<(), db::Error> {
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

    fn undo(&self, rwtxn: &mut RwTxn, dbs: &Self::Dbs) -> Result<(), db::Error> {
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

    fn apply(&self, rwtxn: &mut RwTxn, dbs: &Self::Dbs, height: u32) -> Result<(), db::Error> {
        let Self { coinbase, txs } = self;
        let () = coinbase.apply(rwtxn, dbs, height)?;
        for tx in txs {
            let () = tx.apply(rwtxn, &dbs.active_sidechains, height)?;
        }
        Ok(())
    }

    fn undo(&self, rwtxn: &mut RwTxn, dbs: &Self::Dbs) -> Result<(), db::Error> {
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
    pub fn apply(&mut self, diff: D) -> Result<(), db::Error> {
        let () = diff.apply(self.rwtxn, self.dbs, self.height)?;
        self.diffs.push(diff);
        Ok(())
    }
}
