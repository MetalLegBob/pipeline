//! Example: a program-owned wallet that buys on a schedule through Pipeline.
//!
//! An owner creates a vault for one input and one output mint, funds its token accounts
//! and sets how much to spend per purchase, the least to accept for it (a limit price)
//! and how often it may buy. After that anyone can crank it: the cranker picks the
//! market or route, the vault checks the trade is its own (its mints, its token
//! accounts) and swaps the fixed amount through the router with `pipeline-cpi`, signing
//! as the vault PDA. Whatever market the cranker picks, the vault never gets less than
//! its minimum, which the router enforces after Pipeline's fee.
//!
//! The vault is the trader: the vault PDA owns the vault's token accounts, and nobody but
//! this program can sign for it.
//!
//! Instructions (first byte, then little-endian fields):
//!
//! - `0` create: `amount_per_swap u64, min_out_per_swap u64, interval_seconds i64`.
//!   Accounts: owner (signer, writable; pays rent), vault (writable, PDA
//!   `["vault", owner, input_mint, output_mint]`), input mint, output mint, system program.
//! - `1` crank with a swap: `input_hook_accounts u8`. Accounts: vault (writable), then the
//!   router `swap` accounts in the router's order with the vault as the unsigned trader.
//! - `2` crank with a route: `hops u8`, then per hop `strategy_accounts u8,
//!   input_hook_accounts u8`. Accounts: vault (writable), then the router `route`
//!   accounts in the router's order.
//! - `3` withdraw: `amount u64`. Accounts: owner (signer), vault, the vault's token
//!   account, its mint, the destination, the token program, then the mint's transfer-hook
//!   accounts if it has a hook.

use pipeline_cpi::{
    check_token_account, log_stack_height, HopShape, RouteAccounts, RouteArgs, SwapAccounts,
    SwapArgs, TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID,
};
use solana_program::{
    account_info::AccountInfo,
    clock::Clock,
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    msg,
    program::invoke_signed,
    program_error::ProgramError,
    pubkey::Pubkey,
    rent::Rent,
    sysvar::Sysvar,
};

solana_program::declare_id!("8XjwqK6qERiSUPqci7JEgUWN2P3ifpAEuMwFvrMiqQA7");

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);

pub const VAULT_SEED: &[u8] = b"vault";
/// The system program's address (all zeros).
const SYSTEM_PROGRAM_ID: Pubkey = Pubkey::new_from_array([0; 32]);

pub mod instruction {
    pub const CREATE: u8 = 0;
    pub const CRANK_SWAP: u8 = 1;
    pub const CRANK_ROUTE: u8 = 2;
    pub const WITHDRAW: u8 = 3;
}

/// The vault's errors, as `ProgramError::Custom` codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum VaultError {
    BadInstruction = 0xDCA0,
    NotTheVault,
    NotTheOwner,
    WrongMint,
    TooSoon,
}

impl From<VaultError> for ProgramError {
    fn from(error: VaultError) -> Self {
        ProgramError::Custom(error as u32)
    }
}

/// The vault's account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vault {
    pub owner: Pubkey,
    pub input_mint: Pubkey,
    pub output_mint: Pubkey,
    /// What each purchase sends to the strategy, in the input mint (fees come on top).
    pub amount_per_swap: u64,
    /// The least each purchase must leave the vault in the output mint.
    pub min_out_per_swap: u64,
    /// The least time between purchases.
    pub interval_seconds: i64,
    pub last_swap_at: i64,
    pub swaps: u64,
    /// The input sent to strategies, before fees, over every purchase.
    pub total_sent: u64,
    /// The output kept, after fees, over every purchase.
    pub total_received: u64,
    pub bump: u8,
}

impl Vault {
    pub const MAGIC: [u8; 8] = *b"dcavault";
    pub const LEN: usize = 8 + 32 * 3 + 8 * 7 + 1;

    pub fn address(owner: &Pubkey, input_mint: &Pubkey, output_mint: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(
            &[
                VAULT_SEED,
                owner.as_ref(),
                input_mint.as_ref(),
                output_mint.as_ref(),
            ],
            &ID,
        )
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut data = Vec::with_capacity(Self::LEN);
        data.extend_from_slice(&Self::MAGIC);
        data.extend_from_slice(self.owner.as_ref());
        data.extend_from_slice(self.input_mint.as_ref());
        data.extend_from_slice(self.output_mint.as_ref());
        for value in [
            self.amount_per_swap,
            self.min_out_per_swap,
            self.interval_seconds as u64,
            self.last_swap_at as u64,
            self.swaps,
            self.total_sent,
            self.total_received,
        ] {
            data.extend_from_slice(&value.to_le_bytes());
        }
        data.push(self.bump);
        data
    }

    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.len() != Self::LEN || data[..8] != Self::MAGIC {
            return None;
        }
        let key = |at: usize| Pubkey::new_from_array(data[at..at + 32].try_into().unwrap());
        let number = |i: usize| {
            let at = 8 + 32 * 3 + 8 * i;
            u64::from_le_bytes(data[at..at + 8].try_into().unwrap())
        };
        Some(Self {
            owner: key(8),
            input_mint: key(40),
            output_mint: key(72),
            amount_per_swap: number(0),
            min_out_per_swap: number(1),
            interval_seconds: number(2) as i64,
            last_swap_at: number(3) as i64,
            swaps: number(4),
            total_sent: number(5),
            total_received: number(6),
            bump: data[Self::LEN - 1],
        })
    }

    fn load(program_id: &Pubkey, account: &AccountInfo) -> Result<Self, ProgramError> {
        if account.owner != program_id {
            return Err(VaultError::NotTheVault.into());
        }
        Self::decode(&account.try_borrow_data()?).ok_or_else(|| VaultError::NotTheVault.into())
    }

    fn store(&self, account: &AccountInfo) -> ProgramResult {
        account
            .try_borrow_mut_data()?
            .copy_from_slice(&self.encode());
        Ok(())
    }
}

pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let (&tag, args) = data.split_first().ok_or(VaultError::BadInstruction)?;
    match tag {
        instruction::CREATE => create(program_id, accounts, args),
        instruction::CRANK_SWAP => crank_swap(program_id, accounts, args),
        instruction::CRANK_ROUTE => crank_route(program_id, accounts, args),
        instruction::WITHDRAW => withdraw(program_id, accounts, args),
        _ => Err(VaultError::BadInstruction.into()),
    }
}

fn create(program_id: &Pubkey, accounts: &[AccountInfo], args: &[u8]) -> ProgramResult {
    let [owner, vault, input_mint, output_mint, system] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    if !owner.is_signer {
        return Err(ProgramError::MissingRequiredSignature);
    }
    if args.len() != 24 {
        return Err(VaultError::BadInstruction.into());
    }
    let (address, bump) = Vault::address(owner.key, input_mint.key, output_mint.key);
    if *vault.key != address || *system.key != SYSTEM_PROGRAM_ID {
        return Err(VaultError::NotTheVault.into());
    }
    let state = Vault {
        owner: *owner.key,
        input_mint: *input_mint.key,
        output_mint: *output_mint.key,
        amount_per_swap: read_u64(args, 0),
        min_out_per_swap: read_u64(args, 8),
        interval_seconds: read_u64(args, 16) as i64,
        last_swap_at: 0,
        swaps: 0,
        total_sent: 0,
        total_received: 0,
        bump,
    };
    let seeds: &[&[u8]] = &[
        VAULT_SEED,
        owner.key.as_ref(),
        input_mint.key.as_ref(),
        output_mint.key.as_ref(),
        &[bump],
    ];
    // System program CreateAccount (0): lamports, space, owner.
    let mut create = 0u32.to_le_bytes().to_vec();
    create.extend_from_slice(&Rent::get()?.minimum_balance(Vault::LEN).to_le_bytes());
    create.extend_from_slice(&(Vault::LEN as u64).to_le_bytes());
    create.extend_from_slice(program_id.as_ref());
    invoke_signed(
        &Instruction {
            program_id: SYSTEM_PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(*owner.key, true),
                AccountMeta::new(*vault.key, true),
            ],
            data: create,
        },
        &[owner.clone(), vault.clone(), system.clone()],
        &[seeds],
    )?;
    state.store(vault)
}

/// Checks a purchase can happen now, returning the vault's state.
fn ready(program_id: &Pubkey, vault: &AccountInfo, now: i64) -> Result<Vault, ProgramError> {
    let state = Vault::load(program_id, vault)?;
    if state.swaps > 0 && now < state.last_swap_at.saturating_add(state.interval_seconds) {
        return Err(VaultError::TooSoon.into());
    }
    Ok(state)
}

/// Checks the trade is the vault's own: the vault trades, from its own account in its
/// input mint, into its own account in its output mint. The router checks the market
/// side; only the caller can check the output goes back to the caller.
fn check_trade(
    state: &Vault,
    vault: &AccountInfo,
    trader: &AccountInfo,
    trader_input: &AccountInfo,
    trader_output: &AccountInfo,
    input_mint: &Pubkey,
    output_mint: &Pubkey,
) -> ProgramResult {
    if trader.key != vault.key {
        return Err(VaultError::NotTheVault.into());
    }
    if *input_mint != state.input_mint || *output_mint != state.output_mint {
        return Err(VaultError::WrongMint.into());
    }
    check_token_account(trader_input, vault.key, input_mint)?;
    check_token_account(trader_output, vault.key, output_mint)
}

