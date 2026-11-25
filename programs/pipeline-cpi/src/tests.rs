//! The instructions built here must match the off-chain builders in `pipeline-client`
//! byte for byte: same accounts, flags, order and data.

use super::*;
use crate::market::{decode_market, MARKET_DISCRIMINATOR, TOKEN_PROGRAM_ID};
use pipeline_client as client;

/// Converts between `pipeline-client`'s address type and this crate's.
fn ours(key: &client::Pubkey) -> Pubkey {
    Pubkey::new_from_array(key.to_bytes())
}

fn theirs(key: &Pubkey) -> client::Pubkey {
    client::Pubkey::new_from_array(key.to_bytes())
}

/// Backing storage for an `AccountInfo`.
struct Stored {
    key: Pubkey,
    lamports: u64,
    data: Vec<u8>,
    owner: Pubkey,
    writable: bool,
}

/// Storage for the accounts of an off-chain instruction, as a calling program would
/// receive them: same writable flags, no signatures (the trader signs through seeds).
fn stored(metas: &[client::AccountMeta]) -> Vec<Stored> {
    metas
        .iter()
        .map(|meta| Stored {
            key: ours(&meta.pubkey),
            lamports: 0,
            data: vec![],
            owner: Pubkey::default(),
            writable: meta.is_writable,
        })
        .collect()
}

fn infos(storage: &mut [Stored]) -> Vec<AccountInfo<'_>> {
    storage
        .iter_mut()
        .map(|s| {
            AccountInfo::new(
                &s.key,
                false,
                s.writable,
                &mut s.lamports,
                &mut s.data,
                &s.owner,
                false,
            )
        })
        .collect()
}

fn assert_same(built: &Instruction, expected: &client::Instruction) {
    assert_eq!(built.program_id, ours(&expected.program_id));
    assert_eq!(built.data, expected.data);
    let built: Vec<_> = built
        .accounts
        .iter()
        .map(|m| (theirs(&m.pubkey), m.is_signer, m.is_writable))
        .collect();
    let expected: Vec<_> = expected
        .accounts
        .iter()
        .map(|m| (m.pubkey, m.is_signer, m.is_writable))
        .collect();
    assert_eq!(built, expected);
}

fn key() -> client::Pubkey {
    client::Pubkey::new_unique()
}

fn client_swap_accounts(integrator: Option<client::Pubkey>) -> client::SwapAccounts {
    client::SwapAccounts {
        trader: key(),
        strategy_program: key(),
        market_state: key(),
        trader_input: key(),
        trader_output: key(),
        input_vault: key(),
        input_mint: key(),
        output_mint: key(),
        input_token_program: client::TOKEN_2022_PROGRAM_ID,
        output_token_program: client::TOKEN_PROGRAM_ID,
        integrator_fee_account: integrator,
        strategy_accounts: vec![
            client::AccountMeta::new(key(), false),
            client::AccountMeta::new_readonly(key(), false),
            client::AccountMeta::new_readonly(key(), false),
        ],
        input_hook_accounts: vec![
            client::AccountMeta::new_readonly(key(), false),
            client::AccountMeta::new(key(), false),
        ],
    }
}

#[test]
fn discriminators_match_anchor() {
    assert_eq!(
        SWAP_DISCRIMINATOR,
        client::instruction_discriminator("swap")
    );
    assert_eq!(
        ROUTE_DISCRIMINATOR,
        client::instruction_discriminator("route")
    );
    assert_eq!(
        MARKET_DISCRIMINATOR,
        client::account_discriminator("Market")
    );
    assert_eq!(ROUTER_ID, ours(&client::PROGRAM_ID));
    assert_eq!(NATIVE_MINT, ours(&client::wallet::NATIVE_MINT));
}

