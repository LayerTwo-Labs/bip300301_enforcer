//! A block that bitcoind accepts but the enforcer rejects produces no
//! validator event, so a mining helper that only waits on the event stream
//! hangs the trial forever. `mine_gbt_check` must fail instead.

use std::convert::Infallible;

use bip300301_enforcer_lib::{messages::M1ProposeSidechain, types::SidechainDescription};
use bitcoin::{Amount, ScriptBuf, TxOut};
use either::Either;

use crate::{
    mine::{MineGbtError, mine_gbt_check_with_coinbase_outputs},
    setup::{DummySidechain, PostSetup, Sidechain},
};

pub async fn test_mine_rejected_block(mut post_setup: PostSetup) -> anyhow::Result<()> {
    let m1: ScriptBuf = M1ProposeSidechain {
        sidechain_number: DummySidechain::SIDECHAIN_NUMBER,
        description: SidechainDescription(b"mine_rejected_block".to_vec()),
    }
    .try_into()?;
    let m1 = TxOut {
        script_pubkey: m1,
        value: Amount::ZERO,
    };
    // Proposing the same sidechain twice in one coinbase is invalid.
    let duplicate_m1 = [m1.clone(), m1];
    let res = mine_gbt_check_with_coinbase_outputs::<_, Infallible, DummySidechain>(
        &mut post_setup,
        1,
        &duplicate_m1,
        |_| Ok(()),
    )
    .await;
    match res {
        Err(Either::Left(MineGbtError::BlockRejected { block_hash })) => {
            tracing::info!(%block_hash, "mining failed on the rejected block, as expected");
            Ok(())
        }
        other => anyhow::bail!("expected the rejected block to fail mining, got {other:?}"),
    }
}
