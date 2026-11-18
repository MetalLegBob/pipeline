//! A scriptable hostile strategy for fuzzing the Pipeline router.
//!
//! Every entry point that does work runs a small script of operations supplied by the
//! caller, so a fuzzer can make the strategy do anything a real program could: pay or
//! short the trader, refund input, move tokens between any accounts it was given, sign
//! with its state PDA, mint, burn, close, approve, reassign token accounts, move lamports,
//! write account data, set return data, burn compute, fail, or call any program with
//! arbitrary data.
//!
//! - `pipeline_init_market` data (Borsh `Vec<u8>`): `nonce[32] | info_len u16 | info |
//!   script`. Creates the market state at the PDA `["script", nonce]` (paid by the setup
//!   payer), stores `info`, then runs `script`.
//! - `pipeline_market_info` returns the stored `info` bytes verbatim, so the fuzzer
//!   controls exactly what the router decodes.
//! - `pipeline_swap` runs `TradeRequest::extra` as the script.
//! - [`RUN`] (`nonce[32] | script`), called directly, runs a script outside the router,
//!   signed by the state PDA, as a strategy acting on its own would.
//!
//! Scripts are read leniently: every byte string decodes to some sequence of
//! operations, and the script ends where the bytes run out. Account operands are
//! indices into the accounts the program received, taken modulo their number.

use pipeline_strategy_interface::{self as psi, discriminator};
use solana_program::{
    account_info::AccountInfo,
    entrypoint,
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    program::{invoke_signed, set_return_data},
    program_error::ProgramError,
    pubkey::Pubkey,
    rent::Rent,
    sysvar::Sysvar,
};

/// The system program's address (all zeros).
const SYSTEM_PROGRAM_ID: Pubkey = Pubkey::new_from_array([0; 32]);

entrypoint!(process_instruction);

/// Runs a script directly, outside the router.
pub const RUN: [u8; 8] = *b"scriptrn";
pub const STATE_SEED: &[u8] = b"script";
pub const STATE_LEN: usize = 512;
/// Market state layout: nonce, bump, info length, info.
pub const NONCE_AT: usize = 0;
pub const BUMP_AT: usize = 32;
pub const INFO_LEN_AT: usize = 33;
pub const INFO_AT: usize = 35;
pub const MAX_INFO_LEN: usize = STATE_LEN - INFO_AT;

/// Script operations. A tag byte is taken modulo [`OP_COUNT`].
pub mod op {
    /// `from, mint, to, authority, amount`: `TransferChecked` with the mint's owner as
    /// the token program.
    pub const TRANSFER: u8 = 0;
    /// `source, delegate, owner, amount`: `Approve`.
    pub const APPROVE: u8 = 1;
    /// `account, destination, owner`: `CloseAccount`.
    pub const CLOSE: u8 = 2;
    /// `account, new_owner, owner`: `SetAuthority(AccountOwner)`.
    pub const SET_OWNER: u8 = 3;
    /// `account, mint, owner, amount`: `Burn`.
    pub const BURN: u8 = 4;
    /// `mint, account, authority, amount`: `MintTo`.
    pub const MINT_TO: u8 = 5;
    /// `from, to, amount`: moves lamports directly (works only from accounts this
    /// program owns).
    pub const LAMPORTS: u8 = 6;
    /// `from, to, amount`: a system-program transfer signed by `from`.
    pub const SYSTEM_TRANSFER: u8 = 7;
    /// `program, n, n × (account, flags), len, data`: any instruction. Flag bit 0 makes
    /// the account writable, bit 1 a signer.
    pub const INVOKE: u8 = 8;
    /// `account, offset u16, len, bytes`: writes into an account's data.
    pub const WRITE: u8 = 9;
    /// `len, bytes`: sets return data.
    pub const RETURN_DATA: u8 = 10;
    /// Fails the instruction.
    pub const FAIL: u8 = 11;
    /// `n`: burns roughly `n × 1,000` loop iterations of compute.
    pub const BURN_COMPUTE: u8 = 12;
    /// `source, owner`: `Revoke`.
    pub const REVOKE: u8 = 13;
    /// `from, to, authority, amount`: unchecked `Transfer` (no mint).
    pub const TRANSFER_UNCHECKED: u8 = 14;
    /// `from, mint, to, authority, amount, n, n × account`: `TransferChecked` with `n`
    /// extra accounts appended (a transfer hook's program, validation account and extra
    /// accounts), each passed writable if the program received it writable.
    pub const TRANSFER_WITH: u8 = 15;
    /// `account, mint, authority`: `FreezeAccount`.
    pub const FREEZE: u8 = 16;
    /// `account, mint, authority`: `ThawAccount`.
    pub const THAW: u8 = 17;
}
pub const OP_COUNT: u8 = 18;

