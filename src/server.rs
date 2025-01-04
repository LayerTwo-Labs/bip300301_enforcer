use std::{borrow::Cow, collections::HashMap, str::FromStr, sync::Arc};

use bdk_wallet::bip39::Mnemonic;
use bitcoin::{
    absolute::Height,
    hashes::{hmac, ripemd160, sha256, sha512, Hash, HashEngine},
    key::Secp256k1,
    Amount, BlockHash, Transaction, TxOut,
};
use futures::{
    stream::{BoxStream, FusedStream},
    StreamExt as _,
};
use jsonrpsee::core as jsonrpsee;
use miette::IntoDiagnostic as _;
use thiserror::Error;
use tonic::{Request, Response, Status};

use crate::{
    convert,
    messages::{CoinbaseMessage, M1ProposeSidechain, M2AckSidechain, M3ProposeBundle},
    proto::{
        common::{ConsensusHex, Hex, ReverseHex},
        crypto::{
            crypto_service_server::CryptoService, HmacSha512Request, HmacSha512Response,
            Ripemd160Request, Ripemd160Response, Secp256k1SecretKeyToPublicKeyRequest,
            Secp256k1SecretKeyToPublicKeyResponse, Secp256k1SignRequest, Secp256k1SignResponse,
            Secp256k1VerifyRequest, Secp256k1VerifyResponse,
        },
        mainchain::{
            create_sidechain_proposal_response, get_bmm_h_star_commitment_response,
            get_ctip_response::Ctip, get_sidechain_proposals_response::SidechainProposal,
            get_sidechains_response::SidechainInfo, server::ValidatorService,
            wallet_service_server::WalletService, BroadcastWithdrawalBundleRequest,
            BroadcastWithdrawalBundleResponse, CreateBmmCriticalDataTransactionRequest,
            CreateBmmCriticalDataTransactionResponse, CreateDepositTransactionRequest,
            CreateDepositTransactionResponse, CreateNewAddressRequest, CreateNewAddressResponse,
            CreateSidechainProposalRequest, CreateSidechainProposalResponse, CreateWalletRequest,
            CreateWalletResponse, GenerateBlocksRequest, GenerateBlocksResponse, GetBalanceRequest,
            GetBalanceResponse, GetBlockHeaderInfoRequest, GetBlockHeaderInfoResponse,
            GetBlockInfoRequest, GetBlockInfoResponse, GetBmmHStarCommitmentRequest,
            GetBmmHStarCommitmentResponse, GetChainInfoRequest, GetChainInfoResponse,
            GetChainTipRequest, GetChainTipResponse, GetCoinbasePsbtRequest,
            GetCoinbasePsbtResponse, GetCtipRequest, GetCtipResponse, GetSidechainProposalsRequest,
            GetSidechainProposalsResponse, GetSidechainsRequest, GetSidechainsResponse,
            GetTwoWayPegDataRequest, GetTwoWayPegDataResponse, ListTransactionsRequest,
            ListTransactionsResponse, Network, SendTransactionRequest, SendTransactionResponse,
            SubscribeEventsRequest, SubscribeEventsResponse, UnlockWalletRequest,
            UnlockWalletResponse, WalletTransaction,
        },
    },
    types::{BlindedM6, Event, SidechainNumber},
    validator::Validator,
    wallet::error::WalletInitialization,
};

fn invalid_field_value<Message, Error>(
    field_name: &str,
    value: &str,
    source: Error,
) -> tonic::Status
where
    Message: prost::Name,
    Error: std::error::Error + Send + Sync + 'static,
{
    let err = crate::proto::Error::invalid_field_value::<Message, _>(field_name, value, source);
    tonic::Status::invalid_argument(err.to_string())
}

fn missing_field<Message>(field_name: &str) -> tonic::Status
where
    Message: prost::Name,
{
    let err = crate::proto::Error::missing_field::<Message>(field_name);
    tonic::Status::invalid_argument(err.to_string())
}

trait IntoStatus {
    fn into_status(self) -> tonic::Status;
}

impl IntoStatus for crate::proto::Error {
    fn into_status(self) -> tonic::Status {
        let err = anyhow::Error::from(self);
        tonic::Status::invalid_argument(format!("{err:#}"))
    }
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

