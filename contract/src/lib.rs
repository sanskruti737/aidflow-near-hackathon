//! # AidFlow - On-Chain Donation & Relief System
//!
//! A trustless crowdfunding contract for disaster relief and charitable
//! campaigns built on NEAR Protocol.
//!
//! ## Design
//!
//! - **Real escrow**: every donation is a real NEAR transfer into the contract
//!   account. Campaign balances, milestone states, votes and payouts live
//!   entirely on-chain.
//! - **Milestone-gated payouts**: campaign owners define milestones; donors
//!   vote on them; funds are only released to the beneficiary when donors
//!   approve.
//! - **Secure accounting**: donations are credited net of the storage fee they
//!   cause, so the sum of all campaign escrow balances can never exceed the
//!   freely spendable contract balance.
//! - **Asynchronous transfers**: every outgoing transfer (milestone payout,
//!   refund) is scheduled as a promise and settled through a callback that
//!   commits the accounting change on success and fully rolls it back on
//!   failure.
//! - **Pro-rata refunds**: if a campaign expires without reaching its goal,
//!   donors get their money back; if some funds were already paid out to
//!   approved milestones, refunds are computed pro-rata with exact 256-bit
//!   fixed point arithmetic.
//!
//! ## Data model
//!
//! All persisted collections use flat composite keys to avoid nested-prefix
//! hazards:
//!
//! - campaigns: `IterableMap<campaign_id, Campaign>`
//! - milestones: `LookupMap<(campaign_id, milestone_id), Milestone>`
//! - donation log per campaign: `LookupMap<(campaign_id, index), DonationRecord>`
//! - donation log per donor: `LookupMap<(donor, index), DonationRecord>`
//! - refundable donor credit: `LookupMap<(campaign_id, donor), Balance>`
//! - votes: `LookupMap<(campaign_id, milestone_id, voter), bool>`

use near_sdk::json_types::U128;
use near_sdk::store::{IterableMap, LookupMap};
use near_sdk::{
    env, ext_contract, near, require, AccountId, Gas, NearToken, PanicOnDefault, Promise,
    PromiseOrValue, PromiseResult,
};

pub mod types;

#[cfg(test)]
mod tests;

use types::*;

// ---------------------------------------------------------------------------
// Contract state
// ---------------------------------------------------------------------------

#[near(contract_state)]
#[derive(PanicOnDefault)]
pub struct AidFlow {
    /// All campaigns, iterable for the dashboard.
    pub campaigns: IterableMap<CampaignId, Campaign>,
    /// Milestones keyed by `(campaign_id, milestone_id)`.
    pub milestones: LookupMap<(CampaignId, MilestoneId), Milestone>,
    /// Donation log per campaign, addressed by a sequential index.
    pub donations_by_campaign: LookupMap<(CampaignId, u64), DonationRecord>,
    /// Donation log per donor, addressed by a sequential index.
    pub donations_by_donor: LookupMap<(AccountId, u64), DonationRecord>,
    /// How many donation records a donor has (per-donor log length).
    pub donor_donation_counts: LookupMap<AccountId, u64>,
    /// Refundable credit per `(campaign_id, donor)` in yoctoNEAR.
    pub donor_totals: LookupMap<(CampaignId, AccountId), Balance>,
    /// One vote per donor per milestone; value = voted in favor.
    pub votes: LookupMap<(CampaignId, MilestoneId, AccountId), bool>,
    pub next_campaign_id: CampaignId,
    pub stats: GlobalStats,
}

/// Self-callbacks used to settle asynchronous transfers.
#[ext_contract(aidflow_callbacks)]
pub trait AidFlowCallbacks {
    fn on_milestone_release(
        &mut self,
        campaign_id: CampaignId,
        milestone_id: MilestoneId,
        amount: U128,
    );
    fn on_refund(&mut self, campaign_id: CampaignId, donor: AccountId, amount: U128);
}

