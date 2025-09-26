//! Parser for Bitcoin Core block files (blk*.dat)
//!
//! Bitcoin Core stores blocks in sequential .dat files in the blocks directory.
//! Each block is prefixed with:
//! - 4 bytes: Magic number (0xF9BEB4D9 for mainnet)
//! - 4 bytes: Block size (little-endian)
//! - Block data (serialized Bitcoin block)
//!
//! This module provides functionality to parse these files and extract individual blocks.
//!
//! The code is partly based on github.com/max-lt/blk-reader

use std::{fs::File, io::BufReader, path::PathBuf};

use bitcoin::{Network, Transaction, block::Header, consensus::Decodable};
use miette::Diagnostic;
use thiserror::Error;

type XorKey = [u8; 8];

/// Magic numbers for different Bitcoin networks
const MAINNET_MAGIC: [u8; 4] = [0xF9, 0xBE, 0xB4, 0xD9];
const TESTNET_MAGIC: [u8; 4] = [0x0B, 0x11, 0x09, 0x07];
const SIGNET_MAGIC: [u8; 4] = [0x0A, 0x03, 0xCF, 0x40];
const REGTEST_MAGIC: [u8; 4] = [0xFA, 0xBF, 0xB5, 0xDA];

#[derive(Debug, Diagnostic, Error)]
#[error("error parsing block number {index} in file {file_path}")]
pub struct ParseAllBlocksError {
    file_path: PathBuf,
    index: usize,
    source: ParseBlockFileError,
}

/// Errors that can occur during block file parsing
#[derive(Debug, Error)]
pub enum ParseBlockFileError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Invalid magic bytes: {found:02X?}, expected {expected:02X?}")]
    InvalidMagic { found: [u8; 4], expected: [u8; 4] },

    #[error("Invalid network: {found}, expected {expected}")]
    InvalidNetwork { found: Network, expected: Network },

    #[error("Block size too large: {size} bytes")]
    BlockSizeTooLarge { size: u32 },

    #[error("Failed to decode TX data")]
    TxDataDecode(#[source] bitcoin::consensus::encode::Error),

    #[error("Failed to decode block")]
    HeaderDecode(#[source] bitcoin::consensus::encode::Error),

    #[error("Failed to decode uint32")]
    UintDecode(#[source] bitcoin::consensus::encode::Error),

    #[error("Unexpected end of file")]
    UnexpectedEof,

    #[error("Block size mismatch: expected {expected}, got {actual}")]
    BlockSizeMismatch { expected: u32, actual: usize },
}

// A lazily parsed block. We skip parsing all transaction until that's explicitly required.
pub struct ParsedBlock {
    pub header: Header,

    // The raw transaction data
    pub raw_tx_data: Vec<u8>,
}

impl ParsedBlock {
    pub fn parse_tx_data(&self) -> Result<Vec<Transaction>, ParseBlockFileError> {
        Vec::<Transaction>::consensus_decode(&mut self.raw_tx_data.as_slice())
            .map_err(ParseBlockFileError::TxDataDecode)
    }
}

// An XOR reader can read bytes from an underlying reader, and
// apply an XOR key to the bytes.
struct XorReader {
    reader: BufReader<File>,
    xor_key: Option<XorKey>,

    offset: usize,
}

impl bitcoin::io::Read for XorReader {
    fn read(&mut self, buf: &mut [u8]) -> bitcoin::io::Result<usize> {
        // Read data from the underlying reader
        let bytes_read = std::io::Read::read(&mut self.reader, buf)?;

        // Apply the XOR pattern if we have a key
        if let Some(xor_key) = self.xor_key {
            for i in 0..bytes_read {
                buf[i] ^= xor_key[(self.offset + i) % xor_key.len()];
            }
        }

        self.offset += bytes_read;
        Ok(bytes_read)
    }
}

/// Parser for Bitcoin Core block files
pub struct BlockFileParser {
    file_path: PathBuf,
    reader: XorReader,
    network: Network,
}

fn magic_for_network(network: Network) -> [u8; 4] {
    match network {
        Network::Bitcoin => MAINNET_MAGIC,
        Network::Testnet | Network::Testnet4 => TESTNET_MAGIC,
        Network::Signet => SIGNET_MAGIC,
        Network::Regtest => REGTEST_MAGIC,
    }
}

impl BlockFileParser {
    /// Create a new parser for the given block file
    pub fn new(
        xor_key: Option<XorKey>,
        file_path: PathBuf,
        network: Network,
    ) -> Result<Self, std::io::Error> {
        let file = File::open(file_path.clone())?;

        let reader = BufReader::new(file);

        Ok(Self {
            file_path,
            reader: XorReader {
                reader,
                xor_key,
                offset: 0,
            },
            network,
        })
    }

    /// Parse the next block from the file
    pub fn next_block(&mut self) -> Result<Option<ParsedBlock>, ParseBlockFileError> {
        // All blocks are prefixed with the P2P magic bytes
        let mut magic = [0u8; 4];
        match bitcoin::io::Read::read_exact(&mut self.reader, &mut magic) {
            Ok(()) => {}
            Err(e) if e.kind() == bitcoin::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(ParseBlockFileError::Io(e.into())),
        }

        // We reached the end of the file. .dat fields appear to be
        // padded with a bunch of empty data

        // Check if we're at end of file by checking if the magic bytes
        // would be all zeros after un-XORing (if XOR key is present)
        let is_end_of_file = if let Some(xor_key) = self.reader.xor_key {
            // Calculate what these bytes would be if XORed back to original
            let original_bytes: Vec<u8> = magic
                .iter()
                .enumerate()
                .map(|(i, &byte)| {
                    let key_index = (self.reader.offset - 4 + i) % xor_key.len();
                    byte ^ xor_key[key_index]
                })
                .collect();
            original_bytes.iter().all(|&b| b == 0)
        } else {
            magic.iter().all(|b| *b == 0)
        };

        if is_end_of_file {
            return Ok(None);
        }

        let expected_magic = magic_for_network(self.network);
        if magic != expected_magic {
            let known_networks = [
                bitcoin::Network::Bitcoin,
                bitcoin::Network::Regtest,
                bitcoin::Network::Testnet,
                bitcoin::Network::Signet,
            ];

            if let Some(network) = known_networks
                .iter()
                .find(|net| net.magic().to_bytes() == magic)
            {
                return Err(ParseBlockFileError::InvalidNetwork {
                    found: *network,
                    expected: self.network,
                });
            }

            return Err(ParseBlockFileError::InvalidMagic {
                expected: expected_magic,
                found: magic,
            });
        }

        let size = u32::consensus_decode(&mut self.reader)
            .map_err(ParseBlockFileError::UintDecode)? as usize;

        // Read the block header
        let header = Header::consensus_decode(&mut self.reader).unwrap();

        // Skip the rest of the block
        let mut txdata = vec![0; size - 80];
        bitcoin::io::Read::read_exact(&mut self.reader, &mut txdata).unwrap();

        Ok(Some(ParsedBlock {
            header,
            raw_tx_data: txdata,
        }))
    }

    /// Parse all blocks from the file
    pub fn all_blocks(&mut self) -> Result<Vec<ParsedBlock>, ParseAllBlocksError> {
        let mut blocks = Vec::new();

        while let Some(block) = self.next_block().map_err(|e| ParseAllBlocksError {
            file_path: self.file_path.clone(),
            index: blocks.len(),
            source: e,
        })? {
            blocks.push(block);
        }

        Ok(blocks)
    }
}

/// Iterator adapter for parsing blocks one by one
pub struct BlockIterator<'a> {
    parser: &'a mut BlockFileParser,
}

impl<'a> BlockIterator<'a> {
    pub fn new(parser: &'a mut BlockFileParser) -> Self {
        Self { parser }
    }
}

impl<'a> Iterator for BlockIterator<'a> {
    type Item = Result<ParsedBlock, ParseBlockFileError>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.parser.next_block() {
            Ok(Some(block)) => Some(Ok(block)),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        }
    }
}

