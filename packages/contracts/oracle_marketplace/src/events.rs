#![allow(deprecated)]

use soroban_sdk::{Address, BytesN, Env, Symbol};

pub fn emit_provider_registered(env: &Env, provider: &Address, feed_id: &Symbol, bond: i128, fee: i128) {
    env.events().publish(
        ("oracle_marketplace", "ProviderRegistered"),
        (provider.clone(), feed_id.clone(), bond, fee),
    );
}

pub fn emit_provider_hired(env: &Env, call_id: u64, provider: &Address, feed_id: &Symbol, fee: i128) {
    env.events().publish(
        ("oracle_marketplace", "ProviderHired"),
        (call_id, provider.clone(), feed_id.clone(), fee),
    );
}

pub fn emit_query_paid(env: &Env, call_id: u64, provider: &Address, fee: i128) {
    env.events()
        .publish(("oracle_marketplace", "QueryFeePaid"), (call_id, provider.clone(), fee));
}

pub fn emit_provider_slashed(env: &Env, call_id: u64, provider: &Address, slashed: i128) {
    env.events().publish(
        ("oracle_marketplace", "ProviderSlashed"),
        (call_id, provider.clone(), slashed),
    );
}

pub fn emit_oracle_registered(env: &Env, provider: &Address, pubkey: &BytesN<32>, fee_bps: u32) {
    env.events().publish(
        ("oracle_marketplace", "OracleRegistered"),
        (provider.clone(), pubkey.clone(), fee_bps),
    );
}

pub fn emit_oracle_deregistered(env: &Env, provider: &Address, pubkey: &BytesN<32>) {
    env.events().publish(
        ("oracle_marketplace", "OracleDeregistered"),
        (provider.clone(), pubkey.clone()),
    );
}

pub fn emit_oracle_selected(env: &Env, call_id: u64, provider: &BytesN<32>) {
    env.events().publish(
        ("oracle_marketplace", "OracleSelectedForCall"),
        (call_id, provider.clone()),
    );
}

pub fn emit_oracle_rated(env: &Env, provider: &BytesN<32>, user: &Address, satisfied: bool) {
    env.events().publish(
        ("oracle_marketplace", "OracleRated"),
        (provider.clone(), user.clone(), satisfied),
    );
}
