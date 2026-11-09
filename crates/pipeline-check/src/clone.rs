//! Copies a market and everything its trades touch from a cluster into a local LiteSVM,
//! so the checks can trade against real state without sending anything to the cluster.
//!
//! Accounts come from an [`AccountSource`]: [`crate::rpc::RpcSource`] reads a cluster
//! over JSON-RPC (read-only calls only), and [`SvmSource`] reads another LiteSVM, which
//! lets the cloning be tested without a network.
//!
//! What is cloned: the router program (unless a local build is supplied) and its
//! config, the Market, the strategy's registration, program and ProgramData, the market
//! state, mints, vaults and fee accounts, each transfer hook's program, extra-account-meta
//! list and the extra accounts it resolves to, and every account
//! `pipeline_resolve_accounts` lists for every direction and size the checks use,
//! including programs among them. Programs LiteSVM ships (System, SPL Token, Token-2022,
//! Associated Token, Compute Budget and the loaders) and sysvars stay LiteSVM's own.

use std::collections::{BTreeSet, HashMap, HashSet};

use borsh::BorshDeserialize;
use litesvm::LiteSVM;
use pipeline_client::{self as client, pda, AccountMeta, Instruction, Pubkey};
use pipeline_strategy_interface::{self as psi, TradeMode};
use solana_account::Account;
use solana_keypair::Keypair;
use solana_signer::Signer;
use solana_transaction::Transaction;

use crate::checks::{exact_in_sizes, exact_out_sizes, CheckOptions, MAX_COMPUTE_UNITS};
use crate::hooks;
use crate::report::Source;
use crate::svm as vm;

/// The cluster's clock when cloning started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClusterInfo {
    pub slot: u64,
    pub epoch: u64,
    pub unix_timestamp: i64,
    pub genesis_hash: Option<String>,
}

/// Somewhere to read accounts from. Implementations must only read.
pub trait AccountSource {
    /// The accounts at `keys`, in order; `None` where an account does not exist.
    fn get_accounts(&mut self, keys: &[Pubkey]) -> Result<Vec<Option<Account>>, String>;
    fn cluster_info(&mut self) -> Result<ClusterInfo, String>;
}

/// An [`AccountSource`] backed by a LiteSVM, recording every address it was asked for.
pub struct SvmSource {
    svm: LiteSVM,
    info: ClusterInfo,
    pub requested: Vec<Pubkey>,
}

impl SvmSource {
    pub fn new(svm: LiteSVM) -> Self {
        let clock: solana_clock::Clock = svm.get_sysvar();
        SvmSource {
            svm,
            info: ClusterInfo {
                slot: clock.slot,
                epoch: clock.epoch,
                unix_timestamp: clock.unix_timestamp,
                genesis_hash: None,
            },
            requested: vec![],
        }
    }
}

impl AccountSource for SvmSource {
    fn get_accounts(&mut self, keys: &[Pubkey]) -> Result<Vec<Option<Account>>, String> {
        self.requested.extend_from_slice(keys);
        Ok(keys.iter().map(|key| self.svm.get_account(key)).collect())
    }

    fn cluster_info(&mut self) -> Result<ClusterInfo, String> {
        Ok(self.info.clone())
    }
}

/// A market cloned into a local VM, ready for [`crate::check_market_with_source`].
pub struct Cloned {
    pub svm: LiteSVM,
    pub cluster: ClusterInfo,
    /// Accounts copied from the source.
    pub accounts_cloned: usize,
    /// Things that could not be cloned, for the user's information.
    pub notes: Vec<String>,
}

impl Cloned {
    /// The report's description of where the state came from.
    pub fn source(&self) -> Source {
        Source {
            kind: "rpc".into(),
            cluster: self
                .cluster
                .genesis_hash
                .as_deref()
                .and_then(cluster_name)
                .map(str::to_string),
            genesis_hash: self.cluster.genesis_hash.clone(),
            slot: self.cluster.slot,
            unix_timestamp: self.cluster.unix_timestamp,
        }
    }
}

