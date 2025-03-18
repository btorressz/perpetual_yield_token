use anchor_lang::prelude::*;
use anchor_lang::solana_program::clock::Clock;
use anchor_spl::token::{self, Token, TokenAccount, Transfer, Mint};
use std::convert::TryInto;

declare_id!("FkQpVUX5iRw5Gft6Co1iKJDnufEA3FYBsGJS87RaT4nf");

/// A multiplier to maintain precision (using u64 for efficiency).
const REWARD_MULTIPLIER: u64 = 1_000_000_000;
/// Time constants (in seconds).
const SECONDS_IN_DAY: i64 = 86_400;
const THIRTY_DAYS: i64 = 30 * SECONDS_IN_DAY;
const NINETY_DAYS: i64 = 90 * SECONDS_IN_DAY;
/// Bonus multiplier for LP stakers (e.g. 110 means +10% bonus).
const LP_BONUS_MULTIPLIER: u64 = 110;

#[program]
pub mod perpetual_yield_token {
    use super::*;

    /// Initialize the global state.
    pub fn initialize(
        ctx: Context<Initialize>,
        governance: Pubkey,
        cooldown_period: i64,
        early_withdrawal_penalty: u64,
        min_withdraw_interval: i64,
        min_claim_delay: i64,
        insurance_fee_percent: u64,
        utilization_multiplier: u64,
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
        state.pool_info = [
            PoolInfo { lockup_period: 7 * SECONDS_IN_DAY, apr_multiplier: 100, transaction_fee: 50 },
            PoolInfo { lockup_period: 14 * SECONDS_IN_DAY, apr_multiplier: 110, transaction_fee: 75 },
            PoolInfo { lockup_period: 30 * SECONDS_IN_DAY, apr_multiplier: 120, transaction_fee: 100 },
        ];
        Ok(())
    }

    /// Update protocol parameters.
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

    /// Update the utilization multiplier.
    pub fn update_utilization(ctx: Context<UpdateParameters>, utilization_multiplier: u64) -> Result<()> {
        let state = &mut ctx.accounts.global_state;
        state.utilization_multiplier = utilization_multiplier;
        Ok(())
    }

    /// Stake $PYT tokens into a chosen pool (0 = Low, 1 = Medium, 2 = High).
    pub fn stake(ctx: Context<Stake>, amount: u64, pool_type: u8) -> Result<()> {
        let clock = Clock::get()?;
        let state = &mut ctx.accounts.global_state;
        let user = &mut ctx.accounts.user_stake;
        require!(pool_type < 3, CustomError::InvalidPoolType);

        if user.staked_amount > 0 {
            let accumulated = (user.staked_amount as u128)
                .checked_mul(state.acc_reward_per_share as u128)
                .ok_or(CustomError::MathOverflow)?
                / REWARD_MULTIPLIER as u128;
            let pending = accumulated.checked_sub(user.reward_debt as u128)
                .ok_or(CustomError::MathOverflow)?;
            user.pending_rewards = user.pending_rewards
                .checked_add(pending as u64)
                .ok_or(CustomError::MathOverflow)?;
        }

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

        user.staked_amount = user.staked_amount.checked_add(amount).ok_or(CustomError::MathOverflow)?;
        state.total_staked = state.total_staked.checked_add(amount).ok_or(CustomError::MathOverflow)?;
        user.reward_debt = (((user.staked_amount as u128)
            .checked_mul(state.acc_reward_per_share as u128)
            .ok_or(CustomError::MathOverflow)?)
            / REWARD_MULTIPLIER as u128)
            .try_into()
            .unwrap();
        user.stake_timestamp = clock.unix_timestamp;
        user.last_withdrawal_time = clock.unix_timestamp;
        user.pool_type = pool_type;
        Ok(())
    }

    /// Batch stake.
    pub fn batch_stake(ctx: Context<Stake>, amounts: Vec<u64>, pool_type: u8) -> Result<()> {
        let total: u64 = amounts.iter().sum();
        stake(ctx, total, pool_type)
    }

