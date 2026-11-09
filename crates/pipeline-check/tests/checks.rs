//! A well-behaved strategy passes every check; hostile behaviours fail the checks that
//! are meant to catch them.

mod common;

use common::*;
use pipeline_check::{check_market, CheckOptions, Report, Status, CHECKS};
use pipeline_client::{pda, Pubkey};
use sha2::{Digest, Sha256};
use solana_signer::Signer;

fn options_with_extra(behaviour: u8) -> CheckOptions {
    CheckOptions {
        extra: vec![behaviour],
        ..CheckOptions::default()
    }
}

/// Asserts every check has the expected status, showing the report on failure.
fn assert_statuses(report: &Report, expected: &[(&str, Status)]) {
    for (id, status) in expected {
        assert_eq!(
            report.status(id),
            Some(*status),
            "check {id}:\n{}",
            report.to_text()
        );
    }
}

fn assert_all_pass(report: &Report, except: &[(&str, Status)]) {
    for (id, _) in CHECKS {
        let expected = except
            .iter()
            .find(|(check, _)| check == id)
            .map_or(Status::Pass, |(_, status)| *status);
        assert_eq!(
            report.status(id),
            Some(expected),
            "check {id}:\n{}",
            report.to_text()
        );
    }
}

#[test]
fn constant_product_market_passes_every_check() {
    let mut env = Env::new();
    let market = plain_cp_market(&mut env);
    let before = env.svm.get_account(&market.state).unwrap();

    let report = check_market(&env.svm, market.market, &CheckOptions::default());
    println!("{}", report.to_text());
    assert_all_pass(&report, &[]);
    assert_eq!(report.summary.result, Status::Pass);

    // The caller's VM is untouched: trades ran in clones.
    assert_eq!(env.svm.get_account(&market.state).unwrap(), before);
}

#[test]
fn token_2022_transfer_fee_and_hook_market_passes_every_check() {
    let mut env = Env::new();
    let fee_mint = env.create_mint(&TOKEN_2022, 6, Some((100, u64::MAX)), None);
    let hooked = env.hooked_mint();
    let market = cp_market(
        &mut env,
        (fee_mint, TOKEN_2022),
        (hooked, TOKEN_2022),
        1_000_000_000_000,
        true,
    );
    let report = check_market(&env.svm, market.market, &CheckOptions::default());
    println!("{}", report.to_text());
    assert_all_pass(&report, &[]);
    let mint = &report.market.mints[1];
    assert!(mint.extensions.iter().any(|e| e == "TransferHook"));
}

#[test]
fn report_identifies_programs_and_is_valid_json() {
    let mut env = Env::new();
    let market = plain_cp_market(&mut env);
    let report = check_market(&env.svm, market.market, &CheckOptions::default());

    // The hash is of the ELF with trailing zero bytes removed.
    let elf = program("test_strategy_cp");
    let end = elf.iter().rposition(|b| *b != 0).unwrap() + 1;
    let expected: String = Sha256::digest(&elf[..end])
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(
        report.strategy.executable_sha256.as_deref(),
        Some(&expected[..])
    );
    assert_eq!(report.strategy.executable_len, Some(end));
    assert_eq!(report.strategy.program_id, CP_ID.to_string());
    assert_eq!(
        report.router.program_id,
        "PiPEf4fFBpYXCtgfiWgVWzZNXnq2wZ7bnXidJCTwf34"
    );
    assert_eq!(
        report.strategy.upgrade_authority,
        Some(env.admin.pubkey().to_string())
    );

    let json: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();
    assert_eq!(json["schema"], "pipeline-check-report/1");
    assert_eq!(json["source"]["kind"], "svm");
    assert_eq!(json["summary"]["result"], "pass");
    assert_eq!(json["checks"].as_array().unwrap().len(), CHECKS.len());
    assert_eq!(json["checks"][0]["id"], "market_account");
    assert_eq!(
        json["strategy_metadata_uri"],
        "https://example.com/strategy.json"
    );
}

