//! BIP 360 rule worker (v0).
//!
//! Registers as rule id `bip360`, keeps PQC deps in this process only, and can
//! answer rules.v1 requests via stdin (`--once`) or UDS (`--uds PATH`) with
//! RUL1 length-prefix framing. Workers never open ZMQ or call `invalidateblock`.
//!
//! **Fail-closed:** ValidateTx without `tx_hex` → Reject `missing_tx`. With hex,
//! runs SoftForkRule → `pqc::validate_mempool_transaction` using
//! `parent_txs_hex` when present (empty parents otherwise — P2MR spends fail closed).
//! ConnectBlock without `chain_p2mr_utxos_hex` → Reject `missing_chain_p2mr_utxos`.
//!
//! **Capability:** `validation=bip360-pqc-parents-and-chain-utxos-on-wire`.
//! Hub may still apply local UTXO diff after remote Accept.
//!
//! **Shutdown:** SIGTERM/SIGINT set an `AtomicBool` passed to `serve_uds_loop`.

use std::{
    collections::HashMap,
    io::{self, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use bip300301_enforcer_lib::validator::rules::{
    RuleBlockContext, RuleCallError, RuleTxContext, RuleVote, SoftForkRule,
    bip360::{self, Bip360Rule},
    ipc::{
        self, MAX_IPC_BODY_BYTES, RULES_V1_VERSION, RulesV1Request, RulesV1Response,
        VALIDATION_BIP360_PARENTS_AND_CHAIN_UTXOS, bind_uds, decode_block_hex,
        decode_chain_p2mr_utxos_hex, decode_parent_txs_hex, decode_request, decode_tx_hex,
        encode_response, read_capped, serve_uds_loop,
    },
};
use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "cusf_rules_bip360",
    about = "CUSF BIP 360 rule worker (health + rules.v1 stdin/UDS)"
)]
struct Args {
    /// BIP 360 activation height (forwarded at handshake time in a full hub).
    #[arg(long, default_value_t = 0)]
    activation_height: u32,

    /// Print registered rule id / version and exit.
    #[arg(long)]
    health: bool,

    /// Read one JSON rules.v1 request from stdin; write one response to stdout.
    #[arg(long, conflicts_with = "uds")]
    once: bool,

    /// Serve rules.v1 over a Unix domain socket with RUL1 length-prefix framing.
    #[arg(long, value_name = "PATH", conflicts_with = "once")]
    uds: Option<PathBuf>,
}

fn vote_to_response(result: Result<RuleVote, RuleCallError>) -> RulesV1Response {
    match result {
        Ok(RuleVote::Accept) => RulesV1Response::accept(),
        Ok(RuleVote::Reject { reason }) => RulesV1Response::reject(reason),
        Err(err) => RulesV1Response::Error { detail: err.detail },
    }
}