#[near]
impl AidFlow {
    #[init]
    pub fn new() -> Self {
        env::log_str("AidFlow contract initialized");
        Self {
            campaigns: IterableMap::new(StorageKey::Campaigns),
            milestones: LookupMap::new(StorageKey::Milestones),
            donations_by_campaign: LookupMap::new(StorageKey::DonationsByCampaign),
            donations_by_donor: LookupMap::new(StorageKey::DonationsByDonor),
            donor_donation_counts: LookupMap::new(StorageKey::DonorDonationCounts),
            donor_totals: LookupMap::new(StorageKey::DonorTotals),
            votes: LookupMap::new(StorageKey::Votes),
            next_campaign_id: 0,
            stats: GlobalStats::default(),
        }
    }

    // -----------------------------------------------------------------
    // Campaigns
    // -----------------------------------------------------------------

    /// Create a relief campaign. The caller becomes the beneficiary of all
    /// milestone payouts. Storage costs are covered by the attached deposit;
    /// any surplus is returned to the creator.
    #[payable]
    pub fn create_campaign(
        &mut self,
        title: String,
        description: String,
        image_url: String,
        category: String,
        target: U128,
        deadline_ms: u64,
    ) -> CampaignInfo {
        let owner = env::predecessor_account_id();
        let target = target.0;
        require!(!title.trim().is_empty(), "Title cannot be empty");
        require!(title.len() <= MAX_TITLE_LENGTH, "Title too long (max 120 chars)");
        require!(
            description.len() <= MAX_DESCRIPTION_LENGTH,
            "Description too long (max 2000 chars)"
        );
        require!(
            image_url.len() <= MAX_IMAGE_URL_LENGTH,
            "Image URL too long (max 400 chars)"
        );
        require!(!category.trim().is_empty(), "Category cannot be empty");
        require!(category.len() <= MAX_CATEGORY_LENGTH, "Category too long (max 40 chars)");
        require!(target > 0, "Target must be positive");

        let now = env::block_timestamp_ms();
        require!(
            deadline_ms > now.saturating_add(MIN_CAMPAIGN_DURATION_MS),
            "Deadline must be at least 1 minute in the future"
        );

        let campaign = Campaign {
            id: self.next_campaign_id,
            owner: owner.clone(),
            title: title.trim().to_string(),
            description,
            image_url,
            category: category.trim().to_string(),
            target,
            deadline: deadline_ms,
            created_at: now,
            total_donated: 0,
            available_balance: 0,
            total_released: 0,
            total_refunded: 0,
            donor_count: 0,
            milestone_count: 0,
            released_milestone_count: 0,
            donation_count: 0,
        };
        let id = campaign.id;
        let info = campaign.to_info(now);

        let storage_before = env::storage_usage();
        self.campaigns.insert(id, campaign);
        self.next_campaign_id += 1;
        self.stats.campaign_count += 1;
        // `store` collections persist on `flush`/`Drop`; flush now so the
        // storage growth is measured within this call.
        self.campaigns.flush();
        let storage_cost =
            Balance::from(env::storage_usage() - storage_before) * STORAGE_PRICE_PER_BYTE;

        // Refund any surplus deposit back to the creator.
        let attached = env::attached_deposit().as_yoctonear();
        require!(
            attached >= storage_cost,
            "Attached deposit too small to cover campaign storage; attach at least 0.2 NEAR"
        );
        if attached > storage_cost {
            Promise::new(owner)
                .transfer(NearToken::from_yoctonear(attached - storage_cost))
                .detach();
        }

        env::log_str(&format!("Campaign created: id={} target={}yoctoNEAR", id, target));
        info
    }

