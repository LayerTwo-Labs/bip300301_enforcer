//! Persistent P2MR UTXO set for cross-block prevout lookup during block connect.

use std::collections::HashMap;

use bitcoin::{OutPoint, TxOut};
use fallible_iterator::FallibleIterator;
use heed_types::SerdeBincode;
use sneed::{DatabaseUnique, Env, RoTxn, RwTxn, db, env};

#[derive(Clone)]
pub(in crate::validator) struct P2mrUtxoDbs {
    utxos: DatabaseUnique<SerdeBincode<OutPoint>, SerdeBincode<TxOut>>,
}

impl P2mrUtxoDbs {
    pub fn new(env: &Env, rwtxn: &mut RwTxn) -> Result<Self, env::error::CreateDb> {
        let utxos = DatabaseUnique::create(env, rwtxn, "p2mr_utxos")?;
        Ok(Self { utxos })
    }

    /// Load the confirmed-chain P2MR UTXO map.
    pub fn load_map(&self, rotxn: &RoTxn) -> Result<HashMap<OutPoint, TxOut>, db::Error> {
        let mut map = HashMap::new();
        let mut iter = self.utxos.iter(rotxn).map_err(db::error::Iter::from)?;
        while let Some((outpoint, txout)) = iter.next().map_err(db::error::Iter::from)? {
            map.insert(outpoint, txout);
        }
        Ok(map)
    }

    /// Apply a block's P2MR UTXO changes: remove spent outputs, add created ones.
    pub fn apply_diff(
        &self,
        rwtxn: &mut RwTxn,
        spent: &HashMap<OutPoint, TxOut>,
        created: &HashMap<OutPoint, TxOut>,
    ) -> Result<(), db::Error> {
        for outpoint in spent.keys() {
            // Require every spent key to exist on connect; a miss means validation/diff
            // diverged from DB state.
            drop(self.utxos.get(rwtxn, outpoint).map_err(db::Error::from)?);
            self.utxos.delete(rwtxn, outpoint)?;
        }
        for (outpoint, txout) in created {
            self.utxos.put(rwtxn, outpoint, txout)?;
        }
        Ok(())
    }

    /// Undo a block's P2MR UTXO changes: remove created outputs, restore spent ones.
    pub fn undo_diff(
        &self,
        rwtxn: &mut RwTxn,
        spent: &HashMap<OutPoint, TxOut>,
        created: &HashMap<OutPoint, TxOut>,
    ) -> Result<(), db::Error> {
        for outpoint in created.keys() {
            let _ = self.utxos.delete(rwtxn, outpoint)?;
        }
        for (outpoint, txout) in spent {
            self.utxos.put(rwtxn, outpoint, txout)?;
        }
        Ok(())
    }
}
