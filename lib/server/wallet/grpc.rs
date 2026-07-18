use std::{collections::HashMap, str::FromStr};

use bdk_wallet::bip39::Mnemonic;
use bitcoin::{Address, Amount, hashes::Hash as _};
use buffa::MessageField;
use connectrpc::{
    ConnectError, RequestContext, Response, ServiceRequest, ServiceResult, ServiceStream,
};
use futures::{StreamExt as _, stream::BoxStream};

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
            CreateNewAddressRequest, CreateNewAddressResponse, CreateWalletRequest,
            CreateWalletResponse, GenerateBlocksRequest, GenerateBlocksResponse, GetBalanceRequest,
            GetBalanceResponse, GetInfoRequest, GetInfoResponse,
            ListSidechainDepositTransactionsRequest, ListSidechainDepositTransactionsResponse,
            ListTransactionsRequest, ListTransactionsResponse, ListUnspentOutputsRequest,
            ListUnspentOutputsResponse, SendTransactionRequest, SendTransactionResponse,
            UnlockWalletRequest, UnlockWalletResponse, WalletTransaction, get_info_response,
            list_sidechain_deposit_transactions_response::SidechainDepositTransaction,
            list_unspent_outputs_response, send_transaction_request::RequiredUtxo,
        },
        mainchain_service::WalletService,
        unwrap_string, unwrap_u32, unwrap_u64, wrap_timestamp, wrap_u32,
    },
    server::{internal_err, invalid_field_value, missing_field, parse_sidechain_id},
    types::BlindedM6,
    wallet::{CreateTransactionParams, error::WalletInitialization},
};

#[expect(refining_impl_trait_reachable)]
impl WalletService for crate::wallet::Wallet {
    async fn get_info(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, GetInfoRequest>,
    ) -> ServiceResult<GetInfoResponse> {
        let info = self
            .get_wallet_info()
            .await
            .map_err(|err| err.builder().to_connect_error())?;
        Ok(Response::new(GetInfoResponse {
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
            tip: MessageField::some(get_info_response::Tip {
                height: info.tip.1,
                hash: MessageField::some(ReverseHex::encode(&info.tip.0)),
            }),
        }))
    }

    async fn create_new_address(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, CreateNewAddressRequest>,
    ) -> ServiceResult<CreateNewAddressResponse> {
        let address = self
            .get_new_address()
            .await
            .map_err(|err| err.builder().to_connect_error())?;
        Ok(Response::new(CreateNewAddressResponse {
            address: address.to_string(),
        }))
    }

