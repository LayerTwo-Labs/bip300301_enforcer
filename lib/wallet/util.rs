//! Miscellaneous utility functions and types

use tokio::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Async RwLock with upgradable reads. Any number of plain readers can hold it
/// alongside one upgradable reader, which can later upgrade to a writer
/// without another writer getting in first.
pub(in crate::wallet) struct UpgradableRwLock<T> {
    /// Held by writers and upgradable readers, so an upgrade can drop its read
    /// guard and take the write lock with no other writer in between.
    writer_slot: Mutex<()>,
    lock: RwLock<T>,
}

impl<T> UpgradableRwLock<T> {
    pub fn new(value: T) -> Self {
        Self {
            writer_slot: Mutex::new(()),
            lock: RwLock::new(value),
        }
    }

    pub async fn read(&self) -> RwLockReadGuard<'_, T> {
        self.lock.read().await
    }

    pub async fn upgradable_read(&self) -> UpgradableReadGuard<'_, T> {
        let writer_slot = self.writer_slot.lock().await;
        UpgradableReadGuard {
            read_guard: self.lock.read().await,
            writer_slot,
            lock: &self.lock,
        }
    }

    pub async fn write(&self) -> WriteGuard<'_, T> {
        let writer_slot = self.writer_slot.lock().await;
        WriteGuard {
            write_guard: self.lock.write().await,
            _writer_slot: writer_slot,
        }
    }
}

pub(in crate::wallet) struct UpgradableReadGuard<'a, T> {
    read_guard: RwLockReadGuard<'a, T>,
    writer_slot: MutexGuard<'a, ()>,
    lock: &'a RwLock<T>,
}

impl<'a, T> UpgradableReadGuard<'a, T> {
    /// An associated function rather than a method, so that it cannot shadow
    /// methods of the same name on the locked value.
    pub async fn upgrade(guard: Self) -> WriteGuard<'a, T> {
        let Self {
            read_guard,
            writer_slot,
            lock,
        } = guard;
        drop(read_guard);
        WriteGuard {
            write_guard: lock.write().await,
            _writer_slot: writer_slot,
        }
    }
}

impl<T> std::ops::Deref for UpgradableReadGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.read_guard
    }
}

pub(in crate::wallet) struct WriteGuard<'a, T> {
    write_guard: RwLockWriteGuard<'a, T>,
    _writer_slot: MutexGuard<'a, ()>,
}

impl<T> std::ops::Deref for WriteGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.write_guard
    }
}

impl<T> std::ops::DerefMut for WriteGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.write_guard
    }
}

const NONE_MSG: &str = "checked to be `Some` on construction, and not reachable as an `Option`";

/// Write guard over values of `Option<T>` that are guaranteed to be `Some`
pub(in crate::wallet) struct RwLockWriteGuardSome<'a, T>(WriteGuard<'a, Option<T>>);

impl<'a, T> RwLockWriteGuardSome<'a, T> {
    pub fn new(write_guard: WriteGuard<'a, Option<T>>) -> Option<Self> {
        write_guard.is_some().then_some(Self(write_guard))
    }

    /// Use the mutable inner value
    pub fn with_mut<'b, F, Output>(&'b mut self, f: F) -> Output
    where
        F: FnOnce(&'b mut T) -> Output,
    {
        f(self.0.as_mut().expect(NONE_MSG))
    }
}

impl<T> std::ops::Deref for RwLockWriteGuardSome<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.0.as_ref().expect(NONE_MSG)
    }
}

/// Upgradable read guard over values of `Option<T>` that are guaranteed to
/// be `Some`
pub(in crate::wallet) struct RwLockUpgradableReadGuardSome<'a, T>(
    UpgradableReadGuard<'a, Option<T>>,
);

impl<'a, T> RwLockUpgradableReadGuardSome<'a, T> {
    pub fn new(read_guard: UpgradableReadGuard<'a, Option<T>>) -> Option<Self> {
        read_guard.is_some().then_some(Self(read_guard))
    }

    /// An associated function rather than a method, so that it cannot shadow
    /// methods of the same name on the locked value.
    pub async fn upgrade(guard: Self) -> RwLockWriteGuardSome<'a, T> {
        // Holding the writer slot throughout means nothing can have taken the
        // value in the meantime.
        RwLockWriteGuardSome(UpgradableReadGuard::upgrade(guard.0).await)
    }
}

impl<T> std::ops::Deref for RwLockUpgradableReadGuardSome<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.0.as_ref().expect(NONE_MSG)
    }
}

/// Read guard over values of `Option<T>` that are guaranteed to be `Some`
pub(in crate::wallet) struct RwLockReadGuardSome<'a, T>(RwLockReadGuard<'a, T>);

impl<'a, T> RwLockReadGuardSome<'a, T> {
    pub fn new(read_guard: RwLockReadGuard<'a, Option<T>>) -> Option<Self> {
        RwLockReadGuard::try_map(read_guard, Option::as_ref)
            .ok()
            .map(Self)
    }
}

