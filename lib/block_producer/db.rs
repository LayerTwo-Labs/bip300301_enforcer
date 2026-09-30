//! The policy store for drivechain mining decisions

use std::{collections::HashMap, path::Path};

use bitcoin::hashes::{Hash as _, sha256d};
use fallible_iterator::{FallibleIterator as _, IteratorExt as _};
use rusqlite::{Connection, OptionalExtension as _};

use crate::{
    block_producer::error,
    types::{
        AckAllProposalsPolicy, BlindedM6, M6id, SidechainAck, SidechainNumber, SidechainProposal,
        WithdrawalBundleEventKind, WithdrawalBundlePolicy,
    },
    validator::Validator,
};

/// Bundle proposals for a single sidechain, as stored (no validator filtering).
pub(crate) type StoredBundleProposals = Vec<(M6id, BlindedM6<'static>)>;

impl rusqlite::types::ToSql for AckAllProposalsPolicy {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        let name = match self {
            Self::None => "none",
            Self::NewSlots => "new_slots",
            Self::All => "all",
        };
        Ok(rusqlite::types::ToSqlOutput::Borrowed(name.into()))
    }
}

impl rusqlite::types::FromSql for AckAllProposalsPolicy {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        match value.as_str()? {
            "none" => Ok(Self::None),
            "new_slots" => Ok(Self::NewSlots),
            "all" => Ok(Self::All),
            other => Err(rusqlite::types::FromSqlError::Other(Box::new(
                error::UnknownStoredAckPolicy(other.to_owned()),
            ))),
        }
    }
}

impl rusqlite::types::ToSql for WithdrawalBundlePolicy {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        let name = match self {
            Self::None => "none",
            Self::Known => "known",
            Self::All => "all",
            Self::Alarm => "alarm",
        };
        Ok(rusqlite::types::ToSqlOutput::Borrowed(name.into()))
    }
}

impl rusqlite::types::FromSql for WithdrawalBundlePolicy {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
        match value.as_str()? {
            "none" => Ok(Self::None),
            "known" => Ok(Self::Known),
            "all" => Ok(Self::All),
            "alarm" => Ok(Self::Alarm),
            other => Err(rusqlite::types::FromSqlError::Other(Box::new(
                error::UnknownStoredAckPolicy(other.to_owned()),
            ))),
        }
    }
}

/// The drivechain policy database.
pub struct Db {
    conn: tokio::sync::Mutex<Connection>,
}