/// How an amount operand is encoded: a selector byte, then its payload.
pub mod amount {
    /// A little-endian u64 follows.
    pub const LITERAL: u8 = 0;
    /// A little-endian i16 follows, added to the swap's `amount_sent` (saturating).
    pub const SENT_PLUS: u8 = 1;
    /// An account index follows; the amount is that token account's balance.
    pub const BALANCE_OF: u8 = 2;
    /// One byte follows, the amount itself.
    pub const SMALL: u8 = 3;
    /// A little-endian i16 follows, added to the swap request's `amount` (the exact
    /// output wanted, in exact-out mode).
    pub const TRADE_PLUS: u8 = 4;
}

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
        discriminator::MARKET_INFO => {
            let state = accounts.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
            let data = state.try_borrow_data()?;
            let info = stored_info(&data).ok_or(ProgramError::InvalidAccountData)?;
            set_return_data(info);
            Ok(())
        }
        discriminator::QUOTE => {
            let request: psi::TradeRequest = decode(rest)?;
            let quote = psi::QuoteResult {
                amount_in: request.amount,
                amount_out: request.amount,
            };
            set_return_data(&borsh::to_vec(&quote).unwrap());
            Ok(())
        }
        discriminator::RESOLVE_ACCOUNTS => {
            let result = psi::ResolveResult {
                accounts: vec![],
                complete: true,
            };
            set_return_data(&borsh::to_vec(&result).unwrap());
            Ok(())
        }
        discriminator::SWAP => {
            let request: psi::SwapRequest = decode(rest)?;
            let state = accounts
                .get(psi::trade_accounts::MARKET_STATE)
                .ok_or(ProgramError::NotEnoughAccountKeys)?;
            let (nonce, bump) = load_seeds(program_id, state)?;
            Script {
                accounts,
                nonce,
                bump,
                amount_sent: request.amount_sent,
                trade_amount: request.trade.amount,
            }
            .run(&request.trade.extra)
        }
        RUN => {
            let nonce: [u8; 32] = rest
                .get(..32)
                .ok_or(ProgramError::InvalidInstructionData)?
                .try_into()
                .unwrap();
            let (_, bump) = Pubkey::find_program_address(&[STATE_SEED, &nonce], program_id);
            Script {
                accounts,
                nonce,
                bump,
                amount_sent: 0,
                trade_amount: 0,
            }
            .run(&rest[32..])
        }
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

fn decode<T: borsh::BorshDeserialize>(data: &[u8]) -> Result<T, ProgramError> {
    T::try_from_slice(data).map_err(|_| ProgramError::InvalidInstructionData)
}

fn stored_info(data: &[u8]) -> Option<&[u8]> {
    let len = u16::from_le_bytes(data.get(INFO_LEN_AT..INFO_AT)?.try_into().ok()?) as usize;
    data.get(INFO_AT..INFO_AT + len)
}

/// The state PDA's nonce and bump, checked against its address.
fn load_seeds(program_id: &Pubkey, state: &AccountInfo) -> Result<([u8; 32], u8), ProgramError> {
    let data = state.try_borrow_data()?;
    if data.len() < STATE_LEN || state.owner != program_id {
        return Err(ProgramError::InvalidAccountData);
    }
    let nonce: [u8; 32] = data[NONCE_AT..NONCE_AT + 32].try_into().unwrap();
    let bump = data[BUMP_AT];
    let expected = Pubkey::create_program_address(&[STATE_SEED, &nonce, &[bump]], program_id)
        .map_err(|_| ProgramError::InvalidSeeds)?;
    if expected != *state.key {
        return Err(ProgramError::InvalidSeeds);
    }
    Ok((nonce, bump))
}

