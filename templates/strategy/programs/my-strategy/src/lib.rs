//! A Pipeline strategy.
//!
//! A strategy is a program that holds liquidity and decides prices. Pipeline's router
//! sends trades to it, makes sure the trader gets at least their minimum, charges
//! Pipeline's 0.25% fee and publishes a standard swap event, so every market on this
//! program can be found, quoted, traded and charted by anything built on Pipeline.
//!
//! This file implements the five Pipeline instructions plus one custom action:
//!
//! | Instruction                 | Called by                       | Does                              |
//! |-----------------------------|---------------------------------|-----------------------------------|
//! | `pipeline_init_market`      | the router, from `create_market` | creates a pool and its vaults     |
//! | `pipeline_market_info`      | the router, at market creation  | reports mints, vaults, exact-out  |
//! | `pipeline_resolve_accounts` | quoters and SDKs, in simulation | lists the accounts a swap needs   |
//! | `pipeline_quote`            | quoters and SDKs, in simulation | prices a trade, read-only         |
//! | `pipeline_swap`             | the router, from `swap`         | takes the input, pays the output  |
//! | `deposit`                   | liquidity providers, directly   | adds liquidity                    |
//!
//! Prices come from one function, `pricing::quote`, which is the part to change. The rest
//! is plumbing that keeps quotes and swaps identical and the pool safe to route through.
//! `docs/STRATEGY-GUIDE.md` in the Pipeline repository explains every rule referred to
//! below.
//!
//! Each pool trades two mints and lives at `["pool", mint_a, mint_b, fee_bps]`, so a pool
//! with bad parameters can never occupy the address of a good one. Its vaults are the
//! pool's associated token accounts. The pool records its reserves itself rather than
//! reading vault balances, so tokens sent straight to a vault are a donation that never
//! moves the price.

use anchor_lang::prelude::*;
use anchor_lang::solana_program::instruction::{AccountMeta, Instruction};
use anchor_lang::solana_program::program::{invoke_signed, set_return_data};
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token_interface::{Mint, TokenAccount, TokenInterface};
use pipeline_strategy_interface::{self as psi, TradeMode};

pub mod pricing;

// Replaced with your program's address by `scripts/new-strategy.sh`.
declare_id!("MyStrategy111111111111111111111111111111111");

#[program]
pub mod my_strategy {
    use super::*;

    /// Creates a pool. The router calls this from `create_market`, which anyone can call;
    /// `data` is a Borsh-encoded [`InitParams`] chosen by the market creator.
    ///
    /// The creator's own signature never reaches this instruction, so it can't take
    /// tokens from them. Liquidity comes later from `deposit`, which they sign directly
    /// (usually in the same transaction).
    pub fn pipeline_init_market(ctx: Context<InitMarket>, data: Vec<u8>) -> Result<()> {
        let params = InitParams::try_from_slice(&data).map_err(|_| StrategyError::BadInitData)?;
        require!(
            (params.fee_bps as u64) < psi::BPS_DENOMINATOR,
            StrategyError::BadInitData
        );
        require_keys_neq!(
            ctx.accounts.mint_a.key(),
            ctx.accounts.mint_b.key(),
            StrategyError::BadInitData
        );

        // Remember which router market may call `pipeline_swap` on this pool (rule 4:
        // check the market signer). Only the router can sign as this PDA.
        let pool_key = ctx.accounts.pool.key();
        let expected_signer = pipeline_market_signer(&pool_key);
        require_keys_eq!(
            ctx.accounts.market_signer.key(),
            expected_signer,
            StrategyError::NotPipeline
        );

        let pool = &mut ctx.accounts.pool;
        pool.mint_a = ctx.accounts.mint_a.key();
        pool.mint_b = ctx.accounts.mint_b.key();
        pool.vault_a = ctx.accounts.vault_a.key();
        pool.vault_b = ctx.accounts.vault_b.key();
        pool.reserve_a = 0;
        pool.reserve_b = 0;
        pool.fee_bps = params.fee_bps;
        pool.market_signer = expected_signer;
        pool.bump = ctx.bumps.pool;
        Ok(())
    }

