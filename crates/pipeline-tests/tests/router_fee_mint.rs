//! Pipeline takes its fee in a market's fee mint.
//!
//! - The router records the fee mint when it records a market: the strategy's
//!   declaration if there is one, else wrapped SOL if the market trades it, else none.
//! - A trade whose output is the fee mint pays `ceil(received × 25 / 10,000)` out of what
//!   arrived; the trader's minimum applies to what they keep. Other trades pay on the
//!   input as before. The integrator fee always stays in the input.

#![allow(clippy::result_large_err)]

mod common;
mod router_batch;

use borsh::BorshDeserialize;
use common::*;
use pipeline_client::{self as client, pda, AccountMeta, FeeSide, Instruction, Pubkey};
use pipeline_strategy_interface::TradeMode;
use router_batch::*;
use solana_keypair::Keypair;
use solana_signer::Signer;

const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");
/// `info_mode` values of the hostile test strategy (`programs/test-strategy-evil`).
const INFO_DECLARES_MINT_B: u8 = 8;
const INFO_DECLARES_OUT_OF_RANGE: u8 = 9;
const INFO_DECLARES_WITH_TRAILING_BYTE: u8 = 10;

/// A trader holding `tokens` of the market's token (mint A) and `lamports` of wrapped SOL.
struct Trader {
    keypair: Keypair,
    token: Pubkey,
    sol: Pubkey,
}

fn trader(env: &mut Env, market: &CpMarket, tokens: u64, lamports: u64) -> Trader {
    let (keypair, token) = env.trader_with(&market.mint_a, &TOKEN, tokens);
    let sol = wsol_account(env, &keypair.pubkey(), lamports);
    Trader {
        keypair,
        token,
        sol,
    }
}

fn sell(market: &CpMarket, t: &Trader) -> client::SwapAccounts {
    cp_swap_accounts(
        market,
        &t.keypair.pubkey(),
        &market.mint_a,
        &t.token,
        &NATIVE,
        &t.sol,
    )
}

fn buy(market: &CpMarket, t: &Trader) -> client::SwapAccounts {
    cp_swap_accounts(
        market,
        &t.keypair.pubkey(),
        &NATIVE,
        &t.sol,
        &market.mint_a,
        &t.token,
    )
}

fn sol_fee_account(market: &CpMarket) -> Pubkey {
    pda::fee_account(&market.market, &NATIVE, &TOKEN)
}

fn token_fee_account(market: &CpMarket) -> Pubkey {
    pda::fee_account(&market.market, &market.mint_a, &TOKEN)
}

fn swap_result(meta: &litesvm::types::TransactionMetadata) -> client::SwapResult {
    assert_eq!(meta.return_data.program_id, client::PROGRAM_ID);
    client::SwapResult::try_from_slice(&meta.return_data.data).expect("SwapResult")
}

/// An evil-strategy market between `mint_a` and `mint_b` whose market info uses
/// `info_mode`, with funded vaults and fee accounts for both mints.
struct EvilMarket {
    state: Pubkey,
    market: Pubkey,
    vault_a: Pubkey,
    vault_b: Pubkey,
}

fn evil_create_ix(
    env: &mut Env,
    creator: &Pubkey,
    mint_a: Pubkey,
    mint_b: Pubkey,
    info_mode: u8,
) -> (Instruction, EvilMarket) {
    let state =
        Pubkey::find_program_address(&[b"evil", mint_a.as_ref(), mint_b.as_ref()], &EVIL_ID).0;
    let vault_a = env.create_ata(&state, &mint_a, &TOKEN);
    let vault_b = env.create_ata(&state, &mint_b, &TOKEN);
    let ix = client::create_market(&client::CreateMarket {
        creator: *creator,
        strategy_program: EVIL_ID,
        market_state: state,
        market_state_signs: false,
        init_data: vec![info_mode],
        max_creator_lamports: MAX_SETUP_LAMPORTS,
        strategy_accounts: vec![
            AccountMeta::new_readonly(mint_a, false),
            AccountMeta::new_readonly(mint_b, false),
            AccountMeta::new(vault_a, false),
            AccountMeta::new(vault_b, false),
        ],
    });
    let market = EvilMarket {
        state,
        market: pda::market(&EVIL_ID, &state),
        vault_a,
        vault_b,
    };
    (ix, market)
}

// ------------------------------------------------------------------- recording

