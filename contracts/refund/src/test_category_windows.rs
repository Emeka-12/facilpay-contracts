#![cfg(test)]

//! Category-based refund windows (#197).

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token, Address, Env, String,
};

const DAY: u64 = 86_400;
/// Fallback window when a payment is untagged or its category has no window.
const DEFAULT_WINDOW: u64 = 30 * DAY;
const DIGITAL_WINDOW: u64 = 3 * DAY;
const PHYSICAL_WINDOW: u64 = 14 * DAY;
/// Ledger time for every test; payment timestamps are set relative to it.
const NOW: u64 = 100 * DAY;

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
    env.ledger().set_timestamp(NOW);

    let admin = Address::generate(&env);
    let merchant = Address::generate(&env);
    let customer = Address::generate(&env);

    let token = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    token::StellarAssetClient::new(&env, &token).mint(&merchant, &1_000_000);

    let contract_id = env.register(RefundContract, ());
    let client = RefundContractClient::new(&env, &contract_id);
    client.initialize(&admin);

    Setup {
        env,
        client,
        admin,
        merchant,
        customer,
        token,
    }
}

/// Digital goods: 3 days, physical goods: 14 days.
fn configure_windows(s: &Setup) {
    s.client.set_category_window(
        &s.admin,
        &s.merchant,
        &PaymentCategory::DigitalGoods,
        &DIGITAL_WINDOW,
    );
    s.client.set_category_window(
        &s.admin,
        &s.merchant,
        &PaymentCategory::PhysicalGoods,
        &PHYSICAL_WINDOW,
    );
}

/// Requests a refund for a payment made `age` seconds before `NOW`.
fn try_request(
    s: &Setup,
    payment_id: u64,
    age: u64,
) -> Result<Result<u64, soroban_sdk::Error>, Result<Error, soroban_sdk::InvokeError>> {
    s.client.try_request_refund(
        &s.merchant,
        &payment_id,
        &s.customer,
        &100i128,
        &1_000i128,
        &s.token,
        &String::from_str(&s.env, "reason"),
        &RefundReasonCode::Other,
        &(NOW - age),
    )
}

fn assert_accepted(s: &Setup, payment_id: u64, age: u64) {
    let refund_id = try_request(s, payment_id, age)
        .expect("refund should be accepted")
        .expect("refund id should decode");
    assert_eq!(s.client.get_refund(&refund_id).payment_id, payment_id);
}

fn assert_expired(s: &Setup, payment_id: u64, age: u64) {
    assert_eq!(
        try_request(s, payment_id, age),
        Err(Ok(Error::Core(CoreError::RefundWindowExpired)))
    );
}

// ── set_category_window / get_category_window ───────────────────────────────

#[test]
fn test_set_and_get_category_windows() {
    let s = setup();
    assert_eq!(
        s.client
            .get_category_window(&s.merchant, &PaymentCategory::DigitalGoods),
        None
    );

    configure_windows(&s);

    assert_eq!(
        s.client
            .get_category_window(&s.merchant, &PaymentCategory::DigitalGoods),
        Some(DIGITAL_WINDOW)
    );
    assert_eq!(
        s.client
            .get_category_window(&s.merchant, &PaymentCategory::PhysicalGoods),
        Some(PHYSICAL_WINDOW)
    );
    assert_eq!(
        s.client
            .get_category_window(&s.merchant, &PaymentCategory::Service),
        None
    );
}

#[test]
fn test_category_window_can_be_updated() {
    let s = setup();
    configure_windows(&s);
    s.client.set_category_window(
        &s.admin,
        &s.merchant,
        &PaymentCategory::DigitalGoods,
        &(7 * DAY),
    );
    assert_eq!(
        s.client
            .get_category_window(&s.merchant, &PaymentCategory::DigitalGoods),
        Some(7 * DAY)
    );
}

#[test]
fn test_category_windows_are_per_merchant() {
    let s = setup();
    configure_windows(&s);
    let other = Address::generate(&s.env);

    assert_eq!(
        s.client
            .get_category_window(&other, &PaymentCategory::DigitalGoods),
        None
    );

    s.client
        .tag_payment_category(&s.merchant, &1, &PaymentCategory::DigitalGoods);
    assert_eq!(
        s.client.get_effective_window(&s.merchant, &1),
        DIGITAL_WINDOW
    );
    // The other merchant has no DigitalGoods window, so it gets the default.
    assert_eq!(s.client.get_effective_window(&other, &1), DEFAULT_WINDOW);
}

#[test]
fn test_set_category_window_requires_admin() {
    let s = setup();
    let outsider = Address::generate(&s.env);

    assert_eq!(
        s.client.try_set_category_window(
            &outsider,
            &s.merchant,
            &PaymentCategory::DigitalGoods,
            &DIGITAL_WINDOW,
        ),
        Err(Ok(Error::Core(CoreError::Unauthorized)))
    );
    // The merchant cannot configure its own windows either.
    assert_eq!(
        s.client.try_set_category_window(
            &s.merchant,
            &s.merchant,
            &PaymentCategory::DigitalGoods,
            &DIGITAL_WINDOW,
        ),
        Err(Ok(Error::Core(CoreError::Unauthorized)))
    );
    assert_eq!(
        s.client
            .get_category_window(&s.merchant, &PaymentCategory::DigitalGoods),
        None
    );
}

