pub fn bdk_block_hash_to_bitcoin_block_hash(
    hash: bdk_wallet::bitcoin::BlockHash,
) -> bitcoin::BlockHash {
    use bdk_wallet::bitcoin::hashes::Hash as _;
    let bytes = hash.as_byte_array().to_vec();

    use bitcoin::hashes::sha256d::Hash;
    let hash: bitcoin::hashes::sha256d::Hash = Hash::from_slice(&bytes).unwrap();

    bitcoin::BlockHash::from_raw_hash(hash)
}

pub fn bitcoin_tx_to_bdk_tx(
    tx: bitcoin::Transaction,
) -> Result<bdk_wallet::bitcoin::Transaction, bdk_wallet::bitcoin::consensus::encode::Error> {
    let tx_bytes = bitcoin::consensus::serialize(&tx);

    use bdk_wallet::bitcoin::consensus::Decodable as _;
    let decoded = bdk_wallet::bitcoin::Transaction::consensus_decode(&mut tx_bytes.as_slice())?;

    Ok(decoded)
}

pub fn bdk_txid_to_bitcoin_txid(txid: bdk_wallet::bitcoin::Txid) -> bitcoin::Txid {
    use bdk_wallet::bitcoin::hashes::Hash as _;
    let bytes = txid.to_byte_array();

    use bitcoin::hashes::sha256d::Hash;
    let hash: bitcoin::hashes::sha256d::Hash = Hash::from_byte_array(bytes);

    bitcoin::Txid::from_raw_hash(hash)
}

pub fn bitcoin_txid_to_bdk_txid(txid: bitcoin::Txid) -> bdk_wallet::bitcoin::Txid {
    use bitcoin::hashes::Hash as _;
    let bytes = txid.to_byte_array();

    use bdk_wallet::bitcoin::hashes::sha256d::Hash;
    let hash: bdk_wallet::bitcoin::hashes::sha256d::Hash = Hash::from_byte_array(bytes);

    bdk_wallet::bitcoin::Txid::from_raw_hash(hash)
}