/// The well-known clusters, by genesis hash.
pub fn cluster_name(genesis_hash: &str) -> Option<&'static str> {
    match genesis_hash {
        "5eykt4UsFv8P8NJdTREpY1vzqKqZKvdpKuc147dw2N9d" => Some("mainnet-beta"),
        "EtWTRABZaYq6iMfeYKouRu166VU2xqa1wcaWoxPkrZBG" => Some("devnet"),
        "4uhcVJyU9pJkvQyS88uRDiswHXSCkY3zQawwpjk2NsNY" => Some("testnet"),
        _ => None,
    }
}

/// Clones `market` from `source`. `router_elf`, if given, replaces the cluster's router
/// program with a local build. `options` decides which trades' accounts are resolved,
/// so pass the options the checks will use.
pub fn clone_market(
    source: &mut dyn AccountSource,
    market: Pubkey,
    router_elf: Option<&[u8]>,
    options: &CheckOptions,
) -> Result<Cloned, String> {
    let cluster = source.cluster_info()?;
    let mut svm = LiteSVM::new();
    if cluster.slot > 0 {
        svm.warp_to_slot(cluster.slot);
    }
    let mut clock: solana_clock::Clock = svm.get_sysvar();
    clock.epoch = cluster.epoch;
    clock.leader_schedule_epoch = cluster.epoch + 1;
    clock.unix_timestamp = cluster.unix_timestamp;
    clock.epoch_start_timestamp = cluster.unix_timestamp;
    svm.set_sysvar(&clock);

    let mut cloner = Cloner {
        source,
        // Everything LiteSVM starts with (builtins, its bundled token programs and
        // sysvars) stays as it is.
        preexisting: HashSet::new(),
        seen: HashSet::new(),
        svm,
        accounts_cloned: 0,
        notes: vec![],
    };
    if let Some(elf) = router_elf {
        cloner
            .svm
            .add_program(client::PROGRAM_ID, elf)
            .map_err(|e| format!("the local router build does not load: {e}"))?;
        cloner.seen.insert(client::PROGRAM_ID);
        cloner.seen.insert(pda::program_data(&client::PROGRAM_ID));
    }

    cloner.fetch(&[market, pda::config(), client::PROGRAM_ID])?;
    let record = cloner
        .svm
        .get_account(&market)
        .filter(|a| a.owner == client::PROGRAM_ID)
        .and_then(|a| client::decode_market(&a.data))
        .ok_or_else(|| format!("{market} is not a Pipeline Market account"))?;
    let mint_count = record.mint_count as usize;
    let mut keys = vec![
        pda::strategy(&record.strategy_program),
        record.strategy_program,
        record.market_state,
    ];
    keys.extend(record.mints());
    keys.extend(&record.vaults[..mint_count]);
    cloner.fetch(&keys)?;

    // Fee accounts, and transfer-hook programs with their meta lists.
    let mut mints = vec![];
    for mint in record.mints() {
        match vm::mint_facts(&cloner.svm, mint) {
            Ok(facts) => mints.push(facts),
            Err(error) => return Err(format!("cannot read mint: {error}")),
        }
    }
    let mut keys = vec![];
    for mint in &mints {
        keys.push(pda::fee_account(
            &market,
            &mint.address,
            &mint.token_program,
        ));
        if let Some(hook) = mint.transfer_hook_program {
            keys.push(hook);
            keys.push(hooks::validation_address(&hook, &mint.address));
        }
    }
    cloner.fetch(&keys)?;
    cloner.fetch_hook_accounts(&record, &mints)?;
    cloner.resolve_all(&market, &record, &mints, options)?;

    Ok(Cloned {
        svm: cloner.svm,
        cluster,
        accounts_cloned: cloner.accounts_cloned,
        notes: cloner.notes,
    })
}

struct Cloner<'a> {
    source: &'a mut dyn AccountSource,
    svm: LiteSVM,
    preexisting: HashSet<Pubkey>,
    seen: HashSet<Pubkey>,
    accounts_cloned: usize,
    notes: Vec<String>,
}

