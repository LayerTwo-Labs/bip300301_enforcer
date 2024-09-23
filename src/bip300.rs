use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::{io::Cursor, path::Path, str::FromStr, time::Duration};

use bip300301_messages::bitcoin::hashes::Hash;
use bip300301_messages::{
    bitcoin, m6_to_id, parse_coinbase_script, parse_m8_bmm_request, parse_op_drivechain, sha256d,
    CoinbaseMessage, M4AckBundles, ABSTAIN_TWO_BYTES, ALARM_TWO_BYTES,
};
use bitcoin::{
    consensus::Decodable, opcodes::all::OP_RETURN, Block, BlockHash, OutPoint, Transaction,
};
use fallible_iterator::{FallibleIterator, IteratorExt as _};
use futures::{StreamExt, TryFutureExt, TryStreamExt};
use heed::{types::SerdeBincode, Env, EnvOpenOptions};
use heed::{Database, RoTxn, RwTxn};
use miette::{miette, IntoDiagnostic, Result};
use serde::{Deserialize, Serialize};
use tokio::task::JoinHandle;
use tokio::time::{interval, Instant};
use tokio_stream::wrappers::IntervalStream;
use ureq_jsonrpc::{json, Client};

use crate::types::{
    Ctip, Deposit, Hash256, PendingM6id, Sidechain, SidechainProposal, TreasuryUtxo,
};

/*
const WITHDRAWAL_BUNDLE_MAX_AGE: u16 = 26_300;
const WITHDRAWAL_BUNDLE_INCLUSION_THRESHOLD: u16 = WITHDRAWAL_BUNDLE_MAX_AGE / 2; // 13_150, aka 51% hashrate

const USED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE: u16 = WITHDRAWAL_BUNDLE_MAX_AGE; // 26_300
const USED_SIDECHAIN_SLOT_ACTIVATION_THRESHOLD: u16 = USED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE / 2;

const UNUSED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE: u16 = 2016;
const UNUSED_SIDECHAIN_SLOT_ACTIVATION_THRESHOLD: u16 = UNUSED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE / 2; // 1008, aka 51% hashrate

*/

const WITHDRAWAL_BUNDLE_MAX_AGE: u16 = 10;
const WITHDRAWAL_BUNDLE_INCLUSION_THRESHOLD: u16 = WITHDRAWAL_BUNDLE_MAX_AGE / 2; // 5

const USED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE: u16 = WITHDRAWAL_BUNDLE_MAX_AGE; // 5
const USED_SIDECHAIN_SLOT_ACTIVATION_THRESHOLD: u16 = USED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE / 2;

const UNUSED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE: u16 = 10;
const UNUSED_SIDECHAIN_SLOT_ACTIVATION_MAX_FAILS: u16 = 5;
const UNUSED_SIDECHAIN_SLOT_ACTIVATION_THRESHOLD: u16 =
    UNUSED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE - UNUSED_SIDECHAIN_SLOT_ACTIVATION_MAX_FAILS;

/// Unit key. LMDB can't use zero-sized keys, so this encodes to a single byte
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub struct UnitKey;

impl<'de> Deserialize<'de> for UnitKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Deserialize any byte (ignoring it) and return UnitKey
        let _ = u8::deserialize(deserializer)?;
        Ok(UnitKey)
    }
}

impl Serialize for UnitKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // Always serialize to the same arbitrary byte
        serializer.serialize_u8(0x69)
    }
}

#[derive(Clone)]
pub struct Bip300 {
    env: Env,

    data_hash_to_sidechain_proposal:
        Database<SerdeBincode<Hash256>, SerdeBincode<SidechainProposal>>,
    sidechain_number_to_pending_m6ids: Database<SerdeBincode<u8>, SerdeBincode<Vec<PendingM6id>>>,
    sidechain_number_to_sidechain: Database<SerdeBincode<u8>, SerdeBincode<Sidechain>>,
    sidechain_number_to_ctip: Database<SerdeBincode<u8>, SerdeBincode<Ctip>>,
    _previous_votes: Database<SerdeBincode<UnitKey>, SerdeBincode<Vec<Hash256>>>,
    _leading_by_50: Database<SerdeBincode<UnitKey>, SerdeBincode<Vec<Hash256>>>,
    current_block_height: Database<SerdeBincode<UnitKey>, SerdeBincode<u32>>,
    current_chain_tip: Database<SerdeBincode<UnitKey>, SerdeBincode<Hash256>>,
    sidechain_number_sequence_number_to_treasury_utxo:
        Database<SerdeBincode<(u8, u64)>, SerdeBincode<TreasuryUtxo>>,
    sidechain_number_to_treasury_utxo_count: Database<SerdeBincode<u8>, SerdeBincode<u64>>,
    block_height_to_accepted_bmm_block_hashes:
        Database<SerdeBincode<u32>, SerdeBincode<Vec<Hash256>>>,
}

