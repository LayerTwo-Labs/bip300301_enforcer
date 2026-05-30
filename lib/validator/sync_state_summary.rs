//! A deterministic snapshot of BIP300/301 consensus state at a mainchain tip,
//! intended for cross-run consistency checks. See [`SyncStateSummary`] and
//! [`Validator::sync_state_summary`].

use std::collections::HashSet;

use bitcoin::{BlockHash, OutPoint};

use crate::{
    types::{M6id, SidechainDeclaration},
    validator::Validator,
};

impl Validator {
    /// Collect a deterministic snapshot of BIP300/301 consensus state at the
    /// current tip. The result is ordered, so two validators synced to the same
    /// tip and in agreement produce byte-identical summaries (and digests).
    /// Intended for cross-run consistency checks.
    pub fn sync_state_summary(&self) -> Result<SyncStateSummary, miette::Report> {
        let tip_hash = self.get_mainchain_tip()?;
        let tip_height = self
            .try_get_block_height()?
            .ok_or_else(|| miette::miette!("cannot summarize state: validator is not synced"))?;

        let ctips = self.get_ctips()?;

        // Activated sidechains and un-activated proposals live in separate DBs:
        // Gather both so the summary reflects the
        // full set. Active entries come first so they win the dedup below.
        let sidechains_iter = self
            .get_active_sidechains()?
            .into_iter()
            .chain(self.get_sidechains()?.into_iter().map(|(_id, sc)| sc));

        let mut seen = HashSet::new();
        let mut sidechains = Vec::new();
        for sidechain in sidechains_iter {
            let sidechain_number = sidechain.proposal.sidechain_number;
            let description_hash = sidechain.proposal.description.sha256d_hash();
            if !seen.insert((sidechain_number, description_hash)) {
                continue;
            }
            let activation_height = sidechain.status.activation_height;

            let (ctip, ctip_sequence_number, mut pending_withdrawals) =
                // Ctip, sequence number and pending withdrawal bundles only exist
                // for activated sidechains.
                if activation_height.is_some() {
                    let ctip = ctips.get(&sidechain_number).map(|ctip| CtipSummary {
                        outpoint: ctip.outpoint,
                        value_sats: ctip.value.to_sat(),
                    });
                    let ctip_sequence_number = self.get_ctip_sequence_number(sidechain_number)?;
                    let pending: Vec<_> = self
                        .get_pending_withdrawals(&sidechain_number)?
                        .into_iter()
                        .map(|(m6id, info)| PendingWithdrawalSummary {
                            m6id,
                            vote_count: info.vote_count,
                            proposal_height: info.proposal_height,
                        })
                        .collect();
                    (ctip, ctip_sequence_number, pending)
                } else {
                    (None, None, Vec::new())
                };

            // Canonical ordering of bundles by M6id.
            pending_withdrawals.sort_by(|a, b| a.m6id.0.cmp(&b.m6id.0));

            // Human-readable title/description, best-effort
            let declaration = SidechainDeclaration::try_from(&sidechain.proposal.description).ok();

            sidechains.push(SidechainStateSummary {
                sidechain_number: sidechain_number.0,
                description_hash,
                title: declaration.as_ref().map(|d| d.title.clone()),
                description: declaration.map(|d| d.description),
                vote_count: sidechain.status.vote_count,
                proposal_height: sidechain.status.proposal_height,
                activation_height,
                ctip,
                ctip_sequence_number,
                pending_withdrawals,
            });
        }

        // Canonical ordering: by sidechain number, then proposal description
        // hash (a single sidechain number can have multiple competing proposals).
        sidechains.sort_by(|a, b| {
            a.sidechain_number
                .cmp(&b.sidechain_number)
                .then_with(|| a.description_hash.cmp(&b.description_hash))
        });

        Ok(SyncStateSummary {
            tip_hash,
            tip_height,
            sidechains,
        })
    }
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct SyncStateSummary {
    pub tip_hash: BlockHash,
    pub tip_height: u32,
    pub sidechains: Vec<SidechainStateSummary>,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct SidechainStateSummary {
    pub sidechain_number: u8,
    pub description_hash: bitcoin::hashes::sha256d::Hash,
    /// Human-readable title from the M1 declaration, if it parses.
    pub title: Option<String>,
    /// Human-readable description from the M1 declaration, if it parses.
    pub description: Option<String>,
    /// BIP300 M1 ACK vote count for this proposal.
    pub vote_count: u16,
    pub proposal_height: u32,
    /// `Some` once the sidechain has activated.
    pub activation_height: Option<u32>,
    pub ctip: Option<CtipSummary>,
    pub ctip_sequence_number: Option<u64>,
    pub pending_withdrawals: Vec<PendingWithdrawalSummary>,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct CtipSummary {
    pub outpoint: OutPoint,
    pub value_sats: u64,
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct PendingWithdrawalSummary {
    pub m6id: M6id,
    /// BIP300 M3/M4 ACK vote count for this withdrawal bundle.
    pub vote_count: u16,
    pub proposal_height: u32,
}

impl SyncStateSummary {
    pub fn digest(&self) -> bitcoin::hashes::sha256::Hash {
        use bitcoin::hashes::Hash as _;
        let json = serde_json::to_vec(self)
            .expect("SyncStateSummary contain infallibly-serializable fields");
        bitcoin::hashes::sha256::Hash::hash(&json)
    }

    pub fn to_json_pretty(&self) -> String {
        serde_json::to_string_pretty(self)
            .expect("SyncStateSummary is composed of infallibly-serializable fields")
    }

    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Compare this summary against a `reference`, returning a list of
    /// human-readable differences in the consensus values (tip first, then
    /// sidechains). An empty list means the two are byte-for-byte identical
    pub fn diff(&self, reference: &Self) -> Vec<String> {
        let mut diffs = Vec::new();
        if self.tip_hash != reference.tip_hash {
            diffs.push(format!(
                "tip_hash: {} (current) != {} (reference)",
                self.tip_hash, reference.tip_hash
            ));
        }
        if self.tip_height != reference.tip_height {
            diffs.push(format!(
                "tip_height: {} (current) != {} (reference)",
                self.tip_height, reference.tip_height
            ));
        }
        diffs.extend(self.sidechain_diffs(reference));
        diffs
    }

    /// Human-readable differences in the per-sidechain consensus state
    pub fn sidechain_diffs(&self, reference: &Self) -> Vec<String> {
        use std::collections::BTreeMap;

        let mut diffs = Vec::new();

        let key = |sc: &SidechainStateSummary| (sc.sidechain_number, sc.description_hash);
        let current: BTreeMap<_, _> = self.sidechains.iter().map(|sc| (key(sc), sc)).collect();
        let reference_map: BTreeMap<_, _> = reference
            .sidechains
            .iter()
            .map(|sc| (key(sc), sc))
            .collect();

        for (k, sc) in &current {
            match reference_map.get(k) {
                None => diffs.push(format!(
                    "sidechain {} ({}): present now but absent in reference",
                    k.0,
                    short_hash(&k.1)
                )),
                Some(ref_sc) => {
                    diffs.extend(sc.field_diffs(ref_sc));
                }
            }
        }
        for k in reference_map.keys() {
            if !current.contains_key(k) {
                diffs.push(format!(
                    "sidechain {} ({}): in reference but absent now",
                    k.0,
                    short_hash(&k.1)
                ));
            }
        }
        diffs
    }

    pub fn active_sidechain_count(&self) -> usize {
        self.sidechains
            .iter()
            .filter(|sc| sc.activation_height.is_some())
            .count()
    }

    pub fn pending_withdrawal_count(&self) -> usize {
        self.sidechains
            .iter()
            .map(|sc| sc.pending_withdrawals.len())
            .sum()
    }
}

fn short_hash(hash: &bitcoin::hashes::sha256d::Hash) -> String {
    hash.to_string()[..8].to_owned()
}

impl SidechainStateSummary {
    fn field_diffs(&self, reference: &Self) -> Vec<String> {
        let label = format!(
            "sidechain {} ({})",
            self.sidechain_number,
            short_hash(&self.description_hash)
        );
        let mut diffs = Vec::new();
        macro_rules! cmp {
            ($field:ident) => {
                if self.$field != reference.$field {
                    diffs.push(format!(
                        "{label}: {} differs ({:?} now vs {:?} reference)",
                        stringify!($field),
                        self.$field,
                        reference.$field,
                    ));
                }
            };
        }
        cmp!(vote_count);
        cmp!(proposal_height);
        cmp!(activation_height);
        cmp!(ctip_sequence_number);
        cmp!(title);
        cmp!(description);
        if self.ctip != reference.ctip {
            diffs.push(format!(
                "{label}: ctip differs ({:?} now vs {:?} reference)",
                self.ctip, reference.ctip
            ));
        }
        if self.pending_withdrawals != reference.pending_withdrawals {
            diffs.push(format!(
                "{label}: pending withdrawal bundles differ ({} now vs {} reference)",
                self.pending_withdrawals.len(),
                reference.pending_withdrawals.len(),
            ));
        }
        diffs
    }
}

#[cfg(test)]
mod tests {
    use bitcoin::{Txid, hashes::Hash as _};

    use super::*;

    fn sample() -> SyncStateSummary {
        SyncStateSummary {
            tip_hash: BlockHash::all_zeros(),
            tip_height: 15139,
            sidechains: vec![SidechainStateSummary {
                sidechain_number: 9,
                description_hash: bitcoin::hashes::sha256d::Hash::all_zeros(),
                title: Some("Thunder".to_owned()),
                description: Some("A sidechain with a large blocksize".to_owned()),
                vote_count: 6,
                proposal_height: 248,
                activation_height: Some(254),
                ctip: Some(CtipSummary {
                    outpoint: OutPoint::null(),
                    value_sats: 12_345,
                }),
                ctip_sequence_number: Some(3),
                pending_withdrawals: vec![PendingWithdrawalSummary {
                    m6id: M6id(Txid::all_zeros()),
                    vote_count: 1,
                    proposal_height: 250,
                }],
            }],
        }
    }

    #[test]
    fn json_round_trip_preserves_state_and_digest() {
        let original = sample();
        let parsed = SyncStateSummary::from_json(&original.to_json_pretty())
            .expect("consensus-state JSON should round-trip");
        assert_eq!(original, parsed);
        assert_eq!(original.digest(), parsed.digest());
        assert!(original.diff(&parsed).is_empty());
    }

    #[test]
    fn diff_reports_changed_fields_and_membership() {
        let reference = sample();

        // A changed ACK vote count is reported.
        let mut changed = sample();
        changed.sidechains[0].vote_count = 7;
        let diffs = changed.diff(&reference);
        assert!(diffs.iter().any(|d| d.contains("vote_count")), "{diffs:?}");

        // A sidechain present now but not in the reference is reported.
        let mut extra = sample();
        extra.sidechains[0].sidechain_number = 13;
        let diffs = extra.diff(&reference);
        assert!(
            diffs.iter().any(|d| d.contains("absent in reference"))
                && diffs.iter().any(|d| d.contains("absent now")),
            "{diffs:?}"
        );

        // Differing tip is reported.
        let mut tip = sample();
        tip.tip_height = 15140;
        assert!(
            tip.diff(&reference)
                .iter()
                .any(|d| d.contains("tip_height"))
        );
    }
}