impl Cloner<'_> {
    /// Copies the accounts at `keys` that are not already present. Programs are
    /// installed after their ProgramData, which the VM needs to load them.
    fn fetch(&mut self, keys: &[Pubkey]) -> Result<(), String> {
        let mut wanted = vec![];
        for key in keys {
            if self.seen.contains(key) || self.preexisting.contains(key) {
                continue;
            }
            if self.svm.get_account(key).is_some() {
                self.preexisting.insert(*key);
                continue;
            }
            self.seen.insert(*key);
            wanted.push(*key);
        }
        if wanted.is_empty() {
            return Ok(());
        }
        let accounts = self.source.get_accounts(&wanted)?;
        let mut programs = vec![];
        for (key, account) in wanted.into_iter().zip(accounts) {
            let Some(account) = account else {
                continue;
            };
            if account.owner == vm::SYSVAR_OWNER || account.owner == vm::NATIVE_LOADER {
                continue;
            }
            self.accounts_cloned += 1;
            if account.executable {
                programs.push((key, account));
            } else {
                self.install(key, account);
            }
        }
        let program_data: Vec<Pubkey> = programs
            .iter()
            .filter(|(_, account)| account.owner == vm::LOADER_V3)
            .map(|(key, _)| pda::program_data(key))
            .filter(|key| self.seen.insert(*key))
            .collect();
        if !program_data.is_empty() {
            let accounts = self.source.get_accounts(&program_data)?;
            for (key, account) in program_data.into_iter().zip(accounts) {
                if let Some(account) = account {
                    self.accounts_cloned += 1;
                    self.install(key, account);
                }
            }
        }
        for (key, account) in programs {
            self.install(key, account);
        }
        Ok(())
    }

    fn install(&mut self, key: Pubkey, account: Account) {
        if let Err(error) = self.svm.set_account(key, account) {
            self.notes.push(format!(
                "account {key} could not be loaded locally: {error}"
            ));
        }
    }

    /// Hook extra accounts can depend on other accounts' data, so resolve until no new
    /// addresses appear. Trader-specific accounts use placeholders and are not cloned;
    /// for a new trader they would not exist on the cluster either.
    fn fetch_hook_accounts(
        &mut self,
        record: &client::Market,
        mints: &[vm::MintFacts],
    ) -> Result<(), String> {
        for _ in 0..3 {
            let mut keys = vec![];
            for (i, mint) in mints.iter().enumerate() {
                let Some(hook) = mint.transfer_hook_program else {
                    continue;
                };
                let vault = record.vaults[i];
                let transfer = hooks::Transfer {
                    source: vault,
                    mint: mint.address,
                    destination: vault,
                    authority: vm::token_owner(&self.svm, &vault).unwrap_or_default(),
                    amount: 0,
                };
                let svm = &self.svm;
                if let Ok(metas) =
                    hooks::resolve(&hook, &transfer, |key| svm.get_account(key).map(|a| a.data))
                {
                    keys.extend(metas.iter().map(|m| m.pubkey));
                }
            }
            let before = self.seen.len();
            self.fetch(&keys)?;
            if self.seen.len() == before {
                break;
            }
        }
        Ok(())
    }

    /// Runs `pipeline_resolve_accounts` for every trade the checks will make, fetching
    /// each round's accounts before the next round.
    fn resolve_all(
        &mut self,
        market: &Pubkey,
        record: &client::Market,
        mints: &[vm::MintFacts],
        options: &CheckOptions,
    ) -> Result<(), String> {
        let payer = Keypair::new();
        let mut trades = BTreeSet::new();
        for input in 0..mints.len() {
            for output in (0..mints.len()).filter(|o| *o != input) {
                let reserve_in = vm::token_amount(&self.svm, &record.vaults[input]).unwrap_or(0);
                let reserve_out = vm::token_amount(&self.svm, &record.vaults[output]).unwrap_or(0);
                let mut amounts = vec![0, 1];
                amounts.extend(
                    exact_in_sizes(reserve_in, mints[input].decimals, options)
                        .into_iter()
                        .map(|(_, a)| a),
                );
                for amount in amounts {
                    trades.insert((input, output, true, amount));
                }
                if record.supports_exact_out {
                    for (_, amount) in exact_out_sizes(reserve_out) {
                        trades.insert((input, output, false, amount));
                    }
                }
            }
        }
        let mut failures = HashMap::new();
        for (input, output, exact_in, amount) in trades {
            let trade = psi::TradeRequest {
                mode: if exact_in {
                    TradeMode::ExactIn
                } else {
                    TradeMode::ExactOut
                },
                input_index: input as u8,
                output_index: output as u8,
                amount,
                trader: None,
                extra: options.extra.clone(),
            };
            if let Err(error) = self.resolve(&payer, market, record, mints, &trade) {
                failures.entry(error).or_insert((input, output, amount));
            }
        }
        for (error, (input, output, amount)) in failures {
            self.notes.push(format!(
                "resolving {input}→{output} for {amount} stopped early: {error}"
            ));
        }
        Ok(())
    }

    fn resolve(
        &mut self,
        payer: &Keypair,
        market: &Pubkey,
        record: &client::Market,
        mints: &[vm::MintFacts],
        trade: &psi::TradeRequest,
    ) -> Result<(), String> {
        let input = trade.input_index as usize;
        let output = trade.output_index as usize;
        let trader = Pubkey::default();
        let fixed = vec![
            AccountMeta::new_readonly(*market, false),
            AccountMeta::new_readonly(record.market_state, false),
            AccountMeta::new_readonly(trader, false),
            AccountMeta::new_readonly(
                pda::associated_token(&trader, &mints[input].address, &mints[input].token_program),
                false,
            ),
            AccountMeta::new_readonly(
                pda::associated_token(
                    &trader,
                    &mints[output].address,
                    &mints[output].token_program,
                ),
                false,
            ),
            AccountMeta::new_readonly(record.vaults[input], false),
            AccountMeta::new_readonly(mints[input].address, false),
            AccountMeta::new_readonly(mints[output].address, false),
            AccountMeta::new_readonly(mints[input].token_program, false),
            AccountMeta::new_readonly(mints[output].token_program, false),
        ];
        vm::fund_lamports(&mut self.svm, &payer.pubkey(), 1_000_000_000);
        let mut known: Vec<Pubkey> = vec![];
        for round in 0..psi::MAX_RESOLVE_ROUNDS {
            let mut accounts = fixed.clone();
            accounts.extend(known.iter().map(|k| AccountMeta::new_readonly(*k, false)));
            let data = psi::instruction_data(
                psi::discriminator::RESOLVE_ACCOUNTS,
                &psi::ResolveRequest {
                    trade: trade.clone(),
                    round,
                },
            );
            let ix = Instruction::new_with_bytes(record.strategy_program, &data, accounts);
            self.svm.expire_blockhash();
            let tx = Transaction::new_signed_with_payer(
                &[vm::compute_limit_instruction(MAX_COMPUTE_UNITS), ix],
                Some(&payer.pubkey()),
                &[payer],
                self.svm.latest_blockhash(),
            );
            let info = self
                .svm
                .simulate_transaction(tx)
                .map_err(|failed| vm::failure_text(&failed))?;
            if info.meta.return_data.program_id != record.strategy_program {
                return Err("no return data".into());
            }
            let result = psi::ResolveResult::try_from_slice(&info.meta.return_data.data)
                .map_err(|_| "malformed return data".to_string())?;
            known = result
                .accounts
                .iter()
                .map(|a| Pubkey::new_from_array(a.address))
                .collect();
            self.fetch(&known)?;
            if result.complete {
                return Ok(());
            }
        }
        Err(format!(
            "not complete after {} rounds",
            psi::MAX_RESOLVE_ROUNDS
        ))
    }
}
