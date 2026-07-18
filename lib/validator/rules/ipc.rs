//! rules.v1 wire schema and remote client (IPC v0).
//!
//! **Wire body:** JSON (serde) request/response.
//!
//! **UDS framing:** length-prefixed envelope used by [`UdsRuleTransport`] and
//! worker `--uds` servers:
//! ```text
//! [4-byte magic FRAME_MAGIC = b"RUL1"][4-byte BE body length][JSON body]
//! ```
//! Body length is capped at [`MAX_IPC_BODY_BYTES`]. Stdin `--once` workers still
//! use raw JSON only (no frame).
//!
//! **Fail-closed:** remote timeout / transport error → [`VoteSource::Timeout`] /
//! [`VoteSource::Failure`] (counts as no; hub logs at error level when aggregating).
//!
//! **Parent prevouts:** `ValidateTx.parent_txs_hex` carries optional parent txs
//! (txid hex → consensus-serialized tx hex) so remote bip360 can validate spends.
//!
//! **Chain P2MR UTXOs:** `ConnectBlock.chain_p2mr_utxos_hex` carries confirmed
//! P2MR outputs (`txid:vout` → consensus TxOut hex) for remote bip360 connect.
//!
//! **Drivechain state:** `ValidateTx` / `ConnectBlock` optional `drivechain_state`
//! (tip height/hash, active sidechain numbers, current `ctips`, pending M6ids,
//! and inclusion threshold). SoftForkRule with state present runs independent
//! M8 tip, multi-active-OP_DRIVECHAIN, current-ctip structural M5/M6, and
//! pending M6id / vote-threshold checks; connect path also matches M8 to
//! coinbase M7 (accepted_bmm block-local) and folds ctips/pending sequentially.
//! Remaining residual (historical `ctip_outpoint_to_value_seq`, M1–M4) Accept
//! under Local dual-AND. Missing state → `missing_drivechain_state`.
//!
//! **Capability tokens:** `HandshakeOk` / `HealthOk` carry optional `validation`
//! (e.g. `stub-not_implemented`, `bip360-pqc-parents-and-chain-utxos-on-wire`,
//! `drivechain-ctip-m8-pending-m6-checks-on-wire`). Hub **registers** allowlisted
//! real remotes for dual-AND with Local ballots (remote never sole-replaces Local
//! mempool/connect consent); stub remotes are not registered for consent.

use std::{
    collections::BTreeMap,
    io::{Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, RecvTimeoutError},
    },
    thread,
    time::Duration,
};

use bitcoin::{
    OutPoint, TxOut, Txid,
    consensus::{Decodable, Encodable},
};
use serde::{Deserialize, Serialize};

use super::{
    DrivechainStateSnapshot, RuleBallot, RuleBlockContext, RuleId, RuleTxContext, RuleVote,
};

/// Handshake / schema version constant (P5).
pub const RULES_V1_VERSION: u32 = 1;

/// Legacy stub capability (incomplete worker). Not allowlisted for consent.
pub const VALIDATION_STUB_NOT_IMPLEMENTED: &str = "stub-not_implemented";

/// Bip360 worker: mempool parents + connect chain P2MR UTXOs on the wire.
pub const VALIDATION_BIP360_PARENTS_AND_CHAIN_UTXOS: &str =
    "bip360-pqc-parents-and-chain-utxos-on-wire";

/// Drivechain worker: tip + active sidechains + current ctips + pending M6ids on wire.
///
/// SoftForkRule (capability `drivechain-ctip-m8-pending-m6-checks-on-wire`):
/// * missing state → `missing_drivechain_state`
/// * M8 `prev_mainchain_block_hash` ≠ `tip_hash_hex` → Reject independently
/// * multiple OP_DRIVECHAIN on same *active* sidechain → Reject independently
/// * current-ctip structural M5/M6 (spend/replace, zero-diff, missing address, M6 shape)
/// * pending M6id match + `vote_count > withdrawal_bundle_inclusion_threshold`
/// * Connect: coinbase M7 → accepted_bmm match for M8 (block-local); sequential
///   ctip + pending fold across block txs
/// * plain / Path B residual DC markers → Accept (Local dual-AND for historical
///   `ctip_outpoint_to_value_seq` / M1–M4; no dishonest partial extract)
///
/// Residual Path B (accepted): historical map unbounded; M1–M4 proposal DB not
/// on wire. Capability name stays until a real wire surface expands.
pub const VALIDATION_DRIVECHAIN_CTIP_M8_PENDING_M6_CHECKS_ON_WIRE: &str =
    "drivechain-ctip-m8-pending-m6-checks-on-wire";

/// Prior ctip+M8 capability (no pending M6ids on wire). **Not** allowlisted.
pub const VALIDATION_DRIVECHAIN_CTIP_M8_CHECKS_LEGACY: &str = "drivechain-ctip-m8-checks-on-wire";

/// Prior M8-tip-only capability (no current-ctip / connect M7 match). **Not** allowlisted.
pub const VALIDATION_DRIVECHAIN_M8_TIP_CHECK_LEGACY: &str = "drivechain-m8-tip-check-on-wire";

/// Pre-M8-check capability token (presence-only SoftForkRule). **Not** allowlisted.
pub const VALIDATION_DRIVECHAIN_TIP_AND_SIDECHAINS_LEGACY: &str =
    "drivechain-tip-and-sidechains-on-wire";

/// Exact allowlist of validation tokens that may register a remote for consent.
///
/// Unknown / spoofed / stub tokens are **not** real — hub keeps Local.
pub const REAL_VALIDATION_CAPABILITIES: &[&str] = &[
    VALIDATION_BIP360_PARENTS_AND_CHAIN_UTXOS,
    VALIDATION_DRIVECHAIN_CTIP_M8_PENDING_M6_CHECKS_ON_WIRE,
];

/// True when `validation` is an **allowlisted** real capability that may register
/// for remote consent (AND with Local mempool ballots).
///
/// * Exact match in [`REAL_VALIDATION_CAPABILITIES`] only
/// * `None` / empty / stub / any other string → **not** real (no spoof register)
pub fn is_real_validation_capability(validation: Option<&str>) -> bool {
    let Some(v) = validation.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    REAL_VALIDATION_CAPABILITIES.contains(&v)
}

/// Whether a handshake outcome should cause remote consent registration.
///
/// * `Ok(validation)` + allowlisted → register
/// * `Ok(stub/unknown)` or `Err(_)` (unreachable) → **do not register** (keep Local)
pub fn should_register_remote_for_consent(handshake: &Result<Option<String>, String>) -> bool {
    match handshake {
        Ok(validation) => is_real_validation_capability(validation.as_deref()),
        Err(_) => false,
    }
}

/// Default remote call timeout (fail-closed if exceeded).
pub const DEFAULT_REMOTE_TIMEOUT: Duration = Duration::from_secs(5);

/// Minimum remote timeout (zero / sub-ms values are rejected and replaced).
pub const MIN_REMOTE_TIMEOUT: Duration = Duration::from_millis(1);

/// Normalize a configured remote timeout: `0` or empty → [`DEFAULT_REMOTE_TIMEOUT`];
/// otherwise at least [`MIN_REMOTE_TIMEOUT`].
pub fn normalize_remote_timeout(timeout: Duration) -> Duration {
    if timeout.is_zero() {
        DEFAULT_REMOTE_TIMEOUT
    } else if timeout < MIN_REMOTE_TIMEOUT {
        MIN_REMOTE_TIMEOUT
    } else {
        timeout
    }
}

/// Max rules.v1 request body size (stdin / future UDS frame payload).
pub const MAX_IPC_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Framing magic for length-prefixed UDS envelopes (`RUL1`).
pub const FRAME_MAGIC: &[u8; 4] = b"RUL1";

/// Header size: magic (4) + BE length (4).
pub const FRAME_HEADER_LEN: usize = 8;

/// Structured transport outcome (no string-matching for timeout).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    /// Deadline exceeded (counts as [`super::VoteSource::Timeout`]).
    Timeout,
    /// Transport or worker error (counts as [`super::VoteSource::Failure`]).
    Failure { detail: String },
}

impl TransportError {
    pub fn failure(detail: impl Into<String>) -> Self {
        Self::Failure {
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => write!(f, "timeout"),
            Self::Failure { detail } => write!(f, "{detail}"),
        }
    }
}

impl std::error::Error for TransportError {}

/// rules.v1 request (hub → worker).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum RulesV1Request {
    Handshake {
        /// Hub protocol version.
        version: u32,
        /// Expected rule id (worker should match).
        rule_id: String,
    },
    ValidateTx {
        height: u32,
        label: String,
        /// Hex-encoded consensus-serialized transaction (required for real vote).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tx_hex: Option<String>,
        /// Parent transactions for BIP 360 mempool prevout lookup.
        /// Map: txid hex (big-endian bitcoin display) → consensus-serialized tx hex.
        /// Absent/empty → empty parent map (P2MR spends fail-closed).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_txs_hex: Option<BTreeMap<String, String>>,
        /// Drivechain tip + active sidechains (drivechain SoftForkRule).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        drivechain_state: Option<DrivechainStateSnapshot>,
    },
    ConnectBlock {
        height: u32,
        label: String,
        /// Hex-encoded consensus-serialized block when available.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        block_hex: Option<String>,
        /// Confirmed-chain P2MR UTXOs for bip360 SoftForkRule connect.
        /// Map: `"txid:vout"` → consensus-serialized TxOut hex.
        /// Absent when hub has no set; bip360 worker fail-closes if needed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        chain_p2mr_utxos_hex: Option<BTreeMap<String, String>>,
        /// Drivechain tip + active sidechains (drivechain SoftForkRule).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        drivechain_state: Option<DrivechainStateSnapshot>,
    },
    Health,
}

/// rules.v1 response (worker → hub).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "body")]
pub enum RulesV1Response {
    HandshakeOk {
        version: u32,
        rule_id: String,
        /// Capability token for hub consent policy (see `is_real_validation_capability`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        validation: Option<String>,
    },
    HandshakeErr {
        detail: String,
    },
    Vote {
        accept: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    HealthOk {
        rule_id: String,
        /// Same capability token as handshake (operators / smoke scripts).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        validation: Option<String>,
    },
    Error {
        detail: String,
    },
}

impl RulesV1Response {
    pub fn accept() -> Self {
        Self::Vote {
            accept: true,
            reason: None,
        }
    }

    pub fn reject(reason: impl Into<String>) -> Self {
        Self::Vote {
            accept: false,
            reason: Some(reason.into()),
        }
    }

    pub fn into_vote(self) -> Result<RuleVote, String> {
        match self {
            Self::Vote { accept: true, .. } => Ok(RuleVote::Accept),
            Self::Vote {
                accept: false,
                reason,
            } => Ok(RuleVote::reject(
                reason.unwrap_or_else(|| "remote reject".into()),
            )),
            Self::Error { detail } => Err(detail),
            other => Err(format!("unexpected rules.v1 response: {other:?}")),
        }
    }
}

/// Encode a request as JSON bytes (v0 wire body, without length prefix).
pub fn encode_request(req: &RulesV1Request) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(req)
}

/// Decode a response from JSON bytes.
pub fn decode_response(bytes: &[u8]) -> Result<RulesV1Response, serde_json::Error> {
    serde_json::from_slice(bytes)
}

/// Encode response as JSON bytes.
pub fn encode_response(resp: &RulesV1Response) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(resp)
}

/// Decode request from JSON bytes.
pub fn decode_request(bytes: &[u8]) -> Result<RulesV1Request, serde_json::Error> {
    serde_json::from_slice(bytes)
}

/// Consensus-encode a transaction as hex for rules.v1 `tx_hex`.
pub fn encode_tx_hex(tx: &bitcoin::Transaction) -> String {
    let mut buf = Vec::new();
    tx.consensus_encode(&mut buf)
        .expect("transaction consensus encode cannot fail for Vec");
    hex::encode(buf)
}

/// Decode `tx_hex` from rules.v1.
pub fn decode_tx_hex(tx_hex: &str) -> Result<bitcoin::Transaction, String> {
    let bytes = hex::decode(tx_hex).map_err(|e| format!("tx_hex decode: {e}"))?;
    bitcoin::Transaction::consensus_decode(&mut bytes.as_slice())
        .map_err(|e| format!("tx consensus decode: {e}"))
}

/// Consensus-encode a block as hex for rules.v1 `block_hex`.
pub fn encode_block_hex(block: &bitcoin::Block) -> String {
    let mut buf = Vec::new();
    block
        .consensus_encode(&mut buf)
        .expect("block consensus encode cannot fail for Vec");
    hex::encode(buf)
}

/// Decode `block_hex` from rules.v1.
pub fn decode_block_hex(block_hex: &str) -> Result<bitcoin::Block, String> {
    let bytes = hex::decode(block_hex).map_err(|e| format!("block_hex decode: {e}"))?;
    bitcoin::Block::consensus_decode(&mut bytes.as_slice())
        .map_err(|e| format!("block consensus decode: {e}"))
}

/// Encode parent-tx map for rules.v1 `parent_txs_hex`.
pub fn encode_parent_txs_hex(
    parents: &std::collections::HashMap<Txid, bitcoin::Transaction>,
) -> BTreeMap<String, String> {
    parents
        .iter()
        .map(|(txid, tx)| (txid.to_string(), encode_tx_hex(tx)))
        .collect()
}

/// Decode `parent_txs_hex` into a txid → transaction map.
///
/// Fail-closed: invalid entries return `Err` (caller maps to Reject / Failure).
/// Each map key **must** equal `tx.compute_txid()` (forged prevout keys rejected).
pub fn decode_parent_txs_hex(
    parents: &BTreeMap<String, String>,
) -> Result<std::collections::HashMap<Txid, bitcoin::Transaction>, String> {
    let mut out = std::collections::HashMap::with_capacity(parents.len());
    for (txid_hex, tx_hex) in parents {
        let key_txid = Txid::from_str_hex(txid_hex)
            .map_err(|e| format!("parent txid hex `{txid_hex}`: {e}"))?;
        let tx = decode_tx_hex(tx_hex).map_err(|e| format!("parent tx `{txid_hex}`: {e}"))?;
        let body_txid = tx.compute_txid();
        if body_txid != key_txid {
            return Err(format!(
                "parent txid key mismatch: key={txid_hex} body_txid={body_txid}"
            ));
        }
        out.insert(key_txid, tx);
    }
    Ok(out)
}

