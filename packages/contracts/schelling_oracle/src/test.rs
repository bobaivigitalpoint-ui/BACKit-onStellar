#![cfg(test)]

extern crate std;

use crate::{errors::OracleError, SchellingOracle, SchellingOracleClient};
use soroban_sdk::{
    testutils::{Address as _, Ledger as _},
    token::{Client as TokenClient, StellarAssetClient},
    Address, Bytes, BytesN, Env,
};

const ORIGINAL: u32 = 1;
const DISPUTED: u32 = 2;

fn setup_token(env: &Env, admin: &Address) -> Address {
    let token = env.register_stellar_asset_contract_v2(admin.clone());
    let sac = token.address();
    StellarAssetClient::new(env, &sac).mint(admin, &1_000_000_000_000);
    sac
}

fn salt(env: &Env, tag: u8) -> BytesN<32> {
    let mut bytes = [0u8; 32];
    bytes[0] = tag;
    BytesN::from_array(env, &bytes)
}

fn commit_hash(env: &Env, voter: &Address, vote_outcome: u32, salt: &BytesN<32>) -> BytesN<32> {
    use soroban_sdk::xdr::ToXdr;
    let mut raw = Bytes::from_slice(env, b"schelling_vote:");
    raw.append(&voter.clone().to_xdr(env));
    raw.append(&Bytes::from_slice(env, &vote_outcome.to_be_bytes()));
    raw.append(&Bytes::from_slice(env, &salt.to_array()));
    env.crypto().sha256(&raw).into()
}

fn assert_contract_error<T, IE>(
    result: Result<Result<T, IE>, Result<OracleError, soroban_sdk::InvokeError>>,
    expected: OracleError,
) {
    assert!(matches!(
        result,
        Err(Ok(err)) if err == expected
    ));
}

/// Fund `who` with `amount` of `token` from `admin`.
fn fund(env: &Env, token: &Address, admin: &Address, who: &Address, amount: i128) {
    TokenClient::new(env, token).transfer(admin, who, &amount);
}

struct Harness {
    env: Env,
    client: SchellingOracleClient<'static>,
    admin: Address,
    token: Address,
}

fn setup(voting_period_secs: u64, reveal_period_secs: u64, bond_bps: u32) -> Harness {
    let env = Env::default();
    env.mock_all_auths();
    let admin = Address::generate(&env);
    let token = setup_token(&env, &admin);

    let contract_id = env.register(SchellingOracle, ());
    let client = SchellingOracleClient::new(&env, &contract_id);
    client.initialize(&admin, &voting_period_secs, &reveal_period_secs, &bond_bps);

    Harness {
        env,
        client,
        admin,
        token,
    }
}

fn advance(env: &Env, secs: u64) {
    env.ledger().with_mut(|li| {
        li.timestamp += secs;
    });
}

// ─── Basic lifecycle / admin ────────────────────────────────────────────────

#[test]
fn initialize_rejects_double_init() {
    let h = setup(86_400, 86_400, 500);
    let result = h.client.try_initialize(&h.admin, &86_400, &86_400, &500);
    assert_contract_error(result, OracleError::AlreadyInitialized);
}

#[test]
fn set_dispute_params_requires_admin_match() {
    let h = setup(86_400, 86_400, 500);
    let not_admin = Address::generate(&h.env);
    // mock_all_auths bypasses signature checks but not the address-equality
    // gate we apply before require_auth.
    let result = h
        .client
        .try_set_dispute_params(&not_admin, &3_600, &3_600, &500);
    assert_contract_error(result, OracleError::Unauthorized);
}

#[test]
fn min_bond_amount_matches_bond_bps() {
    let h = setup(86_400, 86_400, 500); // 5%
    assert_eq!(h.client.min_bond_amount(&1_000_000), 50_000);
}

// ─── dispute_outcome validation ─────────────────────────────────────────────

#[test]
fn dispute_outcome_rejects_bond_below_minimum() {
    let h = setup(86_400, 86_400, 500);
    let disputer = Address::generate(&h.env);
    fund(&h.env, &h.token, &h.admin, &disputer, 1_000_000);

    let result = h.client.try_dispute_outcome(
        &disputer,
        &1u64,
        &ORIGINAL,
        &DISPUTED,
        &1_000_000i128,
        &h.token,
        &49_999i128, // one below the required 5%
    );
    assert_contract_error(result, OracleError::BondBelowMinimum);
}

#[test]
fn dispute_outcome_rejects_same_outcome() {
    let h = setup(86_400, 86_400, 500);
    let disputer = Address::generate(&h.env);
    fund(&h.env, &h.token, &h.admin, &disputer, 1_000_000);

    let result = h.client.try_dispute_outcome(
        &disputer,
        &1u64,
        &ORIGINAL,
        &ORIGINAL,
        &1_000_000i128,
        &h.token,
        &50_000i128,
    );
    assert_contract_error(result, OracleError::SameOutcome);
}

#[test]
fn dispute_outcome_escrows_the_bond() {
    let h = setup(86_400, 86_400, 500);
    let disputer = Address::generate(&h.env);
    fund(&h.env, &h.token, &h.admin, &disputer, 1_000_000);

    let contract_balance_before = TokenClient::new(&h.env, &h.token).balance(&h.client.address);
    h.client.dispute_outcome(
        &disputer,
        &1u64,
        &ORIGINAL,
        &DISPUTED,
        &1_000_000i128,
        &h.token,
        &50_000i128,
    );
    let contract_balance_after = TokenClient::new(&h.env, &h.token).balance(&h.client.address);
    assert_eq!(contract_balance_after - contract_balance_before, 50_000);
    assert_eq!(
        TokenClient::new(&h.env, &h.token).balance(&disputer),
        1_000_000 - 50_000
    );
}

