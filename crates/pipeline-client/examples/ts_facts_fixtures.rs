//! Prints the shared trust-facts vectors as JSON:
//!
//! ```text
//! cargo run -q -p pipeline-client --example ts_facts_fixtures \
//!     > crates/pipeline-client/tests/fixtures/trust-facts.json
//! ```
//!
//! Both `pipeline_client::facts` / `pipeline_client::metadata` (in
//! `tests/trust_facts.rs`) and the TypeScript SDK (`sdk/ts/test/trust-facts.test.ts`)
//! check themselves against the file, so the two implementations can't drift apart.
//!
//! Each case's expected flags are written here by hand and asserted against the Rust
//! module before anything is printed. Mint accounts with extensions are built with the
//! layout Token-2022 uses (extensions after the account type byte at offset 165, padding
//! away from the multisig length) and, unless a case is deliberately malformed, parsed
//! back with `spl-token-2022-interface` to confirm that Token-2022 itself accepts them.
//!
//! JSON conventions: addresses are base58, `u64` values decimal strings, account data
//! base64, and field names snake_case.

use base64::Engine;
use pipeline_client::facts::{self, extension_type as ext, AccountView};
use pipeline_client::metadata::{parse_strategy_metadata, STRATEGY_METADATA_SCHEMA};
use pipeline_client::{Pubkey, BPF_LOADER_UPGRADEABLE_ID, TOKEN_2022_PROGRAM_ID, TOKEN_PROGRAM_ID};
use serde_json::{json, Value};
use spl_token_2022_interface::extension::transfer_fee::TransferFeeConfig;
use spl_token_2022_interface::extension::{
    BaseStateWithExtensions, ExtensionType, StateWithExtensions,
};
use spl_token_2022_interface::state::Mint;

