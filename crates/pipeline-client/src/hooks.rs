//! Transfer-hook accounts: the extra accounts a Token-2022 `transfer_checked` needs when
//! the mint has a transfer hook.
//!
//! A hooked mint names a hook program. The program keeps an extra-account-meta list at
//! the PDA `["extra-account-metas", mint]`, and Token-2022 calls the hook with the
//! transfer's accounts plus everything on that list. Each entry is a fixed address, a
//! PDA whose seeds come from the transfer (its accounts, their data or the instruction
//! data), or an address read from the transfer's data. A transfer must be given the
//! validation account, every resolved entry and the hook program.
//!
//! [`resolve`] works out those accounts for one transfer. Entries can depend on account
//! data, so it asks for the accounts it needs through a cache: when an account is
//! missing it returns [`HookError::NeedAccount`], and the caller fetches that account,
//! adds it (or `None` if it doesn't exist) and calls again. Swaps make several transfers
//! of the same mint (trader to vault, Pipeline's fee, the integrator fee), so resolve
//! each and combine them with [`merge`].

use std::collections::HashMap;
use std::fmt;

use crate::{swap_fee_account, AccountMeta, FeeSide, Pubkey, SwapAccounts, TOKEN_2022_PROGRAM_ID};

/// `sha256("spl-transfer-hook-interface:execute")[..8]`, the hook's Execute instruction
/// and the TLV type of its extra-account-meta list.
pub const EXECUTE_DISCRIMINATOR: [u8; 8] = [105, 37, 101, 197, 75, 251, 102, 26];

/// Token-2022's extension type number for `TransferHook`.
const TRANSFER_HOOK_EXTENSION: u16 = 14;
/// Where Token-2022 extensions start: after the 165-byte base account and its account
/// type byte (mints are padded to the token account size).
const EXTENSIONS_START: usize = 166;
const ACCOUNT_TYPE_MINT: u8 = 1;
const EXTRA_ACCOUNT_META_LEN: usize = 35;
/// Accounts of the hook's Execute before the extra ones: source, mint, destination,
/// authority and the validation account.
const EXECUTE_FIXED_ACCOUNTS: usize = 5;

/// Account data by address, as the caller has fetched it. `None` means the account
/// doesn't exist.
pub type AccountCache = HashMap<Pubkey, Option<Vec<u8>>>;

/// One token transfer of a hooked mint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HookTransfer {
    pub source: Pubkey,
    pub mint: Pubkey,
    pub destination: Pubkey,
    pub authority: Pubkey,
    pub amount: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HookError {
    /// Fetch this account into the cache and call again.
    NeedAccount(Pubkey),
    /// The mint's validation account doesn't exist or isn't an extra-account-meta list.
    BadValidationAccount(Pubkey),
    /// An entry reads data that the transfer doesn't have: an account that doesn't exist,
    /// bytes past the end of its data, or an account index past the ones resolved so far.
    Unresolvable { entry: usize, reason: &'static str },
    /// An entry asks for a signer, which a swap can't provide.
    SignerRequired { entry: usize },
}

impl fmt::Display for HookError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HookError::NeedAccount(key) => write!(f, "account {key} is needed"),
            HookError::BadValidationAccount(key) => {
                write!(f, "{key} is not a transfer-hook extra-account-meta list")
            }
            HookError::Unresolvable { entry, reason } => {
                write!(
                    f,
                    "transfer-hook extra account {entry} can't be resolved: {reason}"
                )
            }
            HookError::SignerRequired { entry } => {
                write!(f, "transfer-hook extra account {entry} must sign")
            }
        }
    }
}

impl std::error::Error for HookError {}

/// The transfer-hook program of a mint, if it is a Token-2022 mint with a hook set.
/// `owner` is the mint account's owner.
pub fn transfer_hook_program(owner: &Pubkey, mint_data: &[u8]) -> Option<Pubkey> {
    if *owner != TOKEN_2022_PROGRAM_ID
        || mint_data.get(EXTENSIONS_START - 1) != Some(&ACCOUNT_TYPE_MINT)
    {
        return None;
    }
    let mut offset = EXTENSIONS_START;
    while offset + 4 <= mint_data.len() {
        let kind = u16::from_le_bytes([mint_data[offset], mint_data[offset + 1]]);
        let length = u16::from_le_bytes([mint_data[offset + 2], mint_data[offset + 3]]) as usize;
        let value = mint_data.get(offset + 4..offset + 4 + length)?;
        if kind == TRANSFER_HOOK_EXTENSION {
            // Authority (32 bytes), then the program ID; all zeros means no hook.
            let program = Pubkey::new_from_array(value.get(32..64)?.try_into().ok()?);
            return (program != Pubkey::default()).then_some(program);
        }
        offset += 4 + length;
    }
    None
}