        if let Some(source) = self.downcast_ref::<crate::wallet::error::WalletInitialization>() {
            let code = match source {
                crate::wallet::error::WalletInitialization::InvalidPassword => {
                    tonic::Code::InvalidArgument
                }
                crate::wallet::error::WalletInitialization::NotUnlocked => {
                    tonic::Code::FailedPrecondition
                }
                crate::wallet::error::WalletInitialization::DataMismatch => tonic::Code::Internal,
                crate::wallet::error::WalletInitialization::NotFound => tonic::Code::NotFound,
                crate::wallet::error::WalletInitialization::AlreadyExists => {
                    tonic::Code::AlreadyExists
                }
                crate::wallet::error::WalletInitialization::AlreadyUnlocked => {
                    tonic::Code::AlreadyExists
                }
            };
            return tonic::Status::new(code, format!("{self:#}"));
        }

        if let Some(source) = self.downcast_ref::<crate::wallet::error::TonicStatusError>() {
            return source.into_status();
        }

        if let Some(source) = self.downcast_ref::<crate::wallet::error::BitcoinCoreRPC>() {
            // https://github.com/bitcoin/bitcoin/blob/4036ee3f2bf587775e6f388a9bfd2bcdb8fecf1d/src/rpc/protocol.h#L80
            const BITCOIN_CORE_RPC_ERROR_H_NOT_FOUND: i32 = -18;
            match &source.error {
                jsonrpsee::client::error::Error::Call(err)
                    if err.code() == BITCOIN_CORE_RPC_ERROR_H_NOT_FOUND
                        && err.message().contains("No wallet is loaded") =>
                {
                    // Try being super precise here. Easy to confuse the /enforcer/ wallet not being
                    // loaded with the /bitcoin core/ wallet not being loaded.
                    return tonic::Status::failed_precondition(
                        "the underlying Bitcoin Core node has no loaded wallet (fix this: `bitcoin-cli loadwallet <wallet-name>`)",
                    );
                }
                _ => (),
            }
        }

        tracing::warn!("Unable to convert miette::Report to a meaningful tonic::Status: {self:?}");
        tonic::Status::new(tonic::Code::Unknown, format!("{self:#}"))
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

