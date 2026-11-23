//! A deliberately hostile strategy for testing the router. Written without a framework,
//! which also shows that a native program can implement the Pipeline interface.
//!
//! Behaviour is chosen per swap by the first byte of `TradeRequest::extra` (see
//! [`Behaviour`]). Quotes are 1:1 unless a behaviour says otherwise.
//!
//! Market state: PDA `["evil", mint_a, mint_b]` holding mint_a, mint_b, vault_a,
//! vault_b, bump and an `info_mode` byte that makes `pipeline_market_info` lie.
//! Vaults are token accounts owned by the state PDA, created by the test beforehand.
//!
//! `pipeline_seed_liquidity` has its own behaviours (see [`SEED_PRICED`]), including a
//! seeded market that quotes from recorded reserves, for testing seeders' price checks.

use pipeline_strategy_interface::{self as psi, discriminator, TradeMode};
use solana_program::{
    account_info::AccountInfo,
    entrypoint,
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    msg,
    program::{invoke, invoke_signed, set_return_data},
    program_error::ProgramError,
    pubkey::Pubkey,
    rent::Rent,
    sysvar::Sysvar,
};

entrypoint!(process_instruction);

/// Forwards an instruction to the router, signed by this program's registration PDA.
pub const FORWARD_TO_ROUTER: [u8; 8] = *b"evilfwd!";

#[repr(u8)]
pub enum Behaviour {
    /// Pays out 1:1.
    Honest = 0,
    /// Tries to pull extra input from the trader.
    TakeMore = 1,
    /// Pays out a single unit.
    ShortOutput = 2,
    /// Pays out 1:1 and refunds more input than was sent.
    RefundMore = 3,
    /// Calls back into the router.
    Reenter = 4,
    /// Sends the output to another account (the first strategy account after the vault).
    WrongDestination = 5,
    /// Tries to move the market's fees using the market signer it was given.
    StealFees = 6,
    /// Quotes 2:1 but pays 1:1.
    QuoteLies = 7,
    /// Quotes forever.
    QuoteHog = 8,
    /// Tries to move the trader's lamports.
    TakeLamports = 9,
    /// Pulls one more unit of input from the trader using the market signer, which works
    /// only if the market PDA is the input mint's Token-2022 permanent delegate.
    PullAsDelegate = 10,
    /// Pays out 1:1, then takes one more unit than it paid back out of the trader's
    /// output account using the market signer, which works only if the market PDA is
    /// the output mint's Token-2022 permanent delegate.
    PullOutputAsDelegate = 11,
}

/// `info_mode` values that attack the setup payer or other accounts during setup.
pub const INIT_DRAINS_PAYER: u8 = 4;
pub const INIT_ASSIGNS_PAYER: u8 = 5;
pub const INIT_ALLOCATES_PAYER: u8 = 6;
/// Tries to approve the creator's token account (account 8) to the state PDA, with the
/// creator's wallet (account 9) as authority and the token program as account 10.
pub const INIT_APPROVES_CREATOR_TOKENS: u8 = 7;
/// `info_mode` values that declare a fee mint in `pipeline_market_info`: mint B, an index
/// past the last mint, and mint B followed by a stray byte.
pub const INFO_DECLARES_MINT_B: u8 = 8;
pub const INFO_DECLARES_OUT_OF_RANGE: u8 = 9;
pub const INFO_DECLARES_WITH_TRAILING_BYTE: u8 = 10;

pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    let (tag, rest) = data
        .split_at_checked(8)
        .ok_or(ProgramError::InvalidInstructionData)?;
    let tag: [u8; 8] = tag.try_into().unwrap();
    match tag {
        discriminator::INIT_MARKET => init_market(program_id, accounts, rest),
        discriminator::MARKET_INFO => market_info(accounts),
        discriminator::RESOLVE_ACCOUNTS => {
            let request: psi::ResolveRequest = decode(rest)?;
            let state = State::load(&accounts[psi::trade_accounts::MARKET_STATE])?;
            let output_vault = state.vaults()[request.trade.output_index as usize];
            let result = psi::ResolveResult {
                accounts: vec![psi::AccountRef {
                    address: output_vault.to_bytes(),
                    writable: true,
                }],
                complete: true,
            };
            set_return_data(&borsh::to_vec(&result).unwrap());
            Ok(())
        }
        discriminator::QUOTE => {
            let request: psi::TradeRequest = decode(rest)?;
            if let Some(priced) = accounts
                .get(psi::trade_accounts::MARKET_STATE)
                .and_then(Priced::load)
            {
                let quote = priced.quote(&request);
                set_return_data(&borsh::to_vec(&quote).unwrap());
                return Ok(());
            }
            let behaviour = request.extra.first().copied().unwrap_or(0);
            let mut out = request.amount;
            if behaviour == Behaviour::QuoteLies as u8 {
                out *= 2;
            }
            if behaviour == Behaviour::QuoteHog as u8 {
                let mut x: u64 = 1;
                loop {
                    x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1));
                    if x == 0 {
                        break;
                    }
                }
            }
            let quote = psi::QuoteResult {
                amount_in: request.amount,
                amount_out: out,
            };
            set_return_data(&borsh::to_vec(&quote).unwrap());
            Ok(())
        }
        discriminator::SWAP => swap(program_id, accounts, decode(rest)?),
        discriminator::SEED_LIQUIDITY => seed_liquidity(program_id, accounts, decode(rest)?),
        FORWARD_TO_ROUTER => forward_to_router(program_id, accounts, rest),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

fn decode<T: borsh::BorshDeserialize>(data: &[u8]) -> Result<T, ProgramError> {
    T::try_from_slice(data).map_err(|_| ProgramError::InvalidInstructionData)
}

struct State {
    mint_a: Pubkey,
    mint_b: Pubkey,
    vault_a: Pubkey,
    vault_b: Pubkey,
    bump: u8,
    info_mode: u8,
}

const STATE_LEN: usize = 32 * 4 + 2;

impl State {
    fn load(account: &AccountInfo) -> Result<State, ProgramError> {
        let data = account.try_borrow_data()?;
        if data.len() < STATE_LEN {
            return Err(ProgramError::InvalidAccountData);
        }
        let key = |i: usize| Pubkey::new_from_array(data[i * 32..(i + 1) * 32].try_into().unwrap());
        Ok(State {
            mint_a: key(0),
            mint_b: key(1),
            vault_a: key(2),
            vault_b: key(3),
            bump: data[128],
            info_mode: data[129],
        })
    }

    fn vaults(&self) -> [Pubkey; 2] {
        [self.vault_a, self.vault_b]
    }
}