fn key(n: u8) -> Pubkey {
    Pubkey::new_from_array([n; 32])
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

// ---------------------------------------------------------------------------------------
// Mint bytes
// ---------------------------------------------------------------------------------------

#[derive(Default, Clone)]
struct Base {
    mint_authority: Option<Pubkey>,
    freeze_authority: Option<Pubkey>,
    supply: u64,
    decimals: u8,
}

fn base_mint(base: &Base) -> Vec<u8> {
    let mut data = vec![0u8; 82];
    if let Some(authority) = base.mint_authority {
        data[0] = 1;
        data[4..36].copy_from_slice(authority.as_ref());
    }
    data[36..44].copy_from_slice(&base.supply.to_le_bytes());
    data[44] = base.decimals;
    data[45] = 1;
    if let Some(authority) = base.freeze_authority {
        data[46] = 1;
        data[50..82].copy_from_slice(authority.as_ref());
    }
    data
}

/// A Token-2022 mint with TLV extensions, laid out as Token-2022 lays them out.
fn mint_2022(base: &Base, entries: &[(u16, Vec<u8>)]) -> Vec<u8> {
    let mut data = base_mint(base);
    if entries.is_empty() {
        return data;
    }
    data.resize(165, 0);
    data.push(1);
    for (kind, value) in entries {
        data.extend_from_slice(&kind.to_le_bytes());
        data.extend_from_slice(&(value.len() as u16).to_le_bytes());
        data.extend_from_slice(value);
    }
    // An account of exactly the multisig length would be ambiguous; Token-2022 adds an
    // empty (uninitialized) TLV entry.
    if data.len() == 355 {
        data.extend_from_slice(&[0; 4]);
    }
    data
}

fn pubkey_bytes(key: Option<Pubkey>) -> Vec<u8> {
    key.map_or(vec![0; 32], |k| k.to_bytes().to_vec())
}

fn concat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

fn fee(epoch: u64, maximum: u64, bps: u16) -> Vec<u8> {
    concat(&[
        &epoch.to_le_bytes(),
        &maximum.to_le_bytes(),
        &bps.to_le_bytes(),
    ])
}

fn transfer_fee_config(
    authority: Option<Pubkey>,
    older: Vec<u8>,
    newer: Vec<u8>,
) -> (u16, Vec<u8>) {
    let value = concat(&[
        &pubkey_bytes(authority),
        &pubkey_bytes(Some(key(31))),
        &1234u64.to_le_bytes(),
        &older,
        &newer,
    ]);
    (ext::TRANSFER_FEE_CONFIG, value)
}

fn two_keys(kind: u16, a: Option<Pubkey>, b: Option<Pubkey>) -> (u16, Vec<u8>) {
    (kind, concat(&[&pubkey_bytes(a), &pubkey_bytes(b)]))
}

fn one_key(kind: u16, a: Option<Pubkey>) -> (u16, Vec<u8>) {
    (kind, pubkey_bytes(a))
}

fn string(text: &str) -> Vec<u8> {
    concat(&[&(text.len() as u32).to_le_bytes(), text.as_bytes()])
}

fn token_metadata(mint: Pubkey, fields: &[(&str, &str)]) -> (u16, Vec<u8>) {
    let mut value = concat(&[
        &pubkey_bytes(Some(key(40))),
        mint.as_ref(),
        &string("Test Token"),
        &string("TEST"),
        &string("https://example.com/token.json"),
        &(fields.len() as u32).to_le_bytes(),
    ]);
    for (k, v) in fields {
        value.extend(string(k));
        value.extend(string(v));
    }
    (ext::TOKEN_METADATA, value)
}

/// Token-2022's own reading of the mint: its extension types, and the transfer fee in
/// force at `epoch`.
fn official(data: &[u8], epoch: u64) -> (Vec<u16>, Option<u16>) {
    let state = StateWithExtensions::<Mint>::unpack(data).expect("Token-2022 accepts the mint");
    let types: Vec<u16> = state
        .get_extension_types()
        .unwrap()
        .into_iter()
        .map(u16::from)
        .collect();
    // Fixed-size extensions must have exactly Token-2022's size.
    let mut offset = 166;
    while offset + 4 <= data.len() {
        let kind = u16::from_le_bytes([data[offset], data[offset + 1]]);
        let length = u16::from_le_bytes([data[offset + 2], data[offset + 3]]) as usize;
        let extension = ExtensionType::try_from(kind).unwrap();
        if kind != 0 && extension != ExtensionType::TokenMetadata {
            let mut alone = ExtensionType::try_calculate_account_len::<Mint>(&[extension]).unwrap();
            if alone == 359 {
                alone -= 4;
            }
            assert_eq!(length, alone - 170, "{extension:?}");
        }
        offset += 4 + length;
    }
    let fee = state
        .get_extension::<TransferFeeConfig>()
        .ok()
        .map(|config| u16::from(config.get_epoch_fee(epoch).transfer_fee_basis_points));
    (types, fee)
}

struct MintCase {
    name: &'static str,
    owner: Pubkey,
    epoch: u64,
    data: Vec<u8>,
    /// Whether Token-2022 itself must accept the bytes.
    official: bool,
    /// `None` when the bytes are not a mint.
    flags: Option<Vec<&'static str>>,
}

fn mint_cases() -> Vec<MintCase> {
    let t22 = TOKEN_2022_PROGRAM_ID;
    let mint = key(50);
    let authorities = Base {
        mint_authority: Some(key(1)),
        freeze_authority: Some(key(2)),
        supply: 1_000_000_000_000,
        decimals: 9,
    };
    let plain = Base {
        supply: u64::MAX,
        decimals: 6,
        ..Base::default()
    };
    let case = |name, owner, epoch, data, official, flags: Option<&[&'static str]>| MintCase {
        name,
        owner,
        epoch,
        data,
        official,
        flags: flags.map(|f| f.to_vec()),
    };
    let mut cases = vec![
        case(
            "spl-plain",
            TOKEN_PROGRAM_ID,
            0,
            base_mint(&plain),
            false,
            Some(&[]),
        ),
        case(
            "spl-authorities",
            TOKEN_PROGRAM_ID,
            0,
            base_mint(&authorities),
            false,
            Some(&["mint-authority", "freeze-authority"]),
        ),
        case(
            "t22-no-extensions",
            t22,
            0,
            base_mint(&plain),
            false,
            Some(&["token-2022"]),
        ),
        case(
            "t22-metadata",
            t22,
            0,
            mint_2022(
                &Base {
                    mint_authority: Some(key(1)),
                    ..plain.clone()
                },
                &[
                    two_keys(ext::METADATA_POINTER, Some(key(40)), Some(mint)),
                    token_metadata(mint, &[("site", "https://example.com"), ("é", "ü")]),
                ],
            ),
            true,
            Some(&["token-2022", "mint-authority"]),
        ),
        case(
            "t22-transfer-fee-in-force",
            t22,
            10,
            mint_2022(
                &plain,
                &[transfer_fee_config(
                    Some(key(3)),
                    fee(0, 5000, 50),
                    fee(0, 5000, 50),
                )],
            ),
            true,
            Some(&["token-2022", "transfer-fee", "transfer-fee-authority"]),
        ),
        case(
            "t22-transfer-fee-scheduled",
            t22,
            10,
            mint_2022(
                &plain,
                &[transfer_fee_config(None, fee(0, 0, 0), fee(20, 9, 100))],
            ),
            true,
            Some(&["token-2022", "transfer-fee"]),
        ),
        case(
            "t22-transfer-fee-lowered-to-zero",
            t22,
            10,
            mint_2022(
                &plain,
                &[transfer_fee_config(None, fee(0, 9, 100), fee(5, 0, 0))],
            ),
            true,
            Some(&["token-2022"]),
        ),
        case(
            "t22-transfer-fee-before-lowering",
            t22,
            4,
            mint_2022(
                &plain,
                &[transfer_fee_config(None, fee(0, 9, 100), fee(5, 0, 0))],
            ),
            true,
            Some(&["token-2022", "transfer-fee"]),
        ),
        case(
            "t22-transfer-hook",
            t22,
            0,
            mint_2022(
                &plain,
                &[two_keys(ext::TRANSFER_HOOK, Some(key(4)), Some(key(5)))],
            ),
            true,
            Some(&["token-2022", "transfer-hook", "transfer-hook-authority"]),
        ),
        case(
            "t22-transfer-hook-authority-only",
            t22,
            0,
            mint_2022(&plain, &[two_keys(ext::TRANSFER_HOOK, Some(key(4)), None)]),
            true,
            Some(&["token-2022", "transfer-hook-authority"]),
        ),
        case(
            "t22-many-powers",
            t22,
            0,
            mint_2022(
                &authorities,
                &[
                    one_key(ext::MINT_CLOSE_AUTHORITY, Some(key(6))),
                    (ext::DEFAULT_ACCOUNT_STATE, vec![2]),
                    (ext::NON_TRANSFERABLE, vec![]),
                    (
                        ext::INTEREST_BEARING_CONFIG,
                        concat(&[
                            &pubkey_bytes(Some(key(7))),
                            &1_700_000_000i64.to_le_bytes(),
                            &250i16.to_le_bytes(),
                            &1_700_000_100i64.to_le_bytes(),
                            &(-30i16).to_le_bytes(),
                        ]),
                    ),
                    one_key(ext::PERMANENT_DELEGATE, Some(key(8))),
                ],
            ),
            true,
            Some(&[
                "token-2022",
                "mint-authority",
                "freeze-authority",
                "permanent-delegate",
                "non-transferable",
                "default-frozen",
                "closable-mint",
                "interest-bearing",
            ]),
        ),
        case(
            "t22-renounced-powers",
            t22,
            0,
            mint_2022(
                &plain,
                &[
                    one_key(ext::MINT_CLOSE_AUTHORITY, None),
                    (ext::DEFAULT_ACCOUNT_STATE, vec![1]),
                    one_key(ext::PERMANENT_DELEGATE, None),
                    (ext::PAUSABLE, concat(&[&pubkey_bytes(None), &[0]])),
                ],
            ),
            true,
            Some(&["token-2022"]),
        ),
        case(
            "t22-paused",
            t22,
            0,
            mint_2022(
                &plain,
                &[(ext::PAUSABLE, concat(&[&pubkey_bytes(Some(key(9))), &[1]]))],
            ),
            true,
            Some(&["token-2022", "pausable", "paused"]),
        ),
        case(
            "t22-paused-without-authority",
            t22,
            0,
            mint_2022(
                &plain,
                &[(ext::PAUSABLE, concat(&[&pubkey_bytes(None), &[1]]))],
            ),
            true,
            Some(&["token-2022", "pausable", "paused"]),
        ),
        case(
            "t22-scaled-confidential-burn",
            t22,
            0,
            mint_2022(
                &plain,
                &[
                    (
                        ext::SCALED_UI_AMOUNT,
                        concat(&[
                            &pubkey_bytes(Some(key(10))),
                            &1.5f64.to_le_bytes(),
                            &1_800_000_000i64.to_le_bytes(),
                            &2.0f64.to_le_bytes(),
                        ]),
                    ),
                    (
                        ext::CONFIDENTIAL_TRANSFER_MINT,
                        concat(&[&pubkey_bytes(Some(key(11))), &[1], &[0; 32]]),
                    ),
                    one_key(ext::PERMISSIONED_BURN, Some(key(12))),
                ],
            ),
            true,
            Some(&[
                "token-2022",
                "scaled-ui-amount",
                "confidential-transfers",
                "permissioned-burn",
            ]),
        ),
        case(
            "t22-groups",
            t22,
            0,
            mint_2022(
                &plain,
                &[
                    two_keys(ext::GROUP_POINTER, Some(key(13)), Some(mint)),
                    (
                        ext::TOKEN_GROUP,
                        concat(&[
                            &pubkey_bytes(Some(key(13))),
                            mint.as_ref(),
                            &3u64.to_le_bytes(),
                            &10u64.to_le_bytes(),
                        ]),
                    ),
                    two_keys(ext::GROUP_MEMBER_POINTER, Some(key(13)), Some(mint)),
                    (
                        ext::TOKEN_GROUP_MEMBER,
                        concat(&[mint.as_ref(), key(14).as_ref(), &1u64.to_le_bytes()]),
                    ),
                ],
            ),
            true,
            Some(&["token-2022"]),
        ),
        case(
            "t22-confidential-mint-burn-and-fee",
            t22,
            0,
            mint_2022(
                &plain,
                &[
                    transfer_fee_config(None, fee(0, 0, 0), fee(0, 0, 0)),
                    (
                        ext::CONFIDENTIAL_TRANSFER_FEE_CONFIG,
                        concat(&[&pubkey_bytes(Some(key(15))), &[0; 32], &[0], &[0; 64]]),
                    ),
                    (ext::CONFIDENTIAL_MINT_BURN, vec![0; 196]),
                ],
            ),
            true,
            Some(&["token-2022", "confidential-transfers"]),
        ),
        // Deliberately malformed or unusual bytes, which Token-2022 would not produce.
        case(
            "t22-unknown-extension-type",
            t22,
            0,
            mint_2022(&plain, &[(200, vec![1, 2, 3])]),
            false,
            Some(&["token-2022", "unknown-extension"]),
        ),
        case(
            "t22-short-extension",
            t22,
            0,
            mint_2022(&plain, &[(ext::PERMANENT_DELEGATE, vec![8; 31])]),
            false,
            Some(&["token-2022", "unknown-extension"]),
        ),
        case(
            "t22-bad-default-state",
            t22,
            0,
            mint_2022(&plain, &[(ext::DEFAULT_ACCOUNT_STATE, vec![7])]),
            false,
            Some(&["token-2022", "unknown-extension"]),
        ),
        case(
            "t22-truncated-token-metadata",
            t22,
            0,
            {
                let (kind, mut value) = token_metadata(mint, &[]);
                // Claim a longer name than the value holds.
                value[64] = 200;
                mint_2022(&plain, &[(kind, value)])
            },
            false,
            Some(&["token-2022", "unknown-extension"]),
        ),
        case(
            "t22-truncated-tlv",
            t22,
            0,
            {
                let mut data = mint_2022(&plain, &[one_key(ext::PERMANENT_DELEGATE, Some(key(8)))]);
                data.truncate(data.len() - 1);
                data
            },
            false,
            Some(&["token-2022"]),
        ),
        case(
            "t22-entries-after-uninitialized",
            t22,
            0,
            {
                let mut data = mint_2022(&plain, &[]);
                data.resize(165, 0);
                data.push(1);
                data.extend_from_slice(&[0; 4]);
                data.extend_from_slice(&12u16.to_le_bytes());
                data.extend_from_slice(&32u16.to_le_bytes());
                data.extend_from_slice(&[8; 32]);
                data
            },
            false,
            Some(&["token-2022"]),
        ),
        case(
            "not-a-mint-uninitialized",
            TOKEN_PROGRAM_ID,
            0,
            {
                let mut data = base_mint(&plain);
                data[45] = 0;
                data
            },
            false,
            None,
        ),
        case(
            "not-a-mint-wrong-owner",
            key(60),
            0,
            base_mint(&plain),
            false,
            None,
        ),
        case(
            "not-a-mint-short",
            TOKEN_PROGRAM_ID,
            0,
            vec![1; 81],
            false,
            None,
        ),
        case(
            "not-a-mint-spl-with-extensions",
            TOKEN_PROGRAM_ID,
            0,
            mint_2022(&plain, &[(ext::NON_TRANSFERABLE, vec![])]),
            false,
            None,
        ),
        case(
            "not-a-mint-t22-between-lengths",
            t22,
            0,
            {
                let mut data = base_mint(&plain);
                data.resize(120, 0);
                data
            },
            false,
            None,
        ),
        case(
            "not-a-mint-t22-token-account-type",
            t22,
            0,
            {
                let mut data = mint_2022(&plain, &[(ext::NON_TRANSFERABLE, vec![])]);
                data[165] = 2;
                data
            },
            false,
            None,
        ),
    ];
    for case in &mut cases {
        if case.official {
            let decoded = facts::decode_mint(&case.owner, &case.data, case.epoch).unwrap();
            let (types, fee) = official(&case.data, case.epoch);
            assert_eq!(decoded.extension_types, types, "{}", case.name);
            assert_eq!(
                decoded
                    .extensions
                    .transfer_fee
                    .as_ref()
                    .map(|f| f.current.basis_points),
                fee,
                "{}",
                case.name
            );
            assert!(decoded.unknown_extensions.is_empty(), "{}", case.name);
        }
    }
    cases
}

fn mint_json(case: &MintCase) -> Value {
    let decoded = facts::decode_mint(&case.owner, &case.data, case.epoch);
    let flags = decoded.as_ref().map(|d| d.flags.clone());
    assert_eq!(flags, case.flags, "{}", case.name);
    json!({
        "name": case.name,
        "owner": case.owner.to_string(),
        "epoch": case.epoch.to_string(),
        "data": b64(&case.data),
        "expected": decoded.map(|d| json!({
            "flags": d.flags,
            "decimals": d.decimals,
            "supply": d.supply.to_string(),
            "mint_authority": d.mint_authority.map(|k| k.to_string()),
            "freeze_authority": d.freeze_authority.map(|k| k.to_string()),
            "extension_types": d.extension_types,
            "unknown_extensions": d.unknown_extensions,
            "transfer_fee_current_bps": d.extensions.transfer_fee.as_ref().map(|f| f.current.basis_points),
            "token_metadata_name": d.extensions.token_metadata.as_ref().map(|m| m.name.clone()),
        })),
    })
}

// ---------------------------------------------------------------------------------------
// Programs
// ---------------------------------------------------------------------------------------

struct Account {
    owner: Pubkey,
    executable: bool,
    data: Vec<u8>,
}

struct ProgramCase {
    name: &'static str,
    program: Option<Account>,
    program_data: Option<Account>,
    flags: Vec<&'static str>,
}

const ELF: &[u8] = b"\x7fELF\x02\x01\x01\x00 pretend program code \x00\x01";

fn upgradeable(authority: Option<Pubkey>, slot: u64) -> (Account, Account) {
    let program = Account {
        owner: BPF_LOADER_UPGRADEABLE_ID,
        executable: true,
        data: concat(&[&2u32.to_le_bytes(), key(70).as_ref()]),
    };
    let mut header = concat(&[&3u32.to_le_bytes(), &slot.to_le_bytes()]);
    match authority {
        Some(authority) => {
            header.push(1);
            header.extend_from_slice(authority.as_ref());
        }
        None => header.extend_from_slice(&[0; 33]),
    }
    let data = Account {
        owner: BPF_LOADER_UPGRADEABLE_ID,
        executable: false,
        data: concat(&[&header, ELF, &[0; 64]]),
    };
    (program, data)
}

fn loader_v4(status: u64) -> Account {
    Account {
        owner: facts::LOADER_V4_ID,
        executable: true,
        data: concat(&[
            &900u64.to_le_bytes(),
            key(71).as_ref(),
            &status.to_le_bytes(),
            ELF,
            &[0; 8],
        ]),
    }
}

fn program_cases() -> Vec<ProgramCase> {
    let (program, data) = upgradeable(Some(key(72)), 500);
    let (frozen_program, frozen_data) = upgradeable(None, 501);
    let (closed_program, _) = upgradeable(Some(key(72)), 1);
    vec![
        ProgramCase {
            name: "v3-upgradeable",
            program: Some(program),
            program_data: Some(data),
            flags: vec!["upgradeable"],
        },
        ProgramCase {
            name: "v3-frozen",
            program: Some(frozen_program),
            program_data: Some(frozen_data),
            flags: vec!["frozen"],
        },
        ProgramCase {
            name: "v3-program-data-closed",
            program: Some(closed_program),
            program_data: None,
            flags: vec!["program-missing"],
        },
        ProgramCase {
            name: "v4-deployed",
            program: Some(loader_v4(1)),
            program_data: None,
            flags: vec!["upgradeable"],
        },
        ProgramCase {
            name: "v4-finalized",
            program: Some(loader_v4(2)),
            program_data: None,
            flags: vec!["frozen"],
        },
        ProgramCase {
            name: "v4-retracted",
            program: Some(loader_v4(0)),
            program_data: None,
            flags: vec!["program-missing"],
        },
        ProgramCase {
            name: "v4-unknown-status",
            program: Some(loader_v4(9)),
            program_data: None,
            flags: vec![],
        },
        ProgramCase {
            name: "v2",
            program: Some(Account {
                owner: facts::LOADER_V2_ID,
                executable: true,
                data: concat(&[ELF, &[0; 16]]),
            }),
            program_data: None,
            flags: vec!["frozen"],
        },
        ProgramCase {
            name: "native",
            program: Some(Account {
                owner: facts::NATIVE_LOADER_ID,
                executable: true,
                data: b"spl_token".to_vec(),
            }),
            program_data: None,
            flags: vec![],
        },
        ProgramCase {
            name: "not-a-program",
            program: Some(Account {
                owner: key(73),
                executable: false,
                data: vec![1, 2, 3],
            }),
            program_data: None,
            flags: vec!["program-missing"],
        },
        ProgramCase {
            name: "no-account",
            program: None,
            program_data: None,
            flags: vec!["program-missing"],
        },
    ]
}

fn account_json(account: &Option<Account>) -> Value {
    account.as_ref().map_or(Value::Null, |a| {
        json!({ "owner": a.owner.to_string(), "executable": a.executable, "data": b64(&a.data) })
    })
}

fn view(account: &Option<Account>) -> Option<AccountView<'_>> {
    account.as_ref().map(|a| AccountView {
        owner: &a.owner,
        executable: a.executable,
        data: &a.data,
    })
}