/// The hook program's extra-account-meta list for `mint`.
pub fn validation_address(mint: &Pubkey, hook_program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"extra-account-metas", mint.as_ref()], hook_program).0
}

/// The accounts `transfer` needs for `hook_program`: the validation account, every entry
/// on its extra-account-meta list in order, then the hook program. Entries are writable
/// as the list says; the validation account and the program are read-only.
pub fn resolve(
    hook_program: &Pubkey,
    transfer: &HookTransfer,
    cache: &AccountCache,
) -> Result<Vec<AccountMeta>, HookError> {
    let validation = validation_address(&transfer.mint, hook_program);
    let data = lookup(cache, &validation)?.ok_or(HookError::BadValidationAccount(validation))?;
    let entries = extra_account_metas(data).ok_or(HookError::BadValidationAccount(validation))?;

    let mut instruction_data = EXECUTE_DISCRIMINATOR.to_vec();
    instruction_data.extend_from_slice(&transfer.amount.to_le_bytes());
    // The Execute instruction's accounts, which entries refer to by index.
    let mut keys = vec![
        transfer.source,
        transfer.mint,
        transfer.destination,
        transfer.authority,
        validation,
    ];
    let mut metas = vec![AccountMeta::new_readonly(validation, false)];
    for (entry, raw) in entries.chunks_exact(EXTRA_ACCOUNT_META_LEN).enumerate() {
        let kind = raw[0];
        let config: &[u8; 32] = raw[1..33].try_into().unwrap();
        if raw[33] != 0 {
            return Err(HookError::SignerRequired { entry });
        }
        let writable = raw[34] != 0;
        let address = match kind {
            0 => Pubkey::new_from_array(*config),
            1 => {
                let seeds = seeds(config, &keys, &instruction_data, cache, entry)?;
                pda(&seeds, hook_program)
            }
            2 => pubkey_data(config, &keys, &instruction_data, cache, entry)?,
            program_index if program_index >= 128 => {
                let program =
                    *keys
                        .get((program_index - 128) as usize)
                        .ok_or(HookError::Unresolvable {
                            entry,
                            reason: "program account index out of range",
                        })?;
                let seeds = seeds(config, &keys, &instruction_data, cache, entry)?;
                pda(&seeds, &program)
            }
            _ => {
                return Err(HookError::Unresolvable {
                    entry,
                    reason: "unknown entry type",
                })
            }
        };
        keys.push(address);
        metas.push(if writable {
            AccountMeta::new(address, false)
        } else {
            AccountMeta::new_readonly(address, false)
        });
    }
    debug_assert_eq!(keys.len(), EXECUTE_FIXED_ACCOUNTS + metas.len() - 1);
    metas.push(AccountMeta::new_readonly(*hook_program, false));
    Ok(metas)
}

/// Adds `metas` to `into`, once per address; an address is writable if any use needs it.
pub fn merge(into: &mut Vec<AccountMeta>, metas: impl IntoIterator<Item = AccountMeta>) {
    for meta in metas {
        match into.iter_mut().find(|m| m.pubkey == meta.pubkey) {
            Some(existing) => existing.is_writable |= meta.is_writable,
            None => into.push(meta),
        }
    }
}

/// The transfers a router `swap` makes, by mint, so their hook accounts can be resolved.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SwapTransfers {
    /// Transfers of the input mint: trader to the input vault, and the fees taken in it.
    pub input: Vec<HookTransfer>,
    /// Transfers of the output mint: the strategy paying the trader, and Pipeline's fee
    /// when it is taken in the output.
    pub output: Vec<HookTransfer>,
}

/// What a swap moves, for [`swap_transfers`]. Hook entries may be seeded by the amount, so
/// use the quote's figures; an entry that depends on an amount that later differs will
/// differ too.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SwapAmounts {
    /// `SwapArgs::amount_in`.
    pub amount_in: u64,
    /// What the strategy pays out.
    pub strategy_amount_out: u64,
    pub pipeline_fee: u64,
    pub integrator_fee: u64,
}

