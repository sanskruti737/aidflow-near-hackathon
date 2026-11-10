//! Unit tests for the AidFlow contract.
//!
//! The suite drives the contract through the mocked blockchain environment
//! and validates:
//!
//! - campaign creation & validation
//! - donation accounting (net-of-storage-fee crediting)
//! - milestone voting rules (donor-only, one vote each, time windows)
//! - milestone finalization rules (incl. refund-mode override)
//! - fund release accounting incl. asynchronous transfer callbacks
//!   (success confirmation, failure rollback, retry, double-release guard)
//! - pro-rata refund math incl. callbacks
//! - all public views

use crate::types::{mul_div, Balance, CampaignId, MilestoneId};
use crate::AidFlow;

use near_sdk::json_types::U128;
use near_sdk::test_utils::{accounts, VMContextBuilder};
use near_sdk::{
    testing_env, test_vm_config, Gas, NearToken, PromiseResult, RuntimeFeesConfig,
};
use std::collections::HashMap;

const CONTRACT_ACC: usize = 0;
const OWNER: usize = 1;
const ALICE: usize = 2;
const BOB: usize = 3;
const CAROL: usize = 4;

const NEAR: Balance = 10u128.pow(24);
const T0: u64 = 1_700_000_000_000; // fixed epoch, in milliseconds
const DAY_MS: u64 = 86_400_000;
const MONTH_MS: u64 = 30 * DAY_MS;

fn base_ctx() -> VMContextBuilder {
    let mut b = VMContextBuilder::new();
    b.current_account_id(accounts(CONTRACT_ACC))
        .signer_account_id(accounts(OWNER))
        .predecessor_account_id(accounts(OWNER))
        .block_timestamp(T0 * 1_000_000) // nanoseconds
        .account_balance(NearToken::from_near(1000))
        .prepaid_gas(Gas::from_tgas(300));
    b
}

/// Set the environment for a contract call by `caller` with `deposit`
/// (yoctoNEAR) at time `ts_ms` (milliseconds).
fn set_env(caller: usize, deposit: Balance, ts_ms: u64) {
    let mut b = base_ctx();
    b.predecessor_account_id(accounts(caller))
        .signer_account_id(accounts(caller))
        .attached_deposit(NearToken::from_yoctonear(deposit))
        .block_timestamp(ts_ms * 1_000_000);
    testing_env!(b.build());
}

/// Set the environment as the contract itself with one promise result,
/// simulating a callback invoked by our own transfer promise.
fn set_env_callback(ts_ms: u64, result: PromiseResult) {
    let mut b = base_ctx();
    b.predecessor_account_id(accounts(CONTRACT_ACC))
        .signer_account_id(accounts(CONTRACT_ACC))
        .block_timestamp(ts_ms * 1_000_000);
    testing_env!(
        b.build(),
        test_vm_config(),
        RuntimeFeesConfig::test(),
        HashMap::default(),
        vec![result]
    );
}

fn fresh_contract() -> AidFlow {
    AidFlow::new()
}

fn create_campaign_ok(
    c: &mut AidFlow,
    caller: usize,
    target: Balance,
    deadline_ms: u64,
) -> CampaignId {
    set_env(caller, NEAR / 2, T0);
    let info = c.create_campaign(
        "Flood Relief Kerala".to_string(),
        "Rebuild homes destroyed by monsoon floods.".to_string(),
        "https://example.com/img.png".to_string(),
        "Disaster".to_string(),
        U128(target),
        deadline_ms,
    );
    info.id
}

fn donate_ok(c: &mut AidFlow, caller: usize, campaign: CampaignId, amount: Balance) -> Balance {
    set_env(caller, amount, T0);
    let U128(credited) = c.donate(campaign, "Stay strong!".to_string());
    credited
}

fn create_milestone_ok(
    c: &mut AidFlow,
    campaign: CampaignId,
    amount: Balance,
    voting_ms: u64,
) -> MilestoneId {
    set_env(OWNER, 0, T0);
    let info = c.create_milestone(
        campaign,
        "Emergency supplies".to_string(),
        "Food, water and medicine.".to_string(),
        U128(amount),
        voting_ms,
    );
    info.id
}

// -----------------------------------------------------------------------
// Campaign creation
// -----------------------------------------------------------------------

