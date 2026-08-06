//! The policy store for drivechain mining decisions

use std::{collections::HashMap, path::Path};

use bitcoin::hashes::{Hash as _, sha256d};
use fallible_iterator::{FallibleIterator as _, IteratorExt as _};
use rusqlite::{Connection, OptionalExtension as _};

use crate::{
    block_producer::error,
    types::{
        BlindedM6, BmmCommitment, M6id, SidechainAck, SidechainNumber, SidechainProposal,
        SidechainProposalId,
    },
};

/// Bundle proposals for a single sidechain, as stored (no validator filtering).
pub(crate) type StoredBundleProposals = Vec<(M6id, BlindedM6<'static>)>;

/// The wallet's own most recently tracked in-flight BMM bid for a sidechain,
/// used to build a genuine BIP125 replacement (fee-bump or manual
/// input-reuse) instead of an unrelated second transaction. See
/// [`Db::get_tracked_bmm_request`].
pub struct TrackedBmmRequest {
    pub prev_block_hash: bitcoin::BlockHash,
    pub side_block_hash: BmmCommitment,
    pub txid: bitcoin::Txid,
    pub fee_sats: u64,
    pub raw_tx: Vec<u8>,
}

/// Undo rows are kept for this many recently-produced blocks, so that blocks
/// which stay on the main chain (and are therefore never disconnected) don't
/// accumulate undo rows forever.
const BMM_REQUESTS_UNDO_RETAINED_BLOCKS: i64 = 100;

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
            // In-flight (unconfirmed) bid tracking. Recording the broadcast
            // transaction for a BMM request slot lets a later call for the
            // same slot be recognized as a bid increase (RBF fee-bump)
            // rather than an unrelated second transaction; `raw_tx` allows
            // returning the exact previously-broadcast transaction without
            // rebuilding it. The undo table gets the same columns so a reorg
            // that disconnects a block we produced restores requests still
            // tracked -- their transactions are back in the mempool, and a
            // replacement must reuse their inputs.
            //
            // `bid_seq` orders bids by when they were last written, which
            // `rowid` can't: the upsert in `upsert_bmm_request` resolves
            // conflicts with `DO UPDATE`, leaving the existing row's `rowid`
            // untouched, so re-bidding an older slot after a newer one
            // updates a *lower* `rowid`. The `bmm_bid_seq` counter is never
            // decremented, so numbers are only ever handed out once --
            // drawing from `MAX(bid_seq) + 1` over the live table instead
            // would free the highest number for reuse whenever its bid
            // settled. Pre-upgrade rows are backfilled with `bid_seq =
            // rowid` and the counter is seeded past every number already in
            // use, including snapshots not yet pruned.
            M::up(
                "ALTER TABLE bmm_requests ADD COLUMN txid BLOB;
                 ALTER TABLE bmm_requests ADD COLUMN fee_sats INTEGER;
                 ALTER TABLE bmm_requests ADD COLUMN raw_tx BLOB;
                 ALTER TABLE bmm_requests ADD COLUMN bid_seq INTEGER;
                 UPDATE bmm_requests SET bid_seq = rowid WHERE bid_seq IS NULL;
                 ALTER TABLE bmm_requests_undo ADD COLUMN txid BLOB;
                 ALTER TABLE bmm_requests_undo ADD COLUMN fee_sats INTEGER;
                 ALTER TABLE bmm_requests_undo ADD COLUMN raw_tx BLOB;
                 ALTER TABLE bmm_requests_undo ADD COLUMN bid_seq INTEGER;
                 CREATE TABLE bmm_bid_seq
                    (id INTEGER PRIMARY KEY CHECK (id = 0),
                     next INTEGER NOT NULL);
                 INSERT INTO bmm_bid_seq (id, next)
                 SELECT 0, IFNULL(MAX(bid_seq), 0) FROM (
                     SELECT bid_seq FROM bmm_requests
                     UNION ALL
                     SELECT bid_seq FROM bmm_requests_undo
                 );",
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

    /// The most recently tracked bid for `sidechain_number`, whichever slot
    /// it targeted.
    ///
    /// Not scoped to the current slot on purpose. A bid that lost its auction
    /// is consensus-dead, but its transaction stays in bitcoind's mempool
    /// until something replaces it, and `Wallet::create_bmm_request` needs it
    /// to build that replacement.
    pub async fn get_tracked_bmm_request(
        &self,
        sidechain_number: SidechainNumber,
    ) -> Result<Option<TrackedBmmRequest>, rusqlite::Error> {
        type Row = (
            [u8; 32],
            [u8; 32],
            Option<[u8; 32]>,
            Option<i64>,
            Option<Vec<u8>>,
        );
        let with_connection = |connection: &Connection| -> Result<_, rusqlite::Error> {
            let row: Option<Row> = connection
                .query_row(
                    "SELECT prev_block_hash, side_block_hash, txid, fee_sats, raw_tx
                     FROM bmm_requests WHERE sidechain_number = ?1
                     ORDER BY bid_seq DESC, rowid DESC LIMIT 1",
                    [u8::from(sidechain_number)],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    },
                )
                .optional()?;
            let Some((prev_block_hash, side_block_hash, txid, fee_sats, raw_tx)) = row else {
                return Ok(None);
            };
            let (Some(txid), Some(fee_sats), Some(raw_tx)) = (txid, fee_sats, raw_tx) else {
                return Ok(None);
            };
            Ok(Some(TrackedBmmRequest {
                prev_block_hash: bitcoin::BlockHash::from_byte_array(prev_block_hash),
                side_block_hash: BmmCommitment(side_block_hash),
                txid: bitcoin::Txid::from_byte_array(txid),
                fee_sats: fee_sats as u64,
                raw_tx,
            }))
        };
        let connection = self.conn.lock().await;
        with_connection(&connection)
    }

    /// Records the transaction currently tracked as the in-flight bid for
    /// `(sidechain_number, prev_blockhash)`, superseding any previous
    /// tracked bid for that same slot. Must only be called after `raw_tx`
    /// has been successfully broadcast.
    pub async fn upsert_bmm_request(
        &self,
        sidechain_number: SidechainNumber,
        prev_blockhash: &bitcoin::BlockHash,
        side_block_hash: BmmCommitment,
        txid: bitcoin::Txid,
        fee_sats: u64,
        raw_tx: &[u8],
    ) -> Result<(), rusqlite::Error> {
        // Satisfy clippy with a single function call per lock
        let with_connection = |connection: &mut Connection| -> Result<(), rusqlite::Error> {
            let tx = connection.transaction()?;
            tx.execute("UPDATE bmm_bid_seq SET next = next + 1 WHERE id = 0;", [])?;
            tx.execute(
                "INSERT INTO bmm_requests
                (sidechain_number, prev_block_hash, side_block_hash, txid, fee_sats,
                 raw_tx, bid_seq)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6,
                (SELECT next FROM bmm_bid_seq WHERE id = 0))
             ON CONFLICT(sidechain_number, prev_block_hash) DO UPDATE SET
                side_block_hash = excluded.side_block_hash,
                txid = excluded.txid,
                fee_sats = excluded.fee_sats,
                raw_tx = excluded.raw_tx,
                bid_seq = excluded.bid_seq",
                (
                    u8::from(sidechain_number),
                    prev_blockhash.as_byte_array(),
                    side_block_hash.0,
                    txid.as_byte_array(),
                    fee_sats as i64,
                    raw_tx,
                ),
            )?;
            tx.commit()
        };
        let mut connection = self.conn.lock().await;
        with_connection(&mut connection)
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

    // Gets wiped upon generating a new block.
    pub(crate) async fn delete_bundle_proposals<I>(&self, iter: I) -> Result<(), rusqlite::Error>
    where
        I: IntoIterator<Item = (SidechainNumber, M6id)>,
    {
        // Satisfy clippy with a single function call per lock
        let with_connection = |connection: &Connection| -> Result<usize, rusqlite::Error> {
            let mut total_deleted = 0;
            for (sidechain_number, m6id) in iter {
                let deleted = connection.execute(
                    "DELETE FROM bundle_proposals where sidechain_number = ?1 AND bundle_hash = ?2;",
                    (sidechain_number.0, m6id.0.as_byte_array())
                )?;
                total_deleted += deleted;
            }
            Ok(total_deleted)
        };
        let total_deleted = {
            let connection = self.conn.lock().await;
            with_connection(&connection)?
        };

        if total_deleted > 0 {
            tracing::debug!(
                "deleted {} bundle proposal(s) from SQLite DB",
                total_deleted
            );
        }
        Ok(())
    }

    // Gets wiped upon generating a new block.
    pub(crate) async fn delete_pending_sidechain_proposals<I>(
        &self,
        proposals: I,
    ) -> Result<(), rusqlite::Error>
    where
        I: IntoIterator<Item = SidechainProposalId>,
    {
        let with_connection = |connection: &Connection| -> Result<usize, rusqlite::Error> {
            let mut total_deleted = 0;
            for proposal_id in proposals {
                let deleted = connection.execute(
                    "DELETE FROM sidechain_proposals where sidechain_number = ?1 AND data_hash = ?2;",
                    (proposal_id.sidechain_number.0, proposal_id.description_hash.as_byte_array())
                )?;
                total_deleted += deleted;
            }
            Ok(total_deleted)
        };
        let connection = self.conn.lock().await;
        let total_deleted = with_connection(&connection)?;
        drop(connection);

        if total_deleted > 0 {
            tracing::debug!(
                "deleted {} pending sidechain proposal(s) from SQLite DB",
                total_deleted
            );
        }
        Ok(())
    }

    /// The transaction of every tracked in-flight bid, across all sidechains
    /// and slots, paired with the mainchain block it bid to build on. Used to
    /// work out what a connected block did to each, by intersecting against
    /// that block's transactions: a bid in the block won, and one targeting
    /// the same slot but absent from it lost.
    pub(crate) async fn tracked_bmm_bids(
        &self,
    ) -> Result<Vec<(bitcoin::Txid, bitcoin::BlockHash)>, rusqlite::Error> {
        let with_connection = |connection: &Connection| -> Result<_, rusqlite::Error> {
            let mut statement = connection
                .prepare("SELECT txid, prev_block_hash FROM bmm_requests WHERE txid IS NOT NULL")?;
            let bids = statement
                .query_map([], |row| {
                    let txid: [u8; 32] = row.get(0)?;
                    let prev_block_hash: [u8; 32] = row.get(1)?;
                    Ok((
                        bitcoin::Txid::from_byte_array(txid),
                        bitcoin::BlockHash::from_byte_array(prev_block_hash),
                    ))
                })?
                .collect::<Result<_, _>>()?;
            Ok(bids)
        };
        let connection = self.conn.lock().await;
        with_connection(&connection)
    }

    /// Consume the tracked bids that `block_hash` confirmed.
    ///
    /// Keyed on the winning transactions, not the superseded slot: a block
    /// ends the auction for the losing bids too, but their transactions stay
    /// in the mempool and their rows are what lets a later bid replace them.
    ///
    /// Deleted rows are snapshotted into `bmm_requests_undo` first, so
    /// [`Self::restore_bmm_requests`] can put them back if `block_hash` is
    /// disconnected and its transactions return to the mempool.
    pub(crate) async fn delete_won_bmm_requests(
        &self,
        block_hash: &bitcoin::BlockHash,
        won_txids: &[bitcoin::Txid],
    ) -> Result<usize, rusqlite::Error> {
        let mut connection = self.conn.lock().await;
        snapshot_and_delete_won_bmm_requests(&mut connection, block_hash, won_txids)
    }

    /// Whether any tracked bid, for any slot, is `txid`.
    pub async fn contains_bmm_request_txid(
        &self,
        txid: bitcoin::Txid,
    ) -> Result<bool, rusqlite::Error> {
        let with_connection = |connection: &Connection| -> Result<_, rusqlite::Error> {
            connection.query_row(
                "SELECT EXISTS (SELECT 1 FROM bmm_requests WHERE txid = ?1)",
                [txid.as_byte_array()],
                |row| row.get(0),
            )
        };
        let connection = self.conn.lock().await;
        with_connection(&connection)
    }

    /// The distinct blocks that still hold undo rows, i.e. every block whose
    /// consumed BMM requests could still be restored.
    pub(crate) async fn bmm_requests_undo_block_hashes(
        &self,
    ) -> Result<Vec<bitcoin::BlockHash>, rusqlite::Error> {
        // Satisfy clippy with a single function call per lock
        let with_connection = |connection: &Connection| -> Result<_, rusqlite::Error> {
            let mut statement =
                connection.prepare("SELECT DISTINCT block_hash FROM bmm_requests_undo")?;
            let block_hashes = statement
                .query_map([], |row| {
                    Ok(bitcoin::BlockHash::from_byte_array(row.get(0)?))
                })?
                .collect::<Result<_, _>>()?;
            Ok(block_hashes)
        };
        let connection = self.conn.lock().await;
        with_connection(&connection)
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

/// Snapshot the `bmm_requests` rows for `won_txids` into `bmm_requests_undo`
/// (keyed by the confirming `block_hash`) and delete them from `bmm_requests`,
/// within a single transaction. Returns the number of rows deleted.
///
/// Undo rows for blocks beyond the most recent
/// `BMM_REQUESTS_UNDO_RETAINED_BLOCKS` settling blocks are pruned so the table
/// stays bounded.
fn snapshot_and_delete_won_bmm_requests(
    connection: &mut Connection,
    block_hash: &bitcoin::BlockHash,
    won_txids: &[bitcoin::Txid],
) -> Result<usize, rusqlite::Error> {
    if won_txids.is_empty() {
        return Ok(0);
    }
    let tx = connection.transaction()?;
    let mut deleted = 0;
    for txid in won_txids {
        tx.execute(
            "INSERT INTO bmm_requests_undo \
             (block_hash, sidechain_number, prev_block_hash, side_block_hash, \
              txid, fee_sats, raw_tx, bid_seq) \
             SELECT ?1, sidechain_number, prev_block_hash, side_block_hash, \
                    txid, fee_sats, raw_tx, bid_seq \
             FROM bmm_requests WHERE txid = ?2;",
            (block_hash.as_byte_array(), txid.as_byte_array()),
        )?;
        deleted += tx.execute(
            "DELETE FROM bmm_requests WHERE txid = ?;",
            [txid.as_byte_array()],
        )?;
    }
    // Keep only the undo rows for the most recently settling blocks, so blocks
    // that stay on the main chain (and are therefore never disconnected) don't
    // accumulate undo rows forever.
    tx.execute(
        "DELETE FROM bmm_requests_undo \
         WHERE block_hash NOT IN ( \
             SELECT block_hash FROM bmm_requests_undo \
             GROUP BY block_hash \
             ORDER BY MAX(rowid) DESC \
             LIMIT ?1 \
         );",
        [BMM_REQUESTS_UNDO_RETAINED_BLOCKS],
    )?;
    tx.commit()?;
    Ok(deleted)
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
    // Restored bids take fresh `bid_seq` values rather than their original
    // ones. The disconnect makes them live again -- their transactions are
    // back in the mempool -- while any bid placed for the slot this block
    // opened is now consensus-dead. Keeping the old numbers would leave that
    // dead bid ranked highest, so the next replacement would supersede it and
    // leave the restored transaction stranded alongside.
    tx.execute("UPDATE bmm_bid_seq SET next = next + 1 WHERE id = 0;", [])?;
    let restored = tx.execute(
        "INSERT OR IGNORE INTO bmm_requests \
         (sidechain_number, prev_block_hash, side_block_hash, txid, fee_sats, raw_tx, bid_seq) \
         SELECT sidechain_number, prev_block_hash, side_block_hash, \
                txid, fee_sats, raw_tx, \
                (SELECT next FROM bmm_bid_seq WHERE id = 0) \
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

    use super::{restore_bmm_requests_from_undo, snapshot_and_delete_won_bmm_requests};

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
                     txid BLOB,
                     fee_sats INTEGER,
                     raw_tx BLOB,
                     bid_seq INTEGER,
                     UNIQUE(sidechain_number, prev_block_hash));
                 CREATE TABLE bmm_requests_undo
                    (block_hash BLOB NOT NULL,
                     sidechain_number INTEGER NOT NULL,
                     prev_block_hash BLOB NOT NULL,
                     side_block_hash BLOB NOT NULL,
                     txid BLOB,
                     fee_sats INTEGER,
                     raw_tx BLOB,
                     bid_seq INTEGER);
                 CREATE TABLE bmm_bid_seq
                    (id INTEGER PRIMARY KEY CHECK (id = 0),
                     next INTEGER NOT NULL);
                 INSERT INTO bmm_bid_seq (id, next) VALUES (0, 0);",
            )
            .unwrap();
        connection
    }

    fn txid(byte: u8) -> bitcoin::Txid {
        bitcoin::Txid::from_byte_array([byte; 32])
    }

    fn insert_request(
        connection: &Connection,
        sidechain_number: u8,
        prev: &bitcoin::BlockHash,
        bid_txid: bitcoin::Txid,
    ) {
        connection
            .execute(
                "INSERT INTO bmm_requests \
                 (sidechain_number, prev_block_hash, side_block_hash, txid) \
                 VALUES (?1, ?2, ?3, ?4);",
                (
                    sidechain_number,
                    prev.as_byte_array(),
                    block_hash(0).as_byte_array(),
                    bid_txid.as_byte_array(),
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

    /// A BMM request settled by the block that confirmed it is snapshotted,
    /// then restored verbatim when that block is disconnected by a reorg -- the
    /// disconnect puts the bid's transaction back into the mempool, so it needs
    /// tracking again.
    #[test]
    fn bmm_request_restored_when_confirming_block_disconnected() {
        let mut connection = open_db();

        let prev = block_hash(1);
        let mined = block_hash(2);
        let side = block_hash(3);
        let bid = txid(9);

        // Operator queues a BMM request against the current tip `prev`.
        connection
            .execute(
                "INSERT INTO bmm_requests \
                 (sidechain_number, prev_block_hash, side_block_hash, txid) \
                 VALUES (?1, ?2, ?3, ?4);",
                (
                    5,
                    prev.as_byte_array(),
                    side.as_byte_array(),
                    bid.as_byte_array(),
                ),
            )
            .unwrap();
        assert_eq!(row_count(&connection, "bmm_requests"), 1);

        // `mined` confirms the bid, settling it into the undo log.
        snapshot_and_delete_won_bmm_requests(&mut connection, &mined, &[bid]).unwrap();
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
    /// `BMM_REQUESTS_UNDO_RETAINED_BLOCKS` settling blocks are retained, and the
    /// oldest snapshot is dropped once that many newer blocks have settled.
    #[test]
    fn old_undo_rows_are_pruned() {
        let mut connection = open_db();

        // Settle `RETAINED + 1` blocks, each confirming a distinct BMM request.
        let total = super::BMM_REQUESTS_UNDO_RETAINED_BLOCKS + 1;
        for i in 0..total {
            let prev = block_hash(i as u8);
            let mined = block_hash((total - i) as u8);
            let bid = txid(i as u8);
            insert_request(&connection, 0, &prev, bid);
            snapshot_and_delete_won_bmm_requests(&mut connection, &mined, &[bid]).unwrap();
        }

        // The table is bounded to the retention window, not the block count.
        assert_eq!(
            distinct_undo_blocks(&connection),
            super::BMM_REQUESTS_UNDO_RETAINED_BLOCKS
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

/// Tests for the in-flight bid tracking that `Wallet::create_bmm_request`
/// reads, exercised through the real migrated `Db` rather than a hand-rolled
/// schema, so the migrations are covered too.
#[cfg(test)]
mod bmm_bid_tracking_tests {
    use bitcoin::hashes::Hash as _;

    use super::Db;
    use crate::types::{BmmCommitment, SidechainNumber};

    const SIDECHAIN: SidechainNumber = SidechainNumber(5);

    fn block_hash(byte: u8) -> bitcoin::BlockHash {
        bitcoin::BlockHash::from_byte_array([byte; 32])
    }

    fn txid(byte: u8) -> bitcoin::Txid {
        bitcoin::Txid::from_byte_array([byte; 32])
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bip300301-bmm-bid-tracking-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A bid consumed by a block *we* produced must come back **tracked**
    /// when that block is disconnected by a reorg. The disconnect puts the
    /// bid's transaction back into the mempool, so the next bid for this
    /// sidechain has to reuse its inputs as a BIP125 replacement — which it
    /// can only do if `txid`/`fee_sats`/`raw_tx` survived the undo round
    /// trip. Restoring the row with those columns NULL reads as untracked
    /// and silently falls back to fresh coin selection, which is what the
    /// tracking exists to prevent.
    #[tokio::test]
    async fn tracking_columns_survive_reorg_restore() {
        let dir = temp_dir("reorg-restore");
        let db = Db::new(&dir).unwrap();

        let prev = block_hash(1);
        let mined = block_hash(2);
        let side = BmmCommitment([3; 32]);
        let raw_tx = vec![0xde, 0xad, 0xbe, 0xef];

        db.upsert_bmm_request(SIDECHAIN, &prev, side, txid(4), 1_000, &raw_tx)
            .await
            .unwrap();

        // `mined` confirms the bid, settling it into the undo log.
        assert_eq!(
            db.delete_won_bmm_requests(&mined, &[txid(4)])
                .await
                .unwrap(),
            1
        );
        assert!(
            db.get_tracked_bmm_request(SIDECHAIN)
                .await
                .unwrap()
                .is_none(),
            "the settled bid must not still be tracked"
        );

        db.restore_bmm_requests(&mined).await.unwrap();
        let tracked = db
            .get_tracked_bmm_request(SIDECHAIN)
            .await
            .unwrap()
            .expect("a bid restored by a reorg must still be tracked");
        assert_eq!(tracked.prev_block_hash, prev);
        assert_eq!(tracked.side_block_hash, side);
        assert_eq!(tracked.txid, txid(4));
        assert_eq!(tracked.fee_sats, 1_000);
        assert_eq!(tracked.raw_tx, raw_tx);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A block that ends an auction without confirming our bid must leave it
    /// tracked: the transaction stays in the mempool, and the row is what lets
    /// the next bid replace it. Deleting by slot instead would take the losers
    /// down with the winner, since every bid for a slot shares a
    /// `prev_block_hash`.
    #[tokio::test]
    async fn losing_bid_survives_the_block_that_ended_its_auction() {
        let dir = temp_dir("losing-bid");
        let db = Db::new(&dir).unwrap();

        // Two sidechains bid against the same slot; only sidechain 5's bid is
        // confirmed by the block that ends it.
        let slot = block_hash(1);
        let settling_block = block_hash(2);
        let winner = txid(10);
        let loser = txid(11);

        db.upsert_bmm_request(
            SIDECHAIN,
            &slot,
            BmmCommitment([1; 32]),
            winner,
            5_000,
            b"win",
        )
        .await
        .unwrap();
        db.upsert_bmm_request(
            SidechainNumber(6),
            &slot,
            BmmCommitment([2; 32]),
            loser,
            4_000,
            b"lose",
        )
        .await
        .unwrap();

        assert_eq!(
            db.delete_won_bmm_requests(&settling_block, &[winner])
                .await
                .unwrap(),
            1,
            "only the confirmed bid should be settled"
        );

        assert!(
            db.get_tracked_bmm_request(SIDECHAIN)
                .await
                .unwrap()
                .is_none(),
            "the winning bid is settled and must no longer be tracked"
        );
        let still_tracked = db
            .get_tracked_bmm_request(SidechainNumber(6))
            .await
            .unwrap()
            .expect("the losing bid must still be tracked: its tx is still in the mempool");
        assert_eq!(still_tracked.txid, loser);
        assert_eq!(still_tracked.prev_block_hash, slot);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A bid restored by a reorg is live again -- the disconnect put its
    /// transaction back in the mempool -- while a bid placed for the slot that
    /// block opened is now consensus-dead. The restored one is what a
    /// replacement has to supersede, so restoring has to re-rank it above
    /// anything written while it was settled.

    #[tokio::test]
    async fn reorg_restored_bid_outranks_the_slot_it_reverted() {
        let dir = temp_dir("reorg-newer");
        let db = Db::new(&dir).unwrap();

        let slot_a = block_hash(1);
        let settling_block = block_hash(2);
        let slot_b = block_hash(3);
        let bid_a = txid(10);
        let bid_b = txid(11);

        // A wins its auction and is settled by the block that confirmed it.
        db.upsert_bmm_request(
            SIDECHAIN,
            &slot_a,
            BmmCommitment([1; 32]),
            bid_a,
            5_000,
            b"a",
        )
        .await
        .unwrap();
        db.delete_won_bmm_requests(&settling_block, &[bid_a])
            .await
            .unwrap();

        // A new bid is placed against the slot that block opened.
        db.upsert_bmm_request(
            SIDECHAIN,
            &slot_b,
            BmmCommitment([2; 32]),
            bid_b,
            6_000,
            b"b",
        )
        .await
        .unwrap();

        // The block is then disconnected, putting A's transaction back into
        // the mempool and its row back into the table.
        db.restore_bmm_requests(&settling_block).await.unwrap();

        let tracked = db
            .get_tracked_bmm_request(SIDECHAIN)
            .await
            .unwrap()
            .expect("a bid must be tracked");
        assert_eq!(
            tracked.txid, bid_a,
            "after the disconnect A is live again and B is consensus-dead, so A is the bid \
             a replacement must supersede"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// `bid_seq` must keep ordering bids by write order even across
    /// settlement. Drawing it from `MAX(bid_seq) + 1` over the live table did
    /// not: settling the highest-numbered bid freed its number for the next
    /// bid to reuse, and ordering against a row restored by a reorg then fell
    /// to the `rowid` tiebreak instead.
    #[tokio::test]
    async fn bid_seq_is_not_reused_after_settlement() {
        let dir = temp_dir("bid-seq-reuse");
        let db = Db::new(&dir).unwrap();

        let first = txid(1);
        db.upsert_bmm_request(
            SIDECHAIN,
            &block_hash(1),
            BmmCommitment([1; 32]),
            first,
            1,
            b"a",
        )
        .await
        .unwrap();
        db.delete_won_bmm_requests(&block_hash(2), &[first])
            .await
            .unwrap();
        db.upsert_bmm_request(
            SIDECHAIN,
            &block_hash(3),
            BmmCommitment([2; 32]),
            txid(2),
            2,
            b"b",
        )
        .await
        .unwrap();

        let assigned: i64 = {
            let conn = rusqlite::Connection::open(dir.join("db.sqlite")).unwrap();
            conn.query_row("SELECT bid_seq FROM bmm_requests", [], |row| row.get(0))
                .unwrap()
        };
        assert_eq!(
            assigned, 2,
            "the settled bid's number must not be handed out again"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Review finding: a legacy row (no `txid`) shadowing a tracked one, so
    /// that `get_tracked_bmm_request` reports nothing tracked.
    ///
    /// It cannot: the migration backfills pre-upgrade rows with
    /// `bid_seq = rowid` and seeds the counter past all of them, so any bid
    /// written afterwards outranks them. The suggested `WHERE txid IS NOT
    /// NULL` filter would change nothing. (The underlying observation stands
    /// -- a bid in flight across the upgrade has no recorded txid and cannot
    /// be tracked -- but no query can recover what was never stored.)
    #[tokio::test]
    async fn legacy_rows_never_outrank_tracked_ones() {
        let dir = temp_dir("legacy-rank");
        // A pre-upgrade DB carrying an in-flight request, migrated for real.
        {
            let conn = rusqlite::Connection::open(dir.join("db.sqlite")).unwrap();
            conn.execute_batch(super::migration_tests::LEGACY_V7_SCHEMA)
                .unwrap();
            conn.execute(
                "INSERT INTO bmm_requests \
                 (sidechain_number, prev_block_hash, side_block_hash) VALUES (?1, ?2, ?3);",
                (
                    u8::from(SIDECHAIN),
                    block_hash(1).as_byte_array(),
                    block_hash(2).as_byte_array(),
                ),
            )
            .unwrap();
        }
        let db = Db::new(&dir).unwrap();

        db.upsert_bmm_request(
            SIDECHAIN,
            &block_hash(3),
            BmmCommitment([1; 32]),
            txid(7),
            1,
            b"n",
        )
        .await
        .unwrap();

        let tracked = db
            .get_tracked_bmm_request(SIDECHAIN)
            .await
            .unwrap()
            .expect("the new bid outranks the legacy row");
        assert_eq!(tracked.txid, txid(7));

        std::fs::remove_dir_all(&dir).ok();
    }

    /// `get_tracked_bmm_request` must return the most recently *written*
    /// bid. It can't order by `rowid`: the upsert resolves conflicts with
    /// `DO UPDATE`, which leaves the existing row's `rowid` alone, so
    /// re-bidding an older slot updates a lower `rowid` than a newer slot's
    /// row and `ORDER BY rowid DESC` hands back the superseded transaction.
    #[tokio::test]
    async fn most_recently_written_bid_wins_over_higher_rowid() {
        let dir = temp_dir("bid-seq");
        let db = Db::new(&dir).unwrap();

        let t1 = block_hash(1);
        let t2 = block_hash(2);

        // Bid on slot T1, then on slot T2, so T2's row gets the higher
        // `rowid`. Both rows coexist: only blocks *we* produce consume
        // requests, and here the tip advanced by someone else's block.
        db.upsert_bmm_request(
            SIDECHAIN,
            &t1,
            BmmCommitment([11; 32]),
            txid(1),
            1_000,
            b"tx1",
        )
        .await
        .unwrap();
        db.upsert_bmm_request(
            SIDECHAIN,
            &t2,
            BmmCommitment([22; 32]),
            txid(2),
            2_000,
            b"tx2",
        )
        .await
        .unwrap();

        // Re-bid the older slot. This updates T1's row in place, at its
        // original lower `rowid`.
        db.upsert_bmm_request(
            SIDECHAIN,
            &t1,
            BmmCommitment([33; 32]),
            txid(3),
            3_000,
            b"tx3",
        )
        .await
        .unwrap();

        let tracked = db
            .get_tracked_bmm_request(SIDECHAIN)
            .await
            .unwrap()
            .expect("a tracked bid must be found");
        assert_eq!(
            tracked.txid,
            txid(3),
            "the most recently written bid must win, not the highest rowid"
        );
        assert_eq!(tracked.prev_block_hash, t1);
        assert_eq!(tracked.side_block_hash, BmmCommitment([33; 32]));
        assert_eq!(tracked.fee_sats, 3_000);

        std::fs::remove_dir_all(&dir).ok();
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

        // The snapshot-and-delete path `apply_connected_block_policy` hits
        // when a block confirms a tracked bid.
        let mined = bitcoin::BlockHash::from_byte_array([2; 32]);
        db.delete_won_bmm_requests(&mined, &[bitcoin::Txid::from_byte_array([3; 32])])
            .await
            .unwrap();
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
