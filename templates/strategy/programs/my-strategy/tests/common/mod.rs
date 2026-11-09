//! Test setup: the real Pipeline router and this strategy in LiteSVM, a local Solana VM.
//!
//! Programs are loaded from files, so build first:
//!
//! - this strategy: `cargo build-sbf --manifest-path programs/my-strategy/Cargo.toml`
//!   writes `target/deploy/my_strategy.so`
//!   (set `SBF_OUT_DIR` to read it from somewhere else);
//! - the router: `scripts/fetch-router.sh` copies the devnet build to
//!   `programs/my-strategy/tests/fixtures/pipeline.so` (set `PIPELINE_ROUTER_SO` to use
//!   another file).
//!
//! Every helper sends real transactions through the router, the same instructions a
//! front end or desk would send on devnet.

#![allow(dead_code)]

use std::path::PathBuf;

use litesvm::LiteSVM;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey, TradeMode};
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_transaction::Transaction;

pub use pipeline_check::write_token_account;
pub use pipeline_client::interface::{QuoteResult, ResolveRequest, ResolveResult, TradeRequest};

pub const SOL: u64 = 1_000_000_000;
pub const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
pub const TOKEN_2022: Pubkey = client::TOKEN_2022_PROGRAM_ID;

/// Size of an SPL Token mint account.
const SPL_MINT_LEN: usize = 82;

/// The strategy's program address, from its `declare_id!`.
pub fn strategy_id() -> Pubkey {
    Pubkey::new_from_array(my_strategy::ID.to_bytes())
}

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read_program(path: PathBuf, hint: &str) -> Vec<u8> {
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}. {hint}", path.display()))
}