/// The transfers a `swap` with `accounts` makes. `output_vault` is the account the strategy
/// pays from and `output_vault_authority` the authority it transfers with (usually the
/// vault's owner); they are the strategy's, so they are a best guess for a hook keyed on
/// them.
pub fn swap_transfers(
    accounts: &SwapAccounts,
    side: FeeSide,
    output_vault: Pubkey,
    output_vault_authority: Pubkey,
    amounts: SwapAmounts,
) -> SwapTransfers {
    let fee_account = swap_fee_account(accounts, side);
    let from_trader = |destination, amount| HookTransfer {
        source: accounts.trader_input,
        mint: accounts.input_mint,
        destination,
        authority: accounts.trader,
        amount,
    };
    let mut input = vec![from_trader(accounts.input_vault, amounts.amount_in)];
    if side == FeeSide::Input {
        input.push(from_trader(fee_account, amounts.pipeline_fee));
    }
    if let Some(integrator) = accounts.integrator_fee_account {
        input.push(from_trader(integrator, amounts.integrator_fee));
    }
    let mut output = vec![HookTransfer {
        source: output_vault,
        mint: accounts.output_mint,
        destination: accounts.trader_output,
        authority: output_vault_authority,
        amount: amounts.strategy_amount_out,
    }];
    if side == FeeSide::Output {
        output.push(HookTransfer {
            source: accounts.trader_output,
            mint: accounts.output_mint,
            destination: fee_account,
            authority: accounts.trader,
            amount: amounts.pipeline_fee,
        });
    }
    SwapTransfers { input, output }
}

/// The hook accounts for all of `transfers` of one mint, merged.
pub fn resolve_all(
    hook_program: &Pubkey,
    transfers: &[HookTransfer],
    cache: &AccountCache,
) -> Result<Vec<AccountMeta>, HookError> {
    let mut metas = Vec::new();
    for transfer in transfers {
        merge(&mut metas, resolve(hook_program, transfer, cache)?);
    }
    Ok(metas)
}

/// The data of a new, empty token account of `mint` owned by `owner`: what an account the
/// same transaction creates will hold when the hook runs. Seeds that read a token
/// account's mint or owner resolve against it.
pub fn new_token_account_data(mint: &Pubkey, owner: &Pubkey) -> Vec<u8> {
    let mut data = vec![0u8; 165];
    data[..32].copy_from_slice(mint.as_ref());
    data[32..64].copy_from_slice(owner.as_ref());
    // Account state: initialized.
    data[108] = 1;
    data
}

fn lookup<'a>(cache: &'a AccountCache, key: &Pubkey) -> Result<Option<&'a [u8]>, HookError> {
    match cache.get(key) {
        Some(data) => Ok(data.as_deref()),
        None => Err(HookError::NeedAccount(*key)),
    }
}

/// The packed `ExtraAccountMeta` entries of the Execute TLV entry in a validation account.
fn extra_account_metas(data: &[u8]) -> Option<&[u8]> {
    let mut offset = 0;
    while offset + 12 <= data.len() {
        let length = u32::from_le_bytes(data[offset + 8..offset + 12].try_into().unwrap()) as usize;
        let value = data.get(offset + 12..offset + 12 + length)?;
        if data[offset..offset + 8] == EXECUTE_DISCRIMINATOR {
            let count = u32::from_le_bytes(value.get(..4)?.try_into().unwrap()) as usize;
            return value.get(4..4 + count.checked_mul(EXTRA_ACCOUNT_META_LEN)?);
        }
        offset += 12 + length;
    }
    None
}

/// Unpacks an entry's seeds (each a type byte and its arguments, until a zero byte or the
/// end) and resolves them against the transfer.
fn seeds(
    config: &[u8; 32],
    keys: &[Pubkey],
    instruction_data: &[u8],
    cache: &AccountCache,
    entry: usize,
) -> Result<Vec<Vec<u8>>, HookError> {
    let unresolvable = |reason| HookError::Unresolvable { entry, reason };
    let mut seeds = Vec::new();
    let mut i = 0;
    while i < config.len() && config[i] != 0 {
        let arg = |n: usize| {
            config
                .get(i + n)
                .copied()
                .ok_or(unresolvable("truncated seed"))
        };
        match config[i] {
            // Literal: length, then the bytes.
            1 => {
                let length = arg(1)? as usize;
                let bytes = config
                    .get(i + 2..i + 2 + length)
                    .ok_or(unresolvable("truncated seed"))?;
                seeds.push(bytes.to_vec());
                i += 2 + length;
            }
            // Instruction data: offset, length.
            2 => {
                let (start, length) = (arg(1)? as usize, arg(2)? as usize);
                let bytes = instruction_data
                    .get(start..start + length)
                    .ok_or(unresolvable("seed past the end of the instruction data"))?;
                seeds.push(bytes.to_vec());
                i += 3;
            }
            // Account key: account index.
            3 => {
                let key = keys
                    .get(arg(1)? as usize)
                    .ok_or(unresolvable("seed account index out of range"))?;
                seeds.push(key.to_bytes().to_vec());
                i += 2;
            }
            // Account data: account index, offset, length.
            4 => {
                let key = keys
                    .get(arg(1)? as usize)
                    .ok_or(unresolvable("seed account index out of range"))?;
                let (start, length) = (arg(2)? as usize, arg(3)? as usize);
                let data = lookup(cache, key)?.ok_or(unresolvable("seed account doesn't exist"))?;
                let bytes = data
                    .get(start..start + length)
                    .ok_or(unresolvable("seed past the end of the account data"))?;
                seeds.push(bytes.to_vec());
                i += 4;
            }
            _ => return Err(unresolvable("unknown seed type")),
        }
    }
    Ok(seeds)
}

