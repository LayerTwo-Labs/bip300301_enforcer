//! Tier B (TB-sendraw) — P2MR **Bob mempool** interop for enforcer-built spends.
//!
//! # What this asserts
//!
//! After funding a P2MR UTXO on the dual stack (stock Alice + cryptoquick P2MR Bob),
//! selected spends must be accepted into **Bob’s mempool** via `sendrawtransaction`
//! (no `submitblock` escape hatch).
//!
//! ## Shapes
//!
//! 1. **Shape 1 — Schnorr** (green gate): single-leaf `PUSH32 pk OP_CHECKSIG`, control
//!    `c1`, [`TapSighashType::Default`] bare 64 B. **Must PASS** or the trial fails.
//! 2. **Shape 2 — Core-legal hybrid EC+SLH** (`OP_SUBSTR` / OP_SUCCESS127 second site,
//!    ~70 B leaf). Target green on Bob mempool. Enforcer verify rejects `OP_SUBSTR` —
//!    that is fine (Bob-only criterion). See `lib/validator/pqc/core_interop.rs`.
//! 3. **Shape 3 — Full overload kitchen-sink** (Schnorr+ML-DSA+SLH, all `OP_CHECKSIG`,
//!    control `c1`). **Hard green gate** after Core 3B multi-barrier fix (TAPSCRIPT
//!    stack+leaf size ≥ 8000 + size-gated OP_CHECKSIG for ML-DSA/SLH). Fixture:
//!    `pqc::kitchen_sink_3b`. Default trial runs shapes **1+2+3**.
//!    `BIP360_TIER_B_KITCHEN_SINK=1` is now redundant (same as default) and means
//!    “assert kitchen-sink required” (documented).
//!
//! # How to run (opt-in — not in default suite / it-all)
//!
//! ```bash
//! just setup-p2mr   # and just setup for electrs
//! just bip360-tier-b-mempool
//! # raw it: BIP360_TIER_B=1 just it bip360_tier_b_p2mr_mempool
//! ```
//!
//! See [`docs/TIER_B_P2MR_MEMPOOL.md`](../docs/TIER_B_P2MR_MEMPOOL.md).

#![cfg(feature = "bip360")]

use std::time::Duration;

use bip300301_enforcer_lib::validator::pqc::{
    core_interop::{CORE_HYBRID_LEAF_LEN, build_core_hybrid_ec_slh_spend_from_prevout},
    kitchen_sink_3b::{
        CORE_MAX_SCRIPT_ELEMENT_SIZE, CORE_MAX_SCRIPT_ELEMENT_SIZE_SLHDSA, GOLDEN_LEAF_SCRIPT_LEN,
        GOLDEN_MAX_LEAF_PUSH_SIZE, kitchen_sink_3b_protocol_vector,
    },
    limits::SLH_DSA_128S_SIGNATURE_SIZE,
    protocol_vectors::{
        GOLDEN_CONTROL_HEX, GOLDEN_PUBKEY_HEX, SCHNORR_VECTOR_ENTROPY, schnorr_protocol_vector,
    },
    signer_dev::{
        SignAlgorithm, build_kitchen_sink_spend_from_prevout, build_p2mr_spend_from_prevout,
        p2mr_output_for_algorithm,
    },
};
use bitcoin::sighash::TapSighashType;
use futures::{StreamExt as _, channel::mpsc};
use tracing::Instrument as _;

use crate::{
    bip360_block::{HYBRID_EC_ENTROPY, MLDSA_ENTROPY, SCHNORR_ENTROPY, SLH_ENTROPY},
    bip360_dual_node::{
        ROUND_SPEND_OUTPUT, TIER_B_TEST_NAME, assert_bob_p2mr_mempool_accepts_spend,
        fund_core_hybrid_p2mr, fund_kitchen_sink_p2mr, round0_wallet_to_p2mr,
        setup_tier_a_dual_node_peers,
    },
    util::{BinPaths, TestFileRegistry},
};

