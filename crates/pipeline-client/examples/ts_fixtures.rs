//! Prints cross-language test vectors for the TypeScript SDK (`sdk/ts`) as JSON.
//!
//! ```text
//! cargo run -p pipeline-client --example ts_fixtures > sdk/ts/test/fixtures.json
//! ```
//!
//! Every value comes from the Rust crates, so the TypeScript tests can assert that the
//! SDK produces byte-identical instructions, addresses, encodings and fee results.
//!
//! JSON conventions, matching the SDK's TypeScript types:
//! - addresses are base58 strings;
//! - `u64` and `i64` are decimal strings (JSON numbers lose precision above 2^53);
//! - `u8`, `u16` and booleans are JSON numbers and booleans;
//! - byte vectors are lowercase hex strings;
//! - `Option` is the value or `null`;
//! - field names are camelCase.

use borsh::BorshSerialize;
use pipeline_client::{
    self as client, interface as psi, pda, AccountMeta, Instruction, Pubkey, TradeMode,
};
use pipeline_events as events;
use serde_json::{json, Value};

fn key(n: u8) -> Pubkey {
    Pubkey::new_from_array([n; 32])
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn addr(key: &Pubkey) -> Value {
    json!(key.to_string())
}

fn opt_addr(key: &Option<Pubkey>) -> Value {
    key.as_ref().map(addr).unwrap_or(Value::Null)
}

fn big(value: u64) -> Value {
    json!(value.to_string())
}

fn signed(value: i64) -> Value {
    json!(value.to_string())
}

fn opt_big(value: Option<u64>) -> Value {
    value.map(big).unwrap_or(Value::Null)
}

fn mode(mode: TradeMode) -> Value {
    match mode {
        TradeMode::ExactIn => json!(0),
        TradeMode::ExactOut => json!(1),
    }
}

fn meta(meta: &AccountMeta) -> Value {
    json!({
        "address": meta.pubkey.to_string(),
        "isSigner": meta.is_signer,
        "isWritable": meta.is_writable,
    })
}

fn metas(metas: &[AccountMeta]) -> Value {
    Value::Array(metas.iter().map(meta).collect())
}

fn instruction(name: &str, inputs: Value, ix: &Instruction) -> Value {
    json!({
        "name": name,
        "inputs": inputs,
        "programAddress": ix.program_id.to_string(),
        "accounts": metas(&ix.accounts),
        "data": hex(&ix.data),
    })
}

fn encoding(type_name: &str, value: Value, bytes: &[u8]) -> Value {
    json!({ "type": type_name, "value": value, "hex": hex(bytes) })
}

// ---------------------------------------------------------------------------------------
// JSON views of the wire types
// ---------------------------------------------------------------------------------------

fn trade_json(trade: &psi::TradeRequest) -> Value {
    json!({
        "mode": mode(trade.mode),
        "inputIndex": trade.input_index,
        "outputIndex": trade.output_index,
        "amount": big(trade.amount),
        "trader": opt_addr(&trade.trader.map(Pubkey::new_from_array)),
        "extra": hex(&trade.extra),
    })
}

fn account_ref_json(account: &psi::AccountRef) -> Value {
    json!({
        "address": addr(&Pubkey::new_from_array(account.address)),
        "writable": account.writable,
    })
}

fn swap_args_json(args: &client::SwapArgs) -> Value {
    json!({
        "mode": mode(args.mode),
        "amountIn": big(args.amount_in),
        "amountOut": big(args.amount_out),
        "minAmountOut": big(args.min_amount_out),
        "integratorFeeBps": args.integrator_fee_bps,
        "inputHookAccounts": args.input_hook_accounts,
        "extra": hex(&args.extra),
    })
}

/// A swap's accounts as the SDK's `SwapAccounts` takes them.
fn swap_accounts_json(accounts: &client::SwapAccounts) -> Value {
    json!({
        "trader": addr(&accounts.trader),
        "strategyProgram": addr(&accounts.strategy_program),
        "marketState": addr(&accounts.market_state),
        "traderInput": addr(&accounts.trader_input),
        "traderOutput": addr(&accounts.trader_output),
        "inputVault": addr(&accounts.input_vault),
        "inputMint": addr(&accounts.input_mint),
        "outputMint": addr(&accounts.output_mint),
        "inputTokenProgram": addr(&accounts.input_token_program),
        "outputTokenProgram": addr(&accounts.output_token_program),
        "integratorFeeAccount": opt_addr(&accounts.integrator_fee_account),
        "strategyAccounts": metas(&accounts.strategy_accounts),
        "inputHookAccounts": metas(&accounts.input_hook_accounts),
    })
}

fn fee_side_json(side: client::FeeSide) -> Value {
    match side {
        client::FeeSide::Input => json!("input"),
        client::FeeSide::Output => json!("output"),
    }
}

/// The smallest amount arriving in the trader's output that leaves at least `kept` after
/// Pipeline's fee is taken from it, computed from the definition: `None` above a u64.
fn received_for_kept(kept: u64) -> Option<u64> {
    let fee = |r: u128| (r * psi::PIPELINE_FEE_BPS as u128).div_ceil(psi::BPS_DENOMINATOR as u128);
    let kept = kept as u128;
    let mut r = (kept * psi::BPS_DENOMINATOR as u128)
        .div_ceil((psi::BPS_DENOMINATOR - psi::PIPELINE_FEE_BPS) as u128);
    while r - fee(r) < kept {
        r += 1;
    }
    while r > kept && r - 1 - fee(r - 1) >= kept {
        r -= 1;
    }
    u64::try_from(r).ok()
}

fn swap_event_json(event: &events::SwapEvent) -> Value {
    json!({
        "market": addr(&event.market),
        "strategy": addr(&event.strategy),
        "trader": addr(&event.trader),
        "inputMint": addr(&event.input_mint),
        "outputMint": addr(&event.output_mint),
        "mode": event.mode,
        "netSent": big(event.net_sent),
        "received": big(event.received),
        "pipelineFee": big(event.pipeline_fee),
        "integrator": opt_addr(&event.integrator),
        "integratorFee": big(event.integrator_fee),
        "slot": big(event.slot),
        "unixTimestamp": signed(event.unix_timestamp),
        "feeMint": addr(&event.fee_mint),
    })
}

// ---------------------------------------------------------------------------------------
// Sections
// ---------------------------------------------------------------------------------------

fn program_ids() -> Value {
    json!({
        "router": addr(&client::PROGRAM_ID),
        "system": addr(&client::SYSTEM_PROGRAM_ID),
        "token": addr(&client::TOKEN_PROGRAM_ID),
        "token2022": addr(&client::TOKEN_2022_PROGRAM_ID),
        "associatedToken": addr(&client::ASSOCIATED_TOKEN_PROGRAM_ID),
        "bpfLoaderUpgradeable": addr(&client::BPF_LOADER_UPGRADEABLE_ID),
        "routerBytes": hex(&psi::PIPELINE_PROGRAM_ID),
    })
}

fn discriminators() -> Value {
    let router_instructions = [
        "initialize_config",
        "update_config",
        "register_strategy",
        "update_metadata",
        "create_market",
        "register_existing_market",
        "init_fee_account",
        "swap",
        "sweep_fees",
        "migrate_market",
        "route",
        "record_lookup_table",
    ];
    let instructions: serde_json::Map<String, Value> = router_instructions
        .iter()
        .map(|name| {
            (
                name.to_string(),
                json!(hex(&client::instruction_discriminator(name))),
            )
        })
        .collect();
    let accounts: serde_json::Map<String, Value> =
        ["Config", "Strategy", "Market", "MarketLookupTable"]
            .iter()
            .map(|name| {
                (
                    name.to_string(),
                    json!(hex(&client::account_discriminator(name))),
                )
            })
            .collect();
    json!({
        "routerInstructions": instructions,
        "accounts": accounts,
        "strategy": {
            "pipeline_init_market": hex(&psi::discriminator::INIT_MARKET),
            "pipeline_market_info": hex(&psi::discriminator::MARKET_INFO),
            "pipeline_resolve_accounts": hex(&psi::discriminator::RESOLVE_ACCOUNTS),
            "pipeline_quote": hex(&psi::discriminator::QUOTE),
            "pipeline_swap": hex(&psi::discriminator::SWAP),
        },
        "events": {
            "SwapEvent": hex(&events::discriminator::SWAP),
            "StrategyRegistered": hex(&events::discriminator::STRATEGY_REGISTERED),
            "MetadataUpdated": hex(&events::discriminator::METADATA_UPDATED),
            "MarketCreated": hex(&events::discriminator::MARKET_CREATED),
            "FeesSwept": hex(&events::discriminator::FEES_SWEPT),
            "ConfigChanged": hex(&events::discriminator::CONFIG_CHANGED),
            "LookupTableRecorded": hex(&events::discriminator::LOOKUP_TABLE_RECORDED),
        },
        "eventIxTag": hex(&events::EVENT_IX_TAG),
    })
}

fn pdas() -> Value {
    let strategy_program = key(7);
    let market_state = key(8);
    let market = pda::market(&strategy_program, &market_state);
    let mint = key(13);
    let owner = key(30);
    json!([
        { "kind": "config", "inputs": {}, "address": addr(&pda::config()) },
        { "kind": "eventAuthority", "inputs": {}, "address": addr(&pda::event_authority()) },
        {
            "kind": "strategy",
            "inputs": { "strategyProgram": addr(&strategy_program) },
            "address": addr(&pda::strategy(&strategy_program)),
        },
        {
            "kind": "market",
            "inputs": { "strategyProgram": addr(&strategy_program), "marketState": addr(&market_state) },
            "address": addr(&market),
        },
        {
            "kind": "feeAuthority",
            "inputs": { "market": addr(&market) },
            "address": addr(&pda::fee_authority(&market)),
        },
        {
            "kind": "setupPayer",
            "inputs": { "market": addr(&market) },
            "address": addr(&pda::setup_payer(&market)),
        },
        {
            "kind": "feeAccount",
            "inputs": { "market": addr(&market), "mint": addr(&mint), "tokenProgram": addr(&client::TOKEN_PROGRAM_ID) },
            "address": addr(&pda::fee_account(&market, &mint, &client::TOKEN_PROGRAM_ID)),
        },
        {
            "kind": "feeAccount",
            "inputs": { "market": addr(&market), "mint": addr(&mint), "tokenProgram": addr(&client::TOKEN_2022_PROGRAM_ID) },
            "address": addr(&pda::fee_account(&market, &mint, &client::TOKEN_2022_PROGRAM_ID)),
        },
        {
            "kind": "lookupTableRecord",
            "inputs": { "market": addr(&market) },
            "address": addr(&pda::lookup_table_record(&market)),
        },
        {
            "kind": "register",
            "inputs": { "strategyProgram": addr(&strategy_program) },
            "address": addr(&pda::register(&strategy_program)),
        },
        {
            "kind": "programData",
            "inputs": { "program": addr(&client::PROGRAM_ID) },
            "address": addr(&pda::program_data(&client::PROGRAM_ID)),
        },
        {
            "kind": "programData",
            "inputs": { "program": addr(&strategy_program) },
            "address": addr(&pda::program_data(&strategy_program)),
        },
        {
            "kind": "associatedToken",
            "inputs": { "owner": addr(&owner), "mint": addr(&mint), "tokenProgram": addr(&client::TOKEN_PROGRAM_ID) },
            "address": addr(&pda::associated_token(&owner, &mint, &client::TOKEN_PROGRAM_ID)),
        },
        {
            "kind": "associatedToken",
            "inputs": { "owner": addr(&owner), "mint": addr(&mint), "tokenProgram": addr(&client::TOKEN_2022_PROGRAM_ID) },
            "address": addr(&pda::associated_token(&owner, &mint, &client::TOKEN_2022_PROGRAM_ID)),
        },
    ])
}

fn router_instructions() -> Value {
    let mut cases = vec![];

    let config_args = client::ConfigArgs {
        listing_fee_lamports: 1_500_000_000,
        listing_fee_recipient: key(2),
        treasury_owner: key(3),
    };
    cases.push(instruction(
        "initializeConfig",
        json!({
            "admin": addr(&key(1)),
            "args": {
                "listingFeeLamports": big(config_args.listing_fee_lamports),
                "listingFeeRecipient": addr(&config_args.listing_fee_recipient),
                "treasuryOwner": addr(&config_args.treasury_owner),
            },
        }),
        &client::initialize_config(&key(1), &config_args),
    ));

    let update = client::UpdateConfigArgs {
        admin: Some(key(4)),
        listing_fee_lamports: None,
        listing_fee_recipient: Some(key(5)),
        treasury_owner: None,
    };
    for args in [update, client::UpdateConfigArgs::default()] {
        cases.push(instruction(
            "updateConfig",
            json!({
                "admin": addr(&key(1)),
                "args": {
                    "admin": opt_addr(&args.admin),
                    "listingFeeLamports": opt_big(args.listing_fee_lamports),
                    "listingFeeRecipient": opt_addr(&args.listing_fee_recipient),
                    "treasuryOwner": opt_addr(&args.treasury_owner),
                },
            }),
            &client::update_config(&key(1), &args),
        ));
    }

    let uri = "https://example.com/strategy.json";
    cases.push(instruction(
        "registerStrategy",
        json!({
            "payer": addr(&key(1)),
            "registrar": addr(&key(6)),
            "strategyProgram": addr(&key(7)),
            "listingFeeRecipient": addr(&key(2)),
            "metadataUri": uri,
            "maxListingFeeLamports": "10000000",
        }),
        &client::register_strategy(&key(1), &key(6), &key(7), &key(2), uri, 10_000_000),
    ));

    cases.push(instruction(
        "updateMetadata",
        json!({
            "registrar": addr(&key(6)),
            "strategyProgram": addr(&key(7)),
            "metadataUri": "ipfs://metadata",
        }),
        &client::update_metadata(&key(6), &key(7), "ipfs://metadata"),
    ));

    let setup_accounts = vec![
        AccountMeta::new(key(9), false),
        AccountMeta::new_readonly(key(10), false),
        AccountMeta::new(key(11), true),
        AccountMeta::new_readonly(key(12), true),
    ];
    let init_data = vec![1, 2, 3, 255];
    cases.push(instruction(
        "createMarket",
        json!({
            "creator": addr(&key(1)),
            "strategyProgram": addr(&key(7)),
            "marketState": addr(&key(8)),
            "marketStateSigns": true,
            "initData": hex(&init_data),
            "maxCreatorLamports": "123456789",
            "strategyAccounts": metas(&setup_accounts),
        }),
        &client::create_market(&client::CreateMarket {
            creator: key(1),
            strategy_program: key(7),
            market_state: key(8),
            market_state_signs: true,
            init_data,
            max_creator_lamports: 123_456_789,
            strategy_accounts: setup_accounts,
        }),
    ));

    let info_accounts = vec![AccountMeta::new_readonly(key(9), false)];
    cases.push(instruction(
        "registerExistingMarket",
        json!({
            "payer": addr(&key(1)),
            "strategyProgram": addr(&key(7)),
            "marketState": addr(&key(8)),
            "infoAccounts": metas(&info_accounts),
        }),
        &client::register_existing_market(&key(1), &key(7), &key(8), info_accounts),
    ));

    let market = pda::market(&key(7), &key(8));
    for token_program in [client::TOKEN_PROGRAM_ID, client::TOKEN_2022_PROGRAM_ID] {
        cases.push(instruction(
            "initFeeAccount",
            json!({
                "payer": addr(&key(1)),
                "market": addr(&market),
                "mint": addr(&key(13)),
                "tokenProgram": addr(&token_program),
            }),
            &client::init_fee_account(&key(1), &market, &key(13), &token_program),
        ));
    }

    let full_swap = (
        client::SwapAccounts {
            trader: key(30),
            strategy_program: key(7),
            market_state: key(8),
            trader_input: key(31),
            trader_output: key(32),
            input_vault: key(16),
            input_mint: key(13),
            output_mint: key(14),
            input_token_program: client::TOKEN_2022_PROGRAM_ID,
            output_token_program: client::TOKEN_PROGRAM_ID,
            integrator_fee_account: Some(key(20)),
            strategy_accounts: vec![
                AccountMeta::new(key(21), false),
                AccountMeta::new_readonly(key(22), false),
            ],
            input_hook_accounts: vec![
                AccountMeta::new_readonly(key(23), false),
                AccountMeta::new(key(24), false),
                AccountMeta::new_readonly(key(25), false),
            ],
        },
        client::SwapArgs {
            mode: TradeMode::ExactOut,
            amount_in: u64::MAX,
            amount_out: 1_234,
            min_amount_out: 1_000,
            integrator_fee_bps: 50,
            // Overwritten by the builder from `input_hook_accounts`.
            input_hook_accounts: 0,
            extra: vec![9, 8, 7],
        },
    );
    let plain_swap = (
        client::SwapAccounts {
            integrator_fee_account: None,
            strategy_accounts: vec![],
            input_hook_accounts: vec![],
            input_token_program: client::TOKEN_PROGRAM_ID,
            ..full_swap.0.clone()
        },
        client::SwapArgs {
            mode: TradeMode::ExactIn,
            amount_in: 1_000_000,
            amount_out: 0,
            min_amount_out: 990_000,
            integrator_fee_bps: 0,
            input_hook_accounts: 7,
            extra: vec![],
        },
    );
    // `feeSide: null` uses the builder's default: the output side when the output is
    // wrapped SOL, the input side otherwise.
    let sol_out = client::SwapAccounts {
        output_mint: client::wallet::NATIVE_MINT,
        ..plain_swap.0.clone()
    };
    let swaps = [
        (full_swap.0.clone(), full_swap.1.clone(), None),
        (plain_swap.0.clone(), plain_swap.1.clone(), None),
        (sol_out.clone(), plain_swap.1.clone(), None),
        (sol_out, plain_swap.1.clone(), Some(client::FeeSide::Input)),
        (
            full_swap.0.clone(),
            full_swap.1.clone(),
            Some(client::FeeSide::Output),
        ),
    ];
    for (accounts, args, side) in swaps {
        let ix = match side {
            None => client::swap(&accounts, &args),
            Some(side) => client::swap_with_fee_side(&accounts, &args, side),
        };
        cases.push(instruction(
            "swap",
            json!({
                "accounts": swap_accounts_json(&accounts),
                "args": swap_args_json(&args),
                "feeSide": side.map(fee_side_json),
            }),
            &ix,
        ));
    }

    // Routes: two plain hops, and four hops with strategy and hook accounts, an
    // integrator and both fee sides.
    let hop = |n: u8, input_mint: Pubkey, output_mint: Pubkey| client::SwapAccounts {
        trader: key(30),
        strategy_program: key(7),
        market_state: key(8 + n),
        trader_input: key(31 + n),
        trader_output: key(32 + n),
        input_vault: key(16 + n),
        input_mint,
        output_mint,
        input_token_program: client::TOKEN_PROGRAM_ID,
        output_token_program: client::TOKEN_PROGRAM_ID,
        integrator_fee_account: None,
        strategy_accounts: vec![],
        input_hook_accounts: vec![],
    };
    let two_hops = vec![
        client::RouteHopSpec {
            accounts: hop(0, key(13), client::wallet::NATIVE_MINT),
            fee_side: client::FeeSide::Output,
            extra: vec![],
        },
        client::RouteHopSpec {
            accounts: hop(1, client::wallet::NATIVE_MINT, key(14)),
            fee_side: client::FeeSide::Input,
            extra: vec![],
        },
    ];
    let four_hops = vec![
        client::RouteHopSpec {
            accounts: client::SwapAccounts {
                input_token_program: client::TOKEN_2022_PROGRAM_ID,
                strategy_accounts: vec![
                    AccountMeta::new(key(21), false),
                    AccountMeta::new_readonly(key(22), false),
                ],
                input_hook_accounts: vec![AccountMeta::new_readonly(key(23), false)],
                ..hop(0, key(13), key(14))
            },
            fee_side: client::FeeSide::Input,
            extra: vec![1, 2, 3],
        },
        client::RouteHopSpec {
            accounts: hop(1, key(14), client::wallet::NATIVE_MINT),
            fee_side: client::FeeSide::Output,
            extra: vec![],
        },
        client::RouteHopSpec {
            accounts: client::SwapAccounts {
                strategy_accounts: vec![AccountMeta::new(key(24), false)],
                ..hop(2, client::wallet::NATIVE_MINT, key(15))
            },
            fee_side: client::FeeSide::Input,
            extra: vec![],
        },
        client::RouteHopSpec {
            accounts: client::SwapAccounts {
                output_token_program: client::TOKEN_2022_PROGRAM_ID,
                input_hook_accounts: vec![
                    AccountMeta::new_readonly(key(25), false),
                    AccountMeta::new(key(26), false),
                ],
                ..hop(3, key(15), key(16))
            },
            fee_side: client::FeeSide::Output,
            extra: vec![255; 40],
        },
    ];
    let routes = [
        (two_hops, None, 1_000_000u64, 990_000u64, 0u16),
        (four_hops, Some(key(20)), u64::MAX, 1, 9_999),
    ];
    for (hops, integrator, amount_in, min_amount_out, bps) in routes {
        let hops_json: Vec<Value> = hops
            .iter()
            .map(|h| {
                json!({
                    "accounts": swap_accounts_json(&h.accounts),
                    "feeSide": fee_side_json(h.fee_side),
                    "extra": hex(&h.extra),
                })
            })
            .collect();
        cases.push(instruction(
            "route",
            json!({
                "trader": addr(&key(30)),
                "integratorFeeAccount": opt_addr(&integrator),
                "amountIn": big(amount_in),
                "minAmountOut": big(min_amount_out),
                "integratorFeeBps": bps,
                "hops": hops_json,
            }),
            &client::route(&key(30), integrator, &hops, amount_in, min_amount_out, bps),
        ));
    }

    for mints in [
        vec![key(13), key(14)],
        vec![key(13), key(14), key(15), key(16)],
    ] {
        cases.push(instruction(
            "recordLookupTable",
            json!({
                "payer": addr(&key(1)),
                "market": addr(&market),
                "table": addr(&key(35)),
                "mints": mints.iter().map(addr).collect::<Vec<_>>(),
            }),
            &client::record_lookup_table(&key(1), &market, &key(35), &mints),
        ));
    }

    cases.push(instruction(
        "migrateMarket",
        json!({ "payer": addr(&key(1)), "market": addr(&market) }),
        &client::migrate_market(&key(1), &market),
    ));

    let sweeps = [
        (
            client::TOKEN_2022_PROGRAM_ID,
            vec![AccountMeta::new_readonly(key(23), false)],
        ),
        (client::TOKEN_PROGRAM_ID, vec![]),
    ];
    for (token_program, hook_accounts) in sweeps {
        cases.push(instruction(
            "sweepFees",
            json!({
                "market": addr(&market),
                "mint": addr(&key(13)),
                "tokenProgram": addr(&token_program),
                "treasuryOwner": addr(&key(3)),
                "hookAccounts": metas(&hook_accounts),
            }),
            &client::sweep_fees(&market, &key(13), &token_program, &key(3), hook_accounts),
        ));
    }

    Value::Array(cases)
}

/// The market used by the quote fixtures, as `pipeline-quoter`'s `MarketView` would hold it.
struct QuoteMarket {
    market: Pubkey,
    strategy_program: Pubkey,
    market_state: Pubkey,
    mints: Vec<Pubkey>,
    vaults: Vec<Pubkey>,
    token_programs: Vec<Pubkey>,
}

/// Mirrors `pipeline-quoter`'s fixed read accounts for `pipeline_quote` and
/// `pipeline_resolve_accounts`: everything read-only, nothing signs, and the trader's
/// token accounts are its associated token accounts.
fn read_accounts(
    market: &QuoteMarket,
    input: usize,
    output: usize,
    trader: &Pubkey,
) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new_readonly(market.market, false),
        AccountMeta::new_readonly(market.market_state, false),
        AccountMeta::new_readonly(*trader, false),
        AccountMeta::new_readonly(
            pda::associated_token(trader, &market.mints[input], &market.token_programs[input]),
            false,
        ),
        AccountMeta::new_readonly(
            pda::associated_token(
                trader,
                &market.mints[output],
                &market.token_programs[output],
            ),
            false,
        ),
        AccountMeta::new_readonly(market.vaults[input], false),
        AccountMeta::new_readonly(market.mints[input], false),
        AccountMeta::new_readonly(market.mints[output], false),
        AccountMeta::new_readonly(market.token_programs[input], false),
        AccountMeta::new_readonly(market.token_programs[output], false),
    ]
}