#[test]
fn missing_fee_account_warns_and_the_rest_still_runs() {
    let mut env = Env::new();
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let market = cp_market(
        &mut env,
        (mint_a, TOKEN),
        (mint_b, TOKEN),
        1_000_000_000,
        false,
    );
    let report = check_market(&env.svm, market.market, &CheckOptions::default());
    assert_all_pass(&report, &[("fee_accounts", Status::Warn)]);
    assert!(report
        .check("fee_accounts")
        .unwrap()
        .message
        .contains("init_fee_account"));
}

#[test]
fn paused_router_warns_and_checks_run_with_the_pause_lifted() {
    let mut env = Env::new();
    let market = plain_cp_market(&mut env);
    let admin = env.admin.pubkey();
    env.send(
        &[pipeline_client::update_config(
            &admin,
            &pipeline_client::UpdateConfigArgs {
                paused: Some(true),
                ..Default::default()
            },
        )],
        &[],
    );
    let report = check_market(&env.svm, market.market, &CheckOptions::default());
    assert_all_pass(&report, &[("market_account", Status::Warn)]);
}

#[test]
fn honest_hostile_strategy_passes_what_applies() {
    let mut env = Env::new();
    let market = evil_market(&mut env);
    let report = check_market(&env.svm, market.market, &options_with_extra(0));
    println!("{}", report.to_text());
    assert_all_pass(&report, &[("exact_out_refund", Status::Skip)]);
}

#[test]
fn quote_that_lies_fails_quote_matches_swap() {
    let mut env = Env::new();
    let market = evil_market(&mut env);
    // Quotes 2:1 but pays 1:1.
    let report = check_market(&env.svm, market.market, &options_with_extra(7));
    println!("{}", report.to_text());
    assert_statuses(
        &report,
        &[
            ("quotes", Status::Pass),
            ("quote_matches_swap", Status::Fail),
            ("min_out_enforced", Status::Skip),
        ],
    );
    let message = &report.check("quote_matches_swap").unwrap().message;
    assert!(message.contains("quoted 20000 out"), "{message}");
    assert!(message.contains("delivered 10000"), "{message}");
}

#[test]
fn short_output_fails_quote_matches_swap() {
    let mut env = Env::new();
    let market = evil_market(&mut env);
    // Quotes 1:1 but pays a single unit.
    let report = check_market(&env.svm, market.market, &options_with_extra(2));
    assert_statuses(&report, &[("quote_matches_swap", Status::Fail)]);
    let message = &report.check("quote_matches_swap").unwrap().message;
    assert!(message.contains("delivered 1 "), "{message}");
}

#[test]
fn swap_that_always_fails_is_reported() {
    let mut env = Env::new();
    let market = evil_market(&mut env);
    // Pays, then tries to pull more input from the trader, which can never work.
    let report = check_market(&env.svm, market.market, &options_with_extra(1));
    assert_statuses(
        &report,
        &[
            ("quotes", Status::Pass),
            ("quote_matches_swap", Status::Fail),
            ("compute", Status::Skip),
            ("round_trip", Status::Skip),
        ],
    );
    assert!(report
        .check("quote_matches_swap")
        .unwrap()
        .message
        .contains("the swap fails"));
}

#[test]
fn endless_quote_fails_quotes() {
    let mut env = Env::new();
    let market = evil_market(&mut env);
    let report = check_market(&env.svm, market.market, &options_with_extra(8));
    println!("{}", report.to_text());
    assert_statuses(
        &report,
        &[
            ("quotes", Status::Fail),
            ("quote_matches_swap", Status::Skip),
        ],
    );
    assert!(report
        .check("quotes")
        .unwrap()
        .message
        .contains("compute-unit cap"));
}

#[test]
fn market_info_that_changes_after_registration_fails() {
    let mut env = Env::new();
    let market = evil_market(&mut env);
    // The hostile state's `info_mode` byte: 3 reports one vault for two mints.
    let mut state = env.svm.get_account(&market.state).unwrap();
    state.data[129] = 3;
    env.svm.set_account(market.state, state).unwrap();
    let report = check_market(&env.svm, market.market, &options_with_extra(0));
    assert_statuses(&report, &[("market_info", Status::Fail)]);
    assert!(report
        .check("market_info")
        .unwrap()
        .message
        .contains("vaults differ"));
}

