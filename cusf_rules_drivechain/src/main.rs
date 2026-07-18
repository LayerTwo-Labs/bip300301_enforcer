//! Drivechain rule worker (v0).
//!
//! Registers as rule id `drivechain`, prints health/handshake info, and can
//! answer rules.v1 requests via stdin (`--once`) or UDS (`--uds PATH`) with
//! RUL1 length-prefix framing. Workers never open ZMQ or call `invalidateblock`
//! — only the hub does.
//!
//! **Capability:** `validation=drivechain-ctip-m8-pending-m6-checks-on-wire` —
//! SoftForkRule with `drivechain_state` present runs independent M8 tip match +
//! multi-active-OP_DRIVECHAIN + current-ctip structural M5/M6 + pending M6id /
//! vote-threshold checks; connect also matches M8 to coinbase M7 and folds
//! ctips/pending sequentially. Missing state → `missing_drivechain_state`.
//! Residual Path B (Local dual-AND only): historical `ctip_outpoint_to_value_seq`
//! (unbounded — not on wire), M1–M4 proposal/ack DB (partial extract unsafe).
//!
//! **Shutdown:** SIGTERM/SIGINT set an `AtomicBool` passed to `serve_uds_loop`.

use std::{
    io::{self, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use bip300301_enforcer_lib::validator::rules::{
    DrivechainStateSnapshot, RuleCallError, RuleTxContext, RuleVote, SoftForkRule,
    drivechain::{self, DrivechainRule},
    ipc::{
        self, MAX_IPC_BODY_BYTES, RULES_V1_VERSION, RulesV1Request, RulesV1Response,
        VALIDATION_DRIVECHAIN_CTIP_M8_PENDING_M6_CHECKS_ON_WIRE, bind_uds, decode_block_hex,
        decode_request, decode_tx_hex, encode_response, read_capped, serve_uds_loop,
    },
};
use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "cusf_rules_drivechain",
    about = "CUSF BIP 300/301 rule worker (health + rules.v1 stdin/UDS)"
)]
struct Args {
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

fn handle_request(req: RulesV1Request) -> RulesV1Response {
    let rule = DrivechainRule::new();
    match req {
        RulesV1Request::Handshake { version, rule_id } => {
            if version != RULES_V1_VERSION {
                return RulesV1Response::HandshakeErr {
                    detail: format!(
                        "unsupported version {version} (worker supports {RULES_V1_VERSION})"
                    ),
                };
            }
            if rule_id != drivechain::RULE_ID {
                return RulesV1Response::HandshakeErr {
                    detail: format!(
                        "rule id mismatch: worker is {} got {rule_id}",
                        drivechain::RULE_ID
                    ),
                };
            }
            RulesV1Response::HandshakeOk {
                version: RULES_V1_VERSION,
                rule_id: drivechain::RULE_ID.to_string(),
                validation: Some(
                    VALIDATION_DRIVECHAIN_CTIP_M8_PENDING_M6_CHECKS_ON_WIRE.to_string(),
                ),
            }
        }
        RulesV1Request::Health => RulesV1Response::HealthOk {
            rule_id: drivechain::RULE_ID.to_string(),
            validation: Some(VALIDATION_DRIVECHAIN_CTIP_M8_PENDING_M6_CHECKS_ON_WIRE.to_string()),
        },
        RulesV1Request::ValidateTx {
            height,
            label,
            tx_hex,
            parent_txs_hex: _,
            drivechain_state,
        } => {
            let Some(hex) = tx_hex else {
                return RulesV1Response::reject("missing_tx");
            };
            let tx = match decode_tx_hex(&hex) {
                Ok(tx) => tx,
                Err(e) => return RulesV1Response::reject(format!("invalid_tx_hex: {e}")),
            };
            // Own snapshot for lifetime of SoftForkRule call.
            let state_owned: Option<DrivechainStateSnapshot> = drivechain_state;
            let mut ctx = RuleTxContext::with_tx(height, &label, &tx);
            if let Some(ref state) = state_owned {
                ctx = ctx.with_drivechain_state(state);
            }
            vote_to_response(rule.validate_tx(&ctx))
        }
        RulesV1Request::ConnectBlock {
            height,
            label,
            block_hex,
            chain_p2mr_utxos_hex: _,
            drivechain_state,
        } => {
            let Some(hex) = block_hex else {
                return RulesV1Response::reject("missing_block");
            };
            let block = match decode_block_hex(&hex) {
                Ok(b) => b,
                Err(e) => return RulesV1Response::reject(format!("invalid_block_hex: {e}")),
            };
            let state_owned: Option<DrivechainStateSnapshot> = drivechain_state;
            let mut ctx = bip300301_enforcer_lib::validator::rules::RuleBlockContext::with_block(
                height, &label, &block,
            );
            if let Some(ref state) = state_owned {
                ctx = ctx.with_drivechain_state(state);
            }
            vote_to_response(rule.connect_block(&ctx))
        }
    }
}

fn print_ready() {
    let rule = DrivechainRule::new();
    tracing::info!(
        rule_id = rule.id(),
        rules_v1 = RULES_V1_VERSION,
        validation = VALIDATION_DRIVECHAIN_CTIP_M8_PENDING_M6_CHECKS_ON_WIRE,
        frame_magic = %String::from_utf8_lossy(ipc::FRAME_MAGIC),
        max_body = MAX_IPC_BODY_BYTES,
        "cusf_rules_drivechain worker ready (drivechain-ctip-m8-pending-m6-checks-on-wire; M8 tip + multi-active-OP_DRIVECHAIN + current-ctip M5/M6 + pending M6id/votes + connect M7/M8; Local dual-AND for historical/M1–M4 residual)"
    );
    #[expect(clippy::print_stdout)]
    {
        println!(
            "ok rule_id={} rules_v1={} mode=worker validation={}",
            rule.id(),
            RULES_V1_VERSION,
            VALIDATION_DRIVECHAIN_CTIP_M8_PENDING_M6_CHECKS_ON_WIRE
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

    if args.health {
        print_ready();
        return Ok(());
    }

    if let Some(path) = args.uds {
        print_ready();
        let listener = bind_uds(&path)?;
        tracing::info!(path = %path.display(), "listening on UDS (RUL1 framing; nonblocking accept; SIGTERM/SIGINT shutdown)");
        let shutdown = install_shutdown_flag();
        // Always unlink socket after loop (Ok or Err / signal shutdown).
        let serve_result = serve_uds_loop(&listener, handle_request, Some(shutdown.as_ref()));
        drop(std::fs::remove_file(&path));
        serve_result?;
        return Ok(());
    }

    if args.once {
        let buf = read_capped(io::stdin(), MAX_IPC_BODY_BYTES)?;
        let req = decode_request(&buf).map_err(|e| format!("decode request: {e}"))?;
        let resp = handle_request(req);
        let out = encode_response(&resp).map_err(|e| format!("encode response: {e}"))?;
        io::stdout().write_all(&out)?;
        io::stdout().write_all(b"\n")?;
        return Ok(());
    }

    // Default: health-style ready line (no long-running server without --uds).
    print_ready();
    Ok(())
}
