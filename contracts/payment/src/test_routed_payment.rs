#![cfg(test)]
use soroban_sdk::{
    testutils::Address as _,
    token::{Client as TokenClient, StellarAssetClient},
    Address, Env, String,
};

use crate::{Currency, Error, FeatureError, FeeConfig, PaymentContract, PaymentContractClient};

/// Sets up env, contract, admin, token, and a funded customer that has approved the contract.
/// No fee config is set. Returns (env, client, admin, token_addr, customer, merchant).
fn setup() -> (
    Env,
    PaymentContractClient<'static>,
    Address,
    Address,
    Address,
    Address,
) {
    let env = Env::default();
    env.mock_all_auths();

    let contract_id = env.register_contract(None, PaymentContract);
    let client = PaymentContractClient::new(&env, &contract_id);

    let admin = Address::generate(&env);
    let customer = Address::generate(&env);
    let merchant = Address::generate(&env);

    client.initialize(&admin);

    let token_addr = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let token_sa = StellarAssetClient::new(&env, &token_addr);
    let token = TokenClient::new(&env, &token_addr);

    token_sa.mint(&customer, &1_000_000);
    token.approve(&customer, &contract_id, &1_000_000, &10_000);

    (env, client, admin, token_addr, customer, merchant)
}

fn fee_config(admin: &Address, token_addr: &Address, fee_bps: u32) -> FeeConfig {
    FeeConfig {
        fee_bps,
        min_fee: 0,
        max_fee: 0,
        treasury: admin.clone(),
        fee_token: token_addr.clone(),
        active: true,
    }
}

fn create_pending_payment(
    env: &Env,
    client: &PaymentContractClient,
    customer: &Address,
    merchant: &Address,
    token_addr: &Address,
    amount: i128,
) -> u64 {
    client.create_payment(
        customer,
        merchant,
        &amount,
        token_addr,
        &Currency::USDC,
        &0,
        &String::from_str(env, ""),
    )
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Without a fee config only the zero-cost direct route is returned.
#[test]
fn test_get_optimal_route_without_fee_config_returns_direct_route() {
    let (env, client, _admin, token_addr, _customer, _merchant) = setup();
    let output_token = Address::generate(&env);

    let routes = client.get_optimal_route(&token_addr, &output_token, &10_000);

    assert_eq!(routes.len(), 1);
    let direct = routes.get(0).unwrap();
    assert_eq!(direct.input_token, token_addr);
    assert_eq!(direct.output_token, output_token);
    assert_eq!(direct.input_amount, 10_000);
    assert_eq!(direct.output_amount, 10_000);
    assert_eq!(direct.fee_bps, 0);
    assert_eq!(direct.effective_cost, 0);
}

/// With an active fee config a fee-bearing route is added and routes are
/// sorted by effective_cost ascending.
#[test]
fn test_get_optimal_route_with_fee_config_sorted_by_cost() {
    let (env, client, admin, token_addr, _customer, _merchant) = setup();
    let output_token = Address::generate(&env);

    // 2.5% fee
    client.set_fee_config(&admin, &fee_config(&admin, &token_addr, 250));

    let routes = client.get_optimal_route(&token_addr, &output_token, &10_000);

    assert_eq!(routes.len(), 2);
    let first = routes.get(0).unwrap();
    let second = routes.get(1).unwrap();
    assert!(first.effective_cost <= second.effective_cost);

    assert_eq!(first.fee_bps, 0);
    assert_eq!(first.output_amount, 10_000);

    assert_eq!(second.fee_bps, 250);
    assert_eq!(second.effective_cost, 250);
    assert_eq!(second.output_amount, 9_750);
    assert_eq!(second.input_amount, 10_000);
}

/// Executing a valid route transfers the route's output amount from customer to merchant.
#[test]
fn test_execute_routed_payment_transfers_output_amount() {
    let (env, client, admin, token_addr, customer, merchant) = setup();
    let token = TokenClient::new(&env, &token_addr);

    client.set_fee_config(&admin, &fee_config(&admin, &token_addr, 100));

    let amount = 5_000;
    let payment_id =
        create_pending_payment(&env, &client, &customer, &merchant, &token_addr, amount);

    let routes = client.get_optimal_route(&token_addr, &token_addr, &amount);
    // Pick the route matching the currently active fee config.
    let route = routes.iter().find(|r| r.fee_bps == 100).unwrap();
    assert_eq!(route.output_amount, 4_950);

    let customer_before = token.balance(&customer);
    let merchant_before = token.balance(&merchant);

    client.execute_routed_payment(&customer, &merchant, &route, &payment_id);

    assert_eq!(token.balance(&merchant) - merchant_before, 4_950);
    assert_eq!(customer_before - token.balance(&customer), 4_950);
}

/// A route becomes invalid once the fee config changes; mismatched amounts and
/// non-owner customers are rejected as well.
#[test]
fn test_execute_routed_payment_rejects_invalid_routes() {
    let (env, client, admin, token_addr, customer, merchant) = setup();

    let amount = 2_000;
    let payment_id =
        create_pending_payment(&env, &client, &customer, &merchant, &token_addr, amount);

    // Route quoted with no fee config...
    let stale_route = client
        .get_optimal_route(&token_addr, &token_addr, &amount)
        .get(0)
        .unwrap();
    assert_eq!(stale_route.fee_bps, 0);

    // ...wrong input amount against the payment is rejected.
    let mut wrong_amount = stale_route.clone();
    wrong_amount.input_amount = amount + 1;
    assert_eq!(
        client.try_execute_routed_payment(&customer, &merchant, &wrong_amount, &payment_id),
        Err(Ok(Error::Basic(crate::BasicError::InvalidAmount)))
    );

    // ...a caller who is not the payment's customer is rejected.
    let stranger = Address::generate(&env);
    assert_eq!(
        client.try_execute_routed_payment(&stranger, &merchant, &stale_route, &payment_id),
        Err(Ok(Error::Basic(crate::BasicError::Unauthorized)))
    );

    // ...then the fee config changes, making the quoted route stale.
    client.set_fee_config(&admin, &fee_config(&admin, &token_addr, 300));
    assert_eq!(
        client.try_execute_routed_payment(&customer, &merchant, &stale_route, &payment_id),
        Err(Ok(Error::Feature(FeatureError::InvalidFeeConfig)))
    );
}