fn program_json(case: &ProgramCase) -> Value {
    let facts = facts::inspect_program(
        view(&case.program).as_ref(),
        view(&case.program_data).as_ref(),
        true,
    );
    assert_eq!(facts.flags, case.flags, "{}", case.name);
    let mut expected = facts.to_json();
    expected["last_deploy_slot"] = json!(facts.last_deploy_slot.map(|s| s.to_string()));
    json!({
        "name": case.name,
        "program_id": key(69).to_string(),
        "program": account_json(&case.program),
        "program_data": account_json(&case.program_data),
        "expected": expected,
    })
}

// ---------------------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------------------

fn full_document() -> Value {
    json!({
        "schema": STRATEGY_METADATA_SCHEMA,
        "name": "Constant product",
        "description": "x*y=k pool with a configurable fee.",
        "image": "https://example.com/logo.png",
        "category": "amm",
        "tags": ["cpmm", "fees"],
        "links": {
            "website": "https://example.com",
            "source": "https://github.com/example/cp",
            "docs": "ipfs://bafybeigdyrzt5sfp7udm7hu76uh7y26nf3efuylqabf3oclgtqy55fbzdi/docs.html",
            "idl": "ar://abc123/idl.json",
            "twitter": "https://x.com/example",
            "github": "https://github.com/example",
            "discord": "https://discord.gg/example",
            "telegram": "https://t.me/example",
            "farcaster": "https://farcaster.xyz/example"
        },
        "source": { "repository": "https://github.com/example/cp", "commit": "dac82fef2b88", "path": "programs/cp" },
        "init": {
            "description": "Creates a pool for two mints.",
            "idl_type": "InitParams",
            "market_state": "pda:strategy:str:pool,key:input:mint_a,key:input:mint_b,arg:fee_bps",
            "args": [{ "name": "fee_bps", "type": "u16", "label": "Pool fee (basis points)" }],
            "accounts": [
                { "name": "mint_a", "source": "input:mint_a" },
                { "name": "mint_b", "source": "input:mint_b" },
                { "name": "vault_a", "source": "ata:market_state,input:mint_a", "writable": true },
                { "name": "vault_b", "source": "ata:market_state,input:mint_b", "writable": true },
                { "name": "token_program_a", "source": "token_program_of:input:mint_a" },
                { "name": "token_program_b", "source": "token_program_of:input:mint_b" },
                { "name": "associated_token_program", "source": "associated_token_program" }
            ]
        },
        "actions": [{
            "name": "deposit",
            "label": "Add liquidity",
            "kind": "add_liquidity",
            "discriminator": "f223c68952e1f2b6",
            "args": [
                { "name": "amount_a", "type": "u64", "label": "Amount of token A", "decimals_of": "mint:0" },
                { "name": "amount_b", "type": "u64", "label": "Amount of token B", "decimals_of": "mint:1" }
            ],
            "accounts": [
                { "name": "depositor", "source": "wallet", "signer": true },
                { "name": "pool", "source": "market_state", "writable": true },
                { "name": "depositor_a", "source": "wallet_token_account:0", "writable": true },
                { "name": "vault_a", "source": "vault:0", "writable": true },
                { "name": "fixed", "source": "fixed:TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA" },
                { "name": "pda", "source": "pda:11111111111111111111111111111111:hex:00ff,str:a" }
            ]
        }],
        "conformance": {
            "report": "https://example.com/report.json",
            "sha256": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
        },
        "security": {
            "contact": "security@example.com",
            "policy": "https://example.com/security",
            "audits": [{ "by": "Example Audits", "url": "https://example.com/audit.pdf", "date": "2026-10-01" }]
        },
        "x_custom": { "anything": [1, 2, 3] }
    })
}

