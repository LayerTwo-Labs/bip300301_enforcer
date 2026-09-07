//! Nothing the enforcer was configured with as a secret may appear in
//! anything it writes.
//!
//! The unit tests in `lib/cli.rs` cover how the startup configuration dump
//! renders a `SecretString`. This covers everything a *running* enforcer
//! emits around that -- RPC client output, error messages, span fields -- with
//! the harness's `trace` log level turned on, which is where a leak is most
//! likely to show up.

use bip300301_enforcer_lib::cli::SecretString;

use crate::{
    integration_test::fund_enforcer,
    setup::{DummySidechain, PostSetup},
    util::{assert_absent, enforcer_output},
};

pub const TEST_NAME: &str = "no_secrets_in_logs";

/// Standard base64, to reconstruct the HTTP Basic credentials the enforcer
/// sends to Bitcoin Core. A leak through an `Authorization` header would never
/// show the password in plaintext, so searching for the plaintext alone would
/// miss it.
fn base64_encode(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let bytes = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(bytes[0]) << 16) | (u32::from(bytes[1]) << 8) | u32::from(bytes[2]);
        out.push(char::from(ALPHABET[((n >> 18) & 63) as usize]));
        out.push(char::from(ALPHABET[((n >> 12) & 63) as usize]));
        out.push(if chunk.len() > 1 {
            char::from(ALPHABET[((n >> 6) & 63) as usize])
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            char::from(ALPHABET[(n & 63) as usize])
        } else {
            '='
        });
    }
    out
}

pub async fn test_no_secrets_in_logs(mut post_setup: PostSetup) -> anyhow::Result<()> {
    // Drive real work through the enforcer first, so the logs cover node RPC
    // traffic and wallet activity rather than just startup.
    fund_enforcer::<DummySidechain>(&mut post_setup).await?;

    // Take the secret from the harness rather than hardcoding it, so this
    // keeps testing the real value if the harness ever changes it.
    let rpc_user = post_setup
        .bitcoin_cli
        .rpc_user
        .clone()
        .ok_or_else(|| anyhow::anyhow!("harness has no rpc user"))?;
    let rpc_pass: SecretString = post_setup
        .bitcoin_cli
        .rpc_pass
        .clone()
        .ok_or_else(|| anyhow::anyhow!("harness has no rpc password to check for"))?;

    let files = enforcer_output(&post_setup.directories.enforcer_dir)?;
    let total_bytes: usize = files.iter().map(|(_, contents)| contents.len()).sum();
    tracing::info!(
        "scanning {} enforcer output file(s), {total_bytes} bytes",
        files.len()
    );

    // Non-vacuity: prove the scan is reading the content that *would* hold a
    // leak. The dump reports the RPC user verbatim right next to the password,
    // so finding it means a leaked password would have been found too.
    let dumped_user = format!("--node-rpc-user=\"{rpc_user}\"");
    anyhow::ensure!(
        files
            .iter()
            .any(|(_, contents)| contents.contains(&dumped_user)),
        "expected the startup config dump to report {dumped_user}; without it this test \
         proves nothing, since it would be scanning logs that never mention the credentials"
    );
    anyhow::ensure!(
        files
            .iter()
            .any(|(_, contents)| contents.contains("--node-rpc-pass=[redacted]")),
        "the startup config dump must report the password as redacted"
    );

    // Self-check the encoder, so a broken one cannot make the search below
    // silently vacuous.
    assert_eq!(base64_encode(b"hello"), "aGVsbG8=");
    let basic_auth = base64_encode(format!("{rpc_user}:{}", rpc_pass.expose()).as_bytes());

    assert_absent(&files, rpc_pass.expose(), "the node RPC password")?;
    assert_absent(&files, &basic_auth, "the node RPC basic-auth credentials")?;

    tracing::info!("no secrets found in enforcer output");
    Ok(())
}