#[test]
fn create_campaign_stores_state_and_returns_info() {
    let mut c = fresh_contract();
    let id = create_campaign_ok(&mut c, OWNER, 10 * NEAR, T0 + MONTH_MS);

    set_env(OWNER, 0, T0);
    let info = c.get_campaign(id);
    assert_eq!(info.id, id);
    assert_eq!(info.owner, accounts(OWNER));
    assert_eq!(info.title, "Flood Relief Kerala");
    assert_eq!(info.category, "Disaster");
    assert_eq!(info.target.0, 10 * NEAR);
    assert_eq!(info.deadline, T0 + MONTH_MS);
    assert_eq!(info.total_donated.0, 0);
    assert_eq!(info.available_balance.0, 0);
    assert_eq!(info.status, "Active");
    assert!(!info.refundable);
    assert!(info.accepting_donations);
    assert_eq!(info.progress_percent, 0);
    assert_eq!(c.get_campaign_count(), 1);
}

#[test]
fn create_campaign_increments_ids_and_lists_all() {
    let mut c = fresh_contract();
    let a = create_campaign_ok(&mut c, OWNER, NEAR, T0 + DAY_MS);
    let b = create_campaign_ok(&mut c, ALICE, 2 * NEAR, T0 + 2 * DAY_MS);
    assert_eq!(a, 0);
    assert_eq!(b, 1);

    set_env(OWNER, 0, T0);
    let all = c.get_all_campaigns(0, 10);
    assert_eq!(all.len(), 2);
    assert_eq!(c.get_campaign_count(), 2);
    assert_eq!(all[1].owner, accounts(ALICE));
}

#[test]
#[should_panic(expected = "Deadline must be at least 1 minute in the future")]
fn create_campaign_rejects_past_deadline() {
    let mut c = fresh_contract();
    create_campaign_ok(&mut c, OWNER, NEAR, T0); // deadline == now
}

#[test]
#[should_panic(expected = "Target must be positive")]
fn create_campaign_rejects_zero_target() {
    let mut c = fresh_contract();
    create_campaign_ok(&mut c, OWNER, 0, T0 + DAY_MS);
}

#[test]
#[should_panic(expected = "Title cannot be empty")]
fn create_campaign_rejects_empty_title() {
    let mut c = fresh_contract();
    set_env(OWNER, NEAR / 2, T0);
    c.create_campaign(
        "  ".to_string(),
        "d".to_string(),
        String::new(),
        "Health".to_string(),
        U128(NEAR),
        T0 + DAY_MS,
    );
}

// -----------------------------------------------------------------------
// Donations
// -----------------------------------------------------------------------

#[test]
fn donate_credits_campaign_and_donor() {
    let mut c = fresh_contract();
    let id = create_campaign_ok(&mut c, OWNER, 10 * NEAR, T0 + DAY_MS);

    let credited = donate_ok(&mut c, ALICE, id, NEAR);
    // Storage fee is deducted from the credited amount.
    assert!(credited > 0 && credited < NEAR);
    assert!(credited > NEAR - NEAR / 100); // fee is tiny

    set_env(ALICE, 0, T0);
    let balance = c.get_campaign_balance(id);
    assert_eq!(balance.total_donated.0, credited);
    assert_eq!(balance.available_balance.0, credited);
    assert_eq!(balance.donor_count, 1);
    assert_eq!(c.get_donor_credit(id, accounts(ALICE)).0, credited);
    assert_eq!(c.get_campaign(id).donation_count, 1);

    let stats = c.get_contract_stats();
    assert_eq!(stats.total_donated.0, credited);
}

#[test]
fn donate_from_second_donor_increments_donor_count() {
    let mut c = fresh_contract();
    let id = create_campaign_ok(&mut c, OWNER, 10 * NEAR, T0 + DAY_MS);
    donate_ok(&mut c, ALICE, id, NEAR);
    donate_ok(&mut c, BOB, id, NEAR);
    donate_ok(&mut c, ALICE, id, NEAR); // repeat donor

    set_env(OWNER, 0, T0);
    assert_eq!(c.get_campaign(id).donor_count, 2);
    assert_eq!(c.get_campaign(id).donation_count, 3);
    // Repeat donations overwrite existing keys: no storage fee beyond the
    // two new log records, but still strictly less than the gross amount.
    assert!(c.get_donor_credit(id, accounts(ALICE)).0 > NEAR);
    assert!(c.get_donor_credit(id, accounts(ALICE)).0 < 2 * NEAR);
}

