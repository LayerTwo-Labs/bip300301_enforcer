use crate::gen::validator::{
    validator_service_server::ValidatorService, AckBundles, AckBundlesTag, ConnectBlockRequest,
    ConnectBlockResponse, Ctip, Deposit, DisconnectBlockRequest, DisconnectBlockResponse,
    GetCoinbasePsbtRequest, GetCoinbasePsbtResponse, GetCtipRequest, GetCtipResponse,
    GetDepositsRequest, GetDepositsResponse, GetMainBlockHeightRequest, GetMainBlockHeightResponse,
    GetMainChainTipRequest, GetMainChainTipResponse, GetSidechainProposalsRequest,
    GetSidechainProposalsResponse, GetSidechainsRequest, GetSidechainsResponse, Sidechain,
    SidechainProposal,
};

use bip300301_messages::{
    bitcoin::{self, absolute::Height, consensus::Encodable, hashes::Hash, Transaction, TxOut},
    CoinbaseMessage, M4AckBundles,
};
use miette::Result;
use tonic::{Request, Response, Status};

use crate::types;
pub use crate::validator::Validator;

fn invalid_enum_variant<Message>(field_name: &str, variant_name: &str) -> tonic::Status
where
    Message: prost::Name,
{
    let err_msg = format!(
        "Invalid enum variant in field `{}` of message `{}`: `{}`",
        field_name,
        Message::full_name(),
        variant_name
    );
    tonic::Status::invalid_argument(err_msg)
}

#[tonic::async_trait]
impl ValidatorService for Validator {
    async fn connect_block(
        &self,
        _: Request<ConnectBlockRequest>,
    ) -> Result<Response<ConnectBlockResponse>, Status> {
        todo!();
        /*
        // println!("REQUEST = {:?}", request);
        let request = request.into_inner();
        let mut cursor = Cursor::new(request.block);
        let block = Block::consensus_decode(&mut cursor).unwrap();
        let txn = self.env.write_txn().unwrap();
        self.connect_block(&block, request.height).unwrap();
        let response = ConnectBlockResponse {};
        Ok(Response::new(response))
        */
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
                data_hash: *data_hash,
            };
            messages.push(message);
        }
        for propose_bundle in &request.propose_bundles {
            let sidechain_number = propose_bundle.sidechain_number as u8;
            let bundle_txid: &[u8; 32] = &propose_bundle.bundle_txid.as_slice().try_into().unwrap();
            let message = CoinbaseMessage::M3ProposeBundle {
                sidechain_number,
                bundle_txid: *bundle_txid,
            };
            messages.push(message);
        }
        if let Some(ack_bundles) = &request.ack_bundles {
            let message = match ack_bundles.tag() {
                AckBundlesTag::Unspecified => {
                    return Err(invalid_enum_variant::<AckBundles>("tag", "Unspecified"))
                }
                AckBundlesTag::RepeatPrevious => M4AckBundles::RepeatPrevious,
                AckBundlesTag::LeadingBy50 => M4AckBundles::LeadingBy50,
                AckBundlesTag::Upvotes => {
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
                            .map(|upvote| (*upvote).try_into().unwrap())
                            .collect();
                        M4AckBundles::TwoBytes { upvotes }
                    } else {
                        let upvotes = ack_bundles
                            .upvotes
                            .iter()
                            .map(|upvote| (*upvote).try_into().unwrap())
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
        let request = request.into_inner();
        let sidechain_number = request.sidechain_number as u8;
        let deposits = self.get_deposits(sidechain_number).unwrap();
        let mut response = GetDepositsResponse { deposits: vec![] };
        for deposit in deposits {
            let deposit = Deposit {
                address: deposit.address,
                value: deposit.value,
                sequence_number: deposit.sequence_number,
            };
            response.deposits.push(deposit);
        }
        Ok(Response::new(response))
    }

    async fn get_sidechain_proposals(
        &self,
        _request: tonic::Request<GetSidechainProposalsRequest>,
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
        _request: tonic::Request<GetSidechainsRequest>,
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
                    Sidechain {
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
        let sidechain_number = match request.into_inner().sidechain_number {
            Some(sidechain_number) => sidechain_number as u8,
            None => return Err(Status::invalid_argument("must provide sidechain number")),
        };

        if let Some(ctip) = self.get_ctip(sidechain_number).unwrap() {
            let sequence_number = self.get_ctip_sequence_number(sidechain_number).unwrap();
            // get_ctip returned Some(ctip) above, so we know that the sequence_number will also
            // return Some, so we just unwrap it.
            let sequence_number = sequence_number.unwrap();
            let ctip = Ctip {
                txid: ctip.outpoint.txid.as_byte_array().into(),
                vout: ctip.outpoint.vout,
                value: ctip.value,
                sequence_number,
            };
            let response = GetCtipResponse { ctip: Some(ctip) };
            Ok(Response::new(response))
        } else {
            Err(Status::not_found(format!(
                "no chain tip found for sidechain number {}",
                sidechain_number
            )))
        }
    }

    async fn get_main_block_height(
        &self,
        _request: tonic::Request<GetMainBlockHeightRequest>,
    ) -> std::result::Result<tonic::Response<GetMainBlockHeightResponse>, tonic::Status> {
        let height = self.get_main_block_height().unwrap();
        let response = GetMainBlockHeightResponse { height };
        Ok(Response::new(response))
    }

    async fn get_main_chain_tip(
        &self,
        _request: tonic::Request<GetMainChainTipRequest>,
    ) -> std::result::Result<tonic::Response<GetMainChainTipResponse>, tonic::Status> {
        let block_hash = self.get_main_chain_tip().unwrap();
        let response = GetMainChainTipResponse {
            block_hash: block_hash.to_vec(),
        };
        Ok(Response::new(response))
    }

    // This is commented out for now, because it references Protobuf messages that
    // does not exist.
    // async fn get_accepted_bmm_hashes(
    //     &self,
    //     _request: Request<GetAcceptedBmmHashesRequest>,
    // ) -> std::result::Result<tonic::Response<GetAcceptedBmmHashesResponse>, tonic::Status> {
    //     let accepted_bmm_hashes = self.get_accepted_bmm_hashes().unwrap();
    //     let accepted_bmm_hashes = accepted_bmm_hashes
    //         .into_iter()
    //         .map(|(block_height, bmm_hashes)| {
    //             let bmm_hashes = bmm_hashes
    //                 .into_iter()
    //                 .map(|bmm_hash| bmm_hash.to_vec())
    //                 .collect();
    //             BlockHeightBmmHashes {
    //                 block_height,
    //                 bmm_hashes,
    //             }
    //         })
    //         .collect();
    //     let response = GetAcceptedBmmHashesResponse {
    //         accepted_bmm_hashes,
    //     };
    //     Ok(Response::new(response))
    // }
}