/// Consensus-encode a TxOut as hex for rules.v1 `chain_p2mr_utxos_hex` values.
pub fn encode_txout_hex(txout: &TxOut) -> String {
    let mut buf = Vec::new();
    txout
        .consensus_encode(&mut buf)
        .expect("TxOut consensus encode cannot fail for Vec");
    hex::encode(buf)
}

/// Decode a TxOut from hex.
pub fn decode_txout_hex(txout_hex: &str) -> Result<TxOut, String> {
    let bytes = hex::decode(txout_hex).map_err(|e| format!("txout_hex decode: {e}"))?;
    TxOut::consensus_decode(&mut bytes.as_slice())
        .map_err(|e| format!("txout consensus decode: {e}"))
}

/// Wire key for an OutPoint: `txid:vout` (txid = bitcoin display hex).
pub fn encode_outpoint_key(op: &OutPoint) -> String {
    format!("{}:{}", op.txid, op.vout)
}

/// Parse `txid:vout` wire key.
pub fn decode_outpoint_key(key: &str) -> Result<OutPoint, String> {
    let (txid_hex, vout_str) = key
        .rsplit_once(':')
        .ok_or_else(|| format!("outpoint key must be txid:vout, got `{key}`"))?;
    let txid =
        Txid::from_str_hex(txid_hex).map_err(|e| format!("outpoint txid `{txid_hex}`: {e}"))?;
    let vout: u32 = vout_str
        .parse()
        .map_err(|e| format!("outpoint vout `{vout_str}`: {e}"))?;
    Ok(OutPoint { txid, vout })
}

/// Encode chain P2MR UTXO map for rules.v1 `chain_p2mr_utxos_hex`.
pub fn encode_chain_p2mr_utxos_hex(
    utxos: &std::collections::HashMap<OutPoint, TxOut>,
) -> BTreeMap<String, String> {
    utxos
        .iter()
        .map(|(op, txout)| (encode_outpoint_key(op), encode_txout_hex(txout)))
        .collect()
}

/// Decode `chain_p2mr_utxos_hex` into OutPoint → TxOut.
///
/// Fail-closed: invalid entries return `Err`.
pub fn decode_chain_p2mr_utxos_hex(
    utxos: &BTreeMap<String, String>,
) -> Result<std::collections::HashMap<OutPoint, TxOut>, String> {
    let mut out = std::collections::HashMap::with_capacity(utxos.len());
    for (key, txout_hex) in utxos {
        let op = decode_outpoint_key(key).map_err(|e| format!("chain utxo key `{key}`: {e}"))?;
        let txout = decode_txout_hex(txout_hex).map_err(|e| format!("chain utxo `{key}`: {e}"))?;
        out.insert(op, txout);
    }
    Ok(out)
}

/// Txid from bitcoin display hex (big-endian hex string).
trait TxidFromStrHex {
    fn from_str_hex(s: &str) -> Result<Txid, String>;
}

impl TxidFromStrHex for Txid {
    fn from_str_hex(s: &str) -> Result<Txid, String> {
        use std::str::FromStr;
        Txid::from_str(s).map_err(|e| e.to_string())
    }
}

/// Read at most `max` bytes from `r` (DoS cap for worker stdin / IPC body).
pub fn read_capped(mut r: impl Read, max: usize) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let n = r.read(&mut chunk).map_err(|e| format!("read: {e}"))?;
        if n == 0 {
            break;
        }
        if buf.len().saturating_add(n) > max {
            return Err(format!("request exceeds max size {max} bytes"));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok(buf)
}

// ── UDS length-prefix framing (RUL1) ──────────────────────────────────────

/// Encode a body into a framed UDS message: magic + BE length + body.
pub fn encode_frame(body: &[u8]) -> Result<Vec<u8>, TransportError> {
    if body.len() > MAX_IPC_BODY_BYTES {
        return Err(TransportError::failure(format!(
            "frame body {} exceeds max {MAX_IPC_BODY_BYTES}",
            body.len()
        )));
    }
    let mut out = Vec::with_capacity(FRAME_HEADER_LEN + body.len());
    out.extend_from_slice(FRAME_MAGIC);
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(body);
    Ok(out)
}

/// Write one framed message to `w`.
pub fn write_frame(mut w: impl Write, body: &[u8]) -> Result<(), TransportError> {
    let frame = encode_frame(body)?;
    w.write_all(&frame)
        .map_err(|e| TransportError::failure(format!("write frame: {e}")))?;
    w.flush()
        .map_err(|e| TransportError::failure(format!("flush frame: {e}")))?;
    Ok(())
}

/// Read exactly `n` bytes (or error on EOF / short read).
fn read_exact_n(mut r: impl Read, n: usize) -> Result<Vec<u8>, TransportError> {
    let mut buf = vec![0u8; n];
    let mut filled = 0;
    while filled < n {
        match r.read(&mut buf[filled..]) {
            Ok(0) => {
                return Err(TransportError::failure(format!(
                    "unexpected EOF reading frame ({filled}/{n} bytes)"
                )));
            }
            Ok(k) => filled += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Err(TransportError::Timeout);
            }
            Err(e) => {
                return Err(TransportError::failure(format!("read frame: {e}")));
            }
        }
    }
    Ok(buf)
}

/// Read one framed message from `r`. Returns body bytes (JSON).
///
/// Fail-closed on bad magic, oversize length, or truncated body.
pub fn read_frame(mut r: impl Read) -> Result<Vec<u8>, TransportError> {
    let header = read_exact_n(&mut r, FRAME_HEADER_LEN)?;
    if &header[..4] != FRAME_MAGIC.as_slice() {
        return Err(TransportError::failure(format!(
            "bad frame magic: got {:02x?} expected RUL1",
            &header[..4]
        )));
    }
    let len = u32::from_be_bytes([header[4], header[5], header[6], header[7]]) as usize;
    if len > MAX_IPC_BODY_BYTES {
        return Err(TransportError::failure(format!(
            "frame length {len} exceeds max {MAX_IPC_BODY_BYTES}"
        )));
    }
    if len == 0 {
        return Err(TransportError::failure("empty frame body"));
    }
    read_exact_n(r, len)
}

/// Serve one request/response on an already-connected stream (framed).
///
/// `handler` maps a decoded request to a response. Transport/framing errors
/// are returned; handler policy errors should be encoded as `RulesV1Response`.
pub fn serve_framed_connection<F>(
    mut stream: impl Read + Write,
    handler: F,
) -> Result<(), TransportError>
where
    F: FnOnce(RulesV1Request) -> RulesV1Response,
{
    let body = read_frame(&mut stream)?;
    let req = decode_request(&body)
        .map_err(|e| TransportError::failure(format!("decode request: {e}")))?;
    let resp = handler(req);
    let out = encode_response(&resp)
        .map_err(|e| TransportError::failure(format!("encode response: {e}")))?;
    write_frame(stream, &out)
}

/// Like [`serve_framed_connection`] but applies I/O timeouts on a `UnixStream`
/// so a hung peer cannot freeze the single-threaded worker accept loop.
pub fn serve_framed_unix_stream<F>(
    stream: UnixStream,
    handler: F,
    io_timeout: Duration,
) -> Result<(), TransportError>
where
    F: FnOnce(RulesV1Request) -> RulesV1Response,
{
    stream
        .set_read_timeout(Some(io_timeout))
        .map_err(|e| TransportError::failure(format!("set_read_timeout: {e}")))?;
    stream
        .set_write_timeout(Some(io_timeout))
        .map_err(|e| TransportError::failure(format!("set_write_timeout: {e}")))?;
    serve_framed_connection(stream, handler)
}

/// Poll interval while waiting for accept under nonblocking mode (shutdown checks).
pub const UDS_ACCEPT_POLL_MS: u64 = 50;

/// Accept connections on `listener` until `shutdown` is set (or forever if None).
///
/// Each connection: one framed request → handler → framed response, then close.
/// Accepted streams get read/write timeouts (`io_timeout`, default
/// [`DEFAULT_REMOTE_TIMEOUT`]) so a hung client cannot stall the worker.
///
/// **Shutdown:** listener is nonblocking; accept poll sleeps [`UDS_ACCEPT_POLL_MS`]
/// so a set `shutdown` flag exits without process kill. Mid-flight request
/// handling on the worker is still not cancelled mid-handler. Hub-side timed-out
/// helpers use an eager background reaper (not join-on-next-call).
pub fn serve_uds_loop<F>(
    listener: &UnixListener,
    handler: F,
    shutdown: Option<&std::sync::atomic::AtomicBool>,
) -> Result<(), TransportError>
where
    F: FnMut(RulesV1Request) -> RulesV1Response,
{
    serve_uds_loop_with_timeout(listener, handler, shutdown, DEFAULT_REMOTE_TIMEOUT)
}

/// [`serve_uds_loop`] with an explicit per-connection I/O timeout.
pub fn serve_uds_loop_with_timeout<F>(
    listener: &UnixListener,
    mut handler: F,
    shutdown: Option<&std::sync::atomic::AtomicBool>,
    io_timeout: Duration,
) -> Result<(), TransportError>
where
    F: FnMut(RulesV1Request) -> RulesV1Response,
{
    // Nonblocking accept so shutdown can be observed without process kill.
    listener
        .set_nonblocking(true)
        .map_err(|e| TransportError::failure(format!("set_nonblocking: {e}")))?;
    let poll = Duration::from_millis(UDS_ACCEPT_POLL_MS);
    loop {
        if shutdown.is_some_and(|s| s.load(std::sync::atomic::Ordering::Relaxed)) {
            return Ok(());
        }
        match listener.accept() {
            Ok((stream, _)) => {
                // Serve path expects blocking stream I/O with read/write timeouts.
                if let Err(e) = stream.set_nonblocking(false) {
                    tracing::error!(error = %e, "rules UDS set_nonblocking(false) failed");
                    continue;
                }
                if let Err(e) = serve_framed_unix_stream(stream, &mut handler, io_timeout) {
                    tracing::error!(error = %e, "rules UDS connection failed (fail-closed)");
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(poll);
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                return Err(TransportError::failure(format!("accept: {e}")));
            }
        }
    }
}

/// True if `path` accepts a UnixStream connect (live listener).
fn uds_path_is_live(path: &Path) -> bool {
    match UnixStream::connect(path) {
        Ok(stream) => {
            drop(stream);
            true
        }
        Err(_) => false,
    }
}

/// Bind a Unix domain socket at `path`.
///
/// * **Refuse** if `path` already has a **live** listener (connect succeeds) —
///   never unlink a serving peer.
/// * Stale/dead socket files are removed, then we bind a process-private temp
///   path, set mode `0600`, and rename into place.
///
/// **Residual race:** between the live-connect check and `rename`, another
/// process could bind the final path; we re-check once before rename and abort
/// if live, but a TOCTOU window remains under concurrent binders (local-trust
/// deploy only).
pub fn bind_uds(path: impl AsRef<Path>) -> Result<UnixListener, TransportError> {
    use std::os::unix::fs::PermissionsExt;

    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| TransportError::failure(format!("create uds parent: {e}")))?;
    }
    if path.exists() {
        if uds_path_is_live(path) {
            return Err(TransportError::failure(format!(
                "uds path {} is in use (connect succeeded); refuse to replace live socket",
                path.display()
            )));
        }
        // Stale path (no live peer) — safe to remove before rename.
        drop(std::fs::remove_file(path));
    }
    let tmp = path.with_extension(format!("bind.{}", std::process::id()));
    drop(std::fs::remove_file(&tmp));
    let listener = UnixListener::bind(&tmp)
        .map_err(|e| TransportError::failure(format!("bind uds temp {}: {e}", tmp.display())))?;
    // Owner-only access on the socket inode (best-effort). Worker auth is
    // intentional local-trust residual: filesystem permissions only (no mutual
    // auth / MAC on the wire).
    let perms = std::fs::Permissions::from_mode(0o600);
    if let Err(e) = std::fs::set_permissions(&tmp, perms) {
        tracing::warn!(
            path = %tmp.display(),
            error = %e,
            "failed to set uds mode 0600 (continuing with default perms; local-trust residual)"
        );
    }
    // Final race: if another process bound `path` between our check and rename,
    // refuse rather than unlinking their socket.
    if path.exists() && uds_path_is_live(path) {
        drop(std::fs::remove_file(&tmp));
        return Err(TransportError::failure(format!(
            "uds path {} became live before rename; aborting bind",
            path.display()
        )));
    }
    drop(std::fs::remove_file(path));
    std::fs::rename(&tmp, path).map_err(|e| {
        drop(std::fs::remove_file(&tmp));
        TransportError::failure(format!(
            "rename uds {} -> {}: {e}",
            tmp.display(),
            path.display()
        ))
    })?;
    Ok(listener)
}

/// Pluggable transport for remote workers (mock in tests; UDS in production).
pub trait RuleTransport: Send + Sync {
    fn call(&self, request: RulesV1Request) -> Result<RulesV1Response, TransportError>;

    /// Best-effort: interrupt an in-flight [`Self::call`] (e.g. shutdown the
    /// active UDS socket so a blocked mid-read fails quickly). Default is no-op.
    /// Ballot still fail-closed as Timeout immediately on the hub path.
    fn abort_inflight(&self) {}
}

/// Generation-scoped abort handle for the single in-flight UDS call.
struct ActiveUdsCall {
    generation: u64,
    stream: UnixStream,
}

