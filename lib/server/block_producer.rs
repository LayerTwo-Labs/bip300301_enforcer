use std::{collections::HashMap, sync::Arc};

use bitcoin::{
    BlockHash,
    hashes::{Hash as _, sha256d},
};
use buffa::MessageField;
use buffa_types::google::protobuf::UInt32Value;
use connectrpc::{
    ConnectError, RequestContext, Response, ServiceRequest, ServiceResult, ServiceStream,
};
use futures::{
    StreamExt as _,
    stream::{BoxStream, FusedStream},
};

use crate::{
    block_producer::BlockProducer,
    errors::ErrorChain,
    proto::{
        ToStatus,
        common::{Hex, ReverseHex},
        mainchain::{
            CreateSidechainProposalRequest, CreateSidechainProposalResponse,
            GetBlockProducerStateRequest, GetBlockProducerStateResponse, PendingSidechainProposal,
            SetAckAllProposalsRequest, SetAckAllProposalsResponse, SetSidechainAckRequest,
            SetSidechainAckResponse, SidechainAck as SidechainAckMessage, SidechainDeclaration,
            SubmitSidechainProposalRequest, SubmitSidechainProposalResponse,
            create_sidechain_proposal_response,
        },
        mainchain_service::BlockProducerService,
        wrap_u32,
    },
    server::{internal_err, missing_field, parse_sidechain_id},
    types::Event,
};

/// Stream (non-)confirmations for a sidechain proposal
fn stream_proposal_confirmations(
    validator: &crate::validator::Validator,
    sidechain_proposal: crate::types::SidechainProposal,
) -> impl FusedStream<Item = Result<CreateSidechainProposalResponse, ConnectError>> + use<> {
    fn connect_block_event(
        sidechain_proposal: &crate::types::SidechainProposal,
        confirmations: &mut HashMap<BlockHash, (u32, Arc<bitcoin::OutPoint>)>,
        header_info: crate::types::HeaderInfo,
        block_info: crate::types::BlockInfo,
    ) -> CreateSidechainProposalResponse {
        let (confirms, outpoint) = {
            if let Some(vout) = block_info
                .sidechain_proposals()
                .find_map(|(vout, proposal)| {
                    if *proposal == *sidechain_proposal {
                        Some(vout)
                    } else {
                        None
                    }
                })
            {
                let outpoint = bitcoin::OutPoint {
                    txid: block_info.coinbase_txid,
                    vout,
                };
                (1, Arc::new(outpoint))
            } else if let Some((prev_confirms, outpoint)) =
                confirmations.get(&header_info.prev_block_hash).cloned()
            {
                (prev_confirms, outpoint)
            } else {
                let notconfirmed = create_sidechain_proposal_response::NotConfirmed {
                    block_hash: MessageField::some(ReverseHex::encode(&header_info.block_hash)),
                    height: wrap_u32(header_info.height),
                    prev_block_hash: MessageField::some(ReverseHex::encode(
                        &header_info.prev_block_hash,
                    )),
                };
                return CreateSidechainProposalResponse {
                    event: Some(create_sidechain_proposal_response::Event::NotConfirmed(
                        Box::new(notconfirmed),
                    )),
                };
            }
        };
        let confirmed = create_sidechain_proposal_response::Confirmed {
            block_hash: MessageField::some(ReverseHex::encode(&header_info.block_hash)),
            confirmations: wrap_u32(confirms),
            height: wrap_u32(header_info.height),
            outpoint: MessageField::some((&*outpoint).into()),
            prev_block_hash: MessageField::some(ReverseHex::encode(&header_info.prev_block_hash)),
        };
        confirmations.insert(header_info.block_hash, (confirms, outpoint));
        CreateSidechainProposalResponse {
            event: Some(create_sidechain_proposal_response::Event::Confirmed(
                Box::new(confirmed),
            )),
        }
    }

    let mut confirmations = HashMap::<BlockHash, (u32, Arc<bitcoin::OutPoint>)>::new();
    validator.subscribe_events().filter_map(move |res| {
        let resp = match res {
            Ok(event) => match event {
                Event::ConnectBlock {
                    header_info,
                    block_info,
                } => {
                    let resp = connect_block_event(
                        &sidechain_proposal,
                        &mut confirmations,
                        header_info,
                        block_info,
                    );
                    Some(Ok(resp))
                }
                Event::DisconnectBlock { .. } => None,
            },
            Err(err) => Some(Err(err.builder().to_connect_error())),
        };
        futures::future::ready(resp)
    })
}

