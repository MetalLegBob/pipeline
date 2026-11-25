//! The router called by another program: the example DCA vault
//! (`programs/example-dca-vault`), a program-owned wallet that swaps through the router
//! with `pipeline-cpi`, signing as its PDA.
//!
//! - Exact-in swaps through the vault are measured and charged exactly as a wallet's are,
//!   with Pipeline's fee on the input, or on the output when the output is the market's
//!   fee mint (wrapped SOL here).
//! - The vault's minimum is enforced on what it keeps, and the `SwapResult` /
//!   `RouteResult` the vault reads back from the router match the balances and events.
//! - A two-hop `route` works through the vault.
//! - The vault refuses a trade whose output account isn't its own.
//! - Call depth: with the vault at height 1, a transfer hook on the input runs at height
//!   4 and one on the output at height 5, the limit, so both work. A hook that makes a
//!   call of its own on the output side would need height 6: it works for a wallet and
//!   fails, cleanly, through the vault.
//!
//! Each test prints the stack heights the runtime reported and the compute used; run with
//! `--nocapture` to see them.

#![allow(clippy::result_large_err)]

mod common;
mod router_batch;

use common::*;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use pipeline_strategy_interface::fees;
use router_batch::*;
use solana_keypair::Keypair;
use solana_signer::Signer;

const VAULT_PROGRAM_ID: Pubkey = client::pubkey!("8XjwqK6qERiSUPqci7JEgUWN2P3ifpAEuMwFvrMiqQA7");
const HOOK_PROGRAM_ID: Pubkey = client::pubkey!("BfAcC8daPZSS4iEYz1figEshePUr96gDFwtytn7DyNZ3");
const REFILL_HOOK_ID: Pubkey = client::pubkey!("BfAcC8daPZSS4iEYz1figEshePUr96gDFwtytn7DyNZ4");
const TOKEN_2022: Pubkey = client::TOKEN_2022_PROGRAM_ID;
const RESERVE: u64 = 1_000_000_000_000;
const POOL_FEE_BPS: u16 = 30;
/// What the vault spends per purchase.
const AMOUNT: u64 = 1_000_000_000;

/// `pipeline-cpi`'s error for a token account that isn't the expected one.
const WRONG_TOKEN_ACCOUNT: u32 = 0x5049_5007;
/// The vault's `TooSoon` error.
const TOO_SOON: u32 = 0xDCA4;

// ------------------------------------------------------------------ the vault

/// The vault's account, decoded (see `Vault` in the example program).
#[derive(Debug, PartialEq, Eq)]
struct VaultState {
    owner: Pubkey,
    input_mint: Pubkey,
    output_mint: Pubkey,
    amount_per_swap: u64,
    min_out_per_swap: u64,
    swaps: u64,
    total_sent: u64,
    total_received: u64,
}

fn vault_state(env: &Env, vault: &Pubkey) -> VaultState {
    let data = env.account_data(vault);
    assert_eq!(&data[..8], b"dcavault");
    let key = |at: usize| Pubkey::new_from_array(data[at..at + 32].try_into().unwrap());
    let number = |i: usize| {
        let at = 104 + 8 * i;
        u64::from_le_bytes(data[at..at + 8].try_into().unwrap())
    };
    VaultState {
        owner: key(8),
        input_mint: key(40),
        output_mint: key(72),
        amount_per_swap: number(0),
        min_out_per_swap: number(1),
        swaps: number(4),
        total_sent: number(5),
        total_received: number(6),
    }
}

/// A vault and its token accounts.
struct Vault {
    owner: Keypair,
    address: Pubkey,
    input: Pubkey,
    output: Pubkey,
}

fn deploy_vault_program(env: &mut Env) {
    env.svm
        .add_program_from_file(VAULT_PROGRAM_ID, sbf_dir().join("example_dca_vault.so"))
        .expect("build programs first: scripts/build-programs.sh example-dca-vault");
}

