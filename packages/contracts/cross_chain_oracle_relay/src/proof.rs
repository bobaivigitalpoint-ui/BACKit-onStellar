//! Merkle proof and attestation digests for relayed cross-chain data.
use soroban_sdk::{Bytes, BytesN, Env, Vec};

/// Leaf committed to by a source chain's state root for a market outcome.
pub fn outcome_leaf(env: &Env, chain_id: u64, market_id: u64, outcome: u32, price: i128) -> BytesN<32> {
    let mut raw = Bytes::from_slice(env, b"outcome:");
    raw.append(&Bytes::from_slice(env, &chain_id.to_be_bytes()));
    raw.append(&Bytes::from_slice(env, &market_id.to_be_bytes()));
    raw.append(&Bytes::from_slice(env, &outcome.to_be_bytes()));
    raw.append(&Bytes::from_slice(env, &price.to_be_bytes()));
    env.crypto().sha256(&raw).into()
}

/// Leaf committed to by a state root for a relayed price.
pub fn price_leaf(env: &Env, chain_name: &soroban_sdk::String, block_number: u64, price: i128) -> BytesN<32> {
    let mut raw = Bytes::from_slice(env, b"price:");
    raw.append(&chain_name.clone().to_xdr(env));
    raw.append(&Bytes::from_slice(env, &block_number.to_be_bytes()));
    raw.append(&Bytes::from_slice(env, &price.to_be_bytes()));
    env.crypto().sha256(&raw).into()
}

/// Fold `leaf` up the proof into a root. Each pair is hashed in sorted order, so
/// the proof needs no left/right flags. An empty proof means the leaf is the root.
pub fn compute_root(env: &Env, leaf: &BytesN<32>, proof: &Vec<BytesN<32>>) -> BytesN<32> {
    let mut node = leaf.clone();
    for sibling in proof.iter() {
        let (a, b) = if node.to_array() <= sibling.to_array() {
            (node.clone(), sibling)
        } else {
            (sibling, node.clone())
        };
        let mut combined = Bytes::from_slice(env, b"merkle:");
        combined.append(&Bytes::from_slice(env, &a.to_array()));
        combined.append(&Bytes::from_slice(env, &b.to_array()));
        node = env.crypto().sha256(&combined).into();
    }
    node
}

/// The bytes an authorised signer signs to attest a relayed outcome. It commits
/// to the state root the proof resolves to, so an attestation cannot be reused
/// against a different root.
pub fn attestation_message(
    env: &Env,
    chain_id: u64,
    market_id: u64,
    outcome: u32,
    price: i128,
    root: &BytesN<32>,
) -> Bytes {
    let mut raw = Bytes::from_slice(env, b"relay:");
    raw.append(&Bytes::from_slice(env, &chain_id.to_be_bytes()));
    raw.append(&Bytes::from_slice(env, &market_id.to_be_bytes()));
    raw.append(&Bytes::from_slice(env, &outcome.to_be_bytes()));
    raw.append(&Bytes::from_slice(env, &price.to_be_bytes()));
    raw.append(&Bytes::from_slice(env, &root.to_array()));
    let digest: BytesN<32> = env.crypto().sha256(&raw).into();
    Bytes::from_slice(env, &digest.to_array())
}

use soroban_sdk::xdr::ToXdr;
