#![cfg(test)]

use super::*;
use soroban_sdk::testutils::{Address as _, Events};
use soroban_sdk::{token, Address, Env, String, Symbol, TryFromVal};

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

fn request(s: &Setup, payment_id: u64, amount: i128) -> u64 {
    s.client.request_refund(
        &s.merchant,
        &payment_id,
        &s.customer,
        &amount,
        &amount,
        &s.token,
        &String::from_str(&s.env, "filed by mistake"),
        &RefundReasonCode::CustomerRequest,
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
fn customer_can_withdraw_requested_refund() {
    let s = setup();
    let refund_id = request(&s, 1, 1000);

    s.client.withdraw_refund_request(&s.customer, &refund_id);

    assert!(s.env.auths().iter().any(|(addr, _)| *addr == s.customer));
    assert!(has_event(&s.env, "refund_withdrawn"));
    assert_eq!(
        s.client.get_refund(&refund_id).status,
        RefundStatus::Withdrawn
    );
    assert_eq!(
        s.client
            .get_refund_count_by_status(&RefundStatus::Requested),
        0
    );
    assert_eq!(
        s.client
            .get_refund_count_by_status(&RefundStatus::Withdrawn),
        1
    );

    let summary = s.client.get_merchant_refund_summary(&s.merchant);
    assert_eq!(summary.pending_count, 0);
    assert_eq!(summary.pending_amount, 0);
}

#[test]
fn only_the_requesting_customer_can_withdraw() {
    let s = setup();
    let refund_id = request(&s, 1, 1000);

    for other in [
        Address::generate(&s.env),
        s.merchant.clone(),
        s.admin.clone(),
    ] {
        assert_eq!(
            s.client.try_withdraw_refund_request(&other, &refund_id),
            Err(Ok(Error::Core(CoreError::Unauthorized)))
        );
    }
    assert_eq!(
        s.client.get_refund(&refund_id).status,
        RefundStatus::Requested
    );
}

#[test]
fn approved_refund_cannot_be_withdrawn() {
    let s = setup();
    let refund_id = request(&s, 1, 1000);
    s.client.approve_refund(&s.admin, &refund_id);

    assert_eq!(
        s.client
            .try_withdraw_refund_request(&s.customer, &refund_id),
        Err(Ok(Error::Core(CoreError::InvalidStatus)))
    );
    assert_eq!(
        s.client.get_refund(&refund_id).status,
        RefundStatus::Approved
    );
}

#[test]
fn processed_refund_cannot_be_withdrawn() {
    let s = setup();
    let refund_id = request(&s, 1, 1000);
    s.client.approve_refund(&s.admin, &refund_id);
    s.client.process_refund(&s.admin, &refund_id);

    assert_eq!(
        s.client
            .try_withdraw_refund_request(&s.customer, &refund_id),
        Err(Ok(Error::Core(CoreError::InvalidStatus)))
    );
}

#[test]
fn withdrawn_refund_is_terminal() {
    let s = setup();
    let refund_id = request(&s, 1, 1000);
    s.client.withdraw_refund_request(&s.customer, &refund_id);

    assert_eq!(
        s.client
            .try_withdraw_refund_request(&s.customer, &refund_id),
        Err(Ok(Error::Core(CoreError::InvalidStatus)))
    );
    assert_eq!(
        s.client.try_approve_refund(&s.admin, &refund_id),
        Err(Ok(Error::Core(CoreError::InvalidStatus)))
    );
}

#[test]
fn withdraw_unknown_refund_fails() {
    let s = setup();
    assert_eq!(
        s.client.try_withdraw_refund_request(&s.customer, &99),
        Err(Ok(Error::Core(CoreError::RefundNotFound)))
    );
}

#[test]
fn withdrawal_releases_payment_refund_cap_usage() {
    let s = setup();
    let payment_id = 7_u64;
    s.client.set_payment_refund_cap(
        &s.admin,
        &PaymentRefundCap {
            payment_id,
            max_refund_count: 1,
            max_total_amount: 1000,
        },
    );

    let refund_id = request(&s, payment_id, 1000);
    assert_eq!(s.client.get_payment_refund_usage(&payment_id), (1, 1000));

    // The cap is exhausted while the mistaken request is pending.
    let blocked = s.client.try_request_refund(
        &s.merchant,
        &payment_id,
        &s.customer,
        &1000,
        &1000,
        &s.token,
        &String::from_str(&s.env, "second"),
        &RefundReasonCode::CustomerRequest,
        &0,
    );
    assert_eq!(
        blocked,
        Err(Ok(Error::Ext(ExtError::RefundCountCapExceeded)))
    );

    s.client.withdraw_refund_request(&s.customer, &refund_id);
    assert_eq!(s.client.get_payment_refund_usage(&payment_id), (0, 0));

    // The released usage lets a corrected request through.
    request(&s, payment_id, 1000);
    assert_eq!(s.client.get_payment_refund_usage(&payment_id), (1, 1000));
}

#[test]
fn withdrawal_clears_pending_counter_offer() {
    let s = setup();
    let refund_id = request(&s, 1, 1000);
    s.client.counter_offer(&s.merchant, &refund_id, &400);

    s.client.withdraw_refund_request(&s.customer, &refund_id);
    assert_eq!(s.client.get_counter_offer(&refund_id), None);
}
