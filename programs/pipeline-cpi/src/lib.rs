//! Call the Pipeline router from an on-chain program.
//!
//! A program (a vault, a treasury, a keeper, another protocol) can trade through any
//! Pipeline market by invoking the router's `swap` or `route` instruction, signing as the
//! trader with its own PDA. This crate builds those instructions from `AccountInfo`s, in
//! the router's account order, invokes them with signer seeds, and reads back the
//! router's `SwapResult` or `RouteResult` from return data, checking that the return
//! data came from the router.
//!
//! ```ignore
//! // `accounts[1..]` were laid out by the client in the router's own order (the
//! // off-chain `pipeline_client::swap` instruction's accounts, trader unsigned).
//! let swap = pipeline_cpi::SwapAccounts::from_router_order(&accounts[1..], hook_count)?;
//! let result = pipeline_cpi::swap(
//!     &swap,
//!     &pipeline_cpi::SwapArgs::exact_in(amount, min_out),
//!     &[&[b"vault", owner.as_ref(), &[bump]]],
//! )?;
//! msg!("kept {}", result.kept(swap.output_mint.key));
//! ```
//!
//! The router checks every account it is given: market, vault, fee account, token
//! programs. It does not check whose token accounts the trader's input and output are,
//! beyond the trader signing for the input. A calling program that signs as the trader
//! must check that the output account is its own ([`check_token_account`]), or whoever
//! builds the transaction can send the output elsewhere.
//!
//! See [`depth`] for the call-depth budget: a swap whose transfers run a Token-2022
//! transfer hook needs four levels below the caller, so only a top-level instruction can
//! make one under Solana's current limit of five.
//!
//! With the `anchor` feature, [`anchor`] has the same calls taking Anchor's `CpiContext`.

use pipeline_strategy_interface as interface;
use solana_program::account_info::AccountInfo;
use solana_program::instruction::{AccountMeta, Instruction};
use solana_program::program::{get_return_data, invoke_signed};
use solana_program::program_error::ProgramError;
use solana_program::pubkey::Pubkey;

pub use interface::TradeMode;

#[cfg(feature = "anchor")]
pub mod anchor;
pub mod depth;
mod market;

pub use depth::{log_stack_height, stack_height};
pub use market::{
    check_token_account, read_market, FeeSide, MarketView, TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID,
};

/// The Pipeline router.
pub const ROUTER_ID: Pubkey = Pubkey::new_from_array(interface::PIPELINE_PROGRAM_ID);
/// Wrapped SOL. A market that trades it and declares no other fee mint takes Pipeline's
/// fee in it.
pub const NATIVE_MINT: Pubkey = Pubkey::new_from_array(interface::NATIVE_MINT);

/// Anchor's discriminator for the router's `swap`: `sha256("global:swap")[..8]`.
pub const SWAP_DISCRIMINATOR: [u8; 8] = [248, 198, 158, 145, 225, 117, 135, 200];
/// Anchor's discriminator for the router's `route`: `sha256("global:route")[..8]`.
pub const ROUTE_DISCRIMINATOR: [u8; 8] = [229, 23, 203, 151, 122, 227, 173, 42];

/// Accounts before the strategy's own in a `swap`: the trader, twelve market and token
/// accounts, the integrator fee account (or the router's ID in its place), and the
/// router's event authority and program.
pub const SWAP_FIXED_ACCOUNTS: usize = 16;
/// Accounts before the first hop in a `route`: the trader, the integrator fee account (or
/// the router's ID), the event authority and the router.
pub const ROUTE_FIXED_ACCOUNTS: usize = 4;
/// Fixed accounts of each route hop, before its strategy and hook accounts.
pub const ROUTE_HOP_ACCOUNTS: usize = 12;
/// The most hops one `route` runs.
pub const MAX_ROUTE_HOPS: usize = 4;

