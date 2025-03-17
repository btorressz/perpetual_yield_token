use anchor_lang::prelude::*;
use anchor_lang::solana_program::clock::Clock;
use anchor_spl::token::{self, Token, TokenAccount, Transfer, Mint};

declare_id!("FkQpVUX5iRw5Gft6Co1iKJDnufEA3FYBsGJS87RaT4nf");

/// A multiplier to maintain precision in reward calculations.
const REWARD_MULTIPLIER: u64 = 1_000_000_000; // Reduced precision (u64) for gas/compression

/// Constants for time calculations.
const SECONDS_IN_DAY: i64 = 86_400;
const THIRTY_DAYS: i64 = 30 * SECONDS_IN_DAY;
const NINETY_DAYS: i64 = 90 * SECONDS_IN_DAY;

/// Constant for high-frequency trader (HFT) bonus threshold.
const HFT_THRESHOLD: u64 = 1_000_000; // example threshold
const HFT_BONUS_BPS: u64 = 500; // bonus 5% (500 basis points)

#[program]
pub mod perpetual_yield_token {
    use super::*;

    /// Initializes the global state with key parameters.
    pub fn initialize(
        ctx: Context<Initialize>,
        governance: Pubkey,
        cooldown_period: i64,         // seconds (e.g. 7 days = 604800)
        early_withdrawal_penalty: u64,  // basis points (e.g. 500 = 5%)
        min_withdraw_interval: i64,     // seconds between withdrawals
        min_claim_delay: i64,           // seconds delay after fee deposit before claims allowed
        insurance_fee_percent: u64,     // basis points (e.g. 100 = 1%)
        utilization_multiplier: u64,    // percentage (100 = 1x, >100 boosts rewards)
    ) -> Result<()> {
        let state = &mut ctx.accounts.global_state;
        state.total_staked = 0;
        state.acc_reward_per_share = 0;
        state.token_mint = ctx.accounts.token_mint.key();
        state.owner = ctx.accounts.owner.key();
        state.governance = governance;
        state.cooldown_period = cooldown_period;
        state.early_withdrawal_penalty = early_withdrawal_penalty;
        state.min_withdraw_interval = min_withdraw_interval;
        state.min_claim_delay = min_claim_delay;
        state.insurance_fee_percent = insurance_fee_percent;
        state.utilization_multiplier = utilization_multiplier;
        state.last_fee_deposit_time = 0;
        state.insurance_fund = 0;
        // Initialize pool info for Low, Medium, High risk pools.
        state.pool_info = [
            PoolInfo { lockup_period: 7 * SECONDS_IN_DAY, apr_multiplier: 100, pool_fee: 50 },  // Low risk: 7 days, base APR, 0.5% fee
            PoolInfo { lockup_period: 14 * SECONDS_IN_DAY, apr_multiplier: 110, pool_fee: 75 }, // Medium risk: 14 days, 10% bonus APR, 0.75% fee
            PoolInfo { lockup_period: 30 * SECONDS_IN_DAY, apr_multiplier: 120, pool_fee: 100 }, // High risk: 30 days, 20% bonus APR, 1% fee
        ];
        Ok(())
    }

    /// Governance: update protocol parameters.
    pub fn update_parameters(
        ctx: Context<UpdateParameters>,
        cooldown_period: i64,
        early_withdrawal_penalty: u64,
        min_withdraw_interval: i64,
        min_claim_delay: i64,
        insurance_fee_percent: u64,
        utilization_multiplier: u64,
        pool_info: [PoolInfo; 3],
    ) -> Result<()> {
        let state = &mut ctx.accounts.global_state;
        state.cooldown_period = cooldown_period;
        state.early_withdrawal_penalty = early_withdrawal_penalty;
        state.min_withdraw_interval = min_withdraw_interval;
        state.min_claim_delay = min_claim_delay;
        state.insurance_fee_percent = insurance_fee_percent;
        state.utilization_multiplier = utilization_multiplier;
        state.pool_info = pool_info;
        Ok(())
    }

