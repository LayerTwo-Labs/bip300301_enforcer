use bitcoin::{Amount, BlockHash, Transaction, TxOut, absolute::Height, hashes::Hash};
use buffa::MessageField;
use connectrpc::{ConnectError, RequestContext, Response, ServiceRequest, ServiceResult};
use futures::{StreamExt as _, stream::BoxStream};
use miette::IntoDiagnostic as _;

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
            GetCoinbasePSBTRequest, GetCoinbasePSBTResponse, GetCtipRequest, GetCtipResponse,
            GetSidechainProposalsRequest, GetSidechainProposalsResponse, GetSidechainsRequest,
            GetSidechainsResponse, GetTwoWayPegDataRequest, GetTwoWayPegDataResponse, Network,
            StopRequest, StopResponse, SubscribeEventsRequest, SubscribeEventsResponse,
            SubscribeHeaderSyncProgressRequest, SubscribeHeaderSyncProgressResponse,
            get_block_info_response, get_bmm_h_star_commitment_response,
            get_chain_info_response::Bip300Constants, get_ctip_response::Ctip,
            get_sidechain_proposals_response::SidechainProposal,
            get_sidechains_response::SidechainInfo,
        },
        mainchain_service::ValidatorService,
        wrap_u32,
    },
    server::{internal_err, missing_field, parse_sidechain_id, validator::Server},
    types::Thresholds,
};

#[expect(refining_impl_trait_reachable)]
impl ValidatorService for Server {
    async fn get_block_header_info(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, GetBlockHeaderInfoRequest>,
    ) -> ServiceResult<GetBlockHeaderInfoResponse> {
        use crate::proto::mainchain::GetBlockHeaderInfoRequest;
        let GetBlockHeaderInfoRequest {
            block_hash,
            max_ancestors,
            ..
        } = request.to_owned_message();
        let block_hash = block_hash
            .into_option()
            .ok_or_else(|| missing_field::<GetBlockHeaderInfoRequest>("block_hash"))?
            .decode_status::<GetBlockHeaderInfoRequest, _>("block_hash")?;
        let max_ancestors = max_ancestors.unwrap_or(0) as usize;
        let resp = match self
            .validator
            .try_get_header_infos(&block_hash, max_ancestors)
            .map_err(internal_err)?
        {
            Some(infos) => GetBlockHeaderInfoResponse {
                header_infos: infos.into_iter().map(Into::into).collect(),
            },
            None => GetBlockHeaderInfoResponse::default(),
        };
        Ok(Response::new(resp))
    }

    async fn get_block_info(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, GetBlockInfoRequest>,
    ) -> ServiceResult<GetBlockInfoResponse> {
        use crate::proto::mainchain::GetBlockInfoRequest;
        let GetBlockInfoRequest {
            block_hash,
            sidechain_id,
            max_ancestors,
            ..
        } = request.to_owned_message();
        let block_hash = block_hash
            .into_option()
            .ok_or_else(|| missing_field::<GetBlockInfoRequest>("block_hash"))?
            .decode_status::<GetBlockInfoRequest, _>("block_hash")?;
        let sidechain_id = parse_sidechain_id::<GetBlockInfoRequest>(sidechain_id, "sidechain_id")?;
        let max_ancestors = max_ancestors.unwrap_or(0) as usize;
        let resp = match self
            .validator
            .try_get_block_infos(&block_hash, max_ancestors)
            .map_err(internal_err)?
        {
            None => GetBlockInfoResponse::default(),
            Some(infos) => GetBlockInfoResponse {
                infos: infos
                    .into_iter()
                    .map(|(header_info, block_info)| get_block_info_response::Info {
                        header_info: MessageField::some(header_info.into()),
                        block_info: MessageField::some(block_info.as_proto(sidechain_id)),
                    })
                    .collect(),
            },
        };
        Ok(Response::new(resp))
    }