/// Shared implementation for `CreateSidechainProposal` and
/// `SubmitSidechainProposal`: validates the request fields, creates a
/// sidechain proposal (BIP300 M1), and persists it to the local database.
/// Generic over the request message type so that field errors are attributed
/// to the RPC that was actually called.
async fn create_and_persist_sidechain_proposal<Request>(
    producer: &BlockProducer,
    sidechain_id: MessageField<UInt32Value>,
    declaration: MessageField<SidechainDeclaration>,
) -> Result<crate::types::SidechainProposal, ConnectError>
where
    Request: buffa::MessageName,
{
    let sidechain_id = parse_sidechain_id::<Request>(sidechain_id, "sidechain_id")?;
    let declaration: crate::types::SidechainDeclaration = declaration
        .into_option()
        .ok_or_else(|| missing_field::<Request>("declaration"))?
        .try_into()?;
    let (proposal_txout, description) =
        crate::messages::create_sidechain_proposal(sidechain_id, &declaration).map_err(
            |err: bitcoin::script::PushBytesError| ConnectError::unknown(format!("{err:#}")),
        )?;
    tracing::info!("Created sidechain proposal TX output: {:?}", proposal_txout);
    let sidechain_proposal = crate::types::SidechainProposal {
        sidechain_number: sidechain_id,
        description,
    };
    producer
        .db()
        .propose_sidechain(&sidechain_proposal)
        .await
        .map_err(|err| {
            if let rusqlite::Error::SqliteFailure(sqlite_err, _) = err {
                tracing::error!("SQLite error: {:#}", ErrorChain::new(&sqlite_err));
                if sqlite_err.code == rusqlite::ErrorCode::ConstraintViolation {
                    return ConnectError::already_exists("Sidechain proposal already exists");
                }
            }
            ConnectError::internal(err.to_string())
        })?;
    tracing::info!("Persisted sidechain proposal into DB");
    Ok(sidechain_proposal)
}

#[expect(refining_impl_trait_reachable)]
impl BlockProducerService for BlockProducer {
    async fn create_sidechain_proposal(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, CreateSidechainProposalRequest>,
    ) -> ServiceResult<ServiceStream<CreateSidechainProposalResponse>> {
        let CreateSidechainProposalRequest {
            sidechain_id,
            declaration,
            ..
        } = request.to_owned_message();
        let sidechain_proposal = create_and_persist_sidechain_proposal::<
            CreateSidechainProposalRequest,
        >(self, sidechain_id, declaration)
        .await?;
        let stream: BoxStream<'static, _> =
            stream_proposal_confirmations(self.validator(), sidechain_proposal).boxed();
        Ok(Response::new(Box::pin(stream)))
    }

    async fn submit_sidechain_proposal(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, SubmitSidechainProposalRequest>,
    ) -> ServiceResult<SubmitSidechainProposalResponse> {
        let SubmitSidechainProposalRequest {
            sidechain_id,
            declaration,
            ..
        } = request.to_owned_message();
        let _sidechain_proposal = create_and_persist_sidechain_proposal::<
            SubmitSidechainProposalRequest,
        >(self, sidechain_id, declaration)
        .await?;
        Ok(Response::new(SubmitSidechainProposalResponse::default()))
    }