/// Accounts: market signer, state (w), creator (s, w), system program, mint_a, mint_b,
/// vault_a, vault_b. Data: Borsh `Vec<u8>` whose first byte is `info_mode`.
fn init_market(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let [_signer, state, creator, system_program, mint_a, mint_b, vault_a, vault_b, ..] = accounts
    else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let init: Vec<u8> = decode(data)?;
    let (expected, bump) = Pubkey::find_program_address(
        &[b"evil", mint_a.key.as_ref(), mint_b.key.as_ref()],
        program_id,
    );
    if expected != *state.key {
        return Err(ProgramError::InvalidSeeds);
    }
    let lamports = Rent::get()?.minimum_balance(STATE_LEN);
    let mut create = 0u32.to_le_bytes().to_vec();
    create.extend_from_slice(&lamports.to_le_bytes());
    create.extend_from_slice(&(STATE_LEN as u64).to_le_bytes());
    create.extend_from_slice(program_id.as_ref());
    invoke_signed(
        &Instruction {
            program_id: *system_program.key,
            accounts: vec![
                AccountMeta::new(*creator.key, true),
                AccountMeta::new(*state.key, true),
            ],
            data: create,
        },
        &[creator.clone(), state.clone(), system_program.clone()],
        &[&[b"evil", mint_a.key.as_ref(), mint_b.key.as_ref(), &[bump]]],
    )?;
    let mut bytes = state.try_borrow_mut_data()?;
    for (i, key) in [mint_a.key, mint_b.key, vault_a.key, vault_b.key]
        .iter()
        .enumerate()
    {
        bytes[i * 32..(i + 1) * 32].copy_from_slice(key.as_ref());
    }
    bytes[128] = bump;
    bytes[129] = init.first().copied().unwrap_or(0);
    drop(bytes);

    let system = |data: Vec<u8>, metas: Vec<AccountMeta>, infos: &[AccountInfo]| {
        invoke(
            &Instruction {
                program_id: *system_program.key,
                accounts: metas,
                data,
            },
            infos,
        )
    };
    match bytes_mode(&init) {
        INIT_DRAINS_PAYER => {
            let mut data = 2u32.to_le_bytes().to_vec();
            data.extend_from_slice(&1_000_000_000u64.to_le_bytes());
            system(
                data,
                vec![
                    AccountMeta::new(*creator.key, true),
                    AccountMeta::new(*state.key, false),
                ],
                &[creator.clone(), state.clone(), system_program.clone()],
            )?;
        }
        INIT_ASSIGNS_PAYER => {
            let mut data = 1u32.to_le_bytes().to_vec();
            data.extend_from_slice(program_id.as_ref());
            system(
                data,
                vec![AccountMeta::new(*creator.key, true)],
                &[creator.clone(), system_program.clone()],
            )?;
        }
        INIT_ALLOCATES_PAYER => {
            let mut data = 8u32.to_le_bytes().to_vec();
            data.extend_from_slice(&1u64.to_le_bytes());
            system(
                data,
                vec![AccountMeta::new(*creator.key, true)],
                &[creator.clone(), system_program.clone()],
            )?;
        }
        INIT_APPROVES_CREATOR_TOKENS => {
            let token_account = accounts.get(8).ok_or(ProgramError::NotEnoughAccountKeys)?;
            let wallet = accounts.get(9).ok_or(ProgramError::NotEnoughAccountKeys)?;
            let token_program = accounts.get(10).ok_or(ProgramError::NotEnoughAccountKeys)?;
            let mut data = vec![4u8]; // Approve
            data.extend_from_slice(&u64::MAX.to_le_bytes());
            invoke(
                &Instruction {
                    program_id: *token_program.key,
                    accounts: vec![
                        AccountMeta::new(*token_account.key, false),
                        AccountMeta::new_readonly(*state.key, false),
                        AccountMeta::new_readonly(*wallet.key, true),
                    ],
                    data,
                },
                &[
                    token_account.clone(),
                    state.clone(),
                    wallet.clone(),
                    token_program.clone(),
                ],
            )?;
        }
        _ => {}
    }
    Ok(())
}

/// `pipeline_seed_liquidity` behaviours, chosen by the first byte of `SeedRequest::data`.
/// Like [`SEED_TAKES_ESCROWS`], then also reassigns the rent payer to itself.
pub const SEED_ASSIGNS_PAYER: u8 = 0;
pub const SEED_DOES_NOTHING: u8 = 1;
/// Creates its market state at `["evil-seeded", seeder]`, then sends everything in the
/// escrows to the two accounts after the per-mint accounts.
pub const SEED_TAKES_ESCROWS: u8 = 2;
/// A two-mint market that quotes like a fee-free constant-product pool, with knobs for
/// misbehaving. `SeedRequest::data` is this byte, then three little-endian u16 values in
/// basis points (10,000 is honest):
/// - `take_b_bps`: the share of `amounts[1]` it takes (it takes all of `amounts[0]`);
/// - `price_bps`: scales the mint-B reserve it quotes from, so it quotes off its real
///   price;
/// - `sell_bps`: scales the output of trades paying out mint B, so buys and sells are
///   priced asymmetrically;
///
/// then, optionally, a little-endian u64 `held_a`: mint-A tokens it keeps aside as a
/// separate reserve (as a pool with a market-making reserve does), so its price uses only
/// the rest. Omitted means 0.
///
/// It sends what it takes to the two accounts after the per-mint accounts and records
/// it, less `held_a`, as its reserves. Its `pipeline_market_info` declares the market
/// state's associated token accounts as its vaults, but it never checks that the seeder
/// named those: an otherwise honest pool that trusts its caller's accounts.
pub const SEED_PRICED: u8 = 3;
/// [`SEED_PRICED`], but the market lists its mints quote first: its market info declares
/// mint B then mint A, and its quotes and resolved accounts use those indices. Seeders
/// must not assume a target lists its mints in seeding order.
pub const SEED_PRICED_QUOTE_FIRST: u8 = 4;

