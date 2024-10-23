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

use crate::messages::CoinbaseMessage;
use async_broadcast::RecvError;
use bdk::bitcoin::hashes::Hash as _;
use bitcoin::{self, absolute::Height, Amount, BlockHash, Transaction, TxOut};
use futures::{stream::BoxStream, StreamExt, TryStreamExt as _};
use miette::{IntoDiagnostic, Result};
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

fn bdk_block_hash_to_bitcoin_block_hash(hash: bdk::bitcoin::BlockHash) -> bitcoin::BlockHash {
    let bytes = hash.as_byte_array().to_vec();

    use bitcoin::hashes::sha256d::Hash;
    use bitcoin::hashes::Hash as Hm;
    let hash: bitcoin::hashes::sha256d::Hash = Hash::from_slice(&bytes).unwrap();

    bitcoin::BlockHash::from_raw_hash(hash)
}

fn bdk_txid_to_bitcoin_txid(hash: bdk::bitcoin::Txid) -> bitcoin::Txid {
    let bytes = hash.as_byte_array().to_vec();

    use bitcoin::hashes::sha256d::Hash;
    use bitcoin::hashes::Hash as Hm;
    let hash: bitcoin::hashes::sha256d::Hash = Hash::from_slice(&bytes).unwrap();

    bitcoin::Txid::from_raw_hash(hash)
}

trait IntoStatus {
    fn into_status(self) -> tonic::Status;
}

