//! The checks themselves. Every check that trades does so in its own clone of the
//! prepared VM, so checks are independent of each other and the caller's VM is never
//! modified.
//!
//! Quotes go through `pipeline-quoter` in a separate VM that holds only the market's
//! fixed accounts and the accounts `pipeline_resolve_accounts` listed: exactly what an
//! integrator streaming account data would have. Swaps go through the real router.

use std::collections::BTreeSet;

use borsh::BorshDeserialize;
use litesvm::LiteSVM;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use pipeline_quoter::{MarketView, QuoteRequest, Quoter, TraderQuote};
use pipeline_strategy_interface::{self as psi, fees, TradeMode};
use serde_json::{json, Value};
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_transaction::Transaction;

use crate::hooks;
use crate::report::{
    summarize, CheckResult, MarketSummary, MintSummary, OptionsSummary, ProgramSummary, Report,
    Source, Status, SCHEMA,
};
use crate::svm::{self as vm, MintFacts};

/// Every check, in report order: (id, title).
pub const CHECKS: &[(&str, &str)] = &[
    ("market_account", "Market, registration and router config"),
    (
        "market_info",
        "pipeline_market_info matches the recorded market",
    ),
    ("fee_accounts", "Pipeline fee accounts exist for every mint"),
    (
        "mint_extensions",
        "Token-2022 extensions that affect trading",
    ),
    ("resolve_accounts", "pipeline_resolve_accounts completes"),
    ("transaction_size", "A swap fits in one transaction"),
    ("quotes", "Quotes in every direction and size"),
    ("quote_matches_swap", "Quote equals the router swap"),
    (
        "min_out_enforced",
        "Asking for one unit more than the quote fails",
    ),
    ("determinism", "Repeatable, trader-independent quotes"),
    ("compute", "Compute units of a router swap"),
    ("exact_out_refund", "Exact-out refunds unused input"),
    ("round_trip", "Buying then selling straight back"),
    ("dust_and_zero", "Zero and one-unit trades"),
];

/// Solana's limit on a serialised transaction.
pub const MAX_TRANSACTION_SIZE: usize = 1_232;
/// Accounts one transaction can lock.
pub const MAX_TRANSACTION_ACCOUNTS: usize = 64;
/// The most compute a transaction can request.
pub const MAX_COMPUTE_UNITS: u32 = 1_400_000;
/// The router's `InsufficientOutput` error number (Anchor numbers errors from 6000).
pub const ROUTER_INSUFFICIENT_OUTPUT: u32 = 6021;
const TRADER_LAMPORTS: u64 = 100_000_000_000;

/// How to exercise the market.
#[derive(Clone, Debug)]
pub struct CheckOptions {
    /// Exact-in sizes (in the input mint's smallest units) to use in every direction
    /// instead of sizes derived from the vault balances.
    pub amounts: Option<Vec<u64>>,
    /// Strategy-specific `TradeRequest::extra` bytes for every quote and swap.
    pub extra: Vec<u8>,
    /// Compute cap for a strategy's `pipeline_quote`, as integrators' quoters apply it.
    pub quote_compute_limit: u32,
    /// Warn when a router swap uses more compute than this.
    pub compute_warn_units: u64,
}

impl Default for CheckOptions {
    fn default() -> Self {
        CheckOptions {
            amounts: None,
            extra: vec![],
            quote_compute_limit: pipeline_quoter::DEFAULT_COMPUTE_LIMIT,
            compute_warn_units: 200_000,
        }
    }
}

/// Runs every check against `market` in `svm`, which must hold the router (with its
/// config), the strategy program, the Market account and the accounts its trades use,
/// with liquidity already added. `svm` is not modified: trades run in clones of it.
pub fn check_market(svm: &LiteSVM, market: Pubkey, options: &CheckOptions) -> Report {
    let clock: solana_clock::Clock = svm.get_sysvar();
    let source = Source {
        kind: "svm".into(),
        cluster: None,
        genesis_hash: None,
        slot: clock.slot,
        unix_timestamp: clock.unix_timestamp,
    };
    check_market_with_source(svm, market, options, source)
}

/// [`check_market`] with an explicit description of where the state came from, for
/// markets cloned from a cluster.
pub fn check_market_with_source(
    svm: &LiteSVM,
    market: Pubkey,
    options: &CheckOptions,
    source: Source,
) -> Report {
    let mut results = Results::default();
    if let Some(prepared) = prepare(svm, market, options, &mut results) {
        run_checks(prepared, &mut results);
    }
    let record = svm
        .get_account(&market)
        .filter(|a| a.owner == client::PROGRAM_ID)
        .and_then(|a| client::decode_market(&a.data));
    let strategy_program = record.as_ref().map(|r| r.strategy_program);
    let checks = results.finish();
    Report {
        schema: SCHEMA.into(),
        checker_version: env!("CARGO_PKG_VERSION").into(),
        generated_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        source,
        router: program_summary(svm, &client::PROGRAM_ID),
        strategy: match strategy_program {
            Some(program) => program_summary(svm, &program),
            None => ProgramSummary {
                program_id: String::new(),
                loader: None,
                executable_sha256: None,
                executable_len: None,
                upgrade_authority: None,
                deploy_slot: None,
            },
        },
        strategy_metadata_uri: strategy_program
            .and_then(|program| svm.get_account(&pda::strategy(&program)))
            .and_then(|a| client::decode_strategy(&a.data))
            .map(|s| s.metadata_uri),
        market: market_summary(svm, &market, record.as_ref()),
        options: OptionsSummary {
            amounts: options.amounts.clone(),
            extra: vm::hex(&options.extra),
            quote_compute_limit: options.quote_compute_limit,
        },
        summary: summarize(&checks),
        checks,
    }
}

fn program_summary(svm: &LiteSVM, program: &Pubkey) -> ProgramSummary {
    let facts = vm::program_facts(svm, program);
    let hash = facts.as_ref().map(|f| vm::executable_sha256(&f.elf));
    ProgramSummary {
        program_id: program.to_string(),
        loader: facts.as_ref().map(|f| f.loader.to_string()),
        executable_sha256: hash.as_ref().map(|h| h.0.clone()),
        executable_len: hash.map(|h| h.1),
        upgrade_authority: facts
            .as_ref()
            .and_then(|f| f.upgrade_authority)
            .map(|a| a.to_string()),
        deploy_slot: facts.and_then(|f| f.deploy_slot),
    }
}

fn market_summary(
    svm: &LiteSVM,
    market: &Pubkey,
    record: Option<&client::Market>,
) -> MarketSummary {
    let Some(record) = record else {
        return MarketSummary {
            address: market.to_string(),
            market_state: None,
            supports_exact_out: None,
            mints: vec![],
        };
    };
    let mints = record
        .mints()
        .iter()
        .zip(&record.vaults)
        .map(|(mint, vault)| {
            let facts = vm::mint_facts(svm, mint).ok();
            let token_program = facts
                .as_ref()
                .map_or(client::TOKEN_PROGRAM_ID, |f| f.token_program);
            let fee_account = pda::fee_account(market, mint, &token_program);
            MintSummary {
                address: mint.to_string(),
                token_program: token_program.to_string(),
                decimals: facts.as_ref().map_or(0, |f| f.decimals),
                mint_authority: facts
                    .as_ref()
                    .and_then(|f| f.mint_authority)
                    .map(|k| k.to_string()),
                freeze_authority: facts
                    .as_ref()
                    .and_then(|f| f.freeze_authority)
                    .map(|k| k.to_string()),
                extensions: facts.map(|f| f.extensions).unwrap_or_default(),
                vault: vault.to_string(),
                vault_balance: vm::token_amount(svm, vault),
                fee_account: fee_account.to_string(),
                fee_account_exists: svm.get_account(&fee_account).is_some(),
            }
        })
        .collect();
    MarketSummary {
        address: market.to_string(),
        market_state: Some(record.market_state.to_string()),
        supports_exact_out: Some(record.supports_exact_out),
        mints,
    }
}

// ---------------------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------------------

#[derive(Default)]
struct Results {
    items: Vec<CheckResult>,
    /// Why the remaining checks did not run, if an early check stopped them.
    stopped: Option<String>,
}

impl Results {
    fn add(&mut self, id: &str, status: Status, message: impl Into<String>, details: Value) {
        let title = CHECKS
            .iter()
            .find(|(check, _)| *check == id)
            .map(|(_, title)| title.to_string())
            .expect("every check is listed in CHECKS");
        self.items.push(CheckResult {
            id: id.into(),
            title,
            status,
            message: message.into(),
            details,
        });
    }

