//! The sync backend the wallet reports in `GetInfo`.
//!
//! Wallet info is served over the API, and the esplora URL an operator
//! configures may carry credentials, so the endpoint reported there has to
//! come back redacted. The electrum endpoint is `host:port`, with each half
//! defaulting independently. The harness configures both backends; which one
//! the wallet syncs through is `--wallet-sync-source`, esplora by default.

use anyhow::Context as _;
use bip300301_enforcer_lib::proto::mainchain::{GetInfoRequest, get_info_response::sync_source};
use futures::channel::mpsc;

use crate::{
    integration_test::{fund_enforcer, wait_for_wallet_sync},
    setup::{DummySidechain, Mode, Network, PostSetup, PreSetup, SetupOpts},
    util::{BinPaths, ESPLORA_PASSWORD, ESPLORA_USER},
};

pub const TEST_NAME: &str = "wallet_sync_endpoint";

async fn reported_sync_source(
    post_setup: &mut PostSetup,
) -> anyhow::Result<(sync_source::Kind, Option<String>)> {
    let info = post_setup
        .wallet_service_client
        .get_info(GetInfoRequest::default())
        .await?
        .into_owned();
    let kind = info
        .sync_source
        .kind
        .as_known()
        .context("the wallet reported a sync source kind this test does not know")?;
    Ok((kind, info.sync_source.endpoint.clone()))
}

pub async fn test_wallet_sync_endpoint(bin_paths: BinPaths) -> anyhow::Result<()> {
    let (res_tx, _res_rx) = mpsc::unbounded();
    let pre_setup = PreSetup::new(bin_paths.clone(), Network::Regtest)?;
    let setup_opts: SetupOpts = SetupOpts::default();
    let mut post_setup = pre_setup
        .setup(Mode::Mempool, setup_opts, res_tx.clone())
        .await?;
    let () = fund_enforcer::<DummySidechain>(&mut post_setup).await?;
    let () = wait_for_wallet_sync(&mut post_setup).await?;

    let electrum_port = post_setup.reserved_ports.electrs_electrum_rpc.port();
    let esplora_port = post_setup.reserved_ports.electrs_electrum_http.port();

    let (kind, endpoint) = reported_sync_source(&mut post_setup).await?;
    let endpoint = endpoint.context("an esplora wallet must report its endpoint")?;
    anyhow::ensure!(
        kind == sync_source::Kind::KIND_ESPLORA,
        "expected the default esplora source, got {kind:?}",
    );
    anyhow::ensure!(
        !endpoint.contains(ESPLORA_PASSWORD),
        "the esplora password must not be reported over the API: {endpoint}",
    );
    anyhow::ensure!(
        endpoint == format!("http://{ESPLORA_USER}:redacted@127.0.0.1:{esplora_port}/"),
        "expected the esplora URL with its password redacted, got: {endpoint}",
    );

    tracing::info!("restarting enforcer to sync over electrum");
    post_setup
        .restart_enforcer(
            &bin_paths,
            ["--wallet-sync-source=electrum"],
            res_tx.clone(),
        )
        .await?;
    let () = wait_for_wallet_sync(&mut post_setup)
        .await
        .context("the wallet must sync over electrum")?;

    let (kind, endpoint) = reported_sync_source(&mut post_setup).await?;
    anyhow::ensure!(
        kind == sync_source::Kind::KIND_ELECTRUM
            && endpoint.as_deref() == Some(&format!("127.0.0.1:{electrum_port}")),
        "expected the electrum endpoint, got {kind:?} at {endpoint:?}",
    );

    drop(post_setup);
    Ok(())
}
