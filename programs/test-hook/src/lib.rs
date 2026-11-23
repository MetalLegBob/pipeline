//! Token-2022 transfer hook used in tests.
//!
//! `initialize(allowed)` creates the mint's extra-account-meta list. With an empty
//! `allowed` list the hook approves every transfer. Otherwise it also creates a
//! whitelist account (listed as the hook's one extra account) and only approves
//! transfers whose source or destination token account is on it, like a whitelist-gated
//! token's hook.
//!
//! `initialize_seeded` instead lists one extra account of every kind a hook can ask for
//! (fixed, PDAs seeded from the transfer's accounts, their data and the amount, an
//! external PDA and an address read from account data) and approves every transfer.
//! Token-2022 resolves the list itself, so a transfer missing any of them fails; tests
//! use it to check that clients resolve hook accounts the same way.

use solana_program::{
    account_info::AccountInfo,
    entrypoint,
    entrypoint::ProgramResult,
    instruction::{AccountMeta, Instruction},
    msg,
    program::invoke_signed,
    program_error::ProgramError,
    pubkey::Pubkey,
    rent::Rent,
    sysvar::Sysvar,
};

/// `sha256("spl-transfer-hook-interface:execute")[..8]`
const EXECUTE: [u8; 8] = [105, 37, 101, 197, 75, 251, 102, 26];
/// This program's own setup instructions.
const INITIALIZE: [u8; 8] = *b"tsthkini";
const INITIALIZE_SEEDED: [u8; 8] = *b"tsthkin2";

entrypoint!(process_instruction);

pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    match data.get(..8) {
        Some(d) if d == EXECUTE => execute(program_id, accounts),
        Some(d) if d == INITIALIZE => initialize(program_id, accounts, &data[8..]),
        Some(d) if d == INITIALIZE_SEEDED => initialize_seeded(program_id, accounts),
        _ => Err(ProgramError::InvalidInstructionData),
    }
}

/// Accounts: source, mint, destination, authority, validation, then the extra accounts:
/// the whitelist, or the seeded list's accounts.
fn execute(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let source = accounts.first().ok_or(ProgramError::NotEnoughAccountKeys)?;
    let mint = accounts.get(1).ok_or(ProgramError::NotEnoughAccountKeys)?;
    let destination = accounts.get(2).ok_or(ProgramError::NotEnoughAccountKeys)?;
    // Token-2022 leaves the validation account and every extra account out when the
    // transfer wasn't given the validation account; a real hook refuses that, and so
    // does this one.
    let validation_key =
        Pubkey::find_program_address(&[b"extra-account-metas", mint.key.as_ref()], program_id).0;
    if accounts.get(4).map(|a| *a.key) != Some(validation_key) {
        msg!("transfer rejected: no validation account");
        return Err(ProgramError::NotEnoughAccountKeys);
    }
    let whitelist_key =
        Pubkey::find_program_address(&[b"whitelist", mint.key.as_ref()], program_id).0;
    let Some(whitelist) = accounts.get(5).filter(|a| *a.key == whitelist_key) else {
        return Ok(());
    };
    let data = whitelist.try_borrow_data()?;
    let allowed = data
        .chunks_exact(32)
        .any(|key| key == source.key.as_ref() || key == destination.key.as_ref());
    if allowed {
        Ok(())
    } else {
        msg!("transfer rejected: neither source nor destination is whitelisted");
        Err(ProgramError::Custom(0x7e57))
    }
}

