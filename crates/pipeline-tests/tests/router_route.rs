//! The `route` instruction: up to four exact-in hops in one instruction.
//!
//! Each hop is checked, measured and charged like a single swap and spends what the
//! previous hop left the trader; intermediate tokens pass through the trader's own
//! accounts, so balances the trader already held there are untouched (up to rounding dust
//! when a later hop's fee is on its input). One minimum applies to the final output, and
//! an integrator fee is charged once, on the first hop's input.

#![allow(clippy::result_large_err)]

mod common;
mod router_batch;

use borsh::BorshDeserialize;
use common::*;
use pipeline_client::{self as client, pda, AccountMeta, FeeSide, Instruction, Pubkey};
use pipeline_strategy_interface::fees;
use router_batch::*;
use solana_keypair::Keypair;
use solana_signer::Signer;

const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");
const TAKE_MORE: u8 = 1;
const REFUND_MORE: u8 = 3;
/// What the trader already holds in the intermediate token before routing.
const HELD_B: u64 = 777;

/// Mints A, B, C, D (SPL) and wrapped SOL; markets A/B, B/SOL, C/SOL and C/D; a trader
/// holding A, some B already, empty C and D accounts and an empty wrapped-SOL account.
struct World {
    env: Env,
    a: Pubkey,
    b: Pubkey,
    c: Pubkey,
    d: Pubkey,
    ab: CpMarket,
    b_sol: CpMarket,
    c_sol: CpMarket,
    cd: CpMarket,
    trader: Keypair,
    ta: Pubkey,
    tb: Pubkey,
    tc: Pubkey,
    td: Pubkey,
    tsol: Pubkey,
}

fn world() -> World {
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
    let tb = env.create_ata(&trader.pubkey(), &b, &TOKEN);
    env.mint_to(&b, &TOKEN, &tb, HELD_B);
    let tc = env.create_ata(&trader.pubkey(), &c, &TOKEN);
    let td = env.create_ata(&trader.pubkey(), &d, &TOKEN);
    let tsol = wsol_account(&mut env, &trader.pubkey(), 0);
    World {
        env,
        a,
        b,
        c,
        d,
        ab,
        b_sol,
        c_sol,
        cd,
        trader,
        ta,
        tb,
        tc,
        td,
        tsol,
    }
}

impl World {
    /// A hop through a CP market, with the fee on the side the market's record says.
    fn hop(
        &self,
        market: &CpMarket,
        input: Pubkey,
        trader_input: Pubkey,
        output: Pubkey,
        trader_output: Pubkey,
    ) -> client::RouteHopSpec {
        let accounts = cp_swap_accounts(
            market,
            &self.trader.pubkey(),
            &input,
            &trader_input,
            &output,
            &trader_output,
        );
        let record = self.env.market(&market.market);
        client::RouteHopSpec {
            accounts,
            fee_side: FeeSide::of(&record, &output),
            extra: vec![],
        }
    }

    fn a_to_b(&self) -> client::RouteHopSpec {
        self.hop(&self.ab, self.a, self.ta, self.b, self.tb)
    }

    fn b_to_sol(&self) -> client::RouteHopSpec {
        self.hop(&self.b_sol, self.b, self.tb, NATIVE, self.tsol)
    }

    fn sol_to_c(&self) -> client::RouteHopSpec {
        self.hop(&self.c_sol, NATIVE, self.tsol, self.c, self.tc)
    }

    fn c_to_d(&self) -> client::RouteHopSpec {
        self.hop(&self.cd, self.c, self.tc, self.d, self.td)
    }

    fn route(&self, hops: &[client::RouteHopSpec], amount_in: u64, min_out: u64) -> Instruction {
        client::route(&self.trader.pubkey(), None, hops, amount_in, min_out, 0)
    }

    fn send(&mut self, ix: Instruction) -> TxResult {
        let trader = self.trader.insecure_clone();
        self.env.send(&[ix], &[&trader])
    }

    /// Sends through a v0 transaction whose lookup table holds every account but the
    /// trader, for routes too large for a legacy transaction.
    fn send_with_table(&mut self, ix: Instruction) -> TxResult {
        let mut addresses: Vec<Pubkey> = vec![];
        for meta in &ix.accounts {
            if meta.pubkey != self.trader.pubkey() && !addresses.contains(&meta.pubkey) {
                addresses.push(meta.pubkey);
            }
        }
        let table = set_lookup_table(&mut self.env, &addresses, FROZEN);
        let trader = self.trader.insecure_clone();
        send_v0(&mut self.env, &[ix], &[&trader], &[(table, addresses)])
    }

