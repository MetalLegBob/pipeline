//! Stateful fuzzing of the Pipeline router.
//!
//! A fuzz case is a byte tape. The harness decodes it into a sequence of transactions
//! against one world, sends each one, and checks the router's guarantees after every
//! transaction, whether it succeeded or not. The world has the router; a registered
//! scriptable strategy (`test-strategy-script`, which runs a script from the tape during
//! market setup and every swap, so it can behave like any program, honest or not); six
//! markets of two to four mints over SPL Token and Token-2022 mints with a transfer fee,
//! a permanent delegate on a market, a strategy-held mint and freeze authority, an
//! allow-all transfer hook and a hostile hook that refills the trader's input account;
//! traders, a bystander, a treasury and an integrator.
//!
//! Transactions mix every router instruction (with valid, substituted and garbage
//! accounts and data), two swaps in a row, trader transfers, the strategy acting outside
//! the router, and compute-budget prefixes. Between transactions the issuer and the
//! world act too: time passes, the transfer fee changes, the hook's stash and vaults are
//! topped up, the router is frozen.
//!
//! The guarantees checked are written from the router's documented behaviour, not from
//! its code:
//! - nobody who did not sign loses tokens or SOL, or has an account closed, delegated,
//!   reassigned or frozen (the strategy's and hook's own accounts, and issuer powers the
//!   mint grants a market, excepted);
//! - a trader loses at most `amount_in` plus both fees on it from their input account,
//!   and nothing from any other account;
//! - a lone swap's `net_sent` is exactly `min(amount_in, max(what the trader's input
//!   lost during the call, what the input vault kept))`, rebuilt from balances; every
//!   swap pays `ceil(net_sent × 25 / 10,000)` (and the integrator's rate) into the
//!   market's own fee account;
//! - the trader's output account gains `received ≥ min_amount_out`, and the event and
//!   the return data report what really moved;
//! - fee accounts lose tokens only to a sweep, and only into the treasury;
//! - router accounts change only through their instructions: markets never, the config
//!   only by its admin, strategies only by their owner or a new market; every event matches the accounts it describes, and
//!   registrations and metadata updates come from the program's upgrade authority or its
//!   registration PDA;
//! - a market creator loses at most `max_creator_lamports` plus the market's rent;
//! - freezing the router (dropping its upgrade authority) changes none of the above;
//! - the router never panics or faults, whatever it is sent.
//!
//! `router_fuzz` runs 64 cases with shrinking (raise `PROPTEST_CASES` for more).
//! `router_fuzz_campaign` (ignored) is a long, multi-threaded, corpus-guided campaign;
//! see its documentation for the knobs. `FUZZ_TAPE=<hex>` replays one tape with a trace.

mod common;

use std::collections::{BTreeMap, HashMap, HashSet};

use borsh::BorshDeserialize;
use common::*;
use litesvm::LiteSVM;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use pipeline_events::PipelineEvent;
use pipeline_strategy_interface::{self as psi, TradeMode};
use proptest::prelude::*;
use solana_account::Account;
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_transaction::Transaction;

const SCRIPT_ID: Pubkey = Pubkey::new_from_array(*b"pipeline-router-fuzz-strategy!!!");
const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
const TOKEN_2022: Pubkey = client::TOKEN_2022_PROGRAM_ID;
const LOADER_V3: Pubkey = client::BPF_LOADER_UPGRADEABLE_ID;
/// `test-hook` (approves every transfer) and `test-hook-refill` (on every transfer of its
/// mint, empties a stash into a fixed target account).
const HOOK_ID: Pubkey = Pubkey::new_from_array(*b"pipeline-router-fuzz-hook-allow!");
const REFILL_ID: Pubkey = Pubkey::new_from_array(*b"pipeline-router-fuzz-hook-refill");
const COMPUTE_BUDGET_ID: Pubkey = client::pubkey!("ComputeBudget111111111111111111111111111111");

/// Mirrors `test-strategy-script`.
mod script {
    pub const RUN: [u8; 8] = *b"scriptrn";
    pub const STATE_SEED: &[u8] = b"script";
    pub const STATE_LEN: usize = 512;
    pub const TRANSFER: u8 = 0;
    pub const MINT_TO: u8 = 5;
    pub const SYSTEM_TRANSFER: u8 = 7;
    pub const TRANSFER_WITH: u8 = 15;
    pub const FREEZE: u8 = 16;
    pub const LITERAL: u8 = 0;
    pub const SENT_PLUS: u8 = 1;
    pub const BALANCE_OF: u8 = 2;
    pub const SMALL: u8 = 3;
    pub const TRADE_PLUS: u8 = 4;
}

/// Swap account positions as the strategy sees them.
mod at {
    pub const MARKET_SIGNER: u8 = 0;
    pub const STATE: u8 = 1;
    pub const TRADER: u8 = 2;
    pub const TRADER_INPUT: u8 = 3;
    pub const TRADER_OUTPUT: u8 = 4;
    pub const INPUT_VAULT: u8 = 5;
    pub const INPUT_MINT: u8 = 6;
    pub const OUTPUT_MINT: u8 = 7;
    /// First remaining account: by convention, the output vault.
    pub const OUTPUT_VAULT: u8 = 10;
}

/// Swap instruction account positions (see `pipeline_client::swap`).
mod swap_meta {
    pub const TRADER: usize = 0;
    pub const STRATEGY: usize = 1;
    pub const MARKET: usize = 2;
    pub const TRADER_INPUT: usize = 4;
    pub const TRADER_OUTPUT: usize = 5;
    pub const INPUT_VAULT: usize = 6;
    pub const INPUT_MINT: usize = 7;
    pub const OUTPUT_MINT: usize = 8;
    pub const FEE_ACCOUNT: usize = 12;
    pub const INTEGRATOR: usize = 13;
}

const HOOK_COUNT_OFFSET: usize = 8 + 1 + 8 + 8 + 8 + 2;
const MAX_STEPS: usize = 64;
const TRADER_FUNDS: u64 = 1_000_000_000_000;
const VAULT_FUNDS: u64 = 1_000_000_000_000_000;
/// Mint 3's Token-2022 transfer fee.
const TRANSFER_FEE_BPS: u16 = 100;
const TRANSFER_FEE_MAX: u64 = 5_000_000;

// ------------------------------------------------------------------------------- tape

/// Fuzz input. Reads past the end return zeros, so every tape decodes.
struct Tape<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Tape<'_> {
    fn done(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    fn u8(&mut self) -> u8 {
        let b = self.bytes.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        b
    }

    fn u16(&mut self) -> u16 {
        u16::from_le_bytes([self.u8(), self.u8()])
    }

    fn u64(&mut self) -> u64 {
        let mut b = [0u8; 8];
        for x in &mut b {
            *x = self.u8();
        }
        u64::from_le_bytes(b)
    }

    fn chance(&mut self, percent: u8) -> bool {
        self.u8() % 100 < percent
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            self.u16() as usize % n
        }
    }

    fn pick<T: Clone>(&mut self, items: &[T]) -> T {
        items[self.below(items.len())].clone()
    }

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.u8()).collect()
    }

    /// An amount biased towards edges and towards `typical`.
    fn amount(&mut self, typical: u64) -> u64 {
        match self.u8() % 16 {
            0 => 0,
            1 => 1,
            2 => self.u8() as u64,
            3 => typical,
            4 => typical.saturating_add(1),
            5 => typical.saturating_sub(1),
            6 => u64::MAX,
            7 => self.u64(),
            8 => 399 + (self.u8() % 4) as u64,
            9 => 1 + self.u64() % TRADER_FUNDS,
            _ => 1 + self.u64() % 10_000_000,
        }
    }
}

// ------------------------------------------------------------------------------ world

#[derive(Clone)]
struct MintSpec {
    key: Pubkey,
    program: Pubkey,
    /// Its permanent delegate is a market PDA, so balances can move without the holder.
    permanent_delegate: bool,
    /// Accounts every transfer of this mint must carry for its transfer hook (the hook
    /// program, its validation account, then its extra accounts); empty without a hook.
    hook_accounts: Vec<AccountMeta>,
}

#[derive(Clone)]
struct MarketSpec {
    nonce: [u8; 32],
    state: Pubkey,
    market: Pubkey,
    /// Indices into `World::mints`.
    mints: Vec<usize>,
    vaults: Vec<Pubkey>,
}

struct World {
    svm: LiteSVM,
    /// Pays every transaction's fee and nothing else; never passed to a program.
    payer: Keypair,
    admin: Keypair,
    strategy_owner: Keypair,
    traders: Vec<Keypair>,
    /// `trader_accounts[trader][mint]`.
    trader_accounts: Vec<Vec<Pubkey>>,
    victim_accounts: Vec<Pubkey>,
    integrator_accounts: Vec<Pubkey>,
    treasury_accounts: Vec<Pubkey>,
    treasury_owner: Pubkey,
    /// The refill hook's stash (mint 0), which it empties into trader 0's mint-0 account
    /// on every transfer of mint 7.
    stash: Pubkey,
    mints: Vec<MintSpec>,
    markets: Vec<MarketSpec>,
    /// Unregistered copies of the script strategy: upgradeable by the strategy owner,
    /// and immutable.
    spare_programs: Vec<Pubkey>,
    /// Every interesting address, for substitutions and strategy accounts.
    pool: Vec<Pubkey>,
    /// Keypairs that may sign, other than the payer.
    keyring: Vec<Keypair>,
}

impl World {
    fn fork(&self) -> World {
        World {
            svm: self.svm.clone(),
            payer: self.payer.insecure_clone(),
            admin: self.admin.insecure_clone(),
            strategy_owner: self.strategy_owner.insecure_clone(),
            traders: self.traders.iter().map(|k| k.insecure_clone()).collect(),
            trader_accounts: self.trader_accounts.clone(),
            victim_accounts: self.victim_accounts.clone(),
            integrator_accounts: self.integrator_accounts.clone(),
            treasury_accounts: self.treasury_accounts.clone(),
            treasury_owner: self.treasury_owner,
            stash: self.stash,
            mints: self.mints.clone(),
            markets: self.markets.clone(),
            spare_programs: self.spare_programs.clone(),
            pool: self.pool.clone(),
            keyring: self.keyring.iter().map(|k| k.insecure_clone()).collect(),
        }
    }

    fn keypair(&self, key: &Pubkey) -> Option<&Keypair> {
        self.keyring.iter().find(|k| k.pubkey() == *key)
    }

    /// Sends an unchecked setup transaction.
    fn setup(&mut self, ixs: &[Instruction], signers: &[&Keypair]) {
        self.svm.expire_blockhash();
        let mut all = vec![&self.payer];
        all.extend_from_slice(signers);
        let tx = Transaction::new_signed_with_payer(
            ixs,
            Some(&self.payer.pubkey()),
            &all,
            self.svm.latest_blockhash(),
        );
        if let Err(e) = self.svm.send_transaction(tx) {
            panic!("setup failed: {:?}\n{}", e.err, e.meta.logs.join("\n"));
        }
    }

    fn add_to_pool(&mut self, keys: &[Pubkey]) {
        for key in keys {
            if !self.pool.contains(key) {
                self.pool.push(*key);
            }
        }
    }
}

fn state_address(nonce: &[u8; 32]) -> Pubkey {
    Pubkey::find_program_address(&[script::STATE_SEED, nonce], &SCRIPT_ID).0
}

fn nonce_from(n: u64) -> [u8; 32] {
    let mut nonce = [0u8; 32];
    nonce[..8].copy_from_slice(&n.to_le_bytes());
    nonce[8..16].copy_from_slice(b"fuzznonc");
    nonce
}

fn market_info_bytes(mints: &[Pubkey], vaults: &[Pubkey], exact_out: bool) -> Vec<u8> {
    borsh::to_vec(&psi::MarketInfo {
        mints: mints.iter().map(|k| k.to_bytes()).collect(),
        vaults: vaults.iter().map(|k| k.to_bytes()).collect(),
        supports_exact_out: exact_out,
    })
    .unwrap()
}

/// `pipeline_init_market` data for the script strategy.
fn init_data(nonce: &[u8; 32], info: &[u8], init_script: &[u8]) -> Vec<u8> {
    let mut data = nonce.to_vec();
    data.extend_from_slice(&(info.len() as u16).to_le_bytes());
    data.extend_from_slice(info);
    data.extend_from_slice(init_script);
    data
}

fn create_market_ix(
    creator: &Pubkey,
    strategy_program: &Pubkey,
    nonce: &[u8; 32],
    info: &[u8],
    init_script: &[u8],
    max_creator_lamports: u64,
    strategy_accounts: Vec<AccountMeta>,
) -> Instruction {
    client::create_market(&client::CreateMarket {
        creator: *creator,
        strategy_program: *strategy_program,
        market_state: state_address(nonce),
        market_state_signs: false,
        init_data: init_data(nonce, info, init_script),
        max_creator_lamports,
        strategy_accounts,
    })
}

