#![no_std]

mod errors;
mod events;
pub mod providers;
mod storage;
mod types;

#[cfg(test)]
mod test;

pub use types::{MarketplaceConfig, OracleProvider, OracleRating};

use errors::OracleMarketplaceError;
use events::{emit_oracle_deregistered, emit_oracle_rated, emit_oracle_registered, emit_oracle_selected};
use providers::{Hire, HireStatus, Provider};
use soroban_sdk::{contract, contractimpl, token, Address, BytesN, Env, Map, Symbol, Vec};
use storage::*;

#[contract]
pub struct OracleMarketplace;

#[contractimpl]
impl OracleMarketplace {
    pub fn initialize(
        env: Env,
        admin: Address,
        cooldown_secs: u64,
        default_fee_bps: u32,
    ) -> Result<(), OracleMarketplaceError> {
        if get_config(&env).is_some() {
            return Err(OracleMarketplaceError::AlreadyInitialized);
        }
        admin.require_auth();

        let config = MarketplaceConfig {
            admin,
            cooldown_secs,
            default_fee_bps,
        };
        set_config(&env, &config);
        Ok(())
    }

    pub fn register_oracle(
        env: Env,
        provider: Address,
        pubkey: BytesN<32>,
        fee_bps: u32,
        min_stake: i128,
    ) -> Result<(), OracleMarketplaceError> {
        provider.require_auth();

        if get_oracle(&env, &pubkey).is_some() {
            return Err(OracleMarketplaceError::OracleAlreadyRegistered);
        }
        if fee_bps > 10000 {
            return Err(OracleMarketplaceError::InvalidFee);
        }

        let oracle = OracleProvider {
            pubkey: pubkey.clone(),
            address: provider.clone(),
            fee_bps,
            min_stake,
            staked_amount: min_stake,
            total_resolved: 0,
            total_disputes: 0,
            is_active: true,
            registered_at: env.ledger().timestamp(),
            deregister_after: None,
        };

        set_oracle(&env, &pubkey, &oracle);
        add_to_oracle_list(&env, &pubkey);
        emit_oracle_registered(&env, &provider, &pubkey, fee_bps);

        Ok(())
    }

    pub fn deregister_oracle(
        env: Env,
        provider: Address,
        pubkey: BytesN<32>,
    ) -> Result<(), OracleMarketplaceError> {
        provider.require_auth();
        let config = get_config(&env).ok_or(OracleMarketplaceError::NotInitialized)?;
        let oracle = get_oracle(&env, &pubkey).ok_or(OracleMarketplaceError::OracleNotFound)?;

        if !oracle.is_active {
            return Err(OracleMarketplaceError::OracleNotActive);
        }

        if let Some(deregister_after) = oracle.deregister_after {
            if env.ledger().timestamp() < deregister_after {
                return Err(OracleMarketplaceError::CooldownActive);
            }
        }

        let mut updated = oracle;
        updated.is_active = false;
        updated.deregister_after = Some(env.ledger().timestamp() + config.cooldown_secs);
        set_oracle(&env, &pubkey, &updated);
        remove_from_oracle_list(&env, &pubkey);

        emit_oracle_deregistered(&env, &provider, &pubkey);
        Ok(())
    }

    pub fn get_available_oracles(env: Env) -> Vec<OracleProvider> {
        let list = get_oracle_list(&env);
        let mut result = Vec::new(&env);
        for i in 0..list.len() {
            let pubkey = list.get(i).unwrap();
            if let Some(oracle) = get_oracle(&env, &pubkey) {
                if oracle.is_active {
                    result.push_back(oracle);
                }
            }
        }
        result
    }

    pub fn select_oracle_for_call(
        env: Env,
        call_id: u64,
        oracle_pubkey: BytesN<32>,
    ) -> Result<(), OracleMarketplaceError> {
        let oracle = get_oracle(&env, &oracle_pubkey).ok_or(OracleMarketplaceError::OracleNotFound)?;
        if !oracle.is_active {
            return Err(OracleMarketplaceError::OracleNotActive);
        }

        set_call_oracle(&env, call_id, &oracle_pubkey);
        emit_oracle_selected(&env, call_id, &oracle_pubkey);
        Ok(())
    }

    // ── Bonded providers and query bounties (SC-023) ───────────────────────

    /// Choose the token bonds and query fees are paid in (admin only).
    pub fn set_provider_token(env: Env, token: Address) -> Result<(), OracleMarketplaceError> {
        get_config(&env).ok_or(OracleMarketplaceError::NotInitialized)?.admin.require_auth();
        providers::set_bond_token(&env, &token);
        Ok(())
    }

