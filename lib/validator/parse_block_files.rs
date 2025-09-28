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

use std::{
    fs::File,
    io::{self, BufReader},
    path::{Path, PathBuf},
};

use bitcoin::{
    BlockHash, CompactTarget, Network, Transaction, block::Header, consensus::Decodable,
    hashes::Hash,
};
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

    // Byte offset within the block file
    pub offset: usize,
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

    /// Set the offset of the reader.
    pub fn set_offset(&mut self, offset: usize) {
        self.reader.offset = offset;
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
            offset: self.reader.offset,
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

/// Bitcoin Core's VarInt format (MSB base-128 encoding) used in CDiskBlockIndex
/// This is NOT the same as VarInt::consensus_decode
fn read_varint<R: std::io::Read>(reader: &mut R) -> std::io::Result<u64> {
    let mut n = 0u64;

    loop {
        let mut buf = [0u8; 1];
        reader.read_exact(&mut buf)?;
        let ch_data = buf[0];

        // Check for overflow before shifting
        if n > (u64::MAX >> 7) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "VarInt size too large",
            ));
        }

        // Extract the 7-bit payload
        n = (n << 7) | ((ch_data & 0x7F) as u64);

        // Check continuation bit (MSB)
        if (ch_data & 0x80) != 0 {
            // More bytes to read - check for overflow before incrementing
            if n == u64::MAX {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "VarInt size too large",
                ));
            }
            n += 1;
        } else {
            // This was the last byte
            return Ok(n);
        }
    }
}

fn read_uint256<R: std::io::Read>(reader: &mut R) -> std::io::Result<[u8; 32]> {
    let mut hash = [0u8; 32];
    reader.read_exact(&mut hash)?;
    Ok(hash)
}

fn read_uint32<R: std::io::Read>(reader: &mut R) -> std::io::Result<u32> {
    let mut buf = [0u8; 4];
    reader.read_exact(&mut buf)?;
    Ok(u32::from_le_bytes(buf))
}

// Block status flags from Bitcoin Core
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockStatus(pub u32);

impl BlockStatus {
    // Validation levels
    pub const VALID_UNKNOWN: u32 = 0;
    pub const VALID_RESERVED: u32 = 1;
    pub const VALID_TREE: u32 = 2;
    pub const VALID_TRANSACTIONS: u32 = 3;
    pub const VALID_CHAIN: u32 = 4;
    pub const VALID_SCRIPTS: u32 = 5;
    pub const VALID_MASK: u32 = 7;

    // Data availability flags
    pub const HAVE_DATA: u32 = 8;
    pub const HAVE_UNDO: u32 = 16;

    // Failure flags
    pub const FAILED_VALID: u32 = 32;
    pub const FAILED_CHILD: u32 = 64;

    // Other flags
    pub const OPT_WITNESS: u32 = 128;

    pub fn new(value: u32) -> Self {
        Self(value)
    }

    pub fn validation_level(&self) -> u32 {
        self.0 & Self::VALID_MASK
    }

    pub fn has_data(&self) -> bool {
        (self.0 & Self::HAVE_DATA) != 0
    }

    pub fn has_undo(&self) -> bool {
        (self.0 & Self::HAVE_UNDO) != 0
    }

    pub fn is_valid(&self) -> bool {
        self.validation_level() >= Self::VALID_TREE
    }

    pub fn has_witness(&self) -> bool {
        (self.0 & Self::OPT_WITNESS) != 0
    }

    pub fn is_failed(&self) -> bool {
        (self.0 & (Self::FAILED_VALID | Self::FAILED_CHILD)) != 0
    }
}

// Bitcoin Core CDiskBlockIndex structure
// Represents an entry into the `blocks/index` LevelDB.
// Keys are block hashes, values are raw CDiskBlockIndex values.
// https://github.com/bitcoin/bitcoin/blob/3c5d1a468199722da620f1f3d8ae3319980a46d5/src/chain.h#L354
#[derive(Debug, Clone, Copy)]
pub struct CDiskBlockIndex {
    // Serialization version (historically unused)
    pub version: u64,

    // Blockchain position
    pub height: u64,
    pub status: BlockStatus,
    pub tx_count: u64,

    // File storage info (optional based on status)
    pub file_number: Option<u64>,
    pub data_pos: Option<u64>,
    pub undo_pos: Option<u64>,

    // Block header fields
    pub block_version: u32,
    pub prev_block_hash: BlockHash,
    pub merkle_root: BlockHash,
    pub timestamp: u32,
    pub difficulty_target: CompactTarget,
    pub nonce: u32,
}

