//! Read-only trust facts about strategy programs and mints, and the machine-readable flags
//! front ends filter on. Facts only: no scores and no safe/unsafe judgement.
//!
//! This is a port of the TypeScript SDK's `inspect.ts` and must give the same flags for
//! the same bytes. `tests/fixtures/trust-facts.json` holds shared vectors that both
//! test suites check; regenerate it with
//! `cargo run -p pipeline-client --example ts_facts_fixtures`.
//!
//! Everything here decodes bytes the caller has fetched; nothing talks to the network.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use solana_pubkey::{pubkey, Pubkey};

use crate::{BPF_LOADER_UPGRADEABLE_ID, TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID};

// ---------------------------------------------------------------------------------------
// Flags
// ---------------------------------------------------------------------------------------

/// Every flag with its exact meaning, in the SDK's stable order. Names are stable: new
/// flags may be added, but existing ones keep their meaning. The texts equal the SDK's
/// `FLAG_DESCRIPTIONS`.
pub const FLAGS: &[(&str, &str)] = &[
    // Strategy program
    (
        "upgradeable",
        "The strategy program can be upgraded: it has an upgrade authority (loader v3) or is not finalized (loader v4).",
    ),
    (
        "frozen",
        "The strategy program can no longer be upgraded: no upgrade authority (loader v3), finalized (loader v4), or a non-upgradeable loader.",
    ),
    (
        "program-missing",
        "No executable code at the strategy program address (never deployed, closed, or retracted).",
    ),
    (
        "verified-build",
        "OtterSec's verified-builds API reports the program as built from public source, and its hash matches the deployed code. Mainnet only; needs the SDK to hash the program (the default).",
    ),
    (
        "unverified-build",
        "OtterSec's verified-builds API answered but does not report a verified build of the deployed code. Not set when the API could not be reached, off mainnet, or when the program wasn't hashed.",
    ),
    // Registry and metadata
    (
        "unregistered",
        "The program has no Strategy account in the router.",
    ),
    ("metadata-missing", "The strategy's metadata URI is empty."),
    (
        "metadata-unreachable",
        "The metadata URI is unsupported, or could not be fetched (network error, timeout, HTTP error, too large).",
    ),
    (
        "metadata-invalid",
        "The metadata document was fetched but is not valid strategy metadata version 1.",
    ),
    (
        "conformance-report",
        "Valid metadata links a pipeline-check conformance report for the deployed code: its program id and executable hash match.",
    ),
    (
        "conformance-stale",
        "Valid metadata links a pipeline-check conformance report for other code: its program id or executable hash differs from what is deployed.",
    ),
    // Hosted checks (set by the hosted API, which runs pipeline-check on every market)
    (
        "checks-passed",
        "The hosted checks' latest run against the strategy's current code had no failures (warnings and skips allowed).",
    ),
    (
        "checks-failed",
        "The hosted checks' latest run against the strategy's current code had at least one failure.",
    ),
    (
        "checks-pending",
        "No hosted checks result for the strategy's current code yet: never run, or the strategy changed since the last run.",
    ),
    // Mints
    (
        "mint-unreadable",
        "A market mint account is missing or is not an SPL Token or Token-2022 mint.",
    ),
    ("token-2022", "A mint is owned by Token-2022."),
    (
        "mint-authority",
        "A mint has a mint authority, so its supply can grow.",
    ),
    (
        "freeze-authority",
        "A mint has a freeze authority, which can freeze token accounts (including market vaults and fee accounts).",
    ),
    (
        "transfer-hook",
        "A mint has a transfer hook program set: every transfer calls it.",
    ),
    (
        "transfer-hook-authority",
        "A mint's transfer hook has an authority, which can set or change the hook program.",
    ),
    (
        "transfer-fee",
        "A mint charges a non-zero transfer fee now, or from a scheduled epoch.",
    ),
    (
        "transfer-fee-authority",
        "A mint's transfer fee has an authority, which can set or change the fee.",
    ),
    (
        "permanent-delegate",
        "A mint has a permanent delegate, which can transfer or burn tokens from any account.",
    ),
    ("non-transferable", "A mint is non-transferable."),
    (
        "default-frozen",
        "New token accounts for a mint start frozen (DefaultAccountState).",
    ),
    (
        "pausable",
        "A mint is pausable: an authority can pause minting, burning and transfers.",
    ),
    ("paused", "A mint is currently paused."),
    ("closable-mint", "A mint has a close authority."),
    (
        "interest-bearing",
        "A mint is interest-bearing: its UI amount differs from the raw amount.",
    ),
    (
        "scaled-ui-amount",
        "A mint scales its UI amount by a multiplier an authority can change.",
    ),
    (
        "confidential-transfers",
        "A mint supports confidential transfers.",
    ),
    (
        "permissioned-burn",
        "Burning a mint requires an authority\u{2019}s approval.",
    ),
    (
        "unknown-extension",
        "A mint has a Token-2022 extension this SDK does not decode.",
    ),
];

/// The flag's static name, or `None` if `name` is not a flag.
pub fn flag(name: &str) -> Option<&'static str> {
    FLAGS.iter().find(|(n, _)| *n == name).map(|(n, _)| *n)
}

pub fn flag_description(name: &str) -> Option<&'static str> {
    FLAGS.iter().find(|(n, _)| *n == name).map(|(_, d)| *d)
}

/// The flags in `flags` that are known, without duplicates, in [`FLAGS`] order.
pub fn ordered_flags<'a>(flags: impl IntoIterator<Item = &'a str>) -> Vec<&'static str> {
    let set: Vec<&str> = flags.into_iter().collect();
    FLAGS
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| set.contains(name))
        .collect()
}

/// Whether an item with `flags` has every `require` flag and none of the `exclude` flags,
/// like the SDK's `filterMarkets`.
pub fn passes_filter(flags: &[&str], require: &[&str], exclude: &[&str]) -> bool {
    require.iter().all(|flag| flags.contains(flag))
        && !exclude.iter().any(|flag| flags.contains(flag))
}

// ---------------------------------------------------------------------------------------
// Accounts and hashing
// ---------------------------------------------------------------------------------------

/// The parts of an account the facts depend on.
#[derive(Clone, Copy, Debug)]
pub struct AccountView<'a> {
    pub owner: &'a Pubkey,
    pub executable: bool,
    pub data: &'a [u8],
}

fn read_pubkey(data: &[u8], offset: usize) -> Option<Pubkey> {
    let bytes: [u8; 32] = data.get(offset..offset + 32)?.try_into().ok()?;
    Some(Pubkey::new_from_array(bytes))
}

/// Token-2022's `OptionalNonZeroPubkey`: all zeros means none.
fn optional_pubkey(data: &[u8], offset: usize) -> Option<Pubkey> {
    read_pubkey(data, offset).filter(|key| key.to_bytes() != [0; 32])
}

/// SPL Token's `COption<Pubkey>`: a u32 tag, then 32 bytes. Any non-zero tag is "some",
/// as the SDK reads it.
fn c_option_pubkey(data: &[u8], offset: usize) -> Option<Pubkey> {
    if read_u32(data, offset)? == 0 {
        return None;
    }
    read_pubkey(data, offset + 4)
}