    /// Register as a provider of `feed_id`, locking `bond_amount` as collateral
    /// and quoting `fee_per_query`. One registration per provider and feed.
    pub fn register_provider(
        env: Env,
        provider: Address,
        feed_id: Symbol,
        bond_amount: i128,
        fee_per_query: i128,
    ) -> Result<(), OracleMarketplaceError> {
        provider.require_auth();
        let token_addr = providers::bond_token(&env).ok_or(OracleMarketplaceError::BondTokenNotSet)?;
        if bond_amount <= 0 || fee_per_query < 0 {
            return Err(OracleMarketplaceError::InvalidBond);
        }
        if providers::get_provider(&env, &provider, &feed_id).is_some() {
            return Err(OracleMarketplaceError::ProviderAlreadyRegistered);
        }
        token::Client::new(&env, &token_addr).transfer(&provider, &env.current_contract_address(), &bond_amount);
        providers::set_provider(
            &env,
            &Provider {
                provider: provider.clone(),
                feed_id: feed_id.clone(),
                bond: bond_amount,
                fee_per_query,
                active: true,
                queries_served: 0,
                times_slashed: 0,
                withdraw_after: 0,
            },
        );
        events::emit_provider_registered(&env, &provider, &feed_id, bond_amount, fee_per_query);
        Ok(())
    }

    /// Hire a specific provider for `call_id`: the creator escrows the provider's
    /// per-query fee. One provider per call.
    pub fn hire_provider(
        env: Env,
        creator: Address,
        call_id: u64,
        provider: Address,
        feed_id: Symbol,
    ) -> Result<(), OracleMarketplaceError> {
        creator.require_auth();
        let token_addr = providers::bond_token(&env).ok_or(OracleMarketplaceError::BondTokenNotSet)?;
        let p = providers::get_provider(&env, &provider, &feed_id).ok_or(OracleMarketplaceError::ProviderNotFound)?;
        if !p.active {
            return Err(OracleMarketplaceError::OracleNotActive);
        }
        if providers::get_hire(&env, call_id).is_some() {
            return Err(OracleMarketplaceError::AlreadyHired);
        }
        if p.fee_per_query > 0 {
            token::Client::new(&env, &token_addr).transfer(&creator, &env.current_contract_address(), &p.fee_per_query);
        }
        providers::set_hire(
            &env,
            &Hire {
                call_id,
                creator,
                provider: provider.clone(),
                feed_id: feed_id.clone(),
                fee: p.fee_per_query,
                status: HireStatus::Pending,
            },
        );
        providers::set_pending(&env, &provider, &feed_id, providers::pending(&env, &provider, &feed_id) + 1);
        events::emit_provider_hired(&env, call_id, &provider, &feed_id, p.fee_per_query);
        Ok(())
    }

    /// Confirm the provider resolved `call_id` correctly (admin only): releases
    /// the escrowed fee to the provider.
    pub fn confirm_resolution(env: Env, call_id: u64) -> Result<(), OracleMarketplaceError> {
        get_config(&env).ok_or(OracleMarketplaceError::NotInitialized)?.admin.require_auth();
        let token_addr = providers::bond_token(&env).ok_or(OracleMarketplaceError::BondTokenNotSet)?;
        let mut hire = providers::get_hire(&env, call_id).ok_or(OracleMarketplaceError::HireNotFound)?;
        if hire.status != HireStatus::Pending {
            return Err(OracleMarketplaceError::HireClosed);
        }
        let mut p = providers::get_provider(&env, &hire.provider, &hire.feed_id)
            .ok_or(OracleMarketplaceError::ProviderNotFound)?;

        hire.status = HireStatus::Paid;
        providers::set_hire(&env, &hire);
        p.queries_served += 1;
        providers::set_provider(&env, &p);
        providers::set_pending(&env, &p.provider, &p.feed_id, providers::pending(&env, &p.provider, &p.feed_id) - 1);

        if hire.fee > 0 {
            token::Client::new(&env, &token_addr).transfer(&env.current_contract_address(), &p.provider, &hire.fee);
        }
        events::emit_query_paid(&env, call_id, &p.provider, hire.fee);
        Ok(())
    }

