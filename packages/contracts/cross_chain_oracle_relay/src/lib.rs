#![no_std]

mod errors;
mod events;
pub mod proof;
mod storage;
mod types;

#[cfg(test)]
mod test;

use errors::OracleRelayError;
use events::{emit_header_submitted, emit_outcome_relayed, emit_price_relayed, emit_signers_set};
use soroban_sdk::{
    contract, contractimpl, Address, Bytes, BytesN, Env, IntoVal, String, Vec,
};
use storage::*;
use types::{BlockHeader, CrossChainSource, OracleRelayConfig};

const ETH_CHAIN_ID: u64 = 1;

#[contract]
pub struct CrossChainOracleRelay;

#[contractimpl]
impl CrossChainOracleRelay {
    pub fn initialize(
        env: Env,
        admin: Address,
        outcome_manager: Address,
        stellar_oracle: Address,
        min_signatures: u32,
        fee_per_submission: i128,
        fee_token: Address,
        trusted_relayers: Vec<Address>,
    ) -> Result<(), OracleRelayError> {
        if get_config(&env).is_some() {
            return Err(OracleRelayError::AlreadyInitialized);
        }
        admin.require_auth();
        let config = OracleRelayConfig {
            admin: admin.clone(),
            outcome_manager,
            stellar_oracle,
            min_signatures,
            fee_per_submission,
            fee_token,
            trusted_relayers,
        };
        set_config(&env, &config);
        Ok(())
    }

    pub fn submit_block_header(
        env: Env,
        relayer: Address,
        header: BlockHeader,
    ) -> Result<(), OracleRelayError> {
        relayer.require_auth();
        let config = get_config(&env).ok_or(OracleRelayError::NotInitialized)?;

        let authorized = config.trusted_relayers.iter().any(|a| a == relayer);
        if !authorized {
            return Err(OracleRelayError::UnauthorizedRelayer);
        }

        if is_block_relayed(&env, header.block_number) {
            return Err(OracleRelayError::BlockAlreadyRelayed);
        }

        let block_hash = Self::compute_block_hash(&env, &header);
        add_trusted_hash(&env, header.block_number, &block_hash);
        set_state_root(&env, header.block_number, &header.state_root);
        mark_block_relayed(&env, header.block_number);

        emit_header_submitted(&env, &relayer, header.block_number, &block_hash);
        Ok(())
    }

    pub fn relay_price(
        env: Env,
        relayer: Address,
        chain_name: String,
        price: i128,
        block_number: u64,
        merkle_proof: Vec<BytesN<32>>,
        expected_root: BytesN<32>,
    ) -> Result<(), OracleRelayError> {
        relayer.require_auth();
        let config = get_config(&env).ok_or(OracleRelayError::NotInitialized)?;

        if price <= 0 || price > 1_000_000_000_000_000_000 {
            return Err(OracleRelayError::PriceOutOfRange);
        }

        let authorized = config.trusted_relayers.iter().any(|a| a == relayer);
        if !authorized {
            return Err(OracleRelayError::UnauthorizedRelayer);
        }

        let trusted = get_trusted_hash(&env, block_number);
        if trusted.is_none() {
            return Err(OracleRelayError::InvalidBlockHeader);
        }

        // The proof must fold the price leaf up to the state root of the block the
        // relayer submitted (and the root the relayer claims).
        let state_root = get_state_root(&env, block_number).ok_or(OracleRelayError::InvalidBlockHeader)?;
        let leaf = proof::price_leaf(&env, &chain_name, block_number, price);
        let computed = proof::compute_root(&env, &leaf, &merkle_proof);
        if computed != state_root || expected_root != state_root {
            return Err(OracleRelayError::MerkleProofVerificationFailed);
        }

        let source = CrossChainSource {
            chain_name: chain_name.clone(),
            latest_block: block_number,
            last_relayed_at: env.ledger().timestamp(),
            last_price: price,
        };
        set_source(&env, &chain_name, &source);

        emit_price_relayed(&env, &chain_name, price, block_number);
        Ok(())
    }

    /// Authorise the ed25519 keys that may attest outcomes from `chain_id`
    /// (admin only). An empty list removes support for the chain.
    pub fn set_chain_signers(env: Env, chain_id: u64, signers: Vec<BytesN<32>>) -> Result<(), OracleRelayError> {
        let config = get_config(&env).ok_or(OracleRelayError::NotInitialized)?;
        config.admin.require_auth();
        emit_signers_set(&env, chain_id, signers.len());
        set_chain_signers(&env, chain_id, &signers);
        Ok(())
    }