// The idea here is to centralize conversion of lower layer errors into something meaningful
// out from the API.
//
// Lower layer errors that return `miette::Report` can be easily turned into a meaningful
// API response by just doing `into_status()`. We also get the additional benefit of a singular
// place to add logs for unexpected errors.
impl IntoStatus for miette::Report {
    fn into_status(self) -> tonic::Status {
        if let Some(source) = self.downcast_ref::<crate::wallet::error::ElectrumError>() {
            return source.clone().into();
        }

        tracing::warn!("Unable to convert miette::Report to a meaningful tonic::Status: {self:?}");
        tonic::Status::new(tonic::Code::Unknown, self.to_string())
    }
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
        let sidechain_id = {
            let raw_id =
                sidechain_id.ok_or_else(|| missing_field::<GetBlockInfoRequest>("sidechain_id"))?;

            SidechainNumber::try_from(raw_id).map_err(|_| {
                invalid_field_value::<GetBlockInfoRequest>("sidechain_id", &raw_id.to_string())
            })?
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

        let sidechain_id = {
            let raw_id = sidechain_id
                .ok_or_else(|| missing_field::<GetBmmHStarCommitmentRequest>("sidechain_id"))?;

            SidechainNumber::try_from(raw_id).map_err(|_| {
                invalid_field_value::<GetBmmHStarCommitmentRequest>(
                    "sidechain_id",
                    &raw_id.to_string(),
                )
            })?
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
        let tip_hash = self.get_mainchain_tip().map_err(|err| err.into_status())?;

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
            .map(|message| {
                Ok(TxOut {
                    value: Amount::ZERO,
                    script_pubkey: message.try_into().into_diagnostic()?,
                })
            })
            .collect::<Result<Vec<_>>>()
            .map_err(|err| tonic::Status::internal(err.to_string()))?;
        let transaction = Transaction {
            output,
            input: vec![],
            lock_time: bitcoin::absolute::LockTime::Blocks(Height::ZERO),
            version: bitcoin::transaction::Version::TWO,
        };
        let response = GetCoinbasePsbtResponse {
            psbt: Some(ConsensusHex::encode(&transaction)),
        };
        Ok(Response::new(response))
    }

    async fn get_ctip(
        &self,
        request: tonic::Request<GetCtipRequest>,
    ) -> Result<tonic::Response<GetCtipResponse>, tonic::Status> {
        let GetCtipRequest { sidechain_number } = request.into_inner();
        let sidechain_number = {
            let raw_id = sidechain_number
                .ok_or_else(|| missing_field::<GetCtipRequest>("sidechain_number"))?;

            SidechainNumber::try_from(raw_id).map_err(|_| {
                invalid_field_value::<GetCtipRequest>("sidechain_number", &raw_id.to_string())
            })?
        };

        let ctip = self
            .get_ctip(sidechain_number)
            .map_err(|err| err.into_status())?;
        if let Some(ctip) = ctip {
            let sequence_number = self
                .get_ctip_sequence_number(sidechain_number)
                .map_err(|err| err.into_status())?;
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
            .map_err(|err| err.into_status())?;
        let sidechain_proposals = sidechain_proposals
            .into_iter()
            .map(|(data_hash, proposal)| SidechainProposal {
                sidechain_number: u8::from(proposal.sidechain_number) as u32,
                data: Some(proposal.data.clone()),
                declaration: (&proposal)
                    .try_into()
                    .ok()
                    .map(|(_, declaration)| declaration.into()),
                data_hash: Some(ConsensusHex::encode(&data_hash)),
                vote_count: proposal.vote_count as u32,
                proposal_height: proposal.proposal_height,
                proposal_age: 0,
            })
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
        let sidechains = self.get_sidechains().map_err(|err| err.into_status())?;
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

        let sidechain_id = {
            let raw_id = sidechain_id
                .ok_or_else(|| missing_field::<GetTwoWayPegDataRequest>("sidechain_id"))?;

            SidechainNumber::try_from(raw_id).map_err(|_| {
                invalid_field_value::<GetTwoWayPegDataRequest>("sidechain_id", &raw_id.to_string())
            })?
        };

        let start_block_hash: Option<BlockHash> = start_block_hash
            .map(|start_block_hash| {
                start_block_hash.decode_tonic::<GetTwoWayPegDataRequest, _>("start_block_hash")
            })
            .transpose()?
            .map(|bytes| {
                bdk_block_hash_to_bitcoin_block_hash(bdk::bitcoin::BlockHash::from_byte_array(
                    bytes,
                ))
            });

        let end_block_hash: BlockHash = end_block_hash
            .ok_or_else(|| missing_field::<GetTwoWayPegDataRequest>("end_block_hash"))?
            .decode_tonic::<GetTwoWayPegDataRequest, _>("end_block_hash")
            .map(bdk::bitcoin::BlockHash::from_byte_array)
            .map(bdk_block_hash_to_bitcoin_block_hash)?;

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

        let sidechain_id = {
            let raw_id = sidechain_id
                .ok_or_else(|| missing_field::<SubscribeEventsRequest>("sidechain_id"))?;

            SidechainNumber::try_from(raw_id).map_err(|_| {
                invalid_field_value::<SubscribeEventsRequest>("sidechain_id", &raw_id.to_string())
            })?
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
        _request: tonic::Request<CreateNewAddressRequest>,
    ) -> std::result::Result<tonic::Response<CreateNewAddressResponse>, tonic::Status> {
        let wallet = self as &Arc<crate::wallet::Wallet>;

        let address = wallet.get_new_address().map_err(|err| err.into_status())?;

        let response = CreateNewAddressResponse {
            address: address.to_string(),
        };
        Ok(tonic::Response::new(response))
    }

    async fn generate_blocks(
        &self,
        request: tonic::Request<GenerateBlocksRequest>,
    ) -> std::result::Result<tonic::Response<GenerateBlocksResponse>, tonic::Status> {
        // If we're not on regtest, this won't work!
        if self.network() != bitcoin::Network::Regtest {
            return Err(tonic::Status::failed_precondition(
                "can only generate blocks on regtest",
            ));
        }

        // FIXME: Remove `optional` from the `blocks` parameter in the proto file.
        let count = request.into_inner().blocks.unwrap_or(1);
        self.generate(count)
            .await
            .map_err(|err| err.into_status())?;
        let response = GenerateBlocksResponse {};
        Ok(tonic::Response::new(response))
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

    // Legacy Bitcoin Core-based implementation
    // https://github.com/LayerTwo-Labs/mainchain/blob/05e71917042132202248c0c917f8ef120a2a5251/src/wallet/rpcwallet.cpp#L3863-L4008
    async fn create_bmm_critical_data_transaction(
        &self,
        request: tonic::Request<CreateBmmCriticalDataTransactionRequest>,
    ) -> std::result::Result<tonic::Response<CreateBmmCriticalDataTransactionResponse>, tonic::Status>
    {
        let CreateBmmCriticalDataTransactionRequest {
            sidechain_id,
            value_sats,
            height,
            critical_hash,
            prev_bytes,
        } = request.into_inner();

        let amount = value_sats
            .ok_or_else(|| missing_field::<CreateBmmCriticalDataTransactionRequest>("value_sats"))
            .map(bdk::bitcoin::Amount::from_sat)
            .map_err(|_| {
                invalid_field_value::<CreateBmmCriticalDataTransactionRequest>(
                    "value_sats",
                    &value_sats.unwrap_or_default().to_string(),
                )
            })?;

        let locktime = height
            .ok_or_else(|| missing_field::<CreateBmmCriticalDataTransactionRequest>("height"))
            .map(bdk::bitcoin::absolute::LockTime::from_height)?
            .map_err(|_| {
                invalid_field_value::<CreateBmmCriticalDataTransactionRequest>(
                    "height",
                    &height.unwrap_or_default().to_string(),
                )
            })?;

        let sidechain_number = sidechain_id
            .ok_or_else(|| missing_field::<CreateBmmCriticalDataTransactionRequest>("sidechain_id"))
            .map(SidechainNumber::try_from)?
            .map_err(|_| {
                invalid_field_value::<CreateBmmCriticalDataTransactionRequest>(
                    "sidechain_id",
                    &sidechain_id.unwrap_or_default().to_string(),
                )
            })?;

        match self.is_sidechain_active(sidechain_number).await {
            Ok(false) => {
                return Err(tonic::Status::failed_precondition(
                    "sidechain is not active",
                ))
            }
            Ok(true) => (),
            Err(err) => return Err(tonic::Status::from_error(err.into())),
        }

        // This is also called H*
        let critical_hash = critical_hash
            .ok_or_else(|| {
                missing_field::<CreateBmmCriticalDataTransactionRequest>("critical_hash")
            })?
            .decode_tonic::<CreateBmmCriticalDataTransactionRequest, _>("critical_hash")
            .map(bdk::bitcoin::BlockHash::from_byte_array)?;

        let prev_bytes = prev_bytes
            .ok_or_else(|| missing_field::<CreateBmmCriticalDataTransactionRequest>("prev_bytes"))?
            .decode_tonic::<CreateBmmCriticalDataTransactionRequest, _>("prev_bytes")
            .map(bdk::bitcoin::BlockHash::from_byte_array)?;

        let mainchain_tip = self
            .get_mainchain_tip()
            .map_err(|err| tonic::Status::from_error(err.into()))?;

        // If the mainchain tip has progressed beyond this, the request is already
        // expired.
        if mainchain_tip != bdk_block_hash_to_bitcoin_block_hash(prev_bytes) {
            let message = format!(
                "invalid prev_bytes {}: expected {}",
                prev_bytes, mainchain_tip
            );

            return Err(tonic::Status::invalid_argument(message));
        }

        let tx = self
            .create_bmm_request(
                sidechain_number,
                critical_hash,
                prev_bytes,
                amount,
                locktime,
            )
            .await
            .map_err(|err| err.into_status())
            .inspect_err(|err| {
                tracing::error!("Error creating BMM critical data transaction: {}", err);
            })?;

        let txid = tx.txid();
        self.broadcast_transaction(tx)
            .await
            .map_err(|err| err.into_status())?;

        let txid = bdk_txid_to_bitcoin_txid(txid);

        let response = CreateBmmCriticalDataTransactionResponse {
            txid: Some(ConsensusHex::encode(&txid)),
        };
        Ok(tonic::Response::new(response))
    }

    async fn create_deposit_transaction(
        &self,
        _request: tonic::Request<CreateDepositTransactionRequest>,
    ) -> std::result::Result<tonic::Response<CreateDepositTransactionResponse>, tonic::Status> {
        Err(tonic::Status::new(
            tonic::Code::Unimplemented,
            "not implemented",
        ))
    }
}
