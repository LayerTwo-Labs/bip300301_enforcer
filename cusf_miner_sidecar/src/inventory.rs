//! In-memory transaction inventory for the miner sidecar.

use std::collections::HashSet;

use bitcoin::{
    Transaction, Txid,
    consensus::encode::{deserialize_hex, serialize, serialize_hex},
};

use crate::error::Error;

/// Default max transactions held at once (local DoS bound).
pub const DEFAULT_MAX_TXS: usize = 100;

/// Default max total serialized bytes of inventory txs.
pub const DEFAULT_MAX_BYTES: usize = 4 * 1024 * 1024; // 4 MiB

/// Caps for inventory size (count + total consensus-serialized bytes).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InventoryLimits {
    pub max_txs: usize,
    pub max_bytes: usize,
}

impl Default for InventoryLimits {
    fn default() -> Self {
        Self {
            max_txs: DEFAULT_MAX_TXS,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

/// Ordered inventory of raw enforcer-format (or any) transactions awaiting mine.
#[derive(Clone, Debug)]
pub struct TxInventory {
    /// Insertion-ordered txs.
    txs: Vec<Transaction>,
    /// For O(1) duplicate detection.
    seen: HashSet<Txid>,
    /// Running total of consensus-serialized sizes.
    total_bytes: usize,
    limits: InventoryLimits,
}

impl Default for TxInventory {
    fn default() -> Self {
        Self::new()
    }
}

impl TxInventory {
    #[must_use]
    pub fn new() -> Self {
        Self::with_limits(InventoryLimits::default())
    }

    #[must_use]
    pub fn with_limits(limits: InventoryLimits) -> Self {
        Self {
            txs: Vec::new(),
            seen: HashSet::new(),
            total_bytes: 0,
            limits,
        }
    }

    #[must_use]
    pub fn limits(&self) -> InventoryLimits {
        self.limits
    }

    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// Parse consensus hex and insert. Returns txid. Duplicates error.
    pub fn add_raw_hex(&mut self, hex: &str) -> Result<Txid, Error> {
        let hex = hex.trim();
        if hex.is_empty() {
            return Err(Error::InvalidTxHex("empty hex".to_owned()));
        }
        let tx: Transaction =
            deserialize_hex(hex).map_err(|e| Error::InvalidTxHex(format!("{e}")))?;
        self.add_tx(tx)
    }

    /// Insert a decoded transaction.
    ///
    /// # Errors
    ///
    /// - [`Error::DuplicateTxid`] if `txid` is already held
    /// - [`Error::InventoryTxCap`] / [`Error::InventoryByteCap`] if caps exceeded
    pub fn add_tx(&mut self, tx: Transaction) -> Result<Txid, Error> {
        let txid = tx.compute_txid();
        if self.seen.contains(&txid) {
            return Err(Error::DuplicateTxid(txid));
        }
        if self.txs.len() >= self.limits.max_txs {
            return Err(Error::InventoryTxCap {
                have: self.txs.len(),
                max: self.limits.max_txs,
            });
        }
        let add_bytes = serialize(&tx).len();
        if self.total_bytes.saturating_add(add_bytes) > self.limits.max_bytes {
            return Err(Error::InventoryByteCap {
                have_bytes: self.total_bytes,
                add_bytes,
                max_bytes: self.limits.max_bytes,
            });
        }
        self.seen.insert(txid);
        self.total_bytes = self.total_bytes.saturating_add(add_bytes);
        self.txs.push(tx);
        Ok(txid)
    }

    /// Re-insert txs after a failed mine (skips duplicates already present).
    ///
    /// Used when restoring a `take_all` snapshot; concurrent adds that landed
    /// during the mine RPC stay. Re-enforces [`InventoryLimits`]: if concurrent
    /// adds filled the caps, remaining restore candidates are **dropped** (not
    /// error) so inventory never exceeds `max_txs` / `max_bytes`.
    pub fn restore(&mut self, txs: Vec<Transaction>) {
        for tx in txs {
            let txid = tx.compute_txid();
            if self.seen.contains(&txid) {
                continue;
            }
            if self.txs.len() >= self.limits.max_txs {
                break;
            }
            let add_bytes = serialize(&tx).len();
            if self.total_bytes.saturating_add(add_bytes) > self.limits.max_bytes {
                // Skip this tx; later smaller ones might still fit.
                continue;
            }
            self.seen.insert(txid);
            self.total_bytes = self.total_bytes.saturating_add(add_bytes);
            self.txs.push(tx);
        }
    }

    #[must_use]
    pub fn list_txids(&self) -> Vec<Txid> {
        self.txs.iter().map(Transaction::compute_txid).collect()
    }

    /// Consensus-hex encodings in insertion order.
    #[must_use]
    pub fn list_hex(&self) -> Vec<String> {
        self.txs.iter().map(serialize_hex).collect()
    }

    #[must_use]
    pub fn transactions(&self) -> &[Transaction] {
        &self.txs
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.txs.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.txs.is_empty()
    }

    pub fn clear(&mut self) {
        self.txs.clear();
        self.seen.clear();
        self.total_bytes = 0;
    }

    /// Drain all txs so concurrent adds after this call are not wiped by mine.
    pub fn take_all(&mut self) -> Vec<Transaction> {
        self.seen.clear();
        self.total_bytes = 0;
        std::mem::take(&mut self.txs)
    }
}

/// Consensus-serialized size of a transaction (for tests / caps).
#[must_use]
pub fn tx_serialized_len(tx: &Transaction) -> usize {
    serialize(tx).len()
}

#[cfg(test)]
mod tests {
    use bitcoin::{
        Amount, OutPoint, ScriptBuf, Sequence, TxIn, TxOut, Witness, absolute::LockTime,
        consensus::encode::deserialize, script::PushBytesBuf, transaction::Version,
    };

    use super::*;

    fn assert_roundtrip_bytes(tx: &Transaction) {
        let bytes = serialize(tx);
        let back: Transaction = deserialize(&bytes).expect("roundtrip");
        assert_eq!(back.compute_txid(), tx.compute_txid());
    }

    fn dummy_tx(n: u8) -> Transaction {
        // Differ non-witness fields so txids differ (witness is excluded from txid).
        dummy_tx_with_spk_pad(n, 0)
    }

    /// Like [`dummy_tx`] but pads `script_pubkey` so serialized size varies.
    ///
    /// `pad_bytes` is the OP_RETURN payload length (distinct non-witness data →
    /// distinct txids when `n` also differs).
    fn dummy_tx_with_spk_pad(n: u8, pad_bytes: usize) -> Transaction {
        let script_pubkey = if pad_bytes == 0 {
            ScriptBuf::new()
        } else {
            // OP_RETURN + push of pad — grows consensus serialization without witnesses.
            let mut push = PushBytesBuf::with_capacity(pad_bytes);
            for _ in 0..pad_bytes {
                push.push(n).expect("pad within PushBytes capacity");
            }
            ScriptBuf::new_op_return(push)
        };
        Transaction {
            version: Version::TWO,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::new(),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(1000 + u64::from(n)),
                script_pubkey,
            }],
        }
    }

    #[test]
    fn add_list_clear() {
        let mut inv = TxInventory::new();
        let t0 = dummy_tx(0);
        let t1 = dummy_tx(1);
        let id0 = inv.add_tx(t0.clone()).expect("t0");
        let id1 = inv.add_tx(t1).expect("t1");
        assert_eq!(inv.list_txids(), vec![id0, id1]);
        inv.clear();
        assert!(inv.is_empty());
        assert_eq!(inv.total_bytes(), 0);
    }

    #[test]
    fn duplicate_txid_errors() {
        let mut inv = TxInventory::new();
        let t0 = dummy_tx(0);
        let id0 = inv.add_tx(t0.clone()).expect("first");
        let err = inv.add_tx(t0).expect_err("duplicate");
        match err {
            Error::DuplicateTxid(id) => assert_eq!(id, id0),
            other => panic!("expected DuplicateTxid, got {other}"),
        }
        assert_eq!(inv.len(), 1);
    }

    #[test]
    fn tx_cap_errors() {
        let mut inv = TxInventory::with_limits(InventoryLimits {
            max_txs: 2,
            max_bytes: DEFAULT_MAX_BYTES,
        });
        inv.add_tx(dummy_tx(0)).unwrap();
        inv.add_tx(dummy_tx(1)).unwrap();
        let err = inv.add_tx(dummy_tx(2)).expect_err("cap");
        assert!(matches!(err, Error::InventoryTxCap { have: 2, max: 2 }));
    }

    #[test]
    fn byte_cap_errors() {
        let tx = dummy_tx(0);
        let one_len = tx_serialized_len(&tx);
        let mut inv = TxInventory::with_limits(InventoryLimits {
            max_txs: 100,
            max_bytes: one_len,
        });
        inv.add_tx(tx).unwrap();
        let err = inv.add_tx(dummy_tx(1)).expect_err("byte cap");
        assert!(matches!(err, Error::InventoryByteCap { .. }));
    }

    #[test]
    fn take_all_leaves_room_for_concurrent_style_restore() {
        let mut inv = TxInventory::new();
        let a = dummy_tx(0);
        let b = dummy_tx(1);
        let c = dummy_tx(2);
        inv.add_tx(a.clone()).unwrap();
        inv.add_tx(b.clone()).unwrap();
        let taken = inv.take_all();
        assert_eq!(taken.len(), 2);
        assert!(inv.is_empty());
        // Concurrent add while mine in flight:
        inv.add_tx(c.clone()).unwrap();
        // Mine fails → restore taken only (not clear-all):
        inv.restore(taken);
        assert_eq!(inv.len(), 3);
        let ids = inv.list_txids();
        assert!(ids.contains(&c.compute_txid()));
        assert!(ids.contains(&a.compute_txid()));
        assert!(ids.contains(&b.compute_txid()));
    }

    #[test]
    fn restore_re_enforces_tx_cap() {
        let mut inv = TxInventory::with_limits(InventoryLimits {
            max_txs: 2,
            max_bytes: DEFAULT_MAX_BYTES,
        });
        let a = dummy_tx(0);
        let b = dummy_tx(1);
        inv.add_tx(a.clone()).unwrap();
        inv.add_tx(b.clone()).unwrap();
        let taken = inv.take_all();
        // Concurrent adds fill the cap while mine is in flight:
        let c = dummy_tx(2);
        let d = dummy_tx(3);
        inv.add_tx(c.clone()).unwrap();
        inv.add_tx(d.clone()).unwrap();
        assert_eq!(inv.len(), 2);
        inv.restore(taken);
        // Cap still holds; concurrent adds stay; restore overflow dropped.
        assert_eq!(inv.len(), 2);
        assert!(inv.len() <= inv.limits().max_txs);
        let ids = inv.list_txids();
        assert!(ids.contains(&c.compute_txid()));
        assert!(ids.contains(&d.compute_txid()));
        assert!(!ids.contains(&a.compute_txid()));
        assert!(!ids.contains(&b.compute_txid()));
    }

    #[test]
    fn restore_re_enforces_byte_cap() {
        let tx = dummy_tx(0);
        let one_len = tx_serialized_len(&tx);
        let mut inv = TxInventory::with_limits(InventoryLimits {
            max_txs: 100,
            max_bytes: one_len,
        });
        inv.add_tx(tx.clone()).unwrap();
        let taken = inv.take_all();
        // Concurrent add fills the byte budget:
        let concurrent = dummy_tx(1);
        inv.add_tx(concurrent.clone()).unwrap();
        inv.restore(taken);
        assert_eq!(inv.len(), 1);
        assert!(inv.total_bytes() <= inv.limits().max_bytes);
        assert_eq!(inv.list_txids(), vec![concurrent.compute_txid()]);
    }

    /// Restore byte path: skip an oversize candidate, still accept a later smaller one.
    ///
    /// Guards against a `continue`→`break` regression that would stop scanning after
    /// the first oversize restore candidate.
    #[test]
    fn restore_byte_cap_skips_oversize_then_fits_smaller() {
        let small = dummy_tx_with_spk_pad(1, 0);
        let large = dummy_tx_with_spk_pad(2, 200);
        let small_len = tx_serialized_len(&small);
        let large_len = tx_serialized_len(&large);
        assert!(
            large_len > small_len * 2,
            "fixture sizes: large={large_len} small={small_len}"
        );

        // Budget: concurrent fills most of it; remaining headroom fits small but not large.
        let max_bytes = small_len * 3;
        let mut inv = TxInventory::with_limits(InventoryLimits {
            max_txs: 100,
            max_bytes,
        });

        // Concurrent adds while mine in flight: occupy 2× small_len of budget.
        let concurrent = dummy_tx_with_spk_pad(3, 0);
        inv.add_tx(concurrent.clone()).unwrap();
        inv.add_tx(dummy_tx_with_spk_pad(4, 0)).unwrap();
        assert_eq!(inv.total_bytes(), small_len * 2);
        let headroom = max_bytes - inv.total_bytes();
        assert!(
            headroom < large_len && headroom >= small_len,
            "headroom={headroom} large={large_len} small={small_len}"
        );

        // Failed-mine snapshot: [large, small]. Built outside the tight inventory so
        // both sizes can appear in `taken` even though large alone exceeds headroom.
        let taken = vec![large.clone(), small.clone()];
        inv.restore(taken);

        let ids = inv.list_txids();
        assert!(
            !ids.contains(&large.compute_txid()),
            "oversize restore candidate must be skipped"
        );
        assert!(
            ids.contains(&small.compute_txid()),
            "smaller restore candidate after oversize must still fit"
        );
        assert!(ids.contains(&concurrent.compute_txid()));
        assert_eq!(inv.len(), 3); // 2 concurrent + small
        assert!(
            inv.total_bytes() <= max_bytes,
            "total_bytes {} exceeds max_bytes {max_bytes}",
            inv.total_bytes()
        );
        // seen and len stay consistent (no phantom entries).
        assert_eq!(inv.list_txids().len(), inv.len());
    }

    /// Partial tx-cap room: concurrent left one free slot; only first of taken is restored.
    ///
    /// Insertion order after restore: concurrent adds first, then restored in taken order
    /// until `max_txs` (remaining restore candidates dropped, not error).
    #[test]
    fn restore_partial_tx_cap_room() {
        let mut inv = TxInventory::with_limits(InventoryLimits {
            max_txs: 2,
            max_bytes: DEFAULT_MAX_BYTES,
        });
        let a = dummy_tx(0);
        let b = dummy_tx(1);
        inv.add_tx(a.clone()).unwrap();
        inv.add_tx(b.clone()).unwrap();
        let taken = inv.take_all();
        assert_eq!(taken.len(), 2);

        // Concurrent add uses 1 of 2 slots → one free for restore.
        let concurrent = dummy_tx(2);
        inv.add_tx(concurrent.clone()).unwrap();
        assert_eq!(inv.len(), 1);

        inv.restore(taken);

        assert_eq!(inv.len(), 2);
        assert!(inv.len() <= inv.limits().max_txs);
        let ids = inv.list_txids();
        // Concurrent stays; first of taken (a) restored; second (b) dropped for cap.
        assert!(ids.contains(&concurrent.compute_txid()));
        assert!(ids.contains(&a.compute_txid()));
        assert!(!ids.contains(&b.compute_txid()));
        // Order: concurrent first (already present), then a appended by restore.
        assert_eq!(
            ids,
            vec![concurrent.compute_txid(), a.compute_txid()],
            "restore appends in taken order into free slots"
        );
    }

    /// Restore-path duplicate skip: concurrent re-added A; restore of [A, B] keeps A once.
    #[test]
    fn restore_skips_duplicate_txid() {
        let mut inv = TxInventory::new();
        let a = dummy_tx(0);
        let b = dummy_tx(1);
        inv.add_tx(a.clone()).unwrap();
        inv.add_tx(b.clone()).unwrap();
        let taken = inv.take_all();
        assert_eq!(taken.len(), 2);

        // Concurrent re-add of A (same txid as first of taken).
        inv.add_tx(a.clone()).unwrap();
        assert_eq!(inv.len(), 1);

        inv.restore(taken);

        let ids = inv.list_txids();
        let a_count = ids.iter().filter(|id| **id == a.compute_txid()).count();
        assert_eq!(a_count, 1, "A must appear once (restore skips duplicate)");
        assert!(ids.contains(&b.compute_txid()));
        assert_eq!(inv.len(), 2);
        assert_eq!(inv.list_txids().len(), inv.len());
        // total_bytes matches sum of serialized sizes of unique held txs.
        let expected_bytes: usize = inv.transactions().iter().map(tx_serialized_len).sum();
        assert_eq!(inv.total_bytes(), expected_bytes);
    }

    #[test]
    fn raw_hex_roundtrip() {
        let mut inv = TxInventory::new();
        let tx = dummy_tx(7);
        assert_roundtrip_bytes(&tx);
        let hex = serialize_hex(&tx);
        let id = inv.add_raw_hex(&hex).expect("hex ok");
        assert_eq!(id, tx.compute_txid());
        assert_eq!(inv.list_hex(), vec![hex]);
    }

    #[test]
    fn empty_take_all() {
        let mut inv = TxInventory::new();
        assert!(inv.take_all().is_empty());
    }
}
