#![cfg(test)]

//! Round-robin arbitrator auto-assignment (#198) and tiered escalation (#194).

use super::*;
use soroban_sdk::{
    testutils::{Address as _, Ledger},
    token, vec, Address, BytesN, Env, String,
};

const START: u64 = 10_000;
const ESCALATION_TIMEOUT: u64 = 3_600;

struct Setup<'a> {
    env: Env,
    client: RefundContractClient<'a>,
    admin: Address,
    merchant: Address,
    customer: Address,
    token: Address,
    arbs: std::vec::Vec<Address>,
    next_payment_id: core::cell::Cell<u64>,
}

/// Registers `arb_count` arbitrators, in order, on a fresh contract.
fn setup<'a>(arb_count: usize) -> Setup<'a> {
    let env = Env::default();
    env.mock_all_auths();
    env.ledger().set_timestamp(START);

    let admin = Address::generate(&env);
    let merchant = Address::generate(&env);
    let customer = Address::generate(&env);

    let token = env
        .register_stellar_asset_contract_v2(admin.clone())
        .address();
    let token_admin = token::StellarAssetClient::new(&env, &token);
    token_admin.mint(&merchant, &1_000_000);
    token_admin.mint(&customer, &1_000_000);

    let contract_id = env.register(RefundContract, ());
    let client = RefundContractClient::new(&env, &contract_id);
    client.initialize(&admin);

    let mut arbs = std::vec::Vec::new();
    for _ in 0..arb_count {
        let arb = Address::generate(&env);
        client.register_arbitrator(&admin, &arb);
        arbs.push(arb);
    }

    Setup {
        env,
        client,
        admin,
        merchant,
        customer,
        token,
        arbs,
        next_payment_id: core::cell::Cell::new(1),
    }
}

/// Opens an arbitration case for a freshly rejected refund. The merchant pays
/// the arbitration fee in the real token.
fn open_case(s: &Setup) -> u64 {
    let payment_id = s.next_payment_id.get();
    s.next_payment_id.set(payment_id + 1);

    let refund_id = s.client.request_refund(
        &s.merchant,
        &payment_id,
        &s.customer,
        &1_000i128,
        &10_000i128,
        &s.token,
        &String::from_str(&s.env, "reason"),
        &RefundReasonCode::Other,
        &START,
    );
    s.client
        .reject_refund(&s.admin, &refund_id, &String::from_str(&s.env, "rejected"));
    s.client
        .escalate_to_arbitration(&s.merchant, &refund_id, &s.token, &300i128)
}

fn panel(s: &Setup, idx: &[usize]) -> Vec<Address> {
    let mut v = Vec::new(&s.env);
    for i in idx {
        v.push_back(s.arbs[*i].clone());
    }
    v
}

fn hash(env: &Env) -> BytesN<32> {
    BytesN::from_array(env, &[7u8; 32])
}

fn tier_config() -> ArbitrationTierConfig {
    ArbitrationTierConfig {
        junior_quorum: 3,
        senior_quorum: 2,
        escalation_timeout_seconds: ESCALATION_TIMEOUT,
    }
}

// ── configure_auto_assignment ────────────────────────────────────────────────

#[test]
fn test_configure_auto_assignment_requires_admin() {
    let s = setup(3);
    let outsider = Address::generate(&s.env);
    assert_eq!(
        s.client.try_configure_auto_assignment(&outsider, &2),
        Err(Ok(Error::Core(CoreError::Unauthorized)))
    );
    // Nothing was configured, so previews stay empty.
    assert_eq!(s.client.get_next_arbitrators(&2).len(), 0);
}

#[test]
fn test_configure_auto_assignment_without_arbitrators_fails() {
    let s = setup(0);
    assert_eq!(
        s.client.try_configure_auto_assignment(&s.admin, &1),
        Err(Ok(Error::Ext(ExtError::ArbitratorNotFound)))
    );
}

#[test]
fn test_configure_auto_assignment_panel_larger_than_pool_fails() {
    let s = setup(3);
    assert_eq!(
        s.client.try_configure_auto_assignment(&s.admin, &4),
        Err(Ok(Error::Ext(ExtError::ArbitratorNotFound)))
    );
    // Exactly the pool size is allowed.
    s.client.configure_auto_assignment(&s.admin, &3);
    assert_eq!(s.client.get_next_arbitrators(&3), panel(&s, &[0, 1, 2]));
}

// ── auto_assign_arbitrators / get_next_arbitrators ──────────────────────────

