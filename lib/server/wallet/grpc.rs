use std::{borrow::Cow, collections::HashMap, str::FromStr, sync::Arc};

use bdk_wallet::bip39::Mnemonic;
use bitcoin::{Address, Amount, BlockHash, Transaction, hashes::Hash as _};
use futures::{
    StreamExt as _,
    stream::{BoxStream, FusedStream},
};
use thiserror::Error;

use crate::{
    convert,
    errors::ErrorChain,
    proto::{
        StatusBuilder, ToStatus,
        common::ReverseHex,
        mainchain::{
            BroadcastWithdrawalBundleRequest, BroadcastWithdrawalBundleResponse,
            CreateBmmCriticalDataTransactionRequest, CreateBmmCriticalDataTransactionResponse,
            CreateDepositTransactionRequest, CreateDepositTransactionResponse,
            CreateNewAddressRequest, CreateNewAddressResponse, CreateSidechainProposalRequest,
            CreateSidechainProposalResponse, CreateWalletRequest, CreateWalletResponse,
            GenerateBlocksRequest, GenerateBlocksResponse, GetBalanceRequest, GetBalanceResponse,
            GetInfoRequest, GetInfoResponse, ListSidechainDepositTransactionsRequest,
            ListSidechainDepositTransactionsResponse, ListTransactionsRequest,
            ListTransactionsResponse, ListUnspentOutputsRequest, ListUnspentOutputsResponse,
            SendTransactionRequest, SendTransactionResponse, UnlockWalletRequest,
            UnlockWalletResponse, WalletTransaction, create_sidechain_proposal_response,
            get_info_response,
            list_sidechain_deposit_transactions_response::SidechainDepositTransaction,
            list_unspent_outputs_response, send_transaction_request::RequiredUtxo,
            wallet_service_server::WalletService,
        },
    },
    server::{invalid_field_value, missing_field},
    types::{BlindedM6, Event, SidechainNumber},
    wallet::{CreateTransactionParams, error::WalletInitialization},
};

/// Stream (non-)confirmations for a sidechain proposal
fn stream_proposal_confirmations(
    validator: &crate::validator::Validator,
    sidechain_proposal: crate::types::SidechainProposal,
) -> impl FusedStream<Item = Result<CreateSidechainProposalResponse, tonic::Status>> + use<> {
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
            Err(err) => Some(Err(err.builder().to_status())),
        };
        futures::future::ready(resp)
    })
}

#[tonic::async_trait]
impl WalletService for crate::wallet::Wallet {
    type CreateSidechainProposalStream =
        BoxStream<'static, Result<CreateSidechainProposalResponse, tonic::Status>>;

    type GenerateBlocksStream = BoxStream<'static, Result<GenerateBlocksResponse, tonic::Status>>;

    async fn get_info(
        &self,
        _request: tonic::Request<GetInfoRequest>,
    ) -> Result<tonic::Response<GetInfoResponse>, tonic::Status> {
        let info = self
            .get_wallet_info()
            .await
            .map_err(|err| err.builder().to_status())?;

        let response = GetInfoResponse {
            network: info.network.to_string(),
            transaction_count: info.transaction_count as u32,
            unspent_output_count: info.unspent_output_count as u32,
            descriptors: info
                .keychain_descriptors
                .iter()
                .map(|(kind, descriptor)| {
                    (
                        match kind {
                            bdk_wallet::KeychainKind::External => "external".to_string(),
                            bdk_wallet::KeychainKind::Internal => "internal".to_string(),
                        },
                        descriptor.to_string(),
                    )
                })
                .collect(),
            tip: Some(get_info_response::Tip {
                height: info.tip.1,
                hash: Some(ReverseHex::encode(&info.tip.0)),
            }),
        };
        Ok(tonic::Response::new(response))
    }

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
            .map_err(|err: crate::proto::Error| err.builder().to_status())?;
        let (proposal_txout, description) =
            crate::messages::create_sidechain_proposal(sidechain_id, &declaration).map_err(
                |err: bitcoin::script::PushBytesError| tonic::Status::unknown(format!("{err:#}")),
            )?;

