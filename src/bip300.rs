use std::path::Path;

use crate::types::*;
use bip300301_messages::bitcoin;
use bip300301_messages::{
    parse_coinbase_script, parse_op_drivechain, sha256d, CoinbaseMessage, M4AckBundles,
    ABSTAIN_ONE_BYTE, ABSTAIN_TWO_BYTES, ALARM_ONE_BYTE, ALARM_TWO_BYTES,
};
use bitcoin::consensus::Decodable;
use bitcoin::opcodes::all::OP_RETURN;
use bitcoin::BlockHash;
use bitcoin::{Block, OutPoint, Transaction};
use miette::{miette, IntoDiagnostic, Result};
use std::io::Cursor;
use std::str::FromStr;

use heed::Database;
use heed::{types::*, Env, EnvOpenOptions};

/*
const USED_MAX_AGE: u16 = 26_300;
const USED_THRESHOLD: u16 = USED_MAX_AGE / 2;

const UNUSED_MAX_AGE: u16 = 2016;
const UNUSED_THRESHOLD: u16 = UNUSED_MAX_AGE - 201;
*/

const USED_MAX_AGE: u16 = 50;
const USED_THRESHOLD: u16 = USED_MAX_AGE / 2;

const UNUSED_MAX_AGE: u16 = 25;
const UNUSED_THRESHOLD: u16 = UNUSED_MAX_AGE - 5;