impl BlockFileParser {
    /// Get an iterator over all blocks in the file
    pub fn iter(&mut self) -> BlockIterator<'_> {
        BlockIterator::new(self)
    }
}

/// Iterator adapter for parsing blocks one by one
pub struct BlockFileIterator<'a> {
    parser: &'a mut BlockDirectoryParser,
}

impl<'a> BlockFileIterator<'a> {
    pub fn new(parser: &'a mut BlockDirectoryParser) -> Self {
        Self { parser }
    }
}

impl<'a> Iterator for BlockFileIterator<'a> {
    type Item = Result<ParsedBlock, ParseBlockFileError>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.parser.next_block() {
            Ok(Some(block)) => Some(Ok(block)),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        }
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum BlockDirectoryParserError {
    #[error("failed to read XOR key at `{path}`")]
    ReadXorKey {
        path: PathBuf,
        source: std::io::Error,
    },
}

pub struct BlockDirectoryParser {
    dir_path: PathBuf,
    xor_key: Option<XorKey>,
    network: Network,

    // The file we're currently reading
    file_index: u32,

    // Current file parser (if any)
    current_parser: Option<BlockFileParser>,
}

impl BlockDirectoryParser {
    pub fn new(dir_path: PathBuf, network: Network) -> Result<Self, BlockDirectoryParserError> {
        let xor_key = std::fs::read(dir_path.join("xor.dat")).map_err(|e| {
            BlockDirectoryParserError::ReadXorKey {
                path: dir_path.join("xor.dat"),
                source: e,
            }
        })?;

        let xor_key: [u8; 8] = xor_key.try_into().expect("xor.dat is not 8 bytes");

        Ok(Self {
            dir_path,
            xor_key: if xor_key.iter().all(|b| *b == 0) {
                None
            } else {
                Some(xor_key)
            },
            network,
            file_index: 0,
            current_parser: None,
        })
    }