// ─── Commit-reveal timing ───────────────────────────────────────────────────

#[test]
fn vote_on_dispute_rejects_after_commit_deadline() {
    let h = setup(1_000, 1_000, 500);
    let disputer = Address::generate(&h.env);
    let voter = Address::generate(&h.env);
    fund(&h.env, &h.token, &h.admin, &disputer, 1_000_000);
    fund(&h.env, &h.token, &h.admin, &voter, 1_000_000);

    let dispute_id = h.client.dispute_outcome(
        &disputer, &1u64, &ORIGINAL, &DISPUTED, &1_000_000i128, &h.token, &50_000i128,
    );

    advance(&h.env, 1_001);
    let s = salt(&h.env, 1);
    let hash = commit_hash(&h.env, &voter, ORIGINAL, &s);
    let result = h
        .client
        .try_vote_on_dispute(&voter, &dispute_id, &hash, &10_000i128);
    assert_contract_error(result, OracleError::CommitPeriodEnded);
}

#[test]
fn reveal_vote_rejects_before_commit_deadline() {
    let h = setup(1_000, 1_000, 500);
    let disputer = Address::generate(&h.env);
    let voter = Address::generate(&h.env);
    fund(&h.env, &h.token, &h.admin, &disputer, 1_000_000);
    fund(&h.env, &h.token, &h.admin, &voter, 1_000_000);

    let dispute_id = h.client.dispute_outcome(
        &disputer, &1u64, &ORIGINAL, &DISPUTED, &1_000_000i128, &h.token, &50_000i128,
    );
    let s = salt(&h.env, 1);
    let hash = commit_hash(&h.env, &voter, ORIGINAL, &s);
    h.client.vote_on_dispute(&voter, &dispute_id, &hash, &10_000i128);

    let result = h
        .client
        .try_reveal_vote(&voter, &dispute_id, &ORIGINAL, &s);
    assert_contract_error(result, OracleError::RevealPeriodNotStarted);
}

#[test]
fn reveal_vote_rejects_after_reveal_deadline() {
    let h = setup(1_000, 1_000, 500);
    let disputer = Address::generate(&h.env);
    let voter = Address::generate(&h.env);
    fund(&h.env, &h.token, &h.admin, &disputer, 1_000_000);
    fund(&h.env, &h.token, &h.admin, &voter, 1_000_000);

    let dispute_id = h.client.dispute_outcome(
        &disputer, &1u64, &ORIGINAL, &DISPUTED, &1_000_000i128, &h.token, &50_000i128,
    );
    let s = salt(&h.env, 1);
    let hash = commit_hash(&h.env, &voter, ORIGINAL, &s);
    h.client.vote_on_dispute(&voter, &dispute_id, &hash, &10_000i128);

    advance(&h.env, 2_001);
    let result = h
        .client
        .try_reveal_vote(&voter, &dispute_id, &ORIGINAL, &s);
    assert_contract_error(result, OracleError::RevealPeriodEnded);
}

#[test]
fn reveal_vote_rejects_hash_mismatch() {
    let h = setup(1_000, 1_000, 500);
    let disputer = Address::generate(&h.env);
    let voter = Address::generate(&h.env);
    fund(&h.env, &h.token, &h.admin, &disputer, 1_000_000);
    fund(&h.env, &h.token, &h.admin, &voter, 1_000_000);

    let dispute_id = h.client.dispute_outcome(
        &disputer, &1u64, &ORIGINAL, &DISPUTED, &1_000_000i128, &h.token, &50_000i128,
    );
    let s = salt(&h.env, 1);
    let hash = commit_hash(&h.env, &voter, ORIGINAL, &s);
    h.client.vote_on_dispute(&voter, &dispute_id, &hash, &10_000i128);

    advance(&h.env, 1_001);
    // Reveal with a different outcome than committed -> hash mismatch.
    let result = h
        .client
        .try_reveal_vote(&voter, &dispute_id, &DISPUTED, &s);
    assert_contract_error(result, OracleError::CommitmentMismatch);

    // Also try the correct outcome but a wrong salt.
    let wrong_salt = salt(&h.env, 99);
    let result2 = h
        .client
        .try_reveal_vote(&voter, &dispute_id, &ORIGINAL, &wrong_salt);
    assert_contract_error(result2, OracleError::CommitmentMismatch);
}

#[test]
fn resolve_dispute_rejects_before_reveal_deadline() {
    let h = setup(1_000, 1_000, 500);
    let disputer = Address::generate(&h.env);
    fund(&h.env, &h.token, &h.admin, &disputer, 1_000_000);
    let dispute_id = h.client.dispute_outcome(
        &disputer, &1u64, &ORIGINAL, &DISPUTED, &1_000_000i128, &h.token, &50_000i128,
    );
    let result = h.client.try_resolve_dispute(&dispute_id);
    assert_contract_error(result, OracleError::RevealPeriodNotEnded);
}

// ─── Game theory scenarios ──────────────────────────────────────────────────

