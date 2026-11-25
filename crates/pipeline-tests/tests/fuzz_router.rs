//! Stateful fuzzing of the Pipeline router.
//!
//! A fuzz case is a byte tape. The harness decodes it into a sequence of transactions
//! against one world, sends each one, and checks the router's guarantees after every
//! transaction, whether it succeeded or not. The world has the router; a registered
//! scriptable strategy (`test-strategy-script`, which runs a script from the tape during
//! market setup and every swap, so it can behave like any program, honest or not); eight
//! markets of two to four mints over SPL Token and Token-2022 mints with a transfer fee,
//! a permanent delegate on a market, a strategy-held mint and freeze authority, an
//! allow-all transfer hook and a hostile hook that refills the trader's input account,
//! and wrapped SOL (one market pairs it, another declares a different fee mint over it);
//! traders, a bystander, a treasury and an integrator.
//!
//! Transactions mix every router instruction (with valid, substituted and garbage
//! accounts and data), two swaps in a row, trader transfers, the strategy acting outside
//! the router, and compute-budget prefixes. Routes of two to four hops walk the world's
//! markets through the trader's own accounts (honest or scripted strategies, dust and
//! near-balance amounts, a minimum a dry run says is just achievable or one above), and
//! are sometimes broken on purpose. Lookup tables, valid or broken, are written into the
//! SVM and recorded; half the world's markets start with one. Transactions that are too
//! large, and some others, go out as v0 transactions through the recorded tables. Between
//! transactions the issuer and the world act too: time passes, the transfer fee changes,
//! the hook's stash and vaults are topped up, the router is frozen.
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
//!   swap pays `ceil(net_sent × 25 / 10,000)`, or `ceil(received × 25 / 10,000)` out of
//!   the output when the output is the market's fee mint, into the market's own fee
//!   account for that mint, and the integrator's rate on `net_sent`;
//! - every market's fee mint is the strategy's declaration, else wrapped SOL if it
//!   trades it, else none;
//! - the trader's output account never falls and keeps `received` (less an output-side
//!   fee) `≥ min_amount_out`, and the event and the return data report what really
//!   moved;
//! - fee accounts lose tokens only to a sweep, and only into the treasury;
//! - router accounts change only through their instructions: markets never, the config
//!   only by its admin, strategies only by their owner or a new market; every event matches the accounts it describes, and
//!   registrations and metadata updates come from the program's upgrade authority or its
//!   registration PDA;
//! - a route's hops each emit a swap event charged like a single swap on the side the
//!   market's record says; hop 1 sends `amount_in`, and each later hop sends what the
//!   previous one left the trader (all of it when its own fee is on the output, else the
//!   most that fits with its fee); the integrator fee is charged once, on hop 1's input;
//!   what the trader keeps from the last hop is at least the minimum, and the return data
//!   reports it; the trader's own balances of intermediate tokens never fall; a route of
//!   honest strategies moves exactly what its events say, nothing else, and conserves
//!   every mint; a broken route is refused; a dry run's result is what a real run does;
//! - a lookup table is recorded only for a market without one, only if it is a frozen,
//!   active table owned by the lookup table program holding every address the market
//!   needs, with the event and record naming it, and the record never changes after;
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
use solana_message::{v0, AddressLookupTableAccount, VersionedMessage};
use solana_signer::Signer;
use solana_transaction::versioned::VersionedTransaction;
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

/// Route instruction account positions: the trader, the integrator fee account, the event
/// authority and the router, then each hop's `ROUTE_HOP_ACCOUNTS` fixed accounts (see
/// `hop_at`), its strategy accounts and its input hook accounts.
mod route_meta {
    pub const INTEGRATOR: usize = 1;
    pub const FIRST_HOP: usize = 4;
    pub const HOP_ACCOUNTS: usize = 12;
}

/// A route hop's fixed account positions, from the hop's first account.
mod hop_at {
    pub const STRATEGY: usize = 0;
    pub const MARKET: usize = 1;
    pub const STATE: usize = 2;
    pub const TRADER_INPUT: usize = 3;
    pub const TRADER_OUTPUT: usize = 4;
    pub const INPUT_VAULT: usize = 5;
    pub const INPUT_MINT: usize = 6;
    pub const OUTPUT_MINT: usize = 7;
    pub const FEE_ACCOUNT: usize = 11;
}

const LOOKUP_TABLE_PROGRAM: Pubkey = client::ADDRESS_LOOKUP_TABLE_PROGRAM_ID;
/// Lookup table header: u32 state, u64 deactivation slot, u64 last extended slot, u8
/// start index, `Option<Pubkey>` authority, two bytes of padding.
const LOOKUP_TABLE_HEADER: usize = 56;

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