impl Bip300 {
    const NUM_DBS: u32 = 11;

    pub fn new(datadir: &Path) -> Result<Self> {
        let db_dir = datadir.join("./bip300301_enforcer.mdb");
        std::fs::create_dir_all(&db_dir).into_diagnostic()?;
        let env = EnvOpenOptions::new()
            .max_dbs(Self::NUM_DBS)
            .open(db_dir)
            .into_diagnostic()?;
        let data_hash_to_sidechain_proposal = env
            .create_database(Some("data_hash_to_sidechain_proposal"))
            .into_diagnostic()?;
        let sidechain_number_to_pending_m6ids = env
            .create_database(Some("sidechain_number_to_pending_m6ids"))
            .into_diagnostic()?;
        let sidechain_number_to_sidechain = env
            .create_database(Some("sidechain_number_to_sidechain"))
            .into_diagnostic()?;
        let sidechain_number_to_ctip = env
            .create_database(Some("sidechain_number_to_ctip"))
            .into_diagnostic()?;
        let previous_votes = env
            .create_database(Some("previous_votes"))
            .into_diagnostic()?;
        let leading_by_50 = env
            .create_database(Some("leading_by_50"))
            .into_diagnostic()?;
        let current_block_height = env
            .create_database(Some("current_block_height"))
            .into_diagnostic()?;
        let current_chain_tip = env
            .create_database(Some("current_chain_tip"))
            .into_diagnostic()?;
        let sidechain_number_sequence_number_to_treasury_utxo = env
            .create_database(Some("sidechain_number_sequence_number_to_treasury_utxo"))
            .into_diagnostic()?;
        let sidechain_number_to_treasury_utxo_count = env
            .create_database(Some("sidechain_number_to_treasury_utxo_count"))
            .into_diagnostic()?;
        let block_height_to_accepted_bmm_block_hashes = env
            .create_database(Some("block_height_to_accepted_bmm_block_hashes"))
            .into_diagnostic()?;
        Ok(Self {
            env,
            data_hash_to_sidechain_proposal,
            sidechain_number_to_pending_m6ids,
            sidechain_number_to_sidechain,
            sidechain_number_to_ctip,
            _previous_votes: previous_votes,
            _leading_by_50: leading_by_50,
            current_block_height,
            current_chain_tip,
            sidechain_number_sequence_number_to_treasury_utxo,
            sidechain_number_to_treasury_utxo_count,

            block_height_to_accepted_bmm_block_hashes,
        })
    }

    // See https://github.com/LayerTwo-Labs/bip300_bip301_specifications/blob/master/bip300.md#m1-1
    fn handle_m1_propose_sidechain(
        &self,
        rwtxn: &mut RwTxn,
        proposal_height: u32,
        sidechain_number: u8,
        data: Vec<u8>,
    ) -> Result<()> {
        let data_hash: Hash256 = sha256d(&data);
        if self
            .data_hash_to_sidechain_proposal
            .get(rwtxn, &data_hash)
            .into_diagnostic()?
            .is_some()
        {
            // If a proposal with the same data_hash already exists,
            // we ignore this M1.
            //
            // Having the same data_hash means that data is the same as well.
            //
            // Without this rule it would be possible for the miners to reset the vote count for
            // any sidechain proposal at any point.
            return Ok(());
        }
        let sidechain_proposal = SidechainProposal {
            sidechain_number,
            data,
            vote_count: 0,
            proposal_height,
        };
        self.data_hash_to_sidechain_proposal
            .put(rwtxn, &data_hash, &sidechain_proposal)
            .into_diagnostic()
    }