/// The associated token account program, for a [`SEED_PRICED`] market's vaults.
const ASSOCIATED_TOKEN_PROGRAM: Pubkey =
    Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

/// The marker after the plain state that makes a market state [`Priced`].
const PRICED_MARKER: [u8; 8] = *b"pricedcp";
const PRICED_LEN: usize = STATE_LEN + 8 + 8 + 8 + 2 + 2 + 1;

/// A [`SEED_PRICED`] market's reserves and knobs, stored after the plain state.
struct Priced {
    reserve_a: u64,
    reserve_b: u64,
    price_bps: u16,
    sell_bps: u16,
    /// The market's index of mint A: 0, or 1 when it lists its mints quote first.
    index_a: u8,
}

impl Priced {
    fn load(account: &AccountInfo) -> Option<Priced> {
        let data = account.try_borrow_data().ok()?;
        if data.len() < PRICED_LEN || data[STATE_LEN..STATE_LEN + 8] != PRICED_MARKER {
            return None;
        }
        let u64_at = |at: usize| u64::from_le_bytes(data[at..at + 8].try_into().unwrap());
        let u16_at = |at: usize| u16::from_le_bytes(data[at..at + 2].try_into().unwrap());
        let base = STATE_LEN + 8;
        Some(Priced {
            reserve_a: u64_at(base),
            reserve_b: u64_at(base + 8),
            price_bps: u16_at(base + 16),
            sell_bps: u16_at(base + 18),
            index_a: data[base + 20],
        })
    }

    fn quote(&self, request: &psi::TradeRequest) -> psi::QuoteResult {
        let bps = |amount: u64, bps: u16| (amount as u128 * bps as u128 / 10_000) as u64;
        let reserve_b = bps(self.reserve_b, self.price_bps);
        let (reserve_in, reserve_out) = if request.input_index == self.index_a {
            (self.reserve_a, reserve_b)
        } else {
            (reserve_b, self.reserve_a)
        };
        let amount = request.amount as u128;
        let out = (reserve_out as u128 * amount / (reserve_in as u128 + amount)) as u64;
        let out = if request.output_index != self.index_a {
            bps(out, self.sell_bps)
        } else {
            out
        };
        psi::QuoteResult {
            amount_in: request.amount,
            amount_out: out,
        }
    }
}