fn with(mut doc: Value, path: &[&str], value: Value) -> Value {
    let mut target = &mut doc;
    for key in &path[..path.len() - 1] {
        target = match key.parse::<usize>() {
            Ok(index) => &mut target[index],
            Err(_) => &mut target[*key],
        };
    }
    let last = path[path.len() - 1];
    match (last.parse::<usize>(), target.as_array_mut()) {
        (Ok(index), Some(items)) if index == items.len() => items.push(value),
        (Ok(index), _) => target[index] = value,
        (Err(_), _) => target[last] = value,
    }
    doc
}

fn minimal() -> Value {
    json!({ "schema": STRATEGY_METADATA_SCHEMA, "name": "Pool" })
}

fn metadata_cases() -> Vec<(&'static str, Value, bool)> {
    let full = full_document;
    vec![
        ("minimal", minimal(), true),
        ("full", full(), true),
        ("not-an-object", json!([1, 2]), false),
        (
            "wrong-schema",
            with(
                minimal(),
                &["schema"],
                json!("pipeline-strategy-metadata/2"),
            ),
            false,
        ),
        (
            "missing-name",
            json!({ "schema": STRATEGY_METADATA_SCHEMA }),
            false,
        ),
        (
            "null-description",
            with(minimal(), &["description"], Value::Null),
            false,
        ),
        // 33 emoji are 66 UTF-16 code units, over the 64-character limit.
        (
            "long-name-emoji",
            with(minimal(), &["name"], json!("\u{1F600}".repeat(33))),
            false,
        ),
        (
            "name-64-accented",
            with(minimal(), &["name"], json!("é".repeat(64))),
            true,
        ),
        (
            "http-image",
            with(minimal(), &["image"], json!("http://example.com/a.png")),
            false,
        ),
        (
            "not-a-url",
            with(minimal(), &["image"], json!("not a url")),
            false,
        ),
        (
            "https-without-host",
            with(minimal(), &["image"], json!("https://")),
            false,
        ),
        (
            "javascript-link",
            with(
                minimal(),
                &["links"],
                json!({ "website": "javascript:alert(1)" }),
            ),
            false,
        ),
        (
            "unknown-category",
            with(minimal(), &["category"], json!("prediction-market")),
            true,
        ),
        (
            "social-links-elsewhere",
            with(
                minimal(),
                &["links"],
                json!({ "twitter": "https://example.com/me", "github": "https://gist.github.com/x", "discord": "ipfs://cid" }),
            ),
            true,
        ),
        (
            "too-many-tags",
            with(minimal(), &["tags"], json!(vec!["t"; 17])),
            false,
        ),
        (
            "empty-tag",
            with(minimal(), &["tags"], json!(["ok", ""])),
            false,
        ),
        (
            "bad-commit",
            with(full(), &["source", "commit"], json!("xyz")),
            false,
        ),
        (
            "source-without-repository",
            with(minimal(), &["source"], json!({ "path": "p" })),
            false,
        ),
        (
            "bad-conformance-hash",
            with(full(), &["conformance", "sha256"], json!("abc")),
            false,
        ),
        (
            "conformance-without-report",
            with(minimal(), &["conformance"], json!({})),
            false,
        ),
        (
            "bad-audit-date",
            with(
                full(),
                &["security", "audits", "0", "date"],
                json!("2026-13-01"),
            ),
            false,
        ),
        (
            "rollover-audit-date",
            with(
                full(),
                &["security", "audits", "0", "date"],
                json!("2026-02-30"),
            ),
            true,
        ),
        (
            "security-without-contact",
            with(
                minimal(),
                &["security"],
                json!({ "policy": "https://x.com" }),
            ),
            false,
        ),
        (
            "init-uses-market-mint",
            with(
                full(),
                &["init", "accounts", "0", "source"],
                json!("mint:0"),
            ),
            false,
        ),
        (
            "init-market-state-itself",
            with(full(), &["init", "market_state"], json!("market")),
            false,
        ),
        (
            "unknown-seed-arg",
            with(
                full(),
                &["init", "market_state"],
                json!("pda:strategy:arg:nope"),
            ),
            false,
        ),
        (
            "nested-pda",
            with(
                full(),
                &["actions", "0", "accounts", "0", "source"],
                json!("ata:pda:strategy,mint:0"),
            ),
            false,
        ),
        (
            "bad-index",
            with(
                full(),
                &["actions", "0", "accounts", "0", "source"],
                json!("vault:4"),
            ),
            false,
        ),
        (
            "bad-fixed",
            with(
                full(),
                &["actions", "0", "accounts", "0", "source"],
                json!("fixed:notanaddress"),
            ),
            false,
        ),
        (
            "long-str-seed",
            with(
                full(),
                &["actions", "0", "accounts", "0", "source"],
                json!(format!("pda:strategy:str:{}", "a".repeat(33))),
            ),
            false,
        ),
        (
            "odd-hex-seed",
            with(
                full(),
                &["actions", "0", "accounts", "0", "source"],
                json!("pda:strategy:hex:abc"),
            ),
            false,
        ),
        (
            "too-many-seeds",
            with(
                full(),
                &["actions", "0", "accounts", "0", "source"],
                json!(format!("pda:strategy:{}", vec!["str:a"; 16].join(","))),
            ),
            false,
        ),
        (
            "unknown-seed-kind",
            with(
                full(),
                &["actions", "0", "accounts", "0", "source"],
                json!("pda:strategy:num:1"),
            ),
            false,
        ),
        (
            "bad-ata",
            with(
                full(),
                &["actions", "0", "accounts", "0", "source"],
                json!("ata:wallet"),
            ),
            false,
        ),
        (
            "unknown-source",
            with(
                full(),
                &["actions", "0", "accounts", "0", "source"],
                json!("treasury"),
            ),
            false,
        ),
        (
            "duplicate-action",
            with(full(), &["actions", "1"], full()["actions"][0].clone()),
            false,
        ),
        (
            "duplicate-arg",
            with(
                full(),
                &["actions", "0", "args", "1", "name"],
                json!("amount_a"),
            ),
            false,
        ),
        (
            "bad-arg-type",
            with(
                full(),
                &["actions", "0", "args", "0", "type"],
                json!("u128"),
            ),
            false,
        ),
        (
            "bad-discriminator",
            with(full(), &["actions", "0", "discriminator"], json!("f223")),
            false,
        ),
        (
            "unknown-action-kind",
            with(full(), &["actions", "0", "kind"], json!("stake")),
            true,
        ),
        (
            "signer-not-boolean",
            with(
                full(),
                &["actions", "0", "accounts", "0", "signer"],
                json!("yes"),
            ),
            false,
        ),
        (
            "action-without-label",
            with(minimal(), &["actions"], json!([{ "name": "go" }])),
            false,
        ),
        (
            "actions-not-array",
            with(minimal(), &["actions"], json!({})),
            false,
        ),
        (
            "init-not-object",
            with(minimal(), &["init"], json!("x")),
            false,
        ),
    ]
}