    // See https://github.com/LayerTwo-Labs/bip300_bip301_specifications/blob/master/bip300.md#m2-1
    fn handle_m2_ack_sidechain(
        &self,
        rwtxn: &mut RwTxn,
        height: u32,
        sidechain_number: u8,
        data_hash: [u8; 32],
    ) -> Result<()> {
        let sidechain_proposal = self
            .data_hash_to_sidechain_proposal
            .get(rwtxn, &data_hash)
            .into_diagnostic()?;
        let Some(mut sidechain_proposal) = sidechain_proposal else {
            return Ok(());
        };
        if sidechain_proposal.sidechain_number != sidechain_number {
            return Ok(());
        }
        sidechain_proposal.vote_count += 1;
        self.data_hash_to_sidechain_proposal
            .put(rwtxn, &data_hash, &sidechain_proposal)
            .into_diagnostic()?;

        let sidechain_proposal_age = height - sidechain_proposal.proposal_height;

        let sidechain_slot_is_used = self
            .sidechain_number_to_sidechain
            .get(rwtxn, &sidechain_number)
            .into_diagnostic()?
            .is_some();

        let new_sidechain_activated = {
            sidechain_slot_is_used
                && sidechain_proposal.vote_count > USED_SIDECHAIN_SLOT_ACTIVATION_THRESHOLD
                && sidechain_proposal_age <= USED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE as u32
        } || {
            !sidechain_slot_is_used
                && sidechain_proposal.vote_count > UNUSED_SIDECHAIN_SLOT_ACTIVATION_THRESHOLD
                && sidechain_proposal_age <= UNUSED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE as u32
        };

        if new_sidechain_activated {
            println!(
                "sidechain {sidechain_number} in slot {} was activated",
                String::from_utf8(sidechain_proposal.data.clone()).into_diagnostic()?,
            );
            let sidechain = Sidechain {
                sidechain_number,
                data: sidechain_proposal.data,
                proposal_height: sidechain_proposal.proposal_height,
                activation_height: height,
                vote_count: sidechain_proposal.vote_count,
            };
            self.sidechain_number_to_sidechain
                .put(rwtxn, &sidechain_number, &sidechain)
                .into_diagnostic()?;
            self.data_hash_to_sidechain_proposal
                .delete(rwtxn, &data_hash)
                .into_diagnostic()?;
        }
        Ok(())
    }

    fn handle_failed_sidechain_proposals(&self, rwtxn: &mut RwTxn, height: u32) -> Result<()> {
        let failed_proposals: Vec<_> = self
            .data_hash_to_sidechain_proposal
            .iter(rwtxn)
            .into_diagnostic()?
            .map(|item| item.into_diagnostic())
            .transpose_into_fallible()
            .filter_map(|(data_hash, sidechain_proposal)| {
                let sidechain_proposal_age = height - sidechain_proposal.proposal_height;
                let sidechain_slot_is_used = self
                    .sidechain_number_to_sidechain
                    .get(rwtxn, &sidechain_proposal.sidechain_number)
                    .into_diagnostic()?
                    .is_some();
                // FIXME: Do we need to check that the vote_count is below the threshold, or is it
                // enough to check that the max age was exceeded?
                let failed = sidechain_slot_is_used
                    && sidechain_proposal_age > USED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE as u32
                    || !sidechain_slot_is_used
                        && sidechain_proposal_age > UNUSED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE as u32;
                if failed {
                    Ok(Some(data_hash))
                } else {
                    Ok(None)
                }
            })
            .collect()?;
        for failed_proposal_data_hash in &failed_proposals {
            self.data_hash_to_sidechain_proposal
                .delete(rwtxn, failed_proposal_data_hash)
                .into_diagnostic()?;
        }
        Ok(())
    }

    fn handle_m3_propose_bundle(
        &self,
        rwtxn: &mut RwTxn,
        sidechain_number: u8,
        m6id: [u8; 32],
    ) -> Result<()> {
        if self
            .sidechain_number_to_sidechain
            .get(rwtxn, &sidechain_number)
            .into_diagnostic()?
            .is_none()
        {
            return Err(miette!(
                "can't propose bundle, sidechain slot {sidechain_number} is inactive"
            ));
        }
        let pending_m6ids = self
            .sidechain_number_to_pending_m6ids
            .get(rwtxn, &sidechain_number)
            .into_diagnostic()?;
        let mut pending_m6ids = match pending_m6ids {
            Some(pending_m6ids) => pending_m6ids,
            None => vec![],
        };
        let pending_m6id = PendingM6id {
            m6id,
            vote_count: 0,
        };
        pending_m6ids.push(pending_m6id);
        self.sidechain_number_to_pending_m6ids
            .put(rwtxn, &sidechain_number, &pending_m6ids)
            .into_diagnostic()
    }

