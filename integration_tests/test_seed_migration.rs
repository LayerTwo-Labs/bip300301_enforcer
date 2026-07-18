//! Automatic wallet-seed migration out of the pre-split `db.sqlite`.
//!
//! Simulates upgrading a deployed node: the data dir already contains the
//! pre-split wallet DB (`user_version = 7`) with a seed in its `wallet_seeds`
//! table. The enforcer must boot with an initialized wallet backed by
//! `seed.json`, and the legacy seed rows must be gone afterwards.

use anyhow::Context as _;
use bip300301_enforcer_lib::proto::mainchain::{CreateNewAddressRequest, CreateWalletRequest};
use futures::channel::mpsc;

use crate::setup::{Mode, PreSetup, SetupOpts};

const LEGACY_V7_SCHEMA: &str = "
    CREATE TABLE sidechain_proposals
       (sidechain_number INTEGER NOT NULL,
        data_hash BLOB NOT NULL,
        data BLOB NOT NULL,
        UNIQUE(sidechain_number, data_hash));
    CREATE TABLE sidechain_acks
       (number INTEGER NOT NULl,
        data_hash BLOB NOT NULL,
        UNIQUE(number, data_hash));
    CREATE TABLE bundle_proposals
       (sidechain_number INTEGER NOT NULL,
        bundle_hash BLOB NOT NULL,
        bundle_tx BLOB NOT NULL,
        UNIQUE(sidechain_number, bundle_hash));
    CREATE TABLE bundle_acks
       (sidechain_number INTEGER NOT NULL,
        bundle_hash BLOB NOT NULL,
        UNIQUE(sidechain_number, bundle_hash));
    CREATE TABLE bmm_requests
        (sidechain_number INTEGER NOT NULL,
         prev_block_hash BLOB NOT NULL,
         side_block_hash BLOB NOT NULL,
         UNIQUE(sidechain_number, prev_block_hash));
    CREATE TABLE wallet_seeds
        (
         id INTEGER PRIMARY KEY AUTOINCREMENT,
         plaintext_mnemonic TEXT,
         initialization_vector BLOB,
         ciphertext_mnemonic BLOB,
         key_salt BLOB,
         needs_passphrase BOOLEAN NOT NULL DEFAULT FALSE,
         creation_time DATETIME NOT NULL DEFAULT (DATETIME('now'))
        );
    CREATE TABLE bmm_requests_undo
        (block_hash BLOB NOT NULL,
         sidechain_number INTEGER NOT NULL,
         prev_block_hash BLOB NOT NULL,
         side_block_hash BLOB NOT NULL);
    PRAGMA user_version = 7;
";

// The all-zeros BIP39 test vector; any valid mnemonic works here.
const TEST_MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon \
     abandon abandon abandon abandon abandon about";

pub async fn test_seed_migration(setup: PreSetup) -> anyhow::Result<()> {
    // Pre-populate the wallet data dir as a pre-split deployment would have
    // left it. The path matches `app/main.rs`: `<data-dir>/wallet/<chain>`,
    // with no datadir suffix on plain regtest.
    let wallet_dir = setup
        .directories
        .enforcer_dir
        .join("wallet")
        .join("regtest");
    std::fs::create_dir_all(&wallet_dir)?;
    {
        let conn = rusqlite::Connection::open(wallet_dir.join("db.sqlite"))?;
        conn.execute_batch(LEGACY_V7_SCHEMA)?;
        conn.execute(
            "INSERT INTO wallet_seeds (plaintext_mnemonic) VALUES (?)",
            [TEST_MNEMONIC],
        )?;
    }

    let (res_tx, _res_rx) = mpsc::unbounded();
    let setup_opts: SetupOpts = SetupOpts::default();
    let post_setup = setup
        .setup(Mode::Mempool, setup_opts, res_tx)
        .await
        .context("enforcer must boot with the migrated wallet")?;

    // The wallet must be functional with the migrated seed...
    let _address = post_setup
        .wallet_service_client
        .create_new_address(CreateNewAddressRequest::default())
        .await
        .context("wallet must serve requests after the seed migration")?;

    // ...and report as already created, not accept a fresh seed.
    let status = post_setup
        .wallet_service_client
        .create_wallet(CreateWalletRequest::default())
        .await
        .err()
        .ok_or_else(|| anyhow::anyhow!("CreateWallet must fail: a seed already exists"))?;

    anyhow::ensure!(
        status.code == connectrpc::ErrorCode::AlreadyExists,
        "expected AlreadyExists from CreateWallet, got: {status}"
    );

    // The seed now lives in `seed.json`...
    let seed_json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(wallet_dir.join("seed.json"))?)?;
    anyhow::ensure!(
        seed_json["seed"]["mnemonic"] == TEST_MNEMONIC,
        "seed.json must hold the migrated mnemonic, got: {seed_json}"
    );

    // ...and the legacy rows are gone.
    let conn = rusqlite::Connection::open(wallet_dir.join("db.sqlite"))?;
    let seed_rows: i64 =
        conn.query_row("SELECT COUNT(*) FROM wallet_seeds", [], |row| row.get(0))?;
    anyhow::ensure!(seed_rows == 0, "legacy seed rows must be deleted");

    drop(post_setup);
    Ok(())
}
