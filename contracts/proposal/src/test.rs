#![cfg(test)]
extern crate std;

use crate::{ProposalContract, ProposalContractClient, ProposalState};
use astroid_shared::errors::Error;
use soroban_sdk::testutils::{Address as _, Ledger};
use soroban_sdk::{Address, Env, String, Vec};

struct Harness {
    env: Env,
    client: ProposalContractClient<'static>,
    proposer: Address,
    approvers: std::vec::Vec<Address>,
}

fn setup(num_approvers: u32) -> Harness {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(1_000);
    let contract_id = env.register_contract(None, ProposalContract);
    let client = ProposalContractClient::new(&env, &contract_id);
    client.initialize();

    let proposer = Address::generate(&env);
    let mut approvers = std::vec::Vec::new();
    for _ in 0..num_approvers {
        approvers.push(Address::generate(&env));
    }
    Harness {
        env,
        client,
        proposer,
        approvers,
    }
}

fn approver_vec(h: &Harness) -> Vec<Address> {
    let mut v = Vec::new(&h.env);
    for a in &h.approvers {
        v.push_back(a.clone());
    }
    v
}

fn create(h: &Harness, threshold: u32, expires_at: u64) -> u64 {
    h.client.create(
        &h.proposer,
        &String::from_str(&h.env, "acme"),
        &String::from_str(&h.env, "wallet-1"),
        &String::from_str(&h.env, "policy-1"),
        &String::from_str(&h.env, "tx-ref-1"),
        &approver_vec(h),
        &threshold,
        &expires_at,
    )
}

// --- Existing tests ---

#[test]
fn create_starts_pending() {
    let h = setup(3);
    let id = create(&h, 2, 5_000);
    assert_eq!(h.client.state(&id), ProposalState::Pending);
}

#[test]
fn full_lifecycle_to_closed() {
    let h = setup(3);
    let id = create(&h, 2, 5_000);
    h.client.approve(&h.approvers[0], &id);
    let approvals = h.client.approve(&h.approvers[1], &id);
    assert_eq!(approvals, 2);
    assert_eq!(h.client.state(&id), ProposalState::Approved);

    h.client.execute(&h.proposer, &id);
    assert_eq!(h.client.state(&id), ProposalState::Executed);

    h.client.close(&h.proposer, &id);
    assert_eq!(h.client.state(&id), ProposalState::Closed);
}

#[test]
fn execute_before_approved_fails() {
    let h = setup(3);
    let id = create(&h, 2, 5_000);
    h.client.approve(&h.approvers[0], &id); // only 1 of 2
    let res = h.client.try_execute(&h.proposer, &id);
    assert_eq!(res, Err(Ok(Error::ProposalNotApproved)));
}

#[test]
fn non_approver_cannot_approve() {
    let h = setup(3);
    let id = create(&h, 2, 5_000);
    let stranger = Address::generate(&h.env);
    let res = h.client.try_approve(&stranger, &id);
    assert_eq!(res, Err(Ok(Error::NotAnApprover)));
}

#[test]
fn double_approval_rejected() {
    let h = setup(3);
    let id = create(&h, 2, 5_000);
    h.client.approve(&h.approvers[0], &id);
    let res = h.client.try_approve(&h.approvers[0], &id);
    assert_eq!(res, Err(Ok(Error::AlreadySigned)));
}

#[test]
fn reject_moves_to_rejected() {
    let h = setup(3);
    let id = create(&h, 2, 5_000);
    h.client.reject(&h.approvers[0], &id);
    assert_eq!(h.client.state(&id), ProposalState::Rejected);
    // Cannot approve a rejected proposal.
    let res = h.client.try_approve(&h.approvers[1], &id);
    assert_eq!(res, Err(Ok(Error::InvalidProposalState)));
}

#[test]
fn only_proposer_can_cancel() {
    let h = setup(3);
    let id = create(&h, 2, 5_000);
    let res = h.client.try_cancel(&h.approvers[0], &id);
    assert_eq!(res, Err(Ok(Error::Unauthorized)));
    h.client.cancel(&h.proposer, &id);
    assert_eq!(h.client.state(&id), ProposalState::Cancelled);
}