/// Errors from this crate, as `ProgramError::Custom` codes starting at `0x5049_5000`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum CpiError {
    /// The program account passed as the router is not Pipeline's router.
    NotTheRouter = 0x5049_5000,
    /// Fewer accounts than the router's layout needs.
    NotEnoughAccounts,
    /// More hook or strategy accounts declared than were passed, or accounts left over.
    AccountCountMismatch,
    /// The router left no return data, or the return data came from another program.
    NoRouterResult,
    /// The router's return data didn't decode.
    BadRouterResult,
    /// A route has no hops, or more than [`MAX_ROUTE_HOPS`].
    BadRoute,
    /// The account is not a router `Market`.
    NotAMarket,
    /// The token account is not owned by the expected wallet, holds another mint, or is
    /// not a token account.
    WrongTokenAccount,
}

impl From<CpiError> for ProgramError {
    fn from(error: CpiError) -> Self {
        ProgramError::Custom(error as u32)
    }
}

// ---------------------------------------------------------------------------------------
// Swap
// ---------------------------------------------------------------------------------------

/// The accounts of one router `swap`.
pub struct SwapAccounts<'a, 'info> {
    /// The trader: a PDA of the calling program signs through the seeds given to [`swap`].
    pub trader: &'a AccountInfo<'info>,
    pub strategy_program: &'a AccountInfo<'info>,
    pub market: &'a AccountInfo<'info>,
    pub market_state: &'a AccountInfo<'info>,
    /// The trader's token account in the input mint.
    pub trader_input: &'a AccountInfo<'info>,
    /// The token account the output arrives in. The router doesn't check whose it is.
    pub trader_output: &'a AccountInfo<'info>,
    /// The market's vault for the input mint.
    pub input_vault: &'a AccountInfo<'info>,
    pub input_mint: &'a AccountInfo<'info>,
    pub output_mint: &'a AccountInfo<'info>,
    pub input_token_program: &'a AccountInfo<'info>,
    pub output_token_program: &'a AccountInfo<'info>,
    /// The market's fee authority PDA, `["fees", market]` under the router.
    pub fee_authority: &'a AccountInfo<'info>,
    /// The fee authority's associated token account in the mint the fee is taken in: the
    /// output mint when it is the market's fee mint ([`FeeSide::Output`]), else the input
    /// mint. See [`MarketView::fee_side`].
    pub fee_account: &'a AccountInfo<'info>,
    /// A front-end's fee account in the input mint, if `integrator_fee_bps` is not 0.
    pub integrator_fee_account: Option<&'a AccountInfo<'info>>,
    /// The router's event authority PDA, `["__event_authority"]`.
    pub event_authority: &'a AccountInfo<'info>,
    /// The router program.
    pub router: &'a AccountInfo<'info>,
    /// The strategy's own accounts, as its `pipeline_resolve_accounts` lists them. When
    /// the output mint has a transfer hook, its hook accounts go here too.
    pub strategy_accounts: &'a [AccountInfo<'info>],
    /// The input mint's transfer-hook accounts (empty if it has no hook).
    pub input_hook_accounts: &'a [AccountInfo<'info>],
}

