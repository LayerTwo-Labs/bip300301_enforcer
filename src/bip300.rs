use std::path::Path;

use crate::messages::{
    parse_coinbase_script, sha256d, CoinbaseMessage, M4AckBundles, ABSTAIN_ONE_BYTE,
    ABSTAIN_TWO_BYTES, ALARM_ONE_BYTE, ALARM_TWO_BYTES, OP_DRIVECHAIN,
};
use crate::types::*;
use bitcoin::opcodes::all::{OP_PUSHBYTES_1, OP_RETURN};
use bitcoin::opcodes::OP_TRUE;
use bitcoin::{Block, OutPoint, Transaction};
use miette::{miette, IntoDiagnostic, Result};

use heed::Database;
use heed::{types::*, Env, EnvOpenOptions};

pub struct Bip300 {
    env: Env,

    data_hash_to_sidechain_proposal:
        Database<SerdeBincode<Hash256>, SerdeBincode<SidechainProposal>>,
    sidechain_number_to_bundles: Database<SerdeBincode<u8>, SerdeBincode<Vec<Bundle>>>,
    sidechain_number_to_sidechain: Database<SerdeBincode<u8>, SerdeBincode<Sidechain>>,
    sidechain_number_to_ctip: Database<SerdeBincode<u8>, SerdeBincode<Ctip>>,
    previous_votes: Database<Unit, SerdeBincode<Vec<Hash256>>>,
    leading_by_50: Database<Unit, SerdeBincode<Vec<Hash256>>>,

    deposits: Database<SerdeBincode<(u8, OutPoint)>, SerdeBincode<Deposit>>,
}

impl Bip300 {
    const NUM_DBS: u32 = 7;

    pub fn new(datadir: &Path) -> Result<Self> {
        let env = EnvOpenOptions::new()
            .max_dbs(Self::NUM_DBS)
            .open(datadir.join("./bip300.mdb"))
            .into_diagnostic()?;
        let data_hash_to_sidechain_proposal = env
            .create_database(Some("data_hash_to_sidechain_proposal"))
            .into_diagnostic()?;
        let sidechain_number_to_bundles = env
            .create_database(Some("sidechain_number_to_bundles"))
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
        let deposits = env.create_database(Some("deposits")).into_diagnostic()?;
        Ok(Self {
            env,
            data_hash_to_sidechain_proposal,
            sidechain_number_to_bundles,
            sidechain_number_to_sidechain,
            sidechain_number_to_ctip,
            previous_votes,
            leading_by_50,
            deposits,
        })
    }