fn read_only(accounts: &[AccountMeta]) -> Vec<AccountMeta> {
    accounts
        .iter()
        .map(|m| AccountMeta::new_readonly(m.pubkey, false))
        .collect()
}

fn strategy_instructions() -> Value {
    let strategy_program = key(7);
    let market_state = key(8);
    let market = QuoteMarket {
        market: pda::market(&strategy_program, &market_state),
        strategy_program,
        market_state,
        mints: vec![key(13), key(14), key(15)],
        vaults: vec![key(16), key(17), key(18)],
        token_programs: vec![
            client::TOKEN_PROGRAM_ID,
            client::TOKEN_2022_PROGRAM_ID,
            client::TOKEN_PROGRAM_ID,
        ],
    };
    let market_json = json!({
        "market": addr(&market.market),
        "strategyProgram": addr(&market.strategy_program),
        "marketState": addr(&market.market_state),
        "mints": market.mints.iter().map(addr).collect::<Vec<_>>(),
        "vaults": market.vaults.iter().map(addr).collect::<Vec<_>>(),
        "tokenPrograms": market.token_programs.iter().map(addr).collect::<Vec<_>>(),
        "supportsExactOut": true,
    });
    let known = vec![
        AccountMeta::new(key(21), false),
        AccountMeta::new_readonly(key(22), false),
    ];

    let mut cases = vec![];

    let trade = psi::TradeRequest {
        mode: TradeMode::ExactIn,
        input_index: 2,
        output_index: 1,
        amount: 777,
        trader: Some(key(30).to_bytes()),
        extra: vec![1],
    };
    let mut accounts = read_accounts(&market, 2, 1, &key(30));
    accounts.extend(read_only(&known));
    let ix = Instruction::new_with_bytes(
        strategy_program,
        &psi::instruction_data(psi::discriminator::QUOTE, &trade),
        accounts,
    );
    cases.push(instruction(
        "quote",
        json!({ "market": market_json, "trade": trade_json(&trade), "strategyAccounts": metas(&known) }),
        &ix,
    ));

    // An anonymous trade uses the default (all-zero) address as the trader.
    let anonymous = psi::TradeRequest {
        mode: TradeMode::ExactOut,
        input_index: 0,
        output_index: 1,
        amount: 5,
        trader: None,
        extra: vec![],
    };
    for (round, known) in [(0u8, vec![]), (2u8, known.clone())] {
        let mut accounts = read_accounts(&market, 0, 1, &Pubkey::default());
        accounts.extend(read_only(&known));
        let request = psi::ResolveRequest {
            trade: anonymous.clone(),
            round,
        };
        let ix = Instruction::new_with_bytes(
            strategy_program,
            &psi::instruction_data(psi::discriminator::RESOLVE_ACCOUNTS, &request),
            accounts,
        );
        cases.push(instruction(
            "resolveAccounts",
            json!({
                "market": market_json,
                "trade": trade_json(&anonymous),
                "round": round,
                "known": metas(&known),
            }),
            &ix,
        ));
    }

    Value::Array(cases)
}