/// Creates a vault buying `output_mint` with `amount` of `input_mint` per purchase, no
/// less than `min_out` each time, and funds it with `funding` of the input.
#[allow(clippy::too_many_arguments)]
fn create_vault(
    env: &mut Env,
    input_mint: Pubkey,
    input_program: Pubkey,
    output_mint: Pubkey,
    output_program: Pubkey,
    amount: u64,
    min_out: u64,
    interval_seconds: i64,
    funding: u64,
) -> Vault {
    if env.svm.get_account(&VAULT_PROGRAM_ID).is_none() {
        deploy_vault_program(env);
    }
    let owner = env.funded_keypair();
    let address = Pubkey::find_program_address(
        &[
            b"vault",
            owner.pubkey().as_ref(),
            input_mint.as_ref(),
            output_mint.as_ref(),
        ],
        &VAULT_PROGRAM_ID,
    )
    .0;
    let mut data = vec![0u8];
    data.extend_from_slice(&amount.to_le_bytes());
    data.extend_from_slice(&min_out.to_le_bytes());
    data.extend_from_slice(&interval_seconds.to_le_bytes());
    let create = Instruction::new_with_bytes(
        VAULT_PROGRAM_ID,
        &data,
        vec![
            AccountMeta::new(owner.pubkey(), true),
            AccountMeta::new(address, false),
            AccountMeta::new_readonly(input_mint, false),
            AccountMeta::new_readonly(output_mint, false),
            AccountMeta::new_readonly(client::SYSTEM_PROGRAM_ID, false),
        ],
    );
    env.send(&[create], &[&owner]).expect("create vault");
    let input = env.create_ata(&address, &input_mint, &input_program);
    if funding > 0 {
        env.mint_to(&input_mint, &input_program, &input, funding);
    }
    let output = env.create_ata(&address, &output_mint, &output_program);
    Vault {
        owner,
        address,
        input,
        output,
    }
}

/// The vault's crank: its own account, then the router's `swap` accounts in the router's
/// order with the vault as the trader, which the vault signs for itself. `swap` has no
/// `extra` data, so its input-hook count is the byte before the empty `extra`'s length.
fn crank_swap(vault: &Vault, swap: &Instruction) -> Instruction {
    assert_eq!(swap.data[swap.data.len() - 4..], [0; 4]);
    let hooks = swap.data[swap.data.len() - 5];
    assert_eq!(swap.data[..8], client::instruction_discriminator("swap"));
    crank(vault, swap, vec![1, hooks])
}

fn crank_route(vault: &Vault, route: &Instruction, shapes: &[(u8, u8)]) -> Instruction {
    let mut data = vec![2, shapes.len() as u8];
    for (strategy_accounts, input_hook_accounts) in shapes {
        data.extend([*strategy_accounts, *input_hook_accounts]);
    }
    crank(vault, route, data)
}