#[test]
fn donation_history_is_recorded_per_campaign_and_donor() {
    let mut c = fresh_contract();
    let id = create_campaign_ok(&mut c, OWNER, 10 * NEAR, T0 + DAY_MS);
    let credited = donate_ok(&mut c, ALICE, id, NEAR);

    set_env(OWNER, 0, T0);
    let history = c.get_donation_history(id, 0, 10);
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].donor, accounts(ALICE));
    assert_eq!(history[0].amount.0, credited);
    assert_eq!(history[0].gross_attached.0, NEAR);
    assert_eq!(history[0].message, "Stay strong!");
    assert!(!history[0].refunded);

    let mine = c.get_my_donations(accounts(ALICE), 0, 10);
    assert_eq!(mine.len(), 1);
    assert_eq!(mine[0].campaign_id, id);
    assert!(c.get_my_donations(accounts(BOB), 0, 10).is_empty());
}

#[test]
#[should_panic(expected = "Attach some NEAR to donate")]
fn donate_requires_deposit() {
    let mut c = fresh_contract();
    let id = create_campaign_ok(&mut c, OWNER, NEAR, T0 + DAY_MS);
    set_env(ALICE, 0, T0);
    let _ = c.donate(id, "x".to_string());
}

#[test]
#[should_panic(expected = "Campaign is no longer accepting donations")]
fn donate_after_deadline_fails() {
    let mut c = fresh_contract();
    let id = create_campaign_ok(&mut c, OWNER, NEAR, T0 + DAY_MS);
    set_env(ALICE, NEAR, T0 + 2 * DAY_MS);
    let _ = c.donate(id, "late".to_string());
}

#[test]
#[should_panic(expected = "Donation too small to cover on-chain storage")]
fn donate_too_small_for_storage_fails() {
    let mut c = fresh_contract();
    let id = create_campaign_ok(&mut c, OWNER, NEAR, T0 + DAY_MS);
    // 0.001 NEAR cannot cover the storage of the donation records.
    set_env(ALICE, NEAR / 1000, T0);
    let _ = c.donate(id, "tiny".to_string());
}

// -----------------------------------------------------------------------
// Milestones & voting
// -----------------------------------------------------------------------

#[test]
fn milestone_lifecycle_vote_approve_finalize() {
    let mut c = fresh_contract();
    let id = create_campaign_ok(&mut c, OWNER, 10 * NEAR, T0 + MONTH_MS);
    donate_ok(&mut c, ALICE, id, 4 * NEAR);
    donate_ok(&mut c, BOB, id, 3 * NEAR);
    donate_ok(&mut c, CAROL, id, 2 * NEAR);

    let mid = create_milestone_ok(&mut c, id, 5 * NEAR, DAY_MS);

    set_env(OWNER, 0, T0);
    let milestones = c.get_campaign_milestones(id);
    assert_eq!(milestones.len(), 1);
    assert_eq!(milestones[0].state, "VotingOpen");
    assert!(milestones[0].voting_open);
    assert_eq!(milestones[0].amount.0, 5 * NEAR);

    set_env(ALICE, 0, T0 + 1000);
    c.vote_on_milestone(id, mid, true);
    set_env(BOB, 0, T0 + 2000);
    c.vote_on_milestone(id, mid, true);
    set_env(CAROL, 0, T0 + 3000);
    c.vote_on_milestone(id, mid, false);

    set_env(OWNER, 0, T0 + 4000);
    let m = c.get_milestone(id, mid);
    assert_eq!(m.votes_for, 2);
    assert_eq!(m.votes_against, 1);
    assert!(c.has_voted(id, mid, accounts(BOB)));
    assert!(!c.has_voted(id, mid, accounts(OWNER)));

    // Finalize after the voting window closes: 2 for, 1 against -> approved.
    let later = T0 + DAY_MS + 1;
    set_env(CAROL, 0, later); // finalization is permissionless
    let info = c.finalize_milestone(id, mid);
    assert_eq!(info.state, "Approved");
    assert!(!info.voting_open);
    assert_eq!(c.get_milestone(id, mid).state, "Approved");
}

