#![cfg(test)]

use crate::*;
use soroban_sdk::testutils::{Address as _, Events};
use soroban_sdk::{token, Address, BytesN, Env, Symbol, TryFromVal};

fn setup(env: &Env) -> (EscrowContractClient<'_>, Address, Address, Address) {
    env.mock_all_auths();
    let contract_id = env.register(EscrowContract, ());
    let client = EscrowContractClient::new(env, &contract_id);
    let admin = Address::generate(env);
    client.initialize(&admin);

    let token_addr = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let customer = Address::generate(env);
    token::StellarAssetClient::new(env, &token_addr).mint(&customer, &10_000);

    (client, customer, Address::generate(env), token_addr)
}

fn reference(env: &Env, byte: u8) -> BytesN<32> {
    BytesN::from_array(env, &[byte; 32])
}

fn create_with_reference(
    client: &EscrowContractClient,
    customer: &Address,
    merchant: &Address,
    token: &Address,
    amount: i128,
    reference: &BytesN<32>,
) -> u64 {
    client.create_escrow_with_reference(
        customer, merchant, &amount, token, &0_u64, &0_u64, &0_u64, &false, reference,
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
fn lookup_returns_the_matching_escrow() {
    let env = Env::default();
    let (client, customer, merchant, token) = setup(&env);
    let order_ref = reference(&env, 1);

    let escrow_id = create_with_reference(&client, &customer, &merchant, &token, 500, &order_ref);
    assert!(has_event(&env, "escrow_reference_set"));

    let escrow = client.get_escrow_by_reference(&merchant, &order_ref);
    assert_eq!(escrow.id, escrow_id);
    assert_eq!(escrow.customer, customer);
    assert_eq!(escrow.merchant, merchant);
    assert_eq!(escrow.amount, 500);
    assert_eq!(client.get_escrow_reference(&escrow_id), Some(order_ref));
}

#[test]
fn lookup_distinguishes_references_for_the_same_merchant() {
    let env = Env::default();
    let (client, customer, merchant, token) = setup(&env);

    let first = create_with_reference(
        &client,
        &customer,
        &merchant,
        &token,
        100,
        &reference(&env, 1),
    );
    let second = create_with_reference(
        &client,
        &customer,
        &merchant,
        &token,
        200,
        &reference(&env, 2),
    );

    assert_eq!(
        client
            .get_escrow_by_reference(&merchant, &reference(&env, 1))
            .id,
        first
    );
    assert_eq!(
        client
            .get_escrow_by_reference(&merchant, &reference(&env, 2))
            .id,
        second
    );
}

#[test]
fn duplicate_reference_for_same_merchant_is_rejected() {
    let env = Env::default();
    let (client, customer, merchant, token) = setup(&env);
    let order_ref = reference(&env, 7);
    let token_client = token::Client::new(&env, &token);

    create_with_reference(&client, &customer, &merchant, &token, 500, &order_ref);
    let balance_before = token_client.balance(&customer);

    let result = client.try_create_escrow_with_reference(
        &customer, &merchant, &300_i128, &token, &0_u64, &0_u64, &0_u64, &false, &order_ref,
    );
    assert_eq!(
        result,
        Err(Ok(Error::Escrow(EscrowError::DuplicateReference)))
    );
    // Rejected before any funds move.
    assert_eq!(token_client.balance(&customer), balance_before);
}

#[test]
fn same_reference_is_allowed_for_different_merchants() {
    let env = Env::default();
    let (client, customer, merchant_a, token) = setup(&env);
    let merchant_b = Address::generate(&env);
    let order_ref = reference(&env, 3);

    let id_a = create_with_reference(&client, &customer, &merchant_a, &token, 100, &order_ref);
    let id_b = create_with_reference(&client, &customer, &merchant_b, &token, 200, &order_ref);

    assert_ne!(id_a, id_b);
    assert_eq!(
        client.get_escrow_by_reference(&merchant_a, &order_ref).id,
        id_a
    );
    assert_eq!(
        client.get_escrow_by_reference(&merchant_b, &order_ref).id,
        id_b
    );
}

#[test]
fn lookup_of_unknown_reference_returns_not_found() {
    let env = Env::default();
    let (client, customer, merchant, token) = setup(&env);
    create_with_reference(
        &client,
        &customer,
        &merchant,
        &token,
        100,
        &reference(&env, 1),
    );

    assert_eq!(
        client
            .try_get_escrow_by_reference(&merchant, &reference(&env, 9))
            .err(),
        Some(Ok(Error::Escrow(EscrowError::NotFound)))
    );
    // A reference is scoped to its merchant.
    assert_eq!(
        client
            .try_get_escrow_by_reference(&Address::generate(&env), &reference(&env, 1))
            .err(),
        Some(Ok(Error::Escrow(EscrowError::NotFound)))
    );
}

#[test]
fn escrow_created_without_reference_has_none() {
    let env = Env::default();
    let (client, customer, merchant, token) = setup(&env);

    let escrow_id = client.create_escrow(
        &customer, &merchant, &100_i128, &token, &0_u64, &0_u64, &0_u64, &false,
    );
    assert_eq!(client.get_escrow_reference(&escrow_id), None);
}