/// A Token-2022 mint whose permanent delegate is `delegate`.
fn create_delegated_mint(env: &mut Env, delegate: &Pubkey) -> Pubkey {
    use spl_token_2022_interface::extension::ExtensionType;
    let mint = Keypair::new();
    let space =
        ExtensionType::try_calculate_account_len::<spl_token_2022_interface::state::Mint>(&[
            ExtensionType::PermanentDelegate,
        ])
        .unwrap();
    let admin = env.admin.pubkey();
    let rent = env.svm.minimum_balance_for_rent_exemption(space);
    env.send(
        &[
            solana_system_interface::instruction::create_account(
                &admin,
                &mint.pubkey(),
                rent,
                space as u64,
                &TOKEN_2022,
            ),
            spl_token_2022_interface::instruction::initialize_permanent_delegate(
                &TOKEN_2022,
                &mint.pubkey(),
                delegate,
            )
            .unwrap(),
            spl_token_2022_interface::instruction::initialize_mint2(
                &TOKEN_2022,
                &mint.pubkey(),
                &admin,
                None,
                6,
            )
            .unwrap(),
        ],
        &[&mint],
    )
    .expect("delegated mint");
    mint.pubkey()
}

/// An SPL Token mint (6 decimals, admin mint authority) with `freezer` as its freeze
/// authority.
fn create_freezable_mint(env: &mut Env, freezer: &Pubkey) -> Pubkey {
    let mint = Keypair::new();
    let admin = env.admin.pubkey();
    let rent = env.svm.minimum_balance_for_rent_exemption(82);
    env.send(
        &[
            solana_system_interface::instruction::create_account(
                &admin,
                &mint.pubkey(),
                rent,
                82,
                &TOKEN,
            ),
            spl_token_interface::instruction::initialize_mint2(
                &TOKEN,
                &mint.pubkey(),
                &admin,
                Some(freezer),
                6,
            )
            .unwrap(),
        ],
        &[&mint],
    )
    .expect("freezable mint");
    mint.pubkey()
}

fn hook_validation(hook: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"extra-account-metas", mint.as_ref()], hook).0
}

/// Sets up mint 6's allow-all hook and mint 7's refill hook (target: `refill_target`),
/// records each mint's hook accounts, and returns the refill hook's stash.
fn init_hooks(env: &mut Env, mints: &mut [MintSpec], refill_target: Pubkey) -> Pubkey {
    let admin = env.admin.pubkey();
    let (allow_mint, refill_mint) = (mints[6].key, mints[7].key);

    let allow_validation = hook_validation(&HOOK_ID, &allow_mint);
    let whitelist = Pubkey::find_program_address(&[b"whitelist", allow_mint.as_ref()], &HOOK_ID).0;
    let init = Instruction::new_with_bytes(
        HOOK_ID,
        b"tsthkini",
        vec![
            AccountMeta::new(admin, true),
            AccountMeta::new(allow_validation, false),
            AccountMeta::new(whitelist, false),
            AccountMeta::new_readonly(allow_mint, false),
            AccountMeta::new_readonly(client::SYSTEM_PROGRAM_ID, false),
        ],
    );
    env.send(&[init], &[]).expect("allow hook init");
    mints[6].hook_accounts = vec![
        AccountMeta::new_readonly(HOOK_ID, false),
        AccountMeta::new_readonly(allow_validation, false),
    ];

    let refill_validation = hook_validation(&REFILL_ID, &refill_mint);
    let stash_authority = Pubkey::find_program_address(&[b"stash"], &REFILL_ID).0;
    let stash = env.create_ata(&stash_authority, &mints[0].key, &TOKEN);
    let mut data = b"refilini".to_vec();
    for (key, writable) in [
        (stash, 1u8),
        (stash_authority, 0),
        (refill_target, 1),
        (TOKEN, 0),
    ] {
        data.extend_from_slice(key.as_ref());
        data.push(writable);
    }
    let init = Instruction::new_with_bytes(
        REFILL_ID,
        &data,
        vec![
            AccountMeta::new(admin, true),
            AccountMeta::new(refill_validation, false),
            AccountMeta::new_readonly(refill_mint, false),
            AccountMeta::new_readonly(client::SYSTEM_PROGRAM_ID, false),
        ],
    );
    env.send(&[init], &[]).expect("refill hook init");
    mints[7].hook_accounts = vec![
        AccountMeta::new_readonly(REFILL_ID, false),
        AccountMeta::new_readonly(refill_validation, false),
        AccountMeta::new(stash, false),
        AccountMeta::new_readonly(stash_authority, false),
        AccountMeta::new(refill_target, false),
        AccountMeta::new_readonly(TOKEN, false),
    ];
    stash
}

/// The fixed world every case starts from.
fn build_world() -> World {
    let mut env = Env::new();
    let strategy_owner = env.register(&SCRIPT_ID, "test_strategy_script");

    // Market nonces are fixed so mints can name market PDAs before the markets exist.
    let nonces: Vec<[u8; 32]> = (0..6).map(nonce_from).collect();
    let states: Vec<Pubkey> = nonces.iter().map(state_address).collect();
    let market_pdas: Vec<Pubkey> = states.iter().map(|s| pda::market(&SCRIPT_ID, s)).collect();
    env.svm
        .add_program_from_file(HOOK_ID, sbf_dir().join("test_hook.so"))
        .expect("test_hook.so");
    env.svm
        .add_program_from_file(REFILL_ID, sbf_dir().join("test_hook_refill.so"))
        .expect("test_hook_refill.so");

    let mut mints = vec![];
    let mut add_mint = |key, program, permanent_delegate| {
        mints.push(MintSpec {
            key,
            program,
            permanent_delegate,
            hook_accounts: vec![],
        })
    };
    add_mint(env.create_mint(&TOKEN, 6, None, None), TOKEN, false);
    add_mint(env.create_mint(&TOKEN, 9, None, None), TOKEN, false);
    add_mint(
        env.create_mint(&TOKEN_2022, 0, None, None),
        TOKEN_2022,
        false,
    );
    add_mint(
        env.create_mint(
            &TOKEN_2022,
            6,
            Some((TRANSFER_FEE_BPS, TRANSFER_FEE_MAX)),
            None,
        ),
        TOKEN_2022,
        false,
    );
    add_mint(
        create_delegated_mint(&mut env, &market_pdas[1]),
        TOKEN_2022,
        true,
    );
    // Mint 5's mint authority becomes market 2's state, so that market can mint output,
    // and its freeze authority is that state from the start.
    add_mint(create_freezable_mint(&mut env, &states[2]), TOKEN, false);
    // Mints 6 and 7 have transfer hooks: one that approves everything, and one that
    // refills trader 0's mint-0 account from a stash during every transfer.
    add_mint(
        env.create_mint(&TOKEN_2022, 6, None, Some(HOOK_ID)),
        TOKEN_2022,
        false,
    );
    add_mint(
        env.create_mint(&TOKEN_2022, 6, None, Some(REFILL_ID)),
        TOKEN_2022,
        false,
    );

    let market_mints: [Vec<usize>; 6] = [
        vec![0, 1],
        vec![4, 0],
        vec![1, 5, 3],
        vec![2, 3, 0, 1],
        vec![6, 0, 1],
        vec![0, 7],
    ];

    let traders: Vec<Keypair> = (0..3).map(|_| env.funded_keypair()).collect();
    let victim = Pubkey::new_unique();
    let integrator = Pubkey::new_unique();
    let treasury_owner = env.treasury_owner;
    let mut trader_accounts = vec![vec![]; traders.len()];
    let mut victim_accounts = vec![];
    let mut integrator_accounts = vec![];
    let mut treasury_accounts = vec![];
    for mint in &mints {
        for (t, trader) in traders.iter().enumerate() {
            let account = env.create_ata(&trader.pubkey(), &mint.key, &mint.program);
            env.mint_to(&mint.key, &mint.program, &account, TRADER_FUNDS);
            trader_accounts[t].push(account);
        }
        let account = env.create_ata(&victim, &mint.key, &mint.program);
        env.mint_to(&mint.key, &mint.program, &account, TRADER_FUNDS);
        victim_accounts.push(account);
        integrator_accounts.push(env.create_ata(&integrator, &mint.key, &mint.program));
        treasury_accounts.push(env.create_ata(&treasury_owner, &mint.key, &mint.program));
    }

    let stash = init_hooks(&mut env, &mut mints, trader_accounts[0][0]);

    let mut markets = vec![];
    for (i, mint_ids) in market_mints.iter().enumerate() {
        let state = states[i];
        let keys: Vec<Pubkey> = mint_ids.iter().map(|&m| mints[m].key).collect();
        let vaults: Vec<Pubkey> = mint_ids
            .iter()
            .map(|&m| env.create_ata(&state, &mints[m].key, &mints[m].program))
            .collect();
        let creator = env.funded_keypair();
        let ix = create_market_ix(
            &creator.pubkey(),
            &SCRIPT_ID,
            &nonces[i],
            &market_info_bytes(&keys, &vaults, i % 2 == 1),
            &[],
            MAX_SETUP_LAMPORTS,
            vec![],
        );
        env.send(&[ix], &[&creator]).expect("create market");
        let payer = env.admin.pubkey();
        for &m in mint_ids {
            env.send(
                &[client::init_fee_account(
                    &payer,
                    &market_pdas[i],
                    &mints[m].key,
                    &mints[m].program,
                )],
                &[],
            )
            .expect("fee account");
        }
        for (&m, vault) in mint_ids.iter().zip(&vaults) {
            env.mint_to(&mints[m].key, &mints[m].program, vault, VAULT_FUNDS);
        }
        markets.push(MarketSpec {
            nonce: nonces[i],
            state,
            market: market_pdas[i],
            mints: mint_ids.clone(),
            vaults,
        });
    }
    let admin = env.admin.pubkey();
    // Small next to typical swaps: a refill matters when it covers part of the input.
    env.mint_to(&mints[0].key, &TOKEN, &stash, 600_000);
    env.send(
        &[spl_token_interface::instruction::set_authority(
            &TOKEN,
            &mints[5].key,
            Some(&states[2]),
            spl_token_interface::instruction::AuthorityType::MintTokens,
            &admin,
            &[],
        )
        .unwrap()],
        &[],
    )
    .expect("hand mint 5 to market 2");

    let spare_programs = vec![Pubkey::new_unique(), Pubkey::new_unique()];
    env.deploy_strategy(
        &spare_programs[0],
        "test_strategy_script",
        &strategy_owner.pubkey(),
    );
    env.deploy_strategy(&spare_programs[1], "test_strategy_script", &admin);
    set_upgrade_authority(&mut env.svm, &spare_programs[1], None);

    let payer = env.funded_keypair();
    let Env {
        svm,
        admin,
        listing_fee_recipient,
        ..
    } = env;

    let mut world = World {
        svm,
        payer,
        keyring: traders
            .iter()
            .chain([&admin, &strategy_owner])
            .map(|k| k.insecure_clone())
            .collect(),
        admin,
        strategy_owner,
        traders,
        trader_accounts,
        victim_accounts,
        integrator_accounts,
        treasury_accounts,
        treasury_owner,
        stash,
        mints,
        markets,
        spare_programs,
        pool: vec![],
    };

    let mut pool = vec![
        client::PROGRAM_ID,
        pda::config(),
        pda::event_authority(),
        pda::program_data(&client::PROGRAM_ID),
        client::SYSTEM_PROGRAM_ID,
        TOKEN,
        TOKEN_2022,
        client::ASSOCIATED_TOKEN_PROGRAM_ID,
        SCRIPT_ID,
        pda::strategy(&SCRIPT_ID),
        pda::register(&SCRIPT_ID),
        world.admin.pubkey(),
        world.strategy_owner.pubkey(),
        listing_fee_recipient,
        treasury_owner,
        victim,
        integrator,
        Pubkey::new_unique(),
    ];
    pool.extend(world.spare_programs.clone());
    pool.extend(world.traders.iter().map(|t| t.pubkey()));
    pool.extend(world.trader_accounts.iter().flatten());
    pool.extend(&world.victim_accounts);
    pool.extend(&world.integrator_accounts);
    pool.extend(&world.treasury_accounts);
    pool.extend(world.mints.iter().map(|m| m.key));
    pool.extend(
        world
            .mints
            .iter()
            .flat_map(|m| m.hook_accounts.iter().map(|a| a.pubkey)),
    );
    world.add_to_pool(&pool);
    for market in world.markets.clone() {
        world.add_market_to_pool(&market);
    }
    world
}

impl World {
    fn add_market_to_pool(&mut self, market: &MarketSpec) {
        let mut keys = vec![
            market.state,
            market.market,
            pda::fee_authority(&market.market),
            pda::setup_payer(&market.market),
        ];
        keys.extend(&market.vaults);
        for &m in &market.mints {
            keys.push(pda::fee_account(
                &market.market,
                &self.mints[m].key,
                &self.mints[m].program,
            ));
        }
        self.add_to_pool(&keys);
    }
}

// ----------------------------------------------------------------------- transactions

#[derive(Clone, Debug)]
enum Kind {
    Swap(client::SwapArgs),
    Sweep,
    InitFee,
    CreateMarket {
        max_creator_lamports: u64,
    },
    RegisterExisting,
    UpdateConfig,
    RegisterStrategy,
    UpdateMetadata,
    Raw,
    /// The strategy acting on its own, outside the router.
    DirectScript,
    /// A trader-signed token transfer of `amount` out of `from`.
    TraderTransfer {
        from: Pubkey,
        amount: u64,
    },
}

impl Kind {
    fn name(&self) -> &'static str {
        match self {
            Kind::Swap(_) => "swap",
            Kind::Sweep => "sweep_fees",
            Kind::InitFee => "init_fee_account",
            Kind::CreateMarket { .. } => "create_market",
            Kind::RegisterExisting => "register_existing_market",
            Kind::UpdateConfig => "update_config",
            Kind::RegisterStrategy => "register_strategy",
            Kind::UpdateMetadata => "update_metadata",
            Kind::Raw => "raw",
            Kind::DirectScript => "direct_script",
            Kind::TraderTransfer { .. } => "trader_transfer",
        }
    }
}