#[test]
#[should_panic(expected = "Only donors of this campaign can vote")]
fn non_donor_cannot_vote() {
    let mut c = fresh_contract();
    let id = create_campaign_ok(&mut c, OWNER, 10 * NEAR, T0 + MONTH_MS);
    donate_ok(&mut c, ALICE, id, NEAR);
    let mid = create_milestone_ok(&mut c, id, NEAR, DAY_MS);

    set_env(CAROL, 0, T0);
    c.vote_on_milestone(id, mid, true);
}

#[test]
#[should_panic(expected = "You already voted on this milestone")]
fn double_vote_rejected() {
    let mut c = fresh_contract();
    let id = create_campaign_ok(&mut c, OWNER, 10 * NEAR, T0 + MONTH_MS);
    donate_ok(&mut c, ALICE, id, NEAR);
    let mid = create_milestone_ok(&mut c, id, NEAR, DAY_MS);

    set_env(ALICE, 0, T0);
    c.vote_on_milestone(id, mid, true);
    c.vote_on_milestone(id, mid, true);
}

#[test]
#[should_panic(expected = "Voting window has ended")]
fn vote_after_window_fails() {
    let mut c = fresh_contract();
    let id = create_campaign_ok(&mut c, OWNER, 10 * NEAR, T0 + MONTH_MS);
    donate_ok(&mut c, ALICE, id, NEAR);
    let mid = create_milestone_ok(&mut c, id, NEAR, DAY_MS);

    set_env(ALICE, 0, T0 + DAY_MS + 2);
    c.vote_on_milestone(id, mid, true);
}

#[test]
#[should_panic(expected = "Voting window is still open")]
fn finalize_before_window_fails() {
    let mut c = fresh_contract();
    let id = create_campaign_ok(&mut c, OWNER, 10 * NEAR, T0 + MONTH_MS);
    donate_ok(&mut c, ALICE, id, NEAR);
    let mid = create_milestone_ok(&mut c, id, NEAR, DAY_MS);
    set_env(OWNER, 0, T0 + 100);
    let _ = c.finalize_milestone(id, mid);
}

#[test]
fn finalize_rejects_when_opposition_wins() {
    let mut c = fresh_contract();
    let id = create_campaign_ok(&mut c, OWNER, 10 * NEAR, T0 + MONTH_MS);
    donate_ok(&mut c, ALICE, id, NEAR);
    donate_ok(&mut c, BOB, id, NEAR);
    let mid = create_milestone_ok(&mut c, id, NEAR, DAY_MS);

    set_env(ALICE, 0, T0);
    c.vote_on_milestone(id, mid, false);
    set_env(BOB, 0, T0);
    c.vote_on_milestone(id, mid, true);

    let later = T0 + DAY_MS + 1;
    set_env(OWNER, 0, later);
    let info = c.finalize_milestone(id, mid);
    assert_eq!(info.state, "Rejected");
}

#[test]
fn finalize_rejects_on_zero_votes() {
    let mut c = fresh_contract();
    let id = create_campaign_ok(&mut c, OWNER, 10 * NEAR, T0 + MONTH_MS);
    donate_ok(&mut c, ALICE, id, NEAR);
    let mid = create_milestone_ok(&mut c, id, NEAR, DAY_MS);

    let later = T0 + DAY_MS + 1;
    set_env(OWNER, 0, later);
    let info = c.finalize_milestone(id, mid);
    assert_eq!(info.state, "Rejected");
}

#[test]
#[should_panic(expected = "Only the campaign owner can add milestones")]
fn non_owner_cannot_create_milestone() {
    let mut c = fresh_contract();
    let id = create_campaign_ok(&mut c, OWNER, 10 * NEAR, T0 + MONTH_MS);
    donate_ok(&mut c, ALICE, id, NEAR);
    set_env(ALICE, 0, T0);
    let _ = c.create_milestone(
        id,
        "Hijack".to_string(),
        "not allowed".to_string(),
        U128(NEAR),
        DAY_MS,
    );
}