fn seed_liquidity(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    request: psi::SeedRequest,
) -> ProgramResult {
    use psi::seed_accounts as seed;
    let seeder = &accounts[seed::SEEDER];
    let state = &accounts[seed::MARKET_STATE];
    let payer = &accounts[seed::PAYER];
    let system_program = &accounts[seed::SYSTEM_PROGRAM];
    let behaviour = request.data.first().copied().unwrap_or(SEED_ASSIGNS_PAYER);
    if behaviour == SEED_DOES_NOTHING {
        return Ok(());
    }
    let knob = |i: usize| {
        request
            .data
            .get(1 + 2 * i..3 + 2 * i)
            .map_or(10_000, |b| u16::from_le_bytes([b[0], b[1]]))
    };
    let quote_first = behaviour == SEED_PRICED_QUOTE_FIRST;
    let priced = behaviour == SEED_PRICED || quote_first;
    let state_len = if priced { PRICED_LEN } else { STATE_LEN };
    // Create a market state at `["evil-seeded", seeder]`, paid by the rent payer.
    let (expected, bump) =
        Pubkey::find_program_address(&[b"evil-seeded", seeder.key.as_ref()], program_id);
    if expected != *state.key {
        return Err(ProgramError::InvalidSeeds);
    }
    let lamports = Rent::get()?.minimum_balance(state_len);
    let mut create = 0u32.to_le_bytes().to_vec();
    create.extend_from_slice(&lamports.to_le_bytes());
    create.extend_from_slice(&(state_len as u64).to_le_bytes());
    create.extend_from_slice(program_id.as_ref());
    invoke_signed(
        &Instruction {
            program_id: *system_program.key,
            accounts: vec![
                AccountMeta::new(*payer.key, true),
                AccountMeta::new(*state.key, true),
            ],
            data: create,
        },
        &[payer.clone(), state.clone(), system_program.clone()],
        &[&[b"evil-seeded", seeder.key.as_ref(), &[bump]]],
    )?;
    let destinations = seed::LEN + 2 * seed::PER_MINT;
    let mut taken = vec![];
    for i in 0..request.amounts.len() {
        let base = seed::LEN + i * seed::PER_MINT;
        let escrow = &accounts[base + 1];
        let balance = u64::from_le_bytes(escrow.try_borrow_data()?[64..72].try_into().unwrap());
        let amount = match (priced, i) {
            (false, _) => balance,
            (true, 0) => request.amounts[0],
            (true, _) => (request.amounts[i] as u128 * knob(0) as u128 / 10_000) as u64,
        };
        token_transfer(
            &accounts[base + 2],
            escrow,
            &accounts[base],
            &accounts[destinations + i],
            seeder,
            amount,
            &[],
        )?;
        taken.push(amount);
    }
    // A seeded market records its mints and the vaults its market info declares: the
    // accounts it sent to, or for a priced market, its state's associated token accounts.
    {
        let mut data = state.try_borrow_mut_data()?;
        for i in 0..2.min(request.amounts.len()) {
            let base = seed::LEN + i * seed::PER_MINT;
            let mint = accounts[base].key;
            let vault = if priced {
                Pubkey::find_program_address(
                    &[
                        state.key.as_ref(),
                        accounts[base + 2].key.as_ref(),
                        mint.as_ref(),
                    ],
                    &ASSOCIATED_TOKEN_PROGRAM,
                )
                .0
            } else {
                *accounts[destinations + i].key
            };
            let slot = if quote_first { 1 - i } else { i };
            data[slot * 32..(slot + 1) * 32].copy_from_slice(mint.as_ref());
            data[(2 + slot) * 32..(3 + slot) * 32].copy_from_slice(vault.as_ref());
        }
    }
    if priced {
        let mut data = state.try_borrow_mut_data()?;
        let base = STATE_LEN + 8;
        data[STATE_LEN..base].copy_from_slice(&PRICED_MARKER);
        // Tokens held aside as a separate reserve don't price the pool.
        let held_a = request
            .data
            .get(7..15)
            .map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap()));
        let pool_a = taken[0].saturating_sub(held_a);
        data[base..base + 8].copy_from_slice(&pool_a.to_le_bytes());
        data[base + 8..base + 16].copy_from_slice(&taken[1].to_le_bytes());
        data[base + 16..base + 18].copy_from_slice(&knob(1).to_le_bytes());
        data[base + 18..base + 20].copy_from_slice(&knob(2).to_le_bytes());
        data[base + 20] = quote_first as u8;
    }
    if behaviour == SEED_ASSIGNS_PAYER {
        let mut data = 1u32.to_le_bytes().to_vec();
        data.extend_from_slice(program_id.as_ref());
        invoke(
            &Instruction {
                program_id: *system_program.key,
                accounts: vec![AccountMeta::new(*payer.key, true)],
                data,
            },
            &[payer.clone(), system_program.clone()],
        )?;
    }
    Ok(())
}

fn bytes_mode(init: &[u8]) -> u8 {
    init.first().copied().unwrap_or(0)
}

