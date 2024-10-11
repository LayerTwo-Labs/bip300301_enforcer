use std::sync::Arc;

use crate::proto::mainchain::{
    wallet_service_server::WalletService, BroadcastWithdrawalBundleRequest,
    BroadcastWithdrawalBundleResponse, CreateBmmCriticalDataTransactionRequest,
    CreateBmmCriticalDataTransactionResponse, CreateDepositTransactionRequest,
    CreateDepositTransactionResponse, CreateNewAddressRequest, CreateNewAddressResponse,
    GenerateBlocksRequest, GenerateBlocksResponse,
};
use crate::{
    proto::{
        self,
        mainchain::{
            get_bmm_h_star_commitment_response, get_ctip_response::Ctip,
            get_sidechain_proposals_response::SidechainProposal,
            get_sidechains_response::SidechainInfo, server::ValidatorService, ConsensusHex,
            GetBlockHeaderInfoRequest, GetBlockHeaderInfoResponse, GetBlockInfoRequest,
            GetBlockInfoResponse, GetBmmHStarCommitmentRequest, GetBmmHStarCommitmentResponse,
            GetChainInfoRequest, GetChainInfoResponse, GetChainTipRequest, GetChainTipResponse,
            GetCoinbasePsbtRequest, GetCoinbasePsbtResponse, GetCtipRequest, GetCtipResponse,
            GetSidechainProposalsRequest, GetSidechainProposalsResponse, GetSidechainsRequest,
            GetSidechainsResponse, GetTwoWayPegDataRequest, GetTwoWayPegDataResponse, Network,
            ReverseHex, SubscribeEventsRequest, SubscribeEventsResponse,
        },
    },
    types::SidechainNumber,
};

use async_broadcast::RecvError;
use bip300301_messages::{
    bitcoin::{self, absolute::Height, hashes::Hash, Amount, BlockHash, Transaction, TxOut},
    CoinbaseMessage,
};
use futures::{stream::BoxStream, StreamExt, TryStreamExt as _};
use miette::Result;
use tonic::{Request, Response, Status};

use crate::types;
pub use crate::validator::Validator;

fn invalid_field_value<Message>(field_name: &str, value: &str) -> tonic::Status
where
    Message: prost::Name,
{
    let err = proto::Error::invalid_field_value::<Message>(field_name, value);
    tonic::Status::invalid_argument(err.to_string())
}

fn missing_field<Message>(field_name: &str) -> tonic::Status
where
    Message: prost::Name,
{
    let err = proto::Error::missing_field::<Message>(field_name);
    tonic::Status::invalid_argument(err.to_string())
}

#[tonic::async_trait]
impl ValidatorService for Validator {
    async fn get_block_header_info(
        &self,
        request: tonic::Request<GetBlockHeaderInfoRequest>,
    ) -> Result<tonic::Response<GetBlockHeaderInfoResponse>, tonic::Status> {
        let GetBlockHeaderInfoRequest { block_hash } = request.into_inner();
        let block_hash = block_hash
            .ok_or_else(|| missing_field::<GetBlockHeaderInfoRequest>("block_hash"))?
            .decode_tonic::<GetBlockHeaderInfoRequest, _>("block_hash")?;
        let header_info = self
            .get_header_info(&block_hash)
            .map_err(|err| tonic::Status::from_error(Box::new(err)))?;
        let resp = GetBlockHeaderInfoResponse {
            header_info: Some(header_info.into()),
        };
        Ok(tonic::Response::new(resp))
    }

    async fn get_block_info(
        &self,
        request: tonic::Request<GetBlockInfoRequest>,
    ) -> Result<tonic::Response<GetBlockInfoResponse>, tonic::Status> {
        let GetBlockInfoRequest {
            block_hash,
            sidechain_id,
        } = request.into_inner();
        let block_hash = block_hash
            .ok_or_else(|| missing_field::<GetBlockInfoRequest>("block_hash"))?
            .decode_tonic::<GetBlockInfoRequest, _>("block_hash")?;
        let sidechain_id: SidechainNumber = {
            let sidechain_id: u32 =
                sidechain_id.ok_or_else(|| missing_field::<GetBlockInfoRequest>("sidechain_id"))?;
            <u8 as TryFrom<_>>::try_from(sidechain_id)
                .map_err(|_| {
                    invalid_field_value::<GetBlockInfoRequest>(
                        "sidechain_id",
                        &sidechain_id.to_string(),
                    )
                })?
                .into()
        };
        let header_info = self
            .get_header_info(&block_hash)
            .map_err(|err| tonic::Status::from_error(Box::new(err)))?;
        let block_info = self
            .get_block_info(&block_hash)
            .map_err(|err| tonic::Status::from_error(Box::new(err)))?;
        let resp = GetBlockInfoResponse {
            header_info: Some(header_info.into()),
            block_info: Some(block_info.into_proto(sidechain_id)),
        };
        Ok(tonic::Response::new(resp))
    }