fn tier_b_opt_in() -> bool {
    matches!(
        std::env::var("BIP360_TIER_B").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

/// Shape 3 kitchen-sink is always required on the default TB-sendraw path (post-Core 3B).
/// `BIP360_TIER_B_KITCHEN_SINK=1` is redundant and means “assert kitchen-sink required”.
fn kitchen_sink_required() -> bool {
    // Default: always run shape 3. Env may still be set by older recipes; both mean required.
    true
}

async fn bip360_tier_b_p2mr_mempool_task(
    peers: crate::bip360_dual_node::DualNodePeers,
) -> anyhow::Result<()> {
    let mut peers = peers;
    anyhow::ensure!(
        peers.tier_a,
        "Tier B reuses Tier A topology (Alice=stock, Bob=BITCOIND_P2MR)"
    );

    tracing::info!(
        "Tier B (TB-sendraw): Bob mempool interop — shape 1 Schnorr + shape 2 Core hybrid \
         OP_SUBSTR + shape 3 overload kitchen-sink (docs/TIER_B_P2MR_MEMPOOL.md)"
    );

    // --- Shape 1: Schnorr-only (must PASS) — 3C protocol_vectors entropy ---
    // Funding (`round0`) and harness `SCHNORR_ENTROPY` share SCHNORR_VECTOR_ENTROPY.
    anyhow::ensure!(
        SCHNORR_ENTROPY == SCHNORR_VECTOR_ENTROPY,
        "harness SCHNORR_ENTROPY must match protocol_vectors::SCHNORR_VECTOR_ENTROPY"
    );
    let vector = schnorr_protocol_vector().map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        vector.control_block_hex == GOLDEN_CONTROL_HEX
            && vector.public_key_hex == GOLDEN_PUBKEY_HEX,
        "3C Schnorr golden fixture drift before mempool trial"
    );

    let pile = round0_wallet_to_p2mr(&mut peers).await?;
    tracing::info!(?pile.outpoint, "funding complete — shape 1 Schnorr spend");

    let (next_spk, _) = p2mr_output_for_algorithm(SignAlgorithm::Schnorr, &SCHNORR_VECTOR_ENTROPY)
        .map_err(anyhow::Error::msg)?;
    let schnorr_spend = build_p2mr_spend_from_prevout(
        SignAlgorithm::Schnorr,
        &SCHNORR_VECTOR_ENTROPY,
        TapSighashType::Default,
        pile.outpoint,
        pile.txout.clone(),
        ROUND_SPEND_OUTPUT,
        next_spk,
    )
    .map_err(anyhow::Error::msg)?;

    assert_eq!(
        schnorr_spend.input[0].witness.nth(0).map(|s| s.len()),
        Some(64),
        "shape 1 must use bare 64 B Default Schnorr"
    );
    assert_eq!(
        schnorr_spend.input[0].witness.nth(2),
        Some(&[0xc1][..]),
        "shape 1 control block Core wire c1"
    );
    // Leaf matches 3C golden pubkey (live spend has different sighash than synthetic vector).
    let leaf = schnorr_spend.input[0].witness.nth(1).expect("leaf");
    anyhow::ensure!(
        leaf.len() == 34 && &leaf[1..33] == hex::decode(GOLDEN_PUBKEY_HEX)?.as_slice(),
        "shape 1 leaf pubkey must match 3C golden fixture"
    );

    assert_bob_p2mr_mempool_accepts_spend(&peers, &schnorr_spend, &pile.txout, "schnorr")
        .await
        .map_err(|err| {
            anyhow::anyhow!(
                "{err}\n\
                 \n\
                 Shape 1 Schnorr is the TB-sendraw **green gate** — failure here is a \
                 protocol regression (control / merkle / sighash Default). See \
                 docs/TIER_B_P2MR_MEMPOOL.md and pqc::protocol_vectors."
            )
        })?;
    tracing::info!("shape 1 Schnorr: Bob mempool ACCEPT (green gate)");

    // --- Shape 2: Core-legal hybrid EC+SLH (OP_SUBSTR) ---
    // Not full kitchen-sink: ML-DSA leaf pushes still exceed Core element limits (3B).
    let hybrid_pile = fund_core_hybrid_p2mr(&mut peers).await?;
    let dest = peers.alice.mining_address.script_pubkey();
    let hybrid_spend = build_core_hybrid_ec_slh_spend_from_prevout(
        &HYBRID_EC_ENTROPY,
        &SLH_ENTROPY,
        hybrid_pile.outpoint,
        hybrid_pile.txout.clone(),
        ROUND_SPEND_OUTPUT,
        dest,
    )
    .map_err(anyhow::Error::msg)?;

    let w = &hybrid_spend.input[0].witness;
    anyhow::ensure!(
        w.len() == 4,
        "core hybrid witness depth (ec, slh, leaf, ctrl)"
    );
    anyhow::ensure!(
        w.nth(0).map(|s| s.len()) == Some(64),
        "core hybrid EC sig bare 64"
    );
    anyhow::ensure!(
        w.nth(1).map(|s| s.len()) == Some(SLH_DSA_128S_SIGNATURE_SIZE),
        "core hybrid SLH sig bare {SLH_DSA_128S_SIGNATURE_SIZE}"
    );
    anyhow::ensure!(
        w.nth(2).map(|s| s.len()) == Some(CORE_HYBRID_LEAF_LEN),
        "core hybrid leaf {CORE_HYBRID_LEAF_LEN} B"
    );
    anyhow::ensure!(w.nth(3) == Some(&[0xc1][..]), "core hybrid control c1");

    match assert_bob_p2mr_mempool_accepts_spend(
        &peers,
        &hybrid_spend,
        &hybrid_pile.txout,
        "core-hybrid-op-substr",
    )
    .await
    {
        Ok(()) => {
            tracing::info!(
                "shape 2 Core hybrid OP_SUBSTR: Bob mempool ACCEPT — TB-sendraw overall PASS"
            );
        }
        Err(err) => {
            // Prefer hard-pass both shapes when hybrid works. If Core policy/script still
            // rejects an honest OP_SUBSTR hybrid, keep the trial red with residual annotation
            // (Schnorr gate already green — do not soft-pass and hide shape-2).
            return Err(anyhow::anyhow!(
                "{err}\n\
                 \n\
                 Shape 1 Schnorr was green. Shape 2 Core hybrid OP_SUBSTR still rejected \
                 by Bob — residual (capture protocol dump above). Shape 3 kitchen-sink is \
                 also a hard green gate after Core 3B. See docs/TIER_B_P2MR_MEMPOOL.md."
            ));
        }
    }

    tracing::info!(
        "Tier B shapes 1+2 PASS: Bob P2MR mempool accepted Schnorr + Core hybrid OP_SUBSTR"
    );

    // --- Shape 3 (hard green gate post-Core 3B): full overload kitchen-sink ---
    // Multi-OP_CHECKSIG Schnorr+ML-DSA+SLH; Core must accept stack/leaf sizes + PQC verify.
    if kitchen_sink_required() {
        run_kitchen_sink_shape3_accept(&mut peers).await?;
    }

    tracing::info!(
        "Tier B PASS: Bob P2MR mempool accepted Schnorr (shape 1) + Core hybrid OP_SUBSTR \
         (shape 2) + overload kitchen-sink (shape 3)"
    );

    drop(peers.alice.tasks);
    drop(peers.bob.tasks);
    tokio::time::sleep(Duration::from_secs(1)).await;
    peers.alice.directories.base_dir.cleanup()?;
    peers.bob.directories.base_dir.cleanup()?;
    Ok(())
}

/// Shape 3 hard green gate: fund + send kitchen-sink to Bob; **must accept**.
///
/// Post-Core 3B: TAPSCRIPT stack/leaf element size ≥ 8000 and size-gated OP_CHECKSIG
/// (Schnorr / ML-DSA-44 / SLH-DSA). Fixture sizes still document historical 520 barriers.
async fn run_kitchen_sink_shape3_accept(
    peers: &mut crate::bip360_dual_node::DualNodePeers,
) -> anyhow::Result<()> {
    let vector = kitchen_sink_3b_protocol_vector().map_err(anyhow::Error::msg)?;
    anyhow::ensure!(
        vector.control_block_hex == GOLDEN_CONTROL_HEX
            && vector.leaf_script_len == GOLDEN_LEAF_SCRIPT_LEN
            && vector.max_leaf_push_size == GOLDEN_MAX_LEAF_PUSH_SIZE,
        "kitchen-sink golden fixture drift before shape 3 trial \
         (control c1, leaf_len 1387, max push 1312)"
    );
    // Historical 520 barriers: fixture still exceeds classic MAX_SCRIPT_ELEMENT_SIZE
    // so we know we're exercising the raised P2MR/TAPSCRIPT path (8000).
    anyhow::ensure!(
        vector.mldsa_sig_len > CORE_MAX_SCRIPT_ELEMENT_SIZE
            && vector.slh_sig_len > CORE_MAX_SCRIPT_ELEMENT_SIZE
            && vector.max_leaf_push_size > CORE_MAX_SCRIPT_ELEMENT_SIZE
            && vector.slh_sig_len < CORE_MAX_SCRIPT_ELEMENT_SIZE_SLHDSA,
        "kitchen-sink sizes must exceed classic 520 and fit under P2MR 8000"
    );

    let pile = fund_kitchen_sink_p2mr(peers).await?;
    tracing::info!(?pile.outpoint, "funding complete — shape 3 kitchen-sink");

    let dest = peers.alice.mining_address.script_pubkey();
    let kitchen_spend = build_kitchen_sink_spend_from_prevout(
        &HYBRID_EC_ENTROPY,
        &MLDSA_ENTROPY,
        &SLH_ENTROPY,
        TapSighashType::All,
        pile.outpoint,
        pile.txout.clone(),
        ROUND_SPEND_OUTPUT,
        dest,
    )
    .map_err(anyhow::Error::msg)?;

    let w = &kitchen_spend.input[0].witness;
    anyhow::ensure!(w.len() == 5, "kitchen-sink witness depth 5");
    anyhow::ensure!(w.nth(3).map(|s| s.len()) == Some(GOLDEN_LEAF_SCRIPT_LEN));
    anyhow::ensure!(w.nth(4) == Some(&[0xc1][..]), "kitchen-sink control c1");

    assert_bob_p2mr_mempool_accepts_spend(
        peers,
        &kitchen_spend,
        &pile.txout,
        "kitchen-sink-shape3",
    )
    .await
    .map_err(|err| {
        anyhow::anyhow!(
            "{err}\n\
             \n\
             Shape 3 kitchen-sink is a hard green gate after Core 3B (TAPSCRIPT element \
             size ≥ 8000 + multi-OP_CHECKSIG Schnorr/ML-DSA/SLH). Bob must accept via \
             sendraw + getmempoolentry. Capture protocol dump above. See \
             docs/TIER_B_P2MR_MEMPOOL.md and pqc::kitchen_sink_3b."
        )
    })?;

    tracing::info!(
        mldsa_sig_len = vector.mldsa_sig_len,
        slh_sig_len = vector.slh_sig_len,
        max_leaf_push = GOLDEN_MAX_LEAF_PUSH_SIZE,
        "shape 3 kitchen-sink: Bob ACCEPT (sendraw + mempool entry)"
    );
    Ok(())
}

/// Opt-in Tier B mempool interop trial (shapes 1 Schnorr + 2 hybrid + 3 kitchen-sink).
pub async fn test_bip360_tier_b_p2mr_mempool(
    bin_paths: BinPaths,
    file_registry: TestFileRegistry,
) -> anyhow::Result<()> {
    if !tier_b_opt_in() {
        tracing::info!(
            "skipping {TIER_B_TEST_NAME} (opt-in protocol interop). \
             Run: just bip360-tier-b-mempool \
             (or BIP360_TIER_B=1 just it bip360_tier_b_p2mr_mempool)"
        );
        return Ok(());
    }

    let (res_tx, mut res_rx) = mpsc::unbounded();
    let peers = setup_tier_a_dual_node_peers(bin_paths, &file_registry).await?;
    let _test_task: crate::util::AbortOnDrop<()> = tokio::task::spawn({
        async move {
            let res = bip360_tier_b_p2mr_mempool_task(peers).await;
            let _send_err: Result<(), _> = res_tx.unbounded_send(res);
        }
        .instrument(tracing::info_span!("test", name = TIER_B_TEST_NAME))
    })
    .into();
    res_rx
        .next()
        .await
        .ok_or_else(|| anyhow::anyhow!("unexpected end of Tier B mempool task result stream"))?
}