    async fn generate_blocks(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, GenerateBlocksRequest>,
    ) -> ServiceResult<ServiceStream<GenerateBlocksResponse>> {
        use crate::proto::mainchain::GenerateBlocksRequest;
        let GenerateBlocksRequest {
            blocks,
            ack_all_proposals,
            ..
        } = request.to_owned_message();
        let count =
            std::num::NonZeroU32::new(unwrap_u32(blocks).unwrap_or(1)).ok_or_else(|| {
                ConnectError::invalid_argument("must provide a positive number of blocks")
            })?;

        // Only allow one GenerateBlocks call at a time. Concurrent callers
        // will get a rate-limited error instead of queuing up.
        let permit = self
            .generate_blocks_semaphore()
            .clone()
            .try_acquire_owned()
            .map_err(|_| {
                ConnectError::resource_exhausted("GenerateBlocks is already in progress")
            })?;

        self.verify_can_mine(count)
            .await
            .map_err(|err| err.builder().to_connect_error())?;

        tracing::info!("generate blocks: verified ability to mine");

        let stream: BoxStream<'static, _> =
            crate::wallet::Wallet::generate_blocks(self.clone(), count, ack_all_proposals)
                .map(|item| match item {
                    Ok(block_hash) => Ok(GenerateBlocksResponse {
                        block_hash: MessageField::some(ReverseHex::encode(&block_hash)),
                    }),
                    Err(err) => {
                        tracing::error!("{:#}", ErrorChain::new(&err));
                        Err(err.builder().to_connect_error())
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
        Ok(Response::new(Box::pin(stream)))
    }

    async fn broadcast_withdrawal_bundle(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, BroadcastWithdrawalBundleRequest>,
    ) -> ServiceResult<BroadcastWithdrawalBundleResponse> {
        use crate::proto::mainchain::BroadcastWithdrawalBundleRequest;
        let BroadcastWithdrawalBundleRequest {
            sidechain_id,
            transaction,
            ..
        } = request.to_owned_message();
        let sidechain_id =
            parse_sidechain_id::<BroadcastWithdrawalBundleRequest>(sidechain_id, "sidechain_id")?;
        // Reject bundles for sidechains that are not active: an inactive slot has no
        // treasury to withdraw from and, per BIP300 M3, a bundle proposed for it would
        // be a no-op. Fail fast at ingestion rather than persisting a row that can
        // never be acted on. NB: this gate cannot catch a slot that is deactivated by
        // a reorg *after* a bundle is stored, so the block builder
        // (`get_bundle_proposals`) also skips inactive-slot bundles.
        match self.is_sidechain_active(sidechain_id) {
            Ok(false) => {
                return Err(ConnectError::failed_precondition(format!(
                    "cannot accept a withdrawal bundle for sidechain {sidechain_id}: not active"
                )));
            }
            Ok(true) => (),
            Err(err) => return Err(internal_err(err)),
        }
        let transaction_bytes: Vec<u8> = transaction
            .into_option()
            .ok_or_else(|| missing_field::<BroadcastWithdrawalBundleRequest>("transaction"))?
            .value;
        // A blinded M6 is a zero-input tx that Core/sidechains serialize in legacy
        // form, which rust-bitcoin's standard decoder cannot parse;
        // `BlindedM6::deserialize` handles that fallback and validates the bundle.
        let transaction = BlindedM6::deserialize(&transaction_bytes).map_err(|err| {
            invalid_field_value::<BroadcastWithdrawalBundleRequest, _>(
                "transaction",
                &hex::encode(&transaction_bytes),
                err,
            )
        })?;
        let _m6id = self
            .put_withdrawal_bundle(sidechain_id, &transaction)
            .await
            .map_err(|err| StatusBuilder::new(&err).to_connect_error())?;
        Ok(Response::new(BroadcastWithdrawalBundleResponse::default()))
    }

    // Legacy Bitcoin Core-based implementation
    // https://github.com/LayerTwo-Labs/mainchain/blob/05e71917042132202248c0c917f8ef120a2a5251/src/wallet/rpcwallet.cpp#L3863-L4008
    async fn create_bmm_critical_data_transaction(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, CreateBmmCriticalDataTransactionRequest>,
    ) -> ServiceResult<CreateBmmCriticalDataTransactionResponse> {
        use crate::proto::mainchain::CreateBmmCriticalDataTransactionRequest;
        let CreateBmmCriticalDataTransactionRequest {
            sidechain_id,
            value_sats,
            height,
            critical_hash,
            prev_bytes,
            ..
        } = request.to_owned_message();
        let value_sats_raw = unwrap_u64(value_sats).ok_or_else(|| {
            missing_field::<CreateBmmCriticalDataTransactionRequest>("value_sats")
        })?;
        let amount = bdk_wallet::bitcoin::Amount::from_sat(value_sats_raw);
        let height_raw = unwrap_u32(height)
            .ok_or_else(|| missing_field::<CreateBmmCriticalDataTransactionRequest>("height"))?;
        let locktime =
            bdk_wallet::bitcoin::absolute::LockTime::from_height(height_raw).map_err(|err| {
                invalid_field_value::<CreateBmmCriticalDataTransactionRequest, _>(
                    "height",
                    &height_raw.to_string(),
                    err,
                )
            })?;
        let sidechain_number = parse_sidechain_id::<CreateBmmCriticalDataTransactionRequest>(
            sidechain_id,
            "sidechain_id",
        )?;

        match self.is_sidechain_active(sidechain_number) {
            Ok(false) => return Err(ConnectError::failed_precondition("sidechain is not active")),
            Ok(true) => (),
            Err(err) => return Err(internal_err(err)),
        }

        // This is also called H*
        let critical_hash = critical_hash
            .into_option()
            .ok_or_else(|| {
                missing_field::<CreateBmmCriticalDataTransactionRequest>("critical_hash")
            })?
            .decode_status::<CreateBmmCriticalDataTransactionRequest, _>("critical_hash")?;
        let prev_bytes = prev_bytes
            .into_option()
            .ok_or_else(|| missing_field::<CreateBmmCriticalDataTransactionRequest>("prev_bytes"))?
            .decode_status::<CreateBmmCriticalDataTransactionRequest, _>("prev_bytes")
            .map(bdk_wallet::bitcoin::BlockHash::from_byte_array)?;

        tracing::trace!("create_bmm_critical_data_transaction: validated request");

        let mainchain_tip = self.validator().get_mainchain_tip().map_err(internal_err)?;

        tracing::debug!(
            "create_bmm_critical_data_transaction: fetched mainchain tip: {:?}",
            mainchain_tip
        );

        // If the mainchain tip has progressed beyond this, the request is already
        // expired.
        if mainchain_tip != convert::bdk_block_hash_to_bitcoin_block_hash(prev_bytes) {
            return Err(ConnectError::invalid_argument(format!(
                "invalid prev_bytes {prev_bytes}: expected {mainchain_tip}"
            )));
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
            .map_err(|err| err.builder().to_connect_error())
            .and_then(|tx| {
                tx.ok_or_else(|| {
                    ConnectError::already_exists(
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

        let txid = convert::bdk_txid_to_bitcoin_txid(tx.compute_txid());

        tracing::info!(
            "create_bmm_critical_data_transaction: created transaction: {:?}",
            txid
        );

        Ok(Response::new(CreateBmmCriticalDataTransactionResponse {
            txid: MessageField::some(ReverseHex::encode(&txid)),
        }))
    }

    async fn create_deposit_transaction(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, CreateDepositTransactionRequest>,
    ) -> ServiceResult<CreateDepositTransactionResponse> {
        use crate::proto::mainchain::CreateDepositTransactionRequest;
        let CreateDepositTransactionRequest {
            sidechain_id,
            address,
            value_sats,
            fee_sats,
            ..
        } = request.to_owned_message();
        let sidechain_number =
            parse_sidechain_id::<CreateDepositTransactionRequest>(sidechain_id, "sidechain_id")?;
        let address: String = unwrap_string(address)
            .ok_or_else(|| missing_field::<CreateDepositTransactionRequest>("address"))?;
        if address.is_empty() {
            return Err(ConnectError::invalid_argument("address must be non-empty"));
        }
        let value = Amount::from_sat(
            unwrap_u64(value_sats)
                .ok_or_else(|| missing_field::<CreateDepositTransactionRequest>("value_sats"))?,
        );
        if value == Amount::ZERO {
            return Err(ConnectError::invalid_argument(
                "value_sats must be greater than zero",
            ));
        }
        let fee = Amount::from_sat(
            unwrap_u64(fee_sats)
                .ok_or_else(|| missing_field::<CreateDepositTransactionRequest>("fee_sats"))?,
        );
        if !self
            .is_sidechain_active(sidechain_number)
            .map_err(|err| err.builder().to_connect_error())?
        {
            return Err(ConnectError::failed_precondition(format!(
                "sidechain {sidechain_number} is not active"
            )));
        }
        let txid = self
            .create_deposit(sidechain_number, address, value, Some(fee))
            .await
            .map_err(|err| err.builder().to_connect_error())?;

        Ok(Response::new(CreateDepositTransactionResponse {
            txid: MessageField::some(ReverseHex::encode(&txid)),
        }))
    }

    async fn get_balance(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, GetBalanceRequest>,
    ) -> ServiceResult<GetBalanceResponse> {
        let (balance, has_synced) = self
            .get_wallet_balance()
            .await
            .map_err(|err| err.builder().to_connect_error())?;
        Ok(Response::new(GetBalanceResponse {
            confirmed_sats: balance.confirmed.to_sat(),
            pending_sats: (balance.total() - balance.confirmed).to_sat(),
            has_synced,
        }))
    }

    async fn list_unspent_outputs(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, ListUnspentOutputsRequest>,
    ) -> ServiceResult<ListUnspentOutputsResponse> {
        let bdk_utxos = self
            .get_utxos()
            .await
            .map_err(|err| err.builder().to_connect_error())?;
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
                    } => last_seen
                        .map(|s| wrap_timestamp(s as i64))
                        .unwrap_or_default(),
                    bdk_wallet::chain::ChainPosition::Confirmed { .. } => MessageField::none(),
                };
                list_unspent_outputs_response::Output {
                    txid: MessageField::some(ReverseHex::encode(&utxo.outpoint.txid)),
                    vout: utxo.outpoint.vout,
                    value_sats: utxo.txout.value.to_sat(),
                    is_internal: utxo.keychain == bdk_wallet::KeychainKind::Internal,
                    is_confirmed: chain_position.is_some(),
                    confirmed_at_block: chain_position
                        .map(|(anchor, _)| anchor.block_id.height)
                        .unwrap_or_default(),
                    confirmed_at_time: chain_position
                        .map(|(anchor, _)| wrap_timestamp(anchor.confirmation_time as i64))
                        .unwrap_or_default(),
                    confirmed_transitively: chain_position
                        .and_then(|(_, t)| t)
                        .map(|t| MessageField::some(ReverseHex::encode(&t)))
                        .unwrap_or_default(),
                    unconfirmed_last_seen,
                    address: Address::from_script(
                        utxo.txout.script_pubkey.as_script(),
                        self.validator().network(),
                    )
                    .map(|addr| crate::proto::wrap_string(addr.to_string()))
                    .unwrap_or_default(),
                }
            })
            .collect();

        Ok(Response::new(ListUnspentOutputsResponse { outputs }))
    }

    async fn list_sidechain_deposit_transactions(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, ListSidechainDepositTransactionsRequest>,
    ) -> ServiceResult<ListSidechainDepositTransactionsResponse> {
        let transactions = self
            .list_sidechain_deposit_transactions()
            .await
            .map_err(|err| err.builder().to_connect_error())?
            .into_iter()
            .map(|sdt| SidechainDepositTransaction {
                sidechain_number: wrap_u32(sdt.sidechain_number.0 as u32),
                tx: MessageField::some(WalletTransaction::from(&sdt.wallet_tx)),
            })
            .collect();
        Ok(Response::new(ListSidechainDepositTransactionsResponse {
            transactions,
        }))
    }

    async fn list_transactions(
        &self,
        _ctx: RequestContext,
        _request: ServiceRequest<'_, ListTransactionsRequest>,
    ) -> ServiceResult<ListTransactionsResponse> {
        let transactions = self
            .list_wallet_transactions()
            .await
            .map_err(|err| err.builder().to_connect_error())?;
        Ok(Response::new(ListTransactionsResponse {
            transactions: transactions.iter().map(WalletTransaction::from).collect(),
        }))
    }

    async fn send_transaction(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, SendTransactionRequest>,
    ) -> ServiceResult<SendTransactionResponse> {
        use crate::proto::mainchain::SendTransactionRequest;
        let SendTransactionRequest {
            destinations,
            fee_rate,
            op_return_message,
            required_utxos,
            drain_wallet_to,
            ..
        } = request.to_owned_message();

        let required_utxos = required_utxos
            .into_iter()
            .map(|utxo| {
                let txid = utxo
                    .txid
                    .into_option()
                    .ok_or_else(|| missing_field::<RequiredUtxo>("txid"))?
                    .decode_status::<RequiredUtxo, _>("txid")?;
                Ok(bdk_wallet::bitcoin::OutPoint {
                    txid,
                    vout: utxo.vout,
                })
            })
            .collect::<Result<Vec<_>, ConnectError>>()?;

        // Parse and validate all destination addresses, but assume network valid
        let destinations_validated = destinations
            .iter()
            .map(|(address, amount)| {
                use bdk_wallet::IsDust;

                let address = self.parse_checked_address(address)?;

                let amount = Amount::from_sat(*amount);
                if amount.is_dust(&address.script_pubkey()) {
                    return Err(ConnectError::invalid_argument(format!(
                        "amount is below dust limit: {amount} to {address}"
                    )));
                }

                Ok((address, amount))
            })
            .collect::<Result<HashMap<bdk_wallet::bitcoin::Address, Amount>, ConnectError>>()?;

        if destinations_validated.is_empty()
            && !op_return_message.is_set()
            && drain_wallet_to.is_none()
        {
            return Err(ConnectError::invalid_argument(
                "no destinations or op_return_message provided",
            ));
        }

        let drain_wallet_to = drain_wallet_to
            .map(|s| self.parse_checked_address(&s))
            .transpose()?;

        if drain_wallet_to.is_some() && !required_utxos.is_empty() {
            return Err(ConnectError::invalid_argument(
                "cannot provide both drain_wallet_to and required_utxos",
            ));
        }

        let fee_policy = fee_rate
            .into_option()
            .map(|fee_rate| fee_rate.try_into())
            .transpose()?;

        let op_return_message = op_return_message
            .into_option()
            .map(|m| m.decode_status::<SendTransactionRequest, _>("op_return_message"))
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
            .map_err(|err| err.builder().to_connect_error())?;
        Ok(Response::new(SendTransactionResponse {
            txid: MessageField::some(ReverseHex::encode(&txid)),
        }))
    }

    async fn unlock_wallet(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, UnlockWalletRequest>,
    ) -> ServiceResult<UnlockWalletResponse> {
        use crate::proto::mainchain::UnlockWalletRequest;
        let UnlockWalletRequest { password, .. } = request.to_owned_message();
        self.unlock_existing_wallet(password.as_str())
            .await
            .map_err(|err| err.builder().to_connect_error())?;
        Ok(Response::new(UnlockWalletResponse::default()))
    }

    async fn create_wallet(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, CreateWalletRequest>,
    ) -> ServiceResult<CreateWalletResponse> {
        // TODO: needs a way of creating /multiple/ wallets. RPC for unloading/erasing a wallet?
        if self.is_initialized().await {
            let err = WalletInitialization::AlreadyExists;
            return Err(ConnectError::already_exists(format!("{err:#}")));
        }
        use crate::proto::mainchain::CreateWalletRequest;
        let CreateWalletRequest {
            mut mnemonic_words,
            mnemonic_path,
            password,
        } = request.to_owned_message();
        if !mnemonic_words.is_empty() && !mnemonic_path.is_empty() {
            return Err(ConnectError::invalid_argument(
                "cannot provide both mnemonic and mnemonic path",
            ));
        }
        if !mnemonic_path.is_empty() {
            let read = std::fs::read_to_string(&mnemonic_path).map_err(|err| {
                ConnectError::invalid_argument(format!(
                    "failed to read mnemonic from {mnemonic_path}: {err:#}"
                ))
            })?;
            mnemonic_words = read.split_whitespace().map(|s| s.to_string()).collect();
        }
        if !mnemonic_words.is_empty() && mnemonic_words.len() != 12 {
            return Err(ConnectError::invalid_argument("mnemonic must be 12 words"));
        }

        let parsed = if mnemonic_words.is_empty() {
            None
        } else {
            Some(
                Mnemonic::from_str(&mnemonic_words.join(" ")).map_err(|err| {
                    ConnectError::invalid_argument(format!("failed to parse mnemonic: {err:#}"))
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
            .map_err(|err| err.builder().to_connect_error())?;

        Ok(Response::new(CreateWalletResponse::default()))
    }
}