#[test]
fn a_swap_matches_the_client_builder() {
    for (side, integrator, args) in [
        (
            client::FeeSide::Input,
            None,
            client_args(TradeMode::ExactIn, 0, vec![]),
        ),
        (
            client::FeeSide::Output,
            Some(key()),
            client_args(TradeMode::ExactOut, 75, vec![1, 2, 3]),
        ),
    ] {
        let accounts = client_swap_accounts(integrator);
        let expected = client::swap_with_fee_side(&accounts, &args, side);
        let mut storage = stored(&expected.accounts);
        let infos = infos(&mut storage);
        let hooks = accounts.input_hook_accounts.len() as u8;
        let swap = SwapAccounts::from_router_order(&infos, hooks).unwrap();
        assert_eq!(swap.strategy_accounts.len(), 3);
        assert_eq!(swap.input_hook_accounts.len(), 2);
        assert_eq!(swap.integrator_fee_account.is_some(), integrator.is_some());

        let built = swap
            .instruction(&SwapArgs {
                mode: args.mode,
                amount_in: args.amount_in,
                amount_out: args.amount_out,
                min_amount_out: args.min_amount_out,
                integrator_fee_bps: args.integrator_fee_bps,
                extra: args.extra.clone(),
            })
            .unwrap();
        assert_same(&built, &expected);
        // Every account the instruction names is in the infos handed to the runtime.
        let passed: Vec<Pubkey> = swap.account_infos().iter().map(|a| *a.key).collect();
        assert!(built.accounts.iter().all(|m| passed.contains(&m.pubkey)));
    }
}

fn client_args(mode: TradeMode, integrator_fee_bps: u16, extra: Vec<u8>) -> client::SwapArgs {
    client::SwapArgs {
        mode,
        amount_in: 1_234_567,
        amount_out: 89,
        min_amount_out: 1_000,
        integrator_fee_bps,
        input_hook_accounts: 0,
        extra,
    }
}

#[test]
fn a_route_matches_the_client_builder() {
    let first = client_swap_accounts(None);
    let mut second = client_swap_accounts(None);
    second.trader = first.trader;
    second.trader_input = first.trader_output;
    second.input_mint = first.output_mint;
    second.input_hook_accounts = vec![];
    let hops = [
        client::RouteHopSpec {
            accounts: first,
            fee_side: client::FeeSide::Output,
            extra: vec![9],
        },
        client::RouteHopSpec {
            accounts: second,
            fee_side: client::FeeSide::Input,
            extra: vec![],
        },
    ];
    let integrator = key();
    let expected = client::route(
        &hops[0].accounts.trader,
        Some(integrator),
        &hops,
        5_000,
        4_000,
        30,
    );
    let mut storage = stored(&expected.accounts);
    let infos = infos(&mut storage);
    let shapes: Vec<HopShape> = hops
        .iter()
        .map(|hop| HopShape {
            strategy_accounts: hop.accounts.strategy_accounts.len() as u8,
            input_hook_accounts: hop.accounts.input_hook_accounts.len() as u8,
        })
        .collect();
    let mut route = RouteAccounts::from_router_order(&infos, &shapes).unwrap();
    route.hops[0].extra = &[9];
    let built = route
        .instruction(&RouteArgs {
            amount_in: 5_000,
            min_amount_out: 4_000,
            integrator_fee_bps: 30,
        })
        .unwrap();
    assert_same(&built, &expected);

    // Accounts must be used exactly.
    let short = HopShape {
        strategy_accounts: shapes[1].strategy_accounts - 1,
        ..shapes[1]
    };
    assert_eq!(
        RouteAccounts::from_router_order(&infos, &[shapes[0], short]).err(),
        Some(CpiError::AccountCountMismatch.into())
    );
    assert_eq!(
        RouteAccounts::from_router_order(&infos, &[]).err(),
        Some(CpiError::BadRoute.into())
    );
}

