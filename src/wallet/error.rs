use std::sync::LazyLock;

use miette::{diagnostic, Diagnostic};
use regex::Regex;
use serde::Deserialize;
use serde_json::Value as JsonValue;
use thiserror::Error;

// Replace escaped sequences like \"
fn unescape_json(input: &str) -> String {
    input.replace(r#"\""#, r#"""#)
}

/// Greedy match anything between two braces `{` and `}`, inclusive of the
/// braces.
static BRACES_CONTENT_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"\{.*\}"#).unwrap());

#[derive(Debug, Diagnostic, Error)]
#[error("Failed to deserialize `{json_str}`")]
struct JsonDeserializeError {
    json_str: String,
    source: serde_path_to_error::Error<serde_json::Error>,
}

#[derive(Clone, Debug, Deserialize, Diagnostic, Error)]
#[diagnostic(
    code(electrum_error),
    help("The error is from the Electrum server. Check the message for more details.")
)]
#[error("electrum error `{code}`: `{message}`")]
pub struct ElectrumError {
    code: i32,
    message: String,
}

impl From<ElectrumError> for tonic::Status {
    fn from(error: ElectrumError) -> Self {
        let code = match error.code {
            // https://github.com/bitcoin/bitcoin/blob/e8f72aefd20049eac81b150e7f0d33709acd18ed/src/common/messages.cpp
            -25 => tonic::Code::InvalidArgument,
            _ => tonic::Code::Unknown,
        };
        Self::new(code, error.to_string())
    }
}

/// This function extracts the error code and message from an electrum protocol
/// error.
/// Electrum protocol errors are JSON strings that look like
/// `electrum error: {\"code\": -10, \"message\": \"the error message\"}`.
fn extract_electrum_proto_err(
    err: &JsonValue,
) -> Result<Option<ElectrumError>, JsonDeserializeError> {
    let Some(err) = err.as_str() else {
        return Ok(None);
    };
    // Extract the JSON part
    let Some(inner_json_str_escaped) = BRACES_CONTENT_REGEX
        .captures(err)
        .and_then(|cap| cap.get(0))
    else {
        return Ok(None);
    };
    let inner_json_str = unescape_json(inner_json_str_escaped.as_str());
    let deserializer = &mut serde_json::Deserializer::from_str(&inner_json_str);
    let rpc_error: ElectrumError =
        serde_path_to_error::deserialize(deserializer).map_err(|err| JsonDeserializeError {
            json_str: inner_json_str,
            source: err,
        })?;
    Ok(Some(rpc_error))
}

fn convert_electrum_error(
    err: &bdk::electrum_client::Error,
) -> Result<Option<miette::Report>, JsonDeserializeError> {
    match err {
        bdk::electrum_client::Error::Protocol(err) => {
            extract_electrum_proto_err(err).map(|err| err.map(miette::Report::new))
        }
        _ => Ok(None),
    }
}

pub fn convert_bdk_error(err: bdk::Error) -> miette::Report {
    // Helper function to make it easier to log the error in case we're unable
    // to convert to something meaningful.
    fn inner(err: &bdk::Error) -> Result<Option<miette::Report>, JsonDeserializeError> {
        // Add more here, as they appear
        match err {
            bdk::Error::Electrum(e) => convert_electrum_error(e),
            _ => Ok(None),
        }
    }

    match inner(&err) {
        Ok(Some(report)) => report,
        Ok(None) => {
            let diag = diagnostic!("ran into unknown BDK error");
            let report: miette::Report = diag.into();
            report.wrap_err(err)
        }
        Err(json_deserialize_err) => miette::Report::new(json_deserialize_err),
    }
}
