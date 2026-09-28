#![cfg(test)]

use super::*;
use soroban_sdk::testutils::{Address as _, Events, Ledger};
use soroban_sdk::{token, Address, Env, String, Symbol, TryFromVal};

const OFFER_TTL: u64 = 604800;

struct Setup<'a> {
    env: Env,
    client: RefundContractClient<'a>,
    admin: Address,
    merchant: Address,
    customer: Address,
    token: Address,
}

fn setup<'a>() -> Setup<'a> {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(1_000);
    let contract_id = env.register(RefundContract, ());
    let client = RefundContractClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    client.initialize(&admin);

    let token = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    token::StellarAssetClient::new(&env, &token).mint(&contract_id, &1_000_000);

    Setup {
        merchant: Address::generate(&env),
        customer: Address::generate(&env),
        env,
        client,
        admin,
        token,
    }
}

fn request(s: &Setup, amount: i128) -> u64 {
    s.client.request_refund(
        &s.merchant,
        &1,
        &s.customer,
        &amount,
        &amount,
        &s.token,
        &String::from_str(&s.env, "item damaged"),
        &RefundReasonCode::ProductDefect,
        &s.env.ledger().timestamp(),
    )
}

fn has_event(env: &Env, name: &str) -> bool {
    let expected = Symbol::new(env, name);
    env.events().all().iter().any(|(_, topics, _)| {
        topics
            .get(0)
            .and_then(|t| Symbol::try_from_val(env, &t).ok())
            .is_some_and(|s| s == expected)
    })
}

#[test]
fn merchant_counter_offer_is_recorded_without_changing_the_request() {
    let s = setup();
    let refund_id = request(&s, 1000);

    s.client.counter_offer(&s.merchant, &refund_id, &600);
    assert!(has_event(&s.env, "counter_offer_made"));

    assert_eq!(
        s.client.get_counter_offer(&refund_id),
        Some(CounterOffer {
            refund_id,
            merchant: s.merchant.clone(),
            amount: 600,
            offered_at: 1_000,
            expires_at: 1_000 + OFFER_TTL,
        })
    );
    let refund = s.client.get_refund(&refund_id);
    assert_eq!(refund.status, RefundStatus::Requested);
    assert_eq!(refund.amount, 1000);
}

#[test]
fn accepted_amount_is_what_gets_processed() {
    let s = setup();
    let refund_id = request(&s, 1000);
    s.client.counter_offer(&s.merchant, &refund_id, &600);

    s.client.accept_counter_offer(&s.customer, &refund_id);
    assert!(has_event(&s.env, "counter_offer_accepted"));
    assert!(has_event(&s.env, "refund_approved"));

    let refund = s.client.get_refund(&refund_id);
    assert_eq!(refund.status, RefundStatus::Approved);
    assert_eq!(refund.amount, 600);
    assert_eq!(s.client.get_counter_offer(&refund_id), None);
    // Cap usage tracks the accepted amount, not the original request.
    assert_eq!(s.client.get_payment_refund_usage(&1), (1, 600));

    s.client.process_refund(&s.admin, &refund_id);
    assert_eq!(
        token::Client::new(&s.env, &s.token).balance(&s.customer),
        600
    );
    assert_eq!(s.client.get_total_refunded_amount(&1), 600);
}

#[test]
fn counter_offer_must_be_positive_and_below_requested_amount() {
    let s = setup();
    let refund_id = request(&s, 1000);

    for amount in [0_i128, -5, 1000, 1500] {
        assert_eq!(
            s.client.try_counter_offer(&s.merchant, &refund_id, &amount),
            Err(Ok(Error::Ext(ExtError::InvalidCounterOffer)))
        );
    }
    assert_eq!(s.client.get_counter_offer(&refund_id), None);

    // Boundaries just inside the range are accepted.
    s.client.counter_offer(&s.merchant, &refund_id, &1);
    s.client.counter_offer(&s.merchant, &refund_id, &999);
}

#[test]
fn only_the_refund_merchant_can_counter_offer() {
    let s = setup();
    let refund_id = request(&s, 1000);

    for other in [Address::generate(&s.env), s.customer.clone()] {
        assert_eq!(
            s.client.try_counter_offer(&other, &refund_id, &600),
            Err(Ok(Error::Core(CoreError::Unauthorized)))
        );
    }
}

