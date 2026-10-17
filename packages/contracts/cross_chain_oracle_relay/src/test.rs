#![cfg(test)]

extern crate std;

use crate::errors::OracleRelayError;
use crate::proof;
use crate::types::BlockHeader;
use crate::{CrossChainOracleRelay, CrossChainOracleRelayClient};
use ed25519_dalek::{Signer, SigningKey};
use soroban_sdk::{
    contract, contractimpl,
    testutils::Address as _,
    Address, BytesN, Env, String, Vec,
};

const CHAIN_ID: u64 = 8453; // Base
const BLOCK: u64 = 100;

/// Stands in for the outcome manager: records forwarded outcomes and refuses
/// outcome 99 so a rejected forward can be tested.
#[contract]
pub struct MockOutcomeManager;

#[contractimpl]
impl MockOutcomeManager {
    pub fn submit_relayed_outcome(env: Env, relay: Address, call_id: u64, outcome: u32, price: i128) {
        relay.require_auth();
        if outcome == 99 {
            panic!("outcome rejected");
        }
        env.storage().instance().set(&call_id, &(outcome, price));
    }

    pub fn recorded(env: Env, call_id: u64) -> Option<(u32, i128)> {
        env.storage().instance().get(&call_id)
    }
}

struct Fixture<'a> {
    env: Env,
    client: CrossChainOracleRelayClient<'a>,
    manager: MockOutcomeManagerClient<'a>,
    relayer: Address,
    signing_key: SigningKey,
    signer: BytesN<32>,
}

fn sibling(env: &Env) -> BytesN<32> {
    BytesN::from_array(env, &[0xAB; 32])
}

/// Build the state root for a tree holding `leaf` and one sibling.
fn root_for(env: &Env, leaf: &BytesN<32>) -> BytesN<32> {
    proof::compute_root(env, leaf, &Vec::from_array(env, [sibling(env)]))
}

fn header(env: &Env, state_root: &BytesN<32>) -> BlockHeader {
    BlockHeader {
        parent_hash: BytesN::from_array(env, &[1u8; 32]),
        state_root: state_root.clone(),
        transactions_root: BytesN::from_array(env, &[3u8; 32]),
        receipts_root: BytesN::from_array(env, &[4u8; 32]),
        block_number: BLOCK,
        timestamp: 1_234_567_890,
    }
}

/// A relay with a trusted relayer, a registered outcome manager and an authorised
/// signer for `CHAIN_ID`. The block's state root is the root of the tree holding
/// the outcome leaf for (market 7, outcome 1, price 1_000).
fn setup<'a>() -> Fixture<'a> {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let relayer = Address::generate(&env);
    let manager_id = env.register(MockOutcomeManager, ());
    let manager = MockOutcomeManagerClient::new(&env, &manager_id);

    let id = env.register(CrossChainOracleRelay, ());
    let client = CrossChainOracleRelayClient::new(&env, &id);
    client.initialize(
        &admin,
        &manager_id,
        &Address::generate(&env),
        &1,
        &0,
        &Address::generate(&env),
        &Vec::from_array(&env, [relayer.clone()]),
    );

    let signing_key = SigningKey::from_bytes(&[42u8; 32]);
    let signer = BytesN::from_array(&env, &signing_key.verifying_key().to_bytes());
    client.set_chain_signers(&CHAIN_ID, &Vec::from_array(&env, [signer.clone()]));

    let leaf = proof::outcome_leaf(&env, CHAIN_ID, 7, 1, 1_000);
    client.submit_block_header(&relayer, &header(&env, &root_for(&env, &leaf)));

    Fixture { env, client, manager, relayer, signing_key, signer }
}

fn sign(f: &Fixture, market: u64, outcome: u32, price: i128, root: &BytesN<32>) -> BytesN<64> {
    let message = proof::attestation_message(&f.env, CHAIN_ID, market, outcome, price, root);
    let mut buf = [0u8; 32];
    message.copy_into_slice(&mut buf);
    BytesN::from_array(&f.env, &f.signing_key.sign(&buf).to_bytes())
}

fn good_proof(f: &Fixture) -> Vec<BytesN<32>> {
    Vec::from_array(&f.env, [sibling(&f.env)])
}

fn state_root(f: &Fixture) -> BytesN<32> {
    f.client.get_state_root(&BLOCK).unwrap()
}