/// Accounts: market signer, state (w), setup payer (s, w), system program, then any.
fn init_market(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let [_signer, state, payer, system, ..] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let init: Vec<u8> = decode(data)?;
    let nonce: [u8; 32] = init
        .get(..32)
        .ok_or(ProgramError::InvalidInstructionData)?
        .try_into()
        .unwrap();
    let info_len = init
        .get(32..34)
        .map_or(0, |b| u16::from_le_bytes([b[0], b[1]]) as usize)
        .min(MAX_INFO_LEN);
    let info_end = (34 + info_len).min(init.len());
    let info = init.get(34..info_end).unwrap_or(&[]);
    let script = init.get(info_end..).unwrap_or(&[]);

    let (expected, bump) = Pubkey::find_program_address(&[STATE_SEED, &nonce], program_id);
    if expected != *state.key {
        return Err(ProgramError::InvalidSeeds);
    }
    if state.owner != program_id {
        let mut create = 0u32.to_le_bytes().to_vec();
        create.extend_from_slice(&Rent::get()?.minimum_balance(STATE_LEN).to_le_bytes());
        create.extend_from_slice(&(STATE_LEN as u64).to_le_bytes());
        create.extend_from_slice(program_id.as_ref());
        invoke_signed(
            &Instruction {
                program_id: SYSTEM_PROGRAM_ID,
                accounts: vec![
                    AccountMeta::new(*payer.key, true),
                    AccountMeta::new(*state.key, true),
                ],
                data: create,
            },
            &[payer.clone(), state.clone(), system.clone()],
            &[&[STATE_SEED, &nonce, &[bump]]],
        )?;
    }
    {
        let mut bytes = state.try_borrow_mut_data()?;
        bytes[NONCE_AT..NONCE_AT + 32].copy_from_slice(&nonce);
        bytes[BUMP_AT] = bump;
        bytes[INFO_LEN_AT..INFO_AT].copy_from_slice(&(info.len() as u16).to_le_bytes());
        bytes[INFO_AT..INFO_AT + info.len()].copy_from_slice(info);
    }
    Script {
        accounts,
        nonce,
        bump,
        amount_sent: 0,
        trade_amount: 0,
    }
    .run(script)
}

struct Script<'a, 'info> {
    accounts: &'a [AccountInfo<'info>],
    nonce: [u8; 32],
    bump: u8,
    amount_sent: u64,
    trade_amount: u64,
}

/// Reads operands; `None` once the bytes run out, which ends the script.
struct Reader<'a> {
    bytes: &'a [u8],
}

impl Reader<'_> {
    fn byte(&mut self) -> Option<u8> {
        let (&first, rest) = self.bytes.split_first()?;
        self.bytes = rest;
        Some(first)
    }

    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let n = n.min(self.bytes.len());
        let (taken, rest) = self.bytes.split_at(n);
        self.bytes = rest;
        Some(taken)
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes([self.byte()?, self.byte()?]))
    }

    fn u64(&mut self) -> Option<u64> {
        let mut bytes = [0u8; 8];
        for b in &mut bytes {
            *b = self.byte()?;
        }
        Some(u64::from_le_bytes(bytes))
    }
}

