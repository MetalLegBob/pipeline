//! LiteSVM setup for the checker's tests: the router, the constant-product test
//! strategy and the hostile test strategy, as a strategy developer would set them up.
//!
//! Programs are loaded from `programs/target/deploy` (override with `PIPELINE_SBF_DIR`);
//! build them first with `scripts/build-programs.sh`.

#![allow(dead_code)]

use std::path::PathBuf;

use litesvm::LiteSVM;
use pipeline_check::write_token_account;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_transaction::Transaction;

pub const SOL: u64 = 1_000_000_000;
pub const TOKEN: Pubkey = client::TOKEN_PROGRAM_ID;
pub const TOKEN_2022: Pubkey = client::TOKEN_2022_PROGRAM_ID;
pub const CP_ID: Pubkey = client::pubkey!("yRptCdde1du1JYR2DcNFtLC1fkp5Asd6rE3XSvZoUFx");
pub const EVIL_ID: Pubkey = client::pubkey!("Dyn7Jwr9xtQxs4Nyiun58mTAh278SXyqWFX6331uvT72");
pub const HOOK_ID: Pubkey = client::pubkey!("BfAcC8daPZSS4iEYz1figEshePUr96gDFwtytn7DyNZ3");

pub fn sbf_dir() -> PathBuf {
    std::env::var("PIPELINE_SBF_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../programs/target/deploy")
        })
}

pub fn program(name: &str) -> Vec<u8> {
    std::fs::read(sbf_dir().join(format!("{name}.so")))
        .expect("build programs first: scripts/build-programs.sh")
}

pub struct Env {
    pub svm: LiteSVM,
    pub admin: Keypair,
    pub listing_fee_recipient: Pubkey,
}

/// Rewrites the upgrade authority in a loader-v3 program's ProgramData header.
pub fn set_upgrade_authority(svm: &mut LiteSVM, program: &Pubkey, authority: &Pubkey) {
    let address = pda::program_data(program);
    let mut account = svm.get_account(&address).expect("program data");
    account.data[12] = 1;
    account.data[13..45].copy_from_slice(authority.as_ref());
    svm.set_account(address, account).unwrap();
}

impl Env {
    /// The router deployed with its config initialised.
    pub fn new() -> Self {
        let mut svm = LiteSVM::new();
        let admin = Keypair::new();
        svm.airdrop(&admin.pubkey(), 1_000 * SOL).unwrap();
        svm.add_program(client::PROGRAM_ID, &program("pipeline"))
            .unwrap();
        set_upgrade_authority(&mut svm, &client::PROGRAM_ID, &admin.pubkey());
        let mut env = Env {
            svm,
            admin,
            listing_fee_recipient: Pubkey::new_unique(),
        };
        let ix = client::initialize_config(
            &env.admin.pubkey(),
            &client::ConfigArgs {
                listing_fee_lamports: 10_000_000,
                listing_fee_recipient: env.listing_fee_recipient,
                treasury_owner: Pubkey::new_unique(),
            },
        );
        env.send(&[ix], &[]);
        env
    }

    pub fn try_send(
        &mut self,
        instructions: &[Instruction],
        signers: &[&Keypair],
    ) -> Result<(), String> {
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
            .map(|_| ())
            .map_err(|failed| format!("{:?}\n{}", failed.err, failed.meta.logs.join("\n")))
    }

    pub fn send(&mut self, instructions: &[Instruction], signers: &[&Keypair]) {
        self.try_send(instructions, signers).unwrap();
    }

    /// Deploys and registers a strategy program.
    pub fn register(&mut self, program_id: &Pubkey, so_name: &str) {
        self.svm
            .add_program(*program_id, &program(so_name))
            .unwrap();
        let admin = self.admin.pubkey();
        set_upgrade_authority(&mut self.svm, program_id, &admin);
        let ix = client::register_strategy(
            &admin,
            &admin,
            program_id,
            &self.listing_fee_recipient,
            "https://example.com/strategy.json",
            u64::MAX,
        );
        self.send(&[ix], &[]);
    }