#[test]
fn a_sol_market_records_sol_and_a_token_market_records_none() {
    let mut env = Env::new();
    let token = env.create_mint(&TOKEN, 6, None, None);
    let sol = sol_market(&mut env, token);
    let record = env.market(&sol.market);
    assert_eq!(record.fee_mint_index, 1);
    assert_eq!(record.fee_mint(), Some(NATIVE));

    let a = env.create_mint(&TOKEN, 6, None, None);
    let b = env.create_mint(&TOKEN, 6, None, None);
    let plain = token_market(&mut env, a, b);
    let record = env.market(&plain.market);
    assert_eq!(record.fee_mint_index, client::NO_FEE_MINT);
    assert_eq!(record.fee_mint(), None);
}

/// A strategy's declaration wins, even over wrapped SOL in the same market.
#[test]
fn a_declared_fee_mint_overrides_sol() {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    let token = env.create_mint(&TOKEN, 6, None, None);
    let creator = env.funded_keypair();
    // Mint A is wrapped SOL; the strategy declares mint B (the token).
    let (ix, market) = evil_create_ix(
        &mut env,
        &creator.pubkey(),
        NATIVE,
        token,
        INFO_DECLARES_MINT_B,
    );
    env.send(&[ix], &[&creator]).expect("create market");
    let record = env.market(&market.market);
    assert_eq!(record.fee_mint_index, 1);
    assert_eq!(record.fee_mint(), Some(token));
}

#[test]
fn a_bad_declaration_is_refused() {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    for mode in [INFO_DECLARES_OUT_OF_RANGE, INFO_DECLARES_WITH_TRAILING_BYTE] {
        let a = env.create_mint(&TOKEN, 6, None, None);
        let b = env.create_mint(&TOKEN, 6, None, None);
        let creator = env.funded_keypair();
        let (ix, _) = evil_create_ix(&mut env, &creator.pubkey(), a, b, mode);
        assert_fails_with(env.send(&[ix], &[&creator]), "BadMarketInfo");
    }
}

// ------------------------------------------------------------- the fee's side

/// Selling a token into SOL: the fee comes out of the SOL that arrived, the trader keeps
/// the rest, and the token side pays no Pipeline fee.
#[test]
fn selling_into_sol_pays_the_fee_in_sol() {
    let mut env = Env::new();
    let token = env.create_mint(&TOKEN, 6, None, None);
    let market = sol_market(&mut env, token);
    let t = trader(&mut env, &market, 10_000_000_000, 0);
    let amount = 1_000_000_000;
    let (token_before, sol_before) = (env.token_balance(&t.token), env.token_balance(&t.sol));
    let (sol_fees_before, token_fees_before) = (
        env.token_balance(&sol_fee_account(&market)),
        env.token_balance(&token_fee_account(&market)),
    );

    let meta = env
        .send(
            &[client::swap(&sell(&market, &t), &exact_in(amount, 0))],
            &[&t.keypair],
        )
        .expect("sell");
    let event = swap_events(&meta).pop().unwrap();
    assert_eq!(event.fee_mint, NATIVE);
    assert_eq!(event.net_sent, amount);
    assert!(event.received > 0);
    assert_eq!(event.pipeline_fee, fee(event.received));

    let kept = event.received - event.pipeline_fee;
    assert_eq!(env.token_balance(&t.sol) - sol_before, kept);
    assert_eq!(token_before - env.token_balance(&t.token), amount);
    assert_eq!(
        env.token_balance(&sol_fee_account(&market)) - sol_fees_before,
        event.pipeline_fee
    );
    assert_eq!(
        env.token_balance(&token_fee_account(&market)),
        token_fees_before
    );

    let result = swap_result(&meta);
    assert_eq!(result.fee_mint, NATIVE);
    assert_eq!(
        (result.net_sent, result.received, result.pipeline_fee),
        (event.net_sent, event.received, event.pipeline_fee)
    );
}

/// Buying with SOL: the fee is on the input, in SOL, as before.
#[test]
fn buying_with_sol_pays_the_fee_on_the_input() {
    let mut env = Env::new();
    let token = env.create_mint(&TOKEN, 6, None, None);
    let market = sol_market(&mut env, token);
    let t = trader(&mut env, &market, 0, 10 * SOL);
    let amount = SOL / 10;
    let (token_before, sol_before) = (env.token_balance(&t.token), env.token_balance(&t.sol));
    let fees_before = env.token_balance(&sol_fee_account(&market));

    let meta = env
        .send(
            &[client::swap(&buy(&market, &t), &exact_in(amount, 0))],
            &[&t.keypair],
        )
        .expect("buy");
    let event = swap_events(&meta).pop().unwrap();
    assert_eq!(event.fee_mint, NATIVE);
    assert_eq!(event.pipeline_fee, fee(event.net_sent));
    assert_eq!(
        sol_before - env.token_balance(&t.sol),
        event.net_sent + event.pipeline_fee
    );
    assert_eq!(env.token_balance(&t.token) - token_before, event.received);
    assert_eq!(
        env.token_balance(&sol_fee_account(&market)) - fees_before,
        event.pipeline_fee
    );
}