impl CDiskBlockIndex {
    /// Deserialize from byte slice
    pub fn deserialize(reader: &mut impl std::io::Read) -> std::io::Result<Self> {
        // historically unused, should be hard-coded to 259900
        let version = read_varint(reader)?;

        let height = read_varint(reader)?;

        let status_raw = read_varint(reader)?;
        let status = BlockStatus::new(status_raw as u32);

        let tx_count = read_varint(reader)?;

        // Read file/position data if available
        let mut file_number = None;
        let mut data_pos = None;
        let mut undo_pos = None;

        if status.has_data() || status.has_undo() {
            file_number = Some(read_varint(reader)?);
        }

        if status.has_data() {
            data_pos = Some(read_varint(reader)?);
        }

        if status.has_undo() {
            undo_pos = Some(read_varint(reader)?);
        }

        // Read block header fields
        let block_version = read_uint32(reader)?;
        let prev_block_hash = read_uint256(reader)?;
        let merkle_root = read_uint256(reader)?;
        let timestamp = read_uint32(reader)?;
        let difficulty_target = read_uint32(reader).map(CompactTarget::from_consensus)?;
        let nonce = read_uint32(reader)?;

        Ok(CDiskBlockIndex {
            version,
            height,
            status,
            tx_count,
            file_number,
            data_pos,
            undo_pos,
            block_version,
            prev_block_hash: BlockHash::from_byte_array(prev_block_hash),
            merkle_root: BlockHash::from_byte_array(merkle_root),
            timestamp,
            difficulty_target,
            nonce,
        })
    }
}

#[derive(Debug, Diagnostic, Error)]
pub enum FetchBlockIndexError {
    #[error("CDiskBlockIndex not found for key `{hash}`")]
    NotFound { hash: BlockHash },

    #[error("failed to deserialize CDiskBlockIndex for key `{hash}`")]
    Deserialize {
        hash: BlockHash,
        source: std::io::Error,
    },

    #[error("failed to open LevelDB database at `{path}`")]
    Open {
        path: PathBuf,
        source: rusty_leveldb::Status,
    },