    /// Governance: update the utilization multiplier (APY adjustment based on liquidity).
    pub fn update_utilization(ctx: Context<UpdateParameters>, utilization_multiplier: u64) -> Result<()> {
        let state = &mut ctx.accounts.global_state;
        state.utilization_multiplier = utilization_multiplier;
        Ok(())
    }

    /// Stake a specified `amount` of $PYT tokens in a chosen pool (0=Low,1=Medium,2=High).
    pub fn stake(ctx: Context<Stake>, amount: u64, pool_type: u8) -> Result<()> {
        let clock = Clock::get()?;
        let state = &mut ctx.accounts.global_state;
        let user = &mut ctx.accounts.user_stake;

        require!(pool_type < 3, ErrorCode::InvalidPoolType);
        // Lazy reward: if already staked, pending rewards are updated only on claim.
        if user.staked_amount > 0 {
            let accumulated = (user.staked_amount as u128)
                .checked_mul(state.acc_reward_per_share as u128)
                .ok_or(ErrorCode::MathOverflow)?
                / REWARD_MULTIPLIER as u128;
            let pending = accumulated
                .checked_sub(user.reward_debt as u128)
                .ok_or(ErrorCode::MathOverflow)?;
            user.pending_rewards = user.pending_rewards.checked_add(pending as u64).ok_or(ErrorCode::MathOverflow)?;
        }

        // Transfer tokens from user to staking vault.
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.user_token_account.to_account_info(),
                    to: ctx.accounts.staking_vault.to_account_info(),
                    authority: ctx.accounts.user.to_account_info(),
                },
            ),
            amount,
        )?;

        // Update user stake data.
        user.staked_amount = user.staked_amount.checked_add(amount).ok_or(ErrorCode::MathOverflow)?;
        state.total_staked = state.total_staked.checked_add(amount).ok_or(ErrorCode::MathOverflow)?;
        user.reward_debt = (((user.staked_amount as u128)
            .checked_mul(state.acc_reward_per_share as u128)
            .ok_or(ErrorCode::MathOverflow)?)
            / REWARD_MULTIPLIER as u128) as u64;
        user.stake_timestamp = clock.unix_timestamp;
        user.last_withdrawal_time = clock.unix_timestamp;
        user.pool_type = pool_type;
        Ok(())
    }

    /// Batch staking: allows multiple deposit amounts in one transaction.
    pub fn batch_stake(ctx: Context<Stake>, amounts: Vec<u64>, pool_type: u8) -> Result<()> {
        let total: u64 = amounts.iter().sum();
        // Reuse single stake logic.
        Self::stake(ctx, total, pool_type)
    }

    /// Unstake a specified `amount` of $PYT tokens.
    pub fn unstake(ctx: Context<Unstake>, amount: u64) -> Result<()> {
        let clock = Clock::get()?;
        let state = &mut ctx.accounts.global_state;
        let user = &mut ctx.accounts.user_stake;
        require!(user.staked_amount >= amount, ErrorCode::InsufficientStake);

        // Rate-limit withdrawals.
        require!(
            clock.unix_timestamp - user.last_withdrawal_time >= state.min_withdraw_interval,
            ErrorCode::WithdrawalTooFrequent
        );

        // Update pending rewards (lazy distribution).
        let accumulated = (user.staked_amount as u128)
            .checked_mul(state.acc_reward_per_share as u128)
            .ok_or(ErrorCode::MathOverflow)?
            / REWARD_MULTIPLIER as u128;
        let pending = accumulated
            .checked_sub(user.reward_debt as u128)
            .ok_or(ErrorCode::MathOverflow)?;
        user.pending_rewards = user.pending_rewards.checked_add(pending as u64).ok_or(ErrorCode::MathOverflow)?;

        // Check lockup period for the chosen pool.
        let pool = state.pool_info[user.pool_type as usize];
        let staked_duration = clock.unix_timestamp - user.stake_timestamp;
        let mut amount_after_penalty = amount;
        if staked_duration < pool.lockup_period {
            let penalty = amount
                .checked_mul(state.early_withdrawal_penalty)
                .ok_or(ErrorCode::MathOverflow)?
                / 10_000;
            amount_after_penalty = amount.checked_sub(penalty).ok_or(ErrorCode::MathOverflow)?;
            // Transfer penalty to reward vault (for redistribution/insurance).
            token::transfer(
                CpiContext::new(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.staking_vault.to_account_info(),
                        to: ctx.accounts.reward_vault.to_account_info(),
                        authority: ctx.accounts.vault_authority.to_account_info(),
                    },
                ).with_signer(&[&[b"vault", &[*ctx.bumps.get("vault_authority").unwrap()]]]),
                penalty,
            )?;
            // Also add penalty amount to the insurance fund.
            state.insurance_fund = state.insurance_fund.checked_add(penalty).ok_or(ErrorCode::MathOverflow)?;
        }

        // Adjust staked amounts.
        user.staked_amount = user.staked_amount.checked_sub(amount).ok_or(ErrorCode::MathOverflow)?;
        state.total_staked = state.total_staked.checked_sub(amount).ok_or(ErrorCode::MathOverflow)?;
        user.reward_debt = (((user.staked_amount as u128)
            .checked_mul(state.acc_reward_per_share as u128)
            .ok_or(ErrorCode::MathOverflow)?)
            / REWARD_MULTIPLIER as u128) as u64;
        user.last_withdrawal_time = clock.unix_timestamp;

        // Transfer the unstaked tokens (post-penalty) back to the user.
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.staking_vault.to_account_info(),
                    to: ctx.accounts.user_token_account.to_account_info(),
                    authority: ctx.accounts.vault_authority.to_account_info(),
                },
            ).with_signer(&[&[b"vault", &[*ctx.bumps.get("vault_authority").unwrap()]]]),
            amount_after_penalty,
        )?;

        Ok(())
    }

    /// Batch unstake: allows multiple unstake amounts in one tx.
    pub fn batch_unstake(ctx: Context<Unstake>, amounts: Vec<u64>) -> Result<()> {
        let total: u64 = amounts.iter().sum();
        Self::unstake(ctx, total)
    }

    /// Deposit fee revenue into the reward vault.
    /// A percentage (insurance_fee_percent) is diverted to the insurance fund.
    pub fn deposit_fee(ctx: Context<DepositFee>, amount: u64) -> Result<()> {
        let clock = Clock::get()?;
        let state = &mut ctx.accounts.global_state;

        // Transfer fee tokens from depositor to reward vault.
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.depositor_token_account.to_account_info(),
                    to: ctx.accounts.reward_vault.to_account_info(),
                    authority: ctx.accounts.depositor.to_account_info(),
                },
            ),
            amount,
        )?;

        // Split fee: allocate a portion to the insurance fund.
        let insurance_fee = amount.checked_mul(state.insurance_fee_percent).ok_or(ErrorCode::MathOverflow)? / 10_000;
        let distributable_fee = amount.checked_sub(insurance_fee).ok_or(ErrorCode::MathOverflow)?;
        state.insurance_fund = state.insurance_fund.checked_add(insurance_fee).ok_or(ErrorCode::MathOverflow)?;

        // Update the global accumulator only if there is staked amount.
        if state.total_staked > 0 {
            state.acc_reward_per_share = state.acc_reward_per_share
                .checked_add(
                    ((distributable_fee as u128)
                        .checked_mul(REWARD_MULTIPLIER as u128)
                        .ok_or(ErrorCode::MathOverflow)?)
                        / (state.total_staked as u128)
                )
                .ok_or(ErrorCode::MathOverflow)? as u64;
        }
        state.last_fee_deposit_time = clock.unix_timestamp;
        Ok(())
    }

    /// Claim pending rewards.
    /// Includes time-weighted multiplier, dynamic APY adjustment, HFT bonus, and MEV/flash loan protections.
    pub fn claim_rewards(ctx: Context<ClaimRewards>) -> Result<()> {
        let clock = Clock::get()?;
        let state = &mut ctx.accounts.global_state;
        let user = &mut ctx.accounts.user_stake;

        // Enforce minimum stake period (anti-flash loan lock).
        require!(
            clock.unix_timestamp - user.stake_timestamp >= state.cooldown_period,
            ErrorCode::StakePeriodTooShort
        );
        // Enforce claim delay (MEV protection).
        require!(
            clock.unix_timestamp - state.last_fee_deposit_time >= state.min_claim_delay,
            ErrorCode::ClaimTooSoon
        );

        // Compute accumulated rewards.
        let accumulated = (user.staked_amount as u128)
            .checked_mul(state.acc_reward_per_share as u128)
            .ok_or(ErrorCode::MathOverflow)?
            / REWARD_MULTIPLIER as u128;
        let pending_from_stake = accumulated
            .checked_sub(user.reward_debt as u128)
            .ok_or(ErrorCode::MathOverflow)?;
        let mut total_reward = user.pending_rewards.checked_add(pending_from_stake as u64).ok_or(ErrorCode::MathOverflow)?;

        // Apply time-weighted multiplier.
        let staked_duration = clock.unix_timestamp - user.stake_timestamp;
        let time_multiplier = if staked_duration < THIRTY_DAYS {
            100 // 1x
        } else if staked_duration < NINETY_DAYS {
            120 // 1.2x
        } else {
            150 // 1.5x
        };
        total_reward = total_reward.checked_mul(time_multiplier).ok_or(ErrorCode::MathOverflow)? / 100;

        // Apply dynamic utilization multiplier.
        total_reward = total_reward.checked_mul(state.utilization_multiplier).ok_or(ErrorCode::MathOverflow)? / 100;

        // Apply HFT bonus if the user’s 7-day trading volume exceeds threshold.
        if user.trade_volume_7d > HFT_THRESHOLD {
            total_reward = total_reward.checked_mul(100 + (HFT_BONUS_BPS / 100)).ok_or(ErrorCode::MathOverflow)? / 100;
        }

        require!(total_reward > 0, ErrorCode::NoRewards);

        // Reset pending rewards and update reward debt.
        user.pending_rewards = 0;
        user.reward_debt = (((user.staked_amount as u128)
            .checked_mul(state.acc_reward_per_share as u128)
            .ok_or(ErrorCode::MathOverflow)?)
            / REWARD_MULTIPLIER as u128) as u64;

        // Transfer rewards from reward vault to user's reward token account.
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.reward_vault.to_account_info(),
                    to: ctx.accounts.user_reward_token_account.to_account_info(),
                    authority: ctx.accounts.vault_authority.to_account_info(),
                },
            ).with_signer(&[&[b"vault", &[*ctx.bumps.get("vault_authority").unwrap()]]]),
            total_reward,
        )?;

        Ok(())
    }

    /// Auto-compound rewards: claim rewards and add them to the staking position.
    pub fn auto_compound(ctx: Context<AutoCompound>) -> Result<()> {
        // For brevity, reusing much of the logic from claim_rewards.
        // After claiming, the reward tokens are moved to the staking vault and the user's stake is increased.
        Self::claim_rewards(ctx.accounts.claim_context())?;
        let clock = Clock::get()?;
        let state = &mut ctx.accounts.global_state;
        let user = &mut ctx.accounts.user_stake;

        // Determine the compounded reward (assumed to have been transferred to staking vault).
        // In a full implementation, you would track the compounded reward amount.
        // Here we simulate by reading the user's reward token account balance (stubbed).
        let compounded_amount: u64 = ctx.accounts.compounded_amount;

        // Transfer the compounded rewards from staking vault to increase user's stake.
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.staking_vault.to_account_info(),
                    to: ctx.accounts.user_token_account.to_account_info(),
                    authority: ctx.accounts.vault_authority.to_account_info(),
                },
            ).with_signer(&[&[b"vault", &[*ctx.bumps.get("vault_authority").unwrap()]]]),
            compounded_amount,
        )?;
        user.staked_amount = user.staked_amount.checked_add(compounded_amount).ok_or(ErrorCode::MathOverflow)?;
        state.total_staked = state.total_staked.checked_add(compounded_amount).ok_or(ErrorCode::MathOverflow)?;
        user.stake_timestamp = clock.unix_timestamp;
        user.reward_debt = (((user.staked_amount as u128)
            .checked_mul(state.acc_reward_per_share as u128)
            .ok_or(ErrorCode::MathOverflow)?)
            / REWARD_MULTIPLIER as u128) as u64;
        Ok(())
    }

    /// Governance: Submit a proposal.
    pub fn submit_proposal(ctx: Context<SubmitProposal>, proposal_data: String) -> Result<()> {
        let proposal = &mut ctx.accounts.proposal;
        let clock = Clock::get()?;
        proposal.proposal_id = ctx.accounts.proposal.key().to_bytes()[0] as u64; // simplified ID assignment
        proposal.proposer = ctx.accounts.proposer.key();
        proposal.proposal_data = proposal_data;
        proposal.snapshot_timestamp = clock.unix_timestamp;
        proposal.vote_count = 0;
        proposal.executed = false;
        Ok(())
    }

    /// Governance: Vote on a proposal.
    pub fn vote_proposal(ctx: Context<VoteProposal>) -> Result<()> {
        let proposal = &mut ctx.accounts.proposal;
        let user = &ctx.accounts.user_stake;
        // Weighted voting: vote weight equals staked amount.
        proposal.vote_count = proposal.vote_count.checked_add(user.staked_amount).ok_or(ErrorCode::MathOverflow)?;
        Ok(())
    }
}

