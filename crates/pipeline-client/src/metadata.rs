//! Strategy metadata, version 1: validation of the JSON document a strategy's on-chain
//! `metadata_uri` points to, and resolution of `ipfs://` and `ar://` URIs.
//!
//! This is a port of `parseStrategyMetadata` and `resolveMetadataUri` in the TypeScript
//! SDK (`sdk/ts/src/metadata.ts`) and gives the same errors and warnings for the same
//! document; `tests/fixtures/trust-facts.json` holds shared vectors. The JSON Schema is
//! `sdk/ts/schema/strategy-metadata.v1.json`. Fetching is left to the caller.
//!
//! String lengths are counted in UTF-16 code units, as JavaScript's `length` does.

use std::collections::HashSet;

use serde_json::{Map, Value};
use solana_pubkey::Pubkey;
use url::Url;

/// The `schema` value of a version 1 document.
pub const STRATEGY_METADATA_SCHEMA: &str = "pipeline-strategy-metadata/1";

pub const STRATEGY_CATEGORIES: &[&str] =
    &["amm", "launchpad", "order-book", "auction", "lbp", "other"];
pub const ACTION_KINDS: &[&str] = &[
    "add_liquidity",
    "remove_liquidity",
    "claim",
    "create_market",
    "other",
];
/// Argument types, Borsh-encoded in order after the discriminator.
pub const ARG_TYPES: &[&str] = &[
    "u8", "u16", "u32", "u64", "i64", "bool", "pubkey", "bytes", "string",
];
/// URL schemes accepted for links, images and reports.
pub const METADATA_URL_SCHEMES: &[&str] = &["https", "ipfs", "ar"];

/// The sites each social link is expected on. A link elsewhere is accepted with a warning.
pub const SOCIAL_LINK_HOSTS: &[(&str, &[&str])] = &[
    ("twitter", &["x.com", "twitter.com"]),
    ("github", &["github.com"]),
    ("discord", &["discord.gg", "discord.com"]),
    ("telegram", &["t.me", "telegram.me"]),
    ("farcaster", &["farcaster.xyz", "warpcast.com"]),
];

pub const DEFAULT_IPFS_GATEWAY: &str = "https://ipfs.io/ipfs/";
pub const DEFAULT_ARWEAVE_GATEWAY: &str = "https://arweave.net/";
pub const DEFAULT_METADATA_MAX_BYTES: usize = 64 * 1024;

/// A valid document's warnings (values a newer minor revision may add, such as an unknown
/// category), or an invalid document's errors.
pub type ParseResult = Result<Vec<String>, Vec<String>>;

/// The HTTPS URL to fetch for a metadata URI: `https://` as is, `ipfs://<cid>/<path>` and
/// `ar://<id>/<path>` through the gateways (each ending with `/`). `None` for anything
/// else, including `http://`.
pub fn resolve_metadata_uri(
    uri: &str,
    ipfs_gateway: &str,
    arweave_gateway: &str,
) -> Option<String> {
    let trimmed = uri.trim();
    if let Some(path) = trimmed.strip_prefix("ipfs://") {
        let path = path.strip_prefix("ipfs/").unwrap_or(path);
        return (!path.is_empty()).then(|| format!("{ipfs_gateway}{path}"));
    }
    if let Some(path) = trimmed.strip_prefix("ar://") {
        return (!path.is_empty()).then(|| format!("{arweave_gateway}{path}"));
    }
    let parsed = Url::parse(trimmed).ok()?;
    (parsed.scheme() == "https").then(|| trimmed.to_string())
}

