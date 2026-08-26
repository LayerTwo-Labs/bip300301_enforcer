//! BIP23 `getblocktemplate` proposal mode against the enforcer's JSON-RPC
//! endpoint.
//!
//! The enforcer dictates a coinbase, and until proposal mode existed a builder
//! that deviated from it only found out by mining a block and having it
//! invalidated. These trials check the four answers a proposal can produce:
//! accepted, rejected on the block's own self-consistency, rejected by a
//! BIP300 rule, and unanswerable because the block does not build on the tip.
//!
//! Proof of work is deliberately absent from every block built here. A
//! proposal is made *before* the block is mined, so a server that insisted on
//! valid work would reject every honest proposal.
//!
//! The error assertions pin Bitcoin Core's exact codes and messages, since the
//! point of this endpoint is that a miner can repoint an existing
//! `getblocktemplate` call at it. They are linked to Core's source at
//! `6c4fe401e9`, the same tree `app/main.rs` pins for its RPC codes.

use bip300301_enforcer_lib::{
    messages::CoinbaseBuilder,
    types::{SidechainDescription, SidechainNumber, SidechainProposal},
};
use bitcoin::hashes::Hash as _;
use cusf_enforcer_mempool::server::RpcClient as _;

use crate::setup::PostSetup;

pub const TEST_NAME: &str = "gbt_proposal";

/// The `getblocktemplate` params object, built by hand rather than through
/// `BlockTemplateRequest`, so that a mode the type does not model can be sent.
fn proposal_params(mode: serde_json::Value, data: Option<&str>) -> serde_json::Value {
    let mut params = serde_json::json!({
        "mode": mode,
        "rules": ["segwit"],
        "capabilities": ["coinbasetxn"],
    });
    if let Some(data) = data {
        params["data"] = serde_json::Value::String(data.to_owned());
    }
    params
}

/// Send a raw `getblocktemplate` and return the verdict: `None` for JSON
/// `null` (accepted), `Some(reason)` for a rejection string.
async fn propose_hex(
    post_setup: &PostSetup,
    hex: &str,
) -> Result<Option<String>, jsonrpsee::core::client::Error> {
    use jsonrpsee::core::client::ClientT as _;

    post_setup
        .gbt_client
        .request(
            "getblocktemplate",
            jsonrpsee::rpc_params![proposal_params("proposal".into(), Some(hex))],
        )
        .await
}

async fn propose(
    post_setup: &PostSetup,
    block: &bitcoin::Block,
) -> Result<Option<String>, jsonrpsee::core::client::Error> {
    propose_hex(
        post_setup,
        &bitcoin::consensus::encode::serialize_hex(block),
    )
    .await
}

/// Raw hex of the block bitcoind currently has at its tip.
async fn tip_block_hex(post_setup: &PostSetup) -> anyhow::Result<String> {
    use bip300301_enforcer_lib::bins::CommandExt as _;

    let block_hash = post_setup
        .bitcoin_cli
        .command::<String, _, String, _, _>([], "getbestblockhash", [])
        .run_utf8()
        .await?
        .trim()
        .to_owned();
    Ok(post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "getblock", [block_hash, "0".to_owned()])
        .run_utf8()
        .await?
        .trim()
        .to_owned())
}

/// Build an unmined block from the enforcer's current template, exactly as a
/// miner would before grinding a nonce.
async fn block_from_template(post_setup: &PostSetup) -> anyhow::Result<bitcoin::Block> {
    let mut request = bitcoin_jsonrpsee::client::BlockTemplateRequest::default();
    request.capabilities.insert("coinbasetxn".to_owned());
    let template = crate::util::expect_block_template(
        post_setup.gbt_client.get_block_template(request).await?,
    )?;

    anyhow::ensure!(
        template
            .capabilities
            .iter()
            .any(|capability| capability == "proposal"),
        "BIP23 requires a server that accepts proposals to advertise `proposal` \
         in the template's `capabilities`, got {:?}",
        template.capabilities
    );

    let bitcoin_jsonrpsee::client::CoinbaseTxnOrValue::Txn(coinbase_txn) =
        template.coinbase_txn_or_value
    else {
        anyhow::bail!("template has no `coinbasetxn`");
    };
    let txdata: Vec<bitcoin::Transaction> = std::iter::once(&coinbase_txn)
        .chain(&template.transactions)
        .map(|tx| bitcoin::consensus::deserialize(&tx.data))
        .collect::<Result<_, _>>()?;
    let header = bitcoin::block::Header {
        version: template.version,
        prev_blockhash: template.prev_blockhash,
        merkle_root: bitcoin::TxMerkleNode::all_zeros(), // set by `reseal`
        time: std::cmp::max(template.current_time, template.mintime) as u32,
        bits: template.compact_target,
        // Left at zero: a proposal is checked without proof of work.
        nonce: 0,
    };
    let mut block = bitcoin::Block { header, txdata };
    reseal(&mut block);
    Ok(block)
}

