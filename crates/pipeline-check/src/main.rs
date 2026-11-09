//! `pipeline-check`: clones a Pipeline market from a cluster into a local VM, using
//! read-only RPC calls, and runs the strategy checks against it. Nothing is ever sent
//! to the cluster.

use std::process::ExitCode;

use pipeline_check::rpc::{cluster_url, RpcSource};
use pipeline_check::{check_market_with_source, clone_market, CheckOptions, Status};
use pipeline_client::Pubkey;

const USAGE: &str = "\
Usage:
  pipeline-check --url <cluster> --market <address> [options]
  pipeline-check --url <cluster> --list-markets

Clones a Pipeline market into a local VM with read-only RPC calls and checks that its
strategy quotes, trades and settles correctly through the router. Nothing is sent to
the cluster.

Options:
  --url <cluster>        devnet, mainnet, testnet, localnet or an http(s) RPC URL
  --market <address>     the router's Market account (not the strategy's market state)
  --router-so <path>     use a local router build instead of the cluster's router
  --amounts <a,b,...>    exact-in sizes in smallest units, used in every direction
  --extra <hex>          strategy-specific TradeRequest::extra bytes
  --quote-compute <n>    compute cap for quotes (default 200000, as integrators use)
  --json                 print the JSON report instead of text
  --fail-on-warn         exit 1 on warnings as well as failures
  --list-markets         list the router's markets and exit
  -h, --help             show this help

Exit status: 0 when no check fails, 1 when one does, 2 when the market can't be checked.";

struct Args {
    url: String,
    market: Option<Pubkey>,
    router_so: Option<String>,
    json: bool,
    fail_on_warn: bool,
    list_markets: bool,
    options: CheckOptions,
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let mut parsed = Args {
        url: String::new(),
        market: None,
        router_so: None,
        json: false,
        fail_on_warn: false,
        list_markets: false,
        options: CheckOptions::default(),
    };
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--url" | "-u" => parsed.url = cluster_url(&value("--url")?)?,
            "--market" | "-m" => {
                let text = value("--market")?;
                parsed.market = Some(
                    text.parse()
                        .map_err(|_| format!("--market: {text:?} is not an address"))?,
                );
            }
            "--router-so" => parsed.router_so = Some(value("--router-so")?),
            "--amounts" => {
                let text = value("--amounts")?;
                let amounts = text
                    .split(',')
                    .map(|a| a.trim().replace('_', "").parse::<u64>())
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| format!("--amounts: {text:?} is not a list of integers"))?;
                parsed.options.amounts = Some(amounts);
            }
            "--extra" => {
                let text = value("--extra")?;
                parsed.options.extra =
                    parse_hex(&text).ok_or_else(|| format!("--extra: {text:?} is not hex"))?;
            }
            "--quote-compute" => {
                let text = value("--quote-compute")?;
                parsed.options.quote_compute_limit = text
                    .parse()
                    .map_err(|_| format!("--quote-compute: {text:?} is not a number"))?;
            }
            "--json" => parsed.json = true,
            "--fail-on-warn" => parsed.fail_on_warn = true,
            "--list-markets" => parsed.list_markets = true,
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    if parsed.url.is_empty() {
        return Err("--url is required".into());
    }
    if parsed.market.is_none() && !parsed.list_markets {
        return Err("--market is required".into());
    }
    Ok(parsed)
}

fn parse_hex(text: &str) -> Option<Vec<u8>> {
    let text = text.strip_prefix("0x").unwrap_or(text);
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(error) => {
            if error.is_empty() {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            eprintln!("error: {error}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let mut source = match RpcSource::new(&args.url) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::from(2);
        }
    };

    if args.list_markets {
        return match source.markets() {
            Ok(markets) => {
                for (address, market) in markets {
                    let mints: Vec<String> = market.mints().iter().map(|m| m.to_string()).collect();
                    println!(
                        "{address}  strategy {}  mints {}",
                        market.strategy_program,
                        mints.join(",")
                    );
                }
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("error: {error}");
                ExitCode::from(2)
            }
        };
    }

    let router_elf = match &args.router_so {
        Some(path) => match std::fs::read(path) {
            Ok(elf) => Some(elf),
            Err(error) => {
                eprintln!("error: cannot read {path}: {error}");
                return ExitCode::from(2);
            }
        },
        None => None,
    };
    let market = args.market.expect("checked in parse_args");
    eprintln!("Cloning market {market} from {} (read-only)...", args.url);
    let cloned = match clone_market(&mut source, market, router_elf.as_deref(), &args.options) {
        Ok(cloned) => cloned,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::from(2);
        }
    };
    eprintln!(
        "Cloned {} accounts in {} RPC requests at slots {}–{}.",
        cloned.accounts_cloned, source.requests, cloned.cluster.slot, source.highest_slot
    );
    for note in &cloned.notes {
        eprintln!("note: {note}");
    }

    let report = check_market_with_source(&cloned.svm, market, &args.options, cloned.source());
    if args.json {
        println!("{}", report.to_json());
    } else {
        print!("{}", report.to_text());
    }
    let failing = match report.summary.result {
        Status::Fail => true,
        Status::Warn => args.fail_on_warn,
        _ => false,
    };
    if failing {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}
