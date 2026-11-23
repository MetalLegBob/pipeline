//! Transfer-hook account resolution (`pipeline_client::hooks`) against Token-2022 itself.
//!
//! Both mints of a CP market use the test hook's seeded list, which names one extra
//! account of every kind a hook can ask for: a fixed address, PDAs seeded by the
//! transfer's accounts, by their data and by the amount, an external PDA and an address
//! read from account data. Token-2022 resolves that list on chain for every transfer and
//! fails one that is missing any account, so a swap built only from what the resolver
//! returns shows it resolves each kind the way Token-2022 does.
//!
//! With hooks on both mints a swap passes 21 hook accounts, too many for a legacy
//! transaction, so swaps are sent as v0 transactions with the market's lookup table, as
//! the hosted API builds them.

#![allow(clippy::result_large_err)]

mod common;
mod router_batch;

use std::collections::HashSet;

use common::*;
use pipeline_client::hooks::{self, AccountCache, HookError, HookTransfer, SwapAmounts};
use pipeline_client::{self as client, pda, AccountMeta, FeeSide, Instruction, Pubkey};
use pipeline_strategy_interface::fees;
use router_batch::{send_v0, set_lookup_table, FROZEN};
use solana_keypair::Keypair;
use solana_signer::Signer;

const TOKEN_2022: Pubkey = client::TOKEN_2022_PROGRAM_ID;
const HOOK_PROGRAM_ID: Pubkey = client::pubkey!("BfAcC8daPZSS4iEYz1figEshePUr96gDFwtytn7DyNZ3");
const RESERVE: u64 = 1_000_000_000;
const FEE_BPS: u16 = 30;
const SWAP: u64 = 1_000_000;
const INTEGRATOR_FEE_BPS: u16 = 50;

/// A Token-2022 mint whose test hook lists one extra account of every kind.
fn seeded_mint(env: &mut Env) -> Pubkey {
    let mint = env.create_mint(&TOKEN_2022, 6, None, Some(HOOK_PROGRAM_ID));
    let ix = Instruction::new_with_bytes(
        HOOK_PROGRAM_ID,
        b"tsthkin2",
        vec![
            AccountMeta::new(env.admin.pubkey(), true),
            AccountMeta::new(hooks::validation_address(&mint, &HOOK_PROGRAM_ID), false),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new_readonly(client::SYSTEM_PROGRAM_ID, false),
        ],
    );
    env.send(&[ix], &[]).expect("hook initialize");
    mint
}

/// Resolves `transfers`, fetching accounts from the SVM as the resolver asks for them.
fn resolve(env: &Env, transfers: &[HookTransfer]) -> Vec<AccountMeta> {
    let mut cache = AccountCache::new();
    loop {
        match hooks::resolve_all(&HOOK_PROGRAM_ID, transfers, &cache) {
            Ok(metas) => return metas,
            Err(HookError::NeedAccount(key)) => {
                cache.insert(key, env.svm.get_account(&key).map(|a| a.data));
            }
            Err(error) => panic!("{error}"),
        }
    }
}

struct World {
    env: Env,
    market: CpMarket,
    /// The market's lookup table and its addresses.
    table: (Pubkey, Vec<Pubkey>),
    trader: Keypair,
    trader_a: Pubkey,
    trader_b: Pubkey,
    integrator_a: Pubkey,
}

