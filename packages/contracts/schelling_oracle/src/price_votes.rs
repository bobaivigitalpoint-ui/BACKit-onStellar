//! Stake-weighted price consensus (SC-021) with an optional commit-reveal round
//! (SC-022).
//!
//! An admin opens a round for a market. Voters stake tokens behind a price. When
//! voting closes, `finalize_price` computes the stake-weighted median, treats
//! every voter whose price is within one (stake-weighted) standard deviation of
//! it as being in consensus, slashes outliers by 10%, and pays the slashed stake
//! (plus any forfeited stake of voters who never revealed) to the consensus
//! voters pro rata. Every token that came in goes back out.
use soroban_sdk::{contracttype, token, Address, Bytes, BytesN, Env, Vec};

use crate::errors::OracleError;

/// Most voters one round can hold, bounding the work `finalize_price` does.
pub const MAX_PRICE_VOTERS: u32 = 100;
/// Share of an outlier's stake that is slashed: 10%.
pub const OUTLIER_SLASH_BPS: i128 = 1_000;
const BPS: i128 = 10_000;

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PriceRound {
    pub market_id: u64,
    pub stake_token: Address,
    /// Votes (plain rounds) or commitments (commit-reveal rounds) close here.
    pub commit_deadline: u64,
    /// Reveals close here. Equal to `commit_deadline` for plain rounds.
    pub reveal_deadline: u64,
    pub commit_reveal: bool,
    pub finalized: bool,
    /// The consensus price once finalized.
    pub median: i128,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PriceVote {
    pub voter: Address,
    pub stake: i128,
    /// `0` until revealed (plain rounds reveal at once).
    pub price: i128,
    pub revealed: bool,
    pub commitment: BytesN<32>,
}

#[contracttype]
enum PriceKey {
    Round(u64),
    Voters(u64),
    Vote(u64, Address),
}

pub fn get_round(env: &Env, market_id: u64) -> Option<PriceRound> {
    env.storage().instance().get(&PriceKey::Round(market_id))
}

pub fn set_round(env: &Env, round: &PriceRound) {
    env.storage()
        .instance()
        .set(&PriceKey::Round(round.market_id), round);
}

pub fn voters(env: &Env, market_id: u64) -> Vec<Address> {
    env.storage()
        .instance()
        .get(&PriceKey::Voters(market_id))
        .unwrap_or_else(|| Vec::new(env))
}

pub fn get_vote(env: &Env, market_id: u64, voter: &Address) -> Option<PriceVote> {
    env.storage()
        .instance()
        .get(&PriceKey::Vote(market_id, voter.clone()))
}

pub fn set_vote(env: &Env, market_id: u64, vote: &PriceVote) {
    env.storage()
        .instance()
        .set(&PriceKey::Vote(market_id, vote.voter.clone()), vote);
}

pub fn add_voter(env: &Env, market_id: u64, voter: &Address) -> Result<(), OracleError> {
    let mut list = voters(env, market_id);
    if list.len() >= MAX_PRICE_VOTERS {
        return Err(OracleError::TooManyVoters);
    }
    list.push_back(voter.clone());
    env.storage().instance().set(&PriceKey::Voters(market_id), &list);
    Ok(())
}

/// `sha256("schelling_price:" ++ voter ++ price ++ salt)`. The voter is part of
/// the preimage, so a commitment copied from another voter cannot be revealed.
pub fn price_commitment(env: &Env, voter: &Address, price: i128, salt: &BytesN<32>) -> BytesN<32> {
    use soroban_sdk::xdr::ToXdr;
    let mut raw = Bytes::from_slice(env, b"schelling_price:");
    raw.append(&voter.clone().to_xdr(env));
    raw.append(&Bytes::from_slice(env, &price.to_be_bytes()));
    raw.append(&Bytes::from_slice(env, &salt.to_array()));
    env.crypto().sha256(&raw).into()
}

pub fn transfer(env: &Env, token_addr: &Address, from: &Address, to: &Address, amount: i128) {
    if amount > 0 {
        token::Client::new(env, token_addr).transfer(from, to, &amount);
    }
}

fn mul(a: i128, b: i128) -> Result<i128, OracleError> {
    a.checked_mul(b).ok_or(OracleError::Overflow)
}

fn add(a: i128, b: i128) -> Result<i128, OracleError> {
    a.checked_add(b).ok_or(OracleError::Overflow)
}

/// Votes that count (revealed), sorted by price ascending.
fn sorted_revealed(env: &Env, market_id: u64) -> Vec<PriceVote> {
    let mut out: Vec<PriceVote> = Vec::new(env);
    for voter in voters(env, market_id).iter() {
        if let Some(vote) = get_vote(env, market_id, &voter) {
            if vote.revealed {
                // insertion sort by price (at most MAX_PRICE_VOTERS entries)
                let mut at = out.len();
                for i in 0..out.len() {
                    if out.get(i).unwrap().price > vote.price {
                        at = i;
                        break;
                    }
                }
                out.insert(at, vote);
            }
        }
    }
    out
}

/// Stake-weighted (lower) median: the first price at which the cumulative stake
/// reaches half of the total.
pub fn weighted_median(sorted: &Vec<PriceVote>, total_stake: i128) -> Result<i128, OracleError> {
    let mut cumulative: i128 = 0;
    for vote in sorted.iter() {
        cumulative = add(cumulative, vote.stake)?;
        if mul(cumulative, 2)? >= total_stake {
            return Ok(vote.price);
        }
    }
    Err(OracleError::Overflow)
}

/// Settle a round whose voting has closed. Returns the consensus price.
pub fn finalize(env: &Env, admin: &Address, mut round: PriceRound) -> Result<i128, OracleError> {
    let market_id = round.market_id;
    let sorted = sorted_revealed(env, market_id);
    let this = env.current_contract_address();

    // Stake that never made it into the count (unrevealed commitments).
    let mut forfeited: i128 = 0;
    let all = voters(env, market_id);
    for voter in all.iter() {
        let vote = get_vote(env, market_id, &voter).ok_or(OracleError::NoCommitmentFound)?;
        if !vote.revealed {
            forfeited = add(forfeited, vote.stake)?;
        }
    }

    if sorted.is_empty() {
        // Nobody revealed: there is no consensus; the forfeited stake goes to the admin.
        transfer(env, &round.stake_token, &this, admin, forfeited);
        round.finalized = true;
        set_round(env, &round);
        return Ok(0);
    }

    let mut total_stake: i128 = 0;
    let mut weighted_price_sum: i128 = 0;
    for v in sorted.iter() {
        total_stake = add(total_stake, v.stake)?;
        weighted_price_sum = add(weighted_price_sum, mul(v.price, v.stake)?)?;
    }
    let median = weighted_median(&sorted, total_stake)?;
    let mean = weighted_price_sum / total_stake;

    // variance = sum(stake * (price - mean)^2) / total_stake; a voter is in
    // consensus when (price - median)^2 <= variance (within one std deviation),
    // compared in squares to avoid a square root.
    let mut weighted_sq: i128 = 0;
    for v in sorted.iter() {
        let d = v.price - mean;
        weighted_sq = add(weighted_sq, mul(v.stake, mul(d, d)?)?)?;
    }
    let variance = weighted_sq / total_stake;

    let mut consensus: Vec<PriceVote> = Vec::new(env);
    let mut consensus_stake: i128 = 0;
    let mut slash_pool = forfeited;
    let mut outliers: Vec<(PriceVote, i128)> = Vec::new(env);
    for v in sorted.iter() {
        let d = v.price - median;
        if mul(d, d)? <= variance || d == 0 {
            consensus_stake = add(consensus_stake, v.stake)?;
            consensus.push_back(v);
        } else {
            let slash = mul(v.stake, OUTLIER_SLASH_BPS)? / BPS;
            slash_pool = add(slash_pool, slash)?;
            outliers.push_back((v, slash));
        }
    }

    for (v, slash) in outliers.iter() {
        transfer(env, &round.stake_token, &this, &v.voter, v.stake - slash);
    }

    // Consensus voters: their stake back plus a pro-rata share of the slash pool;
    // the last one takes the rounding remainder so nothing is left behind.
    let mut distributed: i128 = 0;
    let last = consensus.len() - 1;
    for (i, v) in consensus.iter().enumerate() {
        let share = if i as u32 == last {
            slash_pool - distributed
        } else {
            mul(slash_pool, v.stake)? / consensus_stake
        };
        distributed = add(distributed, share)?;
        transfer(env, &round.stake_token, &this, &v.voter, add(v.stake, share)?);
    }

    round.finalized = true;
    round.median = median;
    set_round(env, &round);
    Ok(median)
}