    async fn get_bmm_h_star_commitment(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, GetBmmHStarCommitmentRequest>,
    ) -> ServiceResult<GetBmmHStarCommitmentResponse> {
        use crate::proto::mainchain::GetBmmHStarCommitmentRequest;
        let GetBmmHStarCommitmentRequest {
            block_hash,
            sidechain_id,
            max_ancestors,
            ..
        } = request.to_owned_message();
        let block_hash = block_hash
            .into_option()
            .ok_or_else(|| missing_field::<GetBmmHStarCommitmentRequest>("block_hash"))?
            .decode_status::<GetBmmHStarCommitmentRequest, _>("block_hash")?;
        let sidechain_id =
            parse_sidechain_id::<GetBmmHStarCommitmentRequest>(sidechain_id, "sidechain_id")?;
        let max_ancestors = max_ancestors.unwrap_or(0) as usize;
        let bmm_commitments = self
            .validator
            .try_get_bmm_commitments(&block_hash, max_ancestors)
            .map_err(internal_err)?;
        let res = match nonempty::NonEmpty::from_vec(bmm_commitments) {
            None => get_bmm_h_star_commitment_response::Result::BlockNotFound(Box::new(
                get_bmm_h_star_commitment_response::BlockNotFoundError {
                    block_hash: MessageField::some(ReverseHex::encode(&block_hash)),
                },
            )),
            Some(nonempty::NonEmpty { head, tail }) => {
                let commitment = head
                    .get(&sidechain_id)
                    .map(|c| MessageField::some(ConsensusHex::encode(c)))
                    .unwrap_or_default();
                let ancestor_commitments = tail
                    .into_iter()
                    .map(
                        |commitments| get_bmm_h_star_commitment_response::OptionalCommitment {
                            commitment: commitments
                                .get(&sidechain_id)
                                .map(|c| MessageField::some(ConsensusHex::encode(c)))
                                .unwrap_or_default(),
                        },
                    )
                    .collect();
                get_bmm_h_star_commitment_response::Result::Commitment(Box::new(
                    get_bmm_h_star_commitment_response::Commitment {
                        commitment,
                        ancestor_commitments,
                    },
                ))
            }
        };
        Ok(Response::new(GetBmmHStarCommitmentResponse {
            result: Some(res),
        }))
    }

    async fn get_chain_info(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, GetChainInfoRequest>,
    ) -> ServiceResult<GetChainInfoResponse> {
        let bitcoin_network = self.validator.network();
        let network: Network = bitcoin_network.into();
        let Thresholds {
            withdrawal_bundle_max_age,
            withdrawal_bundle_inclusion_threshold,
            used_sidechain_slot_proposal_max_age,
            used_sidechain_slot_activation_threshold,
            unused_sidechain_slot_proposal_max_age,
            unused_sidechain_slot_activation_threshold,
        } = Thresholds::for_network(bitcoin_network);
        Ok(Response::new(GetChainInfoResponse {
            network: network.into(),
            bip300_constants: MessageField::some(Bip300Constants {
                withdrawal_bundle_max_age: withdrawal_bundle_max_age.into(),
                withdrawal_bundle_inclusion_threshold: withdrawal_bundle_inclusion_threshold.into(),
                used_sidechain_slot_proposal_max_age: used_sidechain_slot_proposal_max_age.into(),
                used_sidechain_slot_activation_threshold: used_sidechain_slot_activation_threshold
                    .into(),
                unused_sidechain_slot_proposal_max_age: unused_sidechain_slot_proposal_max_age
                    .into(),
                unused_sidechain_slot_activation_threshold:
                    unused_sidechain_slot_activation_threshold.into(),
            }),
        }))
    }

    async fn get_chain_tip(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, GetChainTipRequest>,
    ) -> ServiceResult<GetChainTipResponse> {
        let Some(tip_hash) = self
            .validator
            .try_get_mainchain_tip()
            .map_err(|err| err.builder().to_connect_error())?
        else {
            return Err(ConnectError::unavailable("Validator is not synced"));
        };
        let header_info = self
            .validator
            .get_header_info(&tip_hash)
            .map_err(internal_err)?;
        Ok(Response::new(GetChainTipResponse {
            block_header_info: MessageField::some(header_info.into()),
        }))
    }

