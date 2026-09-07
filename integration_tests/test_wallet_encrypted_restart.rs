//! Restarting the enforcer on a wallet whose persisted seed is encrypted.
//!
//! An encrypted wallet is locked at startup: nothing loads it until the
//! UnlockWallet RPC arrives with the password. But the enforcer's tasks are
//! spawned together with the Connect RPC server, and the first task to
//! return an error takes the whole process down with it -- so a wallet half
//! that treated "locked" as a hard error put UnlockWallet permanently out of
//! reach, and made an encrypted wallet impossible to restart at all. Each
//! mode did so in its own place: the tip-chasing task in its wallet sync,
//! the wallet mempool task at its startup gate. The test runs in both.
//!
//! What has to hold: the enforcer comes up on an encrypted wallet, keeps
//! connecting blocks while it is locked, serves UnlockWallet, and the wallet
//! then catches up across the gap it slept through without losing its coins.
//! Catching up means replaying the blocks it missed, not just moving its tip:
//! a payment to the wallet confirmed while it was locked must be found.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use bdk_wallet::bip39::{Language, Mnemonic};
use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    proto::{
        common::ReverseHex,
        mainchain::{
            CreateNewAddressRequest, GetBalanceRequest, ListTransactionsRequest,
            UnlockWalletRequest,
        },
    },
    wallet::mnemonic::{EncryptedMnemonic, KdfParams},
};
use bitcoin::Amount;
use futures::channel::mpsc;

use crate::{
    integration_test::{fund_enforcer, wait_for_validator_tip, wait_for_wallet_sync},
    setup::{
        DummySidechain, Mode, Network, PostSetup, PreSetup, SetupOpts, wait_for_tx_in_mempool,
    },
    util::BinPaths,
};

pub const TEST_NAME: &str = "wallet_encrypted_restart";

const PASSWORD: &str = "correct horse battery staple";

/// Blocks mined between the restart and the unlock. Each one is connected
/// while the wallet is locked, and each one is a chance for the enforcer to
/// die before UnlockWallet can be called.
const BLOCKS_WHILE_LOCKED: u32 = 3;

/// Paid to the wallet by bitcoind's own wallet, and confirmed in one of the
/// blocks connected while the wallet is locked. Chosen so that it cannot be
/// mistaken for a maturing coinbase.
const RECEIVED_WHILE_LOCKED: Amount = Amount::from_sat(1_234_567);

/// The enforcer's seed file, laid out as `app/main.rs` does:
/// `<data-dir>/wallet/<chain>`, with no datadir suffix on plain regtest.
fn seed_path(enforcer_dir: &Path) -> PathBuf {
    enforcer_dir
        .join("wallet")
        .join("regtest")
        .join("seed.json")
}