    /// Slash a provider for a verified inaccurate resolution of `call_id` (admin
    /// only). The creator's fee is refunded and `slash_amount` of the provider's
    /// bond (at most the whole bond) is paid to the creator.
    pub fn slash_provider(env: Env, call_id: u64, slash_amount: i128) -> Result<(), OracleMarketplaceError> {
        get_config(&env).ok_or(OracleMarketplaceError::NotInitialized)?.admin.require_auth();
        let token_addr = providers::bond_token(&env).ok_or(OracleMarketplaceError::BondTokenNotSet)?;
        let mut hire = providers::get_hire(&env, call_id).ok_or(OracleMarketplaceError::HireNotFound)?;
        if hire.status != HireStatus::Pending {
            return Err(OracleMarketplaceError::HireClosed);
        }
        let mut p = providers::get_provider(&env, &hire.provider, &hire.feed_id)
            .ok_or(OracleMarketplaceError::ProviderNotFound)?;
        if slash_amount <= 0 || slash_amount > p.bond {
            return Err(OracleMarketplaceError::InvalidSlash);
        }

        hire.status = HireStatus::Slashed;
        providers::set_hire(&env, &hire);
        p.bond -= slash_amount;
        p.times_slashed += 1;
        providers::set_provider(&env, &p);
        providers::set_pending(&env, &p.provider, &p.feed_id, providers::pending(&env, &p.provider, &p.feed_id) - 1);

        token::Client::new(&env, &token_addr).transfer(
            &env.current_contract_address(),
            &hire.creator,
            &(hire.fee + slash_amount),
        );
        events::emit_provider_slashed(&env, call_id, &p.provider, slash_amount);
        Ok(())
    }

    /// Stop accepting hires and start the withdrawal cooldown.
    pub fn deregister_provider(env: Env, provider: Address, feed_id: Symbol) -> Result<(), OracleMarketplaceError> {
        provider.require_auth();
        let config = get_config(&env).ok_or(OracleMarketplaceError::NotInitialized)?;
        let mut p = providers::get_provider(&env, &provider, &feed_id).ok_or(OracleMarketplaceError::ProviderNotFound)?;
        p.active = false;
        p.withdraw_after = env.ledger().timestamp() + config.cooldown_secs;
        providers::set_provider(&env, &p);
        Ok(())
    }

    /// Take back what is left of the bond, after the cooldown and once every
    /// pending hire has been confirmed or slashed. Returns the amount withdrawn.
    pub fn withdraw_provider_bond(env: Env, provider: Address, feed_id: Symbol) -> Result<i128, OracleMarketplaceError> {
        provider.require_auth();
        let token_addr = providers::bond_token(&env).ok_or(OracleMarketplaceError::BondTokenNotSet)?;
        let mut p = providers::get_provider(&env, &provider, &feed_id).ok_or(OracleMarketplaceError::ProviderNotFound)?;
        if p.active || env.ledger().timestamp() < p.withdraw_after {
            return Err(OracleMarketplaceError::WithdrawalLocked);
        }
        if providers::pending(&env, &provider, &feed_id) > 0 {
            return Err(OracleMarketplaceError::PendingHires);
        }
        let amount = p.bond;
        p.bond = 0;
        providers::set_provider(&env, &p);
        if amount > 0 {
            token::Client::new(&env, &token_addr).transfer(&env.current_contract_address(), &provider, &amount);
        }
        Ok(amount)
    }

    pub fn get_provider_info(env: Env, provider: Address, feed_id: Symbol) -> Option<Provider> {
        providers::get_provider(&env, &provider, &feed_id)
    }

    pub fn get_hire(env: Env, call_id: u64) -> Option<Hire> {
        providers::get_hire(&env, call_id)
    }

    pub fn get_call_oracle(env: Env, call_id: u64) -> Option<BytesN<32>> {
        get_call_oracle(&env, call_id)
    }

    pub fn rate_oracle(
        env: Env,
        user: Address,
        oracle_pubkey: BytesN<32>,
        satisfied: bool,
    ) -> Result<(), OracleMarketplaceError> {
        user.require_auth();
        let _oracle = get_oracle(&env, &oracle_pubkey).ok_or(OracleMarketplaceError::OracleNotFound)?;

        let mut ratings = get_oracle_ratings(&env, &oracle_pubkey);
        if ratings.contains_key(user.clone()) {
            return Err(OracleMarketplaceError::AlreadyRated);
        }

        ratings.set(user.clone(), satisfied);
        set_oracle_ratings(&env, &oracle_pubkey, &ratings);

        emit_oracle_rated(&env, &oracle_pubkey, &user, satisfied);
        Ok(())
    }

    pub fn get_oracle_metrics(
        env: Env,
        oracle_pubkey: BytesN<32>,
    ) -> Result<(u64, u64), OracleMarketplaceError> {
        let oracle = get_oracle(&env, &oracle_pubkey).ok_or(OracleMarketplaceError::OracleNotFound)?;
        Ok((oracle.total_resolved, oracle.total_disputes))
    }

    pub fn get_config_view(env: Env) -> Result<MarketplaceConfig, OracleMarketplaceError> {
        get_config(&env).ok_or(OracleMarketplaceError::NotInitialized)
    }
}