#[derive(Clone)]
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

    pub async fn run(&self) {
        let bip300 = self.clone();
        tokio::task::spawn(async move {
            let main_datadir = Path::new("../../data/bitcoin/");
            let main_client = create_client(main_datadir).unwrap();
            let mut current_block_height: u32 = main_client
                .send_request("getblockcount", &[])
                .unwrap()
                .ok_or(miette!("failed to get block count"))
                .unwrap();

            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let block_height: u32 = main_client
                    .send_request("getblockcount", &[])
                    .unwrap()
                    .ok_or(miette!("failed to get block count"))
                    .unwrap();

                if block_height == current_block_height {
                    continue;
                }
                println!("Block height: {block_height}");

                for height in current_block_height..block_height {
                    let block_hash: String = main_client
                        .send_request("getblockhash", &[json!(height)])
                        .unwrap()
                        .ok_or(miette!("failed to get block hash"))
                        .unwrap();
                    let prev_blockhash = BlockHash::from_str(&block_hash).unwrap();

                    println!("Mainchain tip: {prev_blockhash}");

                    let block: String = main_client
                        .send_request("getblock", &[json!(block_hash), json!(0)])
                        .unwrap()
                        .ok_or(miette!("failed to get block"))
                        .unwrap();
                    let block_bytes = hex::decode(&block).unwrap();
                    let mut cursor = Cursor::new(block_bytes);
                    let block = Block::consensus_decode(&mut cursor).unwrap();

                    // dbg!(block);

                    bip300.connect_block(&block, height).unwrap();
                    println!();

                    // check for new block
                    // validate block
                    // if invalid invalidate
                    // if valid connect
                    // wait 1 second
                }
                current_block_height = block_height;
            }
        });
    }

    pub fn connect_block(&self, block: &Block, height: u32) -> Result<()> {
        println!("connect block");
        // TODO: Check that there are no duplicate M2s.
        let coinbase = &block.txdata[0];

        let mut txn = self.env.write_txn().into_diagnostic()?;
        let mut initial_ctip: Option<(u8, OutPoint)> = None;
        let mut sidechain_activation: Option<u8> = None;
        for (vout, output) in coinbase.output.iter().enumerate() {
            match &parse_coinbase_script(&output.script_pubkey) {
                Ok((_, message)) => {
                    match message {
                        CoinbaseMessage::OpDrivechain { sidechain_number } => {
                            let ctip_outpoint = OutPoint {
                                txid: coinbase.txid(),
                                vout: vout as u32,
                            };
                            initial_ctip = Some((*sidechain_number, ctip_outpoint));
                        }
                        CoinbaseMessage::M1ProposeSidechain {
                            sidechain_number,
                            data,
                        } => {
                            println!(
                                "Propose sidechain number {sidechain_number} with data \"{}\"",
                                String::from_utf8(data.clone()).into_diagnostic()?,
                            );
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
                            println!(
                                "Ack sidechain number {sidechain_number} with hash {}",
                                hex::encode(data_hash)
                            );
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

                                    let sidechain_proposal_age =
                                        height - sidechain_proposal.proposal_height;

                                    let used = self
                                        .sidechain_number_to_sidechain
                                        .get(&txn, &sidechain_proposal.sidechain_number)
                                        .into_diagnostic()?
                                        .is_some();

                                    let succeeded = used
                                        && sidechain_proposal.vote_count > USED_THRESHOLD
                                        && sidechain_proposal_age <= USED_MAX_AGE as u32
                                        || !used
                                            && sidechain_proposal.vote_count > UNUSED_THRESHOLD
                                            && sidechain_proposal_age < UNUSED_MAX_AGE as u32;

                                    if succeeded {
                                        println!(
                                            "sidechain {} in slot {} was activated",
                                            String::from_utf8(sidechain_proposal.data.clone())
                                                .into_diagnostic()?,
                                            sidechain_proposal.sidechain_number
                                        );
                                        let sidechain = Sidechain {
                                            sidechain_number: sidechain_proposal.sidechain_number,
                                            data: sidechain_proposal.data,
                                            proposal_height: sidechain_proposal.proposal_height,
                                            activation_height: height,
                                            vote_count: sidechain_proposal.vote_count,
                                        };
                                        self.sidechain_number_to_sidechain
                                            .put(&mut txn, &sidechain.sidechain_number, &sidechain)
                                            .into_diagnostic()?;
                                        self.data_hash_to_sidechain_proposal
                                            .delete(&mut txn, &data_hash)
                                            .into_diagnostic()?;
                                        sidechain_activation = Some(sidechain.sidechain_number);
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
                Err(_) => {}
            }
        }

        let mut failed_proposals = vec![];
        for sidechain_proposal in self
            .data_hash_to_sidechain_proposal
            .iter(&txn)
            .into_diagnostic()?
        {
            let (data_hash, sidechain_proposal) = sidechain_proposal.into_diagnostic()?;
            let sidechain_proposal_age = height - sidechain_proposal.proposal_height;

            let used = self
                .sidechain_number_to_sidechain
                .get(&txn, &sidechain_proposal.sidechain_number)
                .into_diagnostic()?
                .is_some();

            let failed = used && sidechain_proposal_age > USED_MAX_AGE as u32
                || !used && sidechain_proposal_age > UNUSED_MAX_AGE as u32;
            if failed {
                failed_proposals.push(data_hash);
            }
        }
        for failed_proposal_data_hash in &failed_proposals {
            self.data_hash_to_sidechain_proposal
                .delete(&mut txn, &failed_proposal_data_hash)
                .into_diagnostic()?;
        }

        dbg!(sidechain_activation, initial_ctip);
        match (sidechain_activation, initial_ctip) {
            (Some(sidechain_number), None) => {
                return Err(miette!(
                    "no OP_DRIVECHAIN output in coinbase of block activating sidechain {}",
                    sidechain_number
                ));
            }
            (None, Some((sidechain_number, _))) => {
                return Err(miette!(
                    "OP_DRIVECHAIN output present in coinbase of block that doesn't activate sidechain {}",
                    sidechain_number
                ));
            }
            (
                Some(activation_sidechain_number),
                Some((initial_ctip_sidechain_number, initial_ctip_outpoint)),
            ) => {
                if activation_sidechain_number != initial_ctip_sidechain_number {
                    return Err(miette!("activated sidechain number {} doesn't match  sidechain number in OP_DRIVECHAIN output {}", activation_sidechain_number, initial_ctip_sidechain_number));
                }
                let ctip = Ctip {
                    outpoint: initial_ctip_outpoint,
                    value: 0,
                };
                self.sidechain_number_to_ctip
                    .put(&mut txn, &activation_sidechain_number, &ctip)
                    .into_diagnostic()?;
            }
            (None, None) => {}
        };

        for transaction in &block.txdata[1..] {
            // TODO: Check that there is only one OP_DRIVECHAIN per sidechain slot.
            let mut new_ctip = None;
            let mut sidechain_number = None;
            let mut new_total_value = None;
            let mut address = None;
            {
                let output = &transaction.output[0];
                // If OP_DRIVECHAIN script is invalid,
                // for example if it is missing OP_TRUE at the end,
                // it will just be ignored.
                if let Ok((input, number)) = parse_op_drivechain(&output.script_pubkey.to_bytes()) {
                    dbg!(&output.script_pubkey);
                    if new_ctip.is_some() {
                        return Err(miette!("more than one OP_DRIVECHAIN output"));
                    }
                    sidechain_number = Some(number);
                    new_ctip = Some(OutPoint {
                        txid: transaction.txid(),
                        vout: 0 as u32,
                    });
                    new_total_value = Some(output.value);
                }
            }
            {
                let output = &transaction.output[1];
                let script = output.script_pubkey.to_bytes();
                if script[0] == OP_RETURN.to_u8() {
                    address = Some(script[1..].to_vec());
                }
            }
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

    pub fn is_transaction_valid(&self, transaction: &Transaction) -> Result<()> {
        todo!();
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

    pub fn get_ctip(&self, sidechain_number: u8) -> Result<Option<Ctip>> {
        let txn = self.env.read_txn().into_diagnostic()?;
        let ctip = self
            .sidechain_number_to_ctip
            .get(&txn, &sidechain_number)
            .into_diagnostic()?;
        Ok(ctip)
    }
}

// connect block
// is_block_valid
// is_transaction_valid
//
// get data for sidechain

use std::time::{Duration, SystemTime, UNIX_EPOCH};
use ureq_jsonrpc::{json, Client};
fn create_client(main_datadir: &Path) -> Result<Client> {
    let auth = std::fs::read_to_string(main_datadir.join("regtest/.cookie")).into_diagnostic()?;
    let mut auth = auth.split(":");
    let user = auth
        .next()
        .ok_or(miette!("failed to get rpcuser"))?
        .to_string();
    let password = auth
        .next()
        .ok_or(miette!("failed to get rpcpassword"))?
        .to_string();
    Ok(Client {
        host: "localhost".into(),
        port: 18443,
        user,
        password,
        id: "mainchain".into(),
    })
}
