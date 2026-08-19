use std::{
    sync::{Arc, Weak},
    time::SystemTime,
};

use parking_lot::RwLock;

use crate::{errors::ErrorChain, wallet::ChainSourceClient};

#[derive(Default)]
struct SyncState {
    client: Option<Arc<ChainSourceClient>>,
    last_error: Option<String>,
    last_synced_at: Option<SystemTime>,
}

/// A consistent snapshot of the sync state
pub(in crate::wallet) struct SyncStateReport {
    pub connected: bool,
    pub last_error: Option<String>,
    pub last_synced_at: Option<SystemTime>,
}

/// Shared, cheaply cloned handle to the wallet's [`SyncState`].
///
/// Deliberately behind a *synchronous* lock. This lets `clippy::await_holding_lock`
/// to catch places we're doing stupid things.
#[derive(Clone, Default)]
pub(in crate::wallet) struct SharedSyncState(Arc<RwLock<SyncState>>);

impl SharedSyncState {
    pub(in crate::wallet) fn with_last_error(err: Option<String>) -> Self {
        Self(Arc::new(RwLock::new(SyncState {
            client: None,
            last_error: err,
            last_synced_at: None,
        })))
    }

    pub(in crate::wallet) fn with_client(client: ChainSourceClient) -> Self {
        Self(Arc::new(RwLock::new(SyncState {
            client: Some(Arc::new(client)),
            last_error: None,
            last_synced_at: None,
        })))
    }

    pub(in crate::wallet) fn downgrade(&self) -> WeakSyncState {
        WeakSyncState(Arc::downgrade(&self.0))
    }

    pub(in crate::wallet) fn client(&self) -> Option<Arc<ChainSourceClient>> {
        self.0.read().client.clone()
    }

    pub(in crate::wallet) fn has_synced(&self) -> bool {
        self.0.read().last_synced_at.is_some()
    }

    pub(in crate::wallet) fn mark_synced_now(&self) {
        self.0.write().last_synced_at = Some(SystemTime::now());
    }

    pub(in crate::wallet) fn publish_client(&self, client: ChainSourceClient) {
        let mut state = self.0.write();
        state.client = Some(Arc::new(client));
        state.last_error = None;
    }

    pub(in crate::wallet) fn set_last_error<E>(&self, err: &E)
    where
        E: std::error::Error,
    {
        self.0.write().last_error = Some(format!("{:#}", ErrorChain::new(err)));
    }

    /// Record the outcome of an interaction with the backend, so wallet info
    /// can report one that is currently failing. Passes the result through
    /// unchanged.
    pub(in crate::wallet) fn record_result<T, E>(&self, result: Result<T, E>) -> Result<T, E>
    where
        E: std::error::Error,
    {
        match &result {
            Ok(_) => self.0.write().last_error = None,
            Err(err) => self.set_last_error(err),
        }
        result
    }

    pub(in crate::wallet) fn report(&self) -> SyncStateReport {
        let state = self.0.read();
        SyncStateReport {
            connected: state.client.is_some(),
            last_error: state.last_error.clone(),
            last_synced_at: state.last_synced_at,
        }
    }
}

/// Non-owning [`SharedSyncState`] handle. See [`SharedSyncState::downgrade`].
pub(in crate::wallet) struct WeakSyncState(Weak<RwLock<SyncState>>);

impl WeakSyncState {
    /// `None` once the wallet holding the state has been dropped.
    pub(in crate::wallet) fn upgrade(&self) -> Option<SharedSyncState> {
        self.0.upgrade().map(SharedSyncState)
    }
}