    fn next_block(&mut self) -> Result<Option<ParsedBlock>, ParseBlockFileError> {
        loop {
            // If we have a current parser, try to get the next block from it
            if let Some(ref mut parser) = self.current_parser {
                match parser.next_block()? {
                    Some(block) => return Ok(Some(block)),
                    None => {
                        // Current file is exhausted, move to next file
                        self.current_parser = None;
                        self.file_index += 1;
                    }
                }
            } else {
                // No current parser, try to open the next file
                let file_path = self.dir_path.join(format!("blk{:05}.dat", self.file_index));

                // If the file doesn't exist, we're at the end
                if !file_path.exists() {
                    return Ok(None);
                }

                let parser = BlockFileParser::new(self.xor_key, file_path, self.network)
                    .map_err(ParseBlockFileError::Io)?;
                self.current_parser = Some(parser);
            }
        }
    }
}

impl Iterator for BlockDirectoryParser {
    type Item = Result<ParsedBlock, ParseBlockFileError>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.next_block() {
            Ok(Some(block)) => Some(Ok(block)),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn test_parse_real_file_with_xor_key() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("validator/testdata/xor-blk00000.dat");

        let xor_key = [0xf1, 0x64, 0xb4, 0xf3, 0xa7, 0x9d, 0x73, 0x04];

        let mut parser = BlockFileParser::new(Some(xor_key), path, Network::Regtest)
            .expect("failed to create parser");

        let parsed_blocks = parser.all_blocks().expect("failed to parse blocks");
        assert_eq!(parsed_blocks.len(), 6);

        let hashes = parsed_blocks
            .iter()
            .map(|block| block.header.block_hash())
            .collect::<Vec<_>>();

        assert_eq!(hashes.len(), 6);
        assert_eq!(
            hashes.iter().map(|h| h.to_string()).collect::<Vec<_>>(),
            vec![
                "0f9188f13cb7b2c71f2a335e3a4fc328bf5beb436012afca590b1a11466e2206",
                "02979800808c680d35bdf6b78cf8df7eaccffaf8313a2d25ffd671b9ef4feb67",
                "1d4461e97d6609d137315665a018cd942f9be13fe35a0829f0cbf937ea16d3f4",
                "5955b41bce9494bf958880041a21395488dbee65b36f8dd34265b1b00f34ebec",
                "474c5755a8cca48b7e95d1f2b9eb9ffe6625d78a79dbe817e5750891b02affbe",
                "4c13d3fc0bda9b39900e3b41956900c6899f42e7dddcc1150bafdfcc17ace409"
            ]
        );

        let tx_data = parsed_blocks[0]
            .parse_tx_data()
            .expect("failed to parse tx data");

        assert_eq!(tx_data.len(), 1);
        assert_eq!(
            tx_data[0].compute_txid().to_string(),
            "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b"
        );
    }

    #[test]
    fn test_parse_real_file_without_xor_key() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("validator/testdata/plain-blk00000.dat");

        let xor_key = None;

        let mut parser =
            BlockFileParser::new(xor_key, path, Network::Regtest).expect("failed to create parser");

        let parsed_blocks = parser.all_blocks().expect("failed to parse blocks");
        assert_eq!(parsed_blocks.len(), 6);

        let hashes = parsed_blocks
            .iter()
            .map(|block| block.header.block_hash())
            .collect::<Vec<_>>();

        assert_eq!(hashes.len(), 6);
        assert_eq!(
            hashes.iter().map(|h| h.to_string()).collect::<Vec<_>>(),
            vec![
                "0f9188f13cb7b2c71f2a335e3a4fc328bf5beb436012afca590b1a11466e2206",
                "40ba34e781864803552065d29f0ab8cb434e7e4d14fe55a15e3f2965273de6a3",
                "2d20ac17fb6bb0bb383cb6b61f6435c9d1e7a00ff52d82ad4dda5fb4a4f11b3c",
                "542012e6476804c6d6f66305f412c592c3ee96526b16dfd3eee4dc1dfaf05ffd",
                "5c492bbb5891d85630ae6b38e9118ef7f0e32aee28ba3844d36d61426ebce012",
                "490672f9cc9de1a211be619095397c2ca2ea899ae2b7b186fe459ebcf7851f3b"
            ]
        );

        let tx_data = parsed_blocks[0]
            .parse_tx_data()
            .expect("failed to parse tx data");
        assert_eq!(tx_data.len(), 1);
        assert_eq!(
            tx_data[0].compute_txid().to_string(),
            "4a5e1e4baab89f3a32518a88c31bc87f618f76673e2cc77ab2127b7afdeda33b"
        );
    }
}