fn market_info(accounts: &[AccountInfo]) -> ProgramResult {
    let state = State::load(&accounts[0])?;
    let (mints, vaults) = match state.info_mode {
        1 => (
            vec![state.mint_a, state.mint_a],
            vec![state.vault_a, state.vault_b],
        ),
        2 => (vec![state.mint_a], vec![state.vault_a]),
        3 => (vec![state.mint_a, state.mint_b], vec![state.vault_a]),
        _ => (
            vec![state.mint_a, state.mint_b],
            vec![state.vault_a, state.vault_b],
        ),
    };
    let info = psi::MarketInfo {
        mints: mints.iter().map(|k| k.to_bytes()).collect(),
        vaults: vaults.iter().map(|k| k.to_bytes()).collect(),
        supports_exact_out: false,
    };
    let data = match state.info_mode {
        INFO_DECLARES_MINT_B => psi::encode_market_info(&info, Some(1)),
        INFO_DECLARES_OUT_OF_RANGE => psi::encode_market_info(&info, Some(2)),
        INFO_DECLARES_WITH_TRAILING_BYTE => {
            let mut data = psi::encode_market_info(&info, Some(1));
            data.push(7);
            data
        }
        _ => borsh::to_vec(&info).unwrap(),
    };
    set_return_data(&data);
    Ok(())
}

fn swap<'a>(
    program_id: &Pubkey,
    accounts: &[AccountInfo<'a>],
    request: psi::SwapRequest,
) -> ProgramResult {
    use psi::trade_accounts as ix;
    if accounts.len() < ix::LEN + 1 {
        return Err(ProgramError::NotEnoughAccountKeys);
    }
    let state_info = &accounts[ix::MARKET_STATE];
    let state = State::load(state_info)?;
    let output_vault = &accounts[ix::LEN];
    let extra = &accounts[ix::LEN + 1..];
    let seeds: &[&[u8]] = &[
        b"evil",
        state.mint_a.as_ref(),
        state.mint_b.as_ref(),
        &[state.bump],
    ];
    let behaviour = request.trade.extra.first().copied().unwrap_or(0);
    if request.trade.mode != TradeMode::ExactIn {
        return Err(ProgramError::InvalidArgument);
    }
    let amount = request.amount_sent;

    let pay = |to: &AccountInfo<'a>, amount: u64| {
        token_transfer(
            &accounts[ix::OUTPUT_TOKEN_PROGRAM],
            output_vault,
            &accounts[ix::OUTPUT_MINT],
            to,
            state_info,
            amount,
            &[seeds],
        )
    };

    match behaviour {
        b if b == Behaviour::TakeMore as u8 => {
            pay(&accounts[ix::TRADER_OUTPUT], amount)?;
            // The trader is not a signer here, so this must fail.
            token_transfer(
                &accounts[ix::INPUT_TOKEN_PROGRAM],
                &accounts[ix::TRADER_INPUT],
                &accounts[ix::INPUT_MINT],
                &accounts[ix::INPUT_VAULT],
                &accounts[ix::TRADER],
                1,
                &[],
            )
        }
        b if b == Behaviour::ShortOutput as u8 => pay(&accounts[ix::TRADER_OUTPUT], 1),
        b if b == Behaviour::RefundMore as u8 => {
            pay(&accounts[ix::TRADER_OUTPUT], amount)?;
            token_transfer(
                &accounts[ix::INPUT_TOKEN_PROGRAM],
                &accounts[ix::INPUT_VAULT],
                &accounts[ix::INPUT_MINT],
                &accounts[ix::TRADER_INPUT],
                state_info,
                amount + 5,
                &[seeds],
            )
        }
        b if b == Behaviour::Reenter as u8 => {
            let router = extra.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
            msg!("calling back into the router");
            invoke(
                &Instruction {
                    program_id: *router.key,
                    accounts: vec![],
                    data: vec![0; 8],
                },
                &[router.clone()],
            )
        }
        b if b == Behaviour::WrongDestination as u8 => {
            let elsewhere = extra.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
            pay(elsewhere, amount)
        }
        b if b == Behaviour::StealFees as u8 => {
            pay(&accounts[ix::TRADER_OUTPUT], amount)?;
            let fee_account = extra.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
            // The market signer signed this call; try to use it on the fee account.
            token_transfer(
                &accounts[ix::INPUT_TOKEN_PROGRAM],
                fee_account,
                &accounts[ix::INPUT_MINT],
                &accounts[ix::INPUT_VAULT],
                &accounts[ix::MARKET_SIGNER],
                1,
                &[],
            )
        }
        b if b == Behaviour::PullAsDelegate as u8 => {
            pay(&accounts[ix::TRADER_OUTPUT], amount)?;
            token_transfer(
                &accounts[ix::INPUT_TOKEN_PROGRAM],
                &accounts[ix::TRADER_INPUT],
                &accounts[ix::INPUT_MINT],
                &accounts[ix::INPUT_VAULT],
                &accounts[ix::MARKET_SIGNER],
                1,
                &[],
            )
        }
        b if b == Behaviour::PullOutputAsDelegate as u8 => {
            pay(&accounts[ix::TRADER_OUTPUT], amount)?;
            token_transfer(
                &accounts[ix::OUTPUT_TOKEN_PROGRAM],
                &accounts[ix::TRADER_OUTPUT],
                &accounts[ix::OUTPUT_MINT],
                output_vault,
                &accounts[ix::MARKET_SIGNER],
                amount + 1,
                &[],
            )
        }
        b if b == Behaviour::TakeLamports as u8 => {
            pay(&accounts[ix::TRADER_OUTPUT], amount)?;
            let system_program = extra.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
            let mut data = 2u32.to_le_bytes().to_vec();
            data.extend_from_slice(&1_000_000u64.to_le_bytes());
            invoke(
                &Instruction {
                    program_id: *system_program.key,
                    accounts: vec![
                        AccountMeta::new(*accounts[ix::TRADER].key, true),
                        AccountMeta::new(*state_info.key, false),
                    ],
                    data,
                },
                &[
                    accounts[ix::TRADER].clone(),
                    state_info.clone(),
                    system_program.clone(),
                ],
            )
        }
        _ => {
            let _ = program_id;
            pay(&accounts[ix::TRADER_OUTPUT], amount)
        }
    }
}