#[test]
fn not_a_market_fails_and_skips_the_rest() {
    let mut env = Env::new();
    let market = plain_cp_market(&mut env);
    // The strategy's own state is not the router's Market account.
    let report = check_market(&env.svm, market.state, &CheckOptions::default());
    assert_eq!(report.status("market_account"), Some(Status::Fail));
    assert_eq!(report.summary.skip, CHECKS.len() - 1);
    assert_eq!(report.summary.result, Status::Fail);

    let report = check_market(&env.svm, Pubkey::new_unique(), &CheckOptions::default());
    assert_eq!(report.status("market_account"), Some(Status::Fail));
}

#[test]
fn explicit_amounts_are_used() {
    let mut env = Env::new();
    let market = plain_cp_market(&mut env);
    let options = CheckOptions {
        amounts: Some(vec![12_345, 6_789_000]),
        ..CheckOptions::default()
    };
    let report = check_market(&env.svm, market.market, &options);
    assert_all_pass(&report, &[]);
    let quotes = &report.check("quotes").unwrap().details["quotes"];
    assert!(quotes
        .as_array()
        .unwrap()
        .iter()
        .any(|q| q["amount"] == 12_345));
    assert!(pda::config() != Pubkey::default());
}

#[test]
fn market_that_refuses_every_trade_warns_rather_than_fails() {
    let mut env = Env::new();
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    // No liquidity: the strategy refuses to price an empty pool, as it should.
    let market = cp_market(&mut env, (mint_a, TOKEN), (mint_b, TOKEN), 0, true);
    let report = check_market(&env.svm, market.market, &CheckOptions::default());
    println!("{}", report.to_text());
    assert_statuses(
        &report,
        &[
            ("quotes", Status::Warn),
            ("quote_matches_swap", Status::Skip),
            ("round_trip", Status::Skip),
        ],
    );
    assert_eq!(report.summary.fail, 0, "{}", report.to_text());
}

#[test]
fn router_minimum_output_error_number_matches_the_router() {
    // Anchor numbers a program's errors from 6000 in declaration order.
    let source = include_str!("../../../programs/pipeline/src/lib.rs");
    let body = &source[source.find("#[error_code]").unwrap()..];
    let body = &body[body.find('{').unwrap() + 1..body.find("\n}").unwrap()];
    let names: Vec<&str> = body
        .lines()
        .map(str::trim)
        .filter(|line| line.ends_with(',') && line.starts_with(|c: char| c.is_ascii_uppercase()))
        .map(|line| line.trim_end_matches(','))
        .collect();
    let index = names
        .iter()
        .position(|n| *n == "InsufficientOutput")
        .unwrap();
    assert_eq!(
        pipeline_check::ROUTER_INSUFFICIENT_OUTPUT,
        6000 + index as u32
    );
}

#[test]
fn every_direction_and_size_is_measured() {
    let mut env = Env::new();
    let market = plain_cp_market(&mut env);
    let report = check_market(&env.svm, market.market, &CheckOptions::default());

    // Round trips run in both orders of the pair.
    let trips = report.check("round_trip").unwrap().details["trips"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(trips, 2, "{}", report.to_text());

    // Transaction size is measured for every quoted case, not one per direction.
    let swaps = report.check("transaction_size").unwrap().details["swaps"]
        .as_array()
        .unwrap()
        .len();
    let quoted = report.check("quote_matches_swap").unwrap().details["swaps"]
        .as_array()
        .unwrap()
        .len();
    assert!(swaps > 2 && swaps >= quoted, "{}", report.to_text());

    // The CP strategy leaves the minimum to the router, so the router's own error refuses.
    let tested = report.check("min_out_enforced").unwrap().details["tested"]
        .as_array()
        .unwrap()
        .clone();
    assert!(!tested.is_empty());
    for case in tested {
        assert_eq!(case["refused_by"], "router", "{case}");
    }
}