    /// Donate attached NEAR to a campaign.
    ///
    /// The credited amount is the attached deposit minus the exact storage
    /// fee the donation records introduce. This keeps the escrow invariant
    /// `sum(campaign.available_balance) <= free contract balance` exact at
    /// all times.
    #[payable]
    pub fn donate(&mut self, campaign_id: CampaignId, message: String) -> U128 {
        let donor = env::predecessor_account_id();
        let gross = env::attached_deposit().as_yoctonear();
        require!(gross > 0, "Attach some NEAR to donate");
        require!(message.len() <= MAX_MESSAGE_LENGTH, "Message too long (max 280 chars)");

        let now = env::block_timestamp_ms();
        require!(
            self.campaigns.contains_key(&campaign_id),
            "Campaign not found"
        );
        {
            let campaign = self.campaigns.get(&campaign_id).expect("campaign exists");
            require!(
                campaign.accepting_donations(now),
                "Campaign is no longer accepting donations"
            );
        }

        let was_donor = self
            .donor_totals
            .get(&(campaign_id, donor.clone()))
            .copied()
            .unwrap_or(0);
        let donor_index = self
            .donor_donation_counts
            .get(&donor)
            .copied()
            .unwrap_or(0);
        let campaign_index = {
            let campaign = self.campaigns.get(&campaign_id).expect("campaign exists");
            campaign.donation_count
        };

        // ---- Phase 1: write provisional records and measure storage ----
        let storage_before = env::storage_usage();

        let record = DonationRecord {
            campaign_id,
            donor: donor.clone(),
            amount: 0,
            gross_attached: gross,
            message,
            timestamp: now,
            refunded: false,
        };
        self.donations_by_campaign
            .insert((campaign_id, campaign_index), record.clone());
        self.donations_by_donor
            .insert((donor.clone(), donor_index), record);
        // Touch the credit entry so its first-write cost is measured too.
        self.donor_totals.insert((campaign_id, donor.clone()), was_donor);
        self.donor_donation_counts.insert(donor.clone(), donor_index);
        if was_donor == 0 {
            self.campaigns.get_mut(&campaign_id).expect("campaign exists").donor_count += 1;
        }

        // `store` collections buffer writes in memory and only persist them on
        // `Drop`/`flush`. Flush now so the storage growth of this donation is
        // visible to `env::storage_usage()` within this call.
        self.donations_by_campaign.flush();
        self.donations_by_donor.flush();
        self.donor_totals.flush();
        self.donor_donation_counts.flush();
        self.campaigns.flush();

        let storage_cost =
            Balance::from(env::storage_usage() - storage_before) * STORAGE_PRICE_PER_BYTE;
        require!(
            gross > storage_cost,
            "Donation too small to cover on-chain storage; attach at least 0.1 NEAR"
        );
        let credited = gross - storage_cost;

        // ---- Phase 2: finalize values (fixed-width fields, no extra storage) ----
        self.donations_by_campaign
            .get_mut(&(campaign_id, campaign_index))
            .expect("record exists")
            .amount = credited;
        self.donations_by_donor
            .get_mut(&(donor.clone(), donor_index))
            .expect("record exists")
            .amount = credited;
        self.donor_totals
            .insert((campaign_id, donor.clone()), was_donor + credited);
        self.donor_donation_counts
            .insert(donor.clone(), donor_index + 1);
        {
            let campaign = self.campaigns.get_mut(&campaign_id).expect("campaign exists");
            campaign.total_donated += credited;
            campaign.available_balance += credited;
            campaign.donation_count += 1;
        }
        self.stats.total_donated += credited;

        env::log_str(&format!(
            "Donation received: campaign={} donor={} amount={}yoctoNEAR (gross {})",
            campaign_id, donor, credited, gross
        ));
        U128(credited)
    }

    // -----------------------------------------------------------------
    // Milestones
    // -----------------------------------------------------------------

