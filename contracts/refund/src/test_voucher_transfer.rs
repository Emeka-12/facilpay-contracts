#![cfg(test)]

use super::*;
use soroban_sdk::testutils::{Address as _, Events, Ledger};
use soroban_sdk::{token, Address, Env, String, Symbol, TryFromVal};

const EXPIRY: u64 = 1_000;

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

/// Requests and approves a refund on `payment_id`, then issues a voucher for it.
fn issue_voucher(s: &Setup, payment_id: u64, amount: i128) -> u64 {
    let refund_id = s.client.request_refund(
        &s.merchant,
        &payment_id,
        &s.customer,
        &amount,
        &amount,
        &s.token,
        &String::from_str(&s.env, "store credit"),
        &RefundReasonCode::CustomerRequest,
        &s.env.ledger().timestamp(),
    );
    s.client.approve_refund(&s.admin, &refund_id);
    s.client.issue_refund_voucher(&s.admin, &refund_id, &EXPIRY)
}

fn voucher_ids(s: &Setup, owner: &Address) -> std::vec::Vec<u64> {
    let mut ids: std::vec::Vec<u64> = s
        .client
        .get_customer_vouchers(owner)
        .iter()
        .map(|v| v.voucher_id)
        .collect();
    ids.sort();
    ids
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
fn transfer_moves_voucher_between_owner_indexes() {
    let s = setup();
    let voucher_id = issue_voucher(&s, 1, 500);
    let new_owner = Address::generate(&s.env);

    s.client
        .transfer_voucher(&s.customer, &voucher_id, &new_owner);
    assert!(has_event(&s.env, "voucher_transferred"));

    assert_eq!(
        s.client.get_voucher(&voucher_id).unwrap().customer,
        new_owner
    );
    assert!(voucher_ids(&s, &s.customer).is_empty());
    assert_eq!(voucher_ids(&s, &new_owner), std::vec![voucher_id]);
}

#[test]
fn after_transfer_only_new_owner_can_redeem() {
    let s = setup();
    let voucher_id = issue_voucher(&s, 1, 500);
    let new_owner = Address::generate(&s.env);
    s.client
        .transfer_voucher(&s.customer, &voucher_id, &new_owner);

    assert_eq!(
        s.client
            .try_redeem_refund_voucher(&s.customer, &voucher_id, &1),
        Err(Ok(Error::Core(CoreError::Unauthorized)))
    );

    s.client.redeem_refund_voucher(&new_owner, &voucher_id, &1);
    assert_eq!(
        token::Client::new(&s.env, &s.token).balance(&new_owner),
        500
    );
}

#[test]
fn new_owner_can_transfer_again_but_old_owner_cannot() {
    let s = setup();
    let voucher_id = issue_voucher(&s, 1, 500);
    let second = Address::generate(&s.env);
    let third = Address::generate(&s.env);
    s.client.transfer_voucher(&s.customer, &voucher_id, &second);

    assert_eq!(
        s.client
            .try_transfer_voucher(&s.customer, &voucher_id, &third),
        Err(Ok(Error::Core(CoreError::Unauthorized)))
    );
    s.client.transfer_voucher(&second, &voucher_id, &third);
    assert_eq!(s.client.get_voucher(&voucher_id).unwrap().customer, third);
    assert!(voucher_ids(&s, &second).is_empty());
}

#[test]
fn transfer_from_the_middle_of_an_index_keeps_the_rest() {
    let s = setup();
    let v1 = issue_voucher(&s, 1, 100);
    let v2 = issue_voucher(&s, 2, 200);
    let v3 = issue_voucher(&s, 3, 300);
    let new_owner = Address::generate(&s.env);

    s.client.transfer_voucher(&s.customer, &v2, &new_owner);

    assert_eq!(voucher_ids(&s, &s.customer), std::vec![v1, v3]);
    assert_eq!(voucher_ids(&s, &new_owner), std::vec![v2]);
}

#[test]
fn merchant_can_issue_non_transferable_vouchers() {
    let s = setup();
    assert!(s.client.get_vouchers_transferable(&s.merchant));
    let before = issue_voucher(&s, 1, 100);

    s.client.set_vouchers_transferable(&s.merchant, &false);
    assert!(has_event(&s.env, "voucher_transferability_set"));
    assert!(!s.client.get_vouchers_transferable(&s.merchant));
    let locked = issue_voucher(&s, 2, 200);
    assert!(!s.client.is_voucher_transferable(&locked));

    let recipient = Address::generate(&s.env);
    assert_eq!(
        s.client
            .try_transfer_voucher(&s.customer, &locked, &recipient),
        Err(Ok(Error::Ext(ExtError::VoucherNotTransferable)))
    );
    // The setting is snapshotted at issuance: earlier vouchers stay
    // transferable, and re-enabling doesn't unlock `locked`.
    s.client.transfer_voucher(&s.customer, &before, &recipient);
    s.client.set_vouchers_transferable(&s.merchant, &true);
    assert!(!s.client.is_voucher_transferable(&locked));

    // The owner can still redeem a non-transferable voucher.
    s.client.redeem_refund_voucher(&s.customer, &locked, &2);
}

#[test]
fn redeemed_voucher_cannot_be_transferred() {
    let s = setup();
    let voucher_id = issue_voucher(&s, 1, 500);
    s.client.redeem_refund_voucher(&s.customer, &voucher_id, &1);

    assert_eq!(
        s.client
            .try_transfer_voucher(&s.customer, &voucher_id, &Address::generate(&s.env)),
        Err(Ok(Error::Ext(ExtError::VoucherAlreadyRedeemed)))
    );
}

#[test]
fn expired_voucher_cannot_be_transferred() {
    let s = setup();
    let voucher_id = issue_voucher(&s, 1, 500);
    let expires_at = s.client.get_voucher(&voucher_id).unwrap().expires_at;

    s.env.ledger().set_timestamp(expires_at + 1);
    assert_eq!(
        s.client
            .try_transfer_voucher(&s.customer, &voucher_id, &Address::generate(&s.env)),
        Err(Ok(Error::Ext(ExtError::VoucherExpired)))
    );
}

#[test]
fn transfer_rejects_non_owner_self_transfer_and_unknown_voucher() {
    let s = setup();
    let voucher_id = issue_voucher(&s, 1, 500);
    let stranger = Address::generate(&s.env);

    assert_eq!(
        s.client
            .try_transfer_voucher(&stranger, &voucher_id, &stranger),
        Err(Ok(Error::Core(CoreError::Unauthorized)))
    );
    assert_eq!(
        s.client
            .try_transfer_voucher(&s.customer, &voucher_id, &s.customer),
        Err(Ok(Error::Ext(ExtError::InvalidVoucherRecipient)))
    );
    assert_eq!(
        s.client.try_transfer_voucher(&s.customer, &99, &stranger),
        Err(Ok(Error::Ext(ExtError::VoucherNotFound)))
    );
}
