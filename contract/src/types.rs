//! Shared types for the AidFlow smart contract.
//!
//! Internal (persisted) types use plain `u128` balances in yoctoNEAR and are
//! serialized with Borsh. Public view types wrap balances in `U128` so that
//! JSON consumers never lose precision on large values.

use near_sdk::borsh::{self, BorshDeserialize, BorshSerialize};
use near_sdk::json_types::U128;
use near_sdk::serde::{Deserialize, Serialize};
use near_sdk::{AccountId, BorshStorageKey};

/// Campaign identifier (sequential).
pub type CampaignId = u64;
/// Milestone identifier, unique within a campaign (sequential per campaign).
pub type MilestoneId = u64;
/// Balance in yoctoNEAR (1 NEAR = 10^24 yoctoNEAR).
pub type Balance = u128;

/// Storage price from NEAR genesis config: 1 byte = 10^19 yoctoNEAR.
pub const STORAGE_PRICE_PER_BYTE: Balance = 10_000_000_000_000_000_000;

/// Upper bounds to keep every refund computation inside the safe mul-div domain.
pub const MUL_DIV_MAX: Balance = 1 << 120;

pub const MAX_TITLE_LENGTH: usize = 120;
pub const MAX_DESCRIPTION_LENGTH: usize = 2000;
pub const MAX_IMAGE_URL_LENGTH: usize = 400;
pub const MAX_CATEGORY_LENGTH: usize = 40;
pub const MAX_MESSAGE_LENGTH: usize = 280;

/// A campaign must run for at least one more minute when created.
pub const MIN_CAMPAIGN_DURATION_MS: u64 = 60_000;
/// Milestone voting windows are clamped between 30 seconds and 30 days.
pub const MIN_VOTING_DURATION_MS: u64 = 30_000;
pub const MAX_VOTING_DURATION_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// Gas reserved for the promise callback after an outgoing transfer.
pub const GAS_FOR_CALLBACK: u64 = 10;

/// Unique storage prefixes for every persisted collection.
#[derive(BorshStorageKey, BorshSerialize, BorshDeserialize)]
pub enum StorageKey {
    Campaigns,
    Milestones,
    DonationsByCampaign,
    DonationsByDonor,
    DonorDonationCounts,
    DonorTotals,
    Votes,
}

/// Milestone lifecycle, fully controlled by contract logic.
#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
pub enum MilestoneState {
    /// Donors are casting votes.
    VotingOpen,
    /// Donors approved the milestone; funds can be released by the owner.
    Approved,
    /// Donors rejected the milestone.
    Rejected,
    /// Funds for this milestone were paid out to the campaign beneficiary.
    Released,
}

impl MilestoneState {
    pub fn as_str(&self) -> &'static str {
        match self {
            MilestoneState::VotingOpen => "VotingOpen",
            MilestoneState::Approved => "Approved",
            MilestoneState::Rejected => "Rejected",
            MilestoneState::Released => "Released",
        }
    }
}

/// A relief campaign. All amounts are in yoctoNEAR.
#[derive(BorshSerialize, BorshDeserialize, Clone, Debug)]
pub struct Campaign {
    pub id: CampaignId,
    /// Beneficiary of released milestone funds (the creator).
    pub owner: AccountId,
    pub title: String,
    pub description: String,
    pub image_url: String,
    pub category: String,
    /// Funding goal.
    pub target: Balance,
    /// Deadline (milliseconds since Unix epoch).
    pub deadline: u64,
    pub created_at: u64,
    /// Lifetime sum of credited donations (gross attached minus storage fees).
    pub total_donated: Balance,
    /// Funds currently held in escrow for this campaign.
    pub available_balance: Balance,
    /// Sum of milestone payouts performed (confirmed by transfer callbacks).
    pub total_released: Balance,
    /// Sum of refunds returned to donors (confirmed by transfer callbacks).
    pub total_refunded: Balance,
    pub donor_count: u32,
    pub milestone_count: u64,
    pub released_milestone_count: u64,
    /// Number of donation records logged for this campaign.
    pub donation_count: u64,
}

impl Campaign {
    /// Whether the campaign is still accepting donations.
    pub fn accepting_donations(&self, now_ms: u64) -> bool {
        now_ms <= self.deadline
    }