fn read_u16(data: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        data.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn read_i16(data: &[u8], offset: usize) -> Option<i16> {
    Some(i16::from_le_bytes(
        data.get(offset..offset + 2)?.try_into().ok()?,
    ))
}

fn read_u32(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        data.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_u64(data: &[u8], offset: usize) -> Option<u64> {
    Some(u64::from_le_bytes(
        data.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

fn read_i64(data: &[u8], offset: usize) -> Option<i64> {
    Some(i64::from_le_bytes(
        data.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

fn read_f64(data: &[u8], offset: usize) -> Option<f64> {
    Some(f64::from_le_bytes(
        data.get(offset..offset + 8)?.try_into().ok()?,
    ))
}

/// Lowercase hex SHA-256 of `executable` with trailing zero bytes removed. This is what
/// `solana-verify get-program-hash` prints and OtterSec reports as `on_chain_hash`.
pub fn executable_sha256(executable: &[u8]) -> String {
    let len = executable
        .iter()
        .rposition(|byte| *byte != 0)
        .map_or(0, |i| i + 1);
    hex(&Sha256::digest(&executable[..len]))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn opt_key(key: &Option<Pubkey>) -> Value {
    key.map_or(Value::Null, |k| json!(k.to_string()))
}

// ---------------------------------------------------------------------------------------
// Programs
// ---------------------------------------------------------------------------------------

pub const LOADER_V1_ID: Pubkey = pubkey!("BPFLoader1111111111111111111111111111111111");
pub const LOADER_V2_ID: Pubkey = pubkey!("BPFLoader2111111111111111111111111111111111");
pub const LOADER_V4_ID: Pubkey = pubkey!("LoaderV411111111111111111111111111111111111");
pub const NATIVE_LOADER_ID: Pubkey = pubkey!("NativeLoader1111111111111111111111111111111");

/// Bytes before the ELF in a loader-v3 ProgramData account: a u32 state tag, the u64
/// deploy slot, and an `Option<Pubkey>` upgrade authority (1 + 32 bytes).
pub const PROGRAMDATA_METADATA_LEN: usize = 45;
/// Bytes before the ELF in a loader-v4 program account: the u64 deploy slot, the
/// authority (or next version) and a u64 status.
pub const LOADER_V4_METADATA_LEN: usize = 48;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgramLoader {
    Upgradeable,
    V4,
    V2,
    V1,
    Native,
    Other,
    /// No account at the program address.
    None,
}

impl ProgramLoader {
    /// The SDK's `ProgramLoader` string.
    pub fn as_str(self) -> &'static str {
        match self {
            ProgramLoader::Upgradeable => "upgradeable",
            ProgramLoader::V4 => "v4",
            ProgramLoader::V2 => "v2",
            ProgramLoader::V1 => "v1",
            ProgramLoader::Native => "native",
            ProgramLoader::Other => "other",
            ProgramLoader::None => "none",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoaderV4Status {
    Retracted,
    Deployed,
    Finalized,
    /// A status number this module doesn't know; it sets neither program flag.
    Unknown,
}

impl LoaderV4Status {
    pub fn as_str(self) -> &'static str {
        match self {
            LoaderV4Status::Retracted => "retracted",
            LoaderV4Status::Deployed => "deployed",
            LoaderV4Status::Finalized => "finalized",
            LoaderV4Status::Unknown => "unknown",
        }
    }
}

/// Loader, upgradeability, upgrade authority, last deploy slot and code hash of a program,
/// as the SDK's `ProgramInspection`.
#[derive(Clone, Debug, PartialEq)]
pub struct ProgramFacts {
    pub loader: ProgramLoader,
    /// The loader (owner) address, or `None` with no account.
    pub loader_address: Option<Pubkey>,
    /// Executable code is deployed.
    pub deployed: bool,
    pub upgradeable: bool,
    /// `None` when the program is frozen, or for loaders without one.
    pub upgrade_authority: Option<Pubkey>,
    /// Loader v3 only.
    pub program_data_address: Option<Pubkey>,
    /// Slot of the last deployment or upgrade (loaders v3 and v4).
    pub last_deploy_slot: Option<u64>,
    /// Loader v4 only.
    pub loader_v4_status: Option<LoaderV4Status>,
    /// See [`executable_sha256`]. For loader v3 the ELF is the ProgramData account after
    /// its 45-byte header; for loaders v1 and v2 the whole program account; for loader v4
    /// the program account after its 48-byte header. `None` when not computed or not
    /// deployed.
    pub executable_sha256: Option<String>,
    pub flags: Vec<&'static str>,
}

/// The ProgramData address a loader-v3 program account points to.
pub fn program_data_address(program: &AccountView) -> Option<Pubkey> {
    // UpgradeableLoaderState::Program { programdata_address }: u32 tag 2, then 32 bytes.
    if *program.owner != BPF_LOADER_UPGRADEABLE_ID || read_u32(program.data, 0)? != 2 {
        return None;
    }
    read_pubkey(program.data, 4)
}

/// Program facts from the program account and, for loader v3, the ProgramData account
/// it points to. With `hash` false, the data may be just the headers (the first 48 bytes
/// of the program account and 45 of the ProgramData account) and no hash is computed.
pub fn inspect_program(
    program: Option<&AccountView>,
    program_data: Option<&AccountView>,
    hash: bool,
) -> ProgramFacts {
    let mut facts = ProgramFacts {
        loader: ProgramLoader::None,
        loader_address: None,
        deployed: false,
        upgradeable: false,
        upgrade_authority: None,
        program_data_address: None,
        last_deploy_slot: None,
        loader_v4_status: None,
        executable_sha256: None,
        flags: vec![],
    };
    if let Some(account) = program {
        facts.loader_address = Some(*account.owner);
        let owner = *account.owner;
        if owner == BPF_LOADER_UPGRADEABLE_ID {
            facts.loader = ProgramLoader::Upgradeable;
            inspect_upgradeable(account, program_data, &mut facts, hash);
        } else if owner == LOADER_V4_ID {
            facts.loader = ProgramLoader::V4;
            inspect_loader_v4(account, &mut facts, hash);
        } else if owner == LOADER_V1_ID || owner == LOADER_V2_ID {
            facts.loader = if owner == LOADER_V1_ID {
                ProgramLoader::V1
            } else {
                ProgramLoader::V2
            };
            facts.deployed = account.executable && !account.data.is_empty();
            if hash && facts.deployed {
                facts.executable_sha256 = Some(executable_sha256(account.data));
            }
        } else if owner == NATIVE_LOADER_ID {
            facts.loader = ProgramLoader::Native;
            facts.deployed = account.executable;
        } else {
            facts.loader = ProgramLoader::Other;
            facts.deployed = account.executable;
        }
    }
    facts.flags = program_flags(&facts);
    facts
}

fn inspect_upgradeable(
    account: &AccountView,
    program_data: Option<&AccountView>,
    facts: &mut ProgramFacts,
    hash: bool,
) {
    if account.data.len() < 36 {
        return;
    }
    let Some(address) = program_data_address(account) else {
        return;
    };
    facts.program_data_address = Some(address);
    // UpgradeableLoaderState::ProgramData { slot: u64, upgrade_authority: Option<Pubkey> }.
    let Some(data) = program_data else {
        return;
    };
    if *data.owner != BPF_LOADER_UPGRADEABLE_ID
        || data.data.len() < PROGRAMDATA_METADATA_LEN
        || read_u32(data.data, 0) != Some(3)
    {
        return;
    }
    facts.deployed = account.executable;
    facts.last_deploy_slot = read_u64(data.data, 4);
    facts.upgrade_authority = if data.data[12] == 1 {
        read_pubkey(data.data, 13)
    } else {
        None
    };
    facts.upgradeable = facts.upgrade_authority.is_some();
    if hash {
        facts.executable_sha256 = Some(executable_sha256(&data.data[PROGRAMDATA_METADATA_LEN..]));
    }
}

fn inspect_loader_v4(account: &AccountView, facts: &mut ProgramFacts, hash: bool) {
    // LoaderV4State { slot: u64, authority_address_or_next_version: Pubkey, status: u64 }.
    if account.data.len() < LOADER_V4_METADATA_LEN {
        return;
    }
    let status = match read_u64(account.data, 40) {
        Some(0) => LoaderV4Status::Retracted,
        Some(1) => LoaderV4Status::Deployed,
        Some(2) => LoaderV4Status::Finalized,
        _ => LoaderV4Status::Unknown,
    };
    facts.loader_v4_status = Some(status);
    facts.last_deploy_slot = read_u64(account.data, 0);
    facts.deployed = status != LoaderV4Status::Retracted;
    if matches!(status, LoaderV4Status::Retracted | LoaderV4Status::Deployed) {
        facts.upgradeable = true;
        facts.upgrade_authority = read_pubkey(account.data, 8);
    }
    if hash && facts.deployed {
        facts.executable_sha256 = Some(executable_sha256(&account.data[LOADER_V4_METADATA_LEN..]));
    }
}

fn program_flags(facts: &ProgramFacts) -> Vec<&'static str> {
    if !facts.deployed {
        vec!["program-missing"]
    } else if facts.upgradeable {
        vec!["upgradeable"]
    } else if !matches!(facts.loader, ProgramLoader::Native | ProgramLoader::Other)
        && facts.loader_v4_status != Some(LoaderV4Status::Unknown)
    {
        vec!["frozen"]
    } else {
        vec![]
    }
}

impl ProgramFacts {
    /// snake_case JSON with addresses in base58 and the slot as a number.
    pub fn to_json(&self) -> Value {
        json!({
            "loader": self.loader.as_str(),
            "loader_address": opt_key(&self.loader_address),
            "deployed": self.deployed,
            "upgradeable": self.upgradeable,
            "upgrade_authority": opt_key(&self.upgrade_authority),
            "program_data_address": opt_key(&self.program_data_address),
            "last_deploy_slot": self.last_deploy_slot,
            "loader_v4_status": self.loader_v4_status.map(LoaderV4Status::as_str),
            "executable_sha256": self.executable_sha256,
            "flags": self.flags,
        })
    }
}

// ---------------------------------------------------------------------------------------
// Verified builds (OtterSec)
// ---------------------------------------------------------------------------------------

pub const DEFAULT_VERIFY_API_URL: &str = "https://verify.osec.io";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifiedBuildStatus {
    Verified,
    Unverified,
    /// The API could not be reached or answered unexpectedly.
    Unknown,
    /// The program is not on mainnet, which the API covers.
    NotApplicable,
}

impl VerifiedBuildStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            VerifiedBuildStatus::Verified => "verified",
            VerifiedBuildStatus::Unverified => "unverified",
            VerifiedBuildStatus::Unknown => "unknown",
            VerifiedBuildStatus::NotApplicable => "not-applicable",
        }
    }
}

/// A program's record in OtterSec's verified-builds API, as the SDK's `VerifiedBuild`.
#[derive(Clone, Debug, PartialEq)]
pub struct VerifiedBuild {
    pub status: VerifiedBuildStatus,
    pub verified: bool,
    pub repository: Option<String>,
    pub commit: Option<String>,
    /// OtterSec's hash of the deployed code (as [`executable_sha256`]).
    pub onchain_hash: Option<String>,
    /// OtterSec's hash of the build from source.
    pub executable_hash: Option<String>,
    /// ISO timestamp without a time zone, as the API returns it.
    pub last_verified_at: Option<String>,
    pub message: Option<String>,
    /// Why the status is `Unknown`.
    pub error: Option<String>,
}

impl VerifiedBuild {
    pub fn none(status: VerifiedBuildStatus, error: Option<String>) -> Self {
        VerifiedBuild {
            status,
            verified: false,
            repository: None,
            commit: None,
            onchain_hash: None,
            executable_hash: None,
            last_verified_at: None,
            message: None,
            error,
        }
    }

    /// Reads a `GET <api>/status/<program>` response body.
    pub fn from_response(body: &Value) -> Self {
        let Some(verified) = body.get("is_verified").and_then(Value::as_bool) else {
            return Self::none(
                VerifiedBuildStatus::Unknown,
                Some("unexpected response".into()),
            );
        };
        // The API reports a missing commit as the string "None", and other gaps as "".
        let text = |key: &str| {
            body.get(key)
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty() && *v != "None")
                .map(String::from)
        };
        VerifiedBuild {
            status: if verified {
                VerifiedBuildStatus::Verified
            } else {
                VerifiedBuildStatus::Unverified
            },
            verified,
            repository: text("repo_url"),
            commit: text("commit"),
            onchain_hash: text("on_chain_hash"),
            executable_hash: text("executable_hash"),
            last_verified_at: text("last_verified_at"),
            message: text("message"),
            error: None,
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "status": self.status.as_str(),
            "verified": self.verified,
            "repository": self.repository,
            "commit": self.commit,
            "onchain_hash": self.onchain_hash,
            "executable_hash": self.executable_hash,
            "last_verified_at": self.last_verified_at,
            "message": self.message,
            "error": self.error,
        })
    }
}

// ---------------------------------------------------------------------------------------
// Strategies
// ---------------------------------------------------------------------------------------

/// What became of a strategy's metadata document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetadataStatus {
    /// The URI is empty.
    Missing,
    NotFetched,
    /// Unsupported URI, network error, timeout, HTTP error or too large.
    Unreachable,
    /// Fetched, but not JSON or not valid version 1 metadata.
    Invalid,
    Valid,
}

impl MetadataStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            MetadataStatus::Missing => "missing",
            MetadataStatus::NotFetched => "not-fetched",
            MetadataStatus::Unreachable => "unreachable",
            MetadataStatus::Invalid => "invalid",
            MetadataStatus::Valid => "valid",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "missing" => MetadataStatus::Missing,
            "not-fetched" => MetadataStatus::NotFetched,
            "unreachable" => MetadataStatus::Unreachable,
            "invalid" => MetadataStatus::Invalid,
            "valid" => MetadataStatus::Valid,
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------------------
// Conformance reports
// ---------------------------------------------------------------------------------------

/// The largest conformance report fetched.
pub const CONFORMANCE_REPORT_MAX_BYTES: usize = 1024 * 1024;
/// The `schema` of a `pipeline-check` JSON report.
pub const CHECK_REPORT_SCHEMA: &str = "pipeline-check-report/1";

/// Whether a strategy's self-published conformance report is about its deployed code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConformanceStatus {
    /// The report's strategy program id and executable hash match the deployed program.
    Current,
    /// The report is about another program, or other code than is deployed.
    Stale,
    /// The report could not be checked; `reason` says why. Neither flag is set.
    Unverified,
}

impl ConformanceStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            ConformanceStatus::Current => "current",
            ConformanceStatus::Stale => "stale",
            ConformanceStatus::Unverified => "unverified",
        }
    }
}

