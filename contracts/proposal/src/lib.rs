#![no_std]
//! # Astroid Proposal Contract
//!
//! Represents an action awaiting approval and drives it through the lifecycle
//! (PRD Doc 7 §Proposal):
//!
//! ```text
//! Created ─▶ Pending ─▶ Approved ─▶ Executed
//!    │          │           │
//!    ▼          ▼           ▼
//!  Cancelled  Rejected    Closed
//!            / Expired
//! ```
//!
//! A proposal links off-chain context — `wallet`, `policy`, `org` and a `tx_ref`
//! transaction reference — so the backend can reconstruct why money moved. The
//! contract records an explicit approver allow-list and an approval threshold;
//! reaching the threshold moves the proposal to `Approved`, after which it may
//! be `Executed` (marked done) and finally `Closed`.
//!
//! Functions: `create`, `approve`, `reject`, `cancel`, `expire`, `execute`,
//! `close`, `delegate`, `revoke_delegation`.

use astroid_shared::constants::{
    INSTANCE_BUMP_AMOUNT, INSTANCE_LIFETIME_THRESHOLD, MAX_APPROVERS, PERSISTENT_BUMP_AMOUNT,
    PERSISTENT_LIFETIME_THRESHOLD,
};
use astroid_shared::errors::Error;
use astroid_shared::math::checked_add;
use astroid_shared::validation::require_non_empty;
use soroban_sdk::{
    contract, contractimpl, contracttype, symbol_short, Address, Env, Map, String, Vec,
};

/// Proposal lifecycle state.
#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProposalState {
    Created = 0,
    Pending = 1,
    Approved = 2,
    Executed = 3,
    Closed = 4,
    Rejected = 5,
    Cancelled = 6,
    Expired = 7,
}

/// Stored proposal record. `approvers` is the allow-list of addresses eligible
/// to approve; `threshold` approvals move it to `Approved`.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Proposal {
    pub proposer: Address,
    pub org: String,
    /// Links (opaque references owned by the backend / other contracts).
    pub wallet: String,
    pub policy: String,
    pub tx_ref: String,
    pub approvers: Vec<Address>,
    pub threshold: u32,
    pub approvals: u32,
    pub state: ProposalState,
    pub expires_at: u64,
}

#[contracttype]
#[derive(Clone)]
enum DataKey {
    ProposalCount,
    Proposal(u64),
    Approval(u64, Address),
    /// Stores the delegation map: delegator → delegatee.
    DelegationMap,
}

/// Maximum delegation chain depth to prevent gas exhaustion.
const MAX_DELEGATION_DEPTH: u32 = 10;

#[contract]
pub struct ProposalContract;

