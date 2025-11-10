use anchor_lang::prelude::*;
use anchor_lang::solana_program::hash::hash;
use anchor_spl::{
    associated_token::AssociatedToken,
    token::{self, Mint, Token, TokenAccount, Transfer},
};

declare_id!("5ggd1t1UMGWHyiTGKmSgftmWAqtJnt8RmBh447s3DN8");

#[program]
pub mod race_vault {
    use super::*;

    pub fn initialize(ctx: Context<Initialize>) -> Result<()> {
        let cfg = &mut ctx.accounts.config;
        cfg.authority = ctx.accounts.authority.key();
        cfg.mint = ctx.accounts.mint.key();
        cfg.vault_signer_bump = ctx.bumps.vault_signer;
        cfg.paused = false;
        
        // Initialize global payout registry
        let global_registry = &mut ctx.accounts.global_payout_registry;
        global_registry.total_pending = 0;
        global_registry.total_claimed = 0;
        global_registry.total_payout_count = 0;
        global_registry.total_recipient_count = 0;
        global_registry.last_updated = Clock::get()?.unix_timestamp;
        
        Ok(())
    }

    pub fn deposit(ctx: Context<Deposit>, amount: u64) -> Result<()> {
        require!(amount > 0, VaultError::ZeroAmount);

        let cpi_accounts = Transfer {
            from: ctx.accounts.depositor_token.to_account_info(),
            to: ctx.accounts.vault_token.to_account_info(),
            authority: ctx.accounts.depositor.to_account_info(),
        };
        let cpi_ctx = CpiContext::new(ctx.accounts.token_program.to_account_info(), cpi_accounts);
        token::transfer(cpi_ctx, amount)?;
        Ok(())
    }

    /// Reconcile: reads actual vault balance and emits event for off-chain tracking.
    /// Use this to sync tokens sent directly to vault (not through deposit).
    pub fn reconcile(ctx: Context<Reconcile>) -> Result<()> {
        let vault_balance = ctx.accounts.vault_token.amount;

        emit!(ReconcileEvent {
            vault_token: ctx.accounts.vault_token.key(),
            balance: vault_balance,
            timestamp: Clock::get()?.unix_timestamp,
        });

        Ok(())
    }

    /// Register a payout (admin-only) - creates a pending payout receipt
    /// Client must provide hash(race_id) - validated on-chain for security
    pub fn register_payout(
        ctx: Context<RegisterPayout>,
        race_id: String,
        race_id_hash: [u8; 32],  // Client provides hash, we validate
        points: u64,
        amount: u64,
    ) -> Result<()> {
        let config = &ctx.accounts.config;
        
        // Check if paused
        require!(!config.paused, VaultError::ProgramPaused);
        require!(amount > 0, VaultError::ZeroAmount);
        
        // Validate the hash matches the race_id
        let computed_hash = hash(race_id.as_bytes()).to_bytes();
        require!(
            computed_hash == race_id_hash,
            VaultError::InvalidRaceIdHash
        );

        // Write receipt (prevents double-registration)
        let receipt = &mut ctx.accounts.payout_receipt;
        receipt.race_id_hash = race_id_hash;
        receipt.recipient = ctx.accounts.recipient.key();
        receipt.points = points;
        receipt.amount = amount;
        receipt.timestamp = Clock::get()?.unix_timestamp;

        // Update payout registry
        let registry = &mut ctx.accounts.payout_registry;
        
        // Track if this is a new recipient for global stats
        let is_new_recipient = registry.recipient == Pubkey::default();
        
        // Initialize registry if this is the first payout
        if is_new_recipient {
            registry.recipient = ctx.accounts.recipient.key();
        }
        
        registry.total_pending = registry.total_pending.checked_add(amount).ok_or(VaultError::Overflow)?;
        registry.payout_count += 1;
        registry.last_updated = Clock::get()?.unix_timestamp;
        
        // Update global payout registry
        let global_registry = &mut ctx.accounts.global_payout_registry;
        global_registry.total_pending = global_registry.total_pending.checked_add(amount).ok_or(VaultError::Overflow)?;
        global_registry.total_payout_count += 1;
        if is_new_recipient {
            global_registry.total_recipient_count += 1;
        }
        global_registry.last_updated = Clock::get()?.unix_timestamp;

        // Emit for off-chain indexing
        emit!(PayoutRegisteredEvent {
            race_id,  // Original CUID for off-chain indexing
            race_id_hash,  // Hash for on-chain lookups
            recipient: receipt.recipient,
            points,
            amount,
            timestamp: receipt.timestamp,
        });

        Ok(())
    }