#[test]
fn test_set_category_window_requires_admin_signature() {
    let s = setup();
    s.env.set_auths(&[]);

    assert!(s
        .client
        .try_set_category_window(
            &s.admin,
            &s.merchant,
            &PaymentCategory::DigitalGoods,
            &DIGITAL_WINDOW,
        )
        .is_err());
    assert_eq!(
        s.client
            .get_category_window(&s.merchant, &PaymentCategory::DigitalGoods),
        None
    );
}

// ── tag_payment_category ─────────────────────────────────────────────────────

#[test]
fn test_tag_payment_category_requires_merchant_signature() {
    let s = setup();
    configure_windows(&s);

    s.client
        .tag_payment_category(&s.merchant, &1, &PaymentCategory::DigitalGoods);
    let auths = s.env.auths();
    assert_eq!(auths.len(), 1);
    assert_eq!(auths[0].0, s.merchant);

    s.env.set_auths(&[]);
    assert!(s
        .client
        .try_tag_payment_category(&s.merchant, &2, &PaymentCategory::DigitalGoods)
        .is_err());
    assert_eq!(
        s.client.get_effective_window(&s.merchant, &2),
        DEFAULT_WINDOW
    );
}

#[test]
fn test_payment_cannot_be_retagged() {
    let s = setup();
    configure_windows(&s);
    s.client
        .tag_payment_category(&s.merchant, &1, &PaymentCategory::DigitalGoods);

    assert_eq!(
        s.client
            .try_tag_payment_category(&s.merchant, &1, &PaymentCategory::PhysicalGoods),
        Err(Ok(Error::Core(CoreError::AlreadyProcessed)))
    );
    assert_eq!(
        s.client.get_effective_window(&s.merchant, &1),
        DIGITAL_WINDOW
    );
}

// ── get_effective_window ─────────────────────────────────────────────────────

#[test]
fn test_effective_window_uses_category_window_for_tagged_payments() {
    let s = setup();
    configure_windows(&s);
    s.client
        .tag_payment_category(&s.merchant, &1, &PaymentCategory::DigitalGoods);
    s.client
        .tag_payment_category(&s.merchant, &2, &PaymentCategory::PhysicalGoods);

    assert_eq!(
        s.client.get_effective_window(&s.merchant, &1),
        DIGITAL_WINDOW
    );
    assert_eq!(
        s.client.get_effective_window(&s.merchant, &2),
        PHYSICAL_WINDOW
    );
}

#[test]
fn test_effective_window_falls_back_to_default() {
    let s = setup();
    configure_windows(&s);

    // Untagged payment.
    assert_eq!(
        s.client.get_effective_window(&s.merchant, &1),
        DEFAULT_WINDOW
    );

    // Tagged with a category that has no window for this merchant.
    s.client
        .tag_payment_category(&s.merchant, &2, &PaymentCategory::Service);
    assert_eq!(
        s.client.get_effective_window(&s.merchant, &2),
        DEFAULT_WINDOW
    );
}

#[test]
fn test_effective_window_uses_merchant_policy_default() {
    let s = setup();
    let mut tiers = Vec::new(&s.env);
    tiers.push_back(RefundTier {
        days_from_purchase: 30,
        max_refund_bps: 10_000,
    });
    s.client.set_refund_policy(&s.merchant, &tiers);

    assert_eq!(
        s.client.get_effective_window(&s.merchant, &1),
        DEFAULT_WINDOW
    );
}

// ── Enforcement in request_refund (boundaries at the exact window edge) ──────

#[test]
fn test_digital_goods_refund_accepted_at_exact_window_edge() {
    let s = setup();
    configure_windows(&s);
    s.client
        .tag_payment_category(&s.merchant, &1, &PaymentCategory::DigitalGoods);

    assert_accepted(&s, 1, DIGITAL_WINDOW);
}

#[test]
fn test_digital_goods_refund_rejected_one_second_after_window() {
    let s = setup();
    configure_windows(&s);
    s.client
        .tag_payment_category(&s.merchant, &1, &PaymentCategory::DigitalGoods);

    assert_expired(&s, 1, DIGITAL_WINDOW + 1);
}

#[test]
fn test_physical_goods_refund_window_edges() {
    let s = setup();
    configure_windows(&s);
    s.client
        .tag_payment_category(&s.merchant, &1, &PaymentCategory::PhysicalGoods);
    s.client
        .tag_payment_category(&s.merchant, &2, &PaymentCategory::PhysicalGoods);

    assert_expired(&s, 1, PHYSICAL_WINDOW + 1);
    assert_accepted(&s, 2, PHYSICAL_WINDOW);
}

#[test]
fn test_same_age_is_judged_by_each_payments_category() {
    let s = setup();
    configure_windows(&s);
    s.client
        .tag_payment_category(&s.merchant, &1, &PaymentCategory::DigitalGoods);
    s.client
        .tag_payment_category(&s.merchant, &2, &PaymentCategory::PhysicalGoods);

    // 5 days old: outside the 3-day digital window, inside the 14-day physical one.
    assert_expired(&s, 1, 5 * DAY);
    assert_accepted(&s, 2, 5 * DAY);
}

#[test]
fn test_untagged_refund_uses_default_window_edges() {
    let s = setup();
    configure_windows(&s);

    // Well past every category window, but untagged payments get the default.
    assert_accepted(&s, 1, 20 * DAY);
    assert_accepted(&s, 2, DEFAULT_WINDOW);
    assert_expired(&s, 3, DEFAULT_WINDOW + 1);
}
