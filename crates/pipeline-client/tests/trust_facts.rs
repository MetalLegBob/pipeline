//! Checks `facts` and `metadata` against the shared vectors in
//! `tests/fixtures/trust-facts.json`, which the TypeScript SDK's tests check too. If the
//! Rust side changes on purpose, regenerate the file with
//! `cargo run -q -p pipeline-client --example ts_facts_fixtures > crates/pipeline-client/tests/fixtures/trust-facts.json`
//! and make the SDK agree.

use base64::Engine;
use pipeline_client::facts::{self, AccountView};
use pipeline_client::metadata::parse_strategy_metadata;
use pipeline_client::Pubkey;
use serde_json::{json, Value};

const FIXTURES: &str = include_str!("fixtures/trust-facts.json");

fn fixtures() -> Value {
    serde_json::from_str(FIXTURES).unwrap()
}

fn bytes(value: &Value) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(value.as_str().unwrap())
        .unwrap()
}

fn pubkey(value: &Value) -> Pubkey {
    value.as_str().unwrap().parse().unwrap()
}

#[test]
fn flag_table_matches() {
    let table: Vec<(String, String)> = serde_json::from_value(fixtures()["flags"].clone()).unwrap();
    let ours: Vec<(String, String)> = facts::FLAGS
        .iter()
        .map(|(n, d)| (n.to_string(), d.to_string()))
        .collect();
    assert_eq!(table, ours);
}

#[test]
fn mints_decode_to_the_shared_flags() {
    for case in fixtures()["mints"].as_array().unwrap() {
        let name = &case["name"];
        let epoch: u64 = case["epoch"].as_str().unwrap().parse().unwrap();
        let decoded = facts::decode_mint(&pubkey(&case["owner"]), &bytes(&case["data"]), epoch);
        let actual = decoded.map(|d| {
            json!({
                "flags": d.flags,
                "decimals": d.decimals,
                "supply": d.supply.to_string(),
                "mint_authority": d.mint_authority.map(|k| k.to_string()),
                "freeze_authority": d.freeze_authority.map(|k| k.to_string()),
                "extension_types": d.extension_types,
                "unknown_extensions": d.unknown_extensions,
                "transfer_fee_current_bps": d.extensions.transfer_fee.as_ref().map(|f| f.current.basis_points),
                "token_metadata_name": d.extensions.token_metadata.as_ref().map(|m| m.name.clone()),
            })
        });
        assert_eq!(actual.unwrap_or(Value::Null), case["expected"], "{name}");
    }
}

struct Owned {
    owner: Pubkey,
    executable: bool,
    data: Vec<u8>,
}

fn account(value: &Value) -> Option<Owned> {
    (!value.is_null()).then(|| Owned {
        owner: pubkey(&value["owner"]),
        executable: value["executable"].as_bool().unwrap(),
        data: bytes(&value["data"]),
    })
}

fn view(account: &Option<Owned>) -> Option<AccountView<'_>> {
    account.as_ref().map(|a| AccountView {
        owner: &a.owner,
        executable: a.executable,
        data: &a.data,
    })
}

#[test]
fn programs_give_the_shared_facts() {
    for case in fixtures()["programs"].as_array().unwrap() {
        let program = account(&case["program"]);
        let program_data = account(&case["program_data"]);
        let facts =
            facts::inspect_program(view(&program).as_ref(), view(&program_data).as_ref(), true);
        let mut actual = facts.to_json();
        actual["last_deploy_slot"] = json!(facts.last_deploy_slot.map(|s| s.to_string()));
        assert_eq!(actual, case["expected"], "{}", case["name"]);
    }
}

#[test]
fn metadata_documents_validate_the_same() {
    for case in fixtures()["metadata"].as_array().unwrap() {
        let result = parse_strategy_metadata(&case["document"]);
        let (mut errors, mut warnings) = match result {
            Ok(warnings) => (vec![], warnings),
            Err(errors) => (errors, vec![]),
        };
        errors.sort();
        warnings.sort();
        assert_eq!(
            errors.is_empty(),
            case["ok"].as_bool().unwrap(),
            "{}",
            case["name"]
        );
        assert_eq!(json!(errors), case["errors"], "{}", case["name"]);
        assert_eq!(json!(warnings), case["warnings"], "{}", case["name"]);
    }
}