    /// Unstake $PYT tokens.
    pub fn unstake(ctx: Context<Unstake>, amount: u64) -> Result<()> {
        let clock = Clock::get()?;
        let state = &mut ctx.accounts.global_state;
        let user = &mut ctx.accounts.user_stake;
        require!(user.staked_amount >= amount, CustomError::InsufficientStake);
        require!(
            clock.unix_timestamp - user.last_withdrawal_time >= state.min_withdraw_interval,
            CustomError::WithdrawalTooFrequent
        );

        let accumulated = (user.staked_amount as u128)
            .checked_mul(state.acc_reward_per_share as u128)
            .ok_or(CustomError::MathOverflow)?
            / REWARD_MULTIPLIER as u128;
        let pending = accumulated.checked_sub(user.reward_debt as u128)
            .ok_or(CustomError::MathOverflow)?;
        user.pending_rewards = user.pending_rewards.checked_add(pending as u64)
            .ok_or(CustomError::MathOverflow)?;

        // Clone pool info so we don't move it.
        let pool = state.pool_info[user.pool_type as usize].clone();
        let staked_duration = clock.unix_timestamp - user.stake_timestamp;
        let mut amount_after_penalty = amount;
        if staked_duration < pool.lockup_period {
            let penalty = amount.checked_mul(state.early_withdrawal_penalty)
                .ok_or(CustomError::MathOverflow)?
                / 10_000;
            amount_after_penalty = amount.checked_sub(penalty).ok_or(CustomError::MathOverflow)?;
            token::transfer(
                CpiContext::new(
                    ctx.accounts.token_program.to_account_info(),
                    Transfer {
                        from: ctx.accounts.staking_vault.to_account_info(),
                        to: ctx.accounts.reward_vault.to_account_info(),
                        authority: ctx.accounts.vault_authority.to_account_info(),
                    },
                )
                .with_signer(&[&[b"vault", &[ctx.bumps.vault_authority]]]),
                penalty,
            )?;
            state.insurance_fund = state.insurance_fund.checked_add(penalty).ok_or(CustomError::MathOverflow)?;
        }

        user.staked_amount = user.staked_amount.checked_sub(amount).ok_or(CustomError::MathOverflow)?;
        state.total_staked = state.total_staked.checked_sub(amount).ok_or(CustomError::MathOverflow)?;
        user.reward_debt = (((user.staked_amount as u128)
            .checked_mul(state.acc_reward_per_share as u128)
            .ok_or(CustomError::MathOverflow)?)
            / REWARD_MULTIPLIER as u128)
            .try_into()
            .unwrap();
        user.last_withdrawal_time = clock.unix_timestamp;

        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.staking_vault.to_account_info(),
                    to: ctx.accounts.user_token_account.to_account_info(),
                    authority: ctx.accounts.vault_authority.to_account_info(),
                },
            )
            .with_signer(&[&[b"vault", &[ctx.bumps.vault_authority]]]),
            amount_after_penalty,
        )?;
        Ok(())
    }

    /// Batch unstake.
    pub fn batch_unstake(ctx: Context<Unstake>, amounts: Vec<u64>) -> Result<()> {
        let total: u64 = amounts.iter().sum();
        unstake(ctx, total)
    }

    /// Deposit transaction revenue into the reward vault.
    pub fn deposit_transaction_fee(ctx: Context<DepositFee>, amount: u64) -> Result<()> {
        let clock = Clock::get()?;
        let state = &mut ctx.accounts.global_state;
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
        let insurance_fee = amount.checked_mul(state.insurance_fee_percent)
            .ok_or(CustomError::MathOverflow)? / 10_000;
        let distributable = amount.checked_sub(insurance_fee).ok_or(CustomError::MathOverflow)?;
        state.insurance_fund = state.insurance_fund.checked_add(insurance_fee).ok_or(CustomError::MathOverflow)?;
        if state.total_staked > 0 {
            let add_amount: u64 = (((distributable as u128)
                .checked_mul(REWARD_MULTIPLIER as u128)
                .ok_or(CustomError::MathOverflow)?)
                / (state.total_staked as u128))
                .try_into()
                .unwrap();
            state.acc_reward_per_share = state.acc_reward_per_share.checked_add(add_amount).ok_or(CustomError::MathOverflow)?;
        }
        state.last_fee_deposit_time = clock.unix_timestamp;
        Ok(())
    }

    /// Claim pending rewards.
    pub fn claim_rewards(ctx: Context<ClaimRewards>, proof: String) -> Result<()> {
        let mut ctx = ctx;
        _claim_rewards(&mut ctx, proof)
    }

    /// Auto-compound: claim rewards and restake them.
    pub fn auto_compound(ctx: Context<AutoCompound>, proof: String, compounded_amount: u64) -> Result<()> {
        // Build a new ClaimRewards struct from AutoCompound accounts.
        let mut claim_accounts = ClaimRewards {
            global_state: ctx.accounts.global_state.clone(),
            user_stake: ctx.accounts.user_stake.clone(),
            reward_vault: ctx.accounts.reward_vault.clone(),
            user_reward_token_account: ctx.accounts.user_token_account.clone(),
            vault_authority: ctx.accounts.vault_authority.clone(),
            token_program: ctx.accounts.token_program.clone(),
        };
        // Create a mutable Context for ClaimRewards using default bumps.
        let mut claim_ctx = Context {
            program_id: ctx.program_id,
            accounts: &mut claim_accounts,
            remaining_accounts: ctx.remaining_accounts.clone(),
            bumps: Default::default(),
        };
        _claim_rewards(&mut claim_ctx, proof)?;
        let clock = Clock::get()?;
        let state = &mut ctx.accounts.global_state;
        let user = &mut ctx.accounts.user_stake;
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.staking_vault.to_account_info(),
                    to: ctx.accounts.user_token_account.to_account_info(),
                    authority: ctx.accounts.vault_authority.to_account_info(),
                },
            )
            .with_signer(&[&[b"vault", &[ctx.bumps.vault_authority]]]),
            compounded_amount,
        )?;
        user.staked_amount = user.staked_amount.checked_add(compounded_amount).ok_or(CustomError::MathOverflow)?;
        state.total_staked = state.total_staked.checked_add(compounded_amount).ok_or(CustomError::MathOverflow)?;
        user.stake_timestamp = clock.unix_timestamp;
        user.reward_debt = (((user.staked_amount as u128)
            .checked_mul(state.acc_reward_per_share as u128)
            .ok_or(CustomError::MathOverflow)?)
            / REWARD_MULTIPLIER as u128)
            .try_into()
            .unwrap();
        Ok(())
    }

    /// LP Staking: stake LP tokens.
    pub fn lp_stake(ctx: Context<LPStake>, amount: u64) -> Result<()> {
        let clock = Clock::get()?;
        let state = &mut ctx.accounts.global_state;
        let lp_user = &mut ctx.accounts.lp_user_stake;
        if lp_user.staked_amount > 0 {
            let accumulated = (lp_user.staked_amount as u128)
                .checked_mul(state.acc_reward_per_share as u128)
                .ok_or(CustomError::MathOverflow)?
                / REWARD_MULTIPLIER as u128;
            let pending = accumulated.checked_sub(lp_user.reward_debt as u128)
                .ok_or(CustomError::MathOverflow)?;
            lp_user.pending_rewards = lp_user.pending_rewards.checked_add(pending as u64)
                .ok_or(CustomError::MathOverflow)?;
        }
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.user_lp_token_account.to_account_info(),
                    to: ctx.accounts.lp_staking_vault.to_account_info(),
                    authority: ctx.accounts.user.to_account_info(),
                },
            ),
            amount,
        )?;
        lp_user.staked_amount = lp_user.staked_amount.checked_add(amount).ok_or(CustomError::MathOverflow)?;
        lp_user.reward_debt = (((lp_user.staked_amount as u128)
            .checked_mul(state.acc_reward_per_share as u128)
            .ok_or(CustomError::MathOverflow)?)
            / REWARD_MULTIPLIER as u128)
            .try_into()
            .unwrap();
        lp_user.stake_timestamp = clock.unix_timestamp;
        lp_user.last_withdrawal_time = clock.unix_timestamp;
        Ok(())
    }

    /// LP Unstake.
    pub fn lp_unstake(ctx: Context<LPUnstake>, amount: u64) -> Result<()> {
        let clock = Clock::get()?;
        let state = &mut ctx.accounts.global_state;
        let lp_user = &mut ctx.accounts.lp_user_stake;
        require!(lp_user.staked_amount >= amount, CustomError::InsufficientStake);
        require!(
            clock.unix_timestamp - lp_user.last_withdrawal_time >= state.min_withdraw_interval,
            CustomError::WithdrawalTooFrequent
        );
        let accumulated = (lp_user.staked_amount as u128)
            .checked_mul(state.acc_reward_per_share as u128)
            .ok_or(CustomError::MathOverflow)?
            / REWARD_MULTIPLIER as u128;
        let pending = accumulated.checked_sub(lp_user.reward_debt as u128)
            .ok_or(CustomError::MathOverflow)?;
        lp_user.pending_rewards = lp_user.pending_rewards.checked_add(pending as u64)
            .ok_or(CustomError::MathOverflow)?;
        lp_user.staked_amount = lp_user.staked_amount.checked_sub(amount).ok_or(CustomError::MathOverflow)?;
        lp_user.reward_debt = (((lp_user.staked_amount as u128)
            .checked_mul(state.acc_reward_per_share as u128)
            .ok_or(CustomError::MathOverflow)?)
            / REWARD_MULTIPLIER as u128)
            .try_into()
            .unwrap();
        lp_user.last_withdrawal_time = clock.unix_timestamp;
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.lp_staking_vault.to_account_info(),
                    to: ctx.accounts.user_lp_token_account.to_account_info(),
                    authority: ctx.accounts.vault_authority.to_account_info(),
                },
            )
            .with_signer(&[&[b"vault", &[ctx.bumps.vault_authority]]]),
            amount,
        )?;
        Ok(())
    }

    /// LP Claim Rewards.
    pub fn lp_claim_rewards(ctx: Context<LPClaimRewards>, proof: String) -> Result<()> {
        let clock = Clock::get()?;
        let state = &mut ctx.accounts.global_state;
        let lp_user = &mut ctx.accounts.lp_user_stake;
        require!(verify_mev_proof(&proof), CustomError::InvalidMEVProof);
        require!(
            clock.unix_timestamp - lp_user.stake_timestamp >= state.cooldown_period,
            CustomError::StakePeriodTooShort
        );
        require!(
            clock.unix_timestamp - state.last_fee_deposit_time >= state.min_claim_delay,
            CustomError::ClaimTooSoon
        );
        let accumulated = (lp_user.staked_amount as u128)
            .checked_mul(state.acc_reward_per_share as u128)
            .ok_or(CustomError::MathOverflow)?
            / REWARD_MULTIPLIER as u128;
        let pending_from_stake = accumulated.checked_sub(lp_user.reward_debt as u128)
            .ok_or(CustomError::MathOverflow)?;
        let mut total_reward = lp_user.pending_rewards.checked_add(pending_from_stake as u64)
            .ok_or(CustomError::MathOverflow)?;
        let staked_duration = clock.unix_timestamp - lp_user.stake_timestamp;
        let time_multiplier = if staked_duration < THIRTY_DAYS { 100 }
                              else if staked_duration < NINETY_DAYS { 120 }
                              else { 150 };
        total_reward = total_reward.checked_mul(time_multiplier).ok_or(CustomError::MathOverflow)? / 100;
        total_reward = total_reward.checked_mul(LP_BONUS_MULTIPLIER).ok_or(CustomError::MathOverflow)? / 100;
        total_reward = total_reward.checked_mul(state.utilization_multiplier).ok_or(CustomError::MathOverflow)? / 100;
        let rebate = calculate_rebate(lp_user.trade_volume_7d);
        total_reward = total_reward.checked_mul(100 + rebate).ok_or(CustomError::MathOverflow)? / 100;
        require!(total_reward > 0, CustomError::NoRewards);
        lp_user.pending_rewards = 0;
        lp_user.reward_debt = (((lp_user.staked_amount as u128)
            .checked_mul(state.acc_reward_per_share as u128)
            .ok_or(CustomError::MathOverflow)?)
            / REWARD_MULTIPLIER as u128)
            .try_into()
            .unwrap();
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.to_account_info(),
                Transfer {
                    from: ctx.accounts.reward_vault.to_account_info(),
                    to: ctx.accounts.user_reward_token_account.to_account_info(),
                    authority: ctx.accounts.vault_authority.to_account_info(),
                },
            )
            .with_signer(&[&[b"vault", &[ctx.bumps.vault_authority]]]),
            total_reward,
        )?;
        Ok(())
    }
}

