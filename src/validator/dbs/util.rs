use std::{
    ops::DerefMut,
    path::{Path, PathBuf},
    sync::Arc,
};

use educe::Educe;
use fallible_iterator::{FallibleIterator, IteratorExt};
use heed::{BytesDecode, BytesEncode, EnvOpenOptions, RoTxn};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Unit key. LMDB can't use zero-sized keys, so this encodes to a single byte
#[derive(Clone, Copy, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub struct UnitKey;

impl<'de> Deserialize<'de> for UnitKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // Deserialize any byte (ignoring it) and return UnitKey
        let _ = u8::deserialize(deserializer)?;
        Ok(UnitKey)
    }
}

impl Serialize for UnitKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        // Always serialize to the same arbitrary byte
        serializer.serialize_u8(0x69)
    }
}

#[derive(Debug, Error)]
#[error("Error commiting write txn for database dir `{db_dir}`")]
pub struct CommitWriteTxnError {
    db_dir: PathBuf,
    source: heed::Error,
}

/// Wrapper for heed's `RwTxn`
pub struct RwTxn<'a> {
    inner: heed::RwTxn<'a>,
    db_dir: &'a Path,
}

impl<'rwtxn> RwTxn<'rwtxn> {
    pub fn commit(self) -> Result<(), CommitWriteTxnError> {
        self.inner.commit().map_err(|err| CommitWriteTxnError {
            db_dir: self.db_dir.to_owned(),
            source: err,
        })
    }
}

impl<'rwtxn> std::ops::Deref for RwTxn<'rwtxn> {
    type Target = heed::RwTxn<'rwtxn>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<'rwtxn> std::ops::DerefMut for RwTxn<'rwtxn> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<'rwtxn> AsMut<heed::RwTxn<'rwtxn>> for RwTxn<'rwtxn> {
    fn as_mut(&mut self) -> &mut heed::RwTxn<'rwtxn> {
        self.deref_mut()
    }
}

/// DB errors
pub mod db_error {
    use std::path::PathBuf;

    use thiserror::Error;

    fn display_key_bytes(
        key_bytes: &Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>>,
    ) -> String {
        match key_bytes {
            Ok(key_bytes) => {
                format!("key: `{}`", hex::encode(key_bytes))
            }
            Err(encode_err) => {
                format!("key encoding failed with error `{encode_err:#}`")
            }
        }
    }

    #[derive(Debug, Error)]
    #[error(
        "Failed to delete from db `{db_name}` at `{db_path}` ({})",
        display_key_bytes(.key_bytes)
    )]
    pub struct Delete {
        pub(super) db_name: &'static str,
        pub(super) db_path: PathBuf,
        pub(super) key_bytes: Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>>,
        pub(super) source: heed::Error,
    }

    #[derive(Debug, Error)]
    #[error("Failed to read first item from db `{db_name}` at `{db_path}`")]
    pub struct First {
        pub(super) db_name: &'static str,
        pub(super) db_path: PathBuf,
        pub(super) source: heed::Error,
    }

    #[derive(Debug, Error)]
    #[error("Failed to initialize read-only iterator for db `{db_name}` at `{db_path}`")]
    pub struct IterInit {
        pub(super) db_name: &'static str,
        pub(super) db_path: PathBuf,
        pub(super) source: heed::Error,
    }

    #[derive(Debug, Error)]
    #[error("Failed to read item of read-only iterator for db `{db_name}` at `{db_path}`")]
    pub struct IterItem {
        pub(super) db_name: &'static str,
        pub(super) db_path: PathBuf,
        pub(super) source: heed::Error,
    }

    #[derive(Debug, Error)]
    pub enum Iter {
        #[error(transparent)]
        Init(#[from] IterInit),
        #[error(transparent)]
        Item(#[from] IterItem),
    }

    #[derive(Debug, Error)]
    #[error("Failed to read length for db `{db_name}` at `{db_path}`")]
    pub struct Len {
        pub(super) db_name: &'static str,
        pub(super) db_path: PathBuf,
        pub(super) source: heed::Error,
    }

    fn display_value_bytes(
        value_bytes: &Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>>,
    ) -> String {
        match value_bytes {
            Ok(value_bytes) => {
                format!("value: `{}`", hex::encode(value_bytes))
            }
            Err(encode_err) => {
                format!("value encoding failed with error `{encode_err:#}`")
            }
        }
    }

    #[derive(Debug, Error)]
    #[error(
        "Failed to write to db `{db_name}` at `{db_path}` ({}, {})",
        display_key_bytes(.key_bytes),
        display_value_bytes(.value_bytes)
    )]
    pub struct Put {
        pub(super) db_name: &'static str,
        pub(super) db_path: PathBuf,
        pub(super) key_bytes: Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>>,
        pub(super) value_bytes: Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>>,
        pub(super) source: heed::Error,
    }

    #[derive(Debug, Error)]
    #[error(
        "Failed to read from db `{db_name}` at `{db_path}` ({}): {}",
        display_key_bytes(.key_bytes),
        .source
    )]
    pub struct TryGet {
        pub(super) db_name: &'static str,
        pub(super) db_path: PathBuf,
        pub(super) key_bytes: Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>>,
        pub(super) source: heed::Error,
    }

    #[derive(Debug, Error)]
    pub enum Get {
        #[error(transparent)]
        TryGet(#[from] TryGet),
        #[error(
            "Missing value from db `{db_name}` at `{db_path}` (key: {})",
            hex::encode(.key_bytes)
        )]
        MissingValue {
            db_name: &'static str,
            db_path: PathBuf,
            key_bytes: Vec<u8>,
        },
    }
}