impl<'a, 'info> SwapAccounts<'a, 'info> {
    /// Reads a swap's accounts laid out in the router's own order: the
    /// [`SWAP_FIXED_ACCOUNTS`] fixed accounts, the strategy's accounts, then
    /// `input_hook_accounts` hook accounts. This is the order of the off-chain
    /// `pipeline_client::swap` instruction's accounts, so a client can append those to
    /// the calling program's own accounts (with the trader not signing).
    pub fn from_router_order(
        accounts: &'a [AccountInfo<'info>],
        input_hook_accounts: u8,
    ) -> Result<Self, ProgramError> {
        if accounts.len() < SWAP_FIXED_ACCOUNTS {
            return Err(CpiError::NotEnoughAccounts.into());
        }
        let (fixed, rest) = accounts.split_at(SWAP_FIXED_ACCOUNTS);
        let hooks = input_hook_accounts as usize;
        if rest.len() < hooks {
            return Err(CpiError::AccountCountMismatch.into());
        }
        let (strategy_accounts, input_hook_accounts) = rest.split_at(rest.len() - hooks);
        Ok(Self {
            trader: &fixed[0],
            strategy_program: &fixed[1],
            market: &fixed[2],
            market_state: &fixed[3],
            trader_input: &fixed[4],
            trader_output: &fixed[5],
            input_vault: &fixed[6],
            input_mint: &fixed[7],
            output_mint: &fixed[8],
            input_token_program: &fixed[9],
            output_token_program: &fixed[10],
            fee_authority: &fixed[11],
            fee_account: &fixed[12],
            integrator_fee_account: optional_account(&fixed[13]),
            event_authority: &fixed[14],
            router: &fixed[15],
            strategy_accounts,
            input_hook_accounts,
        })
    }

    /// The `swap` instruction. Fails if `router` is not Pipeline's router.
    pub fn instruction(&self, args: &SwapArgs) -> Result<Instruction, ProgramError> {
        check_router(self.router)?;
        let mut metas = Vec::with_capacity(
            SWAP_FIXED_ACCOUNTS + self.strategy_accounts.len() + self.input_hook_accounts.len(),
        );
        metas.extend(self.fixed_metas());
        metas.extend(self.strategy_accounts.iter().map(passed_on));
        metas.extend(self.input_hook_accounts.iter().map(passed_on));
        let hooks = u8::try_from(self.input_hook_accounts.len())
            .map_err(|_| CpiError::AccountCountMismatch)?;
        Ok(Instruction {
            program_id: ROUTER_ID,
            accounts: metas,
            data: args.data(hooks),
        })
    }

    /// The metas of the [`SWAP_FIXED_ACCOUNTS`], with the trader signing.
    pub(crate) fn fixed_metas(&self) -> [AccountMeta; SWAP_FIXED_ACCOUNTS] {
        [
            AccountMeta::new_readonly(*self.trader.key, true),
            AccountMeta::new_readonly(*self.strategy_program.key, false),
            AccountMeta::new_readonly(*self.market.key, false),
            AccountMeta::new(*self.market_state.key, false),
            AccountMeta::new(*self.trader_input.key, false),
            AccountMeta::new(*self.trader_output.key, false),
            AccountMeta::new(*self.input_vault.key, false),
            AccountMeta::new_readonly(*self.input_mint.key, false),
            AccountMeta::new_readonly(*self.output_mint.key, false),
            AccountMeta::new_readonly(*self.input_token_program.key, false),
            AccountMeta::new_readonly(*self.output_token_program.key, false),
            AccountMeta::new_readonly(*self.fee_authority.key, false),
            AccountMeta::new(*self.fee_account.key, false),
            integrator_meta(self.integrator_fee_account),
            AccountMeta::new_readonly(*self.event_authority.key, false),
            AccountMeta::new_readonly(ROUTER_ID, false),
        ]
    }

    /// Every account the instruction names, for `invoke_signed`.
    pub fn account_infos(&self) -> Vec<AccountInfo<'info>> {
        let mut infos = Vec::with_capacity(
            SWAP_FIXED_ACCOUNTS + self.strategy_accounts.len() + self.input_hook_accounts.len(),
        );
        infos.extend(
            [
                self.trader,
                self.strategy_program,
                self.market,
                self.market_state,
                self.trader_input,
                self.trader_output,
                self.input_vault,
                self.input_mint,
                self.output_mint,
                self.input_token_program,
                self.output_token_program,
                self.fee_authority,
                self.fee_account,
                self.integrator_fee_account.unwrap_or(self.router),
                self.event_authority,
                self.router,
            ]
            .into_iter()
            .cloned(),
        );
        infos.extend_from_slice(self.strategy_accounts);
        infos.extend_from_slice(self.input_hook_accounts);
        infos
    }
}

/// What to ask one swap for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwapArgs {
    pub mode: TradeMode,
    /// Exact-in: what the strategy receives (fees come on top). Exact-out: the most it
    /// may receive.
    pub amount_in: u64,
    /// Exact-out only: the output the strategy must send.
    pub amount_out: u64,
    /// The least the trader must end up with, measured on the output account (after
    /// Pipeline's fee when the fee is taken in the output).
    pub min_amount_out: u64,
    /// A front-end's own fee, in basis points of the input, paid to
    /// `integrator_fee_account`.
    pub integrator_fee_bps: u16,
    /// Strategy-specific data passed to its `pipeline_swap`.
    pub extra: Vec<u8>,
}