    /// A mint, optionally Token-2022 with a transfer fee `(bps, max)` and/or a hook.
    pub fn create_mint(
        &mut self,
        token_program: &Pubkey,
        decimals: u8,
        transfer_fee: Option<(u16, u64)>,
        transfer_hook: Option<Pubkey>,
    ) -> Pubkey {
        use spl_token_2022_interface::extension::ExtensionType;
        let mint = Keypair::new();
        let mut extensions = vec![];
        if transfer_fee.is_some() {
            extensions.push(ExtensionType::TransferFeeConfig);
        }
        if transfer_hook.is_some() {
            extensions.push(ExtensionType::TransferHook);
        }
        let space = if *token_program == TOKEN {
            82
        } else {
            ExtensionType::try_calculate_account_len::<spl_token_2022_interface::state::Mint>(
                &extensions,
            )
            .unwrap()
        };
        let authority = self.admin.pubkey();
        let mut ixs = vec![create_account(
            &authority,
            &mint.pubkey(),
            self.svm.minimum_balance_for_rent_exemption(space),
            space as u64,
            token_program,
        )];
        if let Some((bps, max)) = transfer_fee {
            ixs.push(
                spl_token_2022_interface::extension::transfer_fee::instruction::initialize_transfer_fee_config(
                    token_program,
                    &mint.pubkey(),
                    Some(&authority),
                    Some(&authority),
                    bps,
                    max,
                )
                .unwrap(),
            );
        }
        if let Some(hook) = transfer_hook {
            ixs.push(
                spl_token_2022_interface::extension::transfer_hook::instruction::initialize(
                    token_program,
                    &mint.pubkey(),
                    Some(authority),
                    Some(hook),
                )
                .unwrap(),
            );
        }
        ixs.push(
            spl_token_2022_interface::instruction::initialize_mint2(
                token_program,
                &mint.pubkey(),
                &authority,
                None,
                decimals,
            )
            .unwrap(),
        );
        self.send(&ixs, &[&mint]);
        mint.pubkey()
    }

    /// A Token-2022 mint whose test hook approves every transfer.
    pub fn hooked_mint(&mut self) -> Pubkey {
        if self.svm.get_account(&HOOK_ID).is_none() {
            self.svm
                .add_program(HOOK_ID, &program("test_hook"))
                .unwrap();
        }
        let mint = self.create_mint(&TOKEN_2022, 6, None, Some(HOOK_ID));
        let ix = Instruction::new_with_bytes(
            HOOK_ID,
            b"tsthkini",
            vec![
                AccountMeta::new(self.admin.pubkey(), true),
                AccountMeta::new(hook_validation(&mint), false),
                AccountMeta::new(
                    Pubkey::find_program_address(&[b"whitelist", mint.as_ref()], &HOOK_ID).0,
                    false,
                ),
                AccountMeta::new_readonly(mint, false),
                AccountMeta::new_readonly(client::SYSTEM_PROGRAM_ID, false),
            ],
        );
        self.send(&[ix], &[]);
        mint
    }

    /// Gives `owner` `amount` of `mint` by writing their token account directly.
    pub fn give(
        &mut self,
        mint: &Pubkey,
        token_program: &Pubkey,
        owner: &Pubkey,
        amount: u64,
    ) -> Pubkey {
        write_token_account(&mut self.svm, mint, token_program, owner, amount).unwrap()
    }
}

pub fn hook_validation(mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"extra-account-metas", mint.as_ref()], &HOOK_ID).0
}

/// The accounts Token-2022 needs to run the test hook for `mint`.
pub fn hook_accounts(mint: &Pubkey) -> Vec<AccountMeta> {
    vec![
        AccountMeta::new_readonly(HOOK_ID, false),
        AccountMeta::new_readonly(hook_validation(mint), false),
    ]
}

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

// ------------------------------------------------------------------ CP strategy

pub struct Market {
    pub market: Pubkey,
    pub state: Pubkey,
    pub mint_a: Pubkey,
    pub mint_b: Pubkey,
}

pub fn cp_pool(mint_a: &Pubkey, mint_b: &Pubkey, fee_bps: u16) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"pool",
            mint_a.as_ref(),
            mint_b.as_ref(),
            &fee_bps.to_le_bytes(),
        ],
        &CP_ID,
    )
    .0
}