/// Wrapper for heed's `Database`
#[derive(Educe)]
#[educe(Clone, Debug)]
pub struct Database<KC, DC> {
    inner: heed::Database<KC, DC>,
    name: &'static str,
    path: Arc<PathBuf>,
}

impl<KC, DC> Database<KC, DC> {
    pub fn delete<'a>(
        &self,
        rwtxn: &mut RwTxn<'_>,
        key: &'a KC::EItem,
    ) -> Result<bool, db_error::Delete>
    where
        KC: BytesEncode<'a>,
    {
        self.inner.delete(rwtxn, key).map_err(|err| {
            let key_bytes =
                <KC as BytesEncode>::bytes_encode(key).map(|key_bytes| key_bytes.to_vec());
            db_error::Delete {
                db_name: self.name,
                db_path: (*self.path).clone(),
                key_bytes,
                source: err,
            }
        })
    }

    #[allow(clippy::type_complexity)]
    pub fn first<'txn>(
        &self,
        rotxn: &'txn RoTxn<'_>,
    ) -> Result<Option<(KC::DItem, DC::DItem)>, db_error::First>
    where
        KC: BytesDecode<'txn>,
        DC: BytesDecode<'txn>,
    {
        self.inner.first(rotxn).map_err(|err| db_error::First {
            db_name: self.name,
            db_path: (*self.path).clone(),
            source: err,
        })
    }

    pub fn iter<'txn>(
        &self,
        rotxn: &'txn RoTxn<'_>,
    ) -> Result<
        fallible_iterator::MapErr<
            fallible_iterator::Convert<heed::RoIter<'txn, KC, DC>>,
            impl FnMut(heed::Error) -> db_error::IterItem + '_,
        >,
        db_error::IterInit,
    >
    where
        KC: BytesDecode<'txn>,
        DC: BytesDecode<'txn>,
    {
        match self.inner.iter(rotxn) {
            Ok(it) => Ok(it.transpose_into_fallible().map_err({
                let db_path = self.path.clone();
                move |err| db_error::IterItem {
                    db_name: self.name,
                    db_path: (*db_path).clone(),
                    source: err,
                }
            })),
            Err(err) => Err(db_error::IterInit {
                db_name: self.name,
                db_path: (*self.path).clone(),
                source: err,
            }),
        }
    }

    pub fn len(&self, rotxn: &RoTxn<'_>) -> Result<u64, db_error::Len> {
        self.inner.len(rotxn).map_err(|err| db_error::Len {
            db_name: self.name,
            db_path: (*self.path).clone(),
            source: err,
        })
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn put<'a>(
        &self,
        rwtxn: &mut RwTxn<'_>,
        key: &'a KC::EItem,
        data: &'a DC::EItem,
    ) -> Result<(), db_error::Put>
    where
        KC: BytesEncode<'a>,
        DC: BytesEncode<'a>,
    {
        self.inner.put(rwtxn, key, data).map_err(|err| {
            let key_bytes =
                <KC as BytesEncode>::bytes_encode(key).map(|key_bytes| key_bytes.to_vec());
            let value_bytes =
                <DC as BytesEncode>::bytes_encode(data).map(|value_bytes| value_bytes.to_vec());
            db_error::Put {
                db_name: self.name,
                db_path: (*self.path).clone(),
                key_bytes,
                value_bytes,
                source: err,
            }
        })
    }

    pub fn try_get<'a, 'txn>(
        &self,
        rotxn: &'txn RoTxn<'_>,
        key: &'a KC::EItem,
    ) -> Result<Option<DC::DItem>, db_error::TryGet>
    where
        KC: BytesEncode<'a>,
        DC: BytesDecode<'txn>,
    {
        self.inner.get(rotxn, key).map_err(|err| {
            let key_bytes =
                <KC as BytesEncode>::bytes_encode(key).map(|key_bytes| key_bytes.to_vec());
            db_error::TryGet {
                db_name: self.name,
                db_path: (*self.path).clone(),
                key_bytes,
                source: err,
            }
        })
    }

    pub fn get<'a, 'txn>(
        &self,
        rotxn: &'txn RoTxn<'_>,
        key: &'a KC::EItem,
    ) -> Result<DC::DItem, db_error::Get>
    where
        KC: BytesEncode<'a>,
        DC: BytesDecode<'txn>,
    {
        self.try_get(rotxn, key)?.ok_or_else(|| {
            let key_bytes = <KC as BytesEncode>::bytes_encode(key)
                // Safety: key must encode successfully, as try_get succeeded
                .unwrap()
                .to_vec();
            db_error::Get::MissingValue {
                db_name: self.name,
                db_path: (*self.path).clone(),
                key_bytes,
            }
        })
    }
}