/// Honest majority (siding with the original oracle outcome) wins: the
/// disputer's bond is distributed pro-rata to the voters who correctly
/// upheld the original outcome, weighted by stake.
#[test]
fn honest_majority_wins_disputer_loses_bond() {
    let h = setup(1_000, 1_000, 500); // 5% bond
    let disputer = Address::generate(&h.env);
    let v1 = Address::generate(&h.env); // votes ORIGINAL, stake 30_000
    let v2 = Address::generate(&h.env); // votes ORIGINAL, stake 70_000
    let v3 = Address::generate(&h.env); // votes DISPUTED, stake 20_000 (minority, loses stake)

    fund(&h.env, &h.token, &h.admin, &disputer, 1_000_000);
    fund(&h.env, &h.token, &h.admin, &v1, 1_000_000);
    fund(&h.env, &h.token, &h.admin, &v2, 1_000_000);
    fund(&h.env, &h.token, &h.admin, &v3, 1_000_000);

    let bond = 50_000i128; // 5% of 1_000_000
    let dispute_id = h.client.dispute_outcome(
        &disputer, &1u64, &ORIGINAL, &DISPUTED, &1_000_000i128, &h.token, &bond,
    );

    let s1 = salt(&h.env, 1);
    let s2 = salt(&h.env, 2);
    let s3 = salt(&h.env, 3);
    h.client.vote_on_dispute(&v1, &dispute_id, &commit_hash(&h.env, &v1, ORIGINAL, &s1), &30_000);
    h.client.vote_on_dispute(&v2, &dispute_id, &commit_hash(&h.env, &v2, ORIGINAL, &s2), &70_000);
    h.client.vote_on_dispute(&v3, &dispute_id, &commit_hash(&h.env, &v3, DISPUTED, &s3), &20_000);

    advance(&h.env, 1_001);
    h.client.reveal_vote(&v1, &dispute_id, &ORIGINAL, &s1);
    h.client.reveal_vote(&v2, &dispute_id, &ORIGINAL, &s2);
    h.client.reveal_vote(&v3, &dispute_id, &DISPUTED, &s3);

    advance(&h.env, 1_001);

    let disputer_balance_before = TokenClient::new(&h.env, &h.token).balance(&disputer);
    let v1_balance_before = TokenClient::new(&h.env, &h.token).balance(&v1);
    let v2_balance_before = TokenClient::new(&h.env, &h.token).balance(&v2);
    let v3_balance_before = TokenClient::new(&h.env, &h.token).balance(&v3);

    h.client.resolve_dispute(&dispute_id);

    // Disputer loses: gets nothing back, no change in balance.
    assert_eq!(
        TokenClient::new(&h.env, &h.token).balance(&disputer),
        disputer_balance_before
    );

    // Reward pool = bond (50_000) + disputed_total (20_000) = 70_000,
    // split pro-rata over original_total = 100_000.
    // v1: stake 30_000 back + 30_000/100_000 * 70_000 = 30_000 + 21_000 = 51_000
    // v2: stake 70_000 back + 70_000/100_000 * 70_000 = 70_000 + 49_000 = 119_000
    let v1_payout = TokenClient::new(&h.env, &h.token).balance(&v1) - v1_balance_before;
    let v2_payout = TokenClient::new(&h.env, &h.token).balance(&v2) - v2_balance_before;
    assert_eq!(v1_payout, 51_000);
    assert_eq!(v2_payout, 119_000);

    // v3 (the losing minority) gets nothing back — their stake was forfeited.
    assert_eq!(
        TokenClient::new(&h.env, &h.token).balance(&v3),
        v3_balance_before
    );

    let dispute = h.client.get_dispute(&dispute_id);
    assert_eq!(dispute.result, crate::types::DisputeResult::DisputerLost);
}

/// Dishonest majority: more revealed stake sides with the disputer, so the
/// disputer wins and captures the bond back plus the losing voters' stake.
#[test]
fn dishonest_majority_disputer_wins() {
    let h = setup(1_000, 1_000, 500);
    let disputer = Address::generate(&h.env);
    let v1 = Address::generate(&h.env); // votes DISPUTED, stake 80_000 (majority, correct)
    let v2 = Address::generate(&h.env); // votes ORIGINAL, stake 40_000 (minority, wrong, forfeits)

    fund(&h.env, &h.token, &h.admin, &disputer, 1_000_000);
    fund(&h.env, &h.token, &h.admin, &v1, 1_000_000);
    fund(&h.env, &h.token, &h.admin, &v2, 1_000_000);

    let bond = 50_000i128;
    let dispute_id = h.client.dispute_outcome(
        &disputer, &1u64, &ORIGINAL, &DISPUTED, &1_000_000i128, &h.token, &bond,
    );

    let s1 = salt(&h.env, 1);
    let s2 = salt(&h.env, 2);
    h.client.vote_on_dispute(&v1, &dispute_id, &commit_hash(&h.env, &v1, DISPUTED, &s1), &80_000);
    h.client.vote_on_dispute(&v2, &dispute_id, &commit_hash(&h.env, &v2, ORIGINAL, &s2), &40_000);

    advance(&h.env, 1_001);
    h.client.reveal_vote(&v1, &dispute_id, &DISPUTED, &s1);
    h.client.reveal_vote(&v2, &dispute_id, &ORIGINAL, &s2);
    advance(&h.env, 1_001);

    let disputer_balance_before = TokenClient::new(&h.env, &h.token).balance(&disputer);
    let v1_balance_before = TokenClient::new(&h.env, &h.token).balance(&v1);
    let v2_balance_before = TokenClient::new(&h.env, &h.token).balance(&v2);

    h.client.resolve_dispute(&dispute_id);

    // Disputer gets bond back (50_000) + the losing voters' forfeited stake
    // (v2's 40_000) = 90_000.
    let disputer_payout = TokenClient::new(&h.env, &h.token).balance(&disputer) - disputer_balance_before;
    assert_eq!(disputer_payout, 90_000);

    // v1 (correct, sided with disputer) just gets their own stake back.
    let v1_payout = TokenClient::new(&h.env, &h.token).balance(&v1) - v1_balance_before;
    assert_eq!(v1_payout, 80_000);

    // v2 (wrong) gets nothing back.
    assert_eq!(
        TokenClient::new(&h.env, &h.token).balance(&v2),
        v2_balance_before
    );

    let dispute = h.client.get_dispute(&dispute_id);
    assert_eq!(dispute.result, crate::types::DisputeResult::DisputerWon);
}