#[test]
fn expired_proposal_cannot_be_approved() {
    let h = setup(3);
    let id = create(&h, 2, 5_000);
    // Advance beyond expiry.
    h.env.ledger().set_timestamp(6_000);
    let res = h.client.try_approve(&h.approvers[0], &id);
    assert_eq!(res, Err(Ok(Error::ProposalExpired)));
    // The failed approval is rolled back by the host, so the proposal is still
    // Pending on-chain. The terminal `Expired` transition is recorded only via
    // the permissionless `expire()` path (see `explicit_expire_transition`).
    assert_eq!(h.client.state(&id), ProposalState::Pending);
    h.client.expire(&id);
    assert_eq!(h.client.state(&id), ProposalState::Expired);
}

#[test]
fn explicit_expire_transition() {
    let h = setup(3);
    let id = create(&h, 2, 5_000);
    // Cannot expire before deadline.
    let early = h.client.try_expire(&id);
    assert_eq!(early, Err(Ok(Error::InvalidProposalState)));
    h.env.ledger().set_timestamp(6_000);
    h.client.expire(&id);
    assert_eq!(h.client.state(&id), ProposalState::Expired);
}

#[test]
fn create_with_bad_threshold_fails() {
    let h = setup(2);
    // threshold 3 > 2 approvers
    let res = h.client.try_create(
        &h.proposer,
        &String::from_str(&h.env, "acme"),
        &String::from_str(&h.env, "wallet-1"),
        &String::from_str(&h.env, "policy-1"),
        &String::from_str(&h.env, "tx-ref-1"),
        &approver_vec(&h),
        &3,
        &5_000,
    );
    assert_eq!(res, Err(Ok(Error::InvalidThreshold)));
}

#[test]
fn create_with_past_expiry_fails() {
    let h = setup(2);
    let res = h.client.try_create(
        &h.proposer,
        &String::from_str(&h.env, "acme"),
        &String::from_str(&h.env, "wallet-1"),
        &String::from_str(&h.env, "policy-1"),
        &String::from_str(&h.env, "tx-ref-1"),
        &approver_vec(&h),
        &1,
        &500, // in the past (now = 1000)
    );
    assert_eq!(res, Err(Ok(Error::InvalidInput)));
}

// --- Delegation tests ---

#[test]
fn delegate_sets_delegation() {
    let h = setup(3);
    let delegator = h.approvers[0].clone();
    let delegatee = h.approvers[1].clone();
    h.client.delegate(&delegator, &delegatee);
    assert_eq!(
        h.client.get_delegation(&delegator),
        Some(delegatee)
    );
}

#[test]
fn delegate_self_fails() {
    let h = setup(3);
    let delegator = h.approvers[0].clone();
    let res = h.client.try_delegate(&delegator, &delegator);
    assert_eq!(res, Err(Ok(Error::InvalidInput)));
}

#[test]
fn delegate_circular_direct_rejected() {
    let h = setup(3);
    let a = h.approvers[0].clone();
    let b = h.approvers[1].clone();
    // A delegates to B
    h.client.delegate(&a, &b);
    // B tries to delegate to A → cycle
    let res = h.client.try_delegate(&b, &a);
    assert_eq!(res, Err(Ok(Error::CircularDelegation)));
}

#[test]
fn delegate_circular_transitive_rejected() {
    let h = setup(4);
    let a = h.approvers[0].clone();
    let b = h.approvers[1].clone();
    let c = h.approvers[2].clone();
    // A → B → C
    h.client.delegate(&a, &b);
    h.client.delegate(&b, &c);
    // C tries to delegate to A → cycle A→B→C→A
    let res = h.client.try_delegate(&c, &a);
    assert_eq!(res, Err(Ok(Error::CircularDelegation)));
}

#[test]
fn revoke_delegation_succeeds() {
    let h = setup(3);
    let delegator = h.approvers[0].clone();
    let delegatee = h.approvers[1].clone();
    h.client.delegate(&delegator, &delegatee);
    assert_eq!(h.client.get_delegation(&delegator), Some(delegatee));
    h.client.revoke_delegation(&delegator);
    assert_eq!(h.client.get_delegation(&delegator), None);
}

#[test]
fn revoke_nonexistent_delegation_fails() {
    let h = setup(3);
    let delegator = h.approvers[0].clone();
    let res = h.client.try_revoke_delegation(&delegator);
    assert_eq!(res, Err(Ok(Error::NotFound)));
}