    pub fn connect_block(&self, block: &Block, height: u32) -> Result<()> {
        println!("connect block");
        // TODO: Check that there are no duplicate M2s.
        let coinbase = &block.txdata[0];

        let mut txn = self.env.write_txn().into_diagnostic()?;
        for output in &coinbase.output {
            match &parse_coinbase_script(&output.script_pubkey) {
                Ok((_, message)) => {
                    match message {
                        CoinbaseMessage::M1ProposeSidechain {
                            sidechain_number,
                            data,
                        } => {
                            let data_hash: Hash256 = sha256d(&data);
                            if self
                                .data_hash_to_sidechain_proposal
                                .get(&txn, &data_hash)
                                .into_diagnostic()?
                                .is_some()
                            {
                                continue;
                            }
                            let sidechain_proposal = SidechainProposal {
                                sidechain_number: *sidechain_number,
                                data: data.clone(),
                                vote_count: 0,
                                proposal_height: height,
                            };
                            self.data_hash_to_sidechain_proposal
                                .put(&mut txn, &data_hash, &sidechain_proposal)
                                .into_diagnostic()?;
                        }
                        CoinbaseMessage::M2AckSidechain {
                            sidechain_number,
                            data_hash,
                        } => {
                            let sidechain_proposal = self
                                .data_hash_to_sidechain_proposal
                                .get(&txn, data_hash)
                                .into_diagnostic()?;
                            if let Some(mut sidechain_proposal) = sidechain_proposal {
                                // Does it make sense to check for sidechain number?
                                if sidechain_proposal.sidechain_number == *sidechain_number {
                                    sidechain_proposal.vote_count += 1;

                                    self.data_hash_to_sidechain_proposal
                                        .put(&mut txn, data_hash, &sidechain_proposal)
                                        .into_diagnostic()?;

                                    const USED_MAX_AGE: u16 = 26_300;
                                    const USED_THRESHOLD: u16 = 13_150;

                                    const UNUSED_MAX_AGE: u16 = 2016;
                                    const UNUSED_THRESHOLD: u16 = UNUSED_MAX_AGE - 201;

                                    let sidechain_proposal_age =
                                        height - sidechain_proposal.proposal_height;

                                    let used = self
                                        .sidechain_number_to_sidechain
                                        .get(&txn, &sidechain_proposal.sidechain_number)
                                        .into_diagnostic()?
                                        .is_some();

                                    let failed = used
                                        && sidechain_proposal_age > USED_MAX_AGE as u32
                                        && sidechain_proposal.vote_count <= USED_THRESHOLD
                                        || !used
                                            && sidechain_proposal_age > UNUSED_MAX_AGE as u32
                                            && sidechain_proposal.vote_count <= UNUSED_THRESHOLD;

                                    let succeeded = used
                                        && sidechain_proposal.vote_count > USED_THRESHOLD
                                        || !used
                                            && sidechain_proposal.vote_count > UNUSED_THRESHOLD;

                                    if failed {
                                        self.data_hash_to_sidechain_proposal
                                            .delete(&mut txn, data_hash)
                                            .into_diagnostic()?;
                                    } else if succeeded {
                                        if sidechain_proposal.vote_count > USED_THRESHOLD {
                                            let sidechain = Sidechain {
                                                sidechain_number: sidechain_proposal
                                                    .sidechain_number,
                                                data: sidechain_proposal.data,
                                                proposal_height: sidechain_proposal.proposal_height,
                                                activation_height: height,
                                                vote_count: sidechain_proposal.vote_count,
                                            };
                                            self.sidechain_number_to_sidechain
                                                .put(
                                                    &mut txn,
                                                    &sidechain.sidechain_number,
                                                    &sidechain,
                                                )
                                                .into_diagnostic()?;
                                            self.data_hash_to_sidechain_proposal
                                                .delete(&mut txn, &data_hash)
                                                .into_diagnostic()?;
                                        }
                                    };
                                }
                            }
                        }
                        CoinbaseMessage::M3ProposeBundle {
                            sidechain_number,
                            bundle_txid,
                        } => {
                            let bundles = self
                                .sidechain_number_to_bundles
                                .get(&txn, &sidechain_number)
                                .into_diagnostic()?;
                            if let Some(mut bundles) = bundles {
                                let bundle = Bundle {
                                    bundle_txid: *bundle_txid,
                                    vote_count: 0,
                                };
                                bundles.push(bundle);
                                self.sidechain_number_to_bundles
                                    .put(&mut txn, &sidechain_number, &bundles)
                                    .into_diagnostic()?;
                            }
                        }
                        CoinbaseMessage::M4AckBundles(m4) => match m4 {
                            M4AckBundles::LeadingBy50 => {
                                todo!();
                            }
                            M4AckBundles::RepeatPrevious => {
                                todo!();
                            }
                            M4AckBundles::OneByte { upvotes } => {
                                for (sidechain_number, vote) in upvotes.iter().enumerate() {
                                    if *vote == ABSTAIN_ONE_BYTE {
                                        continue;
                                    }
                                    let bundles = self
                                        .sidechain_number_to_bundles
                                        .get(&txn, &(sidechain_number as u8))
                                        .into_diagnostic()?;
                                    if let Some(mut bundles) = bundles {
                                        if *vote == ALARM_ONE_BYTE {
                                            for bundle in &mut bundles {
                                                if bundle.vote_count > 0 {
                                                    bundle.vote_count -= 1;
                                                }
                                            }
                                        } else if let Some(bundle) = bundles.get_mut(*vote as usize)
                                        {
                                            bundle.vote_count += 1;
                                        }
                                        self.sidechain_number_to_bundles
                                            .put(&mut txn, &(sidechain_number as u8), &bundles)
                                            .into_diagnostic()?;
                                    }
                                }
                            }
                            M4AckBundles::TwoBytes { upvotes } => {
                                for (sidechain_number, vote) in upvotes.iter().enumerate() {
                                    if *vote == ABSTAIN_TWO_BYTES {
                                        continue;
                                    }
                                    let bundles = self
                                        .sidechain_number_to_bundles
                                        .get(&txn, &(sidechain_number as u8))
                                        .into_diagnostic()?;
                                    if let Some(mut bundles) = bundles {
                                        if *vote == ALARM_TWO_BYTES {
                                            for bundle in &mut bundles {
                                                if bundle.vote_count > 0 {
                                                    bundle.vote_count -= 1;
                                                }
                                            }
                                        } else if let Some(bundle) = bundles.get_mut(*vote as usize)
                                        {
                                            bundle.vote_count += 1;
                                        }
                                        self.sidechain_number_to_bundles
                                            .put(&mut txn, &(sidechain_number as u8), &bundles)
                                            .into_diagnostic()?;
                                    }
                                }
                            }
                        },
                    }
                }
                Err(err) => {
                    return Err(miette!("failed to parse coinbase script: {err}"));
                }
            }
        }

        for transaction in &block.txdata[1..] {
            // TODO: Check that there is only onen OP_DRIVECHAIN.
            let mut new_ctip = None;
            let mut sidechain_number = None;
            let mut new_total_value = None;
            let mut address = None;
            {
                let output = &transaction.output[0];
                let script = output.script_pubkey.to_bytes();
                if script[0] == OP_DRIVECHAIN.to_u8() {
                    if new_ctip.is_some() {
                        return Err(miette!("more than one OP_DRIVECHAIN output"));
                    }
                    if script[1] != OP_PUSHBYTES_1.to_u8() {
                        return Err(miette!("invalid OP_DRIVECHAIN output"));
                    }
                    if script[3] != OP_TRUE.to_u8() {
                        return Err(miette!("invalid OP_DRIVECHAIN output"));
                    }
                    sidechain_number = Some(script[2]);
                    new_ctip = Some(OutPoint {
                        txid: transaction.txid(),
                        vout: 0 as u32,
                    });
                    new_total_value = Some(output.value.to_sat());
                }
            }
            {
                let output = &transaction.output[1];
                let script = output.script_pubkey.to_bytes();
                if script[0] == OP_RETURN.to_u8() {
                    address = Some(script[1..].to_vec());
                }
            }
            for (vout, output) in transaction.output.iter().enumerate() {}
            if let (Some(new_ctip), Some(sidechain_number), Some(new_total_value), Some(address)) =
                (new_ctip, sidechain_number, new_total_value, address)
            {
                let mut old_ctip_found = false;
                let old_total_value = {
                    let old_ctip = self
                        .sidechain_number_to_ctip
                        .get(&txn, &sidechain_number)
                        .into_diagnostic()?;
                    if let Some(old_ctip) = old_ctip {
                        for input in &transaction.input {
                            if input.previous_output == old_ctip.outpoint {
                                old_ctip_found = true;
                            }
                        }
                        old_ctip.value
                    } else {
                        return Err(miette!("sidechain {sidechain_number} doesn't have ctip"));
                    }
                };
                if old_ctip_found {
                    if new_total_value >= old_total_value {
                        // M5
                        // deposit
                        // What would happen if new CTIP value is equal to old CTIP value?
                        // for now it is treated as a deposit of 0.
                        let deposit = Deposit {
                            address,
                            value: new_total_value - old_total_value,
                            total_value: new_total_value,
                        };
                        self.deposits
                            .put(&mut txn, &(sidechain_number, new_ctip), &deposit)
                            .into_diagnostic()?;
                    } else {
                        // M6
                        // set correspondidng withdrawal bundle hash as spent
                        todo!();
                    }
                    let new_ctip = Ctip {
                        outpoint: new_ctip,
                        value: new_total_value,
                    };
                    self.sidechain_number_to_ctip
                        .put(&mut txn, &sidechain_number, &new_ctip)
                        .into_diagnostic()?;
                } else {
                    return Err(miette!(
                        "old ctip wasn't spent for sidechain {sidechain_number}"
                    ));
                }
            }
            dbg!(transaction);
        }
        txn.commit().into_diagnostic()?;
        Ok(())
    }

    pub fn disconnect_block(&self, block: &Block) -> Result<()> {
        todo!();
    }

    pub fn is_block_valid(&self, block: &Block) -> Result<()> {
        // validate a block
        todo!();
    }

    pub fn is_transaction_valid(&self, transaction: &Transaction) -> Result<()> {
        todo!();
    }
}