    /// Reports the pool's mints, vaults and exact-out support. The router records them
    /// when the market is created; integrators read them from the router's Market.
    ///
    /// `Account<Pool>` checks that the account is owned by this program and is a `Pool`.
    /// That matters: anyone can call the router's `register_existing_market` on any
    /// account this program owns, so this must refuse anything that isn't a real pool.
    pub fn pipeline_market_info(ctx: Context<MarketInfoAccounts>) -> Result<()> {
        let pool = &ctx.accounts.pool;
        let info = psi::MarketInfo {
            mints: vec![pool.mint_a.to_bytes(), pool.mint_b.to_bytes()],
            vaults: vec![pool.vault_a.to_bytes(), pool.vault_b.to_bytes()],
            supports_exact_out: pricing::SUPPORTS_EXACT_OUT,
        };
        set_return_data(&borsh_encode(&info)?);
        Ok(())
    }

    /// Lists the strategy-specific accounts a swap needs, after the router's ten fixed
    /// trade accounts. Here that is just the output vault. If you need to read some
    /// accounts before you know the rest, return what you know with `complete: false`;
    /// you'll be called again with them appended (up to 4 rounds).
    pub fn pipeline_resolve_accounts(
        ctx: Context<ReadTrade>,
        request: psi::ResolveRequest,
    ) -> Result<()> {
        let pool = &ctx.accounts.pool;
        let (_, output_vault) = pool.vaults(&request.trade)?;
        let result = psi::ResolveResult {
            accounts: vec![psi::AccountRef {
                address: output_vault.to_bytes(),
                writable: true,
            }],
            complete: true,
        };
        set_return_data(&borsh_encode(&result)?);
        Ok(())
    }

    /// Prices a trade without changing anything. Runs in simulation, so nothing signs
    /// and the trader accounts may be placeholders: read only the pool.
    pub fn pipeline_quote(ctx: Context<ReadTrade>, request: psi::TradeRequest) -> Result<()> {
        let quote = pricing::quote(&ctx.accounts.pool, &request)?;
        set_return_data(&borsh_encode(&quote)?);
        Ok(())
    }