impl<'info> Script<'_, 'info> {
    fn run(&self, script: &[u8]) -> ProgramResult {
        let mut reader = Reader { bytes: script };
        while let Some(tag) = reader.byte() {
            match self.step(tag % OP_COUNT, &mut reader) {
                Some(result) => result?,
                None => break,
            }
        }
        Ok(())
    }

    fn account(&self, reader: &mut Reader) -> Option<&AccountInfo<'info>> {
        let index = reader.byte()? as usize % self.accounts.len();
        Some(&self.accounts[index])
    }

    fn amount(&self, reader: &mut Reader) -> Option<u64> {
        Some(match reader.byte()? % 5 {
            amount::LITERAL => reader.u64()?,
            amount::SENT_PLUS => {
                let delta = reader.u16()? as i16;
                self.amount_sent.saturating_add_signed(delta as i64)
            }
            amount::BALANCE_OF => token_balance(self.account(reader)?),
            amount::SMALL => reader.byte()? as u64,
            _ => {
                let delta = reader.u16()? as i16;
                self.trade_amount.saturating_add_signed(delta as i64)
            }
        })
    }

    /// Runs one operation. `None` means the script ended inside its operands.
    fn step(&self, tag: u8, r: &mut Reader) -> Option<ProgramResult> {
        Some(match tag {
            op::TRANSFER => {
                let (from, mint, to, authority) = (
                    self.account(r)?,
                    self.account(r)?,
                    self.account(r)?,
                    self.account(r)?,
                );
                let amount = self.amount(r)?;
                let mut data = vec![12u8];
                data.extend_from_slice(&amount.to_le_bytes());
                data.push(mint_decimals(mint));
                self.call(
                    *mint.owner,
                    vec![
                        AccountMeta::new(*from.key, false),
                        AccountMeta::new_readonly(*mint.key, false),
                        AccountMeta::new(*to.key, false),
                        AccountMeta::new_readonly(*authority.key, true),
                    ],
                    data,
                )
            }
            op::APPROVE => {
                let (source, delegate, owner) =
                    (self.account(r)?, self.account(r)?, self.account(r)?);
                let mut data = vec![4u8];
                data.extend_from_slice(&self.amount(r)?.to_le_bytes());
                self.call(
                    *source.owner,
                    vec![
                        AccountMeta::new(*source.key, false),
                        AccountMeta::new_readonly(*delegate.key, false),
                        AccountMeta::new_readonly(*owner.key, true),
                    ],
                    data,
                )
            }
            op::CLOSE => {
                let (account, destination, owner) =
                    (self.account(r)?, self.account(r)?, self.account(r)?);
                self.call(
                    *account.owner,
                    vec![
                        AccountMeta::new(*account.key, false),
                        AccountMeta::new(*destination.key, false),
                        AccountMeta::new_readonly(*owner.key, true),
                    ],
                    vec![9u8],
                )
            }
            op::SET_OWNER => {
                let (account, new_owner, owner) =
                    (self.account(r)?, self.account(r)?, self.account(r)?);
                let mut data = vec![6u8, 2, 1];
                data.extend_from_slice(new_owner.key.as_ref());
                self.call(
                    *account.owner,
                    vec![
                        AccountMeta::new(*account.key, false),
                        AccountMeta::new_readonly(*owner.key, true),
                    ],
                    data,
                )
            }
            op::BURN => {
                let (account, mint, owner) = (self.account(r)?, self.account(r)?, self.account(r)?);
                let mut data = vec![8u8];
                data.extend_from_slice(&self.amount(r)?.to_le_bytes());
                self.call(
                    *account.owner,
                    vec![
                        AccountMeta::new(*account.key, false),
                        AccountMeta::new(*mint.key, false),
                        AccountMeta::new_readonly(*owner.key, true),
                    ],
                    data,
                )
            }
            op::MINT_TO => {
                let (mint, account, authority) =
                    (self.account(r)?, self.account(r)?, self.account(r)?);
                let mut data = vec![7u8];
                data.extend_from_slice(&self.amount(r)?.to_le_bytes());
                self.call(
                    *mint.owner,
                    vec![
                        AccountMeta::new(*mint.key, false),
                        AccountMeta::new(*account.key, false),
                        AccountMeta::new_readonly(*authority.key, true),
                    ],
                    data,
                )
            }
            op::LAMPORTS => {
                let (from, to) = (self.account(r)?, self.account(r)?);
                let amount = self.amount(r)?;
                move_lamports(from, to, amount)
            }
            op::SYSTEM_TRANSFER => {
                let (from, to) = (self.account(r)?, self.account(r)?);
                let mut data = 2u32.to_le_bytes().to_vec();
                data.extend_from_slice(&self.amount(r)?.to_le_bytes());
                self.call(
                    SYSTEM_PROGRAM_ID,
                    vec![
                        AccountMeta::new(*from.key, true),
                        AccountMeta::new(*to.key, false),
                    ],
                    data,
                )
            }
            op::INVOKE => {
                let program = *self.account(r)?.key;
                let count = r.byte()? % 8;
                let mut metas = vec![];
                for _ in 0..count {
                    let account = self.account(r)?;
                    let flags = r.byte()?;
                    metas.push(AccountMeta {
                        pubkey: *account.key,
                        is_writable: flags & 1 != 0,
                        is_signer: flags & 2 != 0,
                    });
                }
                let len = (r.byte()? % 64) as usize;
                let data = r.take(len)?.to_vec();
                self.call(program, metas, data)
            }
            op::WRITE => {
                let account = self.account(r)?;
                let offset = r.u16()? as usize;
                let len = (r.byte()? % 32) as usize;
                let bytes = r.take(len)?;
                write_data(account, offset, bytes)
            }
            op::RETURN_DATA => {
                let len = (r.byte()? % 64) as usize;
                set_return_data(r.take(len)?);
                Ok(())
            }
            op::FAIL => Err(ProgramError::Custom(0xF022)),
            op::BURN_COMPUTE => {
                let rounds = r.byte()? as u64 * 1_000;
                let mut x: u64 = 1;
                for _ in 0..rounds {
                    x = std::hint::black_box(x.wrapping_mul(6364136223846793005).wrapping_add(1));
                }
                Ok(())
            }
            op::TRANSFER_WITH => {
                let (from, mint, to, authority) = (
                    self.account(r)?,
                    self.account(r)?,
                    self.account(r)?,
                    self.account(r)?,
                );
                let amount = self.amount(r)?;
                let mut metas = vec![
                    AccountMeta::new(*from.key, false),
                    AccountMeta::new_readonly(*mint.key, false),
                    AccountMeta::new(*to.key, false),
                    AccountMeta::new_readonly(*authority.key, true),
                ];
                for _ in 0..r.byte()? % 8 {
                    let extra = self.account(r)?;
                    metas.push(AccountMeta {
                        pubkey: *extra.key,
                        is_signer: false,
                        is_writable: extra.is_writable,
                    });
                }
                let mut data = vec![12u8];
                data.extend_from_slice(&amount.to_le_bytes());
                data.push(mint_decimals(mint));
                self.call(*mint.owner, metas, data)
            }
            op::FREEZE | op::THAW => {
                let (account, mint, authority) =
                    (self.account(r)?, self.account(r)?, self.account(r)?);
                let tag = if tag == op::FREEZE { 10u8 } else { 11 };
                self.call(
                    *mint.owner,
                    vec![
                        AccountMeta::new(*account.key, false),
                        AccountMeta::new_readonly(*mint.key, false),
                        AccountMeta::new_readonly(*authority.key, true),
                    ],
                    vec![tag],
                )
            }
            op::REVOKE => {
                let (source, owner) = (self.account(r)?, self.account(r)?);
                self.call(
                    *source.owner,
                    vec![
                        AccountMeta::new(*source.key, false),
                        AccountMeta::new_readonly(*owner.key, true),
                    ],
                    vec![5u8],
                )
            }
            _ => {
                let (from, to, authority) = (self.account(r)?, self.account(r)?, self.account(r)?);
                let mut data = vec![3u8];
                data.extend_from_slice(&self.amount(r)?.to_le_bytes());
                self.call(
                    *from.owner,
                    vec![
                        AccountMeta::new(*from.key, false),
                        AccountMeta::new(*to.key, false),
                        AccountMeta::new_readonly(*authority.key, true),
                    ],
                    data,
                )
            }
        })
    }

    /// Invokes `program`, signed by the state PDA. Every account this program received
    /// is passed along, so any of them can appear in `metas`.
    fn call(&self, program: Pubkey, metas: Vec<AccountMeta>, data: Vec<u8>) -> ProgramResult {
        invoke_signed(
            &Instruction {
                program_id: program,
                accounts: metas,
                data,
            },
            self.accounts,
            &[&[STATE_SEED, &self.nonce, &[self.bump]]],
        )
    }
}