/// Accounts: router program, then the router instruction's accounts. Data: the router
/// instruction data. Our `["pipeline-register"]` PDA signs.
fn forward_to_router(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let (router, rest) = accounts
        .split_first()
        .ok_or(ProgramError::NotEnoughAccountKeys)?;
    let (register_pda, bump) = Pubkey::find_program_address(&[psi::seeds::REGISTER], program_id);
    let metas = rest
        .iter()
        .map(|a| {
            let signer = a.is_signer || *a.key == register_pda;
            if a.is_writable {
                AccountMeta::new(*a.key, signer)
            } else {
                AccountMeta::new_readonly(*a.key, signer)
            }
        })
        .collect();
    let mut infos = rest.to_vec();
    infos.push(router.clone());
    invoke_signed(
        &Instruction {
            program_id: *router.key,
            accounts: metas,
            data: data.to_vec(),
        },
        &infos,
        &[&[psi::seeds::REGISTER, &[bump]]],
    )
}

fn token_transfer<'a>(
    token_program: &AccountInfo<'a>,
    from: &AccountInfo<'a>,
    mint: &AccountInfo<'a>,
    to: &AccountInfo<'a>,
    authority: &AccountInfo<'a>,
    amount: u64,
    signer_seeds: &[&[&[u8]]],
) -> ProgramResult {
    let decimals = mint.try_borrow_data()?[44];
    let mut data = vec![12u8];
    data.extend_from_slice(&amount.to_le_bytes());
    data.push(decimals);
    invoke_signed(
        &Instruction {
            program_id: *token_program.key,
            accounts: vec![
                AccountMeta::new(*from.key, false),
                AccountMeta::new_readonly(*mint.key, false),
                AccountMeta::new(*to.key, false),
                AccountMeta::new_readonly(*authority.key, true),
            ],
            data,
        },
        &[
            from.clone(),
            mint.clone(),
            to.clone(),
            authority.clone(),
            token_program.clone(),
        ],
        signer_seeds,
    )
}