fn world() -> World {
    let mut env = Env::new();
    env.svm
        .add_program_from_file(HOOK_PROGRAM_ID, sbf_dir().join("test_hook.so"))
        .unwrap();
    env.register(&CP_PROGRAM_ID, "test_strategy_cp");
    let mint_a = seeded_mint(&mut env);
    let mint_b = seeded_mint(&mut env);

    let creator = env.funded_keypair();
    let ix = cp_create_market_ix(
        &creator.pubkey(),
        &mint_a,
        &TOKEN_2022,
        &mint_b,
        &TOKEN_2022,
        FEE_BPS,
    );
    env.send(&[ix], &[&creator]).expect("create_market");
    let pool = cp_pool(&mint_a, &mint_b, FEE_BPS);
    let market = CpMarket {
        pool,
        market: pda::market(&CP_PROGRAM_ID, &pool),
        mint_a,
        mint_b,
        token_program_a: TOKEN_2022,
        token_program_b: TOKEN_2022,
        vault_a: pda::associated_token(&pool, &mint_a, &TOKEN_2022),
        vault_b: pda::associated_token(&pool, &mint_b, &TOKEN_2022),
    };
    let payer = env.admin.pubkey();
    env.send(
        &[
            client::init_fee_account(&payer, &market.market, &mint_a, &TOKEN_2022),
            client::init_fee_account(&payer, &market.market, &mint_b, &TOKEN_2022),
        ],
        &[],
    )
    .expect("init fee accounts");

    // The deposit is two hooked transfers too; resolve them the same way.
    let lp = env.funded_keypair();
    let lp_a = env.create_ata(&lp.pubkey(), &mint_a, &TOKEN_2022);
    let lp_b = env.create_ata(&lp.pubkey(), &mint_b, &TOKEN_2022);
    env.mint_to(&mint_a, &TOKEN_2022, &lp_a, RESERVE);
    env.mint_to(&mint_b, &TOKEN_2022, &lp_b, RESERVE);
    let deposit = |source, mint, destination| HookTransfer {
        source,
        mint,
        destination,
        authority: lp.pubkey(),
        amount: RESERVE,
    };
    let mut deposit_hooks = resolve(&env, &[deposit(lp_a, mint_a, market.vault_a)]);
    hooks::merge(
        &mut deposit_hooks,
        resolve(&env, &[deposit(lp_b, mint_b, market.vault_b)]),
    );
    let ix = cp_deposit_ix(
        &lp.pubkey(),
        &market,
        &lp_a,
        &lp_b,
        RESERVE,
        RESERVE,
        deposit_hooks,
    );
    env.send(&[ix], &[&lp]).expect("deposit");

    let (trader, trader_a) = env.trader_with(&mint_a, &TOKEN_2022, RESERVE);
    let trader_b = env.create_ata(&trader.pubkey(), &mint_b, &TOKEN_2022);
    let integrator_a = env.create_ata(&Pubkey::new_unique(), &mint_a, &TOKEN_2022);
    let fee_authority = pda::fee_authority(&market.market);
    let addresses = vec![
        client::PROGRAM_ID,
        pda::event_authority(),
        market.market,
        pool,
        CP_PROGRAM_ID,
        fee_authority,
        mint_a,
        market.vault_a,
        TOKEN_2022,
        pda::fee_account(&market.market, &mint_a, &TOKEN_2022),
        mint_b,
        market.vault_b,
        pda::fee_account(&market.market, &mint_b, &TOKEN_2022),
    ];
    let table = (set_lookup_table(&mut env, &addresses, FROZEN), addresses);
    World {
        env,
        market,
        table,
        trader,
        trader_a,
        trader_b,
        integrator_a,
    }
}

/// Sends one swap as a v0 transaction with the market's lookup table. Five hooked
/// transfers need more than the default 200,000 compute units.
fn send_swap(w: &mut World, ix: Instruction) -> TxResult {
    let table = w.table.clone();
    let mut limit = vec![2u8];
    limit.extend_from_slice(&400_000u32.to_le_bytes());
    let budget = Instruction::new_with_bytes(
        client::pubkey!("ComputeBudget111111111111111111111111111111"),
        &limit,
        vec![],
    );
    send_v0(&mut w.env, &[budget, ix], &[&w.trader], &[table])
}

struct ResolvedSwap {
    accounts: client::SwapAccounts,
    args: client::SwapArgs,
    expected_out: u64,
}

/// An exact-in swap of A for B with an integrator fee, its hook accounts on both sides
/// coming only from the resolver.
fn resolved_swap(w: &World) -> ResolvedSwap {
    let m = &w.market;
    let mut accounts = cp_swap_accounts(
        m,
        &w.trader.pubkey(),
        &m.mint_a,
        &w.trader_a,
        &m.mint_b,
        &w.trader_b,
    );
    accounts.integrator_fee_account = Some(w.integrator_a);
    let expected_out = cp_out(RESERVE, RESERVE, SWAP, FEE_BPS);
    let amounts = SwapAmounts {
        amount_in: SWAP,
        strategy_amount_out: expected_out,
        pipeline_fee: fees::pipeline_fee(SWAP),
        integrator_fee: fees::ceil_bps(SWAP, INTEGRATOR_FEE_BPS as u64).unwrap(),
    };
    let transfers = hooks::swap_transfers(&accounts, FeeSide::Input, m.vault_b, m.pool, amounts);
    assert_eq!(
        transfers.input.len(),
        3,
        "vault, Pipeline fee and integrator fee"
    );
    assert_eq!(transfers.output.len(), 1);
    accounts.input_hook_accounts = resolve(&w.env, &transfers.input);
    hooks::merge(
        &mut accounts.strategy_accounts,
        resolve(&w.env, &transfers.output),
    );
    let mut args = exact_in(SWAP, expected_out);
    args.integrator_fee_bps = INTEGRATOR_FEE_BPS;
    ResolvedSwap {
        accounts,
        args,
        expected_out,
    }
}

#[test]
fn swaps_with_resolved_hook_accounts_on_both_sides() {
    let mut w = world();
    let swap = resolved_swap(&w);
    // Validation, six entries and the program, for each side's mint. Two entries depend
    // on the destination (its owner, and a PDA of the owner and amount), so the input's
    // three transfers (vault, Pipeline fee, integrator fee) need three of each.
    assert_eq!(swap.accounts.input_hook_accounts.len(), 8 + 2 * 2);
    assert_eq!(swap.accounts.strategy_accounts.len(), 1 + 8);

    let before_a = w.env.token_balance(&w.trader_a);
    let ix = client::swap(&swap.accounts, &swap.args);
    send_swap(&mut w, ix).expect("swap");
    assert_eq!(w.env.token_balance(&w.trader_b), swap.expected_out);
    let spent = before_a - w.env.token_balance(&w.trader_a);
    assert_eq!(
        spent,
        SWAP + fees::pipeline_fee(SWAP) + fees::ceil_bps(SWAP, INTEGRATOR_FEE_BPS as u64).unwrap()
    );
}

