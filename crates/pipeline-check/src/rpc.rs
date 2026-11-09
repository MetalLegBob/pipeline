//! A read-only JSON-RPC [`AccountSource`]. It only calls `getMultipleAccounts`,
//! `getProgramAccounts`, `getSlot`, `getEpochInfo`, `getBlockTime` and
//! `getGenesisHash`; it has no way to send a transaction.

use std::time::Duration;

use base64::Engine;
use pipeline_client::{self as client, Pubkey};
use serde_json::{json, Value};
use solana_account::Account;

use crate::clone::{AccountSource, ClusterInfo};

/// `getMultipleAccounts` accepts at most this many addresses per call.
const MAX_ACCOUNTS_PER_CALL: usize = 100;
const ATTEMPTS: u32 = 6;

/// Expands the cluster shorthands `devnet`, `mainnet`/`mainnet-beta`, `testnet` and
/// `localnet`; anything else must be an `http(s)://` URL.
pub fn cluster_url(name: &str) -> Result<String, String> {
    let url = match name {
        "devnet" | "d" => "https://api.devnet.solana.com",
        "mainnet" | "mainnet-beta" | "m" => "https://api.mainnet-beta.solana.com",
        "testnet" | "t" => "https://api.testnet.solana.com",
        "localnet" | "localhost" | "l" => "http://127.0.0.1:8899",
        url if url.starts_with("http://") || url.starts_with("https://") => url,
        other => {
            return Err(format!(
            "unknown cluster {other:?}: use devnet, mainnet, testnet, localnet or an http(s) URL"
        ))
        }
    };
    Ok(url.to_string())
}

pub struct RpcSource {
    url: String,
    http: reqwest::blocking::Client,
    /// Number of RPC requests made.
    pub requests: usize,
    /// The highest slot any account read was served at. Later reads ask for at least
    /// this slot (`minContextSlot`), so the clone never mixes in older state; it can
    /// still span a few slots, which this and `ClusterInfo::slot` bound.
    pub highest_slot: u64,
}

impl RpcSource {
    pub fn new(url: &str) -> Result<Self, String> {
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(RpcSource {
            url: url.to_string(),
            http,
            requests: 0,
            highest_slot: 0,
        })
    }

    /// One JSON-RPC call, retried with backoff on rate limits and transport errors.
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let mut delay = Duration::from_millis(500);
        let mut last_error = String::new();
        for attempt in 0..ATTEMPTS {
            if attempt > 0 {
                std::thread::sleep(delay);
                delay *= 2;
            }
            self.requests += 1;
            let response = match self.http.post(&self.url).json(&body).send() {
                Ok(response) => response,
                Err(error) => {
                    last_error = error.to_string();
                    continue;
                }
            };
            let status = response.status();
            if status.as_u16() == 429 || status.is_server_error() {
                last_error = format!("HTTP {status}");
                continue;
            }
            let value: Value = response
                .json()
                .map_err(|e| format!("{method}: unreadable response: {e}"))?;
            if let Some(error) = value.get("error") {
                return Err(format!("{method}: {error}"));
            }
            return value
                .get("result")
                .cloned()
                .ok_or_else(|| format!("{method}: response has no result"));
        }
        Err(format!("{method}: {last_error}"))
    }

    /// Every Market account the router has recorded, by address.
    pub fn markets(&mut self) -> Result<Vec<(Pubkey, client::Market)>, String> {
        let discriminator = base64::engine::general_purpose::STANDARD
            .encode(client::account_discriminator("Market"));
        let result = self.call(
            "getProgramAccounts",
            json!([
                client::PROGRAM_ID.to_string(),
                {
                    "encoding": "base64",
                    "commitment": "confirmed",
                    "filters": [{ "memcmp": { "offset": 0, "bytes": discriminator, "encoding": "base64" } }],
                }
            ]),
        )?;
        let entries = result
            .as_array()
            .ok_or("getProgramAccounts: expected a list")?;
        let mut markets = vec![];
        for entry in entries {
            let address: Pubkey = entry["pubkey"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .ok_or("getProgramAccounts: bad address")?;
            if let Some(account) = parse_account(&entry["account"])? {
                if let Some(market) = client::decode_market(&account.data) {
                    markets.push((address, market));
                }
            }
        }
        Ok(markets)
    }
}

