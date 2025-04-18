use std::time::Instant;

use bitcoin::{block::Header, hashes::Hash, BlockHash, CompactTarget, TxMerkleNode};
use reqwest::{Client, Url};
use serde::Deserialize;
use thiserror::Error;
use tracing::instrument;

#[derive(Debug, Error)]
pub enum MainRestClientError {
    #[error("URL parse error: {0}")]
    URL(#[from] url::ParseError),
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Invalid block hash format")]
    InvalidBlockHash,
    #[error("Invalid block header format")]
    InvalidBlockHeader,
}

#[derive(Debug, Clone)]
pub struct MainRestClient {
    client: Client,
    base_url: Url,
}

#[derive(Debug, Deserialize)]
struct RestHeader {
    hash: BlockHash,
    height: u32,
    version: bitcoin::blockdata::block::Version,
    previousblockhash: Option<BlockHash>,
    merkleroot: TxMerkleNode,
    time: u32,
    bits: String,
    nonce: u32,
}

impl MainRestClient {
    pub fn new(base_url: Url) -> Self {
        Self {
            client: Client::new(),
            base_url,
        }
    }

    /// Fetches a block header from Bitcoin Core's REST API
    /// https://github.com/bitcoin/bitcoin/blob/master/doc/REST-interface.md#blockheaders
    #[instrument(skip(self))]
    pub async fn get_block_headers(
        &self,
        block_hash: &BlockHash,
        descendants: usize,
    ) -> Result<Vec<(Header, BlockHash, u32)>, MainRestClientError> {
        let start = Instant::now();

        let url = self.base_url.join(&format!(
            "rest/headers/{}.json?count={}",
            block_hash, descendants
        ))?;

        let response = self.client.get(url).send().await?;

        if !response.status().is_success() {
            return Err(MainRestClientError::Http(
                response.error_for_status().unwrap_err(),
            ));
        }

        let headers = response
            .json::<Vec<RestHeader>>()
            .await
            .inspect_err(|err| {
                tracing::warn!("Failed to parse block headers: {err:#?}");
            })?;

        tracing::debug!(
            "Fetched {} block header(s) in {:?}: {} -> {}",
            headers.len(),
            start.elapsed(),
            headers.first().map(|h| h.height).unwrap_or(0),
            headers.last().map(|h| h.height).unwrap_or(0),
        );

        Ok(headers
            .into_iter()
            .map(|header| {
                (
                    Header {
                        version: header.version,
                        prev_blockhash: header.previousblockhash.unwrap_or(BlockHash::all_zeros()),
                        merkle_root: header.merkleroot,
                        time: header.time,
                        bits: CompactTarget::from_unprefixed_hex(&header.bits).unwrap(),
                        nonce: header.nonce,
                    },
                    header.hash,
                    header.height,
                )
            })
            .collect())
    }
}