    /// Settles a trade. The router has just moved the trader's input into the input
    /// vault; this pays the output to the trader and refunds any unused input.
    ///
    /// Remaining accounts: the output vault (as `pipeline_resolve_accounts` listed it),
    /// then any transfer-hook accounts, which are forwarded to every token transfer.
    pub fn pipeline_swap<'info>(
        ctx: Context<'info, SwapAccounts<'info>>,
        request: psi::SwapRequest,
    ) -> Result<()> {
        let pool = &ctx.accounts.pool;
        // Only this pool's router market may trade it, so every trade pays Pipeline's
        // fee and appears in Pipeline's events. Remove this check to also accept direct
        // calls (they pay no Pipeline fee).
        require_keys_eq!(
            ctx.accounts.market_signer.key(),
            pool.market_signer,
            StrategyError::NotPipeline
        );
        let trade = &request.trade;
        let (input_vault, output_vault) = pool.vaults(trade)?;
        require_keys_eq!(
            ctx.accounts.input_vault.key(),
            input_vault,
            StrategyError::WrongAccount
        );
        let output_vault_info = ctx
            .remaining_accounts
            .first()
            .ok_or(StrategyError::WrongAccount)?;
        require_keys_eq!(
            output_vault_info.key(),
            output_vault,
            StrategyError::WrongAccount
        );
        let hook_accounts = &ctx.remaining_accounts[1..];

        // Credit what actually arrived in the vault beyond the recorded reserve, capped at
        // what the router sent in this call. Measuring handles Token-2022 transfer fees;
        // the cap stops tokens sent to the vault earlier from counting as fee-free input.
        let (reserve_in, _) = pool.reserves(trade)?;
        let vault_balance = token_balance(&ctx.accounts.input_vault.to_account_info())?;
        let credited = psi::credited_input(vault_balance, reserve_in, request.amount_sent);

        // Price with the same function as `pipeline_quote`, so quotes and swaps agree.
        let priced = psi::TradeRequest {
            amount: match trade.mode {
                TradeMode::ExactIn => credited,
                TradeMode::ExactOut => trade.amount,
            },
            ..trade.clone()
        };
        let quote = pricing::quote(pool, &priced)?;
        require!(
            quote.amount_in <= credited,
            StrategyError::InsufficientInput
        );
        let refund = credited - quote.amount_in;

        // The pool PDA owns both vaults and signs for them.
        let fee_seed = pool.fee_bps.to_le_bytes();
        let bump = [pool.bump];
        let pool_seeds: &[&[u8]] = &[
            b"pool",
            pool.mint_a.as_ref(),
            pool.mint_b.as_ref(),
            &fee_seed,
            &bump,
        ];
        let pool_info = ctx.accounts.pool.to_account_info();
        if refund > 0 {
            transfer_checked(
                &ctx.accounts.input_token_program.to_account_info(),
                &ctx.accounts.input_vault.to_account_info(),
                &ctx.accounts.input_mint.to_account_info(),
                &ctx.accounts.trader_input.to_account_info(),
                &pool_info,
                hook_accounts,
                refund,
                ctx.accounts.input_mint.decimals,
                &[pool_seeds],
            )?;
        }
        if quote.amount_out > 0 {
            transfer_checked(
                &ctx.accounts.output_token_program.to_account_info(),
                output_vault_info,
                &ctx.accounts.output_mint.to_account_info(),
                &ctx.accounts.trader_output.to_account_info(),
                &pool_info,
                hook_accounts,
                quote.amount_out,
                ctx.accounts.output_mint.decimals,
                &[pool_seeds],
            )?;
        }

        // Move the reserves by exactly what the trade took in and paid out, never by
        // vault balances, so donations stay out of the price.
        let (reserve_in, reserve_out) = ctx.accounts.pool.reserves(trade)?;
        let new_reserve_in = reserve_in
            .checked_add(quote.amount_in)
            .ok_or(StrategyError::Overflow)?;
        let new_reserve_out = reserve_out
            .checked_sub(quote.amount_out)
            .ok_or(StrategyError::InsufficientLiquidity)?;
        ctx.accounts
            .pool
            .set_reserves(trade, new_reserve_in, new_reserve_out);
        Ok(())
    }

    /// Custom action: add liquidity. Liquidity providers call it directly, not through
    /// the router, and it pays no Pipeline fee. `metadata.json` describes it so desks can
    /// show a form for it.
    ///
    /// This starter pool has no LP shares and no withdrawal: anyone can deposit and
    /// nobody can take liquidity out. Add shares and a `withdraw` action before real
    /// users deposit.
    ///
    /// While either side is empty, a deposit must add both tokens, so a pool never trades
    /// with one side empty. Remaining accounts are transfer-hook accounts.
    pub fn deposit<'info>(
        ctx: Context<'info, Deposit<'info>>,
        amount_a: u64,
        amount_b: u64,
    ) -> Result<()> {
        let accounts = &ctx.accounts;
        let pool = &accounts.pool;
        if pool.reserve_a == 0 || pool.reserve_b == 0 {
            require!(amount_a > 0 && amount_b > 0, StrategyError::OneSidedDeposit);
        }
        // Credit what arrived (less than `amount` for transfer-fee tokens), so earlier
        // donations to the vaults stay out of the reserves.
        let mut arrived = [0u64; 2];
        let sides = [
            (
                &accounts.depositor_a,
                &accounts.mint_a,
                &accounts.vault_a,
                &accounts.token_program_a,
                amount_a,
            ),
            (
                &accounts.depositor_b,
                &accounts.mint_b,
                &accounts.vault_b,
                &accounts.token_program_b,
                amount_b,
            ),
        ];
        for (i, (from, mint, vault, token_program, amount)) in sides.into_iter().enumerate() {
            if amount == 0 {
                continue;
            }
            let before = token_balance(&vault.to_account_info())?;
            transfer_checked(
                &token_program.to_account_info(),
                &from.to_account_info(),
                &mint.to_account_info(),
                &vault.to_account_info(),
                &accounts.depositor.to_account_info(),
                ctx.remaining_accounts,
                amount,
                mint.decimals,
                &[],
            )?;
            arrived[i] = token_balance(&vault.to_account_info())? - before;
        }
        let pool = &mut ctx.accounts.pool;
        pool.reserve_a = pool
            .reserve_a
            .checked_add(arrived[0])
            .ok_or(StrategyError::Overflow)?;
        pool.reserve_b = pool
            .reserve_b
            .checked_add(arrived[1])
            .ok_or(StrategyError::Overflow)?;
        Ok(())
    }
}