impl SwapArgs {
    /// An exact-in swap of `amount_in` with no integrator fee.
    pub fn exact_in(amount_in: u64, min_amount_out: u64) -> Self {
        Self {
            mode: TradeMode::ExactIn,
            amount_in,
            amount_out: 0,
            min_amount_out,
            integrator_fee_bps: 0,
            extra: Vec::new(),
        }
    }

    /// The instruction data: the discriminator, then the router's Borsh `SwapArgs`.
    fn data(&self, input_hook_accounts: u8) -> Vec<u8> {
        let mut data = Vec::with_capacity(8 + 1 + 8 * 3 + 2 + 1 + 4 + self.extra.len());
        data.extend_from_slice(&SWAP_DISCRIMINATOR);
        data.push(trade_mode_byte(self.mode));
        data.extend_from_slice(&self.amount_in.to_le_bytes());
        data.extend_from_slice(&self.amount_out.to_le_bytes());
        data.extend_from_slice(&self.min_amount_out.to_le_bytes());
        data.extend_from_slice(&self.integrator_fee_bps.to_le_bytes());
        data.push(input_hook_accounts);
        push_bytes(&mut data, &self.extra);
        data
    }
}

/// What a swap did, as the router returns it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SwapResult {
    /// The input the swap was charged for, excluding fees.
    pub net_sent: u64,
    /// What arrived in the trader's output account, before any fee taken from it.
    pub received: u64,
    pub pipeline_fee: u64,
    pub integrator_fee: u64,
    /// The mint `pipeline_fee` was taken in.
    pub fee_mint: Pubkey,
}

impl SwapResult {
    /// Byte length of the router's Borsh encoding.
    pub const LEN: usize = 8 * 4 + 32;

    /// What the trader keeps of `received`: all of it, less Pipeline's fee when the fee
    /// was taken in `output_mint`.
    pub fn kept(&self, output_mint: &Pubkey) -> u64 {
        if self.fee_mint == *output_mint {
            self.received.saturating_sub(self.pipeline_fee)
        } else {
            self.received
        }
    }

    /// The total the trader parted with in the input mint: the swap's input plus the fees
    /// taken in the input.
    pub fn spent(&self, input_mint: &Pubkey) -> u64 {
        let pipeline_fee = if self.fee_mint == *input_mint {
            self.pipeline_fee
        } else {
            0
        };
        self.net_sent
            .saturating_add(pipeline_fee)
            .saturating_add(self.integrator_fee)
    }

    /// Decodes the router's return data.
    pub fn decode(data: &[u8]) -> Result<Self, ProgramError> {
        let bytes = padded::<{ Self::LEN }>(data)?;
        Ok(Self {
            net_sent: read_u64(&bytes, 0),
            received: read_u64(&bytes, 8),
            pipeline_fee: read_u64(&bytes, 16),
            integrator_fee: read_u64(&bytes, 24),
            fee_mint: Pubkey::new_from_array(bytes[32..64].try_into().unwrap()),
        })
    }
}

/// Invokes the router's `swap`, signing with `signer_seeds` (the trader's PDA seeds, with
/// its bump), and returns the router's result.
pub fn swap(
    accounts: &SwapAccounts,
    args: &SwapArgs,
    signer_seeds: &[&[&[u8]]],
) -> Result<SwapResult, ProgramError> {
    let instruction = accounts.instruction(args)?;
    invoke_signed(&instruction, &accounts.account_infos(), signer_seeds)?;
    swap_result()
}

/// Reads the `SwapResult` the router just returned. Call it right after the router's
/// instruction: any later CPI replaces the return data.
pub fn swap_result() -> Result<SwapResult, ProgramError> {
    SwapResult::decode(&router_return_data()?)
}

// ---------------------------------------------------------------------------------------
// Route
// ---------------------------------------------------------------------------------------