fn fee_cases() -> Value {
    let max = u64::MAX;
    let ceil_cases: Vec<(u64, u64)> = vec![
        (0, 25),
        (1, 25),
        (1, 0),
        (399, 25),
        (400, 25),
        (401, 25),
        (10_000, 25),
        (10_001, 25),
        (123_456_789, 9_999),
        (max, 25),
        (max, 10_000),
        (max, 10_001),
        (max, max),
    ];
    let ceil_bps: Vec<Value> = ceil_cases
        .iter()
        .map(|&(amount, bps)| {
            json!({
                "amount": big(amount),
                "bps": big(bps),
                "result": opt_big(psi::fees::ceil_bps(amount, bps)),
            })
        })
        .collect();

    let pipeline_fee: Vec<Value> = [0, 1, 399, 400, 401, 10_000, 10_001, max / 2, max]
        .iter()
        .map(|&net_sent| {
            json!({
                "netSent": big(net_sent),
                "result": big(psi::fees::pipeline_fee(net_sent)),
            })
        })
        .collect();

    let bps_values = [0u16, 1, 50, 100, 9_999, u16::MAX];
    let mut max_total_spend = vec![];
    for amount in [0, 1, 400, 401, 1_000_000, max / 2, max - 1, max] {
        for bps in bps_values {
            max_total_spend.push(json!({
                "amountSent": big(amount),
                "integratorFeeBps": bps,
                "result": opt_big(psi::fees::max_total_spend(amount, bps)),
            }));
        }
    }

    let mut amount_for_budget = vec![];
    for budget in [
        0,
        1,
        2,
        402,
        999,
        1_000,
        1_000_000,
        123_456_789,
        max / 2,
        max,
    ] {
        for bps in bps_values {
            amount_for_budget.push(json!({
                "budget": big(budget),
                "integratorFeeBps": bps,
                "result": big(psi::fees::amount_for_budget(budget, bps)),
            }));
        }
    }

    let received_for_kept_cases: Vec<Value> = [
        0,
        1,
        2,
        398,
        399,
        400,
        997,
        998,
        1_000,
        10_000,
        123_456_789,
        max / 2,
        max - max / 400,
        max - max / 400 + 1,
        max,
    ]
    .iter()
    .map(|&kept| {
        json!({
            "kept": big(kept),
            "result": opt_big(received_for_kept(kept)),
        })
    })
    .collect();

    json!({
        "ceilBps": ceil_bps,
        "pipelineFee": pipeline_fee,
        "receivedForKept": received_for_kept_cases,
        "maxTotalSpend": max_total_spend,
        "amountForBudget": amount_for_budget,
    })
}

