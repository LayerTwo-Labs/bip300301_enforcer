use std::time::Instant;

use bitcoin::{BlockHash, CompactTarget, TxMerkleNode, block::Header, hashes::Hash};
use miette::Diagnostic;
use reqwest::{Client, Url};
use serde::{Deserialize, de::DeserializeOwned};
use thiserror::Error;
use tracing::instrument;

use crate::errors::ErrorChain;

#[derive(Debug, Diagnostic, Error)]
pub enum MainRestClientError {
    #[error("URL parse error: {0}")]
    URL(#[from] url::ParseError),
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("Invalid block hash format")]
    InvalidBlockHash,
    #[error("Invalid block header format")]
    InvalidBlockHeader,
    #[error("Bitcoin Core REST server is not enabled")]
    RestServerNotEnabled,
    #[error("Bitcoin Core REST server at `{url}` is not reachable")]
    RestServerNotReachable {
        #[source]
        err: reqwest::Error,
        url: Url,
    },
}

#[derive(Debug, Clone)]
pub struct MainRestClient {
    client: Client,
    base_url: Url,
}

#[derive(Debug, Deserialize)]
pub struct ChainInfo {
    pub chain: String,
    pub blocks: u64,
    pub headers: u64,
    pub bestblockhash: BlockHash,
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

    async fn do_request<T: DeserializeOwned>(&self, url: Url) -> Result<T, MainRestClientError> {
        let response = match self.client.get(url).send().await {
            Ok(response) => response,
            Err(err) => {
                // To make it easier for the caller of this function to debug what's going wrong,
                // we indicate this with an extra clear error message.
                if err.is_connect() {
                    return Err(MainRestClientError::RestServerNotReachable {
                        err,
                        url: self.base_url.clone(),
                    });
                }
                return Err(MainRestClientError::Http(err));
            }
        };

        // Strictly speaking we cannot know if a 404 indicates us messing up the path,
        // or the server not being enabled. However, we're not exposing any method for
        // calling a particular path to the user, so we can assume that a 404 here means
        // the server is not enabled.
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(MainRestClientError::RestServerNotEnabled);
        }

        if !response.status().is_success() {
            return Err(MainRestClientError::Http(
                response.error_for_status().unwrap_err(),
            ));
        }

        response
            .json::<T>()
            .await
            .inspect_err(|err| {
                tracing::warn!("failed to parse response: {:#}", ErrorChain::new(err))
            })
            .map_err(MainRestClientError::Http)
    }

    pub async fn get_chain_info(&self) -> Result<ChainInfo, MainRestClientError> {
        let url = self.base_url.join("rest/chaininfo.json")?;
        self.do_request(url).await
    }

    /// Fetches a block header from Bitcoin Core's REST API
    /// https://github.com/bitcoin/bitcoin/blob/master/doc/REST-interface.md#blockheaders
    /// Returns a vec of headers, block hashes, and height for each header
    #[instrument(skip(self))]
    pub async fn get_block_headers(
        &self,
        block_hash: &BlockHash,
        descendants: usize,
    ) -> Result<Vec<(Header, BlockHash, u32)>, MainRestClientError> {
        let start = Instant::now();

        let url = self.base_url.join(&format!(
            "rest/headers/{block_hash}.json?count={descendants}",
        ))?;

        let headers = self.do_request::<Vec<RestHeader>>(url).await?;

        tracing::debug!(
            "Fetched {} block header(s) in {}: {} -> {}",
            headers.len(),
            jiff::SignedDuration::try_from(start.elapsed()).unwrap(),
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