    /// Claim all pending payouts for a wallet (anyone can call)
    /// Transfers all pending payouts to the recipient's token account
    pub fn claim_pending_payouts(
        ctx: Context<ClaimPendingPayouts>,
        recipient: Pubkey,
    ) -> Result<()> {
        let config = &ctx.accounts.config;
        
        // Check if paused
        require!(!config.paused, VaultError::ProgramPaused);
        
        // Validate recipient matches the account
        require!(
            recipient == ctx.accounts.recipient.key(),
            VaultError::InvalidRecipient
        );

        // Get pending payouts from registry
        let registry = &mut ctx.accounts.payout_registry;
        
        // Check if there are pending payouts
        require!(registry.total_pending > 0, VaultError::NoPendingPayouts);
        
        // Check vault has sufficient balance
        require!(
            ctx.accounts.vault_token.amount >= registry.total_pending,
            VaultError::InsufficientBalance
        );

        let total_pending = registry.total_pending;
        let payout_count = registry.payout_count;

        // Update registry - move pending to claimed
        registry.total_pending = 0;
        registry.total_claimed = registry.total_claimed.checked_add(total_pending).ok_or(VaultError::Overflow)?;
        registry.last_updated = Clock::get()?.unix_timestamp;
        
        // Update global registry - move pending to claimed
        let global_registry = &mut ctx.accounts.global_payout_registry;
        global_registry.total_pending = global_registry.total_pending.checked_sub(total_pending).ok_or(VaultError::Overflow)?;
        global_registry.total_claimed = global_registry.total_claimed.checked_add(total_pending).ok_or(VaultError::Overflow)?;
        global_registry.last_updated = Clock::get()?.unix_timestamp;

        // Transfer from vault (PDA signer) to recipient ATA
        let cfg = &ctx.accounts.config;

        // PDA signer seeds
        let config_key = cfg.key();
        let seeds: &[&[u8]] = &[
            b"vault_signer",
            config_key.as_ref(),
            &[cfg.vault_signer_bump],
        ];
        
        let signer = &[seeds];

        let cpi_accounts = Transfer {
            from: ctx.accounts.vault_token.to_account_info(),
            to: ctx.accounts.recipient_token.to_account_info(),
            authority: ctx.accounts.vault_signer.to_account_info(),
        };
        let cpi_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            cpi_accounts,
            signer,
        );
        token::transfer(cpi_ctx, total_pending)?;

        // Emit for off-chain indexing
        emit!(PayoutsClaimedEvent {
            recipient: registry.recipient,
            total_amount: total_pending,
            payout_count,
            timestamp: Clock::get()?.unix_timestamp,
        });

