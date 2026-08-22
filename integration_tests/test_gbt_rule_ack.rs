//! Node builds that enforce the drivechain rules refuse to serve
//! `getblocktemplate` in `template` mode unless the client acknowledges the
//! `bip300301` rule: what they hand out is a plain layer 1 template, missing
//! the BIP300/BIP301 commitments that only the enforcer adds, and mining it
//! unfinished would orphan sidechain activity.
//!
//! This pins both halves of that contract from the enforcer's side, and stays
//! meaningful against node builds without the gate -- there, the extra rule the
//! enforcer always sends has to be harmless, which is what lets us send it
//! unconditionally.

use bip300301_enforcer_lib::{bins::CommandExt as _, rpc_client::BIP300301_RULE};

use crate::setup::PostSetup;

pub const TEST_NAME: &str = "gbt_rule_ack";

/// `getblocktemplate` in `template` mode, with `rules` as the JSON array body.
/// The error is returned as a string: it is the node's refusal message, which
/// is the thing under test, not a failure of the harness.
async fn request_template(
    post_setup: &PostSetup,
    rules: &str,
) -> Result<serde_json::Value, String> {
    let request = format!(r#"{{"rules":[{rules}]}}"#);
    match post_setup
        .bitcoin_cli
        .command::<String, _, _, _, _>([], "getblocktemplate", [request])
        .run_utf8()
        .await
    {
        Ok(json) => serde_json::from_str(&json).map_err(|err| err.to_string()),
        Err(err) => Err(err.to_string()),
    }
}

fn template_rules(template: &serde_json::Value) -> anyhow::Result<Vec<String>> {
    template["rules"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("block template has no `rules` array"))?
        .iter()
        .map(|rule| {
            rule.as_str()
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("block template rule is not a string: {rule}"))
        })
        .collect()
}

pub async fn test_gbt_rule_ack(post_setup: PostSetup) -> anyhow::Result<()> {
    let unacked = request_template(&post_setup, r#""segwit""#).await;
    let acked = request_template(&post_setup, &format!(r#""segwit","{BIP300301_RULE}""#)).await;

    // Whatever the node build, the request the enforcer itself makes must be
    // served. A node that rejects this is one the enforcer cannot mine on.
    let template = acked.map_err(|err| {
        anyhow::anyhow!("node refused the enforcer's own template request: {err}")
    })?;
    let rules = template_rules(&template)?;
    let mandatory = format!("!{BIP300301_RULE}");

    match unacked {
        // Gated build: the refusal has to name the rule that fixes it, and the
        // template we do get has to carry the BIP9 `!` marker, so that a miner
        // which does not understand the rule refuses the template in turn.
        Err(err) => {
            anyhow::ensure!(
                err.contains(BIP300301_RULE),
                "node refused a template without `{BIP300301_RULE}`, but its error does not name \
                 the rule: {err}"
            );
            anyhow::ensure!(
                rules.contains(&mandatory),
                "node requires `{BIP300301_RULE}` of its clients, but does not mark its templates \
                 `{mandatory}`; got {rules:?}"
            );
        }
        // Ungated build (stock Core, bitcoin-patched): nothing to enforce. Hold
        // it to the converse, so this cannot quietly pass on a gated node whose
        // gate has regressed.
        Ok(_) => {
            anyhow::ensure!(
                !rules.contains(&mandatory),
                "node marks its templates `{mandatory}` but served one to a client that never \
                 acknowledged `{BIP300301_RULE}`"
            );
        }
    }
    Ok(())
}