/// Hub-side UDS transport: dials `path`, sends one framed request, reads one framed response.
///
/// Socket read/write timeouts are set from [`Self::io_timeout`] so a hung worker
/// cannot block the helper thread forever (hub still enforces
/// [`RemoteRuleClient::timeout`] as well).
///
/// **One in-flight call per transport:** [`RuleTransport::call`] is serialized
/// on an internal mutex so concurrent helpers cannot overwrite each other's
/// abort handle. Matches hub model (one remote worker path per
/// [`RemoteRuleClient`]). On join-timeout, [`Self::abort_inflight`] shuts down
/// the active stream (does **not** take the call mutex — avoids deadlock with
/// a blocked mid-read). Active slot is also generation-scoped so an orphaned
/// clear cannot wipe a later call after the mutex is released.
pub struct UdsRuleTransport {
    path: PathBuf,
    /// Applied to the UnixStream read/write timeouts (defaults to [`DEFAULT_REMOTE_TIMEOUT`]).
    pub io_timeout: Duration,
    /// Serializes [`RuleTransport::call`] — at most one in-flight dial/read.
    /// Not held by [`Self::abort_inflight`].
    call_lock: Mutex<()>,
    /// Monotonic call generation (paired with [`Self::active`]).
    active_gen: AtomicU64,
    /// Abort handle for the single in-flight [`RuleTransport::call`], if any.
    active: Mutex<Option<ActiveUdsCall>>,
}

impl std::fmt::Debug for UdsRuleTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UdsRuleTransport")
            .field("path", &self.path)
            .field("io_timeout", &self.io_timeout)
            .field("active_gen", &self.active_gen.load(Ordering::Relaxed))
            .field(
                "active_generation",
                &self
                    .active
                    .lock()
                    .ok()
                    .and_then(|g| g.as_ref().map(|a| a.generation)),
            )
            .finish_non_exhaustive()
    }
}

impl Clone for UdsRuleTransport {
    fn clone(&self) -> Self {
        Self {
            path: self.path.clone(),
            io_timeout: self.io_timeout,
            call_lock: Mutex::new(()),
            active_gen: AtomicU64::new(0),
            active: Mutex::new(None),
        }
    }
}

impl UdsRuleTransport {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            io_timeout: DEFAULT_REMOTE_TIMEOUT,
            call_lock: Mutex::new(()),
            active_gen: AtomicU64::new(0),
            active: Mutex::new(None),
        }
    }

    pub fn with_io_timeout(mut self, timeout: Duration) -> Self {
        self.io_timeout = normalize_remote_timeout(timeout);
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Install abort handle for this call; returns generation for scoped clear.
    /// Caller must hold [`Self::call_lock`] (single in-flight).
    fn install_active(&self, stream: UnixStream) -> u64 {
        let generation = self.active_gen.fetch_add(1, Ordering::Relaxed) + 1;
        let entry = ActiveUdsCall { generation, stream };
        match self.active.lock() {
            Ok(mut g) => *g = Some(entry),
            Err(poisoned) => *poisoned.into_inner() = Some(entry),
        }
        generation
    }

    /// Clear abort handle only if it still belongs to `generation` (orphan-safe).
    fn clear_active_if(&self, generation: u64) {
        let mut slot = match self.active.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        if slot.as_ref().is_some_and(|a| a.generation == generation) {
            *slot = None;
        }
    }

    /// Current active generation, if any (`#[cfg(test)]` observability).
    #[cfg(test)]
    fn active_generation(&self) -> Option<u64> {
        match self.active.lock() {
            Ok(g) => g.as_ref().map(|a| a.generation),
            Err(poisoned) => poisoned.into_inner().as_ref().map(|a| a.generation),
        }
    }

    /// True when another thread currently holds the call lock (test helper).
    #[cfg(test)]
    fn try_call_lock(&self) -> bool {
        self.call_lock.try_lock().is_ok()
    }
}

impl RuleTransport for UdsRuleTransport {
    fn call(&self, request: RulesV1Request) -> Result<RulesV1Response, TransportError> {
        // One in-flight dial/read per transport: concurrent callers wait so
        // install_active / abort_inflight always target that single call.
        let _call_guard = self.call_lock.lock().unwrap_or_else(|p| p.into_inner());

        let mut stream = UnixStream::connect(&self.path).map_err(|e| {
            TransportError::failure(format!("connect uds {}: {e}", self.path.display()))
        })?;
        // Enforce I/O deadline on the socket itself (not a no-op).
        stream
            .set_read_timeout(Some(self.io_timeout))
            .map_err(|e| TransportError::failure(format!("set_read_timeout: {e}")))?;
        stream
            .set_write_timeout(Some(self.io_timeout))
            .map_err(|e| TransportError::failure(format!("set_write_timeout: {e}")))?;

        // Clone for abort path (shutdown from timeout). Generation-scoped so an
        // orphaned helper cannot clear a later call's handle after unlock.
        let generation = match stream.try_clone() {
            Ok(abort_handle) => Some(self.install_active(abort_handle)),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    path = %self.path.display(),
                    "UdsRuleTransport try_clone failed; mid-read abort disabled for this call"
                );
                None
            }
        };

        let result = (|| {
            let body = encode_request(&request)
                .map_err(|e| TransportError::failure(format!("encode request: {e}")))?;
            write_frame(&mut stream, &body)?;
            // Half-close write side so a peer that waits for EOF on a multi-message
            // stream still finishes; framed servers only need the frame.
            drop(stream.shutdown(std::net::Shutdown::Write));

            let resp_body = read_frame(&mut stream)?;
            decode_response(&resp_body)
                .map_err(|e| TransportError::failure(format!("decode response: {e}")))
        })();

        if let Some(call_gen) = generation {
            self.clear_active_if(call_gen);
        }
        // _call_guard drops here — next waiter may enter.
        result
    }

    fn abort_inflight(&self) {
        // Do not acquire call_lock (would deadlock with a blocked mid-read).
        // With serialized call, active is always the single in-flight call.
        let taken = match self.active.lock() {
            Ok(mut g) => g.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some(ActiveUdsCall { stream, .. }) = taken {
            // Both directions: wake blocked read and prevent further writes.
            drop(stream.shutdown(std::net::Shutdown::Both));
        }
    }
}

/// Map a known worker rule id string to a static [`RuleId`].
///
/// Residual (not unlocked this loop): only compile-time known ids
/// (`drivechain`, `bip360`); no freeform/dynamic RuleId registration.
pub fn static_rule_id(name: &str) -> Result<RuleId, String> {
    match name {
        "drivechain" => Ok("drivechain"),
        "bip360" => Ok("bip360"),
        other => Err(format!(
            "unknown rules worker id `{other}` (supported: drivechain, bip360)"
        )),
    }
}

/// One configured remote worker endpoint (`RULE_ID=PATH`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RulesWorkerEndpoint {
    pub rule_id: RuleId,
    pub path: PathBuf,
}

impl RulesWorkerEndpoint {
    /// Parse `RULE_ID=PATH` (e.g. `bip360=/tmp/cusf-rules-bip360.sock`).
    pub fn parse(s: &str) -> Result<Self, String> {
        let (id, path) = s
            .split_once('=')
            .ok_or_else(|| format!("expected RULE_ID=PATH, got `{s}`"))?;
        let rule_id = static_rule_id(id.trim())?;
        let path = PathBuf::from(path.trim());
        if path.as_os_str().is_empty() {
            return Err("rules worker path must not be empty".into());
        }
        Ok(Self { rule_id, path })
    }
}

/// Build a [`super::BackendEngine`] with Remote UDS backends for each endpoint.
///
/// Handshakes each worker and reads the `validation` capability token.
///
/// **Consent policy (no soft-Accept; no brick on stub/unreachable):**
/// * Successful handshake **and** allowlisted real `validation` → register Remote
///   (mempool: Local ballot **AND** remote; see BlockHandler).
/// * Stub / unknown / missing capability → **do not register**; keep Local; log warn.
/// * Unreachable (handshake Err) → **do not register**; keep Local; log error.
///   Fail-closed still applies once a *real* remote is registered.
pub fn build_remote_backend_engine(
    endpoints: &[RulesWorkerEndpoint],
    timeout: Duration,
) -> super::BackendEngine {
    use super::{BackendEngine, RuleBackend};

    let timeout = normalize_remote_timeout(timeout);
    let mut engine = BackendEngine::new();
    for ep in endpoints {
        let transport = Arc::new(UdsRuleTransport::new(ep.path.clone()).with_io_timeout(timeout));
        let client = RemoteRuleClient::new(ep.rule_id, transport).with_timeout(timeout);
        match client.handshake() {
            Ok(()) => {
                let validation = client.validation_capability();
                let hs = Ok(validation.clone());
                if !should_register_remote_for_consent(&hs) {
                    tracing::warn!(
                        rule_id = ep.rule_id,
                        path = %ep.path.display(),
                        validation = validation.as_deref().unwrap_or("(none)"),
                        "remote worker validation not allowlisted/real: \
                         NOT registering for consent (Local ballot kept)"
                    );
                    continue;
                }
                tracing::info!(
                    rule_id = ep.rule_id,
                    path = %ep.path.display(),
                    validation = validation.as_deref().unwrap_or("(none)"),
                    timeout_ms = timeout.as_millis() as u64,
                    "registered remote rules worker (handshake ok; allowlisted validation; \
                     mempool Local AND remote)"
                );
                engine.register(RuleBackend::Remote(client));
            }
            Err(e) => {
                // Unreachable: do NOT register — keep Local (list drivechain/bip360
                // on CLI while worker is down must not brick tip/mempool).
                tracing::error!(
                    rule_id = ep.rule_id,
                    path = %ep.path.display(),
                    error = %e,
                    "rules worker unreachable at boot: NOT registering for consent \
                     (Local ballot kept; fix worker then restart hub)"
                );
            }
        }
    }
    engine
}

/// Configurable remote client. **`timeout` is enforced** via a join deadline
/// around each transport call; deadline miss → [`TransportError::Timeout`] →
/// [`VoteSource::Timeout`](super::VoteSource::Timeout).
///
/// **Dual deadline:** hub `timeout` (join wait) **and** `UdsRuleTransport::io_timeout`
/// (socket read/write). Both should be set to the same operator value (as in
/// [`build_remote_backend_engine`]). Zero timeout is normalized via
/// [`normalize_remote_timeout`].
///
/// Slot for at most one timed-out helper: `(join_handle, cancel_generation)`.
type OrphanJoinSlot = Arc<Mutex<Option<(thread::JoinHandle<()>, u64)>>>;

/// `rule_id` is fixed at construction (not publicly mutable after handshake).
/// After handshake, [`Self::validation_capability`] holds the worker token used
/// by hub consent policy ([`is_real_validation_capability`]).
///
/// **Orphan helper policy:** on deadline miss the helper is interrupted via
/// [`RuleTransport::abort_inflight`] (UDS: shutdown active socket so mid-read
/// fails quickly) plus an **eager background reaper** that joins the helper so
/// the next call is not blocked. At most one prior orphan is tracked; a second
/// timeout eager-reaps any displaced handle. [`Self::cancel_requested`] is
/// generation-scoped status: set on timeout, cleared only by the reaper for that
/// generation. SoftForkRule path still fail-closed immediately as Timeout
/// (never soft-Accept).
#[derive(Clone)]
pub struct RemoteRuleClient {
    rule_id: RuleId,
    /// Public for operators to inspect/configure before use; not mutated after
    /// first call by the client itself. Always ≥ [`MIN_REMOTE_TIMEOUT`] after
    /// [`Self::with_timeout`] / construction.
    pub timeout: Duration,
    transport: Arc<dyn RuleTransport>,
    /// At most one in-flight timed-out helper being background-joined.
    /// Stores `(join_handle, cancel_generation)` so reapers only clear cancel
    /// for their own generation.
    orphan: OrphanJoinSlot,
    /// Capability token from last successful handshake (`validation` field).
    validation: Arc<Mutex<Option<String>>>,
    /// Cancel status: true after join-timeout until matching reaper clears it.
    /// Paired with [`RuleTransport::abort_inflight`] on timeout.
    cancel: Arc<AtomicBool>,
    /// Monotonic generation for timeout → reaper pairing (only matching reaper
    /// clears [`Self::cancel`]).
    cancel_gen: Arc<AtomicU64>,
}

impl std::fmt::Debug for RemoteRuleClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteRuleClient")
            .field("rule_id", &self.rule_id)
            .field("timeout", &self.timeout)
            .field("validation", &self.validation_capability())
            .field("cancel_requested", &self.cancel_requested())
            .finish_non_exhaustive()
    }
}