    fn balances(&self) -> [u64; 6] {
        [self.ta, self.tb, self.tc, self.td, self.tsol]
            .map(|account| self.env.token_balance(&account))
            .into_iter()
            .chain([self.env.lamports(&self.trader.pubkey())])
            .collect::<Vec<_>>()
            .try_into()
            .unwrap()
    }
}

fn route_result(meta: &litesvm::types::TransactionMetadata) -> client::RouteResult {
    assert_eq!(meta.return_data.program_id, client::PROGRAM_ID);
    client::RouteResult::try_from_slice(&meta.return_data.data).expect("RouteResult")
}

/// A → B → SOL: two events in order, hop 2 spends exactly what hop 1 delivered (its fee is
/// on its SOL output), the trader's existing B is untouched, and the result reports what
/// the trader kept.
#[test]
fn two_hops_chain_through_the_traders_accounts() {
    let mut w = world();
    let amount = 1_000_000_000;
    let a_before = w.env.token_balance(&w.ta);
    let ix = w.route(&[w.a_to_b(), w.b_to_sol()], amount, 1);
    let meta = w.send(ix).expect("route");

    let events = swap_events(&meta);
    assert_eq!(events.len(), 2);
    let (first, second) = (&events[0], &events[1]);
    assert_eq!(
        (first.market, first.input_mint, first.output_mint),
        (w.ab.market, w.a, w.b)
    );
    assert_eq!(
        (second.market, second.input_mint, second.output_mint),
        (w.b_sol.market, w.b, NATIVE)
    );
    assert_eq!(first.fee_mint, w.a);
    assert_eq!(second.fee_mint, NATIVE);
    assert_eq!(first.net_sent, amount);
    // Hop 1's fee was on its input, so the trader kept all of `received`; hop 2's fee is
    // on its output, so it spends all of it.
    assert_eq!(second.net_sent, first.received);
    assert_eq!(w.env.token_balance(&w.tb), HELD_B);
    assert_eq!(
        a_before - w.env.token_balance(&w.ta),
        amount + first.pipeline_fee
    );

    let kept = second.received - second.pipeline_fee;
    assert_eq!(w.env.token_balance(&w.tsol), kept);
    let result = route_result(&meta);
    assert_eq!(result.net_sent, first.net_sent);
    assert_eq!(result.amount_out, kept);
    assert_eq!(result.integrator_fee, 0);
}

/// SOL → B → A: hop 2's fee is on its input, so it spends the most that, with its fee,
/// fits in what hop 1 delivered. The trader's existing B is untouched apart from at most
/// a unit or two of rounding dust.
#[test]
fn a_later_hop_with_its_fee_on_the_input_stays_within_what_arrived() {
    let mut w = world();
    let trader = w.trader.pubkey();
    w.env.mint_to(&NATIVE, &TOKEN, &w.tsol, 10 * SOL);
    let sol_to_b = w.hop(&w.b_sol, NATIVE, w.tsol, w.b, w.tb);
    let b_to_a = w.hop(&w.ab, w.b, w.tb, w.a, w.ta);
    assert_eq!(sol_to_b.fee_side, FeeSide::Input);
    assert_eq!(b_to_a.fee_side, FeeSide::Input);
    let ix = client::route(&trader, None, &[sol_to_b, b_to_a], SOL, 1, 0);
    let meta = w.send(ix).expect("route");

    let events = swap_events(&meta);
    let (first, second) = (&events[0], &events[1]);
    let spendable = fees::amount_for_budget(first.received, 0);
    assert_eq!(second.net_sent, spendable);
    assert!(spendable + second.pipeline_fee <= first.received);
    let b_after = w.env.token_balance(&w.tb);
    assert!(b_after >= HELD_B, "the trader's own B was spent");
    let dust = b_after - HELD_B;
    assert!(dust <= 2, "{dust} units of B left behind");
    assert_eq!(dust, first.received - spendable - second.pipeline_fee);
}

/// The integrator fee is charged once, on hop 1's input, and shows only in hop 1's event.
#[test]
fn the_integrator_fee_is_charged_once_on_the_first_input() {
    let mut w = world();
    let integrator = w.env.create_ata(&Pubkey::new_unique(), &w.a, &TOKEN);
    let amount = 1_000_000_000;
    let a_before = w.env.token_balance(&w.ta);
    let ix = client::route(
        &w.trader.pubkey(),
        Some(integrator),
        &[w.a_to_b(), w.b_to_sol()],
        amount,
        1,
        100,
    );
    let meta = w.send(ix).expect("route with an integrator fee");
    let events = swap_events(&meta);
    let integrator_fee = ((amount as u128 * 100).div_ceil(10_000)) as u64;
    assert_eq!(events[0].integrator, Some(integrator));
    assert_eq!(events[0].integrator_fee, integrator_fee);
    assert_eq!(events[1].integrator, None);
    assert_eq!(events[1].integrator_fee, 0);
    assert_eq!(w.env.token_balance(&integrator), integrator_fee);
    assert_eq!(
        a_before - w.env.token_balance(&w.ta),
        amount + events[0].pipeline_fee + integrator_fee
    );
    assert_eq!(route_result(&meta).integrator_fee, integrator_fee);
    assert_eq!(w.env.token_balance(&w.tb), HELD_B);
}