fn encodings() -> Value {
    let mut cases = vec![];

    let trades = [
        psi::TradeRequest {
            mode: TradeMode::ExactIn,
            input_index: 0,
            output_index: 1,
            amount: 1_000,
            trader: None,
            extra: vec![],
        },
        psi::TradeRequest {
            mode: TradeMode::ExactOut,
            input_index: 3,
            output_index: 2,
            amount: u64::MAX,
            trader: Some(key(30).to_bytes()),
            extra: vec![0, 1, 2, 254, 255],
        },
    ];
    for trade in &trades {
        cases.push(encoding(
            "TradeRequest",
            trade_json(trade),
            &borsh::to_vec(trade).unwrap(),
        ));
    }

    let swap_request = psi::SwapRequest {
        trade: trades[1].clone(),
        amount_sent: 2_000,
        min_amount_out: 1_999,
    };
    cases.push(encoding(
        "SwapRequest",
        json!({
            "trade": trade_json(&swap_request.trade),
            "amountSent": big(swap_request.amount_sent),
            "minAmountOut": big(swap_request.min_amount_out),
        }),
        &borsh::to_vec(&swap_request).unwrap(),
    ));

    let quote = psi::QuoteResult {
        amount_in: 12_345,
        amount_out: u64::MAX - 1,
    };
    cases.push(encoding(
        "QuoteResult",
        json!({ "amountIn": big(quote.amount_in), "amountOut": big(quote.amount_out) }),
        &borsh::to_vec(&quote).unwrap(),
    ));

    let info = psi::MarketInfo {
        mints: vec![key(13).to_bytes(), key(14).to_bytes(), key(15).to_bytes()],
        vaults: vec![key(16).to_bytes(), key(17).to_bytes(), key(18).to_bytes()],
        supports_exact_out: true,
    };
    cases.push(encoding(
        "MarketInfo",
        json!({
            "mints": info.mints.iter().map(|m| addr(&Pubkey::new_from_array(*m))).collect::<Vec<_>>(),
            "vaults": info.vaults.iter().map(|v| addr(&Pubkey::new_from_array(*v))).collect::<Vec<_>>(),
            "supportsExactOut": info.supports_exact_out,
        }),
        &borsh::to_vec(&info).unwrap(),
    ));

    let route_args = client::RouteArgs {
        amount_in: u64::MAX,
        min_amount_out: 7,
        integrator_fee_bps: 50,
        hops: vec![
            client::RouteHop {
                strategy_accounts: 2,
                input_hook_accounts: 1,
                extra: vec![9, 8],
            },
            client::RouteHop {
                strategy_accounts: 0,
                input_hook_accounts: 0,
                extra: vec![],
            },
        ],
    };
    cases.push(encoding(
        "RouteArgs",
        json!({
            "amountIn": big(route_args.amount_in),
            "minAmountOut": big(route_args.min_amount_out),
            "integratorFeeBps": route_args.integrator_fee_bps,
            "hops": route_args.hops.iter().map(|h| json!({
                "strategyAccounts": h.strategy_accounts,
                "inputHookAccounts": h.input_hook_accounts,
                "extra": hex(&h.extra),
            })).collect::<Vec<_>>(),
        }),
        &borsh::to_vec(&route_args).unwrap(),
    ));
    let route_result = client::RouteResult {
        net_sent: 1_000_000,
        amount_out: 0,
        integrator_fee: u64::MAX,
    };
    cases.push(encoding(
        "RouteResult",
        json!({
            "netSent": big(route_result.net_sent),
            "amountOut": big(route_result.amount_out),
            "integratorFee": big(route_result.integrator_fee),
        }),
        &borsh::to_vec(&route_result).unwrap(),
    ));

    let resolve_request = psi::ResolveRequest {
        trade: trades[0].clone(),
        round: 3,
    };
    cases.push(encoding(
        "ResolveRequest",
        json!({ "trade": trade_json(&resolve_request.trade), "round": resolve_request.round }),
        &borsh::to_vec(&resolve_request).unwrap(),
    ));

    let account_ref = psi::AccountRef {
        address: key(21).to_bytes(),
        writable: true,
    };
    cases.push(encoding(
        "AccountRef",
        account_ref_json(&account_ref),
        &borsh::to_vec(&account_ref).unwrap(),
    ));

    let resolve_results = [
        psi::ResolveResult {
            accounts: vec![
                account_ref,
                psi::AccountRef {
                    address: key(22).to_bytes(),
                    writable: false,
                },
            ],
            complete: false,
        },
        psi::ResolveResult {
            accounts: vec![],
            complete: true,
        },
    ];
    for result in &resolve_results {
        cases.push(encoding(
            "ResolveResult",
            json!({
                "accounts": result.accounts.iter().map(account_ref_json).collect::<Vec<_>>(),
                "complete": result.complete,
            }),
            &borsh::to_vec(result).unwrap(),
        ));
    }

    let config_args = client::ConfigArgs {
        listing_fee_lamports: 42,
        listing_fee_recipient: key(2),
        treasury_owner: key(3),
    };
    cases.push(encoding(
        "ConfigArgs",
        json!({
            "listingFeeLamports": big(config_args.listing_fee_lamports),
            "listingFeeRecipient": addr(&config_args.listing_fee_recipient),
            "treasuryOwner": addr(&config_args.treasury_owner),
        }),
        &borsh::to_vec(&config_args).unwrap(),
    ));

    let update_configs = [
        client::UpdateConfigArgs {
            admin: None,
            listing_fee_lamports: Some(u64::MAX),
            listing_fee_recipient: None,
            treasury_owner: Some(key(3)),
        },
        client::UpdateConfigArgs::default(),
    ];
    for args in &update_configs {
        cases.push(encoding(
            "UpdateConfigArgs",
            json!({
                "admin": opt_addr(&args.admin),
                "listingFeeLamports": opt_big(args.listing_fee_lamports),
                "listingFeeRecipient": opt_addr(&args.listing_fee_recipient),
                "treasuryOwner": opt_addr(&args.treasury_owner),
            }),
            &borsh::to_vec(args).unwrap(),
        ));
    }

    let swap_args = client::SwapArgs {
        mode: TradeMode::ExactOut,
        amount_in: 10_000,
        amount_out: 9_000,
        min_amount_out: 8_999,
        integrator_fee_bps: 65_535,
        input_hook_accounts: 2,
        extra: vec![0xde, 0xad],
    };
    cases.push(encoding(
        "SwapArgs",
        swap_args_json(&swap_args),
        &borsh::to_vec(&swap_args).unwrap(),
    ));

    let swap_result = client::SwapResult {
        net_sent: 10_000,
        received: 9_000,
        pipeline_fee: 25,
        integrator_fee: u64::MAX,
        fee_mint: key(14),
    };
    cases.push(encoding(
        "SwapResult",
        json!({
            "netSent": big(swap_result.net_sent),
            "received": big(swap_result.received),
            "pipelineFee": big(swap_result.pipeline_fee),
            "integratorFee": big(swap_result.integrator_fee),
            "feeMint": addr(&swap_result.fee_mint),
        }),
        &borsh::to_vec(&swap_result).unwrap(),
    ));

    Value::Array(cases)
}