#[derive(Debug, Error)]
#[error("Error opening database env at (`{path}`)")]
pub struct OpenEnvError {
    path: PathBuf,
    source: heed::Error,
}

#[derive(Debug, Error)]
#[error("Error creating database `{name}` in `{path}`")]
pub struct CreateDbError {
    name: &'static str,
    path: PathBuf,
    source: heed::Error,
}

#[derive(Debug, Error)]
#[error("Error creating read txn for database dir `{db_dir}`")]
pub struct ReadTxnError {
    db_dir: PathBuf,
    source: heed::Error,
}

#[derive(Debug, Error)]
#[error("Error creating write txn for database dir `{db_dir}`")]
pub struct WriteTxnError {
    db_dir: PathBuf,
    source: heed::Error,
}

/// Wrapper for heed's `Env`
#[derive(Clone, Debug)]
pub struct Env {
    inner: heed::Env,
    path: Arc<PathBuf>,
}

impl Env {
    pub unsafe fn open(opts: &EnvOpenOptions, path: PathBuf) -> Result<Self, OpenEnvError> {
        let inner = match opts.open(&path) {
            Ok(env) => env,
            Err(err) => return Err(OpenEnvError { path, source: err }),
        };
        Ok(Self {
            inner,
            path: Arc::new(path),
        })
    }

    pub fn create_db<KC, DC>(
        &self,
        rwtxn: &mut RwTxn<'_>,
        name: &'static str,
    ) -> Result<Database<KC, DC>, CreateDbError>
    where
        KC: 'static,
        DC: 'static,
    {
        let inner = self
            .inner
            .create_database(rwtxn, Some(name))
            .map_err(|err| CreateDbError {
                name,
                path: (*self.path).clone(),
                source: err,
            })?;
        Ok(Database {
            inner,
            name,
            path: self.path.clone(),
        })
    }

    pub fn read_txn(&self) -> Result<RoTxn<'_>, ReadTxnError> {
        self.inner.read_txn().map_err(|err| ReadTxnError {
            db_dir: (*self.path).clone(),
            source: err,
        })
    }

    pub fn write_txn(&self) -> Result<RwTxn<'_>, WriteTxnError> {
        let inner = self.inner.write_txn().map_err(|err| WriteTxnError {
            db_dir: (*self.path).clone(),
            source: err,
        })?;
        Ok(RwTxn {
            inner,
            db_dir: &self.path,
        })
    }
}