fn sorted(mut items: Vec<String>) -> Vec<String> {
    items.sort();
    items
}

fn metadata_json((name, document, ok): (&str, Value, bool)) -> Value {
    let result = parse_strategy_metadata(&document);
    assert_eq!(result.is_ok(), ok, "{name}: {result:?}");
    let (errors, warnings) = match result {
        Ok(warnings) => (vec![], warnings),
        Err(errors) => (errors, vec![]),
    };
    json!({
        "name": name,
        "document": document,
        "ok": ok,
        // Sorted, because JSON object key order is not preserved everywhere.
        "errors": sorted(errors),
        "warnings": sorted(warnings),
    })
}

// ---------------------------------------------------------------------------------------
// Conformance reports
// ---------------------------------------------------------------------------------------

/// (name, report body, `conformance.sha256`, deployed hash, expected status and reason).
type ConformanceCase = (
    &'static str,
    Vec<u8>,
    Option<String>,
    Option<&'static str>,
    (facts::ConformanceStatus, Option<&'static str>),
);

fn conformance_cases() -> Vec<ConformanceCase> {
    use facts::ConformanceStatus::*;
    let program = key(69).to_string();
    let deployed = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
    let report = |id: &str, hash: &str| {
        serde_json::to_vec_pretty(&json!({
            "schema": "pipeline-check-report/1",
            "checker_version": "0.1.0",
            "generated_at": 1_794_000_000u64,
            "strategy": { "program_id": id, "executable_sha256": hash, "loader": "bpf-loader-upgradeable" },
            "summary": { "result": "warn", "pass": 12, "warn": 1, "fail": 0, "skip": 1 },
            "checks": [],
        }))
        .unwrap()
    };
    let current = report(&program, deployed);
    let current_sha = facts::evaluate_conformance_report(&current, None, &key(69), None)
        .report_sha256
        .unwrap();
    let mut with_bom = b"\xEF\xBB\xBF".to_vec();
    with_bom.extend_from_slice(&current);
    vec![
        (
            "current",
            current.clone(),
            None,
            Some(deployed),
            (Current, None),
        ),
        (
            "current-pinned",
            current.clone(),
            Some(current_sha.to_uppercase()),
            Some(deployed),
            (Current, None),
        ),
        (
            "current-with-bom",
            with_bom,
            None,
            Some(deployed),
            (Current, None),
        ),
        (
            "pinned-other-body",
            current.clone(),
            Some("0".repeat(64)),
            Some(deployed),
            (Unverified, Some("sha256-mismatch")),
        ),
        (
            "older-code",
            report(&program, &"1".repeat(64)),
            None,
            Some(deployed),
            (Stale, Some("other-code")),
        ),
        (
            "other-program",
            report(&key(70).to_string(), deployed),
            None,
            Some(deployed),
            (Stale, Some("other-program")),
        ),
        (
            "deployed-not-hashed",
            current,
            None,
            None,
            (Unverified, Some("code-not-hashed")),
        ),
        (
            "not-json",
            b"<html>not a report</html>".to_vec(),
            None,
            Some(deployed),
            (Unverified, Some("not-json")),
        ),
        (
            "wrong-schema",
            br#"{"schema":"other/1","strategy":{"program_id":"x","executable_sha256":"y"}}"#
                .to_vec(),
            None,
            Some(deployed),
            (Unverified, Some("not-a-report")),
        ),
        (
            "no-strategy",
            br#"{"schema":"pipeline-check-report/1"}"#.to_vec(),
            None,
            Some(deployed),
            (Unverified, Some("not-a-report")),
        ),
    ]
}

fn conformance_json((name, body, pinned, deployed, expected): ConformanceCase) -> Value {
    let program = key(69);
    let facts = facts::evaluate_conformance_report(&body, pinned.as_deref(), &program, deployed);
    assert_eq!((facts.status, facts.reason), expected, "{name}");
    json!({
        "name": name,
        "body": b64(&body),
        "pinned_sha256": pinned,
        "program_id": program.to_string(),
        "deployed_sha256": deployed,
        "expected": facts.to_json(),
    })
}

fn main() {
    let flags: Vec<Value> = facts::FLAGS
        .iter()
        .map(|(name, description)| json!([name, description]))
        .collect();
    let fixtures = json!({
        "flags": flags,
        "mints": mint_cases().iter().map(mint_json).collect::<Vec<_>>(),
        "programs": program_cases().iter().map(program_json).collect::<Vec<_>>(),
        "metadata": metadata_cases().into_iter().map(metadata_json).collect::<Vec<_>>(),
        "conformance": conformance_cases().into_iter().map(conformance_json).collect::<Vec<_>>(),
    });
    println!("{}", serde_json::to_string_pretty(&fixtures).unwrap());
}