    async fn get_coinbase_psbt(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, GetCoinbasePSBTRequest>,
    ) -> ServiceResult<GetCoinbasePSBTResponse> {
        use crate::proto::mainchain::GetCoinbasePSBTRequest;
        let request = request.to_owned_message();
        let mut messages = Vec::<CoinbaseMessage>::new();
        for propose in request.propose_sidechains {
            let m1: M1ProposeSidechain = propose.try_into()?;
            messages.push(m1.into());
        }
        for ack in request.ack_sidechains {
            let m2: M2AckSidechain = ack.try_into()?;
            messages.push(m2.into());
        }
        for propose in request.propose_bundles {
            let m3: M3ProposeBundle = propose.try_into()?;
            messages.push(m3.into());
        }
        let ack_bundles = request
            .ack_bundles
            .into_option()
            .ok_or_else(|| missing_field::<GetCoinbasePSBTRequest>("ack_bundles"))?;
        let m4: CoinbaseMessage = ack_bundles.try_into()?;
        messages.push(m4);
        let output = messages
            .into_iter()
            .map(|m| {
                Ok(TxOut {
                    value: Amount::ZERO,
                    script_pubkey: m.try_into().into_diagnostic()?,
                })
            })
            .collect::<miette::Result<Vec<_>>>()
            .map_err(internal_err)?;
        let transaction = Transaction {
            output,
            input: vec![],
            lock_time: bitcoin::absolute::LockTime::Blocks(Height::ZERO),
            version: bitcoin::transaction::Version::TWO,
        };
        Ok(Response::new(GetCoinbasePSBTResponse {
            psbt: MessageField::some(ConsensusHex::encode(&transaction)),
        }))
    }

    async fn get_ctip(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, GetCtipRequest>,
    ) -> ServiceResult<GetCtipResponse> {
        use crate::proto::mainchain::GetCtipRequest;
        let GetCtipRequest {
            sidechain_number, ..
        } = request.to_owned_message();
        let sidechain_number =
            parse_sidechain_id::<GetCtipRequest>(sidechain_number, "sidechain_number")?;
        let ctip = self
            .validator
            .try_get_ctip(sidechain_number)
            .map_err(|err| err.builder().to_connect_error())?;
        let response = if let Some(ctip) = ctip {
            let sequence_number = self
                .validator
                .get_ctip_sequence_number(sidechain_number)
                .map_err(|err| err.builder().to_connect_error())?
                // get_ctip returned Some(ctip) above, so we know that the sequence_number will also
                // return Some, so we just unwrap it.
                .unwrap();
            GetCtipResponse {
                ctip: MessageField::some(Ctip {
                    txid: MessageField::some(ReverseHex::encode(&ctip.outpoint.txid)),
                    vout: ctip.outpoint.vout,
                    value: ctip.value.to_sat(),
                    sequence_number,
                }),
            }
        } else {
            GetCtipResponse::default()
        };
        Ok(Response::new(response))
    }

    async fn get_sidechain_proposals(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, GetSidechainProposalsRequest>,
    ) -> ServiceResult<GetSidechainProposalsResponse> {
        let Some(tip) = self
            .validator
            .try_get_mainchain_tip()
            .map_err(|err| err.builder().to_connect_error())?
        else {
            return Ok(Response::new(GetSidechainProposalsResponse::default()));
        };
        let mainchain_tip_height = self
            .validator
            .get_header_info(&tip)
            .map_err(|err| err.builder().to_connect_error())?
            .height;
        let proposals = self
            .validator
            .get_sidechains()
            .map_err(|err| err.builder().to_connect_error())?;
        let sidechain_proposals = proposals
            .into_iter()
            .map(|(proposal_id, sidechain)| {
                let description = ConsensusHex::encode(&sidechain.proposal.description.0);
                let declaration =
                    crate::types::SidechainDeclaration::try_from(&sidechain.proposal.description)
                        .map(crate::proto::mainchain::SidechainDeclaration::from)
                        .ok();
                SidechainProposal {
                    sidechain_number: wrap_u32(sidechain.proposal.sidechain_number.0 as u32),
                    description: MessageField::some(description),
                    declaration: declaration.map(MessageField::some).unwrap_or_default(),
                    description_sha256d_hash: MessageField::some(ReverseHex::encode(
                        &proposal_id.description_hash,
                    )),
                    vote_count: wrap_u32(sidechain.status.vote_count as u32),
                    proposal_height: wrap_u32(sidechain.status.proposal_height),
                    proposal_age: wrap_u32(mainchain_tip_height - sidechain.status.proposal_height),
                }
            })
            .collect();
        Ok(Response::new(GetSidechainProposalsResponse {
            sidechain_proposals,
        }))
    }

