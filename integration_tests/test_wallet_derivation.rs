//! The wallet derives where BIP 84 says it should.
//!
//! Mainnet wallets derive under SLIP-44 coin type `0`; every test network
//! shares `1`. The harness only runs test networks, so the mainnet coin type
//! is requested through the hidden `--wallet-derivation-coin-type`, and the
//! first receive address of the BIP 84 test mnemonic is compared against the
//! published vector. The encodings differ between networks, the witness
//! program does not.

use std::str::FromStr as _;

use anyhow::Context as _;
use bip300301_enforcer_lib::proto::mainchain::CreateNewAddressRequest;
use futures::channel::mpsc;

use crate::{
    setup::{Mode, Network, PreSetup, SetupOpts, enforcer_regtest_wallet_dir},
    util::BinPaths,
};

pub const TEST_NAME: &str = "wallet_derivation";

/// The mnemonic BIP 84's test vectors derive from.
const BIP84_MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon \
                              abandon abandon abandon abandon abandon about";

/// BIP 84's first receive address for [`BIP84_MNEMONIC`] on mainnet.
const BIP84_FIRST_RECEIVE: &str = "bc1qcr8te4kr609gcawutmrza0j4xv80jy8z306fyu";

pub async fn test_wallet_derivation(bin_paths: BinPaths) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let pre_setup = PreSetup::new(bin_paths, Network::Regtest)?;

    let wallet_dir = enforcer_regtest_wallet_dir(&pre_setup.directories.enforcer_dir);
    std::fs::create_dir_all(&wallet_dir)?;
    std::fs::write(
        wallet_dir.join("seed.json"),
        serde_json::json!({
            "version": 1,
            "created_at": { "secs_since_epoch": 0, "nanos_since_epoch": 0 },
            "seed": { "type": "plaintext", "mnemonic": BIP84_MNEMONIC },
        })
        .to_string(),
    )?;

    let setup_opts: SetupOpts = SetupOpts {
        enforcer_args: vec!["--wallet-derivation-coin-type=0".to_owned()],
        ..Default::default()
    };
    let post_setup = pre_setup
        .setup(Mode::Mempool, setup_opts, res_tx)
        .await
        .context("enforcer must boot on the BIP 84 test mnemonic")?;

    let address = post_setup
        .wallet_service_client
        .create_new_address(CreateNewAddressRequest::default())
        .await?
        .into_owned()
        .address;
    let derived = bitcoin::Address::from_str(&address)?.assume_checked();
    let expected = bitcoin::Address::from_str(BIP84_FIRST_RECEIVE)?.assume_checked();
    anyhow::ensure!(
        derived.script_pubkey() == expected.script_pubkey(),
        "the first receive address under coin type 0 must match BIP 84's vector \
         {BIP84_FIRST_RECEIVE}, got: {address}",
    );

    drop(post_setup);
    Ok(())
}