fn crank(vault: &Vault, router_ix: &Instruction, data: Vec<u8>) -> Instruction {
    let mut accounts = vec![AccountMeta::new(vault.address, false)];
    for meta in &router_ix.accounts {
        let mut meta = meta.clone();
        if meta.pubkey == vault.address {
            meta.is_signer = false;
        }
        accounts.push(meta);
    }
    Instruction::new_with_bytes(VAULT_PROGRAM_ID, &data, accounts)
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

// --------------------------------------------------------------- measurement

/// The stack height of every program invocation, from the runtime's
/// `Program <id> invoke [<height>]` log lines.
fn invocations(logs: &[String]) -> Vec<(Pubkey, usize)> {
    logs.iter()
        .filter_map(|line| {
            let rest = line.strip_prefix("Program ")?;
            let (program, rest) = rest.split_once(" invoke [")?;
            let height = rest.strip_suffix(']')?.parse().ok()?;
            Some((program.parse().ok()?, height))
        })
        .collect()
}

fn name(program: &Pubkey) -> String {
    let names = [
        (VAULT_PROGRAM_ID, "dca vault"),
        (client::PROGRAM_ID, "router"),
        (CP_PROGRAM_ID, "CP strategy"),
        (TOKEN, "SPL Token"),
        (TOKEN_2022, "Token-2022"),
        (HOOK_PROGRAM_ID, "test hook"),
        (REFILL_HOOK_ID, "refill hook"),
    ];
    names
        .iter()
        .find(|(id, _)| id == program)
        .map(|(_, name)| name.to_string())
        .unwrap_or_else(|| program.to_string())
}

/// The deepest height each program ran at, in order of first appearance.
fn deepest(logs: &[String]) -> Vec<(String, usize)> {
    let mut deepest: Vec<(String, usize)> = vec![];
    for (program, height) in invocations(logs) {
        let program = name(&program);
        match deepest.iter_mut().find(|(p, _)| *p == program) {
            Some((_, h)) => *h = (*h).max(height),
            None => deepest.push((program, height)),
        }
    }
    deepest
}

fn max_height(logs: &[String]) -> usize {
    invocations(logs).iter().map(|(_, h)| *h).max().unwrap_or(0)
}

fn report(label: &str, logs: &[String], compute_units: u64) {
    println!(
        "{label}: max stack height {}, {compute_units} compute units",
        max_height(logs)
    );
    for (program, height) in deepest(logs) {
        println!("    {program:<12} deepest at {height}");
    }
}

/// The vault's own report of its stack height (`pipeline_cpi::log_stack_height`).
fn logged_height(logs: &[String]) -> usize {
    logs.iter()
        .find_map(|line| {
            let rest = line.strip_prefix("Program log: dca-vault: stack height ")?;
            rest.split_once(" of ")?.0.parse().ok()
        })
        .expect("the vault logs its stack height")
}

/// What the vault logged from the `SwapResult` it read back: (sent, kept, pipeline fee).
fn logged_swap_result(logs: &[String]) -> (u64, u64, u64) {
    let line = logs
        .iter()
        .find_map(|line| line.strip_prefix("Program log: dca-vault: sent "))
        .expect("the vault logs the result");
    let numbers: Vec<u64> = line
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .map(|s| s.parse().unwrap())
        .collect();
    (numbers[0], numbers[1], numbers[2])
}

// ---------------------------------------------------------------------- swaps

/// Selling one SPL token for another: Pipeline's fee is 0.25% of the input, on top.
#[test]
fn the_vault_swaps_exact_in_with_the_fee_on_the_input() {
    let mut env = Env::new();
    let a = env.create_mint(&TOKEN, 6, None, None);
    let b = env.create_mint(&TOKEN, 6, None, None);
    let market = token_market(&mut env, a, b);
    let expected_out = cp_out(RESERVE, RESERVE, AMOUNT, POOL_FEE_BPS);
    let vault = create_vault(
        &mut env,
        a,
        TOKEN,
        b,
        TOKEN,
        AMOUNT,
        expected_out,
        0,
        10 * AMOUNT,
    );
    let fee_account = pda::fee_account(&market.market, &a, &TOKEN);
    let input_before = env.token_balance(&vault.input);

    let swap = client::swap(
        &cp_swap_accounts(&market, &vault.address, &a, &vault.input, &b, &vault.output),
        &exact_in(AMOUNT, expected_out),
    );
    let meta = env.send(&[crank_swap(&vault, &swap)], &[]).expect("crank");
    report(
        "vault swap, fee on input",
        &meta.logs,
        meta.compute_units_consumed,
    );

    let fee = fees::pipeline_fee(AMOUNT);
    assert_eq!(input_before - env.token_balance(&vault.input), AMOUNT + fee);
    assert_eq!(env.token_balance(&fee_account), fee);
    assert_eq!(env.token_balance(&vault.output), expected_out);

    let event = swap_events(&meta).pop().unwrap();
    assert_eq!(event.trader, vault.address);
    assert_eq!(event.net_sent, AMOUNT);
    assert_eq!(event.received, expected_out);
    assert_eq!(event.pipeline_fee, fee);
    assert_eq!(event.fee_mint, a);

    // What the vault read back from the router is what happened.
    assert_eq!(logged_swap_result(&meta.logs), (AMOUNT, expected_out, fee));
    assert_eq!(meta.return_data.program_id, client::PROGRAM_ID);
    let state = vault_state(&env, &vault.address);
    assert_eq!(state.swaps, 1);
    assert_eq!(state.total_sent, AMOUNT);
    assert_eq!(state.total_received, expected_out);

    assert_eq!(logged_height(&meta.logs), 1);
    assert_eq!(max_height(&meta.logs), 4);

    // The same swap from a wallet, for the compute comparison.
    let (trader, trader_a) = env.trader_with(&a, &TOKEN, 10 * AMOUNT);
    let trader_b = env.create_ata(&trader.pubkey(), &b, &TOKEN);
    let direct = client::swap(
        &cp_swap_accounts(&market, &trader.pubkey(), &a, &trader_a, &b, &trader_b),
        &exact_in(AMOUNT, 1),
    );
    let meta = env.send(&[direct], &[&trader]).expect("wallet swap");
    report(
        "wallet swap, same market",
        &meta.logs,
        meta.compute_units_consumed,
    );
}

/// Buying wrapped SOL: SOL is the market's fee mint, so Pipeline's fee comes out of the
/// SOL that arrived, and the vault's minimum applies to what it keeps.
#[test]
fn buying_sol_pays_the_fee_in_sol_and_the_minimum_applies_to_what_is_kept() {
    let mut env = Env::new();
    let token = env.create_mint(&TOKEN, 6, None, None);
    let market = sol_market(&mut env, token);
    let received = cp_out(RESERVE, 100 * SOL, AMOUNT, POOL_FEE_BPS);
    let fee = fees::pipeline_fee(received);
    let kept = received - fee;

    // A minimum of everything that arrives is more than the vault keeps: refused.
    let greedy = create_vault(
        &mut env,
        token,
        TOKEN,
        NATIVE,
        TOKEN,
        AMOUNT,
        received,
        0,
        AMOUNT * 2,
    );
    let swap = |vault: &Vault, min| {
        client::swap(
            &cp_swap_accounts(
                &market,
                &vault.address,
                &token,
                &vault.input,
                &NATIVE,
                &vault.output,
            ),
            &exact_in(AMOUNT, min),
        )
    };
    let result = env.send(&[crank_swap(&greedy, &swap(&greedy, received))], &[]);
    assert_fails_with(result, "InsufficientOutput");
    assert_eq!(env.token_balance(&greedy.input), AMOUNT * 2);
    assert_eq!(vault_state(&env, &greedy.address).swaps, 0);

    // Exactly what it keeps: accepted.
    let vault = create_vault(
        &mut env,
        token,
        TOKEN,
        NATIVE,
        TOKEN,
        AMOUNT,
        kept,
        0,
        AMOUNT * 2,
    );
    let sol_fees = pda::fee_account(&market.market, &NATIVE, &TOKEN);
    let token_fees = pda::fee_account(&market.market, &token, &TOKEN);
    let meta = env
        .send(&[crank_swap(&vault, &swap(&vault, kept))], &[])
        .expect("crank");
    report(
        "vault swap, fee on output",
        &meta.logs,
        meta.compute_units_consumed,
    );

    assert_eq!(env.token_balance(&vault.input), AMOUNT);
    assert_eq!(env.token_balance(&vault.output), kept);
    assert_eq!(env.token_balance(&sol_fees), fee);
    assert_eq!(env.token_balance(&token_fees), 0);
    let event = swap_events(&meta).pop().unwrap();
    assert_eq!(event.trader, vault.address);
    assert_eq!(event.received, received);
    assert_eq!(event.pipeline_fee, fee);
    assert_eq!(event.fee_mint, NATIVE);
    assert_eq!(logged_swap_result(&meta.logs), (AMOUNT, kept, fee));
    let state = vault_state(&env, &vault.address);
    assert_eq!(state.total_received, kept);
    assert_eq!(max_height(&meta.logs), 4);
}

/// Selling wrapped SOL: SOL is the input and the fee mint, so the fee is on the input.
#[test]
fn selling_sol_pays_the_fee_on_the_input() {
    let mut env = Env::new();
    let token = env.create_mint(&TOKEN, 6, None, None);
    let market = sol_market(&mut env, token);
    let amount = SOL / 10;
    let out = cp_out(100 * SOL, RESERVE, amount, POOL_FEE_BPS);
    let vault = create_vault(&mut env, NATIVE, TOKEN, token, TOKEN, amount, out, 0, SOL);
    let swap = client::swap(
        &cp_swap_accounts(
            &market,
            &vault.address,
            &NATIVE,
            &vault.input,
            &token,
            &vault.output,
        ),
        &exact_in(amount, out),
    );
    env.send(&[crank_swap(&vault, &swap)], &[]).expect("crank");
    let fee = fees::pipeline_fee(amount);
    assert_eq!(env.token_balance(&vault.input), SOL - amount - fee);
    assert_eq!(env.token_balance(&vault.output), out);
    assert_eq!(
        env.token_balance(&pda::fee_account(&market.market, &NATIVE, &TOKEN)),
        fee
    );
}

/// The router measures whatever output account it is given. Only the caller knows which
/// is its own, so the vault refuses a crank that sends its purchase elsewhere.
#[test]
fn the_vault_refuses_output_sent_to_someone_else() {
    let mut env = Env::new();
    let a = env.create_mint(&TOKEN, 6, None, None);
    let b = env.create_mint(&TOKEN, 6, None, None);
    let market = token_market(&mut env, a, b);
    let vault = create_vault(&mut env, a, TOKEN, b, TOKEN, AMOUNT, 1, 0, 10 * AMOUNT);
    let thief = env.funded_keypair();
    let thief_b = env.create_ata(&thief.pubkey(), &b, &TOKEN);

    let swap = client::swap(
        &cp_swap_accounts(&market, &vault.address, &a, &vault.input, &b, &thief_b),
        &exact_in(AMOUNT, 1),
    );
    let result = env.send(&[crank_swap(&vault, &swap)], &[]);
    assert_fails_with(result, &format!("Custom({WRONG_TOKEN_ACCOUNT})"));
    assert_eq!(env.token_balance(&vault.input), 10 * AMOUNT);
    assert_eq!(env.token_balance(&thief_b), 0);
}

/// Purchases are spaced by the vault's interval.
#[test]
fn purchases_wait_for_the_interval() {
    let mut env = Env::new();
    let a = env.create_mint(&TOKEN, 6, None, None);
    let b = env.create_mint(&TOKEN, 6, None, None);
    let market = token_market(&mut env, a, b);
    let vault = create_vault(&mut env, a, TOKEN, b, TOKEN, AMOUNT, 1, 3_600, 10 * AMOUNT);
    let swap = client::swap(
        &cp_swap_accounts(&market, &vault.address, &a, &vault.input, &b, &vault.output),
        &exact_in(AMOUNT, 1),
    );
    env.send(&[crank_swap(&vault, &swap)], &[]).expect("first");
    assert_fails_with(
        env.send(&[crank_swap(&vault, &swap)], &[]),
        &format!("Custom({TOO_SOON})"),
    );
    let mut clock: solana_clock::Clock = env.svm.get_sysvar();
    clock.unix_timestamp += 3_600;
    env.svm.set_sysvar(&clock);
    env.send(&[crank_swap(&vault, &swap)], &[]).expect("second");
    assert_eq!(vault_state(&env, &vault.address).swaps, 2);
}

/// Only the owner withdraws, and the vault signs the transfer as the token accounts'
/// owner.
#[test]
fn only_the_owner_withdraws() {
    let mut env = Env::new();
    let a = env.create_mint(&TOKEN, 6, None, None);
    let b = env.create_mint(&TOKEN, 6, None, None);
    let vault = create_vault(&mut env, a, TOKEN, b, TOKEN, AMOUNT, 1, 0, AMOUNT);
    let destination = env.create_ata(&vault.owner.pubkey(), &a, &TOKEN);
    let withdraw = |signer: &Pubkey| {
        let mut data = vec![3u8];
        data.extend_from_slice(&AMOUNT.to_le_bytes());
        Instruction::new_with_bytes(
            VAULT_PROGRAM_ID,
            &data,
            vec![
                AccountMeta::new_readonly(*signer, true),
                AccountMeta::new_readonly(vault.address, false),
                AccountMeta::new(vault.input, false),
                AccountMeta::new_readonly(a, false),
                AccountMeta::new(destination, false),
                AccountMeta::new_readonly(TOKEN, false),
            ],
        )
    };
    let stranger = env.funded_keypair();
    assert!(env
        .send(&[withdraw(&stranger.pubkey())], &[&stranger])
        .is_err());
    env.send(&[withdraw(&vault.owner.pubkey())], &[&vault.owner])
        .expect("withdraw");
    assert_eq!(env.token_balance(&destination), AMOUNT);
    assert_eq!(env.token_balance(&vault.input), 0);
}

// ---------------------------------------------------------------------- route

/// A → B → C in one `route` through the vault. The intermediate B passes through the
/// vault's own B account; each hop pays 0.25%; the vault reads the `RouteResult`.
#[test]
fn a_two_hop_route_through_the_vault() {
    let mut env = Env::new();
    let a = env.create_mint(&TOKEN, 6, None, None);
    let b = env.create_mint(&TOKEN, 6, None, None);
    let c = env.create_mint(&TOKEN, 6, None, None);
    let ab = token_market(&mut env, a, b);
    let bc = token_market(&mut env, b, c);
    let first_out = cp_out(RESERVE, RESERVE, AMOUNT, POOL_FEE_BPS);
    // The second hop spends what the first delivered, less its own input-side fee.
    let second_in = fees::amount_for_budget(first_out, 0);
    let expected_out = cp_out(RESERVE, RESERVE, second_in, POOL_FEE_BPS);

    let vault = create_vault(
        &mut env,
        a,
        TOKEN,
        c,
        TOKEN,
        AMOUNT,
        expected_out,
        0,
        10 * AMOUNT,
    );
    let middle = env.create_ata(&vault.address, &b, &TOKEN);
    let hops = [
        client::RouteHopSpec {
            accounts: cp_swap_accounts(&ab, &vault.address, &a, &vault.input, &b, &middle),
            fee_side: client::FeeSide::Input,
            extra: vec![],
        },
        client::RouteHopSpec {
            accounts: cp_swap_accounts(&bc, &vault.address, &b, &middle, &c, &vault.output),
            fee_side: client::FeeSide::Input,
            extra: vec![],
        },
    ];
    let route = client::route(&vault.address, None, &hops, AMOUNT, expected_out, 0);
    let meta = env
        .send(&[crank_route(&vault, &route, &[(1, 0), (1, 0)])], &[])
        .expect("crank route");
    report(
        "vault route, 2 hops",
        &meta.logs,
        meta.compute_units_consumed,
    );

    let events = swap_events(&meta);
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|e| e.trader == vault.address));
    assert_eq!(events[0].net_sent, AMOUNT);
    assert_eq!(events[0].pipeline_fee, fees::pipeline_fee(AMOUNT));
    assert_eq!(events[1].net_sent, second_in);
    assert_eq!(events[1].pipeline_fee, fees::pipeline_fee(second_in));
    assert_eq!(
        env.token_balance(&vault.input),
        10 * AMOUNT - AMOUNT - fees::pipeline_fee(AMOUNT)
    );
    assert_eq!(env.token_balance(&vault.output), expected_out);
    assert_eq!(
        env.token_balance(&middle),
        first_out - second_in - fees::pipeline_fee(second_in)
    );

    let route_log = meta
        .logs
        .iter()
        .find_map(|line| line.strip_prefix("Program log: dca-vault: route sent "))
        .expect("the vault logs the route result");
    assert_eq!(route_log, format!("{AMOUNT}, kept {expected_out}"));
    let state = vault_state(&env, &vault.address);
    assert_eq!(state.total_sent, AMOUNT);
    assert_eq!(state.total_received, expected_out);
    assert_eq!(logged_height(&meta.logs), 1);
    assert_eq!(max_height(&meta.logs), 4);
}