        Ok(())
    }

    /// Transfer authority to new admin (current authority only)
    pub fn transfer_authority(
        ctx: Context<TransferAuthority>,
        new_authority: Pubkey,
    ) -> Result<()> {
        let config = &mut ctx.accounts.config;
        let old_authority = config.authority;
        config.authority = new_authority;

        emit!(AuthorityTransferEvent {
            old_authority,
            new_authority,
            timestamp: Clock::get()?.unix_timestamp,
        });

        Ok(())
    }

    /// Update config parameters (admin only)
    pub fn update_config(
        ctx: Context<UpdateConfig>,
        paused: Option<bool>,
    ) -> Result<()> {
        let config = &mut ctx.accounts.config;

        if let Some(pause_state) = paused {
            config.paused = pause_state;
        }

        emit!(ConfigUpdateEvent {
            paused: config.paused,
            timestamp: Clock::get()?.unix_timestamp,
        });

        Ok(())
    }

    /// Close config account (admin only) - for reinitialization or cleanup
    /// NOTE: This only closes the config account, not the vault token account
    /// Tokens remain safe in the vault token account
    pub fn close(ctx: Context<Close>) -> Result<()> {
        emit!(ConfigCloseEvent {
            authority: ctx.accounts.authority.key(),
            timestamp: Clock::get()?.unix_timestamp,
        });
        Ok(())
    }

    /// Register referral bonus (admin only)
    /// Creates a referral bonus record for a specific race, referrer, and referee
    pub fn register_referral_bonus(
        ctx: Context<RegisterReferralBonus>,
        race_id: String,
        referrer: Pubkey,
        referee: Pubkey,
        amount: u64,
    ) -> Result<()> {
        let config = &ctx.accounts.config;
        
        // Check if paused
        require!(!config.paused, VaultError::ProgramPaused);
        require!(amount > 0, VaultError::ZeroAmount);
        
        // Validate race_id length (Solana PDA seeds max 32 bytes)
        require!(race_id.len() <= 32, VaultError::RaceIdTooLong);
        
        // Prevent self-referrals
        require!(referrer != referee, VaultError::SelfReferralNotAllowed);

        // Create referral bonus record
        let referral_bonus = &mut ctx.accounts.referral_bonus;
        referral_bonus.race_id = race_id;
        referral_bonus.referrer = referrer;
        referral_bonus.referee = referee;
        referral_bonus.amount = amount;
        referral_bonus.claimed = false;
        referral_bonus.timestamp = Clock::get()?.unix_timestamp;

        // Update referrer registry
        let registry = &mut ctx.accounts.referrer_registry;
        
        // Initialize registry if this is the first bonus (H-01 fix)
        if registry.referrer == Pubkey::default() {
            registry.referrer = referrer;
        }
        
        registry.total_pending = registry.total_pending.checked_add(amount).ok_or(VaultError::Overflow)?;
        registry.bonus_count += 1;
        registry.last_updated = Clock::get()?.unix_timestamp;

        // Emit event for off-chain tracking
        emit!(ReferralBonusRegisteredEvent {
            race_id: referral_bonus.race_id.clone(),
            referrer: referral_bonus.referrer,
            referee: referral_bonus.referee,
            amount: referral_bonus.amount,
            timestamp: referral_bonus.timestamp,
        });

        Ok(())
    }


    /// Claim all pending referral bonuses for a referrer using registry
    /// This method reads the registry to get the total pending amount and transfers it
    pub fn claim_pending_bonuses(ctx: Context<ClaimPendingBonuses>) -> Result<()> {
        let config = &ctx.accounts.config;
        
        // Check if paused
        require!(!config.paused, VaultError::ProgramPaused);
        
        let registry = &mut ctx.accounts.referrer_registry;
        
        // Check if there are pending bonuses
        require!(registry.total_pending > 0, VaultError::NoPendingBonuses);
        
        // Check vault has sufficient balance
        require!(
            ctx.accounts.vault_token.amount >= registry.total_pending,
            VaultError::InsufficientBalance
        );

        let amount_to_claim = registry.total_pending;
        let bonus_count = registry.bonus_count;

        // Update registry
        registry.total_pending = 0;
        registry.total_claimed = registry.total_claimed.checked_add(amount_to_claim).ok_or(VaultError::Overflow)?;
        registry.last_updated = Clock::get()?.unix_timestamp;

        // Transfer tokens to referrer
        let cfg = &ctx.accounts.config;
        let config_key = cfg.key();
        let seeds: &[&[u8]] = &[
            b"vault_signer",
            config_key.as_ref(),
            &[cfg.vault_signer_bump],
        ];
        let signer = &[seeds];

        let cpi_accounts = Transfer {
            from: ctx.accounts.vault_token.to_account_info(),
            to: ctx.accounts.referrer_token.to_account_info(),
            authority: ctx.accounts.vault_signer.to_account_info(),
        };
        let cpi_ctx = CpiContext::new_with_signer(
            ctx.accounts.token_program.to_account_info(),
            cpi_accounts,
            signer,
        );
        token::transfer(cpi_ctx, amount_to_claim)?;

        // Emit event
        emit!(PendingBonusesClaimedEvent {
            referrer: registry.referrer,
            amount: amount_to_claim,
            bonus_count,
            timestamp: Clock::get()?.unix_timestamp,
        });

        Ok(())
    }

    /// Get pending bonuses for a referrer
    /// This is a true read function that returns data directly
    pub fn get_pending_bonuses(ctx: Context<GetPendingBonuses>) -> Result<ReferrerRegistry> {
        let registry = &ctx.accounts.referrer_registry;
        
        // Return the registry data directly
        Ok(ReferrerRegistry {
            referrer: registry.referrer,
            total_pending: registry.total_pending,
            total_claimed: registry.total_claimed,
            bonus_count: registry.bonus_count,
            last_updated: registry.last_updated,
        })
    }

    /// Get all bonuses (pending and claimed) for a referrer
    /// This is a true read function that returns data directly
    pub fn get_all_bonuses(ctx: Context<GetAllBonuses>) -> Result<ReferrerRegistry> {
        let registry = &ctx.accounts.referrer_registry;
        
        // Return the registry data directly
        Ok(ReferrerRegistry {
            referrer: registry.referrer,
            total_pending: registry.total_pending,
            total_claimed: registry.total_claimed,
            bonus_count: registry.bonus_count,
            last_updated: registry.last_updated,
        })
    }

    /// Get pending payouts for a recipient
    /// This is a true read function that returns data directly
    pub fn get_pending_payouts(ctx: Context<GetPendingPayouts>) -> Result<PayoutRegistry> {
        let registry = &ctx.accounts.payout_registry;
        
        // Return the registry data directly
        Ok(PayoutRegistry {
            recipient: registry.recipient,
            total_pending: registry.total_pending,
            total_claimed: registry.total_claimed,
            payout_count: registry.payout_count,
            last_updated: registry.last_updated,
        })
    }

    /// Get global payout statistics
    /// This is a true read function that returns data directly
    pub fn get_global_payout_stats(ctx: Context<GetGlobalPayoutStats>) -> Result<GlobalPayoutRegistry> {
        let registry = &ctx.accounts.global_payout_registry;
        
        // Return the global registry data directly
        Ok(GlobalPayoutRegistry {
            total_pending: registry.total_pending,
            total_claimed: registry.total_claimed,
            total_payout_count: registry.total_payout_count,
            total_recipient_count: registry.total_recipient_count,
            last_updated: registry.last_updated,
        })
    }
}

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(
        init,
        payer = authority,
        space = 8 + Config::SIZE,
        seeds = [b"config", mint.key().as_ref()],
        bump
    )]
    pub config: Account<'info, Config>,

    #[account(
        seeds = [b"vault_signer", config.key().as_ref()],
        bump
        // no init: it's just the signer PDA, no lamports
    )]
    /// CHECK: PDA signer, no data
    pub vault_signer: UncheckedAccount<'info>,

    pub mint: Account<'info, Mint>,

    /// Program-owned vault ATA (created if missing)
    #[account(
        init_if_needed,
        payer = authority,
        associated_token::mint = mint,
        associated_token::authority = vault_signer
    )]
    pub vault_token: Account<'info, TokenAccount>,

    /// Global payout registry (tracks all payouts across all recipients)
    #[account(
        init,
        payer = authority,
        space = 8 + GlobalPayoutRegistry::SIZE,
        seeds = [b"global_payout_registry", config.key().as_ref()],
        bump
    )]
    pub global_payout_registry: Account<'info, GlobalPayoutRegistry>,

    #[account(mut)]
    pub authority: Signer<'info>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct Deposit<'info> {
    #[account(
        seeds = [b"config", mint.key().as_ref()],
        bump
    )]
    pub config: Account<'info, Config>,

    /// PDA signer (authority of the vault token)
    #[account(
        seeds = [b"vault_signer", config.key().as_ref()],
        bump = config.vault_signer_bump
    )]
    /// CHECK: PDA signer
    pub vault_signer: UncheckedAccount<'info>,

    pub mint: Account<'info, Mint>,

    #[account(
        mut,
        associated_token::mint = mint,
        associated_token::authority = vault_signer
    )]
    pub vault_token: Account<'info, TokenAccount>,

    #[account(mut)]
    pub depositor: Signer<'info>,

    #[account(
        mut,
        associated_token::mint = mint,
        associated_token::authority = depositor
    )]
    pub depositor_token: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
}