    fn handle_m4_votes(&self, rwtxn: &mut RwTxn, upvotes: &[u16]) -> Result<()> {
        for (sidechain_number, vote) in upvotes.iter().enumerate() {
            let vote = *vote;
            if vote == ABSTAIN_TWO_BYTES {
                continue;
            }
            let pending_m6ids = self
                .sidechain_number_to_pending_m6ids
                .get(rwtxn, &(sidechain_number as u8))
                .into_diagnostic()?;
            let Some(mut pending_m6ids) = pending_m6ids else {
                continue;
            };
            if vote == ALARM_TWO_BYTES {
                for pending_m6id in &mut pending_m6ids {
                    if pending_m6id.vote_count > 0 {
                        pending_m6id.vote_count -= 1;
                    }
                }
            } else if let Some(pending_m6id) = pending_m6ids.get_mut(vote as usize) {
                pending_m6id.vote_count += 1;
            }
            self.sidechain_number_to_pending_m6ids
                .put(rwtxn, &(sidechain_number as u8), &pending_m6ids)
                .into_diagnostic()?;
        }
        Ok(())
    }

    fn handle_m4_ack_bundles(&self, rwtxn: &mut RwTxn, m4: &M4AckBundles) -> Result<()> {
        match m4 {
            M4AckBundles::LeadingBy50 => {
                todo!();
            }
            M4AckBundles::RepeatPrevious => {
                todo!();
            }
            M4AckBundles::OneByte { upvotes } => {
                let upvotes: Vec<u16> = upvotes.iter().map(|vote| *vote as u16).collect();
                self.handle_m4_votes(rwtxn, &upvotes)
            }
            M4AckBundles::TwoBytes { upvotes } => self.handle_m4_votes(rwtxn, upvotes),
        }
    }

    fn handle_failed_m6ids(&self, rwtxn: &mut RwTxn) -> Result<()> {
        let mut updated_slots = HashMap::new();
        for item in self
            .sidechain_number_to_pending_m6ids
            .iter(rwtxn)
            .into_diagnostic()?
        {
            let (sidechain_number, pending_m6ids) = item.into_diagnostic()?;
            let mut failed_m6ids = HashSet::new();
            for pending_m6id in &pending_m6ids {
                if pending_m6id.vote_count > WITHDRAWAL_BUNDLE_MAX_AGE {
                    failed_m6ids.insert(pending_m6id.m6id);
                }
            }
            let pending_m6ids: Vec<_> = pending_m6ids
                .into_iter()
                .filter(|pending_m6id| !failed_m6ids.contains(&pending_m6id.m6id))
                .collect();
            updated_slots.insert(sidechain_number, pending_m6ids);
        }
        for (sidechain_number, pending_m6ids) in updated_slots {
            self.sidechain_number_to_pending_m6ids
                .put(rwtxn, &sidechain_number, &pending_m6ids)
                .into_diagnostic()?;
        }
        Ok(())
    }

    fn get_old_ctip(&self, rotxn: &RoTxn, sidechain_number: u8) -> Result<Option<Ctip>> {
        Ok(self
            .sidechain_number_to_ctip
            .get(rotxn, &sidechain_number)
            .into_diagnostic()?)
    }