struct Step {
    kind: Kind,
    ix: Instruction,
}

/// Where a swap's hook accounts sit among the accounts the strategy receives.
struct Layout {
    out_hooks: Vec<u8>,
    in_hooks: Vec<u8>,
    exact_out: bool,
}

/// A market created by this transaction that later steps may trade, if it succeeds.
/// A valid market's mint indices and vaults.
type ValidInfo = (Vec<usize>, Vec<Pubkey>);

struct Pending {
    market: MarketSpec,
}

// --------------------------------------------------------------------------- generator

impl World {
    /// Every token account of `mint` the world knows: traders', the bystander's, the
    /// integrator's, the treasury's, and every market's vaults and fee accounts.
    fn token_accounts_of(&self, mint: usize) -> Vec<Pubkey> {
        let spec = &self.mints[mint];
        let mut accounts: Vec<Pubkey> = self.trader_accounts.iter().map(|a| a[mint]).collect();
        accounts.push(self.victim_accounts[mint]);
        accounts.push(self.integrator_accounts[mint]);
        accounts.push(self.treasury_accounts[mint]);
        for market in &self.markets {
            if let Some(i) = market.mints.iter().position(|&m| m == mint) {
                accounts.push(market.vaults[i]);
                accounts.push(pda::fee_account(&market.market, &spec.key, &spec.program));
            }
        }
        accounts
    }

    fn any_signer(&self, tape: &mut Tape) -> Pubkey {
        self.keyring[tape.below(self.keyring.len())].pubkey()
    }

    fn pool_pick(&self, tape: &mut Tape) -> Pubkey {
        tape.pick(&self.pool)
    }

    /// Random extra accounts, never signing.
    fn pool_metas(&self, tape: &mut Tape, max: usize) -> Vec<AccountMeta> {
        let n = tape.below(max + 1);
        (0..n)
            .map(|_| {
                let key = self.pool_pick(tape);
                if tape.chance(60) {
                    AccountMeta::new(key, false)
                } else {
                    AccountMeta::new_readonly(key, false)
                }
            })
            .collect()
    }

    /// Replaces a few accounts with pool accounts and flips some writable flags. A
    /// replaced signer keeps signing only if we hold its key.
    fn mutate_accounts(&self, tape: &mut Tape, ix: &mut Instruction) {
        if ix.accounts.is_empty() || !tape.chance(18) {
            return;
        }
        for _ in 0..1 + tape.below(3) {
            let i = tape.below(ix.accounts.len());
            match tape.u8() % 4 {
                0 => ix.accounts[i].is_writable = !ix.accounts[i].is_writable,
                _ => {
                    let key = if ix.accounts[i].is_signer {
                        self.any_signer(tape)
                    } else {
                        self.pool_pick(tape)
                    };
                    ix.accounts[i].pubkey = key;
                }
            }
        }
    }

    /// Script operations: honest templates mixed with raw operations.
    fn swap_script(
        &self,
        tape: &mut Tape,
        market: &MarketSpec,
        output: usize,
        layout: &Layout,
    ) -> Vec<u8> {
        let mut s = vec![];
        let sent_plus = |s: &mut Vec<u8>, delta: i16| {
            s.push(script::SENT_PLUS);
            s.extend_from_slice(&delta.to_le_bytes());
        };
        let small_delta = |tape: &mut Tape| (tape.u8() % 7) as i16 - 3;
        // A token transfer, carrying the mint's hook accounts if it has a hook.
        let transfer =
            |s: &mut Vec<u8>, from: u8, mint: u8, to: u8, authority: u8, hooks: &[u8]| {
                if hooks.is_empty() {
                    s.extend([script::TRANSFER, from, mint, to, authority]);
                } else {
                    s.extend([script::TRANSFER_WITH, from, mint, to, authority]);
                }
            };
        let hook_tail = |s: &mut Vec<u8>, hooks: &[u8]| {
            if !hooks.is_empty() {
                s.push(hooks.len() as u8);
                s.extend(hooks);
            }
        };
        // Market 2 holds mint 5's mint authority, so it can mint its output instead.
        let can_mint = market.market == self.markets[2].market && market.mints[output] == 5;
        let pay = |s: &mut Vec<u8>, tape: &mut Tape| {
            if can_mint && tape.chance(50) {
                s.extend([
                    script::MINT_TO,
                    at::OUTPUT_MINT,
                    at::TRADER_OUTPUT,
                    at::STATE,
                ]);
                sent_plus(s, small_delta(tape));
                return;
            }
            transfer(
                s,
                at::OUTPUT_VAULT,
                at::OUTPUT_MINT,
                at::TRADER_OUTPUT,
                at::STATE,
                &layout.out_hooks,
            );
            if layout.exact_out && tape.chance(70) {
                s.push(script::TRADE_PLUS);
                s.extend_from_slice(&small_delta(tape).to_le_bytes());
            } else {
                sent_plus(s, small_delta(tape));
            }
            hook_tail(s, &layout.out_hooks);
        };
        let refund = |s: &mut Vec<u8>, tape: &mut Tape| {
            transfer(
                s,
                at::INPUT_VAULT,
                at::INPUT_MINT,
                at::TRADER_INPUT,
                at::STATE,
                &layout.in_hooks,
            );
            match tape.u8() % 3 {
                0 => sent_plus(s, -(tape.u8() as i16)),
                1 => sent_plus(s, -((tape.u16() % 30_000) as i16)),
                _ => s.extend([script::SMALL, tape.u8()]),
            }
            hook_tail(s, &layout.in_hooks);
        };
        match tape.u8() % 12 {
            0..=3 => pay(&mut s, tape),
            4 => {
                // Pays, then refunds part (or more than all) of the input, as an
                // exact-out swap refunds what it did not need.
                pay(&mut s, tape);
                refund(&mut s, tape);
            }
            5 => {}
            6 => {
                // Pays, then pulls more input (or output back) with the market signer,
                // which works only for a mint whose permanent delegate is the market.
                pay(&mut s, tape);
                let (from, mint, to, hooks) = if tape.chance(50) {
                    (
                        at::TRADER_INPUT,
                        at::INPUT_MINT,
                        at::INPUT_VAULT,
                        &layout.in_hooks,
                    )
                } else {
                    (
                        at::TRADER_OUTPUT,
                        at::OUTPUT_MINT,
                        at::OUTPUT_VAULT,
                        &layout.out_hooks,
                    )
                };
                let authority = if tape.chance(70) {
                    at::MARKET_SIGNER
                } else {
                    at::TRADER
                };
                transfer(&mut s, from, mint, to, authority, hooks);
                match tape.u8() % 3 {
                    0 => s.extend([script::BALANCE_OF, from]),
                    1 => sent_plus(&mut s, tape.u16() as i16),
                    _ => s.extend([script::SMALL, tape.u8()]),
                }
                hook_tail(&mut s, hooks);
            }
            7 => {
                // Pays the output to somewhere else.
                let to = 11 + tape.u8() % 4;
                transfer(
                    &mut s,
                    at::OUTPUT_VAULT,
                    at::OUTPUT_MINT,
                    to,
                    at::STATE,
                    &layout.out_hooks,
                );
                sent_plus(&mut s, 0);
                hook_tail(&mut s, &layout.out_hooks);
            }
            8 => {
                // Moves the input vault's balance around before paying.
                let to = 11 + tape.u8() % 4;
                transfer(
                    &mut s,
                    at::INPUT_VAULT,
                    at::INPUT_MINT,
                    to,
                    at::STATE,
                    &layout.in_hooks,
                );
                s.extend([script::BALANCE_OF, at::INPUT_VAULT]);
                hook_tail(&mut s, &layout.in_hooks);
                pay(&mut s, tape);
            }
            9 => {
                // Pays, then freezes an account with the state's freeze authority (mint
                // 5's is market 2's state): the trader's input or output, or a bystander.
                pay(&mut s, tape);
                let (account, mint) = match tape.u8() % 3 {
                    0 => (at::TRADER_INPUT, at::INPUT_MINT),
                    1 => (at::TRADER_OUTPUT, at::OUTPUT_MINT),
                    _ => (11 + tape.u8() % 4, at::OUTPUT_MINT),
                };
                s.extend([script::FREEZE, account, mint, at::STATE]);
            }
            10 => {
                // Burns compute, then pays: deep enough to need a raised compute limit.
                s.extend([12, tape.u8()]);
                pay(&mut s, tape);
            }
            _ => {
                let len = tape.below(28);
                s.extend(tape.bytes(len));
            }
        }
        if tape.chance(25) {
            let len = tape.below(20);
            s.extend(tape.bytes(len));
        }
        s
    }

    fn gen_swap(&self, tape: &mut Tape) -> Step {
        let t = tape.below(self.traders.len());
        let trader = self.traders[t].pubkey();
        let market = tape.pick(&self.markets);
        let n = market.mints.len();
        let input = tape.below(n);
        let mut output = (input + 1 + tape.below(n - 1)) % n;
        if tape.chance(2) {
            output = input;
        }
        let (in_mint, out_mint) = (market.mints[input], market.mints[output]);
        let mode = if tape.chance(12) {
            TradeMode::ExactOut
        } else {
            TradeMode::ExactIn
        };
        let amount_in = tape.amount(1_000_000);
        let amount_out = if tape.chance(50) {
            amount_in.saturating_sub(tape.u16() as u64)
        } else {
            tape.amount(amount_in)
        };
        let min_amount_out = match tape.u8() % 6 {
            0..=2 => 0,
            3 => amount_in,
            4 => amount_in.saturating_sub(tape.u8() as u64),
            _ => tape.amount(amount_in),
        };
        let integrator_fee_bps = match tape.u8() % 8 {
            0..=4 => 0,
            5 => tape.u16() % 500,
            6 => 10_000,
            _ => tape.u16(),
        };
        let integrator_fee_account = match tape.u8() % 16 {
            0..=9 if integrator_fee_bps == 0 => None,
            0..=11 => Some(self.integrator_accounts[in_mint]),
            12 => Some(self.trader_accounts[t][in_mint]),
            13 => Some(pda::fee_account(
                &market.market,
                &self.mints[in_mint].key,
                &self.mints[in_mint].program,
            )),
            _ => Some(self.pool_pick(tape)),
        };
        // Strategy accounts: the output vault, the output mint's hook accounts, a few
        // random accounts; the router appends the input mint's hook accounts last.
        let mut strategy_accounts = vec![AccountMeta::new(market.vaults[output], false)];
        let out_hook_accounts = &self.mints[out_mint].hook_accounts;
        let out_hooks: Vec<u8> = (0..out_hook_accounts.len() as u8).map(|k| 11 + k).collect();
        strategy_accounts.extend(out_hook_accounts.iter().cloned());
        strategy_accounts.extend(self.pool_metas(tape, 4));
        let input_hook_accounts = if tape.chance(92) {
            self.mints[in_mint].hook_accounts.clone()
        } else {
            self.pool_metas(tape, 3)
        };
        let in_base = 10 + strategy_accounts.len() as u8;
        let layout = Layout {
            out_hooks,
            in_hooks: (0..input_hook_accounts.len() as u8)
                .map(|k| in_base + k)
                .collect(),
            exact_out: mode == TradeMode::ExactOut,
        };
        let args = client::SwapArgs {
            mode,
            amount_in,
            amount_out,
            min_amount_out,
            integrator_fee_bps,
            input_hook_accounts: input_hook_accounts.len() as u8,
            extra: self.swap_script(tape, &market, output, &layout),
        };
        let accounts = client::SwapAccounts {
            trader,
            strategy_program: SCRIPT_ID,
            market_state: market.state,
            trader_input: self.trader_accounts[t][in_mint],
            trader_output: self.trader_accounts[t][out_mint],
            input_vault: market.vaults[input],
            input_mint: self.mints[in_mint].key,
            output_mint: self.mints[out_mint].key,
            input_token_program: self.mints[in_mint].program,
            output_token_program: self.mints[out_mint].program,
            integrator_fee_account,
            strategy_accounts,
            input_hook_accounts,
        };
        let mut ix = client::swap(&accounts, &args);
        let mut args = args;
        args.input_hook_accounts = accounts.input_hook_accounts.len() as u8;
        if tape.chance(12) {
            // Swap one token account for another real one of the right mint.
            let (slot, mint) = match tape.u8() % 5 {
                0 => (swap_meta::TRADER_INPUT, in_mint),
                1 => (swap_meta::TRADER_OUTPUT, out_mint),
                2 => (swap_meta::INPUT_VAULT, in_mint),
                3 => (swap_meta::FEE_ACCOUNT, in_mint),
                _ => (swap_meta::INTEGRATOR, in_mint),
            };
            ix.accounts[slot].pubkey = tape.pick(&self.token_accounts_of(mint));
        }
        if tape.chance(5) {
            args.input_hook_accounts = tape.u8() % 8;
            ix.data[HOOK_COUNT_OFFSET] = args.input_hook_accounts;
        }
        self.mutate_accounts(tape, &mut ix);
        Step {
            kind: Kind::Swap(args),
            ix,
        }
    }