/// Validates a parsed JSON value as strategy metadata version 1. Unknown keys are allowed.
pub fn parse_strategy_metadata(json: &Value) -> ParseResult {
    let mut c = Checker::default();
    let Some(doc) = c.object(Some(json), "$") else {
        return Err(c.errors);
    };
    if doc.get("schema").and_then(Value::as_str) != Some(STRATEGY_METADATA_SCHEMA) {
        c.error("schema", format!("must be \"{STRATEGY_METADATA_SCHEMA}\""));
    }
    c.string(doc.get("name"), "name", 64, true);
    c.string(doc.get("description"), "description", 2000, false);
    c.url(doc.get("image"), "image", false);
    match doc.get("category") {
        None => {}
        Some(Value::String(category)) => {
            if !STRATEGY_CATEGORIES.contains(&category.as_str()) {
                c.warn(format!("category: unknown category \"{category}\""));
            }
        }
        Some(_) => c.error("category", "must be a string"),
    }
    for (i, tag) in c.array(doc.get("tags"), "tags", 16).iter().enumerate() {
        c.string(Some(tag), &format!("tags[{i}]"), 32, true);
    }
    if let Some(links) = doc.get("links") {
        if let Some(links) = c.object(Some(links), "links") {
            for (key, value) in links {
                let path = format!("links.{key}");
                c.url(Some(value), &path, false);
                let hosts = SOCIAL_LINK_HOSTS
                    .iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, hosts)| *hosts);
                if let (Some(hosts), Some(text)) = (hosts, value.as_str()) {
                    if !on_host(text, hosts) {
                        c.warn(format!("{path}: expected a link on {}", hosts.join(" or ")));
                    }
                }
            }
        }
    }
    if let Some(source) = doc.get("source") {
        if let Some(source) = c.object(Some(source), "source") {
            c.url(source.get("repository"), "source.repository", true);
            if let Some(commit) = source.get("commit") {
                let ok = commit
                    .as_str()
                    .is_some_and(|s| (7..=64).contains(&s.len()) && is_hex(s));
                if !ok {
                    c.error(
                        "source.commit",
                        "must be a commit hash (7 to 64 hex digits)",
                    );
                }
            }
            c.string(source.get("path"), "source.path", 256, false);
        }
    }
    if let Some(init) = doc.get("init") {
        check_init(&mut c, init);
    }
    check_actions(&mut c, doc.get("actions"));
    if let Some(conformance) = doc.get("conformance") {
        if let Some(conformance) = c.object(Some(conformance), "conformance") {
            c.url(conformance.get("report"), "conformance.report", true);
            if let Some(sha) = conformance.get("sha256") {
                if !sha.as_str().is_some_and(|s| s.len() == 64 && is_hex(s)) {
                    c.error("conformance.sha256", "must be 64 hex digits");
                }
            }
        }
    }
    if let Some(security) = doc.get("security") {
        if let Some(security) = c.object(Some(security), "security") {
            c.string(security.get("contact"), "security.contact", 256, true);
            c.url(security.get("policy"), "security.policy", false);
            let audits = c.array(security.get("audits"), "security.audits", 32);
            for (i, audit) in audits.iter().enumerate() {
                let p = format!("security.audits[{i}]");
                let Some(audit) = c.object(Some(audit), &p) else {
                    continue;
                };
                c.string(audit.get("by"), &format!("{p}.by"), 128, true);
                c.url(audit.get("url"), &format!("{p}.url"), false);
                if let Some(date) = audit.get("date") {
                    if !date.as_str().is_some_and(is_date) {
                        c.error(&format!("{p}.date"), "must be a date as YYYY-MM-DD");
                    }
                }
            }
        }
    }
    if c.errors.is_empty() {
        Ok(c.warnings)
    } else {
        Err(c.errors)
    }
}

/// `YYYY-MM-DD` that JavaScript's `Date.parse` accepts: any month 01 to 12 and any day
/// 01 to 31 (it rolls 2026-02-30 over into March rather than rejecting it).
fn is_date(text: &str) -> bool {
    let bytes = text.as_bytes();
    let digits = |range: std::ops::Range<usize>| bytes[range].iter().all(u8::is_ascii_digit);
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    if !digits(0..4) || !digits(5..7) || !digits(8..10) {
        return false;
    }
    let month: u32 = text[5..7].parse().unwrap_or(0);
    let day: u32 = text[8..10].parse().unwrap_or(0);
    (1..=12).contains(&month) && (1..=31).contains(&day)
}

