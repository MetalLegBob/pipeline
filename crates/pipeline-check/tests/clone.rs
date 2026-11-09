//! Cloning a market into a fresh VM, as the command-line tool does over RPC, with the
//! "cluster" being another LiteSVM.

mod common;

use common::*;
use pipeline_check::{
    check_market, check_market_with_source, clone_market, CheckOptions, Status, SvmSource,
};
use pipeline_client::{self as client, pda};

#[test]
fn cloned_market_passes_the_same_checks_as_the_original() {
    let mut env = Env::new();
    let fee_mint = env.create_mint(&TOKEN_2022, 6, Some((50, u64::MAX)), None);
    let hooked = env.hooked_mint();
    let market = cp_market(
        &mut env,
        (fee_mint, TOKEN_2022),
        (hooked, TOKEN_2022),
        1_000_000_000_000,
        true,
    );
    let options = CheckOptions::default();
    let original = check_market(&env.svm, market.market, &options);

    let mut source = SvmSource::new(env.svm.clone());
    let cloned = clone_market(&mut source, market.market, None, &options).unwrap();
    assert!(cloned.notes.is_empty(), "{:?}", cloned.notes);
    let report = check_market_with_source(&cloned.svm, market.market, &options, cloned.source());
    println!("{}", report.to_text());
    assert_eq!(report.summary.result, Status::Pass, "{}", report.to_text());
    assert_eq!(report.source.kind, "rpc");

    // Same programs, same market, same outcome.
    assert_eq!(
        report.strategy.executable_sha256,
        original.strategy.executable_sha256
    );
    assert_eq!(
        report.router.executable_sha256,
        original.router.executable_sha256
    );
    assert_eq!(
        report.strategy.upgrade_authority,
        original.strategy.upgrade_authority
    );
    for (a, b) in report.checks.iter().zip(&original.checks) {
        assert_eq!((&a.id, a.status), (&b.id, b.status));
    }

    // The hook program, its meta list and the fee accounts came across; the liquidity
    // provider's accounts did not.
    for key in [
        HOOK_ID,
        hook_validation(&hooked),
        pda::fee_account(&market.market, &fee_mint, &TOKEN_2022),
        pda::program_data(&CP_ID),
    ] {
        assert!(source.requested.contains(&key), "{key} was not cloned");
    }
    assert!(
        cloned.accounts_cloned < 40,
        "cloned {} accounts",
        cloned.accounts_cloned
    );
}

#[test]
fn a_local_router_build_replaces_the_cluster_router() {
    let mut env = Env::new();
    let market = plain_cp_market(&mut env);
    let mut source = SvmSource::new(env.svm.clone());
    let router = program("pipeline");
    let cloned = clone_market(
        &mut source,
        market.market,
        Some(&router),
        &CheckOptions::default(),
    )
    .unwrap();
    assert!(!source.requested.contains(&client::PROGRAM_ID));
    let report = check_market_with_source(
        &cloned.svm,
        market.market,
        &CheckOptions::default(),
        cloned.source(),
    );
    assert_eq!(report.summary.result, Status::Pass, "{}", report.to_text());
}

#[test]
fn cloning_something_that_is_not_a_market_fails() {
    let mut env = Env::new();
    let market = plain_cp_market(&mut env);
    let mut source = SvmSource::new(env.svm.clone());
    let error = clone_market(&mut source, market.state, None, &CheckOptions::default())
        .err()
        .unwrap();
    assert!(error.contains("not a Pipeline Market"), "{error}");
}
