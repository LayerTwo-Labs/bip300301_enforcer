use bitcoin::{Amount, BlockHash, Transaction, TxOut, absolute::Height, hashes::Hash};
use futures::{StreamExt as _, stream::BoxStream};
use miette::IntoDiagnostic as _;
use tonic::{Request, Response, Status};

use crate::{
    convert,
    messages::{CoinbaseMessage, M1ProposeSidechain, M2AckSidechain, M3ProposeBundle},
    proto::{
        ToStatus as _,
        common::{ConsensusHex, ReverseHex},
        mainchain::{
            GetBlockHeaderInfoRequest, GetBlockHeaderInfoResponse, GetBlockInfoRequest,
            GetBlockInfoResponse, GetBmmHStarCommitmentRequest, GetBmmHStarCommitmentResponse,
            GetChainInfoRequest, GetChainInfoResponse, GetChainTipRequest, GetChainTipResponse,
            GetCoinbasePsbtRequest, GetCoinbasePsbtResponse, GetCtipRequest, GetCtipResponse,
            GetSidechainProposalsRequest, GetSidechainProposalsResponse, GetSidechainsRequest,
            GetSidechainsResponse, GetTwoWayPegDataRequest, GetTwoWayPegDataResponse, Network,
            StopRequest, StopResponse, SubscribeEventsRequest, SubscribeEventsResponse,
            SubscribeHeaderSyncProgressRequest, SubscribeHeaderSyncProgressResponse,
            get_block_info_response, get_bmm_h_star_commitment_response, get_ctip_response::Ctip,
            get_sidechain_proposals_response::SidechainProposal,
            get_sidechains_response::SidechainInfo, server::ValidatorService,
        },
    },
    server::{invalid_field_value, missing_field, validator::Server},
    types::SidechainNumber,
};

