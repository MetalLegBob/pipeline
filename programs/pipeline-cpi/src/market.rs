//! Reading the router's `Market` record and checking token accounts, on-chain.

use pipeline_strategy_interface::MAX_MARKET_MINTS;
use solana_program::account_info::AccountInfo;
use solana_program::program_error::ProgramError;
use solana_program::pubkey::Pubkey;

use crate::{CpiError, ROUTER_ID};

/// Anchor's discriminator for the router's `Market`: `sha256("account:Market")[..8]`.
pub const MARKET_DISCRIMINATOR: [u8; 8] = [219, 190, 213, 55, 0, 227, 198, 154];
/// `Market::fee_mint_index` when the market has no fee mint.
const NO_FEE_MINT: u8 = u8::MAX;

// Byte offsets in the router's `Market` account (Borsh, after the discriminator).
const STRATEGY_PROGRAM: usize = 8;
const MARKET_STATE: usize = STRATEGY_PROGRAM + 32;
const MINT_COUNT: usize = MARKET_STATE + 32;
const MINTS: usize = MINT_COUNT + 1;
const VAULTS: usize = MINTS + 32 * MAX_MARKET_MINTS;
const SUPPORTS_EXACT_OUT: usize = VAULTS + 32 * MAX_MARKET_MINTS;
const FEE_MINT_INDEX: usize = SUPPORTS_EXACT_OUT + 1 + 32 + 8 + 1 + 1;
/// The length of a `Market` account.
pub const MARKET_LEN: usize = FEE_MINT_INDEX + 1;

/// Which side of a trade pays Pipeline's fee.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeeSide {
    /// In the input mint, on top of what the strategy receives.
    Input,
    /// In the output mint, out of what arrived: the output is the market's fee mint.
    Output,
}

/// The parts of a router `Market` record a calling program needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarketView {
    pub strategy_program: Pubkey,
    pub market_state: Pubkey,
    pub mints: Vec<Pubkey>,
    /// The market's input vault for each mint, in the same order.
    pub vaults: Vec<Pubkey>,
    pub supports_exact_out: bool,
    /// The mint Pipeline takes its fee in when it is one side of a trade, if any.
    pub fee_mint: Option<Pubkey>,
}

impl MarketView {
    /// The side a trade into `output_mint` pays Pipeline's fee on. The swap's
    /// `fee_account` is the market's fee account in that side's mint.
    pub fn fee_side(&self, output_mint: &Pubkey) -> FeeSide {
        if self.fee_mint == Some(*output_mint) {
            FeeSide::Output
        } else {
            FeeSide::Input
        }
    }

    /// The vault input in `mint` is sent to.
    pub fn vault_for(&self, mint: &Pubkey) -> Option<Pubkey> {
        let index = self.mints.iter().position(|m| m == mint)?;
        Some(self.vaults[index])
    }
}

/// Reads a router `Market` account, checking that the router owns it and that it is a
/// `Market`.
pub fn read_market(market: &AccountInfo) -> Result<MarketView, ProgramError> {
    if *market.owner != ROUTER_ID {
        return Err(CpiError::NotAMarket.into());
    }
    let data = market.try_borrow_data()?;
    decode_market(&data).ok_or_else(|| CpiError::NotAMarket.into())
}

pub(crate) fn decode_market(data: &[u8]) -> Option<MarketView> {
    if data.len() < MARKET_LEN || data[..8] != MARKET_DISCRIMINATOR {
        return None;
    }
    let key = |at: usize| Pubkey::new_from_array(data[at..at + 32].try_into().unwrap());
    let count = data[MINT_COUNT] as usize;
    if count > MAX_MARKET_MINTS {
        return None;
    }
    let fee_mint_index = data[FEE_MINT_INDEX];
    let fee_mint = if fee_mint_index == NO_FEE_MINT {
        None
    } else if (fee_mint_index as usize) < count {
        Some(key(MINTS + 32 * fee_mint_index as usize))
    } else {
        return None;
    };
    Some(MarketView {
        strategy_program: key(STRATEGY_PROGRAM),
        market_state: key(MARKET_STATE),
        mints: (0..count).map(|i| key(MINTS + 32 * i)).collect(),
        vaults: (0..count).map(|i| key(VAULTS + 32 * i)).collect(),
        supports_exact_out: data[SUPPORTS_EXACT_OUT] != 0,
        fee_mint,
    })
}

/// SPL Token.
pub const TOKEN_PROGRAM_ID: Pubkey =
    solana_program::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
/// Token-2022.
pub const TOKEN_2022_PROGRAM_ID: Pubkey =
    solana_program::pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

/// Checks that `account` is an initialised SPL Token or Token-2022 account in `mint`
/// whose owner is `owner`.
///
/// A program that swaps as the trader must check its output account this way: the router
/// measures what arrives in whatever output account it is given, so without the check
/// whoever builds the transaction could take the output.
pub fn check_token_account(
    account: &AccountInfo,
    owner: &Pubkey,
    mint: &Pubkey,
) -> Result<(), ProgramError> {
    // Base layout shared by both token programs: mint, owner, amount, delegate, state.
    // A Token-2022 account with extensions follows it with an account-type byte (2 for
    // a token account), which tells it apart from a mint padded to the same length.
    const OWNER: usize = 32;
    const STATE: usize = 108;
    const BASE_LEN: usize = 165;
    const ACCOUNT_TYPE_ACCOUNT: u8 = 2;
    if *account.owner != TOKEN_PROGRAM_ID && *account.owner != TOKEN_2022_PROGRAM_ID {
        return Err(CpiError::WrongTokenAccount.into());
    }
    let data = account.try_borrow_data()?;
    let matches = data.len() >= BASE_LEN
        && data
            .get(BASE_LEN)
            .is_none_or(|&kind| kind == ACCOUNT_TYPE_ACCOUNT)
        && data[STATE] != 0
        && data[..OWNER] == mint.as_ref()[..]
        && data[OWNER..OWNER + 32] == owner.as_ref()[..];
    if matches {
        Ok(())
    } else {
        Err(CpiError::WrongTokenAccount.into())
    }
}
