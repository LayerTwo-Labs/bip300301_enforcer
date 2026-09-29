//! Mining utilities

use std::sync::Arc;

use bip300301_enforcer_lib::{
    bins::{CommandError, CommandExt},
    proto::{
        self, ToStatus,
        mainchain::{
            AckAllProposalsPolicy, BlockHeaderInfo, GenerateToAddressRequest,
            GenerateToAddressResponse, SetAckAllProposalsRequest, SetWithdrawalBundlePolicyRequest,
            SubscribeEventsRequest, SubscribeEventsResponse, WithdrawalBundlePolicy,
            subscribe_events_response, subscribe_events_response::event::ConnectBlock,
        },
    },
};
use bitcoin::{Transaction, TxOut};
use connectrpc::ConnectError;
use either::Either;
use jsonrpsee::{core::client::ClientT as _, rpc_params};
use thiserror::Error;
use tokio::time::{Instant, sleep, sleep_until, timeout};

use crate::{
    block_verdict::chaintip_status,
    setup::{
        MiningMode, Network, PostSetup, Sidechain, WAIT_POLL_INTERVAL_SLOW, WAIT_TIMEOUT,
        wait_until,
    },
    signet_miner::TemplateSource,
    util::VarError,
};

/// Block until the enforcer's block template carries `txid`.
///
/// A transaction is in bitcoind's mempool the moment `sendrawtransaction`
/// returns, but the enforcer builds templates from its own mempool mirror,
/// which is fed asynchronously off bitcoind's ZMQ sequence stream. Mining as
/// soon as a send RPC returns therefore races that stream: the template is
/// built from a mirror that has not caught up, the block goes out without the
/// transaction, and whatever the test expected it to pay for never confirms.
/// The lag is milliseconds, which is exactly what makes it an intermittent
/// failure rather than an obvious one.
///
/// Only meaningful in [`crate::setup::Mode::GetBlockTemplate`], which is the
/// mode that serves the enforcer's `getblocktemplate`.
pub async fn wait_for_tx_in_block_template(
    post_setup: &PostSetup,
    txid: &bitcoin::Txid,
) -> anyhow::Result<()> {
    use cusf_enforcer_mempool::server::RpcClient as _;
    wait_until(
        &format!("tx `{txid}` to reach the enforcer's block template"),
        || async {
            let mut request = bitcoin_jsonrpsee::client::BlockTemplateRequest::default();
            request.capabilities.insert("coinbasetxn".to_owned());
            let template = crate::util::expect_block_template(
                post_setup.gbt_client.get_block_template(request).await?,
            )?;
            Ok(template.transactions.iter().any(|tx| tx.txid == *txid))
        },
    )
    .await
}