#[derive(Accounts)]
#[instruction(race_id: String, race_id_hash: [u8; 32])]
pub struct RegisterPayout<'info> {
    #[account(
        seeds = [b"config", mint.key().as_ref()],
        bump,
        has_one = authority,
        has_one = mint
    )]
    pub config: Account<'info, Config>,

    /// Admin authority (only admin can register payouts)
    #[account(mut)]
    pub authority: Signer<'info>,

    pub mint: Account<'info, Mint>,

    /// Recipient wallet to receive payout
    /// CHECK: validated by ATA derivation below
    pub recipient: UncheckedAccount<'info>,

    /// One receipt per (race_id_hash, recipient). Prevents replays.
    #[account(
        init,
        payer = authority,
        space = 8 + PayoutReceipt::SIZE,
        seeds = [
            b"receipt", 
            config.key().as_ref(), 
            &race_id_hash,
            recipient.key().as_ref()
        ],
        bump
    )]
    pub payout_receipt: Account<'info, PayoutReceipt>,

    /// Payout registry (tracks total pending payouts for this recipient)
    #[account(
        init_if_needed,
        payer = authority,
        space = 8 + PayoutRegistry::SIZE,
        seeds = [b"payout_registry", config.key().as_ref(), recipient.key().as_ref()],
        bump
    )]
    pub payout_registry: Account<'info, PayoutRegistry>,

    /// Global payout registry (tracks all payouts)
    #[account(
        mut,
        seeds = [b"global_payout_registry", config.key().as_ref()],
        bump
    )]
    pub global_payout_registry: Account<'info, GlobalPayoutRegistry>,

    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