// ----------------------------------------------------------------- call depth

fn validation(mint: &Pubkey, hook_program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"extra-account-metas", mint.as_ref()], hook_program).0
}

/// A Token-2022 mint whose test hook approves every transfer.
fn hooked_mint(env: &mut Env) -> Pubkey {
    if env.svm.get_account(&HOOK_PROGRAM_ID).is_none() {
        env.svm
            .add_program_from_file(HOOK_PROGRAM_ID, sbf_dir().join("test_hook.so"))
            .unwrap();
    }
    let mint = env.create_mint(&TOKEN_2022, 6, None, Some(HOOK_PROGRAM_ID));
    let whitelist =
        Pubkey::find_program_address(&[b"whitelist", mint.as_ref()], &HOOK_PROGRAM_ID).0;
    let ix = Instruction::new_with_bytes(
        HOOK_PROGRAM_ID,
        b"tsthkini",
        vec![
            AccountMeta::new(env.admin.pubkey(), true),
            AccountMeta::new(validation(&mint, &HOOK_PROGRAM_ID), false),
            AccountMeta::new(whitelist, false),
            AccountMeta::new_readonly(mint, false),
            AccountMeta::new_readonly(client::SYSTEM_PROGRAM_ID, false),
        ],
    );
    env.send(&[ix], &[]).expect("hook initialize");
    mint
}