/// Router accounts as stored on chain: the 8-byte Anchor discriminator, then Borsh.
fn router_accounts() -> Value {
    fn account_bytes(name: &str, value: &impl BorshSerialize) -> Vec<u8> {
        let mut data = client::account_discriminator(name).to_vec();
        value.serialize(&mut data).unwrap();
        data
    }

    let config = client::Config {
        admin: key(1),
        reserved: 0,
        listing_fee_lamports: 1_500_000_000,
        listing_fee_recipient: key(2),
        treasury_owner: key(3),
        bump: 254,
    };
    let strategy = client::Strategy {
        program_id: key(7),
        metadata_uri: "https://example.com/strategy.json".into(),
        interface_version: psi::INTERFACE_VERSION,
        registered_at: -1_700_000_000,
        market_count: 3,
        bump: 253,
    };
    let market = client::Market {
        strategy_program: key(7),
        market_state: key(8),
        mint_count: 3,
        mints: [key(13), key(14), key(15), Pubkey::default()],
        vaults: [key(16), key(17), key(18), Pubkey::default()],
        supports_exact_out: true,
        created_by: key(1),
        created_at: 1_760_000_000,
        bump: 252,
        fee_authority_bump: 251,
        fee_mint_index: 1,
    };
    let lookup = client::MarketLookupTable {
        market: key(40),
        table: key(35),
        bump: 249,
    };

    json!([
        encoding(
            "Config",
            json!({
                "admin": addr(&config.admin),
                "reserved": config.reserved,
                "listingFeeLamports": big(config.listing_fee_lamports),
                "listingFeeRecipient": addr(&config.listing_fee_recipient),
                "treasuryOwner": addr(&config.treasury_owner),
                "bump": config.bump,
            }),
            &account_bytes("Config", &config),
        ),
        encoding(
            "Strategy",
            json!({
                "programId": addr(&strategy.program_id),
                "metadataUri": strategy.metadata_uri,
                "interfaceVersion": strategy.interface_version,
                "registeredAt": signed(strategy.registered_at),
                "marketCount": big(strategy.market_count),
                "bump": strategy.bump,
            }),
            &account_bytes("Strategy", &strategy),
        ),
        encoding(
            "Market",
            json!({
                "strategyProgram": addr(&market.strategy_program),
                "marketState": addr(&market.market_state),
                "mintCount": market.mint_count,
                "mints": market.mints.iter().map(addr).collect::<Vec<_>>(),
                "vaults": market.vaults.iter().map(addr).collect::<Vec<_>>(),
                "supportsExactOut": market.supports_exact_out,
                "createdBy": addr(&market.created_by),
                "createdAt": signed(market.created_at),
                "bump": market.bump,
                "feeAuthorityBump": market.fee_authority_bump,
                "feeMintIndex": market.fee_mint_index,
            }),
            &account_bytes("Market", &market),
        ),
        encoding(
            "MarketLookupTable",
            json!({
                "market": addr(&lookup.market),
                "table": addr(&lookup.table),
                "bump": lookup.bump,
            }),
            &account_bytes("MarketLookupTable", &lookup),
        ),
    ])
}