#[test]
fn test_round_robin_order_across_consecutive_cases() {
    let s = setup(5);
    s.client.configure_auto_assignment(&s.admin, &2);

    let expected: [[usize; 2]; 5] = [[0, 1], [2, 3], [4, 0], [1, 2], [3, 4]];
    for want in expected.iter() {
        let case_id = open_case(&s);
        assert_eq!(s.client.get_next_arbitrators(&2), panel(&s, want));
        let assigned = s.client.auto_assign_arbitrators(&case_id);
        assert_eq!(assigned, panel(&s, want));
    }
    // A full cycle (5 cases × 2 = 10 slots = 2 × pool) lands back on the start.
    assert_eq!(s.client.get_next_arbitrators(&2), panel(&s, &[0, 1]));
}

#[test]
fn test_assigned_panel_is_stored_on_case() {
    let s = setup(5);
    s.client.configure_auto_assignment(&s.admin, &3);

    let case_a = open_case(&s);
    let case_b = open_case(&s);
    // escalate_to_arbitration seeds every case with the full pool.
    assert_eq!(s.client.get_arbitration_case(&case_a).arbitrators.len(), 5);

    let panel_a = s.client.auto_assign_arbitrators(&case_a);
    let panel_b = s.client.auto_assign_arbitrators(&case_b);

    assert_eq!(panel_a, panel(&s, &[0, 1, 2]));
    assert_eq!(panel_b, panel(&s, &[3, 4, 0]));
    assert_eq!(s.client.get_arbitration_case(&case_a).arbitrators, panel_a);
    assert_eq!(s.client.get_arbitration_case(&case_b).arbitrators, panel_b);

    // The stored panel is what gates voting on the case.
    assert_eq!(
        s.client
            .try_cast_arbitration_vote(&s.arbs[3], &case_a, &true, &hash(&s.env)),
        Err(Ok(Error::Core(CoreError::NotArbitrator)))
    );
    s.client
        .cast_arbitration_vote(&s.arbs[0], &case_a, &true, &hash(&s.env));
    assert_eq!(s.client.get_arbitration_case(&case_a).votes_for_refund, 1);
}

#[test]
fn test_get_next_arbitrators_does_not_advance_rotation() {
    let s = setup(4);
    assert_eq!(s.client.get_next_arbitrators(&2).len(), 0); // not configured yet

    s.client.configure_auto_assignment(&s.admin, &2);
    assert_eq!(s.client.get_next_arbitrators(&2), panel(&s, &[0, 1]));
    assert_eq!(s.client.get_next_arbitrators(&2), panel(&s, &[0, 1]));

    // Preview size is independent of panel size and clamps to the pool.
    assert_eq!(s.client.get_next_arbitrators(&3), panel(&s, &[0, 1, 2]));
    assert_eq!(s.client.get_next_arbitrators(&9), panel(&s, &[0, 1, 2, 3]));
    assert_eq!(s.client.get_next_arbitrators(&0).len(), 0);

    let case_id = open_case(&s);
    s.client.auto_assign_arbitrators(&case_id);
    assert_eq!(s.client.get_next_arbitrators(&2), panel(&s, &[2, 3]));
}

#[test]
fn test_auto_assign_without_configuration_fails() {
    let s = setup(3);
    let case_id = open_case(&s);
    assert_eq!(
        s.client.try_auto_assign_arbitrators(&case_id),
        Err(Ok(Error::Core(CoreError::PolicyNotFound)))
    );
}

#[test]
fn test_auto_assign_unknown_case_fails_without_advancing_rotation() {
    let s = setup(3);
    s.client.configure_auto_assignment(&s.admin, &2);

    assert_eq!(
        s.client.try_auto_assign_arbitrators(&999),
        Err(Ok(Error::Core(CoreError::RefundNotFound)))
    );
    // The failed call is rolled back, rotation index included.
    assert_eq!(s.client.get_next_arbitrators(&2), panel(&s, &[0, 1]));
}

#[test]
fn test_auto_assign_not_enough_available_arbitrators_fails() {
    let s = setup(4);
    s.client.configure_auto_assignment(&s.admin, &3);
    let case_id = open_case(&s);

    // Two arbitrators opt out, leaving 2 available for a panel of 3.
    s.client.set_arbitrator_availability(&s.arbs[1], &false);
    s.client.set_arbitrator_availability(&s.arbs[2], &false);

    assert_eq!(
        s.client.try_auto_assign_arbitrators(&case_id),
        Err(Ok(Error::Ext(ExtError::ArbitratorNotFound)))
    );

    // Back to 3 available: rotation runs over the available list only.
    s.client.set_arbitrator_availability(&s.arbs[2], &true);
    assert_eq!(
        s.client.auto_assign_arbitrators(&case_id),
        panel(&s, &[0, 2, 3])
    );
}

// ── reset_rotation_index ────────────────────────────────────────────────────