/// Four hops (A → B → SOL → C → D) in one v0 transaction with a lookup table.
#[test]
fn four_hops_work() {
    let mut w = world();
    let hops = [w.a_to_b(), w.b_to_sol(), w.sol_to_c(), w.c_to_d()];
    let ix = w.route(&hops, 1_000_000_000, 1);
    let meta = w.send_with_table(ix).expect("four-hop route");
    let events = swap_events(&meta);
    assert_eq!(events.len(), 4);
    for pair in events.windows(2) {
        assert_eq!(pair[0].output_mint, pair[1].input_mint);
    }
    assert_eq!(w.env.token_balance(&w.tb), HELD_B);
    let last = &events[3];
    let kept = last.received
        - if last.fee_mint == w.d {
            last.pipeline_fee
        } else {
            0
        };
    assert_eq!(w.env.token_balance(&w.td), kept);
    assert_eq!(route_result(&meta).amount_out, kept);
}

#[test]
fn zero_or_five_hops_are_refused() {
    let mut w = world();
    let ix = w.route(&[], 1_000, 0);
    assert_fails_with(w.send(ix), "BadRoute");
    // Five copies of the same hop keep the transaction small; the hop count is checked
    // before anything moves.
    let hops = vec![w.a_to_b(); 5];
    let ix = w.route(&hops, 1_000, 0);
    let before = w.balances();
    assert_fails_with(w.send_with_table(ix), "BadRoute");
    assert_eq!(w.balances(), before);
}

#[test]
fn hops_that_do_not_chain_are_refused() {
    let mut w = world();
    let before = w.balances();

    // Hop 2 spends from an account other than hop 1's output (someone else's B).
    let stranger_b = w.env.create_ata(&Pubkey::new_unique(), &w.b, &TOKEN);
    w.env.mint_to(&w.b, &TOKEN, &stranger_b, 1_000_000);
    let mut second = w.b_to_sol();
    second.accounts.trader_input = stranger_b;
    let ix = w.route(&[w.a_to_b(), second], 1_000_000, 0);
    assert_fails_with(w.send(ix), "BadRoute");

    // Hop 2's input mint isn't hop 1's output mint (C into SOL after A into B).
    let mut second = w.hop(&w.c_sol, w.c, w.tb, NATIVE, w.tsol);
    second.accounts.trader_input = w.tb;
    let ix = w.route(&[w.a_to_b(), second], 1_000_000, 0);
    assert_fails_with(w.send(ix), "BadRoute");

    // Extra accounts after the last hop.
    let mut ix = w.route(&[w.a_to_b()], 1_000_000, 0);
    ix.accounts
        .push(AccountMeta::new_readonly(Pubkey::new_unique(), false));
    assert_fails_with(w.send(ix), "BadRoute");

    // Fewer accounts than the last hop declares.
    let mut ix = w.route(&[w.a_to_b()], 1_000_000, 0);
    ix.accounts.pop();
    assert_fails_with(w.send(ix), "BadRoute");

    assert_eq!(w.balances(), before);
}

/// Route account positions: trader, integrator, event authority, program, then each hop's
/// fixed accounts (strategy program, market, market state, trader input, trader output,
/// input vault, input mint, output mint, input and output token programs, fee authority,
/// fee account).
const HOP0: usize = 4;
const MARKET: usize = HOP0 + 1;
const FEE_AUTHORITY: usize = HOP0 + 10;

#[test]
fn a_hop_with_the_wrong_market_or_fee_authority_is_refused() {
    let mut w = world();
    let before = w.balances();

    // Another real market, not the PDA of this hop's strategy and state.
    let mut ix = w.route(&[w.a_to_b()], 1_000_000, 0);
    ix.accounts[MARKET].pubkey = w.cd.market;
    assert_fails_with(w.send(ix), "BadRoute");

    // A router account that isn't a market.
    let mut ix = w.route(&[w.a_to_b()], 1_000_000, 0);
    ix.accounts[MARKET].pubkey = pda::config();
    assert_fails_with(w.send(ix), "AccountDiscriminatorMismatch");

    // An account the router doesn't own.
    let mut ix = w.route(&[w.a_to_b()], 1_000_000, 0);
    ix.accounts[MARKET].pubkey = w.ta;
    assert_fails_with(w.send(ix), "BadRoute");

    // Another market's fee authority.
    let mut ix = w.route(&[w.a_to_b()], 1_000_000, 0);
    ix.accounts[FEE_AUTHORITY].pubkey = pda::fee_authority(&w.cd.market);
    assert_fails_with(w.send(ix), "BadRoute");

    assert_eq!(w.balances(), before);
}