fn is_hex(text: &str) -> bool {
    text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `^[A-Za-z_][A-Za-z0-9_]{0,63}$`.
fn is_name(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && text.len() <= 64
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// A base58 address, as `isAddress` in `@solana/kit` checks it.
fn is_address(text: &str) -> bool {
    (32..=44).contains(&text.len()) && text.parse::<Pubkey>().is_ok()
}

/// Whether `url` is an https link on one of `hosts` or a subdomain of one.
fn on_host(url: &str, hosts: &[&str]) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
    parsed.scheme() == "https"
        && hosts
            .iter()
            .any(|h| host == *h || host.ends_with(&format!(".{h}")))
}

fn js_length(text: &str) -> usize {
    text.encode_utf16().count()
}

#[derive(Default)]
struct Checker {
    errors: Vec<String>,
    warnings: Vec<String>,
}

impl Checker {
    fn error(&mut self, path: &str, message: impl AsRef<str>) {
        self.errors.push(format!("{path}: {}", message.as_ref()));
    }

    fn warn(&mut self, message: String) {
        self.warnings.push(message);
    }

    /// The object, or `None` with an error. `None` input (an absent key) is an error too:
    /// callers only pass absent values where an object is required.
    fn object<'a>(
        &mut self,
        value: Option<&'a Value>,
        path: &str,
    ) -> Option<&'a Map<String, Value>> {
        match value {
            Some(Value::Object(map)) => Some(map),
            _ => {
                self.error(path, "must be an object");
                None
            }
        }
    }

    /// Whether `value` is a string within the limits. An absent optional value is not an
    /// error but returns `false`, as in the SDK.
    fn string(
        &mut self,
        value: Option<&Value>,
        path: &str,
        max_length: usize,
        required: bool,
    ) -> bool {
        let Some(value) = value else {
            if required {
                self.error(path, "must be a non-empty string");
            }
            return false;
        };
        let text = match value.as_str() {
            Some(text) if !(required && text.is_empty()) => text,
            _ => {
                self.error(
                    path,
                    if required {
                        "must be a non-empty string"
                    } else {
                        "must be a string"
                    },
                );
                return false;
            }
        };
        if js_length(text) > max_length {
            self.error(path, format!("must be at most {max_length} characters"));
            return false;
        }
        true
    }

    fn url(&mut self, value: Option<&Value>, path: &str, required: bool) {
        if !self.string(value, path, 512, required) {
            return;
        }
        let text = value.and_then(Value::as_str).unwrap_or("");
        let Ok(parsed) = Url::parse(text) else {
            self.error(path, "must be a URL");
            return;
        };
        if !METADATA_URL_SCHEMES.contains(&parsed.scheme()) {
            self.error(path, "URL scheme must be one of https:, ipfs:, ar:");
        }
    }

    fn boolean(&mut self, value: Option<&Value>, path: &str) {
        if value.is_some_and(|v| !v.is_boolean()) {
            self.error(path, "must be a boolean");
        }
    }

    /// The array's items; empty when absent, not an array, or too long (the last two with
    /// an error).
    fn array<'a>(&mut self, value: Option<&'a Value>, path: &str, max_items: usize) -> &'a [Value] {
        match value {
            None => &[],
            Some(Value::Array(items)) if items.len() <= max_items => items,
            Some(Value::Array(_)) => {
                self.error(path, format!("must have at most {max_items} items"));
                &[]
            }
            Some(_) => {
                self.error(path, "must be an array");
                &[]
            }
        }
    }
}