#[test]
fn delegated_vote_increases_approval_count() {
    let h = setup(3);
    let id = create(&h, 3, 5_000); // threshold = 3
    // Approver 0 delegates to approver 1
    h.client.delegate(&h.approvers[0], &h.approvers[1]);
    // Approver 1 approves: 1 direct + 1 delegated = 2 votes
    let approvals = h.client.approve(&h.approvers[1], &id);
    assert_eq!(approvals, 2);
    // Approver 2 approves: 2 + 1 = 3 → threshold met
    let approvals = h.client.approve(&h.approvers[2], &id);
    assert_eq!(approvals, 3);
    assert_eq!(h.client.state(&id), ProposalState::Approved);
}

#[test]
fn delegated_vote_reaches_threshold_early() {
    let h = setup(3);
    let id = create(&h, 2, 5_000); // threshold = 2
    // Approver 0 delegates to approver 1
    h.client.delegate(&h.approvers[0], &h.approvers[1]);
    // Approver 1 approves: 1 direct + 1 delegated = 2 → threshold met
    let approvals = h.client.approve(&h.approvers[1], &id);
    assert_eq!(approvals, 2);
    assert_eq!(h.client.state(&id), ProposalState::Approved);
}

#[test]
fn transitive_delegation_resolves() {
    let h = setup(4);
    let id = create(&h, 3, 5_000); // threshold = 3
    // A → B → C (transitive: A delegates through B to C)
    h.client.delegate(&h.approvers[0], &h.approvers[1]);
    h.client.delegate(&h.approvers[1], &h.approvers[2]);
    // C approves: 1 direct + 2 delegated (A and B both chain to C) = 3
    let approvals = h.client.approve(&h.approvers[2], &id);
    assert_eq!(approvals, 3);
    assert_eq!(h.client.state(&id), ProposalState::Approved);
}

#[test]
fn delegated_power_view() {
    let h = setup(3);
    // Approver 0 delegates to approver 1
    h.client.delegate(&h.approvers[0], &h.approvers[1]);
    assert_eq!(h.client.get_delegated_power(&h.approvers[1]), 1);
    assert_eq!(h.client.get_delegated_power(&h.approvers[0]), 0);
    assert_eq!(h.client.get_delegated_power(&h.approvers[2]), 0);
}

#[test]
fn revoked_delegation_not_counted() {
    let h = setup(3);
    let id = create(&h, 3, 5_000);
    // Approver 0 delegates to approver 1, then revokes
    h.client.delegate(&h.approvers[0], &h.approvers[1]);
    h.client.revoke_delegation(&h.approvers[0]);
    // Approver 1 approves: only 1 direct vote (delegation revoked)
    let approvals = h.client.approve(&h.approvers[1], &id);
    assert_eq!(approvals, 1);
    assert_eq!(h.client.state(&id), ProposalState::Pending);
}

#[test]
fn replace_delegation_updates_power() {
    let h = setup(4);
    // Approver 0 delegates to approver 1
    h.client.delegate(&h.approvers[0], &h.approvers[1]);
    assert_eq!(h.client.get_delegated_power(&h.approvers[1]), 1);
    // Approver 0 re-delegates to approver 2
    h.client.delegate(&h.approvers[0], &h.approvers[2]);
    assert_eq!(h.client.get_delegated_power(&h.approvers[1]), 0);
    assert_eq!(h.client.get_delegated_power(&h.approvers[2]), 1);
}

#[test]
fn multiple_delegators_same_delegatee() {
    let h = setup(4);
    let id = create(&h, 4, 5_000); // threshold = 4
    // Approver 0 and 1 both delegate to approver 2
    h.client.delegate(&h.approvers[0], &h.approvers[2]);
    h.client.delegate(&h.approvers[1], &h.approvers[2]);
    assert_eq!(h.client.get_delegated_power(&h.approvers[2]), 2);
    // Approver 2 approves: 1 direct + 2 delegated = 3
    let approvals = h.client.approve(&h.approvers[2], &id);
    assert_eq!(approvals, 3);
    // Approver 3 approves: 3 + 1 = 4 → threshold met
    let approvals = h.client.approve(&h.approvers[3], &id);
    assert_eq!(approvals, 4);
    assert_eq!(h.client.state(&id), ProposalState::Approved);
}