/// Unrevealed votes are forfeited entirely regardless of which side wins,
/// and their stake feeds into the winning side's reward pool.
#[test]
fn unrevealed_votes_are_forfeited() {
    let h = setup(1_000, 1_000, 500);
    let disputer = Address::generate(&h.env);
    let v1 = Address::generate(&h.env); // votes ORIGINAL, stake 50_000, reveals
    let v2 = Address::generate(&h.env); // commits stake 30_000, never reveals

    fund(&h.env, &h.token, &h.admin, &disputer, 1_000_000);
    fund(&h.env, &h.token, &h.admin, &v1, 1_000_000);
    fund(&h.env, &h.token, &h.admin, &v2, 1_000_000);

    let bond = 50_000i128;
    let dispute_id = h.client.dispute_outcome(
        &disputer, &1u64, &ORIGINAL, &DISPUTED, &1_000_000i128, &h.token, &bond,
    );

    let s1 = salt(&h.env, 1);
    let s2 = salt(&h.env, 2);
    h.client.vote_on_dispute(&v1, &dispute_id, &commit_hash(&h.env, &v1, ORIGINAL, &s1), &50_000);
    h.client.vote_on_dispute(&v2, &dispute_id, &commit_hash(&h.env, &v2, ORIGINAL, &s2), &30_000);

    advance(&h.env, 1_001);
    h.client.reveal_vote(&v1, &dispute_id, &ORIGINAL, &s1);
    // v2 never reveals.
    advance(&h.env, 1_001);

    let v1_balance_before = TokenClient::new(&h.env, &h.token).balance(&v1);
    let v2_balance_before = TokenClient::new(&h.env, &h.token).balance(&v2);
    let contract_balance_before = TokenClient::new(&h.env, &h.token).balance(&h.client.address);

    h.client.resolve_dispute(&dispute_id);

    // Original side wins (only revealed voter was v1, on ORIGINAL side).
    // reward_pool = bond (50_000) + disputed_total (0) + unrevealed (30_000) = 80_000
    // v1 gets stake back (50_000) + 100% share (50_000/50_000 * 80_000) = 130_000
    let v1_payout = TokenClient::new(&h.env, &h.token).balance(&v1) - v1_balance_before;
    assert_eq!(v1_payout, 130_000);

    // v2 never reveals -> gets nothing back, permanently forfeited.
    assert_eq!(
        TokenClient::new(&h.env, &h.token).balance(&v2),
        v2_balance_before
    );

    // Contract's balance should now be fully drained for this dispute
    // (bond + both voters' stake all left the contract to v1).
    let contract_balance_after = TokenClient::new(&h.env, &h.token).balance(&h.client.address);
    assert_eq!(contract_balance_before - contract_balance_after, 130_000);
}

/// If nobody ever votes at all, the dispute resolves as Void and the
/// disputer simply gets their bond back — no winners, no losers.
#[test]
fn no_voters_resolves_void_and_refunds_bond() {
    let h = setup(1_000, 1_000, 500);
    let disputer = Address::generate(&h.env);
    fund(&h.env, &h.token, &h.admin, &disputer, 1_000_000);

    let bond = 50_000i128;
    let dispute_id = h.client.dispute_outcome(
        &disputer, &1u64, &ORIGINAL, &DISPUTED, &1_000_000i128, &h.token, &bond,
    );

    advance(&h.env, 2_001);
    let balance_before = TokenClient::new(&h.env, &h.token).balance(&disputer);
    h.client.resolve_dispute(&dispute_id);
    let balance_after = TokenClient::new(&h.env, &h.token).balance(&disputer);
    assert_eq!(balance_after - balance_before, bond);

    let dispute = h.client.get_dispute(&dispute_id);
    assert_eq!(dispute.result, crate::types::DisputeResult::Void);
}

/// Everyone commits but nobody reveals: the "winning" side (original,
/// since 0 == 0 defaults to disputer-loses) has zero revealed stake, so the
/// whole pool (bond + all forfeited stake) routes to the admin/treasury
/// instead of panicking on a division by zero.
#[test]
fn all_unrevealed_routes_pool_to_admin() {
    let h = setup(1_000, 1_000, 500);
    let disputer = Address::generate(&h.env);
    let v1 = Address::generate(&h.env);

    fund(&h.env, &h.token, &h.admin, &disputer, 1_000_000);
    fund(&h.env, &h.token, &h.admin, &v1, 1_000_000);

    let bond = 50_000i128;
    let dispute_id = h.client.dispute_outcome(
        &disputer, &1u64, &ORIGINAL, &DISPUTED, &1_000_000i128, &h.token, &bond,
    );
    let s1 = salt(&h.env, 1);
    h.client.vote_on_dispute(&v1, &dispute_id, &commit_hash(&h.env, &v1, ORIGINAL, &s1), &40_000);

    advance(&h.env, 2_001); // skip straight past the reveal deadline, no reveal happens

    let admin_balance_before = TokenClient::new(&h.env, &h.token).balance(&h.admin);
    h.client.resolve_dispute(&dispute_id);
    let admin_balance_after = TokenClient::new(&h.env, &h.token).balance(&h.admin);

    // bond (50_000) + unrevealed (40_000) = 90_000 all routed to admin.
    assert_eq!(admin_balance_after - admin_balance_before, 90_000);
}