/// Every resolved account is one Token-2022 needs: leave any one out and the swap fails.
#[test]
fn every_resolved_account_is_needed() {
    let mut w = world();
    let swap = resolved_swap(&w);
    // The output vault is the strategy's own account, not a hook account.
    let output_vault = w.market.vault_b;
    let mut seen = HashSet::new();
    let input = swap.accounts.input_hook_accounts.clone();
    let output = swap.accounts.strategy_accounts.clone();
    let mut tried = 0;

    for (index, meta) in input.iter().enumerate() {
        let mut accounts = swap.accounts.clone();
        accounts.input_hook_accounts.remove(index);
        let mut args = swap.args.clone();
        args.input_hook_accounts = accounts.input_hook_accounts.len() as u8;
        // A hook account both sides use is still passed through the other side's list.
        if output.iter().any(|m| m.pubkey == meta.pubkey) {
            continue;
        }
        let result = send_swap(&mut w, client::swap(&accounts, &args));
        assert!(
            result.is_err(),
            "swap passed without input account {}",
            meta.pubkey
        );
        seen.insert(meta.pubkey);
        tried += 1;
    }
    for (index, meta) in output.iter().enumerate() {
        if meta.pubkey == output_vault || input.iter().any(|m| m.pubkey == meta.pubkey) {
            continue;
        }
        let mut accounts = swap.accounts.clone();
        accounts.strategy_accounts.remove(index);
        let result = send_swap(&mut w, client::swap(&accounts, &swap.args));
        assert!(
            result.is_err(),
            "swap passed without output account {}",
            meta.pubkey
        );
        seen.insert(meta.pubkey);
        tried += 1;
    }
    // Both mints share the hook program, the fixed address and the external PDA built
    // on it, so those pass through either side's list and aren't left out alone. That
    // leaves 9 of the input's 12 and 5 of the output's 8, every one of which is needed.
    assert_eq!(tried, seen.len());
    assert_eq!(tried, 9 + 5);

    // And with all of them, it works.
    let ix = client::swap(&swap.accounts, &swap.args);
    send_swap(&mut w, ix).expect("swap");
}

/// Writable entries must be passed writable: Token-2022 refuses to pass a read-only
/// account to the hook as writable.
#[test]
fn writable_entries_must_be_writable() {
    let mut w = world();
    let swap = resolved_swap(&w);
    let mut accounts = swap.accounts.clone();
    let writable = accounts
        .input_hook_accounts
        .iter_mut()
        .find(|m| m.is_writable)
        .expect("the seeded list has writable entries");
    writable.is_writable = false;
    let result = send_swap(&mut w, client::swap(&accounts, &swap.args));
    assert!(result.is_err());
}

/// The output side's transfers when Pipeline's fee is taken in the output: the strategy's
/// payout and the fee from the trader's output account.
#[test]
fn output_side_fee_adds_its_transfer() {
    let w = world();
    let m = &w.market;
    let accounts = cp_swap_accounts(
        m,
        &w.trader.pubkey(),
        &m.mint_a,
        &w.trader_a,
        &m.mint_b,
        &w.trader_b,
    );
    let amounts = SwapAmounts {
        amount_in: SWAP,
        strategy_amount_out: 900,
        pipeline_fee: 3,
        integrator_fee: 0,
    };
    let transfers = hooks::swap_transfers(&accounts, FeeSide::Output, m.vault_b, m.pool, amounts);
    assert_eq!(transfers.input.len(), 1);
    assert_eq!(
        transfers.output[1],
        HookTransfer {
            source: w.trader_b,
            mint: m.mint_b,
            destination: pda::fee_account(&m.market, &m.mint_b, &TOKEN_2022),
            authority: w.trader.pubkey(),
            amount: 3,
        }
    );
}

/// The resolver reads the hook program from real Token-2022 mints, including one with
/// another extension before the hook, and finds none on a mint without one.
#[test]
fn reads_the_hook_program_from_real_mints() {
    let mut env = Env::new();
    let plain = env.create_mint(&TOKEN_2022, 6, None, None);
    let hooked = env.create_mint(&TOKEN_2022, 6, None, Some(HOOK_PROGRAM_ID));
    let fee_and_hook = env.create_mint(&TOKEN_2022, 6, Some((100, 1_000)), Some(HOOK_PROGRAM_ID));
    let classic = env.create_mint(&client::TOKEN_PROGRAM_ID, 6, None, None);
    let program = |mint: &Pubkey| {
        let account = env.svm.get_account(mint).unwrap();
        hooks::transfer_hook_program(&account.owner, &account.data)
    };
    assert_eq!(program(&plain), None);
    assert_eq!(program(&hooked), Some(HOOK_PROGRAM_ID));
    assert_eq!(program(&fee_and_hook), Some(HOOK_PROGRAM_ID));
    assert_eq!(program(&classic), None);
}