fn relay(f: &Fixture, market: u64, outcome: u32, price: i128) -> Result<(), OracleRelayError> {
    let root = state_root(f);
    let sig = sign(f, market, outcome, price, &root);
    match f.client.try_relay_outcome(
        &f.relayer, &market, &CHAIN_ID, &outcome, &price, &BLOCK, &f.signer, &good_proof(f), &sig,
    ) {
        Ok(Ok(())) => Ok(()),
        Err(Ok(e)) => Err(e),
        other => panic!("unexpected result: {other:?}"),
    }
}

#[test]
fn a_correctly_proven_and_signed_outcome_is_forwarded_to_the_outcome_manager() {
    let f = setup();
    assert_eq!(relay(&f, 7, 1, 1_000), Ok(()));
    assert_eq!(f.manager.recorded(&7), Some((1, 1_000)));
    assert!(f.client.is_outcome_relayed(&CHAIN_ID, &7));
}

#[test]
fn a_chain_with_no_signers_is_rejected() {
    let f = setup();
    f.client.set_chain_signers(&CHAIN_ID, &Vec::new(&f.env));
    assert_eq!(relay(&f, 7, 1, 1_000), Err(OracleRelayError::UnsupportedChain));

    let root = state_root(&f);
    let sig = sign(&f, 7, 1, 1_000, &root);
    let other_chain = f.client.try_relay_outcome(
        &f.relayer, &7, &1, &1, &1_000, &BLOCK, &f.signer, &good_proof(&f), &sig,
    );
    assert_eq!(other_chain, Err(Ok(OracleRelayError::UnsupportedChain)));
}

#[test]
fn a_signer_not_authorised_for_the_chain_is_rejected() {
    let f = setup();
    let root = state_root(&f);
    let stranger = SigningKey::from_bytes(&[7u8; 32]);
    let stranger_pub = BytesN::from_array(&f.env, &stranger.verifying_key().to_bytes());
    let message = proof::attestation_message(&f.env, CHAIN_ID, 7, 1, 1_000, &root);
    let mut buf = [0u8; 32];
    message.copy_into_slice(&mut buf);
    let sig = BytesN::from_array(&f.env, &stranger.sign(&buf).to_bytes());

    let result = f.client.try_relay_outcome(
        &f.relayer, &7, &CHAIN_ID, &1, &1_000, &BLOCK, &stranger_pub, &good_proof(&f), &sig,
    );
    assert_eq!(result, Err(Ok(OracleRelayError::UnauthorizedSigner)));
    assert_eq!(f.manager.recorded(&7), None);
}

#[test]
fn a_corrupted_merkle_proof_is_rejected() {
    let f = setup();
    let root = state_root(&f);
    let sig = sign(&f, 7, 1, 1_000, &root);
    let bad_proof = Vec::from_array(&f.env, [BytesN::from_array(&f.env, &[0xCD; 32])]);

    let result = f.client.try_relay_outcome(
        &f.relayer, &7, &CHAIN_ID, &1, &1_000, &BLOCK, &f.signer, &bad_proof, &sig,
    );
    assert_eq!(result, Err(Ok(OracleRelayError::MerkleProofVerificationFailed)));
    assert!(!f.client.is_outcome_relayed(&CHAIN_ID, &7));
}

#[test]
fn a_tampered_outcome_or_price_fails_the_proof() {
    let f = setup();
    // The proof commits to (market 7, outcome 1, price 1_000); claiming another
    // outcome or price, even with a valid signature, cannot reach the state root.
    assert_eq!(relay(&f, 7, 2, 1_000), Err(OracleRelayError::MerkleProofVerificationFailed));
    assert_eq!(relay(&f, 7, 1, 1_001), Err(OracleRelayError::MerkleProofVerificationFailed));
    assert_eq!(f.manager.recorded(&7), None);
}

#[test]
fn a_bad_signature_reverts_the_relay() {
    let f = setup();
    let root = state_root(&f);
    // Signed for a different price.
    let sig = sign(&f, 7, 1, 2_000, &root);
    let result = f.client.try_relay_outcome(
        &f.relayer, &7, &CHAIN_ID, &1, &1_000, &BLOCK, &f.signer, &good_proof(&f), &sig,
    );
    assert!(result.is_err());
    assert!(!f.client.is_outcome_relayed(&CHAIN_ID, &7));
    assert_eq!(f.manager.recorded(&7), None);
}

