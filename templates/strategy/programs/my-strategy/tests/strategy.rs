//! Your safety net while you change the pricing: create a market through the real
//! router, add liquidity, trade it, then run every `pipeline-check` check on it.
//!
//! `pipeline-check` quotes and swaps the market in every direction and size, through the
//! router, and fails if quotes and swaps disagree, minimums aren't enforced, zero input
//! buys something, and more (see its README). It is the same check anyone can run
//! against your devnet market, so passing here means passing there.
//!
//! Build first: `scripts/fetch-router.sh` and
//! `cargo build-sbf --manifest-path programs/my-strategy/Cargo.toml`.

mod common;

use common::*;
use pipeline_check::{check_market, CheckOptions, Status};
use pipeline_client::pda;
use solana_signer::Signer;

const POOL_FEE_BPS: u16 = 30;

/// Runs every check on `market` and fails the test if any check fails. Warnings are
/// printed; decide for yourself whether they matter for your strategy.
fn assert_passes_pipeline_check(env: &Env, market: &Market) {
    let report = check_market(&env.svm, market.market, &CheckOptions::default());
    println!("{}", report.to_text());
    let failed: Vec<_> = report
        .checks
        .iter()
        .filter(|check| check.status == Status::Fail)
        .map(|check| check.id.as_str())
        .collect();
    assert!(failed.is_empty(), "failed checks: {failed:?}");
}

#[test]
fn market_trades_through_the_router() {
    let mut env = Env::new();
    let mint_a = env.create_mint(&TOKEN, 6, None);
    let mint_b = env.create_mint(&TOKEN, 9, None);
    let market = env
        .create_market((mint_a, TOKEN), (mint_b, TOKEN), POOL_FEE_BPS)
        .unwrap();

    // Liquidity, from the strategy's own `deposit` action.
    let provider = env.funded_wallet(&market, 10_000_000_000_000);
    env.deposit(&market, &provider, 1_000_000_000_000, 2_000_000_000_000)
        .unwrap();

    // Quote a trade, then make it through the router with the quote as the minimum.
    let trader = env.funded_wallet(&market, 1_000_000_000);
    let amount_in = 5_000_000;
    let quote = env.quote(&market, &exact_in(0, 1, amount_in)).unwrap();
    assert!(quote.amount_out > 0);
    let output_account = pda::associated_token(&trader.pubkey(), &mint_b, &TOKEN);
    let before = env.balance(&output_account);
    let result = env
        .swap(&market, &trader, (0, 1), amount_in, quote.amount_out)
        .unwrap();

    // The trader got exactly the quote and paid Pipeline's 0.25% on top.
    assert_eq!(result.received, quote.amount_out);
    assert_eq!(env.balance(&output_account) - before, quote.amount_out);
    assert_eq!(result.net_sent, amount_in);
    assert_eq!(result.pipeline_fee, amount_in.div_ceil(400));

    // Asking for one unit more than the pool gives fails the whole swap.
    let quote = env.quote(&market, &exact_in(1, 0, amount_in)).unwrap();
    let too_much = env.swap(&market, &trader, (1, 0), amount_in, quote.amount_out + 1);
    assert!(too_much.is_err());

    assert_passes_pipeline_check(&env, &market);
}

#[test]
fn transfer_fee_token_market_passes_pipeline_check() {
    // A Token-2022 mint that keeps 1% of every transfer: the pool must price what
    // actually arrives in its vault (`credited_input`), not what was sent.
    let mut env = Env::new();
    let fee_mint = env.create_mint(&TOKEN_2022, 6, Some(100));
    let plain_mint = env.create_mint(&TOKEN, 9, None);
    let market = env
        .create_market((fee_mint, TOKEN_2022), (plain_mint, TOKEN), POOL_FEE_BPS)
        .unwrap();
    let provider = env.funded_wallet(&market, 10_000_000_000_000);
    env.deposit(&market, &provider, 1_000_000_000_000, 1_000_000_000_000)
        .unwrap();

    assert_passes_pipeline_check(&env, &market);
}

#[test]
fn an_empty_pool_refuses_to_trade() {
    let mut env = Env::new();
    let mint_a = env.create_mint(&TOKEN, 6, None);
    let mint_b = env.create_mint(&TOKEN, 6, None);
    let market = env
        .create_market((mint_a, TOKEN), (mint_b, TOKEN), POOL_FEE_BPS)
        .unwrap();
    assert!(env.quote(&market, &exact_in(0, 1, 1_000)).is_err());

    // The first deposit must add both tokens.
    let provider = env.funded_wallet(&market, 1_000_000);
    assert!(env.deposit(&market, &provider, 1_000, 0).is_err());
}

#[test]
fn a_pool_exists_once_per_mints_and_fee() {
    let mut env = Env::new();
    let mint_a = env.create_mint(&TOKEN, 6, None);
    let mint_b = env.create_mint(&TOKEN, 6, None);
    env.create_market((mint_a, TOKEN), (mint_b, TOKEN), POOL_FEE_BPS)
        .unwrap();
    let again = env.create_market((mint_a, TOKEN), (mint_b, TOKEN), POOL_FEE_BPS);
    assert!(again.is_err());
    // A different fee is a different pool.
    env.create_market((mint_a, TOKEN), (mint_b, TOKEN), 100)
        .unwrap();
}