#[test]
#[should_panic(expected = "Milestone amounts exceed the campaign target")]
fn milestones_cannot_exceed_target() {
    let mut c = fresh_contract();
    let id = create_campaign_ok(&mut c, OWNER, 10 * NEAR, T0 + MONTH_MS);
    donate_ok(&mut c, ALICE, id, 10 * NEAR);
    create_milestone_ok(&mut c, id, 6 * NEAR, DAY_MS);
    // 6 + 6 > 10
    create_milestone_ok(&mut c, id, 6 * NEAR, DAY_MS);
}

// -----------------------------------------------------------------------
// Fund release with async transfer callback
// -----------------------------------------------------------------------

#[test]
fn release_funds_success_confirms_payout() {
    let mut c = fresh_contract();
    let id = create_campaign_ok(&mut c, OWNER, 5 * NEAR, T0 + MONTH_MS);
    donate_ok(&mut c, ALICE, id, 8 * NEAR);
    donate_ok(&mut c, BOB, id, 2 * NEAR);
    let mid = create_milestone_ok(&mut c, id, 5 * NEAR, DAY_MS);

    set_env(ALICE, 0, T0 + 1000);
    c.vote_on_milestone(id, mid, true);
    let later = T0 + DAY_MS + 1;
    set_env(CAROL, 0, later);
    let _ = c.finalize_milestone(id, mid);

    // Snapshot escrow before release.
    set_env(OWNER, 0, later);
    let before = c.get_campaign_balance(id);
    assert!(before.available_balance.0 >= 5 * NEAR);

    // Owner releases the milestone funds.
    let _ = c.release_funds(id, mid);

    // Optimistic debit is already applied while the transfer is in flight...
    let during = c.get_campaign_balance(id);
    assert_eq!(during.available_balance.0, before.available_balance.0 - 5 * NEAR);
    assert_eq!(during.total_released.0, 5 * NEAR);
    assert_eq!(during.total_donated.0, before.total_donated.0);
    let m = c.get_milestone(id, mid);
    assert_eq!(m.state, "Approved");
    assert!(m.payout_pending);

    // ...the transfer succeeds and the callback confirms the payout.
    set_env_callback(later + 1000, PromiseResult::Successful(vec![]));
    c.on_milestone_release(id, mid, U128(5 * NEAR));

    let after = c.get_campaign_balance(id);
    assert_eq!(after.available_balance.0, before.available_balance.0 - 5 * NEAR);
    assert_eq!(after.total_released.0, 5 * NEAR);
    assert_eq!(after.total_donated.0, before.total_donated.0);
    assert_eq!(after.total_refunded.0, 0);
    let m = c.get_milestone(id, mid);
    assert_eq!(m.state, "Released");
    assert!(!m.payout_pending);
    assert!(m.released_at.is_some());

    let stats = c.get_contract_stats();
    assert_eq!(stats.total_released.0, 5 * NEAR);

    // Fully funded with every milestone paid out -> completed campaign.
    assert_eq!(c.get_campaign(id).status, "Completed");
}

#[test]
fn release_funds_failure_rolls_back_and_allows_retry() {
    let mut c = fresh_contract();
    let id = create_campaign_ok(&mut c, OWNER, 10 * NEAR, T0 + MONTH_MS);
    donate_ok(&mut c, ALICE, id, 8 * NEAR);
    let mid = create_milestone_ok(&mut c, id, 5 * NEAR, DAY_MS);

    set_env(ALICE, 0, T0 + 1000);
    c.vote_on_milestone(id, mid, true);
    let later = T0 + DAY_MS + 1;
    set_env(CAROL, 0, later);
    let _ = c.finalize_milestone(id, mid);

    set_env(OWNER, 0, later);
    let before = c.get_campaign_balance(id);
    let _ = c.release_funds(id, mid);

    // Transfer fails: the callback must roll everything back.
    set_env_callback(later + 1000, PromiseResult::Failed);
    c.on_milestone_release(id, mid, U128(5 * NEAR));

    let after = c.get_campaign_balance(id);
    assert_eq!(after.available_balance.0, before.available_balance.0);
    assert_eq!(after.total_released.0, 0);
    let m = c.get_milestone(id, mid);
    assert_eq!(m.state, "Approved"); // back to approvable state
    assert!(!m.payout_pending);
    assert_eq!(c.get_contract_stats().total_released.0, 0);

    // The owner can retry the release after a failed transfer.
    set_env(OWNER, 0, later + 2000);
    let _ = c.release_funds(id, mid);
    set_env_callback(later + 3000, PromiseResult::Successful(vec![]));
    c.on_milestone_release(id, mid, U128(5 * NEAR));
    assert_eq!(c.get_milestone(id, mid).state, "Released");
    assert_eq!(c.get_campaign_balance(id).total_released.0, 5 * NEAR);
}