/// `pipeline_market_info` return data with and without a declared fee mint.
fn market_infos() -> Value {
    let info = psi::MarketInfo {
        mints: vec![key(13).to_bytes(), client::wallet::NATIVE_MINT.to_bytes()],
        vaults: vec![key(16).to_bytes(), key(17).to_bytes()],
        supports_exact_out: false,
    };
    let info_json = json!({
        "mints": info.mints.iter().map(|m| addr(&Pubkey::new_from_array(*m))).collect::<Vec<_>>(),
        "vaults": info.vaults.iter().map(|v| addr(&Pubkey::new_from_array(*v))).collect::<Vec<_>>(),
        "supportsExactOut": info.supports_exact_out,
    });
    let cases: Vec<Value> = [None, Some(0u8), Some(1)]
        .iter()
        .map(|&fee_mint| {
            let bytes = psi::encode_market_info(&info, fee_mint);
            assert_eq!(
                psi::decode_market_info(&bytes),
                Some((info.clone(), fee_mint))
            );
            json!({ "info": info_json, "feeMint": fee_mint, "hex": hex(&bytes) })
        })
        .collect();
    Value::Array(cases)
}

/// `lookup_table_addresses` for a market account. The second case shares a token program
/// and a fee account's owner between mints, so duplicates are dropped.
fn lookup_tables() -> Value {
    let market = client::Market {
        strategy_program: key(7),
        market_state: key(8),
        mint_count: 3,
        mints: [key(13), key(14), key(15), Pubkey::default()],
        vaults: [key(16), key(17), key(18), Pubkey::default()],
        supports_exact_out: true,
        created_by: key(1),
        created_at: 1_760_000_000,
        bump: 252,
        fee_authority_bump: 251,
        fee_mint_index: client::NO_FEE_MINT,
    };
    let mut account = client::account_discriminator("Market").to_vec();
    market.serialize(&mut account).unwrap();
    let address = pda::market(&market.strategy_program, &market.market_state);
    let cases: Vec<Value> = [
        vec![
            client::TOKEN_PROGRAM_ID,
            client::TOKEN_2022_PROGRAM_ID,
            client::TOKEN_PROGRAM_ID,
        ],
        vec![client::TOKEN_PROGRAM_ID; 3],
        vec![client::TOKEN_2022_PROGRAM_ID],
    ]
    .iter()
    .map(|token_programs| {
        json!({
            "marketAddress": addr(&address),
            "market": hex(&account),
            "tokenPrograms": token_programs.iter().map(addr).collect::<Vec<_>>(),
            "addresses": client::lookup_table_addresses(&address, &market, token_programs)
                .iter()
                .map(addr)
                .collect::<Vec<_>>(),
        })
    })
    .collect();
    Value::Array(cases)
}