/// One hop of a `route`: the same market and token accounts as a swap, without the
/// trader, integrator or router accounts, and the strategy's data for the hop.
pub struct RouteHopAccounts<'a, 'info> {
    pub strategy_program: &'a AccountInfo<'info>,
    pub market: &'a AccountInfo<'info>,
    pub market_state: &'a AccountInfo<'info>,
    /// The first hop's input is the trader's input; each later hop's input must be the
    /// previous hop's output account.
    pub trader_input: &'a AccountInfo<'info>,
    pub trader_output: &'a AccountInfo<'info>,
    pub input_vault: &'a AccountInfo<'info>,
    pub input_mint: &'a AccountInfo<'info>,
    pub output_mint: &'a AccountInfo<'info>,
    pub input_token_program: &'a AccountInfo<'info>,
    pub output_token_program: &'a AccountInfo<'info>,
    pub fee_authority: &'a AccountInfo<'info>,
    /// The fee account on the hop's fee side, as for a swap.
    pub fee_account: &'a AccountInfo<'info>,
    pub strategy_accounts: &'a [AccountInfo<'info>],
    pub input_hook_accounts: &'a [AccountInfo<'info>],
    /// Strategy-specific data for this hop's `pipeline_swap`.
    pub extra: &'a [u8],
}

/// The accounts of a router `route`.
pub struct RouteAccounts<'a, 'info> {
    pub trader: &'a AccountInfo<'info>,
    /// A front-end's fee account in the first hop's input mint.
    pub integrator_fee_account: Option<&'a AccountInfo<'info>>,
    pub event_authority: &'a AccountInfo<'info>,
    pub router: &'a AccountInfo<'info>,
    pub hops: Vec<RouteHopAccounts<'a, 'info>>,
}

/// How many strategy and input-hook accounts one route hop has after its fixed ones.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HopShape {
    pub strategy_accounts: u8,
    pub input_hook_accounts: u8,
}