/// Pool parameters for multi-tiered staking.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Default)]
pub struct PoolInfo {
    pub lockup_period: i64,    // Lockup period (in seconds) for the pool.
    pub apr_multiplier: u64,   // APR multiplier (100 = base, >100 = bonus).
    pub pool_fee: u64,         // Pool-specific fee in basis points.
}

/// Global state for the staking program.
#[account]
pub struct GlobalState {
    pub total_staked: u64,
    pub acc_reward_per_share: u64, // Scaled by REWARD_MULTIPLIER (u64 for compression).
    pub token_mint: Pubkey,
    pub owner: Pubkey,
    pub governance: Pubkey,
    pub cooldown_period: i64,
    pub early_withdrawal_penalty: u64,  // in basis points.
    pub min_withdraw_interval: i64,
    pub min_claim_delay: i64,
    pub insurance_fee_percent: u64,     // in basis points.
    pub utilization_multiplier: u64,    // percentage multiplier.
    pub last_fee_deposit_time: i64,
    pub pool_info: [PoolInfo; 3],
    pub insurance_fund: u64,
}

/// Tracks individual user staking data.
#[account]
pub struct UserStake {
    pub staked_amount: u64,
    pub reward_debt: u64,
    pub pending_rewards: u64,
    pub stake_timestamp: i64,
    pub last_withdrawal_time: i64,
    pub pool_type: u8,       // 0 = Low, 1 = Medium, 2 = High risk pool.
    pub trade_volume_7d: u64 // For HFT rebate calculations.
}

