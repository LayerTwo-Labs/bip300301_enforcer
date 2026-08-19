//! The wallet's asynchronous locks, and the order between them.
//!
//! The BDK wallet and its persistence are held across await points.
//! Rather than write the required order down in a comment and hope every
//! future caller reads it, this module owns the locks privately and hands
//! them out only in that order: full scan slot, then wallet, then database.

use std::time::Duration;

use crate::wallet::{
    BdkWallet, Persistence, error,
    util::{RwLockReadGuardSome, RwLockUpgradableReadGuardSome, RwLockWriteGuardSome},
};

mod sealed {
    pub trait Sealed {}
}

/// Evidence that the wallet is held for writing. Implemented for write
/// guards only, so a read guard cannot be used to reach the DB.
pub(in crate::wallet) trait HeldWallet: sealed::Sealed {}

impl sealed::Sealed for RwLockWriteGuardSome<'_, BdkWallet> {}
impl HeldWallet for RwLockWriteGuardSome<'_, BdkWallet> {}

impl sealed::Sealed for async_lock::RwLockWriteGuard<'_, Option<BdkWallet>> {}
impl HeldWallet for async_lock::RwLockWriteGuard<'_, Option<BdkWallet>> {}

/// Warn if a lock takes this long to acquire.
const LOCK_WARN_DURATION: Duration = Duration::from_secs(1);

/// Await a lock, warning if acquisition takes longer than
/// [`LOCK_WARN_DURATION`].
async fn acquire_warn_slow<Guard>(lock: impl Future<Output = Guard>, what: &str) -> Guard {
    use futures::future::{Either, select};
    tracing::trace!("wallet: acquiring {what}");
    match select(
        std::pin::pin!(lock),
        std::pin::pin!(tokio::time::sleep(LOCK_WARN_DURATION)),
    )
    .await
    {
        Either::Left((guard, _sleep)) => guard,
        Either::Right(((), acquiring_lock)) => {
            tracing::warn!(
                "wallet: waiting over {} to acquire {what}",
                jiff::SignedDuration::try_from(LOCK_WARN_DURATION).unwrap(),
            );
            acquiring_lock.await
        }
    }
}

/// Evidence that the caller holds the full scan slot. See
/// [`WalletLocks::full_scan`].
pub(in crate::wallet) type FullScanGuard<'a> = tokio::sync::MutexGuard<'a, ()>;

/// The full scan slot, the BDK wallet, and its persistence, which must
/// always be taken in that order.
pub(in crate::wallet) struct WalletLocks {
    /// Held for the duration of a full scan. Outermost of the three: a scan
    /// takes this before the wallet lock, and nothing takes it while a wallet
    /// guard is held.
    ///
    /// A full scan holds the wallet across minutes of network I/O, and the
    /// enforcer's block connection path needs that same wallet. Serialising
    /// scans here lets an on-demand caller be turned away
    /// ([`Self::try_full_scan`]) instead of queueing on the wallet lock and
    /// extending the stall for every scan it asks for.
    full_scan: tokio::sync::Mutex<()>,
    /// Unlocked, ready-to-go wallet: `Some`. Locked wallet: `None`.
    bitcoin_wallet: async_lock::RwLock<Option<BdkWallet>>,
    bdk_db: tokio::sync::Mutex<Persistence>,
}

impl WalletLocks {
    pub(in crate::wallet) fn new(wallet: Option<BdkWallet>, database: Persistence) -> Self {
        Self {
            full_scan: tokio::sync::Mutex::new(()),
            bitcoin_wallet: async_lock::RwLock::new(wallet),
            bdk_db: tokio::sync::Mutex::new(database),
        }
    }

    /// Claim the full scan slot, waiting for any scan already running to
    /// finish first.
    pub(in crate::wallet) async fn full_scan(&self) -> FullScanGuard<'_> {
        acquire_warn_slow(self.full_scan.lock(), "full scan slot").await
    }

    /// Claim the full scan slot, or `None` if a scan is already running.
    pub(in crate::wallet) fn try_full_scan(&self) -> Option<FullScanGuard<'_>> {
        self.full_scan.try_lock().ok()
    }

    /// The BDK database.
    ///
    /// Takes a wallet guard by reference purely as evidence that the wallet
    /// lock is already held, which is what makes the required order the only
    /// expressible one. The guard is not read.
    pub(in crate::wallet) async fn db<Held>(
        &self,
        _held: &Held,
    ) -> tokio::sync::MutexGuard<'_, Persistence>
    where
        Held: HeldWallet,
    {
        acquire_warn_slow(self.bdk_db.lock(), "bdk db lock").await
    }

    pub(in crate::wallet) async fn read(
        &self,
    ) -> Result<RwLockReadGuardSome<'_, BdkWallet>, error::NotUnlocked> {
        let guard = acquire_warn_slow(self.bitcoin_wallet.read(), "read lock").await;
        RwLockReadGuardSome::new(guard).ok_or(error::NotUnlocked)
    }

    pub(in crate::wallet) async fn upgradable_read(
        &self,
    ) -> Result<RwLockUpgradableReadGuardSome<'_, BdkWallet>, error::NotUnlocked> {
        let guard = acquire_warn_slow(
            self.bitcoin_wallet.upgradable_read(),
            "upgradable read lock",
        )
        .await;
        RwLockUpgradableReadGuardSome::new(guard).ok_or(error::NotUnlocked)
    }

    pub(in crate::wallet) async fn write(
        &self,
    ) -> Result<RwLockWriteGuardSome<'_, BdkWallet>, error::NotUnlocked> {
        let guard = acquire_warn_slow(self.bitcoin_wallet.write(), "write lock").await;
        RwLockWriteGuardSome::new(guard).ok_or(error::NotUnlocked)
    }

    /// The wallet slot itself, including when it is empty: for asking whether
    /// a wallet is loaded, and for installing one.
    pub(in crate::wallet) async fn read_slot(
        &self,
    ) -> async_lock::RwLockReadGuard<'_, Option<BdkWallet>> {
        acquire_warn_slow(self.bitcoin_wallet.read(), "read lock (slot)").await
    }

    pub(in crate::wallet) async fn write_slot(
        &self,
    ) -> async_lock::RwLockWriteGuard<'_, Option<BdkWallet>> {
        acquire_warn_slow(self.bitcoin_wallet.write(), "write lock (slot)").await
    }
}
