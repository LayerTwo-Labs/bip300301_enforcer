//! Behavioral tests that pin architecture claims from FINAL_REPORT.md.
//!
//! Each trial maps to a documented claim; failures mean the claim (or Core/enforcer
//! behavior) changed and docs must be updated.
//!
//! | Trial | Claim |
//! |-------|--------|
//! | `cusf_claim_testmempoolaccept_no_insert` | `testmempoolaccept` never inserts into `mapTx` |
//! | `cusf_claim_stock_rejects_p2mr_spend` (bip360) | Stock Alice mempool rejects enforcer P2MR spends |
//! | `cusf_claim_invalid_drivechain_block_invalidated` | Drivechain enforcer `invalidateblock` on bad M1 |

use std::time::Duration;

use bip300301_enforcer_lib::bins::CommandExt as _;
use bitcoin::consensus::encode::serialize_hex;

use crate::setup::PostSetup;

// ─── Claim: testmempoolaccept does not admit ───────────────────────────────

/// After `testmempoolaccept`, the txid must be absent from `getrawmempool`.
/// Control: the same hex via `sendrawtransaction` appears in the mempool.
///
/// Uses bitcoind wallet only (validator-only enforcer / mature coinbases).
pub async fn test_cusf_claim_testmempoolaccept_no_insert(
    post_setup: PostSetup,
) -> anyhow::Result<()> {
    let dest = post_setup.receive_address.to_string();
    // outputs JSON object form required by createrawtransaction
    let outputs = format!(r#"{{"{dest}":0.001}}"#);
    let raw = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "createrawtransaction", ["[]".to_string(), outputs])
        .run_utf8()
        .await?;
    let funded = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "fundrawtransaction", [raw.trim().to_string()])
        .run_utf8()
        .await?;
    let funded_val: serde_json::Value = serde_json::from_str(&funded)?;
    let hex = funded_val
        .get("hex")
        .and_then(|h| h.as_str())
        .ok_or_else(|| anyhow::anyhow!("fundrawtransaction missing hex: {funded}"))?
        .to_string();
    let signed = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "signrawtransactionwithwallet", [hex])
        .run_utf8()
        .await?;
    let signed_val: serde_json::Value = serde_json::from_str(&signed)?;
    let signed_hex = signed_val
        .get("hex")
        .and_then(|h| h.as_str())
        .ok_or_else(|| anyhow::anyhow!("signrawtransactionwithwallet missing hex: {signed}"))?
        .to_string();
    anyhow::ensure!(
        signed_val.get("complete").and_then(|c| c.as_bool()) == Some(true),
        "wallet could not fully sign: {signed}"
    );

    let decoded = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "decoderawtransaction", [signed_hex.clone()])
        .run_utf8()
        .await?;
    let decoded_val: serde_json::Value = serde_json::from_str(&decoded)?;
    let txid = decoded_val
        .get("txid")
        .and_then(|t| t.as_str())
        .ok_or_else(|| anyhow::anyhow!("decoderawtransaction missing txid"))?
        .to_string();

    let accept = post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "testmempoolaccept", [format!(r#"["{signed_hex}"]"#)])
        .run_utf8()
        .await?;
    tracing::info!(%accept, %txid, "testmempoolaccept result");

    let mempool = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getrawmempool", [])
        .run_utf8()
        .await?;
    anyhow::ensure!(
        !mempool.contains(&txid),
        "CLAIM FAIL: testmempoolaccept left txid {txid} in getrawmempool: {mempool}\n\
         Claim: test_accept never inserts into mapTx (FINAL_REPORT §3.3)"
    );

    drop(
        post_setup
            .bitcoin_cli
            .command::<String, _, _, _, _>([], "sendrawtransaction", [signed_hex, "0".to_string()])
            .run_utf8()
            .await?,
    );
    let mempool_after = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getrawmempool", [])
        .run_utf8()
        .await?;
    anyhow::ensure!(
        mempool_after.contains(&txid),
        "control failed: sendraw did not put {txid} in mempool: {mempool_after}"
    );

    tracing::info!(%txid, "PASS claim: testmempoolaccept no insert; sendraw inserts");
    Ok(())
}

// ─── Claim: stock Core rejects P2MR spends in mempool (bip360) ─────────────

#[cfg(feature = "bip360")]
pub async fn test_cusf_claim_stock_rejects_p2mr_spend(
    pre_setup: crate::setup::PreSetup,
) -> anyhow::Result<()> {
    use bip300301_enforcer_lib::validator::pqc::signer_dev::SignAlgorithm;
    use futures::channel::mpsc;

    use crate::{
        bip360_block::{
            SCHNORR_ENTROPY, bip360_setup_opts, build_block_with_coinbase,
            build_valid_p2mr_spend_txs, funding_prevout, prepare_coinbase, submit_block,
            wait_for_enforcer_synced,
        },
        block_verdict::{Expect, assert_enforcer_verdict},
        setup::Mode,
    };

    let (res_tx, _res_rx) = mpsc::unbounded();
    let mut post = pre_setup
        .setup(Mode::NoMempool, bip360_setup_opts(), res_tx)
        .await?;
    wait_for_enforcer_synced(&mut post).await?;

    let prev = funding_prevout(&post).await?;
    let (funding_tx, spend_tx) =
        build_valid_p2mr_spend_txs(&post, prev, SignAlgorithm::Schnorr, &SCHNORR_ENTROPY).await?;

    // Confirm funding only (P2MR *output* creation) so the spend is a loose tx candidate.
    let (template, coinbase) = prepare_coinbase(&mut post).await?;
    let fund_block =
        build_block_with_coinbase(&post, &template, coinbase, vec![funding_tx.clone()]).await?;
    let fund_hash = submit_block(&mut post, &fund_block).await?;
    assert_enforcer_verdict(
        &mut post,
        fund_hash,
        Expect::Accepted,
        Duration::from_secs(15),
    )
    .await?;

    let spend_hex = serialize_hex(&spend_tx);
    let spend_txid = spend_tx.compute_txid().to_string();

    let accept = post
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "testmempoolaccept", [format!(r#"["{spend_hex}"]"#)])
        .run_utf8()
        .await?;
    tracing::info!(%accept, "stock testmempoolaccept for P2MR spend");
    let accept_val: serde_json::Value = serde_json::from_str(&accept)?;
    let allowed = accept_val
        .as_array()
        .and_then(|a| a.first())
        .and_then(|e| e.get("allowed"))
        .and_then(|a| a.as_bool());
    anyhow::ensure!(
        allowed != Some(true),
        "CLAIM FAIL: stock testmempoolaccept allowed P2MR spend {spend_txid}: {accept}\n\
         Claim: stock Alice mempool rejects P2MR spends (FINAL_REPORT §7)"
    );

    let send_err = post
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "sendrawtransaction", [spend_hex, "0".to_string()])
        .run_utf8()
        .await;
    anyhow::ensure!(
        send_err.is_err(),
        "CLAIM FAIL: stock sendraw accepted P2MR spend {spend_txid}\n\
         Claim: stock Alice mempool rejects P2MR spends (FINAL_REPORT §7)"
    );
    tracing::info!(
        error = %format!("{:#}", send_err.as_ref().err().unwrap()),
        %spend_txid,
        funding = %funding_tx.compute_txid(),
        "PASS claim: stock Core rejects P2MR spend (testmempoolaccept + sendraw)"
    );
    Ok(())
}