    async fn get_bmm_h_star_commitment(
        &self,
        request: tonic::Request<GetBmmHStarCommitmentRequest>,
    ) -> Result<tonic::Response<GetBmmHStarCommitmentResponse>, tonic::Status> {
        let GetBmmHStarCommitmentRequest {
            block_hash,
            sidechain_id,
        } = request.into_inner();
        let block_hash = block_hash
            .ok_or_else(|| missing_field::<GetBmmHStarCommitmentRequest>("block_hash"))?
            .decode_tonic::<GetBmmHStarCommitmentRequest, _>("block_hash")?;
        let sidechain_id: SidechainNumber = {
            let sidechain_id: u32 = sidechain_id
                .ok_or_else(|| missing_field::<GetBmmHStarCommitmentRequest>("sidechain_id"))?;
            <u8 as TryFrom<_>>::try_from(sidechain_id)
                .map_err(|_| {
                    invalid_field_value::<GetBmmHStarCommitmentRequest>(
                        "sidechain_id",
                        &sidechain_id.to_string(),
                    )
                })?
                .into()
        };
        let bmm_commitments = self
            .try_get_bmm_commitments(&block_hash)
            .map_err(|err| tonic::Status::from_error(Box::new(err)))?;
        let res = match bmm_commitments {
            None => get_bmm_h_star_commitment_response::Result::BlockNotFound(
                get_bmm_h_star_commitment_response::BlockNotFoundError {
                    block_hash: Some(ReverseHex::encode(&block_hash)),
                },
            ),
            Some(bmm_commitments) => {
                let commitment = bmm_commitments.get(&sidechain_id);
                get_bmm_h_star_commitment_response::Result::Commitment(
                    get_bmm_h_star_commitment_response::Commitment {
                        commitment: commitment.map(ConsensusHex::encode),
                    },
                )
            }
        };
        let resp = GetBmmHStarCommitmentResponse { result: Some(res) };
        Ok(tonic::Response::new(resp))
    }

    async fn get_chain_info(
        &self,
        request: tonic::Request<GetChainInfoRequest>,
    ) -> Result<tonic::Response<GetChainInfoResponse>, tonic::Status> {
        let GetChainInfoRequest {} = request.into_inner();
        let network: Network = self.network().into();
        let resp = GetChainInfoResponse {
            network: network as i32,
        };
        Ok(tonic::Response::new(resp))
    }

    async fn get_chain_tip(
        &self,
        request: tonic::Request<GetChainTipRequest>,
    ) -> Result<tonic::Response<GetChainTipResponse>, tonic::Status> {
        let GetChainTipRequest {} = request.into_inner();
        let tip_hash = self
            .get_mainchain_tip()
            .map_err(|err| tonic::Status::from_error(err.into()))?;
        let header_info = self
            .get_header_info(&tip_hash)
            .map_err(|err| tonic::Status::from_error(err.into()))?;
        let resp = GetChainTipResponse {
            block_header_info: Some(header_info.into()),
        };
        Ok(tonic::Response::new(resp))
    }

    async fn get_coinbase_psbt(
        &self,
        request: Request<GetCoinbasePsbtRequest>,
    ) -> Result<Response<GetCoinbasePsbtResponse>, Status> {
        let request = request.into_inner();
        let mut messages = Vec::<CoinbaseMessage>::new();
        for propose_sidechain in request.propose_sidechains {
            let message = propose_sidechain
                .try_into()
                .map_err(|err: proto::Error| tonic::Status::invalid_argument(err.to_string()))?;
            messages.push(message);
        }
        for ack_sidechain in request.ack_sidechains {
            let message = ack_sidechain
                .try_into()
                .map_err(|err: proto::Error| tonic::Status::invalid_argument(err.to_string()))?;
            messages.push(message);
        }
        for propose_bundle in request.propose_bundles {
            let message = propose_bundle
                .try_into()
                .map_err(|err: proto::Error| tonic::Status::invalid_argument(err.to_string()))?;
            messages.push(message);
        }
        let ack_bundles = request
            .ack_bundles
            .ok_or_else(|| missing_field::<GetCoinbasePsbtRequest>("ack_bundles"))?;
        {
            let message = ack_bundles
                .try_into()
                .map_err(|err: proto::Error| tonic::Status::invalid_argument(err.to_string()))?;
            messages.push(message);
        }
        let output = messages
            .into_iter()
            .map(|message| TxOut {
                value: Amount::ZERO,
                script_pubkey: message.into(),
            })
            .collect();
        let transasction = Transaction {
            output,
            input: vec![],
            lock_time: bitcoin::absolute::LockTime::Blocks(Height::ZERO),
            version: bitcoin::transaction::Version::TWO,
        };
        let response = GetCoinbasePsbtResponse {
            psbt: Some(ConsensusHex::encode(&transasction)),
        };
        Ok(Response::new(response))
    }

