//! Mining utilities

use std::sync::Arc;

use bip300301_enforcer_lib::{
    bins::{CommandError, CommandExt, SignetMiner},
    proto::{
        self, ToStatus,
        mainchain::{
            BlockHeaderInfo, GenerateToAddressRequest, GenerateToAddressResponse,
            SetAckAllProposalsRequest, SubscribeEventsRequest, SubscribeEventsResponse,
            subscribe_events_response, subscribe_events_response::event::ConnectBlock,
        },
    },
};
use bitcoin::Address;
use connectrpc::ConnectError;
use either::Either;
use thiserror::Error;

use crate::{
    setup::{MiningMode, Network, PostSetup, Sidechain},
    util::VarError,
};

/// Mine a single signet block
async fn mine_single_signet(
    signet_miner: &SignetMiner,
    mining_address: &Address,
) -> Result<(), CommandError> {
    let _mine_output = signet_miner
        .command(
            "generate",
            vec![
                "--address",
                &mining_address.to_string(),
                "--block-interval",
                "1",
            ],
        )
        .run_utf8()
        .await?;
    Ok(())
}

#[derive(Debug, Error)]
pub enum MineGbtError {
    #[error("Unexpected block disconnect")]
    BlockDisconnect,
    #[error(transparent)]
    Command(#[from] CommandError),
    #[error(transparent)]
    ConsensusDecode(#[from] bitcoin::consensus::encode::Error),
    #[error(transparent)]
    ConsensusDecodeHex(#[from] bitcoin::consensus::encode::FromHexError),
    #[error(transparent)]
    GbtClient(#[from] jsonrpsee::core::ClientError),
    #[error("Missing coinbasetxn in block template")]
    MissingCoinbaseTxn,
    #[error("Expected block event")]
    NoBlockEvent,
    #[error("Submitting block failed with error: `{err_msg}`")]
    SubmitBlock { err_msg: String },
    #[error(transparent)]
    ValidatorClient(#[from] ConnectError),
    #[error(transparent)]
    Var(#[from] Arc<VarError>),
}

/// Mine a single GBT block and return its hash directly, without waiting for
/// a `ConnectBlock` event -- unlike `mine`/`mine_gbt_check`, safe to use when
/// the caller expects the enforcer to *reject* the resulting block (in which
/// case no `ConnectBlock` event will ever arrive).
pub(crate) async fn mine_gbt(
    post_setup: &mut PostSetup,
) -> Result<bitcoin::BlockHash, MineGbtError> {
    use cusf_enforcer_mempool::server::RpcClient;
    let mut gbt_request = bitcoin_jsonrpsee::client::BlockTemplateRequest::default();
    gbt_request.capabilities.insert("coinbasetxn".to_owned());
    tracing::debug!("Requesting block template");
    let block_template = post_setup
        .gbt_client
        .get_block_template(gbt_request)
        .await?;
    let bitcoin_jsonrpsee::client::CoinbaseTxnOrValue::Txn(coinbase_tx) =
        block_template.coinbase_txn_or_value
    else {
        return Err(MineGbtError::MissingCoinbaseTxn);
    };
    let merkle_root = {
        let hashes = std::iter::once(&coinbase_tx)
            .chain(&block_template.transactions)
            .map(|tx| tx.txid.to_raw_hash());
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
    let txdata = std::iter::once(coinbase_tx)
        .chain(block_template.transactions)
        .map(|tx| bitcoin::consensus::deserialize(&tx.data))
        .collect::<Result<_, _>>()?;
    let block = bitcoin::Block { header, txdata };
    let block_hash = block.block_hash();
    let submitblock_output = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "submitblock",
            [bitcoin::consensus::encode::serialize_hex(&block)],
        )
        .run_utf8()
        .await?;
    if !submitblock_output.is_empty() {
        return Err(MineGbtError::SubmitBlock {
            err_msg: submitblock_output,
        });
    }
    Ok(block_hash)
}

#[derive(Debug, Error)]
pub enum MineSignetError {
    #[error("Unexpected block disconnect")]
    BlockDisconnect,
    #[error(transparent)]
    Command(#[from] CommandError),
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
        let () = mine_single_signet(signet_miner, &post_setup.mining_address)
            .await
            .map_err(|err| Either::Left(err.into()))?;
        let Some(view) = stream
            .message()
            .await
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

// Mine blocks, running a check after each block
pub async fn mine_gbt_check<F, Err, S>(
    post_setup: &mut PostSetup,
    blocks: u32,
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
        let _block_hash = mine_gbt(post_setup).await.map_err(Either::Left)?;
        let Some(view) = stream
            .message()
            .await
            .map_err(|err| Either::Left(err.into()))?
        else {
            return Err(Either::Left(MineGbtError::NoBlockEvent));
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
            Event::DisconnectBlock(_) => return Err(Either::Left(MineGbtError::BlockDisconnect)),
        };
    }
    Ok(())
}

// Mine blocks via `GenerateToAddress`, running a check after each block.
// `GenerateToAddress` mines with the persisted ACK policy, so set it first to
// mirror the requested per-call behavior.
pub async fn mine_generateblocks_check<F, Err>(
    post_setup: &mut PostSetup,
    blocks: u32,
    ack_all_proposals: Option<bool>,
    mut check: F,
) -> Result<(), Either<ConnectError, Err>>
where
    F: FnMut(bitcoin::BlockHash) -> Result<(), Err>,
{
    let () = post_setup
        .block_producer_service_client
        .set_ack_all_proposals(SetAckAllProposalsRequest {
            ack_all: ack_all_proposals.unwrap_or(false),
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
    #[error(transparent)]
    CoinbaseClaim(#[from] CoinbaseClaimError),
}

#[derive(Debug, Error)]
pub enum CoinbaseClaimError {
    #[error(transparent)]
    Command(#[from] CommandError),
    #[error("failed to parse verbose block")]
    ParseBlock(#[from] serde_json::Error),
    #[error("invalid amount in verbose block")]
    ParseAmount(#[from] bitcoin::amount::ParseAmountError),
    #[error("invalid block hash in verbose block")]
    ParseHash(#[from] bitcoin::hashes::hex::HexToArrayError),
    #[error("transaction `{txid}` in block `{block_hash}` has no fee field")]
    MissingFee {
        block_hash: bitcoin::BlockHash,
        txid: String,
    },
    #[error("block `{block_hash}` has no coinbase transaction")]
    NoCoinbase { block_hash: bitcoin::BlockHash },
    #[error("block `{block_hash}` has no previous block hash")]
    NoPrevBlockHash { block_hash: bitcoin::BlockHash },
    #[error(
        "coinbase of block `{block_hash}` claims {claimed} but subsidy + fees is {expected} \
         (subsidy {subsidy}, fees {fees}): the difference is destroyed"
    )]
    ValueMismatch {
        block_hash: bitcoin::BlockHash,
        claimed: bitcoin::Amount,
        expected: bitcoin::Amount,
        subsidy: bitcoin::Amount,
        fees: bitcoin::Amount,
    },
}

/// Assert that the coinbase of `block_hash` claims the regtest subsidy plus
/// every fee paid by the block's transactions, returning the block's parent
/// hash (for walking a freshly mined range). A coinbase claiming less is
/// consensus-valid -- the difference is simply destroyed -- which would
/// silently burn BMM bids, whose fee is the miner's auction revenue.
pub async fn assert_coinbase_claims_fees(
    post_setup: &PostSetup,
    block_hash: bitcoin::BlockHash,
) -> Result<bitcoin::BlockHash, CoinbaseClaimError> {
    #[derive(serde::Deserialize)]
    struct Vout {
        value: f64,
    }
    #[derive(serde::Deserialize)]
    struct BlockTx {
        txid: String,
        fee: Option<f64>,
        vout: Vec<Vout>,
    }
    #[derive(serde::Deserialize)]
    struct VerboseBlock {
        height: u32,
        previousblockhash: Option<String>,
        tx: Vec<BlockTx>,
    }

    let block_json = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "getblock", [block_hash.to_string(), "2".to_owned()])
        .run_utf8()
        .await?;
    let block: VerboseBlock = serde_json::from_str(&block_json)?;

    let coinbase = block
        .tx
        .first()
        .ok_or(CoinbaseClaimError::NoCoinbase { block_hash })?;
    let mut claimed = bitcoin::Amount::ZERO;
    for vout in &coinbase.vout {
        claimed += bitcoin::Amount::from_btc(vout.value)?;
    }
    let mut fees = bitcoin::Amount::ZERO;
    for tx in &block.tx[1..] {
        let fee = tx.fee.ok_or_else(|| CoinbaseClaimError::MissingFee {
            block_hash,
            txid: tx.txid.clone(),
        })?;
        fees += bitcoin::Amount::from_btc(fee)?;
    }

    const REGTEST_HALVING_INTERVAL: u32 = 150;
    let halvings = block.height / REGTEST_HALVING_INTERVAL;
    let subsidy = if halvings >= 64 {
        bitcoin::Amount::ZERO
    } else {
        bitcoin::Amount::from_sat((50 * bitcoin::Amount::ONE_BTC.to_sat()) >> halvings)
    };

    let expected = subsidy + fees;
    if claimed != expected {
        return Err(CoinbaseClaimError::ValueMismatch {
            block_hash,
            claimed,
            expected,
            subsidy,
            fees,
        });
    }
    block
        .previousblockhash
        .ok_or(CoinbaseClaimError::NoPrevBlockHash { block_hash })?
        .parse()
        .map_err(CoinbaseClaimError::from)
}

/// Assert [`assert_coinbase_claims_fees`] for the `blocks` most recently
/// mined blocks, walking back from the current tip.
async fn assert_recent_coinbases_claim_fees(
    post_setup: &PostSetup,
    blocks: u32,
) -> Result<(), CoinbaseClaimError> {
    let mut block_hash: bitcoin::BlockHash = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getbestblockhash", [])
        .run_utf8()
        .await?
        .trim()
        .parse()?;
    for _ in 0..blocks {
        block_hash = assert_coinbase_claims_fees(post_setup, block_hash).await?;
    }
    Ok(())
}

pub async fn mine<S>(
    post_setup: &mut PostSetup,
    blocks: u32,
    ack_all_proposals: Option<bool>,
) -> Result<(), MineError>
where
    S: Sidechain,
{
    use std::convert::Infallible;
    let () = match (post_setup.network, post_setup.mode.mining_mode()) {
        (Network::Regtest, MiningMode::GenerateBlocks) => {
            mine_generateblocks_check(post_setup, blocks, ack_all_proposals, |_| {
                Ok::<_, Infallible>(())
            })
            .await
            .map_err(|err| match err {
                Either::Left(err) => MineError::GenerateToAddress(err),
            })?
        }
        (Network::Regtest, MiningMode::GetBlockTemplate) => {
            mine_gbt_check::<_, Infallible, S>(post_setup, blocks, |_| Ok(()))
                .await
                .map_err(|err| match err {
                    Either::Left(err) => MineError::Gbt(err),
                })?
        }
        (Network::Signet, MiningMode::GetBlockTemplate) => {
            return mine_signet_check::<_, Infallible, S>(post_setup, blocks, |_| Ok(()))
                .await
                .map_err(|err| match err {
                    Either::Left(err) => MineError::Signet(err),
                });
        }
        (Network::Signet, MiningMode::GenerateBlocks) => {
            return Err(MineError::SignetGenerateBlocks);
        }
    };
    // Every regtest block mined through the enforcer -- either mode -- must
    // pay its fees to the coinbase, not destroy them.
    let () = assert_recent_coinbases_claim_fees(post_setup, blocks).await?;
    Ok(())
}

/// Mine blocks, and check the events for each block
pub async fn mine_check_block_events<F, S>(
    post_setup: &mut PostSetup,
    blocks: u32,
    ack_all_proposals: Option<bool>,
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
        let () = mine::<S>(post_setup, 1, ack_all_proposals).await?;
        let Some(view) = events.message().await? else {
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