#[instruction(recipient: Pubkey)]
pub struct ClaimPendingPayouts<'info> {
    #[account(
        seeds = [b"config", mint.key().as_ref()],
        bump,
        has_one = mint
    )]
    pub config: Account<'info, Config>,

    /// PDA signer owning the vault ATA
    #[account(
        seeds = [b"vault_signer", config.key().as_ref()],
        bump = config.vault_signer_bump
    )]
    /// CHECK: PDA signer
    pub vault_signer: UncheckedAccount<'info>,

    pub mint: Account<'info, Mint>,

    #[account(
        mut,
        associated_token::mint = mint,
        associated_token::authority = vault_signer
    )]
    pub vault_token: Account<'info, TokenAccount>,

    /// Recipient wallet to receive payout
    /// CHECK: validated by constraint below
    pub recipient: UncheckedAccount<'info>,

    /// Recipient ATA (auto-created if needed)
    #[account(
        init_if_needed,
        payer = payer,
        associated_token::mint = mint,
        associated_token::authority = recipient
    )]
    pub recipient_token: Account<'info, TokenAccount>,

    /// Payout registry (tracks total pending payouts for this recipient)
    #[account(
        mut,
        constraint = payout_registry.recipient == recipient.key()
    )]
    pub payout_registry: Account<'info, PayoutRegistry>,

    /// Global payout registry (tracks all payouts)
    #[account(
        mut,
        seeds = [b"global_payout_registry", config.key().as_ref()],
        bump
    )]
    pub global_payout_registry: Account<'info, GlobalPayoutRegistry>,

    /// Payer for transaction fees (can be anyone)
    #[account(mut)]
    pub payer: Signer<'info>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct Reconcile<'info> {
    #[account(
        seeds = [b"config", mint.key().as_ref()],
        bump,
        has_one = authority,
        has_one = mint
    )]
    pub config: Account<'info, Config>,

    /// Only authority can reconcile
    pub authority: Signer<'info>,

    #[account(
        seeds = [b"vault_signer", config.key().as_ref()],
        bump = config.vault_signer_bump
    )]
    /// CHECK: PDA signer
    pub vault_signer: UncheckedAccount<'info>,

    pub mint: Account<'info, Mint>,

    #[account(
        associated_token::mint = mint,
        associated_token::authority = vault_signer
    )]
    pub vault_token: Account<'info, TokenAccount>,
}