    /// Create a milestone for a campaign (owner only). Donors vote on it
    /// until the voting window ends.
    pub fn create_milestone(
        &mut self,
        campaign_id: CampaignId,
        title: String,
        description: String,
        amount: U128,
        voting_duration_ms: u64,
    ) -> MilestoneInfo {
        let caller = env::predecessor_account_id();
        require!(!title.trim().is_empty(), "Milestone title cannot be empty");
        require!(
            title.len() <= MAX_TITLE_LENGTH,
            "Milestone title too long (max 120 chars)"
        );
        require!(
            description.len() <= MAX_DESCRIPTION_LENGTH,
            "Milestone description too long (max 2000 chars)"
        );
        require!(
            voting_duration_ms >= MIN_VOTING_DURATION_MS,
            "Voting duration must be at least 30 seconds"
        );
        require!(
            voting_duration_ms <= MAX_VOTING_DURATION_MS,
            "Voting duration must be at most 30 days"
        );

        let now = env::block_timestamp_ms();
        let campaign = self
            .campaigns
            .get(&campaign_id)
            .unwrap_or_else(|| env::panic_str("Campaign not found"));
        require!(
            caller == campaign.owner,
            "Only the campaign owner can add milestones"
        );
        require!(
            now <= campaign.deadline || campaign.total_donated >= campaign.target,
            "Campaign expired without funding"
        );

        let amount = amount.0;
        require!(amount > 0, "Milestone amount must be positive");

        // Sum of milestone amounts that can still pay out must stay within
        // the campaign target (prevents over-allocating donations).
        let mut allocated: Balance = 0;
        for i in 0..campaign.milestone_count {
            if let Some(m) = self.milestones.get(&(campaign_id, i)) {
                if m.state != MilestoneState::Rejected {
                    allocated += m.amount;
                }
            }
        }
        require!(
            allocated + amount <= campaign.target,
            "Milestone amounts exceed the campaign target"
        );

        let milestone = Milestone {
            id: campaign.milestone_count,
            campaign_id,
            title: title.trim().to_string(),
            description,
            amount,
            state: MilestoneState::VotingOpen,
            votes_for: 0,
            votes_against: 0,
            voting_end: now + voting_duration_ms,
            created_at: now,
            released_at: None,
            payout_pending: false,
        };
        let info = milestone.to_info(now);
        let id = milestone.id;
        self.milestones.insert((campaign_id, id), milestone);
        self.campaigns
            .get_mut(&campaign_id)
            .expect("campaign exists")
            .milestone_count += 1;

        env::log_str(&format!(
            "Milestone created: campaign={} milestone={} amount={}yoctoNEAR voting_until={}",
            campaign_id, id, amount, info.voting_end
        ));
        info
    }

    /// Cast a vote on a milestone. Only donors of that campaign may vote and
    /// each donor votes exactly once per milestone.
    pub fn vote_on_milestone(
        &mut self,
        campaign_id: CampaignId,
        milestone_id: MilestoneId,
        support: bool,
    ) {
        let voter = env::predecessor_account_id();
        let now = env::block_timestamp_ms();

        let credit = self
            .donor_totals
            .get(&(campaign_id, voter.clone()))
            .copied()
            .unwrap_or(0);
        require!(credit > 0, "Only donors of this campaign can vote");

        let milestone = self
            .milestones
            .get_mut(&(campaign_id, milestone_id))
            .unwrap_or_else(|| env::panic_str("Milestone not found"));
        require!(milestone.state == MilestoneState::VotingOpen, "Milestone voting is closed");
        require!(now < milestone.voting_end, "Voting window has ended");

        require!(
            self.votes
                .insert((campaign_id, milestone_id, voter.clone()), support)
                .is_none(),
            "You already voted on this milestone"
        );

        if support {
            milestone.votes_for += 1;
        } else {
            milestone.votes_against += 1;
        }
        env::log_str(&format!(
            "Milestone vote: campaign={} milestone={} voter={} support={}",
            campaign_id, milestone_id, voter, support
        ));
    }

    /// Close voting and evaluate the result. Permissionless: anyone may
    /// finalize once the voting window has passed. A milestone is approved
    /// when it received strictly more supporting votes than opposing ones.
    pub fn finalize_milestone(
        &mut self,
        campaign_id: CampaignId,
        milestone_id: MilestoneId,
    ) -> MilestoneInfo {
        let now = env::block_timestamp_ms();
        let campaign = self
            .campaigns
            .get(&campaign_id)
            .unwrap_or_else(|| env::panic_str("Campaign not found"));
        let milestone = self
            .milestones
            .get_mut(&(campaign_id, milestone_id))
            .unwrap_or_else(|| env::panic_str("Milestone not found"));
        require!(
            milestone.state == MilestoneState::VotingOpen,
            "Milestone is not open for finalization"
        );
        require!(now >= milestone.voting_end, "Voting window is still open");

        let approved = milestone.votes_for > milestone.votes_against
            && milestone.votes_for > 0
            && !campaign.in_refund_mode(now);
        milestone.state = if approved {
            MilestoneState::Approved
        } else {
            MilestoneState::Rejected
        };
        let state = milestone.state.as_str();
        let info = milestone.to_info(now);
        env::log_str(&format!(
            "Milestone finalized: campaign={} milestone={} state={} votes_for={} votes_against={}",
            campaign_id, milestone_id, state, info.votes_for, info.votes_against
        ));
        info
    }

