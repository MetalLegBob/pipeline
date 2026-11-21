//! Shared setup for the fee-mint, route and lookup-table tests: SOL-paired markets,
//! wrapped-SOL accounts, lookup-table accounts and v0 transactions.

#![allow(dead_code, clippy::result_large_err)]

use super::common::*;
use pipeline_client::{self as client, Instruction, Pubkey};
use solana_account::Account;
use solana_keypair::Keypair;
use solana_message::{v0, AddressLookupTableAccount};
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction::VersionedMessage;

pub const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
pub const NATIVE: Pubkey = client::wallet::NATIVE_MINT;
pub const LOOKUP_TABLE_PROGRAM: Pubkey =
    client::pubkey!("AddressLookupTab1e1111111111111111111111111");

/// `ceil(amount × 25 / 10,000)`, Pipeline's fee.
pub fn fee(amount: u64) -> u64 {
    ((amount as u128 * 25).div_ceil(10_000)) as u64
}

/// A CP market between `token` (SPL, mint A) and wrapped SOL (mint B), with liquidity.
pub fn sol_market(env: &mut Env, token: Pubkey) -> CpMarket {
    setup_cp_market_with_mints(
        env,
        token,
        TOKEN,
        NATIVE,
        TOKEN,
        1_000_000_000_000,
        100 * SOL,
        30,
        vec![],
    )
}

/// A CP market between two SPL mints, with liquidity.
pub fn token_market(env: &mut Env, a: Pubkey, b: Pubkey) -> CpMarket {
    setup_cp_market_with_mints(
        env,
        a,
        TOKEN,
        b,
        TOKEN,
        1_000_000_000_000,
        1_000_000_000_000,
        30,
        vec![],
    )
}

/// `owner`'s wrapped-SOL account holding `lamports`.
pub fn wsol_account(env: &mut Env, owner: &Pubkey, lamports: u64) -> Pubkey {
    let account = env.create_ata(owner, &NATIVE, &TOKEN);
    if lamports > 0 {
        env.mint_to(&NATIVE, &TOKEN, &account, lamports);
    }
    account
}

/// The settings of a lookup-table account written by [`set_lookup_table`].
#[derive(Clone, Copy)]
pub struct TableState {
    pub frozen: bool,
    pub deactivated: bool,
}

pub const FROZEN: TableState = TableState {
    frozen: true,
    deactivated: false,
};

/// Writes an address lookup table account holding `addresses` at a new address, in the
/// lookup table program's layout: u32 state (1), u64 deactivation slot, u64 last extended
/// slot, u8 start index, `Option<Pubkey>` authority, two bytes of padding, addresses.
pub fn set_lookup_table(env: &mut Env, addresses: &[Pubkey], state: TableState) -> Pubkey {
    let key = Pubkey::new_unique();
    let mut data = vec![0u8; 56];
    data[0..4].copy_from_slice(&1u32.to_le_bytes());
    let deactivation = if state.deactivated { 1 } else { u64::MAX };
    data[4..12].copy_from_slice(&deactivation.to_le_bytes());
    data[12..20].copy_from_slice(&0u64.to_le_bytes());
    data[20] = addresses.len() as u8;
    if !state.frozen {
        data[21] = 1;
        data[22..54].copy_from_slice(Pubkey::new_unique().as_ref());
    }
    for address in addresses {
        data.extend_from_slice(address.as_ref());
    }
    let lamports = env.svm.minimum_balance_for_rent_exemption(data.len());
    env.svm
        .set_account(
            key,
            Account {
                lamports,
                data,
                owner: LOOKUP_TABLE_PROGRAM,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    key
}

/// Sends a v0 transaction paid by the admin that loads addresses from `tables`.
pub fn send_v0(
    env: &mut Env,
    instructions: &[Instruction],
    signers: &[&Keypair],
    tables: &[(Pubkey, Vec<Pubkey>)],
) -> TxResult {
    env.svm.expire_blockhash();
    let tables: Vec<AddressLookupTableAccount> = tables
        .iter()
        .map(|(key, addresses)| AddressLookupTableAccount {
            key: *key,
            addresses: addresses.clone(),
        })
        .collect();
    let message = v0::Message::try_compile(
        &env.admin.pubkey(),
        instructions,
        &tables,
        env.svm.latest_blockhash(),
    )
    .expect("compile v0 message");
    let mut all: Vec<&Keypair> = vec![&env.admin];
    all.extend_from_slice(signers);
    let tx = VersionedTransaction::try_new(VersionedMessage::V0(message), &all).expect("sign");
    let size = bincode::serialize(&tx)
        .map(|b| b.len())
        .unwrap_or(usize::MAX);
    assert!(
        size <= MAX_TRANSACTION_SIZE,
        "v0 transaction is {size} bytes, over Solana's {MAX_TRANSACTION_SIZE}-byte limit"
    );
    env.svm.send_transaction(tx)
}