/// Multiple concurrent disputes (on different call_ids) must not leak state
/// into each other: separate voter lists, separate bond escrow, separate
/// resolution outcomes.
#[test]
fn multiple_concurrent_disputes_do_not_interfere() {
    let h = setup(1_000, 1_000, 500);

    let disputer_a = Address::generate(&h.env);
    let disputer_b = Address::generate(&h.env);
    let voter_a = Address::generate(&h.env);
    let voter_b = Address::generate(&h.env);

    fund(&h.env, &h.token, &h.admin, &disputer_a, 1_000_000);
    fund(&h.env, &h.token, &h.admin, &disputer_b, 1_000_000);
    fund(&h.env, &h.token, &h.admin, &voter_a, 1_000_000);
    fund(&h.env, &h.token, &h.admin, &voter_b, 1_000_000);

    // Dispute A: disputer wins.
    let dispute_a = h.client.dispute_outcome(
        &disputer_a, &1u64, &ORIGINAL, &DISPUTED, &1_000_000i128, &h.token, &50_000i128,
    );
    // Dispute B: disputer loses.
    let dispute_b = h.client.dispute_outcome(
        &disputer_b, &2u64, &ORIGINAL, &DISPUTED, &1_000_000i128, &h.token, &50_000i128,
    );
    assert_ne!(dispute_a, dispute_b);

    let sa = salt(&h.env, 10);
    let sb = salt(&h.env, 20);
    // voter_a votes DISPUTED on dispute A (helps disputer_a win).
    h.client.vote_on_dispute(&voter_a, &dispute_a, &commit_hash(&h.env, &voter_a, DISPUTED, &sa), &100_000);
    // voter_b votes ORIGINAL on dispute B (helps disputer_b lose).
    h.client.vote_on_dispute(&voter_b, &dispute_b, &commit_hash(&h.env, &voter_b, ORIGINAL, &sb), &100_000);

    advance(&h.env, 1_001);
    h.client.reveal_vote(&voter_a, &dispute_a, &DISPUTED, &sa);
    h.client.reveal_vote(&voter_b, &dispute_b, &ORIGINAL, &sb);
    advance(&h.env, 1_001);

    h.client.resolve_dispute(&dispute_a);
    h.client.resolve_dispute(&dispute_b);

    let a = h.client.get_dispute(&dispute_a);
    let b = h.client.get_dispute(&dispute_b);
    assert_eq!(a.result, crate::types::DisputeResult::DisputerWon);
    assert_eq!(b.result, crate::types::DisputeResult::DisputerLost);

    // Cross-check: voter_a never touched dispute B and vice versa.
    let voter_a_on_b = h.client.try_get_commitment(&dispute_b, &voter_a);
    assert!(voter_a_on_b.is_err());
    let voter_b_on_a = h.client.try_get_commitment(&dispute_a, &voter_b);
    assert!(voter_b_on_a.is_err());
}

#[test]
fn double_resolve_rejected() {
    let h = setup(1_000, 1_000, 500);
    let disputer = Address::generate(&h.env);
    fund(&h.env, &h.token, &h.admin, &disputer, 1_000_000);
    let dispute_id = h.client.dispute_outcome(
        &disputer, &1u64, &ORIGINAL, &DISPUTED, &1_000_000i128, &h.token, &50_000i128,
    );
    advance(&h.env, 2_001);
    h.client.resolve_dispute(&dispute_id);
    let result = h.client.try_resolve_dispute(&dispute_id);
    assert_contract_error(result, OracleError::AlreadyResolved);
}

#[test]
fn double_commit_rejected() {
    let h = setup(1_000, 1_000, 500);
    let disputer = Address::generate(&h.env);
    let voter = Address::generate(&h.env);
    fund(&h.env, &h.token, &h.admin, &disputer, 1_000_000);
    fund(&h.env, &h.token, &h.admin, &voter, 1_000_000);
    let dispute_id = h.client.dispute_outcome(
        &disputer, &1u64, &ORIGINAL, &DISPUTED, &1_000_000i128, &h.token, &50_000i128,
    );
    let s = salt(&h.env, 1);
    h.client.vote_on_dispute(&voter, &dispute_id, &commit_hash(&h.env, &voter, ORIGINAL, &s), &10_000);
    let result = h.client.try_vote_on_dispute(
        &voter,
        &dispute_id,
        &commit_hash(&h.env, &voter, ORIGINAL, &s),
        &10_000,
    );
    assert_contract_error(result, OracleError::AlreadyCommitted);
}

#[test]
fn double_reveal_rejected() {
    let h = setup(1_000, 1_000, 500);
    let disputer = Address::generate(&h.env);
    let voter = Address::generate(&h.env);
    fund(&h.env, &h.token, &h.admin, &disputer, 1_000_000);
    fund(&h.env, &h.token, &h.admin, &voter, 1_000_000);
    let dispute_id = h.client.dispute_outcome(
        &disputer, &1u64, &ORIGINAL, &DISPUTED, &1_000_000i128, &h.token, &50_000i128,
    );
    let s = salt(&h.env, 1);
    h.client.vote_on_dispute(&voter, &dispute_id, &commit_hash(&h.env, &voter, ORIGINAL, &s), &10_000);
    advance(&h.env, 1_001);
    h.client.reveal_vote(&voter, &dispute_id, &ORIGINAL, &s);
    let result = h.client.try_reveal_vote(&voter, &dispute_id, &ORIGINAL, &s);
    assert_contract_error(result, OracleError::AlreadyRevealed);
}