#[test]
fn only_the_router_is_invoked() {
    let accounts = client_swap_accounts(None);
    let expected = client::swap(&accounts, &client_args(TradeMode::ExactIn, 0, vec![]));
    let mut storage = stored(&expected.accounts);
    storage[15].key = Pubkey::new_unique();
    let infos = infos(&mut storage);
    let swap = SwapAccounts::from_router_order(&infos, 2).unwrap();
    assert_eq!(
        swap.instruction(&SwapArgs::exact_in(1, 1)).err(),
        Some(CpiError::NotTheRouter.into())
    );
    assert_eq!(
        SwapAccounts::from_router_order(&infos[..15], 0).err(),
        Some(CpiError::NotEnoughAccounts.into())
    );
    assert_eq!(
        SwapAccounts::from_router_order(&infos[..16], 1).err(),
        Some(CpiError::AccountCountMismatch.into())
    );
}

#[test]
fn results_decode_like_the_router_encodes_them() {
    let fee_mint = key();
    let result = client::SwapResult {
        net_sent: 1_000,
        received: 2_000,
        pipeline_fee: 5,
        integrator_fee: 7,
        fee_mint,
    };
    let bytes = borsh_bytes(&result);
    let decoded = SwapResult::decode(&bytes).unwrap();
    assert_eq!(decoded.net_sent, 1_000);
    assert_eq!(decoded.received, 2_000);
    assert_eq!(decoded.pipeline_fee, 5);
    assert_eq!(decoded.integrator_fee, 7);
    assert_eq!(decoded.fee_mint, ours(&fee_mint));
    // The fee came out of the output when the output is the fee mint.
    assert_eq!(decoded.kept(&ours(&fee_mint)), 1_995);
    assert_eq!(decoded.kept(&Pubkey::new_unique()), 2_000);
    assert_eq!(decoded.spent(&ours(&fee_mint)), 1_012);
    assert_eq!(decoded.spent(&Pubkey::new_unique()), 1_007);

    let route = client::RouteResult {
        net_sent: 10,
        amount_out: 20,
        integrator_fee: 0,
        input_pipeline_fee: 0,
    };
    let bytes = borsh_bytes(&route);
    // Transaction metadata trims trailing zeros; the short form decodes the same.
    let trimmed = &bytes[..16];
    for data in [&bytes[..], trimmed] {
        assert_eq!(
            RouteResult::decode(data).unwrap(),
            RouteResult {
                net_sent: 10,
                amount_out: 20,
                integrator_fee: 0,
                input_pipeline_fee: 0,
            }
        );
    }
    assert!(RouteResult::decode(&[0; RouteResult::LEN + 1]).is_err());
}

fn borsh_bytes<T: borsh::BorshSerialize>(value: &T) -> Vec<u8> {
    borsh::to_vec(value).unwrap()
}

#[test]
fn a_market_record_reads_like_the_client_decodes_it() {
    let mints = [key(), client::wallet::NATIVE_MINT, key(), key()];
    let vaults = [key(), key(), key(), key()];
    for (count, fee_mint_index, fee_mint) in
        [(2u8, 1u8, Some(mints[1])), (3, client::NO_FEE_MINT, None)]
    {
        let market = client::Market {
            strategy_program: key(),
            market_state: key(),
            mint_count: count,
            mints,
            vaults,
            supports_exact_out: true,
            created_by: key(),
            created_at: 1_700_000_000,
            bump: 254,
            fee_authority_bump: 253,
            fee_mint_index,
        };
        let mut data = client::account_discriminator("Market").to_vec();
        data.extend(borsh_bytes(&market));
        assert_eq!(data.len(), market::MARKET_LEN);
        let view = decode_market(&data).unwrap();
        assert_eq!(view.strategy_program, ours(&market.strategy_program));
        assert_eq!(view.market_state, ours(&market.market_state));
        assert_eq!(view.mints.len(), count as usize);
        assert_eq!(view.vault_for(&ours(&mints[0])), Some(ours(&vaults[0])));
        assert!(view.supports_exact_out);
        assert_eq!(view.fee_mint, fee_mint.map(|m| ours(&m)));
        let side = if fee_mint.is_some() {
            FeeSide::Output
        } else {
            FeeSide::Input
        };
        assert_eq!(view.fee_side(&ours(&mints[1])), side);
        assert_eq!(view.fee_side(&ours(&mints[0])), FeeSide::Input);
    }
    assert!(decode_market(&[0; market::MARKET_LEN]).is_none());
}

