//! Starting on a wallet whose persisted descriptor is not the one this build
//! derives.
//!
//! The BIP 84 coin type used to be the test-network `1'` on every network,
//! mainnet included. Making it network-aware changed what a mainnet enforcer
//! derives, and BDK will not open a wallet under a descriptor other than the
//! one it was persisted with, so every mainnet wallet from an older build
//! stopped loading. One test per obligation that follows: a wallet on the
//! legacy derivation must still open and keep deriving where its funds are,
//! and one on neither derivation must still be refused -- saying what
//! differs, not just that a load failed.
//!
//! Neither is reachable on the networks the harness runs, where the coin type
//! never changed, so both enforcers here are started with the hidden
//! `--wallet-derivation-coin-type`. It decides which descriptor the enforcer
//! asks for, which is the whole of what the upgrade changed.

use anyhow::Context as _;
use bip300301_enforcer_lib::proto::mainchain::{GetBalanceRequest, GetInfoRequest};
use futures::channel::mpsc;

use crate::{
    integration_test::{fund_enforcer, wait_for_wallet_sync},
    setup::{DummySidechain, Mode, Network, PostSetup, PreSetup, SetupOpts, read_enforcer_log},
    util::BinPaths,
};

pub const LEGACY_TEST_NAME: &str = "wallet_legacy_descriptor";
pub const FOREIGN_TEST_NAME: &str = "wallet_foreign_descriptor";

/// Emitted (warn) when the wallet only opens under the legacy descriptor.
/// Nothing else tells the fallback apart from an ordinary load.
const LEGACY_FALLBACK_LOG: &str = "Loaded existing BDK wallet under the legacy coin type";

/// What a mainnet build derives under. Regtest's own is the legacy one, so
/// starting with this against an ordinary regtest wallet is the upgrade.
const MAINNET_COIN_TYPE: u32 = 0;

/// Neither what a network implies nor the legacy value: a wallet under it
/// belongs to no build the fallback knows about.
const FOREIGN_COIN_TYPE: u32 = 7;

/// The account paths as they appear in the public descriptors the enforcer
/// reports, where the path is the key's origin and `/0/*` follows the xpub.
const LEGACY_ORIGIN: &str = "/84'/1'/0']";
const FOREIGN_ORIGIN: &str = "/84'/7'/0']";

fn coin_type_args(coin_type: u32) -> Vec<String> {
    vec![format!("--wallet-derivation-coin-type={coin_type}")]
}

/// What must survive the restart: the descriptor, and the coins it sees.
#[derive(Debug, Eq, PartialEq)]
struct WalletState {
    external_descriptor: String,
    unspent_output_count: u32,
    confirmed_sats: u64,
}

async fn wallet_state(post_setup: &mut PostSetup) -> anyhow::Result<WalletState> {
    let info = post_setup
        .wallet_service_client
        .get_info(GetInfoRequest::default())
        .await?
        .into_owned();
    let external_descriptor = info
        .descriptors
        .get("external")
        .ok_or_else(|| {
            anyhow::anyhow!(
                "wallet reported no external descriptor, only: {:?}",
                info.descriptors
            )
        })?
        .clone();
    let balance = post_setup
        .wallet_service_client
        .get_balance(GetBalanceRequest::default())
        .await?
        .into_owned();
    Ok(WalletState {
        external_descriptor,
        unspent_output_count: info.unspent_output_count,
        confirmed_sats: balance.confirmed_sats,
    })
}

/// A miette report is a box-drawn tree wrapped to a fixed width, and it wraps
/// *inside* long tokens, so a descriptor arrives in pieces. Two views of it,
/// so that no match depends on where the wrapping landed.
struct Report {
    /// Words in order, tree markers dropped. For matching prose.
    words: String,
    /// The same with spaces removed, for a token wrapping may have broken.
    packed: String,
}

fn parse_report(raw: &str) -> Report {
    let words = raw
        .split_whitespace()
        .filter(|token| token.chars().any(char::is_alphanumeric))
        .collect::<Vec<_>>()
        .join(" ");
    let packed = words.split_whitespace().collect();
    Report { words, packed }
}