impl<T> std::ops::Deref for RwLockReadGuardSome<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use std::pin::pin;

    use futures::FutureExt as _;

    use super::{UpgradableReadGuard, UpgradableRwLock};

    #[tokio::test]
    async fn readers_share_with_upgradable_reader() {
        let lock = UpgradableRwLock::new(0);
        let _upgradable = lock.upgradable_read().await;
        assert!(lock.read().now_or_never().is_some());
        assert!(lock.upgradable_read().now_or_never().is_none());
        assert!(lock.write().now_or_never().is_none());
    }

    #[expect(
        clippy::significant_drop_tightening,
        reason = "False positive: the guard moves into `pin!`"
    )]
    #[tokio::test]
    async fn upgrade_waits_for_readers() {
        let lock = UpgradableRwLock::new(0);
        let upgradable = lock.upgradable_read().await;
        let reader = lock.read().await;
        let mut upgrade = pin!(UpgradableReadGuard::upgrade(upgradable));
        assert!(upgrade.as_mut().now_or_never().is_none());
        drop(reader);
        assert!(upgrade.now_or_never().is_some());
    }

    /// A writer queued before the upgrade must still not get in ahead of it
    #[tokio::test]
    async fn upgrade_beats_queued_writer() {
        let lock = UpgradableRwLock::new(0);
        let upgradable = lock.upgradable_read().await;
        let mut write = pin!(lock.write());
        assert!(write.as_mut().now_or_never().is_none());

        let mut upgraded = UpgradableReadGuard::upgrade(upgradable)
            .now_or_never()
            .expect("no reader or writer holds the lock");
        assert_eq!(*upgraded, 0);
        *upgraded = 1;
        drop(upgraded);

        assert_eq!(*write.await, 1);
    }

    /// Randomised concurrent use of the lock, checking its guarantees from
    /// inside every critical section.
    mod stress {
        use std::{
            future::Future,
            sync::{
                Arc,
                atomic::{AtomicU64, AtomicUsize, Ordering::SeqCst},
            },
            time::Duration,
        };

        use futures::FutureExt as _;

        use super::super::{UpgradableReadGuard, UpgradableRwLock};

        const TASKS: u64 = 32;
        const OPS_PER_TASK: usize = 2_000;

        /// Who holds the lock. Only changed while holding the matching guard,
        /// and decremented before it is released.
        #[derive(Default)]
        struct Occupancy {
            readers: AtomicUsize,
            upgradable: AtomicUsize,
            writers: AtomicUsize,
            writers_waiting: AtomicUsize,
        }

        /// How often the interleavings worth testing actually happened
        #[derive(Debug, Default)]
        struct Coverage {
            writes: AtomicU64,
            upgrades: AtomicU64,
            reads_beside_upgradable: AtomicU64,
            upgrades_past_readers: AtomicU64,
            upgrades_past_waiting_writer: AtomicU64,
            cancelled: AtomicU64,
        }

        struct Task {
            rng: u64,
            lock: Arc<UpgradableRwLock<u64>>,
            occupancy: Arc<Occupancy>,
            coverage: Arc<Coverage>,
        }

        impl Task {
            /// xorshift64
            fn random_below(&mut self, n: u64) -> u64 {
                self.rng ^= self.rng << 13;
                self.rng ^= self.rng >> 7;
                self.rng ^= self.rng << 17;
                self.rng % n
            }

            async fn pause(&mut self) {
                match self.random_below(16) {
                    0..=7 => (),
                    8..=14 => tokio::task::yield_now().await,
                    _ => tokio::time::sleep(Duration::from_micros(100)).await,
                }
            }

            /// Acquire, or, one time in ten, poll once and give up, as a
            /// dropped request handler would
            async fn acquire<F: Future>(&mut self, fut: F) -> Option<F::Output> {
                if self.random_below(10) == 0 {
                    let res = fut.now_or_never();
                    if res.is_none() {
                        self.coverage.cancelled.fetch_add(1, SeqCst);
                    }
                    res
                } else {
                    Some(fut.await)
                }
            }

            fn assert_exclusive(&self) {
                assert_eq!(
                    self.occupancy.readers.load(SeqCst),
                    0,
                    "reader beside writer"
                );
                assert_eq!(
                    self.occupancy.upgradable.load(SeqCst),
                    0,
                    "upgradable reader beside writer"
                );
                assert_eq!(self.occupancy.writers.load(SeqCst), 1, "two writers");
            }

            async fn read(&mut self) {
                let lock = self.lock.clone();
                let Some(guard) = self.acquire(lock.read()).await else {
                    return;
                };
                self.occupancy.readers.fetch_add(1, SeqCst);
                assert_eq!(
                    self.occupancy.writers.load(SeqCst),
                    0,
                    "reader beside writer"
                );
                if self.occupancy.upgradable.load(SeqCst) > 0 {
                    self.coverage.reads_beside_upgradable.fetch_add(1, SeqCst);
                }
                let seen = *guard;
                self.pause().await;
                assert_eq!(*guard, seen, "value changed under a read guard");
                assert_eq!(
                    self.occupancy.writers.load(SeqCst),
                    0,
                    "reader beside writer"
                );
                self.occupancy.readers.fetch_sub(1, SeqCst);
                drop(guard);
            }

            #[expect(
                clippy::significant_drop_tightening,
                reason = "False positive: the guard is held until the explicit drop"
            )]
            async fn write(&mut self) {
                let lock = self.lock.clone();
                self.occupancy.writers_waiting.fetch_add(1, SeqCst);
                let guard = self.acquire(lock.write()).await;
                self.occupancy.writers_waiting.fetch_sub(1, SeqCst);
                let Some(mut guard) = guard else {
                    return;
                };
                self.occupancy.writers.fetch_add(1, SeqCst);
                self.assert_exclusive();
                *guard += 1;
                self.coverage.writes.fetch_add(1, SeqCst);
                self.pause().await;
                self.assert_exclusive();
                self.occupancy.writers.fetch_sub(1, SeqCst);
                drop(guard);
            }

            async fn upgradable_read(&mut self) {
                let lock = self.lock.clone();
                let Some(guard) = self.acquire(lock.upgradable_read()).await else {
                    return;
                };
                assert_eq!(
                    self.occupancy.upgradable.fetch_add(1, SeqCst),
                    0,
                    "two upgradable readers"
                );
                assert_eq!(
                    self.occupancy.writers.load(SeqCst),
                    0,
                    "upgradable reader beside writer"
                );
                let seen = *guard;
                self.pause().await;
                assert_eq!(*guard, seen, "value changed under an upgradable guard");
                if self.random_below(3) == 0 {
                    self.occupancy.upgradable.fetch_sub(1, SeqCst);
                    drop(guard);
                    return;
                }

                let readers_present = self.occupancy.readers.load(SeqCst) > 0;
                let writer_waiting = self.occupancy.writers_waiting.load(SeqCst) > 0;
                // Holding the writer slot throughout the upgrade is what keeps
                // writers out, which the value check below verifies
                self.occupancy.upgradable.fetch_sub(1, SeqCst);
                let Some(mut guard) = self.acquire(UpgradableReadGuard::upgrade(guard)).await
                else {
                    return;
                };
                self.occupancy.writers.fetch_add(1, SeqCst);
                self.assert_exclusive();
                assert_eq!(*guard, seen, "a writer got in during the upgrade");
                self.coverage.upgrades.fetch_add(1, SeqCst);
                if readers_present {
                    self.coverage.upgrades_past_readers.fetch_add(1, SeqCst);
                }
                if writer_waiting {
                    self.coverage
                        .upgrades_past_waiting_writer
                        .fetch_add(1, SeqCst);
                }
                *guard += 1;
                self.pause().await;
                self.assert_exclusive();
                self.occupancy.writers.fetch_sub(1, SeqCst);
                drop(guard);
            }

            async fn run(mut self) {
                for _ in 0..OPS_PER_TASK {
                    match self.random_below(10) {
                        0..=4 => self.read().await,
                        5..=6 => self.write().await,
                        _ => self.upgradable_read().await,
                    }
                }
            }
        }

        #[expect(
            clippy::print_stderr,
            reason = "Coverage counts show what the run exercised"
        )]
        #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
        async fn guarantees_hold_under_contention() {
            let lock = Arc::new(UpgradableRwLock::new(0));
            let occupancy = Arc::new(Occupancy::default());
            let coverage = Arc::new(Coverage::default());
            let tasks = (1..=TASKS).map(|seed| {
                tokio::spawn(
                    Task {
                        rng: seed.wrapping_mul(0x9E37_79B9_7F4A_7C15),
                        lock: lock.clone(),
                        occupancy: occupancy.clone(),
                        coverage: coverage.clone(),
                    }
                    .run(),
                )
            });
            tokio::time::timeout(
                Duration::from_secs(120),
                futures::future::try_join_all(tasks),
            )
            .await
            .expect("deadlock: tasks did not finish")
            .expect("a task panicked");

            eprintln!("{coverage:#?}");
            for (count, what) in [
                (&coverage.upgrades, "upgrades"),
                (
                    &coverage.reads_beside_upgradable,
                    "reads beside an upgradable reader",
                ),
                (
                    &coverage.upgrades_past_readers,
                    "upgrades waiting on readers",
                ),
                (
                    &coverage.upgrades_past_waiting_writer,
                    "upgrades past a waiting writer",
                ),
                (&coverage.cancelled, "cancelled acquisitions"),
            ] {
                assert!(count.load(SeqCst) > 0, "never exercised: {what}");
            }
            let writes = coverage.writes.load(SeqCst) + coverage.upgrades.load(SeqCst);
            assert_eq!(*lock.read().await, writes, "a write was lost");
        }
    }
}