    /// Release the funds of an approved milestone to the campaign
    /// beneficiary (owner only). The accounting change is settled by the
    /// transfer callback; on failure it is fully rolled back.
    pub fn release_funds(
        &mut self,
        campaign_id: CampaignId,
        milestone_id: MilestoneId,
    ) -> PromiseOrValue<U128> {
        let caller = env::predecessor_account_id();
        let now = env::block_timestamp_ms();
        let campaign = self
            .campaigns
            .get(&campaign_id)
            .unwrap_or_else(|| env::panic_str("Campaign not found"));
        require!(
            caller == campaign.owner,
            "Only the campaign owner can release funds"
        );
        require!(!campaign.in_refund_mode(now), "Campaign is in refund mode; funds cannot be released");

        let milestone = self
            .milestones
            .get(&(campaign_id, milestone_id))
            .unwrap_or_else(|| env::panic_str("Milestone not found"));
        require!(
            milestone.state == MilestoneState::Approved,
            "Milestone must be approved by donors first"
        );
        require!(!milestone.payout_pending, "Payout already in flight");
        let amount = milestone.amount;
        require!(
            campaign.available_balance >= amount,
            "Insufficient available funds for this milestone"
        );
        let beneficiary = campaign.owner.clone();

        // Debit the escrow optimistically; the callback settles or rolls back.
        {
            let campaign = self.campaigns.get_mut(&campaign_id).expect("campaign exists");
            campaign.available_balance -= amount;
            campaign.total_released += amount;
            campaign.released_milestone_count += 1;
        }
        self.stats.total_released += amount;
        self.milestones
            .get_mut(&(campaign_id, milestone_id))
            .expect("milestone exists")
            .payout_pending = true;

        let payout = Promise::new(beneficiary).transfer(NearToken::from_yoctonear(amount));
        let callback = aidflow_callbacks::ext(env::current_account_id())
            .with_static_gas(Gas::from_tgas(GAS_FOR_CALLBACK))
            .on_milestone_release(campaign_id, milestone_id, U128(amount));
        env::log_str(&format!(
            "Milestone payout scheduled: campaign={} milestone={} amount={}yoctoNEAR",
            campaign_id, milestone_id, amount
        ));
        PromiseOrValue::Promise(payout.then(callback))
    }

    /// Request a refund for the caller's donations to a campaign that
    /// expired without reaching its goal. Refunds are pro-rata if milestone
    /// funds were already released.
    pub fn request_refund(&mut self, campaign_id: CampaignId) -> PromiseOrValue<U128> {
        let donor = env::predecessor_account_id();
        let now = env::block_timestamp_ms();
        let campaign = self
            .campaigns
            .get(&campaign_id)
            .unwrap_or_else(|| env::panic_str("Campaign not found"));
        require!(now > campaign.deadline, "Campaign has not expired yet");
        require!(
            campaign.total_donated < campaign.target,
            "Goal was reached; refunds are disabled"
        );

        let key = (campaign_id, donor.clone());
        let credit = self.donor_totals.get(&key).copied().unwrap_or(0);
        require!(credit > 0, "No refundable donation for this campaign");

        // Pro-rata share of what is still held in escrow.
        let amount = mul_div(credit, campaign.available_balance, campaign.total_donated);
        require!(amount > 0, "Refundable amount rounds to zero");

        // Debit optimistically; the callback settles or rolls back.
        self.donor_totals.insert(key, 0u128);
        {
            let campaign = self.campaigns.get_mut(&campaign_id).expect("campaign exists");
            campaign.available_balance -= amount;
            campaign.total_refunded += amount;
        }
        self.stats.total_refunded += amount;

        let refund = Promise::new(donor.clone()).transfer(NearToken::from_yoctonear(amount));
        let callback = aidflow_callbacks::ext(env::current_account_id())
            .with_static_gas(Gas::from_tgas(GAS_FOR_CALLBACK))
            .on_refund(campaign_id, donor, U128(amount));
        env::log_str(&format!(
            "Refund scheduled: campaign={} amount={}yoctoNEAR",
            campaign_id, amount
        ));
        PromiseOrValue::Promise(refund.then(callback))
    }