    fn gen_sweep(&self, tape: &mut Tape) -> Step {
        let market = tape.pick(&self.markets);
        let mint = self.mints[tape.pick(&market.mints)].clone();
        let config = self.config();
        let hooks = if tape.chance(90) {
            mint.hook_accounts.clone()
        } else {
            vec![]
        };
        let mut ix = client::sweep_fees(
            &market.market,
            &mint.key,
            &mint.program,
            &config.treasury_owner,
            hooks,
        );
        self.mutate_accounts(tape, &mut ix);
        Step {
            kind: Kind::Sweep,
            ix,
        }
    }

    fn gen_init_fee(&self, tape: &mut Tape) -> Step {
        let market = tape.pick(&self.markets);
        let mint = if tape.chance(80) {
            self.mints[tape.pick(&market.mints)].clone()
        } else {
            tape.pick(&self.mints)
        };
        let program = if tape.chance(90) {
            mint.program
        } else {
            tape.pick(&[TOKEN, TOKEN_2022])
        };
        let mut ix =
            client::init_fee_account(&self.payer.pubkey(), &market.market, &mint.key, &program);
        self.mutate_accounts(tape, &mut ix);
        Step {
            kind: Kind::InitFee,
            ix,
        }
    }

    /// Market info the strategy will report: mostly valid, sometimes broken. Returns the
    /// bytes and, if valid, the market's mints and vaults.
    fn gen_info(&self, tape: &mut Tape, state: &Pubkey) -> (Vec<u8>, Option<ValidInfo>) {
        // Distinct mints: a random start and stride (coprime with the count when the
        // count is prime; duplicates are dropped otherwise), so it ends on any tape.
        let n = self.mints.len();
        let want = 2 + tape.below(3);
        let start = tape.below(n);
        let stride = 1 + tape.below(n - 1);
        let mut mint_ids: Vec<usize> = vec![];
        for k in 0..n {
            let m = (start + k * stride) % n;
            if !mint_ids.contains(&m) && mint_ids.len() < want {
                mint_ids.push(m);
            }
        }
        if mint_ids.len() < 2 {
            mint_ids = vec![start, (start + 1) % n];
        }
        let mut keys: Vec<Pubkey> = mint_ids.iter().map(|&m| self.mints[m].key).collect();
        let mut vaults: Vec<Pubkey> = mint_ids
            .iter()
            .map(|&m| {
                if tape.chance(85) {
                    pda::associated_token(state, &self.mints[m].key, &self.mints[m].program)
                } else {
                    self.pool_pick(tape)
                }
            })
            .collect();
        let exact_out = tape.chance(50);
        match tape.u8() % 12 {
            0 => keys.push(keys[0]),
            1 => keys.truncate(1),
            2 => {
                vaults.pop();
            }
            3 => {
                let len = tape.below(80);
                return (tape.bytes(len), None);
            }
            4 => {
                while keys.len() < 5 {
                    keys.push(Pubkey::new_unique());
                    vaults.push(Pubkey::new_unique());
                }
            }
            5 => keys[1] = self.pool_pick(tape),
            _ => {
                let info = market_info_bytes(&keys, &vaults, exact_out);
                return (info, Some((mint_ids, vaults)));
            }
        }
        (market_info_bytes(&keys, &vaults, exact_out), None)
    }

    fn gen_create_market(&mut self, tape: &mut Tape) -> (Step, Option<Pending>, Vec<Keypair>) {
        let creator = Keypair::new();
        self.svm.airdrop(&creator.pubkey(), 10 * SOL).unwrap();
        let nonce = nonce_from(1_000 + tape.u16() as u64);
        let state = state_address(&nonce);
        let (info, valid) = self.gen_info(tape, &state);
        let mut remaining = self.pool_metas(tape, 4);
        // Where the creator sits among the strategy's accounts, if passed (signing).
        let creator_at = tape.chance(20).then(|| {
            remaining.push(AccountMeta::new(creator.pubkey(), true));
            (4 + remaining.len() - 1) as u8
        });
        // A fresh keypair that signs keeps its signature for the strategy (by design).
        let fresh = Keypair::new();
        if tape.chance(10) {
            remaining.push(AccountMeta::new(fresh.pubkey(), true));
        }
        let mut init_script = vec![];
        match tape.u8() % 6 {
            0 => {
                // Spend the setup payer's SOL on someone else.
                let to = 4 + tape.below(remaining.len().max(1)) as u8;
                init_script.extend([script::SYSTEM_TRANSFER, 2, to, script::LITERAL]);
                init_script.extend(tape.amount(SOL).to_le_bytes());
            }
            1 => {
                let len = tape.below(24);
                init_script.extend(tape.bytes(len));
            }
            2 | 3 => {
                // Take the creator's SOL, which works only if the router passed on the
                // creator's signature.
                let from = creator_at.unwrap_or(4 + tape.below(remaining.len().max(1)) as u8);
                init_script.extend([script::SYSTEM_TRANSFER, from, 1, script::LITERAL]);
                init_script.extend((5 * SOL).to_le_bytes());
            }
            _ => {}
        }
        let rent = self
            .svm
            .minimum_balance_for_rent_exemption(script::STATE_LEN);
        let max_creator_lamports = match tape.u8() % 6 {
            0 => 0,
            1 => rent,
            2 => rent - 1,
            3 => tape.amount(SOL),
            _ => MAX_SETUP_LAMPORTS,
        };
        let program = if tape.chance(92) {
            SCRIPT_ID
        } else {
            self.pool_pick(tape)
        };
        let mut ix = create_market_ix(
            &creator.pubkey(),
            &program,
            &nonce,
            &info,
            &init_script,
            max_creator_lamports,
            remaining,
        );
        self.mutate_accounts(tape, &mut ix);
        let pending = valid.map(|(mints, vaults)| Pending {
            market: MarketSpec {
                nonce,
                state,
                market: pda::market(&SCRIPT_ID, &state),
                mints,
                vaults,
            },
        });
        (
            Step {
                kind: Kind::CreateMarket {
                    max_creator_lamports,
                },
                ix,
            },
            pending,
            vec![creator, fresh],
        )
    }

    fn gen_register_existing(&mut self, tape: &mut Tape) -> Step {
        // A market state the strategy made by itself.
        let nonce = nonce_from(100_000 + tape.u16() as u64);
        let (state, bump) = Pubkey::find_program_address(&[script::STATE_SEED, &nonce], &SCRIPT_ID);
        if self.svm.get_account(&state).is_none() {
            let (info, _) = self.gen_info(tape, &state);
            let mut data = vec![0u8; script::STATE_LEN];
            data[..32].copy_from_slice(&nonce);
            data[32] = bump;
            let len = info.len().min(script::STATE_LEN - 35);
            data[33..35].copy_from_slice(&(len as u16).to_le_bytes());
            data[35..35 + len].copy_from_slice(&info[..len]);
            let owner = if tape.chance(90) {
                SCRIPT_ID
            } else {
                self.pool_pick(tape)
            };
            let account = Account {
                lamports: self
                    .svm
                    .minimum_balance_for_rent_exemption(script::STATE_LEN),
                data,
                owner,
                executable: false,
                rent_epoch: 0,
            };
            self.svm.set_account(state, account).unwrap();
            self.add_to_pool(&[state]);
        }
        let program = if tape.chance(92) {
            SCRIPT_ID
        } else {
            self.pool_pick(tape)
        };
        let info_accounts = self.pool_metas(tape, 3);
        let mut ix =
            client::register_existing_market(&self.payer.pubkey(), &program, &state, info_accounts);
        self.mutate_accounts(tape, &mut ix);
        Step {
            kind: Kind::RegisterExisting,
            ix,
        }
    }

    fn gen_update_config(&self, tape: &mut Tape) -> Step {
        let signer = if tape.chance(70) {
            self.config().admin
        } else {
            self.any_signer(tape)
        };
        let signer = if self.keypair(&signer).is_some() {
            signer
        } else {
            self.admin.pubkey()
        };
        let rent0 = self.svm.minimum_balance_for_rent_exemption(0);
        let mut args = client::UpdateConfigArgs::default();
        if tape.chance(15) {
            args.admin = Some(match tape.u8() % 3 {
                0 => self.admin.pubkey(),
                1 => self.traders[0].pubkey(),
                _ => self.pool_pick(tape),
            });
        }
        if tape.chance(30) {
            args.listing_fee_lamports = Some(match tape.u8() % 7 {
                0 => 0,
                1 => rent0 - 1,
                2 => rent0,
                3 => 1_000_000_000_000,
                4 => 1_000_000_000_001,
                5 => u64::MAX,
                _ => tape.u64(),
            });
        }
        if tape.chance(20) {
            args.listing_fee_recipient = Some(self.pool_pick(tape));
        }
        if tape.chance(20) {
            args.treasury_owner = Some(if tape.chance(50) {
                self.treasury_owner
            } else {
                self.pool_pick(tape)
            });
        }
        let mut ix = client::update_config(&signer, &args);
        self.mutate_accounts(tape, &mut ix);
        Step {
            kind: Kind::UpdateConfig,
            ix,
        }
    }

    fn gen_register_strategy(&self, tape: &mut Tape) -> Step {
        let program = match tape.u8() % 6 {
            0 | 1 => self.spare_programs[0],
            2 => self.spare_programs[1],
            3 => SCRIPT_ID,
            4 => client::PROGRAM_ID,
            _ => self.pool_pick(tape),
        };
        let registrar = self.any_signer(tape);
        let payer = self.any_signer(tape);
        let uri = "u".repeat(tape.below(230));
        let config = self.config();
        let max_fee = match tape.u8() % 3 {
            0 => config.listing_fee_lamports,
            1 => config.listing_fee_lamports.saturating_sub(1),
            _ => u64::MAX,
        };
        let recipient = if tape.chance(90) {
            config.listing_fee_recipient
        } else {
            self.pool_pick(tape)
        };
        let mut ix =
            client::register_strategy(&payer, &registrar, &program, &recipient, &uri, max_fee);
        self.mutate_accounts(tape, &mut ix);
        Step {
            kind: Kind::RegisterStrategy,
            ix,
        }
    }

    fn gen_update_metadata(&self, tape: &mut Tape) -> Step {
        let program = match tape.u8() % 3 {
            0 => SCRIPT_ID,
            1 => self.spare_programs[0],
            _ => self.pool_pick(tape),
        };
        let registrar = self.any_signer(tape);
        let uri = "m".repeat(tape.below(230));
        let mut ix = client::update_metadata(&registrar, &program, &uri);
        self.mutate_accounts(tape, &mut ix);
        Step {
            kind: Kind::UpdateMetadata,
            ix,
        }
    }

    fn gen_raw(&self, tape: &mut Tape) -> Step {
        const NAMES: [&str; 10] = [
            "initialize_config",
            "update_config",
            "register_strategy",
            "update_metadata",
            "create_market",
            "register_existing_market",
            "init_fee_account",
            "swap",
            "sweep_fees",
            "idl_write",
        ];
        let mut data = if tape.chance(85) {
            client::instruction_discriminator(tape.pick(&NAMES)).to_vec()
        } else {
            tape.bytes(8)
        };
        let len = tape.below(120);
        data.extend(tape.bytes(len));
        let n = tape.below(22);
        let accounts = (0..n)
            .map(|_| {
                if tape.chance(10) {
                    AccountMeta::new(self.any_signer(tape), true)
                } else {
                    let key = self.pool_pick(tape);
                    AccountMeta {
                        pubkey: key,
                        is_signer: false,
                        is_writable: tape.chance(50),
                    }
                }
            })
            .collect();
        Step {
            kind: Kind::Raw,
            ix: Instruction::new_with_bytes(client::PROGRAM_ID, &data, accounts),
        }
    }

    fn gen_direct_script(&self, tape: &mut Tape) -> Step {
        let market = tape.pick(&self.markets);
        let mut accounts = vec![AccountMeta::new(market.state, false)];
        accounts.extend(market.vaults.iter().map(|v| AccountMeta::new(*v, false)));
        accounts.extend(
            market
                .mints
                .iter()
                .map(|&m| AccountMeta::new(self.mints[m].key, false)),
        );
        accounts.push(AccountMeta::new_readonly(TOKEN, false));
        accounts.push(AccountMeta::new_readonly(TOKEN_2022, false));
        for &m in &market.mints {
            accounts.extend(self.mints[m].hook_accounts.iter().cloned());
        }
        accounts.extend(self.pool_metas(tape, 4));
        let mut data = script::RUN.to_vec();
        data.extend(market.nonce);
        let len = tape.below(32);
        data.extend(tape.bytes(len));
        Step {
            kind: Kind::DirectScript,
            ix: Instruction::new_with_bytes(SCRIPT_ID, &data, accounts),
        }
    }

    fn gen_trader_transfer(&self, tape: &mut Tape) -> Step {
        let t = tape.below(self.traders.len());
        let m = tape.below(self.mints.len());
        let mint = &self.mints[m];
        let from = self.trader_accounts[t][m];
        let to = if tape.chance(60) {
            let market = tape.pick(&self.markets);
            market
                .mints
                .iter()
                .position(|&x| x == m)
                .map(|i| market.vaults[i])
                .unwrap_or_else(|| self.pool_pick(tape))
        } else {
            self.pool_pick(tape)
        };
        let amount = tape.amount(10_000);
        let decimals = self.svm.get_account(&mint.key).unwrap().data[44];
        let mut ix = spl_token_2022_interface::instruction::transfer_checked(
            &mint.program,
            &from,
            &mint.key,
            &to,
            &self.traders[t].pubkey(),
            &[],
            amount,
            decimals,
        )
        .unwrap();
        ix.accounts.extend(mint.hook_accounts.iter().cloned());
        Step {
            kind: Kind::TraderTransfer { from, amount },
            ix,
        }
    }