/// The market creator's setup data: `create_market`'s `init_data`, Borsh-encoded.
#[derive(AnchorSerialize, AnchorDeserialize)]
pub struct InitParams {
    /// The pool's own trading fee, kept in the pool. Pipeline's fee is separate.
    pub fee_bps: u16,
}

/// A pool: the market state account the router records and passes to every call.
#[account]
#[derive(InitSpace)]
pub struct Pool {
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
    pub vault_a: Pubkey,
    pub vault_b: Pubkey,
    /// What the pool owns in each vault, as far as pricing is concerned.
    pub reserve_a: u64,
    pub reserve_b: u64,
    pub fee_bps: u16,
    /// The router's market PDA for this pool, the only caller allowed to swap.
    pub market_signer: Pubkey,
    pub bump: u8,
}

impl Pool {
    /// The input and output vaults of a trade. Mint index 0 is A and 1 is B.
    pub fn vaults(&self, trade: &psi::TradeRequest) -> Result<(Pubkey, Pubkey)> {
        match (trade.input_index, trade.output_index) {
            (0, 1) => Ok((self.vault_a, self.vault_b)),
            (1, 0) => Ok((self.vault_b, self.vault_a)),
            _ => err!(StrategyError::WrongAccount),
        }
    }

    /// The input and output reserves of a trade.
    pub fn reserves(&self, trade: &psi::TradeRequest) -> Result<(u64, u64)> {
        match (trade.input_index, trade.output_index) {
            (0, 1) => Ok((self.reserve_a, self.reserve_b)),
            (1, 0) => Ok((self.reserve_b, self.reserve_a)),
            _ => err!(StrategyError::WrongAccount),
        }
    }

    fn set_reserves(&mut self, trade: &psi::TradeRequest, reserve_in: u64, reserve_out: u64) {
        if trade.input_index == 0 {
            self.reserve_a = reserve_in;
            self.reserve_b = reserve_out;
        } else {
            self.reserve_b = reserve_in;
            self.reserve_a = reserve_out;
        }
    }
}

/// The router's market PDA for a pool of this program: `["market", program, pool]`
/// under the Pipeline program.
fn pipeline_market_signer(pool: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[psi::seeds::MARKET, crate::ID.as_ref(), pool.as_ref()],
        &Pubkey::new_from_array(psi::PIPELINE_PROGRAM_ID),
    )
    .0
}

fn borsh_encode<T: AnchorSerialize>(value: &T) -> Result<Vec<u8>> {
    let mut encoded = Vec::new();
    value.serialize(&mut encoded)?;
    Ok(encoded)
}

/// The amount field of an SPL Token or Token-2022 account.
fn token_balance(account: &AccountInfo) -> Result<u64> {
    let data = account.try_borrow_data()?;
    let amount = data
        .get(64..72)
        .ok_or(StrategyError::WrongAccount)?
        .try_into()
        .map_err(|_| StrategyError::WrongAccount)?;
    Ok(u64::from_le_bytes(amount))
}