    #[error("failed to copy block index LevelDB at `{src}` to `{dst}`")]
    Copy {
        src: PathBuf,
        dst: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to close LevelDB database at `{path}`")]
    Close {
        path: PathBuf,
        source: rusty_leveldb::Status,
    },

    #[error("failed to cleanup block index LevelDB at `{path}`")]
    Cleanup {
        path: PathBuf,
        source: std::io::Error,
    },
}

fn fetch_block_index_inner(
    db: &mut rusty_leveldb::DB,
    hash: BlockHash,
) -> Result<CDiskBlockIndex, FetchBlockIndexError> {
    // Keys are prefixed with 'b', and then the raw value of the hash.
    // Note that this is the reverse byte order of how hashes are
    // displayed when formatted as a string!
    let mut key = vec![b'b'];
    key.extend(hash.to_byte_array());

    let Some(value) = db.get(&key) else {
        return Err(FetchBlockIndexError::NotFound { hash });
    };

    CDiskBlockIndex::deserialize(&mut value.as_ref())
        .map_err(|err| FetchBlockIndexError::Deserialize { hash, source: err })
}

fn copy_dir(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> io::Result<()> {
    std::fs::create_dir_all(dst.as_ref())?;

    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;

        // We only expected plain files in the LevelDB directory
        if !ty.is_file() {
            continue;
        }

        std::fs::copy(entry.path(), dst.as_ref().join(entry.file_name()))?;
    }
    Ok(())
}

// LevelDB only supports a single reader. It uses a locking mechanism to ensure this,
// and it does not appear like it's at all possible to get around this. We therefore
// do this in a slightly hacky way: we copy over the entire database directory to a
// tmp directory, wipe the lock file, read what we need and then delete the entire
// thing.
//
// A fully synced mainchain is per september 2025 around 144 megabytes. Should in other
// words be fine to do, considering this is an operation we don't expect to have to do
// often.
pub fn fetch_block_index<P>(
    path: P,
    hash: BlockHash,
) -> Result<CDiskBlockIndex, FetchBlockIndexError>
where
    P: AsRef<Path>,
{
    let path = path.as_ref().to_path_buf();

    let clone_path = std::env::temp_dir().join(format!(
        "block-index-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    ));

    tracing::debug!(
        "cloning block index LevelDB: {} -> {}",
        path.display(),
        clone_path.display()
    );

    copy_dir(&path, &clone_path).map_err(|e| FetchBlockIndexError::Copy {
        src: path.clone(),
        dst: clone_path.clone(),
        source: e,
    })?;

    tracing::debug!("opening block index LevelDB: {}", clone_path.display());

    let opts = rusty_leveldb::Options::default();
    let mut db = rusty_leveldb::DB::open(clone_path.clone(), opts).map_err(|e| {
        FetchBlockIndexError::Open {
            path: path.clone(),
            source: e.clone(),
        }
    })?;

    let res = fetch_block_index_inner(&mut db, hash);

    db.close().map_err(|e| FetchBlockIndexError::Close {
        path: path.clone(),
        source: e,
    })?;

    // We should cleanup up after ourselves, to not eat more of the users
    // resources than necessary
    tracing::debug!(
        "cleaning up block index LevelDB: {}",
        clone_path.clone().display()
    );
    std::fs::remove_dir_all(clone_path.clone()).map_err(|e| FetchBlockIndexError::Cleanup {
        path: clone_path.clone(),
        source: e,
    })?;

    res
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use bitcoin::Target;

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

    // This block is not fully downloaded and processed by the machine syncing Core which it originated from,
    // which is why the TX count and file pos information is incorrect.
    #[test]
    fn test_parse_cdisk_block_index_not_fully_downloaded() {
        // Block 293374 0000000000000000485620AA56D1A61C14F564233336772F3211F987FC390000
        // Reverse the endianness into 000039FC87F911322F7736332364F5141CA6D156AA2056480000000000000000
        // Add 'b' in hex as a prefix , and we have our key
        // $ ./ldb --db=$HOME/.bitcoin/blocks/index get --value_hex --key_hex 0x62000039FC87F911322F7736332364F5141CA6D156AA2056480000000000000000
        // 0x8EED3C90F27E020002000000ED2223DDCBBA95C7CCE6CC31CE5CA7463BE8EC7720232ACB0000000000000000F9BF954DB4429571D2B379E49C624C3C9D0D8D9FCB91F8123F22E2558BA8DAB5003E395399DB0019146AE23F
        const RAW_BLOCK_INDEX: &str = "8EED3C90F27E020002000000ED2223DDCBBA95C7CCE6CC31CE5CA7463BE8EC7720232ACB0000000000000000F9BF954DB4429571D2B379E49C624C3C9D0D8D9FCB91F8123F22E2558BA8DAB5003E395399DB0019146AE23F";

        let data = hex::decode(RAW_BLOCK_INDEX).unwrap();
        let block_index = CDiskBlockIndex::deserialize(&mut data.as_slice())
            .expect("failed to deserialize CDiskBlockIndex");

        assert_eq!(block_index.version, 259_900);
        assert_eq!(block_index.height, 293374);
        assert_eq!(block_index.timestamp, 1396260352);
        assert_eq!(
            block_index.prev_block_hash.to_string(),
            "0000000000000000cb2a232077ece83b46a75cce31cce6ccc795bacbdd2322ed"
        );
        assert_eq!(
            block_index.merkle_root.to_string(),
            "b5daa88b55e2223f12f891cb9f8d0d9d3c4c629ce479b3d2719542b44d95bff9"
        );
        assert_eq!(block_index.nonce, 0x3fe26a14);
        assert_eq!(
            Target::from_compact(block_index.difficulty_target).difficulty_float(),
            5006860589.2054
        );

        assert!(block_index.file_number.is_none());
        assert!(block_index.data_pos.is_none());
        assert!(block_index.undo_pos.is_none());
        assert_eq!(block_index.tx_count, 0);
    }

    #[test]
    fn test_parse_cdisk_block_index_fully_downloaded() {
        // Block 135401 0000000000000a753b507e1a96d77a89cc8732bd279636bb892d0114dd794496
        // $ ./ldb --db=$HOME/.bitcoin/blocks/index get --value_hex --key_hex 0x62964479dd14012d89bb369627bd3287cc897ad7961a7e503b750a000000000000
        const RAW_BLOCK_INDEX: &str = "8EED3C87A0691D3002ADFE9B3284DCBE5601000000BAE693F577F9977B0F7B183EA9F42F08A64A3EC2F366B1F96C07000000000000FCFC068613CE1D3B756ACF0D09E26D201BE5328727F1D29D171182FFA6BFA64EC0AE174ECFBB0A1AE0DC85D6";

        let data = hex::decode(RAW_BLOCK_INDEX).unwrap();
        let block_index = CDiskBlockIndex::deserialize(&mut data.as_slice())
            .expect("failed to deserialize CDiskBlockIndex");

        assert_eq!(block_index.version, 259_900);
        assert_eq!(block_index.height, 135401);
        assert_eq!(block_index.timestamp, 1310174912);
        assert_eq!(
            block_index.prev_block_hash.to_string(),
            "000000000000076cf9b166f3c23e4aa6082ff4a93e187b0f7b97f977f593e6ba"
        );
        assert_eq!(
            block_index.merkle_root.to_string(),
            "4ea6bfa6ff8211179dd2f1278732e51b206de2090dcf6a753b1dce138606fcfc"
        );
        assert_eq!(block_index.nonce, 0xd685dce0);
        assert_eq!(
            Target::from_compact(block_index.difficulty_target).difficulty_float(),
            1563027.9961162233
        );

        assert_eq!(block_index.file_number.unwrap_or_default(), 2);
        assert_eq!(block_index.data_pos.unwrap_or_default(), 98553394);
        assert_eq!(block_index.undo_pos.unwrap_or_default(), 12017622);
        assert_eq!(block_index.tx_count, 48);
    }
}