impl Db {
    pub fn new(data_dir: &Path) -> Result<Self, error::InitDbConnection> {
        use rusqlite_migration::{M, Migrations};
        // This DB (`db.sqlite`) predates the wallet/producer split: existing
        // deployments already carry the pre-split wallet's migration history at
        // `user_version` 7, and `rusqlite_migration` only tracks the version
        // counter, not which statements produced it. The list below must
        // therefore keep every pre-split slot verbatim, in order — including
        // `wallet_seeds`, which the producer itself never touches — and only
        // ever append.
        let migrations = Migrations::new(vec![
            M::up(
                "CREATE TABLE sidechain_proposals
               (sidechain_number INTEGER NOT NULL,
                data_hash BLOB NOT NULL,
                data BLOB NOT NULL,
                UNIQUE(sidechain_number, data_hash));",
            ),
            M::up(
                "CREATE TABLE sidechain_acks
               (number INTEGER NOT NULl,
                data_hash BLOB NOT NULL,
                UNIQUE(number, data_hash));",
            ),
            M::up(
                "CREATE TABLE bundle_proposals
               (sidechain_number INTEGER NOT NULL,
                bundle_hash BLOB NOT NULL,
                bundle_tx BLOB NOT NULL,
                UNIQUE(sidechain_number, bundle_hash));",
            ),
            M::up(
                "CREATE TABLE bundle_acks
               (sidechain_number INTEGER NOT NULL,
                bundle_hash BLOB NOT NULL,
                UNIQUE(sidechain_number, bundle_hash));",
            ),
            M::up(
                "CREATE TABLE bmm_requests
                (sidechain_number INTEGER NOT NULL,
                 prev_block_hash BLOB NOT NULL,
                 side_block_hash BLOB NOT NULL,
                 UNIQUE(sidechain_number, prev_block_hash));",
            ),
            // Legacy slot: the pre-split wallet kept its seed here. The seed
            // now lives in the wallet's own `seed.json`, and the wallet
            // migrates it out automatically on startup (see
            // `crate::wallet::seed_store`), but this slot has to stay so that
            // fresh and pre-split DBs agree on what `user_version` N means.
            // The producer never reads or writes this table, and drops it
            // again once it holds no seed (see
            // `drop_legacy_wallet_seeds_if_empty`).
            M::up(
                "CREATE TABLE wallet_seeds
                (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 plaintext_mnemonic TEXT,

                 -- encryption values
                 initialization_vector BLOB,
                 ciphertext_mnemonic BLOB,
                 key_salt BLOB,

                 -- boolean that indicates if the wallet uses a BIP39 passphrase
                 needs_passphrase BOOLEAN NOT NULL DEFAULT FALSE,

                 -- timestamp of the creation of the seed
                 creation_time DATETIME NOT NULL DEFAULT (DATETIME('now'))
                );",
            ),
            M::up(
                "CREATE TABLE bmm_requests_undo
                (block_hash BLOB NOT NULL,
                 sidechain_number INTEGER NOT NULL,
                 prev_block_hash BLOB NOT NULL,
                 side_block_hash BLOB NOT NULL);",
            ),
            // Single-row settings table
            M::up(
                "CREATE TABLE block_producer_settings
                (id INTEGER PRIMARY KEY CHECK (id = 0),
                 ack_all_proposals BOOLEAN NOT NULL);
                 INSERT INTO block_producer_settings (id, ack_all_proposals)
                 VALUES (0, TRUE);",
            ),
            M::up(
                "CREATE TABLE block_producer_settings_new
                (id INTEGER PRIMARY KEY CHECK (id = 0),
                 ack_policy TEXT NOT NULL
                   CHECK (ack_policy IN ('none', 'new_slots', 'all')));
                 INSERT INTO block_producer_settings_new (id, ack_policy)
                 SELECT id, CASE WHEN ack_all_proposals THEN 'new_slots' ELSE 'none' END
                 FROM block_producer_settings;
                 DROP TABLE block_producer_settings;
                 ALTER TABLE block_producer_settings_new
                   RENAME TO block_producer_settings;",
            ),
            M::up(
                "ALTER TABLE block_producer_settings
                 ADD COLUMN bundle_policy TEXT NOT NULL DEFAULT 'known'
                   CHECK (bundle_policy IN ('none', 'known', 'all', 'alarm'));
                 UPDATE block_producer_settings
                 SET bundle_policy = CASE WHEN ack_policy = 'none' THEN 'none' ELSE 'known' END;",
            ),
            // Dead since BMM bids became plain high-fee M8 transactions
            M::up(
                "DROP TABLE bmm_requests;
                 DROP TABLE bmm_requests_undo;",
            ),
            M::up(
                "ALTER TABLE sidechain_proposals ADD COLUMN mined_in TEXT;
                 ALTER TABLE sidechain_proposals ADD COLUMN queued_at INTEGER;
                 ALTER TABLE bundle_proposals ADD COLUMN settled_in TEXT;
                 ALTER TABLE bundle_proposals ADD COLUMN queued_at INTEGER;
                 ALTER TABLE bundle_proposals ADD COLUMN paid_out INTEGER;
                 ALTER TABLE block_producer_settings ADD COLUMN policy_tip TEXT;",
            ),
            // Settled bundles are kept as tombstones, and `settled_in` sits
            // after the bundle tx, so finding the pending ones without this
            // reads every bundle submitted.
            M::up(
                "CREATE INDEX pending_bundle_proposals
                 ON bundle_proposals (sidechain_number) WHERE settled_in IS NULL;",
            ),
        ]);

        let path = data_dir.join("db.sqlite");
        let mut conn = Connection::open(path.clone())?;
        tracing::info!("Created database connection to {}", path.display());
        migrations.to_latest(&mut conn)?;
        tracing::debug!("Ran migrations on {}", path.display());
        let () = drop_legacy_wallet_seeds_if_empty(&mut conn)?;
        Ok(Self {
            conn: tokio::sync::Mutex::new(conn),
        })
    }