#[test]
#[should_panic(expected = "Milestone must be approved by donors first")]
fn cannot_release_twice_after_successful_payout() {
    let mut c = fresh_contract();
    let id = create_campaign_ok(&mut c, OWNER, 10 * NEAR, T0 + MONTH_MS);
    donate_ok(&mut c, ALICE, id, 8 * NEAR);
    let mid = create_milestone_ok(&mut c, id, 5 * NEAR, DAY_MS);

    set_env(ALICE, 0, T0 + 1000);
    c.vote_on_milestone(id, mid, true);
    let later = T0 + DAY_MS + 1;
    set_env(CAROL, 0, later);
    let _ = c.finalize_milestone(id, mid);
    set_env(OWNER, 0, later);
    let _ = c.release_funds(id, mid);
    set_env_callback(later + 1000, PromiseResult::Successful(vec![]));
    c.on_milestone_release(id, mid, U128(5 * NEAR));

    // Milestone is Released now; a second release must be impossible.
    set_env(OWNER, 0, later + 3000);
    let _ = c.release_funds(id, mid);
}

#[test]
#[should_panic(expected = "Only the campaign owner can release funds")]
fn non_owner_cannot_release_funds() {
    let mut c = fresh_contract();
    let id = create_campaign_ok(&mut c, OWNER, 10 * NEAR, T0 + MONTH_MS);
    donate_ok(&mut c, ALICE, id, 8 * NEAR);
    let mid = create_milestone_ok(&mut c, id, 5 * NEAR, DAY_MS);
    set_env(ALICE, 0, T0 + DAY_MS + 1);
    let _ = c.release_funds(id, mid);
}

#[test]
#[should_panic(expected = "Milestone must be approved by donors first")]
fn cannot_release_unapproved_milestone() {
    let mut c = fresh_contract();
    let id = create_campaign_ok(&mut c, OWNER, 10 * NEAR, T0 + MONTH_MS);
    donate_ok(&mut c, ALICE, id, 8 * NEAR);
    let mid = create_milestone_ok(&mut c, id, 5 * NEAR, DAY_MS);
    // Before the campaign deadline, while the milestone is still voting.
    set_env(OWNER, 0, T0 + 1000);
    let _ = c.release_funds(id, mid);
}

#[test]
#[should_panic(expected = "Insufficient available funds for this milestone")]
fn release_insufficient_funds_fails() {
    let mut c = fresh_contract();
    let id = create_campaign_ok(&mut c, OWNER, 10 * NEAR, T0 + MONTH_MS);
    donate_ok(&mut c, ALICE, id, NEAR); // only ~1 NEAR available
    let mid = create_milestone_ok(&mut c, id, 5 * NEAR, DAY_MS);

    set_env(ALICE, 0, T0 + 1000);
    c.vote_on_milestone(id, mid, true);
    let later = T0 + DAY_MS + 1;
    set_env(OWNER, 0, later);
    let _ = c.finalize_milestone(id, mid);
    let _ = c.release_funds(id, mid);
}

// -----------------------------------------------------------------------
// Refunds
// -----------------------------------------------------------------------