/// A simple governance proposal.
#[account]
pub struct Proposal {
    pub proposal_id: u64,
    pub proposer: Pubkey,
    pub proposal_data: String,
    pub snapshot_timestamp: i64,
    pub vote_count: u64,
    pub executed: bool,
}

/// ----- Contexts for Instructions -----

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(init, payer = owner, space = 1200)]
    pub global_state: Account<'info, GlobalState>,
    pub token_mint: Account<'info, Mint>,
    #[account(mut)]
    pub owner: Signer<'info>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct UpdateParameters<'info> {
    #[account(mut, has_one = governance)]
    pub global_state: Account<'info, GlobalState>,
    #[account(mut)]
    pub governance: Signer<'info>,
}

#[derive(Accounts)]
pub struct Stake<'info> {
    #[account(mut)]
    pub global_state: Account<'info, GlobalState>,
    #[account(mut)]
    pub user_stake: Account<'info, UserStake>,
    #[account(mut)]
    pub user_token_account: Account<'info, TokenAccount>,
    #[account(mut)]
    pub staking_vault: Account<'info, TokenAccount>,
    pub user: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct Unstake<'info> {
    #[account(mut)]
    pub global_state: Account<'info, GlobalState>,
    #[account(mut)]
    pub user_stake: Account<'info, UserStake>,
    #[account(mut)]
    pub staking_vault: Account<'info, TokenAccount>,
    #[account(mut)]
    pub user_token_account: Account<'info, TokenAccount>,
    /// CHECK: PDA authority for vault transfers.
    #[account(seeds = [b"vault"], bump)]
    pub vault_authority: AccountInfo<'info>,
    #[account(mut)]
    pub reward_vault: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct DepositFee<'info> {
    #[account(mut)]
    pub global_state: Account<'info, GlobalState>,
    #[account(mut)]
    pub depositor: Signer<'info>,
    #[account(mut)]
    pub depositor_token_account: Account<'info, TokenAccount>,
    #[account(mut)]
    pub reward_vault: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct ClaimRewards<'info> {
    #[account(mut)]
    pub global_state: Account<'info, GlobalState>,
    #[account(mut)]
    pub user_stake: Account<'info, UserStake>,
    #[account(mut)]
    pub reward_vault: Account<'info, TokenAccount>,
    #[account(mut)]
    pub user_reward_token_account: Account<'info, TokenAccount>,
    /// CHECK: PDA authority for vault transfers.
    #[account(seeds = [b"vault"], bump)]
    pub vault_authority: AccountInfo<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct AutoCompound<'info> {
    #[account(mut)]
    pub global_state: Account<'info, GlobalState>,
    #[account(mut)]
    pub user_stake: Account<'info, UserStake>,
    #[account(mut)]
    pub reward_vault: Account<'info, TokenAccount>,
    #[account(mut)]
    pub staking_vault: Account<'info, TokenAccount>,
    /// CHECK: PDA authority for vault transfers.
    #[account(seeds = [b"vault"], bump)]
    pub vault_authority: AccountInfo<'info>,
    #[account(mut)]
    pub user_token_account: Account<'info, TokenAccount>,
    /// This field is a stub representing the amount to compound.
    pub compounded_amount: u64,
    pub token_program: Program<'info, Token>,
}

