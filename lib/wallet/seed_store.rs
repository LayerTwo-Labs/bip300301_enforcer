//! The wallet's seed store: a single JSON file (`seed.json`).
//!
//! Before the wallet/producer split, the seed lived in a `wallet_seeds` table
//! inside the shared `db.sqlite` (now the block producer's policy store).
//! [`SeedStore::new`] migrates such a legacy seed automatically: it writes the
//! seed file, verifies it round-trips, and only then deletes the legacy rows.
//! Every crash point either leaves the legacy seed untouched or leaves the
//! seed in both places, and the duplicate resolves idempotently on the next
//! startup. There is no ordering in which the seed exists nowhere.

use std::path::{Path, PathBuf};

use bdk_wallet::bip39::{Language, Mnemonic};
use either::Either;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::wallet::{
    error,
    mnemonic::{EncryptedMnemonic, KdfParams},
};

/// A seed to persist: either plaintext, or encrypted under a password.
pub(in crate::wallet) enum Seed<'a> {
    Plaintext(&'a Mnemonic),
    Encrypted(&'a EncryptedMnemonic),
}

const SEED_FILE_NAME: &str = "seed.json";

const CURRENT_VERSION: u32 = 1;

/// The seed as stored in `seed.json`
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum StoredSeed {
    Plaintext {
        mnemonic: String,
    },
    Encrypted {
        #[serde(with = "hex::serde")]
        initialization_vector: Vec<u8>,
        #[serde(with = "hex::serde")]
        ciphertext_mnemonic: Vec<u8>,
        #[serde(with = "hex::serde")]
        key_salt: Vec<u8>,
        /// Absent on seeds written before the cost was recorded.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kdf: Option<StoredKdfParams>,
    },
}

/// The Argon2id cost a seed was encrypted under, as stored in `seed.json`.
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
struct StoredKdfParams {
    memory_kib: u32,
    iterations: u32,
    parallelism: u32,
}

impl From<KdfParams> for StoredKdfParams {
    fn from(
        KdfParams {
            memory_kib,
            iterations,
            parallelism,
        }: KdfParams,
    ) -> Self {
        Self {
            memory_kib,
            iterations,
            parallelism,
        }
    }
}

impl From<StoredKdfParams> for KdfParams {
    fn from(
        StoredKdfParams {
            memory_kib,
            iterations,
            parallelism,
        }: StoredKdfParams,
    ) -> Self {
        Self {
            memory_kib,
            iterations,
            parallelism,
        }
    }
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
struct SeedFile {
    version: u32,
    /// Informational only. Carried over from the legacy DB on migration.
    created_at: std::time::SystemTime,
    /// The node's tip height when this seed was generated `None` for restored
    /// seeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    birthday_height: Option<u32>,
    seed: StoredSeed,
}

/// The wallet's seed store.
pub(in crate::wallet) struct SeedStore {
    path: PathBuf,
    /// Serializes seed inserts, so two concurrent `CreateWallet` calls cannot
    /// both pass the "does a seed already exist" check. Stores in *other*
    /// processes are held off by the publish in [`write_seed_file`], which
    /// refuses to replace a seed file that is already there.
    insert_lock: tokio::sync::Mutex<()>,
}

impl SeedStore {
    pub(in crate::wallet) fn new(data_dir: &Path) -> Result<Self, error::InitSeedStore> {
        let path = data_dir.join(SEED_FILE_NAME);
        let () = migrate_legacy_seed(&path, data_dir)?;
        Ok(Self {
            path,
            insert_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// The stored seed, if the wallet has been created.
    pub(in crate::wallet) async fn read_mnemonic(
        &self,
    ) -> Result<Option<Either<Mnemonic, EncryptedMnemonic>>, error::ReadSeed> {
        let Some(seed_file) = read_seed_file(&self.path)? else {
            return Ok(None);
        };
        let seed = match seed_file.seed {
            StoredSeed::Plaintext { mnemonic } => Either::Left(
                Mnemonic::parse_in_normalized(Language::English, &mnemonic)
                    .map_err(error::ParseMnemonic::from)?,
            ),
            StoredSeed::Encrypted {
                initialization_vector,
                ciphertext_mnemonic,
                key_salt,
                kdf,
            } => Either::Right(EncryptedMnemonic {
                initialization_vector,
                ciphertext_mnemonic,
                key_salt,
                kdf: kdf.map_or(KdfParams::LEGACY, KdfParams::from),
            }),
        };
        Ok(Some(seed))
    }

    /// The wallet's birthday height, if one was recorded at creation.
    pub(in crate::wallet) async fn read_birthday_height(
        &self,
    ) -> Result<Option<u32>, error::ReadSeed> {
        Ok(read_seed_file(&self.path)?.and_then(|seed_file| seed_file.birthday_height))
    }

    /// Persist the seed for a newly created wallet. `birthday_height` must
    /// only be `Some` for freshly GENERATED seeds (see [`SeedFile`]).
    pub(in crate::wallet) async fn insert_seed(
        &self,
        seed: Seed<'_>,
        birthday_height: Option<u32>,
    ) -> Result<(), error::InsertSeed> {
        let _guard = self.insert_lock.lock().await;
        if self.path.exists() {
            return Err(error::InsertSeed::AlreadyExists);
        }
        let seed_file = SeedFile {
            version: CURRENT_VERSION,
            created_at: std::time::SystemTime::now(),
            birthday_height,
            seed: match seed {
                Seed::Plaintext(mnemonic) => StoredSeed::Plaintext {
                    mnemonic: mnemonic.to_string(),
                },
                Seed::Encrypted(encrypted) => StoredSeed::Encrypted {
                    initialization_vector: encrypted.initialization_vector.clone(),
                    ciphertext_mnemonic: encrypted.ciphertext_mnemonic.clone(),
                    key_salt: encrypted.key_salt.clone(),
                    kdf: Some(encrypted.kdf.into()),
                },
            },
        };
        let () = write_seed_file(&self.path, &seed_file).map_err(|err| {
            // Losing the publish race: another process created the wallet
            // between the check above and the write. Report the wallet as
            // already there, which is what the caller now sees on disk.
            if err.kind() == std::io::ErrorKind::AlreadyExists {
                error::InsertSeed::AlreadyExists
            } else {
                error::InsertSeed::Io(err)
            }
        })?;
        Ok(())
    }
}

fn read_seed_file(path: &Path) -> Result<Option<SeedFile>, error::ReadSeed> {
    let contents = match std::fs::read(path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(error::ReadSeedInner::Io(err).into()),
    };
    let seed_file: SeedFile =
        serde_json::from_slice(&contents).map_err(error::ReadSeedInner::Json)?;
    Ok(Some(seed_file))
}

/// Write the seed file atomically and durably: temp file (0600) + fsync +
/// hard link + directory fsync, so a crash can never leave a bad seed file.
///
/// Publishing with a link rather than a rename is what makes "there is no
/// seed yet" the kernel's decision: [`std::fs::hard_link`] fails with
/// [`std::io::ErrorKind::AlreadyExists`] if the seed file is already there,
/// where a rename would silently replace a seed another process just wrote.
/// An `AlreadyExists` out of here therefore means exactly that: someone else
/// got their seed in first.
fn write_seed_file(path: &Path, seed_file: &SeedFile) -> Result<(), std::io::Error> {
    let contents = serde_json::to_vec_pretty(seed_file).expect("seed file serialization is total");
    // Unique per write: two processes writing at once must not meet on the
    // same temp path, or one would publish the other's half-written seed.
    let tmp_path = {
        use rand::Rng as _;
        let mut nonce_bytes = [0u8; 8];
        rand::rng().fill_bytes(&mut nonce_bytes);
        let (pid, nonce) = (std::process::id(), hex::encode(nonce_bytes));
        path.with_extension(format!("json.{pid}.{nonce}.tmp"))
    };
    {
        // The 0600 only takes effect when the open actually creates the file.
        // An existing temp file must never be reused. A leftover from a
        // crashed write, or one planted by another process, would otherwise
        // decide the seed's permissions, or redirect the write through a
        // symlink. Unlink first, then create exclusively — if anything races
        // us back onto the path, `create_new` fails instead of writing the
        // seed somewhere it does not belong.
        match std::fs::remove_file(&tmp_path) {
            Ok(()) => (),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => (),
            Err(err) => return Err(err),
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        // Something racing us onto our own temp path is not the seed file
        // being taken; don't let it read back as "a wallet already exists".
        let mut file = options.open(&tmp_path).map_err(|err| {
            if err.kind() == std::io::ErrorKind::AlreadyExists {
                std::io::Error::other(format!("temp seed file {} is in use", tmp_path.display()))
            } else {
                err
            }
        })?;
        let () = std::io::Write::write_all(&mut file, &contents)?;
        let () = file.sync_all()?;
    }
    // First writer wins, decided by the kernel rather than by a check the
    // losers can slip past.
    let published = std::fs::hard_link(&tmp_path, path);
    // The temp file is a second on-disk copy of the seed either way; failing
    // to unlink it leaves a stale 0600 file, nothing worse.
    let _unlinked = std::fs::remove_file(&tmp_path);
    let () = published?;
    let dir = std::fs::File::open(path.parent().expect("seed file path has a parent"))?;
    dir.sync_all()
}

/// Move a legacy seed out of the pre-split `db.sqlite` into the seed file,
/// if one is there. Copy → verify → delete: the legacy rows are only deleted
/// once the written file has been read back and matches, so no crash point
/// loses the seed. Deleting the rows leaves the empty `wallet_seeds` table
/// for the producer to drop on its next open (see
/// `crate::block_producer::db`). A crash between write and delete leaves the
/// seed in both places; the next startup lands in the "file already matches"
/// branch and just re-runs the delete.
fn migrate_legacy_seed(path: &Path, data_dir: &Path) -> Result<(), error::InitSeedStore> {
    let legacy_db = data_dir.join("db.sqlite");
    if !legacy_db.exists() {
        return Ok(());
    }
    let conn = Connection::open_with_flags(
        &legacy_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let Some(legacy_seed) = read_legacy_seed(&conn)? else {
        return Ok(());
    };

    match read_seed_file(path).map_err(|err| error::InitSeedStore::ReadSeedFile {
        seed_file: path.to_owned(),
        source: err,
    })? {
        // Crash recovery: the seed was already copied, only the legacy delete
        // is missing.
        Some(existing) if existing.seed == legacy_seed => (),
        Some(_) => {
            return Err(error::InitSeedStore::LegacySeedMismatch {
                legacy_db,
                seed_file: path.to_owned(),
            });
        }
        None => {
            // Migrating a plaintext seed that would not parse back would
            // trade a recoverable legacy row for a broken seed file. Check
            // before writing anything.
            if let StoredSeed::Plaintext { mnemonic } = &legacy_seed {
                let _: Mnemonic = Mnemonic::parse_in_normalized(Language::English, mnemonic)
                    .map_err(error::ParseMnemonic::from)?;
            }
            let created_at: Option<i64> = conn
                .query_row(
                    "SELECT CAST(strftime('%s', creation_time) AS INTEGER)
                     FROM wallet_seeds",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or_default();
            let seed_file = SeedFile {
                version: CURRENT_VERSION,
                // Legacy wallets have unknowable history: no birthday.
                birthday_height: None,
                created_at: created_at
                    .and_then(|secs| {
                        let secs = u64::try_from(secs).ok()?;
                        Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs))
                    })
                    .unwrap_or_else(std::time::SystemTime::now),
                seed: legacy_seed,
            };
            let () = write_seed_file(path, &seed_file)?;
            let written =
                read_seed_file(path).map_err(|err| error::InitSeedStore::ReadSeedFile {
                    seed_file: path.to_owned(),
                    source: err,
                })?;
            if written.as_ref() != Some(&seed_file) {
                return Err(error::InitSeedStore::VerifySeedFile {
                    seed_file: path.to_owned(),
                });
            }
        }
    }

    let deleted = conn.execute("DELETE FROM wallet_seeds", [])?;
    if deleted > 0 {
        tracing::info!(
            "Migrated the wallet seed from the legacy {} into {}",
            legacy_db.display(),
            path.display(),
        );
    }
    Ok(())
}

/// Read the seed from the legacy `wallet_seeds` table, if the table exists
/// and holds one.
fn read_legacy_seed(conn: &Connection) -> Result<Option<StoredSeed>, error::InitSeedStore> {
    let has_table: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master
          WHERE type = 'table' AND name = 'wallet_seeds')",
        [],
        |row| row.get(0),
    )?;
    if !has_table {
        return Ok(None);
    }
    let mut statement = conn.prepare(
        "SELECT plaintext_mnemonic, initialization_vector,
                ciphertext_mnemonic, key_salt FROM wallet_seeds",
    )?;
    let row = statement.query_row([], |row| {
        let plaintext_mnemonic: Option<String> = row.get("plaintext_mnemonic")?;
        let iv: Option<Vec<u8>> = row.get("initialization_vector")?;
        let ciphertext: Option<Vec<u8>> = row.get("ciphertext_mnemonic")?;
        let key_salt: Option<Vec<u8>> = row.get("key_salt")?;
        Ok((plaintext_mnemonic, iv, ciphertext, key_salt))
    });
    match row {
        Ok((Some(mnemonic), None, None, None)) => Ok(Some(StoredSeed::Plaintext { mnemonic })),
        Ok((None, Some(initialization_vector), Some(ciphertext_mnemonic), Some(key_salt))) => {
            Ok(Some(StoredSeed::Encrypted {
                initialization_vector,
                ciphertext_mnemonic,
                key_salt,
                kdf: None,
            }))
        }
        // Don't print the actual contents, just indicate which values are set.
        Ok((plaintext_mnemonic, iv, ciphertext, key_salt)) => {
            Err(error::InitSeedStore::InvalidLegacySeed {
                plaintext_mnemonic_is_some: plaintext_mnemonic.is_some(),
                iv_is_some: iv.is_some(),
                ciphertext_is_some: ciphertext.is_some(),
                key_salt_is_some: key_salt.is_some(),
            })
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

#[cfg(test)]
mod tests {
    use bdk_wallet::bip39::{Language, Mnemonic};

    use super::{EncryptedMnemonic, KdfParams, Seed, SeedStore, error};

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bip300301-seed-store-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id(),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // The all-zeros BIP39 test vector; any valid mnemonic works here.
    const TEST_MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon \
         abandon abandon abandon abandon abandon about";

    /// Simulate a pre-split data dir: a `db.sqlite` with a `wallet_seeds`
    /// table. Returns the connection so tests can insert seed rows.
    fn write_legacy_db(dir: &std::path::Path) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(dir.join("db.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS wallet_seeds
                (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 plaintext_mnemonic TEXT,
                 initialization_vector BLOB,
                 ciphertext_mnemonic BLOB,
                 key_salt BLOB,
                 needs_passphrase BOOLEAN NOT NULL DEFAULT FALSE,
                 creation_time DATETIME NOT NULL DEFAULT (DATETIME('now'))
                );",
        )
        .unwrap();
        conn
    }

    fn legacy_seed_rows(dir: &std::path::Path) -> i64 {
        let conn = rusqlite::Connection::open(dir.join("db.sqlite")).unwrap();
        conn.query_row("SELECT COUNT(*) FROM wallet_seeds", [], |row| row.get(0))
            .unwrap()
    }

    /// Upgrading a pre-split deployment: a plaintext legacy seed is moved into
    /// `seed.json` on startup, the legacy rows are deleted, and the migration
    /// is idempotent across restarts.
    #[tokio::test]
    async fn legacy_plaintext_seed_migrates_automatically() {
        let dir = temp_dir("migrate-plaintext");
        let conn = write_legacy_db(&dir);
        conn.execute(
            "INSERT INTO wallet_seeds (plaintext_mnemonic) VALUES (?)",
            [TEST_MNEMONIC],
        )
        .unwrap();
        drop(conn);

        for _ in 0..2 {
            let store = SeedStore::new(&dir).unwrap();
            let mnemonic = store
                .read_mnemonic()
                .await
                .unwrap()
                .expect("legacy seed must be migrated")
                .left()
                .expect("legacy seed was plaintext");
            assert_eq!(mnemonic.to_string(), TEST_MNEMONIC);
        }
        assert_eq!(legacy_seed_rows(&dir), 0, "legacy rows must be deleted");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An encrypted legacy seed migrates without needing the passphrase: the
    /// ciphertext, IV and salt move across verbatim.
    #[tokio::test]
    async fn legacy_encrypted_seed_migrates_automatically() {
        let dir = temp_dir("migrate-encrypted");
        let conn = write_legacy_db(&dir);
        conn.execute(
            "INSERT INTO wallet_seeds
                (initialization_vector, ciphertext_mnemonic, key_salt)
             VALUES (?, ?, ?)",
            (&[1u8; 12][..], &[2u8; 64][..], &[3u8; 16][..]),
        )
        .unwrap();
        drop(conn);

        let store = SeedStore::new(&dir).unwrap();
        let encrypted = store
            .read_mnemonic()
            .await
            .unwrap()
            .expect("legacy seed must be migrated")
            .right()
            .expect("legacy seed was encrypted");
        assert_eq!(encrypted.initialization_vector, vec![1u8; 12]);
        assert_eq!(encrypted.ciphertext_mnemonic, vec![2u8; 64]);
        assert_eq!(encrypted.key_salt, vec![3u8; 16]);
        assert_eq!(legacy_seed_rows(&dir), 0, "legacy rows must be deleted");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Crash recovery: `seed.json` was written but the process died before the
    /// legacy rows were deleted. The next startup must recognize the matching
    /// duplicate and finish the delete instead of erroring.
    #[tokio::test]
    async fn interrupted_migration_finishes_on_next_startup() {
        let dir = temp_dir("migrate-interrupted");
        let conn = write_legacy_db(&dir);
        conn.execute(
            "INSERT INTO wallet_seeds (plaintext_mnemonic) VALUES (?)",
            [TEST_MNEMONIC],
        )
        .unwrap();
        drop(conn);

        // First startup migrated...
        let _store = SeedStore::new(&dir).unwrap();
        // ...but "crashed before the delete": put the legacy row back.
        let conn = write_legacy_db(&dir);
        conn.execute(
            "INSERT INTO wallet_seeds (plaintext_mnemonic) VALUES (?)",
            [TEST_MNEMONIC],
        )
        .unwrap();
        drop(conn);

        let store = SeedStore::new(&dir).unwrap();
        assert!(store.read_mnemonic().await.unwrap().is_some());
        assert_eq!(legacy_seed_rows(&dir), 0, "delete must be re-run");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A legacy seed that differs from the one already in `seed.json` is a
    /// state the wallet cannot resolve on its own: refuse to start, and leave
    /// both seeds untouched.
    #[tokio::test]
    async fn mismatched_legacy_seed_aborts_startup() {
        let dir = temp_dir("migrate-mismatch");
        let store = SeedStore::new(&dir).unwrap();
        let mnemonic = Mnemonic::parse_in(Language::English, TEST_MNEMONIC).unwrap();
        store
            .insert_seed(Seed::Plaintext(&mnemonic), None)
            .await
            .unwrap();
        drop(store);

        // A different valid mnemonic in the legacy store.
        let conn = write_legacy_db(&dir);
        conn.execute(
            "INSERT INTO wallet_seeds (plaintext_mnemonic) VALUES (?)",
            ["legal winner thank year wave sausage worth useful legal winner thank yellow"],
        )
        .unwrap();
        drop(conn);

        let Err(err) = SeedStore::new(&dir) else {
            panic!("mismatched legacy seed must abort")
        };
        assert!(matches!(
            err,
            error::InitSeedStore::LegacySeedMismatch { .. }
        ));
        assert_eq!(legacy_seed_rows(&dir), 1, "legacy seed must be untouched");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Fresh deployments: the producer creates `db.sqlite` (and immediately
    /// drops the empty legacy `wallet_seeds` slot) before the wallet opens its
    /// seed store. The migration must accept a `db.sqlite` that has no
    /// `wallet_seeds` table at all.
    #[tokio::test]
    async fn fresh_producer_db_does_not_trip_migration() {
        let dir = temp_dir("fresh-producer-db");
        let _producer_db = crate::block_producer::Db::new(&dir).unwrap();
        let store = SeedStore::new(&dir).unwrap();
        assert!(store.read_mnemonic().await.unwrap().is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The full upgrade sequence for a wallet deployment, in real startup
    /// order: the producer opens `db.sqlite` first and must keep the
    /// un-migrated seed; the wallet then migrates it into `seed.json`; on the
    /// next boot the producer drops the emptied legacy table.
    #[tokio::test]
    async fn full_upgrade_sequence_moves_seed_and_drops_legacy_table() {
        let dir = temp_dir("full-upgrade");
        {
            let conn = rusqlite::Connection::open(dir.join("db.sqlite")).unwrap();
            conn.execute_batch(crate::block_producer::db::migration_tests::LEGACY_V7_SCHEMA)
                .unwrap();
            conn.execute(
                "INSERT INTO wallet_seeds (plaintext_mnemonic) VALUES (?)",
                [TEST_MNEMONIC],
            )
            .unwrap();
        }

        // First boot: the producer opens (and migrates) db.sqlite before the
        // wallet initializes, exactly as in `app/main.rs`.
        drop(crate::block_producer::Db::new(&dir).unwrap());
        let store = SeedStore::new(&dir).unwrap();
        assert!(store.read_mnemonic().await.unwrap().is_some());
        drop(store);

        // Second boot: the producer reaps the now-empty legacy table.
        drop(crate::block_producer::Db::new(&dir).unwrap());
        let conn = rusqlite::Connection::open(dir.join("db.sqlite")).unwrap();
        let has_table: bool = conn
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM sqlite_master
                  WHERE type = 'table' AND name = 'wallet_seeds')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!has_table, "emptied legacy table must be dropped");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A second seed insert must be rejected with `AlreadyExists`, so a repeated
    /// `CreateWallet` surfaces `AlreadyExists` rather than storing two seeds.
    #[tokio::test]
    async fn insert_seed_rejects_a_second_seed() {
        let dir = temp_dir("already-exists");
        let store = SeedStore::new(&dir).unwrap();
        let mnemonic = Mnemonic::parse_in(Language::English, TEST_MNEMONIC).unwrap();

        store
            .insert_seed(Seed::Plaintext(&mnemonic), None)
            .await
            .unwrap();
        let err = store
            .insert_seed(Seed::Plaintext(&mnemonic), None)
            .await
            .expect_err("second insert must fail");
        assert!(matches!(err, error::InsertSeed::AlreadyExists));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Stores constructed independently — as two processes would be — must
    /// not both accept a seed. The in-process lock cannot see them, so the
    /// publish itself has to be first-writer-wins: exactly one insert
    /// succeeds, the rest are told the wallet already exists, and the seed
    /// left on disk is the winner's.
    #[test]
    fn concurrent_stores_do_not_overwrite_each_others_seed() {
        // Distinct BIP39 test vectors, so a lost race shows up as the wrong
        // seed and not just as a second `Ok`.
        const MNEMONICS: [&str; 4] = [
            TEST_MNEMONIC,
            "legal winner thank year wave sausage worth useful legal winner thank yellow",
            "letter advice cage absurd amount doctor acoustic avoid letter advice cage above",
            "zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo zoo wrong",
        ];

        let seed_dir = temp_dir("concurrent-insert");
        let start = std::sync::Barrier::new(MNEMONICS.len());
        let (dir, barrier) = (seed_dir.as_path(), &start);
        let results: Vec<Result<String, error::InsertSeed>> = std::thread::scope(|scope| {
            let handles: Vec<_> = MNEMONICS
                .into_iter()
                .map(|mnemonic| {
                    scope.spawn(move || {
                        let mnemonic = Mnemonic::parse_in(Language::English, mnemonic).unwrap();
                        let store = SeedStore::new(dir).unwrap();
                        // Line the inserts up on the check they must not all
                        // pass.
                        let _released = barrier.wait();
                        let inserted = store.insert_seed(Seed::Plaintext(&mnemonic), None);
                        futures::executor::block_on(inserted).map(|()| mnemonic.to_string())
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect()
        });

        let winners: Vec<&String> = results
            .iter()
            .filter_map(|result| result.as_ref().ok())
            .collect();
        assert_eq!(winners.len(), 1, "exactly one insert may succeed");
        let winner = winners[0].clone();
        for result in &results {
            assert!(
                matches!(result, Ok(_) | Err(error::InsertSeed::AlreadyExists)),
                "the losers must be told the wallet already exists"
            );
        }

        let store = SeedStore::new(dir).unwrap();
        let stored = futures::executor::block_on(store.read_mnemonic())
            .unwrap()
            .expect("the winner's seed must be on disk")
            .left()
            .expect("the seed was plaintext");
        assert_eq!(stored.to_string(), winner, "the winner's seed must survive");
        std::fs::remove_dir_all(&seed_dir).ok();
    }

    /// A recorded birthday round-trips; absence stays absent; and a seed
    /// file written before the field existed reads back as `None`.
    #[tokio::test]
    async fn birthday_height_roundtrip_and_backcompat() {
        let dir = temp_dir("birthday");
        let store = SeedStore::new(&dir).unwrap();
        let mnemonic = Mnemonic::parse_in(Language::English, TEST_MNEMONIC).unwrap();
        store
            .insert_seed(Seed::Plaintext(&mnemonic), Some(958_537))
            .await
            .unwrap();
        assert_eq!(
            store.read_birthday_height().await.unwrap(),
            Some(958_537),
            "birthday must round-trip"
        );
        drop(store);
        std::fs::remove_dir_all(&dir).ok();

        // Restored seed: no birthday.
        let dir = temp_dir("birthday-none");
        let store = SeedStore::new(&dir).unwrap();
        store
            .insert_seed(Seed::Plaintext(&mnemonic), None)
            .await
            .unwrap();
        assert_eq!(store.read_birthday_height().await.unwrap(), None);

        // Pre-birthday seed file (field absent entirely) must parse as None.
        let raw = std::fs::read_to_string(dir.join("seed.json")).unwrap();
        assert!(
            !raw.contains("birthday_height"),
            "None must not be serialized"
        );
        assert!(store.read_mnemonic().await.unwrap().is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The seed file must not be readable by other users.
    #[cfg(unix)]
    #[tokio::test]
    async fn seed_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = temp_dir("permissions");
        let store = SeedStore::new(&dir).unwrap();
        let mnemonic = Mnemonic::parse_in(Language::English, TEST_MNEMONIC).unwrap();
        store
            .insert_seed(Seed::Plaintext(&mnemonic), None)
            .await
            .unwrap();

        let mode = std::fs::metadata(dir.join("seed.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A world-readable temp file left over from a crashed write (or planted)
    /// must not become the seed file's permissions.
    #[cfg(unix)]
    #[tokio::test]
    async fn stale_temp_file_does_not_widen_seed_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = temp_dir("stale-tmp");
        let tmp_path = dir.join("seed.json.tmp");
        std::fs::write(&tmp_path, b"stale").unwrap();
        std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o666)).unwrap();

        let store = SeedStore::new(&dir).unwrap();
        let mnemonic = Mnemonic::parse_in(Language::English, TEST_MNEMONIC).unwrap();
        store
            .insert_seed(Seed::Plaintext(&mnemonic), None)
            .await
            .unwrap();

        let mode = std::fs::metadata(dir.join("seed.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "stale temp file must not be reused");
        assert_eq!(
            store
                .read_mnemonic()
                .await
                .unwrap()
                .unwrap()
                .left()
                .unwrap()
                .to_string(),
            TEST_MNEMONIC,
            "the stale contents must not survive into the seed file"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An encrypted seed records the cost it was stretched with, and a seed
    /// file written before that field existed reads back as the legacy cost
    /// and still decrypts.
    #[tokio::test]
    async fn encrypted_seed_round_trips_its_kdf_cost() {
        let mnemonic = Mnemonic::parse_in(Language::English, TEST_MNEMONIC).unwrap();

        let dir = temp_dir("kdf-current");
        let store = SeedStore::new(&dir).unwrap();
        let encrypted = EncryptedMnemonic::encrypt(&mnemonic, "pw", KdfParams::CURRENT).unwrap();
        store
            .insert_seed(Seed::Encrypted(&encrypted), None)
            .await
            .unwrap();
        let read = store
            .read_mnemonic()
            .await
            .unwrap()
            .unwrap()
            .right()
            .expect("seed was encrypted");
        assert_eq!(read.kdf, KdfParams::CURRENT, "the cost must round-trip");
        assert_eq!(read.decrypt("pw").unwrap().to_string(), TEST_MNEMONIC);
        std::fs::remove_dir_all(&dir).ok();

        // Now a seed exactly as an older build left it: stretched with the
        // old cost, and with no `kdf` field to say so.
        let dir = temp_dir("kdf-legacy");
        let store = SeedStore::new(&dir).unwrap();
        let legacy = EncryptedMnemonic::encrypt(&mnemonic, "pw", KdfParams::LEGACY).unwrap();
        store
            .insert_seed(Seed::Encrypted(&legacy), None)
            .await
            .unwrap();
        let path = dir.join("seed.json");
        let mut seed_file: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            seed_file["seed"]
                .as_object_mut()
                .unwrap()
                .remove("kdf")
                .is_some(),
            "the field under test must be present to begin with"
        );
        std::fs::write(&path, serde_json::to_vec_pretty(&seed_file).unwrap()).unwrap();

        let read = store
            .read_mnemonic()
            .await
            .unwrap()
            .unwrap()
            .right()
            .expect("seed was encrypted");
        assert_eq!(
            read.kdf,
            KdfParams::LEGACY,
            "an absent cost must mean the pre-bump default"
        );
        assert_eq!(
            read.decrypt("pw").unwrap().to_string(),
            TEST_MNEMONIC,
            "a wallet created before the bump must still unlock"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
