use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token, Address, Env, String,
};

use astroid_shared::errors::Error;

use crate::{EscrowContract, EscrowContractClient, EscrowState};

const START: u64 = 1_000;

struct Harness<'a> {
    env: Env,
    client: EscrowContractClient<'a>,
    asset: Address,
    sender: Address,
    recipient: Address,
    arbiter: Address,
}

/// Register an escrow contract plus a test SAC token, and mint `funded` of the
/// asset to the sender so `create` moves real value into custody.
fn setup(funded: i128) -> Harness<'static> {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().with_mut(|l| l.timestamp = START);

    let id = env.register_contract(None, EscrowContract);
    let client = EscrowContractClient::new(&env, &id);
    client.initialize();

    let token_admin = Address::generate(&env);
    let asset = env
        .register_stellar_asset_contract_v2(token_admin)
        .address();

    let sender = Address::generate(&env);
    let recipient = Address::generate(&env);
    let arbiter = Address::generate(&env);
    if funded > 0 {
        token::StellarAssetClient::new(&env, &asset).mint(&sender, &funded);
    }

    Harness {
        env,
        client,
        asset,
        sender,
        recipient,
        arbiter,
    }
}

fn balance(h: &Harness, who: &Address) -> i128 {
    token::TokenClient::new(&h.env, &h.asset).balance(who)
}

fn create(h: &Harness, amount: i128, deadline: u64) -> u64 {
    h.client.create(
        &h.sender,
        &h.recipient,
        &h.arbiter,
        &h.asset,
        &amount,
        &deadline,
        &String::from_str(&h.env, "payment"),
    )
}

#[test]
fn full_cycle_create_release() {
    let h = setup(10_000);
    let id = create(&h, 10_000, START + 86_400);
    assert_eq!(id, 1);
    // Funds are now in the escrow's custody, out of the sender's account.
    assert_eq!(balance(&h, &h.sender), 0);
    assert_eq!(balance(&h, &h.client.address), 10_000);
    assert_eq!(h.client.get(&id).state, EscrowState::Funded);

    h.client.release(&h.arbiter, &id);
    assert_eq!(h.client.get(&id).state, EscrowState::Released);
    // The recipient received the real tokens; custody is empty.
    assert_eq!(balance(&h, &h.recipient), 10_000);
    assert_eq!(balance(&h, &h.client.address), 0);

    h.client.close(&h.sender, &id);
    assert_eq!(h.client.get(&id).state, EscrowState::Closed);
}

#[test]
fn non_arbiter_cannot_release() {
    let h = setup(5_000);
    let id = create(&h, 5_000, START + 100);
    let intruder = Address::generate(&h.env);

    let res = h.client.try_release(&intruder, &id);
    assert_eq!(res, Err(Ok(Error::Unauthorized)));
    // Nothing moved.
    assert_eq!(balance(&h, &h.client.address), 5_000);
    assert_eq!(balance(&h, &h.recipient), 0);
}

#[test]
fn release_after_deadline_is_refused() {
    let h = setup(5_000);
    let id = create(&h, 5_000, START + 100);

    // Releasing after the deadline is refused with EscrowExpired. The host rolls
    // back state on the returned error, so the escrow stays Funded and the sender
    // can still reclaim funds via the permissionless refund path.
    h.env.ledger().with_mut(|l| l.timestamp = START + 200);
    let res = h.client.try_release(&h.arbiter, &id);
    assert_eq!(res, Err(Ok(Error::EscrowExpired)));
    assert_eq!(h.client.get(&id).state, EscrowState::Funded);
    assert_eq!(balance(&h, &h.client.address), 5_000);
}

#[test]
fn refund_returns_funds_after_deadline() {
    let h = setup(5_000);
    let id = create(&h, 5_000, START + 100);

    h.env.ledger().with_mut(|l| l.timestamp = START + 200);
    h.client.refund(&h.sender, &id);
    assert_eq!(h.client.get(&id).state, EscrowState::Refunded);
    // The sender got the real tokens back; custody is empty.
    assert_eq!(balance(&h, &h.sender), 5_000);
    assert_eq!(balance(&h, &h.client.address), 0);
}

#[test]
fn refund_before_deadline_rejected() {
    let h = setup(5_000);
    let id = create(&h, 5_000, START + 100);

    let res = h.client.try_refund(&h.sender, &id);
    assert_eq!(res, Err(Ok(Error::InvalidState)));
    assert_eq!(balance(&h, &h.client.address), 5_000);
}