#[derive(Accounts)]
pub struct TransferAuthority<'info> {
    #[account(
        mut,
        seeds = [b"config", mint.key().as_ref()],
        bump,
        has_one = authority
    )]
    pub config: Account<'info, Config>,

    pub authority: Signer<'info>,

    pub mint: Account<'info, Mint>,
}

#[derive(Accounts)]
pub struct UpdateConfig<'info> {
    #[account(
        mut,
        seeds = [b"config", mint.key().as_ref()],
        bump,
        has_one = authority
    )]
    pub config: Account<'info, Config>,

    pub authority: Signer<'info>,

    pub mint: Account<'info, Mint>,
}

#[derive(Accounts)]
pub struct Close<'info> {
    #[account(
        mut,
        seeds = [b"config", mint.key().as_ref()],
        bump,
        has_one = authority,
        close = authority
    )]
    pub config: Account<'info, Config>,

    pub mint: Account<'info, Mint>,

    #[account(mut)]
    pub authority: Signer<'info>,
}

#[derive(Accounts)]
#[instruction(race_id: String, referrer: Pubkey, referee: Pubkey)]
pub struct RegisterReferralBonus<'info> {
    #[account(
        seeds = [b"config", mint.key().as_ref()],
        bump,
        has_one = authority
    )]
    pub config: Account<'info, Config>,

    /// Admin authority (only admin can register referral bonuses)
    #[account(mut)]
    pub authority: Signer<'info>,

    pub mint: Account<'info, Mint>,

    /// Referral bonus account (unique per race_id + referrer + referee combination)
    #[account(
        init,
        payer = authority,
        space = 8 + ReferralBonus::SIZE,
        seeds = [
            b"referral_bonus",
            config.key().as_ref(),
            race_id.as_bytes(),
            referrer.as_ref(),
            referee.as_ref()
        ],
        bump
    )]
    pub referral_bonus: Account<'info, ReferralBonus>,

    /// Referrer registry (tracks total pending bonuses for this referrer)
    #[account(
        init_if_needed,
        payer = authority,
        space = 8 + ReferrerRegistry::SIZE,
        seeds = [b"referrer_registry", config.key().as_ref(), referrer.as_ref()],
        bump
    )]
    pub referrer_registry: Account<'info, ReferrerRegistry>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub rent: Sysvar<'info, Rent>,
}