    fn handle_m5_m6(&self, rwtxn: &mut RwTxn, transaction: &Transaction) -> Result<()> {
        // TODO: Check that there is only one OP_DRIVECHAIN per sidechain slot.
        let (sidechain_number, new_ctip, new_total_value) = {
            let output = &transaction.output[0];
            // If OP_DRIVECHAIN script is invalid,
            // for example if it is missing OP_TRUE at the end,
            // it will just be ignored.
            if let Ok((_input, sidechain_number)) =
                parse_op_drivechain(&output.script_pubkey.to_bytes())
            {
                let new_ctip = OutPoint {
                    txid: transaction.txid(),
                    vout: 0,
                };
                let new_total_value = output.value;

                (sidechain_number, new_ctip, new_total_value)
            } else {
                return Ok(());
            }
        };

        let address = {
            let output = &transaction.output[1];
            let script = output.script_pubkey.to_bytes();
            if script[0] == OP_RETURN.to_u8() {
                Some(script[1..].to_vec())
            } else {
                None
            }
        };

        let old_total_value = {
            if let Some(old_ctip) = self.get_old_ctip(rwtxn, sidechain_number)? {
                let old_ctip_found = transaction
                    .input
                    .iter()
                    .any(|input| input.previous_output == old_ctip.outpoint);
                if !old_ctip_found {
                    return Err(miette!(
                        "old ctip wasn't spent for sidechain {sidechain_number}"
                    ));
                }
                old_ctip.value
            } else {
                0
            }
        };
        let treasury_utxo = TreasuryUtxo {
            outpoint: new_ctip,
            address,
            total_value: new_total_value,
            previous_total_value: old_total_value,
        };
        dbg!(&treasury_utxo);

        // M6
        if new_total_value < old_total_value {
            let mut m6_valid = false;
            let m6id = m6_to_id(transaction, old_total_value);
            if let Some(pending_m6ids) = self
                .sidechain_number_to_pending_m6ids
                .get(rwtxn, &sidechain_number)
                .into_diagnostic()?
            {
                for pending_m6id in &pending_m6ids {
                    if pending_m6id.m6id == m6id
                        && pending_m6id.vote_count > WITHDRAWAL_BUNDLE_INCLUSION_THRESHOLD
                    {
                        m6_valid = true;
                    }
                }
                if m6_valid {
                    let pending_m6ids: Vec<_> = pending_m6ids
                        .into_iter()
                        .filter(|pending_m6id| pending_m6id.m6id != m6id)
                        .collect();
                    self.sidechain_number_to_pending_m6ids
                        .put(rwtxn, &sidechain_number, &pending_m6ids)
                        .into_diagnostic()?;
                }
            }
            if !m6_valid {
                return Err(miette!("m6 is invalid"));
            }
        }
        let mut treasury_utxo_count = self
            .sidechain_number_to_treasury_utxo_count
            .get(rwtxn, &sidechain_number)
            .into_diagnostic()?
            .unwrap_or(0);
        // Sequence numbers begin at 0, so the total number of treasury utxos in the database
        // gives us the *next* sequence number.
        let sequence_number = treasury_utxo_count;
        self.sidechain_number_sequence_number_to_treasury_utxo
            .put(rwtxn, &(sidechain_number, sequence_number), &treasury_utxo)
            .into_diagnostic()?;
        treasury_utxo_count += 1;
        self.sidechain_number_to_treasury_utxo_count
            .put(rwtxn, &sidechain_number, &treasury_utxo_count)
            .into_diagnostic()?;
        let new_ctip = Ctip {
            outpoint: new_ctip,
            value: new_total_value,
        };
        self.sidechain_number_to_ctip
            .put(rwtxn, &sidechain_number, &new_ctip)
            .into_diagnostic()?;
        Ok(())
    }

    fn handle_m8(
        transaction: &Transaction,
        accepted_bmm_requests: &HashSet<(u8, [u8; 32])>,
        prev_mainchain_block_hash: &[u8; 32],
    ) -> Result<()> {
        let output = &transaction.output[0];
        let script = output.script_pubkey.to_bytes();

        if let Ok((_input, bmm_request)) = parse_m8_bmm_request(&script) {
            if !accepted_bmm_requests.contains(&(
                bmm_request.sidechain_number,
                bmm_request.sidechain_block_hash,
            )) {
                return Err(miette!(
                    "can't include a BMM request that was not accepted by the miners"
                ));
            }
            if bmm_request.prev_mainchain_block_hash != *prev_mainchain_block_hash {
                return Err(miette!("BMM request expired"));
            }
        }
        Ok(())
    }