#[test]
fn expire_marks_then_refund_returns() {
    let h = setup(5_000);
    let id = create(&h, 5_000, START + 100);

    // Cannot expire before the deadline.
    let early = h.client.try_expire(&id);
    assert_eq!(early, Err(Ok(Error::InvalidState)));

    h.env.ledger().with_mut(|l| l.timestamp = START + 200);
    h.client.expire(&id);
    assert_eq!(h.client.get(&id).state, EscrowState::Expired);
    // Marking Expired must NOT move funds — they wait for refund.
    assert_eq!(balance(&h, &h.client.address), 5_000);
    assert_eq!(balance(&h, &h.sender), 0);

    h.client.refund(&h.sender, &id);
    assert_eq!(h.client.get(&id).state, EscrowState::Refunded);
    assert_eq!(balance(&h, &h.sender), 5_000);
    assert_eq!(balance(&h, &h.client.address), 0);
}

#[test]
fn released_escrow_cannot_be_refunded() {
    let h = setup(5_000);
    let id = create(&h, 5_000, START + 100);
    h.client.release(&h.arbiter, &id);

    // Even past the deadline, a released escrow cannot double-spend via refund.
    h.env.ledger().with_mut(|l| l.timestamp = START + 200);
    let res = h.client.try_refund(&h.sender, &id);
    assert_eq!(res, Err(Ok(Error::InvalidState)));
    assert_eq!(balance(&h, &h.recipient), 5_000);
    assert_eq!(balance(&h, &h.client.address), 0);
}

#[test]
fn cannot_close_while_expired() {
    let h = setup(5_000);
    let id = create(&h, 5_000, START + 100);
    h.env.ledger().with_mut(|l| l.timestamp = START + 200);
    h.client.expire(&id);

    // Closing an Expired escrow would strand its still-held funds — refused.
    let res = h.client.try_close(&h.sender, &id);
    assert_eq!(res, Err(Ok(Error::InvalidState)));
    assert_eq!(balance(&h, &h.client.address), 5_000);
}

#[test]
fn create_rejects_bad_input() {
    let h = setup(5_000);
    // recipient == sender
    let r1 = h.client.try_create(
        &h.sender,
        &h.sender,
        &h.arbiter,
        &h.asset,
        &1_000,
        &(START + 100),
        &String::from_str(&h.env, "x"),
    );
    assert_eq!(r1, Err(Ok(Error::InvalidInput)));
    // deadline in the past
    let r2 = h.client.try_create(
        &h.sender,
        &h.recipient,
        &h.arbiter,
        &h.asset,
        &1_000,
        &(START - 500),
        &String::from_str(&h.env, "x"),
    );
    assert_eq!(r2, Err(Ok(Error::InvalidInput)));
    // non-positive amount
    let r3 = h.client.try_create(
        &h.sender,
        &h.recipient,
        &h.arbiter,
        &h.asset,
        &0,
        &(START + 100),
        &String::from_str(&h.env, "x"),
    );
    assert_eq!(r3, Err(Ok(Error::InvalidAmount)));
    // No successful escrow was created, so the sender keeps every token.
    assert_eq!(balance(&h, &h.sender), 5_000);
}

// --- Clawback tests ---

#[test]
fn clawback_after_timeout_succeeds() {
    let h = setup(5_000);
    let id = create(&h, 5_000, START + 100);

    // Advance past the deadline.
    h.env.ledger().with_mut(|l| l.timestamp = START + 200);

    // Sender can clawback after timeout.
    h.client.clawback(&h.sender, &id);
    assert_eq!(h.client.get(&id).state, EscrowState::Refunded);
    // The sender got the real tokens back; custody is empty.
    assert_eq!(balance(&h, &h.sender), 5_000);
    assert_eq!(balance(&h, &h.client.address), 0);
}

#[test]
fn clawback_before_timeout_rejected() {
    let h = setup(5_000);
    let id = create(&h, 5_000, START + 100);

    // Cannot clawback before deadline without cancellation.
    let res = h.client.try_clawback(&h.sender, &id);
    assert_eq!(res, Err(Ok(Error::InvalidState)));
    assert_eq!(balance(&h, &h.client.address), 5_000);
}