/// Accounts: payer (signer), validation PDA, whitelist PDA, mint, system program.
/// Data: N × 32-byte token account addresses allowed by the whitelist.
fn initialize(program_id: &Pubkey, accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    let [payer, validation, whitelist, mint, system_program] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let (validation_key, validation_bump) =
        Pubkey::find_program_address(&[b"extra-account-metas", mint.key.as_ref()], program_id);
    let (whitelist_key, whitelist_bump) =
        Pubkey::find_program_address(&[b"whitelist", mint.key.as_ref()], program_id);
    if *validation.key != validation_key || *whitelist.key != whitelist_key {
        return Err(ProgramError::InvalidSeeds);
    }

    // TLV entry for the Execute instruction: discriminator, length, then a PodSlice of
    // 35-byte ExtraAccountMeta entries (type 0 = fixed address).
    let extra_count: u32 = if data.is_empty() { 0 } else { 1 };
    let mut tlv = EXECUTE.to_vec();
    tlv.extend_from_slice(&(4 + 35 * extra_count).to_le_bytes());
    tlv.extend_from_slice(&extra_count.to_le_bytes());
    if extra_count == 1 {
        tlv.push(0);
        tlv.extend_from_slice(whitelist_key.as_ref());
        tlv.push(0); // not a signer
        tlv.push(0); // not writable
    }

    create_pda(
        payer,
        validation,
        system_program,
        program_id,
        tlv.len(),
        &[
            b"extra-account-metas",
            mint.key.as_ref(),
            &[validation_bump],
        ],
    )?;
    validation.try_borrow_mut_data()?.copy_from_slice(&tlv);
    if extra_count == 1 {
        create_pda(
            payer,
            whitelist,
            system_program,
            program_id,
            data.len(),
            &[b"whitelist", mint.key.as_ref(), &[whitelist_bump]],
        )?;
        whitelist.try_borrow_mut_data()?.copy_from_slice(data);
    }
    Ok(())
}

/// A fixed address the seeded list names.
const SEEDED_FIXED: Pubkey = Pubkey::new_from_array([42; 32]);

/// Accounts: payer (signer), validation PDA, mint, system program.
fn initialize_seeded(program_id: &Pubkey, accounts: &[AccountInfo]) -> ProgramResult {
    let [payer, validation, mint, system_program] = accounts else {
        return Err(ProgramError::NotEnoughAccountKeys);
    };
    let (validation_key, validation_bump) =
        Pubkey::find_program_address(&[b"extra-account-metas", mint.key.as_ref()], program_id);
    if *validation.key != validation_key {
        return Err(ProgramError::InvalidSeeds);
    }

    // Execute's accounts are source 0, mint 1, destination 2, authority 3, validation 4,
    // then these entries from 5 on. Seeds are packed as: 1 literal (length, bytes),
    // 2 instruction data (offset, length), 3 account key (index), 4 account data (index,
    // offset, length).
    let entries: [(u8, &[u8], bool); 6] = [
        // 5: a fixed address.
        (0, SEEDED_FIXED.as_ref(), false),
        // 6: this program's PDA of "src" and the source account.
        (1, &[1, 3, b's', b'r', b'c', 3, 0], true),
        // 7: this program's PDA of the destination's owner and the amount.
        (1, &[4, 2, 32, 32, 2, 8, 8], false),
        // 8: a PDA of account 5 as the program, seeded "x".
        (128 + 5, &[1, 1, b'x'], false),
        // 9: the destination's owner, read from its data.
        (2, &[2, 2, 32], true),
        // 10: this program's PDA of account 6.
        (1, &[3, 6], false),
    ];
    let mut tlv = EXECUTE.to_vec();
    tlv.extend_from_slice(&(4 + 35 * entries.len() as u32).to_le_bytes());
    tlv.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (kind, config, writable) in entries {
        let mut padded = [0u8; 32];
        padded[..config.len()].copy_from_slice(config);
        tlv.push(kind);
        tlv.extend_from_slice(&padded);
        tlv.push(0); // not a signer
        tlv.push(writable as u8);
    }
    create_pda(
        payer,
        validation,
        system_program,
        program_id,
        tlv.len(),
        &[
            b"extra-account-metas",
            mint.key.as_ref(),
            &[validation_bump],
        ],
    )?;
    validation.try_borrow_mut_data()?.copy_from_slice(&tlv);
    Ok(())
}

fn create_pda<'a>(
    payer: &AccountInfo<'a>,
    account: &AccountInfo<'a>,
    system_program: &AccountInfo<'a>,
    owner: &Pubkey,
    space: usize,
    seeds: &[&[u8]],
) -> ProgramResult {
    let lamports = Rent::get()?.minimum_balance(space);
    // System program CreateAccount: u32 tag 0, lamports, space, owner.
    let mut data = 0u32.to_le_bytes().to_vec();
    data.extend_from_slice(&lamports.to_le_bytes());
    data.extend_from_slice(&(space as u64).to_le_bytes());
    data.extend_from_slice(owner.as_ref());
    let create_account = Instruction {
        program_id: *system_program.key,
        accounts: vec![
            AccountMeta::new(*payer.key, true),
            AccountMeta::new(*account.key, true),
        ],
        data,
    };
    invoke_signed(
        &create_account,
        &[payer.clone(), account.clone(), system_program.clone()],
        &[seeds],
    )
}
