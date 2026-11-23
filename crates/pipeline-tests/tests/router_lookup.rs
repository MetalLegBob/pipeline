//! Each market can have one address lookup table, recorded in the router.
//!
//! `record_lookup_table` accepts a table only if it is owned by the lookup table program,
//! frozen (no authority), active, and holds every account the router needs for the
//! market; the first valid table recorded for a market stays.

#![allow(clippy::result_large_err)]

mod common;
mod router_batch;

use common::*;
use pipeline_client::{self as client, pda, Pubkey};
use pipeline_events::PipelineEvent;
use router_batch::*;
use solana_account::Account;
use solana_signer::Signer;

struct Setup {
    env: Env,
    market: CpMarket,
    required: Vec<Pubkey>,
    mints: Vec<Pubkey>,
}

fn setup() -> Setup {
    let mut env = Env::new();
    let token = env.create_mint(&TOKEN, 6, None, None);
    let market = sol_market(&mut env, token);
    let record = env.market(&market.market);
    let required = client::lookup_table_addresses(&market.market, &record, &[TOKEN, TOKEN]);
    Setup {
        env,
        mints: vec![token, NATIVE],
        market,
        required,
    }
}

impl Setup {
    fn record(&mut self, table: &Pubkey, mints: &[Pubkey]) -> TxResult {
        let payer = self.env.funded_keypair();
        let ix = client::record_lookup_table(&payer.pubkey(), &self.market.market, table, mints);
        self.env.send(&[ix], &[&payer])
    }
}

#[test]
fn a_frozen_table_with_the_markets_accounts_is_recorded() {
    let mut s = setup();
    // The strategy's own static accounts may come along too.
    let mut addresses = s.required.clone();
    addresses.push(CP_PROGRAM_ID);
    let table = set_lookup_table(&mut s.env, &addresses, FROZEN);
    let mints = s.mints.clone();
    let meta = s.record(&table, &mints).expect("record");

    let record = client::decode_market_lookup_table(
        &s.env
            .account_data(&pda::lookup_table_record(&s.market.market)),
    )
    .expect("record account");
    assert_eq!(record.market, s.market.market);
    assert_eq!(record.table, table);
    let recorded: Vec<_> = events(&meta)
        .into_iter()
        .filter_map(|e| match e {
            PipelineEvent::LookupTableRecorded(e) => Some(e),
            _ => None,
        })
        .collect();
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        (recorded[0].market, recorded[0].table),
        (s.market.market, table)
    );

    // The first table stays: a second record, even of a valid table, fails.
    let another = set_lookup_table(&mut s.env, &s.required.clone(), FROZEN);
    assert_fails_with(s.record(&another, &mints), "already in use");
}

#[test]
fn an_unfrozen_or_deactivated_table_is_refused() {
    let mut s = setup();
    let mints = s.mints.clone();
    let required = s.required.clone();
    for state in [
        TableState {
            frozen: false,
            deactivated: false,
        },
        TableState {
            frozen: true,
            deactivated: true,
        },
    ] {
        let table = set_lookup_table(&mut s.env, &required, state);
        assert_fails_with(s.record(&table, &mints), "BadLookupTable");
    }
}

#[test]
fn a_table_missing_any_required_account_is_refused() {
    let mut s = setup();
    let mints = s.mints.clone();
    let required = s.required.clone();
    for i in 0..required.len() {
        let mut addresses = required.clone();
        addresses.remove(i);
        let table = set_lookup_table(&mut s.env, &addresses, FROZEN);
        assert_fails_with(s.record(&table, &mints), "BadLookupTable");
    }
    // With all of them it is accepted.
    let table = set_lookup_table(&mut s.env, &required, FROZEN);
    s.record(&table, &mints).expect("complete table");
}

#[test]
fn a_table_not_owned_by_the_lookup_table_program_is_refused() {
    let mut s = setup();
    let mints = s.mints.clone();
    let required = s.required.clone();
    let table = set_lookup_table(&mut s.env, &required, FROZEN);
    let account = s.env.svm.get_account(&table).unwrap();
    for owner in [client::SYSTEM_PROGRAM_ID, client::PROGRAM_ID] {
        let fake = Pubkey::new_unique();
        s.env
            .svm
            .set_account(
                fake,
                Account {
                    owner,
                    ..account.clone()
                },
            )
            .unwrap();
        assert_fails_with(s.record(&fake, &mints), "BadLookupTable");
    }
}

