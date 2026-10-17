//! Bonded oracle providers and per-query bounties (SC-023).
//!
//! A provider registers a data feed, locks a collateral bond in this contract
//! and quotes a fee per query. A market creator hires a provider for a call by
//! escrowing that fee. When the protocol confirms the resolution the provider is
//! paid; when it verifies the resolution was wrong, the creator gets the fee back
//! and the provider's bond is slashed to the creator. Bonds are only returned
//! after a cooldown and when no hire is still pending.
use soroban_sdk::{contracttype, Address, Env, Symbol};

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct Provider {
    pub provider: Address,
    pub feed_id: Symbol,
    pub bond: i128,
    pub fee_per_query: i128,
    pub active: bool,
    pub queries_served: u64,
    pub times_slashed: u64,
    /// Set by `deregister_provider`; the bond can be withdrawn after this time.
    pub withdraw_after: u64,
}

#[contracttype]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum HireStatus {
    Pending,
    Paid,
    Slashed,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct Hire {
    pub call_id: u64,
    pub creator: Address,
    pub provider: Address,
    pub feed_id: Symbol,
    pub fee: i128,
    pub status: HireStatus,
}

#[contracttype]
enum ProviderKey {
    BondToken,
    Provider(Address, Symbol),
    Hire(u64),
    Pending(Address, Symbol),
}

pub fn bond_token(env: &Env) -> Option<Address> {
    env.storage().instance().get(&ProviderKey::BondToken)
}

pub fn set_bond_token(env: &Env, token: &Address) {
    env.storage().instance().set(&ProviderKey::BondToken, token);
}

pub fn get_provider(env: &Env, provider: &Address, feed_id: &Symbol) -> Option<Provider> {
    env.storage()
        .persistent()
        .get(&ProviderKey::Provider(provider.clone(), feed_id.clone()))
}

pub fn set_provider(env: &Env, p: &Provider) {
    env.storage()
        .persistent()
        .set(&ProviderKey::Provider(p.provider.clone(), p.feed_id.clone()), p);
}

pub fn get_hire(env: &Env, call_id: u64) -> Option<Hire> {
    env.storage().persistent().get(&ProviderKey::Hire(call_id))
}

pub fn set_hire(env: &Env, h: &Hire) {
    env.storage().persistent().set(&ProviderKey::Hire(h.call_id), h);
}

pub fn pending(env: &Env, provider: &Address, feed_id: &Symbol) -> u32 {
    env.storage()
        .persistent()
        .get(&ProviderKey::Pending(provider.clone(), feed_id.clone()))
        .unwrap_or(0)
}

pub fn set_pending(env: &Env, provider: &Address, feed_id: &Symbol, n: u32) {
    env.storage()
        .persistent()
        .set(&ProviderKey::Pending(provider.clone(), feed_id.clone()), &n);
}