fn hook_accounts(mint: &Pubkey) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new_readonly(HOOK_PROGRAM_ID, false),
        AccountMeta::new_readonly(validation(mint, &HOOK_PROGRAM_ID), false),
    ]
}

/// Input hook: vault (1) → router (2) → Token-2022 (3) → hook (4).
#[test]
fn a_hooked_input_mint_runs_its_hook_at_height_four() {
    let mut env = Env::new();
    let hooked = hooked_mint(&mut env);
    let plain = env.create_mint(&TOKEN, 6, None, None);
    let market = setup_cp_market_with_mints(
        &mut env,
        hooked,
        TOKEN_2022,
        plain,
        TOKEN,
        RESERVE,
        RESERVE,
        POOL_FEE_BPS,
        hook_accounts(&hooked),
    );
    let out = cp_out(RESERVE, RESERVE, AMOUNT, POOL_FEE_BPS);
    let vault = create_vault(
        &mut env,
        hooked,
        TOKEN_2022,
        plain,
        TOKEN,
        AMOUNT,
        out,
        0,
        10 * AMOUNT,
    );
    let mut accounts = cp_swap_accounts(
        &market,
        &vault.address,
        &hooked,
        &vault.input,
        &plain,
        &vault.output,
    );
    accounts.input_hook_accounts = hook_accounts(&hooked);
    let swap = client::swap(&accounts, &exact_in(AMOUNT, out));
    let meta = env
        .send(&[compute(400_000), crank_swap(&vault, &swap)], &[])
        .expect("crank with a hooked input");
    report(
        "vault swap, hooked input",
        &meta.logs,
        meta.compute_units_consumed,
    );

    let heights = deepest(&meta.logs);
    assert!(
        heights.contains(&("test hook".to_string(), 4)),
        "{heights:?}"
    );
    assert_eq!(max_height(&meta.logs), 4);
    assert_eq!(env.token_balance(&vault.output), out);
    assert_eq!(
        env.token_balance(&pda::fee_account(&market.market, &hooked, &TOKEN_2022)),
        fees::pipeline_fee(AMOUNT)
    );
}