    pub fn connect_block(&self, rwtxn: &mut RwTxn, block: &Block, height: u32) -> Result<()> {
        // TODO: Check that there are no duplicate M2s.
        let coinbase = &block.txdata[0];
        let mut bmmed_sidechain_slots = HashSet::new();
        let mut accepted_bmm_requests = HashSet::new();
        for output in &coinbase.output {
            let Ok((_, message)) = parse_coinbase_script(&output.script_pubkey) else {
                continue;
            };
            match message {
                CoinbaseMessage::M1ProposeSidechain {
                    sidechain_number,
                    data,
                } => {
                    /*
                    println!(
                        "Propose sidechain number {sidechain_number} with data \"{}\"",
                        String::from_utf8(data.clone()).into_diagnostic()?,
                    );
                    */
                    self.handle_m1_propose_sidechain(
                        rwtxn,
                        height,
                        sidechain_number,
                        data.clone(),
                    )?;
                }
                CoinbaseMessage::M2AckSidechain {
                    sidechain_number,
                    data_hash,
                } => {
                    /*
                    println!(
                        "Ack sidechain number {sidechain_number} with hash {}",
                        hex::encode(data_hash)
                    );
                    */
                    self.handle_m2_ack_sidechain(rwtxn, height, sidechain_number, data_hash)?;
                }
                CoinbaseMessage::M3ProposeBundle {
                    sidechain_number,
                    bundle_txid,
                } => {
                    self.handle_m3_propose_bundle(rwtxn, sidechain_number, bundle_txid)?;
                }
                CoinbaseMessage::M4AckBundles(m4) => {
                    self.handle_m4_ack_bundles(rwtxn, &m4)?;
                }
                CoinbaseMessage::M7BmmAccept {
                    sidechain_number,
                    sidechain_block_hash,
                } => {
                    if bmmed_sidechain_slots.contains(&sidechain_number) {
                        return Err(miette!(
                            "more than one block bmmed in siidechain slot {sidechain_number}"
                        ));
                    }
                    bmmed_sidechain_slots.insert(sidechain_number);
                    accepted_bmm_requests.insert((sidechain_number, sidechain_block_hash));
                }
            }
        }

        {
            let accepted_bmm_block_hashes: Vec<_> = accepted_bmm_requests
                .iter()
                .map(|(_sidechain_number, hash)| *hash)
                .collect();
            self.block_height_to_accepted_bmm_block_hashes
                .put(rwtxn, &height, &accepted_bmm_block_hashes)
                .into_diagnostic()?;
            const MAX_BMM_BLOCK_DEPTH: usize = 6 * 24 * 7; // 1008 blocks = ~1 week of time
            if self
                .block_height_to_accepted_bmm_block_hashes
                .len(rwtxn)
                .into_diagnostic()?
                > MAX_BMM_BLOCK_DEPTH
            {
                let (block_height, _) = self
                    .block_height_to_accepted_bmm_block_hashes
                    .first(rwtxn)
                    .into_diagnostic()?
                    .unwrap();
                self.block_height_to_accepted_bmm_block_hashes
                    .delete(rwtxn, &block_height)
                    .into_diagnostic()?;
            }
        }

        self.handle_failed_sidechain_proposals(rwtxn, height)?;
        self.handle_failed_m6ids(rwtxn)?;

        let prev_mainchain_block_hash = block.header.prev_blockhash.as_byte_array();

        for transaction in &block.txdata[1..] {
            self.handle_m5_m6(rwtxn, transaction)?;
            Self::handle_m8(
                transaction,
                &accepted_bmm_requests,
                prev_mainchain_block_hash,
            )?;
        }
        Ok(())
    }

    // TODO: Add unit tests ensuring that `connect_block` and `disconnect_block` are inverse
    // operations.
    pub fn _disconnect_block(&self, _block: &Block) -> Result<()> {
        todo!();
    }

    pub fn _is_transaction_valid(&self, _transaction: &Transaction) -> Result<()> {
        todo!();
    }

    fn initial_sync(&self, main_client: &Client) -> Result<()> {
        let mut txn = self.env.write_txn().into_diagnostic()?;
        let mut height = self
            .current_block_height
            .get(&txn, &UnitKey)
            .into_diagnostic()?
            .unwrap_or(0);
        let main_block_height: u32 = main_client
            .send_request("getblockcount", &[])
            .into_diagnostic()?
            .ok_or(miette!("failed to get block count"))?;
        while height < main_block_height {
            let block_hash: String = main_client
                .send_request("getblockhash", &[json!(height)])
                .into_diagnostic()?
                .ok_or(miette!("failed to get block hash"))?;
            let block: String = main_client
                .send_request("getblock", &[json!(block_hash), json!(0)])
                .into_diagnostic()?
                .ok_or(miette!("failed to get block"))?;
            let block_bytes = hex::decode(&block).unwrap();
            let mut cursor = Cursor::new(block_bytes);
            let block = Block::consensus_decode(&mut cursor).unwrap();
            self.connect_block(&mut txn, &block, height)?;
            {
                /*
                main_client
                    .send_request("invalidateblock", &[json!(block_hash)])
                    .into_diagnostic()?
                    .ok_or(miette!("failed to invalidate block"))?;
                */
            }
            height += 1;
            let block_hash = hex::decode(block_hash)
                .into_diagnostic()?
                .try_into()
                .unwrap();
            self.current_chain_tip
                .put(&mut txn, &UnitKey, &block_hash)
                .into_diagnostic()?;
        }
        self.current_block_height
            .put(&mut txn, &UnitKey, &height)
            .into_diagnostic()?;
        txn.commit().into_diagnostic()?;
        Ok(())
    }