/// Router events as they appear in a self-CPI instruction's data.
fn router_events() -> Value {
    fn event_bytes(discriminator: [u8; 8], value: &impl BorshSerialize) -> Vec<u8> {
        let mut data = events::EVENT_IX_TAG.to_vec();
        data.extend_from_slice(&discriminator);
        value.serialize(&mut data).unwrap();
        // The Rust decoder must accept every fixture, so the two languages cannot agree
        // on wrong bytes.
        assert!(events::decode(&data).is_some());
        data
    }

    let mut cases = vec![];

    let swaps = [
        events::SwapEvent {
            market: key(40),
            strategy: key(7),
            trader: key(30),
            input_mint: key(13),
            output_mint: key(14),
            mode: 1,
            net_sent: 1_000_000,
            received: 997_000,
            pipeline_fee: 2_500,
            integrator: Some(key(20)),
            integrator_fee: 5_000,
            slot: 123_456_789,
            unix_timestamp: 1_760_000_000,
            fee_mint: key(13),
        },
        events::SwapEvent {
            market: key(40),
            strategy: key(7),
            trader: key(30),
            input_mint: key(14),
            output_mint: key(13),
            mode: 0,
            net_sent: u64::MAX,
            received: 0,
            pipeline_fee: 46_116_860_184_273_880,
            integrator: None,
            integrator_fee: 0,
            slot: 1,
            unix_timestamp: -5,
            fee_mint: key(13),
        },
    ];
    for event in &swaps {
        cases.push(encoding(
            "Swap",
            swap_event_json(event),
            &event_bytes(events::discriminator::SWAP, event),
        ));
    }

    let registered = events::StrategyRegistered {
        strategy: key(7),
        fee_paid: 1_500_000_000,
        metadata_uri: "https://example.com/strategy.json".into(),
    };
    cases.push(encoding(
        "StrategyRegistered",
        json!({
            "strategy": addr(&registered.strategy),
            "feePaid": big(registered.fee_paid),
            "metadataUri": registered.metadata_uri,
        }),
        &event_bytes(events::discriminator::STRATEGY_REGISTERED, &registered),
    ));

    let updated = events::MetadataUpdated {
        strategy: key(7),
        metadata_uri: "ipfs://metadata \u{2713}".into(),
    };
    cases.push(encoding(
        "MetadataUpdated",
        json!({
            "strategy": addr(&updated.strategy),
            "metadataUri": updated.metadata_uri,
        }),
        &event_bytes(events::discriminator::METADATA_UPDATED, &updated),
    ));

    let created = events::MarketCreated {
        market: key(40),
        strategy: key(7),
        market_state: key(8),
        mints: vec![key(13), key(14)],
        created_by: key(1),
    };
    cases.push(encoding(
        "MarketCreated",
        json!({
            "market": addr(&created.market),
            "strategy": addr(&created.strategy),
            "marketState": addr(&created.market_state),
            "mints": created.mints.iter().map(addr).collect::<Vec<_>>(),
            "createdBy": addr(&created.created_by),
        }),
        &event_bytes(events::discriminator::MARKET_CREATED, &created),
    ));

    let swept = events::FeesSwept {
        market: key(40),
        mint: key(13),
        amount: 77,
    };
    cases.push(encoding(
        "FeesSwept",
        json!({
            "market": addr(&swept.market),
            "mint": addr(&swept.mint),
            "amount": big(swept.amount),
        }),
        &event_bytes(events::discriminator::FEES_SWEPT, &swept),
    ));

    let changed = events::ConfigChanged {
        admin: key(1),
        listing_fee_lamports: 0,
        listing_fee_recipient: key(2),
        treasury_owner: key(3),
    };
    cases.push(encoding(
        "ConfigChanged",
        json!({
            "admin": addr(&changed.admin),
            "listingFeeLamports": big(changed.listing_fee_lamports),
            "listingFeeRecipient": addr(&changed.listing_fee_recipient),
            "treasuryOwner": addr(&changed.treasury_owner),
        }),
        &event_bytes(events::discriminator::CONFIG_CHANGED, &changed),
    ));

    let recorded = events::LookupTableRecorded {
        market: key(40),
        table: key(35),
    };
    cases.push(encoding(
        "LookupTableRecorded",
        json!({ "market": addr(&recorded.market), "table": addr(&recorded.table) }),
        &event_bytes(events::discriminator::LOOKUP_TABLE_RECORDED, &recorded),
    ));

    Value::Array(cases)
}