#[test]
fn only_the_refund_customer_can_accept() {
    let s = setup();
    let refund_id = request(&s, 1000);
    s.client.counter_offer(&s.merchant, &refund_id, &600);

    for other in [Address::generate(&s.env), s.merchant.clone()] {
        assert_eq!(
            s.client.try_accept_counter_offer(&other, &refund_id),
            Err(Ok(Error::Core(CoreError::Unauthorized)))
        );
    }
}

#[test]
fn accept_without_offer_fails() {
    let s = setup();
    let refund_id = request(&s, 1000);

    assert_eq!(
        s.client.try_accept_counter_offer(&s.customer, &refund_id),
        Err(Ok(Error::Ext(ExtError::CounterOfferNotFound)))
    );
}

#[test]
fn expired_offer_leaves_the_original_request() {
    let s = setup();
    let refund_id = request(&s, 1000);
    s.client.counter_offer(&s.merchant, &refund_id, &600);

    // Still acceptable at the expiry instant; not one second later.
    s.env.ledger().set_timestamp(1_000 + OFFER_TTL + 1);
    assert_eq!(
        s.client.try_accept_counter_offer(&s.customer, &refund_id),
        Err(Ok(Error::Ext(ExtError::CounterOfferExpired)))
    );

    let refund = s.client.get_refund(&refund_id);
    assert_eq!(refund.status, RefundStatus::Requested);
    assert_eq!(refund.amount, 1000);

    s.client.approve_refund(&s.admin, &refund_id);
    s.client.process_refund(&s.admin, &refund_id);
    assert_eq!(
        token::Client::new(&s.env, &s.token).balance(&s.customer),
        1000
    );
}

#[test]
fn offer_can_be_accepted_up_to_its_expiry() {
    let s = setup();
    let refund_id = request(&s, 1000);
    s.client.counter_offer(&s.merchant, &refund_id, &600);

    s.env.ledger().set_timestamp(1_000 + OFFER_TTL);
    s.client.accept_counter_offer(&s.customer, &refund_id);
    assert_eq!(s.client.get_refund(&refund_id).amount, 600);
}

#[test]
fn new_offer_replaces_previous_one() {
    let s = setup();
    let refund_id = request(&s, 1000);
    s.client.counter_offer(&s.merchant, &refund_id, &500);
    s.client.counter_offer(&s.merchant, &refund_id, &700);

    s.client.accept_counter_offer(&s.customer, &refund_id);
    assert_eq!(s.client.get_refund(&refund_id).amount, 700);
}

#[test]
fn cannot_counter_offer_a_decided_refund() {
    let s = setup();
    let refund_id = request(&s, 1000);
    s.client.approve_refund(&s.admin, &refund_id);

    assert_eq!(
        s.client.try_counter_offer(&s.merchant, &refund_id, &600),
        Err(Ok(Error::Core(CoreError::InvalidStatus)))
    );
}

#[test]
fn admin_decision_discards_pending_offer() {
    let s = setup();
    let approved = request(&s, 1000);
    s.client.counter_offer(&s.merchant, &approved, &600);
    s.client.approve_refund(&s.admin, &approved);
    assert_eq!(s.client.get_counter_offer(&approved), None);
    assert_eq!(s.client.get_refund(&approved).amount, 1000);

    let rejected = s.client.request_refund(
        &s.merchant,
        &2,
        &s.customer,
        &500,
        &500,
        &s.token,
        &String::from_str(&s.env, "second"),
        &RefundReasonCode::Other,
        &s.env.ledger().timestamp(),
    );
    s.client.counter_offer(&s.merchant, &rejected, &100);
    s.client
        .reject_refund(&s.admin, &rejected, &String::from_str(&s.env, "no"));
    assert_eq!(s.client.get_counter_offer(&rejected), None);
    assert_eq!(
        s.client.try_accept_counter_offer(&s.customer, &rejected),
        Err(Ok(Error::Core(CoreError::InvalidStatus)))
    );
}