    /// A campaign enters refund mode when it expired without reaching its goal.
    pub fn in_refund_mode(&self, now_ms: u64) -> bool {
        now_ms > self.deadline && self.total_donated < self.target
    }

    /// Human readable status derived purely from on-chain state.
    pub fn status(&self, now_ms: u64) -> String {
        if self.in_refund_mode(now_ms) {
            "Refundable".to_string()
        } else if self.total_donated >= self.target {
            if self.milestone_count > 0 && self.released_milestone_count == self.milestone_count {
                "Completed".to_string()
            } else {
                "Funded".to_string()
            }
        } else if now_ms > self.deadline {
            "Expired".to_string()
        } else {
            "Active".to_string()
        }
    }
}

/// A milestone with donor-gated voting and contract-enforced payout.
#[derive(BorshSerialize, BorshDeserialize, Clone, Debug)]
pub struct Milestone {
    pub id: MilestoneId,
    pub campaign_id: CampaignId,
    pub title: String,
    pub description: String,
    /// Amount (yoctoNEAR) payable to the campaign owner when approved.
    pub amount: Balance,
    pub state: MilestoneState,
    pub votes_for: u32,
    pub votes_against: u32,
    /// Voting window end (milliseconds since Unix epoch).
    pub voting_end: u64,
    pub created_at: u64,
    /// Set once the payout transfer succeeded.
    pub released_at: Option<u64>,
    /// True while a payout transfer is in flight (guards double release).
    pub payout_pending: bool,
}

/// An immutable donation entry. `amount` is the effective credited value
/// (what the donor attached minus the storage fee the donation had to cover).
#[derive(BorshSerialize, BorshDeserialize, Clone, Debug)]
pub struct DonationRecord {
    pub campaign_id: CampaignId,
    pub donor: AccountId,
    pub amount: Balance,
    /// Total yoctoNEAR the donor attached with the call.
    pub gross_attached: Balance,
    pub message: String,
    /// Block timestamp in milliseconds.
    pub timestamp: u64,
    /// Set to true once a refund covering this donation completed.
    pub refunded: bool,
}

/// Global counters, updated on every financial mutation.
#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, Default)]
pub struct GlobalStats {
    pub campaign_count: u64,
    pub total_donated: Balance,
    pub total_released: Balance,
    pub total_refunded: Balance,
}

// ---------------------------------------------------------------------------
// JSON view types
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(crate = "near_sdk::serde")]
pub struct CampaignInfo {
    pub id: CampaignId,
    pub owner: AccountId,
    pub title: String,
    pub description: String,
    pub image_url: String,
    pub category: String,
    pub target: U128,
    pub deadline: u64,
    pub created_at: u64,
    pub total_donated: U128,
    pub available_balance: U128,
    pub total_released: U128,
    pub total_refunded: U128,
    pub donor_count: u32,
    pub milestone_count: u64,
    pub released_milestone_count: u64,
    pub donation_count: u64,
    pub status: String,
    /// Integer percentage of the target reached (0..100).
    pub progress_percent: u16,
    /// True when the campaign expired without funding and donors may refund.
    pub refundable: bool,
    /// True while donations are accepted.
    pub accepting_donations: bool,
}

