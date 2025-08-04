//! Mining utilities

use bip300301_enforcer_lib::{
    bins::{CommandError, CommandExt, SignetMiner},
    proto::{
        self, ToStatus,
        mainchain::{
            BlockHeaderInfo, GenerateBlocksRequest, GenerateBlocksResponse, SubscribeEventsRequest,
            subscribe_events_response::event::ConnectBlock,
        },
        sidechain::{SubscribeEventsResponse, subscribe_events_response},
    },
};
use bitcoin::Address;
use either::Either;
use futures::TryStreamExt as _;
use thiserror::Error;

use crate::setup::{MiningMode, Network, PostSetup, Sidechain};

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
    ValidatorClient(#[from] tonic::Status),
}

async fn mine_gbt(post_setup: &mut PostSetup) -> Result<bitcoin::BlockHash, MineGbtError> {
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
        .bitcoin_util
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
    if submitblock_output.is_empty() {
        tracing::debug!(%block_hash, %submitblock_output, "Submitted block");
        Ok(block_hash)
    } else {
        Err(MineGbtError::SubmitBlock {
            err_msg: submitblock_output,
        })
    }
}

#[derive(Debug, Error)]
pub enum MineSignetError {
    #[error("Unexpected block disconnect")]
    BlockDisconnect,
    #[error(transparent)]
    Command(#[from] CommandError),
    #[error("Expected block event")]
    NoBlockEvent,
    #[error(transparent)]
    ValidatorClient(#[from] tonic::Status),
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
    let mut stream = post_setup
        .validator_service_client
        .subscribe_events(SubscribeEventsRequest {
            sidechain_id: Some(S::SIDECHAIN_NUMBER.0.into()),
        })
        .await
        .map_err(|err| Either::Left(err.into()))?
        .into_inner();
    for _ in 0..blocks {
        let () = mine_single_signet(&post_setup.signet_miner, &post_setup.mining_address)
            .await
            .map_err(|err| Either::Left(err.into()))?;
        let Some(resp) = stream
            .try_next()
            .await
            .map_err(|err| Either::Left(err.into()))?
        else {
            return Err(Either::Left(MineSignetError::NoBlockEvent));
        };
        let resp_event = resp
            .event
            .ok_or_else(|| proto::Error::missing_field::<SubscribeEventsResponse>("event"))
            .map_err(|err| Either::Left(err.builder().to_status().into()))?
            .event
            .ok_or_else(|| proto::Error::missing_field::<subscribe_events_response::Event>("event"))
            .map_err(|err| Either::Left(err.builder().to_status().into()))?;
        match resp_event {
            Event::ConnectBlock(connect_block) => {
                let header_info = connect_block
                    .header_info
                    .ok_or_else(|| proto::Error::missing_field::<ConnectBlock>("header_info"))
                    .map_err(|err| Either::Left(err.builder().to_status().into()))?;
                let block_hash = header_info
                    .block_hash
                    .ok_or_else(|| proto::Error::missing_field::<BlockHeaderInfo>("block_hash"))
                    .map_err(|err| Either::Left(err.builder().to_status().into()))?
                    .decode_tonic::<BlockHeaderInfo, _>("block_hash")
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
        .subscribe_events(SubscribeEventsRequest {
            sidechain_id: Some(S::SIDECHAIN_NUMBER.0.into()),
        })
        .await
        .map_err(|err| Either::Left(err.into()))?
        .into_inner();
    for _ in 0..blocks {
        let _block_hash = mine_gbt(post_setup).await.map_err(Either::Left)?;
        let Some(resp) = stream
            .try_next()
            .await
            .map_err(|err| Either::Left(err.into()))?
        else {
            return Err(Either::Left(MineGbtError::NoBlockEvent));
        };
        let resp_event = resp
            .event
            .ok_or_else(|| proto::Error::missing_field::<SubscribeEventsResponse>("event"))
            .map_err(|err| Either::Left(err.builder().to_status().into()))?
            .event
            .ok_or_else(|| proto::Error::missing_field::<subscribe_events_response::Event>("event"))
            .map_err(|err| Either::Left(err.builder().to_status().into()))?;
        match resp_event {
            Event::ConnectBlock(connect_block) => {
                let header_info = connect_block
                    .header_info
                    .ok_or_else(|| proto::Error::missing_field::<ConnectBlock>("header_info"))
                    .map_err(|err| Either::Left(err.builder().to_status().into()))?;
                let block_hash = header_info
                    .block_hash
                    .ok_or_else(|| proto::Error::missing_field::<BlockHeaderInfo>("block_hash"))
                    .map_err(|err| Either::Left(err.builder().to_status().into()))?
                    .decode_tonic::<BlockHeaderInfo, _>("block_hash")
                    .map_err(|err| Either::Left(err.into()))?;
                check(block_hash).map_err(Either::Right)?
            }
            Event::DisconnectBlock(_) => return Err(Either::Left(MineGbtError::BlockDisconnect)),
        };
    }
    Ok(())
}

// Mine blocks, running a check after each block
pub async fn mine_generateblocks_check<F, Err>(
    post_setup: &mut PostSetup,
    blocks: u32,
    ack_all_proposals: Option<bool>,
    mut check: F,
) -> Result<(), Either<tonic::Status, Err>>
where
    F: FnMut(bitcoin::BlockHash) -> Result<(), Err>,
{
    let request = GenerateBlocksRequest {
        blocks: Some(blocks),
        ack_all_proposals: ack_all_proposals.unwrap_or(false),
    };
    let mut stream = post_setup
        .wallet_service_client
        .generate_blocks(request)
        .await
        .map_err(Either::Left)?
        .into_inner();
    while let Some(resp) = stream.try_next().await.map_err(Either::Left)? {
        let GenerateBlocksResponse { block_hash } = resp;
        let block_hash = block_hash
            .ok_or_else(|| {
                proto::Error::missing_field::<GenerateBlocksResponse>("block_hash").into()
            })
            .map_err(Either::Left)?
            .decode_tonic::<GenerateBlocksResponse, _>("block_hash")
            .map_err(Either::Left)?;
        let () = check(block_hash).map_err(Either::Right)?;
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum MineError {
    #[error(transparent)]
    GenerateBlocks(tonic::Status),
    #[error(transparent)]
    Gbt(MineGbtError),
    #[error(transparent)]
    Signet(MineSignetError),
    #[error("GenerateBlocks is not supported on Signet")]
    SignetGenerateBlocks,
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
    match (post_setup.network, post_setup.mode.mining_mode()) {
        (Network::Regtest, MiningMode::GenerateBlocks) => {
            mine_generateblocks_check(post_setup, blocks, ack_all_proposals, |_| {
                Ok::<_, Infallible>(())
            })
            .await
            .map_err(|err| match err {
                Either::Left(err) => MineError::GenerateBlocks(err),
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
        .subscribe_events(SubscribeEventsRequest {
            sidechain_id: Some(S::SIDECHAIN_NUMBER.0.into()),
        })
        .await?
        .into_inner();
    for blocks_mined in 0..blocks {
        let () = mine::<S>(post_setup, 1, ack_all_proposals).await?;
        let Some(event) = events
            .try_next()
            .await?
            .and_then(|event| event.event)
            .and_then(|event| event.event)
        else {
            anyhow::bail!("Expected a block event")
        };
        let proto::mainchain::subscribe_events_response::event::Event::ConnectBlock(connect_block) =
            event
        else {
            anyhow::bail!("Expected connect block event")
        };
        let Some(block_info) = connect_block.block_info else {
            anyhow::bail!("Expected block info")
        };
        let () = check(blocks_mined, block_info)?;
    }
    Ok(())
}