#[test]
fn refund_full_amount_when_goal_not_met() {
    let mut c = fresh_contract();
    let id = create_campaign_ok(&mut c, OWNER, 10 * NEAR, T0 + DAY_MS);
    let credited = donate_ok(&mut c, ALICE, id, 2 * NEAR);

    // Not expired yet.
    set_env(ALICE, 0, T0 + 100);
    let before = c.get_campaign_balance(id);
    assert_eq!(before.available_balance.0, credited);

    // Expired without reaching the goal -> refundable.
    let later = T0 + DAY_MS + 1;
    set_env(ALICE, 0, later);
    let info = c.get_campaign(id);
    assert_eq!(info.status, "Refundable");
    assert!(info.refundable);
    assert!(!info.accepting_donations);

    let _ = c.request_refund(id);

    // The transfer succeeds and the callback confirms.
    set_env_callback(later + 1000, PromiseResult::Successful(vec![]));
    c.on_refund(id, accounts(ALICE), U128(credited));

    assert_eq!(c.get_donor_credit(id, accounts(ALICE)).0, 0);
    let balance = c.get_campaign_balance(id);
    assert_eq!(balance.available_balance.0, 0);
    assert_eq!(balance.total_refunded.0, credited);
    let mine = c.get_my_donations(accounts(ALICE), 0, 10);
    assert!(mine.iter().all(|d| d.refunded));
    assert_eq!(c.get_contract_stats().total_refunded.0, credited);
}

#[test]
fn refund_failure_rolls_back_credit() {
    let mut c = fresh_contract();
    let id = create_campaign_ok(&mut c, OWNER, 10 * NEAR, T0 + DAY_MS);
    let credited = donate_ok(&mut c, ALICE, id, 2 * NEAR);

    let later = T0 + DAY_MS + 1;
    set_env(ALICE, 0, later);
    let _ = c.request_refund(id);

    // Transfer failed: the credit must be restored.
    set_env_callback(later + 1000, PromiseResult::Failed);
    c.on_refund(id, accounts(ALICE), U128(credited));

    assert_eq!(c.get_donor_credit(id, accounts(ALICE)).0, credited);
    let balance = c.get_campaign_balance(id);
    assert_eq!(balance.available_balance.0, credited);
    assert_eq!(balance.total_refunded.0, 0);
}

#[test]
fn refund_is_pro_rata_after_partial_release() {
    let mut c = fresh_contract();
    // Target 10 NEAR, deadline in one day.
    let id = create_campaign_ok(&mut c, OWNER, 10 * NEAR, T0 + DAY_MS);
    // Alice donates 6 NEAR (minus a tiny storage fee).
    let alice_credit = donate_ok(&mut c, ALICE, id, 6 * NEAR);

    // Milestone of 4 NEAR, approved and released before expiry.
    let half_day = DAY_MS / 2;
    let mid = create_milestone_ok(&mut c, id, 4 * NEAR, half_day);
    set_env(ALICE, 0, T0 + 1000);
    c.vote_on_milestone(id, mid, true);
    let settled = T0 + half_day + 1;
    set_env(CAROL, 0, settled);
    let _ = c.finalize_milestone(id, mid);
    set_env(OWNER, 0, settled);
    let _ = c.release_funds(id, mid);
    set_env_callback(settled + 1000, PromiseResult::Successful(vec![]));
    c.on_milestone_release(id, mid, U128(4 * NEAR));

    // Campaign expired below target -> refund mode.
    let later = T0 + DAY_MS + 1;
    set_env(ALICE, 0, later);
    assert!(c.get_campaign(id).refundable);

    // Alice's pro-rata share: credit * available / total_donated.
    let balance = c.get_campaign_balance(id);
    assert_eq!(balance.total_donated.0, alice_credit);
    assert_eq!(balance.available_balance.0, alice_credit - 4 * NEAR);
    let expected = mul_div(alice_credit, balance.available_balance.0, balance.total_donated.0);
    assert_eq!(expected, alice_credit - 4 * NEAR);

    let _ = c.request_refund(id);
    set_env_callback(later + 2000, PromiseResult::Successful(vec![]));
    c.on_refund(id, accounts(ALICE), U128(expected));

    assert_eq!(c.get_donor_credit(id, accounts(ALICE)).0, 0);
    let final_balance = c.get_campaign_balance(id);
    assert_eq!(
        final_balance.available_balance.0,
        balance.available_balance.0 - expected
    );
    assert_eq!(final_balance.total_refunded.0, expected);
}

#[test]
#[should_panic(expected = "Campaign has not expired yet")]
fn refund_before_deadline_fails() {
    let mut c = fresh_contract();
    let id = create_campaign_ok(&mut c, OWNER, 10 * NEAR, T0 + DAY_MS);
    donate_ok(&mut c, ALICE, id, NEAR);
    set_env(ALICE, 0, T0 + 100);
    let _ = c.request_refund(id);
}