            SidechainNumber::try_from(raw_id).map_err(|err| {
                invalid_field_value::<GetBlockInfoRequest, _>(
                    "sidechain_id",
                    &raw_id.to_string(),
                    err,
                )
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
            block_info: Some(block_info.as_proto(sidechain_id)),
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

            SidechainNumber::try_from(raw_id).map_err(|err| {
                invalid_field_value::<GetBmmHStarCommitmentRequest, _>(
                    "sidechain_id",
                    &raw_id.to_string(),
                    err,
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
        let Some(tip_hash) = self
            .try_get_mainchain_tip()
            .into_diagnostic()
            .map_err(|err| err.into_status())?
        else {
            return Err(tonic::Status::unavailable("Validator is not synced"));
        };
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
            let m1: M1ProposeSidechain = propose_sidechain
                .try_into()
                .map_err(|err: crate::proto::Error| err.into_status())?;
            messages.push(m1.into());
        }
        for ack_sidechain in request.ack_sidechains {
            let m2: M2AckSidechain = ack_sidechain
                .try_into()
                .map_err(|err: crate::proto::Error| err.into_status())?;
            messages.push(m2.into());
        }
        for propose_bundle in request.propose_bundles {
            let m3: M3ProposeBundle = propose_bundle
                .try_into()
                .map_err(|err: crate::proto::Error| err.into_status())?;
            messages.push(m3.into());
        }
        let ack_bundles = request
            .ack_bundles
            .ok_or_else(|| missing_field::<GetCoinbasePsbtRequest>("ack_bundles"))?;
        {
            let message = ack_bundles
                .try_into()
                .map_err(|err: crate::proto::Error| err.into_status())?;
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
            .try_get_ctip(sidechain_number)
            .map_err(|err| err.into_status())?;
        if let Some(ctip) = ctip {
            let sequence_number = self
                .get_ctip_sequence_number(sidechain_number)
                .map_err(|err| err.into_status())?;
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
        let Some(mainchain_tip) = self
            .try_get_mainchain_tip()
            .into_diagnostic()
            .map_err(|err| err.into_status())?
        else {
            let response = GetSidechainProposalsResponse {
                sidechain_proposals: Vec::new(),
            };
            return Ok(Response::new(response));
        };
        let mainchain_tip_height = self
            .get_header_info(&mainchain_tip)
            .into_diagnostic()
            .map_err(|err| err.into_status())?
            .height;
        let sidechain_proposals = self
            .get_sidechains()
            .into_diagnostic()
            .map_err(|err| err.into_status())?;
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
            .get_active_sidechains()
            .into_diagnostic()
            .map_err(|err| err.into_status())?;
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

            SidechainNumber::try_from(raw_id).map_err(|err| {
                invalid_field_value::<SubscribeEventsRequest, _>(
                    "sidechain_id",
                    &raw_id.to_string(),
                    err,
                )
            })?
        };

        let stream = self
            .subscribe_events()
            .map(move |res| match res.into_diagnostic() {
                Ok(event) => Ok(SubscribeEventsResponse {
                    event: Some(event.into_proto(sidechain_id).into()),
                }),
                Err(err) => Err(err.into_status()),
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

/// Stream (non-)confirmations for a sidechain proposal
fn stream_proposal_confirmations(
    validator: &Validator,
    sidechain_proposal: crate::types::SidechainProposal,
) -> impl FusedStream<Item = Result<CreateSidechainProposalResponse, tonic::Status>> {
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
                    block_hash: Some(ReverseHex::encode(&header_info.block_hash)),
                    height: Some(header_info.height),
                    prev_block_hash: Some(ReverseHex::encode(&header_info.prev_block_hash)),
                };
                let event = create_sidechain_proposal_response::Event::NotConfirmed(notconfirmed);
                return CreateSidechainProposalResponse { event: Some(event) };
            }
        };
        let confirmed = create_sidechain_proposal_response::Confirmed {
            block_hash: Some(ReverseHex::encode(&header_info.block_hash)),
            confirmations: Some(confirms),
            height: Some(header_info.height),
            outpoint: Some((&*outpoint).into()),
            prev_block_hash: Some(ReverseHex::encode(&header_info.prev_block_hash)),
        };
        confirmations.insert(header_info.block_hash, (confirms, outpoint));
        let event = create_sidechain_proposal_response::Event::Confirmed(confirmed);
        CreateSidechainProposalResponse { event: Some(event) }
    }

    let mut confirmations = HashMap::<BlockHash, (u32, Arc<bitcoin::OutPoint>)>::new();
    validator.subscribe_events().filter_map(move |res| {
        let resp = match res.into_diagnostic() {
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
            Err(err) => Some(Err(err.into_status())),
        };
        futures::future::ready(resp)
    })
}

#[tonic::async_trait]
impl WalletService for crate::wallet::Wallet {
    type CreateSidechainProposalStream =
        BoxStream<'static, Result<CreateSidechainProposalResponse, tonic::Status>>;

    type GenerateBlocksStream = BoxStream<'static, Result<GenerateBlocksResponse, tonic::Status>>;

    async fn create_sidechain_proposal(
        &self,
        request: tonic::Request<CreateSidechainProposalRequest>,
    ) -> Result<tonic::Response<Self::CreateSidechainProposalStream>, tonic::Status> {
        let CreateSidechainProposalRequest {
            sidechain_id,
            declaration,
        } = request.into_inner();
        let sidechain_id = {
            let raw_id = sidechain_id
                .ok_or_else(|| missing_field::<CreateSidechainProposalRequest>("sidechain_id"))?;
            SidechainNumber::try_from(raw_id).map_err(|err| {
                invalid_field_value::<CreateSidechainProposalRequest, _>(
                    "sidechain_id",
                    &raw_id.to_string(),
                    err,
                )
            })?
        };
        let declaration = declaration
            .ok_or_else(|| missing_field::<CreateSidechainProposalRequest>("declaration"))?
            .try_into()
            .map_err(|err: crate::proto::Error| err.into_status())?;
        let (proposal_txout, description) =
            crate::messages::create_sidechain_proposal(sidechain_id, &declaration)
                .into_diagnostic()
                .map_err(|err| err.into_status())?;

        tracing::info!("Created sidechain proposal TX output: {:?}", proposal_txout);
        let sidechain_proposal = crate::types::SidechainProposal {
            sidechain_number: sidechain_id,
            description,
        };
        let () = self.propose_sidechain(&sidechain_proposal).map_err(|err| {
            if let rusqlite::Error::SqliteFailure(sqlite_err, _) = err {
                tracing::error!("SQLite error: {:?}", sqlite_err);

                if sqlite_err.code == rusqlite::ErrorCode::ConstraintViolation {
                    return tonic::Status::already_exists("Sidechain proposal already exists");
                }
            }

            tonic::Status::internal(err.to_string())
        })?;

        tracing::info!("Persisted sidechain proposal into DB",);

        let stream = stream_proposal_confirmations(self.validator(), sidechain_proposal).boxed();

        Ok(tonic::Response::new(stream))
    }

    async fn create_new_address(
        &self,
        _request: tonic::Request<CreateNewAddressRequest>,
    ) -> std::result::Result<tonic::Response<CreateNewAddressResponse>, tonic::Status> {
        let address = self.get_new_address().map_err(|err| err.into_status())?;

        let response = CreateNewAddressResponse {
            address: address.to_string(),
        };
        Ok(tonic::Response::new(response))
    }

    async fn generate_blocks(
        &self,
        request: tonic::Request<GenerateBlocksRequest>,
    ) -> std::result::Result<tonic::Response<Self::GenerateBlocksStream>, tonic::Status> {
        let GenerateBlocksRequest {
            blocks,
            ack_all_proposals,
        } = request.into_inner();
        let count = blocks.unwrap_or(1);

        self.verify_can_mine(count)
            .await
            .map_err(|err| err.into_status())?;

        tracing::info!("generate blocks: verified ability to mine");

        let stream = crate::wallet::Wallet::generate_blocks(self.clone(), count, ack_all_proposals)
            .map(|stream_item| match stream_item {
                Ok(block_hash) => Ok(GenerateBlocksResponse {
                    block_hash: Some(ReverseHex::encode(&block_hash)),
                }),
                Err(err) => {
                    tracing::error!("{err:#}");
                    Err(err.into_status())
                }
            })
            .boxed();
        Ok(tonic::Response::new(stream))
    }

    async fn broadcast_withdrawal_bundle(
        &self,
        request: tonic::Request<BroadcastWithdrawalBundleRequest>,
    ) -> std::result::Result<tonic::Response<BroadcastWithdrawalBundleResponse>, tonic::Status>
    {
        let BroadcastWithdrawalBundleRequest {
            sidechain_id,
            transaction,
        } = request.into_inner();
        let sidechain_id = {
            let raw_id = sidechain_id
                .ok_or_else(|| missing_field::<BroadcastWithdrawalBundleRequest>("sidechain_id"))?;
            SidechainNumber::try_from(raw_id).map_err(|err| {
                invalid_field_value::<BroadcastWithdrawalBundleRequest, _>(
                    "sidechain_id",
                    &raw_id.to_string(),
                    err,
                )
            })?
        };
        let transaction_bytes = transaction
            .ok_or_else(|| missing_field::<BroadcastWithdrawalBundleRequest>("transaction"))?;
        let transaction: Transaction = bitcoin::consensus::deserialize(&transaction_bytes)
            .map_err(|err| {
                invalid_field_value::<BroadcastWithdrawalBundleRequest, _>(
                    "transaction",
                    &hex::encode(&transaction_bytes),
                    err,
                )
            })?;
        let transaction: BlindedM6 =
            Cow::<Transaction>::Owned(transaction)
                .try_into()
                .map_err(|err| {
                    invalid_field_value::<BroadcastWithdrawalBundleRequest, _>(
                        "transaction",
                        &hex::encode(transaction_bytes),
                        err,
                    )
                })?;
        let _m6id = self
            .put_withdrawal_bundle(sidechain_id, &transaction)
            .map_err(|err| err.into_status())?;
        /*
        self.broadcast_transaction(transaction.tx().into_owned())
            .await
            .map_err(|err| err.into_status())?;
        */
        let response = BroadcastWithdrawalBundleResponse {};
        Ok(tonic::Response::new(response))
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
            .map(bdk_wallet::bitcoin::Amount::from_sat)
            .map_err(|err| {
                invalid_field_value::<CreateBmmCriticalDataTransactionRequest, _>(
                    "value_sats",
                    &value_sats.unwrap_or_default().to_string(),
                    err,
                )
            })?;

        let locktime = height
            .ok_or_else(|| missing_field::<CreateBmmCriticalDataTransactionRequest>("height"))
            .map(bdk_wallet::bitcoin::absolute::LockTime::from_height)?
            .map_err(|err| {
                invalid_field_value::<CreateBmmCriticalDataTransactionRequest, _>(
                    "height",
                    &height.unwrap_or_default().to_string(),
                    err,
                )
            })?;

        let sidechain_number = sidechain_id
            .ok_or_else(|| missing_field::<CreateBmmCriticalDataTransactionRequest>("sidechain_id"))
            .map(SidechainNumber::try_from)?
            .map_err(|err| {
                invalid_field_value::<CreateBmmCriticalDataTransactionRequest, _>(
                    "sidechain_id",
                    &sidechain_id.unwrap_or_default().to_string(),
                    err,
                )
            })?;

        match self.is_sidechain_active(sidechain_number) {
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
            .decode_tonic::<CreateBmmCriticalDataTransactionRequest, _>("critical_hash")?;

        let prev_bytes = prev_bytes
            .ok_or_else(|| missing_field::<CreateBmmCriticalDataTransactionRequest>("prev_bytes"))?
            .decode_tonic::<CreateBmmCriticalDataTransactionRequest, _>("prev_bytes")
            .map(bdk_wallet::bitcoin::BlockHash::from_byte_array)?;

        let mainchain_tip = self
            .validator()
            .get_mainchain_tip()
            .map_err(|err| tonic::Status::from_error(err.into()))?;

        // If the mainchain tip has progressed beyond this, the request is already
        // expired.
        if mainchain_tip != convert::bdk_block_hash_to_bitcoin_block_hash(prev_bytes) {
            let message = format!(
                "invalid prev_bytes {}: expected {}",
                prev_bytes, mainchain_tip
            );

            return Err(tonic::Status::invalid_argument(message));
        }

        let tx = self
            .create_bmm_request(
                sidechain_number,
                prev_bytes,
                critical_hash,
                amount,
                locktime,
            )
            .map_err(|err| err.into_status())
            .and_then(|tx| {
                tx.ok_or_else(|| {
                    tonic::Status::new(
                        tonic::Code::AlreadyExists,
                        "BMM request with same `sidechain_number` and `prev_bytes` already exists",
                    )
                })
            })
            .inspect_err(|err| {
                tracing::error!("Error creating BMM critical data transaction: {}", err);
            })?;

        let txid = tx.compute_txid();
        /*
        self.broadcast_transaction(tx)
            .await
            .map_err(|err| err.into_status())?;
        */

        let txid = convert::bdk_txid_to_bitcoin_txid(txid);

        let response = CreateBmmCriticalDataTransactionResponse {
            txid: Some(ReverseHex::encode(&txid)),
        };
        Ok(tonic::Response::new(response))
    }

    async fn create_deposit_transaction(
        &self,
        request: tonic::Request<CreateDepositTransactionRequest>,
    ) -> std::result::Result<tonic::Response<CreateDepositTransactionResponse>, tonic::Status> {
        let CreateDepositTransactionRequest {
            sidechain_id,
            address,
            value_sats,
            fee_sats,
        } = request.into_inner();

        let sidechain_number = sidechain_id
            .ok_or_else(|| missing_field::<CreateDepositTransactionRequest>("sidechain_id"))
            .map(SidechainNumber::try_from)?
            .map_err(|err| {
                invalid_field_value::<CreateDepositTransactionRequest, _>(
                    "sidechain_id",
                    &sidechain_id.unwrap_or_default().to_string(),
                    err,
                )
            })?;
        let address: Vec<u8> = address
            .ok_or_else(|| missing_field::<CreateDepositTransactionRequest>("address"))?
            .decode_tonic::<CreateDepositTransactionRequest, _>("address")?;
        if address.is_empty() {
            return Err(invalid_field_value::<CreateDepositTransactionRequest, _>(
                "address",
                &hex::encode(address),
                Error::AddressMustBeNonEmpty,
            ));
        }
        let value = value_sats
            .ok_or_else(|| missing_field::<CreateDepositTransactionRequest>("value_sats"))
            .map(Amount::from_sat)?;
        if value == Amount::ZERO {
            return Err(invalid_field_value::<CreateDepositTransactionRequest, _>(
                "value_sats",
                &value.to_string(),
                Error::ValueMustBeGreaterThanZero,
            ));
        }
        let fee = fee_sats
            .ok_or_else(|| missing_field::<CreateDepositTransactionRequest>("fee_sats"))
            .map(Amount::from_sat)?;

        if !self
            .is_sidechain_active(sidechain_number)
            .map_err(|err| err.into_status())?
        {
            return Err(tonic::Status::new(
                tonic::Code::FailedPrecondition,
                format!("sidechain {sidechain_number} is not active"),
            ));
        }

        let txid = self
            .create_deposit(sidechain_number, address, value, Some(fee))
            .await
            .map_err(|err| err.into_status())?;

        let txid = ReverseHex::encode(&txid);
        let response = CreateDepositTransactionResponse { txid: Some(txid) };
        Ok(tonic::Response::new(response))
    }

    async fn get_balance(
        &self,
        request: tonic::Request<GetBalanceRequest>,
    ) -> Result<tonic::Response<GetBalanceResponse>, tonic::Status> {
        let GetBalanceRequest {} = request.into_inner();

        let balance = self
            .get_wallet_balance()
            .await
            .map_err(|err| err.into_status())?;

        let response = GetBalanceResponse {
            confirmed_sats: balance.confirmed.to_sat(),
            pending_sats: (balance.total() - balance.confirmed).to_sat(),
        };

        Ok(tonic::Response::new(response))
    }

    async fn list_transactions(
        &self,
        _: tonic::Request<ListTransactionsRequest>,
    ) -> Result<tonic::Response<ListTransactionsResponse>, tonic::Status> {
        let transactions = self
            .list_wallet_transactions()
            .await
            .map_err(|err| err.into_status())?;

        let response = ListTransactionsResponse {
            transactions: transactions.iter().map(WalletTransaction::from).collect(),
        };
        Ok(tonic::Response::new(response))
    }

    async fn send_transaction(
        &self,
        request: tonic::Request<SendTransactionRequest>,
    ) -> Result<tonic::Response<SendTransactionResponse>, tonic::Status> {
        let SendTransactionRequest {
            destinations,
            fee_rate,
            op_return_message,
        } = request.into_inner();

        // Parse and validate all destination addresses, but assume network valid
        let destinations_validated = destinations
            .iter()
            .map(|(address, amount)| {
                bdk_wallet::bitcoin::Address::from_str(address)
                    .map_err(|e| {
                        tonic::Status::invalid_argument(format!(
                            "could not parse bitcoin address: {}",
                            e
                        ))
                    })
                    .map(|addr| (addr.assume_checked(), *amount))
            })
            .collect::<Result<HashMap<bdk_wallet::bitcoin::Address, u64>, tonic::Status>>()?;

        let fee_policy = fee_rate
            .map(|fee_rate| fee_rate.try_into())
            .transpose()
            .map_err(|err: crate::proto::Error| err.into_status())?;

        let op_return_message = op_return_message
            .map(|op_return_message| {
                op_return_message.decode_tonic::<SendTransactionRequest, _>("op_return_message")
            })
            .transpose()?;

        let txid = self
            .send_wallet_transaction(destinations_validated, fee_policy, op_return_message)
            .await
            .map_err(|err| err.into_status())?;

        let txid = ReverseHex::encode(&txid);
        let response = SendTransactionResponse { txid: Some(txid) };
        Ok(tonic::Response::new(response))
    }

    async fn unlock_wallet(
        &self,
        request: tonic::Request<UnlockWalletRequest>,
    ) -> Result<tonic::Response<UnlockWalletResponse>, tonic::Status> {
        let UnlockWalletRequest { password } = request.into_inner();
        self.unlock_existing_wallet(password.as_str())
            .map_err(|err| err.into_status())?;

        Ok(tonic::Response::new(UnlockWalletResponse {}))
    }

    async fn create_wallet(
        &self,
        request: tonic::Request<CreateWalletRequest>,
    ) -> Result<tonic::Response<CreateWalletResponse>, tonic::Status> {
        // TODO: needs a way of creating /multiple/ wallets. RPC for unloading/erasing a wallet?
        if self.is_initialized() {
            let err = WalletInitialization::AlreadyExists;
            return Err(tonic::Status::new(
                tonic::Code::AlreadyExists,
                format!("{err:#}"),
            ));
        }

        let CreateWalletRequest {
            mut mnemonic_words,
            mnemonic_path,
            password,
        } = request.into_inner();

        if !mnemonic_words.is_empty() && !mnemonic_path.is_empty() {
            return Err(tonic::Status::new(
                tonic::Code::InvalidArgument,
                "cannot provide both mnemonic and mnemonic path",
            ));
        }

        if !mnemonic_path.is_empty() {
            let read = std::fs::read_to_string(&mnemonic_path).map_err(|err| {
                tonic::Status::new(
                    tonic::Code::InvalidArgument,
                    format!("failed to read mnemonic from {}: {}", mnemonic_path, err),
                )
            })?;

            mnemonic_words = read.split_whitespace().map(|s| s.to_string()).collect();
        }

        if !mnemonic_words.is_empty() && mnemonic_words.len() != 12 {
            return Err(tonic::Status::new(
                tonic::Code::InvalidArgument,
                "mnemonic must be 12 words",
            ));
        }

        let parsed = if mnemonic_words.is_empty() {
            None
        } else {
            Some(
                Mnemonic::from_str(&mnemonic_words.join(" ")).map_err(|err| {
                    tonic::Status::new(
                        tonic::Code::InvalidArgument,
                        format!("failed to parse mnemonic: {err:#}"),
                    )
                })?,
            )
        };

        let password = if password.is_empty() {
            None
        } else {
            Some(password.as_str())
        };

        self.create_wallet(parsed, password)
            .map_err(|err| err.into_status())?;

        Ok(tonic::Response::new(CreateWalletResponse {}))
    }
}

#[derive(Debug, Error)]
enum Error {
    #[error("value must be greater than zero")]
    ValueMustBeGreaterThanZero,

    #[error("address must be non-empty")]
    AddressMustBeNonEmpty,
}

#[derive(Debug, Default)]
pub struct CryptoServiceServer;

const SECP256K1_SECRET_KEY_LENGTH: usize = 32;
const SECP256K1_PUBLIC_KEY_LENGTH: usize = 33;
const SECP256K1_SIGNATURE_COMPACT_SERIALIZATION_LENGTH: usize = 64;

#[tonic::async_trait]
impl CryptoService for CryptoServiceServer {
    async fn ripemd160(
        &self,
        request: tonic::Request<Ripemd160Request>,
    ) -> std::result::Result<tonic::Response<Ripemd160Response>, tonic::Status> {
        let Ripemd160Request { msg } = request.into_inner();
        let msg: Vec<u8> = msg
            .ok_or_else(|| missing_field::<Ripemd160Request>("msg"))?
            .decode_tonic::<Ripemd160Request, _>("msg")?;
        let digest = ripemd160::Hash::hash(&msg);
        let response = Ripemd160Response {
            digest: Some(Hex::encode(&digest.as_byte_array())),
        };
        Ok(tonic::Response::new(response))
    }

    async fn hmac_sha512(
        &self,
        request: tonic::Request<HmacSha512Request>,
    ) -> std::result::Result<tonic::Response<HmacSha512Response>, tonic::Status> {
        let HmacSha512Request { key, msg } = request.into_inner();
        let key: Vec<u8> = key
            .ok_or_else(|| missing_field::<HmacSha512Request>("key"))?
            .decode_tonic::<HmacSha512Request, _>("key")?;
        let msg: Vec<u8> = msg
            .ok_or_else(|| missing_field::<HmacSha512Request>("msg"))?
            .decode_tonic::<HmacSha512Request, _>("msg")?;
        let mut engine = hmac::HmacEngine::<sha512::Hash>::new(&key);
        engine.input(&msg);
        let hmac = hmac::Hmac::<sha512::Hash>::from_engine(engine);
        let response = HmacSha512Response {
            hmac: Some(Hex::encode(&hmac.as_byte_array())),
        };
        Ok(tonic::Response::new(response))
    }

    async fn secp256k1_secret_key_to_public_key(
        &self,
        request: tonic::Request<Secp256k1SecretKeyToPublicKeyRequest>,
    ) -> std::result::Result<tonic::Response<Secp256k1SecretKeyToPublicKeyResponse>, tonic::Status>
    {
        let Secp256k1SecretKeyToPublicKeyRequest { secret_key } = request.into_inner();
        let secret_key: [u8; SECP256K1_SECRET_KEY_LENGTH] = secret_key
            .ok_or_else(|| missing_field::<Secp256k1SecretKeyToPublicKeyRequest>("secret_key"))?
            .decode_tonic::<Secp256k1SecretKeyToPublicKeyRequest, _>("secret_key")?;
        let secret_key = bitcoin::secp256k1::SecretKey::from_slice(&secret_key).map_err(|err| {
            invalid_field_value::<Secp256k1SecretKeyToPublicKeyRequest, _>("secret_key", "", err)
        })?;
        let secp = Secp256k1::new();
        let public_key = bitcoin::secp256k1::PublicKey::from_secret_key(&secp, &secret_key);
        let public_key: [u8; SECP256K1_PUBLIC_KEY_LENGTH] = public_key.serialize();
        let response = Secp256k1SecretKeyToPublicKeyResponse {
            public_key: Some(ConsensusHex::encode(&public_key)),
        };
        Ok(tonic::Response::new(response))
    }

    async fn secp256k1_sign(
        &self,
        request: tonic::Request<Secp256k1SignRequest>,
    ) -> std::result::Result<tonic::Response<Secp256k1SignResponse>, tonic::Status> {
        let Secp256k1SignRequest {
            message,
            secret_key,
        } = request.into_inner();
        let message: Vec<u8> = message
            .ok_or_else(|| missing_field::<Secp256k1SignRequest>("message"))?
            .decode_tonic::<Secp256k1SignRequest, _>("message")?;
        let digest = sha256::Hash::hash(&message).to_byte_array();
        let message = bitcoin::secp256k1::Message::from_digest(digest);
        let secret_key: [u8; SECP256K1_SECRET_KEY_LENGTH] = secret_key
            .ok_or_else(|| missing_field::<Secp256k1SignRequest>("secret_key"))?
            .decode_tonic::<Secp256k1SignRequest, _>("secret_key")?;
        let secret_key = bitcoin::secp256k1::SecretKey::from_slice(&secret_key)
            .map_err(|err| invalid_field_value::<Secp256k1SignRequest, _>("secret_key", "", err))?;
        let secp = Secp256k1::new();
        let signature = secp.sign_ecdsa(&message, &secret_key);
        let signature = signature.serialize_compact();
        let response = Secp256k1SignResponse {
            signature: Some(Hex::encode(&signature)),
        };
        Ok(tonic::Response::new(response))
    }

    async fn secp256k1_verify(
        &self,
        request: tonic::Request<Secp256k1VerifyRequest>,
    ) -> std::result::Result<tonic::Response<Secp256k1VerifyResponse>, tonic::Status> {
        let Secp256k1VerifyRequest {
            message,
            signature,
            public_key,
        } = request.into_inner();
        let message: Vec<u8> = message
            .ok_or_else(|| missing_field::<Secp256k1VerifyRequest>("message"))?
            .decode_tonic::<Secp256k1VerifyRequest, _>("message")?;
        let digest = sha256::Hash::hash(&message).to_byte_array();
        let message = bitcoin::secp256k1::Message::from_digest(digest);
        let signature: Vec<u8> = signature
            .ok_or_else(|| missing_field::<Secp256k1VerifyRequest>("signature"))?
            .decode_tonic::<Secp256k1VerifyRequest, _>("signature")?;
        let signature_len = signature.len();
        let signature: [u8; SECP256K1_SIGNATURE_COMPACT_SERIALIZATION_LENGTH] =
            signature.try_into().map_err(|_err| {
                tonic::Status::new(
                    tonic::Code::InvalidArgument,
                    format!("invalid signature length {signature_len}, must be {SECP256K1_SIGNATURE_COMPACT_SERIALIZATION_LENGTH}"),
                )
            })?;
        let signature =
            bitcoin::secp256k1::ecdsa::Signature::from_compact(&signature).map_err(|err| {
                invalid_field_value::<Secp256k1VerifyRequest, _>(
                    "signature",
                    &hex::encode(signature),
                    err,
                )
            })?;
        let public_key: [u8; SECP256K1_PUBLIC_KEY_LENGTH] = public_key
            .ok_or_else(|| missing_field::<Secp256k1VerifyRequest>("public_key"))?
            .decode_tonic::<Secp256k1VerifyRequest, _>("public_key")?;
        let public_key = bitcoin::secp256k1::PublicKey::from_slice(&public_key).map_err(|err| {
            invalid_field_value::<Secp256k1VerifyRequest, _>(
                "public_key",
                &hex::encode(public_key),
                err,
            )
        })?;
        let secp = Secp256k1::new();
        let valid = secp.verify_ecdsa(&message, &signature, &public_key).is_ok();
        let response = Secp256k1VerifyResponse { valid };
        Ok(tonic::Response::new(response))
    }
}