        tracing::info!("Created sidechain proposal TX output: {:?}", proposal_txout);
        let sidechain_proposal = crate::types::SidechainProposal {
            sidechain_number: sidechain_id,
            description,
        };
        let () = self
            .propose_sidechain(&sidechain_proposal)
            .await
            .map_err(|err| {
                if let rusqlite::Error::SqliteFailure(sqlite_err, _) = err {
                    tracing::error!("SQLite error: {:#}", ErrorChain::new(&sqlite_err));

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
        let address = self
            .get_new_address()
            .await
            .map_err(|err| err.builder().to_status())?;

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
        let count = std::num::NonZeroU32::new(blocks.unwrap_or(1)).ok_or_else(|| {
            tonic::Status::invalid_argument("must provide a positive number of blocks")
        })?;

        // Only allow one GenerateBlocks call at a time. Concurrent callers
        // will get a rate-limited error instead of queuing up.
        let permit = self
            .generate_blocks_semaphore()
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                tonic::Status::resource_exhausted("GenerateBlocks is already in progress")
            })?;

        self.verify_can_mine(count)
            .await
            .map_err(|err| err.builder().to_status())?;

        tracing::info!("generate blocks: verified ability to mine");

        let stream = crate::wallet::Wallet::generate_blocks(self.clone(), count, ack_all_proposals)
            .map(|stream_item| match stream_item {
                Ok(block_hash) => Ok(GenerateBlocksResponse {
                    block_hash: Some(ReverseHex::encode(&block_hash)),
                }),
                Err(err) => {
                    tracing::error!("{:#}", ErrorChain::new(&err));
                    Err(err.builder().to_status())
                }
            })
            // Hold the permit for the lifetime of the stream, so that
            // the semaphore is released when the stream completes or is
            // dropped.
            .map(move |item| {
                let _permit = &permit;
                item
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
            .await
            .map_err(|err| StatusBuilder::new(&err).to_status())?;
        /*
        self.broadcast_transaction(transaction.tx().into_owned())
            .await
            .map_err(|err| err.builder().to_status())?;
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
        tracing::trace!("create_bmm_critical_data_transaction: starting");
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
                ));
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

        tracing::trace!("create_bmm_critical_data_transaction: validated request");

        let mainchain_tip = self
            .validator()
            .get_mainchain_tip()
            .map_err(|err| tonic::Status::from_error(err.into()))?;

        tracing::debug!(
            "create_bmm_critical_data_transaction: fetched mainchain tip: {:?}",
            mainchain_tip
        );

        // If the mainchain tip has progressed beyond this, the request is already
        // expired.
        if mainchain_tip != convert::bdk_block_hash_to_bitcoin_block_hash(prev_bytes) {
            let message = format!("invalid prev_bytes {prev_bytes}: expected {mainchain_tip}",);

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
            .await
            .map_err(|err| err.builder().to_status())
            .and_then(|tx| {
                tx.ok_or_else(|| {
                    tonic::Status::new(
                        tonic::Code::AlreadyExists,
                        "BMM request with same `sidechain_number` and `prev_bytes` already exists",
                    )
                })
            })
            .inspect_err(|err| {
                tracing::error!(
                    "Error creating BMM critical data transaction: {:#}",
                    ErrorChain::new(err)
                );
            })?;

        let txid = tx.compute_txid();

        tracing::info!(
            "create_bmm_critical_data_transaction: created transaction: {:?}",
            txid
        );

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
        let address: String =
            address.ok_or_else(|| missing_field::<CreateDepositTransactionRequest>("address"))?;
        if address.is_empty() {
            return Err(invalid_field_value::<CreateDepositTransactionRequest, _>(
                "address",
                &address,
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
            .map_err(|err| err.builder().to_status())?
        {
            return Err(tonic::Status::new(
                tonic::Code::FailedPrecondition,
                format!("sidechain {sidechain_number} is not active"),
            ));
        }

        let txid = self
            .create_deposit(sidechain_number, address, value, Some(fee))
            .await
            .map_err(|err| err.builder().to_status())?;

        let txid = ReverseHex::encode(&txid);
        let response = CreateDepositTransactionResponse { txid: Some(txid) };
        Ok(tonic::Response::new(response))
    }

    async fn get_balance(
        &self,
        request: tonic::Request<GetBalanceRequest>,
    ) -> Result<tonic::Response<GetBalanceResponse>, tonic::Status> {
        tracing::trace!("get_balance: starting");
        let GetBalanceRequest {} = request.into_inner();

        tracing::trace!("get_balance: fetching from BDK wallet");

        let (balance, has_synced) = self
            .get_wallet_balance()
            .await
            .map_err(|err| err.builder().to_status())?;

        tracing::trace!("get_balance: fetched balance: {:?}", balance);

        let response = GetBalanceResponse {
            confirmed_sats: balance.confirmed.to_sat(),
            pending_sats: (balance.total() - balance.confirmed).to_sat(),
            has_synced,
        };

        Ok(tonic::Response::new(response))
    }

    async fn list_unspent_outputs(
        &self,
        request: tonic::Request<ListUnspentOutputsRequest>,
    ) -> Result<tonic::Response<ListUnspentOutputsResponse>, tonic::Status> {
        let ListUnspentOutputsRequest {} = request.into_inner();
        let bdk_utxos = self
            .get_utxos()
            .await
            .map_err(|err| err.builder().to_status())?;

        let outputs = bdk_utxos
            .into_iter()
            .map(|utxo| {
                let chain_position = match utxo.chain_position {
                    bdk_wallet::chain::ChainPosition::Unconfirmed { .. } => None,
                    bdk_wallet::chain::ChainPosition::Confirmed {
                        anchor,
                        transitively,
                    } => Some((anchor, transitively)),
                };

                let unconfirmed_last_seen = match utxo.chain_position {
                    bdk_wallet::chain::ChainPosition::Unconfirmed {
                        last_seen,
                        first_seen: _,
                    } => last_seen.map(|last_seen| prost_types::Timestamp {
                        seconds: last_seen as i64,
                        nanos: 0,
                    }),
                    bdk_wallet::chain::ChainPosition::Confirmed { .. } => None,
                };
                list_unspent_outputs_response::Output {
                    txid: Some(ReverseHex::encode(&utxo.outpoint.txid)),
                    vout: utxo.outpoint.vout,
                    value_sats: utxo.txout.value.to_sat(),
                    is_internal: utxo.keychain == bdk_wallet::KeychainKind::Internal,
                    is_confirmed: chain_position.is_some(),
                    confirmed_at_block: chain_position
                        .map(|(anchor, _)| anchor.block_id.height)
                        .unwrap_or_default(),
                    confirmed_at_time: chain_position.map(|(anchor, _)| prost_types::Timestamp {
                        seconds: anchor.confirmation_time as i64,
                        nanos: 0,
                    }),
                    confirmed_transitively: chain_position
                        .and_then(|(_, transitively)| transitively)
                        .map(|transitively| ReverseHex::encode(&transitively)),
                    unconfirmed_last_seen,
                    address: Address::from_script(
                        utxo.txout.script_pubkey.as_script(),
                        self.validator().network(),
                    )
                    .map(|addr| addr.to_string())
                    .ok(),
                }
            })
            .collect();

        Ok(tonic::Response::new(ListUnspentOutputsResponse { outputs }))
    }

    async fn list_sidechain_deposit_transactions(
        &self,
        request: tonic::Request<ListSidechainDepositTransactionsRequest>,
    ) -> Result<tonic::Response<ListSidechainDepositTransactionsResponse>, tonic::Status> {
        let ListSidechainDepositTransactionsRequest {} = request.into_inner();
        let transactions = self
            .list_sidechain_deposit_transactions()
            .await
            .map_err(|err| err.builder().to_status())?
            .into_iter()
            .map(|sidechain_deposit_tx| SidechainDepositTransaction {
                sidechain_number: Some(sidechain_deposit_tx.sidechain_number.0.into()),
                tx: Some(WalletTransaction::from(&sidechain_deposit_tx.wallet_tx)),
            })
            .collect();
        let response = ListSidechainDepositTransactionsResponse { transactions };
        Ok(tonic::Response::new(response))
    }

    async fn list_transactions(
        &self,
        request: tonic::Request<ListTransactionsRequest>,
    ) -> Result<tonic::Response<ListTransactionsResponse>, tonic::Status> {
        let ListTransactionsRequest {} = request.into_inner();
        let transactions = self
            .list_wallet_transactions()
            .await
            .map_err(|err| err.builder().to_status())?;

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
            required_utxos,
            drain_wallet_to,
        } = request.into_inner();

        let required_utxos = required_utxos
            .iter()
            .map(|utxo| {
                let txid = utxo
                    .txid
                    .as_ref()
                    .ok_or_else(|| missing_field::<RequiredUtxo>("txid"))?;

                let txid = txid.clone().decode_tonic::<RequiredUtxo, _>("txid")?;

                Ok(bdk_wallet::bitcoin::OutPoint {
                    txid,
                    vout: utxo.vout,
                })
            })
            .collect::<Result<Vec<_>, tonic::Status>>()?;

        // Parse and validate all destination addresses, but assume network valid
        let destinations_validated = destinations
            .iter()
            .map(|(address, amount)| {
                use bdk_wallet::IsDust;

                let address = self.parse_checked_address(address)?;

                let amount = Amount::from_sat(*amount);
                if amount.is_dust(&address.script_pubkey()) {
                    return Err(tonic::Status::invalid_argument(format!(
                        "amount is below dust limit: {amount} to {address}",
                    )));
                }

                Ok((address, amount))
            })
            .collect::<Result<HashMap<bdk_wallet::bitcoin::Address, Amount>, tonic::Status>>()?;

        if destinations_validated.is_empty()
            && op_return_message.is_none()
            && drain_wallet_to.is_none()
        {
            return Err(tonic::Status::invalid_argument(
                "no destinations or op_return_message provided",
            ));
        }

        let drain_wallet_to = drain_wallet_to
            .map(|drain_wallet_to| self.parse_checked_address(&drain_wallet_to))
            .transpose()?;

        if drain_wallet_to.is_some() && !required_utxos.is_empty() {
            return Err(tonic::Status::invalid_argument(
                "cannot provide both drain_wallet_to and required_utxos",
            ));
        }

        let fee_policy = fee_rate
            .map(|fee_rate| fee_rate.try_into())
            .transpose()
            .map_err(|err: crate::proto::Error| err.builder().to_status())?;

        let op_return_message = op_return_message
            .map(|op_return_message| {
                op_return_message.decode_tonic::<SendTransactionRequest, _>("op_return_message")
            })
            .transpose()?;

        let txid = self
            .send_wallet_transaction(
                destinations_validated,
                CreateTransactionParams {
                    fee_policy,
                    op_return_message,
                    required_utxos,
                    drain_wallet_to,
                },
            )
            .await
            .map_err(|err| err.builder().to_status())?;

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
            .await
            .map_err(|err| err.builder().to_status())?;

        Ok(tonic::Response::new(UnlockWalletResponse {}))
    }

    async fn create_wallet(
        &self,
        request: tonic::Request<CreateWalletRequest>,
    ) -> Result<tonic::Response<CreateWalletResponse>, tonic::Status> {
        // TODO: needs a way of creating /multiple/ wallets. RPC for unloading/erasing a wallet?
        if self.is_initialized().await {
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
                    format!("failed to read mnemonic from {mnemonic_path}: {err:#}"),
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
            .await
            .map_err(|err| err.builder().to_status())?;

        Ok(tonic::Response::new(CreateWalletResponse {}))
    }

    async fn get_bip47_payment_code(
        &self,
        request: tonic::Request<crate::proto::mainchain::GetBip47PaymentCodeRequest>,
    ) -> Result<tonic::Response<crate::proto::mainchain::GetBip47PaymentCodeResponse>, tonic::Status>
    {
        let req = request.into_inner();
        let version = bip47_version_from_proto(req.version)?;
        let code = self
            .bip47_payment_code(version)
            .await
            .map_err(|err| err.builder().to_status())?;
        let notification_address = bip47_notification_address(&code, self.validator().network())
            .map_err(|err| err.builder().to_status())?;
        Ok(tonic::Response::new(
            crate::proto::mainchain::GetBip47PaymentCodeResponse {
                payment_code: code.to_string(),
                notification_address,
                version: bip47_version_to_proto(version) as i32,
            },
        ))
    }

    async fn send_to_bip47_payment_code(
        &self,
        request: tonic::Request<crate::proto::mainchain::SendToBip47PaymentCodeRequest>,
    ) -> Result<
        tonic::Response<crate::proto::mainchain::SendToBip47PaymentCodeResponse>,
        tonic::Status,
    > {
        let req = request.into_inner();
        let recipient: crate::wallet::reusable_payments::PaymentCode = req
            .payment_code
            .parse()
            .map_err(|err: crate::wallet::reusable_payments::bip47::ParseError| {
                tonic::Status::invalid_argument(format!("invalid payment_code: {err}"))
            })?;
        let amount = bitcoin::Amount::from_sat(req.amount_sats);
        let result = self
            .send_to_payment_code(recipient, amount, req.fee_sat_per_vbyte)
            .await
            .map_err(|err| err.builder().to_status())?;
        Ok(tonic::Response::new(
            crate::proto::mainchain::SendToBip47PaymentCodeResponse {
                notification_txid: result.notification_txid.map(|t| ReverseHex::encode(&t)),
                payment_txid: Some(ReverseHex::encode(&result.payment_txid)),
                sender_index: result.sender_index,
                version: bip47_version_to_proto(result.version) as i32,
            },
        ))
    }

    async fn list_bip47_inbound_payers(
        &self,
        _request: tonic::Request<crate::proto::mainchain::ListBip47InboundPayersRequest>,
    ) -> Result<
        tonic::Response<crate::proto::mainchain::ListBip47InboundPayersResponse>,
        tonic::Status,
    > {
        let payers = self
            .list_bip47_inbound_payers()
            .await
            .map_err(|err| err.builder().to_status())?;
        let proto_payers = payers
            .into_iter()
            .map(|p| crate::proto::mainchain::Bip47InboundPayer {
                payment_code: p.sender_payment_code.to_string(),
                version: bip47_version_to_proto(p.sender_payment_code.version()) as i32,
                next_receive_index: p.next_receive_index,
                total_received_sats: p.total_received_sats,
                first_seen: Some(prost_types::Timestamp {
                    seconds: p.first_seen_unix,
                    nanos: 0,
                }),
            })
            .collect();
        Ok(tonic::Response::new(
            crate::proto::mainchain::ListBip47InboundPayersResponse {
                payers: proto_payers,
            },
        ))
    }

    async fn get_reusable_scan_status(
        &self,
        _request: tonic::Request<crate::proto::mainchain::GetReusableScanStatusRequest>,
    ) -> Result<
        tonic::Response<crate::proto::mainchain::GetReusableScanStatusResponse>,
        tonic::Status,
    > {
        let status = self
            .reusable_scan_status_snapshot()
            .await
            .map_err(|err| err.builder().to_status())?;
        Ok(tonic::Response::new(
            crate::proto::mainchain::GetReusableScanStatusResponse {
                tip_height: status.tip_height,
                last_scanned_height: status.last_scanned_height,
                birthday_height: status.birthday_height,
                catching_up: status.catching_up,
            },
        ))
    }

    async fn rescan_reusable_payments(
        &self,
        request: tonic::Request<crate::proto::mainchain::RescanReusablePaymentsRequest>,
    ) -> Result<
        tonic::Response<crate::proto::mainchain::RescanReusablePaymentsResponse>,
        tonic::Status,
    > {
        let req = request.into_inner();
        self.rescan_reusable_payments(req.from_height)
            .await
            .map_err(|err| err.builder().to_status())?;
        Ok(tonic::Response::new(
            crate::proto::mainchain::RescanReusablePaymentsResponse {
                scheduled_from_height: req.from_height,
            },
        ))
    }

    async fn get_silent_payment_address(
        &self,
        request: tonic::Request<crate::proto::mainchain::GetSilentPaymentAddressRequest>,
    ) -> Result<
        tonic::Response<crate::proto::mainchain::GetSilentPaymentAddressResponse>,
        tonic::Status,
    > {
        let req = request.into_inner();
        let addr = self
            .silent_payment_address(req.label)
            .await
            .map_err(|err| err.builder().to_status())?;
        Ok(tonic::Response::new(
            crate::proto::mainchain::GetSilentPaymentAddressResponse {
                address: addr.to_string(),
            },
        ))
    }

    async fn create_silent_payment_label(
        &self,
        request: tonic::Request<crate::proto::mainchain::CreateSilentPaymentLabelRequest>,
    ) -> Result<
        tonic::Response<crate::proto::mainchain::CreateSilentPaymentLabelResponse>,
        tonic::Status,
    > {
        let req = request.into_inner();
        let name = req.name.trim();
        if name.is_empty() {
            return Err(tonic::Status::invalid_argument("name must be non-empty"));
        }
        let m = self
            .create_silent_payment_label(name)
            .await
            .map_err(|err| err.builder().to_status())?;
        let labeled = self
            .silent_payment_address(Some(m))
            .await
            .map_err(|err| err.builder().to_status())?;
        Ok(tonic::Response::new(
            crate::proto::mainchain::CreateSilentPaymentLabelResponse {
                label_m: m,
                labeled_address: labeled.to_string(),
            },
        ))
    }

    async fn list_silent_payment_labels(
        &self,
        _request: tonic::Request<crate::proto::mainchain::ListSilentPaymentLabelsRequest>,
    ) -> Result<
        tonic::Response<crate::proto::mainchain::ListSilentPaymentLabelsResponse>,
        tonic::Status,
    > {
        let raw = self
            .list_silent_payment_labels()
            .await
            .map_err(|err| err.builder().to_status())?;
        let mut labels = Vec::with_capacity(raw.len());
        for (m, name) in raw {
            let labeled = self
                .silent_payment_address(Some(m))
                .await
                .map_err(|err| err.builder().to_status())?;
            labels.push(crate::proto::mainchain::SilentPaymentLabel {
                m,
                name,
                address: labeled.to_string(),
            });
        }
        Ok(tonic::Response::new(
            crate::proto::mainchain::ListSilentPaymentLabelsResponse { labels },
        ))
    }

    async fn send_to_silent_payment(
        &self,
        request: tonic::Request<crate::proto::mainchain::SendToSilentPaymentRequest>,
    ) -> Result<tonic::Response<crate::proto::mainchain::SendToSilentPaymentResponse>, tonic::Status>
    {
        let req = request.into_inner();
        let expected_network = self.validator().network();
        let mut parsed_recipients = Vec::with_capacity(req.recipients.len());
        for r in req.recipients {
            let addr = crate::wallet::reusable_payments::SilentPaymentAddress::parse_for_network(
                &r.sp_address,
                expected_network,
            )
            .map_err(
                |err: crate::wallet::reusable_payments::silent_payments::ParseError| {
                    tonic::Status::invalid_argument(format!("invalid sp_address: {err}"))
                },
            )?;
            parsed_recipients.push((addr, bitcoin::Amount::from_sat(r.amount_sats)));
        }
        let txid = self
            .send_to_silent_payment(parsed_recipients, req.fee_sat_per_vbyte)
            .await
            .map_err(|err| err.builder().to_status())?;
        Ok(tonic::Response::new(
            crate::proto::mainchain::SendToSilentPaymentResponse {
                txid: Some(ReverseHex::encode(&txid)),
            },
        ))
    }

    async fn list_silent_payment_receives(
        &self,
        request: tonic::Request<crate::proto::mainchain::ListSilentPaymentReceivesRequest>,
    ) -> Result<
        tonic::Response<crate::proto::mainchain::ListSilentPaymentReceivesResponse>,
        tonic::Status,
    > {
        let req = request.into_inner();
        let (raw, tip_height) = self
            .list_silent_payment_receives(req.min_confirmations, req.limit)
            .await
            .map_err(|err| err.builder().to_status())?;
        let items = raw
            .into_iter()
            .map(|r| crate::proto::mainchain::SilentPaymentReceive {
                txid: Some(ReverseHex::encode(&r.txid)),
                vout: r.vout,
                output_pubkey: r.output_pubkey.serialize().to_vec(),
                amount_sats: r.amount.to_sat(),
                tweak_k: r.tweak_k,
                label_m: r.label_m,
                label_name: r.label_name,
                height: r.height,
                spent_in_txid: r.spent_in_txid.map(|t| ReverseHex::encode(&t)),
            })
            .collect();
        Ok(tonic::Response::new(
            crate::proto::mainchain::ListSilentPaymentReceivesResponse {
                items,
                scan_tip_height: tip_height,
            },
        ))
    }
}

fn bip47_version_from_proto(
    raw: i32,
) -> Result<crate::wallet::reusable_payments::bip47::Version, tonic::Status> {
    use crate::{proto::mainchain::Bip47Version as P, wallet::reusable_payments::bip47::Version};
    let parsed = P::try_from(raw)
        .map_err(|_| tonic::Status::invalid_argument(format!("unknown Bip47Version {raw}")))?;
    match parsed {
        P::V1 => Ok(Version::V1),
        P::V3 => Ok(Version::V3),
        P::Unspecified => Err(tonic::Status::invalid_argument(
            "Bip47Version must be V1 or V3",
        )),
    }
}

fn bip47_version_to_proto(
    v: crate::wallet::reusable_payments::bip47::Version,
) -> crate::proto::mainchain::Bip47Version {
    use crate::{proto::mainchain::Bip47Version as P, wallet::reusable_payments::bip47::Version};
    match v {
        Version::V1 => P::V1,
        Version::V3 => P::V3,
    }
}

fn bip47_notification_address(
    code: &crate::wallet::reusable_payments::PaymentCode,
    network: bitcoin::Network,
) -> Result<String, crate::wallet::error::ReusablePayments> {
    let secp = bitcoin::secp256k1::Secp256k1::new();
    let addr = crate::wallet::reusable_payments::bip47::notification_address(code, network, &secp)?;
    Ok(addr.to_string())
}

#[derive(Debug, Error)]
enum Error {
    #[error("value must be greater than zero")]
    ValueMustBeGreaterThanZero,

    #[error("address must be non-empty")]
    AddressMustBeNonEmpty,
}
