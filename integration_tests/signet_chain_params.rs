//! Everything that determines what the cached signet chain *is*.
//!
//! Kept in its own file, separate from the rest of the harness, so that CI can
//! key its cache of the pre-mined chain on this file's hash: the cache then
//! survives ordinary edits to `setup.rs`, and is invalidated exactly when the
//! chain these values describe would actually differ.
//!
//! Changing anything here means every cached chain (local and CI) is stale.
//! Locally, the harness notices and re-mines; see `cached_signet_chain_dir`.

/// Fixed secret key backing the signet challenge used by the integration
/// tests.
///
/// TEST ONLY -- this is a hardcoded, publicly known key for a throwaway local
/// signet. It must never be used to hold real funds on any network.
///
/// It is fixed rather than random so that the whole signet chain (challenge,
/// network magic, genesis block) is reproducible across runs, which is what
/// lets a pre-mined chain be cached and reused instead of re-mined -- mining
/// signet blocks costs real proof-of-work, and mining the 100 blocks needed
/// for coinbase maturity dominated the entire test suite's runtime.
pub const SIGNET_CHALLENGE_SECRET_KEY: [u8; 32] = [
    0x1b, 0x30, 0x03, 0x01, 0x1b, 0x30, 0x03, 0x01, 0x1b, 0x30, 0x03, 0x01, 0x1b, 0x30, 0x03, 0x01,
    0x1b, 0x30, 0x03, 0x01, 0x1b, 0x30, 0x03, 0x01, 0x1b, 0x30, 0x03, 0x01, 0x1b, 0x30, 0x03, 0x01,
];

/// Number of blocks in the cached signet chain. Must exceed
/// `COINBASE_MATURITY` (100) so that the earliest coinbases are spendable the
/// moment a test starts, which is the entire point of caching it.
pub const SIGNET_CACHED_CHAIN_BLOCKS: u32 = 110;
