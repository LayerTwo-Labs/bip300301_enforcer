//! Miscellaneous utility functions and types

use std::ops::{Deref, DerefMut};

use async_lock::{RwLockReadGuard, RwLockUpgradableReadGuard, RwLockWriteGuard};

/// Guard over values of `Option<T>` that are guaranteed to be `Some`.
/// Checked once on construction; only `T` is exposed afterwards, so the
/// `Option` cannot become `None` while the guard is held.
#[repr(transparent)]
pub(in crate::wallet) struct GuardSome<G>(G);

const INVARIANT: &str = "GuardSome is only constructed over Some";

pub(in crate::wallet) type RwLockReadGuardSome<'a, T> = GuardSome<RwLockReadGuard<'a, Option<T>>>;

pub(in crate::wallet) type RwLockUpgradableReadGuardSome<'a, T> =
    GuardSome<RwLockUpgradableReadGuard<'a, Option<T>>>;

pub(in crate::wallet) type RwLockWriteGuardSome<'a, T> = GuardSome<RwLockWriteGuard<'a, Option<T>>>;

impl<G, T> GuardSome<G>
where
    G: Deref<Target = Option<T>>,
{
    pub fn new(guard: G) -> Option<Self> {
        guard.is_some().then_some(Self(guard))
    }
}

impl<G, T> GuardSome<G>
where
    G: DerefMut<Target = Option<T>>,
{
    /// Use the mutable inner value
    pub fn with_mut<'a, F, Output>(&'a mut self, f: F) -> Output
    where
        T: 'a,
        F: FnOnce(&'a mut T) -> Output,
    {
        f(self.0.as_mut().expect(INVARIANT))
    }
}

impl<'a, T> RwLockUpgradableReadGuardSome<'a, T> {
    /// This is an associated function that needs to be used as
    /// RwLockUpgradableReadGuard::upgrade(...).
    /// A method would interfere with methods of the same name on the contents
    /// of the locked data.
    pub async fn upgrade(s: Self) -> RwLockWriteGuardSome<'a, T> {
        GuardSome(RwLockUpgradableReadGuard::upgrade(s.0).await)
    }
}

impl<G, T> Deref for GuardSome<G>
where
    G: Deref<Target = Option<T>>,
{
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref().expect(INVARIANT)
    }
}