#[test]
fn reveal_vote_rejects_invalid_outcome() {
    let h = setup(1_000, 1_000, 500);
    let disputer = Address::generate(&h.env);
    let voter = Address::generate(&h.env);
    fund(&h.env, &h.token, &h.admin, &disputer, 1_000_000);
    fund(&h.env, &h.token, &h.admin, &voter, 1_000_000);
    let dispute_id = h.client.dispute_outcome(
        &disputer, &1u64, &ORIGINAL, &DISPUTED, &1_000_000i128, &h.token, &50_000i128,
    );
    let s = salt(&h.env, 1);
    // Commit to some third outcome value entirely.
    h.client.vote_on_dispute(&voter, &dispute_id, &commit_hash(&h.env, &voter, 99, &s), &10_000);
    advance(&h.env, 1_001);
    let result = h.client.try_reveal_vote(&voter, &dispute_id, &99u32, &s);
    assert_contract_error(result, OracleError::InvalidVoteOutcome);
}

/// Tie: equal revealed stake on both sides defaults to the disputer losing.
#[test]
fn tie_defaults_to_disputer_losing() {
    let h = setup(1_000, 1_000, 500);
    let disputer = Address::generate(&h.env);
    let v1 = Address::generate(&h.env);
    let v2 = Address::generate(&h.env);
    fund(&h.env, &h.token, &h.admin, &disputer, 1_000_000);
    fund(&h.env, &h.token, &h.admin, &v1, 1_000_000);
    fund(&h.env, &h.token, &h.admin, &v2, 1_000_000);

    let dispute_id = h.client.dispute_outcome(
        &disputer, &1u64, &ORIGINAL, &DISPUTED, &1_000_000i128, &h.token, &50_000i128,
    );
    let s1 = salt(&h.env, 1);
    let s2 = salt(&h.env, 2);
    h.client.vote_on_dispute(&v1, &dispute_id, &commit_hash(&h.env, &v1, ORIGINAL, &s1), &50_000);
    h.client.vote_on_dispute(&v2, &dispute_id, &commit_hash(&h.env, &v2, DISPUTED, &s2), &50_000);
    advance(&h.env, 1_001);
    h.client.reveal_vote(&v1, &dispute_id, &ORIGINAL, &s1);
    h.client.reveal_vote(&v2, &dispute_id, &DISPUTED, &s2);
    advance(&h.env, 1_001);
    h.client.resolve_dispute(&dispute_id);

    let dispute = h.client.get_dispute(&dispute_id);
    assert_eq!(dispute.result, crate::types::DisputeResult::DisputerLost);
}

// ─── SC-022: dispute commitments are bound to the voter ─────────────────────

#[test]
fn a_copied_dispute_commitment_cannot_be_revealed_by_the_copycat() {
    let h = setup(1_000, 1_000, 500);
    let disputer = Address::generate(&h.env);
    let honest = Address::generate(&h.env);
    let copycat = Address::generate(&h.env);
    for who in [&disputer, &honest, &copycat] {
        fund(&h.env, &h.token, &h.admin, who, 1_000_000);
    }
    let dispute_id = h.client.dispute_outcome(
        &disputer, &1u64, &ORIGINAL, &DISPUTED, &1_000_000i128, &h.token, &50_000i128,
    );

    let s = salt(&h.env, 7);
    let hash = commit_hash(&h.env, &honest, DISPUTED, &s);
    h.client.vote_on_dispute(&honest, &dispute_id, &hash, &10_000i128);
    // The copycat saw the hash on chain and submits it as their own.
    h.client.vote_on_dispute(&copycat, &dispute_id, &hash, &10_000i128);

    advance(&h.env, 1_001);
    h.client.reveal_vote(&honest, &dispute_id, &DISPUTED, &s);
    // Once the honest voter reveals (outcome, salt), the copycat knows both, but
    // the hash does not match under the copycat's own address.
    let result = h.client.try_reveal_vote(&copycat, &dispute_id, &DISPUTED, &s);
    assert_contract_error(result, OracleError::CommitmentMismatch);
}

// ─── SC-021: stake-weighted price consensus ─────────────────────────────────

struct Voters {
    list: std::vec::Vec<Address>,
}

fn funded_voters(h: &Harness, n: usize) -> Voters {
    let list: std::vec::Vec<Address> = (0..n).map(|_| Address::generate(&h.env)).collect();
    for v in &list {
        fund(&h.env, &h.token, &h.admin, v, 1_000_000);
    }
    Voters { list }
}

fn balance(h: &Harness, who: &Address) -> i128 {
    TokenClient::new(&h.env, &h.token).balance(who)
}

fn open_plain(h: &Harness, market: u64, secs: u64) {
    let now = h.env.ledger().timestamp();
    h.client
        .open_price_round(&h.admin, &market, &h.token, &(now + secs), &(now + secs), &false);
}