#[test]
fn clawback_after_cancel_always_succeeds() {
    let h = setup(5_000);
    let id = create(&h, 5_000, START + 100_000);

    // Cancel the escrow (before deadline).
    h.client.cancel(&h.sender, &id);
    assert!(h.client.get(&id).cancelled);

    // Sender can immediately clawback because escrow is cancelled.
    h.client.clawback(&h.sender, &id);
    assert_eq!(h.client.get(&id).state, EscrowState::Refunded);
    assert_eq!(balance(&h, &h.sender), 5_000);
    assert_eq!(balance(&h, &h.client.address), 0);
}

#[test]
fn arbiter_can_cancel_for_clawback() {
    let h = setup(5_000);
    let id = create(&h, 5_000, START + 100_000);

    // Arbiter can cancel the escrow.
    h.client.cancel(&h.arbiter, &id);
    assert!(h.client.get(&id).cancelled);

    // Sender (organization) can now clawback.
    h.client.clawback(&h.sender, &id);
    assert_eq!(h.client.get(&id).state, EscrowState::Refunded);
    assert_eq!(balance(&h, &h.sender), 5_000);
}

#[test]
fn non_sender_cannot_clawback() {
    let h = setup(5_000);
    let id = create(&h, 5_000, START + 100);

    // Advance past the deadline.
    h.env.ledger().with_mut(|l| l.timestamp = START + 200);

    // Recipient or arbiter cannot clawback — only the sender.
    let res = h.client.try_clawback(&h.recipient, &id);
    assert_eq!(res, Err(Ok(Error::Unauthorized)));
}

#[test]
fn non_authorized_cannot_cancel() {
    let h = setup(5_000);
    let id = create(&h, 5_000, START + 100_000);

    let intruder = Address::generate(&h.env);
    let res = h.client.try_cancel(&intruder, &id);
    assert_eq!(res, Err(Ok(Error::Unauthorized)));
    assert!(!h.client.get(&id).cancelled);
}

#[test]
fn clawback_rejected_after_release() {
    let h = setup(5_000);
    let id = create(&h, 5_000, START + 100);

    // Release the escrow to the recipient.
    h.client.release(&h.arbiter, &id);
    assert_eq!(h.client.get(&id).state, EscrowState::Released);

    // Even past the deadline, clawback on a released escrow is invalid.
    h.env.ledger().with_mut(|l| l.timestamp = START + 200);
    let res = h.client.try_clawback(&h.sender, &id);
    assert_eq!(res, Err(Ok(Error::InvalidState)));
    // Funds already moved to recipient — nothing left.
    assert_eq!(balance(&h, &h.recipient), 5_000);
    assert_eq!(balance(&h, &h.client.address), 0);
}

#[test]
fn clawback_executes_successfully() {
    let h = setup(5_000);
    let id = create(&h, 5_000, START + 100);

    // Advance past the deadline.
    h.env.ledger().with_mut(|l| l.timestamp = START + 200);

    // Clawback should execute without error and transition state.
    h.client.clawback(&h.sender, &id);
    assert_eq!(h.client.get(&id).state, EscrowState::Refunded);
    // Verify funds were returned to sender.
    assert_eq!(balance(&h, &h.sender), 5_000);
    assert_eq!(balance(&h, &h.client.address), 0);
}

#[test]
fn cancel_and_clawback_time_lock_escrow() {
    let h = setup(5_000);
    let token_admin = Address::generate(&h.env);
    let asset = h
        .env
        .register_stellar_asset_contract_v2(token_admin)
        .address();
    token::StellarAssetClient::new(&h.env, &asset).mint(&h.sender, &3_000);

    let id = h.client.initialize_timelock(
        &h.sender,
        &h.recipient,
        &h.arbiter,
        &asset,
        &3_000,
        &(START + 100_000),
        &String::from_str(&h.env, "time-lock"),
    );
    assert_eq!(h.client.get(&id).state, EscrowState::Created);
    assert_eq!(h.client.get(&id).funded_amount, 0);

    // Cancel the time-locked escrow.
    h.client.cancel(&h.sender, &id);
    assert!(h.client.get(&id).cancelled);

    // Note: clawback requires state == Funded or Expired.
    // Created state is not supported for clawback — this is by design.
    let res = h.client.try_clawback(&h.sender, &id);
    assert_eq!(res, Err(Ok(Error::InvalidState)));
}
