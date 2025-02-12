use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Mutex;

use rusqlite::{Connection, Result};

use bdk_wallet::{AsyncWalletPersister, ChangeSet};

/// A simple thread‑safe wrapper around a rusqlite::Connection that implements AsyncWalletPersister.
#[derive(Debug)]
pub struct ThreadSafeConnection(Mutex<Connection>);

impl ThreadSafeConnection {
    /// Opens a new connection at the given path and returns a new ThreadSafeConnection.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let conn = Connection::open(path)?;
        Ok(ThreadSafeConnection(Mutex::new(conn)))
    }
}

/// Cribbed from the implementation of WalletPersister from BDK
/// https://github.com/bitcoindevkit/bdk/blob/7067da1522c5c2ae4e457846cfe5bd6aefafbe9e/crates/wallet/src/wallet/persisted.rs#L271-L287
impl AsyncWalletPersister for ThreadSafeConnection {
    type Error = rusqlite::Error;

    /// Initializes the persister, e.g., running any necessary migrations.
    /// This implementation acquires a transaction, runs the necessary initialization
    /// on sqlite tables, builds a ChangeSet from the transaction, commits, and returns it.
    fn initialize<'a>(
        persister: &'a mut Self,
    ) -> Pin<Box<dyn Future<Output = Result<ChangeSet, Self::Error>> + Send + 'a>>
    where
        Self: 'a,
    {
        Box::pin(async move {
            let mut conn = persister.0.lock().expect("Mutex poisoned");
            let tx = conn.transaction()?;
            ChangeSet::init_sqlite_tables(&tx)?;
            let cs = ChangeSet::from_sqlite(&tx)?;
            tx.commit()?;
            drop(conn);
            Ok(cs)
        })
    }

    /// Persists the given changeset.
    fn persist<'a>(
        persister: &'a mut Self,
        changeset: &'a ChangeSet,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>>
    where
        Self: 'a,
    {
        Box::pin(async move {
            let mut conn = persister.0.lock().expect("Mutex poisoned");
            let tx = conn.transaction()?;
            changeset.persist_to_sqlite(&tx)?;
            tx.commit()?;
            drop(conn);
            Ok(())
        })
    }
}