#[contractimpl]
impl ProposalContract {
    /// Initialize the id counter and the delegation map. Idempotent-guarded.
    pub fn initialize(env: Env) -> Result<(), Error> {
        if env.storage().instance().has(&DataKey::ProposalCount) {
            return Err(Error::AlreadyInitialized);
        }
        env.storage().instance().set(&DataKey::ProposalCount, &0u64);
        let delegation_map: Map<Address, Address> = Map::new(&env);
        env.storage()
            .instance()
            .set(&DataKey::DelegationMap, &delegation_map);
        env.storage()
            .instance()
            .extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);
        Ok(())
    }

    /// Create a proposal in `Pending` state. `proposer` must authorize. The
    /// approver allow-list must be non-empty and `threshold` within its size.
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        env: Env,
        proposer: Address,
        org: String,
        wallet: String,
        policy: String,
        tx_ref: String,
        approvers: Vec<Address>,
        threshold: u32,
        expires_at: u64,
    ) -> Result<u64, Error> {
        proposer.require_auth();
        require_non_empty(&org)?;
        let n = approvers.len();
        if n == 0 || n > MAX_APPROVERS {
            return Err(Error::InvalidInput);
        }
        if threshold == 0 || threshold > n {
            return Err(Error::InvalidThreshold);
        }
        if expires_at != 0 && expires_at <= env.ledger().timestamp() {
            return Err(Error::InvalidInput);
        }

        let mut count: u64 = env
            .storage()
            .instance()
            .get(&DataKey::ProposalCount)
            .ok_or(Error::NotInitialized)?;
        count = checked_add(count as i128, 1)? as u64;
        let id = count;

        let proposal = Proposal {
            proposer: proposer.clone(),
            org,
            wallet,
            policy,
            tx_ref,
            approvers,
            threshold,
            approvals: 0,
            state: ProposalState::Pending,
            expires_at,
        };
        env.storage()
            .persistent()
            .set(&DataKey::Proposal(id), &proposal);
        Self::bump(&env, id);
        env.storage().instance().set(&DataKey::ProposalCount, &count);
        env.storage()
            .instance()
            .extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);

        env.events().publish(
            (symbol_short!("proposal"), symbol_short!("created")),
            (id, proposer),
        );
        Ok(id)
    }

    /// Approve a proposal. Caller must be on the approver allow-list and may
    /// approve only once. Reaching `threshold` transitions to `Approved`.
    ///
    /// Vote weight is 1 (direct) + any delegated voting power. Delegated power
    /// is resolved by following delegation chains from other approvers to the
    /// caller, up to `MAX_DELEGATION_DEPTH`.
    pub fn approve(env: Env, caller: Address, id: u64) -> Result<u32, Error> {
        caller.require_auth();
        let mut proposal = Self::load(&env, id)?;
        Self::ensure_not_expired(&env, &proposal)?;
        if proposal.state != ProposalState::Pending {
            return Err(Error::InvalidProposalState);
        }
        if !proposal.approvers.contains(&caller) {
            return Err(Error::NotAnApprover);
        }
        let akey = DataKey::Approval(id, caller.clone());
        if env.storage().persistent().get(&akey).unwrap_or(false) {
            return Err(Error::AlreadySigned);
        }
        env.storage().persistent().set(&akey, &true);
        // Count the caller's direct vote plus any delegated votes.
        let delegated = Self::count_delegated_power(&env, &caller)?;
        let vote_weight = checked_add(1i128, delegated as i128)? as u32;
        proposal.approvals = checked_add(proposal.approvals as i128, vote_weight as i128)? as u32;
        if proposal.approvals >= proposal.threshold {
            proposal.state = ProposalState::Approved;
        }
        Self::store(&env, id, &proposal);
        env.events().publish(
            (symbol_short!("proposal"), symbol_short!("approved")),
            (id, caller, proposal.approvals),
        );
        Ok(proposal.approvals)
    }

    /// Reject a proposal. Any approver may reject a pending proposal, which
    /// moves it to the terminal `Rejected` state.
    pub fn reject(env: Env, caller: Address, id: u64) -> Result<(), Error> {
        caller.require_auth();
        let mut proposal = Self::load(&env, id)?;
        if proposal.state != ProposalState::Pending {
            return Err(Error::InvalidProposalState);
        }
        if !proposal.approvers.contains(&caller) {
            return Err(Error::NotAnApprover);
        }
        proposal.state = ProposalState::Rejected;
        Self::store(&env, id, &proposal);
        env.events().publish(
            (symbol_short!("proposal"), symbol_short!("rejected")),
            (id, caller),
        );
        Ok(())
    }

    /// Cancel a proposal. Only the original proposer may cancel, and only before
    /// it is executed/closed.
    pub fn cancel(env: Env, caller: Address, id: u64) -> Result<(), Error> {
        caller.require_auth();
        let mut proposal = Self::load(&env, id)?;
        if caller != proposal.proposer {
            return Err(Error::Unauthorized);
        }
        if matches!(
            proposal.state,
            ProposalState::Executed | ProposalState::Closed | ProposalState::Cancelled
        ) {
            return Err(Error::InvalidProposalState);
        }
        proposal.state = ProposalState::Cancelled;
        Self::store(&env, id, &proposal);
        env.events()
            .publish((symbol_short!("proposal"), symbol_short!("cancelled")), id);
        Ok(())
    }

    /// Mark a proposal expired if its deadline has passed. Permissionless
    /// (anyone may trigger the transition; state gate protects correctness).
    pub fn expire(env: Env, id: u64) -> Result<(), Error> {
        let mut proposal = Self::load(&env, id)?;
        if !matches!(
            proposal.state,
            ProposalState::Pending | ProposalState::Approved
        ) {
            return Err(Error::InvalidProposalState);
        }
        if proposal.expires_at == 0 || env.ledger().timestamp() < proposal.expires_at {
            return Err(Error::InvalidProposalState);
        }
        proposal.state = ProposalState::Expired;
        Self::store(&env, id, &proposal);
        env.events()
            .publish((symbol_short!("proposal"), symbol_short!("expired")), id);
        Ok(())
    }

    /// Execute an approved proposal. Only the proposer may execute (the actual
    /// value movement happens in the wallet/treasury; this records completion).
    pub fn execute(env: Env, caller: Address, id: u64) -> Result<(), Error> {
        caller.require_auth();
        let mut proposal = Self::load(&env, id)?;
        Self::ensure_not_expired(&env, &proposal)?;
        if caller != proposal.proposer {
            return Err(Error::Unauthorized);
        }
        if proposal.state != ProposalState::Approved {
            return Err(Error::ProposalNotApproved);
        }
        proposal.state = ProposalState::Executed;
        Self::store(&env, id, &proposal);
        env.events()
            .publish((symbol_short!("proposal"), symbol_short!("executed")), id);
        Ok(())
    }

    /// Close an executed proposal (terminal). Only the proposer may close.
    pub fn close(env: Env, caller: Address, id: u64) -> Result<(), Error> {
        caller.require_auth();
        let mut proposal = Self::load(&env, id)?;
        if caller != proposal.proposer {
            return Err(Error::Unauthorized);
        }
        if proposal.state != ProposalState::Executed {
            return Err(Error::InvalidProposalState);
        }
        proposal.state = ProposalState::Closed;
        Self::store(&env, id, &proposal);
        env.events()
            .publish((symbol_short!("proposal"), symbol_short!("closed")), id);
        Ok(())
    }

    // --- delegation ---

    /// Delegate voting power to another address. The delegatee will receive
    /// the delegator's voting weight when approving proposals. Prevents
    /// circular delegations and enforces a maximum chain depth.
    pub fn delegate(env: Env, caller: Address, delegatee: Address) -> Result<(), Error> {
        caller.require_auth();
        if caller == delegatee {
            return Err(Error::InvalidInput);
        }
        // Prevent circular delegations by checking if delegatee already
        // delegates (directly or transitively) back to caller.
        if Self::would_create_cycle(&env, &caller, &delegatee, 0)? {
            return Err(Error::CircularDelegation);
        }
        let mut delegation_map: Map<Address, Address> = env
            .storage()
            .instance()
            .get(&DataKey::DelegationMap)
            .unwrap_or_else(|| Map::new(&env));
        delegation_map.set(caller.clone(), delegatee.clone());
        env.storage()
            .instance()
            .set(&DataKey::DelegationMap, &delegation_map);
        env.storage()
            .instance()
            .extend_ttl(INSTANCE_LIFETIME_THRESHOLD, INSTANCE_BUMP_AMOUNT);
        env.events().publish(
            (symbol_short!("proposal"), symbol_short!("delegated")),
            (caller, delegatee),
        );
        Ok(())
    }

    /// Revoke a previously set delegation. After revocation, the caller's
    /// voting power is no longer forwarded to the former delegatee.
    pub fn revoke_delegation(env: Env, caller: Address) -> Result<(), Error> {
        caller.require_auth();
        let mut delegation_map: Map<Address, Address> = env
            .storage()
            .instance()
            .get(&DataKey::DelegationMap)
            .unwrap_or_else(|| Map::new(&env));
        if !delegation_map.contains_key(caller.clone()) {
            return Err(Error::NotFound);
        }
        delegation_map.remove(caller.clone());
        env.storage()
            .instance()
            .set(&DataKey::DelegationMap, &delegation_map);
        env.events().publish(
            (symbol_short!("proposal"), symbol_short!("undeleg")),
            caller,
        );
        Ok(())
    }

    /// View: return the delegatee for a given delegator, if any.
    pub fn get_delegation(env: Env, delegator: Address) -> Option<Address> {
        let delegation_map: Map<Address, Address> = env
            .storage()
            .instance()
            .get(&DataKey::DelegationMap)
            .unwrap_or_else(|| Map::new(&env));
        delegation_map.get(delegator)
    }

    /// View: compute the total delegated voting power arriving at `addr`.
    /// Iterates the delegation map to find all delegators whose chain
    /// terminates at `addr`, respecting MAX_DELEGATION_DEPTH.
    pub fn get_delegated_power(env: Env, addr: Address) -> u32 {
        Self::count_delegated_power(&env, &addr).unwrap_or(0)
    }

    // --- views ---

    pub fn get(env: Env, id: u64) -> Result<Proposal, Error> {
        Self::load(&env, id)
    }

    pub fn state(env: Env, id: u64) -> Result<ProposalState, Error> {
        Ok(Self::load(&env, id)?.state)
    }

    // --- internal helpers ---

    fn load(env: &Env, id: u64) -> Result<Proposal, Error> {
        env.storage()
            .persistent()
            .get(&DataKey::Proposal(id))
            .ok_or(Error::NotFound)
    }

    fn store(env: &Env, id: u64, proposal: &Proposal) {
        env.storage().persistent().set(&DataKey::Proposal(id), proposal);
        Self::bump(env, id);
    }

    /// Surface [`Error::ProposalExpired`] when the deadline has passed so callers
    /// fail safely. This deliberately does NOT persist the `Expired` state: on the
    /// Soroban host, returning `Err` rolls back every storage write from the
    /// invocation, so the terminal transition is recorded only through the
    /// permissionless [`ProposalContract::expire`] entrypoint (which returns `Ok`).
    fn ensure_not_expired(env: &Env, proposal: &Proposal) -> Result<(), Error> {
        if proposal.expires_at != 0 && env.ledger().timestamp() >= proposal.expires_at {
            return Err(Error::ProposalExpired);
        }
        Ok(())
    }

    fn bump(env: &Env, id: u64) {
        env.storage().persistent().extend_ttl(
            &DataKey::Proposal(id),
            PERSISTENT_LIFETIME_THRESHOLD,
            PERSISTENT_BUMP_AMOUNT,
        );
    }

    /// Check if delegating from `delegator` to `delegatee` would create a
    /// cycle. Traverses the delegation chain starting from `delegatee` to see
    /// if it eventually reaches `delegator`.
    fn would_create_cycle(
        env: &Env,
        delegator: &Address,
        delegatee: &Address,
        depth: u32,
    ) -> Result<bool, Error> {
        if depth >= MAX_DELEGATION_DEPTH {
            return Err(Error::DelegationDepthExceeded);
        }
        let delegation_map: Map<Address, Address> = env
            .storage()
            .instance()
            .get(&DataKey::DelegationMap)
            .unwrap_or_else(|| Map::new(env));
        match delegation_map.get(delegatee.clone()) {
            Some(next_addr) => {
                if next_addr == *delegator {
                    return Ok(true);
                }
                Self::would_create_cycle(env, delegator, &next_addr, depth + 1)
            }
            None => Ok(false),
        }
    }

    /// Count how many delegators have delegated their voting power to `addr`
    /// (transitively). Iterates the delegation map and resolves each chain
    /// to see if it terminates at `addr`.
    fn count_delegated_power(env: &Env, addr: &Address) -> Result<u32, Error> {
        let delegation_map: Map<Address, Address> = env
            .storage()
            .instance()
            .get(&DataKey::DelegationMap)
            .unwrap_or_else(|| Map::new(env));
        let mut count: u32 = 0;
        // Iterate all delegators to find those whose chain ends at `addr`.
        let delegator_addresses: Vec<Address> = delegation_map.keys();
        for delegator in delegator_addresses.iter() {
            let delegatee = delegation_map.get(delegator.clone()).unwrap();
            // Follow the chain from this delegator's delegatee.
            if Self::resolve_delegation(env, &delegatee, addr, 1)? {
                count = checked_add(count as i128, 1)? as u32;
            }
        }
        Ok(count)
    }

    /// Follow a delegation chain from `current` to see if it reaches `target`.
    /// `depth` starts at 1 (the first hop is already resolved by the caller).
    fn resolve_delegation(
        env: &Env,
        current: &Address,
        target: &Address,
        depth: u32,
    ) -> Result<bool, Error> {
        if depth >= MAX_DELEGATION_DEPTH {
            return Err(Error::DelegationDepthExceeded);
        }
        if *current == *target {
            return Ok(true);
        }
        let delegation_map: Map<Address, Address> = env
            .storage()
            .instance()
            .get(&DataKey::DelegationMap)
            .unwrap_or_else(|| Map::new(env));
        match delegation_map.get(current.clone()) {
            Some(next) => Self::resolve_delegation(env, &next, target, depth + 1),
            None => Ok(false),
        }
    }
}

#[cfg(test)]
mod test;