    // FIXME: Rewrite all of this to be more readable.
    /// Single iteration of the task loop
    fn task_loop_once(&self, main_client: &Client) -> Result<()> {
        let mut txn = self.env.write_txn().into_diagnostic()?;
        let mut height = self
            .current_block_height
            .get(&txn, &UnitKey)
            .into_diagnostic()?
            .unwrap_or(0);
        let main_block_height: u32 = main_client
            .send_request("getblockcount", &[])
            .into_diagnostic()?
            .ok_or(miette!("failed to get block count"))?;
        if main_block_height == height {
            return Ok(());
        }
        println!("Block height: {main_block_height}");

        while height < main_block_height {
            let block_hash: String = main_client
                .send_request("getblockhash", &[json!(height)])
                .into_diagnostic()?
                .ok_or(miette!("failed to get block hash"))?;
            let prev_blockhash = BlockHash::from_str(&block_hash).unwrap();

            println!("Mainchain tip: {prev_blockhash}");

            let block: String = main_client
                .send_request("getblock", &[json!(block_hash), json!(0)])
                .into_diagnostic()?
                .ok_or(miette!("failed to get block"))?;
            let block_bytes = hex::decode(&block).unwrap();
            let mut cursor = Cursor::new(block_bytes);
            let block = Block::consensus_decode(&mut cursor).unwrap();

            if self.connect_block(&mut txn, &block, height).is_err() {
                /*
                main_client
                    .send_request("invalidateblock", &[json!(block_hash)])
                    .into_diagnostic()?
                    .ok_or(miette!("failed to invalidate block"))?;
                */
            }
            println!();

            // check for new block
            // validate block
            // if invalid invalidate
            // if valid connect
            // wait 1 second
            height += 1;
            let block_hash = hex::decode(block_hash)
                .into_diagnostic()?
                .try_into()
                .unwrap();
            self.current_chain_tip
                .put(&mut txn, &UnitKey, &block_hash)
                .into_diagnostic()?;
        }
        self.current_block_height
            .put(&mut txn, &UnitKey, &height)
            .into_diagnostic()?;
        txn.commit().into_diagnostic()?;
        Ok(())
    }

    async fn task(&self, main_datadir: PathBuf, main_address: SocketAddr) -> Result<()> {
        let main_client = &create_client(main_datadir, main_address)?;
        self.initial_sync(&main_client)?;
        let interval = interval(Duration::from_secs(1));
        IntervalStream::new(interval)
            .map(Ok)
            .try_for_each(move |_: Instant| async move { self.task_loop_once(main_client) })
            .await
    }

    pub fn run(&self, main_datadir: &Path, main_address: SocketAddr) -> JoinHandle<()> {
        let main_datadir = main_datadir.to_owned();
        let main_address = main_address.to_owned();
        let this = self.clone();
        tokio::task::spawn(async move {
            this.task(main_datadir, main_address)
                .unwrap_or_else(|err| eprintln!("{err:#}"))
                .await
        })
    }

    pub fn get_sidechain_proposals(&self) -> Result<Vec<(Hash256, SidechainProposal)>> {
        let txn = self.env.read_txn().into_diagnostic()?;
        let mut sidechain_proposals = vec![];
        for sidechain_proposal in self
            .data_hash_to_sidechain_proposal
            .iter(&txn)
            .into_diagnostic()?
        {
            let (data_hash, sidechain_proposal) = sidechain_proposal.into_diagnostic()?;
            sidechain_proposals.push((data_hash, sidechain_proposal));
        }
        Ok(sidechain_proposals)
    }

