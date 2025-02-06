//! Miscellaneous utility functions and types

#[allow(clippy::significant_drop_tightening, reason = "False positive")]
mod rwlock_write_guard_some {
    /// Write guard over values of `Option<T>` that are guaranteed to be `Some`
    #[ouroboros::self_referencing]
    struct Inner<'a, T> {
        write_guard: parking_lot::RwLockWriteGuard<'a, Option<T>>,
        #[borrows(mut write_guard)]
        inner: &'this mut T,
    }

    /// Write guard over values of `Option<T>` that are guaranteed to be `Some`
    #[repr(transparent)]
    pub struct RwLockWriteGuardSome<'a, T>(Inner<'a, T>);

    impl<'a, T> RwLockWriteGuardSome<'a, T> {
        /// Panics if the inner value is `None`
        pub(in crate::wallet::util) fn new_unchecked(
            write_guard: parking_lot::RwLockWriteGuard<'a, Option<T>>,
        ) -> Self {
            Self(Inner::new(write_guard, |write_guard| {
                write_guard
                    .as_mut()
                    .expect("Inner value of RwLockWriteGuardSome should be Some")
            }))
        }
    }

    impl<T> RwLockWriteGuardSome<'_, T> {
        /// Use the mutable inner value
        pub fn with_mut<F, Output>(&mut self, f: F) -> Output
        where
            F: FnOnce(&mut T) -> Output,
        {
            self.0.with_inner_mut(|inner| f(*inner))
        }
    }

    impl<T> std::ops::Deref for RwLockWriteGuardSome<'_, T> {
        type Target = T;

        fn deref(&self) -> &Self::Target {
            self.0.borrow_inner()
        }
    }
}

pub(in crate::wallet) use rwlock_write_guard_some::RwLockWriteGuardSome;

#[allow(clippy::significant_drop_tightening, reason = "False positive")]
mod rwlock_upgradable_read_guard_some {
    use parking_lot::RwLockUpgradableReadGuard;

    use super::RwLockWriteGuardSome;

    /// Upgradable read guard over values of `Option<T>` that are guaranteed to
    /// be `Some`
    #[ouroboros::self_referencing]
    struct Inner<'a, T> {
        read_guard: RwLockUpgradableReadGuard<'a, Option<T>>,
        #[borrows(read_guard)]
        inner: &'this T,
    }

    /// Upgradable read guard over values of `Option<T>` that are guaranteed to
    /// be `Some`
    #[repr(transparent)]
    pub struct RwLockUpgradableReadGuardSome<'a, T>(Inner<'a, T>);

    impl<'a, T> RwLockUpgradableReadGuardSome<'a, T> {
        pub fn new(read_guard: RwLockUpgradableReadGuard<'a, Option<T>>) -> Option<Self> {
            match Inner::try_new(read_guard, |read_guard| read_guard.as_ref().ok_or(())) {
                Ok(inner) => Some(Self(inner)),
                Err(()) => None,
            }
        }

        /// Tries to atomically upgrade an upgradable read lock into an exclusive
        /// write lock, until a timeout is reached.
        ///
        /// If the access could not be granted before the timeout expires, then the
        /// current guard is returned.
        ///
        /// This is an associated function that needs to be used as
        /// RwLockUpgradableReadGuard::try_upgrade_for(...).
        /// A method would interfere with methods of the same name on the contents
        /// of the locked data.
        pub fn try_upgrade_for(
            s: Self,
            timeout: std::time::Duration,
        ) -> Result<RwLockWriteGuardSome<'a, T>, Self> {
            let read_guard = s.0.into_heads().read_guard;
            match RwLockUpgradableReadGuard::try_upgrade_for(read_guard, timeout) {
                Ok(write_guard) => Ok(RwLockWriteGuardSome::new_unchecked(write_guard)),
                Err(read_guard) => {
                    let inner = Inner::new(read_guard, |read_guard| {
                        read_guard
                            .as_ref()
                            .expect("Inner value of RwLockUpgradableReadGuardSome should be Some")
                    });
                    Err(Self(inner))
                }
            }
        }
    }

    impl<T> std::ops::Deref for RwLockUpgradableReadGuardSome<'_, T> {
        type Target = T;

        fn deref(&self) -> &Self::Target {
            self.0.borrow_inner()
        }
    }
}

pub(in crate::wallet) use rwlock_upgradable_read_guard_some::RwLockUpgradableReadGuardSome;