    /// Something outside the router happens between transactions: time passes, the
    /// issuer changes mint 3's transfer fee, the refill hook's stash or a vault is topped
    /// up, or the router is frozen (which must change nothing).
    fn environment(&mut self, tape: &mut Tape) -> &'static str {
        let admin = self.admin.insecure_clone();
        match tape.u8() % 32 {
            0..=6 => {
                let mut clock = self.svm.get_sysvar::<solana_clock::Clock>();
                clock.slot += 1 + tape.u16() as u64 * 50;
                clock.epoch += (tape.u8() % 3) as u64;
                clock.unix_timestamp += tape.u16() as i64 * 20;
                self.svm.set_sysvar(&clock);
                "time passes"
            }
            7..=13 => {
                let bps = tape.u16() % 2_001;
                let max = tape.amount(5_000_000);
                let ix = spl_token_2022_interface::extension::transfer_fee::instruction::set_transfer_fee(
                    &TOKEN_2022,
                    &self.mints[3].key,
                    &admin.pubkey(),
                    &[],
                    bps,
                    max,
                )
                .unwrap();
                self.setup(&[ix], &[&admin]);
                "transfer fee changes"
            }
            14..=22 => {
                let ix = spl_token_interface::instruction::mint_to(
                    &TOKEN,
                    &self.mints[0].key,
                    &self.stash,
                    &admin.pubkey(),
                    &[],
                    1 + tape.amount(1_000_000) % TRADER_FUNDS,
                )
                .unwrap();
                self.setup(&[ix], &[&admin]);
                "stash topped up"
            }
            23..=30 => {
                let market = tape.pick(&self.markets);
                let i = tape.below(market.mints.len());
                let m = market.mints[i];
                // Mint 5's mint authority belongs to market 2's state.
                if m == 5 || self.svm.get_account(&market.vaults[i]).is_none() {
                    return "nothing";
                }
                let ix = spl_token_2022_interface::instruction::mint_to(
                    &self.mints[m].program,
                    &self.mints[m].key,
                    &market.vaults[i],
                    &admin.pubkey(),
                    &[],
                    tape.amount(VAULT_FUNDS),
                )
                .unwrap();
                self.svm.expire_blockhash();
                let tx = Transaction::new_signed_with_payer(
                    &[ix],
                    Some(&self.payer.pubkey()),
                    &[&self.payer, &admin],
                    self.svm.latest_blockhash(),
                );
                // May fail (a vault that is not a token account, or overflow): fine.
                let _ = self.svm.send_transaction(tx);
                "vault topped up"
            }
            _ => {
                set_upgrade_authority(&mut self.svm, &client::PROGRAM_ID, None);
                "router frozen"
            }
        }
    }

    fn config(&self) -> client::Config {
        client::decode_config(&self.svm.get_account(&pda::config()).unwrap().data).unwrap()
    }
}

// ------------------------------------------------------------------------------ oracle

#[derive(Clone, Debug, PartialEq, Eq)]
struct TokenState {
    mint: Pubkey,
    owner: Pubkey,
    amount: u64,
    delegate: Option<Pubkey>,
    frozen: bool,
}

fn token_state(account: &Account) -> Option<TokenState> {
    if account.owner != TOKEN && account.owner != TOKEN_2022 {
        return None;
    }
    let data = &account.data;
    let is_account = data.len() == 165 || (data.len() > 165 && data.len() != 355 && data[165] == 2);
    if !is_account {
        return None;
    }
    let key = |at: usize| Pubkey::new_from_array(data[at..at + 32].try_into().unwrap());
    Some(TokenState {
        mint: key(0),
        owner: key(32),
        amount: u64::from_le_bytes(data[64..72].try_into().unwrap()),
        delegate: (data[72] == 1).then(|| key(76)),
        frozen: data[108] == 2,
    })
}

type Snapshot = HashMap<Pubkey, Option<Account>>;

fn snapshot(svm: &LiteSVM, keys: &HashSet<Pubkey>) -> Snapshot {
    keys.iter()
        .map(|k| {
            let account = svm
                .get_account(k)
                .filter(|a| !a.executable && a.owner != LOADER_V3);
            (*k, account)
        })
        .collect()
}

fn balance(snap: &Snapshot, key: &Pubkey) -> Option<u64> {
    snap.get(key)?
        .as_ref()
        .and_then(token_state)
        .map(|t| t.amount)
}

fn lamports(snap: &Snapshot, key: &Pubkey) -> u64 {
    snap.get(key)
        .and_then(|a| a.as_ref())
        .map_or(0, |a| a.lamports)
}

fn ceil_bps(amount: u64, bps: u64) -> u128 {
    (amount as u128 * bps as u128).div_ceil(10_000)
}

/// What a mint's Token-2022 transfer fee (as configured for `epoch`) withholds from a
/// transfer of `amount`; 0 for a mint without one.
fn transfer_fee_cut(mint: Option<&Account>, epoch: u64, amount: u64) -> u64 {
    use spl_token_2022_interface::extension::{
        transfer_fee::TransferFeeConfig, BaseStateWithExtensions, StateWithExtensions,
    };
    let Some(mint) = mint.filter(|m| m.owner == TOKEN_2022) else {
        return 0;
    };
    let Ok(state) =
        StateWithExtensions::<spl_token_2022_interface::state::Mint>::unpack(&mint.data)
    else {
        return 0;
    };
    state
        .get_extension::<TransferFeeConfig>()
        .ok()
        .and_then(|config| config.calculate_epoch_fee(epoch, amount))
        .unwrap_or(0)
}

/// A mint's freeze authority (SPL layout: `COption<Pubkey>` at 46).
fn freeze_authority(mint: Option<&Account>) -> Option<Pubkey> {
    let data = &mint?.data;
    (data.get(46..50)? == [1, 0, 0, 0])
        .then(|| Pubkey::new_from_array(data[50..82].try_into().unwrap()))
}

/// Router events emitted by each top-level instruction.
fn events_by_instruction(
    tx: &Transaction,
    meta: &litesvm::types::TransactionMetadata,
) -> Vec<Vec<PipelineEvent>> {
    let keys = &tx.message.account_keys;
    let mut out = vec![vec![]; tx.message.instructions.len()];
    for (i, inner) in meta.inner_instructions.iter().enumerate() {
        for ix in inner {
            let program = keys[ix.instruction.program_id_index as usize];
            if program != client::PROGRAM_ID {
                continue;
            }
            if let Some(event) = pipeline_events::decode(&ix.instruction.data) {
                out[i].push(event);
            }
        }
    }
    out
}

struct Context<'a> {
    world: &'a World,
    steps: &'a [Step],
    signers: &'a HashSet<Pubkey>,
    before: &'a Snapshot,
    after: &'a Snapshot,
    market_rent: u64,
    epoch: u64,
    /// Top-level instructions before the steps (a compute-budget instruction).
    event_offset: usize,
    /// Known, accepted behaviours seen in this transaction.
    notes: std::cell::RefCell<Vec<String>>,
}