/// The strategy's compiled program.
pub fn strategy_so() -> Vec<u8> {
    let dir = std::env::var("SBF_OUT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| crate_dir().join("../../target/deploy"));
    read_program(
        dir.join("my_strategy.so"),
        "Build it first: cargo build-sbf --manifest-path programs/my-strategy/Cargo.toml",
    )
}

/// The Pipeline router's compiled program.
pub fn router_so() -> Vec<u8> {
    let path = std::env::var("PIPELINE_ROUTER_SO")
        .map(PathBuf::from)
        .unwrap_or_else(|_| crate_dir().join("tests/fixtures/pipeline.so"));
    read_program(path, "Run `scripts/fetch-router.sh` first.")
}

/// Rewrites the upgrade authority in an upgradeable program's ProgramData header, as if
/// `authority` had deployed it.
fn set_upgrade_authority(svm: &mut LiteSVM, program: &Pubkey, authority: &Pubkey) {
    let address = pda::program_data(program);
    let mut account = svm.get_account(&address).expect("program data");
    account.data[12] = 1;
    account.data[13..45].copy_from_slice(authority.as_ref());
    svm.set_account(address, account).unwrap();
}

/// A market recorded by the router.
pub struct Market {
    /// The router's Market account, the address integrators and `pipeline-check` use.
    pub market: Pubkey,
    /// The strategy's pool.
    pub pool: Pubkey,
    pub mints: [Pubkey; 2],
    pub token_programs: [Pubkey; 2],
    pub vaults: [Pubkey; 2],
}

/// A VM with the router configured and this strategy deployed and registered.
pub struct Env {
    pub svm: LiteSVM,
    /// Pays for everything, is the router's admin and the strategy's upgrade authority.
    pub admin: Keypair,
}

impl Env {
    pub fn new() -> Self {
        let mut svm = LiteSVM::new();
        let admin = Keypair::new();
        svm.airdrop(&admin.pubkey(), 1_000 * SOL).unwrap();
        svm.add_program(client::PROGRAM_ID, &router_so()).unwrap();
        set_upgrade_authority(&mut svm, &client::PROGRAM_ID, &admin.pubkey());
        svm.add_program(strategy_id(), &strategy_so()).unwrap();
        set_upgrade_authority(&mut svm, &strategy_id(), &admin.pubkey());
        let mut env = Env { svm, admin };

        // The router's one-time configuration, as on devnet: a 0.01 SOL listing fee.
        let fee_recipient = Pubkey::new_unique();
        let config = client::initialize_config(
            &env.admin.pubkey(),
            &client::ConfigArgs {
                listing_fee_lamports: 10_000_000,
                listing_fee_recipient: fee_recipient,
                treasury_owner: Pubkey::new_unique(),
            },
        );
        env.send(&[config], &[]).unwrap();

        // Register the strategy, signed by its upgrade authority.
        let admin = env.admin.pubkey();
        let register = client::register_strategy(
            &admin,
            &admin,
            &strategy_id(),
            &fee_recipient,
            "https://example.com/metadata.json",
            10_000_000,
        );
        env.send(&[register], &[]).unwrap();
        env
    }

    /// Sends a transaction paid for by the admin. Returns the router's or strategy's
    /// return data, or the error and logs.
    pub fn send(
        &mut self,
        instructions: &[Instruction],
        signers: &[&Keypair],
    ) -> Result<Vec<u8>, String> {
        self.svm.expire_blockhash();
        let mut all = vec![&self.admin];
        all.extend_from_slice(signers);
        let tx = Transaction::new_signed_with_payer(
            instructions,
            Some(&self.admin.pubkey()),
            &all,
            self.svm.latest_blockhash(),
        );
        self.svm
            .send_transaction(tx)
            .map(|meta| meta.return_data.data)
            .map_err(|failed| format!("{:?}\n{}", failed.err, failed.meta.logs.join("\n")))
    }

    /// Runs a read-only instruction in simulation and returns its return data.
    pub fn simulate(&self, instruction: Instruction) -> Result<Vec<u8>, String> {
        let tx = Transaction::new_signed_with_payer(
            &[instruction],
            Some(&self.admin.pubkey()),
            &[&self.admin],
            self.svm.latest_blockhash(),
        );
        self.svm
            .simulate_transaction(tx)
            .map(|info| info.meta.return_data.data)
            .map_err(|failed| format!("{:?}\n{}", failed.err, failed.meta.logs.join("\n")))
    }

    /// A new mint. For Token-2022, `transfer_fee_bps` adds a transfer fee.
    pub fn create_mint(
        &mut self,
        token_program: &Pubkey,
        decimals: u8,
        transfer_fee_bps: Option<u16>,
    ) -> Pubkey {
        use spl_token_2022_interface::extension::{transfer_fee, ExtensionType};
        use spl_token_2022_interface::{instruction, state};

        let mint = Keypair::new();
        let authority = self.admin.pubkey();
        let extensions = match transfer_fee_bps {
            Some(_) => vec![ExtensionType::TransferFeeConfig],
            None => vec![],
        };
        let space = if *token_program == TOKEN {
            SPL_MINT_LEN
        } else {
            ExtensionType::try_calculate_account_len::<state::Mint>(&extensions).unwrap()
        };
        let mut instructions = vec![create_account(
            &authority,
            &mint.pubkey(),
            self.svm.minimum_balance_for_rent_exemption(space),
            space as u64,
            token_program,
        )];
        if let Some(bps) = transfer_fee_bps {
            instructions.push(
                transfer_fee::instruction::initialize_transfer_fee_config(
                    token_program,
                    &mint.pubkey(),
                    Some(&authority),
                    Some(&authority),
                    bps,
                    u64::MAX,
                )
                .unwrap(),
            );
        }
        instructions.push(
            instruction::initialize_mint2(
                token_program,
                &mint.pubkey(),
                &authority,
                None,
                decimals,
            )
            .unwrap(),
        );
        self.send(&instructions, &[&mint]).unwrap();
        mint.pubkey()
    }

    /// A new wallet holding `amount` of each mint, plus some SOL.
    pub fn funded_wallet(&mut self, market: &Market, amount: u64) -> Keypair {
        let wallet = Keypair::new();
        self.svm.airdrop(&wallet.pubkey(), SOL).unwrap();
        for (mint, token_program) in market.mints.iter().zip(&market.token_programs) {
            write_token_account(&mut self.svm, mint, token_program, &wallet.pubkey(), amount)
                .unwrap();
        }
        wallet
    }

    /// Creates a pool through the router's `create_market` and makes both mints sellable
    /// with `init_fee_account`, in one transaction, exactly as `metadata.json`'s `init`
    /// describes it.
    pub fn create_market(
        &mut self,
        (mint_a, token_program_a): (Pubkey, Pubkey),
        (mint_b, token_program_b): (Pubkey, Pubkey),
        fee_bps: u16,
    ) -> Result<Market, String> {
        let program = strategy_id();
        let fee_seed = fee_bps.to_le_bytes();
        let seeds: &[&[u8]] = &[b"pool", mint_a.as_ref(), mint_b.as_ref(), &fee_seed];
        let (pool, _) = Pubkey::find_program_address(seeds, &program);
        let vault_a = pda::associated_token(&pool, &mint_a, &token_program_a);
        let vault_b = pda::associated_token(&pool, &mint_b, &token_program_b);
        let init_data = borsh::to_vec(&my_strategy::InitParams { fee_bps }).unwrap();
        let creator = self.admin.pubkey();
        let create = client::create_market(&client::CreateMarket {
            creator,
            strategy_program: program,
            market_state: pool,
            market_state_signs: false,
            init_data,
            max_creator_lamports: SOL,
            strategy_accounts: vec![
                AccountMeta::new_readonly(mint_a, false),
                AccountMeta::new_readonly(mint_b, false),
                AccountMeta::new(vault_a, false),
                AccountMeta::new(vault_b, false),
                AccountMeta::new_readonly(token_program_a, false),
                AccountMeta::new_readonly(token_program_b, false),
                AccountMeta::new_readonly(client::ASSOCIATED_TOKEN_PROGRAM_ID, false),
            ],
        });
        let market = pda::market(&program, &pool);
        let fee_a = client::init_fee_account(&creator, &market, &mint_a, &token_program_a);
        let fee_b = client::init_fee_account(&creator, &market, &mint_b, &token_program_b);
        self.send(&[create, fee_a, fee_b], &[])?;
        Ok(Market {
            market,
            pool,
            mints: [mint_a, mint_b],
            token_programs: [token_program_a, token_program_b],
            vaults: [vault_a, vault_b],
        })
    }

    /// The strategy's `deposit` action, signed by `depositor`, from their associated
    /// token accounts.
    pub fn deposit(
        &mut self,
        market: &Market,
        depositor: &Keypair,
        amount_a: u64,
        amount_b: u64,
    ) -> Result<(), String> {
        let owner = depositor.pubkey();
        let [mint_a, mint_b] = market.mints;
        let [token_program_a, token_program_b] = market.token_programs;
        let mut data = client::instruction_discriminator("deposit").to_vec();
        data.extend_from_slice(&amount_a.to_le_bytes());
        data.extend_from_slice(&amount_b.to_le_bytes());
        let accounts = vec![
            AccountMeta::new_readonly(owner, true),
            AccountMeta::new(market.pool, false),
            AccountMeta::new(
                pda::associated_token(&owner, &mint_a, &token_program_a),
                false,
            ),
            AccountMeta::new(
                pda::associated_token(&owner, &mint_b, &token_program_b),
                false,
            ),
            AccountMeta::new(market.vaults[0], false),
            AccountMeta::new(market.vaults[1], false),
            AccountMeta::new_readonly(mint_a, false),
            AccountMeta::new_readonly(mint_b, false),
            AccountMeta::new_readonly(token_program_a, false),
            AccountMeta::new_readonly(token_program_b, false),
        ];
        let deposit = Instruction::new_with_bytes(strategy_id(), &data, accounts);
        self.send(&[deposit], &[depositor]).map(|_| ())
    }

    /// The router's ten fixed trade accounts for a read-only call, with placeholders for
    /// the trader, as an integrator's quoter passes them.
    fn read_accounts(&self, market: &Market, trade: &TradeRequest) -> Vec<AccountMeta> {
        let input = trade.input_index as usize;
        let output = trade.output_index as usize;
        let placeholder = client::SYSTEM_PROGRAM_ID;
        vec![
            AccountMeta::new_readonly(market.market, false),
            AccountMeta::new_readonly(market.pool, false),
            AccountMeta::new_readonly(placeholder, false),
            AccountMeta::new_readonly(placeholder, false),
            AccountMeta::new_readonly(placeholder, false),
            AccountMeta::new_readonly(market.vaults[input], false),
            AccountMeta::new_readonly(market.mints[input], false),
            AccountMeta::new_readonly(market.mints[output], false),
            AccountMeta::new_readonly(market.token_programs[input], false),
            AccountMeta::new_readonly(market.token_programs[output], false),
        ]
    }

    /// The strategy accounts a swap needs, from `pipeline_resolve_accounts`.
    pub fn resolve_accounts(
        &self,
        market: &Market,
        trade: &TradeRequest,
    ) -> Result<Vec<AccountMeta>, String> {
        let mut known: Vec<AccountMeta> = vec![];
        for round in 0..client::interface::MAX_RESOLVE_ROUNDS {
            let request = ResolveRequest {
                trade: trade.clone(),
                round,
            };
            let mut accounts = self.read_accounts(market, trade);
            accounts.extend(
                known
                    .iter()
                    .map(|a| AccountMeta::new_readonly(a.pubkey, false)),
            );
            let instruction = strategy_call(
                client::interface::discriminator::RESOLVE_ACCOUNTS,
                &request,
                accounts,
            );
            let data = self.simulate(instruction)?;
            let result: ResolveResult = borsh::from_slice(&data).map_err(|e| e.to_string())?;
            known = result
                .accounts
                .iter()
                .map(|a| {
                    let address = Pubkey::new_from_array(a.address);
                    if a.writable {
                        AccountMeta::new(address, false)
                    } else {
                        AccountMeta::new_readonly(address, false)
                    }
                })
                .collect();
            if result.complete {
                return Ok(known);
            }
        }
        Err("account resolution did not complete".into())
    }

    /// The strategy's `pipeline_quote`, in vault amounts (before Pipeline's fee).
    pub fn quote(&self, market: &Market, trade: &TradeRequest) -> Result<QuoteResult, String> {
        let strategy_accounts = self.resolve_accounts(market, trade)?;
        let mut accounts = self.read_accounts(market, trade);
        accounts.extend(
            strategy_accounts
                .iter()
                .map(|a| AccountMeta::new_readonly(a.pubkey, false)),
        );
        let instruction = strategy_call(client::interface::discriminator::QUOTE, trade, accounts);
        let data = self.simulate(instruction)?;
        borsh::from_slice(&data).map_err(|e| e.to_string())
    }

    /// An exact-in swap through the router of `amount_in` of mint `input` for mint
    /// `output`, by `trader` from their associated token accounts.
    pub fn swap(
        &mut self,
        market: &Market,
        trader: &Keypair,
        (input, output): (usize, usize),
        amount_in: u64,
        min_amount_out: u64,
    ) -> Result<client::SwapResult, String> {
        let owner = trader.pubkey();
        let trade = exact_in(input, output, amount_in);
        let accounts = client::SwapAccounts {
            trader: owner,
            strategy_program: strategy_id(),
            market_state: market.pool,
            trader_input: pda::associated_token(
                &owner,
                &market.mints[input],
                &market.token_programs[input],
            ),
            trader_output: pda::associated_token(
                &owner,
                &market.mints[output],
                &market.token_programs[output],
            ),
            input_vault: market.vaults[input],
            input_mint: market.mints[input],
            output_mint: market.mints[output],
            input_token_program: market.token_programs[input],
            output_token_program: market.token_programs[output],
            integrator_fee_account: None,
            strategy_accounts: self.resolve_accounts(market, &trade)?,
            input_hook_accounts: vec![],
        };
        let args = client::SwapArgs {
            mode: TradeMode::ExactIn,
            amount_in,
            amount_out: 0,
            min_amount_out,
            integrator_fee_bps: 0,
            input_hook_accounts: 0,
            extra: vec![],
        };
        let data = self.send(&[client::swap(&accounts, &args)], &[trader])?;
        borsh::from_slice(&data).map_err(|e| e.to_string())
    }

    /// A token account's balance.
    pub fn balance(&self, account: &Pubkey) -> u64 {
        let data = self.svm.get_account(account).expect("token account").data;
        u64::from_le_bytes(data[64..72].try_into().unwrap())
    }
}

impl Default for Env {
    fn default() -> Self {
        Self::new()
    }
}

/// An anonymous exact-in trade request.
pub fn exact_in(input: usize, output: usize, amount: u64) -> TradeRequest {
    TradeRequest {
        mode: TradeMode::ExactIn,
        input_index: input as u8,
        output_index: output as u8,
        amount,
        trader: None,
        extra: vec![],
    }
}

fn strategy_call<T: borsh::BorshSerialize>(
    discriminator: [u8; 8],
    argument: &T,
    accounts: Vec<AccountMeta>,
) -> Instruction {
    let mut data = discriminator.to_vec();
    data.extend(borsh::to_vec(argument).unwrap());
    Instruction::new_with_bytes(strategy_id(), &data, accounts)
}

/// System program `CreateAccount`.
fn create_account(
    from: &Pubkey,
    to: &Pubkey,
    lamports: u64,
    space: u64,
    owner: &Pubkey,
) -> Instruction {
    let mut data = 0u32.to_le_bytes().to_vec();
    data.extend_from_slice(&lamports.to_le_bytes());
    data.extend_from_slice(&space.to_le_bytes());
    data.extend_from_slice(owner.as_ref());
    Instruction::new_with_bytes(
        client::SYSTEM_PROGRAM_ID,
        &data,
        vec![AccountMeta::new(*from, true), AccountMeta::new(*to, true)],
    )
}