/// A wallet on the legacy derivation must open under a build that derives a
/// different one, and go on deriving where its coins are.
pub async fn test_wallet_legacy_descriptor(bin_paths: BinPaths) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let pre_setup = PreSetup::new(bin_paths.clone(), Network::Regtest)?;
    let setup_opts: SetupOpts = SetupOpts::default();
    let mut post_setup = pre_setup
        .setup(Mode::Mempool, setup_opts, res_tx.clone())
        .await?;

    // Regtest's coin type is the legacy one, so an ordinary boot leaves
    // exactly the wallet an old build would have.
    let () = fund_enforcer::<DummySidechain>(&mut post_setup).await?;
    let () = wait_for_wallet_sync(&mut post_setup).await?;
    let before = wallet_state(&mut post_setup).await?;
    anyhow::ensure!(
        before.external_descriptor.contains(LEGACY_ORIGIN),
        "the wallet to be migrated must sit on the legacy derivation, got: {}",
        before.external_descriptor,
    );
    anyhow::ensure!(
        before.confirmed_sats > 0 && before.unspent_output_count > 0,
        "the wallet must hold coins, or losing sight of them would not show up here: {before:?}",
    );

    // The upgrade: same data dir, under a build asking for a descriptor the
    // database does not hold.
    tracing::info!("restarting enforcer under the mainnet coin type");
    post_setup
        .restart_enforcer(
            &bin_paths,
            coin_type_args(MAINNET_COIN_TYPE),
            res_tx.clone(),
        )
        .await
        .context(
            "the enforcer must start on a wallet persisted under the legacy \
             coin type, not exit with a descriptor mismatch",
        )?;
    let () = wait_for_wallet_sync(&mut post_setup).await?;

    let after = wallet_state(&mut post_setup).await?;
    anyhow::ensure!(
        after == before,
        "the wallet must be unchanged across the upgrade.\n before: {before:?}\n after:  {after:?}",
    );

    // Prove the fallback is what got us here, not an override the restart
    // quietly ignored.
    let enforcer_log = read_enforcer_log(&post_setup.directories.enforcer_dir)?;
    anyhow::ensure!(
        enforcer_log.contains(LEGACY_FALLBACK_LOG),
        "expected the wallet to report opening under the legacy coin type, but \
         {LEGACY_FALLBACK_LOG:?} never appeared in the enforcer log. Did the \
         enforcer come up without --wallet-derivation-coin-type={MAINNET_COIN_TYPE}, \
         leaving it deriving what was already persisted?",
    );

    drop(post_setup);
    Ok(())
}

/// A wallet on neither derivation must still stop the enforcer, reporting
/// what differs rather than just that a load failed.
pub async fn test_wallet_foreign_descriptor(bin_paths: BinPaths) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let pre_setup = PreSetup::new(bin_paths.clone(), Network::Regtest)?;
    let setup_opts: SetupOpts = SetupOpts {
        enforcer_args: coin_type_args(FOREIGN_COIN_TYPE),
        ..Default::default()
    };
    let mut post_setup = pre_setup
        .setup(Mode::Mempool, setup_opts, res_tx.clone())
        .await?;
    let foreign = wallet_state(&mut post_setup).await?;
    anyhow::ensure!(
        foreign.external_descriptor.contains(FOREIGN_ORIGIN),
        "this wallet must sit on a derivation no build knows, or the fallback \
         would rightly adopt it: {}",
        foreign.external_descriptor,
    );

    // Restart without the override: regtest wants the legacy descriptor, so
    // the fallback has nowhere else to look.
    tracing::info!("restarting enforcer against a foreign wallet");
    let restarted = post_setup
        .restart_enforcer(&bin_paths, Vec::<String>::new(), res_tx.clone())
        .await;
    anyhow::ensure!(
        restarted.is_err(),
        "the enforcer must not start on a wallet it cannot account for",
    );

    // The failure is a startup error, so it is on stderr rather than in the
    // rolling log: `main` returns it before anything logs it.
    let stderr_path = post_setup.directories.enforcer_dir.join("stderr.txt");
    let raw = std::fs::read_to_string(&stderr_path)
        .with_context(|| format!("reading {}", stderr_path.display()))?;
    let report = parse_report(&raw);
    // The actionable diagnosis, not the opaque "failed to load wallet" this
    // used to report, and what actually differs.
    for expected in ["wallet data mismatch", "Descriptor mismatch"] {
        anyhow::ensure!(
            report.words.contains(expected),
            "the startup failure must mention {expected:?}, got: {raw}",
        );
    }
    // Both paths: which wallet would open under this build, and which one is
    // actually there.
    for expected in [LEGACY_ORIGIN, FOREIGN_ORIGIN] {
        anyhow::ensure!(
            report.packed.contains(expected),
            "the startup failure must name the {expected:?} account path, got: {raw}",
        );
    }

    drop(post_setup);
    Ok(())
}
