pub fn bdk_block_hash_to_bitcoin_block_hash(hash: bdk::bitcoin::BlockHash) -> bitcoin::BlockHash {
    use bdk::bitcoin::hashes::Hash as _;
    let bytes = hash.as_byte_array().to_vec();

    use bitcoin::hashes::sha256d::Hash;
    use bitcoin::hashes::Hash as Hm;
    let hash: bitcoin::hashes::sha256d::Hash = Hash::from_slice(&bytes).unwrap();

    bitcoin::BlockHash::from_raw_hash(hash)
}

pub fn bdk_txid_to_bitcoin_txid(hash: bdk::bitcoin::Txid) -> bitcoin::Txid {
    use bdk::bitcoin::hashes::Hash as _;
    let bytes = hash.as_byte_array().to_vec();

    use bitcoin::hashes::sha256d::Hash;
    use bitcoin::hashes::Hash as _;
    let hash: bitcoin::hashes::sha256d::Hash = Hash::from_slice(&bytes).unwrap();

    bitcoin::Txid::from_raw_hash(hash)
}

pub fn bitcoin_txid_to_bdk_txid(hash: bitcoin::Txid) -> bdk::bitcoin::Txid {
    use bitcoin::hashes::Hash as _;
    let bytes = hash.as_byte_array().to_vec();

    use bdk::bitcoin::hashes::sha256d::Hash;
    use bdk::bitcoin::hashes::Hash as _;
    let hash: bdk::bitcoin::hashes::sha256d::Hash = Hash::from_slice(&bytes).unwrap();

    bdk::bitcoin::Txid::from_raw_hash(hash)
}
