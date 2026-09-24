//! Contends for the wallet lock from every direction at once: gRPC reads and
//! writes, full scans (an upgradable read held across chain-source I/O, then
//! upgraded), block connects and mempool-driven applies. Every call must
//! finish, and afterwards the wallet must agree with a fresh full scan.

use std::{
    collections::HashSet,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering::SeqCst},
    },
    time::{Duration, Instant},
};

use bip300301_enforcer_lib::{
    bins::CommandExt as _,
    proto::{
        self,
        mainchain::{
            CreateNewAddressRequest, FullScanRequest, GetBalanceRequest, ListTransactionsRequest,
            SendTransactionRequest, SendTransactionResponse, WalletTransaction,
        },
    },
};
use bitcoin::Txid;

use crate::{
    integration_test::{fund_enforcer, wait_for_electrs_tip, wait_for_wallet_sync},
    setup::{DummySidechain, PostSetup},
};

pub const TEST_NAME: &str = "wallet_lock_contention";

const LOAD_DURATION: Duration = Duration::from_secs(45);

/// Generous, since a full scan holds the wallet across chain-source I/O. It
/// turns a deadlock into a failure.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

async fn call<T>(call: impl Future<Output = anyhow::Result<T>>) -> anyhow::Result<T> {
    tokio::time::timeout(CALL_TIMEOUT, call)
        .await
        .map_err(|_| anyhow::anyhow!("wallet call did not finish within {CALL_TIMEOUT:?}"))?
}

async fn mine(post_setup: &PostSetup, address: &str) -> anyhow::Result<()> {
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "generatetoaddress",
            ["1".to_owned(), address.to_owned()],
        )
        .run_utf8()
        .await?;
    Ok(())
}

async fn balance(post_setup: &PostSetup) -> anyhow::Result<(u64, u64)> {
    let balance = post_setup
        .wallet_service_client
        .get_balance(GetBalanceRequest::default())
        .await?
        .into_owned();
    Ok((balance.confirmed_sats, balance.pending_sats))
}

async fn wallet_txids(post_setup: &PostSetup) -> anyhow::Result<HashSet<Txid>> {
    post_setup
        .wallet_service_client
        .list_transactions(ListTransactionsRequest::default())
        .await?
        .into_owned()
        .transactions
        .into_iter()
        .map(|tx| {
            Ok(tx
                .txid
                .into_option()
                .ok_or_else(|| proto::Error::missing_field::<WalletTransaction>("txid"))?
                .decode::<WalletTransaction, Txid>("txid")?)
        })
        .collect()
}

pub async fn test_wallet_lock_contention(mut post_setup: PostSetup) -> anyhow::Result<()> {
    fund_enforcer::<DummySidechain>(&mut post_setup).await?;
    let miner_address = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getnewaddress", [])
        .run_utf8()
        .await?
        .trim()
        .to_owned();
    // Mature the funding coinbases, so there is something to send
    post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>(
            [],
            "generatetoaddress",
            ["110".to_owned(), miner_address.clone()],
        )
        .run_utf8()
        .await?;
    wait_for_wallet_sync(&mut post_setup).await?;
    wait_for_electrs_tip(&post_setup).await?;

    let deadline = Instant::now() + LOAD_DURATION;
    let sent_txids = Arc::new(Mutex::new(Vec::new()));
    let scans_completed = Arc::new(AtomicUsize::new(0));
    let mut workers = tokio::task::JoinSet::<anyhow::Result<()>>::new();
    let client = &post_setup.wallet_service_client;

    for _ in 0..2 {
        let client = client.clone();
        workers.spawn(async move {
            while Instant::now() < deadline {
                call(async { Ok(client.get_balance(GetBalanceRequest::default()).await?) }).await?;
                call(async {
                    Ok(client
                        .list_transactions(ListTransactionsRequest::default())
                        .await?)
                })
                .await?;
            }
            Ok(())
        });
    }

    let writer = client.clone();
    workers.spawn(async move {
        while Instant::now() < deadline {
            call(async {
                Ok(writer
                    .create_new_address(CreateNewAddressRequest::default())
                    .await?)
            })
            .await?;
        }
        Ok(())
    });

    // A write to build the transaction, a read to sign it and a write to
    // apply it, plus the mempool hook's own apply
    let sender = client.clone();
    let sent = sent_txids.clone();
    workers.spawn(async move {
        while Instant::now() < deadline {
            let address = sender
                .create_new_address(CreateNewAddressRequest::default())
                .await?
                .into_owned()
                .address;
            let txid = call(async {
                Ok(sender
                    .send_transaction(SendTransactionRequest {
                        destinations: [(address, 10_000)].into_iter().collect(),
                        ..Default::default()
                    })
                    .await?
                    .into_owned()
                    .txid
                    .into_option()
                    .ok_or_else(|| proto::Error::missing_field::<SendTransactionResponse>("txid"))?
                    .decode::<SendTransactionResponse, Txid>("txid")?)
            })
            .await?;
            sent.lock().unwrap().push(txid);
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        Ok(())
    });

    let scanner = client.clone();
    let scans = scans_completed.clone();
    workers.spawn(async move {
        while Instant::now() < deadline {
            match call(async { Ok(scanner.full_scan(FullScanRequest::default()).await) }).await? {
                Ok(_) => {
                    scans.fetch_add(1, SeqCst);
                }
                // electrs can report a new tip before indexing it, and blocks
                // arrive every few seconds. Not the wallet lock's concern.
                Err(err) if err.code == connectrpc::ErrorCode::Unavailable => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(err) => return Err(err.into()),
            }
        }
        Ok(())
    });

    // Block connects take the wallet write lock
    while Instant::now() < deadline {
        mine(&post_setup, &miner_address).await?;
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    while let Some(res) = workers.join_next().await {
        res??;
    }

    let sent_txids = sent_txids.lock().unwrap().clone();
    let scans_completed = scans_completed.load(SeqCst);
    tracing::info!(sends = sent_txids.len(), scans_completed, "load finished");
    anyhow::ensure!(!sent_txids.is_empty(), "no transaction was sent under load");
    // Every completed scan upgraded its read lock under contention
    anyhow::ensure!(scans_completed > 0, "no full scan completed under load");

    // Confirm everything, and let the wallet and electrs catch up
    mine(&post_setup, &miner_address).await?;
    wait_for_wallet_sync(&mut post_setup).await?;
    wait_for_electrs_tip(&post_setup).await?;

    let balance_before = balance(&post_setup).await?;
    let txids_before = wallet_txids(&post_setup).await?;
    anyhow::ensure!(
        sent_txids.iter().all(|txid| txids_before.contains(txid)),
        "a sent transaction is missing from the wallet"
    );
    post_setup
        .wallet_service_client
        .full_scan(FullScanRequest::default())
        .await?;
    anyhow::ensure!(
        balance(&post_setup).await? == balance_before,
        "balance drifted from a fresh full scan"
    );
    anyhow::ensure!(
        wallet_txids(&post_setup).await? == txids_before,
        "transactions drifted from a fresh full scan"
    );
    Ok(())
}
