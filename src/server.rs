use std::io::Cursor;

use bip300_messages::bitcoin;

use bip300_messages::bitcoin::hashes::Hash;
use bitcoin::absolute::Height;
use bitcoin::consensus::{Decodable, Encodable};
use bitcoin::{Block, Transaction, TxOut};
use bip300301_enforcer_proto::validator::{
    GetCtipRequest, GetCtipResponse, GetDepositsRequest, GetDepositsResponse,
    GetSidechainProposalsRequest, GetSidechainProposalsResponse, GetSidechainsRequest,
    GetSidechainsResponse, SidechainProposal,
};
use miette::Result;
use tonic::{Request, Response, Status};

use validator::validator_server::Validator;
use validator::{ConnectBlockRequest, ConnectBlockResponse};
use validator::{DisconnectBlockRequest, DisconnectBlockResponse};

pub use crate::bip300::Bip300;
use crate::types;

use self::validator::{AckBundlesEnum, GetCoinbasePsbtRequest, GetCoinbasePsbtResponse};
use bip300_messages::{CoinbaseMessage, M4AckBundles};

pub use bip300301_enforcer_proto::validator;

#[tonic::async_trait]
impl Validator for Bip300 {
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
                value: 0,
                script_pubkey: message.into(),
            })
            .collect();
        let transasction = Transaction {
            output,
            input: vec![],
            lock_time: bitcoin::absolute::LockTime::Blocks(Height::ZERO),
            version: 2,
        };
        let mut psbt = vec![];
        transasction.consensus_encode(&mut psbt).unwrap();

        let response = GetCoinbasePsbtResponse { psbt };
        Ok(Response::new(response))
    }

    async fn get_deposits(
        &self,
        request: Request<GetDepositsRequest>,
    ) -> Result<Response<GetDepositsResponse>, Status> {
        todo!();
    }

    async fn get_sidechain_proposals(
        &self,
        request: tonic::Request<GetSidechainProposalsRequest>,
    ) -> std::result::Result<tonic::Response<GetSidechainProposalsResponse>, tonic::Status> {
        let sidechain_proposals = self.get_sidechain_proposals().unwrap();

        let sidechain_proposals = sidechain_proposals
            .into_iter()
            .map(
                |(
                    data_hash,
                    types::SidechainProposal {
                        sidechain_number,
                        data,
                        vote_count,
                        proposal_height,
                    },
                )| {
                    SidechainProposal {
                        sidechain_number: sidechain_number as u32,
                        data,
                        data_hash: data_hash.to_vec(),
                        vote_count: vote_count as u32,
                        proposal_height,
                        proposal_age: 0,
                    }
                },
            )
            .collect();
        let response = GetSidechainProposalsResponse {
            sidechain_proposals,
        };
        Ok(Response::new(response))
    }

    async fn get_sidechains(
        &self,
        request: tonic::Request<GetSidechainsRequest>,
    ) -> std::result::Result<tonic::Response<GetSidechainsResponse>, tonic::Status> {
        let sidechains = self.get_sidechains().unwrap();

        let sidechains = sidechains
            .into_iter()
            .map(
                |types::Sidechain {
                     sidechain_number,
                     data,
                     vote_count,
                     proposal_height,
                     activation_height,
                 }| {
                    bip300301_enforcer_proto::validator::Sidechain {
                        sidechain_number: sidechain_number as u32,
                        data,
                        vote_count: vote_count as u32,
                        proposal_height,
                        activation_height,
                    }
                },
            )
            .collect();
        let response = GetSidechainsResponse { sidechains };
        Ok(Response::new(response))
    }

    async fn get_ctip(
        &self,
        request: tonic::Request<GetCtipRequest>,
    ) -> std::result::Result<tonic::Response<GetCtipResponse>, tonic::Status> {
        let sidechain_number = request.into_inner().sidechain_number;
        let ctip = self.get_ctip(sidechain_number as u8).unwrap().unwrap();
        let txid = ctip.outpoint.txid.as_byte_array().into();
        let vout = ctip.outpoint.vout as u32;
        let value = ctip.value;
        let response = GetCtipResponse { txid, vout, value };
        Ok(Response::new(response))
    }
}