fn crank_swap(program_id: &Pubkey, accounts: &[AccountInfo], args: &[u8]) -> ProgramResult {
    let (vault, router_accounts) = accounts
        .split_first()
        .ok_or(ProgramError::NotEnoughAccountKeys)?;
    let [input_hook_accounts] = args else {
        return Err(VaultError::BadInstruction.into());
    };
    let now = Clock::get()?.unix_timestamp;
    let mut state = ready(program_id, vault, now)?;
    let swap = SwapAccounts::from_router_order(router_accounts, *input_hook_accounts)?;
    check_trade(
        &state,
        vault,
        swap.trader,
        swap.trader_input,
        swap.trader_output,
        swap.input_mint.key,
        swap.output_mint.key,
    )?;

    log_stack_height("dca-vault");
    let result = pipeline_cpi::swap(
        &swap,
        &SwapArgs::exact_in(state.amount_per_swap, state.min_out_per_swap),
        &[&vault_seeds(&state)],
    )?;
    let kept = result.kept(swap.output_mint.key);
    msg!(
        "dca-vault: sent {}, kept {}, pipeline fee {}",
        result.net_sent,
        kept,
        result.pipeline_fee
    );
    record(&mut state, now, result.net_sent, kept);
    state.store(vault)
}

fn crank_route(program_id: &Pubkey, accounts: &[AccountInfo], args: &[u8]) -> ProgramResult {
    let (vault, router_accounts) = accounts
        .split_first()
        .ok_or(ProgramError::NotEnoughAccountKeys)?;
    let (&hops, shapes) = args.split_first().ok_or(VaultError::BadInstruction)?;
    if shapes.len() != 2 * hops as usize {
        return Err(VaultError::BadInstruction.into());
    }
    let shapes: Vec<HopShape> = shapes
        .chunks_exact(2)
        .map(|shape| HopShape {
            strategy_accounts: shape[0],
            input_hook_accounts: shape[1],
        })
        .collect();
    let now = Clock::get()?.unix_timestamp;
    let mut state = ready(program_id, vault, now)?;
    let route = RouteAccounts::from_router_order(router_accounts, &shapes)?;
    let (first, last) = match (route.hops.first(), route.hops.last()) {
        (Some(first), Some(last)) => (first, last),
        _ => return Err(VaultError::BadInstruction.into()),
    };
    // The router makes each hop spend from the previous hop's output account, so only
    // the two ends need checking here.
    check_trade(
        &state,
        vault,
        route.trader,
        first.trader_input,
        last.trader_output,
        first.input_mint.key,
        last.output_mint.key,
    )?;

    log_stack_height("dca-vault");
    let result = pipeline_cpi::route(
        &route,
        &RouteArgs {
            amount_in: state.amount_per_swap,
            min_amount_out: state.min_out_per_swap,
            integrator_fee_bps: 0,
        },
        &[&vault_seeds(&state)],
    )?;
    msg!(
        "dca-vault: route sent {}, kept {}",
        result.net_sent,
        result.amount_out
    );
    record(&mut state, now, result.net_sent, result.amount_out);
    state.store(vault)
}

fn record(state: &mut Vault, now: i64, sent: u64, kept: u64) {
    state.last_swap_at = now;
    state.swaps = state.swaps.saturating_add(1);
    state.total_sent = state.total_sent.saturating_add(sent);
    state.total_received = state.total_received.saturating_add(kept);
}

fn withdraw(program_id: &Pubkey, accounts: &[AccountInfo], args: &[u8]) -> ProgramResult {
    let [owner, vault, source, mint, destination, token_program, hook_accounts @ ..] = accounts
    else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    if args.len() != 8 {
        return Err(VaultError::BadInstruction.into());
    }
    let state = Vault::load(program_id, vault)?;
    if !owner.is_signer || *owner.key != state.owner {
        return Err(VaultError::NotTheOwner.into());
    }
    if *token_program.key != TOKEN_PROGRAM_ID && *token_program.key != TOKEN_2022_PROGRAM_ID {
        return Err(ProgramError::IncorrectProgramId);
    }
    check_token_account(source, vault.key, mint.key)?;
    // TransferChecked (12) has the same layout in SPL Token and Token-2022; the token
    // program checks that it owns the accounts and runs the mint's hook, if any.
    let decimals = *mint
        .try_borrow_data()?
        .get(44)
        .ok_or(VaultError::WrongMint)?;
    let mut data = vec![12];
    data.extend_from_slice(args);
    data.push(decimals);
    let mut metas = vec![
        AccountMeta::new(*source.key, false),
        AccountMeta::new_readonly(*mint.key, false),
        AccountMeta::new(*destination.key, false),
        AccountMeta::new_readonly(*vault.key, true),
    ];
    metas.extend(hook_accounts.iter().map(|account| AccountMeta {
        pubkey: *account.key,
        is_signer: false,
        is_writable: account.is_writable,
    }));
    let mut infos = vec![
        source.clone(),
        mint.clone(),
        destination.clone(),
        vault.clone(),
        token_program.clone(),
    ];
    infos.extend(hook_accounts.iter().cloned());
    invoke_signed(
        &Instruction {
            program_id: *token_program.key,
            accounts: metas,
            data,
        },
        &infos,
        &[&vault_seeds(&state)],
    )
}

/// The vault PDA's signer seeds, with its bump.
fn vault_seeds(state: &Vault) -> [&[u8]; 5] {
    [
        VAULT_SEED,
        state.owner.as_ref(),
        state.input_mint.as_ref(),
        state.output_mint.as_ref(),
        std::slice::from_ref(&state.bump),
    ]
}

fn read_u64(data: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(data[at..at + 8].try_into().unwrap())
}