    /// Start a DB that has no cursor yet, fresh or from before there was one,
    /// at the validator's tip. Must run before the validator syncs further:
    /// the producer used to delete rows once their proposal or bundle was
    /// mined or settled, so whatever rows an old DB holds are pending as of
    /// the last block it saw, which is that tip. Queueing them at its height
    /// keeps a proposal or bundle that was resubmitted after an earlier M1 or
    /// settlement pending, as it was.
    pub(crate) fn start_cursor(
        &mut self,
        validator: &Validator,
    ) -> Result<(), error::InitDbConnection> {
        let tx = self.conn.get_mut().transaction()?;
        let policy_tip: Option<String> = tx.query_row(
            "SELECT policy_tip FROM block_producer_settings",
            [],
            |row| row.get(0),
        )?;
        if policy_tip.is_some() {
            return Ok(());
        }
        let Some(tip) = validator.try_get_mainchain_tip()? else {
            return Ok(());
        };
        let tip_height = validator.get_header_info(&tip)?.height;
        tx.execute(
            "UPDATE block_producer_settings SET policy_tip = ?1",
            [tip.to_string()],
        )?;
        tx.execute(
            "UPDATE sidechain_proposals SET queued_at = ?1 WHERE queued_at IS NULL",
            [tip_height],
        )?;
        tx.execute(
            "UPDATE bundle_proposals SET queued_at = ?1 WHERE queued_at IS NULL",
            [tip_height],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Sidechain proposals *we* authored. Not yet on the chain, so not yet
    /// votable — these become M1s in the coinbase.
    pub(crate) async fn get_our_sidechain_proposals(
        &self,
        validator: &Validator,
    ) -> Result<Vec<SidechainProposal>, error::Reconcile> {
        self.reconciled(validator, |tx, _tip_height| {
            let mut statement = tx.prepare(
                "SELECT sidechain_number, data FROM sidechain_proposals WHERE mined_in IS NULL",
            )?;
            let proposals = statement
                .query_map([], |row| {
                    let data: Vec<u8> = row.get(1)?;
                    let sidechain_number: u8 = row.get::<_, u8>(0)?;
                    Ok(SidechainProposal {
                        sidechain_number: sidechain_number.into(),
                        description: data.into(),
                    })
                })?
                .collect::<Result<_, _>>()?;
            Ok(proposals)
        })
        .await
    }

    pub async fn get_sidechain_acks(&self) -> Result<Vec<SidechainAck>, rusqlite::Error> {
        // Satisfy clippy with a single function call per lock
        let with_connection = |connection: &Connection| -> Result<_, _> {
            let mut statement =
                connection.prepare("SELECT number, data_hash FROM sidechain_acks")?;
            let rows = statement
                .query_map([], |row| {
                    let description_hash: [u8; 32] = row.get(1)?;
                    Ok(SidechainAck {
                        sidechain_number: SidechainNumber(row.get(0)?),
                        description_hash: sha256d::Hash::from_byte_array(description_hash),
                    })
                })?
                .collect::<Result<_, _>>()?;
            Ok(rows)
        };
        let connection = self.conn.lock().await;
        with_connection(&connection)
    }

    /// Bundle proposals as stored, with no validator filtering. Callers that need
    /// the active-sidechain filter want [`super::BlockProducer::get_bundle_proposals`].
    pub(crate) async fn get_bundle_proposals(
        &self,
        validator: &Validator,
    ) -> Result<HashMap<SidechainNumber, StoredBundleProposals>, error::GetBundleProposals> {
        self.reconciled(validator, |tx, _tip_height| {
            let mut statement = tx.prepare(
                "SELECT sidechain_number, bundle_hash, bundle_tx FROM bundle_proposals \
                 WHERE settled_in IS NULL",
            )?;
            let mut bundle_proposals = HashMap::<_, Vec<_>>::new();
            let () = statement
                .query_map([], |row| {
                    let sidechain_number = SidechainNumber(row.get(0)?);
                    let m6id_bytes: [u8; 32] = row.get(1)?;
                    let m6id = M6id::from(m6id_bytes);
                    let bundle_tx_bytes: Vec<u8> = row.get(2)?;
                    Ok((sidechain_number, m6id, bundle_tx_bytes))
                })?
                .transpose_into_fallible()
                .map_err(error::GetBundleProposals::from)
                .for_each(|(sidechain_number, m6id, bundle_tx_bytes)| {
                    let bundle_proposal_tx = BlindedM6::deserialize(&bundle_tx_bytes)?;
                    bundle_proposals
                        .entry(sidechain_number)
                        .or_default()
                        .push((m6id, bundle_proposal_tx));
                    Ok(())
                })?;
            Ok(bundle_proposals)
        })
        .await
    }

    pub async fn ack_sidechain(
        &self,
        sidechain_number: SidechainNumber,
        data_hash: sha256d::Hash,
    ) -> Result<(), rusqlite::Error> {
        let sidechain_number: u8 = sidechain_number.into();
        let data_hash: &[u8; 32] = data_hash.as_byte_array();
        let connection = self.conn.lock().await;
        connection.execute(
            "INSERT INTO sidechain_acks (number, data_hash) VALUES (?1, ?2)",
            (sidechain_number, data_hash),
        )?;
        drop(connection);
        Ok(())
    }

    /// Persists a sidechain proposal. On regtest it is picked up by the next
    /// block generation; on a PoW network it goes out in the next template.
    pub(crate) async fn propose_sidechain(
        &self,
        validator: &Validator,
        proposal: &SidechainProposal,
    ) -> Result<(), error::Reconcile> {
        let sidechain_number: u8 = proposal.sidechain_number.into();
        let data_hash = proposal.description.sha256d_hash().to_byte_array();
        self.reconciled(validator, |tx, tip_height| {
            // Re-proposing one that was already mined queues it again, past
            // that M1. A pending duplicate still trips the UNIQUE constraint
            // below.
            let requeued = tx.execute(
                "UPDATE sidechain_proposals SET mined_in = NULL, queued_at = ?3 \
                 WHERE sidechain_number = ?1 AND data_hash = ?2 AND mined_in IS NOT NULL",
                (sidechain_number, data_hash, tip_height),
            )?;
            if requeued == 0 {
                tx.execute(
                    "INSERT INTO sidechain_proposals (sidechain_number, data_hash, data, queued_at) VALUES (?1, ?2, ?3, ?4)",
                    (sidechain_number, data_hash, &proposal.description.0, tip_height),
                )?;
            }
            Ok(())
        })
        .await
    }

    /// Which sidechain proposals get ACKed on top of `sidechain_acks`.
    pub async fn get_ack_policy(&self) -> Result<AckAllProposalsPolicy, rusqlite::Error> {
        let connection = self.conn.lock().await;
        connection.query_row(
            "SELECT ack_policy FROM block_producer_settings WHERE id = 0",
            [],
            |row| row.get(0),
        )
    }

    pub async fn set_ack_policy(
        &self,
        policy: AckAllProposalsPolicy,
    ) -> Result<(), rusqlite::Error> {
        self.conn
            .lock()
            .await
            .execute(
                "UPDATE block_producer_settings SET ack_policy = ?1 WHERE id = 0",
                [policy],
            )
            .map(|_| ())
    }

    /// Which pending withdrawal bundles get upvoted on top of `bundle_acks`.
    pub async fn get_bundle_policy(&self) -> Result<WithdrawalBundlePolicy, rusqlite::Error> {
        let connection = self.conn.lock().await;
        connection.query_row(
            "SELECT bundle_policy FROM block_producer_settings WHERE id = 0",
            [],
            |row| row.get(0),
        )
    }

    pub async fn set_bundle_policy(
        &self,
        policy: WithdrawalBundlePolicy,
    ) -> Result<(), rusqlite::Error> {
        self.conn
            .lock()
            .await
            .execute(
                "UPDATE block_producer_settings SET bundle_policy = ?1 WHERE id = 0",
                [policy],
            )
            .map(|_| ())
    }

    /// Withdrawal bundles explicitly ACKed by the operator, whatever the
    /// standing policy says.
    pub async fn get_bundle_acks(&self) -> Result<Vec<(SidechainNumber, M6id)>, rusqlite::Error> {
        // Satisfy clippy with a single function call per lock
        let with_connection = |connection: &Connection| -> Result<_, rusqlite::Error> {
            let mut statement =
                connection.prepare("SELECT sidechain_number, bundle_hash FROM bundle_acks")?;
            let rows = statement
                .query_map([], |row| {
                    let m6id: [u8; 32] = row.get(1)?;
                    Ok((SidechainNumber(row.get(0)?), M6id::from(m6id)))
                })?
                .collect::<Result<_, _>>()?;
            Ok(rows)
        };
        let connection = self.conn.lock().await;
        with_connection(&connection)
    }

    pub async fn ack_bundle(
        &self,
        sidechain_number: SidechainNumber,
        m6id: M6id,
    ) -> Result<(), rusqlite::Error> {
        let sidechain_number: u8 = sidechain_number.into();
        self.conn
            .lock()
            .await
            .execute(
                "INSERT OR IGNORE INTO bundle_acks (sidechain_number, bundle_hash)
                 VALUES (?1, ?2)",
                (sidechain_number, m6id.0.to_byte_array()),
            )
            .map(|_| ())
    }

    /// Withdraw an explicit ACK, whether the operator NACKed it or the bundle
    /// simply stopped being pending.
    pub async fn delete_bundle_ack(
        &self,
        sidechain_number: SidechainNumber,
        m6id: M6id,
    ) -> Result<(), rusqlite::Error> {
        let sidechain_number: u8 = sidechain_number.into();
        self.conn
            .lock()
            .await
            .execute(
                "DELETE FROM bundle_acks WHERE sidechain_number = ?1 AND bundle_hash = ?2",
                (sidechain_number, m6id.0.to_byte_array()),
            )
            .map(|_| ())
    }

    pub async fn nack_sidechain(
        &self,
        sidechain_number: u8,
        data_hash: &[u8; 32],
    ) -> Result<(), rusqlite::Error> {
        self.conn.lock().await.execute(
            "DELETE FROM sidechain_acks WHERE number = ?1 AND data_hash = ?2",
            (sidechain_number, data_hash),
        )?;
        Ok(())
    }

    pub(crate) async fn put_withdrawal_bundle(
        &self,
        validator: &Validator,
        sidechain_number: SidechainNumber,
        blinded_m6: &BlindedM6<'static>,
    ) -> Result<M6id, error::PutWithdrawalBundle> {
        let m6id = blinded_m6.compute_m6id();
        // Always encode with rust-bitcoin. A zero-input bundle round-trips
        // because `BlindedM6::deserialize` reads this encoding back, and a
        // finalized M6 has a treasury input anyway.
        let tx_bytes = blinded_m6.serialize();
        self.reconciled(validator, |tx, tip_height| {
            let paid_out: Option<Option<bool>> = tx
                .query_row(
                    "SELECT paid_out FROM bundle_proposals \
                     WHERE sidechain_number = ?1 AND bundle_hash = ?2",
                    (sidechain_number.0, m6id.0.as_byte_array()),
                    |row| row.get(0),
                )
                .optional()?;
            if paid_out == Some(Some(true)) {
                return Err(error::PutWithdrawalBundle::AlreadyPaidOut { m6id });
            }
            tx.execute(
                // Resubmitting a failed bundle queues it again, past that
                // failure.
                "INSERT INTO bundle_proposals \
                 (sidechain_number, bundle_hash, bundle_tx, queued_at) \
                 VALUES (?1, ?2, ?3, ?4) \
                 ON CONFLICT (sidechain_number, bundle_hash) \
                 DO UPDATE SET settled_in = NULL, paid_out = NULL, queued_at = excluded.queued_at",
                (
                    sidechain_number.0,
                    m6id.0.as_byte_array(),
                    tx_bytes,
                    tip_height,
                ),
            )?;
            Ok(m6id)
        })
        .await
    }

    /// Run `f` against the proposal tables once they are in line with the
    /// chain, in the same transaction, passing the height of the tip they are
    /// in line with (`None` if the validator has no tip yet). Every read and
    /// write of those tables must go through here.
    async fn reconciled<T, E>(
        &self,
        validator: &Validator,
        f: impl FnOnce(&rusqlite::Transaction<'_>, Option<u32>) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<error::Reconcile> + From<rusqlite::Error>,
    {
        let mut connection = self.conn.lock().await;
        let tx = connection.transaction()?;
        let tip_height = reconcile(&tx, validator)?;
        let res = f(&tx, tip_height)?;
        tx.commit()?;
        drop(connection);
        Ok(res)
    }
}

/// Bring the policy tables' `mined_in`/`settled_in` marks in line with the
/// validator's chain, from the `policy_tip` cursor up to its current tip, and
/// return the tip's height. Without a usable cursor the marks are rebuilt from
/// the whole chain.
///
/// A row only takes marks from blocks above its `queued_at`, the tip height it
/// was (re)queued at: queueing it means past whatever is on the chain already,
/// and a rebuild or a reorg that reconnects that must not undo it.
fn reconcile(
    tx: &rusqlite::Transaction<'_>,
    validator: &Validator,
) -> Result<Option<u32>, error::Reconcile> {
    let policy_tip: Option<String> = tx.query_row(
        "SELECT policy_tip FROM block_producer_settings",
        [],
        |row| row.get(0),
    )?;
    let policy_tip = policy_tip
        .map(|policy_tip| policy_tip.parse::<bitcoin::BlockHash>())
        .transpose()?;
    let Some(diff) = validator.chain_diff(policy_tip)? else {
        return Ok(None);
    };
    if policy_tip == Some(diff.tip) {
        return Ok(Some(diff.tip_height));
    }
    if diff.rebuilt {
        tracing::warn!(?policy_tip, "no usable policy tip, rebuilding the marks");
        tx.execute("UPDATE sidechain_proposals SET mined_in = NULL", [])?;
        tx.execute(
            "UPDATE bundle_proposals SET settled_in = NULL, paid_out = NULL",
            [],
        )?;
    }
    for block_hash in &diff.disconnected {
        let block_hash = block_hash.to_string();
        tx.execute(
            "UPDATE sidechain_proposals SET mined_in = NULL WHERE mined_in = ?1",
            [&block_hash],
        )?;
        tx.execute(
            "UPDATE bundle_proposals SET settled_in = NULL, paid_out = NULL \
             WHERE settled_in = ?1",
            [&block_hash],
        )?;
    }
    let first_connected_height = diff.tip_height + 1 - diff.connected.len() as u32;
    for ((block_hash, block_info), height) in diff.connected.iter().zip(first_connected_height..) {
        let block_hash = block_hash.to_string();
        for (_vout, proposal) in block_info.sidechain_proposals() {
            let id = proposal.compute_id();
            tx.execute(
                "UPDATE sidechain_proposals SET mined_in = ?1
                 WHERE sidechain_number = ?2 AND data_hash = ?3
                    AND (queued_at IS NULL OR queued_at < ?4)",
                (
                    &block_hash,
                    id.sidechain_number.0,
                    id.description_hash.as_byte_array(),
                    height,
                ),
            )?;
        }
        for event in block_info.withdrawal_bundle_events() {
            let paid_out = match event.kind {
                WithdrawalBundleEventKind::Failed => false,
                WithdrawalBundleEventKind::Succeeded { .. } => true,
                WithdrawalBundleEventKind::Submitted => continue,
            };
            tx.execute(
                "UPDATE bundle_proposals SET settled_in = ?1, paid_out = ?5
                 WHERE sidechain_number = ?2 AND bundle_hash = ?3
                    AND (queued_at IS NULL OR queued_at < ?4)",
                (
                    &block_hash,
                    event.sidechain_id.0,
                    event.m6id.0.as_byte_array(),
                    height,
                    paid_out,
                ),
            )?;
        }
    }
    if !(diff.disconnected.is_empty() && diff.connected.is_empty()) {
        tracing::debug!(
            tip = %diff.tip,
            disconnected = diff.disconnected.len(),
            connected = diff.connected.len(),
            "reconciled policy DB with the chain",
        );
    }
    tx.execute(
        "UPDATE block_producer_settings SET policy_tip = ?1",
        [diff.tip.to_string()],
    )?;
    Ok(Some(diff.tip_height))
}

/// Drop the legacy `wallet_seeds` table once it no longer holds a seed. This
/// cannot be an appended migration: migrations run exactly once, before the
/// wallet has had a chance to migrate an existing seed into its `seed.json`
/// (the producer opens `db.sqlite` first, and a block producer has no
/// wallet at all — it must never delete a seed it cannot migrate). Running on
/// every open instead means: fresh DBs immediately lose the empty table the
/// legacy migration slot just created, and upgraded DBs lose it on the first
/// start after the wallet's automatic seed migration has emptied it.
fn drop_legacy_wallet_seeds_if_empty(conn: &mut Connection) -> Result<(), rusqlite::Error> {
    let tx = conn.transaction()?;
    let has_table: bool = tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master
          WHERE type = 'table' AND name = 'wallet_seeds')",
        [],
        |row| row.get(0),
    )?;
    if has_table {
        let empty: bool = tx.query_row(
            "SELECT NOT EXISTS (SELECT 1 FROM wallet_seeds)",
            [],
            |row| row.get(0),
        )?;
        if empty {
            tx.execute("DROP TABLE wallet_seeds", [])?;
            tracing::info!("Dropped empty legacy wallet_seeds table from the producer DB");
        }
    }
    tx.commit()
}

#[cfg(test)]
mod schema_tests {
    use super::Db;

    /// The producer's DB holds policy, never key material: it is opened by
    /// producers that have no wallet, so a seed table here would be a seed table
    /// in a process that should hold no keys. The legacy `wallet_seeds`
    /// migration slot still exists for `user_version` alignment with pre-split
    /// DBs, but on a fresh DB the empty table it creates is dropped again
    /// before `Db::new` returns.
    #[test]
    fn policy_db_holds_no_key_material() {
        let dir = std::env::temp_dir().join(format!(
            "bip300301-policy-db-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let tables: Vec<String> = {
            let db = Db::new(&dir).unwrap();
            let conn = db.conn.blocking_lock();
            let mut statement = conn
                .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
                .unwrap();
            let tables = statement
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap();
            drop(statement);
            drop(conn);
            tables
        };
        std::fs::remove_dir_all(&dir).ok();

        // Not vacuous: the policy tables really were created.
        for expected in ["sidechain_proposals", "sidechain_acks", "bundle_proposals"] {
            assert!(
                tables.iter().any(|table| table == expected),
                "expected policy table `{expected}` in the producer DB, got: {tables:?}"
            );
        }
        assert!(
            !tables.iter().any(|table| table.contains("seed")),
            "the block producer's DB must hold no seed table, got: {tables:?}"
        );
    }
}

#[cfg(test)]
pub(crate) mod migration_tests {
    use rusqlite::Connection;

    use super::{AckAllProposalsPolicy, Db, WithdrawalBundlePolicy};

    /// The exact schema the pre-split wallet's 7 migrations left behind
    /// (`lib/wallet/mod.rs` before the block producer was split out), with
    /// `user_version = 7`. Every deployed node upgrades from this.
    pub(crate) const LEGACY_V7_SCHEMA: &str = "
        CREATE TABLE sidechain_proposals
           (sidechain_number INTEGER NOT NULL,
            data_hash BLOB NOT NULL,
            data BLOB NOT NULL,
            UNIQUE(sidechain_number, data_hash));
        CREATE TABLE sidechain_acks
           (number INTEGER NOT NULl,
            data_hash BLOB NOT NULL,
            UNIQUE(number, data_hash));
        CREATE TABLE bundle_proposals
           (sidechain_number INTEGER NOT NULL,
            bundle_hash BLOB NOT NULL,
            bundle_tx BLOB NOT NULL,
            UNIQUE(sidechain_number, bundle_hash));
        CREATE TABLE bundle_acks
           (sidechain_number INTEGER NOT NULL,
            bundle_hash BLOB NOT NULL,
            UNIQUE(sidechain_number, bundle_hash));
        CREATE TABLE bmm_requests
            (sidechain_number INTEGER NOT NULL,
             prev_block_hash BLOB NOT NULL,
             side_block_hash BLOB NOT NULL,
             UNIQUE(sidechain_number, prev_block_hash));
        CREATE TABLE wallet_seeds
            (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             plaintext_mnemonic TEXT,
             initialization_vector BLOB,
             ciphertext_mnemonic BLOB,
             key_salt BLOB,
             needs_passphrase BOOLEAN NOT NULL DEFAULT FALSE,
             creation_time DATETIME NOT NULL DEFAULT (DATETIME('now'))
            );
        CREATE TABLE bmm_requests_undo
            (block_hash BLOB NOT NULL,
             sidechain_number INTEGER NOT NULL,
             prev_block_hash BLOB NOT NULL,
             side_block_hash BLOB NOT NULL);
        PRAGMA user_version = 7;
    ";

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bip300301-producer-migration-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_legacy_db(dir: &std::path::Path, schema: &str) {
        let conn = Connection::open(dir.join("db.sqlite")).unwrap();
        conn.execute_batch(schema).unwrap();
    }

    fn has_wallet_seeds_table(dir: &std::path::Path) -> bool {
        let conn = Connection::open(dir.join("db.sqlite")).unwrap();
        conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master
              WHERE type = 'table' AND name = 'wallet_seeds')",
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    /// Upgrading a deployed node: its `db.sqlite` sits at the pre-split
    /// wallet's `user_version = 7`, so only appended migrations may run — a
    /// reordered or trimmed migration list silently runs nothing (the exact
    /// bug this test was written for), which no fresh-dir test can catch. The
    /// producer must end up with `block_producer_settings` (read on every
    /// block template), while leaving an un-migrated wallet seed strictly alone — keyless producers
    /// in particular have no wallet that could ever migrate it. Only once the
    /// seed is gone may the legacy table be dropped.
    #[tokio::test]
    async fn legacy_v7_wallet_db_upgrades_cleanly() {
        let dir = temp_dir("v7");
        write_legacy_db(&dir, LEGACY_V7_SCHEMA);
        {
            let conn = Connection::open(dir.join("db.sqlite")).unwrap();
            conn.execute(
                "INSERT INTO wallet_seeds (plaintext_mnemonic) VALUES (?)",
                ["abandon abandon abandon abandon abandon abandon \
                  abandon abandon abandon abandon abandon about"],
            )
            .unwrap();
        }

        let db = Db::new(&dir).unwrap();
        assert_eq!(
            db.get_ack_policy().await.unwrap(),
            AckAllProposalsPolicy::NewSlots,
            "block_producer_settings must be created auto-ACKing new slots, \
             and no further: a replacement evicts a running sidechain"
        );
        assert_eq!(
            db.get_bundle_policy().await.unwrap(),
            WithdrawalBundlePolicy::Known,
            "a node that was voting on bundles must keep voting, but only for \
             the bundles it holds itself"
        );
        drop(db);

        assert!(has_wallet_seeds_table(&dir));
        let conn = Connection::open(dir.join("db.sqlite")).unwrap();
        let seed_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM wallet_seeds", [], |row| row.get(0))
            .unwrap();
        assert_eq!(seed_rows, 1, "the un-migrated seed must survive untouched");

        // Once the seed has been migrated out (here: deleted directly, in
        // production: by `crate::wallet::seed_store`), the next open drops
        // the emptied legacy table.
        conn.execute("DELETE FROM wallet_seeds", []).unwrap();
        drop(conn);
        drop(Db::new(&dir).unwrap());
        assert!(
            !has_wallet_seeds_table(&dir),
            "emptied legacy wallet_seeds table must be dropped"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
