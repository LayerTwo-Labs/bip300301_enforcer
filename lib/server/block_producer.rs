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
        common::{ConsensusHex, Hex, ReverseHex},
        mainchain::{
            AckAllProposalsPolicy, CreateSidechainProposalRequest, CreateSidechainProposalResponse,
            GetBlockProducerStateRequest, GetBlockProducerStateResponse, PendingSidechainProposal,
            SetAckAllProposalsRequest, SetAckAllProposalsResponse, SetSidechainAckRequest,
            SetSidechainAckResponse, SetWithdrawalBundleAckRequest, SetWithdrawalBundleAckResponse,
            SetWithdrawalBundlePolicyRequest, SetWithdrawalBundlePolicyResponse,
            SidechainAck as SidechainAckMessage, SidechainDeclaration,
            SubmitSidechainProposalRequest, SubmitSidechainProposalResponse, WithdrawalBundleAck,
            WithdrawalBundlePolicy, create_sidechain_proposal_response,
        },
        mainchain_service::BlockProducerService,
        wrap_u32,
    },
    server::{internal_err, invalid_field_value, missing_field, parse_sidechain_id},
    types::{Event, M6id},
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

/// Reject a BIP300 message RPC while the next block — the first one whose
/// coinbase could carry the message — is still below the activation height,
/// where the validator ignores it. `impossible` states what could otherwise
/// never happen, e.g. "a sidechain proposal cannot confirm".
fn ensure_bip300_activation_height_reached(
    producer: &BlockProducer,
    impossible: &str,
) -> Result<(), ConnectError> {
    let activation_height = producer
        .validator()
        .network_params()
        .bip300_activation_height;
    let next_block_height = producer
        .validator()
        .try_get_block_height()
        .map_err(internal_err)?
        .map_or(0, |tip_height| tip_height.saturating_add(1));

    if next_block_height < activation_height {
        return Err(ConnectError::failed_precondition(format!(
            "BIP300 activates at block height {activation_height}! {impossible} before then"
        )));
    }
    Ok(())
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
    let () =
        ensure_bip300_activation_height_reached(producer, "a sidechain proposal cannot confirm")?;
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
        let () =
            ensure_bip300_activation_height_reached(self, "a sidechain ACK cannot take effect")?;
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
        let SetAckAllProposalsRequest { policy, .. } = request.to_owned_message();
        // No default: the whole point of the policy is choosing whether to
        // vote to evict running sidechains, so an unset or unrecognized value
        // is rejected rather than guessed at.
        let policy: crate::types::AckAllProposalsPolicy = policy.try_into().map_err(|err| {
            invalid_field_value::<SetAckAllProposalsRequest, _>("policy", &policy.to_string(), err)
        })?;
        self.db()
            .set_ack_policy(policy)
            .await
            .map_err(internal_err)?;
        Ok(Response::new(SetAckAllProposalsResponse::default()))
    }

    async fn set_withdrawal_bundle_policy(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, SetWithdrawalBundlePolicyRequest>,
    ) -> ServiceResult<SetWithdrawalBundlePolicyResponse> {
        let SetWithdrawalBundlePolicyRequest { policy, .. } = request.to_owned_message();
        // As with the sidechain ACK policy: no default, since the choice
        // includes actively downvoting every bundle in sight.
        let policy: crate::types::WithdrawalBundlePolicy = policy.try_into().map_err(|err| {
            invalid_field_value::<SetWithdrawalBundlePolicyRequest, _>(
                "policy",
                &policy.to_string(),
                err,
            )
        })?;
        self.db()
            .set_bundle_policy(policy)
            .await
            .map_err(internal_err)?;
        Ok(Response::new(SetWithdrawalBundlePolicyResponse::default()))
    }

    async fn set_withdrawal_bundle_ack(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, SetWithdrawalBundleAckRequest>,
    ) -> ServiceResult<SetWithdrawalBundleAckResponse> {
        let () = ensure_bip300_activation_height_reached(
            self,
            "a withdrawal bundle ACK cannot take effect",
        )?;
        let SetWithdrawalBundleAckRequest {
            sidechain_number,
            m6id,
            ack,
            ..
        } = request.to_owned_message();
        let sidechain_number = parse_sidechain_id::<SetWithdrawalBundleAckRequest>(
            sidechain_number,
            "sidechain_number",
        )?;
        let m6id: M6id = m6id
            .into_option()
            .ok_or_else(|| missing_field::<SetWithdrawalBundleAckRequest>("m6id"))?
            .decode_status::<SetWithdrawalBundleAckRequest, bitcoin::Txid>("m6id")?
            .into();
        if ack {
            self.db().ack_bundle(sidechain_number, m6id).await
        } else {
            self.db().delete_bundle_ack(sidechain_number, m6id).await
        }
        .map_err(internal_err)?;
        Ok(Response::new(SetWithdrawalBundleAckResponse::default()))
    }

    async fn get_block_producer_state(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, GetBlockProducerStateRequest>,
    ) -> ServiceResult<GetBlockProducerStateResponse> {
        let ack_policy = self.db().get_ack_policy().await.map_err(internal_err)?;
        let withdrawal_bundle_policy = self.db().get_bundle_policy().await.map_err(internal_err)?;

        let explicit_bundle_acks = self
            .db()
            .get_bundle_acks()
            .await
            .map_err(internal_err)?
            .into_iter()
            .map(|(sidechain_number, m6id)| WithdrawalBundleAck {
                sidechain_number: wrap_u32(sidechain_number.0 as u32),
                m6id: MessageField::some(ConsensusHex::encode(&m6id.0)),
            })
            .collect();

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
            ack_policy: AckAllProposalsPolicy::from(ack_policy).into(),
            explicit_acks,
            withdrawal_bundle_policy: WithdrawalBundlePolicy::from(withdrawal_bundle_policy).into(),
            explicit_bundle_acks,
        }))
    }
}