    fn finish(mut self) -> Vec<CheckResult> {
        let stopped = self
            .stopped
            .take()
            .unwrap_or_else(|| "Not run.".to_string());
        CHECKS
            .iter()
            .map(|(id, title)| {
                self.items
                    .iter()
                    .find(|c| c.id == *id)
                    .cloned()
                    .unwrap_or_else(|| CheckResult {
                        id: id.to_string(),
                        title: title.to_string(),
                        status: Status::Skip,
                        message: stopped.clone(),
                        details: Value::Null,
                    })
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------------------
// Preparation and the market_account check
// ---------------------------------------------------------------------------------------

struct Prepared {
    /// The caller's VM with the router's pause lifted and missing fee accounts created.
    base: LiteSVM,
    market: Pubkey,
    record: client::Market,
    view: MarketView,
    mints: Vec<MintFacts>,
    options: CheckOptions,
}

impl Prepared {
    /// A new trader with a random key. Fixed keys would let a strategy recognise the
    /// checker's traders and treat them differently from everyone else.
    fn trader(&self) -> Keypair {
        Keypair::new()
    }

    fn mint_count(&self) -> usize {
        self.mints.len()
    }

    fn directions(&self) -> Vec<(usize, usize)> {
        let n = self.mint_count();
        (0..n)
            .flat_map(|i| (0..n).filter(move |j| *j != i).map(move |j| (i, j)))
            .collect()
    }

    fn vault_balance(&self, index: usize) -> u64 {
        vm::token_amount(&self.base, &self.record.vaults[index]).unwrap_or(0)
    }

    fn exact_in_sizes(&self, input: usize) -> Vec<(String, u64)> {
        exact_in_sizes(
            self.vault_balance(input),
            self.mints[input].decimals,
            &self.options,
        )
    }

    /// The representative exact-in size for single-size checks.
    fn medium(&self, input: usize) -> Option<u64> {
        let sizes = self.exact_in_sizes(input);
        sizes
            .iter()
            .find(|(label, _)| label == "medium")
            .or_else(|| sizes.get(sizes.len() / 2))
            .map(|(_, amount)| *amount)
    }

    fn exact_out_sizes(&self, output: usize) -> Vec<(String, u64)> {
        if !self.record.supports_exact_out {
            return vec![];
        }
        exact_out_sizes(self.vault_balance(output))
    }

    fn cases(&self) -> Vec<Case> {
        let mut cases = vec![];
        for (input, output) in self.directions() {
            let mut sizes = vec![("dust".to_string(), 1)];
            sizes.extend(self.exact_in_sizes(input));
            for (label, amount) in sizes {
                cases.push(Case {
                    input,
                    output,
                    mode: TradeMode::ExactIn,
                    label,
                    amount,
                });
            }
            for (label, amount) in self.exact_out_sizes(output) {
                cases.push(Case {
                    input,
                    output,
                    mode: TradeMode::ExactOut,
                    label,
                    amount,
                });
            }
        }
        cases
    }

    fn token_program(&self, index: usize) -> Pubkey {
        self.mints[index].token_program
    }

    /// A fresh clone of the prepared VM with `trader` funded: SOL for fees,
    /// `input_amount` of the input mint and an empty output account.
    fn funded_clone(
        &self,
        trader: &Keypair,
        input: usize,
        output: usize,
        input_amount: u64,
    ) -> Result<LiteSVM, String> {
        let mut svm = self.base.clone();
        vm::fund_lamports(&mut svm, &trader.pubkey(), TRADER_LAMPORTS);
        vm::write_token_account(
            &mut svm,
            &self.mints[input].address,
            &self.token_program(input),
            &trader.pubkey(),
            input_amount,
        )?;
        vm::ensure_token_account(
            &mut svm,
            &self.mints[output].address,
            &self.token_program(output),
            &trader.pubkey(),
        )?;
        Ok(svm)
    }
}

/// Exact-in sizes for an input mint, without dust: explicit amounts if given, otherwise
/// small, medium and large fractions of the input vault's balance (or of one whole token
/// when the vault is nearly empty, as on the buying side of some launch curves).
pub(crate) fn exact_in_sizes(
    reserve: u64,
    decimals: u8,
    options: &CheckOptions,
) -> Vec<(String, u64)> {
    if let Some(amounts) = &options.amounts {
        return amounts.iter().map(|a| ("custom".to_string(), *a)).collect();
    }
    let sizes = if reserve >= 2_000_000 {
        [reserve / 100_000, reserve / 1_000, reserve / 20]
    } else {
        let unit = 10u64.saturating_pow(decimals as u32);
        [(unit / 100).max(2), unit, unit.saturating_mul(100)]
    };
    let mut seen = BTreeSet::new();
    ["small", "medium", "large"]
        .iter()
        .zip(sizes)
        .filter(|(_, amount)| *amount > 1 && seen.insert(*amount))
        .map(|(label, amount)| (label.to_string(), amount))
        .collect()
}

/// Exact-out sizes (what the trader receives): fractions of the output vault's balance.
pub(crate) fn exact_out_sizes(reserve: u64) -> Vec<(String, u64)> {
    if reserve < 2_000 {
        return vec![];
    }
    vec![
        ("medium".to_string(), reserve / 1_000),
        ("large".to_string(), reserve / 20),
    ]
}

fn prepare(
    svm: &LiteSVM,
    market: Pubkey,
    options: &CheckOptions,
    results: &mut Results,
) -> Option<Prepared> {
    const ID: &str = "market_account";
    let stop = |results: &mut Results, message: String| {
        results.add(ID, Status::Fail, message, Value::Null);
        results.stopped = Some("Not run: the market could not be loaded.".into());
        None
    };

    if vm::program_facts(svm, &client::PROGRAM_ID).is_none() {
        return stop(
            results,
            format!(
                "The Pipeline router ({}) is not deployed in this VM.",
                client::PROGRAM_ID
            ),
        );
    }
    let Some(config) = svm
        .get_account(&pda::config())
        .and_then(|a| client::decode_config(&a.data))
    else {
        return stop(
            results,
            "The router's Config account is missing; call initialize_config first.".into(),
        );
    };
    let Some(account) = svm.get_account(&market) else {
        return stop(results, format!("Market account {market} does not exist."));
    };
    let record = match client::decode_market(&account.data) {
        Some(record) if account.owner == client::PROGRAM_ID => record,
        _ => {
            return stop(
                results,
                format!("{market} is not a Pipeline Market account (wrong owner or layout)."),
            )
        }
    };
    let strategy = record.strategy_program;
    if vm::program_facts(svm, &strategy).is_none() {
        return stop(
            results,
            format!("Strategy program {strategy} is not deployed (or its ProgramData is missing)."),
        );
    }
    match svm.get_account(&record.market_state) {
        Some(state) if state.owner == strategy => {}
        Some(state) => {
            return stop(
                results,
                format!(
                    "Market state {} is owned by {}, not the strategy.",
                    record.market_state, state.owner
                ),
            )
        }
        None => {
            return stop(
                results,
                format!("Market state {} does not exist.", record.market_state),
            )
        }
    }
    let mut mints = vec![];
    for mint in record.mints() {
        match vm::mint_facts(svm, mint) {
            Ok(facts) => mints.push(facts),
            Err(error) => return stop(results, format!("{error}.")),
        }
    }

    let mut status = Status::Pass;
    let mut notes = vec![];
    let registered = svm
        .get_account(&pda::strategy(&strategy))
        .and_then(|a| client::decode_strategy(&a.data));
    if registered.is_none() {
        status = Status::Fail;
        notes.push(format!(
            "Strategy {strategy} is not registered with the router."
        ));
    }
    let missing_vaults: Vec<String> = record.vaults[..record.mint_count as usize]
        .iter()
        .filter(|v| svm.get_account(v).is_none())
        .map(|v| v.to_string())
        .collect();
    if !missing_vaults.is_empty() {
        status = Status::Fail;
        notes.push(format!(
            "Vault account(s) {} do not exist, so input in that mint has nowhere to go.",
            missing_vaults.join(", ")
        ));
    }

    let mut base = svm.clone();
    if config.paused {
        status = status.max(Status::Warn);
        notes.push(
            "The router is paused here, so swaps would fail; the checks ran with the pause lifted locally."
                .into(),
        );
        let mut account = base.get_account(&pda::config()).unwrap();
        // Config layout: 8-byte discriminator, admin (32), then the `paused` flag.
        account.data[40] = 0;
        base.set_account(pda::config(), account).unwrap();
    }
    if notes.is_empty() {
        notes.push(format!(
            "Market of registered strategy {strategy} with {} mints; router config found.",
            mints.len()
        ));
    }
    results.add(
        ID,
        status,
        notes.join(" "),
        json!({
            "strategy_registered": registered.is_some(),
            "router_paused": config.paused,
            "mint_count": mints.len(),
        }),
    );

    let token_programs = mints.iter().map(|m| m.token_program).collect();
    let view = MarketView::from_market(market, &record, token_programs).ok()?;
    Some(Prepared {
        base,
        market,
        record,
        view,
        mints,
        options: options.clone(),
    })
}

fn run_checks(mut prepared: Prepared, results: &mut Results) {
    check_market_info(&prepared, results);
    // Creates missing fee accounts in the prepared VM, so later checks can sell every mint.
    check_fee_accounts(&mut prepared, results);
    let p = &prepared;
    check_mint_extensions(p, results);

    let mut quotes = match QuoteVm::new(&p.base, p) {
        Ok(quotes) => quotes,
        Err(error) => {
            results.add(
                "resolve_accounts",
                Status::Fail,
                format!("The strategy could not be loaded into a quoter: {error}"),
                Value::Null,
            );
            results.stopped = Some("Not run: the strategy could not be quoted.".into());
            return;
        }
    };
    let table: Vec<QuoteRow> = p
        .cases()
        .into_iter()
        .map(|case| {
            let attempt = quotes.quote(&p.base, p, &case, None);
            QuoteRow { case, attempt }
        })
        .collect();
    check_resolve_accounts(p, &table, results);
    check_transaction_size(p, &table, results);
    check_quotes(p, &table, results);
    let swaps = check_quote_matches_swap(p, &mut quotes, &table, results);
    check_min_out(p, &swaps, results);
    check_determinism(p, &mut quotes, results);
    check_compute(p, &swaps, results);
    check_exact_out_refund(p, &mut quotes, &table, results);
    check_round_trip(p, &mut quotes, results);
    check_dust_and_zero(p, &mut quotes, &table, results);
}

// ---------------------------------------------------------------------------------------
// Quoting
// ---------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Case {
    input: usize,
    output: usize,
    mode: TradeMode,
    label: String,
    amount: u64,
}

impl Case {
    fn name(&self) -> String {
        let mode = match self.mode {
            TradeMode::ExactIn => "exact-in",
            TradeMode::ExactOut => "exact-out",
        };
        format!(
            "mint {}→{} {mode} {} ({})",
            self.input, self.output, self.amount, self.label
        )
    }

    fn direction(&self) -> String {
        format!("{}→{}", self.input, self.output)
    }
}

#[derive(Clone, Debug)]
struct Resolved {
    accounts: Vec<AccountMeta>,
    rounds: u8,
}

#[derive(Clone, Debug)]
struct QuoteAttempt {
    resolved: Result<Resolved, String>,
    /// `None` if resolution failed.
    quote: Option<Result<TraderQuote, String>>,
}

impl QuoteAttempt {
    fn ok(&self) -> Option<(&Resolved, &TraderQuote)> {
        match (&self.resolved, &self.quote) {
            (Ok(resolved), Some(Ok(quote))) => Some((resolved, quote)),
            _ => None,
        }
    }

    fn error(&self) -> Option<String> {
        match (&self.resolved, &self.quote) {
            (Err(error), _) => Some(format!("account resolution failed: {error}")),
            (_, Some(Err(error))) => Some(error.clone()),
            _ => None,
        }
    }
}

struct QuoteRow {
    case: Case,
    attempt: QuoteAttempt,
}

/// A `pipeline-quoter` fed from a VM with only the accounts an integrator would know
/// about: the market's fixed accounts and whatever account resolution lists.
struct QuoteVm {
    quoter: Quoter,
}

impl QuoteVm {
    fn new(svm: &LiteSVM, p: &Prepared) -> Result<Self, String> {
        let mut quoter = Quoter::new();
        quoter.set_compute_limit(p.options.quote_compute_limit);
        let clock: solana_clock::Clock = svm.get_sysvar();
        quoter.set_clock(clock.slot, clock.epoch, clock.unix_timestamp);
        let mut keys = vec![
            p.record.strategy_program,
            p.market,
            pda::config(),
            p.record.market_state,
        ];
        keys.extend(p.record.mints());
        keys.extend(&p.record.vaults[..p.mint_count()]);
        sync(&mut quoter, svm, &keys).map_err(|e| e.to_string())?;
        Ok(QuoteVm { quoter })
    }

    fn set_clock_from(&mut self, svm: &LiteSVM) {
        let clock: solana_clock::Clock = svm.get_sysvar();
        self.quoter
            .set_clock(clock.slot, clock.epoch, clock.unix_timestamp);
    }

    fn quote(
        &mut self,
        svm: &LiteSVM,
        p: &Prepared,
        case: &Case,
        trader: Option<Pubkey>,
    ) -> QuoteAttempt {
        let request = QuoteRequest {
            input_mint: p.mints[case.input].address,
            output_mint: p.mints[case.output].address,
            mode: case.mode,
            amount: case.amount,
            trader,
            integrator_fee_bps: 0,
            extra: p.options.extra.clone(),
        };
        let mut rounds = 1u8;
        let resolved = self
            .quoter
            .try_resolve_accounts(&p.view, &request, |quoter, keys| {
                rounds += 1;
                sync(quoter, svm, keys)
            });
        let accounts = match resolved {
            Ok(accounts) => accounts,
            Err(error) => {
                return QuoteAttempt {
                    resolved: Err(error.to_string()),
                    quote: None,
                }
            }
        };
        let keys: Vec<Pubkey> = accounts.iter().map(|m| m.pubkey).collect();
        if let Err(error) = sync(&mut self.quoter, svm, &keys) {
            return QuoteAttempt {
                resolved: Err(error.to_string()),
                quote: None,
            };
        }
        let quote = self
            .quoter
            .quote(&p.view, &request, &accounts)
            .map_err(|e| e.to_string());
        QuoteAttempt {
            resolved: Ok(Resolved { accounts, rounds }),
            quote: Some(quote),
        }
    }
}

/// Copies `keys` from the VM into the quoter: data accounts as they are, programs by
/// loading their executables. Sysvars and programs the quoter already has are skipped.
fn sync(
    quoter: &mut Quoter,
    svm: &LiteSVM,
    keys: &[Pubkey],
) -> Result<(), pipeline_quoter::QuoteError> {
    for key in keys {
        let Some(account) = svm.get_account(key) else {
            continue;
        };
        if account.owner == vm::SYSVAR_OWNER {
            continue;
        }
        if account.executable {
            if quoter.account(key).is_some() {
                continue;
            }
            if let Some(facts) = vm::program_facts(svm, key) {
                quoter.load_program(*key, &facts.elf)?;
            }
            continue;
        }
        quoter.set_account(*key, account)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Swapping
// ---------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct SwapRun {
    outcome: Result<client::SwapResult, String>,
    compute_units: u64,
    strategy_units: Option<u64>,
    out_of_compute: bool,
    /// The trader's input balance before minus after.
    spent: u64,
    /// The trader's output balance after minus before.
    received: u64,
}

/// Swap accounts for `trader`, with transfer-hook accounts resolved for the input
/// transfer (trader to vault) and the output transfer (vault to trader).
#[allow(clippy::too_many_arguments)]
fn swap_accounts(
    svm: &LiteSVM,
    p: &Prepared,
    trader: &Pubkey,
    input: usize,
    output: usize,
    amount: u64,
    output_amount: u64,
    resolved: &[AccountMeta],
) -> client::SwapAccounts {
    let input_mint = p.mints[input].address;
    let output_mint = p.mints[output].address;
    let trader_input = pda::associated_token(trader, &input_mint, &p.token_program(input));
    let trader_output = pda::associated_token(trader, &output_mint, &p.token_program(output));
    let account_data = |key: &Pubkey| svm.get_account(key).map(|a| a.data);
    let mut strategy_accounts = resolved.to_vec();
    let mut input_hook_accounts = vec![];
    // Unresolvable hook accounts are left out; the swap then fails and says why.
    if let Some(hook) = p.mints[input].transfer_hook_program {
        let transfer = hooks::Transfer {
            source: trader_input,
            mint: input_mint,
            destination: p.record.vaults[input],
            authority: *trader,
            amount,
        };
        input_hook_accounts = hooks::resolve(&hook, &transfer, account_data).unwrap_or_default();
    }
    if let Some(hook) = p.mints[output].transfer_hook_program {
        let vault = p.record.vaults[output];
        let transfer = hooks::Transfer {
            source: vault,
            mint: output_mint,
            destination: trader_output,
            authority: vm::token_owner(svm, &vault).unwrap_or_default(),
            // Hooks may derive accounts from the amount, so use the expected output.
            amount: output_amount,
        };
        strategy_accounts
            .extend(hooks::resolve(&hook, &transfer, account_data).unwrap_or_default());
    }
    client::SwapAccounts {
        trader: *trader,
        strategy_program: p.record.strategy_program,
        market_state: p.record.market_state,
        trader_input,
        trader_output,
        input_vault: p.record.vaults[input],
        input_mint,
        output_mint,
        input_token_program: p.token_program(input),
        output_token_program: p.token_program(output),
        integrator_fee_account: None,
        strategy_accounts,
        input_hook_accounts,
    }
}

fn swap_args(
    case: &Case,
    quote: Option<&TraderQuote>,
    amount_in: u64,
    min_out: u64,
    extra: &[u8],
) -> client::SwapArgs {
    client::SwapArgs {
        mode: case.mode,
        amount_in,
        amount_out: match case.mode {
            TradeMode::ExactIn => 0,
            TradeMode::ExactOut => quote.map_or(0, |q| q.strategy_amount_out),
        },
        min_amount_out: min_out,
        integrator_fee_bps: 0,
        input_hook_accounts: 0,
        extra: extra.to_vec(),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_swap(
    svm: &mut LiteSVM,
    p: &Prepared,
    trader: &Keypair,
    input: usize,
    output: usize,
    args: &client::SwapArgs,
    resolved: &[AccountMeta],
    expected_out: u64,
) -> SwapRun {
    let accounts = swap_accounts(
        svm,
        p,
        &trader.pubkey(),
        input,
        output,
        args.amount_in,
        expected_out,
        resolved,
    );
    let input_before = vm::token_amount(svm, &accounts.trader_input).unwrap_or(0);
    let output_before = vm::token_amount(svm, &accounts.trader_output).unwrap_or(0);
    svm.expire_blockhash();
    let tx = Transaction::new_signed_with_payer(
        &[
            vm::compute_limit_instruction(MAX_COMPUTE_UNITS),
            client::swap(&accounts, args),
        ],
        Some(&trader.pubkey()),
        &[trader],
        svm.latest_blockhash(),
    );
    let strategy_line = format!("Program {} consumed ", p.record.strategy_program);
    let strategy_units = |logs: &[String]| {
        logs.iter()
            .find_map(|line| line.strip_prefix(&strategy_line))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|n| n.parse().ok())
    };
    match svm.send_transaction(tx) {
        Ok(meta) => {
            let input_after = vm::token_amount(svm, &accounts.trader_input).unwrap_or(0);
            let output_after = vm::token_amount(svm, &accounts.trader_output).unwrap_or(0);
            let outcome = if meta.return_data.program_id == client::PROGRAM_ID {
                client::SwapResult::try_from_slice(&meta.return_data.data)
                    .map_err(|_| "the router returned malformed swap results".to_string())
            } else {
                Err("the router returned no swap result".to_string())
            };
            SwapRun {
                outcome,
                compute_units: meta.compute_units_consumed,
                strategy_units: strategy_units(&meta.logs),
                out_of_compute: false,
                spent: input_before.saturating_sub(input_after),
                received: output_after.saturating_sub(output_before),
            }
        }
        Err(failed) => SwapRun {
            outcome: Err(vm::failure_text(&failed)),
            compute_units: failed.meta.compute_units_consumed,
            strategy_units: strategy_units(&failed.meta.logs),
            out_of_compute: vm::out_of_compute(&failed),
            spent: 0,
            received: 0,
        },
    }
}

/// One executed quote-versus-swap comparison.
struct SwapSample {
    case: Case,
    trader: Keypair,
    quote: TraderQuote,
    resolved: Vec<AccountMeta>,
    run: SwapRun,
    matched: bool,
}

// ---------------------------------------------------------------------------------------
// The checks
// ---------------------------------------------------------------------------------------

fn check_market_info(p: &Prepared, results: &mut Results) {
    const ID: &str = "market_info";
    let mut svm = p.base.clone();
    let payer = p.trader();
    vm::fund_lamports(&mut svm, &payer.pubkey(), TRADER_LAMPORTS);
    let ix = Instruction::new_with_bytes(
        p.record.strategy_program,
        &psi::discriminator::MARKET_INFO,
        vec![AccountMeta::new_readonly(p.record.market_state, false)],
    );
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[&payer],
        svm.latest_blockhash(),
    );
    let info = match svm.simulate_transaction(tx) {
        Ok(sim) if sim.meta.return_data.program_id == p.record.strategy_program => {
            psi::MarketInfo::try_from_slice(&sim.meta.return_data.data).ok()
        }
        Ok(_) => None,
        Err(failed) => {
            results.add(
                ID,
                Status::Warn,
                format!(
                    "pipeline_market_info fails when given only the market state ({}). If it needs more accounts, the checker can't tell which.",
                    vm::failure_text(&failed)
                ),
                Value::Null,
            );
            return;
        }
    };
    let Some(info) = info else {
        results.add(
            ID,
            Status::Fail,
            "pipeline_market_info returned no or malformed MarketInfo return data.",
            Value::Null,
        );
        return;
    };
    let reported_mints: Vec<Pubkey> = info
        .mints
        .iter()
        .map(|m| Pubkey::new_from_array(*m))
        .collect();
    let reported_vaults: Vec<Pubkey> = info
        .vaults
        .iter()
        .map(|m| Pubkey::new_from_array(*m))
        .collect();
    let recorded_vaults = &p.record.vaults[..p.mint_count()];
    let mut problems = vec![];
    if reported_mints != p.record.mints() {
        problems.push(format!(
            "mints differ (strategy now reports {}, router recorded {})",
            join(&reported_mints),
            join(p.record.mints())
        ));
    }
    if reported_vaults != recorded_vaults {
        problems.push(format!(
            "vaults differ (strategy now reports {}, router recorded {})",
            join(&reported_vaults),
            join(recorded_vaults)
        ));
    }
    if info.supports_exact_out != p.record.supports_exact_out {
        problems.push(format!(
            "exact-out support differs (strategy now says {}, router recorded {})",
            info.supports_exact_out, p.record.supports_exact_out
        ));
    }
    let details = json!({
        "reported_mints": reported_mints.iter().map(|k| k.to_string()).collect::<Vec<_>>(),
        "reported_vaults": reported_vaults.iter().map(|k| k.to_string()).collect::<Vec<_>>(),
        "reported_supports_exact_out": info.supports_exact_out,
    });
    if problems.is_empty() {
        results.add(
            ID,
            Status::Pass,
            "The strategy reports the same mints, vaults and exact-out support the router recorded.",
            details,
        );
    } else {
        results.add(
            ID,
            Status::Fail,
            format!(
                "The router routes by its recorded market, but {}.",
                problems.join("; ")
            ),
            details,
        );
    }
}

fn join(keys: &[Pubkey]) -> String {
    let keys: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
    format!("[{}]", keys.join(", "))
}

fn check_fee_accounts(p: &mut Prepared, results: &mut Results) {
    const ID: &str = "fee_accounts";
    let mut missing = vec![];
    let mut frozen = vec![];
    let mut not_created = vec![];
    let payer = p.trader();
    vm::fund_lamports(&mut p.base, &payer.pubkey(), TRADER_LAMPORTS);
    for (i, mint) in p.mints.iter().enumerate() {
        let fee_account = pda::fee_account(&p.market, &mint.address, &mint.token_program);
        match p.base.get_account(&fee_account) {
            Some(account) => {
                // AccountState::Frozen
                if account.data.get(108) == Some(&2) {
                    frozen.push(i);
                }
            }
            None => {
                missing.push(i);
                // Create it locally so the other checks can sell this mint.
                let ix = client::init_fee_account(
                    &payer.pubkey(),
                    &p.market,
                    &mint.address,
                    &mint.token_program,
                );
                p.base.expire_blockhash();
                let tx = Transaction::new_signed_with_payer(
                    &[ix],
                    Some(&payer.pubkey()),
                    &[&payer],
                    p.base.latest_blockhash(),
                );
                if let Err(failed) = p.base.send_transaction(tx) {
                    not_created.push(format!("mint {i}: {}", vm::failure_text(&failed)));
                }
            }
        }
    }
    let details = json!({
        "missing": missing,
        "frozen": frozen,
        "local_creation_failures": not_created,
    });
    let mut notes = vec![];
    if !missing.is_empty() {
        let list: Vec<String> = missing
            .iter()
            .map(|i| format!("mint {i} ({})", p.mints[*i].address))
            .collect();
        notes.push(format!(
            "No fee account for {}: selling that mint through Pipeline fails until anyone sends the router's init_fee_account for it (pipeline_client::init_fee_account). The checks created it locally.",
            list.join(", ")
        ));
    }
    if !frozen.is_empty() {
        notes.push(format!(
            "The fee account for mint(s) {frozen:?} is frozen, so selling those mints fails."
        ));
    }
    if !not_created.is_empty() {
        notes.push(format!(
            "init_fee_account itself fails: {}.",
            not_created.join("; ")
        ));
    }
    if notes.is_empty() {
        results.add(
            ID,
            Status::Pass,
            "Every mint has its Pipeline fee account.",
            details,
        );
    } else {
        results.add(ID, Status::Warn, notes.join(" "), details);
    }
}

fn check_mint_extensions(p: &Prepared, results: &mut Results) {
    const ID: &str = "mint_extensions";
    let mut warnings = vec![];
    let mut facts = vec![];
    for (i, mint) in p.mints.iter().enumerate() {
        if !mint.extensions.is_empty() {
            facts.push(format!("mint {i}: {}", mint.extensions.join(", ")));
        }
        if mint.default_frozen {
            warnings.push(format!(
                "mint {i}'s new token accounts start frozen, so its fee account and new traders' accounts need thawing before it can be sold"
            ));
        }
        if mint.non_transferable {
            warnings.push(format!(
                "mint {i} is non-transferable, so it cannot be traded"
            ));
        }
        if let Some(hook) = mint.transfer_hook_program {
            if vm::program_facts(&p.base, &hook).is_none() {
                warnings.push(format!(
                    "mint {i}'s transfer hook program {hook} is not deployed here"
                ));
                continue;
            }
            let placeholder = hooks::Transfer {
                source: Pubkey::new_from_array([1; 32]),
                mint: mint.address,
                destination: p.record.vaults[i],
                authority: Pubkey::new_from_array([2; 32]),
                amount: 0,
            };
            let data = |key: &Pubkey| p.base.get_account(key).map(|a| a.data);
            if let Err(error) = hooks::resolve(&hook, &placeholder, data) {
                warnings.push(format!(
                    "mint {i}'s transfer-hook accounts can't be resolved: {error}"
                ));
            }
        }
    }
    let details = json!({
        "mints": p.mints.iter().map(|m| json!({
            "address": m.address.to_string(),
            "extensions": m.extensions,
            "transfer_hook_program": m.transfer_hook_program.map(|k| k.to_string()),
        })).collect::<Vec<_>>(),
    });
    if !warnings.is_empty() {
        results.add(
            ID,
            Status::Warn,
            format!("{}.", capitalise(&warnings.join("; "))),
            details,
        );
    } else if facts.is_empty() {
        results.add(ID, Status::Pass, "No Token-2022 extensions.", details);
    } else {
        results.add(
            ID,
            Status::Pass,
            format!(
                "Extensions present, none of which stop trading: {}.",
                facts.join("; ")
            ),
            details,
        );
    }
}

fn capitalise(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn check_resolve_accounts(p: &Prepared, table: &[QuoteRow], results: &mut Results) {
    const ID: &str = "resolve_accounts";
    let mut directions = vec![];
    let mut worst = Status::Pass;
    let mut notes = vec![];
    let mut max_rounds = 0;
    let mut max_accounts = 0;
    for (input, output) in p.directions() {
        let rows: Vec<&QuoteRow> = table
            .iter()
            .filter(|r| r.case.input == input && r.case.output == output)
            .collect();
        let failures: Vec<String> = rows
            .iter()
            .filter_map(|r| {
                r.attempt
                    .resolved
                    .as_ref()
                    .err()
                    .map(|e| format!("{}: {e}", r.case.name()))
            })
            .collect();
        let resolved: Vec<&Resolved> = rows
            .iter()
            .filter_map(|r| r.attempt.resolved.as_ref().ok())
            .collect();
        for r in &resolved {
            max_rounds = max_rounds.max(r.rounds);
            max_accounts = max_accounts.max(r.accounts.len());
        }
        if resolved.is_empty() {
            worst = worst.max(Status::Fail);
        } else if !failures.is_empty() {
            worst = worst.max(Status::Warn);
        }
        if let Some(first) = failures.first() {
            notes.push(first.clone());
        }
        directions.push(json!({
            "direction": format!("{input}→{output}"),
            "rounds": resolved.iter().map(|r| r.rounds).max(),
            "accounts": resolved.first().map(|r| r.accounts.iter().map(|m| json!({
                "address": m.pubkey.to_string(),
                "writable": m.is_writable,
            })).collect::<Vec<_>>()),
            "failures": failures,
        }));
    }
    let details =
        json!({ "max_rounds": max_rounds, "max_accounts": max_accounts, "directions": directions });
    let message = match worst {
        Status::Pass => format!(
            "Resolves in at most {max_rounds} of {} rounds in every direction; swaps need up to {max_accounts} strategy account(s).",
            psi::MAX_RESOLVE_ROUNDS
        ),
        Status::Warn => format!(
            "Resolution fails for some sizes. First failure: {}.",
            notes.first().cloned().unwrap_or_default()
        ),
        _ => format!(
            "Resolution fails for every size in at least one direction, so integrators can't build swaps there. First failure: {}.",
            notes.first().cloned().unwrap_or_default()
        ),
    };
    results.add(ID, worst, message, details);
}

/// The first successful exact-in quote in a direction, preferring the medium size.
fn representative(table: &[QuoteRow], input: usize, output: usize) -> Option<&QuoteRow> {
    let rows = || {
        table.iter().filter(move |r| {
            r.case.input == input
                && r.case.output == output
                && r.case.mode == TradeMode::ExactIn
                && r.case.label != "dust"
                && r.attempt.ok().is_some()
        })
    };
    rows()
        .find(|r| r.case.label == "medium")
        .or_else(|| rows().next())
}

fn check_transaction_size(p: &Prepared, table: &[QuoteRow], results: &mut Results) {
    const ID: &str = "transaction_size";
    let trader = p.trader();
    let mut worst = Status::Skip;
    let mut rows = vec![];
    let mut largest = (0usize, 0usize, 0usize);
    // Every quoted case, in both modes and at every size: a strategy's account list can
    // grow with the trade (crossing more ticks, say), so the largest trade decides.
    for row in table {
        let Some((resolved, quote)) = row.attempt.ok() else {
            continue;
        };
        let (input, output) = (row.case.input, row.case.output);
        let accounts = swap_accounts(
            &p.base,
            p,
            &trader.pubkey(),
            input,
            output,
            quote.amount_in,
            quote.strategy_amount_out,
            &resolved.accounts,
        );
        let args = swap_args(
            &row.case,
            Some(quote),
            quote.amount_in,
            quote.amount_out,
            &p.options.extra,
        );
        let tx = Transaction::new_with_payer(
            &[
                vm::compute_limit_instruction(MAX_COMPUTE_UNITS),
                client::swap(&accounts, &args),
            ],
            Some(&trader.pubkey()),
        );
        let size = bincode::serialize(&tx)
            .map(|b| b.len())
            .unwrap_or(usize::MAX);
        let keys = tx.message.account_keys.len();
        // With a lookup table, every account except the signer and the two invoked
        // programs can move into the table: 31 bytes saved per account, plus about 36
        // bytes for the version byte, table address and index lists.
        let lookupable = keys.saturating_sub(3);
        let with_table = size - lookupable * 31 + 36;
        let status = if keys > MAX_TRANSACTION_ACCOUNTS || with_table > MAX_TRANSACTION_SIZE {
            Status::Fail
        } else if size > MAX_TRANSACTION_SIZE {
            Status::Warn
        } else {
            Status::Pass
        };
        worst = worst.max(status);
        if size > largest.0 {
            largest = (size, keys, with_table);
        }
        rows.push(json!({
            "case": row.case.name(),
            "serialized_bytes": size,
            "account_keys": keys,
            "estimated_bytes_with_lookup_table": with_table,
        }));
    }
    let (size, keys, with_table) = largest;
    let details = json!({
        "limit_bytes": MAX_TRANSACTION_SIZE,
        "limit_accounts": MAX_TRANSACTION_ACCOUNTS,
        "includes": "one swap instruction and a compute-limit instruction, signed by the trader",
        "swaps": rows,
    });
    let message = match worst {
        Status::Skip => "No direction could be quoted, so no swap was built.".to_string(),
        Status::Pass => format!(
            "The largest swap is {size} of {MAX_TRANSACTION_SIZE} bytes with {keys} accounts, so it fits in a legacy transaction."
        ),
        Status::Warn => format!(
            "The largest swap is {size} bytes ({keys} accounts), over {MAX_TRANSACTION_SIZE}: it needs a v0 transaction with an address lookup table (about {with_table} bytes then)."
        ),
        _ => format!(
            "The largest swap needs {keys} accounts and about {with_table} bytes even with a lookup table, beyond Solana's {MAX_TRANSACTION_ACCOUNTS}-account / {MAX_TRANSACTION_SIZE}-byte limits."
        ),
    };
    results.add(ID, worst, message, details);
}

fn quote_json(row: &QuoteRow) -> Value {
    json!({
        "case": row.case.name(),
        "direction": row.case.direction(),
        "mode": format!("{:?}", row.case.mode),
        "size": row.case.label,
        "amount": row.case.amount,
        "quote": row.attempt.ok().map(|(_, q)| json!({
            "amount_in": q.amount_in,
            "net_sent": q.net_sent,
            "pipeline_fee": q.pipeline_fee,
            "total_spend": q.total_spend,
            "amount_out": q.amount_out,
            "strategy_amount_out": q.strategy_amount_out,
            "compute_units": q.compute_units,
        })),
        "error": row.attempt.error(),
    })
}

fn check_quotes(p: &Prepared, table: &[QuoteRow], results: &mut Results) {
    const ID: &str = "quotes";
    // Dust may legitimately fail to quote; it has its own check.
    let sized: Vec<&QuoteRow> = table.iter().filter(|r| r.case.label != "dust").collect();
    let failed: Vec<&&QuoteRow> = sized.iter().filter(|r| r.attempt.ok().is_none()).collect();
    let max_units = sized
        .iter()
        .filter_map(|r| r.attempt.ok().map(|(_, q)| q.compute_units))
        .max()
        .unwrap_or(0);
    let details = json!({
        "quote_compute_limit": p.options.quote_compute_limit,
        "max_quote_compute_units": max_units,
        "quotes": table.iter().map(quote_json).collect::<Vec<_>>(),
    });
    let dead_directions: Vec<String> = p
        .directions()
        .into_iter()
        .filter(|(i, o)| {
            !sized
                .iter()
                .any(|r| r.case.input == *i && r.case.output == *o && r.attempt.ok().is_some())
        })
        .map(|(i, o)| format!("{i}→{o}"))
        .collect();
    let first_error = failed
        .first()
        .map(|r| {
            format!(
                "{}: {}",
                r.case.name(),
                r.attempt.error().unwrap_or_default()
            )
        })
        .unwrap_or_default();
    let out_of_compute = |error: &str| {
        error.contains("exceeded CUs meter") || error.contains("ComputationalBudgetExceeded")
    };
    // Running out of compute or answering malformed data is always a defect; a strategy
    // refusing a trade (closed, not open yet, empty) may be deliberate.
    let defective = failed.iter().any(|r| {
        let error = r.attempt.error().unwrap_or_default();
        out_of_compute(&error) || error.contains("malformed")
    });
    let compute_hint = if out_of_compute(&first_error) {
        format!(
            " Quotes must fit integrators' {} compute-unit cap.",
            p.options.quote_compute_limit
        )
    } else {
        String::new()
    };
    if sized.is_empty() {
        results.add(ID, Status::Skip, "No sizes to quote.", details);
    } else if failed.len() == sized.len() && defective {
        results.add(
            ID,
            Status::Fail,
            format!("No quote succeeded. First failure: {first_error}.{compute_hint}"),
            details,
        );
    } else if failed.len() == sized.len() {
        results.add(
            ID,
            Status::Warn,
            format!(
                "The strategy refused every quote, so the trading checks were skipped. That is expected if the market is closed or not open yet; otherwise integrators can't price it. First failure: {first_error}."
            ),
            details,
        );
    } else if !failed.is_empty() {
        let directions = if dead_directions.is_empty() {
            String::new()
        } else {
            format!(
                " Direction(s) {} can't be quoted at all, so integrators show no price there.",
                dead_directions.join(", ")
            )
        };
        results.add(
            ID,
            Status::Warn,
            format!(
                "{} of {} quotes failed.{directions} First failure: {first_error}.{compute_hint}",
                failed.len(),
                sized.len()
            ),
            details,
        );
    } else {
        results.add(
            ID,
            Status::Pass,
            format!(
                "All {} quotes succeeded through pipeline-quoter using only the resolved accounts (up to {max_units} compute units each).",
                sized.len()
            ),
            details,
        );
    }
}

fn check_quote_matches_swap(
    p: &Prepared,
    quotes: &mut QuoteVm,
    table: &[QuoteRow],
    results: &mut Results,
) -> Vec<SwapSample> {
    const ID: &str = "quote_matches_swap";
    let mut samples = vec![];
    let mut mismatches = vec![];
    let mut rows = vec![];
    for row in table {
        if row.case.label == "dust" || row.attempt.ok().is_none() {
            continue;
        }
        let trader = p.trader();
        let attempt = quotes.quote(&p.base, p, &row.case, Some(trader.pubkey()));
        let Some((resolved, quote)) = attempt.ok() else {
            mismatches.push(format!(
                "{}: the anonymous quote works but the quote for a trader fails ({})",
                row.case.name(),
                attempt.error().unwrap_or_default()
            ));
            continue;
        };
        let (resolved, quote) = (resolved.accounts.clone(), quote.clone());
        let budget = fees::max_total_spend(quote.amount_in, 0).unwrap_or(u64::MAX);
        let mut svm = match p.funded_clone(&trader, row.case.input, row.case.output, budget) {
            Ok(svm) => svm,
            Err(error) => {
                mismatches.push(format!(
                    "{}: could not fund a trader: {error}",
                    row.case.name()
                ));
                continue;
            }
        };
        let args = swap_args(
            &row.case,
            Some(&quote),
            quote.amount_in,
            quote.amount_out,
            &p.options.extra,
        );
        let run = run_swap(
            &mut svm,
            p,
            &trader,
            row.case.input,
            row.case.output,
            &args,
            &resolved,
            quote.strategy_amount_out,
        );
        let matched = match &run.outcome {
            Ok(result) => {
                // The router's own figures, and the trader's balances, must both match.
                let same = result.received == quote.amount_out
                    && result.net_sent == quote.net_sent
                    && result.pipeline_fee == quote.pipeline_fee
                    && run.received == quote.amount_out
                    && run.spent == quote.total_spend;
                if !same {
                    mismatches.push(format!(
                        "{}: quoted {} out for {} in plus {} fee, swap delivered {} for {} in plus {} fee (trader balances: -{} / +{})",
                        row.case.name(),
                        quote.amount_out,
                        quote.net_sent,
                        quote.pipeline_fee,
                        result.received,
                        result.net_sent,
                        result.pipeline_fee,
                        run.spent,
                        run.received
                    ));
                }
                same
            }
            Err(error) => {
                // Find out what the swap actually delivers by dropping the minimum.
                let mut retry = p
                    .funded_clone(&trader, row.case.input, row.case.output, budget)
                    .expect("funding worked a moment ago");
                let loose = swap_args(
                    &row.case,
                    Some(&quote),
                    quote.amount_in,
                    0,
                    &p.options.extra,
                );
                let second = run_swap(
                    &mut retry,
                    p,
                    &trader,
                    row.case.input,
                    row.case.output,
                    &loose,
                    &resolved,
                    quote.strategy_amount_out,
                );
                match second.outcome {
                    Ok(result) => mismatches.push(format!(
                        "{}: quoted {} out for {} in, swap delivered {} for {} (with the quote as minimum it failed: {error})",
                        row.case.name(),
                        quote.amount_out,
                        quote.net_sent,
                        result.received,
                        result.net_sent
                    )),
                    Err(_) => mismatches.push(format!(
                        "{}: the quote succeeds but the swap fails: {error}",
                        row.case.name()
                    )),
                }
                false
            }
        };
        rows.push(json!({
            "case": row.case.name(),
            "quoted_out": quote.amount_out,
            "quoted_net_sent": quote.net_sent,
            "quoted_pipeline_fee": quote.pipeline_fee,
            "received": run.outcome.as_ref().ok().map(|r| r.received),
            "net_sent": run.outcome.as_ref().ok().map(|r| r.net_sent),
            "pipeline_fee": run.outcome.as_ref().ok().map(|r| r.pipeline_fee),
            "compute_units": run.compute_units,
            "error": run.outcome.as_ref().err(),
            "matched": matched,
        }));
        samples.push(SwapSample {
            case: row.case.clone(),
            trader,
            quote,
            resolved,
            run,
            matched,
        });
    }
    let details = json!({ "swaps": rows, "mismatches": mismatches });
    if samples.is_empty() && mismatches.is_empty() {
        results.add(
            ID,
            Status::Skip,
            "No quote succeeded, so nothing was swapped.",
            details,
        );
    } else if mismatches.is_empty() {
        results.add(
            ID,
            Status::Pass,
            format!(
                "All {} router swaps delivered exactly the quoted output and charged exactly the quoted input and fee.",
                samples.len()
            ),
            details,
        );
    } else {
        results.add(
            ID,
            Status::Fail,
            format!(
                "{} of {} swaps differ from their quotes. First: {}. Compute pipeline_swap with the same code as pipeline_quote.",
                mismatches.len(),
                samples.len().max(mismatches.len()),
                mismatches[0]
            ),
            details,
        );
    }
    samples
}

fn check_min_out(p: &Prepared, samples: &[SwapSample], results: &mut Results) {
    const ID: &str = "min_out_enforced";
    let mut tested = vec![];
    let mut problems = vec![];
    for (input, output) in p.directions() {
        let Some(sample) = samples
            .iter()
            .filter(|s| {
                s.matched
                    && s.case.input == input
                    && s.case.output == output
                    && s.case.mode == TradeMode::ExactIn
            })
            .min_by_key(|s| (s.case.label != "medium") as u8)
        else {
            continue;
        };
        let budget = fees::max_total_spend(sample.quote.amount_in, 0).unwrap_or(u64::MAX);
        let Ok(mut svm) = p.funded_clone(&sample.trader, input, output, budget) else {
            continue;
        };
        let min_out = sample.quote.amount_out.saturating_add(1);
        let args = swap_args(
            &sample.case,
            Some(&sample.quote),
            sample.quote.amount_in,
            min_out,
            &p.options.extra,
        );
        let run = run_swap(
            &mut svm,
            p,
            &sample.trader,
            input,
            output,
            &args,
            &sample.resolved,
            sample.quote.strategy_amount_out,
        );
        // Any refusal means nothing was delivered. The router enforces the minimum on every
        // swap; a strategy may also refuse first with its own error (it sees the minimum in
        // `SwapRequest`). The report records which one refused.
        let by_router = run
            .outcome
            .as_ref()
            .err()
            .is_some_and(|error| is_router_error(error, ROUTER_INSUFFICIENT_OUTPUT));
        if let Ok(result) = &run.outcome {
            problems.push(format!(
                "{}: a swap with minimum {min_out} succeeded and delivered {}",
                sample.case.name(),
                result.received
            ));
        }
        tested.push(json!({
            "case": sample.case.name(),
            "min_amount_out": min_out,
            "rejected": run.outcome.is_err(),
            "refused_by": match &run.outcome {
                Ok(_) => Value::Null,
                Err(_) if by_router => json!("router"),
                Err(_) => json!("strategy"),
            },
            "error": run.outcome.as_ref().err(),
        }));
    }
    let details = json!({ "tested": tested });
    if tested.is_empty() {
        results.add(
            ID,
            Status::Skip,
            "No swap matched its quote, so there is nothing to tighten.",
            details,
        );
    } else if problems.is_empty() {
        results.add(
            ID,
            Status::Pass,
            format!(
                "In {} direction(s), asking for one unit more than the quote makes the swap fail.",
                tested.len()
            ),
            details,
        );
    } else {
        results.add(
            ID,
            Status::Fail,
            format!(
                "The trader's minimum was not enforced: {}.",
                problems.join("; ")
            ),
            details,
        );
    }
}

/// Whether a failed swap's error is the router's custom error `code` on the swap
/// instruction (index 1, after the compute-limit instruction).
fn is_router_error(error: &str, code: u32) -> bool {
    error.starts_with(&format!("InstructionError(1, Custom({code}))"))
}

fn check_determinism(p: &Prepared, quotes: &mut QuoteVm, results: &mut Results) {
    const ID: &str = "determinism";
    let mut repeat_failures = vec![];
    let mut trader_dependent = vec![];
    let mut clock_dependent = vec![];
    let mut tested = 0;
    let trader_a = p.trader().pubkey();
    let trader_b = p.trader().pubkey();
    for (input, output) in p.directions() {
        let Some(amount) = p.medium(input) else {
            continue;
        };
        let case = Case {
            input,
            output,
            mode: TradeMode::ExactIn,
            label: "medium".into(),
            amount,
        };
        let first = quotes.quote(&p.base, p, &case, None);
        let Some((_, first_quote)) = first.ok() else {
            continue;
        };
        tested += 1;
        let first_quote = first_quote.clone();
        let again = quotes.quote(&p.base, p, &case, None);
        if again.ok().map(|(_, q)| q.amount_out) != Some(first_quote.amount_out) {
            repeat_failures.push(case.direction());
        }
        let a = quotes.quote(&p.base, p, &case, Some(trader_a));
        let b = quotes.quote(&p.base, p, &case, Some(trader_b));
        let a_out = a.ok().map(|(_, q)| q.amount_out);
        let b_out = b.ok().map(|(_, q)| q.amount_out);
        if a_out != Some(first_quote.amount_out) || b_out != Some(first_quote.amount_out) {
            trader_dependent.push(format!(
                "{}: anonymous {}, trader A {}, trader B {}",
                case.direction(),
                first_quote.amount_out,
                a_out.map_or("failed".into(), |v| v.to_string()),
                b_out.map_or("failed".into(), |v| v.to_string())
            ));
        }
        // A minute later. Time-dependent pricing is allowed; this is only reported.
        let clock: solana_clock::Clock = p.base.get_sysvar();
        quotes
            .quoter
            .set_clock(clock.slot + 150, clock.epoch, clock.unix_timestamp + 60);
        let later = quotes.quote(&p.base, p, &case, None);
        quotes.set_clock_from(&p.base);
        let later_out = later.ok().map(|(_, q)| q.amount_out);
        if later_out != Some(first_quote.amount_out) {
            clock_dependent.push(format!(
                "{}: {} now, {} a minute later",
                case.direction(),
                first_quote.amount_out,
                later_out.map_or("failed".into(), |v| v.to_string())
            ));
        }
    }
    let details = json!({
        "directions_tested": tested,
        "not_repeatable": repeat_failures,
        "trader_dependent": trader_dependent,
        "clock_dependent": clock_dependent,
    });
    if tested == 0 {
        results.add(ID, Status::Skip, "No direction could be quoted.", details);
        return;
    }
    let clock_note = if clock_dependent.is_empty() {
        String::new()
    } else {
        format!(
            " Prices move with the clock ({}), so quotes go stale quickly.",
            clock_dependent.join("; ")
        )
    };
    if !repeat_failures.is_empty() {
        results.add(
            ID,
            Status::Fail,
            format!(
                "The same quote twice in the same state gave different answers in direction(s) {}.{clock_note}",
                repeat_failures.join(", ")
            ),
            details,
        );
    } else if !trader_dependent.is_empty() {
        results.add(
            ID,
            Status::Warn,
            format!(
                "Quotes depend on the trader ({}). That is allowed, but anonymous quotes from aggregators will differ from what traders get.{clock_note}",
                trader_dependent.join("; ")
            ),
            details,
        );
    } else {
        results.add(
            ID,
            Status::Pass,
            format!(
                "Repeated quotes agree, and two different traders get the anonymous quote.{clock_note}"
            ),
            details,
        );
    }
}

fn check_compute(p: &Prepared, samples: &[SwapSample], results: &mut Results) {
    const ID: &str = "compute";
    let succeeded: Vec<&SwapSample> = samples.iter().filter(|s| s.run.outcome.is_ok()).collect();
    let exhausted: Vec<&SwapSample> = samples.iter().filter(|s| s.run.out_of_compute).collect();
    let max = succeeded.iter().map(|s| s.run.compute_units).max();
    let min = succeeded.iter().map(|s| s.run.compute_units).min();
    let strategy_max = succeeded.iter().filter_map(|s| s.run.strategy_units).max();
    let details = json!({
        "max_swap_units": max,
        "min_swap_units": min,
        "max_strategy_units": strategy_max,
        "warn_above": p.options.compute_warn_units,
        "limit": MAX_COMPUTE_UNITS,
    });
    if let Some(sample) = exhausted.first() {
        results.add(
            ID,
            Status::Fail,
            format!(
                "{}: the swap ran out of compute even with the {MAX_COMPUTE_UNITS}-unit maximum.",
                sample.case.name()
            ),
            details,
        );
        return;
    }
    let Some(max) = max else {
        results.add(
            ID,
            Status::Skip,
            "No swap succeeded, so nothing was measured.",
            details,
        );
        return;
    };
    let strategy = strategy_max.map_or(String::new(), |s| {
        format!(", of which the strategy used up to {s}")
    });
    if max > p.options.compute_warn_units {
        results.add(
            ID,
            Status::Warn,
            format!(
                "Router swaps used up to {max} compute units{strategy}. That is above {}, which leaves little room for multi-hop routes.",
                p.options.compute_warn_units
            ),
            details,
        );
    } else {
        results.add(
            ID,
            Status::Pass,
            format!(
                "Router swaps used {}–{max} compute units{strategy}.",
                min.unwrap_or(max)
            ),
            details,
        );
    }
}

fn check_exact_out_refund(
    p: &Prepared,
    quotes: &mut QuoteVm,
    table: &[QuoteRow],
    results: &mut Results,
) {
    const ID: &str = "exact_out_refund";
    if !p.record.supports_exact_out {
        results.add(
            ID,
            Status::Skip,
            "The market does not support exact-out.",
            Value::Null,
        );
        return;
    }
    let mut problems = vec![];
    let mut tested = vec![];
    for (input, output) in p.directions() {
        let Some(row) = table.iter().find(|r| {
            r.case.input == input
                && r.case.output == output
                && r.case.mode == TradeMode::ExactOut
                && r.attempt.ok().is_some()
        }) else {
            continue;
        };
        let trader = p.trader();
        let attempt = quotes.quote(&p.base, p, &row.case, Some(trader.pubkey()));
        let Some((resolved, quote)) = attempt.ok() else {
            continue;
        };
        let (resolved, quote) = (resolved.accounts.clone(), quote.clone());
        // Allow twice the quoted input; the strategy should keep only what it needs.
        let max_in = quote
            .amount_in
            .saturating_mul(2)
            .max(quote.amount_in.saturating_add(1));
        let budget = fees::max_total_spend(max_in, 0).unwrap_or(u64::MAX);
        let Ok(mut svm) = p.funded_clone(&trader, input, output, budget) else {
            continue;
        };
        let args = swap_args(
            &row.case,
            Some(&quote),
            max_in,
            quote.amount_out,
            &p.options.extra,
        );
        let run = run_swap(
            &mut svm,
            p,
            &trader,
            input,
            output,
            &args,
            &resolved,
            quote.strategy_amount_out,
        );
        let name = row.case.name();
        match &run.outcome {
            Err(error) => problems.push(format!(
                "{name}: with a maximum input of {max_in} (quote needs {}) the swap fails: {error}",
                quote.amount_in
            )),
            Ok(result) => {
                let has_input_fee = p.mints[input]
                    .extensions
                    .iter()
                    .any(|e| e == "TransferFeeConfig");
                if result.net_sent >= max_in {
                    problems.push(format!(
                        "{name}: the strategy kept the whole maximum of {max_in} though the quote needs {}",
                        quote.net_sent
                    ));
                } else if !has_input_fee && result.net_sent != quote.net_sent {
                    problems.push(format!(
                        "{name}: the trader was charged for {} but the quote needs {}",
                        result.net_sent, quote.net_sent
                    ));
                }
                if run.spent != result.net_sent + result.pipeline_fee {
                    problems.push(format!(
                        "{name}: the trader spent {} but net_sent plus fee is {}",
                        run.spent,
                        result.net_sent + result.pipeline_fee
                    ));
                }
            }
        }
        tested.push(json!({
            "case": name,
            "max_amount_in": max_in,
            "quoted_net_sent": quote.net_sent,
            "net_sent": run.outcome.as_ref().ok().map(|r| r.net_sent),
            "pipeline_fee": run.outcome.as_ref().ok().map(|r| r.pipeline_fee),
            "trader_spent": run.spent,
            "received": run.outcome.as_ref().ok().map(|r| r.received),
        }));
    }
    let details = json!({ "tested": tested, "problems": problems });
    if tested.is_empty() {
        results.add(ID, Status::Skip, "No exact-out quote succeeded.", details);
    } else if problems.is_empty() {
        results.add(
            ID,
            Status::Pass,
            format!(
                "In {} direction(s), an exact-out swap allowed twice the quoted input kept only the quoted input, and the fee was charged on that.",
                tested.len()
            ),
            details,
        );
    } else {
        results.add(
            ID,
            Status::Fail,
            format!("{}.", problems.join("; ")),
            details,
        );
    }
}

fn check_round_trip(p: &Prepared, quotes: &mut QuoteVm, results: &mut Results) {
    const ID: &str = "round_trip";
    let mut trips = vec![];
    let mut warnings = vec![];
    // Both orders of every pair: free profit can exist in one direction only.
    for (first, second) in p.directions() {
        let Some(amount) = p.medium(first) else {
            continue;
        };
        let trader = p.trader();
        let buy = Case {
            input: first,
            output: second,
            mode: TradeMode::ExactIn,
            label: "medium".into(),
            amount,
        };
        let attempt = quotes.quote(&p.base, p, &buy, Some(trader.pubkey()));
        let Ok(resolved) = attempt.resolved.as_ref() else {
            continue;
        };
        let budget = fees::max_total_spend(amount, 0).unwrap_or(u64::MAX);
        let Ok(mut svm) = p.funded_clone(&trader, first, second, budget) else {
            continue;
        };
        let args = swap_args(&buy, None, amount, 0, &p.options.extra);
        let bought = run_swap(
            &mut svm,
            p,
            &trader,
            first,
            second,
            &args,
            &resolved.accounts,
            0,
        );
        let Ok(bought_result) = bought.outcome.clone() else {
            continue;
        };
        if bought_result.received == 0 {
            continue;
        }
        // Sell everything received, paying Pipeline's fee out of it.
        let sell_amount = fees::amount_for_budget(bought_result.received, 0);
        let sell = Case {
            input: second,
            output: first,
            mode: TradeMode::ExactIn,
            label: "everything bought".into(),
            amount: sell_amount,
        };
        let sold = match QuoteVm::new(&svm, p) {
            Ok(mut after) => {
                after.set_clock_from(&svm);
                let attempt = after.quote(&svm, p, &sell, Some(trader.pubkey()));
                match attempt.resolved {
                    Ok(resolved) => {
                        let args = swap_args(&sell, None, sell_amount, 0, &p.options.extra);
                        run_swap(
                            &mut svm,
                            p,
                            &trader,
                            second,
                            first,
                            &args,
                            &resolved.accounts,
                            0,
                        )
                        .outcome
                    }
                    Err(error) => Err(format!("account resolution failed: {error}")),
                }
            }
            Err(error) => Err(error),
        };
        let put_in = bought_result.net_sent + bought_result.pipeline_fee;
        match &sold {
            Err(error) => warnings.push(format!(
                "{first}→{second}→{first}: bought {} but could not sell straight back: {error}",
                bought_result.received
            )),
            Ok(result) if result.received > put_in => warnings.push(format!(
                "{first}→{second}→{first}: put in {put_in} (with Pipeline's fee) and got {} back, a free profit",
                result.received
            )),
            Ok(result) if result.received > bought_result.net_sent => warnings.push(format!(
                "{first}→{second}→{first}: put in {} before Pipeline's fee and got {} back, a profit for anyone calling the strategy directly",
                bought_result.net_sent, result.received
            )),
            Ok(_) => {}
        }
        trips.push(json!({
            "route": format!("{first}→{second}→{first}"),
            "put_in": bought_result.net_sent,
            "pipeline_fees": bought_result.pipeline_fee + sold.as_ref().map_or(0, |r| r.pipeline_fee),
            "intermediate": bought_result.received,
            "got_back": sold.as_ref().ok().map(|r| r.received),
            "error": sold.as_ref().err(),
        }));
    }
    let details = json!({ "trips": trips });
    if trips.is_empty() {
        results.add(
            ID,
            Status::Skip,
            "No buy succeeded, so nothing was sold back.",
            details,
        );
    } else if warnings.is_empty() {
        results.add(
            ID,
            Status::Pass,
            format!(
                "In {} pair(s), buying and immediately selling back returned less than was put in.",
                trips.len()
            ),
            details,
        );
    } else {
        results.add(
            ID,
            Status::Warn,
            format!("{}.", capitalise(&warnings.join("; "))),
            details,
        );
    }
}

fn check_dust_and_zero(
    p: &Prepared,
    quotes: &mut QuoteVm,
    table: &[QuoteRow],
    results: &mut Results,
) {
    const ID: &str = "dust_and_zero";
    let mut failures = vec![];
    let mut warnings = vec![];
    let mut rows = vec![];
    for (input, output) in p.directions() {
        let zero = Case {
            input,
            output,
            mode: TradeMode::ExactIn,
            label: "zero".into(),
            amount: 0,
        };
        let zero_quote = quotes.quote(&p.base, p, &zero, None);
        if let Some((_, quote)) = zero_quote.ok() {
            if quote.amount_out > 0 {
                failures.push(format!(
                    "{input}→{output}: quotes {} output for zero input",
                    quote.amount_out
                ));
            }
        }
        let resolved = zero_quote
            .resolved
            .as_ref()
            .ok()
            .map(|r| r.accounts.clone())
            .or_else(|| {
                representative(table, input, output)
                    .and_then(|r| r.attempt.resolved.as_ref().ok())
                    .map(|r| r.accounts.clone())
            });
        let Some(resolved) = resolved else {
            continue;
        };

        let trader = p.trader();
        let Ok(mut svm) = p.funded_clone(&trader, input, output, 10) else {
            continue;
        };
        let zero_swap = run_swap(
            &mut svm,
            p,
            &trader,
            input,
            output,
            &swap_args(&zero, None, 0, 0, &p.options.extra),
            &resolved,
            0,
        );
        if let Ok(result) = &zero_swap.outcome {
            failures.push(format!(
                "{input}→{output}: a zero-input swap succeeded and delivered {}",
                result.received
            ));
        }

        let dust = table
            .iter()
            .find(|r| r.case.input == input && r.case.output == output && r.case.label == "dust");
        let dust_quote = dust.and_then(|r| r.attempt.ok()).map(|(_, q)| q.clone());
        let trader = p.trader();
        let Ok(mut svm) = p.funded_clone(&trader, input, output, 10) else {
            continue;
        };
        let one = Case {
            label: "dust".into(),
            amount: 1,
            ..zero.clone()
        };
        let one_swap = run_swap(
            &mut svm,
            p,
            &trader,
            input,
            output,
            &swap_args(&one, None, 1, 0, &p.options.extra),
            &resolved,
            0,
        );
        if let (Ok(result), Some(quote)) = (&one_swap.outcome, &dust_quote) {
            if result.received != quote.amount_out {
                warnings.push(format!(
                    "{input}→{output}: a 1-unit swap delivered {} but was quoted {}",
                    result.received, quote.amount_out
                ));
            }
        }
        rows.push(json!({
            "direction": format!("{input}→{output}"),
            "zero_quote_out": zero_quote.ok().map(|(_, q)| q.amount_out),
            "zero_quote_error": zero_quote.error(),
            "zero_swap_error": zero_swap.outcome.as_ref().err(),
            "one_unit_quote_out": dust_quote.as_ref().map(|q| q.amount_out),
            "one_unit_swap_received": one_swap.outcome.as_ref().ok().map(|r| r.received),
            "one_unit_swap_error": one_swap.outcome.as_ref().err(),
        }));
    }
    let details = json!({ "directions": rows });
    if rows.is_empty() && failures.is_empty() {
        results.add(ID, Status::Skip, "No direction could be resolved.", details);
    } else if !failures.is_empty() {
        results.add(
            ID,
            Status::Fail,
            format!("{}.", capitalise(&failures.join("; "))),
            details,
        );
    } else if !warnings.is_empty() {
        results.add(
            ID,
            Status::Warn,
            format!("{}.", capitalise(&warnings.join("; "))),
            details,
        );
    } else {
        results.add(
            ID,
            Status::Pass,
            "Zero-input quotes give no output, zero-input swaps are refused, and 1-unit swaps either fail cleanly or deliver what was quoted.",
            details,
        );
    }
}