#[test]
#[should_panic(expected = "Goal was reached; refunds are disabled")]
fn refund_disabled_when_goal_reached() {
    let mut c = fresh_contract();
    let id = create_campaign_ok(&mut c, OWNER, NEAR, T0 + DAY_MS);
    donate_ok(&mut c, ALICE, id, 2 * NEAR);
    let later = T0 + DAY_MS + 1;
    set_env(ALICE, 0, later);
    let _ = c.request_refund(id);
}

#[test]
#[should_panic(expected = "No refundable donation for this campaign")]
fn refund_twice_fails() {
    let mut c = fresh_contract();
    let id = create_campaign_ok(&mut c, OWNER, 10 * NEAR, T0 + DAY_MS);
    let credited = donate_ok(&mut c, ALICE, id, NEAR);
    let later = T0 + DAY_MS + 1;
    set_env(ALICE, 0, later);
    let _ = c.request_refund(id);
    set_env_callback(later + 1000, PromiseResult::Successful(vec![]));
    c.on_refund(id, accounts(ALICE), U128(credited));
    set_env(ALICE, 0, later + 2000);
    let _ = c.request_refund(id);
}

#[test]
#[should_panic(expected = "Campaign is in refund mode; funds cannot be released")]
fn release_blocked_in_refund_mode() {
    let mut c = fresh_contract();
    let id = create_campaign_ok(&mut c, OWNER, 10 * NEAR, T0 + DAY_MS);
    donate_ok(&mut c, ALICE, id, NEAR);
    let mid = create_milestone_ok(&mut c, id, NEAR, DAY_MS);
    set_env(ALICE, 0, T0 + 1000);
    c.vote_on_milestone(id, mid, true);
    let later = T0 + DAY_MS + 1;
    set_env(CAROL, 0, later);
    let _ = c.finalize_milestone(id, mid); // rejected: campaign in refund mode
    set_env(OWNER, 0, later);
    let _ = c.release_funds(id, mid);
}

// -----------------------------------------------------------------------
// Views & status
// -----------------------------------------------------------------------

#[test]
fn campaign_status_transitions() {
    let mut c = fresh_contract();
    let id = create_campaign_ok(&mut c, OWNER, 2 * NEAR, T0 + DAY_MS);
    // Active
    set_env(OWNER, 0, T0 + 100);
    assert_eq!(c.get_campaign(id).status, "Active");
    // Funded
    donate_ok(&mut c, ALICE, id, 3 * NEAR);
    set_env(OWNER, 0, T0 + 200);
    assert_eq!(c.get_campaign(id).status, "Funded");
    assert_eq!(c.get_campaign(id).progress_percent, 100);
    // Refundable only when goal not met
    let id2 = create_campaign_ok(&mut c, OWNER, 9 * NEAR, T0 + DAY_MS);
    donate_ok(&mut c, BOB, id2, NEAR);
    set_env(OWNER, 0, T0 + DAY_MS + 1);
    assert_eq!(c.get_campaign(id2).status, "Refundable");
    assert_eq!(c.get_campaign(id).status, "Funded");
}

#[test]
fn get_all_campaigns_paginates() {
    let mut c = fresh_contract();
    for _ in 0..5 {
        create_campaign_ok(&mut c, OWNER, NEAR, T0 + DAY_MS);
    }
    set_env(OWNER, 0, T0);
    assert_eq!(c.get_all_campaigns(0, 3).len(), 3);
    assert_eq!(c.get_all_campaigns(3, 3).len(), 2);
    assert_eq!(c.get_all_campaigns(10, 3).len(), 0);
}

#[test]
fn milestone_finalization_blocked_in_refund_mode() {
    let mut c = fresh_contract();
    let id = create_campaign_ok(&mut c, OWNER, 10 * NEAR, T0 + DAY_MS);
    donate_ok(&mut c, ALICE, id, NEAR);
    let mid = create_milestone_ok(&mut c, id, NEAR, DAY_MS);
    set_env(ALICE, 0, T0 + 1000);
    c.vote_on_milestone(id, mid, true);
    let later = T0 + DAY_MS + 1;
    set_env(CAROL, 0, later);
    // Majority voted yes but the campaign expired unfunded -> rejected.
    let info = c.finalize_milestone(id, mid);
    assert_eq!(info.state, "Rejected");
}
