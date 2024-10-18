use std::fmt;

use miette::{diagnostic, Diagnostic};
use regex::Regex;
use serde::Deserialize;

pub fn convert_bdk_error(err: bdk::Error) -> miette::Report {
    // Helper function to make it easier to log the error in case we're unable
    // to convert to something meaningful.
    fn inner(err: &bdk::Error) -> Option<miette::Report> {
        // Add more here, as they appear
        match err {
            bdk::Error::Electrum(e) => convert_electrum_error(e),
            _ => None,
        }
    }

    inner(&err).unwrap_or_else(|| {
        let diag = diagnostic!("ran into unknown BDK error");
        let report: miette::Report = diag.into();
        report.wrap_err(err)
    })
}

fn convert_electrum_error(err: &bdk::electrum_client::Error) -> Option<miette::Report> {
    match err {
        bdk::electrum_client::Error::Protocol(err) => {
            extract_rpc_error(err).map(|rpc_error| rpc_error.into())
        }
        _ => None,
    }
}

use serde_json::Value;
use thiserror::Error;
use tonic::{Code, Status};

#[derive(Error, Debug, Diagnostic, Deserialize, Clone)]
#[diagnostic(
    code(electrum_error),
    help("The error is from the Electrum server. Check the message for more details.")
)]
pub struct ElectrumError {
    code: i32,
    message: String,
}

impl From<ElectrumError> for Status {
    fn from(error: ElectrumError) -> Self {
        let code = match error.code {
            // https://github.com/bitcoin/bitcoin/blob/e8f72aefd20049eac81b150e7f0d33709acd18ed/src/common/messages.cpp
            -25 => Code::InvalidArgument,
            _ => Code::Unknown,
        };
        Status::new(code, error.to_string())
    }
}

impl fmt::Display for ElectrumError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "electrum error {}: {}", self.code, self.message)
    }
}

fn extract_rpc_error(input: &Value) -> Option<ElectrumError> {
    // Error output from Electrum server has a prefix, followed by an escaped JSON object.
    // We need to:
    // 1. Find the actual JSON part of the message
    // 2. Unescape it.
    let re = Regex::new(r#"\{.*\}"#).unwrap();

    // Extract the JSON part
    if let Some(caps) = re.captures(&input.to_string()) {
        if let Some(json_str) = caps.get(0) {
            let json_part = json_str.as_str();

            let rpc_error: ElectrumError = serde_json::from_str(&unescape_json(json_part))
                .inspect_err(|e| {
                    tracing::error!("Failed to parse RPC error: {e:#}");
                })
                .ok()?;
            return Some(rpc_error);
        }
    }

    None
}

// Replace escaped sequences like \"
fn unescape_json(input: &str) -> String {
    input.replace(r#"\""#, r#"""#)
}