/// The integrator fee stays in the input token when Pipeline's fee moves to the output.
#[test]
fn the_integrator_fee_stays_in_the_input() {
    let mut env = Env::new();
    let token = env.create_mint(&TOKEN, 6, None, None);
    let market = sol_market(&mut env, token);
    let t = trader(&mut env, &market, 10_000_000_000, 0);
    let integrator = env.create_ata(&Pubkey::new_unique(), &market.mint_a, &TOKEN);
    let mut accounts = sell(&market, &t);
    accounts.integrator_fee_account = Some(integrator);
    let mut args = exact_in(1_000_000_000, 0);
    args.integrator_fee_bps = 100;
    let token_before = env.token_balance(&t.token);

    let meta = env
        .send(&[client::swap(&accounts, &args)], &[&t.keypair])
        .expect("sell with an integrator fee");
    let event = swap_events(&meta).pop().unwrap();
    assert_eq!(event.fee_mint, NATIVE);
    assert_eq!(
        event.integrator_fee,
        ((event.net_sent as u128 * 100).div_ceil(10_000)) as u64
    );
    assert_eq!(env.token_balance(&integrator), event.integrator_fee);
    assert_eq!(
        token_before - env.token_balance(&t.token),
        event.net_sent + event.integrator_fee
    );
}

/// The minimum applies to what the trader keeps after the fee: one more than that fails,
/// exactly that passes.
#[test]
fn the_minimum_applies_to_what_the_trader_keeps() {
    let mut env = Env::new();
    let token = env.create_mint(&TOKEN, 6, None, None);
    let market = sol_market(&mut env, token);
    let t = trader(&mut env, &market, 10_000_000_000, 0);
    let amount = 1_000_000_000;

    let snapshot = env.svm.clone();
    let meta = env
        .send(
            &[client::swap(&sell(&market, &t), &exact_in(amount, 0))],
            &[&t.keypair],
        )
        .expect("dry run");
    let event = swap_events(&meta).pop().unwrap();
    let kept = event.received - event.pipeline_fee;
    assert!(event.pipeline_fee > 0);
    env.svm = snapshot;

    let too_high = client::swap(&sell(&market, &t), &exact_in(amount, kept + 1));
    assert_fails_with(env.send(&[too_high], &[&t.keypair]), "InsufficientOutput");
    // A minimum of `received` (what the old rule checked) must fail too.
    let gross = client::swap(&sell(&market, &t), &exact_in(amount, event.received));
    assert_fails_with(env.send(&[gross], &[&t.keypair]), "InsufficientOutput");
    let exact = client::swap(&sell(&market, &t), &exact_in(amount, kept));
    env.send(&[exact], &[&t.keypair])
        .expect("a minimum of exactly what is kept");
}

#[test]
fn the_wrong_fee_side_is_refused() {
    let mut env = Env::new();
    let token = env.create_mint(&TOKEN, 6, None, None);
    let market = sol_market(&mut env, token);
    let t = trader(&mut env, &market, 10_000_000_000, 10 * SOL);

    // A sell into SOL built with the fee on the input names the token fee account.
    let ix =
        client::swap_with_fee_side(&sell(&market, &t), &exact_in(1_000_000, 0), FeeSide::Input);
    assert_fails_with(env.send(&[ix], &[&t.keypair]), "WrongFeeAccount");
    // A buy with SOL built with the fee on the output names the token fee account too.
    let ix =
        client::swap_with_fee_side(&buy(&market, &t), &exact_in(1_000_000, 0), FeeSide::Output);
    assert_fails_with(env.send(&[ix], &[&t.keypair]), "WrongFeeAccount");
}

/// The fee can't be "paid" into the very account it comes from.
#[test]
fn the_fee_account_cannot_be_the_traders_output() {
    let mut env = Env::new();
    let token = env.create_mint(&TOKEN, 6, None, None);
    let market = sol_market(&mut env, token);
    let t = trader(&mut env, &market, 10_000_000_000, 0);
    let mut accounts = sell(&market, &t);
    accounts.trader_output = sol_fee_account(&market);
    let ix = client::swap(&accounts, &exact_in(1_000_000, 0));
    assert_fails_with(env.send(&[ix], &[&t.keypair]), "FeeToTraderInput");
}

