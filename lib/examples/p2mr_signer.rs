//! Minimal external signer for P2MR script-path spends (CUSF overload model).
//!
//! Builds a single-leaf `PUSH <pk> OP_CHECKSIG` spend, signs the tapscript sighash,
//! and prints JSON with witness stack components for regtest demos.
//!
//! **Security:** demo-only tooling. `--entropy-hex` is visible in `ps`, shell history,
//! and `/proc`. Never use mainnet secrets; prefer isolated regtest keys only.
//!
//! See `docs/P2MR_SIGNER.md` for usage.

#![allow(clippy::print_stdout)]

use std::process::ExitCode;

use bip300301_enforcer_lib::validator::quantum::signer_dev::{
    SignAlgorithm, sighash_label, sign_p2mr_script_path_spend, tx_to_hex, validate_entropy,
    validate_output_value,
};
use bitcoin::sighash::TapSighashType;
use clap::{Parser, ValueEnum};
use serde::Serialize;

/// P2MR script-path spend signer (overload model: `PUSH <pk> OP_CHECKSIG`).
#[derive(Parser, Debug)]
#[command(
    name = "p2mr_signer",
    about = "Sign P2MR script-path spends for regtest demos (demo-only; never use mainnet secrets)"
)]
struct Cli {
    /// Signature algorithm: schnorr (secp256k1), mldsa (ML-DSA-44), or slh (SLH-DSA-SHA2-128s).
    #[arg(long, value_enum)]
    algorithm: SignAlgorithmArg,

    /// Hex-encoded entropy for deterministic key generation.
    /// Schnorr: exactly 32 bytes (64 hex chars). ML-DSA / SLH-DSA: at least 128 bytes (256 hex chars).
    /// Visible in process listings — demo/regtest only; never use mainnet secrets.
    #[arg(long)]
    entropy_hex: String,

    /// Tapscript sighash type for the signature.
    #[arg(long, value_enum, default_value_t = SighashArg::All)]
    sighash: SighashArg,

    /// Funding prevout value in satoshis (used for sighash and demo tx).
    #[arg(long, default_value_t = 50_000)]
    prevout_value: u64,

    /// Spend output value in satoshis (must be <= `--prevout-value`).
    #[arg(long, default_value_t = 40_000)]
    output_value: u64,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SignAlgorithmArg {
    Schnorr,
    Mldsa,
    Slh,
}

impl From<SignAlgorithmArg> for SignAlgorithm {
    fn from(arg: SignAlgorithmArg) -> Self {
        match arg {
            SignAlgorithmArg::Schnorr => SignAlgorithm::Schnorr,
            SignAlgorithmArg::Mldsa => SignAlgorithm::Mldsa,
            SignAlgorithmArg::Slh => SignAlgorithm::Slh,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
enum SighashArg {
    #[default]
    All,
    None,
    Single,
    #[value(name = "all,anyonecanpay")]
    AllAnyonecanpay,
    #[value(name = "none,anyonecanpay")]
    NoneAnyonecanpay,
    #[value(name = "single,anyonecanpay")]
    SingleAnyonecanpay,
}

impl SighashArg {
    const fn to_tap_sighash_type(self) -> TapSighashType {
        match self {
            Self::All => TapSighashType::All,
            Self::None => TapSighashType::None,
            Self::Single => TapSighashType::Single,
            Self::AllAnyonecanpay => TapSighashType::AllPlusAnyoneCanPay,
            Self::NoneAnyonecanpay => TapSighashType::NonePlusAnyoneCanPay,
            Self::SingleAnyonecanpay => TapSighashType::SinglePlusAnyoneCanPay,
        }
    }
}

#[derive(Serialize)]
struct SignerOutput {
    algorithm: &'static str,
    sighash: &'static str,
    public_key_hex: String,
    leaf_script_hex: String,
    merkle_root_hex: String,
    script_pubkey_hex: String,
    control_block_hex: String,
    sighash_hex: String,
    signature_hex: String,
    witness_stack_hex: Vec<String>,
    funding_tx_hex: String,
    unsigned_spend_tx_hex: String,
    signed_spend_tx_hex: String,
}

fn main() -> ExitCode {
    if let Err(err) = run() {
        eprintln!("{err}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn run() -> Result<(), String> {
    let cli = Cli::parse();
    let entropy =
        hex::decode(cli.entropy_hex.trim()).map_err(|e| format!("invalid --entropy-hex: {e}"))?;
    let algorithm = SignAlgorithm::from(cli.algorithm);
    validate_entropy(&entropy, algorithm)?;
    validate_output_value(cli.output_value, cli.prevout_value)?;

    let sighash_type = cli.sighash.to_tap_sighash_type();
    let signed = sign_p2mr_script_path_spend(
        algorithm,
        &entropy,
        sighash_type,
        cli.prevout_value,
        cli.output_value,
    )?;

    let output = SignerOutput {
        algorithm: algorithm.label(),
        sighash: sighash_label(signed.sighash_type),
        public_key_hex: hex::encode(&signed.public_key),
        leaf_script_hex: hex::encode(signed.leaf_script.as_bytes()),
        merkle_root_hex: hex::encode(signed.merkle_root),
        script_pubkey_hex: hex::encode(signed.script_pubkey.as_bytes()),
        control_block_hex: hex::encode(&signed.control_block),
        sighash_hex: hex::encode(signed.sighash),
        signature_hex: hex::encode(&signed.signature),
        witness_stack_hex: signed.witness.iter().map(hex::encode).collect(),
        funding_tx_hex: tx_to_hex(&signed.funding_tx),
        unsigned_spend_tx_hex: tx_to_hex(&signed.unsigned_spend_tx),
        signed_spend_tx_hex: tx_to_hex(&signed.signed_spend_tx),
    };

    let json = serde_json::to_string_pretty(&output).map_err(|e| format!("json encode: {e}"))?;
    println!("{json}");
    Ok(())
}
