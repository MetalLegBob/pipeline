//! A swap's output can't fall after the router measures it.
//!
//! The router measures the trader's balances around the strategy call, then charges its
//! fees. Those fee transfers run the input mint's transfer hook (the issuer's code)
//! after the measurement. Here the input mint's hook empties a stash account on every
//! transfer, and the swap's output account is that stash: the strategy pays into it, the
//! router measures the payment, and the hook moves it all away during Pipeline's fee
//! transfer. Without a final check the swap succeeded and its event reported an output
//! the account no longer held (found by the router fuzzer). The router now re-reads the
//! output after its fee transfers and refuses the swap (`OutputDecreased`).

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use pipeline_client::{self as client, AccountMeta, Instruction, Pubkey};
use solana_signer::Signer;

const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const TOKEN_2022: Pubkey = client::TOKEN_2022_PROGRAM_ID;
const REFILL_HOOK_ID: Pubkey = client::pubkey!("BfAcC8daPZSS4iEYz1figEshePUr96gDFwtytn7DyNZ4");
const RESERVE: u64 = 1_000_000_000_000;
const AMOUNT: u64 = 1_000_000_000;

fn refill_validation(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"extra-account-metas", mint.as_ref()], &REFILL_HOOK_ID).0
}

struct World {
    env: Env,
    market: CpMarket,
    trader: solana_keypair::Keypair,
    trader_in: Pubkey,
    stash: Pubkey,
    hook_accounts: Vec<AccountMeta>,
    mint_in: Pubkey,
    mint_out: Pubkey,
}

/// Input: a Token-2022 mint whose hook empties a stash of the output mint into another
/// holder's account on every transfer. Output: a plain SPL mint. A CP market between
/// them, and a trader holding the input.
fn world() -> World {
    let mut env = Env::new();
    env.svm
        .add_program_from_file(REFILL_HOOK_ID, sbf_dir().join("test_hook_refill.so"))
        .unwrap();
    let mint_in = env.create_mint(&TOKEN_2022, 6, None, Some(REFILL_HOOK_ID));
    let mint_out = env.create_mint(&TOKEN, 6, None, None);
    let stash_authority = Pubkey::find_program_address(&[b"stash"], &REFILL_HOOK_ID).0;
    let stash = env.create_ata(&stash_authority, &mint_out, &TOKEN);
    let elsewhere = env.create_ata(&Pubkey::new_unique(), &mint_out, &TOKEN);

    // Hook extra accounts: stash (w), stash authority, target (w), SPL Token.
    let mut init_data = b"refilini".to_vec();
    for (key, writable) in [
        (stash, 1u8),
        (stash_authority, 0),
        (elsewhere, 1),
        (TOKEN, 0),
    ] {
        init_data.extend_from_slice(key.as_ref());
        init_data.push(writable);
    }
    let init = Instruction::new_with_bytes(
        REFILL_HOOK_ID,
        &init_data,
        vec![
            AccountMeta::new(env.admin.pubkey(), true),
            AccountMeta::new(refill_validation(&mint_in), false),
            AccountMeta::new_readonly(mint_in, false),
            AccountMeta::new_readonly(client::SYSTEM_PROGRAM_ID, false),
        ],
    );
    env.send(&[init], &[]).expect("hook init");
    let hook_accounts = vec![
        AccountMeta::new_readonly(REFILL_HOOK_ID, false),
        AccountMeta::new_readonly(refill_validation(&mint_in), false),
        AccountMeta::new(stash, false),
        AccountMeta::new_readonly(stash_authority, false),
        AccountMeta::new(elsewhere, false),
        AccountMeta::new_readonly(TOKEN, false),
    ];
    let market = setup_cp_market_with_mints(
        &mut env,
        mint_in,
        TOKEN_2022,
        mint_out,
        TOKEN,
        RESERVE,
        RESERVE,
        30,
        hook_accounts.clone(),
    );
    let (trader, trader_in) = env.trader_with(&mint_in, &TOKEN_2022, 10 * AMOUNT);
    World {
        env,
        market,
        trader,
        trader_in,
        stash,
        hook_accounts,
        mint_in,
        mint_out,
    }
}

fn swap_into(w: &World, output: &Pubkey) -> Instruction {
    let mut accounts = cp_swap_accounts(
        &w.market,
        &w.trader.pubkey(),
        &w.mint_in,
        &w.trader_in,
        &w.mint_out,
        output,
    );
    accounts.input_hook_accounts = w.hook_accounts.clone();
    client::swap(&accounts, &exact_in(AMOUNT, 0))
}

fn compute(units: u32) -> Instruction {
    let mut data = vec![2u8];
    data.extend_from_slice(&units.to_le_bytes());
    Instruction::new_with_bytes(
        client::pubkey!("ComputeBudget111111111111111111111111111111"),
        &data,
        vec![],
    )
}

#[test]
fn an_output_emptied_by_a_hook_during_the_fee_transfer_is_refused() {
    let mut w = world();
    assert_eq!(w.env.token_balance(&w.stash), 0);
    let ix = swap_into(&w, &w.stash.clone());
    let trader = w.trader.insecure_clone();
    let result = w.env.send(&[compute(400_000), ix], &[&trader]);
    assert_fails_with(result, "OutputDecreased");
    assert_eq!(w.env.token_balance(&w.stash), 0);
}

/// The same swap into an account the hook doesn't touch works, and the trader keeps
/// what the event reports.
#[test]
fn control_the_same_swap_into_the_traders_own_account_works() {
    let mut w = world();
    let trader_out = w.env.create_ata(&w.trader.pubkey(), &w.mint_out, &TOKEN);
    let ix = swap_into(&w, &trader_out);
    let trader = w.trader.insecure_clone();
    let meta = w
        .env
        .send(&[compute(400_000), ix], &[&trader])
        .expect("swap");
    let event = swap_events(&meta).pop().unwrap();
    assert_eq!(w.env.token_balance(&trader_out), event.received);
}