fn check_args(c: &mut Checker, value: Option<&Value>, path: &str, init: bool) -> HashSet<String> {
    let mut names = HashSet::new();
    for (i, arg) in c.array(value, path, 32).iter().enumerate() {
        let p = format!("{path}[{i}]");
        let Some(arg) = c.object(Some(arg), &p) else {
            continue;
        };
        match arg.get("name").and_then(Value::as_str) {
            Some(name) if is_name(name) => {
                if !names.insert(name.to_string()) {
                    c.error(
                        &format!("{p}.name"),
                        format!("duplicate argument \"{name}\""),
                    );
                }
            }
            _ => c.error(
                &format!("{p}.name"),
                "must be an identifier (letters, digits, _)",
            ),
        }
        let type_ok = arg
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|t| ARG_TYPES.contains(&t));
        if !type_ok {
            c.error(
                &format!("{p}.type"),
                format!("must be one of {}", ARG_TYPES.join(", ")),
            );
        }
        c.string(arg.get("label"), &format!("{p}.label"), 64, false);
        c.string(
            arg.get("description"),
            &format!("{p}.description"),
            1000,
            false,
        );
        let decimals_path = format!("{p}.decimals_of");
        if c.string(arg.get("decimals_of"), &decimals_path, 256, false) {
            let text = arg["decimals_of"].as_str().unwrap_or("");
            check_source(c, text, &decimals_path, init, &names);
        }
    }
    names
}

fn check_source(c: &mut Checker, text: &str, path: &str, init: bool, arg_names: &HashSet<String>) {
    let source = match parse_account_source(text) {
        Ok(source) => source,
        Err(error) => {
            c.error(path, error);
            return;
        }
    };
    if init && source.needs_market() {
        c.error(
            path,
            format!("\"{text}\" refers to the market's mints, which don't exist before the market is created"),
        );
    }
    if let AccountSource::Pda { seeds, .. } = &source {
        for seed in seeds {
            if let PdaSeed::Arg(name) = seed {
                if !arg_names.contains(name) {
                    c.error(path, format!("seed refers to unknown argument \"{name}\""));
                }
            }
        }
    }
}

fn check_accounts(
    c: &mut Checker,
    value: Option<&Value>,
    path: &str,
    init: bool,
    arg_names: &HashSet<String>,
) {
    for (i, account) in c.array(value, path, 64).iter().enumerate() {
        let p = format!("{path}[{i}]");
        let Some(account) = c.object(Some(account), &p) else {
            continue;
        };
        c.string(account.get("name"), &format!("{p}.name"), 64, true);
        let source_path = format!("{p}.source");
        if c.string(account.get("source"), &source_path, 512, true) {
            let text = account["source"].as_str().unwrap_or("");
            check_source(c, text, &source_path, init, arg_names);
        }
        c.boolean(account.get("signer"), &format!("{p}.signer"));
        c.boolean(account.get("writable"), &format!("{p}.writable"));
    }
}

fn check_init(c: &mut Checker, init: &Value) {
    let Some(init) = c.object(Some(init), "init") else {
        return;
    };
    c.string(init.get("description"), "init.description", 2000, false);
    c.string(init.get("idl_type"), "init.idl_type", 128, false);
    let arg_names = check_args(c, init.get("args"), "init.args", true);
    if c.string(init.get("market_state"), "init.market_state", 512, false) {
        let market_state = init["market_state"].as_str().unwrap_or("");
        if market_state != "new_keypair" {
            check_source(c, market_state, "init.market_state", true, &arg_names);
            if market_state == "market_state" || market_state == "market" {
                c.error("init.market_state", "cannot refer to the market itself");
            }
        }
    }
    check_accounts(c, init.get("accounts"), "init.accounts", true, &arg_names);
}