    async fn get_ctip(
        &self,
        request: tonic::Request<GetCtipRequest>,
    ) -> Result<tonic::Response<GetCtipResponse>, tonic::Status> {
        let GetCtipRequest { sidechain_number } = request.into_inner();
        let sidechain_number: SidechainNumber = {
            let sidechain_number: u32 = sidechain_number
                .ok_or_else(|| missing_field::<GetCtipRequest>("sidechain_number"))?;
            <u8 as TryFrom<_>>::try_from(sidechain_number)
                .map_err(|_| {
                    invalid_field_value::<GetCtipRequest>(
                        "sidechain_number",
                        &sidechain_number.to_string(),
                    )
                })?
                .into()
        };
        let ctip = self
            .get_ctip(sidechain_number)
            .map_err(|err| tonic::Status::internal(err.to_string()))?;
        if let Some(ctip) = ctip {
            let sequence_number = self
                .get_ctip_sequence_number(sidechain_number)
                .map_err(|err| tonic::Status::internal(err.to_string()))?;
            // get_ctip returned Some(ctip) above, so we know that the sequence_number will also
            // return Some, so we just unwrap it.
            let sequence_number = sequence_number.unwrap();
            let ctip = Ctip {
                txid: Some(ConsensusHex::encode(&ctip.outpoint.txid)),
                vout: ctip.outpoint.vout,
                value: ctip.value.to_sat(),
                sequence_number,
            };
            let response = GetCtipResponse { ctip: Some(ctip) };
            Ok(Response::new(response))
        } else {
            let response = GetCtipResponse { ctip: None };
            Ok(Response::new(response))
        }
    }

    /*
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
    */

    async fn get_sidechain_proposals(
        &self,
        request: tonic::Request<GetSidechainProposalsRequest>,
    ) -> Result<tonic::Response<GetSidechainProposalsResponse>, tonic::Status> {
        let GetSidechainProposalsRequest {} = request.into_inner();
        let sidechain_proposals = self
            .get_sidechain_proposals()
            .map_err(|err| tonic::Status::internal(err.to_string()))?;
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
                        sidechain_number: u8::from(sidechain_number) as u32,
                        data: Some(data),
                        data_hash: Some(ConsensusHex::encode(&data_hash)),
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
    ) -> Result<tonic::Response<GetSidechainsResponse>, tonic::Status> {
        let GetSidechainsRequest {} = request.into_inner();
        let sidechains = self
            .get_sidechains()
            .map_err(|err| tonic::Status::internal(err.to_string()))?;
        let sidechains = sidechains
            .into_iter()
            .map(|sidechain| {
                let types::Sidechain {
                    sidechain_number,
                    data,
                    vote_count,
                    proposal_height,
                    activation_height,
                } = sidechain;
                SidechainInfo {
                    sidechain_number: u8::from(sidechain_number) as u32,
                    data: Some(data),
                    vote_count: vote_count as u32,
                    proposal_height,
                    activation_height,
                }
            })
            .collect();
        let response = GetSidechainsResponse { sidechains };
        Ok(Response::new(response))
    }

