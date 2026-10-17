#![cfg(test)]

extern crate std;

use crate::{
    types::{MarketplaceConfig, OracleProvider},
    OracleMarketplace, OracleMarketplaceClient,
};
use soroban_sdk::{
    testutils::Address as AddressTest,
    Address, BytesN, Env,
};

#[test]
fn initialize_marketplace() {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);

    let contract_id = env.register(OracleMarketplace, ());
    let client = OracleMarketplaceClient::new(&env, &contract_id);
    client.initialize(&admin, &3600u64, &100u32);

    let config = client.get_config_view();
    assert_eq!(config.admin, admin);
    assert_eq!(config.cooldown_secs, 3600);
    assert_eq!(config.default_fee_bps, 100);
}

#[test]
fn register_and_deregister_oracle() {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let provider = Address::generate(&env);
    let pubkey = BytesN::from_array(&env, &[1u8; 32]);

    let contract_id = env.register(OracleMarketplace, ());
    let client = OracleMarketplaceClient::new(&env, &contract_id);
    client.initialize(&admin, &3600u64, &100u32);

    client.register_oracle(&provider, &pubkey, &100u32, &0i128);

    let oracles = client.get_available_oracles();
    assert_eq!(oracles.len(), 1);

    client.deregister_oracle(&provider, &pubkey);

    let oracles = client.get_available_oracles();
    assert_eq!(oracles.len(), 0);
}

#[test]
fn select_oracle_for_call_and_rate() {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let provider = Address::generate(&env);
    let user = Address::generate(&env);
    let pubkey = BytesN::from_array(&env, &[1u8; 32]);

    let contract_id = env.register(OracleMarketplace, ());
    let client = OracleMarketplaceClient::new(&env, &contract_id);
    client.initialize(&admin, &3600u64, &100u32);

    client.register_oracle(&provider, &pubkey, &100u32, &0i128);
    client.select_oracle_for_call(&42u64, &pubkey);

    let selected = client.get_call_oracle(&42u64);
    assert_eq!(selected, Some(pubkey.clone()));

    client.rate_oracle(&user, &pubkey, &true);

    let metrics = client.get_oracle_metrics(&pubkey);
    assert_eq!(metrics, (0u64, 0u64));
}

// ── SC-023: bonded providers and query bounties ──────────────────────────────

use crate::errors::OracleMarketplaceError;
use crate::providers::HireStatus;
use soroban_sdk::testutils::Ledger as _;
use soroban_sdk::token::{Client as TokenClient, StellarAssetClient};
use soroban_sdk::{symbol_short, Symbol};

const FEED: Symbol = symbol_short!("BTCUSD");

struct Bonded<'a> {
    env: Env,
    client: OracleMarketplaceClient<'a>,
    token: Address,
    provider: Address,
    creator: Address,
}

fn bonded<'a>() -> Bonded<'a> {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let provider = Address::generate(&env);
    let creator = Address::generate(&env);

    let id = env.register(OracleMarketplace, ());
    let client = OracleMarketplaceClient::new(&env, &id);
    client.initialize(&admin, &3600u64, &100u32);

    let token = env.register_stellar_asset_contract_v2(admin).address();
    let sac = StellarAssetClient::new(&env, &token);
    sac.mint(&provider, &10_000);
    sac.mint(&creator, &10_000);
    client.set_provider_token(&token);

    Bonded { env, client, token, provider, creator }
}

fn bal(b: &Bonded, who: &Address) -> i128 {
    TokenClient::new(&b.env, &b.token).balance(who)
}

#[test]
fn registering_a_provider_locks_the_bond_in_escrow() {
    let b = bonded();
    b.client.register_provider(&b.provider, &FEED, &1_000, &50);

    assert_eq!(bal(&b, &b.provider), 9_000);
    assert_eq!(bal(&b, &b.client.address), 1_000);
    let info = b.client.get_provider_info(&b.provider, &FEED).unwrap();
    assert_eq!((info.bond, info.fee_per_query, info.active), (1_000, 50, true));
}

#[test]
fn registration_is_validated_and_unique() {
    let b = bonded();
    assert_eq!(
        b.client.try_register_provider(&b.provider, &FEED, &0, &50),
        Err(Ok(OracleMarketplaceError::InvalidBond))
    );
    assert_eq!(
        b.client.try_register_provider(&b.provider, &FEED, &1_000, &-1),
        Err(Ok(OracleMarketplaceError::InvalidBond))
    );
    b.client.register_provider(&b.provider, &FEED, &1_000, &50);
    assert_eq!(
        b.client.try_register_provider(&b.provider, &FEED, &1_000, &50),
        Err(Ok(OracleMarketplaceError::ProviderAlreadyRegistered))
    );
}