fn market_info_bytes(
    mints: &[Pubkey],
    vaults: &[Pubkey],
    exact_out: bool,
    fee_mint: Option<u8>,
) -> Vec<u8> {
    let info = psi::MarketInfo {
        mints: mints.iter().map(|k| k.to_bytes()).collect(),
        vaults: vaults.iter().map(|k| k.to_bytes()).collect(),
        supports_exact_out: exact_out,
    };
    psi::encode_market_info(&info, fee_mint)
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

/// Writes an address lookup table account holding `addresses` at `key`, in the lookup
/// table program's layout (see `LOOKUP_TABLE_HEADER`). A table that isn't frozen keeps a
/// random authority; an active one has a deactivation slot of `u64::MAX`.
fn write_lookup_table(
    svm: &mut LiteSVM,
    key: Pubkey,
    owner: Pubkey,
    addresses: &[Pubkey],
    frozen: bool,
    deactivation_slot: u64,
) {
    let mut data = vec![0u8; LOOKUP_TABLE_HEADER];
    data[0..4].copy_from_slice(&1u32.to_le_bytes());
    data[4..12].copy_from_slice(&deactivation_slot.to_le_bytes());
    data[20] = addresses.len().min(255) as u8;
    if !frozen {
        data[21] = 1;
        data[22..54].copy_from_slice(Pubkey::new_unique().as_ref());
    }
    for address in addresses {
        data.extend_from_slice(address.as_ref());
    }
    let account = Account {
        lamports: svm.minimum_balance_for_rent_exemption(data.len()),
        data,
        owner,
        executable: false,
        rent_epoch: 0,
    };
    svm.set_account(key, account).unwrap();
}

/// The addresses a lookup table account holds, valid or not.
fn table_addresses(account: &Account) -> Vec<Pubkey> {
    account
        .data
        .get(LOOKUP_TABLE_HEADER..)
        .unwrap_or_default()
        .chunks_exact(32)
        .map(|key| Pubkey::new_from_array(key.try_into().unwrap()))
        .collect()
}

/// Whether `table` is one `record_lookup_table` may record for the market at `market`:
/// owned by the lookup table program, a lookup table, active, frozen, and holding every
/// address the market needs.
fn table_is_valid_for(svm: &LiteSVM, market: &Pubkey, table: &Pubkey) -> bool {
    let Some(record) = svm
        .get_account(market)
        .filter(|a| a.owner == client::PROGRAM_ID)
        .and_then(|a| client::decode_market(&a.data))
    else {
        return false;
    };
    let Some(account) = svm.get_account(table) else {
        return false;
    };
    let data = &account.data;
    let well_formed = account.owner == LOOKUP_TABLE_PROGRAM
        && data.len() >= LOOKUP_TABLE_HEADER
        && (data.len() - LOOKUP_TABLE_HEADER).is_multiple_of(32)
        && data[0..4] == 1u32.to_le_bytes()
        && data[4..12] == u64::MAX.to_le_bytes()
        && data[21] == 0;
    let programs: Vec<Pubkey> = record
        .mints()
        .iter()
        .map(|mint| svm.get_account(mint).map_or(Pubkey::default(), |a| a.owner))
        .collect();
    let held = table_addresses(&account);
    well_formed
        && client::lookup_table_addresses(market, &record, &programs)
            .iter()
            .all(|key| held.contains(key))
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
    let nonces: Vec<[u8; 32]> = (0..8).map(nonce_from).collect();
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
    // Mint 8 is wrapped SOL, every market's default fee mint.
    add_mint(client::wallet::NATIVE_MINT, TOKEN, false);
    // Wrapping SOL for the traders' and vaults' balances spends the admin's lamports,
    // more than the test faucet holds.
    let admin_key = env.admin.pubkey();
    let mut admin_account = env.svm.get_account(&admin_key).unwrap();
    admin_account.lamports += 3 * VAULT_FUNDS + 10 * TRADER_FUNDS;
    env.svm.set_account(admin_key, admin_account).unwrap();

    let market_mints: [Vec<usize>; 8] = [
        vec![0, 1],
        vec![4, 0],
        vec![1, 5, 3],
        vec![2, 3, 0, 1],
        vec![6, 0, 1],
        vec![0, 7],
        vec![8, 0],
        vec![3, 1, 8],
    ];
    // Market 7 declares its transfer-fee mint as its fee mint, over wrapped SOL.
    let declared_fee_mints: [Option<u8>; 8] = [None, None, None, None, None, None, None, Some(0)];

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
            &market_info_bytes(&keys, &vaults, i % 2 == 1, declared_fee_mints[i]),
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

    // Half the markets start with a recorded lookup table, holding what the router needs
    // plus the traders' accounts and hook accounts for the market's mints, so v0
    // transactions through it are small; the rest are left for the fuzzer to record.
    for i in [0, 2, 4, 6] {
        let market = world.markets[i].clone();
        let mut addresses = world.lookup_addresses(&market);
        for &m in &market.mints {
            let extras = world
                .trader_accounts
                .iter()
                .map(|accounts| accounts[m])
                .chain(world.mints[m].hook_accounts.iter().map(|a| a.pubkey));
            for key in extras {
                if !addresses.contains(&key) {
                    addresses.push(key);
                }
            }
        }
        let table = Pubkey::new_unique();
        write_lookup_table(
            &mut world.svm,
            table,
            LOOKUP_TABLE_PROGRAM,
            &addresses,
            true,
            u64::MAX,
        );
        let mints: Vec<Pubkey> = market.mints.iter().map(|&m| world.mints[m].key).collect();
        let ix = client::record_lookup_table(&world.payer.pubkey(), &market.market, &table, &mints);
        world.setup(&[ix], &[]);
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
            pda::lookup_table_record(&market.market),
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

    /// The addresses `record_lookup_table` requires in a table for `market`.
    fn lookup_addresses(&self, market: &MarketSpec) -> Vec<Pubkey> {
        let Some(record) = self
            .svm
            .get_account(&market.market)
            .and_then(|a| client::decode_market(&a.data))
        else {
            return vec![];
        };
        let programs: Vec<Pubkey> = market
            .mints
            .iter()
            .map(|&m| self.mints[m].program)
            .collect();
        client::lookup_table_addresses(&market.market, &record, &programs)
    }
}

// ----------------------------------------------------------------------- transactions

/// A route the fuzzer built, and what it knows about how it should go.
#[derive(Clone, Debug)]
struct RouteStep {
    args: client::RouteArgs,
    /// Per hop: `Some(delta)` if its strategy only pays `amount_sent + delta` of the
    /// output to the trader, `None` if it runs an arbitrary script.
    pays: Vec<Option<i16>>,
    /// Why the route must be refused, if it was built broken on purpose.
    broken: Option<&'static str>,
    /// Some accounts were substituted at random: no expectations either way.
    mutated: bool,
    /// What a dry run of the same route did, alone in a transaction: what the trader
    /// kept, or the error.
    expect: Option<Result<u64, String>>,
}

impl RouteStep {
    fn honest(&self) -> bool {
        !self.mutated && self.pays.iter().all(Option::is_some)
    }
}

#[derive(Clone, Debug)]
enum Kind {
    Swap(client::SwapArgs),
    Route(Box<RouteStep>),
    /// `record_lookup_table` with a table of the kind `variant` describes; `accept` if the
    /// table is valid and the market has no table yet.
    RecordLookupTable {
        variant: &'static str,
        accept: bool,
    },
    Sweep,
    InitFee,
    CreateMarket {
        max_creator_lamports: u64,
        /// The fee mint the market info declares, if known (`None` when the info or the
        /// setup script is random).
        declared_fee_mint: Option<Option<u8>>,
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
            Kind::Route(_) => "route",
            Kind::RecordLookupTable { .. } => "record_lookup_table",
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
        // Pay the fee on the side the market's record says, and now and then on the
        // other side (the router must refuse the wrong fee account).
        let real_side = self
            .svm
            .get_account(&market.market)
            .and_then(|a| client::decode_market(&a.data))
            .map_or(client::FeeSide::Input, |m| {
                client::FeeSide::of(&m, &self.mints[out_mint].key)
            });
        let side = if tape.chance(5) {
            match real_side {
                client::FeeSide::Input => client::FeeSide::Output,
                client::FeeSide::Output => client::FeeSide::Input,
            }
        } else {
            real_side
        };
        let fee_mint = match side {
            client::FeeSide::Input => in_mint,
            client::FeeSide::Output => out_mint,
        };
        let mut ix = client::swap_with_fee_side(&accounts, &args, side);
        let mut args = args;
        args.input_hook_accounts = accounts.input_hook_accounts.len() as u8;
        if tape.chance(12) {
            // Swap one token account for another real one of the right mint.
            let (slot, mint) = match tape.u8() % 5 {
                0 => (swap_meta::TRADER_INPUT, in_mint),
                1 => (swap_meta::TRADER_OUTPUT, out_mint),
                2 => (swap_meta::INPUT_VAULT, in_mint),
                3 => (swap_meta::FEE_ACCOUNT, fee_mint),
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

    /// An honest strategy script: pays `amount_sent + delta` of the output from the
    /// output vault to the trader and does nothing else. (It doesn't mint: the router
    /// passes the strategy the output mint read-only.) Returns the script and `delta`.
    fn honest_pay(&self, tape: &mut Tape, layout: &Layout) -> (Vec<u8>, i16) {
        let delta = (tape.u8() % 7) as i16 - 3;
        let op = if layout.out_hooks.is_empty() {
            script::TRANSFER
        } else {
            script::TRANSFER_WITH
        };
        let mut s = vec![
            op,
            at::OUTPUT_VAULT,
            at::OUTPUT_MINT,
            at::TRADER_OUTPUT,
            at::STATE,
            script::SENT_PLUS,
        ];
        s.extend_from_slice(&delta.to_le_bytes());
        if !layout.out_hooks.is_empty() {
            s.push(layout.out_hooks.len() as u8);
            s.extend(&layout.out_hooks);
        }
        (s, delta)
    }

    /// The side a trade through `market` into the world's mint `output` pays Pipeline's
    /// fee on, from the market's record.
    fn fee_side(&self, market: &Pubkey, output: usize) -> client::FeeSide {
        self.svm
            .get_account(market)
            .and_then(|a| client::decode_market(&a.data))
            .map_or(client::FeeSide::Input, |m| {
                client::FeeSide::of(&m, &self.mints[output].key)
            })
    }

    /// One route hop for trader `t` through `market`, from its `input`th mint to its
    /// `output`th, through the trader's own accounts, with the fee on the side the market's
    /// record says. The strategy pays honestly, or (only if `honest` is false, half the
    /// time) runs an arbitrary script with a few extra accounts. Returns the hop and its
    /// honest payment's delta.
    fn route_hop(
        &self,
        tape: &mut Tape,
        t: usize,
        market: &MarketSpec,
        input: usize,
        output: usize,
        honest: bool,
    ) -> (client::RouteHopSpec, Option<i16>) {
        let (in_mint, out_mint) = (market.mints[input], market.mints[output]);
        let mut strategy_accounts = vec![AccountMeta::new(market.vaults[output], false)];
        let out_hook_accounts = &self.mints[out_mint].hook_accounts;
        strategy_accounts.extend(out_hook_accounts.iter().cloned());
        let scripted = !honest && tape.chance(50);
        if scripted {
            strategy_accounts.extend(self.pool_metas(tape, 3));
        }
        let input_hook_accounts = self.mints[in_mint].hook_accounts.clone();
        let in_base = 10 + strategy_accounts.len() as u8;
        let layout = Layout {
            out_hooks: (0..out_hook_accounts.len() as u8).map(|k| 11 + k).collect(),
            in_hooks: (0..input_hook_accounts.len() as u8)
                .map(|k| in_base + k)
                .collect(),
            exact_out: false,
        };
        let (extra, pay) = if scripted {
            (self.swap_script(tape, market, output, &layout), None)
        } else {
            let (script, delta) = self.honest_pay(tape, &layout);
            (script, Some(delta))
        };
        let accounts = client::SwapAccounts {
            trader: self.traders[t].pubkey(),
            strategy_program: SCRIPT_ID,
            market_state: market.state,
            trader_input: self.trader_accounts[t][in_mint],
            trader_output: self.trader_accounts[t][out_mint],
            input_vault: market.vaults[input],
            input_mint: self.mints[in_mint].key,
            output_mint: self.mints[out_mint].key,
            input_token_program: self.mints[in_mint].program,
            output_token_program: self.mints[out_mint].program,
            integrator_fee_account: None,
            strategy_accounts,
            input_hook_accounts,
        };
        let spec = client::RouteHopSpec {
            accounts,
            fee_side: self.fee_side(&market.market, out_mint),
            extra,
        };
        (spec, pay)
    }

    /// A route: a walk of two to four hops over the world's markets through the trader's
    /// own accounts, or, a quarter of the time, a route broken on purpose.
    fn gen_route(&self, tape: &mut Tape) -> Step {
        const BREAKS: [&str; 12] = [
            "input account off the chain",
            "input mint off the chain",
            "wrong fee account",
            "market not of its state",
            "five hops",
            "no hops",
            "hop account counts",
            "extra or missing account",
            "integrator is the input",
            "wrong input vault",
            "same mint in and out",
            "integrator fee without account",
        ];
        let t = tape.below(self.traders.len());
        let trader = self.traders[t].pubkey();
        let broken = tape.chance(25).then(|| tape.pick(&BREAKS));
        let hop_count = match broken {
            Some("five hops") => 5,
            Some("no hops") => 0,
            _ => 2 + tape.below(3),
        };
        let honest = tape.chance(70);

        // Walk from a mint of a random market; cycles are allowed.
        let first = tape.pick(&self.markets);
        let start = tape.pick(&first.mints);
        let mut current = start;
        let mut specs = vec![];
        let mut pays = vec![];
        let mut hop_mints = vec![];
        for _ in 0..hop_count {
            let options: Vec<&MarketSpec> = self
                .markets
                .iter()
                .filter(|m| m.mints.contains(&current))
                .collect();
            let market = options[tape.below(options.len())];
            let n = market.mints.len();
            let input = market.mints.iter().position(|&m| m == current).unwrap();
            let output = (input + 1 + tape.below(n - 1)) % n;
            let (spec, pay) = self.route_hop(tape, t, market, input, output, honest);
            specs.push(spec);
            pays.push(pay);
            hop_mints.push((market.clone(), input, output));
            current = market.mints[output];
        }

        let (mut integrator, mut bps) = match tape.u8() % 8 {
            0..=3 => (None, 0),
            4 => (Some(self.integrator_accounts[start]), 0),
            5 | 6 => (Some(self.integrator_accounts[start]), tape.u16() % 500),
            _ => (
                Some(self.integrator_accounts[start]),
                1 + tape.u16() % 10_000,
            ),
        };
        let balance = self
            .svm
            .get_account(&self.trader_accounts[t][start])
            .as_ref()
            .and_then(token_state)
            .map_or(0, |s| s.amount);
        let input_fee = specs
            .first()
            .is_some_and(|s| s.fee_side == client::FeeSide::Input);
        // The most hop 1 can send with its fees still within the trader's balance.
        let affordable = most_within(balance, |a| {
            a as u128 + if input_fee { ceil_bps(a, 25) } else { 0 } + ceil_bps(a, bps as u64)
        });
        let amount_in = match tape.u8() % 10 {
            0 => 1 + tape.u8() as u64 % 4,
            // Around 400, where the fee's rounding steps.
            1 => 396 + tape.u8() as u64 % 10,
            2 => affordable,
            3 => affordable.saturating_add(1),
            4 => affordable.saturating_sub(tape.u8() as u64),
            _ => tape.amount(1_000_000),
        };

        // Breaks made to the hops before the instruction is built.
        let pick_hop =
            |tape: &mut Tape, from: usize| from + tape.below(hop_count.max(from + 1) - from);
        match broken {
            Some("input account off the chain") => {
                let k = pick_hop(tape, 1);
                let m = hop_mints[k].0.mints[hop_mints[k].1];
                let others = [
                    self.victim_accounts[m],
                    self.trader_accounts[(t + 1) % self.traders.len()][m],
                    self.integrator_accounts[m],
                ];
                specs[k].accounts.trader_input = tape.pick(&others);
            }
            Some("input mint off the chain") => {
                let k = pick_hop(tape, 1);
                let m = hop_mints[k].0.mints[hop_mints[k].1];
                let other = (m + 1 + tape.below(self.mints.len() - 1)) % self.mints.len();
                specs[k].accounts.input_mint = self.mints[other].key;
                specs[k].accounts.input_token_program = self.mints[other].program;
            }
            Some("same mint in and out") => {
                let k = pick_hop(tape, 0);
                let a = &mut specs[k].accounts;
                a.output_mint = a.input_mint;
                a.trader_output = a.trader_input;
                a.output_token_program = a.input_token_program;
            }
            Some("wrong input vault") => {
                let k = pick_hop(tape, 0);
                let (market, input, _) = &hop_mints[k];
                let vault = market.vaults[*input];
                let others: Vec<Pubkey> = self
                    .token_accounts_of(market.mints[*input])
                    .into_iter()
                    .filter(|a| *a != vault)
                    .collect();
                specs[k].accounts.input_vault = tape.pick(&others);
            }
            Some("integrator is the input") => integrator = Some(self.trader_accounts[t][start]),
            Some("integrator fee without account") => {
                integrator = None;
                bps = 1 + tape.u16() % 500;
            }
            _ => {}
        }
        let mut args = client::RouteArgs {
            amount_in,
            min_amount_out: 0,
            integrator_fee_bps: bps,
            hops: specs
                .iter()
                .map(|s| client::RouteHop {
                    strategy_accounts: s.accounts.strategy_accounts.len() as u8,
                    input_hook_accounts: s.accounts.input_hook_accounts.len() as u8,
                    extra: s.extra.clone(),
                })
                .collect(),
        };
        let mut ix = client::route(&trader, integrator, &specs, amount_in, 0, bps);

        // Breaks made to the instruction itself.
        let hop_start = |args: &client::RouteArgs, k: usize| {
            route_meta::FIRST_HOP
                + args.hops[..k]
                    .iter()
                    .map(|h| {
                        route_meta::HOP_ACCOUNTS
                            + h.strategy_accounts as usize
                            + h.input_hook_accounts as usize
                    })
                    .sum::<usize>()
        };
        match broken {
            Some("wrong fee account") => {
                let k = pick_hop(tape, 0);
                let (market, input, output) = &hop_mints[k];
                let at = hop_start(&args, k) + hop_at::FEE_ACCOUNT;
                let right = ix.accounts[at].pubkey;
                let fee_account = |market: &Pubkey, m: usize| {
                    pda::fee_account(market, &self.mints[m].key, &self.mints[m].program)
                };
                let fee_mint = match specs[k].fee_side {
                    client::FeeSide::Input => market.mints[*input],
                    client::FeeSide::Output => market.mints[*output],
                };
                let mut wrong = vec![
                    fee_account(&market.market, market.mints[*input]),
                    fee_account(&market.market, market.mints[*output]),
                    specs[k].accounts.trader_input,
                    specs[k].accounts.trader_output,
                    pda::fee_authority(&market.market),
                ];
                for other in &self.markets {
                    if other.market != market.market && other.mints.contains(&fee_mint) {
                        wrong.push(fee_account(&other.market, fee_mint));
                    }
                }
                wrong.retain(|k| *k != right);
                ix.accounts[at].pubkey = tape.pick(&wrong);
            }
            Some("market not of its state") => {
                let k = pick_hop(tape, 0);
                let at = hop_start(&args, k);
                let own = hop_mints[k].0.market;
                let others: Vec<&MarketSpec> =
                    self.markets.iter().filter(|m| m.market != own).collect();
                let other = others[tape.below(others.len())];
                if tape.chance(50) {
                    ix.accounts[at + hop_at::MARKET].pubkey = other.market;
                } else {
                    ix.accounts[at + hop_at::STATE].pubkey = other.state;
                }
            }
            Some("hop account counts") => {
                let k = pick_hop(tape, 0);
                let hop = &mut args.hops[k];
                match tape.u8() % 4 {
                    0 => hop.strategy_accounts += 1,
                    1 => hop.strategy_accounts -= 1,
                    2 => hop.input_hook_accounts += 1,
                    _ if hop.input_hook_accounts > 0 => hop.input_hook_accounts -= 1,
                    _ => hop.strategy_accounts += 2,
                }
                ix.data = route_data(&args);
            }
            Some("extra or missing account") => {
                if tape.chance(50) {
                    ix.accounts.pop();
                } else {
                    let key = self.pool_pick(tape);
                    ix.accounts.push(AccountMeta::new_readonly(key, false));
                }
            }
            _ => {}
        }

        let original = ix.accounts.clone();
        if broken.is_none() && tape.chance(10) {
            self.mutate_accounts(tape, &mut ix);
        }
        let mutated = ix.accounts != original;

        // A dry run of the route alone, with no minimum, gives what the trader would keep;
        // the minimum is then just achievable, one above, or anything.
        let mut expect = None;
        args.min_amount_out = match tape.u8() % 4 {
            0 => 0,
            _ => tape.amount(amount_in),
        };
        if broken.is_none() && !mutated {
            expect = self.dry_run(&ix, t);
            if let Some(Ok(kept)) = expect {
                args.min_amount_out = match tape.u8() % 5 {
                    0 | 1 => kept,
                    2 => kept.saturating_add(1),
                    3 => 0,
                    _ => tape.amount(kept),
                };
                if args.min_amount_out > kept {
                    expect = Some(Err("InsufficientOutput".into()));
                }
            }
        }
        ix.data = route_data(&args);
        Step {
            kind: Kind::Route(Box::new(RouteStep {
                args,
                pays,
                broken,
                mutated,
                expect,
            })),
            ix,
        }
    }

    /// Runs `ix` alone, signed by trader `t`, without committing it: what the trader
    /// keeps (the route's return data), or the instruction error's name. `None` if the
    /// transaction failed for another reason, or ran out of compute (which a raised
    /// compute limit could change).
    fn dry_run(&self, ix: &Instruction, t: usize) -> Option<Result<u64, String>> {
        let tx = Transaction::new_signed_with_payer(
            std::slice::from_ref(ix),
            Some(&self.payer.pubkey()),
            &[&self.payer, &self.traders[t]],
            self.svm.latest_blockhash(),
        );
        match self.svm.simulate_transaction(tx) {
            Ok(info) => {
                let result = client::RouteResult::try_from_slice(&info.meta.return_data.data)
                    .map_or(Err("no route result".to_string()), |r| Ok(r.amount_out));
                Some(result)
            }
            Err(failed) => {
                let err = format!("{:?}", failed.err);
                let instruction_error = err.starts_with("InstructionError");
                let name = error_name(&failed.meta.logs, &err);
                (instruction_error && !out_of_compute(&name)).then_some(Err(name))
            }
        }
    }

    /// `record_lookup_table` for a market, with a table written into the SVM that is
    /// valid, or broken in one way, or recorded with the wrong mints or market.
    fn gen_record_lookup_table(&mut self, tape: &mut Tape) -> Step {
        let market = tape.pick(&self.markets);
        let required = self.lookup_addresses(&market);
        let mut addresses = required.clone();
        if tape.chance(50) {
            for _ in 0..tape.below(6) {
                let key = self.pool_pick(tape);
                if !addresses.contains(&key) {
                    addresses.push(key);
                }
            }
        }
        if !addresses.is_empty() {
            let k = tape.below(addresses.len());
            addresses.rotate_left(k);
        }
        let mut mints: Vec<Pubkey> = market.mints.iter().map(|&m| self.mints[m].key).collect();
        let mut market_key = market.market;
        let (mut owner, mut frozen, mut deactivation) = (LOOKUP_TABLE_PROGRAM, true, u64::MAX);
        let mut garbage = None;
        let variant = match tape.u8() % 12 {
            0..=4 => "valid",
            5 => {
                frozen = false;
                "unfrozen"
            }
            6 => {
                deactivation = match tape.u8() % 3 {
                    0 => 0,
                    1 => self.svm.get_sysvar::<solana_clock::Clock>().slot,
                    _ => u64::MAX - 1,
                };
                "deactivated"
            }
            7 if !required.is_empty() => {
                let gone = tape.pick(&required);
                addresses.retain(|k| *k != gone);
                "missing an address"
            }
            8 => {
                owner = tape.pick(&[
                    client::SYSTEM_PROGRAM_ID,
                    client::PROGRAM_ID,
                    TOKEN,
                    SCRIPT_ID,
                ]);
                "wrong owner"
            }
            9 => {
                match tape.u8() % 4 {
                    0 => mints.reverse(),
                    1 => {
                        let i = tape.below(mints.len());
                        let n = self.mints.len();
                        let m = market.mints[i];
                        mints[i] = self.mints[(m + 1 + tape.below(n - 1)) % n].key;
                    }
                    2 => {
                        mints.pop();
                    }
                    _ => mints.push(tape.pick(&self.mints).key),
                }
                "wrong mints"
            }
            10 => {
                market_key = self.pool_pick(tape);
                "another account as the market"
            }
            _ => {
                let len = tape.below(200);
                garbage = Some(tape.bytes(len));
                "garbage"
            }
        };
        let table = Pubkey::new_unique();
        write_lookup_table(
            &mut self.svm,
            table,
            owner,
            &addresses,
            frozen,
            deactivation,
        );
        if let Some(data) = garbage {
            let mut account = self.svm.get_account(&table).unwrap();
            account.data = data;
            self.svm.set_account(table, account).unwrap();
        }
        // Whether the router should take it, judged from the accounts alone.
        let market_mints = self
            .svm
            .get_account(&market_key)
            .filter(|a| a.owner == client::PROGRAM_ID)
            .and_then(|a| client::decode_market(&a.data))
            .map(|m| m.mints().to_vec());
        let unrecorded = !holds_lookup_record(
            self.svm
                .get_account(&pda::lookup_table_record(&market_key))
                .as_ref(),
        );
        let accept = table_is_valid_for(&self.svm, &market_key, &table)
            && market_mints == Some(mints.clone())
            && unrecorded;
        let variant = if variant == "valid" && !unrecorded {
            "valid, for a market that has one"
        } else {
            variant
        };
        let ix = client::record_lookup_table(&self.payer.pubkey(), &market_key, &table, &mints);
        Step {
            kind: Kind::RecordLookupTable { variant, accept },
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
    /// bytes, the market's mints and vaults if valid, and the fee mint the bytes declare
    /// (`None` for random bytes).
    fn gen_info(
        &self,
        tape: &mut Tape,
        state: &Pubkey,
    ) -> (Vec<u8>, Option<ValidInfo>, Option<Option<u8>>) {
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
        // Mostly no declaration; sometimes one of the market's mints, or out of range.
        let fee_mint = match tape.u8() % 8 {
            0 | 1 => Some(tape.below(keys.len()) as u8),
            2 => Some(keys.len() as u8 + tape.u8() % 3),
            _ => None,
        };
        match tape.u8() % 12 {
            0 => keys.push(keys[0]),
            1 => keys.truncate(1),
            2 => {
                vaults.pop();
            }
            3 => {
                let len = tape.below(80);
                return (tape.bytes(len), None, None);
            }
            4 => {
                while keys.len() < 5 {
                    keys.push(Pubkey::new_unique());
                    vaults.push(Pubkey::new_unique());
                }
            }
            5 => keys[1] = self.pool_pick(tape),
            _ => {
                let info = market_info_bytes(&keys, &vaults, exact_out, fee_mint);
                let valid = fee_mint.is_none_or(|index| (index as usize) < keys.len());
                return (info, valid.then_some((mint_ids, vaults)), Some(fee_mint));
            }
        }
        (
            market_info_bytes(&keys, &vaults, exact_out, fee_mint),
            None,
            Some(fee_mint),
        )
    }

    fn gen_create_market(&mut self, tape: &mut Tape) -> (Step, Option<Pending>, Vec<Keypair>) {
        let creator = Keypair::new();
        self.svm.airdrop(&creator.pubkey(), 10 * SOL).unwrap();
        let nonce = nonce_from(1_000 + tape.u16() as u64);
        let state = state_address(&nonce);
        let (info, valid, mut declared_fee_mint) = self.gen_info(tape, &state);
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
                // A random script may rewrite the state the market info is read from.
                let len = tape.below(24);
                init_script.extend(tape.bytes(len));
                declared_fee_mint = None;
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
                    declared_fee_mint,
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
            let (info, _, _) = self.gen_info(tape, &state);
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
                3 => 10_000_000_000,
                4 => 10_000_000_001,
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
        const NAMES: [&str; 12] = [
            "initialize_config",
            "update_config",
            "register_strategy",
            "update_metadata",
            "create_market",
            "register_existing_market",
            "init_fee_account",
            "swap",
            "route",
            "record_lookup_table",
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

    /// `ixs` as a v0 transaction loading accounts from the recorded lookup tables of the
    /// markets they name, and, if that is still too large and `must_fit` (the legacy
    /// transaction is too large), from a table of its own as well. Returns the
    /// transaction, its account keys (loaded ones included) and a label, or `None` if no
    /// recorded table applies and the transaction needn't fit, or it can't fit.
    fn v0_transaction(
        &mut self,
        ixs: &[Instruction],
        signers: &[&Keypair],
        must_fit: bool,
    ) -> Option<(VersionedTransaction, Vec<Pubkey>, &'static str)> {
        let named: HashSet<Pubkey> = ixs
            .iter()
            .flat_map(|ix| ix.accounts.iter().map(|a| a.pubkey))
            .collect();
        let mut tables: Vec<AddressLookupTableAccount> = vec![];
        for market in &self.markets {
            if !named.contains(&market.market) {
                continue;
            }
            let Some(record) = self
                .svm
                .get_account(&pda::lookup_table_record(&market.market))
                .and_then(|a| client::decode_market_lookup_table(&a.data))
            else {
                continue;
            };
            let Some(table) = self.svm.get_account(&record.table) else {
                continue;
            };
            if !tables.iter().any(|t| t.key == record.table) {
                tables.push(AddressLookupTableAccount {
                    key: record.table,
                    addresses: table_addresses(&table),
                });
            }
        }
        if tables.is_empty() && !must_fit {
            return None;
        }
        let payer = self.payer.pubkey();
        let compile = |tables: &[AddressLookupTableAccount], blockhash| {
            let message = v0::Message::try_compile(&payer, ixs, tables, blockhash).ok()?;
            VersionedTransaction::try_new(VersionedMessage::V0(message), signers).ok()
        };
        let size =
            |tx: &VersionedTransaction| bincode::serialize(tx).map_or(usize::MAX, |b| b.len());
        let mut tx = compile(&tables, self.svm.latest_blockhash())?;
        let mut via = " (v0)";
        if size(&tx) > MAX_TRANSACTION_SIZE {
            if !must_fit {
                return None;
            }
            let held: HashSet<Pubkey> = tables
                .iter()
                .flat_map(|t| t.addresses.iter().copied())
                .collect();
            let mut rest: Vec<Pubkey> = vec![];
            for meta in ixs.iter().flat_map(|ix| &ix.accounts) {
                if !meta.is_signer && !held.contains(&meta.pubkey) && !rest.contains(&meta.pubkey) {
                    rest.push(meta.pubkey);
                }
            }
            rest.truncate(255);
            let key = Pubkey::new_unique();
            write_lookup_table(
                &mut self.svm,
                key,
                LOOKUP_TABLE_PROGRAM,
                &rest,
                true,
                u64::MAX,
            );
            tables.push(AddressLookupTableAccount {
                key,
                addresses: rest,
            });
            tx = compile(&tables, self.svm.latest_blockhash())?;
            via = " (v0, own table)";
            if size(&tx) > MAX_TRANSACTION_SIZE {
                return None;
            }
        }
        let VersionedMessage::V0(message) = &tx.message else {
            unreachable!("compiled as v0");
        };
        // Loaded accounts follow the static ones: every table's writable ones, then every
        // table's read-only ones.
        let mut keys = message.account_keys.clone();
        let table = |key: &Pubkey| tables.iter().find(|t| t.key == *key).unwrap();
        for lookup in &message.address_table_lookups {
            let addresses = &table(&lookup.account_key).addresses;
            keys.extend(
                lookup
                    .writable_indexes
                    .iter()
                    .map(|&i| addresses[i as usize]),
            );
        }
        for lookup in &message.address_table_lookups {
            let addresses = &table(&lookup.account_key).addresses;
            keys.extend(
                lookup
                    .readonly_indexes
                    .iter()
                    .map(|&i| addresses[i as usize]),
            );
        }
        Some((tx, keys, via))
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

/// The largest `a <= budget` with `cost(a) <= budget`, for a non-decreasing `cost`.
fn most_within(budget: u64, cost: impl Fn(u64) -> u128) -> u64 {
    let (mut low, mut high) = (0u64, budget);
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        if cost(mid) <= budget as u128 {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    low
}

/// What a later route hop with its fee on the input may send out of `kept`: the most
/// that, with Pipeline's fee on it, fits.
fn spendable(kept: u64) -> u64 {
    most_within(kept, |a| a as u128 + ceil_bps(a, 25))
}

fn route_data(args: &client::RouteArgs) -> Vec<u8> {
    let mut data = client::instruction_discriminator("route").to_vec();
    data.extend(borsh::to_vec(args).unwrap());
    data
}

/// Transfer fees withheld in a Token-2022 account (0 for any other account).
fn withheld(account: &Account) -> u64 {
    use spl_token_2022_interface::extension::{
        transfer_fee::TransferFeeAmount, BaseStateWithExtensions, StateWithExtensions,
    };
    if account.owner != TOKEN_2022 {
        return 0;
    }
    StateWithExtensions::<spl_token_2022_interface::state::Account>::unpack(&account.data)
        .ok()
        .and_then(|state| {
            state
                .get_extension::<TransferFeeAmount>()
                .ok()
                .map(|e| u64::from(e.withheld_amount))
        })
        .unwrap_or(0)
}

/// A token account's balance plus the transfer fees withheld in it: what arrived, before
/// the mint's transfer fee was set aside (0 for anything else).
fn gross(snap: &Snapshot, key: &Pubkey) -> i128 {
    snap.get(key)
        .and_then(|a| a.as_ref())
        .and_then(|a| token_state(a).map(|t| t.amount as i128 + withheld(a) as i128))
        .unwrap_or(0)
}

/// A mint's supply (both token programs keep it at offset 36).
fn supply(snap: &Snapshot, mint: &Pubkey) -> i128 {
    snap.get(mint)
        .and_then(|a| a.as_ref())
        .and_then(|a| a.data.get(36..44))
        .map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap()) as i128)
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

/// Router events emitted by each of a transaction's `instructions` top-level
/// instructions; `keys` are its account keys, including any loaded from lookup tables.
fn events_by_instruction(
    keys: &[Pubkey],
    instructions: usize,
    meta: &litesvm::types::TransactionMetadata,
) -> Vec<Vec<PipelineEvent>> {
    let mut out = vec![vec![]; instructions];
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
    keys: &[Pubkey],
    meta: &litesvm::types::TransactionMetadata,
) -> Result<(), String> {
    let world = ctx.world;
    let (before, after) = (ctx.before, ctx.after);
    let events = events_by_instruction(keys, ctx.event_offset + ctx.steps.len(), meta)
        .split_off(ctx.event_offset);
    let mint_account = |mint: &Pubkey| after.get(mint).and_then(|a| a.as_ref());
    let stash_authority = Pubkey::find_program_address(&[b"stash"], &REFILL_ID).0;
    // Did the refill hook move anything (a token transfer out of its stash)?
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
                let market_record = before
                    .get(&event.market)
                    .and_then(|a| a.as_ref())
                    .and_then(|a| client::decode_market(&a.data))
                    .ok_or("swap through an unknown market")?;
                // The fee is in the output when the output is the market's fee mint.
                let fee_on_output = client::FeeSide::of(&market_record, &event.output_mint)
                    == client::FeeSide::Output;
                let (fee_mint, fee_base) = if fee_on_output {
                    (event.output_mint, event.received)
                } else {
                    (event.input_mint, event.net_sent)
                };
                if event.fee_mint != fee_mint {
                    return Err(format!(
                        "swap {i}: fee taken in {} but the market's fee side says {fee_mint}",
                        event.fee_mint
                    ));
                }
                if event.pipeline_fee as u128 != ceil_bps(fee_base, 25) {
                    return Err(format!(
                        "swap {i}: pipeline fee {} on {fee_base} (fee on output: {fee_on_output})",
                        event.pipeline_fee
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
                let output_fee = if fee_on_output { event.pipeline_fee } else { 0 };
                let kept = event.received - output_fee;
                if kept < args.min_amount_out {
                    return Err(format!(
                        "swap {i}: received {} (keeping {kept}) < min {}",
                        event.received, args.min_amount_out
                    ));
                }
                // Fees go to the market's own fee account for the fee mint.
                let fee_program = after
                    .get(&fee_mint)
                    .and_then(|a| a.as_ref())
                    .map(|a| a.owner)
                    .ok_or("fee mint missing")?;
                let expected = pda::fee_account(&event.market, &fee_mint, &fee_program);
                if key(swap_meta::FEE_ACCOUNT) != expected {
                    return Err(format!(
                        "swap {i}: fee paid to {} instead of the market's fee account {expected}",
                        key(swap_meta::FEE_ACCOUNT)
                    ));
                }
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
                let input_side_fee = if fee_on_output {
                    0
                } else {
                    ceil_bps(args.amount_in, 25)
                };
                let max_spend = args.amount_in as u128
                    + input_side_fee
                    + ceil_bps(args.amount_in, args.integrator_fee_bps as u64);
                *allowance.entry(input).or_default() += max_spend;

                let cut = |mint: &Pubkey, fee: u64| {
                    fee.saturating_sub(transfer_fee_cut(mint_account(mint), ctx.epoch, fee))
                };
                *min_fee_gain.entry(fee_account).or_default() +=
                    cut(&fee_mint, event.pipeline_fee) as u128;
                if let Some(integrator) = integrator {
                    *min_fee_gain.entry(integrator).or_default() +=
                        cut(&event.input_mint, event.integrator_fee) as u128;
                }

                // Exact accounting when the swap is alone in its transaction.
                if ctx.steps.len() == 1 {
                    let output = key(swap_meta::TRADER_OUTPUT);
                    let out_before =
                        balance(before, &output).ok_or("output not a token account")?;
                    let out_after = balance(after, &output).ok_or("output closed")?;
                    let moved = out_after as i128 - out_before as i128;
                    let refilled_after =
                        refilled && output == refill_target && moved > kept as i128;
                    if refilled_after {
                        // The refill hook topped up the output account during the
                        // router's fee transfers, after `received` was measured.
                        ctx.notes
                            .borrow_mut()
                            .push("a hook topped up the output after measurement".into());
                    } else if moved != kept as i128 {
                        return Err(format!(
                            "swap {i}: trader output moved {moved} but the event says received {} and fee on output {output_fee}",
                            event.received
                        ));
                    }
                    let in_before = balance(before, &input).ok_or("input not a token account")?;
                    let in_after = balance(after, &input).ok_or("input closed")?;
                    let lost = in_before as i128 - in_after as i128;
                    let mut fees = (event.pipeline_fee - output_fee) as i128;
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
                            result.fee_mint,
                        ) != (
                            event.net_sent,
                            event.received,
                            event.pipeline_fee,
                            event.integrator_fee,
                            event.fee_mint,
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
            Kind::Route(route) => {
                let mut budget = Budget {
                    allowance: &mut allowance,
                    min_fee_gain: &mut min_fee_gain,
                };
                check_route(ctx, i, ix, route, &events[i], meta, &mut budget)?;
            }
            Kind::RecordLookupTable { variant, accept } => {
                // The record and its event are checked below, with any other recording.
                if !accept {
                    return Err(format!(
                        "record_lookup_table {i} accepted a table that is {variant}"
                    ));
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
                declared_fee_mint,
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
                // The fee mint is the declaration, else wrapped SOL if traded, else none.
                let record = after
                    .get(&market)
                    .and_then(|a| a.as_ref())
                    .and_then(|a| client::decode_market(&a.data));
                if let (Some(declared), Some(record)) = (declared_fee_mint, record) {
                    let expected = declared.unwrap_or_else(|| {
                        record
                            .mints()
                            .iter()
                            .position(|m| *m == client::wallet::NATIVE_MINT)
                            .map_or(client::NO_FEE_MINT, |index| index as u8)
                    });
                    if record.fee_mint_index != expected {
                        return Err(format!(
                            "create_market {i}: fee mint index {} but expected {expected}",
                            record.fee_mint_index
                        ));
                    }
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
                PipelineEvent::LookupTableRecorded(e) => {
                    let record_key = pda::lookup_table_record(&e.market);
                    if holds_lookup_record(before.get(&record_key).and_then(|a| a.as_ref())) {
                        return Err(format!(
                            "a second table {} was recorded for market {}",
                            e.table, e.market
                        ));
                    }
                    let record = after
                        .get(&record_key)
                        .and_then(|a| a.as_ref())
                        .filter(|a| a.owner == client::PROGRAM_ID)
                        .and_then(|a| client::decode_market_lookup_table(&a.data))
                        .ok_or(format!("LookupTableRecorded {e:?} without a record"))?;
                    let bump = Pubkey::find_program_address(
                        &[b"lookup", e.market.as_ref()],
                        &client::PROGRAM_ID,
                    );
                    if (record.market, record.table) != (e.market, e.table)
                        || (record_key, record.bump) != bump
                    {
                        return Err(format!(
                            "LookupTableRecorded {e:?} but the record is {record:?}"
                        ));
                    }
                    if !table_is_valid_for(&world.svm, &e.market, &e.table) {
                        return Err(format!(
                            "recorded table {} is not a frozen, active table holding market {}'s addresses",
                            e.table, e.market
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
            // The first table recorded for a market stays.
            if client::decode_market_lookup_table(&old.data).is_some() && new.data != old.data {
                return Err(format!("lookup table record {key} changed"));
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
            let only_swaps = ctx
                .steps
                .iter()
                .all(|s| matches!(s.kind, Kind::Swap(_) | Kind::Route(_)));
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
        // A new lookup table record comes with its event (checked above).
        if let Some(record) = client::decode_market_lookup_table(&new.data) {
            let announced = events.iter().flatten().any(|e| {
                matches!(e, PipelineEvent::LookupTableRecorded(r)
                    if (r.market, r.table) == (record.market, record.table))
            });
            if !announced || pda::lookup_table_record(&record.market) != *key {
                return Err(format!(
                    "lookup table record {key} ({record:?}) appeared without its event"
                ));
            }
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
        if market.fee_mint_index != client::NO_FEE_MINT
            && market.fee_mint_index as usize >= mints.len()
        {
            return Err(format!(
                "new market {key} has fee mint index {} of {} mints",
                market.fee_mint_index,
                mints.len()
            ));
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

/// What a transaction's successful steps may take from signers' token accounts, and the
/// least each fee account must gain.
struct Budget<'a> {
    allowance: &'a mut HashMap<Pubkey, u128>,
    min_fee_gain: &'a mut HashMap<Pubkey, u128>,
}

/// Each route hop's fixed accounts and strategy accounts, cut from the instruction by
/// the counts its arguments declare; `None` if they don't add up to the accounts passed.
type HopAccounts<'a> = (&'a [AccountMeta], &'a [AccountMeta]);

fn route_hops<'a>(ix: &'a Instruction, args: &client::RouteArgs) -> Option<Vec<HopAccounts<'a>>> {
    let mut at = route_meta::FIRST_HOP;
    let mut hops = vec![];
    for hop in &args.hops {
        let fixed_end = at + route_meta::HOP_ACCOUNTS;
        let strategy_end = fixed_end + hop.strategy_accounts as usize;
        let end = strategy_end + hop.input_hook_accounts as usize;
        if end > ix.accounts.len() {
            return None;
        }
        hops.push((
            &ix.accounts[at..fixed_end],
            &ix.accounts[fixed_end..strategy_end],
        ));
        at = end;
    }
    (at == ix.accounts.len()).then_some(hops)
}

/// What a route of honest strategies must have done to every account, from its events.
#[derive(Default)]
struct RouteModel {
    /// The change in each of the trader's accounts.
    trader: HashMap<Pubkey, i128>,
    /// What each fee account (and the integrator's) gained, counting transfer fees
    /// withheld in it.
    fees: HashMap<Pubkey, i128>,
    /// The hops' vaults, which only conservation constrains.
    vaults: HashSet<Pubkey>,
}

/// Checks a successful route, step `i`: its hops' events against each other, the
/// markets and the arguments, and, when it ran alone through honest strategies, every
/// balance it could have touched.
fn check_route(
    ctx: &Context,
    i: usize,
    ix: &Instruction,
    route: &RouteStep,
    events: &[PipelineEvent],
    meta: &litesvm::types::TransactionMetadata,
    budget: &mut Budget,
) -> Result<(), String> {
    if let Some(why) = route.broken {
        return Err(format!(
            "route {i} was broken on purpose ({why}) yet succeeded"
        ));
    }
    let (before, after) = (ctx.before, ctx.after);
    let world = ctx.world;
    let args = &route.args;
    let hops = route_hops(ix, args).ok_or(format!(
        "route {i} succeeded with accounts that don't match its {} hops",
        args.hops.len()
    ))?;
    if !(1..=client::MAX_ROUTE_HOPS).contains(&hops.len()) {
        return Err(format!("route {i} of {} hops succeeded", hops.len()));
    }
    let swaps: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            PipelineEvent::Swap(s) => Some(s),
            _ => None,
        })
        .collect();
    if swaps.len() != hops.len() {
        return Err(format!(
            "route {i} of {} hops emitted {} swap events",
            hops.len(),
            swaps.len()
        ));
    }
    let trader = ix.accounts[0].pubkey;
    let integrator =
        Some(ix.accounts[route_meta::INTEGRATOR].pubkey).filter(|k| *k != client::PROGRAM_ID);
    let alone = ctx.steps.len() == 1;
    let exact = alone && route.honest();
    let refill_target = world.trader_accounts[0][0];
    let stash_lost = gross(before, &world.stash) - gross(after, &world.stash);
    let mint_account = |mint: &Pubkey| after.get(mint).and_then(|a| a.as_ref());
    let cut = |mint: &Pubkey, fee: u64| {
        fee.saturating_sub(transfer_fee_cut(mint_account(mint), ctx.epoch, fee)) as u128
    };

    let mut model = RouteModel::default();
    // What the current hop sends: `amount_in` for hop 1, then what the previous hop
    // left the trader.
    let mut sent = args.amount_in;
    // The previous hop's output account, its mint, and what the trader kept of it.
    let mut previous: Option<(Pubkey, Pubkey, u64)> = None;
    let mut outputs = vec![];
    // Pipeline's fee in the route's input mint: hop 1's, if it pays on its input.
    let mut input_pipeline_fee = 0;
    for (k, ((fixed, strategy), event)) in hops.iter().zip(&swaps).enumerate() {
        let key = |at: usize| fixed[at].pubkey;
        let market = key(hop_at::MARKET);
        let record = before
            .get(&market)
            .and_then(|a| a.as_ref())
            .filter(|a| a.owner == client::PROGRAM_ID)
            .and_then(|a| client::decode_market(&a.data))
            .ok_or(format!(
                "route {i} hop {k} went through {market}, which is not a market"
            ))?;
        let (input, output) = (key(hop_at::TRADER_INPUT), key(hop_at::TRADER_OUTPUT));
        let (in_mint, out_mint) = (key(hop_at::INPUT_MINT), key(hop_at::OUTPUT_MINT));
        if event.trader != trader
            || event.market != market
            || event.strategy != record.strategy_program
            || event.input_mint != in_mint
            || event.output_mint != out_mint
            || event.mode != TradeMode::ExactIn as u8
        {
            return Err(format!(
                "route {i} hop {k}: event doesn't describe the hop: {event:?}"
            ));
        }
        if key(hop_at::STRATEGY) != record.strategy_program
            || pda::market(&key(hop_at::STRATEGY), &key(hop_at::STATE)) != market
            || record.vault_for(&in_mint) != Some(key(hop_at::INPUT_VAULT))
            || !record.mints().contains(&out_mint)
            || in_mint == out_mint
        {
            return Err(format!(
                "route {i} hop {k}: accounts don't match market {market}"
            ));
        }
        let fee_on_output = client::FeeSide::of(&record, &out_mint) == client::FeeSide::Output;
        if let Some((prev_output, prev_mint, kept)) = previous {
            if (input, in_mint) != (prev_output, prev_mint) {
                return Err(format!(
                    "route {i} hop {k} spent {input} ({in_mint}) but hop {} paid {prev_output} ({prev_mint})",
                    k - 1
                ));
            }
            sent = if fee_on_output { kept } else { spendable(kept) };
        }
        if event.net_sent == 0 || event.net_sent > sent {
            return Err(format!(
                "route {i} hop {k}: net_sent {} outside 1..={sent}",
                event.net_sent
            ));
        }
        let (fee_mint, fee_base) = if fee_on_output {
            (out_mint, event.received)
        } else {
            (in_mint, event.net_sent)
        };
        if event.fee_mint != fee_mint {
            return Err(format!(
                "route {i} hop {k}: fee taken in {} but the market's fee side says {fee_mint}",
                event.fee_mint
            ));
        }
        if event.pipeline_fee as u128 != ceil_bps(fee_base, 25) {
            return Err(format!(
                "route {i} hop {k}: pipeline fee {} on {fee_base} (fee on output: {fee_on_output})",
                event.pipeline_fee
            ));
        }
        let fee_account = key(hop_at::FEE_ACCOUNT);
        let fee_program = mint_account(&fee_mint)
            .map(|a| a.owner)
            .ok_or("fee mint missing")?;
        let expected = pda::fee_account(&market, &fee_mint, &fee_program);
        if fee_account != expected {
            return Err(format!(
                "route {i} hop {k}: fee paid to {fee_account} instead of the market's fee account {expected}"
            ));
        }
        // The integrator fee: once, on hop 1's input.
        let (hop_integrator, bps) = if k == 0 {
            (integrator, args.integrator_fee_bps)
        } else {
            (None, 0)
        };
        if event.integrator != hop_integrator
            || event.integrator_fee as u128 != ceil_bps(event.net_sent, bps as u64)
        {
            return Err(format!(
                "route {i} hop {k}: integrator {:?} fee {} but expected {hop_integrator:?} at {bps} bps on net_sent {}",
                event.integrator, event.integrator_fee, event.net_sent
            ));
        }
        let output_fee = if fee_on_output { event.pipeline_fee } else { 0 };
        let input_fee = event.pipeline_fee - output_fee;
        let kept = event.received.checked_sub(output_fee).ok_or(format!(
            "route {i} hop {k}: fee {output_fee} out of an output of {}",
            event.received
        ))?;
        if exact {
            if event.net_sent != sent {
                return Err(format!(
                    "route {i} hop {k}: an honest strategy was sent {sent} but net_sent is {}",
                    event.net_sent
                ));
            }
            let delta = route.pays[k].unwrap_or_default();
            let paid = sent.saturating_add_signed(delta as i64);
            let arrived = paid - transfer_fee_cut(mint_account(&out_mint), ctx.epoch, paid);
            // The refill hook may top up the output account while it is measured.
            let refilled = stash_lost != 0 && output == refill_target;
            if event.received != arrived && !refilled {
                return Err(format!(
                    "route {i} hop {k}: the strategy paid {paid} ({arrived} after transfer fees) but received is {}",
                    event.received
                ));
            }
        }

        if k == 0 {
            input_pipeline_fee = input_fee;
            let fee = if fee_on_output {
                0
            } else {
                ceil_bps(args.amount_in, 25)
            };
            *budget.allowance.entry(input).or_default() += args.amount_in as u128
                + fee
                + ceil_bps(args.amount_in, args.integrator_fee_bps as u64);
        }
        *budget.min_fee_gain.entry(fee_account).or_default() += cut(&fee_mint, event.pipeline_fee);
        *model.trader.entry(input).or_default() -=
            event.net_sent as i128 + input_fee as i128 + event.integrator_fee as i128;
        *model.trader.entry(output).or_default() += kept as i128;
        *model.fees.entry(fee_account).or_default() += event.pipeline_fee as i128;
        if let Some(account) = hop_integrator {
            *budget.min_fee_gain.entry(account).or_default() += cut(&in_mint, event.integrator_fee);
            *model.fees.entry(account).or_default() += event.integrator_fee as i128;
        }
        model.vaults.insert(key(hop_at::INPUT_VAULT));
        if let Some(vault) = strategy.first() {
            model.vaults.insert(vault.pubkey);
        }
        outputs.push(output);
        previous = Some((output, out_mint, kept));
    }

    let kept = previous.map_or(0, |p| p.2);
    if kept < args.min_amount_out {
        return Err(format!(
            "route {i}: the trader kept {kept} < min {}",
            args.min_amount_out
        ));
    }
    // The return data is the route's when nothing runs after it.
    if i + 1 == ctx.steps.len() {
        if meta.return_data.program_id != client::PROGRAM_ID {
            return Err(format!(
                "route {i}: return data is from {}",
                meta.return_data.program_id
            ));
        }
        let result = client::RouteResult::try_from_slice(&meta.return_data.data)
            .map_err(|_| format!("route {i}: return data does not decode"))?;
        let expected = client::RouteResult {
            net_sent: swaps[0].net_sent,
            amount_out: kept,
            integrator_fee: swaps[0].integrator_fee,
            input_pipeline_fee,
        };
        if result != expected {
            return Err(format!(
                "route {i}: return data {result:?} but the hops say {expected:?}"
            ));
        }
    }
    let outputs_all = outputs.clone();
    // What the trader already held in intermediate tokens is never spent (a mint whose
    // permanent delegate is a market aside).
    if alone {
        let first_input = hops[0].0[hop_at::TRADER_INPUT].pubkey;
        outputs.pop();
        for account in outputs {
            let Some(old) = before
                .get(&account)
                .and_then(|a| a.as_ref())
                .and_then(token_state)
            else {
                continue;
            };
            let delegated = world
                .mints
                .iter()
                .any(|m| m.key == old.mint && m.permanent_delegate);
            let now = balance(after, &account).unwrap_or(0);
            if account != first_input && !delegated && now < old.amount {
                return Err(format!(
                    "route {i} spent the trader's own {account}: {} -> {now}",
                    old.amount
                ));
            }
        }
    }
    if exact {
        // The trader's input spend is what the return data reports, when no later hop
        // pays back into the first input account.
        let first_input = hops[0].0[hop_at::TRADER_INPUT].pubkey;
        let refilled = stash_lost != 0 && first_input == refill_target;
        if !outputs_all.contains(&first_input) && !refilled {
            let spent = balance(before, &first_input).unwrap_or(0) as i128
                - balance(after, &first_input).unwrap_or(0) as i128;
            let reported = swaps[0].net_sent as i128
                + input_pipeline_fee as i128
                + swaps[0].integrator_fee as i128;
            if spent != reported {
                return Err(format!(
                    "route {i}: the trader's input lost {spent} but net_sent + input_pipeline_fee + integrator_fee is {reported}"
                ));
            }
        }
        check_route_exact(ctx, i, &model, stash_lost)?;
    }
    Ok(())
}

/// A route of honest strategies, alone in its transaction: every token account moved
/// exactly as its events say (vaults aside), nothing else moved (but the refill hook's
/// stash into its target), and every mint is conserved across the accounts watched.
fn check_route_exact(
    ctx: &Context,
    i: usize,
    model: &RouteModel,
    stash_lost: i128,
) -> Result<(), String> {
    let (before, after) = (ctx.before, ctx.after);
    let world = ctx.world;
    let refill_target = world.trader_accounts[0][0];
    let mut seen = HashSet::new();
    // Per mint: the change in what the watched accounts hold, withheld fees included.
    let mut moved: HashMap<Pubkey, i128> = HashMap::new();
    for (key, old) in before {
        let old_token = old.as_ref().and_then(token_state);
        let new_token = after
            .get(key)
            .and_then(|a| a.as_ref())
            .and_then(token_state);
        let Some(mint) = old_token.as_ref().or(new_token.as_ref()).map(|t| t.mint) else {
            continue;
        };
        seen.insert(*key);
        let amount = |t: &Option<TokenState>| t.as_ref().map_or(0, |t| t.amount as i128);
        let changed = amount(&new_token) - amount(&old_token);
        let changed_gross = gross(after, key) - gross(before, key);
        *moved.entry(mint).or_default() += changed_gross;
        if model.vaults.contains(key) || *key == world.stash {
            continue;
        }
        if let Some(&expected) = model.trader.get(key) {
            if *key == refill_target && stash_lost != 0 {
                ctx.notes
                    .borrow_mut()
                    .push("the refill hook topped up an account of a route".into());
            } else if changed != expected {
                return Err(format!(
                    "route {i}: trader account {key} moved {changed} but its hops say {expected}"
                ));
            }
        } else if let Some(&expected) = model.fees.get(key) {
            if changed_gross != expected {
                return Err(format!(
                    "route {i}: fee account {key} gained {changed_gross} but the events say {expected}"
                ));
            }
        } else if *key == refill_target {
            if changed != stash_lost {
                return Err(format!(
                    "route {i}: the refill target moved {changed} but the stash lost {stash_lost}"
                ));
            }
        } else if changed != 0 || changed_gross != 0 {
            return Err(format!(
                "route {i} moved {changed} ({changed_gross} with withheld fees) in {key}, which takes no part in it"
            ));
        }
    }
    for key in model.trader.keys().chain(model.fees.keys()) {
        if !seen.contains(key) {
            return Err(format!("route {i}: {key} is not a watched token account"));
        }
    }
    for (mint, total) in moved {
        let minted = supply(after, &mint) - supply(before, &mint);
        if total != minted {
            return Err(format!(
                "route {i} doesn't conserve {mint}: its accounts moved {total}, its supply {minted}"
            ));
        }
    }
    Ok(())
}

/// Whether an error's name says the transaction ran out of compute.
fn out_of_compute(name: &str) -> bool {
    name.contains("ComputationalBudgetExceeded") || name.contains("ProgramFailedToComplete")
}

/// What the generator knew in advance about a step alone in its transaction: that a
/// route does what its dry run did, and that a valid table for a market without one is
/// recorded. `result` is the transaction's metadata, or its error's name.
fn check_expectation(
    steps: &[Step],
    result: Result<&litesvm::types::TransactionMetadata, &str>,
) -> Result<(), String> {
    let [step] = steps else {
        return Ok(());
    };
    // A dry run has the default compute limit; the transaction may not have had more.
    if result.is_err_and(out_of_compute) {
        return Ok(());
    }
    match (&step.kind, result) {
        (Kind::Route(route), result) => match (&route.expect, result) {
            (Some(Ok(kept)), Ok(meta)) => {
                let result = client::RouteResult::try_from_slice(&meta.return_data.data)
                    .map(|r| r.amount_out)
                    .ok();
                if result != Some(*kept) {
                    return Err(format!("route kept {result:?} but its dry run kept {kept}"));
                }
                Ok(())
            }
            (Some(Ok(kept)), Err(name)) => Err(format!(
                "route failed ({name}) but its dry run kept {kept}, and the minimum is {}",
                route.args.min_amount_out
            )),
            (Some(Err(expected)), Ok(_)) => Err(format!(
                "route succeeded but its dry run (or its minimum) said {expected}"
            )),
            (Some(Err(expected)), Err(name)) if name != expected => Err(format!(
                "route failed with {name} but its dry run (or its minimum) said {expected}"
            )),
            _ => Ok(()),
        },
        (Kind::RecordLookupTable { variant, accept }, Err(name)) if *accept => Err(format!(
            "record_lookup_table refused a {variant} table for a market without one: {name}"
        )),
        _ => Ok(()),
    }
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
    let market_rent = world.svm.minimum_balance_for_rent_exemption(8 + 365);
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
            0..=44 => world.gen_swap(&mut tape),
            45..=58 => world.gen_route(&mut tape),
            59..=64 => world.gen_sweep(&mut tape),
            65..=67 => world.gen_init_fee(&mut tape),
            68..=74 => {
                let (step, p, signers) = world.gen_create_market(&mut tape);
                pending = p;
                extra_signers.extend(signers);
                step
            }
            75..=77 => world.gen_register_existing(&mut tape),
            78..=82 => world.gen_record_lookup_table(&mut tape),
            83..=87 => world.gen_update_config(&mut tape),
            88..=90 => world.gen_register_strategy(&mut tape),
            91 | 92 => world.gen_update_metadata(&mut tape),
            _ => world.gen_raw(&mut tape),
        };
        steps.push(main);
        if tape.chance(14) {
            steps.push(match tape.u8() % 4 {
                0 => world.gen_sweep(&mut tape),
                1 => world.gen_direct_script(&mut tape),
                2 => world.gen_route(&mut tape),
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
        let legacy = Transaction::new_signed_with_payer(
            &ixs,
            Some(&world.payer.pubkey()),
            &refs,
            world.svm.latest_blockhash(),
        );
        let oversize =
            bincode::serialize(&legacy).map_or(usize::MAX, |b| b.len()) > MAX_TRANSACTION_SIZE;
        let trades = steps
            .iter()
            .any(|s| matches!(s.kind, Kind::Swap(_) | Kind::Route(_)));
        // Too large for a legacy transaction, or now and then anyway: a v0 transaction
        // through the markets' recorded lookup tables (and a table of its own if those
        // aren't enough).
        let v0 = if oversize || (trades && tape.chance(20)) {
            world.v0_transaction(&ixs, &refs, oversize)
        } else {
            None
        };
        let (tx, keys, via) = match v0 {
            Some(v0) => v0,
            None if oversize => {
                stats.oversize += 1;
                continue;
            }
            None => {
                let keys = legacy.message.account_keys.clone();
                (VersionedTransaction::from(legacy), keys, "")
            }
        };
        stats.transactions += 1;
        // Swaps are labelled with their market: `@3` for the world's market 3, `@new` for
        // one created during the case, `@?` for a substituted market account. Routes with
        // their hop count, `/scripted` if a strategy may do anything, and why they were
        // broken; lookup table records with their table.
        let label = steps
            .iter()
            .map(|s| match &s.kind {
                Kind::Swap(_) => {
                    let market = s.ix.accounts[swap_meta::MARKET].pubkey;
                    match world.markets.iter().position(|m| m.market == market) {
                        Some(i) if i < base.markets.len() => format!("swap@{i}"),
                        Some(_) => "swap@new".to_string(),
                        None => "swap@?".to_string(),
                    }
                }
                Kind::Route(r) => {
                    let mut label = format!("route{}", r.args.hops.len());
                    if !r.pays.iter().all(Option::is_some) {
                        label.push_str("/scripted");
                    }
                    if r.mutated {
                        label.push_str("/substituted");
                    }
                    if let Some(why) = r.broken {
                        label.push_str(&format!("!{why}"));
                    }
                    label
                }
                Kind::RecordLookupTable { variant, .. } => {
                    format!("record_lookup_table({variant})")
                }
                _ => s.kind.name().to_string(),
            })
            .collect::<Vec<_>>()
            .join("+")
            + via;
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
                check_success(&ctx, &keys, meta)
                    .and_then(|()| check_expectation(&steps, Ok(meta)))
                    .map_err(|e| (e, meta.logs.clone()))
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
                    match check_failure(&failed.meta.logs)
                        .and_then(|note| check_expectation(&steps, Err(&name)).map(|()| note))
                    {
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
                // A random init script can rewrite the state the market info is read
                // from, so the router may have recorded other mints than were generated.
                // Only a market recorded as expected joins the world.
                let recorded = world
                    .svm
                    .get_account(&p.market.market)
                    .and_then(|a| client::decode_market(&a.data));
                let expected: Vec<Pubkey> =
                    p.market.mints.iter().map(|&m| world.mints[m].key).collect();
                let as_expected = recorded.is_some_and(|m| {
                    m.mints() == expected.as_slice()
                        && m.vaults[..m.mint_count as usize] == p.market.vaults[..]
                });
                if as_expected {
                    // Fund the new market's fee accounts so later steps can trade.
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

/// Whether an account is a market's lookup table record. Passing an address as a
/// writable account leaves an empty system account behind in the SVM, which isn't one.
fn holds_lookup_record(account: Option<&Account>) -> bool {
    account.is_some_and(|a| {
        a.owner == client::PROGRAM_ID && client::decode_market_lookup_table(&a.data).is_some()
    })
}