impl<'a, 'info> RouteAccounts<'a, 'info> {
    /// Reads a route's accounts laid out in the router's own order (as the off-chain
    /// `pipeline_client::route` lays them out): the [`ROUTE_FIXED_ACCOUNTS`], then per hop
    /// its [`ROUTE_HOP_ACCOUNTS`], strategy accounts and input hook accounts. Every
    /// account must be used. Each hop's `extra` is empty; set it afterwards if needed.
    pub fn from_router_order(
        accounts: &'a [AccountInfo<'info>],
        shapes: &[HopShape],
    ) -> Result<Self, ProgramError> {
        if shapes.is_empty() || shapes.len() > MAX_ROUTE_HOPS {
            return Err(CpiError::BadRoute.into());
        }
        if accounts.len() < ROUTE_FIXED_ACCOUNTS {
            return Err(CpiError::NotEnoughAccounts.into());
        }
        let (fixed, mut rest) = accounts.split_at(ROUTE_FIXED_ACCOUNTS);
        let mut hops = Vec::with_capacity(shapes.len());
        for shape in shapes {
            let strategy = shape.strategy_accounts as usize;
            let take = ROUTE_HOP_ACCOUNTS + strategy + shape.input_hook_accounts as usize;
            if rest.len() < take {
                return Err(CpiError::NotEnoughAccounts.into());
            }
            let (hop, tail) = rest.split_at(take);
            rest = tail;
            let (strategy_accounts, input_hook_accounts) =
                hop[ROUTE_HOP_ACCOUNTS..].split_at(strategy);
            hops.push(RouteHopAccounts {
                strategy_program: &hop[0],
                market: &hop[1],
                market_state: &hop[2],
                trader_input: &hop[3],
                trader_output: &hop[4],
                input_vault: &hop[5],
                input_mint: &hop[6],
                output_mint: &hop[7],
                input_token_program: &hop[8],
                output_token_program: &hop[9],
                fee_authority: &hop[10],
                fee_account: &hop[11],
                strategy_accounts,
                input_hook_accounts,
                extra: &[],
            });
        }
        if !rest.is_empty() {
            return Err(CpiError::AccountCountMismatch.into());
        }
        Ok(Self {
            trader: &fixed[0],
            integrator_fee_account: optional_account(&fixed[1]),
            event_authority: &fixed[2],
            router: &fixed[3],
            hops,
        })
    }

    /// The `route` instruction. Fails if `router` is not Pipeline's router or the hop
    /// count is out of range.
    pub fn instruction(&self, args: &RouteArgs) -> Result<Instruction, ProgramError> {
        check_router(self.router)?;
        if self.hops.is_empty() || self.hops.len() > MAX_ROUTE_HOPS {
            return Err(CpiError::BadRoute.into());
        }
        let mut metas = vec![
            AccountMeta::new_readonly(*self.trader.key, true),
            integrator_meta(self.integrator_fee_account),
            AccountMeta::new_readonly(*self.event_authority.key, false),
            AccountMeta::new_readonly(ROUTER_ID, false),
        ];
        let mut data = Vec::with_capacity(8 + 8 * 2 + 2 + 4 + self.hops.len() * 6);
        data.extend_from_slice(&ROUTE_DISCRIMINATOR);
        data.extend_from_slice(&args.amount_in.to_le_bytes());
        data.extend_from_slice(&args.min_amount_out.to_le_bytes());
        data.extend_from_slice(&args.integrator_fee_bps.to_le_bytes());
        data.extend_from_slice(&(self.hops.len() as u32).to_le_bytes());
        for hop in &self.hops {
            metas.extend([
                AccountMeta::new_readonly(*hop.strategy_program.key, false),
                AccountMeta::new_readonly(*hop.market.key, false),
                AccountMeta::new(*hop.market_state.key, false),
                AccountMeta::new(*hop.trader_input.key, false),
                AccountMeta::new(*hop.trader_output.key, false),
                AccountMeta::new(*hop.input_vault.key, false),
                AccountMeta::new_readonly(*hop.input_mint.key, false),
                AccountMeta::new_readonly(*hop.output_mint.key, false),
                AccountMeta::new_readonly(*hop.input_token_program.key, false),
                AccountMeta::new_readonly(*hop.output_token_program.key, false),
                AccountMeta::new_readonly(*hop.fee_authority.key, false),
                AccountMeta::new(*hop.fee_account.key, false),
            ]);
            metas.extend(hop.strategy_accounts.iter().map(passed_on));
            metas.extend(hop.input_hook_accounts.iter().map(passed_on));
            let count = |accounts: &[AccountInfo]| {
                u8::try_from(accounts.len()).map_err(|_| CpiError::AccountCountMismatch)
            };
            data.push(count(hop.strategy_accounts)?);
            data.push(count(hop.input_hook_accounts)?);
            push_bytes(&mut data, hop.extra);
        }
        Ok(Instruction {
            program_id: ROUTER_ID,
            accounts: metas,
            data,
        })
    }

    /// Every account the instruction names, for `invoke_signed`.
    pub fn account_infos(&self) -> Vec<AccountInfo<'info>> {
        let mut infos = vec![
            self.trader.clone(),
            self.integrator_fee_account.unwrap_or(self.router).clone(),
            self.event_authority.clone(),
            self.router.clone(),
        ];
        for hop in &self.hops {
            infos.extend(
                [
                    hop.strategy_program,
                    hop.market,
                    hop.market_state,
                    hop.trader_input,
                    hop.trader_output,
                    hop.input_vault,
                    hop.input_mint,
                    hop.output_mint,
                    hop.input_token_program,
                    hop.output_token_program,
                    hop.fee_authority,
                    hop.fee_account,
                ]
                .into_iter()
                .cloned(),
            );
            infos.extend_from_slice(hop.strategy_accounts);
            infos.extend_from_slice(hop.input_hook_accounts);
        }
        infos
    }
}

/// What to ask a route for. Routes are exact-in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RouteArgs {
    /// What the first hop's strategy receives (fees come on top). Later hops spend what
    /// the previous hop left the trader.
    pub amount_in: u64,
    /// The least the trader must keep from the last hop.
    pub min_amount_out: u64,
    /// Charged once, on the first hop's input.
    pub integrator_fee_bps: u16,
}

