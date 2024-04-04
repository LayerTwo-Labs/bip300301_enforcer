use std::io::Cursor;

use bitcoin::absolute::Height;
use bitcoin::consensus::{Decodable, Encodable};
use bitcoin::transaction::Version;
use bitcoin::{Amount, Block, Transaction, TxOut};
use enforcer_proto::validator::{GetDepositsRequest, GetDepositsResponse};
use miette::Result;
use tonic::{Request, Response, Status};

use validator::validator_server::Validator;
use validator::{ConnectBlockRequest, ConnectBlockResponse};
use validator::{DisconnectBlockRequest, DisconnectBlockResponse};
use validator::{IsValidRequest, IsValidResponse};

pub use crate::bip300::Bip300;

use self::validator::{AckBundlesEnum, GetCoinbasePsbtRequest, GetCoinbasePsbtResponse};
use crate::messages::{CoinbaseMessage, M4AckBundles};

pub use enforcer_proto::validator;

#[tonic::async_trait]
impl Validator for Bip300 {
    async fn is_valid(
        &self,
        _request: Request<IsValidRequest>,
    ) -> Result<Response<IsValidResponse>, Status> {
        let response = IsValidResponse { valid: true };
        Ok(Response::new(response))
    }

    async fn connect_block(
        &self,
        request: Request<ConnectBlockRequest>,
    ) -> Result<Response<ConnectBlockResponse>, Status> {
        // println!("REQUEST = {:?}", request);
        let request = request.into_inner();
        let mut cursor = Cursor::new(request.block);
        let block = Block::consensus_decode(&mut cursor).unwrap();
        self.connect_block(&block, request.height).unwrap();
        let response = ConnectBlockResponse {};
        Ok(Response::new(response))
    }

    async fn disconnect_block(
        &self,
        _request: Request<DisconnectBlockRequest>,
    ) -> Result<Response<DisconnectBlockResponse>, Status> {
        let response = DisconnectBlockResponse {};
        Ok(Response::new(response))
    }

    async fn get_coinbase_psbt(
        &self,
        request: Request<GetCoinbasePsbtRequest>,
    ) -> Result<Response<GetCoinbasePsbtResponse>, Status> {
        let request = request.into_inner();
        let mut messages = vec![];
        for propose_sidechain in &request.propose_sidechains {
            let sidechain_number = propose_sidechain.sidechain_number as u8;
            let data = propose_sidechain.data.clone();
            let message = CoinbaseMessage::M1ProposeSidechain {
                sidechain_number,
                data,
            };
            messages.push(message);
        }
        for ack_sidechain in &request.ack_sidechains {
            let sidechain_number = ack_sidechain.sidechain_number as u8;
            let data_hash: &[u8; 32] = ack_sidechain.data_hash.as_slice().try_into().unwrap();
            let message = CoinbaseMessage::M2AckSidechain {
                sidechain_number,
                data_hash: data_hash.clone(),
            };
            messages.push(message);
        }
        for propose_bundle in &request.propose_bundles {
            let sidechain_number = propose_bundle.sidechain_number as u8;
            let bundle_txid: &[u8; 32] = &propose_bundle.bundle_txid.as_slice().try_into().unwrap();
            let message = CoinbaseMessage::M3ProposeBundle {
                sidechain_number,
                bundle_txid: bundle_txid.clone(),
            };
            messages.push(message);
        }
        if let Some(ack_bundles) = &request.ack_bundles {
            let message = match ack_bundles.tag() {
                AckBundlesEnum::RepeatPrevious => M4AckBundles::RepeatPrevious,
                AckBundlesEnum::LeadingBy50 => M4AckBundles::LeadingBy50,
                AckBundlesEnum::Upvotes => {
                    let mut two_bytes = false;
                    for upvote in &ack_bundles.upvotes {
                        if *upvote > u8::MAX as u32 {
                            two_bytes = true;
                        }
                        if *upvote > u16::MAX as u32 {
                            panic!("upvote too large");
                        }
                    }
                    if two_bytes {
                        let upvotes = ack_bundles
                            .upvotes
                            .iter()
                            .map(|upvote| upvote.clone().try_into().unwrap())
                            .collect();
                        M4AckBundles::TwoBytes { upvotes }
                    } else {
                        let upvotes = ack_bundles
                            .upvotes
                            .iter()
                            .map(|upvote| upvote.clone().try_into().unwrap())
                            .collect();
                        M4AckBundles::OneByte { upvotes }
                    }
                }
            };
            let message = CoinbaseMessage::M4AckBundles(message);
            messages.push(message);
        }

        let output = messages
            .into_iter()
            .map(|message| TxOut {
                value: Amount::from_sat(0),
                script_pubkey: message.into(),
            })
            .collect();
        let transasction = Transaction {
            output,
            input: vec![],
            lock_time: bitcoin::absolute::LockTime::Blocks(Height::ZERO),
            version: Version::TWO,
        };
        let mut psbt = vec![];
        transasction.consensus_encode(&mut psbt).unwrap();

        let response = GetCoinbasePsbtResponse { psbt };
        Ok(Response::new(response))
    }

    async fn get_deposits(&self, request: Request<GetDepositsRequest>) -> Result<Response<GetDepositsResponse>, Status> {
        todo!();
    }
}