pub(crate) fn _claim_rewards(ctx: &mut Context<ClaimRewards>, proof: String) -> Result<()> {
    let clock = Clock::get()?;
    let state = &mut ctx.accounts.global_state;
    let user = &mut ctx.accounts.user_stake;
    require!(verify_mev_proof(&proof), CustomError::InvalidMEVProof);
    require!(
        clock.unix_timestamp - user.stake_timestamp >= state.cooldown_period,
        CustomError::StakePeriodTooShort
    );
    require!(
        clock.unix_timestamp - state.last_fee_deposit_time >= state.min_claim_delay,
        CustomError::ClaimTooSoon
    );
    let accumulated = (user.staked_amount as u128)
        .checked_mul(state.acc_reward_per_share as u128)
        .ok_or(CustomError::MathOverflow)?
        / REWARD_MULTIPLIER as u128;
    let pending_from_stake = accumulated.checked_sub(user.reward_debt as u128)
        .ok_or(CustomError::MathOverflow)?;
    let mut total_reward = user.pending_rewards.checked_add(pending_from_stake as u64)
        .ok_or(CustomError::MathOverflow)?;
    let staked_duration = clock.unix_timestamp - user.stake_timestamp;
    let time_multiplier = if staked_duration < THIRTY_DAYS { 100 }
                          else if staked_duration < NINETY_DAYS { 120 }
                          else { 150 };
    total_reward = total_reward.checked_mul(time_multiplier).ok_or(CustomError::MathOverflow)? / 100;
    total_reward = total_reward.checked_mul(state.utilization_multiplier).ok_or(CustomError::MathOverflow)? / 100;
    let rebate = calculate_rebate(user.trade_volume_7d);
    total_reward = total_reward.checked_mul(100 + rebate).ok_or(CustomError::MathOverflow)? / 100;
    require!(total_reward > 0, CustomError::NoRewards);
    user.pending_rewards = 0;
    user.reward_debt = (((user.staked_amount as u128)
        .checked_mul(state.acc_reward_per_share as u128)
        .ok_or(CustomError::MathOverflow)?)
        / REWARD_MULTIPLIER as u128)
        .try_into()
        .unwrap();
    token::transfer(
        CpiContext::new(
            ctx.accounts.token_program.to_account_info(),
            Transfer {
                from: ctx.accounts.reward_vault.to_account_info(),
                to: ctx.accounts.user_reward_token_account.to_account_info(),
                authority: ctx.accounts.vault_authority.to_account_info(),
            },
        )
        .with_signer(&[&[b"vault", &[ctx.bumps.vault_authority]]]),
        total_reward,
    )?;
    Ok(())
}