#[test]
fn consensus_voters_are_rewarded_and_outliers_slashed_ten_percent() {
    let h = setup(1_000, 1_000, 500);
    let v = funded_voters(&h, 5);
    open_plain(&h, 1, 100);

    // Prices 99, 100, 100, 101 and an outlier at 500, 1_000 stake each.
    for (voter, price) in v.list.iter().zip([99i128, 100, 100, 101, 500]) {
        h.client.submit_price_vote(voter, &1, &price, &1_000);
    }
    advance(&h.env, 101);

    let median = h.client.finalize_price(&1);

    assert_eq!(median, 100);
    let round = h.client.get_price_round(&1);
    assert!(round.finalized);
    assert_eq!(round.median, 100);
    // Outlier loses 10% (100); the four consensus voters split it: 25 each.
    for consensus in &v.list[..4] {
        assert_eq!(balance(&h, consensus), 1_000_000 + 25);
    }
    assert_eq!(balance(&h, &v.list[4]), 1_000_000 - 100);
    // Every token that came in went back out.
    assert_eq!(balance(&h, &h.client.address), 0);
}

#[test]
fn the_median_is_weighted_by_stake_not_by_head_count() {
    let h = setup(1_000, 1_000, 500);
    let v = funded_voters(&h, 3);
    open_plain(&h, 2, 100);

    h.client.submit_price_vote(&v.list[0], &2, &10, &100);
    h.client.submit_price_vote(&v.list[1], &2, &20, &100);
    h.client.submit_price_vote(&v.list[2], &2, &30, &5_000);
    advance(&h.env, 101);

    // A plain median would be 20; the 5_000 stake on 30 decides it.
    assert_eq!(h.client.finalize_price(&2), 30);
}

#[test]
fn rounding_remainder_goes_to_the_last_consensus_voter_so_nothing_is_stranded() {
    let h = setup(1_000, 1_000, 500);
    let v = funded_voters(&h, 4);
    open_plain(&h, 3, 100);

    // Three consensus voters share the outlier's 10% slash of 101 -> 10 (not divisible by 3).
    for (voter, price) in v.list.iter().zip([100i128, 100, 100, 100_000]) {
        let stake = if price == 100_000 { 101 } else { 1_000 };
        h.client.submit_price_vote(voter, &3, &price, &stake);
    }
    advance(&h.env, 101);
    h.client.finalize_price(&3);

    assert_eq!(balance(&h, &h.client.address), 0);
    let gained: i128 = v.list[..3].iter().map(|a| balance(&h, a) - 1_000_000).sum();
    assert_eq!(gained, 10);
}

#[test]
fn price_votes_are_validated() {
    let h = setup(1_000, 1_000, 500);
    let v = funded_voters(&h, 1);
    open_plain(&h, 4, 100);

    assert_contract_error(
        h.client.try_submit_price_vote(&v.list[0], &4, &0, &1_000),
        OracleError::InvalidPrice,
    );
    assert_contract_error(
        h.client.try_submit_price_vote(&v.list[0], &4, &100, &0),
        OracleError::InvalidStakeAmount,
    );
    assert_contract_error(
        h.client.try_submit_price_vote(&v.list[0], &99, &100, &1_000),
        OracleError::PriceRoundNotFound,
    );
    h.client.submit_price_vote(&v.list[0], &4, &100, &1_000);
    assert_contract_error(
        h.client.try_submit_price_vote(&v.list[0], &4, &100, &1_000),
        OracleError::AlreadyVoted,
    );
}

#[test]
fn a_round_cannot_be_finalized_early_or_twice_and_rejects_late_votes() {
    let h = setup(1_000, 1_000, 500);
    let v = funded_voters(&h, 2);
    open_plain(&h, 5, 100);
    h.client.submit_price_vote(&v.list[0], &5, &100, &1_000);

    assert_contract_error(h.client.try_finalize_price(&5), OracleError::PriceVotingOpen);
    advance(&h.env, 100);
    assert_contract_error(
        h.client.try_submit_price_vote(&v.list[1], &5, &100, &1_000),
        OracleError::PriceVotingClosed,
    );
    h.client.finalize_price(&5);
    assert_contract_error(h.client.try_finalize_price(&5), OracleError::AlreadyFinalized);
}

#[test]
fn only_the_admin_opens_rounds_and_a_market_has_one_round() {
    let h = setup(1_000, 1_000, 500);
    let stranger = Address::generate(&h.env);
    let now = h.env.ledger().timestamp();
    assert_contract_error(
        h.client
            .try_open_price_round(&stranger, &6, &h.token, &(now + 10), &(now + 10), &false),
        OracleError::Unauthorized,
    );
    open_plain(&h, 6, 10);
    assert_contract_error(
        h.client
            .try_open_price_round(&h.admin, &6, &h.token, &(now + 10), &(now + 10), &false),
        OracleError::PriceRoundExists,
    );
    assert_contract_error(
        h.client
            .try_open_price_round(&h.admin, &7, &h.token, &now, &now, &false),
        OracleError::InvalidPeriod,
    );
}

// ─── SC-022: commit-reveal price rounds ─────────────────────────────────────

fn open_commit_reveal(h: &Harness, market: u64) {
    let now = h.env.ledger().timestamp();
    h.client
        .open_price_round(&h.admin, &market, &h.token, &(now + 100), &(now + 200), &true);
}

fn commit(h: &Harness, voter: &Address, market: u64, price: i128, tag: u8, stake: i128) -> BytesN<32> {
    let s = salt(&h.env, tag);
    let hash = h.client.price_commitment_hash(voter, &price, &s);
    h.client.commit_price_vote(voter, &market, &hash, &stake);
    s
}

#[test]
fn committed_prices_stay_secret_until_revealed() {
    let h = setup(1_000, 1_000, 500);
    let v = funded_voters(&h, 1);
    open_commit_reveal(&h, 10);
    commit(&h, &v.list[0], 10, 12_345, 1, 1_000);

    let stored = h.client.get_price_vote(&10, &v.list[0]).unwrap();
    assert!(!stored.revealed);
    assert_eq!(stored.price, 0, "the plaintext price is not stored during voting");
}