#[derive(Accounts)]
pub struct ClaimPendingBonuses<'info> {
    #[account(
        seeds = [b"config", mint.key().as_ref()],
        bump,
        has_one = mint
    )]
    pub config: Account<'info, Config>,

    /// PDA signer owning the vault ATA
    #[account(
        seeds = [b"vault_signer", config.key().as_ref()],
        bump = config.vault_signer_bump
    )]
    /// CHECK: PDA signer
    pub vault_signer: UncheckedAccount<'info>,

    pub mint: Account<'info, Mint>,

    #[account(
        mut,
        associated_token::mint = mint,
        associated_token::authority = vault_signer
    )]
    pub vault_token: Account<'info, TokenAccount>,

    /// Referrer registry (tracks total pending bonuses for this referrer)
    #[account(
        mut,
        constraint = referrer_registry.referrer == referrer.key()
    )]
    pub referrer_registry: Account<'info, ReferrerRegistry>,

    /// Referrer address (must match the registry)
    /// CHECK: validated by constraint below
    pub referrer: UncheckedAccount<'info>,

    /// Referrer's token account (auto-created if needed)
    #[account(
        init_if_needed,
        payer = payer,
        associated_token::mint = mint,
        associated_token::authority = referrer
    )]
    pub referrer_token: Account<'info, TokenAccount>,

    /// Payer for transaction fees (can be anyone)
    #[account(mut)]
    pub payer: Signer<'info>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub rent: Sysvar<'info, Rent>,
}

#[derive(Accounts)]
pub struct GetPendingBonuses<'info> {
    #[account(
        seeds = [b"config", mint.key().as_ref()],
        bump,
        has_one = mint
    )]
    pub config: Account<'info, Config>,

    pub mint: Account<'info, Mint>,

    /// Referrer registry (tracks total pending bonuses for this referrer)
    #[account(
        constraint = referrer_registry.referrer == referrer.key()
    )]
    pub referrer_registry: Account<'info, ReferrerRegistry>,

    /// Referrer address (must match the registry)
    /// CHECK: validated by constraint below
    pub referrer: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct GetAllBonuses<'info> {
    #[account(
        seeds = [b"config", mint.key().as_ref()],
        bump,
        has_one = mint
    )]
    pub config: Account<'info, Config>,

    pub mint: Account<'info, Mint>,

    /// Referrer registry (tracks total pending bonuses for this referrer)
    #[account(
        constraint = referrer_registry.referrer == referrer.key()
    )]
    pub referrer_registry: Account<'info, ReferrerRegistry>,

    /// Referrer address (must match the registry)
    /// CHECK: validated by constraint below
    pub referrer: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct GetPendingPayouts<'info> {
    #[account(
        seeds = [b"config", mint.key().as_ref()],
        bump,
        has_one = mint
    )]
    pub config: Account<'info, Config>,

    pub mint: Account<'info, Mint>,

    /// Payout registry (tracks total pending payouts for this recipient)
    #[account(
        constraint = payout_registry.recipient == recipient.key()
    )]
    pub payout_registry: Account<'info, PayoutRegistry>,

    /// Recipient address (must match the registry)
    /// CHECK: validated by constraint below
    pub recipient: UncheckedAccount<'info>,
}

#[derive(Accounts)]
pub struct GetGlobalPayoutStats<'info> {
    #[account(
        seeds = [b"config", mint.key().as_ref()],
        bump,
        has_one = mint
    )]
    pub config: Account<'info, Config>,

    pub mint: Account<'info, Mint>,

    /// Global payout registry (tracks all payouts)
    #[account(
        seeds = [b"global_payout_registry", config.key().as_ref()],
        bump
    )]
    pub global_payout_registry: Account<'info, GlobalPayoutRegistry>,
}

#[account]
pub struct Config {
    pub authority: Pubkey,
    pub mint: Pubkey,
    pub vault_signer_bump: u8,
    pub paused: bool,
}
impl Config {
    pub const SIZE: usize = 32 + 32 + 1 + 1;  // 66 bytes
}

#[account]
pub struct PayoutReceipt {
    pub race_id_hash: [u8; 32],  // Hash of the race_id (CUID)
    pub recipient: Pubkey,
    pub points: u64,
    pub amount: u64,
    pub timestamp: i64,
}
impl PayoutReceipt {
    pub const SIZE: usize = 32 + 32 + 8 + 8 + 8;  // 88 bytes
}