#[test]
fn token_accounts_are_checked_for_owner_mint_and_kind() {
    let mint = Pubkey::new_unique();
    let owner = Pubkey::new_unique();
    let mut data = vec![0u8; 165];
    data[..32].copy_from_slice(mint.as_ref());
    data[32..64].copy_from_slice(owner.as_ref());
    data[108] = 1;
    let check = |data: &[u8], token_program: &Pubkey, owner: &Pubkey| {
        let key = Pubkey::new_unique();
        let mut lamports = 0;
        let mut data = data.to_vec();
        let info = AccountInfo::new(
            &key,
            false,
            true,
            &mut lamports,
            &mut data,
            token_program,
            false,
        );
        check_token_account(&info, owner, &mint)
    };
    assert!(check(&data, &TOKEN_PROGRAM_ID, &owner).is_ok());
    let wrong = Err(CpiError::WrongTokenAccount.into());
    assert_eq!(
        check(&data, &TOKEN_PROGRAM_ID, &Pubkey::new_unique()),
        wrong
    );
    assert_eq!(check(&data, &Pubkey::new_unique(), &owner), wrong);
    // An uninitialised account, and a Token-2022 mint padded to an account's length.
    let mut uninitialised = data.clone();
    uninitialised[108] = 0;
    assert_eq!(check(&uninitialised, &TOKEN_PROGRAM_ID, &owner), wrong);
    let mut extended = data.clone();
    extended.push(1);
    assert_eq!(
        check(&extended, &market::TOKEN_2022_PROGRAM_ID, &owner),
        wrong
    );
    extended[165] = 2;
    assert!(check(&extended, &market::TOKEN_2022_PROGRAM_ID, &owner).is_ok());
}

/// The Anchor accounts struct and context put the same accounts in the same order.
#[cfg(feature = "anchor")]
#[test]
fn the_anchor_accounts_match_the_client_builder() {
    use anchor_lang::{ToAccountInfos, ToAccountMetas};

    let accounts = client_swap_accounts(Some(key()));
    let expected = client::swap(&accounts, &client_args(TradeMode::ExactIn, 0, vec![]));
    let mut storage = stored(&expected.accounts);
    let infos = infos(&mut storage);
    let swap = SwapAccounts::from_router_order(&infos, 2).unwrap();
    let anchor_swap = crate::anchor::Swap {
        trader: swap.trader.clone(),
        strategy_program: swap.strategy_program.clone(),
        market: swap.market.clone(),
        market_state: swap.market_state.clone(),
        trader_input: swap.trader_input.clone(),
        trader_output: swap.trader_output.clone(),
        input_vault: swap.input_vault.clone(),
        input_mint: swap.input_mint.clone(),
        output_mint: swap.output_mint.clone(),
        input_token_program: swap.input_token_program.clone(),
        output_token_program: swap.output_token_program.clone(),
        fee_authority: swap.fee_authority.clone(),
        fee_account: swap.fee_account.clone(),
        integrator_fee_account: swap.integrator_fee_account.cloned(),
        event_authority: swap.event_authority.clone(),
        router: swap.router.clone(),
    };
    let metas = anchor_swap.to_account_metas(None);
    let built = swap.instruction(&SwapArgs::exact_in(0, 0)).unwrap();
    assert_eq!(metas, built.accounts[..SWAP_FIXED_ACCOUNTS]);
    let keys: Vec<Pubkey> = anchor_swap
        .to_account_infos()
        .iter()
        .map(|a| *a.key)
        .collect();
    let plain: Vec<Pubkey> = swap.account_infos()[..SWAP_FIXED_ACCOUNTS]
        .iter()
        .map(|a| *a.key)
        .collect();
    assert_eq!(keys, plain);
}