#[tonic::async_trait]
impl ValidatorService for Server {
    async fn get_block_header_info(
        &self,
        request: tonic::Request<GetBlockHeaderInfoRequest>,
    ) -> Result<tonic::Response<GetBlockHeaderInfoResponse>, tonic::Status> {
        let GetBlockHeaderInfoRequest {
            block_hash,
            max_ancestors,
        } = request.into_inner();
        let block_hash = block_hash
            .ok_or_else(|| missing_field::<GetBlockHeaderInfoRequest>("block_hash"))?
            .decode_tonic::<GetBlockHeaderInfoRequest, _>("block_hash")?;
        let resp = match self
            .validator
            .try_get_header_infos(&block_hash, max_ancestors.unwrap_or(0) as usize)
            .map_err(|err| tonic::Status::from_error(Box::new(err)))?
        {
            Some(header_infos) => GetBlockHeaderInfoResponse {
                header_infos: header_infos
                    .into_iter()
                    .map(|header_info| header_info.into())
                    .collect(),
            },
            None => GetBlockHeaderInfoResponse {
                header_infos: Vec::new(),
            },
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
            max_ancestors,
        } = request.into_inner();
        let block_hash = block_hash
            .ok_or_else(|| missing_field::<GetBlockInfoRequest>("block_hash"))?
            .decode_tonic::<GetBlockInfoRequest, _>("block_hash")?;
        let sidechain_id = {
            let raw_id =
                sidechain_id.ok_or_else(|| missing_field::<GetBlockInfoRequest>("sidechain_id"))?;

            SidechainNumber::try_from(raw_id).map_err(|err| {
                invalid_field_value::<GetBlockInfoRequest, _>(
                    "sidechain_id",
                    &raw_id.to_string(),
                    err,
                )
            })?
        };
        let resp = match self
            .validator
            .try_get_block_infos(&block_hash, max_ancestors.unwrap_or(0) as usize)
            .map_err(|err| tonic::Status::from_error(Box::new(err)))?
        {
            None => GetBlockInfoResponse { infos: Vec::new() },
            Some(block_infos) => GetBlockInfoResponse {
                infos: block_infos
                    .into_iter()
                    .map(|(header_info, block_info)| get_block_info_response::Info {
                        header_info: Some(header_info.into()),
                        block_info: Some(block_info.as_proto(sidechain_id)),
                    })
                    .collect(),
            },
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
            max_ancestors,
        } = request.into_inner();
        let block_hash = block_hash
            .ok_or_else(|| missing_field::<GetBmmHStarCommitmentRequest>("block_hash"))?
            .decode_tonic::<GetBmmHStarCommitmentRequest, _>("block_hash")?;

        let sidechain_id = {
            let raw_id = sidechain_id
                .ok_or_else(|| missing_field::<GetBmmHStarCommitmentRequest>("sidechain_id"))?;

            SidechainNumber::try_from(raw_id).map_err(|err| {
                invalid_field_value::<GetBmmHStarCommitmentRequest, _>(
                    "sidechain_id",
                    &raw_id.to_string(),
                    err,
                )
            })?
        };
        let bmm_commitments = self
            .validator
            .try_get_bmm_commitments(&block_hash, max_ancestors.unwrap_or(0) as usize)
            .map_err(|err| tonic::Status::from_error(Box::new(err)))?;
        let res = match nonempty::NonEmpty::from_vec(bmm_commitments) {
            None => {
                let err = get_bmm_h_star_commitment_response::BlockNotFoundError {
                    block_hash: Some(ReverseHex::encode(&block_hash)),
                };
                get_bmm_h_star_commitment_response::Result::BlockNotFound(err)
            }
            Some(nonempty::NonEmpty { head, tail }) => {
                get_bmm_h_star_commitment_response::Result::Commitment(
                    get_bmm_h_star_commitment_response::Commitment {
                        commitment: head.get(&sidechain_id).map(ConsensusHex::encode),
                        ancestor_commitments: tail
                            .into_iter()
                            .map(|commitments| {
                                get_bmm_h_star_commitment_response::OptionalCommitment {
                                    commitment: commitments
                                        .get(&sidechain_id)
                                        .map(ConsensusHex::encode),
                                }
                            })
                            .collect(),
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
        let network: Network = self.validator.network().into();
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
        let Some(tip_hash) = self
            .validator
            .try_get_mainchain_tip()
            .map_err(|err| err.builder().to_status())?
        else {
            return Err(tonic::Status::unavailable("Validator is not synced"));
        };
        let header_info = self
            .validator
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
            let m1: M1ProposeSidechain = propose_sidechain
                .try_into()
                .map_err(|err: crate::proto::Error| err.builder().to_status())?;
            messages.push(m1.into());
        }
        for ack_sidechain in request.ack_sidechains {
            let m2: M2AckSidechain = ack_sidechain
                .try_into()
                .map_err(|err: crate::proto::Error| err.builder().to_status())?;
            messages.push(m2.into());
        }
        for propose_bundle in request.propose_bundles {
            let m3: M3ProposeBundle = propose_bundle
                .try_into()
                .map_err(|err: crate::proto::Error| err.builder().to_status())?;
            messages.push(m3.into());
        }
        let ack_bundles = request
            .ack_bundles
            .ok_or_else(|| missing_field::<GetCoinbasePsbtRequest>("ack_bundles"))?;
        {
            let message = ack_bundles
                .try_into()
                .map_err(|err: crate::proto::Error| err.builder().to_status())?;
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
            .collect::<miette::Result<Vec<_>>>()
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

            SidechainNumber::try_from(raw_id).map_err(|err| {
                invalid_field_value::<GetCtipRequest, _>(
                    "sidechain_number",
                    &raw_id.to_string(),
                    err,
                )
            })?
        };

        let ctip = self
            .validator
            .try_get_ctip(sidechain_number)
            .map_err(|err| err.builder().to_status())?;
        if let Some(ctip) = ctip {
            let sequence_number = self
                .validator
                .get_ctip_sequence_number(sidechain_number)
                .map_err(|err| err.builder().to_status())?;
            // get_ctip returned Some(ctip) above, so we know that the sequence_number will also
            // return Some, so we just unwrap it.
            let sequence_number = sequence_number.unwrap();
            let ctip = Ctip {
                txid: Some(ReverseHex::encode(&ctip.outpoint.txid)),
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

    async fn get_sidechain_proposals(
        &self,
        request: tonic::Request<GetSidechainProposalsRequest>,
    ) -> Result<tonic::Response<GetSidechainProposalsResponse>, tonic::Status> {
        let GetSidechainProposalsRequest {} = request.into_inner();
        let Some(mainchain_tip) = self
            .validator
            .try_get_mainchain_tip()
            .map_err(|err| err.builder().to_status())?
        else {
            let response = GetSidechainProposalsResponse {
                sidechain_proposals: Vec::new(),
            };
            return Ok(Response::new(response));
        };
        let mainchain_tip_height = self
            .validator
            .get_header_info(&mainchain_tip)
            .map_err(|err| err.builder().to_status())?
            .height;
        let sidechain_proposals = self
            .validator
            .get_sidechains()
            .map_err(|err| err.builder().to_status())?;
        let sidechain_proposals = sidechain_proposals
            .into_iter()
            .map(|(proposal_id, sidechain)| {
                let description = ConsensusHex::encode(&sidechain.proposal.description.0);
                let declaration =
                    crate::types::SidechainDeclaration::try_from(&sidechain.proposal.description)
                        .map(crate::proto::mainchain::SidechainDeclaration::from)
                        .ok();
                SidechainProposal {
                    sidechain_number: Some(sidechain.proposal.sidechain_number.0 as u32),
                    description: Some(description),
                    declaration,
                    description_sha256d_hash: Some(ReverseHex::encode(
                        &proposal_id.description_hash,
                    )),
                    vote_count: Some(sidechain.status.vote_count as u32),
                    proposal_height: Some(sidechain.status.proposal_height),
                    proposal_age: Some(mainchain_tip_height - sidechain.status.proposal_height),
                }
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
        let sidechains = self
            .validator
            .get_active_sidechains()
            .map_err(|err| err.builder().to_status())?;
        let sidechains = sidechains.into_iter().map(SidechainInfo::from).collect();
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

            SidechainNumber::try_from(raw_id).map_err(|err| {
                invalid_field_value::<GetTwoWayPegDataRequest, _>(
                    "sidechain_id",
                    &raw_id.to_string(),
                    err,
                )
            })?
        };

        let start_block_hash: Option<BlockHash> = start_block_hash
            .map(|start_block_hash| {
                start_block_hash.decode_tonic::<GetTwoWayPegDataRequest, _>("start_block_hash")
            })
            .transpose()?
            .map(|bytes| {
                convert::bdk_block_hash_to_bitcoin_block_hash(
                    bdk_wallet::bitcoin::BlockHash::from_byte_array(bytes),
                )
            });

        let end_block_hash: BlockHash = end_block_hash
            .ok_or_else(|| missing_field::<GetTwoWayPegDataRequest>("end_block_hash"))?
            .decode_tonic::<GetTwoWayPegDataRequest, _>("end_block_hash")
            .map(bdk_wallet::bitcoin::BlockHash::from_byte_array)
            .map(convert::bdk_block_hash_to_bitcoin_block_hash)?;

        match self
            .validator
            .get_two_way_peg_data(start_block_hash, end_block_hash)
        {
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

            SidechainNumber::try_from(raw_id).map_err(|err| {
                invalid_field_value::<SubscribeEventsRequest, _>(
                    "sidechain_id",
                    &raw_id.to_string(),
                    err,
                )
            })?
        };

        let stream = self
            .validator
            .subscribe_events()
            .map(move |res| match res {
                Ok(event) => Ok(SubscribeEventsResponse {
                    event: Some(event.into_proto(sidechain_id).into()),
                }),
                Err(err) => Err(err.builder().to_status()),
            })
            .boxed();
        Ok(tonic::Response::new(stream))
    }

    type SubscribeHeaderSyncProgressStream =
        BoxStream<'static, Result<SubscribeHeaderSyncProgressResponse, tonic::Status>>;

    async fn subscribe_header_sync_progress(
        &self,
        request: tonic::Request<SubscribeHeaderSyncProgressRequest>,
    ) -> Result<tonic::Response<Self::SubscribeHeaderSyncProgressStream>, tonic::Status> {
        let SubscribeHeaderSyncProgressRequest {} = request.into_inner();
        let Some(rx) = self.validator.subscribe_header_sync_progress() else {
            return Err(tonic::Status::unavailable("No header sync in progress"));
        };
        let stream = tokio_stream::wrappers::WatchStream::new(rx)
            .map(|progress| Ok(progress.into()))
            .boxed();
        Ok(tonic::Response::new(stream))
    }

    async fn stop(
        &self,
        _: tonic::Request<StopRequest>,
    ) -> Result<tonic::Response<StopResponse>, tonic::Status> {
        let mut shutdown_tx = self.shutdown_tx.clone();
        if shutdown_tx.is_closed() {
            return Err(tonic::Status::unavailable("Shutdown channel is closed"));
        }

        tracing::info!("received stop request, sending on shutdown channel");

        match shutdown_tx.try_send(()) {
            Ok(_) => Ok(tonic::Response::new(StopResponse {})),
            Err(err) => {
                let msg = format!("Failed to send shutdown signal: {err:#}");
                Err(tonic::Status::unavailable(msg))
            }
        }
    }
}