/// Recompute the merkle root after the transactions have been edited. Any
/// builder that rewrites a template's coinbase has to do this, and forgetting
/// it is one of the mistakes proposal mode is meant to catch.
fn reseal(block: &mut bitcoin::Block) {
    block.header.merkle_root = block.compute_merkle_root().unwrap();
}

/// A syntactically valid M1, for a slot the harness does not otherwise use.
fn m1_txout(slot: u8) -> anyhow::Result<bitcoin::TxOut> {
    let mut txouts = Vec::new();
    let mut builder = CoinbaseBuilder::new(&mut txouts)?;
    builder.propose_sidechain(SidechainProposal {
        sidechain_number: SidechainNumber(slot),
        description: SidechainDescription::from(b"gbt proposal test".to_vec()),
    })?;
    let mut extension = builder.build_extension()?;
    anyhow::ensure!(extension.len() == 1, "expected exactly one M1 output");
    Ok(extension.remove(0))
}

pub async fn test_gbt_proposal(post_setup: PostSetup) -> anyhow::Result<()> {
    // 1. The template the enforcer just served must be acceptable to it. If
    // this fails, nothing below distinguishes a real rejection from a
    // proposal path that rejects everything.
    let block = block_from_template(&post_setup).await?;
    let verdict = propose(&post_setup, &block).await?;
    anyhow::ensure!(
        verdict.is_none(),
        "the enforcer rejected a block built from its own template: {verdict:?}"
    );
    tracing::info!("unmodified template block accepted as a proposal");

    // 2. Self-consistency: a merkle root that does not match the transactions.
    // Caught locally, without consulting the node.
    {
        let mut bad = block.clone();
        bad.header.merkle_root = bitcoin::TxMerkleNode::all_zeros();
        let verdict = propose(&post_setup, &bad).await?;
        anyhow::ensure!(
            verdict.as_deref() == Some("bad-txnmrklroot"),
            "a block whose merkle root does not match its transactions must be \
             rejected as `bad-txnmrklroot`, got {verdict:?}"
        );
    }

    // 3. Not on the tip. The enforcer can only evaluate a block against its
    // own tip, so this is unanswerable rather than invalid.
    {
        let mut stale = block.clone();
        stale.header.prev_blockhash = bitcoin::BlockHash::all_zeros();
        let verdict = propose(&post_setup, &stale).await?;
        anyhow::ensure!(
            verdict.as_deref() == Some("inconclusive-not-best-prevblk"),
            "a block that does not build on the tip must be reported as \
             `inconclusive-not-best-prevblk`, got {verdict:?}"
        );
    }

    // 3b. A block the node already has. Only bitcoind can answer `duplicate`,
    // so this fails if the proposal stops being forwarded: the enforcer on its
    // own would say `inconclusive-not-best-prevblk`, since the tip does not
    // build on itself.
    {
        let verdict = propose_hex(&post_setup, &tip_block_hex(&post_setup).await?).await?;
        anyhow::ensure!(
            verdict.as_deref() == Some("duplicate"),
            "a block the node already has must be reported as `duplicate`, got {verdict:?}"
        );
    }

    // 4. A BIP300 rule. Two identical M1s in one coinbase, which is what a
    // pool splicing its own messages into the enforcer's coinbase produces
    // when it does not know the enforcer already emitted one.
    //
    // This is the case that distinguishes the enforcer's answer from
    // bitcoind's: the block below is consensus-valid, and only the enforcer
    // rejects it.
    {
        let mut duplicate_m1 = block.clone();
        let m1 = m1_txout(7)?;
        let coinbase = duplicate_m1
            .txdata
            .first_mut()
            .ok_or_else(|| anyhow::anyhow!("block has no coinbase"))?;
        coinbase.output.push(m1.clone());
        coinbase.output.push(m1);
        reseal(&mut duplicate_m1);

        // Appending outputs after the witness commitment leaves it intact: it
        // commits to witness data, not to the coinbase's own outputs. Assert
        // it, so a failure below is attributable to the M1s.
        anyhow::ensure!(
            duplicate_m1.check_witness_commitment(),
            "the doubled-M1 block must remain consensus-valid, or it does not \
             isolate the BIP300 rule under test"
        );

        let verdict = propose(&post_setup, &duplicate_m1).await?;
        let reason = verdict.ok_or_else(|| {
            anyhow::anyhow!(
                "a coinbase carrying the same M1 twice must be rejected, but the \
                 enforcer accepted it"
            )
        })?;
        anyhow::ensure!(
            reason.contains("M1 sidechain proposal"),
            "expected the duplicate-M1 rule to be named in the rejection, got `{reason}`"
        );
        tracing::info!("duplicate M1 rejected as `{reason}`");
    }

    // 5. An unrecognised mode must be an error, not a silently served
    // template. This is the failure the endpoint had before proposal mode:
    // a caller checking only for an RPC error read a template as "accepted".
    {
        use jsonrpsee::core::client::ClientT as _;

        // https://github.com/bitcoin/bitcoin/blob/6c4fe401e908cff1b67d80035b117aae15fe7db6/src/rpc/mining.cpp#L763
        let res: Result<serde_json::Value, _> = post_setup
            .gbt_client
            .request(
                "getblocktemplate",
                jsonrpsee::rpc_params![proposal_params("nonsense".into(), None)],
            )
            .await;
        let jsonrpsee::core::client::Error::Call(err) = res.err().ok_or_else(|| {
            anyhow::anyhow!("`mode: \"nonsense\"` must be an error, not a template")
        })?
        else {
            anyhow::bail!(
                "`mode: \"nonsense\"` must fail as a JSON-RPC error, not at the transport"
            )
        };
        anyhow::ensure!(
            err.code() == -8 && err.message() == "Invalid mode",
            "an unrecognised mode must report -8 `Invalid mode`, the code and \
             message Bitcoin Core uses, but it reported {} ({})",
            err.code(),
            err.message()
        );
    }

    // 5b. A `mode` that is not a string at all. Bitcoin Core reports this the
    // same way as an unrecognised mode, rather than as malformed params.
    // https://github.com/bitcoin/bitcoin/blob/6c4fe401e908cff1b67d80035b117aae15fe7db6/src/rpc/mining.cpp#L726
    {
        use jsonrpsee::core::client::ClientT as _;

        let res: Result<serde_json::Value, _> = post_setup
            .gbt_client
            .request(
                "getblocktemplate",
                jsonrpsee::rpc_params![proposal_params(serde_json::json!(42), None)],
            )
            .await;
        let jsonrpsee::core::client::Error::Call(err) = res
            .err()
            .ok_or_else(|| anyhow::anyhow!("a non-string `mode` must be an error"))?
        else {
            anyhow::bail!("a non-string `mode` must fail as a JSON-RPC error")
        };
        anyhow::ensure!(
            err.code() == -8 && err.message() == "Invalid mode",
            "a non-string `mode` must report -8 `Invalid mode`, got {} ({})",
            err.code(),
            err.message()
        );
    }

    // 5c. A proposal with no usable `data`. Core tests this with `isStr()`, so
    // absent and present-but-not-a-string are the same error -- hence the
    // "String key" in the message -- and it is `RPC_TYPE_ERROR`, not
    // `RPC_INVALID_PARAMETER`.
    // https://github.com/bitcoin/bitcoin/blob/6c4fe401e908cff1b67d80035b117aae15fe7db6/src/rpc/mining.cpp#L733
    for params in [
        proposal_params("proposal".into(), None),
        serde_json::json!({
            "mode": "proposal",
            "data": 42,
            "rules": ["segwit"],
            "capabilities": ["coinbasetxn"],
        }),
    ] {
        use jsonrpsee::core::client::ClientT as _;

        let res: Result<serde_json::Value, _> = post_setup
            .gbt_client
            .request("getblocktemplate", jsonrpsee::rpc_params![&params])
            .await;
        let jsonrpsee::core::client::Error::Call(err) = res.err().ok_or_else(|| {
            anyhow::anyhow!("a proposal with no usable `data` must be an error: {params}")
        })?
        else {
            anyhow::bail!("a proposal with no usable `data` must fail as a JSON-RPC error")
        };
        anyhow::ensure!(
            err.code() == -3 && err.message() == "Missing data String key for proposal",
            "`{params}` must report -3 (`RPC_TYPE_ERROR`) with Core's message, \
             got {} ({})",
            err.code(),
            err.message()
        );
    }

    // 6. Undecodable `data` is an error too, matching Core's `-22`.
    // https://github.com/bitcoin/bitcoin/blob/6c4fe401e908cff1b67d80035b117aae15fe7db6/src/rpc/mining.cpp#L737
    {
        use jsonrpsee::core::client::ClientT as _;

        let res: Result<serde_json::Value, _> = post_setup
            .gbt_client
            .request(
                "getblocktemplate",
                jsonrpsee::rpc_params![proposal_params("proposal".into(), Some("deadbeef"))],
            )
            .await;
        let jsonrpsee::core::client::Error::Call(err) = res
            .err()
            .ok_or_else(|| anyhow::anyhow!("undecodable `data` must be an error"))?
        else {
            anyhow::bail!("undecodable `data` must fail as a JSON-RPC error")
        };
        anyhow::ensure!(
            err.code() == -22 && err.message() == "Block decode failed",
            "undecodable `data` must report -22 (`RPC_DESERIALIZATION_ERROR`) with \
             Core's message, got {} ({})",
            err.code(),
            err.message()
        );
    }

    // The proposals above are dry runs: none of them may have advanced the
    // enforcer, or a rejected proposal could poison the chain state.
    let after = block_from_template(&post_setup).await?;
    anyhow::ensure!(
        after.header.prev_blockhash == block.header.prev_blockhash,
        "proposals moved the enforcer's tip from {} to {}",
        block.header.prev_blockhash,
        after.header.prev_blockhash
    );

    Ok(())
}