/// A constant-product market between two mints with `reserve` of each deposited (none
/// if `reserve` is 0). Fee accounts are created only if `fee_accounts` is set.
pub fn cp_market(
    env: &mut Env,
    (mint_a, tp_a): (Pubkey, Pubkey),
    (mint_b, tp_b): (Pubkey, Pubkey),
    reserve: u64,
    fee_accounts: bool,
) -> Market {
    if env.svm.get_account(&pda::strategy(&CP_ID)).is_none() {
        env.register(&CP_ID, "test_strategy_cp");
    }
    let fee_bps: u16 = 30;
    let pool = cp_pool(&mint_a, &mint_b, fee_bps);
    let vault_a = pda::associated_token(&pool, &mint_a, &tp_a);
    let vault_b = pda::associated_token(&pool, &mint_b, &tp_b);
    let creator = env.admin.pubkey();
    let ix = client::create_market(&client::CreateMarket {
        creator,
        strategy_program: CP_ID,
        market_state: pool,
        market_state_signs: false,
        init_data: borsh::to_vec(&fee_bps).unwrap(),
        max_creator_lamports: SOL,
        strategy_accounts: vec![
            AccountMeta::new_readonly(mint_a, false),
            AccountMeta::new_readonly(mint_b, false),
            AccountMeta::new(vault_a, false),
            AccountMeta::new(vault_b, false),
            AccountMeta::new_readonly(tp_a, false),
            AccountMeta::new_readonly(tp_b, false),
            AccountMeta::new_readonly(client::ASSOCIATED_TOKEN_PROGRAM_ID, false),
        ],
    });
    env.send(&[ix], &[]);
    let market = pda::market(&CP_ID, &pool);
    if fee_accounts {
        env.send(
            &[
                client::init_fee_account(&creator, &market, &mint_a, &tp_a),
                client::init_fee_account(&creator, &market, &mint_b, &tp_b),
            ],
            &[],
        );
    }

    let market_address = market;
    let market = Market {
        market: market_address,
        state: pool,
        mint_a,
        mint_b,
    };
    if reserve == 0 {
        return market;
    }
    let lp = Keypair::new();
    env.svm.airdrop(&lp.pubkey(), SOL).unwrap();
    let lp_a = env.give(&mint_a, &tp_a, &lp.pubkey(), reserve * 2);
    let lp_b = env.give(&mint_b, &tp_b, &lp.pubkey(), reserve * 2);
    let mut data = client::instruction_discriminator("deposit").to_vec();
    data.extend_from_slice(&reserve.to_le_bytes());
    data.extend_from_slice(&reserve.to_le_bytes());
    let mut accounts = vec![
        AccountMeta::new_readonly(lp.pubkey(), true),
        AccountMeta::new(pool, false),
        AccountMeta::new(lp_a, false),
        AccountMeta::new(lp_b, false),
        AccountMeta::new(vault_a, false),
        AccountMeta::new(vault_b, false),
        AccountMeta::new_readonly(mint_a, false),
        AccountMeta::new_readonly(mint_b, false),
        AccountMeta::new_readonly(tp_a, false),
        AccountMeta::new_readonly(tp_b, false),
    ];
    for (mint, tp) in [(mint_a, tp_a), (mint_b, tp_b)] {
        if tp == TOKEN_2022 && env.svm.get_account(&hook_validation(&mint)).is_some() {
            accounts.extend(hook_accounts(&mint));
        }
    }
    env.send(
        &[Instruction::new_with_bytes(CP_ID, &data, accounts)],
        &[&lp],
    );
    market
}

/// A CP market between two new SPL Token mints (6 and 9 decimals).
pub fn plain_cp_market(env: &mut Env) -> Market {
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 9, None, None);
    cp_market(
        env,
        (mint_a, TOKEN),
        (mint_b, TOKEN),
        1_000_000_000_000,
        true,
    )
}

// ------------------------------------------------------------------ hostile strategy

/// A market on the hostile strategy, 1:1 with 1,000,000,000 of each mint in its vaults.
/// Its behaviour is picked per trade by the first byte of `TradeRequest::extra`.
pub fn evil_market(env: &mut Env) -> Market {
    if env.svm.get_account(&pda::strategy(&EVIL_ID)).is_none() {
        env.register(&EVIL_ID, "test_strategy_evil");
    }
    let mint_a = env.create_mint(&TOKEN, 6, None, None);
    let mint_b = env.create_mint(&TOKEN, 6, None, None);
    let state =
        Pubkey::find_program_address(&[b"evil", mint_a.as_ref(), mint_b.as_ref()], &EVIL_ID).0;
    let vault_a = env.give(&mint_a, &TOKEN, &state, 1_000_000_000);
    let vault_b = env.give(&mint_b, &TOKEN, &state, 1_000_000_000);
    let creator = env.admin.pubkey();
    let ix = client::create_market(&client::CreateMarket {
        creator,
        strategy_program: EVIL_ID,
        market_state: state,
        market_state_signs: false,
        init_data: vec![0],
        max_creator_lamports: SOL,
        strategy_accounts: vec![
            AccountMeta::new_readonly(mint_a, false),
            AccountMeta::new_readonly(mint_b, false),
            AccountMeta::new(vault_a, false),
            AccountMeta::new(vault_b, false),
        ],
    });
    env.send(&[ix], &[]);
    let market = pda::market(&EVIL_ID, &state);
    env.send(
        &[
            client::init_fee_account(&creator, &market, &mint_a, &TOKEN),
            client::init_fee_account(&creator, &market, &mint_b, &TOKEN),
        ],
        &[],
    );
    Market {
        market,
        state,
        mint_a,
        mint_b,
    }
}