impl RemoteRuleClient {
    pub fn new(rule_id: RuleId, transport: Arc<dyn RuleTransport>) -> Self {
        Self {
            rule_id,
            timeout: DEFAULT_REMOTE_TIMEOUT,
            transport,
            orphan: Arc::new(Mutex::new(None)),
            validation: Arc::new(Mutex::new(None)),
            cancel: Arc::new(AtomicBool::new(false)),
            cancel_gen: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Status: true after a join-timeout until the reaper for that cancel
    /// generation clears it. On timeout the hub also calls
    /// [`RuleTransport::abort_inflight`] (UDS socket shutdown) so mid-read
    /// helpers fail quickly. Ballot is fail-closed immediately as Timeout.
    pub fn cancel_requested(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }

    /// Stable rule id fixed at construction (handshake must match).
    pub fn rule_id(&self) -> RuleId {
        self.rule_id
    }

    /// Capability token from last successful handshake (if any).
    pub fn validation_capability(&self) -> Option<String> {
        match self.validation.lock() {
            Ok(g) => g.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// True when handshake reported a real (non-stub) validation capability.
    pub fn has_real_validation(&self) -> bool {
        is_real_validation_capability(self.validation_capability().as_deref())
    }

    /// Set join deadline (and should match UDS `io_timeout`). Zero → default.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = normalize_remote_timeout(timeout);
        self
    }

    /// Non-blocking: if a prior timed-out helper is still tracked, move it to an
    /// eager background join so this call never blocks on a long orphan.
    /// Does not clear [`Self::cancel`] (generation-scoped reapers own that).
    fn kick_orphan_reaper(&self) {
        let taken = match self.orphan.lock() {
            Ok(mut g) => g.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        if let Some((h, generation)) = taken {
            let cancel = Arc::clone(&self.cancel);
            let cancel_gen = Arc::clone(&self.cancel_gen);
            thread::spawn(move || {
                drop(h.join());
                // Only clear cancel if no newer timeout superseded this generation.
                if cancel_gen.load(Ordering::Relaxed) == generation {
                    cancel.store(false, Ordering::Relaxed);
                }
            });
        }
    }

    /// Run transport call on a helper thread; wait up to `self.timeout`.
    ///
    /// On timeout: bump cancel generation, set cancel flag, spawn an **eager
    /// background reaper** for the helper (and any displaced prior orphan) so
    /// the next call is not blocked, and return fail-closed
    /// [`TransportError::Timeout`] immediately — consent never soft-Accepts.
    fn call_with_deadline(
        &self,
        request: RulesV1Request,
    ) -> Result<RulesV1Response, TransportError> {
        // Never block the hot path on a prior timed-out helper.
        self.kick_orphan_reaper();

        let (tx, rx) = mpsc::channel();
        let transport = Arc::clone(&self.transport);
        let handle = thread::spawn(move || {
            drop(tx.send(transport.call(request)));
        });
        match rx.recv_timeout(self.timeout) {
            Ok(result) => {
                // Helper finished in time — join to avoid detach.
                drop(handle.join());
                result
            }
            Err(RecvTimeoutError::Timeout) => {
                let generation = self.cancel_gen.fetch_add(1, Ordering::Relaxed) + 1;
                self.cancel.store(true, Ordering::Relaxed);
                // Wake blocked UDS mid-read (or other transport I/O) promptly.
                self.transport.abort_inflight();
                // Track at most one orphan; eager-reap any displaced previous
                // immediately so we never join on the hot path.
                let displaced = match self.orphan.lock() {
                    Ok(mut g) => g.replace((handle, generation)),
                    Err(poisoned) => poisoned.into_inner().replace((handle, generation)),
                };
                if let Some((prev, prev_generation)) = displaced {
                    let cancel = Arc::clone(&self.cancel);
                    let cancel_gen = Arc::clone(&self.cancel_gen);
                    thread::spawn(move || {
                        drop(prev.join());
                        if cancel_gen.load(Ordering::Relaxed) == prev_generation {
                            cancel.store(false, Ordering::Relaxed);
                        }
                    });
                }
                // Eager reaper for the just-stored handle (generation-scoped clear).
                let orphan = Arc::clone(&self.orphan);
                let cancel = Arc::clone(&self.cancel);
                let cancel_gen = Arc::clone(&self.cancel_gen);
                thread::spawn(move || {
                    let taken = match orphan.lock() {
                        Ok(mut g) => g.take(),
                        Err(poisoned) => poisoned.into_inner().take(),
                    };
                    if let Some((h, stored_generation)) = taken {
                        drop(h.join());
                        if cancel_gen.load(Ordering::Relaxed) == stored_generation {
                            cancel.store(false, Ordering::Relaxed);
                        }
                    }
                });
                Err(TransportError::Timeout)
            }
            Err(RecvTimeoutError::Disconnected) => {
                drop(handle.join());
                Err(TransportError::failure("remote worker thread disconnected"))
            }
        }
    }

    /// Perform handshake; version skew → Failure-style error string.
    /// Stores worker `validation` capability for hub consent policy.
    pub fn handshake(&self) -> Result<(), String> {
        let resp = self
            .call_with_deadline(RulesV1Request::Handshake {
                version: RULES_V1_VERSION,
                rule_id: self.rule_id.to_string(),
            })
            .map_err(|e| e.to_string())?;
        match resp {
            RulesV1Response::HandshakeOk {
                version,
                rule_id,
                validation,
            } => {
                if version != RULES_V1_VERSION {
                    return Err(format!(
                        "rules version skew: hub={RULES_V1_VERSION} worker={version}"
                    ));
                }
                if rule_id != self.rule_id {
                    return Err(format!(
                        "rule id mismatch: expected {} got {rule_id}",
                        self.rule_id
                    ));
                }
                match self.validation.lock() {
                    Ok(mut g) => *g = validation,
                    Err(poisoned) => *poisoned.into_inner() = validation,
                }
                Ok(())
            }
            RulesV1Response::HandshakeErr { detail } => Err(detail),
            other => Err(format!("unexpected handshake response: {other:?}")),
        }
    }

    fn ballot_from_call(&self, request: RulesV1Request) -> RuleBallot {
        match self.call_with_deadline(request) {
            Ok(resp) => match resp.into_vote() {
                Ok(vote) => RuleBallot::explicit(self.rule_id, vote),
                Err(detail) => RuleBallot::failure(self.rule_id, detail),
            },
            Err(TransportError::Timeout) => RuleBallot::timeout(self.rule_id),
            Err(TransportError::Failure { detail }) => RuleBallot::failure(self.rule_id, detail),
        }
    }

    pub fn validate_tx(&self, ctx: &RuleTxContext<'_>) -> RuleBallot {
        self.ballot_from_call(request_validate_tx(ctx))
    }

    pub fn connect_block(&self, ctx: &RuleBlockContext<'_>) -> RuleBallot {
        self.ballot_from_call(request_connect_block(ctx))
    }
}

/// Mock transport for unit tests only (not compiled into non-test lib).
#[cfg(test)]
#[derive(Default)]
struct MockTransport {
    inner: Mutex<MockTransportInner>,
}

#[cfg(test)]
#[derive(Default)]
struct MockTransportInner {
    /// Queue of scripted outcomes (pop front).
    script: Vec<Result<RulesV1Response, TransportError>>,
    /// Default ValidateTx/ConnectBlock vote when script empty.
    default_accept: bool,
}

#[cfg(test)]
impl MockTransport {
    fn new() -> Self {
        Self::default()
    }

    fn always_accept() -> Self {
        let t = Self::new();
        if let Ok(mut g) = t.inner.lock() {
            g.default_accept = true;
        }
        t
    }

    fn push(&self, outcome: Result<RulesV1Response, TransportError>) {
        if let Ok(mut g) = self.inner.lock() {
            g.script.push(outcome);
        }
    }

    fn push_timeout(&self) {
        self.push(Err(TransportError::Timeout));
    }

    fn push_failure(&self, detail: impl Into<String>) {
        self.push(Err(TransportError::failure(detail)));
    }
}

#[cfg(test)]
impl RuleTransport for MockTransport {
    fn call(&self, request: RulesV1Request) -> Result<RulesV1Response, TransportError> {
        let mut g = self
            .inner
            .lock()
            .map_err(|_| TransportError::failure("mock mutex poisoned"))?;
        if !g.script.is_empty() {
            return g.script.remove(0);
        }
        match request {
            RulesV1Request::Handshake { version, rule_id } => {
                if version != RULES_V1_VERSION {
                    return Ok(RulesV1Response::HandshakeErr {
                        detail: format!("unsupported version {version}"),
                    });
                }
                // Mock defaults to allowlisted bip360 token so has_real_validation is true
                // for consent-policy unit tests (spoof tokens are not allowlisted).
                Ok(RulesV1Response::HandshakeOk {
                    version,
                    rule_id,
                    validation: Some(VALIDATION_BIP360_PARENTS_AND_CHAIN_UTXOS.into()),
                })
            }
            RulesV1Request::Health => Ok(RulesV1Response::HealthOk {
                rule_id: "mock".into(),
                validation: Some(VALIDATION_BIP360_PARENTS_AND_CHAIN_UTXOS.into()),
            }),
            RulesV1Request::ValidateTx { .. } | RulesV1Request::ConnectBlock { .. } => {
                if g.default_accept {
                    Ok(RulesV1Response::accept())
                } else {
                    Ok(RulesV1Response::reject("mock default reject"))
                }
            }
        }
    }
}

/// Build a ValidateTx request from a context (includes `tx_hex` / parents /
/// `drivechain_state` when present).
///
/// Crate-visible helper for hub wiring and unit tests (not a public API surface).
pub(crate) fn request_validate_tx(ctx: &RuleTxContext<'_>) -> RulesV1Request {
    RulesV1Request::ValidateTx {
        height: ctx.height,
        label: ctx.label.to_string(),
        tx_hex: ctx.tx.map(encode_tx_hex),
        parent_txs_hex: ctx.parent_txs.map(encode_parent_txs_hex),
        drivechain_state: ctx.drivechain_state.cloned(),
    }
}

/// Build a ConnectBlock request from a context.
pub(crate) fn request_connect_block(ctx: &RuleBlockContext<'_>) -> RulesV1Request {
    RulesV1Request::ConnectBlock {
        height: ctx.height,
        label: ctx.label.to_string(),
        block_hex: ctx.block.map(encode_block_hex),
        chain_p2mr_utxos_hex: ctx.chain_p2mr_utxos.map(encode_chain_p2mr_utxos_hex),
        drivechain_state: ctx.drivechain_state.cloned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validator::rules::{
        BackendEngine, RuleBackend, RuleCallError, RuleEngine, SoftForkRule, VoteSource,
    };

    struct AlwaysAccept {
        id: RuleId,
    }

    impl SoftForkRule for AlwaysAccept {
        fn id(&self) -> RuleId {
            self.id
        }
        fn validate_tx(&self, _: &RuleTxContext<'_>) -> Result<RuleVote, RuleCallError> {
            Ok(RuleVote::Accept)
        }
        fn connect_block(&self, _: &RuleBlockContext<'_>) -> Result<RuleVote, RuleCallError> {
            Ok(RuleVote::Accept)
        }
    }

    fn tx_ctx() -> RuleTxContext<'static> {
        RuleTxContext::label_only(1, "t")
    }

    fn sample_tx() -> bitcoin::Transaction {
        bitcoin::Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: bitcoin::absolute::LockTime::ZERO,
            input: vec![],
            output: vec![],
        }
    }

    #[test]
    fn rules_v1_version_is_one() {
        assert_eq!(RULES_V1_VERSION, 1);
    }

    #[test]
    fn json_roundtrip_validate_tx() {
        let mut parents = BTreeMap::new();
        parents.insert("aa".repeat(32), "00".into());
        let req = RulesV1Request::ValidateTx {
            height: 42,
            label: "lab".into(),
            tx_hex: Some("00".into()),
            parent_txs_hex: Some(parents),
            drivechain_state: None,
        };
        let bytes = encode_request(&req).unwrap();
        let decoded = decode_request(&bytes).unwrap();
        assert_eq!(decoded, req);

        let resp = RulesV1Response::reject("nope");
        let bytes = encode_response(&resp).unwrap();
        let decoded = decode_response(&bytes).unwrap();
        assert_eq!(decoded, resp);
    }

    #[test]
    fn json_roundtrip_validate_tx_with_drivechain_state() {
        use crate::validator::rules::DrivechainCtipSnapshot;
        let state = DrivechainStateSnapshot {
            tip_height: 99,
            tip_hash_hex: "ef".repeat(32),
            active_sidechain_numbers: vec![0, 7],
            ctips: vec![],
            pending_m6ids: vec![],
            withdrawal_bundle_inclusion_threshold: 5,
        };
        let req = RulesV1Request::ValidateTx {
            height: 99,
            label: "dc".into(),
            tx_hex: Some("00".into()),
            parent_txs_hex: None,
            drivechain_state: Some(state.clone()),
        };
        let bytes = encode_request(&req).unwrap();
        let decoded = decode_request(&bytes).unwrap();
        assert_eq!(decoded, req);
        match decoded {
            RulesV1Request::ValidateTx {
                drivechain_state: Some(s),
                ..
            } => {
                assert_eq!(s.tip_height, 99);
                assert_eq!(s.active_sidechain_numbers, vec![0, 7]);
            }
            other => panic!("expected populated drivechain_state, got {other:?}"),
        }

        let req_cb = RulesV1Request::ConnectBlock {
            height: 99,
            label: "dc".into(),
            block_hex: Some("00".into()),
            chain_p2mr_utxos_hex: None,
            drivechain_state: Some(state),
        };
        let bytes = encode_request(&req_cb).unwrap();
        assert_eq!(decode_request(&bytes).unwrap(), req_cb);

        // Non-empty ctips + pending M6ids round-trip (Path A wire).
        use crate::validator::rules::DrivechainPendingM6idSnapshot;
        let with_ctips = DrivechainStateSnapshot {
            tip_height: 10,
            tip_hash_hex: "ab".repeat(32),
            active_sidechain_numbers: vec![1],
            ctips: vec![DrivechainCtipSnapshot {
                sidechain_number: 1,
                outpoint_txid_hex:
                    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
                outpoint_vout: 2,
                value_sats: 42_000,
            }],
            pending_m6ids: vec![DrivechainPendingM6idSnapshot {
                sidechain_number: 1,
                // Non-palindromic m6id hex (endianness regression guard).
                m6id_hex: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
                vote_count: 6,
                proposal_height: 3,
            }],
            withdrawal_bundle_inclusion_threshold: 5,
        };
        let req_ctip = RulesV1Request::ValidateTx {
            height: 10,
            label: "ctip".into(),
            tx_hex: Some("00".into()),
            parent_txs_hex: None,
            drivechain_state: Some(with_ctips.clone()),
        };
        let decoded = decode_request(&encode_request(&req_ctip).unwrap()).unwrap();
        assert_eq!(decoded, req_ctip);
        match decoded {
            RulesV1Request::ValidateTx {
                drivechain_state: Some(s),
                ..
            } => {
                assert_eq!(s.ctips.len(), 1);
                assert_eq!(s.ctips[0].sidechain_number, 1);
                assert_eq!(s.ctips[0].outpoint_vout, 2);
                assert_eq!(s.ctips[0].value_sats, 42_000);
                assert_eq!(s.pending_m6ids.len(), 1);
                assert_eq!(s.pending_m6ids[0].vote_count, 6);
                assert_eq!(s.withdrawal_bundle_inclusion_threshold, 5);
            }
            other => panic!("expected ctips/pending on wire, got {other:?}"),
        }
        let req_cb_ctip = RulesV1Request::ConnectBlock {
            height: 10,
            label: "ctip".into(),
            block_hex: Some("00".into()),
            chain_p2mr_utxos_hex: None,
            drivechain_state: Some(with_ctips),
        };
        assert_eq!(
            decode_request(&encode_request(&req_cb_ctip).unwrap()).unwrap(),
            req_cb_ctip
        );
    }

    #[test]
    fn frame_roundtrip_and_bad_magic() {
        let body = b"{\"method\":\"Health\"}";
        let frame = encode_frame(body).unwrap();
        assert_eq!(&frame[..4], FRAME_MAGIC.as_slice());
        let decoded = read_frame(frame.as_slice()).unwrap();
        assert_eq!(decoded, body);

        let mut bad = frame.clone();
        bad[0] = b'X';
        let err = read_frame(bad.as_slice()).unwrap_err();
        assert!(matches!(err, TransportError::Failure { .. }));
        assert!(err.to_string().contains("magic"));
    }

    #[test]
    fn frame_rejects_oversize_and_empty() {
        let oversize = vec![0u8; MAX_IPC_BODY_BYTES + 1];
        assert!(encode_frame(&oversize).is_err());

        // Craft empty-body frame manually
        let mut empty = Vec::from(*FRAME_MAGIC);
        empty.extend_from_slice(&0u32.to_be_bytes());
        let err = read_frame(empty.as_slice()).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn frame_rejects_oversize_length_field() {
        // Header claims MAX+1 body bytes; read must fail before allocating that much.
        let mut frame = Vec::from(*FRAME_MAGIC);
        let bad_len = (MAX_IPC_BODY_BYTES as u32).saturating_add(1);
        frame.extend_from_slice(&bad_len.to_be_bytes());
        // no body
        let err = read_frame(frame.as_slice()).unwrap_err();
        assert!(err.to_string().contains("exceeds max"), "got {err}");
    }

    #[test]
    fn frame_rejects_truncated_body() {
        let body = b"{\"method\":\"Health\"}";
        let mut frame = Vec::from(*FRAME_MAGIC);
        frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
        frame.extend_from_slice(&body[..body.len() / 2]); // truncated
        let err = read_frame(frame.as_slice()).unwrap_err();
        assert!(
            err.to_string().contains("EOF") || err.to_string().contains("unexpected"),
            "got {err}"
        );
    }

    #[test]
    fn parent_txs_hex_rejects_key_txid_mismatch() {
        let parent = sample_tx();
        let wrong_key = "aa".repeat(32); // not compute_txid of parent
        let mut map = BTreeMap::new();
        map.insert(wrong_key, encode_tx_hex(&parent));
        let err = decode_parent_txs_hex(&map).unwrap_err();
        assert!(
            err.contains("key mismatch") || err.contains("mismatch"),
            "got {err}"
        );
    }

    #[test]
    fn remote_ballot_collected_for_backend_engine_path() {
        // When remotes are registered, collect_tx_ballots produces fail-closed votes.
        let mock = Arc::new(MockTransport::new());
        mock.push_failure("worker down");
        let remote = RemoteRuleClient::new("bip360", mock);
        let mut engine = BackendEngine::new();
        engine.register(RuleBackend::Remote(remote));
        assert!(engine.contains("bip360"));
        let ballots = engine.collect_tx_ballots(&tx_ctx());
        assert_eq!(ballots.len(), 1);
        assert!(!ballots[0].consents());
        assert!(!RuleEngine::decide(&ballots).is_accept());
    }

    #[test]
    fn remote_sends_parent_txs_hex_when_ctx_has_parents() {
        let parent = sample_tx();
        let parent_txid = parent.compute_txid();
        let child = sample_tx();
        let mut parents = std::collections::HashMap::new();
        parents.insert(parent_txid, parent);
        struct Capture {
            last: Mutex<Option<RulesV1Request>>,
        }
        impl RuleTransport for Capture {
            fn call(&self, request: RulesV1Request) -> Result<RulesV1Response, TransportError> {
                *self.last.lock().unwrap() = Some(request);
                Ok(RulesV1Response::accept())
            }
        }
        let cap = Arc::new(Capture {
            last: Mutex::new(None),
        });
        let client = RemoteRuleClient::new("bip360", cap.clone());
        let ctx = RuleTxContext::with_tx_and_parents(1, "lab", &child, &parents);
        assert!(client.validate_tx(&ctx).consents());
        let captured = cap.last.lock().unwrap().clone();
        match captured {
            Some(RulesV1Request::ValidateTx {
                parent_txs_hex: Some(map),
                tx_hex: Some(_),
                ..
            }) => {
                assert!(map.contains_key(&parent_txid.to_string()));
            }
            other => panic!("expected parent_txs_hex on wire, got {other:?}"),
        }
    }

    #[test]
    fn remote_omits_parent_txs_hex_when_ctx_has_none() {
        struct Capture {
            last: Mutex<Option<RulesV1Request>>,
        }
        impl RuleTransport for Capture {
            fn call(&self, request: RulesV1Request) -> Result<RulesV1Response, TransportError> {
                *self.last.lock().unwrap() = Some(request);
                Ok(RulesV1Response::accept())
            }
        }
        let cap = Arc::new(Capture {
            last: Mutex::new(None),
        });
        let client = RemoteRuleClient::new("bip360", cap.clone());
        let tx = sample_tx();
        let ctx = RuleTxContext::with_tx(1, "lab", &tx);
        assert!(client.validate_tx(&ctx).consents());
        let captured = cap.last.lock().unwrap().clone();
        match captured {
            Some(RulesV1Request::ValidateTx {
                parent_txs_hex: None,
                ..
            }) => {}
            other => panic!("expected no parent_txs_hex, got {other:?}"),
        }
    }

    #[test]
    fn bind_uds_refuses_live_socket() {
        use temp_dir::TempDir;
        let dir = TempDir::new().unwrap();
        let sock = dir.child("live.sock");
        let _listener = bind_uds(&sock).unwrap();
        // Second bind against live path must refuse (not unlink the first).
        let err = bind_uds(&sock).unwrap_err();
        assert!(
            err.to_string().contains("in use") || err.to_string().contains("live"),
            "got {err}"
        );
    }

    #[test]
    fn normalize_remote_timeout_rejects_zero() {
        assert_eq!(
            normalize_remote_timeout(Duration::ZERO),
            DEFAULT_REMOTE_TIMEOUT
        );
        assert_eq!(
            normalize_remote_timeout(Duration::from_millis(1)),
            Duration::from_millis(1)
        );
        let client = RemoteRuleClient::new("bip360", Arc::new(MockTransport::always_accept()))
            .with_timeout(Duration::ZERO);
        assert_eq!(client.timeout, DEFAULT_REMOTE_TIMEOUT);
    }

    #[test]
    fn uds_hung_peer_hits_io_timeout() {
        use temp_dir::TempDir;
        // Peer accepts then never reads/writes — client must not hang forever.
        let dir = TempDir::new().unwrap();
        let sock = dir.child("hung.sock");
        let listener = bind_uds(&sock).unwrap();
        let server = thread::spawn(move || {
            let (_stream, _) = listener.accept().unwrap();
            // Hold the connection open but do not answer.
            thread::sleep(Duration::from_secs(5));
        });
        let transport =
            Arc::new(UdsRuleTransport::new(&sock).with_io_timeout(Duration::from_millis(80)));
        let client =
            RemoteRuleClient::new("bip360", transport).with_timeout(Duration::from_millis(100));
        let ballot = client.validate_tx(&tx_ctx());
        assert!(
            matches!(
                ballot.source,
                super::super::VoteSource::Timeout | super::super::VoteSource::Failure { .. }
            ),
            "hung peer must fail-closed, got {:?}",
            ballot.source
        );
        assert!(!ballot.consents());
        // Reap orphan join on next call (may fail handshake; must not hang).
        drop(client.handshake());
        drop(server.join());
    }

    #[test]
    fn uds_roundtrip_hub_client_worker() {
        use std::sync::atomic::{AtomicBool, Ordering};

        use temp_dir::TempDir;

        let dir = TempDir::new().unwrap();
        let sock = dir.child("rules.sock");
        let listener = bind_uds(&sock).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = Arc::clone(&stop);
        // One framed request per connection (matches UdsRuleTransport dial-per-call).
        let server = thread::spawn(move || {
            let handler = |req: RulesV1Request| match req {
                RulesV1Request::ValidateTx { tx_hex: None, .. } => {
                    RulesV1Response::reject("missing_tx")
                }
                RulesV1Request::ValidateTx {
                    tx_hex: Some(_), ..
                } => RulesV1Response::accept(),
                RulesV1Request::Handshake { version, rule_id } => RulesV1Response::HandshakeOk {
                    version,
                    rule_id,
                    validation: Some(VALIDATION_BIP360_PARENTS_AND_CHAIN_UTXOS.into()),
                },
                RulesV1Request::Health => RulesV1Response::HealthOk {
                    rule_id: "bip360".into(),
                    validation: Some(VALIDATION_BIP360_PARENTS_AND_CHAIN_UTXOS.into()),
                },
                RulesV1Request::ConnectBlock { .. } => RulesV1Response::accept(),
            };
            // handshake + missing_tx + with_tx = 3 connections
            for _ in 0..3 {
                if stop_t.load(Ordering::Relaxed) {
                    break;
                }
                let (stream, _) = listener.accept().unwrap();
                serve_framed_connection(stream, handler).unwrap();
            }
            stop_t.store(true, Ordering::Relaxed);
        });

        let transport =
            Arc::new(UdsRuleTransport::new(&sock).with_io_timeout(Duration::from_secs(2)));
        let client =
            RemoteRuleClient::new("bip360", transport).with_timeout(Duration::from_secs(2));
        client.handshake().unwrap();

        // Fail-closed: missing tx_hex → Reject
        let ballot = client.validate_tx(&tx_ctx());
        assert!(!ballot.consents());
        match &ballot.source {
            super::super::VoteSource::Explicit(RuleVote::Reject { reason }) => {
                assert!(reason.contains("missing_tx"));
            }
            other => panic!("expected explicit missing_tx reject, got {other:?}"),
        }

        // With tx → Accept
        let tx = sample_tx();
        let ctx = RuleTxContext::with_tx(1, "t", &tx);
        assert!(client.validate_tx(&ctx).consents());

        server.join().unwrap();
        assert!(stop.load(Ordering::Relaxed));
    }

    #[test]
    fn uds_unreachable_is_failure_ballot() {
        let transport = Arc::new(UdsRuleTransport::new(
            "/tmp/cusf-rules-no-such-socket-xyz.sock",
        ));
        let client =
            RemoteRuleClient::new("drivechain", transport).with_timeout(Duration::from_millis(200));
        let ballot = client.validate_tx(&tx_ctx());
        assert!(!ballot.consents());
        assert!(matches!(
            ballot.source,
            super::super::VoteSource::Failure { .. }
        ));
    }

    /// Shared freeform / dynamic RuleId residual matrix (mapping + CLI parse locks).
    /// Keep vectors identical so residual unlock cannot green one layer only.
    const FREEFORM_RULE_IDS: &[&str] = &[
        "custom",
        "my-rule",
        "Drivechain",
        "BIP360",
        "drivechain-extra",
        "bip360-v2",
        "rules-worker",
        "",
        " ",
        "drivechain bip360",
        "sidechain",
        "plugin-42",
    ];

    fn assert_unknown_rule_id_err(name: &str, err: &str) {
        assert_eq!(
            err,
            format!("unknown rules worker id `{name}` (supported: drivechain, bip360)"),
            "freeform/padded `{name}` must fail with exact residual mapping error"
        );
    }

    /// Positive bip360 round-trip only. Freeform/malformed negatives live in
    /// dedicated residual locks below (single authoritative freeform surface).
    #[test]
    fn rules_worker_endpoint_parse() {
        let ep = RulesWorkerEndpoint::parse("bip360=/tmp/x.sock").unwrap();
        assert_eq!(ep.rule_id, "bip360");
        assert_eq!(ep.path, PathBuf::from("/tmp/x.sock"));
    }

    /// Dynamic `RuleId` residual lock: only compile-time known ids map.
    /// Freeform / dynamic names must fail mapping (no String RuleId unlock).
    #[test]
    fn static_rule_id_allowlist_only_drivechain_bip360() {
        assert_eq!(static_rule_id("drivechain").unwrap(), "drivechain");
        assert_eq!(static_rule_id("bip360").unwrap(), "bip360");

        for freeform in FREEFORM_RULE_IDS {
            let err = static_rule_id(freeform).expect_err(freeform);
            assert_unknown_rule_id_err(freeform, &err);
        }

        // Exact-match only at map layer (no trim); CLI trim is separate.
        for padded in [" drivechain", "drivechain ", "\tbip360", "bip360\n"] {
            let err = static_rule_id(padded).expect_err(padded);
            assert_unknown_rule_id_err(padded, &err);
        }
    }

    /// CLI `RULE_ID=PATH` has no freeform registration path: unknown ids Err at parse.
    #[test]
    fn rules_worker_endpoint_rejects_freeform_dynamic_rule_ids() {
        let ok_dc = RulesWorkerEndpoint::parse("drivechain=/tmp/dc.sock").unwrap();
        assert_eq!(ok_dc.rule_id, "drivechain");
        assert_eq!(ok_dc.path, PathBuf::from("/tmp/dc.sock"));
        let ok_360 = RulesWorkerEndpoint::parse("bip360=/tmp/b.sock").unwrap();
        assert_eq!(ok_360.rule_id, "bip360");
        assert_eq!(ok_360.path, PathBuf::from("/tmp/b.sock"));

        // Whitespace around allowlisted id still maps (trim) — not freeform.
        let trimmed_360 = RulesWorkerEndpoint::parse("  bip360  =/tmp/t.sock").unwrap();
        assert_eq!(trimmed_360.rule_id, "bip360");
        assert_eq!(trimmed_360.path, PathBuf::from("/tmp/t.sock"));
        let trimmed_dc = RulesWorkerEndpoint::parse("  drivechain  =/tmp/dc.sock").unwrap();
        assert_eq!(trimmed_dc.rule_id, "drivechain");
        assert_eq!(trimmed_dc.path, PathBuf::from("/tmp/dc.sock"));

        // Freeform ids — shared matrix, exact residual mapping error after trim.
        for id in FREEFORM_RULE_IDS {
            let s = format!("{id}=/tmp/x.sock");
            let err = RulesWorkerEndpoint::parse(&s).expect_err(&s);
            // parse trims the id segment before static_rule_id.
            assert_unknown_rule_id_err(id.trim(), &err);
        }

        // Format failures (no `=`).
        for bad in ["nope", "drivechain", "bip360"] {
            let err = RulesWorkerEndpoint::parse(bad).expect_err(bad);
            assert!(
                err.contains("expected RULE_ID=PATH"),
                "format fail `{bad}`, got: {err}"
            );
        }

        // Empty path after trim (allowlisted id still present).
        for bad in ["bip360=", "drivechain=", "bip360=   ", "drivechain=\t"] {
            let err = RulesWorkerEndpoint::parse(bad).expect_err(bad);
            assert!(
                err.contains("path must not be empty"),
                "empty path `{bad}`, got: {err}"
            );
        }
    }

    #[test]
    fn parent_txs_hex_roundtrip_and_request() {
        let parent = sample_tx();
        let txid = parent.compute_txid();
        let mut map = std::collections::HashMap::new();
        map.insert(txid, parent.clone());
        let wire = encode_parent_txs_hex(&map);
        let back = decode_parent_txs_hex(&wire).unwrap();
        assert_eq!(back.get(&txid).unwrap().compute_txid(), txid);

        let child = sample_tx();
        let ctx = RuleTxContext::with_tx_and_parents(1, "lab", &child, &map);
        match request_validate_tx(&ctx) {
            RulesV1Request::ValidateTx {
                parent_txs_hex: Some(p),
                tx_hex: Some(_),
                ..
            } => {
                assert!(p.contains_key(&txid.to_string()));
            }
            other => panic!("expected parents on wire, got {other:?}"),
        }
    }

    #[test]
    fn json_roundtrip_handshake_and_error() {
        let req = RulesV1Request::Handshake {
            version: RULES_V1_VERSION,
            rule_id: "drivechain".into(),
        };
        assert_eq!(decode_request(&encode_request(&req).unwrap()).unwrap(), req);

        let err = RulesV1Response::Error {
            detail: "worker boom".into(),
        };
        assert_eq!(
            decode_response(&encode_response(&err).unwrap()).unwrap(),
            err
        );

        let health = RulesV1Request::Health;
        assert_eq!(
            decode_request(&encode_request(&health).unwrap()).unwrap(),
            health
        );
    }

    #[test]
    fn rules_v1_error_response_maps_to_failure_ballot() {
        let mock = Arc::new(MockTransport::new());
        mock.push(Ok(RulesV1Response::Error {
            detail: "worker internal error".into(),
        }));
        let remote = RemoteRuleClient::new("bip360", mock);
        let ballot = remote.validate_tx(&tx_ctx());
        assert!(!ballot.consents());
        assert!(matches!(
            ballot.source,
            VoteSource::Failure { ref detail } if detail.contains("worker internal error")
        ));
    }

    #[test]
    fn request_validate_tx_includes_hex_when_present() {
        let tx = sample_tx();
        let ctx = RuleTxContext::with_tx(1, "lab", &tx);
        match request_validate_tx(&ctx) {
            RulesV1Request::ValidateTx {
                tx_hex: Some(h), ..
            } => {
                assert_eq!(h, encode_tx_hex(&tx));
                assert!(decode_tx_hex(&h).is_ok());
            }
            other => panic!("expected ValidateTx with hex, got {other:?}"),
        }
    }

    #[test]
    fn remote_sends_tx_hex_when_ctx_has_tx() {
        let tx = sample_tx();
        let hex = encode_tx_hex(&tx);
        struct Capture {
            last: Mutex<Option<RulesV1Request>>,
        }
        impl RuleTransport for Capture {
            fn call(&self, request: RulesV1Request) -> Result<RulesV1Response, TransportError> {
                *self.last.lock().unwrap() = Some(request);
                Ok(RulesV1Response::accept())
            }
        }
        let cap = Arc::new(Capture {
            last: Mutex::new(None),
        });
        let client = RemoteRuleClient::new("bip360", cap.clone());
        let ctx = RuleTxContext::with_tx(3, "x", &tx);
        assert!(client.validate_tx(&ctx).consents());
        let captured = cap.last.lock().unwrap().clone();
        match captured {
            Some(RulesV1Request::ValidateTx {
                tx_hex: Some(h),
                height: 3,
                ..
            }) => assert_eq!(h, hex),
            other => panic!("expected ValidateTx with hex, got {other:?}"),
        }
    }

    #[test]
    fn remote_sends_block_hex_when_ctx_has_block() {
        use bitcoin::{Network, blockdata::constants::genesis_block};
        let block = genesis_block(Network::Regtest);
        let hex = encode_block_hex(&block);
        struct Capture {
            last: Mutex<Option<RulesV1Request>>,
        }
        impl RuleTransport for Capture {
            fn call(&self, request: RulesV1Request) -> Result<RulesV1Response, TransportError> {
                *self.last.lock().unwrap() = Some(request);
                Ok(RulesV1Response::accept())
            }
        }
        let cap = Arc::new(Capture {
            last: Mutex::new(None),
        });
        let client = RemoteRuleClient::new("drivechain", cap.clone());
        let ctx = RuleBlockContext::with_block(0, "b", &block);
        assert!(client.connect_block(&ctx).consents());
        let captured = cap.last.lock().unwrap().clone();
        match captured {
            Some(RulesV1Request::ConnectBlock {
                block_hex: Some(h),
                height: 0,
                ..
            }) => {
                assert_eq!(h, hex);
                assert!(decode_block_hex(&h).is_ok());
            }
            other => panic!("expected ConnectBlock with block_hex, got {other:?}"),
        }
    }

    #[test]
    fn remote_failure_rejects_backend_engine() {
        let mock = Arc::new(MockTransport::new());
        mock.push_failure("worker crashed");
        let remote = RemoteRuleClient::new("bip360", mock);

        let mut engine = BackendEngine::new();
        engine.register(RuleBackend::Local(Box::new(AlwaysAccept {
            id: "drivechain",
        })));
        engine.register(RuleBackend::Remote(remote));

        match engine.validate_tx(&tx_ctx()) {
            super::super::AggregateDecision::Reject { rejections } => {
                assert_eq!(rejections.len(), 1);
                assert_eq!(rejections[0].rule_id, "bip360");
                assert!(matches!(rejections[0].source, VoteSource::Failure { .. }));
            }
            super::super::AggregateDecision::Accept => panic!("failure must reject"),
        }
    }

    #[test]
    fn remote_timeout_structured_rejects() {
        let mock = Arc::new(MockTransport::new());
        mock.push_timeout();
        let remote = RemoteRuleClient::new("bip360", mock).with_timeout(Duration::from_millis(50));

        let ballot = remote.validate_tx(&tx_ctx());
        assert!(!ballot.consents());
        assert!(matches!(ballot.source, VoteSource::Timeout));
        assert!(!RuleEngine::decide(&[ballot]).is_accept());
    }

    #[test]
    fn remote_deadline_enforced_on_slow_transport() {
        struct Slow;
        impl RuleTransport for Slow {
            fn call(&self, _: RulesV1Request) -> Result<RulesV1Response, TransportError> {
                thread::sleep(Duration::from_millis(200));
                Ok(RulesV1Response::accept())
            }
        }
        let remote =
            RemoteRuleClient::new("bip360", Arc::new(Slow)).with_timeout(Duration::from_millis(30));
        let ballot = remote.validate_tx(&tx_ctx());
        assert!(matches!(ballot.source, VoteSource::Timeout));
        assert!(!ballot.consents());
    }

    /// Timeout path calls [`RuleTransport::abort_inflight`] so mid-read can
    /// fail closed quickly (UDS shutdown) without waiting only for io_timeout.
    #[test]
    fn remote_timeout_invokes_abort_inflight() {
        struct SlowAbortable {
            aborted: AtomicBool,
        }
        impl RuleTransport for SlowAbortable {
            fn call(&self, _: RulesV1Request) -> Result<RulesV1Response, TransportError> {
                // Block until abort_inflight or overall safety deadline.
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                while !self.aborted.load(Ordering::Relaxed) {
                    if std::time::Instant::now() > deadline {
                        break;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                if self.aborted.load(Ordering::Relaxed) {
                    Err(TransportError::Timeout)
                } else {
                    Ok(RulesV1Response::accept())
                }
            }
            fn abort_inflight(&self) {
                self.aborted.store(true, Ordering::Relaxed);
            }
        }
        let transport = Arc::new(SlowAbortable {
            aborted: AtomicBool::new(false),
        });
        let remote = RemoteRuleClient::new("bip360", transport.clone())
            .with_timeout(Duration::from_millis(40));
        let ballot = remote.validate_tx(&tx_ctx());
        assert!(matches!(ballot.source, VoteSource::Timeout));
        assert!(!ballot.consents());
        assert!(
            transport.aborted.load(Ordering::Relaxed),
            "timeout must call abort_inflight"
        );
    }

    /// Live UDS: hub join-timeout + abort_inflight unblocks mid-read well under
    /// a long socket `io_timeout` (fail-closed Timeout immediately on ballot).
    #[test]
    fn uds_abort_inflight_unblocks_mid_read() {
        use temp_dir::TempDir;
        let dir = TempDir::new().unwrap();
        let sock = dir.child("abort.sock");
        let listener = bind_uds(&sock).unwrap();
        // Peer accepts, holds until peer disconnect/abort (no long fixed sleep).
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            // Block on a second read until abort/EOF closes the peer.
            let mut buf = [0u8; 1];
            drop((&stream).read(&mut buf));
        });
        let transport =
            Arc::new(UdsRuleTransport::new(&sock).with_io_timeout(Duration::from_secs(30)));
        let client =
            RemoteRuleClient::new("bip360", transport).with_timeout(Duration::from_millis(80));
        let started = std::time::Instant::now();
        let ballot = client.validate_tx(&tx_ctx());
        let elapsed = started.elapsed();
        assert!(
            matches!(
                ballot.source,
                VoteSource::Timeout | VoteSource::Failure { .. }
            ),
            "hung peer must fail-closed, got {:?}",
            ballot.source
        );
        assert!(!ballot.consents());
        // Join deadline ~80ms + abort; must not wait the 30s io_timeout.
        assert!(
            elapsed < Duration::from_millis(1500),
            "abort path took too long: {elapsed:?}"
        );
        // Server should unblock promptly once peer aborts/drops.
        let join = server.join();
        assert!(join.is_ok(), "server thread panicked");
    }

    /// Orphaned call's clear_active_if must not wipe a newer call's abort handle.
    #[test]
    fn uds_active_generation_scoped_clear() {
        use temp_dir::TempDir;
        let dir = TempDir::new().unwrap();
        let sock = dir.child("gen.sock");
        // Dummy listener so connect succeeds; accept + drop immediately.
        let listener = bind_uds(&sock).unwrap();
        let _accept = thread::spawn(move || {
            for _ in 0..4 {
                if let Ok((s, _)) = listener.accept() {
                    drop(s);
                }
            }
        });
        let t = UdsRuleTransport::new(&sock).with_io_timeout(Duration::from_millis(50));
        // Simulate two sequential installs without going through full call.
        let s1 = UnixStream::connect(&sock).unwrap();
        let g1 = t.install_active(s1.try_clone().unwrap());
        assert_eq!(t.active_generation(), Some(g1));
        let s2 = UnixStream::connect(&sock).unwrap();
        let g2 = t.install_active(s2.try_clone().unwrap());
        assert_eq!(t.active_generation(), Some(g2));
        assert_ne!(g1, g2);
        // Orphan clear of g1 must leave g2 intact.
        t.clear_active_if(g1);
        assert_eq!(
            t.active_generation(),
            Some(g2),
            "clear of older generation must not wipe newer active"
        );
        // abort takes current (g2) only.
        t.abort_inflight();
        assert_eq!(t.active_generation(), None);
        // clear g2 after abort is a no-op.
        t.clear_active_if(g2);
        assert_eq!(t.active_generation(), None);
        drop(s1);
        drop(s2);
    }

    /// Concurrent `call` is serialized on `call_lock`: a second caller waits
    /// (cannot overwrite the active abort handle). Abort still targets the
    /// single in-flight call and unblocks it; the waiter then proceeds.
    ///
    /// Hang peer mirrors [`uds_abort_inflight_unblocks_mid_read`]: read-until
    /// error/EOF (no timed sleep/drop). Timing bound after `abort_inflight`
    /// proves abort unblocked mid-read (not a peer timeout).
    #[test]
    fn uds_call_serialized_second_waits_abort_targets_inflight() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering};

        use temp_dir::TempDir;

        let dir = TempDir::new().unwrap();
        let sock = dir.child("serial.sock");
        let listener = bind_uds(&sock).unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let accepted_s = Arc::clone(&accepted);

        // Conn 1: hang without responding (client mid-read until abort).
        // Conn 2: framed Accept once the waiter proceeds after lock release.
        // `release_hang` only drops conn1 *after* abort unblocked first — never
        // a timed peer drop that could make first_res.is_err() without abort.
        let release_hang = Arc::new(AtomicBool::new(false));
        let release_hang_s = Arc::clone(&release_hang);
        let server = thread::spawn(move || {
            let (stream1, _) = listener.accept().unwrap();
            accepted_s.fetch_add(1, AtomicOrdering::SeqCst);
            // Side thread: drain request (read-until-EOF on client half-close),
            // then hold fd open (no response, no timed drop) so client mid-read
            // stays blocked until abort_inflight. Mirrors
            // uds_abort_inflight_unblocks_mid_read hang intent.
            let hang = thread::spawn(move || {
                let mut buf = [0u8; 4096];
                loop {
                    match (&stream1).read(&mut buf) {
                        Ok(0) => break, // client half-closed write after frame
                        Ok(_) => continue,
                        Err(_) => return, // peer abort/reset during drain
                    }
                }
                // Hold open until main releases after abort + first.join.
                while !release_hang_s.load(AtomicOrdering::SeqCst) {
                    thread::sleep(Duration::from_millis(5));
                }
                drop(stream1);
            });

            let (stream, _) = listener.accept().unwrap();
            accepted_s.fetch_add(1, AtomicOrdering::SeqCst);
            let handler = |req: RulesV1Request| match req {
                RulesV1Request::ValidateTx { .. } => RulesV1Response::accept(),
                RulesV1Request::Handshake { version, rule_id } => RulesV1Response::HandshakeOk {
                    version,
                    rule_id,
                    validation: Some(VALIDATION_BIP360_PARENTS_AND_CHAIN_UTXOS.into()),
                },
                RulesV1Request::Health => RulesV1Response::HealthOk {
                    rule_id: "bip360".into(),
                    validation: Some(VALIDATION_BIP360_PARENTS_AND_CHAIN_UTXOS.into()),
                },
                RulesV1Request::ConnectBlock { .. } => RulesV1Response::accept(),
            };
            serve_framed_connection(stream, handler).unwrap();
            hang.join().expect("hang peer");
        });

        // Long io_timeout so a no-op abort would hang past the join bound below.
        let transport =
            Arc::new(UdsRuleTransport::new(&sock).with_io_timeout(Duration::from_secs(30)));

        let t1 = Arc::clone(&transport);
        let first = thread::spawn(move || {
            t1.call(RulesV1Request::ValidateTx {
                height: 1,
                label: "t".into(),
                tx_hex: None,
                parent_txs_hex: None,
                drivechain_state: None,
            })
        });

        // Wait until first call holds lock and has dialed.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if accepted.load(AtomicOrdering::SeqCst) >= 1 && !transport.try_call_lock() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            !transport.try_call_lock(),
            "first in-flight call must hold call_lock"
        );
        assert!(
            transport.active_generation().is_some(),
            "first call must install active abort handle"
        );
        assert_eq!(accepted.load(AtomicOrdering::SeqCst), 1);

        let t2 = Arc::clone(&transport);
        let second = thread::spawn(move || {
            t2.call(RulesV1Request::ValidateTx {
                height: 1,
                label: "t".into(),
                tx_hex: None,
                parent_txs_hex: None,
                drivechain_state: None,
            })
        });

        // Second call must not dial while first holds the call mutex.
        thread::sleep(Duration::from_millis(80));
        assert_eq!(
            accepted.load(AtomicOrdering::SeqCst),
            1,
            "second call must wait on call_lock (no second dial yet)"
        );
        assert!(
            !transport.try_call_lock(),
            "call_lock still held by first call"
        );

        // Abort single in-flight (first); must not require call_lock.
        // Bound proves abort unblocked mid-read (peer never timed-drops).
        let abort_started = std::time::Instant::now();
        transport.abort_inflight();

        let first_res = first.join().expect("first call thread");
        let abort_elapsed = abort_started.elapsed();
        assert!(
            first_res.is_err(),
            "aborted first call must fail, got {first_res:?}"
        );
        assert!(
            abort_elapsed < Duration::from_millis(1500),
            "abort_inflight must unblock first mid-read quickly, took {abort_elapsed:?}"
        );
        // Safe to drop hang peer only after abort has already unblocked first.
        release_hang.store(true, AtomicOrdering::SeqCst);

        let second_res = second.join().expect("second call thread");
        assert!(
            second_res.is_ok(),
            "waiter proceeds after first releases lock, got {second_res:?}"
        );
        assert_eq!(
            accepted.load(AtomicOrdering::SeqCst),
            2,
            "second call dials only after first finishes"
        );
        assert!(
            transport.try_call_lock(),
            "call_lock free after both calls complete"
        );
        assert_eq!(transport.active_generation(), None);

        server.join().expect("server");
    }

    #[test]
    fn local_x2_and_accept() {
        let mut engine = BackendEngine::new();
        engine.register(RuleBackend::Local(Box::new(AlwaysAccept { id: "a" })));
        engine.register(RuleBackend::Local(Box::new(AlwaysAccept { id: "b" })));
        assert!(engine.validate_tx(&tx_ctx()).is_accept());
    }

    #[test]
    fn handshake_ok() {
        let mock = Arc::new(MockTransport::always_accept());
        let client = RemoteRuleClient::new("drivechain", mock);
        client.handshake().unwrap();
        assert_eq!(client.rule_id(), "drivechain");
    }

    #[test]
    fn handshake_version_mismatch_fails() {
        let mock = Arc::new(MockTransport::new());
        mock.push(Ok(RulesV1Response::HandshakeOk {
            version: RULES_V1_VERSION + 1,
            rule_id: "drivechain".into(),
            validation: Some(VALIDATION_STUB_NOT_IMPLEMENTED.into()),
        }));
        let client = RemoteRuleClient::new("drivechain", mock);
        let err = client.handshake().unwrap_err();
        assert!(err.contains("version skew") || err.contains("version"));
    }

    #[test]
    fn handshake_rule_id_mismatch_fails() {
        let mock = Arc::new(MockTransport::new());
        mock.push(Ok(RulesV1Response::HandshakeOk {
            version: RULES_V1_VERSION,
            rule_id: "bip360".into(),
            validation: Some(VALIDATION_BIP360_PARENTS_AND_CHAIN_UTXOS.into()),
        }));
        let client = RemoteRuleClient::new("drivechain", mock);
        let err = client.handshake().unwrap_err();
        assert!(err.contains("rule id mismatch"));
    }

    #[test]
    fn is_real_validation_capability_policy() {
        assert!(!is_real_validation_capability(None));
        assert!(!is_real_validation_capability(Some("")));
        assert!(!is_real_validation_capability(Some(
            VALIDATION_STUB_NOT_IMPLEMENTED
        )));
        assert!(!is_real_validation_capability(Some("stub-foo")));
        // Spoof / unknown tokens are NOT real (allowlist only).
        assert!(!is_real_validation_capability(Some("mock-real")));
        assert!(!is_real_validation_capability(Some("accept-all")));
        assert!(is_real_validation_capability(Some(
            VALIDATION_BIP360_PARENTS_AND_CHAIN_UTXOS
        )));
        assert!(is_real_validation_capability(Some(
            VALIDATION_DRIVECHAIN_CTIP_M8_PENDING_M6_CHECKS_ON_WIRE
        )));
        assert!(
            REAL_VALIDATION_CAPABILITIES
                .contains(&VALIDATION_DRIVECHAIN_CTIP_M8_PENDING_M6_CHECKS_ON_WIRE)
        );
        // Legacy tokens must not register for consent.
        assert!(!is_real_validation_capability(Some(
            VALIDATION_DRIVECHAIN_CTIP_M8_CHECKS_LEGACY
        )));
        assert!(!is_real_validation_capability(Some(
            VALIDATION_DRIVECHAIN_M8_TIP_CHECK_LEGACY
        )));
        assert!(!is_real_validation_capability(Some(
            VALIDATION_DRIVECHAIN_TIP_AND_SIDECHAINS_LEGACY
        )));
        assert!(!should_register_remote_for_consent(&Ok(Some(
            VALIDATION_DRIVECHAIN_TIP_AND_SIDECHAINS_LEGACY.into()
        ))));
        assert!(!should_register_remote_for_consent(&Ok(Some(
            VALIDATION_DRIVECHAIN_M8_TIP_CHECK_LEGACY.into()
        ))));
        assert!(!should_register_remote_for_consent(&Ok(Some(
            VALIDATION_DRIVECHAIN_CTIP_M8_CHECKS_LEGACY.into()
        ))));
    }

    #[test]
    fn should_register_remote_for_consent_matrix() {
        assert!(!should_register_remote_for_consent(&Err(
            "connect refused".into()
        )));
        assert!(!should_register_remote_for_consent(&Ok(None)));
        assert!(!should_register_remote_for_consent(&Ok(Some(
            VALIDATION_STUB_NOT_IMPLEMENTED.into()
        ))));
        assert!(!should_register_remote_for_consent(&Ok(Some(
            "spoof-capability".into()
        ))));
        assert!(should_register_remote_for_consent(&Ok(Some(
            VALIDATION_BIP360_PARENTS_AND_CHAIN_UTXOS.into()
        ))));
        assert!(should_register_remote_for_consent(&Ok(Some(
            VALIDATION_DRIVECHAIN_CTIP_M8_PENDING_M6_CHECKS_ON_WIRE.into()
        ))));
    }

    #[test]
    fn build_remote_backend_engine_skips_unreachable() {
        let missing = RulesWorkerEndpoint {
            rule_id: "bip360",
            path: PathBuf::from("/tmp/cusf-rules-no-such-sock-b2e2-unreachable.sock"),
        };
        let engine = build_remote_backend_engine(&[missing], Duration::from_millis(50));
        assert!(
            engine.is_empty(),
            "unreachable must not register (got {:?})",
            engine.registered_ids()
        );
    }

    #[test]
    fn build_remote_backend_engine_skips_stub_live() {
        use std::sync::atomic::{AtomicBool, Ordering};

        use temp_dir::TempDir;

        let dir = TempDir::new().unwrap();
        let sock = dir.child("stub.sock");
        let listener = bind_uds(&sock).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = Arc::clone(&stop);
        let server = thread::spawn(move || {
            serve_uds_loop(
                &listener,
                |req| match req {
                    RulesV1Request::Handshake { version, rule_id } => {
                        RulesV1Response::HandshakeOk {
                            version,
                            rule_id,
                            validation: Some(VALIDATION_STUB_NOT_IMPLEMENTED.into()),
                        }
                    }
                    RulesV1Request::Health => RulesV1Response::HealthOk {
                        rule_id: "drivechain".into(),
                        validation: Some(VALIDATION_STUB_NOT_IMPLEMENTED.into()),
                    },
                    _ => RulesV1Response::reject("unexpected"),
                },
                Some(&stop_t),
            )
        });
        // Socket path exists after bind; brief settle for accept loop.
        thread::sleep(Duration::from_millis(50));
        let ep = RulesWorkerEndpoint {
            rule_id: "drivechain",
            path: sock,
        };
        let engine = build_remote_backend_engine(&[ep], Duration::from_millis(500));
        assert!(
            engine.is_empty(),
            "stub drivechain must not register, got {:?}",
            engine.registered_ids()
        );
        stop.store(true, Ordering::Relaxed);
        drop(server.join());
    }

    #[test]
    fn build_remote_backend_engine_registers_allowlisted() {
        use std::sync::atomic::{AtomicBool, Ordering};

        use temp_dir::TempDir;

        let dir = TempDir::new().unwrap();
        let sock = dir.child("real.sock");
        let listener = bind_uds(&sock).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = Arc::clone(&stop);
        let server = thread::spawn(move || {
            serve_uds_loop(
                &listener,
                |req| match req {
                    RulesV1Request::Handshake { version, rule_id } => {
                        RulesV1Response::HandshakeOk {
                            version,
                            rule_id,
                            validation: Some(VALIDATION_BIP360_PARENTS_AND_CHAIN_UTXOS.into()),
                        }
                    }
                    _ => RulesV1Response::reject("unexpected"),
                },
                Some(&stop_t),
            )
        });
        thread::sleep(Duration::from_millis(50));
        let ep = RulesWorkerEndpoint {
            rule_id: "bip360",
            path: sock,
        };
        let engine = build_remote_backend_engine(&[ep], Duration::from_millis(500));
        assert_eq!(engine.registered_ids(), vec!["bip360"]);
        stop.store(true, Ordering::Relaxed);
        drop(server.join());
    }

    #[test]
    fn build_remote_backend_engine_skips_legacy_drivechain_capability_live() {
        use std::sync::atomic::{AtomicBool, Ordering};

        use temp_dir::TempDir;

        // All demoted drivechain tokens must fail live UDS handshake registration
        // (tip-only presence, M8-tip-only, ctip-m8 without pending M6).
        for (label, token) in [
            (
                "tip-and-sidechains",
                VALIDATION_DRIVECHAIN_TIP_AND_SIDECHAINS_LEGACY,
            ),
            ("m8-tip-only", VALIDATION_DRIVECHAIN_M8_TIP_CHECK_LEGACY),
            (
                "ctip-m8-no-pending",
                VALIDATION_DRIVECHAIN_CTIP_M8_CHECKS_LEGACY,
            ),
        ] {
            let dir = TempDir::new().unwrap();
            let sock = dir.child(format!("dc-legacy-{label}.sock"));
            let listener = bind_uds(&sock).unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let stop_t = Arc::clone(&stop);
            let token_s = token.to_string();
            let server = thread::spawn(move || {
                serve_uds_loop(
                    &listener,
                    move |req| match req {
                        RulesV1Request::Handshake { version, rule_id } => {
                            RulesV1Response::HandshakeOk {
                                version,
                                rule_id,
                                validation: Some(token_s.clone()),
                            }
                        }
                        RulesV1Request::Health => RulesV1Response::HealthOk {
                            rule_id: "drivechain".into(),
                            validation: Some(token_s.clone()),
                        },
                        _ => RulesV1Response::reject("unexpected"),
                    },
                    Some(&stop_t),
                )
            });
            thread::sleep(Duration::from_millis(50));
            let ep = RulesWorkerEndpoint {
                rule_id: "drivechain",
                path: sock,
            };
            let engine = build_remote_backend_engine(&[ep], Duration::from_millis(500));
            assert!(
                engine.is_empty(),
                "legacy drivechain capability {label} ({token}) must not register, got {:?}",
                engine.registered_ids()
            );
            stop.store(true, Ordering::Relaxed);
            drop(server.join());
        }
    }

    #[test]
    fn build_remote_backend_engine_registers_live_drivechain() {
        use std::sync::atomic::{AtomicBool, Ordering};

        use temp_dir::TempDir;

        let dir = TempDir::new().unwrap();
        let sock = dir.child("dc-real.sock");
        let listener = bind_uds(&sock).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = Arc::clone(&stop);
        let server = thread::spawn(move || {
            serve_uds_loop(
                &listener,
                |req| match req {
                    RulesV1Request::Handshake { version, rule_id } => {
                        RulesV1Response::HandshakeOk {
                            version,
                            rule_id,
                            validation: Some(
                                VALIDATION_DRIVECHAIN_CTIP_M8_PENDING_M6_CHECKS_ON_WIRE.into(),
                            ),
                        }
                    }
                    _ => RulesV1Response::reject("unexpected"),
                },
                Some(&stop_t),
            )
        });
        thread::sleep(Duration::from_millis(50));
        let ep = RulesWorkerEndpoint {
            rule_id: "drivechain",
            path: sock,
        };
        let engine = build_remote_backend_engine(&[ep], Duration::from_millis(500));
        assert_eq!(engine.registered_ids(), vec!["drivechain"]);
        stop.store(true, Ordering::Relaxed);
        drop(server.join());
    }

    #[test]
    fn remote_sends_drivechain_state_when_ctx_has_it() {
        use crate::validator::rules::DrivechainCtipSnapshot;
        let state = DrivechainStateSnapshot {
            tip_height: 7,
            tip_hash_hex: "ab".repeat(32),
            active_sidechain_numbers: vec![1, 2],
            ctips: vec![DrivechainCtipSnapshot {
                sidechain_number: 1,
                outpoint_txid_hex: "11".repeat(32),
                outpoint_vout: 0,
                value_sats: 9_000,
            }],
            pending_m6ids: vec![],
            withdrawal_bundle_inclusion_threshold: 5,
        };
        struct Capture {
            last: Mutex<Option<RulesV1Request>>,
        }
        impl RuleTransport for Capture {
            fn call(&self, request: RulesV1Request) -> Result<RulesV1Response, TransportError> {
                *self.last.lock().unwrap() = Some(request);
                Ok(RulesV1Response::accept())
            }
        }
        let cap = Arc::new(Capture {
            last: Mutex::new(None),
        });
        let client = RemoteRuleClient::new("drivechain", cap.clone());
        let tx = sample_tx();
        let ctx = RuleTxContext::with_tx(7, "lab", &tx).with_drivechain_state(&state);
        assert!(client.validate_tx(&ctx).consents());
        let captured = cap.last.lock().unwrap().clone();
        match captured {
            Some(RulesV1Request::ValidateTx {
                drivechain_state: Some(s),
                ..
            }) => {
                assert_eq!(s.tip_height, 7);
                assert_eq!(s.active_sidechain_numbers, vec![1, 2]);
                assert_eq!(s.ctips.len(), 1);
                assert_eq!(s.ctips[0].sidechain_number, 1);
                assert_eq!(s.ctips[0].value_sats, 9_000);
            }
            other => panic!("expected drivechain_state on wire, got {other:?}"),
        }
    }

    #[test]
    fn remote_sends_drivechain_state_on_connect_block() {
        use bitcoin::{Network, blockdata::constants::genesis_block};
        let state = DrivechainStateSnapshot {
            tip_height: 0,
            tip_hash_hex: "cd".repeat(32),
            active_sidechain_numbers: vec![3],
            ctips: vec![],
            pending_m6ids: vec![],
            withdrawal_bundle_inclusion_threshold: 0,
        };
        struct Capture {
            last: Mutex<Option<RulesV1Request>>,
        }
        impl RuleTransport for Capture {
            fn call(&self, request: RulesV1Request) -> Result<RulesV1Response, TransportError> {
                *self.last.lock().unwrap() = Some(request);
                Ok(RulesV1Response::accept())
            }
        }
        let cap = Arc::new(Capture {
            last: Mutex::new(None),
        });
        let client = RemoteRuleClient::new("drivechain", cap.clone());
        let block = genesis_block(Network::Regtest);
        let ctx = RuleBlockContext::with_block(0, "lab", &block).with_drivechain_state(&state);
        assert!(client.connect_block(&ctx).consents());
        let captured = cap.last.lock().unwrap().clone();
        match captured {
            Some(RulesV1Request::ConnectBlock {
                drivechain_state: Some(s),
                block_hex: Some(_),
                ..
            }) => {
                assert_eq!(s.tip_height, 0);
                assert_eq!(s.active_sidechain_numbers, vec![3]);
            }
            other => panic!("expected ConnectBlock drivechain_state on wire, got {other:?}"),
        }
    }

    #[test]
    fn remote_timeout_sets_cancel_and_eager_reaper() {
        struct Slow;
        impl RuleTransport for Slow {
            fn call(&self, _: RulesV1Request) -> Result<RulesV1Response, TransportError> {
                thread::sleep(Duration::from_millis(400));
                Ok(RulesV1Response::accept())
            }
        }
        let remote =
            RemoteRuleClient::new("bip360", Arc::new(Slow)).with_timeout(Duration::from_millis(40));
        let ballot = remote.validate_tx(&tx_ctx());
        assert!(matches!(ballot.source, VoteSource::Timeout));
        assert!(!ballot.consents());
        // Best-effort cancel status is set on timeout (generation-scoped).
        assert!(
            remote.cancel_requested(),
            "cancel_requested must be true immediately after timeout"
        );
        // Second call must return near the join deadline — not after the full
        // 400ms helper sleep (eager reaper; kick_orphan_reaper never blocks).
        let t0 = std::time::Instant::now();
        let ballot2 = remote.validate_tx(&tx_ctx());
        let elapsed = t0.elapsed();
        assert!(matches!(ballot2.source, VoteSource::Timeout));
        assert!(
            remote.cancel_requested(),
            "cancel still true while latest timeout generation is in flight"
        );
        assert!(
            elapsed < Duration::from_millis(200),
            "next call blocked too long on orphan reaper: {elapsed:?}"
        );
    }

    /// Generation-scoped cancel: a finished reaper for an older generation must
    /// not clear cancel while a newer timeout generation is still open.
    #[test]
    fn cancel_generation_scoped_clear_does_not_drop_newer() {
        struct SlowMs {
            ms: u64,
        }
        impl RuleTransport for SlowMs {
            fn call(&self, _: RulesV1Request) -> Result<RulesV1Response, TransportError> {
                thread::sleep(Duration::from_millis(self.ms));
                Ok(RulesV1Response::accept())
            }
        }
        // First timeout (long helper).
        let remote = RemoteRuleClient::new("bip360", Arc::new(SlowMs { ms: 500 }))
            .with_timeout(Duration::from_millis(30));
        assert!(matches!(
            remote.validate_tx(&tx_ctx()).source,
            VoteSource::Timeout
        ));
        assert!(remote.cancel_requested());
        // Immediate second timeout while first orphan may still be reaping.
        assert!(matches!(
            remote.validate_tx(&tx_ctx()).source,
            VoteSource::Timeout
        ));
        assert!(
            remote.cancel_requested(),
            "newer timeout generation must keep cancel_requested true"
        );
        // After both helpers finish, reaper for latest gen should clear.
        thread::sleep(Duration::from_millis(700));
        assert!(
            !remote.cancel_requested(),
            "cancel should clear after latest generation reaper joins"
        );
    }

    #[test]
    fn decode_chain_p2mr_utxos_hex_rejects_bad_key_and_body() {
        let mut map = BTreeMap::new();
        map.insert("not-an-outpoint".into(), "00".into());
        let err = decode_chain_p2mr_utxos_hex(&map).unwrap_err();
        assert!(
            err.contains("outpoint") || err.contains("txid:vout") || err.contains("key"),
            "got {err}"
        );

        let mut map2 = BTreeMap::new();
        // Valid-looking key, garbage value
        let op_key = format!("{}:0", sample_tx().compute_txid());
        map2.insert(op_key, "zzzz".into());
        let err2 = decode_chain_p2mr_utxos_hex(&map2).unwrap_err();
        assert!(
            err2.contains("decode") || err2.contains("hex") || err2.contains("txout"),
            "got {err2}"
        );
    }

    #[test]
    fn handshake_stores_stub_capability_not_real() {
        let mock = Arc::new(MockTransport::new());
        mock.push(Ok(RulesV1Response::HandshakeOk {
            version: RULES_V1_VERSION,
            rule_id: "drivechain".into(),
            validation: Some(VALIDATION_STUB_NOT_IMPLEMENTED.into()),
        }));
        let client = RemoteRuleClient::new("drivechain", mock);
        client.handshake().unwrap();
        assert_eq!(
            client.validation_capability().as_deref(),
            Some(VALIDATION_STUB_NOT_IMPLEMENTED)
        );
        assert!(!client.has_real_validation());
    }

    #[test]
    fn handshake_stores_real_capability() {
        let mock = Arc::new(MockTransport::always_accept());
        let client = RemoteRuleClient::new("bip360", mock);
        client.handshake().unwrap();
        assert!(client.has_real_validation());
    }

    #[test]
    fn chain_p2mr_utxos_hex_roundtrip() {
        use bitcoin::{Amount, ScriptBuf};
        let mut utxos = std::collections::HashMap::new();
        let op = OutPoint {
            txid: sample_tx().compute_txid(),
            vout: 1,
        };
        let txout = TxOut {
            value: Amount::from_sat(42),
            script_pubkey: ScriptBuf::new(),
        };
        utxos.insert(op, txout.clone());
        let wire = encode_chain_p2mr_utxos_hex(&utxos);
        let back = decode_chain_p2mr_utxos_hex(&wire).unwrap();
        assert_eq!(back.get(&op).unwrap().value, Amount::from_sat(42));
        assert_eq!(encode_outpoint_key(&op), format!("{}:{}", op.txid, op.vout));
    }

    #[test]
    fn remote_sends_chain_p2mr_utxos_hex_when_ctx_has_set() {
        use bitcoin::{Amount, Network, ScriptBuf, blockdata::constants::genesis_block};
        let block = genesis_block(Network::Regtest);
        let mut utxos = std::collections::HashMap::new();
        let op = OutPoint {
            txid: sample_tx().compute_txid(),
            vout: 0,
        };
        utxos.insert(
            op,
            TxOut {
                value: Amount::from_sat(1),
                script_pubkey: ScriptBuf::new(),
            },
        );
        struct Capture {
            last: Mutex<Option<RulesV1Request>>,
        }
        impl RuleTransport for Capture {
            fn call(&self, request: RulesV1Request) -> Result<RulesV1Response, TransportError> {
                *self.last.lock().unwrap() = Some(request);
                Ok(RulesV1Response::accept())
            }
        }
        let cap = Arc::new(Capture {
            last: Mutex::new(None),
        });
        let client = RemoteRuleClient::new("bip360", cap.clone());
        let ctx = RuleBlockContext::with_block_and_chain_utxos(1, "connect", &block, &utxos);
        assert!(client.connect_block(&ctx).consents());
        let captured = cap.last.lock().unwrap().clone();
        match captured {
            Some(RulesV1Request::ConnectBlock {
                chain_p2mr_utxos_hex: Some(map),
                block_hex: Some(_),
                ..
            }) => {
                assert!(map.contains_key(&encode_outpoint_key(&op)));
            }
            other => panic!("expected chain_p2mr_utxos_hex on wire, got {other:?}"),
        }
    }

    #[test]
    fn remote_omits_chain_p2mr_utxos_when_ctx_has_none() {
        use bitcoin::{Network, blockdata::constants::genesis_block};
        let block = genesis_block(Network::Regtest);
        struct Capture {
            last: Mutex<Option<RulesV1Request>>,
        }
        impl RuleTransport for Capture {
            fn call(&self, request: RulesV1Request) -> Result<RulesV1Response, TransportError> {
                *self.last.lock().unwrap() = Some(request);
                Ok(RulesV1Response::accept())
            }
        }
        let cap = Arc::new(Capture {
            last: Mutex::new(None),
        });
        let client = RemoteRuleClient::new("bip360", cap.clone());
        let ctx = RuleBlockContext::with_block(0, "b", &block);
        assert!(client.connect_block(&ctx).consents());
        let captured = cap.last.lock().unwrap().clone();
        match captured {
            Some(RulesV1Request::ConnectBlock {
                chain_p2mr_utxos_hex: None,
                ..
            }) => {}
            other => panic!("expected no chain set, got {other:?}"),
        }
    }

    #[test]
    fn serve_uds_loop_honors_shutdown_flag() {
        use std::sync::atomic::{AtomicBool, Ordering};

        use temp_dir::TempDir;

        let dir = TempDir::new().unwrap();
        let sock = dir.child("shutdown.sock");
        let listener = bind_uds(&sock).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_t = Arc::clone(&stop);
        let server = thread::spawn(move || {
            serve_uds_loop(
                &listener,
                |_req| RulesV1Response::HealthOk {
                    rule_id: "t".into(),
                    validation: None,
                },
                Some(&stop_t),
            )
        });
        // Give the loop a chance to enter nonblocking accept poll.
        thread::sleep(Duration::from_millis(80));
        stop.store(true, Ordering::Relaxed);
        let joined = server.join().expect("server thread");
        assert!(joined.is_ok(), "shutdown must exit cleanly: {joined:?}");
    }

    #[test]
    fn handshake_err_response_fails() {
        let mock = Arc::new(MockTransport::new());
        mock.push(Ok(RulesV1Response::HandshakeErr {
            detail: "unsupported".into(),
        }));
        let client = RemoteRuleClient::new("drivechain", mock);
        assert!(client.handshake().unwrap_err().contains("unsupported"));
    }

    #[test]
    fn remote_accept_with_local_accept() {
        let mock = Arc::new(MockTransport::always_accept());
        let remote = RemoteRuleClient::new("bip360", mock);
        let mut engine = BackendEngine::new();
        engine.register(RuleBackend::Local(Box::new(AlwaysAccept {
            id: "drivechain",
        })));
        engine.register(RuleBackend::Remote(remote));
        assert!(engine.validate_tx(&tx_ctx()).is_accept());
    }

    #[test]
    fn read_capped_rejects_oversize() {
        let data = vec![0u8; 64];
        let err = read_capped(data.as_slice(), 32).unwrap_err();
        assert!(err.contains("exceeds max size"));
    }

    #[test]
    fn read_capped_ok_under_limit() {
        let data = b"hello";
        assert_eq!(read_capped(data.as_slice(), 16).unwrap(), b"hello");
    }
}