#[test]
fn a_commit_reveal_round_settles_on_revealed_prices_and_forfeits_unrevealed_stake() {
    let h = setup(1_000, 1_000, 500);
    let v = funded_voters(&h, 4);
    open_commit_reveal(&h, 11);

    let prices = [100i128, 100, 100];
    let salts: std::vec::Vec<BytesN<32>> = v.list[..3]
        .iter()
        .zip(prices)
        .enumerate()
        .map(|(i, (voter, price))| commit(&h, voter, 11, price, i as u8 + 1, 1_000))
        .collect();
    // A fourth voter commits but never reveals.
    commit(&h, &v.list[3], 11, 100, 9, 400);

    advance(&h.env, 101);
    for ((voter, price), s) in v.list[..3].iter().zip(prices).zip(&salts) {
        h.client.reveal_price_vote(voter, &11, &price, s);
    }
    advance(&h.env, 100);

    assert_eq!(h.client.finalize_price(&11), 100);
    // The silent voter's 400 is split between the three revealers (133 + 133 + 134).
    assert_eq!(balance(&h, &v.list[3]), 1_000_000 - 400);
    let gained: i128 = v.list[..3].iter().map(|a| balance(&h, a) - 1_000_000).sum();
    assert_eq!(gained, 400);
    assert_eq!(balance(&h, &h.client.address), 0);
}

#[test]
fn reveals_that_do_not_match_the_commitment_are_rejected() {
    let h = setup(1_000, 1_000, 500);
    let v = funded_voters(&h, 1);
    open_commit_reveal(&h, 12);
    let s = commit(&h, &v.list[0], 12, 100, 1, 1_000);
    advance(&h.env, 101);

    assert_contract_error(
        h.client.try_reveal_price_vote(&v.list[0], &12, &101, &s),
        OracleError::CommitmentMismatch,
    );
    assert_contract_error(
        h.client.try_reveal_price_vote(&v.list[0], &12, &100, &salt(&h.env, 2)),
        OracleError::CommitmentMismatch,
    );
    h.client.reveal_price_vote(&v.list[0], &12, &100, &s);
    assert_contract_error(
        h.client.try_reveal_price_vote(&v.list[0], &12, &100, &s),
        OracleError::AlreadyRevealed,
    );
}

#[test]
fn a_copied_price_commitment_cannot_be_revealed_by_the_copycat() {
    let h = setup(1_000, 1_000, 500);
    let v = funded_voters(&h, 2);
    open_commit_reveal(&h, 13);

    let s = commit(&h, &v.list[0], 13, 100, 1, 1_000);
    let copied = h.client.get_price_vote(&13, &v.list[0]).unwrap().commitment;
    h.client.commit_price_vote(&v.list[1], &13, &copied, &1_000);

    advance(&h.env, 101);
    h.client.reveal_price_vote(&v.list[0], &13, &100, &s);
    assert_contract_error(
        h.client.try_reveal_price_vote(&v.list[1], &13, &100, &s),
        OracleError::CommitmentMismatch,
    );
}

#[test]
fn commit_and_reveal_phases_are_enforced() {
    let h = setup(1_000, 1_000, 500);
    let v = funded_voters(&h, 2);
    open_commit_reveal(&h, 14);
    let s = commit(&h, &v.list[0], 14, 100, 1, 1_000);

    // Too early to reveal.
    assert_contract_error(
        h.client.try_reveal_price_vote(&v.list[0], &14, &100, &s),
        OracleError::RevealPeriodNotStarted,
    );
    // Plain voting is not allowed in a commit-reveal round.
    assert_contract_error(
        h.client.try_submit_price_vote(&v.list[1], &14, &100, &1_000),
        OracleError::WrongRoundKind,
    );

    advance(&h.env, 101);
    // The commit window has closed.
    let hash = h.client.price_commitment_hash(&v.list[1], &100, &salt(&h.env, 2));
    assert_contract_error(
        h.client.try_commit_price_vote(&v.list[1], &14, &hash, &1_000),
        OracleError::CommitPeriodEnded,
    );

    advance(&h.env, 100);
    assert_contract_error(
        h.client.try_reveal_price_vote(&v.list[0], &14, &100, &s),
        OracleError::RevealPeriodEnded,
    );
}

#[test]
fn committing_to_a_plain_round_is_rejected() {
    let h = setup(1_000, 1_000, 500);
    let v = funded_voters(&h, 1);
    open_plain(&h, 15, 100);
    let hash = h.client.price_commitment_hash(&v.list[0], &100, &salt(&h.env, 1));
    assert_contract_error(
        h.client.try_commit_price_vote(&v.list[0], &15, &hash, &1_000),
        OracleError::WrongRoundKind,
    );
}

#[test]
fn a_round_nobody_revealed_in_sends_the_forfeited_stake_to_the_admin() {
    let h = setup(1_000, 1_000, 500);
    let v = funded_voters(&h, 1);
    open_commit_reveal(&h, 16);
    commit(&h, &v.list[0], 16, 100, 1, 1_000);
    let admin_before = balance(&h, &h.admin);

    advance(&h.env, 200);
    assert_eq!(h.client.finalize_price(&16), 0);

    assert_eq!(balance(&h, &h.admin), admin_before + 1_000);
    assert_eq!(balance(&h, &h.client.address), 0);
}