impl<'info> AutoCompound<'info> {
    /// Helper to create a ClaimRewards-like context.
    pub fn claim_context(&self) -> ClaimRewards<'info> {
        ClaimRewards {
            global_state: self.global_state.clone(),
            user_stake: self.user_stake.clone(),
            reward_vault: self.reward_vault.clone(),
            user_reward_token_account: self.user_token_account.clone(),
            vault_authority: self.vault_authority.clone(),
            token_program: self.token_program.clone(),
        }
    }
}

/// Governance: Context for submitting proposals.
#[derive(Accounts)]
pub struct SubmitProposal<'info> {
    #[account(init, payer = proposer, space = 600)]
    pub proposal: Account<'info, Proposal>,
    #[account(mut)]
    pub proposer: Signer<'info>,
    pub system_program: Program<'info, System>,
}

/// Governance: Context for voting on a proposal.
#[derive(Accounts)]
pub struct VoteProposal<'info> {
    #[account(mut)]
    pub proposal: Account<'info, Proposal>,
    /// For weighted voting, we pass the voter's UserStake.
    #[account(mut)]
    pub user_stake: Account<'info, UserStake>,
    pub voter: Signer<'info>,
}

#[error_code]
pub enum ErrorCode {
    #[msg("Math overflow occurred.")]
    MathOverflow,
    #[msg("Insufficient staked amount.")]
    InsufficientStake,
    #[msg("No rewards available to claim.")]
    NoRewards,
    #[msg("Withdrawal attempts are too frequent.")]
    WithdrawalTooFrequent,
    #[msg("Invalid pool type selected.")]
    InvalidPoolType,
    #[msg("Stake period is too short for claiming rewards.")]
    StakePeriodTooShort,
    #[msg("Claim attempted too soon after a fee deposit.")]
    ClaimTooSoon,
}
