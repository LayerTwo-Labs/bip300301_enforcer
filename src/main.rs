mod messages;

use std::time::SystemTime;

use bitcoin::{
    absolute::{Height, LockTime},
    block::Header,
    hashes::Hash,
    Block, BlockHash, CompactTarget, Transaction, TxMerkleNode,
};
use miette::{IntoDiagnostic, Result};

mod server;
mod types;
mod bip300;

use server::{bip300::validator_server::ValidatorServer, Bip300};
use tonic::transport::Server;

#[tokio::main]
async fn main() -> Result<()> {
    let coinbase = Transaction {
        input: vec![],
        output: vec![],
        version: bitcoin::transaction::Version(0),
        lock_time: LockTime::Blocks(Height::ZERO),
    };

    let now = std::time::SystemTime::now();

    let txdata = vec![coinbase];
    let header = Header {
        bits: CompactTarget::from_consensus(0),
        prev_blockhash: BlockHash::all_zeros(),
        merkle_root: TxMerkleNode::all_zeros(),
        nonce: 0,
        time: now
            .duration_since(SystemTime::UNIX_EPOCH)
            .into_diagnostic()?
            .as_secs() as u32,
        version: bitcoin::block::Version::NO_SOFT_FORK_SIGNALLING,
    };

    let block = Block { header, txdata };
    dbg!(block);

    let addr = "[::1]:50051".parse().into_diagnostic()?;
    println!("Listening for gRPC on {addr}");

    let bip300 = Bip300::new()?;

    Server::builder()
        .add_service(ValidatorServer::new(bip300))
        .serve(addr)
        .await
        .into_diagnostic()?;

    Ok(())
}