#[account]
pub struct ReferralBonus {
    pub race_id: String,
    pub referrer: Pubkey,
    pub referee: Pubkey,
    pub amount: u64,
    pub claimed: bool,
    pub timestamp: i64,
}
impl ReferralBonus {
    pub const SIZE: usize = 4 + 32 + 32 + 32 + 8 + 1 + 8;  // 117 bytes (4 for string length)
}

#[account]
pub struct ReferrerRegistry {
    pub referrer: Pubkey,
    pub total_pending: u64,
    pub total_claimed: u64,
    pub bonus_count: u32,
    pub last_updated: i64,
}
impl ReferrerRegistry {
    pub const SIZE: usize = 32 + 8 + 8 + 4 + 8;  // 60 bytes
}

#[account]
pub struct PayoutRegistry {
    pub recipient: Pubkey,
    pub total_pending: u64,
    pub total_claimed: u64,
    pub payout_count: u32,
    pub last_updated: i64,
}
impl PayoutRegistry {
    pub const SIZE: usize = 32 + 8 + 8 + 4 + 8;  // 60 bytes
}

#[account]
pub struct GlobalPayoutRegistry {
    pub total_pending: u64,
    pub total_claimed: u64,
    pub total_payout_count: u32,
    pub total_recipient_count: u32,
    pub last_updated: i64,
}
impl GlobalPayoutRegistry {
    pub const SIZE: usize = 8 + 8 + 4 + 4 + 8;  // 32 bytes
}


#[event]
pub struct ReconcileEvent {
    pub vault_token: Pubkey,
    pub balance: u64,
    pub timestamp: i64,
}

#[event]
pub struct AuthorityTransferEvent {
    pub old_authority: Pubkey,
    pub new_authority: Pubkey,
    pub timestamp: i64,
}

#[event]
pub struct ConfigUpdateEvent {
    pub paused: bool,
    pub timestamp: i64,
}

#[event]
pub struct ConfigCloseEvent {
    pub authority: Pubkey,
    pub timestamp: i64,
}

#[event]
pub struct ReferralBonusRegisteredEvent {
    pub race_id: String,
    pub referrer: Pubkey,
    pub referee: Pubkey,
    pub amount: u64,
    pub timestamp: i64,
}


#[event]
pub struct PendingBonusesClaimedEvent {
    pub referrer: Pubkey,
    pub amount: u64,
    pub bonus_count: u32,
    pub timestamp: i64,
}

#[event]
pub struct PayoutRegisteredEvent {
    pub race_id: String,
    pub race_id_hash: [u8; 32],
    pub recipient: Pubkey,
    pub points: u64,
    pub amount: u64,
    pub timestamp: i64,
}

#[event]
pub struct PayoutsClaimedEvent {
    pub recipient: Pubkey,
    pub total_amount: u64,
    pub payout_count: u32,
    pub timestamp: i64,
}


#[error_code]
pub enum VaultError {
    #[msg("Amount must be > 0")]
    ZeroAmount,
    #[msg("Insufficient balance in vault")]
    InsufficientBalance,
    #[msg("Program is paused")]
    ProgramPaused,
    #[msg("Invalid race_id hash - hash doesn't match race_id")]
    InvalidRaceIdHash,
    #[msg("Arithmetic overflow")]
    Overflow,
    #[msg("No pending bonuses to claim")]
    NoPendingBonuses,
    #[msg("No pending payouts to claim")]
    NoPendingPayouts,
    #[msg("Payout already exists")]
    PayoutAlreadyExists,
    #[msg("Invalid recipient")]
    InvalidRecipient,
    #[msg("Race ID exceeds maximum length of 32 bytes")]
    RaceIdTooLong,
    #[msg("Self-referrals are not allowed")]
    SelfReferralNotAllowed,
}