/// Output hook: vault (1) → router (2) → strategy (3) → Token-2022 (4) → hook (5), the
/// limit.
#[test]
fn a_hooked_output_mint_runs_its_hook_at_height_five() {
    let mut env = Env::new();
    let plain = env.create_mint(&TOKEN, 6, None, None);
    let hooked = hooked_mint(&mut env);
    let market = setup_cp_market_with_mints(
        &mut env,
        plain,
        TOKEN,
        hooked,
        TOKEN_2022,
        RESERVE,
        RESERVE,
        POOL_FEE_BPS,
        hook_accounts(&hooked),
    );
    let out = cp_out(RESERVE, RESERVE, AMOUNT, POOL_FEE_BPS);
    let vault = create_vault(
        &mut env,
        plain,
        TOKEN,
        hooked,
        TOKEN_2022,
        AMOUNT,
        out,
        0,
        10 * AMOUNT,
    );
    let mut accounts = cp_swap_accounts(
        &market,
        &vault.address,
        &plain,
        &vault.input,
        &hooked,
        &vault.output,
    );
    accounts.strategy_accounts.extend(hook_accounts(&hooked));
    let swap = client::swap(&accounts, &exact_in(AMOUNT, out));
    let meta = env
        .send(&[compute(400_000), crank_swap(&vault, &swap)], &[])
        .expect("crank with a hooked output");
    report(
        "vault swap, hooked output",
        &meta.logs,
        meta.compute_units_consumed,
    );

    let heights = deepest(&meta.logs);
    assert!(
        heights.contains(&("test hook".to_string(), 5)),
        "{heights:?}"
    );
    assert_eq!(max_height(&meta.logs), 5);
    assert_eq!(env.token_balance(&vault.output), out);
}