    /// Relay a market outcome attested on a source chain (SC-024).
    ///
    /// Checks, in order: the relayer is trusted; the chain has signers and `signer`
    /// is one of them; the outcome and price are in range; the Merkle proof folds
    /// the outcome leaf up to the state root of a block this contract already
    /// trusts; `signature` is `signer`'s ed25519 signature over the attestation
    /// message for that root; and the (chain, market) outcome has not been relayed
    /// before. It is then forwarded to the outcome manager in the same
    /// transaction: if the outcome manager rejects it, the whole call reverts and
    /// nothing is recorded.
    ///
    /// The outcome manager must expose
    /// `submit_relayed_outcome(relay: Address, call_id: u64, outcome: u32, price: i128)`.
    pub fn relay_outcome(
        env: Env,
        relayer: Address,
        market_id: u64,
        source_chain_id: u64,
        outcome: u32,
        final_price: i128,
        block_number: u64,
        signer: BytesN<32>,
        merkle_proof: Vec<BytesN<32>>,
        signature: BytesN<64>,
    ) -> Result<(), OracleRelayError> {
        relayer.require_auth();
        let config = get_config(&env).ok_or(OracleRelayError::NotInitialized)?;
        if !config.trusted_relayers.iter().any(|a| a == relayer) {
            return Err(OracleRelayError::UnauthorizedRelayer);
        }

        let signers = get_chain_signers(&env, source_chain_id)
            .filter(|s| !s.is_empty())
            .ok_or(OracleRelayError::UnsupportedChain)?;
        if !signers.iter().any(|s| s == signer) {
            return Err(OracleRelayError::UnauthorizedSigner);
        }
        if outcome == 0 {
            return Err(OracleRelayError::InvalidOutcome);
        }
        if final_price <= 0 || final_price > 1_000_000_000_000_000_000 {
            return Err(OracleRelayError::PriceOutOfRange);
        }
        if is_outcome_relayed(&env, source_chain_id, market_id) {
            return Err(OracleRelayError::OutcomeAlreadyRelayed);
        }

        let state_root = get_state_root(&env, block_number).ok_or(OracleRelayError::InvalidBlockHeader)?;
        let leaf = proof::outcome_leaf(&env, source_chain_id, market_id, outcome, final_price);
        if proof::compute_root(&env, &leaf, &merkle_proof) != state_root {
            return Err(OracleRelayError::MerkleProofVerificationFailed);
        }

        // Traps (reverting the call) if the signature is not valid for `signer`.
        let message = proof::attestation_message(&env, source_chain_id, market_id, outcome, final_price, &state_root);
        env.crypto().ed25519_verify(&signer, &message, &signature);

        mark_outcome_relayed(&env, source_chain_id, market_id);

        let args = soroban_sdk::vec![
            &env,
            env.current_contract_address().into_val(&env),
            market_id.into_val(&env),
            outcome.into_val(&env),
            final_price.into_val(&env),
        ];
        let forwarded = env.try_invoke_contract::<(), soroban_sdk::Error>(
            &config.outcome_manager,
            &soroban_sdk::Symbol::new(&env, "submit_relayed_outcome"),
            args,
        );
        if !matches!(forwarded, Ok(Ok(()))) {
            return Err(OracleRelayError::ForwardFailed);
        }

        emit_outcome_relayed(&env, market_id, source_chain_id, outcome, final_price);
        Ok(())
    }

    pub fn is_outcome_relayed(env: Env, source_chain_id: u64, market_id: u64) -> bool {
        storage::is_outcome_relayed(&env, source_chain_id, market_id)
    }

    pub fn get_cross_chain_source(env: Env, chain_name: String) -> Option<CrossChainSource> {
        storage::get_source(&env, &chain_name)
    }

    pub fn get_all_sources(env: Env) -> Vec<CrossChainSource> {
        let _config = match get_config(&env) {
            Some(c) => c,
            None => return Vec::new(&env),
        };
        let mut v = Vec::new(&env);
        let eth = String::from_str(&env, "ethereum:1");
        if let Some(s) = storage::get_source(&env, &eth) {
            v.push_back(s);
        }
        v
    }

    /// The state root recorded for a relayed block.
    pub fn get_state_root(env: Env, block_number: u64) -> Option<BytesN<32>> {
        storage::get_state_root(&env, block_number)
    }

    pub fn get_trusted_block_hash(env: Env, block_number: u64) -> Option<BytesN<32>> {
        get_trusted_hash(&env, block_number)
    }

    pub fn fallback_to_stellar_oracle(env: Env) -> Result<i128, OracleRelayError> {
        let config = get_config(&env).ok_or(OracleRelayError::NotInitialized)?;
        let result: Result<i128, soroban_sdk::Error> = env.invoke_contract(
            &config.stellar_oracle,
            &soroban_sdk::Symbol::new(&env, "latest_price"),
            soroban_sdk::vec![&env],
        );
        result.map_err(|_| OracleRelayError::InvalidBlockHeader)
    }

    fn compute_block_hash(env: &Env, header: &BlockHeader) -> BytesN<32> {
        let mut raw = Bytes::from_slice(env, b"eth_block:");
        raw.append(&Bytes::from_slice(env, &header.block_number.to_be_bytes()));
        raw.append(&Bytes::from_slice(env, &header.parent_hash.to_array()));
        raw.append(&Bytes::from_slice(env, &header.state_root.to_array()));
        raw.append(&Bytes::from_slice(env, &header.transactions_root.to_array()));
        raw.append(&Bytes::from_slice(env, &header.receipts_root.to_array()));
        raw.append(&Bytes::from_slice(env, &header.timestamp.to_be_bytes()));
        env.crypto().sha256(&raw).into()
    }
}