fn token_balance(account: &AccountInfo) -> u64 {
    account
        .try_borrow_data()
        .ok()
        .and_then(|data| Some(u64::from_le_bytes(data.get(64..72)?.try_into().ok()?)))
        .unwrap_or(0)
}

fn mint_decimals(mint: &AccountInfo) -> u8 {
    mint.try_borrow_data()
        .ok()
        .and_then(|data| data.get(44).copied())
        .unwrap_or(0)
}

fn move_lamports(from: &AccountInfo, to: &AccountInfo, amount: u64) -> ProgramResult {
    if from.key == to.key {
        return Ok(());
    }
    let mut from_lamports = from.try_borrow_mut_lamports()?;
    let mut to_lamports = to.try_borrow_mut_lamports()?;
    **from_lamports = from_lamports
        .checked_sub(amount)
        .ok_or(ProgramError::InsufficientFunds)?;
    **to_lamports = to_lamports
        .checked_add(amount)
        .ok_or(ProgramError::ArithmeticOverflow)?;
    Ok(())
}

fn write_data(account: &AccountInfo, offset: usize, bytes: &[u8]) -> ProgramResult {
    let mut data = account.try_borrow_mut_data()?;
    let end = offset.saturating_add(bytes.len()).min(data.len());
    let start = offset.min(end);
    data[start..end].copy_from_slice(&bytes[..end - start]);
    Ok(())
}
