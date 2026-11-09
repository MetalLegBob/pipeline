//! The check report: a stable, documented JSON shape (see the crate README) and a
//! plain-text rendering for people.

use std::fmt::Write as _;

use serde::Serialize;

/// Identifies the JSON layout. Bumped only for incompatible changes; new optional
/// fields and new checks do not change it.
pub const SCHEMA: &str = "pipeline-check-report/1";

/// The outcome of one check. Ordered from least to most serious, so the overall result
/// is the maximum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Not applicable to this market, or a prerequisite failed.
    Skip,
    Pass,
    /// Works, but something integrators or traders should know about.
    Warn,
    /// Broken in a way that makes quoting or trading through Pipeline misbehave.
    Fail,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Skip => "SKIP",
            Status::Pass => "PASS",
            Status::Warn => "WARN",
            Status::Fail => "FAIL",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct CheckResult {
    /// Stable identifier, such as `quote_matches_swap`.
    pub id: String,
    pub title: String,
    pub status: Status,
    /// One or two plain-English sentences.
    pub message: String,
    /// Check-specific numbers. Informational: fields may be added over time.
    pub details: serde_json::Value,
}

/// Where the market's state came from.
#[derive(Clone, Debug, Serialize)]
pub struct Source {
    /// `"svm"` for a caller-supplied LiteSVM, `"rpc"` for a market cloned from a cluster.
    pub kind: String,
    /// `"devnet"`, `"mainnet-beta"`, `"testnet"` or `null` if unknown.
    pub cluster: Option<String>,
    pub genesis_hash: Option<String>,
    /// The slot the state was read at (the VM's clock slot for `"svm"`).
    pub slot: u64,
    pub unix_timestamp: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProgramSummary {
    pub program_id: String,
    /// `bpf-loader-upgradeable`, `loader-v4`, `bpf-loader-2`, `bpf-loader-1`, or `null`
    /// if the program could not be read.
    pub loader: Option<String>,
    /// SHA-256 (hex) of the executable with trailing zero bytes removed: for the
    /// upgradeable loader, the ProgramData account's data after its 45-byte header.
    pub executable_sha256: Option<String>,
    /// Length in bytes of what was hashed.
    pub executable_len: Option<usize>,
    pub upgrade_authority: Option<String>,
    pub deploy_slot: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct MintSummary {
    pub address: String,
    pub token_program: String,
    pub decimals: u8,
    pub mint_authority: Option<String>,
    pub freeze_authority: Option<String>,
    pub extensions: Vec<String>,
    pub vault: String,
    pub vault_balance: Option<u64>,
    pub fee_account: String,
    pub fee_account_exists: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct MarketSummary {
    pub address: String,
    pub market_state: Option<String>,
    pub supports_exact_out: Option<bool>,
    pub mints: Vec<MintSummary>,
}

#[derive(Clone, Debug, Serialize)]
pub struct OptionsSummary {
    /// Explicit exact-in sizes, if the caller gave any.
    pub amounts: Option<Vec<u64>>,
    /// `TradeRequest::extra`, as hex.
    pub extra: String,
    pub quote_compute_limit: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct Summary {
    /// The most serious status of any check.
    pub result: Status,
    pub pass: usize,
    pub warn: usize,
    pub fail: usize,
    pub skip: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub schema: String,
    pub checker_version: String,
    /// Unix seconds when the report was produced.
    pub generated_at: u64,
    pub source: Source,
    pub router: ProgramSummary,
    pub strategy: ProgramSummary,
    /// The strategy's registered metadata URL, if it is registered.
    pub strategy_metadata_uri: Option<String>,
    pub market: MarketSummary,
    pub options: OptionsSummary,
    pub summary: Summary,
    pub checks: Vec<CheckResult>,
}

impl Report {
    pub fn check(&self, id: &str) -> Option<&CheckResult> {
        self.checks.iter().find(|c| c.id == id)
    }

    pub fn status(&self, id: &str) -> Option<Status> {
        self.check(id).map(|c| c.status)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("a report always serialises")
    }

    pub fn to_text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "Pipeline check {}", self.checker_version);
        let _ = writeln!(out, "  market    {}", self.market.address);
        let _ = writeln!(
            out,
            "  strategy  {}  sha256 {}",
            self.strategy.program_id,
            self.strategy
                .executable_sha256
                .as_deref()
                .unwrap_or("unavailable")
        );
        let _ = writeln!(
            out,
            "  router    {}  sha256 {}",
            self.router.program_id,
            self.router
                .executable_sha256
                .as_deref()
                .unwrap_or("unavailable")
        );
        let source = match (&self.source.kind[..], &self.source.cluster) {
            ("rpc", Some(cluster)) => format!("cloned from {cluster} at slot {}", self.source.slot),
            ("rpc", None) => format!("cloned from RPC at slot {}", self.source.slot),
            _ => format!("local VM at slot {}", self.source.slot),
        };
        let _ = writeln!(out, "  source    {source}");
        for (i, mint) in self.market.mints.iter().enumerate() {
            let extensions = if mint.extensions.is_empty() {
                String::new()
            } else {
                format!(" [{}]", mint.extensions.join(", "))
            };
            let _ = writeln!(
                out,
                "  mint {i}    {} ({} decimals){extensions}, vault holds {}",
                mint.address,
                mint.decimals,
                mint.vault_balance
                    .map_or("nothing (missing)".to_string(), |b| b.to_string())
            );
        }
        let _ = writeln!(out);
        let width = self.checks.iter().map(|c| c.id.len()).max().unwrap_or(0);
        for check in &self.checks {
            let _ = writeln!(
                out,
                "  {}  {:width$}  {}",
                check.status.label(),
                check.id,
                check.message
            );
        }
        let _ = writeln!(out);
        let _ = writeln!(
            out,
            "Result: {} ({} pass, {} warn, {} fail, {} skip)",
            self.summary.result.label(),
            self.summary.pass,
            self.summary.warn,
            self.summary.fail,
            self.summary.skip
        );
        out
    }
}

pub(crate) fn summarize(checks: &[CheckResult]) -> Summary {
    let count = |status| checks.iter().filter(|c| c.status == status).count();
    Summary {
        result: checks
            .iter()
            .map(|c| c.status)
            .max()
            .unwrap_or(Status::Skip),
        pass: count(Status::Pass),
        warn: count(Status::Warn),
        fail: count(Status::Fail),
        skip: count(Status::Skip),
    }
}