    async fn get_sidechains(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, GetSidechainsRequest>,
    ) -> ServiceResult<GetSidechainsResponse> {
        let sidechains = self
            .validator
            .get_active_sidechains()
            .map_err(|err| err.builder().to_connect_error())?;
        let sidechains = sidechains.into_iter().map(SidechainInfo::from).collect();
        Ok(Response::new(GetSidechainsResponse { sidechains }))
    }

    async fn get_two_way_peg_data(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, GetTwoWayPegDataRequest>,
    ) -> ServiceResult<GetTwoWayPegDataResponse> {
        use crate::proto::mainchain::GetTwoWayPegDataRequest;
        let GetTwoWayPegDataRequest {
            sidechain_id,
            start_block_hash,
            end_block_hash,
            ..
        } = request.to_owned_message();
        let sidechain_id =
            parse_sidechain_id::<GetTwoWayPegDataRequest>(sidechain_id, "sidechain_id")?;
        let start_block_hash: Option<BlockHash> = start_block_hash
            .into_option()
            .map(|h| h.decode_status::<GetTwoWayPegDataRequest, _>("start_block_hash"))
            .transpose()?
            .map(|bytes| {
                convert::bdk_block_hash_to_bitcoin_block_hash(
                    bdk_wallet::bitcoin::BlockHash::from_byte_array(bytes),
                )
            });
        let end_block_hash: BlockHash = end_block_hash
            .into_option()
            .ok_or_else(|| missing_field::<GetTwoWayPegDataRequest>("end_block_hash"))?
            .decode_status::<GetTwoWayPegDataRequest, _>("end_block_hash")
            .map(bdk_wallet::bitcoin::BlockHash::from_byte_array)
            .map(convert::bdk_block_hash_to_bitcoin_block_hash)?;
        let two_way_peg_data = self
            .validator
            .get_two_way_peg_data(start_block_hash, end_block_hash)
            .map_err(internal_err)?;
        let blocks = two_way_peg_data
            .into_iter()
            .filter_map(|d| d.into_proto(sidechain_id))
            .collect();
        Ok(Response::new(GetTwoWayPegDataResponse { blocks }))
    }

    async fn subscribe_events(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, SubscribeEventsRequest>,
    ) -> ServiceResult<connectrpc::ServiceStream<SubscribeEventsResponse>> {
        use crate::proto::mainchain::SubscribeEventsRequest;
        let SubscribeEventsRequest { sidechain_id, .. } = request.to_owned_message();
        let sidechain_id =
            parse_sidechain_id::<SubscribeEventsRequest>(sidechain_id, "sidechain_id")?;
        let stream: BoxStream<'static, _> = self
            .validator
            .subscribe_events()
            .map(move |res| match res {
                Ok(event) => Ok(SubscribeEventsResponse {
                    event: MessageField::some(event.into_proto(sidechain_id).into()),
                }),
                Err(err) => Err(err.builder().to_connect_error()),
            })
            .boxed();
        Ok(Response::new(Box::pin(stream)))
    }

    async fn subscribe_header_sync_progress(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, SubscribeHeaderSyncProgressRequest>,
    ) -> ServiceResult<connectrpc::ServiceStream<SubscribeHeaderSyncProgressResponse>> {
        let Some(rx) = self.validator.subscribe_header_sync_progress() else {
            return Err(ConnectError::unavailable("No header sync in progress"));
        };
        let stream: BoxStream<'static, _> = tokio_stream::wrappers::WatchStream::new(rx)
            .map(|progress| Ok(progress.into()))
            .boxed();
        Ok(Response::new(Box::pin(stream)))
    }

    async fn stop(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, StopRequest>,
    ) -> ServiceResult<StopResponse> {
        if self.cancel.is_cancelled() {
            return Err(ConnectError::unavailable(
                "Validator is already shutting down",
            ));
        }
        tracing::info!("received stop request, cancelling token");
        self.cancel.cancel();
        Ok(Response::new(StopResponse::default()))
    }
}