    pub fn get_sidechains(&self) -> Result<Vec<Sidechain>> {
        let txn = self.env.read_txn().into_diagnostic()?;
        let mut sidechains = vec![];
        for sidechain in self
            .sidechain_number_to_sidechain
            .iter(&txn)
            .into_diagnostic()?
        {
            let (_sidechain_number, sidechain) = sidechain.into_diagnostic()?;
            sidechains.push(sidechain);
        }
        Ok(sidechains)
    }

    pub fn get_ctip_sequence_number(&self, sidechain_number: u8) -> Result<Option<u64>> {
        let rotxn = self.env.read_txn().into_diagnostic()?;
        let treasury_utxo_count = self
            .sidechain_number_to_treasury_utxo_count
            .get(&rotxn, &sidechain_number)
            .into_diagnostic()?;
        let sequence_number = match treasury_utxo_count {
            // Sequence numbers begin at 0, so the total number of treasury utxos in the database
            // gives us the *next* sequence number.
            //
            // In order to get the current sequence number we decrement it by one.
            Some(treasury_utxo_count) => Some(treasury_utxo_count - 1),
            None => None,
        };
        Ok(sequence_number)
    }

    pub fn get_ctip(&self, sidechain_number: u8) -> Result<Option<Ctip>> {
        let txn = self.env.read_txn().into_diagnostic()?;
        let ctip = self
            .sidechain_number_to_ctip
            .get(&txn, &sidechain_number)
            .into_diagnostic()?;
        Ok(ctip)
    }

    pub fn get_main_block_height(&self) -> Result<u32> {
        let txn = self.env.read_txn().into_diagnostic()?;
        let height = self
            .current_block_height
            .get(&txn, &UnitKey)
            .into_diagnostic()?
            .unwrap_or(0);
        Ok(height)
    }

    pub fn get_main_chain_tip(&self) -> Result<[u8; 32]> {
        let txn = self.env.read_txn().into_diagnostic()?;
        let block_hash = self
            .current_chain_tip
            .get(&txn, &UnitKey)
            .into_diagnostic()?
            .unwrap_or([0; 32]);
        Ok(block_hash)
    }

    pub fn get_deposits(&self, sidechain_number: u8) -> Result<Vec<Deposit>> {
        let txn = self.env.read_txn().into_diagnostic()?;
        let treasury_utxos_range = self
            .sidechain_number_sequence_number_to_treasury_utxo
            .range(&txn, &((sidechain_number, 0)..(sidechain_number, u64::MAX)))
            .into_diagnostic()?;
        let mut deposits = vec![];
        for item in treasury_utxos_range {
            let ((_, sequence_number), treasury_utxo) = item.into_diagnostic()?;
            if treasury_utxo.total_value > treasury_utxo.previous_total_value
                && treasury_utxo.address.is_some()
            {
                let deposit = Deposit {
                    sequence_number,
                    address: treasury_utxo.address.unwrap(),
                    value: treasury_utxo.total_value - treasury_utxo.previous_total_value,
                };
                deposits.push(deposit);
            }
        }
        Ok(deposits)
    }

    pub fn get_accepted_bmm_hashes(&self) -> Result<Vec<(u32, Vec<[u8; 32]>)>> {
        let mut block_height_accepted_bmm_hashes = vec![];
        let txn = self.env.read_txn().into_diagnostic()?;
        for item in self
            .block_height_to_accepted_bmm_block_hashes
            .iter(&txn)
            .into_diagnostic()?
        {
            let (block_height, accepted_bmm_hashes) = item.into_diagnostic()?;
            block_height_accepted_bmm_hashes.push((block_height, accepted_bmm_hashes.to_vec()));
        }
        Ok(block_height_accepted_bmm_hashes)
    }
}

fn create_client(main_datadir: PathBuf, main_address: SocketAddr) -> Result<Client> {
    let cookie_path = main_datadir.join("regtest/.cookie");
    let auth = std::fs::read_to_string(cookie_path.clone()).map_err(|err| {
        miette!(
            "unable to read bitcoind cookie at {}: {}",
            cookie_path.display(),
            err
        )
    })?;

    let mut auth = auth.split(':');
    let user = auth
        .next()
        .ok_or(miette!("failed to get rpcuser"))?
        .to_string();
    let password = auth
        .next()
        .ok_or(miette!("failed to get rpcpassword"))?
        .to_string();
    Ok(Client {
        host: main_address.ip().to_string(),
        port: 18443,
        user,
        password,
        id: "mainchain".into(),
    })
}