/// Exact-out into SOL: asking the strategy for the smallest gross `G` with
/// `G - ceil(G × 25 / 10,000) >= X` leaves the trader at least `X`, and one less would not.
#[test]
fn exact_out_into_sol_asks_for_the_amount_plus_the_fee() {
    let mut env = Env::new();
    let token = env.create_mint(&TOKEN, 6, None, None);
    let market = sol_market(&mut env, token);
    let t = trader(&mut env, &market, 100_000_000_000, 0);
    let wanted = SOL / 100;
    let mut gross = wanted;
    while gross - fee(gross) < wanted {
        gross += 1;
    }
    assert!(gross - 1 - fee(gross - 1) < wanted, "gross is the smallest");

    let args = client::SwapArgs {
        mode: TradeMode::ExactOut,
        amount_in: 50_000_000_000,
        amount_out: gross,
        min_amount_out: wanted,
        integrator_fee_bps: 0,
        input_hook_accounts: 0,
        extra: vec![],
    };
    let sol_before = env.token_balance(&t.sol);
    let meta = env
        .send(&[client::swap(&sell(&market, &t), &args)], &[&t.keypair])
        .expect("exact-out sell");
    let event = swap_events(&meta).pop().unwrap();
    assert_eq!(event.received, gross);
    assert_eq!(event.pipeline_fee, fee(gross));
    let kept = env.token_balance(&t.sol) - sol_before;
    assert!(kept >= wanted, "kept {kept}, wanted {wanted}");
    assert_eq!(kept, gross - fee(gross));
}

/// A market whose fee mint is declared: trades into the declared mint pay on the output,
/// trades out of it pay on the input. Uses the evil strategy honestly (1:1).
#[test]
fn a_declared_fee_mint_is_charged_on_the_output() {
    let mut env = Env::new();
    env.register(&EVIL_ID, "test_strategy_evil");
    let a = env.create_mint(&TOKEN, 6, None, None);
    let b = env.create_mint(&TOKEN, 6, None, None);
    let creator = env.funded_keypair();
    let (ix, market) = evil_create_ix(&mut env, &creator.pubkey(), a, b, INFO_DECLARES_MINT_B);
    env.send(&[ix], &[&creator]).expect("create");
    let admin = env.admin.pubkey();
    env.send(
        &[
            client::init_fee_account(&admin, &market.market, &a, &TOKEN),
            client::init_fee_account(&admin, &market.market, &b, &TOKEN),
        ],
        &[],
    )
    .unwrap();
    env.mint_to(&a, &TOKEN, &market.vault_a, 1_000_000_000);
    env.mint_to(&b, &TOKEN, &market.vault_b, 1_000_000_000);
    let (trader, trader_a) = env.trader_with(&a, &TOKEN, 10_000_000);
    let trader_b = env.create_ata(&trader.pubkey(), &b, &TOKEN);

    let accounts = client::SwapAccounts {
        trader: trader.pubkey(),
        strategy_program: EVIL_ID,
        market_state: market.state,
        trader_input: trader_a,
        trader_output: trader_b,
        input_vault: market.vault_a,
        input_mint: a,
        output_mint: b,
        input_token_program: TOKEN,
        output_token_program: TOKEN,
        integrator_fee_account: None,
        strategy_accounts: vec![AccountMeta::new(market.vault_b, false)],
        input_hook_accounts: vec![],
    };
    let record = env.market(&market.market);
    let side = FeeSide::of(&record, &b);
    assert_eq!(side, FeeSide::Output);
    // The default (no declaration) would put it on the input: refused.
    let wrong = client::swap(&accounts, &exact_in(400_000, 0));
    assert_fails_with(env.send(&[wrong], &[&trader]), "WrongFeeAccount");
    let meta = env
        .send(
            &[client::swap_with_fee_side(
                &accounts,
                &exact_in(400_000, 0),
                side,
            )],
            &[&trader],
        )
        .expect("swap into the declared fee mint");
    let event = swap_events(&meta).pop().unwrap();
    assert_eq!(event.fee_mint, b);
    assert_eq!(event.received, 400_000);
    assert_eq!(event.pipeline_fee, fee(400_000));
    assert_eq!(env.token_balance(&trader_b), 400_000 - fee(400_000));
    assert_eq!(env.token_balance(&trader_a), 10_000_000 - 400_000);
    assert_eq!(
        env.token_balance(&pda::fee_account(&market.market, &b, &TOKEN)),
        fee(400_000)
    );
}
