//! The policy store for drivechain mining decisions

use std::{collections::HashMap, path::Path};

use bitcoin::hashes::{Hash as _, sha256d};
use fallible_iterator::{FallibleIterator as _, IteratorExt as _};
use rusqlite::Connection;

use crate::{
    block_producer::error,
    types::{
        BlindedM6, BmmCommitment, M6id, SidechainAck, SidechainNumber, SidechainProposal,
        SidechainProposalId,
    },
};

/// Bundle proposals for a single sidechain, as stored (no validator filtering).
pub(crate) type StoredBundleProposals = Vec<(M6id, BlindedM6<'static>)>;

/// Undo rows are kept for this many recently-produced blocks, so that blocks
/// which stay on the main chain (and are therefore never disconnected) don't
/// accumulate undo rows forever.
const UNDO_RETAINED_BLOCKS: i64 = 100;

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
                "CREATE TABLE bundle_proposals_undo
                (block_hash BLOB NOT NULL,
                 sidechain_number INTEGER NOT NULL,
                 bundle_hash BLOB NOT NULL,
                 bundle_tx BLOB NOT NULL);
                 CREATE TABLE sidechain_proposals_undo
                (block_hash BLOB NOT NULL,
                 sidechain_number INTEGER NOT NULL,
                 data_hash BLOB NOT NULL,
                 data BLOB NOT NULL);",
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

    /// Sidechain proposals *we* authored. Not yet on the chain, so not yet
    /// votable — these become M1s in the coinbase.
    pub async fn get_our_sidechain_proposals(
        &self,
    ) -> Result<Vec<SidechainProposal>, rusqlite::Error> {
        // Satisfy clippy with a single function call per lock
        let with_connection = |connection: &Connection| -> Result<_, rusqlite::Error> {
            let mut statement =
                connection.prepare("SELECT sidechain_number, data FROM sidechain_proposals")?;

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
        };
        let connection = self.conn.lock().await;
        with_connection(&connection)
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
    ) -> Result<HashMap<SidechainNumber, StoredBundleProposals>, error::GetBundleProposals> {
        // Satisfy clippy with a single function call per lock
        let with_connection = |connection: &Connection| -> Result<_, error::GetBundleProposals> {
            let mut statement = connection
                .prepare("SELECT sidechain_number, bundle_hash, bundle_tx FROM bundle_proposals")?;
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
        };
        let connection = self.conn.lock().await;
        with_connection(&connection)
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

    pub async fn delete_sidechain_ack(&self, ack: &SidechainAck) -> Result<(), rusqlite::Error> {
        let connection = self.conn.lock().await;
        connection.execute(
            "DELETE FROM sidechain_acks WHERE number = ?1 AND data_hash = ?2",
            (ack.sidechain_number.0, ack.description_hash.as_byte_array()),
        )?;
        drop(connection);
        Ok(())
    }

    /// Persists a sidechain proposal. On regtest it is picked up by the next
    /// block generation; on a PoW network it goes out in the next template.
    pub async fn propose_sidechain(
        &self,
        proposal: &SidechainProposal,
    ) -> Result<(), rusqlite::Error> {
        let sidechain_number: u8 = proposal.sidechain_number.into();
        self.conn.lock().await.execute(
            "INSERT INTO sidechain_proposals (sidechain_number, data_hash, data) VALUES (?1, ?2, ?3)",
            (sidechain_number, proposal.description.sha256d_hash().to_byte_array(), &proposal.description.0),
        )?;
        Ok(())
    }

    /// ACK every active sidechain proposal, whatever `sidechain_acks` says.
    pub async fn get_ack_all_proposals(&self) -> Result<bool, rusqlite::Error> {
        let connection = self.conn.lock().await;
        connection.query_row(
            "SELECT ack_all_proposals FROM block_producer_settings WHERE id = 0",
            [],
            |row| row.get(0),
        )
    }

    pub async fn set_ack_all_proposals(&self, ack_all: bool) -> Result<(), rusqlite::Error> {
        self.conn
            .lock()
            .await
            .execute(
                "UPDATE block_producer_settings SET ack_all_proposals = ?1 WHERE id = 0",
                [ack_all],
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

    /// BMM requests with the given previous blockhash: (sidechain number, side
    /// blockhash) pairs, which become the M7 accepts in the coinbase.
    pub async fn get_bmm_requests(
        &self,
        prev_blockhash: &bitcoin::BlockHash,
    ) -> Result<Vec<(SidechainNumber, BmmCommitment)>, rusqlite::Error> {
        // Satisfy clippy with a single function call per lock
        let with_connection = |connection: &Connection| -> Result<_, _> {
            let mut statement = connection
                .prepare(
                    "SELECT sidechain_number, side_block_hash FROM bmm_requests WHERE prev_block_hash = ?"
                )?;

            let queried = statement
                .query_map([prev_blockhash.as_byte_array()], |row| {
                    let sidechain_number = SidechainNumber(row.get(0)?);
                    let side_blockhash = BmmCommitment(row.get(1)?);
                    Ok((sidechain_number, side_blockhash))
                })?
                .collect::<Result<_, _>>()?;

            Ok(queried)
        };
        let connection = self.conn.lock().await;
        with_connection(&connection)
    }

    /// Returns `true` if a BMM request was inserted, `false` if one already
    /// exists for that sidechain and previous blockhash.
    pub async fn insert_new_bmm_request(
        &self,
        sidechain_number: SidechainNumber,
        prev_blockhash: bitcoin::BlockHash,
        side_block_hash: BmmCommitment,
    ) -> Result<bool, rusqlite::Error> {
        // Satisfy clippy with a single function call per lock
        let with_connection = |connection: &Connection| -> Result<bool, rusqlite::Error> {
            connection
                .prepare(
                    "INSERT OR ABORT INTO bmm_requests (sidechain_number, prev_block_hash, side_block_hash) VALUES (?1, ?2, ?3)",
                )?
                .execute((
                    u8::from(sidechain_number),
                    prev_blockhash.to_byte_array(),
                    side_block_hash.0,
                ))
                .map_or_else(
                    |err| if err.sqlite_error_code() == Some(rusqlite::ErrorCode::ConstraintViolation) {
                        Ok(false)
                    } else {
                        Err(err)
                    },
                    |_| Ok(true)
                )
        };
        let connection = self.conn.lock().await;
        with_connection(&connection)
    }

    pub async fn put_withdrawal_bundle(
        &self,
        sidechain_number: SidechainNumber,
        blinded_m6: &BlindedM6<'static>,
    ) -> Result<M6id, rusqlite::Error> {
        let m6id = blinded_m6.compute_m6id();
        // Always encode with rust-bitcoin. A zero-input bundle round-trips
        // because `BlindedM6::deserialize` reads this encoding back, and a
        // finalized M6 has a treasury input anyway.
        let tx_bytes = blinded_m6.serialize();
        self.conn
            .lock()
            .await
            .execute(
                "INSERT OR IGNORE INTO bundle_proposals (sidechain_number, bundle_hash, bundle_tx) VALUES (?1, ?2, ?3)",
                (sidechain_number.0, m6id.0.as_byte_array(), tx_bytes),
            )?;
        Ok(m6id)
    }

    /// Drop the bundle proposals that the connecting `block_hash` settled. The
    /// deleted rows are first snapshotted into `bundle_proposals_undo`, keyed by
    /// that block, so they can be restored (see
    /// [`Self::restore_bundle_proposals`]) if it is later disconnected by a
    /// reorg — the validator puts such bundles back to pending, so the producer
    /// has to be able to keep proposing them.
    pub(crate) async fn delete_bundle_proposals<I>(
        &self,
        block_hash: &bitcoin::BlockHash,
        iter: I,
    ) -> Result<(), rusqlite::Error>
    where
        I: IntoIterator<Item = (SidechainNumber, M6id)>,
    {
        let total_deleted = {
            let mut connection = self.conn.lock().await;
            snapshot_and_delete_bundle_proposals(&mut connection, block_hash, iter)?
        };

        if total_deleted > 0 {
            tracing::debug!(
                "deleted {} bundle proposal(s) from SQLite DB",
                total_deleted
            );
        }
        Ok(())
    }

    /// Restore the bundle proposals that were dropped when `block_hash` was
    /// connected, moving them back out of `bundle_proposals_undo`. Called when
    /// `block_hash` is disconnected by a reorg, so that bundles the validator
    /// has returned to pending are proposed again.
    pub(crate) async fn restore_bundle_proposals(
        &self,
        block_hash: &bitcoin::BlockHash,
    ) -> Result<(), rusqlite::Error> {
        let restored = {
            let mut connection = self.conn.lock().await;
            restore_bundle_proposals_from_undo(&mut connection, block_hash)?
        };
        if restored > 0 {
            tracing::info!(
                %block_hash,
                "restored {restored} bundle proposal(s) from disconnected block",
            );
        }
        Ok(())
    }

    /// Drop the pending sidechain proposals that the connecting `block_hash`
    /// carried on-chain. As with [`Self::delete_bundle_proposals`], the deleted
    /// rows are snapshotted into `sidechain_proposals_undo` first, so that
    /// [`Self::restore_sidechain_proposals`] can put them back if the block is
    /// disconnected by a reorg.
    pub(crate) async fn delete_pending_sidechain_proposals<I>(
        &self,
        block_hash: &bitcoin::BlockHash,
        proposals: I,
    ) -> Result<(), rusqlite::Error>
    where
        I: IntoIterator<Item = SidechainProposalId>,
    {
        let total_deleted = {
            let mut connection = self.conn.lock().await;
            snapshot_and_delete_sidechain_proposals(&mut connection, block_hash, proposals)?
        };

        if total_deleted > 0 {
            tracing::debug!(
                "deleted {} pending sidechain proposal(s) from SQLite DB",
                total_deleted
            );
        }
        Ok(())
    }

    /// Restore the pending sidechain proposals that were dropped when
    /// `block_hash` was connected, moving them back out of
    /// `sidechain_proposals_undo`. Called when `block_hash` is disconnected by a
    /// reorg, so that proposals which are no longer on-chain are re-proposed.
    pub(crate) async fn restore_sidechain_proposals(
        &self,
        block_hash: &bitcoin::BlockHash,
    ) -> Result<(), rusqlite::Error> {
        let restored = {
            let mut connection = self.conn.lock().await;
            restore_sidechain_proposals_from_undo(&mut connection, block_hash)?
        };
        if restored > 0 {
            tracing::info!(
                %block_hash,
                "restored {restored} pending sidechain proposal(s) from disconnected block",
            );
        }
        Ok(())
    }

    /// Consume the BMM requests for `prev_blockhash` when a block is generated on
    /// top of it. The deleted rows are first snapshotted into `bmm_requests_undo`,
    /// keyed by the mined `block_hash`, so they can be restored (see
    /// [`Self::restore_bmm_requests`]) if that block is later disconnected by a
    /// reorg — otherwise the operator would have to re-issue `create_bmm_request`
    /// for the new mainchain tip.
    pub(crate) async fn delete_bmm_requests(
        &self,
        prev_blockhash: &bitcoin::BlockHash,
        block_hash: &bitcoin::BlockHash,
    ) -> Result<(), rusqlite::Error> {
        let mut connection = self.conn.lock().await;
        snapshot_and_delete_bmm_requests(&mut connection, prev_blockhash, block_hash)
    }

    /// Restore the BMM requests that were consumed when `block_hash` was
    /// generated, moving them back out of `bmm_requests_undo`. Called when
    /// `block_hash` is disconnected by a reorg, so the operator's queued BMM
    /// requests can be re-emitted against the new mainchain tip.
    pub(crate) async fn restore_bmm_requests(
        &self,
        block_hash: &bitcoin::BlockHash,
    ) -> Result<(), rusqlite::Error> {
        let restored = {
            let mut connection = self.conn.lock().await;
            restore_bmm_requests_from_undo(&mut connection, block_hash)?
        };
        if restored > 0 {
            tracing::info!(
                %block_hash,
                "restored {restored} BMM request(s) from disconnected block",
            );
        }
        Ok(())
    }
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

/// Snapshot the BMM requests keyed to `prev_blockhash` into `bmm_requests_undo`
/// (keyed by the mined `block_hash`) and delete them from `bmm_requests`, within
/// a single transaction. Called when a block is generated on top of
/// `prev_blockhash`; the snapshot lets `restore_bmm_requests_from_undo` put the
/// requests back if that block is later disconnected by a reorg. Undo rows for
/// blocks beyond the most recent `UNDO_RETAINED_BLOCKS` producing
/// blocks are pruned so the table stays bounded.
fn snapshot_and_delete_bmm_requests(
    connection: &mut Connection,
    prev_blockhash: &bitcoin::BlockHash,
    block_hash: &bitcoin::BlockHash,
) -> Result<(), rusqlite::Error> {
    let tx = connection.transaction()?;
    tx.execute(
        "INSERT INTO bmm_requests_undo \
         (block_hash, sidechain_number, prev_block_hash, side_block_hash) \
         SELECT ?1, sidechain_number, prev_block_hash, side_block_hash \
         FROM bmm_requests WHERE prev_block_hash = ?2;",
        (block_hash.as_byte_array(), prev_blockhash.as_byte_array()),
    )?;
    tx.execute(
        "DELETE FROM bmm_requests where prev_block_hash = ?;",
        [prev_blockhash.as_byte_array()],
    )?;
    let () = prune_undo_table(&tx, "bmm_requests_undo")?;
    tx.commit()
}

/// Keep only the undo rows belonging to the most recent
/// `UNDO_RETAINED_BLOCKS` blocks recorded in `undo_table`, so that blocks which
/// stay on the main chain (and are therefore never disconnected) don't
/// accumulate undo rows forever. `undo_table` is a literal in every caller, so
/// interpolating it is not a user-controlled query.
fn prune_undo_table(
    tx: &rusqlite::Transaction<'_>,
    undo_table: &'static str,
) -> Result<(), rusqlite::Error> {
    tx.execute(
        &format!(
            "DELETE FROM {undo_table} \
             WHERE block_hash NOT IN ( \
                 SELECT block_hash FROM {undo_table} \
                 GROUP BY block_hash \
                 ORDER BY MAX(rowid) DESC \
                 LIMIT ?1 \
             );"
        ),
        [UNDO_RETAINED_BLOCKS],
    )?;
    Ok(())
}

/// Snapshot the bundle proposals settled by the connecting `block_hash` into
/// `bundle_proposals_undo` and delete them from `bundle_proposals`, within a
/// single transaction. Returns the number of rows deleted. The snapshot lets
/// `restore_bundle_proposals_from_undo` put them back if that block is later
/// disconnected by a reorg, matching the validator, which returns the bundles
/// to pending. Old undo rows are pruned so the table stays bounded.
fn snapshot_and_delete_bundle_proposals<I>(
    connection: &mut Connection,
    block_hash: &bitcoin::BlockHash,
    proposals: I,
) -> Result<usize, rusqlite::Error>
where
    I: IntoIterator<Item = (SidechainNumber, M6id)>,
{
    // Most connected blocks settle no bundle at all. Don't open a write
    // transaction (nor re-run the prune) for them.
    let mut proposals = proposals.into_iter().peekable();
    if proposals.peek().is_none() {
        return Ok(0);
    }
    let tx = connection.transaction()?;
    let mut total_deleted = 0;
    for (sidechain_number, m6id) in proposals {
        tx.execute(
            "INSERT INTO bundle_proposals_undo \
             (block_hash, sidechain_number, bundle_hash, bundle_tx) \
             SELECT ?1, sidechain_number, bundle_hash, bundle_tx \
             FROM bundle_proposals WHERE sidechain_number = ?2 AND bundle_hash = ?3;",
            (
                block_hash.as_byte_array(),
                sidechain_number.0,
                m6id.0.as_byte_array(),
            ),
        )?;
        total_deleted += tx.execute(
            "DELETE FROM bundle_proposals where sidechain_number = ?1 AND bundle_hash = ?2;",
            (sidechain_number.0, m6id.0.as_byte_array()),
        )?;
    }
    let () = prune_undo_table(&tx, "bundle_proposals_undo")?;
    tx.commit()?;
    Ok(total_deleted)
}

/// Restore the bundle proposals snapshotted for `block_hash` back into
/// `bundle_proposals`, removing them from `bundle_proposals_undo`, within a
/// single transaction. Returns the number of rows restored. Called when
/// `block_hash` is disconnected by a reorg.
fn restore_bundle_proposals_from_undo(
    connection: &mut Connection,
    block_hash: &bitcoin::BlockHash,
) -> Result<usize, rusqlite::Error> {
    let tx = connection.transaction()?;
    let restored = tx.execute(
        "INSERT OR IGNORE INTO bundle_proposals \
         (sidechain_number, bundle_hash, bundle_tx) \
         SELECT sidechain_number, bundle_hash, bundle_tx \
         FROM bundle_proposals_undo WHERE block_hash = ?;",
        [block_hash.as_byte_array()],
    )?;
    tx.execute(
        "DELETE FROM bundle_proposals_undo WHERE block_hash = ?;",
        [block_hash.as_byte_array()],
    )?;
    tx.commit()?;
    Ok(restored)
}

/// Snapshot the pending sidechain proposals carried on-chain by the connecting
/// `block_hash` into `sidechain_proposals_undo` and delete them from
/// `sidechain_proposals`, within a single transaction. Returns the number of
/// rows deleted. The counterpart of `snapshot_and_delete_bundle_proposals` for
/// `sidechain_proposals`.
fn snapshot_and_delete_sidechain_proposals<I>(
    connection: &mut Connection,
    block_hash: &bitcoin::BlockHash,
    proposals: I,
) -> Result<usize, rusqlite::Error>
where
    I: IntoIterator<Item = SidechainProposalId>,
{
    // As above: most connected blocks carry no sidechain proposal.
    let mut proposals = proposals.into_iter().peekable();
    if proposals.peek().is_none() {
        return Ok(0);
    }
    let tx = connection.transaction()?;
    let mut total_deleted = 0;
    for proposal_id in proposals {
        tx.execute(
            "INSERT INTO sidechain_proposals_undo \
             (block_hash, sidechain_number, data_hash, data) \
             SELECT ?1, sidechain_number, data_hash, data \
             FROM sidechain_proposals WHERE sidechain_number = ?2 AND data_hash = ?3;",
            (
                block_hash.as_byte_array(),
                proposal_id.sidechain_number.0,
                proposal_id.description_hash.as_byte_array(),
            ),
        )?;
        total_deleted += tx.execute(
            "DELETE FROM sidechain_proposals where sidechain_number = ?1 AND data_hash = ?2;",
            (
                proposal_id.sidechain_number.0,
                proposal_id.description_hash.as_byte_array(),
            ),
        )?;
    }
    let () = prune_undo_table(&tx, "sidechain_proposals_undo")?;
    tx.commit()?;
    Ok(total_deleted)
}

/// Restore the pending sidechain proposals snapshotted for `block_hash` back
/// into `sidechain_proposals`, removing them from `sidechain_proposals_undo`,
/// within a single transaction. Returns the number of rows restored. Called
/// when `block_hash` is disconnected by a reorg.
fn restore_sidechain_proposals_from_undo(
    connection: &mut Connection,
    block_hash: &bitcoin::BlockHash,
) -> Result<usize, rusqlite::Error> {
    let tx = connection.transaction()?;
    let restored = tx.execute(
        "INSERT OR IGNORE INTO sidechain_proposals \
         (sidechain_number, data_hash, data) \
         SELECT sidechain_number, data_hash, data \
         FROM sidechain_proposals_undo WHERE block_hash = ?;",
        [block_hash.as_byte_array()],
    )?;
    tx.execute(
        "DELETE FROM sidechain_proposals_undo WHERE block_hash = ?;",
        [block_hash.as_byte_array()],
    )?;
    tx.commit()?;
    Ok(restored)
}

/// Restore the BMM requests snapshotted for `block_hash` back into
/// `bmm_requests`, removing them from `bmm_requests_undo`, within a single
/// transaction. Returns the number of rows restored. Called when `block_hash` is
/// disconnected by a reorg.
fn restore_bmm_requests_from_undo(
    connection: &mut Connection,
    block_hash: &bitcoin::BlockHash,
) -> Result<usize, rusqlite::Error> {
    let tx = connection.transaction()?;
    let restored = tx.execute(
        "INSERT OR IGNORE INTO bmm_requests \
         (sidechain_number, prev_block_hash, side_block_hash) \
         SELECT sidechain_number, prev_block_hash, side_block_hash \
         FROM bmm_requests_undo WHERE block_hash = ?;",
        [block_hash.as_byte_array()],
    )?;
    tx.execute(
        "DELETE FROM bmm_requests_undo WHERE block_hash = ?;",
        [block_hash.as_byte_array()],
    )?;
    tx.commit()?;
    Ok(restored)
}

#[cfg(test)]
mod bmm_requests_undo_tests {
    use bitcoin::hashes::Hash as _;
    use rusqlite::Connection;

    use super::{restore_bmm_requests_from_undo, snapshot_and_delete_bmm_requests};

    fn block_hash(byte: u8) -> bitcoin::BlockHash {
        bitcoin::BlockHash::from_byte_array([byte; 32])
    }

    fn open_db() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        // Verbatim `bmm_requests` schema plus the new `bmm_requests_undo` table,
        // matching the migrations in `Db::new`.
        connection
            .execute_batch(
                "CREATE TABLE bmm_requests
                    (sidechain_number INTEGER NOT NULL,
                     prev_block_hash BLOB NOT NULL,
                     side_block_hash BLOB NOT NULL,
                     UNIQUE(sidechain_number, prev_block_hash));
                 CREATE TABLE bmm_requests_undo
                    (block_hash BLOB NOT NULL,
                     sidechain_number INTEGER NOT NULL,
                     prev_block_hash BLOB NOT NULL,
                     side_block_hash BLOB NOT NULL);",
            )
            .unwrap();
        connection
    }

    fn insert_request(connection: &Connection, sidechain_number: u8, prev: &bitcoin::BlockHash) {
        connection
            .execute(
                "INSERT INTO bmm_requests (sidechain_number, prev_block_hash, side_block_hash) \
                 VALUES (?1, ?2, ?3);",
                (
                    sidechain_number,
                    prev.as_byte_array(),
                    block_hash(0).as_byte_array(),
                ),
            )
            .unwrap();
    }

    fn row_count(connection: &Connection, table: &str) -> i64 {
        connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    fn distinct_undo_blocks(connection: &Connection) -> i64 {
        connection
            .query_row(
                "SELECT COUNT(DISTINCT block_hash) FROM bmm_requests_undo",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// A BMM request consumed by block production is snapshotted, then restored
    /// verbatim when the producing block is disconnected by a reorg.
    #[test]
    fn bmm_request_restored_when_producing_block_disconnected() {
        let mut connection = open_db();

        let prev = block_hash(1);
        let mined = block_hash(2);
        let side = block_hash(3);

        // Operator queues a BMM request against the current tip `prev`.
        connection
            .execute(
                "INSERT INTO bmm_requests (sidechain_number, prev_block_hash, side_block_hash) \
                 VALUES (?1, ?2, ?3);",
                (5, prev.as_byte_array(), side.as_byte_array()),
            )
            .unwrap();
        assert_eq!(row_count(&connection, "bmm_requests"), 1);

        // Producing `mined` on top of `prev` consumes the request into the undo log.
        snapshot_and_delete_bmm_requests(&mut connection, &prev, &mined).unwrap();
        assert_eq!(row_count(&connection, "bmm_requests"), 0);
        assert_eq!(row_count(&connection, "bmm_requests_undo"), 1);

        // Disconnecting `mined` restores the request for the reverted tip `prev`.
        let restored = restore_bmm_requests_from_undo(&mut connection, &mined).unwrap();
        assert_eq!(restored, 1);
        assert_eq!(row_count(&connection, "bmm_requests"), 1);
        assert_eq!(row_count(&connection, "bmm_requests_undo"), 0);

        let (sidechain_number, restored_side): (u64, Vec<u8>) = connection
            .query_row(
                "SELECT sidechain_number, side_block_hash FROM bmm_requests \
                 WHERE prev_block_hash = ?;",
                [prev.as_byte_array()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(sidechain_number, 5);
        assert_eq!(restored_side, side.as_byte_array().to_vec());
    }

    /// Blocks that stay on the main chain (never disconnected) must not
    /// accumulate undo rows without bound: only the most recent
    /// `UNDO_RETAINED_BLOCKS` producing blocks are retained, and the
    /// oldest snapshot is dropped once that many newer blocks have been produced.
    #[test]
    fn old_undo_rows_are_pruned() {
        let mut connection = open_db();

        // Produce `RETAINED + 1` blocks, each consuming a distinct BMM request.
        let total = super::UNDO_RETAINED_BLOCKS + 1;
        for i in 0..total {
            let prev = block_hash(i as u8);
            let mined = block_hash((total - i) as u8);
            insert_request(&connection, 0, &prev);
            snapshot_and_delete_bmm_requests(&mut connection, &prev, &mined).unwrap();
        }

        // The table is bounded to the retention window, not the block count.
        assert_eq!(
            distinct_undo_blocks(&connection),
            super::UNDO_RETAINED_BLOCKS
        );

        // The very first block's snapshot has been pruned, so disconnecting it
        // restores nothing (degrades to pre-fix behaviour for ancient reorgs).
        let oldest = block_hash(total as u8);
        assert_eq!(
            restore_bmm_requests_from_undo(&mut connection, &oldest).unwrap(),
            0
        );

        // The most recent block's snapshot is retained and still restorable.
        let newest = block_hash(1);
        assert_eq!(
            restore_bmm_requests_from_undo(&mut connection, &newest).unwrap(),
            1
        );
    }
}

#[cfg(test)]
mod proposal_undo_tests {
    use bitcoin::hashes::{Hash as _, sha256d};
    use rusqlite::Connection;

    use super::{
        restore_bundle_proposals_from_undo, restore_sidechain_proposals_from_undo,
        snapshot_and_delete_bundle_proposals, snapshot_and_delete_sidechain_proposals,
    };
    use crate::types::{M6id, SidechainNumber, SidechainProposalId};

    fn block_hash(byte: u8) -> bitcoin::BlockHash {
        bitcoin::BlockHash::from_byte_array([byte; 32])
    }

    fn open_db() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        // Verbatim `bundle_proposals` / `sidechain_proposals` schemas plus their
        // undo tables, matching the migrations in `Db::new`.
        connection
            .execute_batch(
                "CREATE TABLE bundle_proposals
                    (sidechain_number INTEGER NOT NULL,
                     bundle_hash BLOB NOT NULL,
                     bundle_tx BLOB NOT NULL,
                     UNIQUE(sidechain_number, bundle_hash));
                 CREATE TABLE bundle_proposals_undo
                    (block_hash BLOB NOT NULL,
                     sidechain_number INTEGER NOT NULL,
                     bundle_hash BLOB NOT NULL,
                     bundle_tx BLOB NOT NULL);
                 CREATE TABLE sidechain_proposals
                    (sidechain_number INTEGER NOT NULL,
                     data_hash BLOB NOT NULL,
                     data BLOB NOT NULL,
                     UNIQUE(sidechain_number, data_hash));
                 CREATE TABLE sidechain_proposals_undo
                    (block_hash BLOB NOT NULL,
                     sidechain_number INTEGER NOT NULL,
                     data_hash BLOB NOT NULL,
                     data BLOB NOT NULL);",
            )
            .unwrap();
        connection
    }

    fn row_count(connection: &Connection, table: &str) -> i64 {
        connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    /// A bundle proposal dropped when the block that settled it was connected is
    /// snapshotted, then restored verbatim when that block is disconnected by a
    /// reorg — the validator returns the bundle to pending, so the producer must
    /// still be holding it.
    #[test]
    fn bundle_proposal_restored_when_connecting_block_disconnected() {
        let mut connection = open_db();

        let connected = block_hash(1);
        let m6id = M6id::from([7; 32]);
        let sidechain_number = SidechainNumber(5);
        let bundle_tx = vec![0xde, 0xad, 0xbe, 0xef];

        connection
            .execute(
                "INSERT INTO bundle_proposals (sidechain_number, bundle_hash, bundle_tx) \
                 VALUES (?1, ?2, ?3);",
                (sidechain_number.0, m6id.0.as_byte_array(), &bundle_tx),
            )
            .unwrap();

        let deleted = snapshot_and_delete_bundle_proposals(
            &mut connection,
            &connected,
            [(sidechain_number, m6id)],
        )
        .unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(row_count(&connection, "bundle_proposals"), 0);
        assert_eq!(row_count(&connection, "bundle_proposals_undo"), 1);

        let restored = restore_bundle_proposals_from_undo(&mut connection, &connected).unwrap();
        assert_eq!(restored, 1);
        assert_eq!(row_count(&connection, "bundle_proposals_undo"), 0);

        let (restored_number, restored_tx): (u8, Vec<u8>) = connection
            .query_row(
                "SELECT sidechain_number, bundle_tx FROM bundle_proposals WHERE bundle_hash = ?;",
                [m6id.0.as_byte_array()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(restored_number, sidechain_number.0);
        assert_eq!(restored_tx, bundle_tx);
    }

    /// A pending sidechain proposal dropped when the block carrying its M1 was
    /// connected is restored verbatim when that block is disconnected, so the
    /// proposal goes back out in a later coinbase instead of being lost.
    #[test]
    fn sidechain_proposal_restored_when_connecting_block_disconnected() {
        let mut connection = open_db();

        let connected = block_hash(2);
        let data = b"my sidechain".to_vec();
        let proposal_id = SidechainProposalId {
            sidechain_number: SidechainNumber(3),
            description_hash: sha256d::Hash::from_byte_array([9; 32]),
        };

        connection
            .execute(
                "INSERT INTO sidechain_proposals (sidechain_number, data_hash, data) \
                 VALUES (?1, ?2, ?3);",
                (
                    proposal_id.sidechain_number.0,
                    proposal_id.description_hash.as_byte_array(),
                    &data,
                ),
            )
            .unwrap();

        let deleted =
            snapshot_and_delete_sidechain_proposals(&mut connection, &connected, [proposal_id])
                .unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(row_count(&connection, "sidechain_proposals"), 0);
        assert_eq!(row_count(&connection, "sidechain_proposals_undo"), 1);

        let restored = restore_sidechain_proposals_from_undo(&mut connection, &connected).unwrap();
        assert_eq!(restored, 1);
        assert_eq!(row_count(&connection, "sidechain_proposals_undo"), 0);

        let (restored_number, restored_data): (u8, Vec<u8>) = connection
            .query_row(
                "SELECT sidechain_number, data FROM sidechain_proposals WHERE data_hash = ?;",
                [proposal_id.description_hash.as_byte_array()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(restored_number, proposal_id.sidechain_number.0);
        assert_eq!(restored_data, data);
    }

    /// Blocks that stay on the main chain must not accumulate undo rows without
    /// bound: only the most recent `UNDO_RETAINED_BLOCKS` snapshots are kept, so
    /// an ancient reorg degrades to the pre-fix behaviour rather than growing
    /// the table forever.
    #[test]
    fn old_proposal_undo_rows_are_pruned() {
        let mut connection = open_db();

        let total = super::UNDO_RETAINED_BLOCKS + 1;
        for i in 0..total {
            let m6id = M6id::from([i as u8; 32]);
            let connected = block_hash((total - i) as u8);
            connection
                .execute(
                    "INSERT INTO bundle_proposals (sidechain_number, bundle_hash, bundle_tx) \
                     VALUES (?1, ?2, ?3);",
                    (0u8, m6id.0.as_byte_array(), vec![0u8]),
                )
                .unwrap();
            let deleted = snapshot_and_delete_bundle_proposals(
                &mut connection,
                &connected,
                [(SidechainNumber(0), m6id)],
            )
            .unwrap();
            assert_eq!(deleted, 1);
        }

        let distinct_undo_blocks: i64 = connection
            .query_row(
                "SELECT COUNT(DISTINCT block_hash) FROM bundle_proposals_undo",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(distinct_undo_blocks, super::UNDO_RETAINED_BLOCKS);

        // The oldest block's snapshot has been pruned; the newest is retained.
        let oldest = block_hash(total as u8);
        assert_eq!(
            restore_bundle_proposals_from_undo(&mut connection, &oldest).unwrap(),
            0
        );
        let newest = block_hash(1);
        assert_eq!(
            restore_bundle_proposals_from_undo(&mut connection, &newest).unwrap(),
            1
        );
    }
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
        for expected in ["sidechain_proposals", "sidechain_acks", "bmm_requests"] {
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
    use bitcoin::hashes::Hash as _;
    use rusqlite::Connection;

    use super::Db;

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
    /// block template) and a working `bmm_requests_undo` snapshot path, while
    /// leaving an un-migrated wallet seed strictly alone — keyless producers
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
        assert!(
            db.get_ack_all_proposals().await.unwrap(),
            "block_producer_settings must be created with ack-all defaulting on"
        );

        // The snapshot-and-delete path `mine` hits after producing a block.
        let prev = bitcoin::BlockHash::from_byte_array([1; 32]);
        let mined = bitcoin::BlockHash::from_byte_array([2; 32]);
        db.delete_bmm_requests(&prev, &mined).await.unwrap();
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
