use std::{io::Cursor, path::Path, str::FromStr, time::Duration};

use bip300301_messages::{
    bitcoin, parse_coinbase_script, parse_op_drivechain, sha256d, CoinbaseMessage, M4AckBundles,
    ABSTAIN_ONE_BYTE, ABSTAIN_TWO_BYTES, ALARM_ONE_BYTE, ALARM_TWO_BYTES,
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

use crate::types::{Bundle, Ctip, Deposit, Hash256, Sidechain, SidechainProposal};

/*
const WITHDRAWAL_BUNDLE_MAX_AGE: u16 = 26_300;
const WITHDRAWAL_BUNDLE_INCLUSION_THRESHOLD: u16 = WITHDRAWAL_BUNDLE_MAX_AGE / 2; // 13_150

const USED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE: u16 = WITHDRAWAL_BUNDLE_MAX_AGE; // 26_300
const USED_SIDECHAIN_SLOT_ACTIVATION_THRESHOLD: u16 = USED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE / 2;

const UNUSED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE: u16 = 2016;
const UNUSED_SIDECHAIN_SLOT_ACTIVATION_THRESHOLD: u16 = UNUSED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE - 201;

const UNUSED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE: u16 = 2016;
const UNUSED_SIDECHAIN_SLOT_ACTIVATION_MAX_FAILS: u16 = 201;
const UNUSED_SIDECHAIN_SLOT_ACTIVATION_THRESHOLD: u16 = UNUSED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE - UNUSED_SIDECHAIN_SLOT_ACTIVATION_MAX_FAILS; // 1815;
*/

const WITHDRAWAL_BUNDLE_MAX_AGE: u16 = 10;
const WITHDRAWAL_BUNDLE_INCLUSION_THRESHOLD: u16 = WITHDRAWAL_BUNDLE_MAX_AGE / 2; // 5

const USED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE: u16 = WITHDRAWAL_BUNDLE_MAX_AGE; // 5
const USED_SIDECHAIN_SLOT_ACTIVATION_THRESHOLD: u16 = USED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE / 2;

const UNUSED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE: u16 = 10;
const UNUSED_SIDECHAIN_SLOT_ACTIVATION_MAX_FAILS: u16 = 5;
const UNUSED_SIDECHAIN_SLOT_ACTIVATION_THRESHOLD: u16 = UNUSED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE - UNUSED_SIDECHAIN_SLOT_ACTIVATION_MAX_FAILS;

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

/// Result from [`Bip300::handle_coinbase`]
#[derive(Debug, Default)]
struct CoinbaseOutcome {
    initial_ctip: Option<(u8, OutPoint)>,
    sidechain_activation: Option<u8>,
}

/// Result from [`tx_first_output_outcome`]
#[derive(Debug)]
struct TxFirstOutputOutcome {
    new_ctip: OutPoint,
    new_total_value: u64,
    sidechain_number: u8,
}

fn tx_first_output_outcome(transaction: &Transaction) -> Option<TxFirstOutputOutcome> {
    let output = &transaction.output[0];
    // If OP_DRIVECHAIN script is invalid,
    // for example if it is missing OP_TRUE at the end,
    // it will just be ignored.
    if let Ok((_input, sidechain_number)) = parse_op_drivechain(&output.script_pubkey.to_bytes()) {
        dbg!(&output.script_pubkey);
        // FIXME: this check will never succeed
        /*
        if new_ctip.is_some() {
            return Err(miette!("more than one OP_DRIVECHAIN output"));
        }
        */
        Some(TxFirstOutputOutcome {
            new_ctip: OutPoint {
                txid: transaction.txid(),
                vout: 0,
            },
            new_total_value: output.value,
            sidechain_number,
        })
    } else {
        None
    }
}

/// Result from [`tx_second_output_outcome`]
#[derive(Debug)]
struct TxSecondOutputOutcome {
    address: Vec<u8>,
}

fn tx_second_output_outcome(transaction: &Transaction) -> Option<TxSecondOutputOutcome> {
    let output = &transaction.output[1];
    let script = output.script_pubkey.to_bytes();
    if script[0] == OP_RETURN.to_u8() {
        Some(TxSecondOutputOutcome {
            address: script[1..].to_vec(),
        })
    } else {
        None
    }
}

#[derive(Clone)]
pub struct Bip300 {
    env: Env,

    data_hash_to_sidechain_proposal:
        Database<SerdeBincode<Hash256>, SerdeBincode<SidechainProposal>>,
    sidechain_number_to_bundles: Database<SerdeBincode<u8>, SerdeBincode<Vec<Bundle>>>,
    sidechain_number_to_sidechain: Database<SerdeBincode<u8>, SerdeBincode<Sidechain>>,
    sidechain_number_to_ctip: Database<SerdeBincode<u8>, SerdeBincode<Ctip>>,
    _previous_votes: Database<SerdeBincode<UnitKey>, SerdeBincode<Vec<Hash256>>>,
    _leading_by_50: Database<SerdeBincode<UnitKey>, SerdeBincode<Vec<Hash256>>>,

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
            _previous_votes: previous_votes,
            _leading_by_50: leading_by_50,
            deposits,
        })
    }

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

    /// Returns true IFF sidechain was activated
    fn handle_m2_ack_sidechain(
        &self,
        rwtxn: &mut RwTxn,
        height: u32,
        sidechain_number: u8,
        data_hash: [u8; 32],
    ) -> Result<bool> {
        let sidechain_proposal = self
            .data_hash_to_sidechain_proposal
            .get(rwtxn, &data_hash)
            .into_diagnostic()?;
        let Some(mut sidechain_proposal) = sidechain_proposal else {
            return Ok(false);
        };
        // Does it make sense to check for sidechain number?
        if sidechain_proposal.sidechain_number != sidechain_number {
            return Ok(false);
        }
        sidechain_proposal.vote_count += 1;
        self.data_hash_to_sidechain_proposal
            .put(rwtxn, &data_hash, &sidechain_proposal)
            .into_diagnostic()?;
        let sidechain_proposal_age = height - sidechain_proposal.proposal_height;
        let used = self
            .sidechain_number_to_sidechain
            .get(rwtxn, &sidechain_number)
            .into_diagnostic()?
            .is_some();
        let succeeded = used
            && sidechain_proposal.vote_count > USED_SIDECHAIN_SLOT_ACTIVATION_THRESHOLD
            && sidechain_proposal_age <= USED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE as u32
            || !used
                && sidechain_proposal.vote_count > UNUSED_SIDECHAIN_SLOT_ACTIVATION_THRESHOLD
                && sidechain_proposal_age < UNUSED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE as u32;
        if !succeeded {
            return Ok(false);
        }
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
        Ok(true)
    }

    fn handle_m3_propose_bundle(
        &self,
        rwtxn: &mut RwTxn,
        sidechain_number: u8,
        bundle_txid: [u8; 32],
    ) -> Result<()> {
        let bundles = self
            .sidechain_number_to_bundles
            .get(rwtxn, &sidechain_number)
            .into_diagnostic()?;
        let Some(mut bundles) = bundles else {
            return Ok(());
        };
        let bundle = Bundle {
            bundle_txid,
            vote_count: 0,
        };
        bundles.push(bundle);
        self.sidechain_number_to_bundles
            .put(rwtxn, &sidechain_number, &bundles)
            .into_diagnostic()
    }

    fn handle_m4_one_byte(&self, rwtxn: &mut RwTxn, upvotes: &[u8]) -> Result<()> {
        for (sidechain_number, vote) in upvotes.iter().enumerate() {
            if *vote == ABSTAIN_ONE_BYTE {
                continue;
            }
            let bundles = self
                .sidechain_number_to_bundles
                .get(rwtxn, &(sidechain_number as u8))
                .into_diagnostic()?;
            let Some(mut bundles) = bundles else { continue };
            if *vote == ALARM_ONE_BYTE {
                for bundle in &mut bundles {
                    if bundle.vote_count > 0 {
                        bundle.vote_count -= 1;
                    }
                }
            } else if let Some(bundle) = bundles.get_mut(*vote as usize) {
                bundle.vote_count += 1;
            }
            self.sidechain_number_to_bundles
                .put(rwtxn, &(sidechain_number as u8), &bundles)
                .into_diagnostic()?;
        }
        Ok(())
    }

    fn handle_m4_two_bytes(&self, rwtxn: &mut RwTxn, upvotes: &[u16]) -> Result<()> {
        for (sidechain_number, vote) in upvotes.iter().enumerate() {
            if *vote == ABSTAIN_TWO_BYTES {
                continue;
            }
            let bundles = self
                .sidechain_number_to_bundles
                .get(rwtxn, &(sidechain_number as u8))
                .into_diagnostic()?;
            let Some(mut bundles) = bundles else { continue };
            if *vote == ALARM_TWO_BYTES {
                for bundle in &mut bundles {
                    if bundle.vote_count > 0 {
                        bundle.vote_count -= 1;
                    }
                }
            } else if let Some(bundle) = bundles.get_mut(*vote as usize) {
                bundle.vote_count += 1;
            }
            self.sidechain_number_to_bundles
                .put(rwtxn, &(sidechain_number as u8), &bundles)
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
            M4AckBundles::OneByte { upvotes } => self.handle_m4_one_byte(rwtxn, upvotes),
            M4AckBundles::TwoBytes { upvotes } => self.handle_m4_two_bytes(rwtxn, upvotes),
        }
    }

    fn handle_coinbase(
        &self,
        rwtxn: &mut RwTxn,
        height: u32,
        coinbase: &Transaction,
    ) -> Result<CoinbaseOutcome> {
        let mut res = CoinbaseOutcome::default();
        for (vout, output) in coinbase.output.iter().enumerate() {
            let Ok((_, message)) = parse_coinbase_script(&output.script_pubkey) else {
                continue;
            };
            match message {
                CoinbaseMessage::OpDrivechain { sidechain_number } => {
                    let ctip_outpoint = OutPoint {
                        txid: coinbase.txid(),
                        vout: vout as u32,
                    };
                    res.initial_ctip = Some((sidechain_number, ctip_outpoint));
                }
                CoinbaseMessage::M1ProposeSidechain {
                    sidechain_number,
                    data,
                } => {
                    println!(
                        "Propose sidechain number {sidechain_number} with data \"{}\"",
                        String::from_utf8(data.clone()).into_diagnostic()?,
                    );
                    let () = self.handle_m1_propose_sidechain(
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
                    println!(
                        "Ack sidechain number {sidechain_number} with hash {}",
                        hex::encode(data_hash)
                    );
                    if self.handle_m2_ack_sidechain(rwtxn, height, sidechain_number, data_hash)? {
                        res.sidechain_activation = Some(sidechain_number);
                    }
                }
                CoinbaseMessage::M3ProposeBundle {
                    sidechain_number,
                    bundle_txid,
                } => {
                    let () = self.handle_m3_propose_bundle(rwtxn, sidechain_number, bundle_txid)?;
                }
                CoinbaseMessage::M4AckBundles(m4) => {
                    let () = self.handle_m4_ack_bundles(rwtxn, &m4)?;
                }
            }
        }
        Ok(res)
    }

    fn handle_failed_proposals(&self, rwtxn: &mut RwTxn, height: u32) -> Result<()> {
        let failed_proposals: Vec<_> = self
            .data_hash_to_sidechain_proposal
            .iter(rwtxn)
            .into_diagnostic()?
            .map(|item| item.into_diagnostic())
            .transpose_into_fallible()
            .filter_map(|(data_hash, sidechain_proposal)| {
                let sidechain_proposal_age = height - sidechain_proposal.proposal_height;
                let used = self
                    .sidechain_number_to_sidechain
                    .get(rwtxn, &sidechain_proposal.sidechain_number)
                    .into_diagnostic()?
                    .is_some();
                let failed = used && sidechain_proposal_age > USED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE as u32
                    || !used && sidechain_proposal_age > UNUSED_SIDECHAIN_SLOT_PROPOSAL_MAX_AGE as u32;
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

    fn handle_coinbase_outcome(
        &self,
        rwtxn: &mut RwTxn,
        coinbase_outcome: &CoinbaseOutcome,
    ) -> Result<()> {
        let CoinbaseOutcome {
            initial_ctip,
            sidechain_activation,
        } = coinbase_outcome;
        match (sidechain_activation, initial_ctip) {
            (Some(sidechain_number), None) =>
                Err(miette!(
                    "no OP_DRIVECHAIN output in coinbase of block activating sidechain {}",
                    sidechain_number
                )),
            (None, Some((sidechain_number, _))) =>
                Err(miette!(
                    "OP_DRIVECHAIN output present in coinbase of block that doesn't activate sidechain {}",
                    sidechain_number
                )),
            (
                Some(activation_sidechain_number),
                Some((initial_ctip_sidechain_number, initial_ctip_outpoint)),
            ) => {
                if activation_sidechain_number != initial_ctip_sidechain_number {
                    return Err(miette!("activated sidechain number {activation_sidechain_number} doesn't match  sidechain number in OP_DRIVECHAIN output {initial_ctip_sidechain_number}"))
                }
                let ctip = Ctip {
                    outpoint: *initial_ctip_outpoint,
                    value: 0,
                };
                self.sidechain_number_to_ctip
                    .put(rwtxn, activation_sidechain_number, &ctip)
                    .into_diagnostic()
            }
            (None, None) => Ok(())
        }
    }

    fn get_old_ctip(&self, rotxn: &RoTxn, sidechain_number: u8) -> Result<Ctip> {
        self.sidechain_number_to_ctip
            .get(rotxn, &sidechain_number)
            .into_diagnostic()?
            .ok_or_else(|| miette!("ctip missing for sidechain {sidechain_number}"))
    }

    fn handle_tx(&self, rwtxn: &mut RwTxn, transaction: &Transaction) -> Result<()> {
        // TODO: Check that there is only one OP_DRIVECHAIN per sidechain slot.
        let (
            Some(TxFirstOutputOutcome {
                new_ctip,
                new_total_value,
                sidechain_number,
            }),
            Some(TxSecondOutputOutcome { address }),
        ) = (
            tx_first_output_outcome(transaction),
            tx_second_output_outcome(transaction),
        )
        else {
            return Ok(());
        };
        let old_total_value = {
            let old_ctip = self.get_old_ctip(rwtxn, sidechain_number)?;
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
        };
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
                .put(rwtxn, &(sidechain_number, new_ctip), &deposit)
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
            .put(rwtxn, &sidechain_number, &new_ctip)
            .into_diagnostic()
    }

    pub fn connect_block(&self, block: &Block, height: u32) -> Result<()> {
        println!("connect block");
        // TODO: Check that there are no duplicate M2s.
        let coinbase = &block.txdata[0];

        let mut rwtxn = self.env.write_txn().into_diagnostic()?;
        let coinbase_outcome = self.handle_coinbase(&mut rwtxn, height, coinbase)?;

        let () = self.handle_failed_proposals(&mut rwtxn, height)?;

        let _ = dbg!(&coinbase_outcome);
        let () = self.handle_coinbase_outcome(&mut rwtxn, &coinbase_outcome)?;

        for transaction in &block.txdata[1..] {
            let () = self.handle_tx(&mut rwtxn, transaction)?;
            dbg!(transaction);
        }
        rwtxn.commit().into_diagnostic()?;
        Ok(())
    }

    pub fn _disconnect_block(&self, _block: &Block) -> Result<()> {
        todo!();
    }

    pub fn _is_transaction_valid(&self, _transaction: &Transaction) -> Result<()> {
        todo!();
    }

    /// Single iteration of the task loop
    fn task_loop_once(&self, main_client: &Client, current_block_height: &mut u32) -> Result<()> {
        let block_height: u32 = main_client
            .send_request("getblockcount", &[])
            .into_diagnostic()?
            .ok_or(miette!("failed to get block count"))?;
        if block_height == *current_block_height {
            return Ok(());
        }
        println!("Block height: {block_height}");

        for height in *current_block_height..block_height {
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

            // dbg!(block);

            self.connect_block(&block, height)?;
            println!();

            // check for new block
            // validate block
            // if invalid invalidate
            // if valid connect
            // wait 1 second
        }
        *current_block_height = block_height;
        Ok(())
    }

    async fn task(&self) -> Result<()> {
        let main_datadir = Path::new("../../data/bitcoin/");
        let main_client = &create_client(main_datadir)?;
        let mut current_block_height: u32 = main_client
            .send_request("getblockcount", &[])
            .into_diagnostic()?
            .ok_or(miette!("failed to get block count"))?;
        let interval = interval(Duration::from_secs(1));
        IntervalStream::new(interval)
            .map(Ok)
            .try_for_each(move |_: Instant| async move {
                self.task_loop_once(main_client, &mut current_block_height)
            })
            .await
    }

    pub fn run(&self) -> JoinHandle<()> {
        let this = self.clone();
        tokio::task::spawn(
            async move { this.task().unwrap_or_else(|err| eprintln!("{err:#}")).await },
        )
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

fn create_client(main_datadir: &Path) -> Result<Client> {
    let auth = std::fs::read_to_string(main_datadir.join("regtest/.cookie")).into_diagnostic()?;
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
        host: "localhost".into(),
        port: 18443,
        user,
        password,
        id: "mainchain".into(),
    })
}