    // -----------------------------------------------------------------
    // Views
    // -----------------------------------------------------------------

    pub fn get_campaign(&self, campaign_id: CampaignId) -> CampaignInfo {
        let now = env::block_timestamp_ms();
        self.campaigns
            .get(&campaign_id)
            .unwrap_or_else(|| env::panic_str("Campaign not found"))
            .to_info(now)
    }

    pub fn get_all_campaigns(&self, from_index: u64, limit: u64) -> Vec<CampaignInfo> {
        let now = env::block_timestamp_ms();
        self.campaigns
            .iter()
            .skip(from_index as usize)
            .take(limit as usize)
            .map(|(_, c)| c.to_info(now))
            .collect()
    }

    pub fn get_campaign_count(&self) -> u64 {
        self.stats.campaign_count
    }

    pub fn get_campaign_milestones(&self, campaign_id: CampaignId) -> Vec<MilestoneInfo> {
        let now = env::block_timestamp_ms();
        let count = self
            .campaigns
            .get(&campaign_id)
            .map(|c| c.milestone_count)
            .unwrap_or(0);
        (0..count)
            .filter_map(|i| self.milestones.get(&(campaign_id, i)))
            .map(|m| m.to_info(now))
            .collect()
    }

    pub fn get_milestone(
        &self,
        campaign_id: CampaignId,
        milestone_id: MilestoneId,
    ) -> MilestoneInfo {
        let now = env::block_timestamp_ms();
        self.milestones
            .get(&(campaign_id, milestone_id))
            .unwrap_or_else(|| env::panic_str("Milestone not found"))
            .to_info(now)
    }

    pub fn get_donation_history(
        &self,
        campaign_id: CampaignId,
        from_index: u64,
        limit: u64,
    ) -> Vec<DonationInfo> {
        let count = self
            .campaigns
            .get(&campaign_id)
            .map(|c| c.donation_count)
            .unwrap_or(0);
        (from_index..count)
            .take(limit as usize)
            .filter_map(|i| self.donations_by_campaign.get(&(campaign_id, i)))
            .map(|r| r.to_info())
            .collect()
    }

    pub fn get_my_donations(
        &self,
        account_id: AccountId,
        from_index: u64,
        limit: u64,
    ) -> Vec<DonationInfo> {
        let count = self
            .donor_donation_counts
            .get(&account_id)
            .copied()
            .unwrap_or(0);
        (from_index..count)
            .take(limit as usize)
            .filter_map(|i| self.donations_by_donor.get(&(account_id.clone(), i)))
            .map(|r| r.to_info())
            .collect()
    }

    pub fn get_campaign_balance(&self, campaign_id: CampaignId) -> CampaignBalanceInfo {
        let c = self
            .campaigns
            .get(&campaign_id)
            .unwrap_or_else(|| env::panic_str("Campaign not found"));
        CampaignBalanceInfo {
            campaign_id,
            target: U128(c.target),
            total_donated: U128(c.total_donated),
            available_balance: U128(c.available_balance),
            total_released: U128(c.total_released),
            total_refunded: U128(c.total_refunded),
            donor_count: c.donor_count,
            contract_balance: U128(env::account_balance().as_yoctonear()),
        }
    }

    /// Refundable credit of `account_id` for a campaign (yoctoNEAR).
    pub fn get_donor_credit(&self, campaign_id: CampaignId, account_id: AccountId) -> U128 {
        U128(
            self.donor_totals
                .get(&(campaign_id, account_id))
                .copied()
                .unwrap_or(0),
        )
    }

