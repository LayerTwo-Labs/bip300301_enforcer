use std::time::Instant;

use bitcoin::{BlockHash, block::Header};
use miette::Diagnostic;
use reqwest::{Client, Response, Url};
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
    #[error("Invalid binary block-header response length: {length} bytes is not a multiple of 80")]
    InvalidBlockHeadersLength { length: usize },
    #[error("Failed to decode binary block header at index {index}")]
    BlockHeaderDecode {
        index: usize,
        #[source]
        source: bitcoin::consensus::encode::Error,
    },
    #[error("Block-header height overflow from height {start_height} at offset {offset}")]
    BlockHeaderHeightOverflow { start_height: u32, offset: usize },
    #[error("Bitcoin Core REST server is not enabled")]
    #[diagnostic(code(bip300301_enforcer::rest_server_not_enabled))]
    #[help("do this with the `-rest` flag or `rest=1` in your Bitcoin Core configuration file")]
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

impl MainRestClient {
    pub fn new(base_url: Url) -> Self {
        Self {
            client: Client::new(),
            base_url,
        }
    }

    async fn send_request(&self, url: Url) -> Result<Response, MainRestClientError> {
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

        Ok(response)
    }

    async fn do_json_request<T: DeserializeOwned>(
        &self,
        url: Url,
    ) -> Result<T, MainRestClientError> {
        self.send_request(url)
            .await?
            .json::<T>()
            .await
            .inspect_err(|err| {
                tracing::warn!("failed to parse response: {:#}", ErrorChain::new(err))
            })
            .map_err(MainRestClientError::Http)
    }

    pub async fn get_chain_info(&self) -> Result<ChainInfo, MainRestClientError> {
        let url = self.base_url.join("rest/chaininfo.json")?;
        self.do_json_request(url).await
    }

    /// Fetches binary block headers from Bitcoin Core's REST API.
    ///
    /// The binary representation contains only the 80-byte consensus headers,
    /// so hashes are recomputed locally and heights are inferred from the
    /// caller-provided starting height.
    /// https://github.com/bitcoin/bitcoin/blob/master/doc/REST-interface.md#blockheaders
    /// Returns a vec of headers, block hashes, and height for each header
    #[instrument(skip(self))]
    pub async fn get_block_headers(
        &self,
        block_hash: &BlockHash,
        start_height: u32,
        descendants: usize,
    ) -> Result<Vec<(Header, BlockHash, u32)>, MainRestClientError> {
        let start = Instant::now();

        let url = self.base_url.join(&format!(
            "rest/headers/{block_hash}.bin?count={descendants}",
        ))?;

        let response = self.send_request(url).await?;
        let bytes = response.bytes().await?;
        let headers = decode_block_headers(&bytes, start_height)?;

        tracing::debug!(
            "Fetched {} block header(s) in {}: {} -> {}",
            headers.len(),
            jiff::SignedDuration::try_from(start.elapsed()).unwrap(),
            headers.first().map(|(_, _, height)| *height).unwrap_or(0),
            headers.last().map(|(_, _, height)| *height).unwrap_or(0),
        );

        if headers
            .first()
            .is_some_and(|(_, returned_hash, _)| returned_hash != block_hash)
        {
            return Err(MainRestClientError::InvalidBlockHash);
        }

        Ok(headers)
    }
}

fn decode_block_headers(
    bytes: &[u8],
    start_height: u32,
) -> Result<Vec<(Header, BlockHash, u32)>, MainRestClientError> {
    const SERIALIZED_HEADER_LEN: usize = 80;

    if !bytes.len().is_multiple_of(SERIALIZED_HEADER_LEN) {
        return Err(MainRestClientError::InvalidBlockHeadersLength {
            length: bytes.len(),
        });
    }

    bytes
        .chunks_exact(SERIALIZED_HEADER_LEN)
        .enumerate()
        .map(|(offset, bytes)| {
            let header: Header = bitcoin::consensus::deserialize(bytes).map_err(|source| {
                MainRestClientError::BlockHeaderDecode {
                    index: offset,
                    source,
                }
            })?;
            let offset_u32 = u32::try_from(offset).map_err(|_| {
                MainRestClientError::BlockHeaderHeightOverflow {
                    start_height,
                    offset,
                }
            })?;
            let height = start_height.checked_add(offset_u32).ok_or(
                MainRestClientError::BlockHeaderHeightOverflow {
                    start_height,
                    offset,
                },
            )?;
            let block_hash = header.block_hash();
            Ok((header, block_hash, height))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use bitcoin::{CompactTarget, TxMerkleNode, block, hashes::Hash as _};

    use super::*;

    fn header(prev_blockhash: BlockHash, nonce: u32) -> Header {
        Header {
            version: block::Version::ONE,
            prev_blockhash,
            merkle_root: TxMerkleNode::all_zeros(),
            time: 1_700_000_000,
            bits: CompactTarget::from_consensus(0x207f_ffff),
            nonce,
        }
    }

    #[test]
    fn decodes_binary_headers_and_infers_hashes_and_heights() {
        let first = header(BlockHash::all_zeros(), 1);
        let second = header(first.block_hash(), 2);
        let bytes = [
            bitcoin::consensus::serialize(&first),
            bitcoin::consensus::serialize(&second),
        ]
        .concat();

        let decoded = decode_block_headers(&bytes, 42).expect("valid binary headers");

        assert_eq!(
            decoded,
            vec![
                (first, first.block_hash(), 42),
                (second, second.block_hash(), 43)
            ]
        );
    }

    #[test]
    fn rejects_a_partial_binary_header() {
        let err = decode_block_headers(&[0; 79], 0).expect_err("79 bytes is not a full header");
        assert!(matches!(
            err,
            MainRestClientError::InvalidBlockHeadersLength { length: 79 }
        ));
    }

    #[test]
    fn rejects_inferred_height_overflow() {
        let first = header(BlockHash::all_zeros(), 1);
        let second = header(first.block_hash(), 2);
        let bytes = [
            bitcoin::consensus::serialize(&first),
            bitcoin::consensus::serialize(&second),
        ]
        .concat();

        let err = decode_block_headers(&bytes, u32::MAX)
            .expect_err("the second inferred height must overflow");
        assert!(matches!(
            err,
            MainRestClientError::BlockHeaderHeightOverflow {
                start_height: u32::MAX,
                offset: 1,
            }
        ));
    }
}