#[test]
fn the_mints_must_be_the_markets_in_order() {
    let mut s = setup();
    let required = s.required.clone();
    let table = set_lookup_table(&mut s.env, &required, FROZEN);
    let (token, native) = (s.mints[0], s.mints[1]);
    // Reversed, missing one, an extra one, and a non-mint in a mint's place.
    for mints in [
        vec![native, token],
        vec![token],
        vec![token, native, token],
        vec![token, s.market.market],
    ] {
        assert_fails_with(s.record(&table, &mints), "BadLookupTable");
    }
    s.record(&table, &[token, native])
        .expect("the market's mints, in order");
}

/// A swap sent as a v0 transaction that loads the market's accounts from its recorded
/// table works like a legacy one.
#[test]
fn a_v0_swap_through_the_recorded_table_works() {
    let mut s = setup();
    let required = s.required.clone();
    let table = set_lookup_table(&mut s.env, &required, FROZEN);
    let mints = s.mints.clone();
    s.record(&table, &mints).expect("record");

    let (trader, trader_token) = s.env.trader_with(&s.market.mint_a, &TOKEN, 10_000_000_000);
    let trader_sol = wsol_account(&mut s.env, &trader.pubkey(), 0);
    let accounts = cp_swap_accounts(
        &s.market,
        &trader.pubkey(),
        &s.market.mint_a,
        &trader_token,
        &NATIVE,
        &trader_sol,
    );
    let ix = client::swap(&accounts, &exact_in(1_000_000_000, 1));
    let meta = send_v0(&mut s.env, &[ix], &[&trader], &[(table, required)])
        .expect("v0 swap with the recorded table");
    let event = swap_events(&meta).pop().unwrap();
    assert_eq!(event.fee_mint, NATIVE);
    assert_eq!(
        s.env.token_balance(&trader_sol),
        event.received - event.pipeline_fee
    );
}

/// `create_market_lookup_table` builds a table through the real lookup table program:
/// created, filled, frozen and recorded, in transactions that each fit. A table with more
/// addresses than fit alongside the freeze and record is filled in a transaction of its
/// own first.
#[test]
fn create_market_lookup_table_creates_freezes_and_records_a_table() {
    for extra in [0usize, 6] {
        let mut s = setup();
        // The lookup table program takes the table's slot from the SlotHashes sysvar.
        s.env.svm.warp_to_slot(1_000);
        s.env
            .svm
            .set_sysvar(&solana_slot_hashes::SlotHashes::new(&[(
                999,
                Default::default(),
            )]));
        let payer = s.env.funded_keypair();
        // The strategy's static accounts, and padding to take the table past the
        // one-transaction size.
        let mut addresses = s.required.clone();
        addresses.push(CP_PROGRAM_ID);
        addresses.extend((0..extra).map(|_| Pubkey::new_unique()));
        let (table, transactions) = client::create_market_lookup_table(
            &payer.pubkey(),
            &s.market.market,
            &s.mints,
            &addresses,
            999,
        );
        let one_transaction = addresses.len() <= client::LOOKUP_TABLE_ONE_TRANSACTION_ADDRESSES;
        assert_eq!(transactions.len(), if one_transaction { 1 } else { 2 });
        for instructions in transactions {
            // `send` refuses transactions over the legacy size limit.
            s.env
                .send(&instructions, &[&payer])
                .expect("lookup table transaction");
        }

        let data = s.env.account_data(&table);
        assert_eq!(
            data[56..]
                .chunks_exact(32)
                .map(|k| Pubkey::new_from_array(k.try_into().unwrap()))
                .collect::<Vec<_>>(),
            addresses
        );
        assert_eq!(data[21], 0, "frozen: no authority");
        let record = client::decode_market_lookup_table(
            &s.env
                .account_data(&pda::lookup_table_record(&s.market.market)),
        )
        .expect("record account");
        assert_eq!(record.table, table);
    }
}