/// Checks every guarantee after a successful transaction.
fn check_success(
    ctx: &Context,
    tx: &Transaction,
    meta: &litesvm::types::TransactionMetadata,
) -> Result<(), String> {
    let world = ctx.world;
    let (before, after) = (ctx.before, ctx.after);
    let events = events_by_instruction(tx, meta).split_off(ctx.event_offset);
    let mint_account = |mint: &Pubkey| after.get(mint).and_then(|a| a.as_ref());
    let stash_authority = Pubkey::find_program_address(&[b"stash"], &REFILL_ID).0;
    // Did the refill hook move anything (a token transfer out of its stash)?
    let keys = &tx.message.account_keys;
    let refilled = meta.inner_instructions.iter().flatten().any(|inner| {
        let ix = &inner.instruction;
        keys[ix.program_id_index as usize] == TOKEN
            && ix.accounts.first().map(|&a| keys[a as usize]) == Some(world.stash)
    });
    let refill_target = world.trader_accounts[0][0];
    let config_before = before
        .get(&pda::config())
        .and_then(|a| a.as_ref())
        .and_then(|a| client::decode_config(&a.data))
        .ok_or("config missing before")?;
    let config_after = after
        .get(&pda::config())
        .and_then(|a| a.as_ref())
        .and_then(|a| client::decode_config(&a.data))
        .ok_or("config missing after")?;

    // Classify the accounts PDAs control.
    let mut fee_accounts: HashMap<Pubkey, Pubkey> = HashMap::new(); // fee account -> market
    let mut strategy_owned: HashSet<Pubkey> = HashSet::new();
    let mut market_pdas: HashSet<Pubkey> = HashSet::new();
    for (key, account) in before.iter().chain(after.iter()) {
        let Some(account) = account else { continue };
        if account.owner == client::PROGRAM_ID {
            if let Some(market) = client::decode_market(&account.data) {
                market_pdas.insert(*key);
                let fee_authority = pda::fee_authority(key);
                for mint in market.mints() {
                    for program in [TOKEN, TOKEN_2022] {
                        fee_accounts
                            .insert(pda::associated_token(&fee_authority, mint, &program), *key);
                    }
                }
                strategy_owned.insert(market.market_state);
            }
        }
        if account.owner == SCRIPT_ID {
            strategy_owned.insert(*key);
        }
    }
    let controlled_by_strategy = |owner: &Pubkey| {
        strategy_owned.contains(owner) || market_pdas.contains(owner) || *owner == SCRIPT_ID
    };
    // The refill hook moves its own stash during transfers of its mint.
    let controlled = |owner: &Pubkey| controlled_by_strategy(owner) || *owner == stash_authority;

    // Per-account spending allowances for signing traders.
    let mut allowance: HashMap<Pubkey, u128> = HashMap::new();
    let mut swept: HashSet<Pubkey> = HashSet::new();
    let mut min_fee_gain: HashMap<Pubkey, u128> = HashMap::new();
    let new_markets = events
        .iter()
        .flatten()
        .filter(|e| matches!(e, PipelineEvent::MarketCreated(_)))
        .count() as u64;
    let mut config_changed_by_ix = false;
    let mut metadata_updated = false;

    for (i, step) in ctx.steps.iter().enumerate() {
        let ix = &step.ix;
        let swap_events: Vec<_> = events[i]
            .iter()
            .filter_map(|e| match e {
                PipelineEvent::Swap(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        match &step.kind {
            Kind::Swap(args) => {
                let key = |at: usize| ix.accounts[at].pubkey;
                let [event] = swap_events.as_slice() else {
                    return Err(format!(
                        "swap {i} emitted {} swap events",
                        swap_events.len()
                    ));
                };
                if event.trader != key(swap_meta::TRADER)
                    || event.market != key(swap_meta::MARKET)
                    || event.input_mint != key(swap_meta::INPUT_MINT)
                    || event.output_mint != key(swap_meta::OUTPUT_MINT)
                    || event.mode != args.mode as u8
                {
                    return Err(format!("swap {i}: event names the wrong trade: {event:?}"));
                }
                let integrator =
                    Some(key(swap_meta::INTEGRATOR)).filter(|k| *k != client::PROGRAM_ID);
                if event.integrator != integrator {
                    return Err(format!(
                        "swap {i}: event integrator {:?} != {integrator:?}",
                        event.integrator
                    ));
                }
                if event.net_sent == 0 || event.net_sent > args.amount_in {
                    return Err(format!(
                        "swap {i}: net_sent {} outside 1..={}",
                        event.net_sent, args.amount_in
                    ));
                }
                if event.pipeline_fee as u128 != ceil_bps(event.net_sent, 25) {
                    return Err(format!(
                        "swap {i}: pipeline fee {} on net_sent {}",
                        event.pipeline_fee, event.net_sent
                    ));
                }
                if event.integrator_fee as u128
                    != ceil_bps(event.net_sent, args.integrator_fee_bps as u64)
                {
                    return Err(format!(
                        "swap {i}: integrator fee {} on net_sent {} at {} bps",
                        event.integrator_fee, event.net_sent, args.integrator_fee_bps
                    ));
                }
                if event.integrator_fee > 0 && integrator.is_none() {
                    return Err(format!("swap {i}: integrator fee without an integrator"));
                }
                if event.received < args.min_amount_out {
                    return Err(format!(
                        "swap {i}: received {} < min {}",
                        event.received, args.min_amount_out
                    ));
                }
                // Fees go to the market's own fee account for the input mint.
                let input_program = after
                    .get(&event.input_mint)
                    .and_then(|a| a.as_ref())
                    .map(|a| a.owner)
                    .ok_or("input mint missing")?;
                let expected = pda::fee_account(&event.market, &event.input_mint, &input_program);
                if key(swap_meta::FEE_ACCOUNT) != expected {
                    return Err(format!(
                        "swap {i}: fee paid to {} instead of the market's fee account {expected}",
                        key(swap_meta::FEE_ACCOUNT)
                    ));
                }
                let market_record = before
                    .get(&event.market)
                    .and_then(|a| a.as_ref())
                    .and_then(|a| client::decode_market(&a.data))
                    .ok_or("swap through an unknown market")?;
                if market_record.vault_for(&event.input_mint) != Some(key(swap_meta::INPUT_VAULT))
                    || !market_record.mints().contains(&event.output_mint)
                    || key(swap_meta::STRATEGY) != market_record.strategy_program
                {
                    return Err(format!(
                        "swap {i}: accounts don't match market {}",
                        event.market
                    ));
                }
                let input = key(swap_meta::TRADER_INPUT);
                let fee_account = key(swap_meta::FEE_ACCOUNT);
                let max_spend = args.amount_in as u128
                    + ceil_bps(args.amount_in, 25)
                    + ceil_bps(args.amount_in, args.integrator_fee_bps as u64);
                *allowance.entry(input).or_default() += max_spend;

                let input_mint = event.input_mint;
                let cut = |fee: u64| {
                    fee.saturating_sub(transfer_fee_cut(mint_account(&input_mint), ctx.epoch, fee))
                };
                *min_fee_gain.entry(fee_account).or_default() += cut(event.pipeline_fee) as u128;
                if let Some(integrator) = integrator {
                    *min_fee_gain.entry(integrator).or_default() +=
                        cut(event.integrator_fee) as u128;
                }

                // Exact accounting when the swap is alone in its transaction.
                if ctx.steps.len() == 1 {
                    let output = key(swap_meta::TRADER_OUTPUT);
                    let out_before =
                        balance(before, &output).ok_or("output not a token account")?;
                    let out_after = balance(after, &output).ok_or("output closed")?;
                    let moved = out_after as i128 - out_before as i128;
                    let output_mint = event.output_mint;
                    let delegated = world
                        .mints
                        .iter()
                        .any(|m| m.key == output_mint && m.permanent_delegate);
                    let refilled_after =
                        refilled && output == refill_target && moved > event.received as i128;
                    if refilled_after {
                        // The refill hook topped up the output account during the
                        // router's fee transfers, after `received` was measured.
                        ctx.notes
                            .borrow_mut()
                            .push("a hook topped up the output after measurement".into());
                    } else if moved < 0 && event.received == 0 && delegated {
                        // Known: a permanent delegate on the market PDA lets the strategy
                        // take the trader's output balance; `received` floors at 0.
                        ctx.notes
                            .borrow_mut()
                            .push("output balance fell through a permanent delegate".into());
                    } else if moved < 0
                        && event.received == 0
                        && before
                            .get(&output)
                            .and_then(|a| a.as_ref())
                            .and_then(token_state)
                            .is_some_and(|t| controlled(&t.owner))
                    {
                        // The "trader's output account" is one the strategy controls (a
                        // substituted vault), so it can take from it; `received` floors
                        // at 0. Nobody but the strategy loses.
                        ctx.notes
                            .borrow_mut()
                            .push("output account is the strategy's and fell".into());
                    } else if moved != event.received as i128 {
                        return Err(format!(
                            "swap {i}: trader output moved {moved} but the event says received {}",
                            event.received
                        ));
                    }
                    let in_before = balance(before, &input).ok_or("input not a token account")?;
                    let in_after = balance(after, &input).ok_or("input closed")?;
                    let lost = in_before as i128 - in_after as i128;
                    let mut fees = event.pipeline_fee as i128;
                    if integrator != Some(input) {
                        fees += event.integrator_fee as i128;
                    }
                    let spent_in_call = lost - fees;
                    if spent_in_call > event.net_sent as i128 {
                        return Err(format!(
                            "swap {i}: trader parted with {spent_in_call} before fees but was charged on net_sent {}",
                            event.net_sent
                        ));
                    }
                    // Rebuild the fee base from outside: min(amount_in, max(what the
                    // trader's input lost during the call, what the input vault kept)).
                    // Only the router's fee transfers touch these accounts after the
                    // call, so the transaction's deltas equal the router's measurements
                    // when the vault is none of the fee accounts.
                    let vault = key(swap_meta::INPUT_VAULT);
                    let distinct = vault != fee_account
                        && Some(vault) != integrator
                        && vault != input
                        && vault != output;
                    if distinct {
                        let kept = balance(after, &vault).unwrap_or(0) as i128
                            - balance(before, &vault).unwrap_or(0) as i128;
                        let expected = (args.amount_in as i128).min(spent_in_call.max(kept).max(0));
                        if expected != event.net_sent as i128 {
                            return Err(format!(
                                "swap {i}: net_sent {} but the trader's input lost {spent_in_call} and the vault kept {kept} (amount_in {})",
                                event.net_sent, args.amount_in
                            ));
                        }
                    }
                    if meta.return_data.program_id == client::PROGRAM_ID {
                        let result = client::SwapResult::try_from_slice(&meta.return_data.data)
                            .map_err(|_| "swap return data does not decode")?;
                        if (
                            result.net_sent,
                            result.received,
                            result.pipeline_fee,
                            result.integrator_fee,
                        ) != (
                            event.net_sent,
                            event.received,
                            event.pipeline_fee,
                            event.integrator_fee,
                        ) {
                            return Err(format!(
                                "swap {i}: return data {result:?} != event {event:?}"
                            ));
                        }
                    } else {
                        return Err(format!(
                            "swap {i}: return data is from {}",
                            meta.return_data.program_id
                        ));
                    }
                }
            }
            Kind::Sweep => {
                swept.insert(ix.accounts[4].pubkey);
                for event in &events[i] {
                    if let PipelineEvent::FeesSwept(e) = event {
                        let fee_account = ix.accounts[4].pubkey;
                        let treasury = ix.accounts[5].pubkey;
                        if treasury == fee_account {
                            // The admin made the fee authority the treasury owner: the
                            // sweep moves the fees to themselves.
                            ctx.notes
                                .borrow_mut()
                                .push("a sweep into the fee account itself".into());
                        } else if ctx.steps.len() == 1 {
                            let lost = balance(before, &fee_account).unwrap_or(0) as i128
                                - balance(after, &fee_account).unwrap_or(0) as i128;
                            let gained = balance(after, &treasury).unwrap_or(0) as i128
                                - balance(before, &treasury).unwrap_or(0) as i128;
                            if lost != e.amount as i128 || gained > lost || gained < 0 {
                                return Err(format!("sweep {i}: fee account lost {lost}, treasury gained {gained}, event {}", e.amount));
                            }
                        }
                        let owner = after
                            .get(&treasury)
                            .and_then(|a| a.as_ref())
                            .and_then(token_state)
                            .map(|t| t.owner);
                        if owner != Some(config_before.treasury_owner)
                            && owner != Some(config_after.treasury_owner)
                        {
                            return Err(format!("sweep {i}: fees went to {treasury}, owned by {owner:?}, not the treasury"));
                        }
                    }
                }
            }
            Kind::CreateMarket {
                max_creator_lamports,
            } => {
                let creator = ix.accounts[0].pubkey;
                let lost = lamports(before, &creator) as i128 - lamports(after, &creator) as i128;
                let limit = *max_creator_lamports as i128 + ctx.market_rent as i128;
                if lost > limit {
                    return Err(format!(
                        "create_market {i}: creator lost {lost} lamports, limit {limit}"
                    ));
                }
                let market = ix.accounts[4].pubkey;
                let setup_payer = pda::setup_payer(&market);
                if lamports(after, &setup_payer) != 0 {
                    return Err(format!(
                        "create_market {i}: setup payer kept {} lamports",
                        lamports(after, &setup_payer)
                    ));
                }
            }
            Kind::RegisterExisting => {}
            Kind::UpdateConfig => {
                if ix.accounts[0].pubkey != config_before.admin {
                    return Err(format!(
                        "update_config {i} succeeded for non-admin {}",
                        ix.accounts[0].pubkey
                    ));
                }
                config_changed_by_ix = true;
            }
            Kind::UpdateMetadata => metadata_updated = true,
            Kind::TraderTransfer { from, amount } => {
                *allowance.entry(*from).or_default() += *amount as u128;
            }
            // Router instructions with arbitrary data and accounts: judged by their
            // effects below, like any other instruction.
            Kind::Raw => {
                config_changed_by_ix = true;
                metadata_updated = true;
            }
            Kind::InitFee | Kind::RegisterStrategy | Kind::DirectScript => {}
        }
    }

    // Events describe what the instructions did.
    let program_authority = |program: &Pubkey| {
        world
            .svm
            .get_account(&pda::program_data(program))
            .and_then(|a| match a.data.get(12) {
                Some(1) => Some(Pubkey::new_from_array(a.data[13..45].try_into().unwrap())),
                _ => None,
            })
    };
    let config_events: Vec<_> = events
        .iter()
        .flatten()
        .filter_map(|e| match e {
            PipelineEvent::ConfigChanged(c) => Some(c.clone()),
            _ => None,
        })
        .collect();
    if let Some(last) = config_events.last() {
        let reported = (
            last.admin,
            last.listing_fee_lamports,
            last.listing_fee_recipient,
            last.treasury_owner,
        );
        let actual = (
            config_after.admin,
            config_after.listing_fee_lamports,
            config_after.listing_fee_recipient,
            config_after.treasury_owner,
        );
        if reported != actual {
            return Err(format!(
                "ConfigChanged {reported:?} but the config is {actual:?}"
            ));
        }
    }
    for (i, step) in ctx.steps.iter().enumerate() {
        for event in &events[i] {
            match event {
                PipelineEvent::MarketCreated(e) => {
                    let market = after
                        .get(&e.market)
                        .and_then(|a| a.as_ref())
                        .and_then(|a| client::decode_market(&a.data))
                        .ok_or(format!(
                            "MarketCreated for {} but no market account",
                            e.market
                        ))?;
                    if market.market_state != e.market_state
                        || market.strategy_program != e.strategy
                        || market.mints() != e.mints.as_slice()
                        || market.created_by != e.created_by
                        || e.created_by != step.ix.accounts[0].pubkey
                    {
                        return Err(format!("MarketCreated {e:?} doesn't match {market:?}"));
                    }
                }
                PipelineEvent::StrategyRegistered(e) => {
                    let registrar = step.ix.accounts[1].pubkey;
                    if program_authority(&e.strategy) != Some(registrar)
                        && pda::register(&e.strategy) != registrar
                    {
                        return Err(format!(
                            "strategy {} registered by {registrar}, neither its upgrade authority nor its registration PDA",
                            e.strategy
                        ));
                    }
                    if config_events.is_empty() && e.fee_paid != config_before.listing_fee_lamports
                    {
                        return Err(format!(
                            "registration paid {} but the listing fee is {}",
                            e.fee_paid, config_before.listing_fee_lamports
                        ));
                    }
                    let record = after
                        .get(&pda::strategy(&e.strategy))
                        .and_then(|a| a.as_ref())
                        .and_then(|a| client::decode_strategy(&a.data))
                        .ok_or("StrategyRegistered without a strategy account")?;
                    if record.program_id != e.strategy
                        || record.metadata_uri != e.metadata_uri
                        || record.market_count != 0
                    {
                        return Err(format!(
                            "StrategyRegistered {e:?} but the record is {record:?}"
                        ));
                    }
                }
                PipelineEvent::MetadataUpdated(e) => {
                    let registrar = step.ix.accounts[0].pubkey;
                    if program_authority(&e.strategy) != Some(registrar)
                        && pda::register(&e.strategy) != registrar
                    {
                        return Err(format!(
                            "metadata of {} updated by {registrar}, neither its upgrade authority nor its registration PDA",
                            e.strategy
                        ));
                    }
                }
                _ => {}
            }
        }
    }

    // The config changes only through an instruction its admin signed, and the unused
    // byte where the pause flag used to be stays 0.
    if config_after.reserved != 0 {
        return Err(format!("config reserved byte is {}", config_after.reserved));
    }
    if config_after != config_before {
        if !config_changed_by_ix {
            return Err(format!(
                "config changed without its instruction: {config_before:?} -> {config_after:?}"
            ));
        }
        let by_admin = ctx.steps.iter().any(|s| {
            s.ix.accounts
                .first()
                .is_some_and(|a| a.is_signer && a.pubkey == config_before.admin)
        });
        if !by_admin {
            return Err(format!(
                "config changed without its admin: {config_before:?} -> {config_after:?}"
            ));
        }
    }

    for (key, old) in before {
        let new = after.get(key).cloned().flatten();
        let old_lamports = old.as_ref().map_or(0, |a| a.lamports);
        let new_lamports = new.as_ref().map_or(0, |a| a.lamports);

        // Router accounts.
        if let Some(old) = old.as_ref().filter(|a| a.owner == client::PROGRAM_ID) {
            let new = new
                .as_ref()
                .ok_or(format!("router account {key} vanished"))?;
            if new.owner != client::PROGRAM_ID || new_lamports < old_lamports {
                return Err(format!("router account {key} lost lamports or ownership"));
            }
            if client::decode_market(&old.data).is_some() && new.data != old.data {
                return Err(format!("market {key} changed"));
            }
            if let (Some(a), Some(b)) = (
                client::decode_strategy(&old.data),
                client::decode_strategy(&new.data),
            ) {
                if a.program_id != b.program_id
                    || a.registered_at != b.registered_at
                    || a.bump != b.bump
                    || b.market_count < a.market_count
                    || b.market_count - a.market_count > new_markets
                    || (a.metadata_uri != b.metadata_uri && !metadata_updated)
                {
                    return Err(format!(
                        "strategy {key} changed unexpectedly: {a:?} -> {b:?}"
                    ));
                }
            }
            continue;
        }

        // Wallets: nobody spends a non-signer's SOL.
        if let Some(old) = old
            .as_ref()
            .filter(|a| a.owner == client::SYSTEM_PROGRAM_ID)
        {
            let is_setup_payer =
                old.data.is_empty() && market_pdas.iter().any(|m| pda::setup_payer(m) == *key);
            if !ctx.signers.contains(key) && !is_setup_payer && new_lamports < old_lamports {
                return Err(format!(
                    "non-signer {key} lost {} lamports",
                    old_lamports - new_lamports
                ));
            }
            // In a transaction that only swaps, a trader's SOL is never touched.
            let only_swaps = ctx.steps.iter().all(|s| matches!(s.kind, Kind::Swap(_)));
            if only_swaps
                && world.traders.iter().any(|t| t.pubkey() == *key)
                && new_lamports < old_lamports
            {
                return Err(format!(
                    "trader {key} lost {} lamports",
                    old_lamports - new_lamports
                ));
            }
            continue;
        }

        // Token accounts.
        let Some(old_token) = old.as_ref().and_then(token_state) else {
            continue;
        };
        let mint = world.mints.iter().find(|m| m.key == old_token.mint);
        if mint.is_some_and(|m| m.permanent_delegate) {
            // Its permanent delegate (a market PDA) can move any holder's balance; the
            // router only bounds the trader's input (checked by the allowance below for
            // single swaps through the trade's own accounts).
            continue;
        }
        let new_token = new.as_ref().and_then(token_state);
        if new_token
            .as_ref()
            .is_some_and(|t| t.frozen && !old_token.frozen)
        {
            // Only the mint's freeze authority can freeze an account.
            let authority = freeze_authority(mint_account(&old_token.mint));
            if authority.is_some_and(|a| controlled_by_strategy(&a)) {
                ctx.notes
                    .borrow_mut()
                    .push("an account was frozen by a strategy-held freeze authority".into());
            } else {
                return Err(format!("token account {key} was frozen by {authority:?}"));
            }
        }
        if controlled(&old_token.owner) || old_token.delegate.is_some_and(|d| controlled(&d)) {
            continue;
        }
        let new_amount = new_token.as_ref().map_or(0, |t| t.amount);
        if let Some(market) = fee_accounts.get(key) {
            if pda::fee_authority(market) == old_token.owner {
                if new_token.as_ref().map(|t| (t.owner, t.delegate))
                    != Some((old_token.owner, None))
                {
                    return Err(format!("fee account {key} closed, reassigned or delegated"));
                }
                if new_amount < old_token.amount && !swept.contains(key) {
                    return Err(format!(
                        "fee account {key} lost {} without a sweep",
                        old_token.amount - new_amount
                    ));
                }
                if !swept.contains(key) {
                    let need = min_fee_gain.get(key).copied().unwrap_or(0);
                    if (new_amount as i128 - old_token.amount as i128) < need as i128 {
                        return Err(format!(
                            "fee account {key} gained {} but swaps charged {need}",
                            new_amount as i128 - old_token.amount as i128
                        ));
                    }
                }
                continue;
            }
        }
        if new_token.as_ref().map(|t| (t.owner, t.delegate))
            != Some((old_token.owner, old_token.delegate))
        {
            if !ctx.signers.contains(&old_token.owner) {
                return Err(format!(
                    "token account {key} of non-signer {} closed or re-authorised",
                    old_token.owner
                ));
            }
            continue;
        }
        if new_amount >= old_token.amount {
            continue;
        }
        let lost = (old_token.amount - new_amount) as u128;
        if !ctx.signers.contains(&old_token.owner) {
            return Err(format!(
                "{key} of non-signer {} lost {lost}",
                old_token.owner
            ));
        }
        let allowed = allowance.get(key).copied().unwrap_or(0);
        if lost > allowed {
            return Err(format!(
                "{key} of signer {} lost {lost}, allowed {allowed}",
                old_token.owner
            ));
        }
    }

    // New markets are well formed.
    for (key, new) in after {
        let (Some(new), None) = (new, before.get(key).cloned().flatten()) else {
            continue;
        };
        if new.owner != client::PROGRAM_ID {
            continue;
        }
        let Some(market) = client::decode_market(&new.data) else {
            continue;
        };
        let mints = market.mints();
        let unique: HashSet<_> = mints.iter().collect();
        if !(2..=4).contains(&mints.len()) || unique.len() != mints.len() {
            return Err(format!("new market {key} has mints {mints:?}"));
        }
        if pda::market(&market.strategy_program, &market.market_state) != *key {
            return Err(format!("new market {key} is not at its PDA"));
        }
        let state_owner = world.svm.get_account(&market.market_state).map(|a| a.owner);
        if state_owner != Some(market.strategy_program) {
            return Err(format!(
                "new market {key}'s state is owned by {state_owner:?}"
            ));
        }
        let fee_bump = Pubkey::find_program_address(
            &[psi::seeds::FEE_AUTHORITY, key.as_ref()],
            &client::PROGRAM_ID,
        )
        .1;
        if market.fee_authority_bump != fee_bump {
            return Err(format!(
                "new market {key} has fee bump {}",
                market.fee_authority_bump
            ));
        }
    }
    Ok(())
}

/// The router must never panic or fault, whatever it is sent: the first program to
/// fail must not be the router failing on its own. Returns a note for the one known,
/// harmless panic: borsh preallocates a `Vec<u8>`/`String` argument's declared length (up
/// to 1 MiB), so a huge length prefix in instruction data exhausts the 32 KiB heap while
/// the arguments are decoded, before the handler runs.
fn check_failure(logs: &[String]) -> Result<Option<String>, String> {
    let router = format!("Program {} ", client::PROGRAM_ID);
    let Some(fail_at) = logs.iter().position(|l| l.contains(" failed: ")) else {
        return Ok(None);
    };
    let line = &logs[fail_at];
    let faults = [
        "panicked",
        "Access violation",
        "memory",
        "Unsupported",
        "divide",
    ];
    if !line.starts_with(&router) || !faults.iter().any(|f| line.contains(f)) {
        return Ok(None);
    }
    let start = logs[..fail_at]
        .iter()
        .rposition(|l| l.starts_with(&router) && l.contains(" invoke ["))
        .unwrap_or(0);
    let handler_ran = logs[start + 1..fail_at]
        .iter()
        .any(|l| l.contains(" invoke ["));
    let decoding_oom = logs[start..fail_at]
        .iter()
        .any(|l| l.contains("memory allocation failed"));
    if decoding_oom && !handler_ran {
        return Ok(Some(
            "argument length prefix exhausted the heap while decoding".into(),
        ));
    }
    Err(format!("router faulted: {line}"))
}

// -------------------------------------------------------------------------------- run

#[derive(Default)]
struct Stats {
    /// (step kinds, outcome) -> count.
    outcomes: BTreeMap<String, u64>,
    transactions: u64,
    oversize: u64,
}

fn error_name(logs: &[String], err: &str) -> String {
    logs.iter()
        .find_map(|l| {
            l.split("Error Code: ")
                .nth(1)
                .map(|s| s.split('.').next().unwrap_or(s).to_string())
        })
        .unwrap_or_else(|| {
            // "InstructionError(1, Custom(1))" -> "InstructionError(Custom(1))".
            match err.strip_prefix("InstructionError(") {
                Some(rest) => {
                    let inner = rest.split_once(", ").map_or(rest, |(_, e)| e);
                    format!("InstructionError({}", inner)
                }
                None => err.split(' ').next().unwrap_or(err).to_string(),
            }
        })
}

fn run_case(base: &World, tape_bytes: &[u8], stats: &mut Stats, trace: bool) -> Result<(), String> {
    let mut world = base.fork();
    let mut tape = Tape {
        bytes: tape_bytes,
        pos: 0,
    };
    let market_rent = world.svm.minimum_balance_for_rent_exemption(8 + 364);
    for step_no in 0..MAX_STEPS {
        if tape.done() {
            break;
        }
        if tape.chance(4) {
            let event = world.environment(&mut tape);
            *stats
                .outcomes
                .entry(format!("environment: {event}"))
                .or_default() += 1;
            if trace {
                eprintln!("step {step_no}: environment: {event}");
            }
            continue;
        }
        let mut steps = vec![];
        let mut pending = None;
        let mut extra_signers = vec![];
        if tape.chance(12) {
            steps.push(if tape.chance(50) {
                world.gen_direct_script(&mut tape)
            } else {
                world.gen_trader_transfer(&mut tape)
            });
        }
        let main = match tape.u8() % 100 {
            0..=54 => world.gen_swap(&mut tape),
            55..=62 => world.gen_sweep(&mut tape),
            63..=66 => world.gen_init_fee(&mut tape),
            67..=74 => {
                let (step, p, signers) = world.gen_create_market(&mut tape);
                pending = p;
                extra_signers.extend(signers);
                step
            }
            75..=78 => world.gen_register_existing(&mut tape),
            79..=84 => world.gen_update_config(&mut tape),
            85..=87 => world.gen_swap(&mut tape),
            88..=90 => world.gen_register_strategy(&mut tape),
            91 | 92 => world.gen_update_metadata(&mut tape),
            _ => world.gen_raw(&mut tape),
        };
        steps.push(main);
        if tape.chance(14) {
            steps.push(match tape.u8() % 3 {
                0 => world.gen_sweep(&mut tape),
                1 => world.gen_direct_script(&mut tape),
                _ => world.gen_swap(&mut tape),
            });
        }

        // A market about to be created needs its vaults in place to trade later.
        if let Some(p) = &pending {
            let payer = world.payer.pubkey();
            let mut ixs = vec![];
            for (&m, vault) in p.market.mints.iter().zip(&p.market.vaults) {
                if *vault
                    == pda::associated_token(
                        &p.market.state,
                        &world.mints[m].key,
                        &world.mints[m].program,
                    )
                {
                    ixs.push(spl_associated_token_account_interface::instruction::create_associated_token_account_idempotent(
                        &payer,
                        &p.market.state,
                        &world.mints[m].key,
                        &world.mints[m].program,
                    ));
                }
            }
            if !ixs.is_empty() {
                world.setup(&ixs, &[]);
            }
        }

        // Signers: everyone marked as signing whose key we hold.
        let mut signers: HashSet<Pubkey> = HashSet::new();
        let mut keypairs: Vec<Keypair> = vec![world.payer.insecure_clone()];
        signers.insert(world.payer.pubkey());
        let mut unsignable = false;
        for step in &steps {
            for meta in step.ix.accounts.iter().filter(|m| m.is_signer) {
                if signers.contains(&meta.pubkey) {
                    continue;
                }
                let key = world
                    .keypair(&meta.pubkey)
                    .map(|k| k.insecure_clone())
                    .or_else(|| {
                        extra_signers
                            .iter()
                            .find(|k| k.pubkey() == meta.pubkey)
                            .map(|k| k.insecure_clone())
                    });
                match key {
                    Some(k) => {
                        signers.insert(k.pubkey());
                        keypairs.push(k);
                    }
                    None => unsignable = true,
                }
            }
        }
        if unsignable {
            continue;
        }

        let mut watch: HashSet<Pubkey> = world.pool.iter().copied().collect();
        for step in &steps {
            watch.extend(step.ix.accounts.iter().map(|m| m.pubkey));
            if matches!(
                step.kind,
                Kind::CreateMarket { .. } | Kind::RegisterExisting
            ) {
                let market = step.ix.accounts[4].pubkey;
                watch.insert(market);
                watch.insert(pda::setup_payer(&market));
            }
        }
        for t in &world.traders {
            watch.insert(t.pubkey());
        }
        // The fee payer pays even for failed transactions.
        watch.remove(&world.payer.pubkey());
        let before = snapshot(&world.svm, &watch);
        world.svm.expire_blockhash();
        let mut ixs: Vec<Instruction> = vec![];
        if tape.chance(30) {
            // SetComputeUnitLimit(1,400,000), so deep scripts and hooks can finish.
            let mut data = vec![2u8];
            data.extend_from_slice(&1_400_000u32.to_le_bytes());
            ixs.push(Instruction::new_with_bytes(
                COMPUTE_BUDGET_ID,
                &data,
                vec![],
            ));
        }
        let event_offset = ixs.len();
        let epoch = world.svm.get_sysvar::<solana_clock::Clock>().epoch;
        ixs.extend(steps.iter().map(|s| s.ix.clone()));
        let refs: Vec<&Keypair> = keypairs.iter().collect();
        let tx = Transaction::new_signed_with_payer(
            &ixs,
            Some(&world.payer.pubkey()),
            &refs,
            world.svm.latest_blockhash(),
        );
        let size = bincode::serialize(&tx).map_or(usize::MAX, |b| b.len());
        if size > MAX_TRANSACTION_SIZE {
            stats.oversize += 1;
            continue;
        }
        stats.transactions += 1;
        // Swaps are labelled with their market: `@3` for the world's market 3, `@new` for
        // one created during the case, `@?` for a substituted market account.
        let label = steps
            .iter()
            .map(|s| match s.kind {
                Kind::Swap(_) => {
                    let market = s.ix.accounts[swap_meta::MARKET].pubkey;
                    match world.markets.iter().position(|m| m.market == market) {
                        Some(i) if i < base.markets.len() => format!("swap@{i}"),
                        Some(_) => "swap@new".to_string(),
                        None => "swap@?".to_string(),
                    }
                }
                _ => s.kind.name().to_string(),
            })
            .collect::<Vec<_>>()
            .join("+");
        let result = world.svm.send_transaction(tx.clone());
        let after = snapshot(&world.svm, &watch);
        let ctx = Context {
            world: &world,
            steps: &steps,
            signers: &signers,
            before: &before,
            after: &after,
            market_rent,
            epoch,
            event_offset,
            notes: Default::default(),
        };
        let verdict = match &result {
            Ok(meta) => {
                *stats.outcomes.entry(format!("{label}: ok")).or_default() += 1;
                check_success(&ctx, &tx, meta).map_err(|e| (e, meta.logs.clone()))
            }
            Err(failed) => {
                let name = error_name(&failed.meta.logs, &format!("{:?}", failed.err));
                *stats
                    .outcomes
                    .entry(format!("{label}: {name}"))
                    .or_default() += 1;
                if after != before {
                    Err((
                        "a failed transaction changed state".into(),
                        failed.meta.logs.clone(),
                    ))
                } else {
                    match check_failure(&failed.meta.logs) {
                        Ok(note) => {
                            ctx.notes.borrow_mut().extend(note);
                            Ok(())
                        }
                        Err(e) => Err((e, failed.meta.logs.clone())),
                    }
                }
            }
        };
        for note in ctx.notes.take() {
            *stats.outcomes.entry(format!("note: {note}")).or_default() += 1;
        }
        if trace {
            let outcome = match &result {
                Ok(_) => "ok".to_string(),
                Err(f) => error_name(&f.meta.logs, &format!("{:?}", f.err)),
            };
            eprintln!("step {step_no}: {label} -> {outcome}");
            for step in &steps {
                eprintln!("    {:?}", step.kind);
            }
            if std::env::var("FUZZ_LOGS").is_ok_and(|f| label.contains(&f)) {
                let logs = match &result {
                    Ok(meta) => &meta.logs,
                    Err(failed) => &failed.meta.logs,
                };
                eprintln!("{}", logs.join("\n"));
            }
        }
        if let Err((message, logs)) = verdict {
            let detail: Vec<String> = steps
                .iter()
                .map(|s| format!("{:?}\n{:?}", s.kind, s.ix))
                .collect();
            return Err(format!(
                "step {step_no} ({label}): {message}\n--- steps\n{}\n--- logs\n{}",
                detail.join("\n"),
                logs.join("\n")
            ));
        }
        if result.is_ok() {
            if let Some(p) = pending {
                // Fund the new market's vaults and fee accounts so later steps can trade.
                let payer = world.payer.pubkey();
                let mut ixs = vec![];
                for &m in &p.market.mints {
                    ixs.push(client::init_fee_account(
                        &payer,
                        &p.market.market,
                        &world.mints[m].key,
                        &world.mints[m].program,
                    ));
                }
                world.setup(&ixs, &[]);
                world.add_market_to_pool(&p.market);
                world.markets.push(p.market);
            }
        }
    }
    Ok(())
}

/// The base world, built once per test thread (LiteSVM is not `Sync`).
fn world() -> &'static World {
    thread_local! {
        static WORLD: &'static World = Box::leak(Box::new(build_world()));
    }
    WORLD.with(|w| *w)
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        failure_persistence: None,
        max_shrink_iters: 4_000,
        ..ProptestConfig::default()
    })]

    #[test]
    fn router_fuzz(tape in proptest::collection::vec(any::<u8>(), 0..2_500)) {
        let mut stats = Stats::default();
        if let Err(e) = run_case(world(), &tape, &mut stats, false) {
            return Err(TestCaseError::fail(format!("{e}\ntape: {}", hex(&tape))));
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    let s = s.trim();
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).expect("hex tape"))
        .collect()
}