    async fn get_two_way_peg_data(
        &self,
        request: tonic::Request<GetTwoWayPegDataRequest>,
    ) -> Result<tonic::Response<GetTwoWayPegDataResponse>, tonic::Status> {
        let GetTwoWayPegDataRequest {
            sidechain_id,
            start_block_hash,
            end_block_hash,
        } = request.into_inner();
        let sidechain_id: SidechainNumber = {
            let sidechain_id: u32 = sidechain_id
                .ok_or_else(|| missing_field::<GetTwoWayPegDataRequest>("sidechain_id"))?;
            <u8 as TryFrom<_>>::try_from(sidechain_id)
                .map_err(|_| {
                    invalid_field_value::<GetTwoWayPegDataRequest>(
                        "sidechain_id",
                        &sidechain_id.to_string(),
                    )
                })?
                .into()
        };
        let start_block_hash: Option<BlockHash> = start_block_hash
            .map(|start_block_hash| {
                start_block_hash.decode_tonic::<GetTwoWayPegDataRequest, _>("start_block_hash")
            })
            .transpose()?
            .map(BlockHash::from_byte_array);
        let end_block_hash: [u8; 32] = end_block_hash
            .ok_or_else(|| missing_field::<GetTwoWayPegDataRequest>("end_block_hash"))?
            .decode_tonic::<GetTwoWayPegDataRequest, _>("end_block_hash")?;
        let end_block_hash = BlockHash::from_byte_array(end_block_hash);
        match self.get_two_way_peg_data(start_block_hash, end_block_hash) {
            Err(err) => Err(tonic::Status::from_error(Box::new(err))),
            Ok(two_way_peg_data) => {
                let two_way_peg_data = two_way_peg_data
                    .into_iter()
                    .filter_map(|two_way_peg_data| two_way_peg_data.into_proto(sidechain_id))
                    .collect();
                let resp = GetTwoWayPegDataResponse {
                    blocks: two_way_peg_data,
                };
                Ok(tonic::Response::new(resp))
            }
        }
    }

    type SubscribeEventsStream = BoxStream<'static, Result<SubscribeEventsResponse, tonic::Status>>;

    async fn subscribe_events(
        &self,
        request: tonic::Request<SubscribeEventsRequest>,
    ) -> Result<tonic::Response<Self::SubscribeEventsStream>, tonic::Status> {
        let SubscribeEventsRequest { sidechain_id } = request.into_inner();
        let sidechain_id: SidechainNumber = {
            let sidechain_id: u32 = sidechain_id
                .ok_or_else(|| missing_field::<GetTwoWayPegDataRequest>("sidechain_id"))?;
            <u8 as TryFrom<_>>::try_from(sidechain_id)
                .map_err(|_| {
                    invalid_field_value::<GetTwoWayPegDataRequest>(
                        "sidechain_id",
                        &sidechain_id.to_string(),
                    )
                })?
                .into()
        };
        let stream = futures::stream::try_unfold(self.subscribe_events(), |mut receiver| async {
            match receiver.recv_direct().await {
                Ok(event) => Ok(Some((event, receiver))),
                Err(RecvError::Closed) => Ok(None),
                Err(RecvError::Overflowed(_)) => Err(tonic::Status::resource_exhausted(
                    "Events stream closed due to overflow",
                )),
            }
        })
        .map_ok(move |event| SubscribeEventsResponse {
            event: Some(event.into_proto(sidechain_id).into()),
        })
        .boxed();
        Ok(tonic::Response::new(stream))
    }

    /*
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
    */

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

#[tonic::async_trait]
impl WalletService for Arc<crate::wallet::Wallet> {
    async fn create_new_address(
        &self,
        request: tonic::Request<CreateNewAddressRequest>,
    ) -> std::result::Result<tonic::Response<CreateNewAddressResponse>, tonic::Status> {
        let wallet = self as &Arc<crate::wallet::Wallet>;

        let address = wallet
            .get_new_address()
            .map_err(|err| tonic::Status::internal(err.to_string()))?;

        let response = CreateNewAddressResponse {
            address: address.to_string(),
        };
        Ok(tonic::Response::new(response))
    }

    async fn generate_blocks(
        &self,
        request: tonic::Request<GenerateBlocksRequest>,
    ) -> std::result::Result<tonic::Response<GenerateBlocksResponse>, tonic::Status> {
        Err(tonic::Status::new(
            tonic::Code::Unimplemented,
            "not implemented",
        ))
    }

    async fn broadcast_withdrawal_bundle(
        &self,
        request: tonic::Request<BroadcastWithdrawalBundleRequest>,
    ) -> std::result::Result<tonic::Response<BroadcastWithdrawalBundleResponse>, tonic::Status>
    {
        Err(tonic::Status::new(
            tonic::Code::Unimplemented,
            "not implemented",
        ))
    }

    async fn create_bmm_critical_data_transaction(
        &self,
        request: tonic::Request<CreateBmmCriticalDataTransactionRequest>,
    ) -> std::result::Result<tonic::Response<CreateBmmCriticalDataTransactionResponse>, tonic::Status>
    {
        Err(tonic::Status::new(
            tonic::Code::Unimplemented,
            "not implemented",
        ))
    }

    async fn create_deposit_transaction(
        &self,
        request: tonic::Request<CreateDepositTransactionRequest>,
    ) -> std::result::Result<tonic::Response<CreateDepositTransactionResponse>, tonic::Status> {
        Err(tonic::Status::new(
            tonic::Code::Unimplemented,
            "not implemented",
        ))
    }
}