#[test]
fn test_reset_rotation_index_restarts_order() {
    let s = setup(5);
    s.client.configure_auto_assignment(&s.admin, &2);

    let c1 = open_case(&s);
    let c2 = open_case(&s);
    assert_eq!(s.client.auto_assign_arbitrators(&c1), panel(&s, &[0, 1]));
    assert_eq!(s.client.auto_assign_arbitrators(&c2), panel(&s, &[2, 3]));
    assert_eq!(s.client.get_next_arbitrators(&2), panel(&s, &[4, 0]));

    s.client.reset_rotation_index(&s.admin);

    assert_eq!(s.client.get_next_arbitrators(&2), panel(&s, &[0, 1]));
    let c3 = open_case(&s);
    let c4 = open_case(&s);
    assert_eq!(s.client.auto_assign_arbitrators(&c3), panel(&s, &[0, 1]));
    assert_eq!(s.client.auto_assign_arbitrators(&c4), panel(&s, &[2, 3]));
    assert_eq!(
        s.client.get_arbitration_case(&c3).arbitrators,
        panel(&s, &[0, 1])
    );
}

#[test]
fn test_reset_rotation_index_requires_admin() {
    let s = setup(3);
    s.client.configure_auto_assignment(&s.admin, &2);
    let case_id = open_case(&s);
    s.client.auto_assign_arbitrators(&case_id);

    let outsider = Address::generate(&s.env);
    assert_eq!(
        s.client.try_reset_rotation_index(&outsider),
        Err(Ok(Error::Core(CoreError::Unauthorized)))
    );
    assert_eq!(s.client.get_next_arbitrators(&2), panel(&s, &[2, 0]));
}

#[test]
fn test_reset_rotation_index_without_configuration_fails() {
    let s = setup(3);
    assert_eq!(
        s.client.try_reset_rotation_index(&s.admin),
        Err(Ok(Error::Core(CoreError::PolicyNotFound)))
    );
}

#[test]
fn test_reconfiguring_resets_rotation() {
    let s = setup(4);
    s.client.configure_auto_assignment(&s.admin, &2);
    let case_id = open_case(&s);
    s.client.auto_assign_arbitrators(&case_id);
    assert_eq!(s.client.get_next_arbitrators(&3), panel(&s, &[2, 3, 0]));

    s.client.configure_auto_assignment(&s.admin, &3);
    let case_id = open_case(&s);
    assert_eq!(
        s.client.auto_assign_arbitrators(&case_id),
        panel(&s, &[0, 1, 2])
    );
}

// ── add_senior_arbitrator / set_arbitration_tier_config ─────────────────────

#[test]
fn test_add_senior_arbitrator_requires_admin() {
    let s = setup(3);
    let outsider = Address::generate(&s.env);
    let senior = Address::generate(&s.env);
    assert_eq!(
        s.client.try_add_senior_arbitrator(&outsider, &senior),
        Err(Ok(Error::Core(CoreError::Unauthorized)))
    );
}

#[test]
fn test_set_arbitration_tier_config_requires_admin() {
    let s = setup(3);
    let outsider = Address::generate(&s.env);
    assert_eq!(
        s.client
            .try_set_arbitration_tier_config(&outsider, &tier_config()),
        Err(Ok(Error::Core(CoreError::Unauthorized)))
    );
}

// ── escalate_arbitration_case ───────────────────────────────────────────────

#[test]
fn test_escalation_moves_case_to_senior_tier_and_clears_votes() {
    let s = setup(3);
    let senior1 = Address::generate(&s.env);
    let senior2 = Address::generate(&s.env);
    s.client.add_senior_arbitrator(&s.admin, &senior1);
    s.client.add_senior_arbitrator(&s.admin, &senior2);
    // A junior can also sit on the senior bench.
    s.client.add_senior_arbitrator(&s.admin, &s.arbs[0]);
    // Adding the same senior twice is a no-op.
    s.client.add_senior_arbitrator(&s.admin, &senior1);
    s.client
        .set_arbitration_tier_config(&s.admin, &tier_config());

    let case_id = open_case(&s);
    assert_eq!(
        s.client.get_arbitration_tier(&case_id),
        ArbitratorTier::Junior
    );

    // Juniors vote 1–1 without reaching quorum.
    s.client
        .cast_arbitration_vote(&s.arbs[0], &case_id, &true, &hash(&s.env));
    s.client
        .cast_arbitration_vote(&s.arbs[1], &case_id, &false, &hash(&s.env));
    let case = s.client.get_arbitration_case(&case_id);
    assert_eq!((case.votes_for_refund, case.votes_against_refund), (1, 1));

    s.env.ledger().set_timestamp(START + ESCALATION_TIMEOUT);
    s.client.escalate_arbitration_case(&case_id);

    let case = s.client.get_arbitration_case(&case_id);
    assert_eq!(
        case.arbitrators,
        vec![&s.env, senior1.clone(), senior2.clone(), s.arbs[0].clone()]
    );
    assert_eq!(case.votes_for_refund, 0);
    assert_eq!(case.votes_against_refund, 0);
    assert_eq!(case.status, ArbitrationStatus::Open);
    assert_eq!(
        s.client.get_arbitration_tier(&case_id),
        ArbitratorTier::Senior
    );

    // Juniors who are not seniors are off the panel.
    assert_eq!(
        s.client
            .try_cast_arbitration_vote(&s.arbs[1], &case_id, &true, &hash(&s.env)),
        Err(Ok(Error::Core(CoreError::NotArbitrator)))
    );
    // Prior vote records were cleared, so arbs[0] can vote again as a senior.
    s.client
        .cast_arbitration_vote(&s.arbs[0], &case_id, &false, &hash(&s.env));
    s.client
        .cast_arbitration_vote(&senior1, &case_id, &false, &hash(&s.env));
    let case = s.client.get_arbitration_case(&case_id);
    assert_eq!((case.votes_for_refund, case.votes_against_refund), (0, 2));
}