fn verify_mev_proof(proof: &str) -> bool {
    !proof.is_empty()
}

fn calculate_rebate(trade_volume: u64) -> u64 {
    if trade_volume <= 10_000 {
        0
    } else if trade_volume <= 100_000 {
        5
    } else if trade_volume <= 1_000_000 {
        10
    } else {
        15
    }
}

#[error_code]
pub enum CustomError {
    #[msg("Invalid pool type.")]
    InvalidPoolType,
    #[msg("Math overflow occurred.")]
    MathOverflow,
    #[msg("Insufficient staked amount.")]
    InsufficientStake,
    #[msg("Withdrawal attempts are too frequent.")]
    WithdrawalTooFrequent,
    #[msg("Invalid or missing MEV proof.")]
    InvalidMEVProof,
    #[msg("Stake period is too short for claiming rewards.")]
    StakePeriodTooShort,
    #[msg("Claim attempted too soon after a transaction fee deposit.")]
    ClaimTooSoon,
    #[msg("No rewards available to claim.")]
    NoRewards,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Default)]
pub struct PoolInfo {
    pub lockup_period: i64,
    pub apr_multiplier: u64,
    pub transaction_fee: u64,
}

#[account]
pub struct GlobalState {
    pub total_staked: u64,
    pub acc_reward_per_share: u64,
    pub token_mint: Pubkey,
    pub owner: Pubkey,
    pub governance: Pubkey,
    pub cooldown_period: i64,
    pub early_withdrawal_penalty: u64,
    pub min_withdraw_interval: i64,
    pub min_claim_delay: i64,
    pub insurance_fee_percent: u64,
    pub utilization_multiplier: u64,
    pub last_fee_deposit_time: i64,
    pub pool_info: [PoolInfo; 3],
    pub insurance_fund: u64,
}

