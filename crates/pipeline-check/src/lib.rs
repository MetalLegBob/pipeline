//! Self-service quality checks for a Pipeline strategy market.
//!
//! The checks quote and trade a market through the real router in a local Solana VM
//! and report, per check, Pass / Warn / Fail / Skip with a plain-English message and
//! the numbers behind it. They are a tool and a published signal, never an admission
//! gate: Pipeline lists every registered strategy whatever its report says.
//!
//! Two ways in:
//!
//! - From a strategy's own LiteSVM tests, after creating a market and adding liquidity:
//!   ```ignore
//!   let report = pipeline_check::check_market(&svm, market, &CheckOptions::default());
//!   assert_eq!(report.summary.fail, 0, "{}", report.to_text());
//!   ```
//! - Against a deployed market with the `pipeline-check` binary, which clones the market
//!   from a cluster over read-only RPC ([`clone_market`]) and runs the same checks.
//!
//! The report layout is documented in the crate's README.

mod checks;
pub mod clone;
pub mod hooks;
pub mod report;
#[cfg(feature = "rpc")]
pub mod rpc;
mod svm;

pub use checks::{
    check_market, check_market_with_source, CheckOptions, CHECKS, MAX_COMPUTE_UNITS,
    MAX_TRANSACTION_ACCOUNTS, MAX_TRANSACTION_SIZE, ROUTER_INSUFFICIENT_OUTPUT,
};
pub use clone::{clone_market, AccountSource, Cloned, ClusterInfo, SvmSource};
pub use report::{CheckResult, Report, Source, Status, SCHEMA};
pub use svm::{executable_sha256, write_token_account};
