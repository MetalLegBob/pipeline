//! Resolves the accounts a Token-2022 transfer hook needs, from the hook's
//! extra-account-meta list (the `spl-tlv-account-resolution` format), so a swap can pass
//! them to the router and the strategy.
//!
//! Supported entries: fixed addresses, PDAs of the hook program or of an earlier
//! account, and addresses read from instruction data or account data. Seeds may be
//! literals, instruction data, account keys or account data. Accounts the hook marks as
//! signers are passed unsigned: nothing in a router swap can sign for them.

use pipeline_client::{AccountMeta, Pubkey};

/// `sha256("spl-transfer-hook-interface:execute")[..8]`, the TLV key of the list.
pub const EXECUTE_DISCRIMINATOR: [u8; 8] = [105, 37, 101, 197, 75, 251, 102, 26];
const ENTRY_LEN: usize = 35;

/// The account holding a mint's extra-account-meta list.
pub fn validation_address(hook_program: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"extra-account-metas", mint.as_ref()], hook_program).0
}

/// One token transfer, as the hook's `Execute` instruction sees it.
#[derive(Clone, Debug)]
pub struct Transfer {
    pub source: Pubkey,
    pub mint: Pubkey,
    pub destination: Pubkey,
    pub authority: Pubkey,
    pub amount: u64,
}

/// The hook accounts for one transfer: the extra accounts in list order, then the hook
/// program and the validation account (the order Token-2022 clients use). `account_data`
/// returns an account's data, or `None` if it does not exist.
pub fn resolve(
    hook_program: &Pubkey,
    transfer: &Transfer,
    account_data: impl Fn(&Pubkey) -> Option<Vec<u8>>,
) -> Result<Vec<AccountMeta>, String> {
    let validation = validation_address(hook_program, &transfer.mint);
    let data = account_data(&validation).ok_or_else(|| {
        format!("transfer hook {hook_program} has no extra-account-meta list at {validation}")
    })?;
    let entries = execute_entries(&data)?;

    let mut instruction_data = EXECUTE_DISCRIMINATOR.to_vec();
    instruction_data.extend_from_slice(&transfer.amount.to_le_bytes());
    // Indices in the list refer to the Execute instruction's accounts.
    let mut keys = vec![
        transfer.source,
        transfer.mint,
        transfer.destination,
        transfer.authority,
        validation,
    ];
    let mut metas = vec![];
    for entry in entries.chunks_exact(ENTRY_LEN) {
        let kind = entry[0];
        let config: &[u8; 32] = entry[1..33].try_into().unwrap();
        let writable = entry[34] != 0;
        let key = match kind {
            0 => Pubkey::new_from_array(*config),
            1 => {
                let seeds = seeds(config, &keys, &instruction_data, &account_data)?;
                pda(&seeds, hook_program)
            }
            2 => pubkey_data(config, &keys, &instruction_data, &account_data)?,
            index @ 128..=255 => {
                let program = keys.get((index - 128) as usize).ok_or_else(|| {
                    format!("hook PDA program index {} is out of range", index - 128)
                })?;
                let seeds = seeds(config, &keys, &instruction_data, &account_data)?;
                pda(&seeds, program)
            }
            other => return Err(format!("unknown extra-account-meta kind {other}")),
        };
        keys.push(key);
        metas.push(if writable {
            AccountMeta::new(key, false)
        } else {
            AccountMeta::new_readonly(key, false)
        });
    }
    metas.push(AccountMeta::new_readonly(*hook_program, false));
    metas.push(AccountMeta::new_readonly(validation, false));
    Ok(metas)
}

/// The packed `ExtraAccountMeta` entries stored under the Execute discriminator.
fn execute_entries(data: &[u8]) -> Result<&[u8], String> {
    let mut offset = 0;
    while offset + 12 <= data.len() {
        let discriminator = &data[offset..offset + 8];
        let length = u32::from_le_bytes(data[offset + 8..offset + 12].try_into().unwrap()) as usize;
        let value = data
            .get(offset + 12..offset + 12 + length)
            .ok_or("extra-account-meta list is truncated")?;
        if discriminator == EXECUTE_DISCRIMINATOR {
            let count = value
                .get(..4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)
                .ok_or("extra-account-meta list has no length")?;
            return value
                .get(4..4 + count * ENTRY_LEN)
                .ok_or_else(|| "extra-account-meta list is shorter than its count".to_string());
        }
        if discriminator == [0; 8] {
            break;
        }
        offset += 12 + length;
    }
    Err("extra-account-meta list has no Execute entry".into())
}

fn pda(seeds: &[Vec<u8>], program: &Pubkey) -> Pubkey {
    let slices: Vec<&[u8]> = seeds.iter().map(|s| s.as_slice()).collect();
    Pubkey::find_program_address(&slices, program).0
}

fn slice(bytes: &[u8], start: usize, len: usize, what: &str) -> Result<Vec<u8>, String> {
    bytes
        .get(start..start + len)
        .map(|b| b.to_vec())
        .ok_or_else(|| format!("hook seed reads past the end of {what}"))
}

fn key_at(keys: &[Pubkey], index: u8) -> Result<Pubkey, String> {
    keys.get(index as usize)
        .copied()
        .ok_or_else(|| format!("hook seed account index {index} is out of range"))
}

