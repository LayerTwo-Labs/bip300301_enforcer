//! `MiningService`: on-demand block generation for regtest and signet.

use connectrpc::{ConnectError, RequestContext, Response, ServiceRequest, ServiceResult};

use crate::{
    block_producer::BlockProducer,
    errors::ErrorChain,
    proto::{
        ToStatus,
        common::ReverseHex,
        mainchain::{GenerateToAddressRequest, GenerateToAddressResponse},
        mainchain_service::MiningService,
        unwrap_u32,
    },
    server::{internal_err, invalid_field_value, missing_field},
};

#[expect(refining_impl_trait_reachable)]
impl MiningService for BlockProducer {
    async fn generate_to_address(
        &self,
        _ctx: RequestContext,
        request: ServiceRequest<'_, GenerateToAddressRequest>,
    ) -> ServiceResult<GenerateToAddressResponse> {
        let GenerateToAddressRequest {
            blocks, address, ..
        } = request.to_owned_message();
        let count =
            std::num::NonZeroU32::new(unwrap_u32(blocks).unwrap_or(1)).ok_or_else(|| {
                ConnectError::invalid_argument("must provide a positive number of blocks")
            })?;
        if address.is_empty() {
            return Err(missing_field::<GenerateToAddressRequest>("address"));
        }
        let coinbase_addr = address
            .parse::<bitcoin::Address<bitcoin::address::NetworkUnchecked>>()
            .map_err(|err| {
                invalid_field_value::<GenerateToAddressRequest, _>("address", &address, err)
            })?
            .require_network(self.validator().network())
            .map_err(|err| {
                invalid_field_value::<GenerateToAddressRequest, _>("address", &address, err)
            })?;

        // Only allow one mining call at a time. Concurrent callers get a
        // rate-limited error instead of queuing up.
        let _permit = self
            .generate_blocks_semaphore()
            .try_acquire()
            .map_err(|_| {
                ConnectError::resource_exhausted("block generation is already in progress")
            })?;

        self.verify_can_mine(count)
            .await
            .map_err(|err| err.builder().to_connect_error())?;

        // The ACK policy is the persisted one, which also governs served block
        // templates.
        let ack_policy = self.db().get_ack_policy().await.map_err(internal_err)?;
        let bundle_policy = self.db().get_bundle_policy().await.map_err(internal_err)?;

        let mut block_hashes = Vec::with_capacity(count.get() as usize);
        for _ in 0..count.get() {
            let block_hash = self
                .generate_block(coinbase_addr.clone(), ack_policy, bundle_policy)
                .await
                .map_err(|err| {
                    tracing::error!("{:#}", ErrorChain::new(&err));
                    err.builder().to_connect_error()
                })?;
            block_hashes.push(ReverseHex::encode(&block_hash));
        }
        Ok(Response::new(GenerateToAddressResponse { block_hashes }))
    }
}