/// A long campaign. Runs `FUZZ_THREADS` threads (default: half the CPUs) for
/// `FUZZ_SECONDS` (default 60) or `FUZZ_CASES` cases in total, from `FUZZ_SEED`.
///
/// Each thread keeps a corpus of tapes that produced an outcome (instruction mix and
/// result) it had not seen before, and spends most cases mutating those (byte flips,
/// splices, inserted, deleted and duplicated chunks) rather than starting from scratch,
/// so it digs into states that took several steps to reach. A violation does not stop
/// the campaign: each distinct one (addresses and numbers ignored) is written, with its
/// tape, to `FUZZ_OUT` (default `$TMPDIR/pipeline-router-fuzz`) and reported at the end,
/// where the test fails if there were any. Progress is printed every `FUZZ_PROGRESS`
/// seconds (default 300). `FUZZ_TAPE=<hex>` replays one tape with a trace instead
/// (`FUZZ_LOGS=<label substring>` adds transaction logs).
#[test]
#[ignore = "long-running; run explicitly"]
fn router_fuzz_campaign() {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};

    let var = |name: &str| std::env::var(name).ok();
    let number = |name: &str| var(name).and_then(|v| v.parse::<u64>().ok());
    if let Some(tape) = var("FUZZ_TAPE") {
        let base = build_world();
        let mut stats = Stats::default();
        if let Err(e) = run_case(&base, &unhex(&tape), &mut stats, true) {
            panic!("{e}");
        }
        return;
    }
    let seed = number("FUZZ_SEED").unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
    });
    let seconds = number("FUZZ_SECONDS").unwrap_or(60);
    let max_cases = number("FUZZ_CASES").unwrap_or(u64::MAX);
    let progress = number("FUZZ_PROGRESS").unwrap_or(300);
    let cpus = std::thread::available_parallelism().map_or(2, |n| n.get());
    let threads = number("FUZZ_THREADS").unwrap_or((cpus / 2).max(1) as u64) as usize;
    let out = var("FUZZ_OUT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("pipeline-router-fuzz"));
    std::fs::create_dir_all(&out).expect("FUZZ_OUT");
    eprintln!(
        "seed {seed}, {threads} threads, {seconds} s, findings in {}",
        out.display()
    );

    struct Finding {
        message: String,
        tape: String,
        count: u64,
    }
    #[derive(Default)]
    struct Shared {
        cases: AtomicU64,
        transactions: AtomicU64,
        corpus: AtomicU64,
        findings: Mutex<BTreeMap<String, Finding>>,
        stats: Mutex<Stats>,
    }
    let shared = Arc::new(Shared::default());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(seconds);

    let workers: Vec<_> = (0..threads)
        .map(|t| {
            let shared = shared.clone();
            let out = out.clone();
            std::thread::Builder::new()
                .stack_size(64 << 20)
                .spawn(move || {
                    let base = build_world();
                    let mut rng =
                        Rng(seed ^ (t as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
                    let mut corpus: Vec<Vec<u8>> = vec![];
                    let mut seen: HashSet<String> = HashSet::new();
                    let mut local = Stats::default();
                    let mut local_cases = 0u64;
                    while std::time::Instant::now() < deadline
                        && shared.cases.load(Ordering::Relaxed) < max_cases
                    {
                        let tape = if corpus.is_empty() || rng.below(100) < 30 {
                            {
                                let n = rng.below(4_000) as usize;
                                rng.bytes(n)
                            }
                        } else {
                            mutate(&mut rng, &corpus)
                        };
                        let mut case = Stats::default();
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            run_case(&base, &tape, &mut case, false)
                        }))
                        .unwrap_or_else(|panic| {
                            let message = panic
                                .downcast_ref::<String>()
                                .cloned()
                                .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                                .unwrap_or_default();
                            Err(format!("harness panicked: {message}"))
                        });
                        let fresh: Vec<String> = case
                            .outcomes
                            .keys()
                            .filter(|k| !seen.contains(*k))
                            .cloned()
                            .collect();
                        if !fresh.is_empty() {
                            seen.extend(fresh);
                            if corpus.len() < 4_096 {
                                corpus.push(tape.clone());
                            } else {
                                let i = rng.below(corpus.len() as u64) as usize;
                                corpus[i] = tape.clone();
                            }
                        }
                        local.merge(case);
                        local_cases += 1;
                        if let Err(message) = result {
                            let key = finding_key(&message);
                            let mut findings = shared.findings.lock().unwrap();
                            let n = findings.len();
                            let entry = findings.entry(key).or_insert_with(|| {
                                let tape = hex(&tape);
                                let path = out.join(format!("finding-{n}.txt"));
                                let _ = std::fs::write(
                                    &path,
                                    format!("{message}\n\nFUZZ_TAPE={tape}\n"),
                                );
                                eprintln!(
                                    "FINDING {n} (thread {t}): {}\n  saved to {}",
                                    message.lines().next().unwrap_or(""),
                                    path.display()
                                );
                                Finding {
                                    message: message.clone(),
                                    tape,
                                    count: 0,
                                }
                            });
                            entry.count += 1;
                        }
                        if local_cases.is_multiple_of(50) {
                            shared.cases.fetch_add(50, Ordering::Relaxed);
                            shared.transactions.fetch_add(
                                std::mem::take(&mut local.transactions),
                                Ordering::Relaxed,
                            );
                            shared
                                .corpus
                                .fetch_max(corpus.len() as u64, Ordering::Relaxed);
                        }
                    }
                    shared.cases.fetch_add(local_cases % 50, Ordering::Relaxed);
                    shared
                        .transactions
                        .fetch_add(local.transactions, Ordering::Relaxed);
                    local.transactions = 0;
                    shared.stats.lock().unwrap().merge(local);
                })
                .expect("spawn")
        })
        .collect();

    let start = std::time::Instant::now();
    let mut last = std::time::Instant::now();
    while workers.iter().any(|w| !w.is_finished()) {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if last.elapsed().as_secs() >= progress {
            last = std::time::Instant::now();
            eprintln!(
                "[{:>6.0?}] {} cases, {} transactions, largest corpus {}, {} distinct findings",
                start.elapsed(),
                shared.cases.load(Ordering::Relaxed),
                shared.transactions.load(Ordering::Relaxed),
                shared.corpus.load(Ordering::Relaxed),
                shared.findings.lock().unwrap().len()
            );
        }
    }
    for worker in workers {
        worker.join().expect("worker");
    }

    let stats = shared.stats.lock().unwrap();
    eprintln!(
        "{} cases, {} transactions ({} oversize skipped) in {:.0?}",
        shared.cases.load(Ordering::Relaxed),
        shared.transactions.load(Ordering::Relaxed),
        stats.oversize,
        start.elapsed()
    );
    for (outcome, count) in &stats.outcomes {
        eprintln!("{count:>10}  {outcome}");
    }
    let findings = shared.findings.lock().unwrap();
    for (i, finding) in findings.values().enumerate() {
        eprintln!(
            "\n=== finding {i} ({} times)\n{}\nFUZZ_TAPE={}",
            finding.count, finding.message, finding.tape
        );
    }
    assert!(findings.is_empty(), "{} distinct findings", findings.len());
}