/// What a conformance report says, and whether it matches the deployed program.
#[derive(Clone, Debug, PartialEq)]
pub struct ConformanceFacts {
    pub status: ConformanceStatus,
    /// Why the status isn't `Current`: `unreachable` (set by the fetcher),
    /// `sha256-mismatch` (the body isn't the report `conformance.sha256` pins),
    /// `not-json`, `not-a-report` (no `pipeline-check-report/1` strategy section),
    /// `code-not-hashed` (the deployed code's hash is unknown), `other-program` or
    /// `other-code`.
    pub reason: Option<&'static str>,
    /// Lowercase hex SHA-256 of the report as fetched.
    pub report_sha256: Option<String>,
    pub program_id: Option<String>,
    pub executable_sha256: Option<String>,
    /// The report's overall result (`pass`, `warn`, `fail`), as published.
    pub result: Option<String>,
    /// Unix seconds when the report was produced, as published.
    pub generated_at: Option<u64>,
}

impl ConformanceFacts {
    /// A report that could not be fetched.
    pub fn unreachable() -> Self {
        ConformanceFacts {
            status: ConformanceStatus::Unverified,
            reason: Some("unreachable"),
            report_sha256: None,
            program_id: None,
            executable_sha256: None,
            result: None,
            generated_at: None,
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "status": self.status.as_str(),
            "reason": self.reason,
            "report_sha256": self.report_sha256,
            "program_id": self.program_id,
            "executable_sha256": self.executable_sha256,
            "result": self.result,
            "generated_at": self.generated_at,
        })
    }
}