    async fn set_sidechain_ack(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, SetSidechainAckRequest>,
    ) -> ServiceResult<SetSidechainAckResponse> {
        let SetSidechainAckRequest {
            sidechain_number,
            description_sha256d_hash,
            ack,
            ..
        } = request.to_owned_message();
        let sidechain_number =
            parse_sidechain_id::<SetSidechainAckRequest>(sidechain_number, "sidechain_number")?;
        let description_hash: sha256d::Hash = description_sha256d_hash
            .into_option()
            .ok_or_else(|| missing_field::<SetSidechainAckRequest>("description_sha256d_hash"))?
            .decode_status::<SetSidechainAckRequest, _>("description_sha256d_hash")?;
        if ack {
            self.db()
                .ack_sidechain(sidechain_number, description_hash)
                .await
        } else {
            self.db()
                .nack_sidechain(sidechain_number.into(), description_hash.as_byte_array())
                .await
        }
        .map_err(internal_err)?;
        Ok(Response::new(SetSidechainAckResponse::default()))
    }

    async fn set_ack_all_proposals(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, SetAckAllProposalsRequest>,
    ) -> ServiceResult<SetAckAllProposalsResponse> {
        let SetAckAllProposalsRequest { ack_all, .. } = request.to_owned_message();
        self.db()
            .set_ack_all_proposals(ack_all)
            .await
            .map_err(internal_err)?;
        Ok(Response::new(SetAckAllProposalsResponse::default()))
    }

    async fn get_block_producer_state(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, GetBlockProducerStateRequest>,
    ) -> ServiceResult<GetBlockProducerStateResponse> {
        let ack_all_proposals = self
            .db()
            .get_ack_all_proposals()
            .await
            .map_err(internal_err)?;

        let explicit_acks = self
            .db()
            .get_sidechain_acks()
            .await
            .map_err(internal_err)?
            .into_iter()
            .map(|ack| SidechainAckMessage {
                sidechain_number: wrap_u32(ack.sidechain_number.0 as u32),
                description_sha256d_hash: MessageField::some(ReverseHex::encode(
                    &ack.description_hash,
                )),
            })
            .collect();

        let pending_proposals = self
            .db()
            .get_our_sidechain_proposals()
            .await
            .map_err(internal_err)?
            .into_iter()
            .map(|proposal| {
                // Best-effort decode; a malformed M1 still returns its raw bytes.
                let declaration =
                    crate::types::SidechainDeclaration::try_from(&proposal.description)
                        .ok()
                        .map(SidechainDeclaration::from);
                PendingSidechainProposal {
                    sidechain_number: wrap_u32(proposal.sidechain_number.0 as u32),
                    description_sha256d_hash: MessageField::some(ReverseHex::encode(
                        &proposal.description.sha256d_hash(),
                    )),
                    declaration: declaration.map(MessageField::some).unwrap_or_default(),
                    description: MessageField::some(Hex::encode(&proposal.description.0)),
                }
            })
            .collect();

        Ok(Response::new(GetBlockProducerStateResponse {
            pending_proposals,
            ack_all_proposals,
            explicit_acks,
        }))
    }
}

#[expect(refining_impl_trait_reachable)]
impl BlockProducerService for crate::wallet::Wallet {
    async fn create_sidechain_proposal(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, CreateSidechainProposalRequest>,
    ) -> ServiceResult<ServiceStream<CreateSidechainProposalResponse>> {
        self.producer()
            .create_sidechain_proposal(ctx, request)
            .await
    }

    async fn submit_sidechain_proposal(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, SubmitSidechainProposalRequest>,
    ) -> ServiceResult<SubmitSidechainProposalResponse> {
        self.producer()
            .submit_sidechain_proposal(ctx, request)
            .await
    }

    async fn set_sidechain_ack(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, SetSidechainAckRequest>,
    ) -> ServiceResult<SetSidechainAckResponse> {
        self.producer().set_sidechain_ack(ctx, request).await
    }

    async fn set_ack_all_proposals(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, SetAckAllProposalsRequest>,
    ) -> ServiceResult<SetAckAllProposalsResponse> {
        self.producer().set_ack_all_proposals(ctx, request).await
    }

    async fn get_block_producer_state(
        &self,
        ctx: RequestContext,
        request: ServiceRequest<'_, GetBlockProducerStateRequest>,
    ) -> ServiceResult<GetBlockProducerStateResponse> {
        self.producer().get_block_producer_state(ctx, request).await
    }
}
