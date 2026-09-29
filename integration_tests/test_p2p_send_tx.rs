//! P2P transaction broadcast against a real node: the node asks for the
//! transaction and has processed it by the time the broadcast returns, and a
//! second broadcast of the same transaction is reported as not submitted,
//! since a node never asks for a transaction it already has.
//!
//! We only speak v1 (plaintext) P2P. A node with BIP324 v2 transport enabled
//! still accepts v1 connections, telling the two apart by their first bytes,
//! so this runs against a node with v2 both disabled and enabled.

use std::net::SocketAddr;

use bip300301_enforcer_lib::p2p::{BroadcastAddr, broadcast_nonstandard_tx};
use bitcoin::{Transaction, p2p::Magic};
use jsonrpsee::{core::client::ClientT as _, rpc_params};

use crate::setup::{PostSetup, bitcoind_regtest_magic};

pub const TEST_NAME: &str = "p2p_send_tx";

/// The node's `-v2transport` setting
#[derive(Clone, Copy, Debug)]
pub enum NodeTransport {
    V1Only,
    V2Enabled,
}

impl NodeTransport {
    pub fn bitcoind_arg(self) -> String {
        let enabled = match self {
            Self::V1Only => 0,
            Self::V2Enabled => 1,
        };
        format!("-v2transport={enabled}")
    }
}

impl std::fmt::Display for NodeTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::V1Only => "v1",
            Self::V2Enabled => "v2",
        })
    }
}

/// Whether the node advertises v2 transport, so a setting it ignored cannot
/// make both runs test the same thing
async fn advertises_v2(post_setup: &PostSetup) -> anyhow::Result<bool> {
    let network_info: serde_json::Value = post_setup
        .bitcoind_client
        .request("getnetworkinfo", rpc_params![])
        .await?;
    let services = network_info["localservicesnames"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("getnetworkinfo returned no localservicesnames"))?;
    Ok(services.iter().any(|service| service == "P2P_V2"))
}

/// A transaction funded and signed by the node's wallet, but not sent
async fn unsent_tx(post_setup: &PostSetup) -> anyhow::Result<Transaction> {
    let client = &post_setup.bitcoind_client;
    let address: String = client.request("getnewaddress", rpc_params![]).await?;
    // Mature a coinbase to spend
    let _block_hashes: Vec<bitcoin::BlockHash> = client
        .request("generatetoaddress", rpc_params![101, &address])
        .await?;
    let raw: String = client
        .request(
            "createrawtransaction",
            rpc_params![serde_json::json!([]), serde_json::json!([{ &address: 1 }])],
        )
        .await?;
    // A fresh chain has no fee history to estimate from
    let funded: serde_json::Value = client
        .request(
            "fundrawtransaction",
            rpc_params![raw, serde_json::json!({ "fee_rate": 2 })],
        )
        .await?;
    let funded_hex = funded["hex"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("fundrawtransaction returned no hex"))?;
    let signed: serde_json::Value = client
        .request("signrawtransactionwithwallet", rpc_params![funded_hex])
        .await?;
    anyhow::ensure!(
        signed["complete"].as_bool() == Some(true),
        "failed to sign: {signed}"
    );
    let signed_hex = signed["hex"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("signrawtransactionwithwallet returned no hex"))?;
    Ok(bitcoin::consensus::encode::deserialize_hex(signed_hex)?)
}

async fn in_mempool(post_setup: &PostSetup, tx: &Transaction) -> anyhow::Result<bool> {
    let mempool: Vec<bitcoin::Txid> = post_setup
        .bitcoind_client
        .request("getrawmempool", rpc_params![])
        .await?;
    Ok(mempool.contains(&tx.compute_txid()))
}

pub async fn test_p2p_send_tx(
    post_setup: PostSetup,
    transport: NodeTransport,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        advertises_v2(&post_setup).await? == matches!(transport, NodeTransport::V2Enabled),
        "the node's advertised services do not match its {transport} transport setting",
    );
    let magic = match bitcoind_regtest_magic() {
        Some(magic) => Magic::from_bytes(
            hex::decode(&magic)?
                .try_into()
                .map_err(|_| anyhow::anyhow!("BITCOIND_REGTEST_MAGIC is not 4 bytes"))?,
        ),
        None => Magic::REGTEST,
    };
    let node_addr: SocketAddr = (
        [127, 0, 0, 1],
        post_setup.reserved_ports.bitcoind_listen.port(),
    )
        .into();
    let tx = unsent_tx(&post_setup).await?;
    let height: i32 = post_setup
        .bitcoind_client
        .request("getblockcount", rpc_params![])
        .await?;
    anyhow::ensure!(!in_mempool(&post_setup, &tx).await?);

    let sent = broadcast_nonstandard_tx(node_addr.into(), height, magic, tx.clone()).await?;
    anyhow::ensure!(
        sent,
        "the node did not ask for a transaction it does not have"
    );
    // No polling: the broadcast only returns once the node has processed it
    anyhow::ensure!(
        in_mempool(&post_setup, &tx).await?,
        "the broadcast returned before the node had processed the transaction"
    );
    tracing::info!(txid = %tx.compute_txid(), "broadcast accepted into the mempool");

    let resent =
        broadcast_nonstandard_tx(BroadcastAddr::Sock(node_addr), height, magic, tx).await?;
    anyhow::ensure!(
        !resent,
        "a transaction the node already has must be reported as not submitted"
    );
    Ok(())
}