fn check_actions(c: &mut Checker, value: Option<&Value>) {
    let mut names = HashSet::new();
    for (i, action) in c.array(value, "actions", 64).iter().enumerate() {
        let p = format!("actions[{i}]");
        let Some(action) = c.object(Some(action), &p) else {
            continue;
        };
        match action.get("name").and_then(Value::as_str) {
            Some(name) if is_name(name) => {
                if !names.insert(name.to_string()) {
                    c.error(&format!("{p}.name"), format!("duplicate action \"{name}\""));
                }
            }
            _ => c.error(
                &format!("{p}.name"),
                "must be an identifier (letters, digits, _)",
            ),
        }
        c.string(action.get("label"), &format!("{p}.label"), 64, true);
        c.string(
            action.get("description"),
            &format!("{p}.description"),
            2000,
            false,
        );
        match action.get("kind") {
            None => {}
            Some(Value::String(kind)) => {
                if !ACTION_KINDS.contains(&kind.as_str()) {
                    c.warn(format!("{p}.kind: unknown kind \"{kind}\""));
                }
            }
            Some(_) => c.error(&format!("{p}.kind"), "must be a string"),
        }
        if let Some(discriminator) = action.get("discriminator") {
            if !discriminator
                .as_str()
                .is_some_and(|s| s.len() == 16 && is_hex(s))
            {
                c.error(&format!("{p}.discriminator"), "must be 16 hex digits");
            }
        }
        let arg_names = check_args(c, action.get("args"), &format!("{p}.args"), false);
        check_accounts(
            c,
            action.get("accounts"),
            &format!("{p}.accounts"),
            false,
            &arg_names,
        );
    }
}

// ---------------------------------------------------------------------------------------
// Account sources
// ---------------------------------------------------------------------------------------

/// One seed of a `pda:` source.
#[derive(Clone, Debug, PartialEq)]
pub enum PdaSeed {
    Str(String),
    Hex(Vec<u8>),
    Key(AccountSource),
    /// The Borsh encoding of the named argument.
    Arg(String),
}

/// A parsed account source string; see `parseAccountSource` in the SDK for the table.
#[derive(Clone, Debug, PartialEq)]
pub enum AccountSource {
    Wallet,
    MarketState,
    Market,
    StrategyProgram,
    SystemProgram,
    AssociatedTokenProgram,
    Mint(u8),
    Vault(u8),
    TokenProgram(u8),
    WalletTokenAccount(u8),
    Fixed(Pubkey),
    Input(String),
    /// `program` is `None` for the strategy program.
    Pda {
        program: Option<Pubkey>,
        seeds: Vec<PdaSeed>,
    },
    Ata {
        owner: Box<AccountSource>,
        mint: Box<AccountSource>,
    },
    TokenProgramOf(Box<AccountSource>),
}

impl AccountSource {
    /// Sources that only exist once a market does, so `init` cannot use them.
    pub fn needs_market(&self) -> bool {
        match self {
            AccountSource::Mint(_)
            | AccountSource::Vault(_)
            | AccountSource::TokenProgram(_)
            | AccountSource::WalletTokenAccount(_) => true,
            AccountSource::Ata { owner, mint } => owner.needs_market() || mint.needs_market(),
            AccountSource::TokenProgramOf(mint) => mint.needs_market(),
            AccountSource::Pda { seeds, .. } => seeds
                .iter()
                .any(|seed| matches!(seed, PdaSeed::Key(source) if source.needs_market())),
            _ => false,
        }
    }
}