#[test]
fn a_creator_hires_a_provider_by_escrowing_the_query_fee() {
    let b = bonded();
    b.client.register_provider(&b.provider, &FEED, &1_000, &50);

    b.client.hire_provider(&b.creator, &1, &b.provider, &FEED);

    assert_eq!(bal(&b, &b.creator), 9_950);
    let hire = b.client.get_hire(&1).unwrap();
    assert_eq!((hire.fee, hire.status), (50, HireStatus::Pending));
    assert_eq!(
        b.client.try_hire_provider(&b.creator, &1, &b.provider, &FEED),
        Err(Ok(OracleMarketplaceError::AlreadyHired))
    );
    let unknown = Address::generate(&b.env);
    assert_eq!(
        b.client.try_hire_provider(&b.creator, &2, &unknown, &FEED),
        Err(Ok(OracleMarketplaceError::ProviderNotFound))
    );
}

#[test]
fn a_confirmed_resolution_pays_the_provider_the_query_fee() {
    let b = bonded();
    b.client.register_provider(&b.provider, &FEED, &1_000, &50);
    b.client.hire_provider(&b.creator, &1, &b.provider, &FEED);

    b.client.confirm_resolution(&1);

    assert_eq!(bal(&b, &b.provider), 9_000 + 50);
    assert_eq!(b.client.get_hire(&1).unwrap().status, HireStatus::Paid);
    assert_eq!(b.client.get_provider_info(&b.provider, &FEED).unwrap().queries_served, 1);
    // Paid once only.
    assert_eq!(
        b.client.try_confirm_resolution(&1),
        Err(Ok(OracleMarketplaceError::HireClosed))
    );
}

#[test]
fn an_inaccurate_resolution_refunds_the_creator_and_slashes_the_bond_to_them() {
    let b = bonded();
    b.client.register_provider(&b.provider, &FEED, &1_000, &50);
    b.client.hire_provider(&b.creator, &1, &b.provider, &FEED);

    b.client.slash_provider(&1, &400);

    // Creator: fee back (50) + 400 of the provider's bond.
    assert_eq!(bal(&b, &b.creator), 10_000 + 400);
    let info = b.client.get_provider_info(&b.provider, &FEED).unwrap();
    assert_eq!((info.bond, info.times_slashed), (600, 1));
    assert_eq!(b.client.get_hire(&1).unwrap().status, HireStatus::Slashed);
    assert_eq!(
        b.client.try_slash_provider(&1, &100),
        Err(Ok(OracleMarketplaceError::HireClosed))
    );
    // The contract holds exactly the remaining bond.
    assert_eq!(bal(&b, &b.client.address), 600);
}

#[test]
fn slash_amounts_are_bounded_by_the_bond() {
    let b = bonded();
    b.client.register_provider(&b.provider, &FEED, &1_000, &50);
    b.client.hire_provider(&b.creator, &1, &b.provider, &FEED);

    assert_eq!(b.client.try_slash_provider(&1, &0), Err(Ok(OracleMarketplaceError::InvalidSlash)));
    assert_eq!(b.client.try_slash_provider(&1, &1_001), Err(Ok(OracleMarketplaceError::InvalidSlash)));
    assert_eq!(b.client.try_slash_provider(&9, &1), Err(Ok(OracleMarketplaceError::HireNotFound)));
}

#[test]
fn the_bond_is_returned_only_after_the_cooldown_and_with_no_pending_hires() {
    let b = bonded();
    b.client.register_provider(&b.provider, &FEED, &1_000, &50);
    b.client.hire_provider(&b.creator, &1, &b.provider, &FEED);

    // Still active: no withdrawal.
    assert_eq!(
        b.client.try_withdraw_provider_bond(&b.provider, &FEED),
        Err(Ok(OracleMarketplaceError::WithdrawalLocked))
    );
    b.client.deregister_provider(&b.provider, &FEED);
    assert_eq!(
        b.client.try_withdraw_provider_bond(&b.provider, &FEED),
        Err(Ok(OracleMarketplaceError::WithdrawalLocked))
    );

    b.env.ledger().with_mut(|l| l.timestamp += 3_601);
    // A hire is still pending.
    assert_eq!(
        b.client.try_withdraw_provider_bond(&b.provider, &FEED),
        Err(Ok(OracleMarketplaceError::PendingHires))
    );

    b.client.confirm_resolution(&1);
    assert_eq!(b.client.withdraw_provider_bond(&b.provider, &FEED), 1_000);
    assert_eq!(bal(&b, &b.provider), 10_000 + 50);
    // A deregistered provider cannot be hired.
    assert_eq!(
        b.client.try_hire_provider(&b.creator, &2, &b.provider, &FEED),
        Err(Ok(OracleMarketplaceError::OracleNotActive))
    );
}

#[test]
fn provider_features_need_a_configured_token() {
    let env = Env::default();
    env.mock_all_auths();
    let id = env.register(OracleMarketplace, ());
    let client = OracleMarketplaceClient::new(&env, &id);
    client.initialize(&Address::generate(&env), &3600u64, &100u32);
    assert_eq!(
        client.try_register_provider(&Address::generate(&env), &FEED, &1_000, &50),
        Err(Ok(OracleMarketplaceError::BondTokenNotSet))
    );
}