/// Checks a fetched conformance report against the deployed program, as the SDK's
/// `evaluateConformanceReport` does. `pinned_sha256` is the metadata's
/// `conformance.sha256`, if any; `deployed_sha256` is [`executable_sha256`] of the
/// deployed code, if known.
pub fn evaluate_conformance_report(
    body: &[u8],
    pinned_sha256: Option<&str>,
    program_id: &Pubkey,
    deployed_sha256: Option<&str>,
) -> ConformanceFacts {
    let report_sha256 = hex(&Sha256::digest(body));
    let mut facts = ConformanceFacts {
        status: ConformanceStatus::Unverified,
        reason: None,
        report_sha256: Some(report_sha256.clone()),
        program_id: None,
        executable_sha256: None,
        result: None,
        generated_at: None,
    };
    let with_reason = |mut facts: ConformanceFacts, reason| {
        facts.reason = Some(reason);
        facts
    };
    if pinned_sha256.is_some_and(|pinned| !pinned.eq_ignore_ascii_case(&report_sha256)) {
        return with_reason(facts, "sha256-mismatch");
    }
    // A leading byte-order mark is skipped, as `TextDecoder` does.
    let text = body.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(body);
    let Ok(report) = serde_json::from_slice::<Value>(text) else {
        return with_reason(facts, "not-json");
    };
    let strategy = &report["strategy"];
    let (Some(id), Some(hash)) = (
        strategy["program_id"].as_str(),
        strategy["executable_sha256"].as_str(),
    ) else {
        return with_reason(facts, "not-a-report");
    };
    if report["schema"].as_str() != Some(CHECK_REPORT_SCHEMA) {
        return with_reason(facts, "not-a-report");
    }
    facts.program_id = Some(id.to_string());
    facts.executable_sha256 = Some(hash.to_string());
    facts.result = report["summary"]["result"].as_str().map(String::from);
    facts.generated_at = report["generated_at"].as_u64();
    facts.recheck(program_id, deployed_sha256);
    facts
}

impl ConformanceFacts {
    /// Compares a readable report with the deployed program again, for example after an
    /// upgrade, without fetching it again. Reports that could not be read stay as they are.
    pub fn recheck(&mut self, program_id: &Pubkey, deployed_sha256: Option<&str>) {
        let (Some(id), Some(hash)) = (&self.program_id, &self.executable_sha256) else {
            return;
        };
        let (status, reason) = if *id != program_id.to_string() {
            (ConformanceStatus::Stale, Some("other-program"))
        } else {
            match deployed_sha256 {
                None => (ConformanceStatus::Unverified, Some("code-not-hashed")),
                Some(deployed) if !hash.eq_ignore_ascii_case(deployed) => {
                    (ConformanceStatus::Stale, Some("other-code"))
                }
                Some(_) => (ConformanceStatus::Current, None),
            }
        };
        self.status = status;
        self.reason = reason;
    }

    /// Reads back [`ConformanceFacts::to_json`].
    pub fn from_json(value: &Value) -> Option<Self> {
        let status = match value["status"].as_str()? {
            "current" => ConformanceStatus::Current,
            "stale" => ConformanceStatus::Stale,
            "unverified" => ConformanceStatus::Unverified,
            _ => return None,
        };
        const REASONS: [&str; 7] = [
            "unreachable",
            "sha256-mismatch",
            "not-json",
            "not-a-report",
            "code-not-hashed",
            "other-program",
            "other-code",
        ];
        let reason = match value["reason"].as_str() {
            Some(text) => Some(*REASONS.iter().find(|r| **r == text)?),
            None => None,
        };
        let text = |key: &str| value[key].as_str().map(String::from);
        Some(ConformanceFacts {
            status,
            reason,
            report_sha256: text("report_sha256"),
            program_id: text("program_id"),
            executable_sha256: text("executable_sha256"),
            result: text("result"),
            generated_at: value["generated_at"].as_u64(),
        })
    }
}

/// A strategy's flags, as the SDK's `inspectStrategy` sets them.
///
/// - `has_conformance`: the metadata is valid and has a `conformance` entry.
/// - Build flags need our own hash of the deployed code: OtterSec's record can be stale
///   (the program upgraded since), so its hash must equal what is deployed now. Without
///   a hash, or without an answer from the API, neither flag is set.
pub fn strategy_flags(
    program: &ProgramFacts,
    registered: bool,
    metadata: MetadataStatus,
    conformance: Option<ConformanceStatus>,
    verified_build: Option<&VerifiedBuild>,
) -> Vec<&'static str> {
    let mut flags = program.flags.clone();
    if !registered {
        flags.push("unregistered");
    } else if metadata == MetadataStatus::Missing {
        flags.push("metadata-missing");
    }
    match metadata {
        MetadataStatus::Unreachable => flags.push("metadata-unreachable"),
        MetadataStatus::Invalid => flags.push("metadata-invalid"),
        MetadataStatus::Valid => match conformance {
            Some(ConformanceStatus::Current) => flags.push("conformance-report"),
            Some(ConformanceStatus::Stale) => flags.push("conformance-stale"),
            Some(ConformanceStatus::Unverified) | None => {}
        },
        _ => {}
    }
    if let Some(build) = verified_build {
        let answered = matches!(
            build.status,
            VerifiedBuildStatus::Verified | VerifiedBuildStatus::Unverified
        );
        if answered && program.deployed {
            if let Some(hash) = &program.executable_sha256 {
                let matches = build.onchain_hash.as_deref() == Some(hash.as_str());
                flags.push(if build.verified && matches {
                    "verified-build"
                } else {
                    "unverified-build"
                });
            }
        }
    }
    ordered_flags(flags)
}

// ---------------------------------------------------------------------------------------
// Mints
// ---------------------------------------------------------------------------------------

/// Token-2022 extension type numbers (`ExtensionType`) this module decodes.
pub mod extension_type {
    pub const TRANSFER_FEE_CONFIG: u16 = 1;
    pub const MINT_CLOSE_AUTHORITY: u16 = 3;
    pub const CONFIDENTIAL_TRANSFER_MINT: u16 = 4;
    pub const DEFAULT_ACCOUNT_STATE: u16 = 6;
    pub const NON_TRANSFERABLE: u16 = 9;
    pub const INTEREST_BEARING_CONFIG: u16 = 10;
    pub const PERMANENT_DELEGATE: u16 = 12;
    pub const TRANSFER_HOOK: u16 = 14;
    pub const CONFIDENTIAL_TRANSFER_FEE_CONFIG: u16 = 16;
    pub const METADATA_POINTER: u16 = 18;
    pub const TOKEN_METADATA: u16 = 19;
    pub const GROUP_POINTER: u16 = 20;
    pub const TOKEN_GROUP: u16 = 21;
    pub const GROUP_MEMBER_POINTER: u16 = 22;
    pub const TOKEN_GROUP_MEMBER: u16 = 23;
    pub const CONFIDENTIAL_MINT_BURN: u16 = 24;
    pub const SCALED_UI_AMOUNT: u16 = 25;
    pub const PAUSABLE: u16 = 26;
    pub const PERMISSIONED_BURN: u16 = 28;
}