/// Unpacks seed configs: 1 literal (length, bytes), 2 instruction data (offset,
/// length), 3 account key (index), 4 account data (index, offset, length); 0 ends.
fn seeds(
    config: &[u8; 32],
    keys: &[Pubkey],
    instruction_data: &[u8],
    account_data: &impl Fn(&Pubkey) -> Option<Vec<u8>>,
) -> Result<Vec<Vec<u8>>, String> {
    let mut seeds = vec![];
    let mut i = 0;
    let byte = |at: usize| {
        config
            .get(at)
            .copied()
            .ok_or_else(|| "hook seed config is truncated".to_string())
    };
    while i < config.len() {
        match config[i] {
            0 => break,
            1 => {
                let len = byte(i + 1)? as usize;
                seeds.push(slice(config, i + 2, len, "the seed config")?);
                i += 2 + len;
            }
            2 => {
                let start = byte(i + 1)? as usize;
                let len = byte(i + 2)? as usize;
                seeds.push(slice(instruction_data, start, len, "the instruction data")?);
                i += 3;
            }
            3 => {
                seeds.push(key_at(keys, byte(i + 1)?)?.to_bytes().to_vec());
                i += 2;
            }
            4 => {
                let key = key_at(keys, byte(i + 1)?)?;
                let start = byte(i + 2)? as usize;
                let len = byte(i + 3)? as usize;
                let data = account_data(&key).ok_or_else(|| {
                    format!("hook seed reads account {key}, which does not exist")
                })?;
                seeds.push(slice(&data, start, len, "the account data")?);
                i += 4;
            }
            other => return Err(format!("unknown hook seed kind {other}")),
        }
    }
    Ok(seeds)
}

/// An address read from instruction data (1, offset) or account data (2, index, offset).
fn pubkey_data(
    config: &[u8; 32],
    keys: &[Pubkey],
    instruction_data: &[u8],
    account_data: &impl Fn(&Pubkey) -> Option<Vec<u8>>,
) -> Result<Pubkey, String> {
    let bytes = match config[0] {
        1 => slice(
            instruction_data,
            config[1] as usize,
            32,
            "the instruction data",
        )?,
        2 => {
            let key = key_at(keys, config[1])?;
            let data = account_data(&key)
                .ok_or_else(|| format!("hook address reads account {key}, which does not exist"))?;
            slice(&data, config[2] as usize, 32, "the account data")?
        }
        other => return Err(format!("unknown hook address kind {other}")),
    };
    Ok(Pubkey::new_from_array(bytes.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn list(entries: &[[u8; ENTRY_LEN]]) -> Vec<u8> {
        let mut data = EXECUTE_DISCRIMINATOR.to_vec();
        data.extend_from_slice(&(4 + ENTRY_LEN * entries.len()).to_le_bytes()[..4]);
        data.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for entry in entries {
            data.extend_from_slice(entry);
        }
        data
    }

    fn entry(kind: u8, config: &[u8], writable: bool) -> [u8; ENTRY_LEN] {
        let mut entry = [0u8; ENTRY_LEN];
        entry[0] = kind;
        entry[1..1 + config.len()].copy_from_slice(config);
        entry[34] = writable as u8;
        entry
    }

    #[test]
    fn resolves_fixed_pda_and_data_addresses() {
        let hook = Pubkey::new_unique();
        let fixed = Pubkey::new_unique();
        let stored = Pubkey::new_unique();
        let transfer = Transfer {
            source: Pubkey::new_unique(),
            mint: Pubkey::new_unique(),
            destination: Pubkey::new_unique(),
            authority: Pubkey::new_unique(),
            amount: 42,
        };
        // A PDA of the hook: ["counter", source owner (account 3 = authority), amount byte].
        let mut pda_seeds = vec![1u8, 7];
        pda_seeds.extend_from_slice(b"counter");
        pda_seeds.extend_from_slice(&[3, 3]);
        pda_seeds.extend_from_slice(&[2, 8, 1]);
        // A PDA of account 5 (the fixed entry, used as a program): [mint key].
        let foreign_seeds = [3u8, 1];
        // An address stored at offset 4 of account 5's data.
        let data_address = [2u8, 5, 4];
        let validation = validation_address(&hook, &transfer.mint);
        let mut accounts = HashMap::new();
        accounts.insert(
            validation,
            list(&[
                entry(0, fixed.as_ref(), false),
                entry(1, &pda_seeds, true),
                entry(128 + 5, &foreign_seeds, false),
                entry(2, &data_address, false),
            ]),
        );
        let mut fixed_data = vec![9u8; 4];
        fixed_data.extend_from_slice(stored.as_ref());
        accounts.insert(fixed, fixed_data);

        let metas = resolve(&hook, &transfer, |key| accounts.get(key).cloned()).unwrap();
        let keys: Vec<Pubkey> = metas.iter().map(|m| m.pubkey).collect();
        let counter =
            Pubkey::find_program_address(&[b"counter", transfer.authority.as_ref(), &[42]], &hook)
                .0;
        let foreign = Pubkey::find_program_address(&[transfer.mint.as_ref()], &fixed).0;
        assert_eq!(
            keys,
            vec![fixed, counter, foreign, stored, hook, validation]
        );
        assert!(metas[1].is_writable);
        assert!(!metas[0].is_writable);
        assert!(metas.iter().all(|m| !m.is_signer));
    }

    #[test]
    fn missing_list_is_an_error() {
        let transfer = Transfer {
            source: Pubkey::new_unique(),
            mint: Pubkey::new_unique(),
            destination: Pubkey::new_unique(),
            authority: Pubkey::new_unique(),
            amount: 1,
        };
        assert!(resolve(&Pubkey::new_unique(), &transfer, |_| None).is_err());
    }
}
