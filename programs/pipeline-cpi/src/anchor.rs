//! Router calls for Anchor programs, in the style of `anchor_spl`: an accounts struct
//! for a `CpiContext`, and functions that take the context.
//!
//! ```ignore
//! let accounts = pipeline_cpi::anchor::Swap {
//!     trader: ctx.accounts.vault.to_account_info(),
//!     // ...the market and token accounts...
//!     integrator_fee_account: None,
//!     event_authority: ctx.accounts.event_authority.to_account_info(),
//!     router: ctx.accounts.router.to_account_info(),
//! };
//! let cpi = CpiContext::new_with_signer(pipeline_cpi::ROUTER_ID, accounts, signer_seeds)
//!     .with_remaining_accounts(ctx.remaining_accounts.to_vec());
//! let result = pipeline_cpi::anchor::swap(cpi, args, input_hook_accounts)?;
//! ```
//!
//! The remaining accounts are the strategy's accounts, then `input_hook_accounts` hook
//! accounts for the input mint, as for [`crate::SwapAccounts`]. Anchor's `AccountInfo`
//! is the same type this crate's plain functions take, so an Anchor program can also call
//! those directly.

use anchor_lang::prelude::{AccountInfo, AccountMeta, CpiContext, Pubkey};
use anchor_lang::{ToAccountInfos, ToAccountMetas};

use crate::{RouteArgs, RouteResult, SwapArgs, SwapResult};

/// The fixed accounts of a router `swap`; see [`crate::SwapAccounts`] for each one.
#[derive(Clone)]
pub struct Swap<'info> {
    pub trader: AccountInfo<'info>,
    pub strategy_program: AccountInfo<'info>,
    pub market: AccountInfo<'info>,
    pub market_state: AccountInfo<'info>,
    pub trader_input: AccountInfo<'info>,
    pub trader_output: AccountInfo<'info>,
    pub input_vault: AccountInfo<'info>,
    pub input_mint: AccountInfo<'info>,
    pub output_mint: AccountInfo<'info>,
    pub input_token_program: AccountInfo<'info>,
    pub output_token_program: AccountInfo<'info>,
    pub fee_authority: AccountInfo<'info>,
    pub fee_account: AccountInfo<'info>,
    pub integrator_fee_account: Option<AccountInfo<'info>>,
    pub event_authority: AccountInfo<'info>,
    pub router: AccountInfo<'info>,
}

impl<'info> Swap<'info> {
    fn plain<'a>(
        &'a self,
        remaining: &'a [AccountInfo<'info>],
        input_hook_accounts: u8,
    ) -> anchor_lang::Result<crate::SwapAccounts<'a, 'info>> {
        let hooks = input_hook_accounts as usize;
        if remaining.len() < hooks {
            return Err(crate::CpiError::AccountCountMismatch.into());
        }
        let (strategy_accounts, input_hook_accounts) = remaining.split_at(remaining.len() - hooks);
        Ok(crate::SwapAccounts {
            trader: &self.trader,
            strategy_program: &self.strategy_program,
            market: &self.market,
            market_state: &self.market_state,
            trader_input: &self.trader_input,
            trader_output: &self.trader_output,
            input_vault: &self.input_vault,
            input_mint: &self.input_mint,
            output_mint: &self.output_mint,
            input_token_program: &self.input_token_program,
            output_token_program: &self.output_token_program,
            fee_authority: &self.fee_authority,
            fee_account: &self.fee_account,
            integrator_fee_account: self.integrator_fee_account.as_ref(),
            event_authority: &self.event_authority,
            router: &self.router,
            strategy_accounts,
            input_hook_accounts,
        })
    }
}

impl ToAccountMetas for Swap<'_> {
    fn to_account_metas(&self, _is_signer: Option<bool>) -> Vec<AccountMeta> {
        let plain = self.plain(&[], 0).expect("no hook accounts declared");
        plain.fixed_metas().to_vec()
    }
}

impl<'info> ToAccountInfos<'info> for Swap<'info> {
    fn to_account_infos(&self) -> Vec<AccountInfo<'info>> {
        let plain = self.plain(&[], 0).expect("no hook accounts declared");
        plain.account_infos()
    }
}

/// Invokes the router's `swap` with the context's signer seeds and returns its result.
/// The context's remaining accounts are the strategy's accounts, then
/// `input_hook_accounts` hook accounts for the input mint.
pub fn swap<'info>(
    ctx: CpiContext<'_, '_, '_, 'info, Swap<'info>>,
    args: SwapArgs,
    input_hook_accounts: u8,
) -> anchor_lang::Result<SwapResult> {
    check_program(&ctx.program_id)?;
    let accounts = ctx
        .accounts
        .plain(&ctx.remaining_accounts, input_hook_accounts)?;
    Ok(crate::swap(&accounts, &args, ctx.signer_seeds)?)
}

/// The fixed accounts of a router `route`. The hops' accounts go in the context's
/// remaining accounts, in the router's order (see [`crate::RouteAccounts`]).
#[derive(Clone)]
pub struct Route<'info> {
    pub trader: AccountInfo<'info>,
    pub integrator_fee_account: Option<AccountInfo<'info>>,
    pub event_authority: AccountInfo<'info>,
    pub router: AccountInfo<'info>,
}

impl ToAccountMetas for Route<'_> {
    fn to_account_metas(&self, _is_signer: Option<bool>) -> Vec<AccountMeta> {
        vec![
            AccountMeta::new_readonly(*self.trader.key, true),
            crate::integrator_meta(self.integrator_fee_account.as_ref()),
            AccountMeta::new_readonly(*self.event_authority.key, false),
            AccountMeta::new_readonly(crate::ROUTER_ID, false),
        ]
    }
}

impl<'info> ToAccountInfos<'info> for Route<'info> {
    fn to_account_infos(&self) -> Vec<AccountInfo<'info>> {
        vec![
            self.trader.clone(),
            self.integrator_fee_account
                .clone()
                .unwrap_or_else(|| self.router.clone()),
            self.event_authority.clone(),
            self.router.clone(),
        ]
    }
}

/// Invokes the router's `route` with the context's signer seeds and returns its result.
/// The context's remaining accounts hold each hop's accounts, shaped by `shapes`; `extras`
/// is each hop's strategy data (missing entries are empty).
pub fn route<'info>(
    ctx: CpiContext<'_, '_, '_, 'info, Route<'info>>,
    args: RouteArgs,
    shapes: &[crate::HopShape],
    extras: &[&[u8]],
) -> anchor_lang::Result<RouteResult> {
    check_program(&ctx.program_id)?;
    let mut all = ctx.accounts.to_account_infos();
    all.extend(ctx.remaining_accounts.iter().cloned());
    let mut accounts = crate::RouteAccounts::from_router_order(&all, shapes)?;
    for (hop, extra) in accounts.hops.iter_mut().zip(extras) {
        hop.extra = extra;
    }
    Ok(crate::route(&accounts, &args, ctx.signer_seeds)?)
}

fn check_program(program_id: &Pubkey) -> anchor_lang::Result<()> {
    if *program_id == crate::ROUTER_ID {
        Ok(())
    } else {
        Err(crate::CpiError::NotTheRouter.into())
    }
}

impl From<crate::CpiError> for anchor_lang::error::Error {
    fn from(error: crate::CpiError) -> Self {
        anchor_lang::prelude::ProgramError::from(error).into()
    }
}
