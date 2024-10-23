use bdk::bitcoin::consensus::Decodable;

pub fn bdk_block_hash_to_bitcoin_block_hash(hash: bdk::bitcoin::BlockHash) -> bitcoin::BlockHash {
    use bdk::bitcoin::hashes::Hash as _;
    let bytes = hash.as_byte_array().to_vec();

    use bitcoin::hashes::sha256d::Hash;
    use bitcoin::hashes::Hash as _;
    let hash: bitcoin::hashes::sha256d::Hash = Hash::from_slice(&bytes).unwrap();

    bitcoin::BlockHash::from_raw_hash(hash)
}

pub fn bitcoin_tx_to_bdk_tx(
    tx: bitcoin::Transaction,
) -> Result<bdk::bitcoin::Transaction, bdk::bitcoin::consensus::encode::Error> {
    let tx_bytes = bitcoin::consensus::serialize(&tx);

    let decoded = bdk::bitcoin::Transaction::consensus_decode(&mut tx_bytes.as_slice())?;

    Ok(decoded)
}

pub fn bdk_txid_to_bitcoin_txid(txid: bdk::bitcoin::Txid) -> bitcoin::Txid {
    use bdk::bitcoin::hashes::Hash as _;
    let bytes = txid.to_byte_array();

    use bitcoin::hashes::sha256d::Hash;
    use bitcoin::hashes::Hash as _;
    let hash: bitcoin::hashes::sha256d::Hash = Hash::from_byte_array(bytes);

    bitcoin::Txid::from_raw_hash(hash)
}

pub fn bitcoin_txid_to_bdk_txid(txid: bitcoin::Txid) -> bdk::bitcoin::Txid {
    use bitcoin::hashes::Hash as _;
    let bytes = txid.to_byte_array();

    use bdk::bitcoin::hashes::sha256d::Hash;
    use bdk::bitcoin::hashes::Hash as _;
    let hash: bdk::bitcoin::hashes::sha256d::Hash = Hash::from_byte_array(bytes);

    bdk::bitcoin::Txid::from_raw_hash(hash)
}