impl Stats {
    fn merge(&mut self, other: Stats) {
        for (k, v) in other.outcomes {
            *self.outcomes.entry(k).or_default() += v;
        }
        self.transactions += other.transactions;
        self.oversize += other.oversize;
    }
}

/// xorshift64.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

/// A mutated copy of a corpus tape.
fn mutate(rng: &mut Rng, corpus: &[Vec<u8>]) -> Vec<u8> {
    let mut tape = corpus[rng.below(corpus.len() as u64) as usize].clone();
    for _ in 0..1 + rng.below(4) {
        let len = tape.len() as u64;
        match rng.below(8) {
            0 => {
                for _ in 0..1 + rng.below(8) {
                    if !tape.is_empty() {
                        let i = rng.below(tape.len() as u64) as usize;
                        tape[i] = rng.next() as u8;
                    }
                }
            }
            1 => {
                // Splice: this tape's prefix, another's suffix.
                let other = &corpus[rng.below(corpus.len() as u64) as usize];
                let i = rng.below(len + 1) as usize;
                let j = rng.below(other.len() as u64 + 1) as usize;
                tape.truncate(i);
                tape.extend_from_slice(&other[j..]);
            }
            2 => {
                let i = rng.below(len + 1) as usize;
                let n = 1 + rng.below(64) as usize;
                let chunk = rng.bytes(n);
                tape.splice(i..i, chunk);
            }
            3 if len > 0 => {
                let i = rng.below(len) as usize;
                let j = (i + 1 + rng.below(128) as usize).min(tape.len());
                tape.drain(i..j);
            }
            4 if len > 0 => {
                // Repeat a chunk somewhere else, so a step happens again.
                let i = rng.below(len) as usize;
                let j = (i + 1 + rng.below(256) as usize).min(tape.len());
                let chunk = tape[i..j].to_vec();
                let at = rng.below(tape.len() as u64 + 1) as usize;
                tape.splice(at..at, chunk);
            }
            5 => {
                let n = rng.below(1_000) as usize;
                let tail = rng.bytes(n);
                tape.extend(tail);
            }
            6 if len > 0 => tape.truncate(rng.below(len) as usize),
            _ => {
                // Overwrite a chunk with one from another tape.
                let other = &corpus[rng.below(corpus.len() as u64) as usize];
                if !other.is_empty() && len > 0 {
                    let j = rng.below(other.len() as u64) as usize;
                    let n = (1 + rng.below(64) as usize).min(other.len() - j);
                    let i = rng.below(len) as usize;
                    let n = n.min(tape.len() - i);
                    tape[i..i + n].copy_from_slice(&other[j..j + n]);
                }
            }
        }
    }
    tape.truncate(8_000);
    tape
}

/// A finding's first line with addresses and numbers blanked, to group repeats.
fn finding_key(message: &str) -> String {
    let line = message.lines().next().unwrap_or("");
    let line = line.split_once("): ").map_or(line, |(_, rest)| rest);
    line.split(|c: char| c.is_whitespace() || c == ',' || c == ':')
        .map(|word| {
            if word.len() >= 32 && word.chars().all(|c| c.is_ascii_alphanumeric()) {
                "K"
            } else if word.chars().any(|c| c.is_ascii_digit()) {
                "#"
            } else {
                word
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