#[test]
fn the_minimum_applies_to_what_the_last_hop_leaves() {
    let mut w = world();
    let hops = [w.a_to_b(), w.b_to_sol()];
    let snapshot = w.env.svm.clone();
    let ix = w.route(&hops, 1_000_000_000, 0);
    let kept = route_result(&w.send(ix).expect("dry run")).amount_out;
    w.env.svm = snapshot;

    let before = w.balances();
    let ix = w.route(&hops, 1_000_000_000, kept + 1);
    assert_fails_with(w.send(ix), "InsufficientOutput");
    assert_eq!(w.balances(), before);
    let ix = w.route(&hops, 1_000_000_000, kept);
    w.send(ix).expect("a minimum of exactly what is kept");
}

/// A hostile strategy as hop 2 gets no more power than in a single swap: it can't pull
/// from the trader's intermediate account (where the trader already holds B), and it
/// can't refund more than it kept. Either way the whole route fails and nothing moves.
#[test]
fn a_hostile_hop_cannot_take_more_than_a_single_swap_could() {
    let mut w = world();
    w.env.register(&EVIL_ID, "test_strategy_evil");
    let e = w.env.create_mint(&TOKEN, 6, None, None);
    let state = Pubkey::find_program_address(&[b"evil", w.b.as_ref(), e.as_ref()], &EVIL_ID).0;
    let vault_b = w.env.create_ata(&state, &w.b, &TOKEN);
    let vault_e = w.env.create_ata(&state, &e, &TOKEN);
    let creator = w.env.funded_keypair();
    let create = client::create_market(&client::CreateMarket {
        creator: creator.pubkey(),
        strategy_program: EVIL_ID,
        market_state: state,
        market_state_signs: false,
        init_data: vec![0],
        max_creator_lamports: MAX_SETUP_LAMPORTS,
        strategy_accounts: vec![
            AccountMeta::new_readonly(w.b, false),
            AccountMeta::new_readonly(e, false),
            AccountMeta::new(vault_b, false),
            AccountMeta::new(vault_e, false),
        ],
    });
    w.env.send(&[create], &[&creator]).expect("evil market");
    let evil_market = pda::market(&EVIL_ID, &state);
    let admin = w.env.admin.pubkey();
    w.env
        .send(
            &[client::init_fee_account(&admin, &evil_market, &w.b, &TOKEN)],
            &[],
        )
        .unwrap();
    w.env.mint_to(&w.b, &TOKEN, &vault_b, 1_000_000_000);
    w.env.mint_to(&e, &TOKEN, &vault_e, 1_000_000_000);
    let te = w.env.create_ata(&w.trader.pubkey(), &e, &TOKEN);

    let (trader, tb, b) = (w.trader.pubkey(), w.tb, w.b);
    let evil_hop = |behaviour: u8| client::RouteHopSpec {
        accounts: client::SwapAccounts {
            trader,
            strategy_program: EVIL_ID,
            market_state: state,
            trader_input: tb,
            trader_output: te,
            input_vault: vault_b,
            input_mint: b,
            output_mint: e,
            input_token_program: TOKEN,
            output_token_program: TOKEN,
            integrator_fee_account: None,
            strategy_accounts: vec![AccountMeta::new(vault_e, false)],
            input_hook_accounts: vec![],
        },
        fee_side: FeeSide::Input,
        extra: vec![behaviour],
    };

    let before = w.balances();
    let ix = w.route(&[w.a_to_b(), evil_hop(TAKE_MORE)], 1_000_000, 0);
    assert_fails_with(w.send(ix), "privilege escalated");
    assert_eq!(w.balances(), before);
    assert_eq!(w.env.token_balance(&te), 0);

    let ix = w.route(&[w.a_to_b(), evil_hop(REFUND_MORE)], 1_000_000, 0);
    assert_fails_with(w.send(ix), "OutputWithoutInput");
    assert_eq!(w.balances(), before);

    // Honest (1:1), it routes, and the trader's existing B is untouched.
    let ix = w.route(&[w.a_to_b(), evil_hop(0)], 1_000_000, 1);
    let meta = w.send(ix).expect("honest evil hop");
    let events = swap_events(&meta);
    assert_eq!(
        w.env.token_balance(&w.tb),
        HELD_B + events[0].received - events[1].net_sent - events[1].pipeline_fee
    );
    assert!(w.env.token_balance(&w.tb) - HELD_B <= 2);
    assert_eq!(w.env.token_balance(&te), events[1].received);
}