/// An output hook that makes a call of its own (the refill hook moves tokens with SPL
/// Token whenever its stash holds any) needs height 6 under the vault. A wallet's swap
/// runs it at 5 and succeeds; the vault's fails with the runtime's call-depth error, and
/// nothing moves.
#[test]
fn an_output_hook_that_calls_out_exceeds_the_limit_under_a_program() {
    let mut env = Env::new();
    env.svm
        .add_program_from_file(REFILL_HOOK_ID, sbf_dir().join("test_hook_refill.so"))
        .unwrap();
    let plain = env.create_mint(&TOKEN, 6, None, None);
    let hooked = env.create_mint(&TOKEN_2022, 6, None, Some(REFILL_HOOK_ID));

    // Hook extra accounts: stash (w), stash authority, refill target (w), SPL Token.
    let stash_authority = Pubkey::find_program_address(&[b"stash"], &REFILL_HOOK_ID).0;
    let stash = env.create_ata(&stash_authority, &plain, &TOKEN);
    let target = env.create_ata(&Pubkey::new_unique(), &plain, &TOKEN);
    let mut init_data = b"refilini".to_vec();
    for (key, writable) in [(stash, 1u8), (stash_authority, 0), (target, 1), (TOKEN, 0)] {
        init_data.extend_from_slice(key.as_ref());
        init_data.push(writable);
    }
    let init = Instruction::new_with_bytes(
        REFILL_HOOK_ID,
        &init_data,
        vec![
            AccountMeta::new(env.admin.pubkey(), true),
            AccountMeta::new(validation(&hooked, &REFILL_HOOK_ID), false),
            AccountMeta::new_readonly(hooked, false),
            AccountMeta::new_readonly(client::SYSTEM_PROGRAM_ID, false),
        ],
    );
    env.send(&[init], &[]).expect("hook init");
    let refill_accounts = vec![
        AccountMeta::new_readonly(REFILL_HOOK_ID, false),
        AccountMeta::new_readonly(validation(&hooked, &REFILL_HOOK_ID), false),
        AccountMeta::new(stash, false),
        AccountMeta::new_readonly(stash_authority, false),
        AccountMeta::new(target, false),
        AccountMeta::new_readonly(TOKEN, false),
    ];
    // The stash is empty while liquidity goes in, so the hook makes no call then.
    let market = setup_cp_market_with_mints(
        &mut env,
        plain,
        TOKEN,
        hooked,
        TOKEN_2022,
        RESERVE,
        RESERVE,
        POOL_FEE_BPS,
        refill_accounts.clone(),
    );
    env.mint_to(&plain, &TOKEN, &stash, 1_000);

    let vault = create_vault(
        &mut env,
        plain,
        TOKEN,
        hooked,
        TOKEN_2022,
        AMOUNT,
        1,
        0,
        10 * AMOUNT,
    );
    let mut accounts = cp_swap_accounts(
        &market,
        &vault.address,
        &plain,
        &vault.input,
        &hooked,
        &vault.output,
    );
    accounts.strategy_accounts.extend(refill_accounts.clone());
    let swap = client::swap(&accounts, &exact_in(AMOUNT, 1));
    let result = env.send(&[compute(400_000), crank_swap(&vault, &swap)], &[]);
    let failed = result.as_ref().expect_err("the vault's crank must fail");
    report(
        "vault swap, output hook calling out",
        &failed.meta.logs,
        failed.meta.compute_units_consumed,
    );
    println!("    error: {:?}", failed.err);
    // The last invocation the runtime started was at the limit; the hook's own call
    // would have been the sixth level.
    assert_eq!(max_height(&failed.meta.logs), 5);
    assert_fails_with(result, "CallDepth");
    assert_eq!(env.token_balance(&vault.input), 10 * AMOUNT);
    assert_eq!(env.token_balance(&stash), 1_000);

    // The same swap from a wallet runs the hook's call at height 5.
    let (trader, trader_in) = env.trader_with(&plain, &TOKEN, 10 * AMOUNT);
    let trader_out = env.create_ata(&trader.pubkey(), &hooked, &TOKEN_2022);
    let mut accounts = cp_swap_accounts(
        &market,
        &trader.pubkey(),
        &plain,
        &trader_in,
        &hooked,
        &trader_out,
    );
    accounts.strategy_accounts.extend(refill_accounts);
    let meta = env
        .send(
            &[
                compute(400_000),
                client::swap(&accounts, &exact_in(AMOUNT, 1)),
            ],
            &[&trader],
        )
        .expect("wallet swap");
    report(
        "wallet swap, output hook calling out",
        &meta.logs,
        meta.compute_units_consumed,
    );
    assert_eq!(max_height(&meta.logs), 5);
    assert_eq!(env.token_balance(&stash), 0);
    assert_eq!(env.token_balance(&target), 1_000);
}