#[test]
fn test_escalation_before_timeout_fails() {
    let s = setup(3);
    s.client
        .add_senior_arbitrator(&s.admin, &Address::generate(&s.env));
    s.client
        .set_arbitration_tier_config(&s.admin, &tier_config());
    let case_id = open_case(&s);

    s.env.ledger().set_timestamp(START + ESCALATION_TIMEOUT - 1);
    assert_eq!(
        s.client.try_escalate_arbitration_case(&case_id),
        Err(Ok(Error::Core(CoreError::CaseNotTimedOut)))
    );
    assert_eq!(
        s.client.get_arbitration_tier(&case_id),
        ArbitratorTier::Junior
    );
}

#[test]
fn test_escalation_without_tier_config_fails() {
    let s = setup(3);
    s.client
        .add_senior_arbitrator(&s.admin, &Address::generate(&s.env));
    let case_id = open_case(&s);
    s.env.ledger().set_timestamp(START + 30 * 86_400);

    assert_eq!(
        s.client.try_escalate_arbitration_case(&case_id),
        Err(Ok(Error::Core(CoreError::CaseNotTimedOut)))
    );
}

#[test]
fn test_escalation_without_senior_arbitrators_fails() {
    let s = setup(3);
    s.client
        .set_arbitration_tier_config(&s.admin, &tier_config());
    let case_id = open_case(&s);
    s.env.ledger().set_timestamp(START + ESCALATION_TIMEOUT);

    assert_eq!(
        s.client.try_escalate_arbitration_case(&case_id),
        Err(Ok(Error::Ext(ExtError::ArbitratorNotFound)))
    );
    // The junior panel is untouched.
    assert_eq!(
        s.client.get_arbitration_case(&case_id).arbitrators,
        panel(&s, &[0, 1, 2])
    );
}

#[test]
fn test_escalating_twice_fails() {
    let s = setup(3);
    s.client
        .add_senior_arbitrator(&s.admin, &Address::generate(&s.env));
    s.client
        .set_arbitration_tier_config(&s.admin, &tier_config());
    let case_id = open_case(&s);
    s.env.ledger().set_timestamp(START + ESCALATION_TIMEOUT);

    s.client.escalate_arbitration_case(&case_id);
    assert_eq!(
        s.client.try_escalate_arbitration_case(&case_id),
        Err(Ok(Error::Ext(ExtError::CaseAlreadyEscalated)))
    );
}

#[test]
fn test_escalating_decided_case_fails() {
    let s = setup(3);
    s.client
        .add_senior_arbitrator(&s.admin, &Address::generate(&s.env));
    s.client
        .set_arbitration_tier_config(&s.admin, &tier_config());
    let case_id = open_case(&s);

    for arb in s.arbs.iter() {
        s.client
            .cast_arbitration_vote(arb, &case_id, &true, &hash(&s.env));
    }
    s.client.close_arbitration_case(&case_id);
    assert_eq!(
        s.client.get_arbitration_case(&case_id).status,
        ArbitrationStatus::Decided
    );

    s.env.ledger().set_timestamp(START + ESCALATION_TIMEOUT);
    assert_eq!(
        s.client.try_escalate_arbitration_case(&case_id),
        Err(Ok(Error::Core(CoreError::InvalidStatus)))
    );
}

#[test]
fn test_escalating_unknown_case_fails() {
    let s = setup(3);
    assert_eq!(
        s.client.try_escalate_arbitration_case(&42),
        Err(Ok(Error::Core(CoreError::RefundNotFound)))
    );
}