fn main() {
    let fixtures = json!({
        "keys": (0u8..=40).map(|n| addr(&key(n))).collect::<Vec<_>>(),
        "programIds": program_ids(),
        "constants": {
            "pipelineFeeBps": psi::PIPELINE_FEE_BPS,
            "bpsDenominator": psi::BPS_DENOMINATOR,
            "interfaceVersion": psi::INTERFACE_VERSION,
            "minMarketMints": psi::MIN_MARKET_MINTS,
            "maxMarketMints": psi::MAX_MARKET_MINTS,
            "maxResolveRounds": psi::MAX_RESOLVE_ROUNDS,
            "maxRouteHops": client::MAX_ROUTE_HOPS,
            "noFeeMint": client::NO_FEE_MINT,
            "nativeMint": addr(&client::wallet::NATIVE_MINT),
            "nativeMintBytes": hex(&psi::NATIVE_MINT),
        },
        "discriminators": discriminators(),
        "pdas": pdas(),
        "instructions": router_instructions(),
        "strategyInstructions": strategy_instructions(),
        "fees": fee_cases(),
        "encodings": encodings(),
        "marketInfos": market_infos(),
        "lookupTables": lookup_tables(),
        "accounts": router_accounts(),
        "events": router_events(),
    });
    println!(
        "{}",
        serde_json::to_string_pretty(&fixtures).expect("fixtures serialise")
    );
}