/// `transfer_checked` that forwards transfer-hook accounts to the token program, so
/// Token-2022 mints with transfer hooks work.
#[allow(clippy::too_many_arguments)]
fn transfer_checked<'info>(
    token_program: &AccountInfo<'info>,
    from: &AccountInfo<'info>,
    mint: &AccountInfo<'info>,
    to: &AccountInfo<'info>,
    authority: &AccountInfo<'info>,
    hook_accounts: &[AccountInfo<'info>],
    amount: u64,
    decimals: u8,
    signer_seeds: &[&[&[u8]]],
) -> Result<()> {
    // SPL Token's `TransferChecked` instruction: tag 12, amount, decimals.
    let mut data = vec![12u8];
    data.extend_from_slice(&amount.to_le_bytes());
    data.push(decimals);
    let mut metas = vec![
        AccountMeta::new(from.key(), false),
        AccountMeta::new_readonly(mint.key(), false),
        AccountMeta::new(to.key(), false),
        AccountMeta::new_readonly(authority.key(), true),
    ];
    let mut infos = vec![from.clone(), mint.clone(), to.clone(), authority.clone()];
    for account in hook_accounts {
        let meta = if account.is_writable {
            AccountMeta::new(account.key(), false)
        } else {
            AccountMeta::new_readonly(account.key(), false)
        };
        metas.push(meta);
        infos.push(account.clone());
    }
    infos.push(token_program.clone());
    let instruction = Instruction {
        program_id: token_program.key(),
        accounts: metas,
        data,
    };
    invoke_signed(&instruction, &infos, signer_seeds)?;
    Ok(())
}

/// The fee part of a pool's address: the first two bytes of the init data, which are
/// the Borsh-encoded `fee_bps`. Malformed data is rejected by `pipeline_init_market`.
fn fee_seed(data: &[u8]) -> &[u8] {
    data.get(..2).unwrap_or(&[0, 0])
}

/// `pipeline_init_market` accounts: the router's four fixed accounts, then this
/// strategy's own, in the order the market creator passes them (see `metadata.json`).
#[derive(Accounts)]
#[instruction(data: Vec<u8>)]
pub struct InitMarket<'info> {
    /// The router's market PDA for this pool.
    pub market_signer: Signer<'info>,
    #[account(
        init,
        payer = payer,
        space = 8 + Pool::INIT_SPACE,
        seeds = [b"pool", mint_a.key().as_ref(), mint_b.key().as_ref(), fee_seed(&data)],
        bump,
    )]
    pub pool: Account<'info, Pool>,
    /// A router PDA holding the SOL the creator allowed for setup. Pay rent from it; the
    /// router returns what is left to the creator.
    #[account(mut)]
    pub payer: Signer<'info>,
    pub system_program: Program<'info, System>,
    pub mint_a: InterfaceAccount<'info, Mint>,
    pub mint_b: InterfaceAccount<'info, Mint>,
    // `init_if_needed`: anyone can create an associated token account in advance, and
    // that must not block the pool.
    #[account(
        init_if_needed,
        payer = payer,
        associated_token::mint = mint_a,
        associated_token::authority = pool,
        associated_token::token_program = token_program_a,
    )]
    pub vault_a: InterfaceAccount<'info, TokenAccount>,
    #[account(
        init_if_needed,
        payer = payer,
        associated_token::mint = mint_b,
        associated_token::authority = pool,
        associated_token::token_program = token_program_b,
    )]
    pub vault_b: InterfaceAccount<'info, TokenAccount>,
    pub token_program_a: Interface<'info, TokenInterface>,
    pub token_program_b: Interface<'info, TokenInterface>,
    pub associated_token_program: Program<'info, AssociatedToken>,
}

#[derive(Accounts)]
pub struct MarketInfoAccounts<'info> {
    pub pool: Account<'info, Pool>,
}