#[account]
pub struct UserStake {
    pub staked_amount: u64,
    pub reward_debt: u64,
    pub pending_rewards: u64,
    pub stake_timestamp: i64,
    pub last_withdrawal_time: i64,
    pub pool_type: u8,
    pub trade_volume_7d: u64,
}

#[account]
pub struct LPUserStake {
    pub staked_amount: u64,
    pub reward_debt: u64,
    pub pending_rewards: u64,
    pub stake_timestamp: i64,
    pub last_withdrawal_time: i64,
    pub trade_volume_7d: u64,
}

#[account]
pub struct Proposal {
    pub proposal_id: u64,
    pub proposer: Pubkey,
    pub proposal_data: String,
    pub snapshot_timestamp: i64,
    pub vote_count: u64,
    pub executed: bool,
}

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
    /// CHECK: PDA authority.
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
    /// CHECK: PDA authority.
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
    /// CHECK: PDA authority.
    #[account(seeds = [b"vault"], bump)]
    pub vault_authority: AccountInfo<'info>,
    #[account(mut)]
    pub user_token_account: Account<'info, TokenAccount>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct LPStake<'info> {
    #[account(mut)]
    pub global_state: Account<'info, GlobalState>,
    #[account(mut)]
    pub lp_user_stake: Account<'info, LPUserStake>,
    #[account(mut)]
    pub user_lp_token_account: Account<'info, TokenAccount>,
    #[account(mut)]
    pub lp_staking_vault: Account<'info, TokenAccount>,
    pub user: Signer<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct LPUnstake<'info> {
    #[account(mut)]
    pub global_state: Account<'info, GlobalState>,
    #[account(mut)]
    pub lp_user_stake: Account<'info, LPUserStake>,
    #[account(mut)]
    pub lp_staking_vault: Account<'info, TokenAccount>,
    #[account(mut)]
    pub user_lp_token_account: Account<'info, TokenAccount>,
    /// CHECK: PDA authority.
    #[account(seeds = [b"vault"], bump)]
    pub vault_authority: AccountInfo<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct LPClaimRewards<'info> {
    #[account(mut)]
    pub global_state: Account<'info, GlobalState>,
    #[account(mut)]
    pub lp_user_stake: Account<'info, LPUserStake>,
    #[account(mut)]
    pub reward_vault: Account<'info, TokenAccount>,
    #[account(mut)]
    pub user_reward_token_account: Account<'info, TokenAccount>,
    /// CHECK: PDA authority.
    #[account(seeds = [b"vault"], bump)]
    pub vault_authority: AccountInfo<'info>,
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct SubmitProposal<'info> {
    #[account(init, payer = proposer, space = 600)]
    pub proposal: Account<'info, Proposal>,
    #[account(mut)]
    pub proposer: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct VoteProposal<'info> {
    #[account(mut)]
    pub proposal: Account<'info, Proposal>,
    #[account(mut)]
    pub user_stake: Account<'info, UserStake>,
    pub voter: Signer<'info>,
}