/// What a route did, as the router returns it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteResult {
    /// What the trader parted with in the first hop's input, before fees.
    pub net_sent: u64,
    /// What the trader keeps from the last hop.
    pub amount_out: u64,
    pub integrator_fee: u64,
    /// Pipeline's fee taken in the route's input mint: the first hop's fee when that hop
    /// pays it on its input, otherwise 0.
    pub input_pipeline_fee: u64,
}

impl RouteResult {
    /// Byte length of the router's Borsh encoding.
    pub const LEN: usize = 8 * 4;

    /// What the trader spent in the route's input mint: `net_sent` plus both fees taken
    /// in it.
    pub fn spent(&self) -> u64 {
        self.net_sent + self.input_pipeline_fee + self.integrator_fee
    }

    /// Decodes the router's return data.
    pub fn decode(data: &[u8]) -> Result<Self, ProgramError> {
        let bytes = padded::<{ Self::LEN }>(data)?;
        Ok(Self {
            net_sent: read_u64(&bytes, 0),
            amount_out: read_u64(&bytes, 8),
            integrator_fee: read_u64(&bytes, 16),
            input_pipeline_fee: read_u64(&bytes, 24),
        })
    }
}

/// Invokes the router's `route`, signing with `signer_seeds`, and returns its result.
pub fn route(
    accounts: &RouteAccounts,
    args: &RouteArgs,
    signer_seeds: &[&[&[u8]]],
) -> Result<RouteResult, ProgramError> {
    let instruction = accounts.instruction(args)?;
    invoke_signed(&instruction, &accounts.account_infos(), signer_seeds)?;
    route_result()
}

/// Reads the `RouteResult` the router just returned.
pub fn route_result() -> Result<RouteResult, ProgramError> {
    RouteResult::decode(&router_return_data()?)
}

// ---------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------

/// The current return data, if the router set it. Another program's return data (a
/// strategy's quote, say) is refused rather than misread as the router's result.
fn router_return_data() -> Result<Vec<u8>, ProgramError> {
    match get_return_data() {
        Some((program, data)) if program == ROUTER_ID => Ok(data),
        _ => Err(CpiError::NoRouterResult.into()),
    }
}

fn check_router(router: &AccountInfo) -> Result<(), ProgramError> {
    if *router.key == ROUTER_ID {
        Ok(())
    } else {
        Err(CpiError::NotTheRouter.into())
    }
}

/// Anchor encodes a missing optional account as the program's own ID.
fn optional_account<'a, 'info>(account: &'a AccountInfo<'info>) -> Option<&'a AccountInfo<'info>> {
    if *account.key == ROUTER_ID {
        None
    } else {
        Some(account)
    }
}

pub(crate) fn integrator_meta(account: Option<&AccountInfo>) -> AccountMeta {
    match account {
        Some(account) => AccountMeta::new(*account.key, false),
        None => AccountMeta::new_readonly(ROUTER_ID, false),
    }
}

/// A strategy or hook account, passed on with its writable flag and never as a signer
/// (the router passes none of them on as signers either).
fn passed_on(account: &AccountInfo) -> AccountMeta {
    if account.is_writable {
        AccountMeta::new(*account.key, false)
    } else {
        AccountMeta::new_readonly(*account.key, false)
    }
}

fn trade_mode_byte(mode: TradeMode) -> u8 {
    match mode {
        TradeMode::ExactIn => 0,
        TradeMode::ExactOut => 1,
    }
}

/// Borsh `Vec<u8>`: a u32 length, then the bytes.
fn push_bytes(data: &mut Vec<u8>, bytes: &[u8]) {
    data.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    data.extend_from_slice(bytes);
}

/// `data` zero-extended to `N` bytes. Transaction metadata trims trailing zero bytes from
/// return data, so a result copied from there may be short; one that is longer is not
/// the router's.
fn padded<const N: usize>(data: &[u8]) -> Result<[u8; N], ProgramError> {
    if data.len() > N {
        return Err(CpiError::BadRouterResult.into());
    }
    let mut bytes = [0u8; N];
    bytes[..data.len()].copy_from_slice(data);
    Ok(bytes)
}

fn read_u64(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

#[cfg(test)]
mod tests;