/// The router's ten fixed trade accounts, for quotes and account resolution. Only the
/// pool is read; in simulation the others may be placeholders.
#[derive(Accounts)]
pub struct ReadTrade<'info> {
    /// CHECK: unused in reads.
    pub market_signer: UncheckedAccount<'info>,
    pub pool: Account<'info, Pool>,
    /// CHECK: unused in reads.
    pub trader: UncheckedAccount<'info>,
    /// CHECK: unused in reads.
    pub trader_input: UncheckedAccount<'info>,
    /// CHECK: unused in reads.
    pub trader_output: UncheckedAccount<'info>,
    /// CHECK: unused in reads.
    pub input_vault: UncheckedAccount<'info>,
    /// CHECK: unused in reads.
    pub input_mint: UncheckedAccount<'info>,
    /// CHECK: unused in reads.
    pub output_mint: UncheckedAccount<'info>,
    /// CHECK: unused in reads.
    pub input_token_program: UncheckedAccount<'info>,
    /// CHECK: unused in reads.
    pub output_token_program: UncheckedAccount<'info>,
}

/// The router's ten fixed trade accounts, as passed to `pipeline_swap`.
#[derive(Accounts)]
pub struct SwapAccounts<'info> {
    /// The router's market PDA; compared with the pool's `market_signer`.
    pub market_signer: Signer<'info>,
    #[account(mut)]
    pub pool: Account<'info, Pool>,
    /// CHECK: the trader's wallet, for identity only. It never signs here.
    pub trader: UncheckedAccount<'info>,
    /// CHECK: receives refunds; the token program checks it.
    #[account(mut)]
    pub trader_input: UncheckedAccount<'info>,
    /// CHECK: receives the output; the token program checks it.
    #[account(mut)]
    pub trader_output: UncheckedAccount<'info>,
    /// CHECK: compared with the pool's vault for the input mint.
    #[account(mut)]
    pub input_vault: UncheckedAccount<'info>,
    pub input_mint: InterfaceAccount<'info, Mint>,
    pub output_mint: InterfaceAccount<'info, Mint>,
    pub input_token_program: Interface<'info, TokenInterface>,
    pub output_token_program: Interface<'info, TokenInterface>,
}

/// `deposit` accounts. The order is part of `metadata.json`; keep them in step.
#[derive(Accounts)]
pub struct Deposit<'info> {
    pub depositor: Signer<'info>,
    #[account(mut, has_one = mint_a, has_one = mint_b, has_one = vault_a, has_one = vault_b)]
    pub pool: Account<'info, Pool>,
    /// CHECK: the token program checks the source and the depositor's authority.
    #[account(mut)]
    pub depositor_a: UncheckedAccount<'info>,
    /// CHECK: the token program checks the source and the depositor's authority.
    #[account(mut)]
    pub depositor_b: UncheckedAccount<'info>,
    /// CHECK: matched against the pool.
    #[account(mut)]
    pub vault_a: UncheckedAccount<'info>,
    /// CHECK: matched against the pool.
    #[account(mut)]
    pub vault_b: UncheckedAccount<'info>,
    pub mint_a: InterfaceAccount<'info, Mint>,
    pub mint_b: InterfaceAccount<'info, Mint>,
    pub token_program_a: Interface<'info, TokenInterface>,
    pub token_program_b: Interface<'info, TokenInterface>,
}

#[error_code]
pub enum StrategyError {
    #[msg("Init data is not valid InitParams")]
    BadInitData,
    #[msg("Caller is not this pool's Pipeline market")]
    NotPipeline,
    #[msg("Unexpected account")]
    WrongAccount,
    #[msg("Not enough input for the requested output")]
    InsufficientInput,
    #[msg("Pool cannot provide that much output")]
    InsufficientLiquidity,
    #[msg("An empty pool needs both tokens in its first deposit")]
    OneSidedDeposit,
    #[msg("Arithmetic overflow")]
    Overflow,
}
