//! Bitcoin Core version compatibility check.
//!
//! Bitcoin Core encodes its version as an integer of the form
//! `10000 * major + 100 * minor + revision`, returned by the
//! `getnetworkinfo` RPC. For example, version `300200` corresponds to
//! Bitcoin Core 30.2.0.
//!
//! The enforcer whitelists supported versions by **major** only, since
//! Bitcoin Core RPC/ABI is stable within a given major release series.

use jsonrpsee::core::client::ClientT;
use miette::Diagnostic;
use serde::Deserialize;
use thiserror::Error;

/// Bitcoin Core major versions the enforcer is known to work with.
/// Keep sorted newest-first. Update when dropping or adding support.
pub const SUPPORTED_BITCOIN_CORE_MAJORS: [u32; 3] = [30, 29, 28];

/// Specific Bitcoin Core releases that integration-test CI exercises, one
/// per entry of [`SUPPORTED_BITCOIN_CORE_MAJORS`]. Format is `MAJOR.MINOR`,
/// matching the layout under <https://bitcoincore.org/bin/>.
///
/// CI parses these literals directly out of this source file — keep the
/// shape (`[&str; N] = [... ]` with double-quoted `MAJOR.MINOR` values)
/// so the grep in `.github/workflows/check_lint_build_release.yaml` keeps
/// working. A unit test (`ci_versions_match_majors`) asserts the two
/// constants stay consistent.
pub const CI_BITCOIN_CORE_VERSIONS: [&str; 3] = ["30.2", "29.1", "28.2"];

/// Extracts the major version from a Bitcoin Core `version` integer as
/// returned by `getnetworkinfo`.
pub fn major_from_version_int(version: u64) -> u32 {
    (version / 10_000) as u32
}

/// Minimal subset of `getnetworkinfo` we need to identify the remote
/// Bitcoin Core's version. The upstream `bitcoin_jsonrpsee::client::NetworkInfo`
/// type doesn't expose these fields, so we decode them ourselves.
#[derive(Debug, Deserialize)]
struct NetworkInfoVersion {
    version: u64,
    subversion: String,
}

#[derive(Debug, Diagnostic, Error)]
#[error("unsupported Bitcoin Core version: `{subversion}` (version {version})")]
#[diagnostic(
    code(bip300301_enforcer::unsupported_bitcoin_core_version),
    url(
        "https://github.com/LayerTwo-Labs/bip300301_enforcer?tab=readme-ov-file#supported-bitcoin-core-versions"
    )
)]
pub struct UnsupportedBitcoinCoreVersion {
    pub version: u64,
    pub subversion: String,
    #[help]
    pub help: String,
}

#[derive(Debug, Diagnostic, Error)]
pub enum VersionCheckError {
    #[error("failed to call `getnetworkinfo` on Bitcoin Core")]
    #[diagnostic(code(bip300301_enforcer::getnetworkinfo_failed))]
    Rpc(#[source] jsonrpsee::core::client::Error),
    #[error(transparent)]
    #[diagnostic(transparent)]
    Unsupported(#[from] UnsupportedBitcoinCoreVersion),
}

/// Detected Bitcoin Core version info.
#[derive(Clone, Debug)]
pub struct DetectedVersion {
    pub version: u64,
    pub subversion: String,
    pub major: u32,
}

/// Query Bitcoin Core for its version, and verify it is supported.
///
/// Behaviour:
/// * If `skip_check` is true, returns the detected version without
///   validation.
/// * If `expected_major` is `Some(N)`, the detected major must equal `N`.
/// * Otherwise the detected major must be in [`SUPPORTED_BITCOIN_CORE_MAJORS`].
pub async fn check_bitcoin_core_version<C>(
    client: &C,
    expected_major: Option<u32>,
    skip_check: bool,
) -> Result<DetectedVersion, VersionCheckError>
where
    C: ClientT + Sync,
{
    let info: NetworkInfoVersion = client
        .request("getnetworkinfo", jsonrpsee::rpc_params![])
        .await
        .map_err(VersionCheckError::Rpc)?;

    let detected = DetectedVersion {
        major: major_from_version_int(info.version),
        version: info.version,
        subversion: info.subversion,
    };

    if skip_check {
        return Ok(detected);
    }

    let allowed_majors: Vec<u32> = match expected_major {
        Some(m) => vec![m],
        None => SUPPORTED_BITCOIN_CORE_MAJORS.to_vec(),
    };

    if allowed_majors.contains(&detected.major) {
        return Ok(detected);
    }

    let help = format_unsupported_help(&allowed_majors, expected_major.is_some());
    Err(VersionCheckError::Unsupported(
        UnsupportedBitcoinCoreVersion {
            version: detected.version,
            subversion: detected.subversion,
            help,
        },
    ))
}

fn format_unsupported_help(allowed_majors: &[u32], explicit_expected: bool) -> String {
    let majors_list = allowed_majors
        .iter()
        .map(|m| m.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    if explicit_expected {
        format!(
            "you asserted Bitcoin Core major version {majors_list} via \
             `--bitcoin-core-expected-version`, but the connected node reports \
             a different version. Either update the flag to match, or remove it \
             to use the built-in supported set. To bypass the check entirely, \
             pass `--bitcoin-core-skip-version-check`."
        )
    } else {
        format!(
            "the enforcer supports Bitcoin Core major versions: {majors_list}. \
             To run against a different version anyway, pass \
             `--bitcoin-core-expected-version <MAJOR>` to assert a specific major, \
             or `--bitcoin-core-skip-version-check` to skip the check entirely."
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn major_from_version_int_extracts_major() {
        assert_eq!(major_from_version_int(300200), 30);
        assert_eq!(major_from_version_int(290100), 29);
        assert_eq!(major_from_version_int(280200), 28);
        assert_eq!(major_from_version_int(270000), 27);
    }

    #[test]
    fn help_mentions_override_flags_and_majors() {
        let help = format_unsupported_help(&[30, 29, 28], false);
        assert!(help.contains("30, 29, 28"));
        assert!(help.contains("--bitcoin-core-expected-version"));
        assert!(help.contains("--bitcoin-core-skip-version-check"));
    }

    #[test]
    fn help_for_explicit_expected_mentions_user_choice() {
        let help = format_unsupported_help(&[27], true);
        assert!(help.contains("27"));
        assert!(help.contains("--bitcoin-core-expected-version"));
    }

    #[test]
    fn ci_versions_match_majors() {
        assert_eq!(
            CI_BITCOIN_CORE_VERSIONS.len(),
            SUPPORTED_BITCOIN_CORE_MAJORS.len(),
            "CI version list must line up with supported majors",
        );
        for (version, major) in CI_BITCOIN_CORE_VERSIONS
            .iter()
            .zip(SUPPORTED_BITCOIN_CORE_MAJORS.iter())
        {
            let (version_major, rest) = version
                .split_once('.')
                .unwrap_or_else(|| panic!("expected MAJOR.MINOR, got `{version}`"));
            let parsed: u32 = version_major
                .parse()
                .unwrap_or_else(|_| panic!("bad major in `{version}`"));
            assert_eq!(parsed, *major, "major mismatch in CI version `{version}`");
            // The minor component must parse as an integer — guards against
            // typos like `"30.x"` that would otherwise silently end up in CI.
            let _: u32 = rest
                .parse()
                .unwrap_or_else(|_| panic!("bad minor in `{version}`"));
        }
    }
}