#[test]
fn an_unknown_block_is_rejected() {
    let f = setup();
    let root = state_root(&f);
    let sig = sign(&f, 7, 1, 1_000, &root);
    let result = f.client.try_relay_outcome(
        &f.relayer, &7, &CHAIN_ID, &1, &1_000, &(BLOCK + 1), &f.signer, &good_proof(&f), &sig,
    );
    assert_eq!(result, Err(Ok(OracleRelayError::InvalidBlockHeader)));
}

#[test]
fn only_trusted_relayers_can_relay() {
    let f = setup();
    let root = state_root(&f);
    let sig = sign(&f, 7, 1, 1_000, &root);
    let intruder = Address::generate(&f.env);
    let result = f.client.try_relay_outcome(
        &intruder, &7, &CHAIN_ID, &1, &1_000, &BLOCK, &f.signer, &good_proof(&f), &sig,
    );
    assert_eq!(result, Err(Ok(OracleRelayError::UnauthorizedRelayer)));
}

#[test]
fn an_outcome_can_only_be_relayed_once() {
    let f = setup();
    assert_eq!(relay(&f, 7, 1, 1_000), Ok(()));
    assert_eq!(relay(&f, 7, 1, 1_000), Err(OracleRelayError::OutcomeAlreadyRelayed));
}

#[test]
fn invalid_outcome_and_price_are_rejected_before_any_proof_work() {
    let f = setup();
    assert_eq!(relay(&f, 7, 0, 1_000), Err(OracleRelayError::InvalidOutcome));
    assert_eq!(relay(&f, 7, 1, 0), Err(OracleRelayError::PriceOutOfRange));
    assert_eq!(relay(&f, 7, 1, -5), Err(OracleRelayError::PriceOutOfRange));
}

#[test]
fn when_the_outcome_manager_rejects_the_outcome_nothing_is_recorded() {
    let f = setup();
    // Re-point the block at a tree holding the outcome the mock refuses (99).
    let leaf = proof::outcome_leaf(&f.env, CHAIN_ID, 8, 99, 1_000);
    let root = root_for(&f.env, &leaf);
    let mut h = header(&f.env, &root);
    h.block_number = BLOCK + 5;
    f.client.submit_block_header(&f.relayer, &h);

    let sig = sign(&f, 8, 99, 1_000, &root);
    let result = f.client.try_relay_outcome(
        &f.relayer, &8, &CHAIN_ID, &99, &1_000, &(BLOCK + 5), &f.signer, &good_proof(&f), &sig,
    );
    assert_eq!(result, Err(Ok(OracleRelayError::ForwardFailed)));
    // Atomic: the relayed marker was rolled back with the failed forward.
    assert!(!f.client.is_outcome_relayed(&CHAIN_ID, &8));
    assert_eq!(f.manager.recorded(&8), None);
}

#[test]
fn relay_price_now_requires_a_proof_that_reaches_the_block_state_root() {
    let f = setup();
    let chain = String::from_str(&f.env, "ethereum:1");
    let price = 1_000_000_000i128;
    let leaf = proof::price_leaf(&f.env, &chain, BLOCK + 10, price);
    let root = root_for(&f.env, &leaf);
    let mut h = header(&f.env, &root);
    h.block_number = BLOCK + 10;
    f.client.submit_block_header(&f.relayer, &h);

    // The old behaviour accepted any proof; a corrupted one is now refused.
    let bad = Vec::from_array(&f.env, [BytesN::from_array(&f.env, &[9u8; 32])]);
    assert_eq!(
        f.client.try_relay_price(&f.relayer, &chain, &price, &(BLOCK + 10), &bad, &root),
        Err(Ok(OracleRelayError::MerkleProofVerificationFailed))
    );

    f.client.relay_price(&f.relayer, &chain, &price, &(BLOCK + 10), &good_proof(&f), &root);
    assert_eq!(f.client.get_cross_chain_source(&chain).unwrap().last_price, price);
}

#[test]
fn the_merkle_root_is_independent_of_sibling_order() {
    let env = Env::default();
    let a = BytesN::from_array(&env, &[1u8; 32]);
    let b = BytesN::from_array(&env, &[2u8; 32]);
    assert_eq!(
        proof::compute_root(&env, &a, &Vec::from_array(&env, [b.clone()])),
        proof::compute_root(&env, &b, &Vec::from_array(&env, [a]))
    );
}