/// An address read from the instruction data or an account's data.
fn pubkey_data(
    config: &[u8; 32],
    keys: &[Pubkey],
    instruction_data: &[u8],
    cache: &AccountCache,
    entry: usize,
) -> Result<Pubkey, HookError> {
    let unresolvable = |reason| HookError::Unresolvable { entry, reason };
    let bytes = match config[0] {
        1 => instruction_data
            .get(config[1] as usize..config[1] as usize + 32)
            .ok_or(unresolvable("address past the end of the instruction data"))?,
        2 => {
            let key = keys
                .get(config[1] as usize)
                .ok_or(unresolvable("address account index out of range"))?;
            let data = lookup(cache, key)?.ok_or(unresolvable("address account doesn't exist"))?;
            data.get(config[2] as usize..config[2] as usize + 32)
                .ok_or(unresolvable("address past the end of the account data"))?
        }
        _ => return Err(unresolvable("unknown address source")),
    };
    Ok(Pubkey::new_from_array(bytes.try_into().unwrap()))
}

fn pda(seeds: &[Vec<u8>], program: &Pubkey) -> Pubkey {
    let seeds: Vec<&[u8]> = seeds.iter().map(Vec::as_slice).collect();
    Pubkey::find_program_address(&seeds, program).0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(byte: u8) -> Pubkey {
        Pubkey::new_from_array([byte; 32])
    }

    fn entry(kind: u8, config: &[u8], writable: bool) -> Vec<u8> {
        let mut raw = vec![kind];
        let mut padded = [0u8; 32];
        padded[..config.len()].copy_from_slice(config);
        raw.extend_from_slice(&padded);
        raw.push(0);
        raw.push(writable as u8);
        raw
    }

    fn validation_data(entries: &[Vec<u8>]) -> Vec<u8> {
        // An unrelated TLV entry first, to check the Execute entry is found by type.
        let mut data = [7u8; 8].to_vec();
        data.extend_from_slice(&3u32.to_le_bytes());
        data.extend_from_slice(&[1, 2, 3]);
        data.extend_from_slice(&EXECUTE_DISCRIMINATOR);
        data.extend_from_slice(&((4 + entries.len() * 35) as u32).to_le_bytes());
        data.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for e in entries {
            data.extend_from_slice(e);
        }
        data
    }

    fn transfer() -> HookTransfer {
        HookTransfer {
            source: key(1),
            mint: key(2),
            destination: key(3),
            authority: key(4),
            amount: 0x0102_0304_0506_0708,
        }
    }

    fn cache_with(hook: &Pubkey, entries: &[Vec<u8>]) -> AccountCache {
        let mut cache = AccountCache::new();
        cache.insert(
            validation_address(&key(2), hook),
            Some(validation_data(entries)),
        );
        cache
    }

    #[test]
    fn resolves_every_entry_type() {
        let hook = key(9);
        let fixed = key(42);
        // Destination token account: mint, then owner at 32..64.
        let mut destination = vec![0u8; 165];
        destination[32..64].copy_from_slice(&[77u8; 32]);
        let entries = [
            entry(0, fixed.as_ref(), false),
            // PDA of the hook: "src", source key.
            entry(1, &[1, 3, b's', b'r', b'c', 3, 0], true),
            // PDA of the hook: destination owner, then the amount.
            entry(1, &[4, 2, 32, 32, 2, 8, 8], false),
            // PDA of the program at account index 5 (the fixed entry): "x".
            entry(128 + 5, &[1, 1, b'x'], false),
            // Address from the destination's data (its owner).
            entry(2, &[2, 2, 32], true),
            // PDA of the hook keyed by entry 1's address (account index 6).
            entry(1, &[3, 6], false),
        ];
        let mut cache = cache_with(&hook, &entries);
        assert_eq!(
            resolve(&hook, &transfer(), &cache),
            Err(HookError::NeedAccount(key(3)))
        );
        cache.insert(key(3), Some(destination));
        let metas = resolve(&hook, &transfer(), &cache).unwrap();

        let source_pda = Pubkey::find_program_address(&[b"src", key(1).as_ref()], &hook).0;
        let expected = vec![
            AccountMeta::new_readonly(validation_address(&key(2), &hook), false),
            AccountMeta::new_readonly(fixed, false),
            AccountMeta::new(source_pda, false),
            AccountMeta::new_readonly(
                Pubkey::find_program_address(
                    &[&[77u8; 32], &0x0102_0304_0506_0708u64.to_le_bytes()],
                    &hook,
                )
                .0,
                false,
            ),
            AccountMeta::new_readonly(Pubkey::find_program_address(&[b"x"], &fixed).0, false),
            AccountMeta::new(key(77), false),
            AccountMeta::new_readonly(
                Pubkey::find_program_address(&[source_pda.as_ref()], &hook).0,
                false,
            ),
            AccountMeta::new_readonly(hook, false),
        ];
        assert_eq!(metas, expected);
    }

    #[test]
    fn empty_list_is_validation_and_program() {
        let hook = key(9);
        let metas = resolve(&hook, &transfer(), &cache_with(&hook, &[])).unwrap();
        assert_eq!(
            metas,
            vec![
                AccountMeta::new_readonly(validation_address(&key(2), &hook), false),
                AccountMeta::new_readonly(hook, false),
            ]
        );
    }

    #[test]
    fn unresolvable_entries_are_errors() {
        let hook = key(9);
        let missing_validation = AccountCache::from([(validation_address(&key(2), &hook), None)]);
        assert!(matches!(
            resolve(&hook, &transfer(), &missing_validation),
            Err(HookError::BadValidationAccount(_))
        ));
        let cases: [(Vec<u8>, &str); 5] = [
            (
                entry(1, &[4, 2, 0, 32], false),
                "seed account doesn't exist",
            ),
            (
                entry(1, &[2, 10, 8], false),
                "seed past the end of the instruction data",
            ),
            (entry(1, &[3, 9], false), "seed account index out of range"),
            (entry(1, &[9], false), "unknown seed type"),
            (entry(5, &[], false), "unknown entry type"),
        ];
        for (raw, reason) in cases {
            let mut cache = cache_with(&hook, &[raw]);
            cache.insert(key(3), None);
            assert_eq!(
                resolve(&hook, &transfer(), &cache),
                Err(HookError::Unresolvable { entry: 0, reason })
            );
        }
        let mut signer = entry(0, key(5).as_ref(), false);
        signer[33] = 1;
        assert_eq!(
            resolve(&hook, &transfer(), &cache_with(&hook, &[signer])),
            Err(HookError::SignerRequired { entry: 0 })
        );
    }

    #[test]
    fn merge_dedupes_and_keeps_writable() {
        let mut metas = vec![AccountMeta::new_readonly(key(1), false)];
        merge(
            &mut metas,
            [
                AccountMeta::new(key(1), false),
                AccountMeta::new_readonly(key(2), false),
            ],
        );
        assert_eq!(
            metas,
            vec![
                AccountMeta::new(key(1), false),
                AccountMeta::new_readonly(key(2), false)
            ]
        );
    }

    #[test]
    fn finds_the_hook_program_in_a_mint() {
        let mut mint = vec![0u8; EXTENSIONS_START];
        mint[EXTENSIONS_START - 1] = ACCOUNT_TYPE_MINT;
        // A transfer-fee config first (type 1, 108 bytes), then the hook.
        mint.extend_from_slice(&1u16.to_le_bytes());
        mint.extend_from_slice(&108u16.to_le_bytes());
        mint.extend_from_slice(&[0u8; 108]);
        mint.extend_from_slice(&TRANSFER_HOOK_EXTENSION.to_le_bytes());
        mint.extend_from_slice(&64u16.to_le_bytes());
        mint.extend_from_slice(&[0u8; 32]);
        mint.extend_from_slice(key(9).as_ref());
        assert_eq!(
            transfer_hook_program(&TOKEN_2022_PROGRAM_ID, &mint),
            Some(key(9))
        );
        assert_eq!(transfer_hook_program(&crate::TOKEN_PROGRAM_ID, &mint), None);
        let len = mint.len();
        mint[len - 32..].fill(0);
        assert_eq!(transfer_hook_program(&TOKEN_2022_PROGRAM_ID, &mint), None);
        assert_eq!(
            transfer_hook_program(&TOKEN_2022_PROGRAM_ID, &[0u8; 82]),
            None
        );
    }
}