fn handle_request(rule: &Bip360Rule, req: RulesV1Request) -> RulesV1Response {
    match req {
        RulesV1Request::Handshake { version, rule_id } => {
            if version != RULES_V1_VERSION {
                return RulesV1Response::HandshakeErr {
                    detail: format!(
                        "unsupported version {version} (worker supports {RULES_V1_VERSION})"
                    ),
                };
            }
            if rule_id != bip360::RULE_ID {
                return RulesV1Response::HandshakeErr {
                    detail: format!(
                        "rule id mismatch: worker is {} got {rule_id}",
                        bip360::RULE_ID
                    ),
                };
            }
            RulesV1Response::HandshakeOk {
                version: RULES_V1_VERSION,
                rule_id: bip360::RULE_ID.to_string(),
                validation: Some(VALIDATION_BIP360_PARENTS_AND_CHAIN_UTXOS.to_string()),
            }
        }
        RulesV1Request::Health => RulesV1Response::HealthOk {
            rule_id: bip360::RULE_ID.to_string(),
            validation: Some(VALIDATION_BIP360_PARENTS_AND_CHAIN_UTXOS.to_string()),
        },
        RulesV1Request::ValidateTx {
            height,
            label,
            tx_hex,
            parent_txs_hex,
            drivechain_state: _,
        } => {
            let Some(hex) = tx_hex else {
                return RulesV1Response::reject("missing_tx");
            };
            let tx = match decode_tx_hex(&hex) {
                Ok(tx) => tx,
                Err(e) => return RulesV1Response::reject(format!("invalid_tx_hex: {e}")),
            };
            let parents = match parent_txs_hex {
                Some(map) => match decode_parent_txs_hex(&map) {
                    Ok(p) => p,
                    Err(e) => {
                        return RulesV1Response::reject(format!("invalid_parent_txs_hex: {e}"));
                    }
                },
                None => HashMap::new(),
            };
            let ctx = if parents.is_empty() {
                RuleTxContext::with_tx(height, &label, &tx)
            } else {
                RuleTxContext::with_tx_and_parents(height, &label, &tx, &parents)
            };
            vote_to_response(rule.validate_tx(&ctx))
        }
        RulesV1Request::ConnectBlock {
            height,
            label,
            block_hex,
            chain_p2mr_utxos_hex,
            drivechain_state: _,
        } => {
            let Some(hex) = block_hex else {
                return RulesV1Response::reject("missing_block");
            };
            let block = match decode_block_hex(&hex) {
                Ok(b) => b,
                Err(e) => return RulesV1Response::reject(format!("invalid_block_hex: {e}")),
            };
            // Fail-closed: SoftForkRule requires chain set (even empty map) on context.
            let Some(map) = chain_p2mr_utxos_hex else {
                return RulesV1Response::reject("missing_chain_p2mr_utxos");
            };
            let chain = match decode_chain_p2mr_utxos_hex(&map) {
                Ok(c) => c,
                Err(e) => {
                    return RulesV1Response::reject(format!("invalid_chain_p2mr_utxos_hex: {e}"));
                }
            };
            let ctx = RuleBlockContext::with_block_and_chain_utxos(height, &label, &block, &chain);
            vote_to_response(rule.connect_block(&ctx))
        }
    }
}

fn print_ready(activation_height: u32) {
    let rule = Bip360Rule::new(activation_height);
    tracing::info!(
        rule_id = rule.id(),
        rules_v1 = RULES_V1_VERSION,
        activation_height,
        validation = VALIDATION_BIP360_PARENTS_AND_CHAIN_UTXOS,
        frame_magic = %String::from_utf8_lossy(ipc::FRAME_MAGIC),
        max_body = MAX_IPC_BODY_BYTES,
        "cusf_rules_bip360 worker ready (parents + chain P2MR UTXOs on wire; hub may still apply local UTXO diff)"
    );
    #[expect(clippy::print_stdout)]
    {
        println!(
            "ok rule_id={} rules_v1={} activation_height={} mode=worker validation={}",
            rule.id(),
            RULES_V1_VERSION,
            activation_height,
            VALIDATION_BIP360_PARENTS_AND_CHAIN_UTXOS
        );
    }
}

/// SIGTERM/SIGINT → AtomicBool for `serve_uds_loop` (no process kill required).
fn install_shutdown_flag() -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    let f = Arc::clone(&flag);
    if let Err(e) = ctrlc::set_handler(move || {
        f.store(true, Ordering::Relaxed);
    }) {
        tracing::warn!(error = %e, "failed to install SIGTERM/SIGINT handler; process kill still works");
    }
    flag
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(io::stderr)
        .init();

    let args = Args::parse();
    let rule = Bip360Rule::new(args.activation_height);

    if args.health {
        print_ready(args.activation_height);
        return Ok(());
    }

    if let Some(path) = args.uds {
        print_ready(args.activation_height);
        let listener = bind_uds(&path)?;
        tracing::info!(path = %path.display(), "listening on UDS (RUL1 framing; nonblocking accept; SIGTERM/SIGINT shutdown)");
        let shutdown = install_shutdown_flag();
        // Always unlink socket after loop (Ok or Err / signal shutdown).
        let serve_result = serve_uds_loop(
            &listener,
            |req| handle_request(&rule, req),
            Some(shutdown.as_ref()),
        );
        drop(std::fs::remove_file(&path));
        serve_result?;
        return Ok(());
    }

    if args.once {
        let buf = read_capped(io::stdin(), MAX_IPC_BODY_BYTES)?;
        let req = decode_request(&buf).map_err(|e| format!("decode request: {e}"))?;
        let resp = handle_request(&rule, req);
        let out = encode_response(&resp).map_err(|e| format!("encode response: {e}"))?;
        io::stdout().write_all(&out)?;
        io::stdout().write_all(b"\n")?;
        return Ok(());
    }

    print_ready(args.activation_height);
    Ok(())
}