impl AccountSource for RpcSource {
    fn get_accounts(&mut self, keys: &[Pubkey]) -> Result<Vec<Option<Account>>, String> {
        let mut accounts = Vec::with_capacity(keys.len());
        for chunk in keys.chunks(MAX_ACCOUNTS_PER_CALL) {
            let addresses: Vec<String> = chunk.iter().map(|k| k.to_string()).collect();
            let mut config = json!({ "encoding": "base64", "commitment": "confirmed" });
            if self.highest_slot > 0 {
                config["minContextSlot"] = json!(self.highest_slot);
            }
            let result = self.call("getMultipleAccounts", json!([addresses, config]))?;
            if let Some(slot) = result["context"]["slot"].as_u64() {
                self.highest_slot = self.highest_slot.max(slot);
            }
            let values = result["value"]
                .as_array()
                .ok_or("getMultipleAccounts: expected a list")?;
            if values.len() != chunk.len() {
                return Err("getMultipleAccounts: wrong number of accounts".into());
            }
            for value in values {
                accounts.push(parse_account(value)?);
            }
        }
        Ok(accounts)
    }

    fn cluster_info(&mut self) -> Result<ClusterInfo, String> {
        let slot = self
            .call("getSlot", json!([{ "commitment": "confirmed" }]))?
            .as_u64()
            .ok_or("getSlot: expected a number")?;
        // Account reads start from here.
        self.highest_slot = self.highest_slot.max(slot);
        let epoch = self.call("getEpochInfo", json!([{ "commitment": "confirmed" }]))?["epoch"]
            .as_u64()
            .ok_or("getEpochInfo: no epoch")?;
        // Recent slots may not have a block time yet; fall back to the local clock.
        let unix_timestamp = self
            .call("getBlockTime", json!([slot]))
            .ok()
            .and_then(|v| v.as_i64())
            .unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs() as i64)
            });
        let genesis_hash = self
            .call("getGenesisHash", json!([]))
            .ok()
            .and_then(|v| v.as_str().map(str::to_string));
        Ok(ClusterInfo {
            slot,
            epoch,
            unix_timestamp,
            genesis_hash,
        })
    }
}

fn parse_account(value: &Value) -> Result<Option<Account>, String> {
    if value.is_null() {
        return Ok(None);
    }
    let data = value["data"][0]
        .as_str()
        .ok_or("account has no base64 data")?;
    let data = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| format!("account data is not base64: {e}"))?;
    let owner: Pubkey = value["owner"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .ok_or("account has no owner")?;
    Ok(Some(Account {
        lamports: value["lamports"]
            .as_u64()
            .ok_or("account has no lamports")?,
        data,
        owner,
        executable: value["executable"].as_bool().unwrap_or(false),
        rent_epoch: value["rentEpoch"].as_u64().unwrap_or(0),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rpc_accounts() {
        let value = json!({
            "lamports": 1_461_600,
            "owner": client::TOKEN_PROGRAM_ID.to_string(),
            "data": [base64::engine::general_purpose::STANDARD.encode([1u8, 2, 3]), "base64"],
            "executable": false,
            "rentEpoch": u64::MAX,
            "space": 3,
        });
        let account = parse_account(&value).unwrap().unwrap();
        assert_eq!(account.data, vec![1, 2, 3]);
        assert_eq!(account.owner, client::TOKEN_PROGRAM_ID);
        assert_eq!(account.rent_epoch, u64::MAX);
        assert_eq!(parse_account(&Value::Null).unwrap(), None);
    }

    #[test]
    fn cluster_shorthands() {
        assert_eq!(
            cluster_url("devnet").unwrap(),
            "https://api.devnet.solana.com"
        );
        assert_eq!(cluster_url("http://x:1").unwrap(), "http://x:1");
        assert!(cluster_url("nonsense").is_err());
    }
}