use extension_type as ext;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransferFee {
    pub epoch: u64,
    pub maximum_fee: u64,
    pub basis_points: u16,
}

impl TransferFee {
    fn read(data: &[u8], offset: usize) -> Option<Self> {
        Some(TransferFee {
            epoch: read_u64(data, offset)?,
            maximum_fee: read_u64(data, offset + 8)?,
            basis_points: read_u16(data, offset + 16)?,
        })
    }

    fn to_json(self) -> Value {
        json!({
            "epoch": self.epoch.to_string(),
            "maximum_fee": self.maximum_fee.to_string(),
            "basis_points": self.basis_points,
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TransferFeeConfig {
    pub authority: Option<Pubkey>,
    pub withdraw_withheld_authority: Option<Pubkey>,
    pub withheld_amount: u64,
    pub older: TransferFee,
    pub newer: TransferFee,
    /// The fee in force at the inspected epoch.
    pub current: TransferFee,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TokenMetadata {
    pub update_authority: Option<Pubkey>,
    pub mint: Pubkey,
    pub name: String,
    pub symbol: String,
    pub uri: String,
    pub additional_metadata: Vec<(String, String)>,
}

/// Decoded Token-2022 mint extensions. `None` means the extension is absent.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MintExtensions {
    pub transfer_fee: Option<TransferFeeConfig>,
    pub mint_close_authority: Option<Option<Pubkey>>,
    /// (authority, auto-approve new accounts).
    pub confidential_transfer: Option<(Option<Pubkey>, bool)>,
    pub confidential_transfer_fee_authority: Option<Option<Pubkey>>,
    pub confidential_mint_burn: bool,
    /// 0 uninitialized, 1 initialized, 2 frozen.
    pub default_account_state: Option<u8>,
    pub non_transferable: bool,
    /// (rate authority, initialization timestamp, pre-update average rate, last update
    /// timestamp, current rate), rates in basis points.
    pub interest_bearing: Option<(Option<Pubkey>, i64, i16, i64, i16)>,
    pub permanent_delegate: Option<Option<Pubkey>>,
    /// (authority, hook program).
    pub transfer_hook: Option<(Option<Pubkey>, Option<Pubkey>)>,
    /// (authority, metadata address).
    pub metadata_pointer: Option<(Option<Pubkey>, Option<Pubkey>)>,
    pub token_metadata: Option<TokenMetadata>,
    /// (authority, group address).
    pub group_pointer: Option<(Option<Pubkey>, Option<Pubkey>)>,
    /// (update authority, mint, size, max size).
    pub token_group: Option<(Option<Pubkey>, Pubkey, u64, u64)>,
    /// (authority, member address).
    pub group_member_pointer: Option<(Option<Pubkey>, Option<Pubkey>)>,
    /// (mint, group, member number).
    pub token_group_member: Option<(Pubkey, Pubkey, u64)>,
    /// (authority, multiplier, new multiplier effective timestamp, new multiplier).
    pub scaled_ui_amount: Option<(Option<Pubkey>, f64, i64, f64)>,
    /// (authority, paused).
    pub pausable: Option<(Option<Pubkey>, bool)>,
    pub permissioned_burn_authority: Option<Option<Pubkey>>,
}

/// Supply, authorities and decoded Token-2022 extensions of a mint, as the SDK's
/// `MintInspection`.
#[derive(Clone, Debug, PartialEq)]
pub struct MintFacts {
    pub token_program: Pubkey,
    pub decimals: u8,
    pub supply: u64,
    pub mint_authority: Option<Pubkey>,
    pub freeze_authority: Option<Pubkey>,
    pub extensions: MintExtensions,
    /// Every extension type present, in account order.
    pub extension_types: Vec<u16>,
    /// Extension types this module does not decode, or could not decode.
    pub unknown_extensions: Vec<u16>,
    pub flags: Vec<&'static str>,
}

const MINT_LEN: usize = 82;
/// Token-2022 pads mints with extensions to the token account length before the account
/// type byte.
const ACCOUNT_TYPE_OFFSET: usize = 165;
const ACCOUNT_TYPE_MINT: u8 = 1;

/// The TLV entries after the account type byte. Stops at an uninitialized (type 0) entry
/// or a truncated one, as the SDK does.
fn tlv_entries(data: &[u8]) -> Vec<(u16, &[u8])> {
    let mut entries = vec![];
    if data.len() <= ACCOUNT_TYPE_OFFSET {
        return entries;
    }
    let mut offset = ACCOUNT_TYPE_OFFSET + 1;
    while offset + 4 <= data.len() {
        let kind = read_u16(data, offset).unwrap_or(0);
        let length = read_u16(data, offset + 2).unwrap_or(0) as usize;
        if kind == 0 {
            break;
        }
        let Some(value) = data.get(offset + 4..offset + 4 + length) else {
            break;
        };
        entries.push((kind, value));
        offset += 4 + length;
    }
    entries
}

/// Whether a mint account has a transfer fee extension, so a caller knows it needs the
/// current epoch to decode it.
pub fn mint_has_transfer_fee(owner: &Pubkey, data: &[u8]) -> bool {
    *owner == TOKEN_2022_PROGRAM_ID
        && tlv_entries(data)
            .iter()
            .any(|(kind, _)| *kind == ext::TRANSFER_FEE_CONFIG)
}

/// Decodes mint account bytes owned by `token_program`. `epoch` picks the transfer fee in
/// force. `None` if the bytes are not an initialized SPL Token or Token-2022 mint.
pub fn decode_mint(token_program: &Pubkey, data: &[u8], epoch: u64) -> Option<MintFacts> {
    if *token_program != TOKEN_PROGRAM_ID && *token_program != TOKEN_2022_PROGRAM_ID {
        return None;
    }
    if data.len() < MINT_LEN || data[45] != 1 {
        return None;
    }
    if data.len() > MINT_LEN
        && (*token_program != TOKEN_2022_PROGRAM_ID
            || data.get(ACCOUNT_TYPE_OFFSET) != Some(&ACCOUNT_TYPE_MINT))
    {
        return None;
    }
    let mut facts = MintFacts {
        token_program: *token_program,
        mint_authority: c_option_pubkey(data, 0),
        supply: read_u64(data, 36)?,
        decimals: data[44],
        freeze_authority: c_option_pubkey(data, 46),
        extensions: MintExtensions::default(),
        extension_types: vec![],
        unknown_extensions: vec![],
        flags: vec![],
    };
    for (kind, value) in tlv_entries(data) {
        facts.extension_types.push(kind);
        if decode_extension(&mut facts.extensions, kind, value, epoch).is_none() {
            facts.unknown_extensions.push(kind);
        }
    }
    facts.flags = mint_flags(&facts);
    Some(facts)
}

/// A u32-length-prefixed string; invalid UTF-8 is replaced, as `TextDecoder` does.
fn read_string(value: &[u8], offset: &mut usize) -> Option<String> {
    let length = read_u32(value, *offset)? as usize;
    let start = *offset + 4;
    let bytes = value.get(start..start.checked_add(length)?)?;
    *offset = start + length;
    Some(String::from_utf8_lossy(bytes).into_owned())
}

fn decode_token_metadata(value: &[u8]) -> Option<TokenMetadata> {
    let mut offset = 64;
    let name = read_string(value, &mut offset)?;
    let symbol = read_string(value, &mut offset)?;
    let uri = read_string(value, &mut offset)?;
    let count = read_u32(value, offset)?;
    offset += 4;
    let mut additional_metadata = vec![];
    for _ in 0..count {
        let key = read_string(value, &mut offset)?;
        let entry = read_string(value, &mut offset)?;
        additional_metadata.push((key, entry));
    }
    Some(TokenMetadata {
        update_authority: optional_pubkey(value, 0),
        mint: read_pubkey(value, 32)?,
        name,
        symbol,
        uri,
        additional_metadata,
    })
}

/// Decodes one extension into `out`. `None` for unknown or malformed ones. Each extension
/// must be at least as long as the fields read, as in the SDK; longer is accepted.
fn decode_extension(out: &mut MintExtensions, kind: u16, value: &[u8], epoch: u64) -> Option<()> {
    let need = |length: usize| (value.len() >= length).then_some(());
    match kind {
        ext::TRANSFER_FEE_CONFIG => {
            need(108)?;
            let older = TransferFee::read(value, 72)?;
            let newer = TransferFee::read(value, 90)?;
            out.transfer_fee = Some(TransferFeeConfig {
                authority: optional_pubkey(value, 0),
                withdraw_withheld_authority: optional_pubkey(value, 32),
                withheld_amount: read_u64(value, 64)?,
                older,
                newer,
                current: if epoch >= newer.epoch { newer } else { older },
            });
        }
        ext::MINT_CLOSE_AUTHORITY => {
            need(32)?;
            out.mint_close_authority = Some(optional_pubkey(value, 0));
        }
        ext::CONFIDENTIAL_TRANSFER_MINT => {
            need(33)?;
            out.confidential_transfer = Some((optional_pubkey(value, 0), value[32] == 1));
        }
        ext::CONFIDENTIAL_TRANSFER_FEE_CONFIG => {
            need(32)?;
            out.confidential_transfer_fee_authority = Some(optional_pubkey(value, 0));
        }
        ext::CONFIDENTIAL_MINT_BURN => out.confidential_mint_burn = true,
        ext::DEFAULT_ACCOUNT_STATE => {
            need(1)?;
            if value[0] > 2 {
                return None;
            }
            out.default_account_state = Some(value[0]);
        }
        ext::NON_TRANSFERABLE => out.non_transferable = true,
        ext::INTEREST_BEARING_CONFIG => {
            need(52)?;
            out.interest_bearing = Some((
                optional_pubkey(value, 0),
                read_i64(value, 32)?,
                read_i16(value, 40)?,
                read_i64(value, 42)?,
                read_i16(value, 50)?,
            ));
        }
        ext::PERMANENT_DELEGATE => {
            need(32)?;
            out.permanent_delegate = Some(optional_pubkey(value, 0));
        }
        ext::TRANSFER_HOOK => {
            need(64)?;
            out.transfer_hook = Some((optional_pubkey(value, 0), optional_pubkey(value, 32)));
        }
        ext::METADATA_POINTER => {
            need(64)?;
            out.metadata_pointer = Some((optional_pubkey(value, 0), optional_pubkey(value, 32)));
        }
        ext::TOKEN_METADATA => {
            need(64)?;
            out.token_metadata = Some(decode_token_metadata(value)?);
        }
        ext::GROUP_POINTER => {
            need(64)?;
            out.group_pointer = Some((optional_pubkey(value, 0), optional_pubkey(value, 32)));
        }
        ext::TOKEN_GROUP => {
            need(80)?;
            out.token_group = Some((
                optional_pubkey(value, 0),
                read_pubkey(value, 32)?,
                read_u64(value, 64)?,
                read_u64(value, 72)?,
            ));
        }
        ext::GROUP_MEMBER_POINTER => {
            need(64)?;
            out.group_member_pointer =
                Some((optional_pubkey(value, 0), optional_pubkey(value, 32)));
        }
        ext::TOKEN_GROUP_MEMBER => {
            need(72)?;
            out.token_group_member = Some((
                read_pubkey(value, 0)?,
                read_pubkey(value, 32)?,
                read_u64(value, 64)?,
            ));
        }
        ext::SCALED_UI_AMOUNT => {
            need(56)?;
            out.scaled_ui_amount = Some((
                optional_pubkey(value, 0),
                read_f64(value, 32)?,
                read_i64(value, 40)?,
                read_f64(value, 48)?,
            ));
        }
        ext::PAUSABLE => {
            need(33)?;
            out.pausable = Some((optional_pubkey(value, 0), value[32] == 1));
        }
        ext::PERMISSIONED_BURN => {
            need(32)?;
            out.permissioned_burn_authority = Some(optional_pubkey(value, 0));
        }
        _ => return None,
    }
    Some(())
}

fn mint_flags(mint: &MintFacts) -> Vec<&'static str> {
    let e = &mint.extensions;
    let mut flags = vec![];
    if mint.token_program == TOKEN_2022_PROGRAM_ID {
        flags.push("token-2022");
    }
    if mint.mint_authority.is_some() {
        flags.push("mint-authority");
    }
    if mint.freeze_authority.is_some() {
        flags.push("freeze-authority");
    }
    if let Some((authority, program)) = &e.transfer_hook {
        if program.is_some() {
            flags.push("transfer-hook");
        }
        if authority.is_some() {
            flags.push("transfer-hook-authority");
        }
    }
    if let Some(fee) = &e.transfer_fee {
        // A fee in force now, or a non-zero fee scheduled for a later epoch. A fee that
        // was lowered to zero and is now in force doesn't count.
        let scheduled = fee.newer.epoch > fee.current.epoch;
        if fee.current.basis_points > 0 || (scheduled && fee.newer.basis_points > 0) {
            flags.push("transfer-fee");
        }
        if fee.authority.is_some() {
            flags.push("transfer-fee-authority");
        }
    }
    if matches!(e.permanent_delegate, Some(Some(_))) {
        flags.push("permanent-delegate");
    }
    if e.non_transferable {
        flags.push("non-transferable");
    }
    if e.default_account_state == Some(2) {
        flags.push("default-frozen");
    }
    if let Some((authority, paused)) = &e.pausable {
        if authority.is_some() || *paused {
            flags.push("pausable");
        }
        if *paused {
            flags.push("paused");
        }
    }
    if matches!(e.mint_close_authority, Some(Some(_))) {
        flags.push("closable-mint");
    }
    if e.interest_bearing.is_some() {
        flags.push("interest-bearing");
    }
    if e.scaled_ui_amount.is_some() {
        flags.push("scaled-ui-amount");
    }
    if e.confidential_transfer.is_some() || e.confidential_mint_burn {
        flags.push("confidential-transfers");
    }
    if e.permissioned_burn_authority.is_some() {
        flags.push("permissioned-burn");
    }
    if !mint.unknown_extensions.is_empty() {
        flags.push("unknown-extension");
    }
    ordered_flags(flags)
}

fn pointer_json(pair: &(Option<Pubkey>, Option<Pubkey>), address: &str) -> Value {
    json!({ "authority": opt_key(&pair.0), address: opt_key(&pair.1) })
}

impl MintExtensions {
    /// The decoded extensions as snake_case JSON, keyed by extension, with u64 and i64
    /// values as decimal strings. Absent extensions are left out.
    pub fn to_json(&self) -> Value {
        let mut out = serde_json::Map::new();
        if let Some(fee) = &self.transfer_fee {
            out.insert(
                "transfer_fee".into(),
                json!({
                    "authority": opt_key(&fee.authority),
                    "withdraw_withheld_authority": opt_key(&fee.withdraw_withheld_authority),
                    "withheld_amount": fee.withheld_amount.to_string(),
                    "older": fee.older.to_json(),
                    "newer": fee.newer.to_json(),
                    "current": fee.current.to_json(),
                }),
            );
        }
        if let Some(authority) = &self.mint_close_authority {
            out.insert(
                "mint_close_authority".into(),
                json!({ "close_authority": opt_key(authority) }),
            );
        }
        if let Some((authority, auto_approve)) = &self.confidential_transfer {
            out.insert(
                "confidential_transfer".into(),
                json!({ "authority": opt_key(authority), "auto_approve_new_accounts": auto_approve }),
            );
        }
        if let Some(authority) = &self.confidential_transfer_fee_authority {
            out.insert(
                "confidential_transfer_fee".into(),
                json!({ "authority": opt_key(authority) }),
            );
        }
        if self.confidential_mint_burn {
            out.insert("confidential_mint_burn".into(), json!(true));
        }
        if let Some(state) = self.default_account_state {
            let state = ["uninitialized", "initialized", "frozen"][state as usize];
            out.insert("default_account_state".into(), json!({ "state": state }));
        }
        if self.non_transferable {
            out.insert("non_transferable".into(), json!(true));
        }
        if let Some((authority, init, pre_rate, last, rate)) = &self.interest_bearing {
            out.insert(
                "interest_bearing".into(),
                json!({
                    "rate_authority": opt_key(authority),
                    "initialization_timestamp": init.to_string(),
                    "pre_update_average_rate_bps": pre_rate,
                    "last_update_timestamp": last.to_string(),
                    "current_rate_bps": rate,
                }),
            );
        }
        if let Some(delegate) = &self.permanent_delegate {
            out.insert(
                "permanent_delegate".into(),
                json!({ "delegate": opt_key(delegate) }),
            );
        }
        if let Some(hook) = &self.transfer_hook {
            out.insert("transfer_hook".into(), pointer_json(hook, "program_id"));
        }
        if let Some(pointer) = &self.metadata_pointer {
            out.insert(
                "metadata_pointer".into(),
                pointer_json(pointer, "metadata_address"),
            );
        }
        if let Some(metadata) = &self.token_metadata {
            out.insert(
                "token_metadata".into(),
                json!({
                    "update_authority": opt_key(&metadata.update_authority),
                    "mint": metadata.mint.to_string(),
                    "name": metadata.name,
                    "symbol": metadata.symbol,
                    "uri": metadata.uri,
                    "additional_metadata": metadata.additional_metadata,
                }),
            );
        }
        if let Some(pointer) = &self.group_pointer {
            out.insert(
                "group_pointer".into(),
                pointer_json(pointer, "group_address"),
            );
        }
        if let Some((authority, mint, size, max_size)) = &self.token_group {
            out.insert(
                "token_group".into(),
                json!({
                    "update_authority": opt_key(authority),
                    "mint": mint.to_string(),
                    "size": size.to_string(),
                    "max_size": max_size.to_string(),
                }),
            );
        }
        if let Some(pointer) = &self.group_member_pointer {
            out.insert(
                "group_member_pointer".into(),
                pointer_json(pointer, "member_address"),
            );
        }
        if let Some((mint, group, number)) = &self.token_group_member {
            out.insert(
                "token_group_member".into(),
                json!({
                    "mint": mint.to_string(),
                    "group": group.to_string(),
                    "member_number": number.to_string(),
                }),
            );
        }
        if let Some((authority, multiplier, at, new_multiplier)) = &self.scaled_ui_amount {
            out.insert(
                "scaled_ui_amount".into(),
                json!({
                    "authority": opt_key(authority),
                    "multiplier": finite(*multiplier),
                    "new_multiplier_effective_timestamp": at.to_string(),
                    "new_multiplier": finite(*new_multiplier),
                }),
            );
        }
        if let Some((authority, paused)) = &self.pausable {
            out.insert(
                "pausable".into(),
                json!({ "authority": opt_key(authority), "paused": paused }),
            );
        }
        if let Some(authority) = &self.permissioned_burn_authority {
            out.insert(
                "permissioned_burn".into(),
                json!({ "authority": opt_key(authority) }),
            );
        }
        Value::Object(out)
    }
}

/// JSON has no NaN or infinity; those become `null`.
fn finite(value: f64) -> Value {
    serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
}

impl MintFacts {
    pub fn to_json(&self) -> Value {
        json!({
            "token_program": self.token_program.to_string(),
            "decimals": self.decimals,
            "supply": self.supply.to_string(),
            "mint_authority": opt_key(&self.mint_authority),
            "freeze_authority": opt_key(&self.freeze_authority),
            "extensions": self.extensions.to_json(),
            "extension_types": self.extension_types,
            "unknown_extensions": self.unknown_extensions,
            "flags": self.flags,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8) -> Pubkey {
        Pubkey::new_from_array([n; 32])
    }

    /// A Token-2022 mint with the given TLV entries.
    fn mint_2022(entries: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut data = vec![0u8; ACCOUNT_TYPE_OFFSET + 1];
        data[45] = 1;
        data[44] = 6;
        data[ACCOUNT_TYPE_OFFSET] = ACCOUNT_TYPE_MINT;
        for (kind, value) in entries {
            data.extend_from_slice(&kind.to_le_bytes());
            data.extend_from_slice(&(value.len() as u16).to_le_bytes());
            data.extend_from_slice(value);
        }
        data
    }

    fn transfer_fee(older: (u64, u16), newer: (u64, u16), authority: bool) -> Vec<u8> {
        let mut value = vec![0u8; 108];
        if authority {
            value[..32].copy_from_slice(&[9; 32]);
        }
        for (offset, (epoch, bps)) in [(72, older), (90, newer)] {
            value[offset..offset + 8].copy_from_slice(&epoch.to_le_bytes());
            value[offset + 8..offset + 16].copy_from_slice(&1000u64.to_le_bytes());
            value[offset + 16..offset + 18].copy_from_slice(&bps.to_le_bytes());
        }
        value
    }

    fn flags_at(data: &[u8], epoch: u64) -> Vec<&'static str> {
        decode_mint(&TOKEN_2022_PROGRAM_ID, data, epoch)
            .unwrap()
            .flags
    }

    #[test]
    fn flag_table_has_unique_names() {
        let mut names: Vec<&str> = FLAGS.iter().map(|(n, _)| *n).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), FLAGS.len());
        assert_eq!(flag("paused"), Some("paused"));
        assert_eq!(flag("nope"), None);
    }

    #[test]
    fn transfer_fee_counts_only_when_in_force_or_scheduled() {
        // In force now.
        let data = mint_2022(&[(1, transfer_fee((0, 0), (5, 100), false))]);
        assert_eq!(flags_at(&data, 5), ["token-2022", "transfer-fee"]);
        // Scheduled from epoch 5.
        assert_eq!(flags_at(&data, 4), ["token-2022", "transfer-fee"]);
        // Lowered to zero and now in force.
        let data = mint_2022(&[(1, transfer_fee((0, 100), (5, 0), true))]);
        assert_eq!(flags_at(&data, 5), ["token-2022", "transfer-fee-authority"]);
        assert_eq!(
            flags_at(&data, 4),
            ["token-2022", "transfer-fee", "transfer-fee-authority"]
        );
    }

    #[test]
    fn malformed_and_unknown_extensions_are_flagged() {
        let data = mint_2022(&[(12, vec![1; 31]), (200, vec![])]);
        let facts = decode_mint(&TOKEN_2022_PROGRAM_ID, &data, 0).unwrap();
        assert_eq!(facts.extension_types, [12, 200]);
        assert_eq!(facts.unknown_extensions, [12, 200]);
        assert_eq!(facts.flags, ["token-2022", "unknown-extension"]);
    }

    #[test]
    fn rejects_non_mints() {
        let mut data = vec![0u8; 82];
        assert!(decode_mint(&TOKEN_PROGRAM_ID, &data, 0).is_none());
        data[45] = 1;
        assert!(decode_mint(&TOKEN_PROGRAM_ID, &data, 0).is_some());
        assert!(decode_mint(&key(1), &data, 0).is_none());
        // SPL Token mints have no extensions.
        let long = mint_2022(&[]);
        assert!(decode_mint(&TOKEN_PROGRAM_ID, &long, 0).is_none());
        assert!(decode_mint(&TOKEN_2022_PROGRAM_ID, &long, 0).is_some());
    }

    #[test]
    fn upgradeable_program_facts_and_hash() {
        let program_data = key(5);
        let mut program = vec![2, 0, 0, 0];
        program.extend_from_slice(program_data.as_ref());
        let mut data = vec![3, 0, 0, 0];
        data.extend_from_slice(&77u64.to_le_bytes());
        data.push(1);
        data.extend_from_slice(&[8; 32]);
        data.extend_from_slice(b"\x7fELF code");
        data.extend_from_slice(&[0; 100]);
        let program_view = AccountView {
            owner: &BPF_LOADER_UPGRADEABLE_ID,
            executable: true,
            data: &program,
        };
        let data_view = AccountView {
            owner: &BPF_LOADER_UPGRADEABLE_ID,
            executable: false,
            data: &data,
        };
        let facts = inspect_program(Some(&program_view), Some(&data_view), true);
        assert_eq!(facts.program_data_address, Some(program_data));
        assert_eq!(facts.last_deploy_slot, Some(77));
        assert_eq!(facts.upgrade_authority, Some(key(8)));
        assert_eq!(facts.flags, ["upgradeable"]);
        assert_eq!(
            facts.executable_sha256.as_deref(),
            Some(hex(&Sha256::digest(b"\x7fELF code")).as_str())
        );

        data[12] = 0;
        let data_view = AccountView {
            owner: &BPF_LOADER_UPGRADEABLE_ID,
            executable: false,
            data: &data,
        };
        let facts = inspect_program(Some(&program_view), Some(&data_view), false);
        assert_eq!(facts.flags, ["frozen"]);
        assert_eq!(facts.executable_sha256, None);

        assert_eq!(inspect_program(None, None, true).flags, ["program-missing"]);
        let facts = inspect_program(Some(&program_view), None, true);
        assert_eq!(facts.flags, ["program-missing"]);
    }

    #[test]
    fn build_flags_need_a_matching_hash() {
        let mut program = ProgramFacts {
            loader: ProgramLoader::Upgradeable,
            loader_address: Some(BPF_LOADER_UPGRADEABLE_ID),
            deployed: true,
            upgradeable: false,
            upgrade_authority: None,
            program_data_address: None,
            last_deploy_slot: Some(1),
            loader_v4_status: None,
            executable_sha256: Some("ab".into()),
            flags: vec!["frozen"],
        };
        let verified = VerifiedBuild::from_response(&json!({
            "is_verified": true, "on_chain_hash": "ab", "commit": "None", "repo_url": ""
        }));
        assert_eq!(verified.commit, None);
        assert_eq!(verified.repository, None);
        let flags = |program: &ProgramFacts, build: &VerifiedBuild| {
            strategy_flags(
                program,
                true,
                MetadataStatus::Valid,
                Some(ConformanceStatus::Current),
                Some(build),
            )
        };
        assert_eq!(
            flags(&program, &verified),
            ["frozen", "verified-build", "conformance-report"]
        );
        program.executable_sha256 = Some("cd".into());
        assert_eq!(
            flags(&program, &verified),
            ["frozen", "unverified-build", "conformance-report"]
        );
        program.executable_sha256 = None;
        assert_eq!(flags(&program, &verified), ["frozen", "conformance-report"]);
        let unknown = VerifiedBuild::from_response(&json!({ "error": "x" }));
        assert_eq!(unknown.status, VerifiedBuildStatus::Unknown);
        assert_eq!(
            strategy_flags(
                &program,
                true,
                MetadataStatus::Missing,
                None,
                Some(&unknown)
            ),
            ["frozen", "metadata-missing"]
        );
        assert_eq!(
            strategy_flags(&program, false, MetadataStatus::Missing, None, None),
            ["frozen", "unregistered"]
        );
    }

    #[test]
    fn conformance_reports_must_match_the_deployed_code() {
        let program = key(1);
        let report = |id: &str, hash: &str| {
            json!({
                "schema": CHECK_REPORT_SCHEMA,
                "generated_at": 7,
                "strategy": { "program_id": id, "executable_sha256": hash },
                "summary": { "result": "warn" },
            })
            .to_string()
            .into_bytes()
        };
        let body = report(&program.to_string(), "AB");
        let current = evaluate_conformance_report(&body, None, &program, Some("ab"));
        assert_eq!(
            (current.status, current.reason),
            (ConformanceStatus::Current, None)
        );
        assert_eq!(current.result.as_deref(), Some("warn"));
        let pinned = current.report_sha256.clone().unwrap().to_uppercase();
        let check = |body: &[u8], pinned: Option<&str>, deployed: Option<&str>| {
            let facts = evaluate_conformance_report(body, pinned, &program, deployed);
            (facts.status, facts.reason)
        };
        assert_eq!(
            check(&body, Some(&pinned), Some("ab")).0,
            ConformanceStatus::Current
        );
        assert_eq!(
            check(&body, Some(&"0".repeat(64)), Some("ab")),
            (ConformanceStatus::Unverified, Some("sha256-mismatch"))
        );
        assert_eq!(
            check(&body, None, Some("cd")),
            (ConformanceStatus::Stale, Some("other-code"))
        );
        assert_eq!(
            check(&body, None, None),
            (ConformanceStatus::Unverified, Some("code-not-hashed"))
        );
        assert_eq!(
            check(&report(&key(2).to_string(), "ab"), None, Some("ab")),
            (ConformanceStatus::Stale, Some("other-program"))
        );
        assert_eq!(
            check(b"<html>", None, Some("ab")),
            (ConformanceStatus::Unverified, Some("not-json"))
        );
        assert_eq!(
            check(b"{\"schema\":\"x\"}", None, Some("ab")),
            (ConformanceStatus::Unverified, Some("not-a-report"))
        );
        // After an upgrade the same report is stale; reading it back changes nothing.
        let mut upgraded = current.clone();
        upgraded.recheck(&program, Some("cd"));
        assert_eq!(upgraded.status, ConformanceStatus::Stale);
        assert_eq!(
            ConformanceFacts::from_json(&upgraded.to_json()),
            Some(upgraded)
        );
        let mut unreadable = evaluate_conformance_report(b"x", None, &program, Some("ab"));
        unreadable.recheck(&program, Some("ab"));
        assert_eq!(unreadable.reason, Some("not-json"));
        let flags = |status| {
            strategy_flags(
                &inspect_program(None, None, false),
                true,
                MetadataStatus::Valid,
                Some(status),
                None,
            )
        };
        assert_eq!(
            flags(ConformanceStatus::Current),
            ["program-missing", "conformance-report"]
        );
        assert_eq!(
            flags(ConformanceStatus::Stale),
            ["program-missing", "conformance-stale"]
        );
        assert_eq!(flags(ConformanceStatus::Unverified), ["program-missing"]);
    }

    #[test]
    fn filter_requires_and_excludes() {
        let flags = ["frozen", "token-2022"];
        assert!(passes_filter(&flags, &["frozen"], &["paused"]));
        assert!(!passes_filter(&flags, &["upgradeable"], &[]));
        assert!(!passes_filter(&flags, &[], &["token-2022"]));
    }
}