/// Parses an account source string. The error text equals the SDK's.
pub fn parse_account_source(text: &str) -> Result<AccountSource, String> {
    let simple = match text {
        "wallet" => Some(AccountSource::Wallet),
        "market_state" => Some(AccountSource::MarketState),
        "market" => Some(AccountSource::Market),
        "strategy_program" => Some(AccountSource::StrategyProgram),
        "system_program" => Some(AccountSource::SystemProgram),
        "associated_token_program" => Some(AccountSource::AssociatedTokenProgram),
        _ => None,
    };
    if let Some(source) = simple {
        return Ok(source);
    }
    let Some((prefix, rest)) = text.split_once(':') else {
        return Err(format!("unknown account source \"{text}\""));
    };
    let indexed: Option<fn(u8) -> AccountSource> = match prefix {
        "mint" => Some(AccountSource::Mint),
        "vault" => Some(AccountSource::Vault),
        "token_program" => Some(AccountSource::TokenProgram),
        "wallet_token_account" => Some(AccountSource::WalletTokenAccount),
        _ => None,
    };
    if let Some(make) = indexed {
        return match rest.as_bytes() {
            [digit @ b'0'..=b'3'] => Ok(make(digit - b'0')),
            _ => Err(format!("\"{text}\": index must be 0 to 3")),
        };
    }
    match prefix {
        "fixed" => {
            if is_address(rest) {
                Ok(AccountSource::Fixed(rest.parse().unwrap()))
            } else {
                Err(format!("\"{text}\": not an address"))
            }
        }
        "input" => {
            if is_name(rest) {
                Ok(AccountSource::Input(rest.to_string()))
            } else {
                Err(format!("\"{text}\": invalid input name"))
            }
        }
        "token_program_of" => Ok(AccountSource::TokenProgramOf(Box::new(parse_nested(
            rest, text,
        )?))),
        "ata" => {
            let parts: Vec<&str> = rest.split(',').collect();
            if parts.len() != 2 {
                return Err(format!("\"{text}\": expected ata:<owner>,<mint>"));
            }
            let owner = parse_nested(parts[0], text)?;
            let mint = parse_nested(parts[1], text)?;
            Ok(AccountSource::Ata {
                owner: Box::new(owner),
                mint: Box::new(mint),
            })
        }
        "pda" => parse_pda(rest, text),
        _ => Err(format!("unknown account source \"{text}\"")),
    }
}

fn parse_nested(text: &str, whole: &str) -> Result<AccountSource, String> {
    if text.starts_with("pda:") || text.starts_with("ata:") {
        return Err(format!("\"{whole}\": pda and ata sources cannot be nested"));
    }
    parse_account_source(text).map_err(|error| format!("\"{whole}\": {error}"))
}

fn parse_pda(rest: &str, whole: &str) -> Result<AccountSource, String> {
    let (program_text, seed_text) = rest.split_once(':').unwrap_or((rest, ""));
    let program = if program_text == "strategy" {
        None
    } else if is_address(program_text) {
        Some(program_text.parse().unwrap())
    } else {
        return Err(format!(
            "\"{whole}\": pda program must be an address or \"strategy\""
        ));
    };
    let mut seeds = vec![];
    if !seed_text.is_empty() {
        for seed in seed_text.split(',') {
            seeds.push(parse_seed(seed, whole)?);
        }
    }
    // Solana allows 16 seeds including the bump, which the SDK adds.
    if seeds.len() > 15 {
        return Err(format!("\"{whole}\": at most 15 seeds"));
    }
    Ok(AccountSource::Pda { program, seeds })
}

