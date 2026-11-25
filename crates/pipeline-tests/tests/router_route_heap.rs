//! A route's hops must fit in the router's heap with as many strategy accounts as real
//! strategies pass (tick arrays, oracles, bins).
//!
//! The router's heap is Solana's default 32 KiB bump allocator, which never frees. Its
//! cross-program calls size their account lists once (a growing list leaves every
//! outgrown buffer behind), so with distinct accounts a route reaches Solana's limit of
//! 64 accounts per transaction before it runs out of heap, at every length. Measured:
//!
//! | Hops | Distinct extra accounts per hop | Then | One account repeated per hop | Then |
//! |---|---|---|---|---|
//! | 1 | 48 | account limit | 123 | heap |
//! | 2 | 20 | account limit | 69 | heap |
//! | 3 | 10 | account limit | 41 | heap |
//! | 4 | 6 | account limit | 26 | heap |

#![allow(clippy::result_large_err)]

mod common;
mod router_batch;

use common::*;
use pipeline_client::{self as client, AccountMeta, FeeSide, Pubkey};
use router_batch::*;
use solana_signer::Signer;

/// A → B → SOL → C → D through four CP markets, each hop passing ten strategy accounts
/// (the output vault and nine more, which the CP strategy ignores). Before the router
/// sized its account lists once, this ran out of heap in hop 4.
#[test]
fn four_hops_with_ten_strategy_accounts_each_fit_in_the_heap() {
    let mut env = Env::new();
    let a = env.create_mint(&TOKEN, 6, None, None);
    let b = env.create_mint(&TOKEN, 6, None, None);
    let c = env.create_mint(&TOKEN, 6, None, None);
    let d = env.create_mint(&TOKEN, 6, None, None);
    let ab = token_market(&mut env, a, b);
    let b_sol = sol_market(&mut env, b);
    let c_sol = sol_market(&mut env, c);
    let cd = token_market(&mut env, c, d);
    let (trader, ta) = env.trader_with(&a, &TOKEN, 100_000_000_000);
    let owner = trader.pubkey();
    let tb = env.create_ata(&owner, &b, &TOKEN);
    let tc = env.create_ata(&owner, &c, &TOKEN);
    let td = env.create_ata(&owner, &d, &TOKEN);
    let tsol = wsol_account(&mut env, &owner, 0);

    let extra: Vec<AccountMeta> = (0..9)
        .map(|_| AccountMeta::new_readonly(Pubkey::new_unique(), false))
        .collect();
    let hops: Vec<client::RouteHopSpec> = [
        (&ab, a, ta, b, tb),
        (&b_sol, b, tb, NATIVE, tsol),
        (&c_sol, NATIVE, tsol, c, tc),
        (&cd, c, tc, d, td),
    ]
    .into_iter()
    .map(|(market, input, trader_input, output, trader_output)| {
        let mut accounts = cp_swap_accounts(
            market,
            &owner,
            &input,
            &trader_input,
            &output,
            &trader_output,
        );
        accounts.strategy_accounts.extend(extra.iter().cloned());
        client::RouteHopSpec {
            accounts,
            fee_side: FeeSide::of(&env.market(&market.market), &output),
            extra: vec![],
        }
    })
    .collect();
    let ix = client::route(&owner, None, &hops, 1_000_000_000, 1, 0);

    // Too large for a legacy transaction: a lookup table holds every account but the
    // trader.
    let mut addresses: Vec<Pubkey> = vec![];
    for meta in &ix.accounts {
        if meta.pubkey != owner && !addresses.contains(&meta.pubkey) {
            addresses.push(meta.pubkey);
        }
    }
    let table = set_lookup_table(&mut env, &addresses, FROZEN);
    let meta = send_v0(&mut env, &[ix], &[&trader], &[(table, addresses)])
        .expect("a four-hop route with ten strategy accounts per hop");
    assert_eq!(swap_events(&meta).len(), 4);
}