/// Rewrite `seed.json` in place, replacing the plaintext mnemonic the harness
/// auto-created with that same mnemonic encrypted under [`PASSWORD`]. The
/// mnemonic itself does not change, so the wallet database beside it still
/// opens under the descriptor it was persisted with -- the only difference is
/// that the enforcer can no longer load it unasked.
fn encrypt_persisted_seed(enforcer_dir: &Path) -> anyhow::Result<()> {
    let path = seed_path(enforcer_dir);
    let raw =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let mut seed_file: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    let mnemonic = {
        let Some(plaintext) = seed_file["seed"]["mnemonic"].as_str() else {
            anyhow::bail!("{} holds no plaintext mnemonic: {raw}", path.display());
        };
        Mnemonic::parse_in(Language::English, plaintext)
            .map_err(|err| anyhow::anyhow!("parsing the persisted mnemonic: {err}"))?
    };
    let encrypted = EncryptedMnemonic::encrypt(&mnemonic, PASSWORD, KdfParams::CURRENT)
        .map_err(|err| anyhow::anyhow!("encrypting the persisted mnemonic: {err}"))?;
    let kdf = encrypted.kdf;
    seed_file["seed"] = serde_json::json!({
        "type": "encrypted",
        "initialization_vector": hex::encode(encrypted.initialization_vector),
        "ciphertext_mnemonic": hex::encode(encrypted.ciphertext_mnemonic),
        "key_salt": hex::encode(encrypted.key_salt),
        "kdf": {
            "memory_kib": kdf.memory_kib,
            "iterations": kdf.iterations,
            "parallelism": kdf.parallelism,
        },
    });
    std::fs::write(&path, serde_json::to_string_pretty(&seed_file)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Mine `blocks` blocks to an address of bitcoind's own wallet, so that the
/// enforcer's balance can only be moved by coinbases maturing, never by new
/// ones being paid to it.
async fn mine_blocks(post_setup: &PostSetup, blocks: u32) -> anyhow::Result<()> {
    let address = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getnewaddress", [])
        .run_utf8()
        .await?
        .trim()
        .to_string();
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "generatetoaddress", [blocks.to_string(), address])
        .run_utf8()
        .await?;
    Ok(())
}

/// Pay `amount` to `address` from bitcoind's own wallet. The chain has no fee
/// history for Core's estimator, so the fee rate is given explicitly.
async fn send_from_core(
    post_setup: &PostSetup,
    address: &str,
    amount: Amount,
) -> anyhow::Result<bitcoin::Txid> {
    let txid = post_setup
        .bitcoin_cli
        .command::<_, _, _, _, _>(
            ["-named"],
            "sendtoaddress",
            [
                format!("address={address}"),
                format!("amount={}", amount.to_btc()),
                "fee_rate=2".to_owned(),
            ],
        )
        .run_utf8()
        .await?
        .trim()
        .parse()?;
    let () = wait_for_tx_in_mempool(&post_setup.bitcoin_cli, &txid).await?;
    Ok(txid)
}

pub async fn test_wallet_encrypted_restart(bin_paths: BinPaths, mode: Mode) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let pre_setup = PreSetup::new(bin_paths.clone(), Network::Regtest)?;
    let setup_opts: SetupOpts = SetupOpts::default();
    let mut post_setup = pre_setup.setup(mode, setup_opts, res_tx.clone()).await?;

    let () = fund_enforcer::<DummySidechain>(&mut post_setup).await?;
    let () = wait_for_wallet_sync(&mut post_setup).await?;
    let balance_before = post_setup
        .wallet_service_client
        .get_balance(GetBalanceRequest::default())
        .await?
        .into_owned();
    anyhow::ensure!(
        balance_before.confirmed_sats > 0,
        "the wallet must hold coins, or losing sight of them would not show up \
         here: {balance_before:?}",
    );
    // Revealed now, so that the wallet reloaded after the unlock is watching
    // it when it replays the blocks it missed.
    let receive_address = post_setup
        .wallet_service_client
        .create_new_address(CreateNewAddressRequest::default())
        .await?
        .into_owned()
        .address;

    // Encrypt while the enforcer is down, so what it starts on afterwards is
    // indistinguishable from a wallet an operator created encrypted to begin
    // with.
    post_setup.kill_enforcer().await?;
    let () = encrypt_persisted_seed(&post_setup.directories.enforcer_dir)?;

    tracing::info!("restarting enforcer on an encrypted (locked) wallet");
    post_setup
        .restart_enforcer(&bin_paths, Vec::<String>::new(), res_tx.clone())
        .await
        .context(
            "the enforcer must come up on an encrypted wallet and serve gRPC, \
             not exit before UnlockWallet can be reached",
        )?;

    // Blocks connected while the wallet is locked must advance the validator,
    // not kill the task connecting them. One of them carries a payment to the
    // wallet, which is in the mempool while locked (where the mempool-backed
    // wallet task sees it first) and then confirmed while still locked.
    let () = mine_blocks(&post_setup, 1).await?;
    let received_txid =
        send_from_core(&post_setup, &receive_address, RECEIVED_WHILE_LOCKED).await?;
    let () = mine_blocks(&post_setup, BLOCKS_WHILE_LOCKED - 1).await?;
    wait_for_validator_tip(&post_setup)
        .await
        .context("the enforcer must keep connecting blocks while its wallet is locked")?;

    tracing::info!("unlocking the wallet");
    let _unlocked = post_setup
        .wallet_service_client
        .unlock_wallet(UnlockWalletRequest {
            password: PASSWORD.to_owned(),
        })
        .await
        .context("UnlockWallet must be reachable on a restarted encrypted wallet")?;

    // The periodic wallet sync is disabled in this harness, so the wallet
    // closes the gap it slept through on the next connected block.
    let () = mine_blocks(&post_setup, 1).await?;
    wait_for_wallet_sync(&mut post_setup)
        .await
        .context("the unlocked wallet must catch up across the blocks it stayed locked for")?;

    let balance_after = post_setup
        .wallet_service_client
        .get_balance(GetBalanceRequest::default())
        .await?
        .into_owned();
    anyhow::ensure!(
        balance_after.confirmed_sats
            >= balance_before.confirmed_sats + RECEIVED_WHILE_LOCKED.to_sat(),
        "the wallet must still see its coins after the encrypted restart, plus \
         what it was paid while locked.\n \
         before: {balance_before:?}\n after:  {balance_after:?}",
    );

    // The balance alone could be explained by coinbases maturing. The
    // payment itself must be there, and confirmed: the wallet replayed the
    // block that carried it, rather than just moving its tip past it.
    let transactions = post_setup
        .wallet_service_client
        .list_transactions(ListTransactionsRequest::default())
        .await?
        .into_owned()
        .transactions;
    let received = transactions
        .iter()
        .find(|tx| tx.txid.clone().into_option() == Some(ReverseHex::encode(&received_txid)))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "the payment confirmed while the wallet was locked must be found after the \
                 unlock: {received_txid} is not among the wallet's transactions"
            )
        })?;
    anyhow::ensure!(
        received.confirmation_info.clone().into_option().is_some(),
        "the payment must be seen as confirmed, not merely unconfirmed: {received:?}",
    );
    anyhow::ensure!(
        received.received_sats == RECEIVED_WHILE_LOCKED.to_sat(),
        "the payment must be credited in full: {received:?}",
    );

    drop(post_setup);
    Ok(())
}