#[derive(Debug, Error)]
pub enum MineGbtError {
    #[error("Unexpected block disconnect")]
    BlockDisconnect,
    #[error(
        "Timed out after {WAIT_TIMEOUT:?} waiting for the validator to connect block `{block_hash}`"
    )]
    BlockEventTimeout { block_hash: bitcoin::BlockHash },
    #[error("The enforcer rejected block `{block_hash}`, see its log for the reason")]
    BlockRejected { block_hash: bitcoin::BlockHash },
    #[error(transparent)]
    Command(#[from] CommandError),
    #[error(transparent)]
    ConsensusDecode(#[from] bitcoin::consensus::encode::Error),
    #[error(transparent)]
    ConsensusDecodeHex(#[from] bitcoin::consensus::encode::FromHexError),
    /// From the enforcer's block template server or bitcoind.
    #[error(transparent)]
    JsonRpc(#[from] jsonrpsee::core::ClientError),
    #[error("Missing coinbasetxn in block template")]
    MissingCoinbaseTxn,
    #[error("`getblocktemplate` answered a BIP23 proposal verdict, not a template")]
    UnexpectedProposalVerdict,
    #[error("Expected block event")]
    NoBlockEvent,
    #[error("Submitting block failed with error: `{err_msg}`")]
    SubmitBlock { err_msg: String },
    #[error(transparent)]
    ValidatorClient(#[from] ConnectError),
    #[error(transparent)]
    Var(#[from] Arc<VarError>),
}

async fn mine_gbt(
    post_setup: &mut PostSetup,
    extra_coinbase_outputs: &[TxOut],
) -> Result<bitcoin::BlockHash, MineGbtError> {
    use cusf_enforcer_mempool::server::RpcClient;
    let mut gbt_request = bitcoin_jsonrpsee::client::BlockTemplateRequest::default();
    gbt_request.capabilities.insert("coinbasetxn".to_owned());
    tracing::debug!("Requesting block template");
    let block_template = post_setup
        .gbt_client
        .get_block_template(gbt_request)
        .await?
        .into_template()
        .ok_or(MineGbtError::UnexpectedProposalVerdict)?;
    let bitcoin_jsonrpsee::client::CoinbaseTxnOrValue::Txn(coinbase_tx) =
        block_template.coinbase_txn_or_value
    else {
        return Err(MineGbtError::MissingCoinbaseTxn);
    };
    let mut coinbase_tx: Transaction = bitcoin::consensus::deserialize(&coinbase_tx.data)?;
    // The coinbase wtxid is all-zero by definition (BIP141), so the witness
    // commitment stays valid.
    coinbase_tx.output.extend_from_slice(extra_coinbase_outputs);
    let txdata: Vec<Transaction> = std::iter::once(Ok(coinbase_tx))
        .chain(
            block_template
                .transactions
                .iter()
                .map(|tx| bitcoin::consensus::deserialize(&tx.data)),
        )
        .collect::<Result<_, _>>()?;
    let merkle_root = {
        let hashes = txdata.iter().map(|tx| tx.compute_txid().to_raw_hash());
        bitcoin::merkle_tree::calculate_root(hashes)
            .map(bitcoin::TxMerkleNode::from)
            .unwrap()
    };
    let header = bitcoin::block::Header {
        version: block_template.version,
        prev_blockhash: block_template.prev_blockhash,
        merkle_root,
        time: std::cmp::max(block_template.current_time, block_template.mintime) as u32,
        bits: block_template.compact_target,
        nonce: u32::from_le_bytes(block_template.nonce_range[..=3].try_into().unwrap()),
    };
    tracing::debug!("Mining header");
    let header_hex = post_setup
        .bitcoin_util()?
        .command::<String, _, _, _, _>(
            [],
            "grind",
            [bitcoin::consensus::encode::serialize_hex(&header)],
        )
        .run_utf8()
        .await?;
    tracing::debug!("Mined header, submitting block...");
    let header: bitcoin::block::Header = bitcoin::consensus::encode::deserialize_hex(&header_hex)?;
    let block = bitcoin::Block { header, txdata };
    let block_hash = block.block_hash();
    // `null` on success, the rejection reason otherwise.
    let submitblock_output: Option<String> = post_setup
        .bitcoind_client
        .request(
            "submitblock",
            rpc_params![bitcoin::consensus::encode::serialize_hex(&block)],
        )
        .await?;
    if let Some(err_msg) = submitblock_output {
        return Err(MineGbtError::SubmitBlock { err_msg });
    }
    Ok(block_hash)
}

#[derive(Debug, Error)]
pub enum MineSignetError {
    #[error("Unexpected block disconnect")]
    BlockDisconnect,
    #[error("Timed out after {WAIT_TIMEOUT:?} waiting for a block event")]
    BlockEventTimeout,
    #[error(transparent)]
    Mine(#[from] crate::signet_miner::MineSignetBlockError),
    #[error("Expected block event")]
    NoBlockEvent,
    #[error("Signet miner not configured")]
    NoSignetMiner,
    #[error(transparent)]
    ValidatorClient(#[from] ConnectError),
}

fn subscribe_request<S: Sidechain>() -> SubscribeEventsRequest {
    SubscribeEventsRequest {
        sidechain_id: proto::wrap_u32(S::SIDECHAIN_NUMBER.0.into()),
    }
}

fn proto_err_to_connect(err: proto::Error) -> ConnectError {
    err.builder().to_connect_error()
}

// Mine blocks, running a check after each block
pub async fn mine_signet_check<F, Err, S>(
    post_setup: &mut PostSetup,
    blocks: u32,
    mut check: F,
) -> Result<(), Either<MineSignetError, Err>>
where
    F: FnMut(bitcoin::BlockHash) -> Result<(), Err>,
    S: Sidechain,
{
    use proto::mainchain::subscribe_events_response::event::Event;
    let signet_miner = post_setup
        .signet_miner
        .as_ref()
        .ok_or(either::Left(MineSignetError::NoSignetMiner))?;
    let mut stream = post_setup
        .validator_service_client
        .subscribe_events(subscribe_request::<S>())
        .await
        .map_err(|err| Either::Left(err.into()))?;
    for _ in 0..blocks {
        let _block_hash = signet_miner
            .mine_block(
                &post_setup.bitcoind_client,
                TemplateSource::Enforcer(&post_setup.gbt_client),
            )
            .await
            .map_err(|err| Either::Left(err.into()))?;
        let Some(view) = timeout(WAIT_TIMEOUT, stream.message())
            .await
            .map_err(|_elapsed| Either::Left(MineSignetError::BlockEventTimeout))?
            .map_err(|err| Either::Left(err.into()))?
        else {
            return Err(Either::Left(MineSignetError::NoBlockEvent));
        };
        let resp: SubscribeEventsResponse = view.to_owned_message();
        let resp_event = resp
            .event
            .into_option()
            .ok_or_else(|| proto::Error::missing_field::<SubscribeEventsResponse>("event"))
            .map_err(|err| Either::Left(proto_err_to_connect(err).into()))?
            .event
            .ok_or_else(|| proto::Error::missing_field::<subscribe_events_response::Event>("event"))
            .map_err(|err| Either::Left(proto_err_to_connect(err).into()))?;
        match resp_event {
            Event::ConnectBlock(connect_block) => {
                let header_info = connect_block
                    .header_info
                    .into_option()
                    .ok_or_else(|| proto::Error::missing_field::<ConnectBlock>("header_info"))
                    .map_err(|err| Either::Left(proto_err_to_connect(err).into()))?;
                let block_hash = header_info
                    .block_hash
                    .into_option()
                    .ok_or_else(|| proto::Error::missing_field::<BlockHeaderInfo>("block_hash"))
                    .map_err(|err| Either::Left(proto_err_to_connect(err).into()))?
                    .decode_status::<BlockHeaderInfo, _>("block_hash")
                    .map_err(|err| Either::Left(err.into()))?;
                check(block_hash).map_err(Either::Right)?
            }
            Event::DisconnectBlock(_) => {
                return Err(Either::Left(MineSignetError::BlockDisconnect));
            }
        };
    }
    Ok(())
}

/// Resolves once bitcoind reports `block_hash` as invalid. That is the only
/// trace of the enforcer rejecting a block: it `invalidateblock`s it and
/// re-syncs to the tip it already had, so no validator event follows.
async fn block_rejected(post_setup: &PostSetup, block_hash: bitcoin::BlockHash) {
    loop {
        match chaintip_status(post_setup, block_hash).await {
            Ok(status) if status.as_deref() == Some("invalid") => return,
            Ok(_) => (),
            Err(err) => tracing::debug!("getchaintips failed: {err:#}"),
        }
        sleep(WAIT_POLL_INTERVAL_SLOW).await;
    }
}

// Mine blocks, running a check after each block
pub async fn mine_gbt_check<F, Err, S>(
    post_setup: &mut PostSetup,
    blocks: u32,
    check: F,
) -> Result<(), Either<MineGbtError, Err>>
where
    F: FnMut(bitcoin::BlockHash) -> Result<(), Err>,
    S: Sidechain,
{
    mine_gbt_check_with_coinbase_outputs::<_, _, S>(post_setup, blocks, &[], check).await
}

/// [`mine_gbt_check`], appending `extra_coinbase_outputs` to each block's
/// coinbase.
pub async fn mine_gbt_check_with_coinbase_outputs<F, Err, S>(
    post_setup: &mut PostSetup,
    blocks: u32,
    extra_coinbase_outputs: &[TxOut],
    mut check: F,
) -> Result<(), Either<MineGbtError, Err>>
where
    F: FnMut(bitcoin::BlockHash) -> Result<(), Err>,
    S: Sidechain,
{
    use proto::mainchain::subscribe_events_response::event::Event;
    let mut stream = post_setup
        .validator_service_client
        .subscribe_events(subscribe_request::<S>())
        .await
        .map_err(|err| Either::Left(err.into()))?;
    for _ in 0..blocks {
        let mined_block_hash = mine_gbt(post_setup, extra_coinbase_outputs)
            .await
            .map_err(Either::Left)?;
        let deadline = Instant::now() + WAIT_TIMEOUT;
        let mut rejected = std::pin::pin!(block_rejected(post_setup, mined_block_hash));
        // Events for blocks connected before this one may still be queued.
        loop {
            let message = tokio::select! {
                message = stream.message() => message.map_err(|err| Either::Left(err.into()))?,
                () = &mut rejected => {
                    return Err(Either::Left(MineGbtError::BlockRejected {
                        block_hash: mined_block_hash,
                    }));
                }
                () = sleep_until(deadline) => {
                    return Err(Either::Left(MineGbtError::BlockEventTimeout {
                        block_hash: mined_block_hash,
                    }));
                }
            };
            let Some(view) = message else {
                return Err(Either::Left(MineGbtError::NoBlockEvent));
            };
            let resp: SubscribeEventsResponse = view.to_owned_message();
            let resp_event = resp
                .event
                .into_option()
                .ok_or_else(|| proto::Error::missing_field::<SubscribeEventsResponse>("event"))
                .map_err(|err| Either::Left(proto_err_to_connect(err).into()))?
                .event
                .ok_or_else(|| {
                    proto::Error::missing_field::<subscribe_events_response::Event>("event")
                })
                .map_err(|err| Either::Left(proto_err_to_connect(err).into()))?;
            let connect_block = match resp_event {
                Event::ConnectBlock(connect_block) => connect_block,
                Event::DisconnectBlock(_) => {
                    return Err(Either::Left(MineGbtError::BlockDisconnect));
                }
            };
            let header_info = connect_block
                .header_info
                .into_option()
                .ok_or_else(|| proto::Error::missing_field::<ConnectBlock>("header_info"))
                .map_err(|err| Either::Left(proto_err_to_connect(err).into()))?;
            let block_hash: bitcoin::BlockHash = header_info
                .block_hash
                .into_option()
                .ok_or_else(|| proto::Error::missing_field::<BlockHeaderInfo>("block_hash"))
                .map_err(|err| Either::Left(proto_err_to_connect(err).into()))?
                .decode_status::<BlockHeaderInfo, _>("block_hash")
                .map_err(|err| Either::Left(err.into()))?;
            if block_hash == mined_block_hash {
                break;
            }
            tracing::debug!(%block_hash, %mined_block_hash, "Skipping event for an earlier block");
        }
        check(mined_block_hash).map_err(Either::Right)?
    }
    Ok(())
}

/// What the producer votes while mining: the sidechain-proposal ACK policy
/// (BIP300 M2) and the withdrawal-bundle policy (M4), which are independent
/// settings on the enforcer.
#[derive(Clone, Copy, Debug)]
pub struct MiningPolicy {
    pub ack: AckAllProposalsPolicy,
    pub bundle: WithdrawalBundlePolicy,
}

impl MiningPolicy {
    /// Vote: ACK proposals for empty sidechain slots, and upvote the pending
    /// withdrawal bundles this node holds itself. What a miner participating
    /// in drivechain normally does.
    pub const VOTE: Self = Self {
        ack: AckAllProposalsPolicy::NewSlots,
        bundle: WithdrawalBundlePolicy::Known,
    };

    /// Cast no votes at all, so nothing moves except by explicit ACK.
    pub const SILENT: Self = Self {
        ack: AckAllProposalsPolicy::None,
        bundle: WithdrawalBundlePolicy::None,
    };
}

// Mine blocks via `GenerateToAddress`, running a check after each block.
// `GenerateToAddress` mines with the persisted policies, so set them first to
// mirror the requested per-call behavior.
pub async fn mine_generateblocks_check<F, Err>(
    post_setup: &mut PostSetup,
    blocks: u32,
    policy: MiningPolicy,
    mut check: F,
) -> Result<(), Either<ConnectError, Err>>
where
    F: FnMut(bitcoin::BlockHash) -> Result<(), Err>,
{
    let () = post_setup
        .block_producer_service_client
        .set_ack_all_proposals(SetAckAllProposalsRequest {
            policy: policy.ack.into(),
        })
        .await
        .map(|_| ())
        .map_err(Either::Left)?;
    let () = post_setup
        .block_producer_service_client
        .set_withdrawal_bundle_policy(SetWithdrawalBundlePolicyRequest {
            policy: policy.bundle.into(),
        })
        .await
        .map(|_| ())
        .map_err(Either::Left)?;
    let request = GenerateToAddressRequest {
        blocks: proto::wrap_u32(blocks),
        address: post_setup.mining_address.to_string(),
    };
    let resp: GenerateToAddressResponse = post_setup
        .mining_service_client
        .generate_to_address(request)
        .await
        .map_err(Either::Left)?
        .into_owned();
    for block_hash in resp.block_hashes {
        let block_hash = block_hash
            .decode_status::<GenerateToAddressResponse, _>("block_hashes")
            .map_err(Either::Left)?;
        let () = check(block_hash).map_err(Either::Right)?;
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum MineError {
    #[error(transparent)]
    GenerateToAddress(ConnectError),
    #[error(transparent)]
    Gbt(MineGbtError),
    #[error(transparent)]
    Signet(MineSignetError),
    #[error("the GenerateBlocks mining mode is not supported on Signet")]
    SignetGenerateBlocks,
}

pub async fn mine<S>(
    post_setup: &mut PostSetup,
    blocks: u32,
    policy: MiningPolicy,
) -> Result<(), MineError>
where
    S: Sidechain,
{
    use std::convert::Infallible;
    match (post_setup.network, post_setup.mode.mining_mode()) {
        (Network::Regtest, MiningMode::GenerateBlocks) => {
            mine_generateblocks_check(post_setup, blocks, policy, |_| Ok::<_, Infallible>(()))
                .await
                .map_err(|err| match err {
                    Either::Left(err) => MineError::GenerateToAddress(err),
                })
        }
        (Network::Regtest, MiningMode::GetBlockTemplate) => {
            mine_gbt_check::<_, Infallible, S>(post_setup, blocks, |_| Ok(()))
                .await
                .map_err(|err| match err {
                    Either::Left(err) => MineError::Gbt(err),
                })
        }
        (Network::Signet, MiningMode::GetBlockTemplate) => {
            mine_signet_check::<_, Infallible, S>(post_setup, blocks, |_| Ok(()))
                .await
                .map_err(|err| match err {
                    Either::Left(err) => MineError::Signet(err),
                })
        }
        (Network::Signet, MiningMode::GenerateBlocks) => Err(MineError::SignetGenerateBlocks),
    }
}

/// Mine blocks, and check the events for each block
pub async fn mine_check_block_events<F, S>(
    post_setup: &mut PostSetup,
    blocks: u32,
    policy: MiningPolicy,
    mut check: F,
) -> anyhow::Result<()>
where
    F: FnMut(u32, proto::mainchain::BlockInfo) -> anyhow::Result<()>,
    S: Sidechain,
{
    tracing::debug!("Mining {blocks} block(s)");
    let mut events = post_setup
        .validator_service_client
        .subscribe_events(subscribe_request::<S>())
        .await?;
    for blocks_mined in 0..blocks {
        let () = mine::<S>(post_setup, 1, policy).await?;
        let Some(view) = timeout(WAIT_TIMEOUT, events.message())
            .await
            .map_err(|_elapsed| {
                anyhow::anyhow!("Timed out after {WAIT_TIMEOUT:?} waiting for a block event")
            })??
        else {
            anyhow::bail!("Expected a block event")
        };
        let resp: SubscribeEventsResponse = view.to_owned_message();
        let Some(event) = resp.event.into_option().and_then(|inner| inner.event) else {
            anyhow::bail!("Expected event")
        };
        let proto::mainchain::subscribe_events_response::event::Event::ConnectBlock(connect_block) =
            event
        else {
            anyhow::bail!("Expected connect block event")
        };
        let Some(block_info) = connect_block.block_info.into_option() else {
            anyhow::bail!("Expected block info")
        };
        let () = check(blocks_mined, block_info)?;
    }
    Ok(())
}