fn parse_seed(seed: &str, whole: &str) -> Result<PdaSeed, String> {
    let (kind, value) = seed.split_once(':').unwrap_or((seed, ""));
    match kind {
        "str" => {
            if value.len() <= 32 {
                Ok(PdaSeed::Str(value.to_string()))
            } else {
                Err(format!("\"{whole}\": str seed longer than 32 bytes"))
            }
        }
        "hex" => {
            if value.len() % 2 != 0 || !is_hex(value) || value.len() > 64 {
                return Err(format!(
                    "\"{whole}\": hex seed must be at most 32 bytes of hex"
                ));
            }
            let bytes = (0..value.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&value[i..i + 2], 16).unwrap())
                .collect();
            Ok(PdaSeed::Hex(bytes))
        }
        "key" => Ok(PdaSeed::Key(parse_nested(value, whole)?)),
        "arg" => {
            if is_name(value) {
                Ok(PdaSeed::Arg(value.to_string()))
            } else {
                Err(format!("\"{whole}\": invalid arg seed name"))
            }
        }
        _ => Err(format!(
            "\"{whole}\": unknown seed \"{seed}\" (expected str:, hex:, key: or arg:)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn minimal_document_is_valid() {
        let doc = json!({ "schema": STRATEGY_METADATA_SCHEMA, "name": "Pool" });
        assert_eq!(parse_strategy_metadata(&doc), Ok(vec![]));
    }

    #[test]
    fn reports_errors_and_warnings() {
        let doc = json!({
            "schema": "other",
            "name": "",
            "image": "http://example.com/a.png",
            "category": "new-thing",
            "links": { "twitter": "https://example.com/me" },
            "conformance": { "report": "ipfs://cid/report.json", "sha256": "xyz" },
        });
        let errors = parse_strategy_metadata(&doc).unwrap_err();
        assert_eq!(
            errors,
            [
                "schema: must be \"pipeline-strategy-metadata/1\"",
                "name: must be a non-empty string",
                "image: URL scheme must be one of https:, ipfs:, ar:",
                "conformance.sha256: must be 64 hex digits",
            ]
        );
        let doc = json!({
            "schema": STRATEGY_METADATA_SCHEMA,
            "name": "Pool",
            "category": "new-thing",
            "links": { "twitter": "https://example.com/me", "github": "https://docs.github.com/x" },
        });
        assert_eq!(
            parse_strategy_metadata(&doc).unwrap(),
            [
                "category: unknown category \"new-thing\"",
                "links.twitter: expected a link on x.com or twitter.com",
            ]
        );
    }

    #[test]
    fn checks_account_sources() {
        let doc = json!({
            "schema": STRATEGY_METADATA_SCHEMA,
            "name": "Pool",
            "init": {
                "args": [{ "name": "fee", "type": "u16" }],
                "market_state": "pda:strategy:str:pool,arg:fee,arg:missing",
                "accounts": [{ "name": "v", "source": "vault:0" }],
            },
            "actions": [{ "name": "go", "label": "Go", "accounts": [{ "name": "x", "source": "ata:pda:strategy,mint:0" }] }],
        });
        let errors = parse_strategy_metadata(&doc).unwrap_err();
        assert_eq!(
            errors,
            [
                "init.market_state: seed refers to unknown argument \"missing\"",
                "init.accounts[0].source: \"vault:0\" refers to the market's mints, which don't exist before the market is created",
                "actions[0].accounts[0].source: \"ata:pda:strategy,mint:0\": pda and ata sources cannot be nested",
            ]
        );
    }

    #[test]
    fn resolves_gateways() {
        let resolve =
            |uri| resolve_metadata_uri(uri, DEFAULT_IPFS_GATEWAY, DEFAULT_ARWEAVE_GATEWAY);
        assert_eq!(
            resolve("ipfs://ipfs/cid/a.json").as_deref(),
            Some("https://ipfs.io/ipfs/cid/a.json")
        );
        assert_eq!(
            resolve("ar://tx").as_deref(),
            Some("https://arweave.net/tx")
        );
        assert_eq!(
            resolve(" https://x.com/m.json ").as_deref(),
            Some("https://x.com/m.json")
        );
        assert_eq!(resolve("http://x.com/m.json"), None);
        assert_eq!(resolve("ipfs://"), None);
        assert_eq!(resolve("ftp://x"), None);
    }

    #[test]
    fn dates_follow_javascript() {
        assert!(is_date("2026-02-30"));
        assert!(is_date("0000-01-01"));
        assert!(!is_date("2026-02-32"));
        assert!(!is_date("2026-13-01"));
        assert!(!is_date("2026-01-00"));
        assert!(!is_date("2026-1-01"));
    }
}