impl Campaign {
    pub fn to_info(&self, now_ms: u64) -> CampaignInfo {
        let progress = if self.target == 0 {
            0
        } else {
            let p = self.total_donated * 100 / self.target;
            if p > 100 {
                100
            } else {
                p as u16
            }
        };
        CampaignInfo {
            id: self.id,
            owner: self.owner.clone(),
            title: self.title.clone(),
            description: self.description.clone(),
            image_url: self.image_url.clone(),
            category: self.category.clone(),
            target: U128(self.target),
            deadline: self.deadline,
            created_at: self.created_at,
            total_donated: U128(self.total_donated),
            available_balance: U128(self.available_balance),
            total_released: U128(self.total_released),
            total_refunded: U128(self.total_refunded),
            donor_count: self.donor_count,
            milestone_count: self.milestone_count,
            released_milestone_count: self.released_milestone_count,
            donation_count: self.donation_count,
            status: self.status(now_ms),
            progress_percent: progress,
            refundable: self.in_refund_mode(now_ms),
            accepting_donations: self.accepting_donations(now_ms),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(crate = "near_sdk::serde")]
pub struct MilestoneInfo {
    pub id: MilestoneId,
    pub campaign_id: CampaignId,
    pub title: String,
    pub description: String,
    pub amount: U128,
    pub state: String,
    pub votes_for: u32,
    pub votes_against: u32,
    pub voting_end: u64,
    pub created_at: u64,
    pub released_at: Option<u64>,
    pub payout_pending: bool,
    /// Voting still open right now.
    pub voting_open: bool,
}

impl Milestone {
    pub fn to_info(&self, now_ms: u64) -> MilestoneInfo {
        MilestoneInfo {
            id: self.id,
            campaign_id: self.campaign_id,
            title: self.title.clone(),
            description: self.description.clone(),
            amount: U128(self.amount),
            state: self.state.as_str().to_string(),
            votes_for: self.votes_for,
            votes_against: self.votes_against,
            voting_end: self.voting_end,
            created_at: self.created_at,
            released_at: self.released_at,
            payout_pending: self.payout_pending,
            voting_open: self.state == MilestoneState::VotingOpen && now_ms < self.voting_end,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(crate = "near_sdk::serde")]
pub struct DonationInfo {
    pub campaign_id: CampaignId,
    pub donor: AccountId,
    pub amount: U128,
    pub gross_attached: U128,
    pub message: String,
    pub timestamp: u64,
    pub refunded: bool,
}

impl DonationRecord {
    pub fn to_info(&self) -> DonationInfo {
        DonationInfo {
            campaign_id: self.campaign_id,
            donor: self.donor.clone(),
            amount: U128(self.amount),
            gross_attached: U128(self.gross_attached),
            message: self.message.clone(),
            timestamp: self.timestamp,
            refunded: self.refunded,
        }
    }
}

/// Full escrow accounting for a single campaign.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(crate = "near_sdk::serde")]
pub struct CampaignBalanceInfo {
    pub campaign_id: CampaignId,
    pub target: U128,
    pub total_donated: U128,
    pub available_balance: U128,
    pub total_released: U128,
    pub total_refunded: U128,
    pub donor_count: u32,
    /// Entire balance of the contract account (transparency).
    pub contract_balance: U128,
}

/// Platform-level statistics.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(crate = "near_sdk::serde")]
pub struct ContractStats {
    pub campaign_count: u64,
    pub total_donated: U128,
    pub total_released: U128,
    pub total_refunded: U128,
    pub contract_balance: U128,
}

// ---------------------------------------------------------------------------
// Exact fixed-point helper: (a * b) / denominator
// ---------------------------------------------------------------------------
//
/// Computes `(a * b) / denominator` exactly using 256-bit intermediate
/// arithmetic, for values bounded by `MUL_DIV_MAX` (see `mul_div` guard).
/// Rounding is towards zero (floor), the remainder is discarded.
///
/// Used for pro-rata refunds so that the sum of all refunds can never
/// exceed the campaign's escrow balance.
pub fn mul_div(a: Balance, b: Balance, denominator: Balance) -> Balance {
    require_safe_mul_div(a, b, denominator);
    // 256-bit product of a and b, as four little-endian 64-bit limbs.
    let mut prod = [0u64; 4];
    mul_256_into(a, b, &mut prod);

    // Binary long division: process the 256 bits from most significant down.
    let mut remainder: u128 = 0;
    let mut quotient = [0u64; 4];
    for bit in (0..256).rev() {
        let limb = prod[bit / 64];
        let bit_val = (limb >> (bit % 64)) & 1;
        // remainder < denominator < 2^120, so this shift never overflows.
        remainder = (remainder << 1) | (bit_val as u128);
        if remainder >= denominator {
            remainder -= denominator;
            quotient[bit / 64] |= 1u64 << (bit % 64);
        }
    }
    debug_assert!(quotient[2] == 0 && quotient[3] == 0);
    (quotient[1] as u128) << 64 | quotient[0] as u128
}

fn mul_256_into(a: Balance, b: Balance, out: &mut [u64; 4]) {
    let a0 = a as u64;
    let a1 = (a >> 64) as u64;
    let b0 = b as u64;
    let b1 = (b >> 64) as u64;

    // a0 * b0
    let t = (a0 as u128) * (b0 as u128);
    out[0] = t as u64;
    out[1] = (t >> 64) as u64;

    // (a0 * b1 + a1 * b0) << 64
    let t1 = (a0 as u128) * (b1 as u128);
    let t2 = (a1 as u128) * (b0 as u128);
    add_shifted(out, 1, t1);
    add_shifted(out, 1, t2);

    // a1 * b1 << 128
    let t3 = (a1 as u128) * (b1 as u128);
    add_shifted(out, 2, t3);
}

/// Adds `value << (limb_index * 64)` into the 4-limb accumulator.
fn add_shifted(acc: &mut [u64; 4], limb_index: usize, value: u128) {
    let lo = value as u64;
    let hi = (value >> 64) as u64;

    let (sum, mut carry) = acc[limb_index].overflowing_add(lo);
    acc[limb_index] = sum;

    let mut i = limb_index + 1;
    let (sum, c) = acc[i].overflowing_add(hi + carry as u64);
    acc[i] = sum;
    carry = c || carry && hi == 0;
    // Propagate any residual carry upward.
    while carry && i + 1 < 4 {
        i += 1;
        let (sum, c) = acc[i].overflowing_add(1);
        acc[i] = sum;
        carry = c;
    }
}

fn require_safe_mul_div(a: Balance, b: Balance, denominator: Balance) {
    assert!(
        denominator > 0,
        "mul_div: denominator must be positive"
    );
    assert!(
        denominator < MUL_DIV_MAX,
        "mul_div: denominator exceeds safe domain"
    );
    assert!(a < MUL_DIV_MAX, "mul_div: operand exceeds safe domain");
    assert!(b < MUL_DIV_MAX, "mul_div: operand exceeds safe domain");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn big(a: u128) -> Balance {
        a * 10u128.pow(24)
    }

    #[test]
    fn mul_div_basics() {
        assert_eq!(mul_div(10, 10, 4), 25);
        assert_eq!(mul_div(100, 0, 7), 0);
        assert_eq!(mul_div(1, 1, 1), 1);
        assert_eq!(mul_div(7, 3, 2), 10); // 21 / 2 = 10.5 -> 10
    }

    #[test]
    fn mul_div_exact_large_values() {
        let total = big(1000);
        let credit = big(333);
        let available = big(777);
        // 333 * 777 / 1000 NEAR at full yoctoNEAR precision -> 258.741 NEAR
        assert_eq!(
            mul_div(credit, available, total),
            333u128 * 777 * 10u128.pow(24) / 1000
        );
    }

    #[test]
    fn mul_div_pro_rata_never_exceeds_available() {
        let total = big(10);
        let available = big(4);
        // Two donors with credits 6 and 4 NEAR.
        let r1 = mul_div(big(6), available, total); // 2.4 NEAR exactly
        let r2 = mul_div(big(4), available, total); // 1.6 NEAR exactly
        // Sum of refunds can never exceed the available escrow.
        assert!(r1 + r2 <= available);
        assert_eq!(r1 + r2, available);
        assert_eq!(r1, big(24) / 10);
        assert_eq!(r2, big(16) / 10);
    }

    #[test]
    fn mul_div_full_credit_refund() {
        // When nothing was released, available == total -> donor gets full credit.
        let total = big(50);
        assert_eq!(mul_div(big(50), total, total), big(50));
        assert_eq!(mul_div(big(17), big(50), total), big(17));
    }

    #[test]
    fn mul_div_max_domain() {
        let a = MUL_DIV_MAX - 1;
        let b = MUL_DIV_MAX - 1;
        let d = MUL_DIV_MAX - 1;
        // (2^120-1)^2 / (2^120-1) = 2^120-1
        assert_eq!(mul_div(a, b, d), a);
    }

    #[test]
    #[should_panic(expected = "denominator must be positive")]
    fn mul_div_zero_denominator_panics() {
        mul_div(1, 1, 0);
    }

    #[test]
    #[should_panic(expected = "safe domain")]
    fn mul_div_domain_guard() {
        mul_div(1 << 125, 2, 3);
    }
}