    pub fn has_voted(
        &self,
        campaign_id: CampaignId,
        milestone_id: MilestoneId,
        account_id: AccountId,
    ) -> bool {
        self.votes
            .get(&(campaign_id, milestone_id, account_id))
            .copied()
            .unwrap_or(false)
    }

    pub fn get_contract_stats(&self) -> ContractStats {
        ContractStats {
            campaign_count: self.stats.campaign_count,
            total_donated: U128(self.stats.total_donated),
            total_released: U128(self.stats.total_released),
            total_refunded: U128(self.stats.total_refunded),
            contract_balance: U128(env::account_balance().as_yoctonear()),
        }
    }

    // -----------------------------------------------------------------
    // Transfer settlement callbacks (private)
    // -----------------------------------------------------------------

    /// Settles a milestone payout. On success the payout is confirmed; on
    /// failure the escrow debit is fully rolled back.
    #[allow(deprecated)]
    #[private]
    pub fn on_milestone_release(
        &mut self,
        campaign_id: CampaignId,
        milestone_id: MilestoneId,
        amount: U128,
    ) {
        let amount = amount.0;
        let success = matches!(env::promise_result(0), PromiseResult::Successful(_));
        if success {
            if let Some(m) = self.milestones.get_mut(&(campaign_id, milestone_id)) {
                m.payout_pending = false;
                // Marks the milestone as fully paid out so it can never be
                // released again.
                m.state = MilestoneState::Released;
                m.released_at = Some(env::block_timestamp_ms());
            }
            env::log_str(&format!(
                "Milestone payout confirmed: campaign={} milestone={} amount={}yoctoNEAR",
                campaign_id, milestone_id, amount
            ));
        } else {
            // Full rollback of the optimistic debit.
            if let Some(c) = self.campaigns.get_mut(&campaign_id) {
                c.available_balance += amount;
                c.total_released -= amount;
                c.released_milestone_count -= 1;
            }
            self.stats.total_released -= amount;
            if let Some(m) = self.milestones.get_mut(&(campaign_id, milestone_id)) {
                m.payout_pending = false;
                m.state = MilestoneState::Approved;
            }
            env::log_str(&format!(
                "Milestone payout FAILED and was rolled back: campaign={} milestone={}",
                campaign_id, milestone_id
            ));
        }
    }

    /// Settles a donor refund. On success the donor's donation records are
    /// marked refunded; on failure the credit is restored.
    #[allow(deprecated)]
    #[private]
    pub fn on_refund(&mut self, campaign_id: CampaignId, donor: AccountId, amount: U128) {
        let amount = amount.0;
        let success = matches!(env::promise_result(0), PromiseResult::Successful(_));
        if success {
            let count = self
                .donor_donation_counts
                .get(&donor)
                .copied()
                .unwrap_or(0);
            for i in 0..count {
                let matches_campaign = self
                    .donations_by_donor
                    .get(&(donor.clone(), i))
                    .map(|r| r.campaign_id == campaign_id)
                    .unwrap_or(false);
                if matches_campaign {
                    if let Some(r) = self.donations_by_donor.get_mut(&(donor.clone(), i)) {
                        r.refunded = true;
                    }
                }
            }
            env::log_str(&format!(
                "Refund confirmed: campaign={} donor={} amount={}yoctoNEAR",
                campaign_id, donor, amount
            ));
        } else {
            // Full rollback of the optimistic debit.
            let key = (campaign_id, donor.clone());
            let credit = self.donor_totals.get(&key).copied().unwrap_or(0);
            self.donor_totals.insert(key, credit + amount);
            if let Some(c) = self.campaigns.get_mut(&campaign_id) {
                c.available_balance += amount;
                c.total_refunded -= amount;
            }
            self.stats.total_refunded -= amount;
            env::log_str(&format!(
                "Refund FAILED and was rolled back: campaign={} donor={}",
                campaign_id, donor
            ));
        }
    }
}