/// With distinct accounts, every route length reaches the transaction's account limit
/// before the router's heap.
#[test]
fn routes_reach_the_account_limit_before_the_heap() {
    for (hop_count, fits) in [(1, 48), (2, 20), (3, 10), (4, 6)] {
        assert_eq!(
            try_route(hop_count, fits, false),
            Ok(()),
            "{hop_count} hops"
        );
        assert_eq!(
            try_route(hop_count, fits + 1, false),
            Err("TooManyAccountLocks".to_string()),
            "{hop_count} hops"
        );
    }
}

/// Prints, for routes of 1 to 4 hops through the markets above, the most extra strategy
/// accounts per hop that still trade, and why the next one fails (the table at the top).
/// Run with `--ignored --nocapture`.
#[test]
#[ignore]
fn measure_route_heap_ceilings() {
    for repeated in [true, false] {
        for hop_count in 1..=4 {
            let mut last_ok = 0;
            let mut failure = String::new();
            for extra in (0..200).step_by(1) {
                match try_route(hop_count, extra, repeated) {
                    Ok(()) => last_ok = extra,
                    Err(e) => {
                        failure = e;
                        break;
                    }
                }
            }
            println!(
                "{} extra accounts, {hop_count} hops: {last_ok} per hop fit; next: {failure}",
                if repeated { "repeated" } else { "new" }
            );
        }
    }
}

fn try_route(hop_count: usize, extra_count: usize, repeated: bool) -> Result<(), String> {
    let mut env = Env::new();
    let a = env.create_mint(&TOKEN, 6, None, None);
    let b = env.create_mint(&TOKEN, 6, None, None);
    let c = env.create_mint(&TOKEN, 6, None, None);
    let d = env.create_mint(&TOKEN, 6, None, None);
    let ab = token_market(&mut env, a, b);
    let b_sol = sol_market(&mut env, b);
    let c_sol = sol_market(&mut env, c);
    let cd = token_market(&mut env, c, d);
    let (trader, ta) = env.trader_with(&a, &TOKEN, 100_000_000_000);
    let owner = trader.pubkey();
    let tb = env.create_ata(&owner, &b, &TOKEN);
    let tc = env.create_ata(&owner, &c, &TOKEN);
    let td = env.create_ata(&owner, &d, &TOKEN);
    let tsol = wsol_account(&mut env, &owner, 0);
    let shared = Pubkey::new_unique();
    let all = [
        (&ab, a, ta, b, tb),
        (&b_sol, b, tb, NATIVE, tsol),
        (&c_sol, NATIVE, tsol, c, tc),
        (&cd, c, tc, d, td),
    ];
    let hops: Vec<client::RouteHopSpec> = all[..hop_count]
        .iter()
        .map(|&(market, input, trader_input, output, trader_output)| {
            let mut accounts = cp_swap_accounts(
                market,
                &owner,
                &input,
                &trader_input,
                &output,
                &trader_output,
            );
            accounts.strategy_accounts.extend((0..extra_count).map(|_| {
                AccountMeta::new_readonly(
                    if repeated {
                        shared
                    } else {
                        Pubkey::new_unique()
                    },
                    false,
                )
            }));
            client::RouteHopSpec {
                accounts,
                fee_side: FeeSide::of(&env.market(&market.market), &output),
                extra: vec![],
            }
        })
        .collect();
    let ix = client::route(&owner, None, &hops, 1_000_000_000, 1, 0);
    let mut addresses: Vec<Pubkey> = vec![];
    for meta in &ix.accounts {
        if meta.pubkey != owner && !addresses.contains(&meta.pubkey) {
            addresses.push(meta.pubkey);
        }
    }
    let table = set_lookup_table(&mut env, &addresses, FROZEN);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        send_v0(&mut env, &[ix], &[&trader], &[(table, addresses)])
    }))
    .map_err(|_| "over the transaction size limit".to_string())?;
    match result {
        Ok(_) => Ok(()),
        Err(failed) => Err(
            if failed.meta.logs.iter().any(|l| l.contains("out of memory")) {
                "heap".to_string()
            } else {
                format!("{:?}", failed.err)
            },
        ),
    }
}
