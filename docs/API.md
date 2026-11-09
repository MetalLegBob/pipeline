# Pipeline API

The Pipeline API is a hosted back end for desks, terminals, bots and chart sites, so they don't each need their own indexer. It:

- indexes every Pipeline strategy, market and swap;
- serves lists, trade history and price candles, with trust facts and filters;
- quotes by running the strategy's own code;
- builds ready-to-sign swap transactions;
- streams live events.

It holds no keys or funds, and everything it serves can be rebuilt from the chain. Everything here can also be done directly with the TypeScript SDK or the Rust crates.

**Hosted devnet API:** <https://pipeline-devnet.up.railway.app>

There are no API keys. Limits are per IP:

- **Reads:** 20 requests per second, with bursts of up to 100.
- **Quotes and swap building:** 5 per second, with bursts of up to 20.

Going over a limit returns `429` with a `retry-after` header.

**Conventions.**

- Token amounts are strings of integers in the token's smallest units, because u64 values don't fit in a JavaScript number.
- Addresses are base58.
- Times are RFC 3339 in responses. As query parameters, they are Unix seconds.
- Only finalized transactions are indexed. Under Alpenglow, finality takes about 150 ms.

## Endpoints

| Endpoint | Returns |
|---|---|
| `GET /health` | Router address, `cluster`, the cluster's finalized slot, and the slot and time of the last indexed router transaction |
| `GET /v1/flags` | Every trust flag with its meaning (see [Trust facts and flags](#trust-facts-and-flags)) |
| `GET /v1/strategies?exclude=&require=` | Registered strategies: program ID, metadata URL, fetched metadata and its status, listing fee paid, registration time, market count, upgrade authority (`upgradeable: false` means frozen), last deploy slot, program facts, code hash, verified-build record and `flags`. `exclude` and `require` filter on flags as for markets |
| `GET /v1/strategies/{program}` | One strategy |
| `GET /v1/markets?mint=&strategy=&sort=&limit=&offset=&exclude=&require=` | Markets, each with its strategy, mints, vaults, vault balances and `flags`. Every mint lists its decimals, token program, mint and freeze authorities, supply, Token-2022 extensions and flags. Each market also has 24-hour stats: trade count, volume per input mint and last trade time. `sort` is `trades_24h` (default) or `newest`. `limit` is at most 500. `exclude` and `require` are comma-separated flags: only markets with none of the `exclude` flags and all of the `require` flags are listed. Unknown flags are refused with 400, listing the valid ones |
| `GET /v1/markets/{market}` | One market, with its strategy, fee authority and fee accounts |
| `GET /v1/markets/{market}/trades?before_slot=&limit=` | Swaps, newest first (`limit` at most 1000). To page back, pass the oldest `slot` you have as `before_slot` |
| `GET /v1/traders/{address}/trades?before_slot=&limit=` | One trader's swaps on every market, newest first, each with its `market`. Paged like market trades |
| `GET /v1/markets/{market}/candles?interval=&from=&to=&base=&quote=` | OHLCV candles built from swaps. Intervals are `1m`, `5m`, `15m`, `1h` (default), `4h` and `1d`. The default range is the last 500 intervals, and the most is 5000. Prices are in `quote` per `base`, after decimals. `base` defaults to the market's first mint, and `quote` to the other mint of a two-mint market |
| `GET /v1/mints/{mint}` | A mint's details, facts and flags, and the markets that trade it |
| `GET /v1/quote?market=&input_mint=&output_mint=&amount=&mode=&trader=&integrator_fee_bps=` | A quote (see below) |
| `POST /v1/swap-transaction` | An unsigned swap transaction (see below) |
| `WS /v1/stream?types=&markets=&mints=` | Live events (see below) |

The hosted instance may hide strategies whose metadata is offensive or illegal. Hidden strategies are left out of lists unless you pass `include_hidden=true`, and every strategy shows its `hidden` flag.

### Trust facts and flags

Pipeline admits any strategy, so the API shows neutral facts and never judges, scores or hides on them. Each fact has a machine-readable flag, with the same names and meanings as the TypeScript SDK's `FLAGS` and `FLAG_DESCRIPTIONS` (`GET /v1/flags` lists them). The Rust crate `pipeline-client` (`facts` and `metadata` modules) and the SDK decode the same bytes to the same flags; shared test vectors in `crates/pipeline-client/tests/fixtures/trust-facts.json` keep them in step.

- **Strategy** (`flags` on each strategy):
  - program: `upgradeable`, `frozen`, `program-missing`. `program` holds the loader, whether code is deployed, the upgrade authority, ProgramData address and last deploy slot. `executable_sha256` is the SHA-256 of the deployed code with trailing zeros removed, as `solana-verify get-program-hash` prints it;
  - metadata: `metadata-missing` (empty URI), `metadata-unreachable` (unsupported URI, network error, timeout, HTTP error or over 64 KiB), `metadata-invalid` (not JSON, or not valid [strategy metadata version 1](STRATEGY-GUIDE.md)), `conformance-report` (valid metadata linking a conformance report, which the API does not check). `metadata_status` is `missing`, `not-fetched`, `unreachable`, `invalid` or `valid`, with `metadata_errors`, `metadata_warnings` and `metadata_url` (the URL fetched). `metadata` is the document as published, valid or not;
  - verified build (mainnet only): `verified-build` when OtterSec's verified-builds API reports the program verified and its `on_chain_hash` equals our `executable_sha256`; otherwise `unverified-build`. Neither is set when the API can't be reached or off mainnet, where `verified_build.status` is `not-applicable`.
- **Mint** (`flags` on each mint): `token-2022`, `mint-authority`, `freeze-authority`, `transfer-hook`, `transfer-hook-authority`, `transfer-fee` (a non-zero fee in force at the current epoch, or scheduled), `transfer-fee-authority`, `permanent-delegate`, `non-transferable`, `default-frozen`, `pausable`, `paused`, `closable-mint`, `interest-bearing`, `scaled-ui-amount`, `confidential-transfers`, `permissioned-burn`, `unknown-extension`, and `mint-unreadable` for an account that is missing or not a mint (`readable: false`). `extensions` holds the decoded Token-2022 extensions by name (transfer fee schedule, hook program, token metadata name, symbol and URI, and so on) and `extension_types` their type numbers.
- **Market**: its strategy's flags plus every mint's. A market whose facts haven't been read yet (for up to one refresh cycle after it is created) has none, and a mint not read yet has `flags: null`.

For example, `/v1/markets?exclude=permanent-delegate,freeze-authority` lists markets where no mint has a permanent delegate or freeze authority, and `/v1/markets?require=frozen` only markets whose strategy can no longer be upgraded.

### Quotes

`mode` is `exact_in` (the default) or `exact_out`.

- **Exact-in:** `amount` is what the trader sends to the strategy. Pipeline's fee and any integrator fee come on top.
- **Exact-out:** `amount` is what you want to receive.

The API loads the strategy's deployed program, fetches the market's current accounts plus any accounts the strategy asks for, and runs the strategy's quote at the cluster's current slot. Results match the chain exactly when the state hasn't changed: on devnet, a 2,000,000-unit swap was quoted at 3,987,840 out and received 3,987,840.

One quote may load at most 8 MiB of account data (the market's accounts plus the strategy's), and fetching the strategy's accounts may take at most 5 seconds. Quotes over either limit fail with 422 and the reason. Build trades on markets that need more with the SDK.

```json
{
  "mode": "exact_in",
  "amount_in": "2000000",
  "net_sent": "2000000",
  "pipeline_fee": "5000",
  "integrator_fee": "0",
  "total_spend": "2005000",
  "amount_out": "3987840",
  "strategy_amount_out": "3987840",
  "compute_units": 4841,
  "slot": 519763335,
  "strategy_accounts": [{ "address": "4V34…", "writable": true, "signer": false }]
}
```

`strategy_accounts` are the extra accounts the swap needs, in order. With them and the SDK, a client can build the swap itself.

### Swap transactions

`POST /v1/swap-transaction` takes the quote parameters as JSON, plus four more:

- `trader` (required);
- `slippage_bps` (default 50, which is 0.5%);
- `integrator_fee_account` (optional);
- `wrap_sol` (default `true`).

It returns the quote, `min_amount_out`, the individual `instructions`, and `transaction`. That is an unsigned legacy transaction in base64 with a recent blockhash, paid for by the trader. The trader signs it and sends it. The transaction does the following, in order:

1. Creates the trader's output token account, if it doesn't exist yet.
2. **If the input is SOL:** wraps enough of the trader's SOL to cover the most the swap can spend, including fees.
3. Creates the market's Pipeline fee account for the input mint, if it doesn't exist yet.
4. Swaps. For exact-in, the minimum received is the quote less the slippage tolerance. For exact-out, the maximum spend is the quote plus the slippage tolerance, and the strategy refunds anything unused.
5. **If SOL is on either side:** closes the temporary wrapped-SOL account. Any refund and all SOL output come back as plain SOL.

So traders only ever deal in SOL, never wrapped SOL. Closing the account also unwraps any wrapped SOL the trader already held in it. To trade from an existing wrapped-SOL balance instead, pass `wrap_sol: false`.

There are two limits in this version:

- For tokens other than SOL, the trader must already hold the input token in their associated token account.
- Input mints with transfer hooks are refused, because the hook's accounts must be resolved by the client. Build those swaps with the SDK.

Amounts so large that the most the swap can spend, with slippage and fees, does not fit in a u64 are refused with 422.

The same transaction builder is available to Rust clients as `pipeline_client::wallet::swap_with_setup`.


### Live stream

Connect a WebSocket to `/v1/stream`. Each message is a JSON object with a `type`:

- `swap`: market, trader, mints, `net_sent`, `received`, fees, signature, slot and block time.
- `market_created`: market, strategy and mints.
- `strategy_registered`: strategy.
- `lagged`: you were too slow and missed `missed` events. Re-read the REST endpoints to catch up.

You can filter with query parameters. Each takes a comma-separated list:

- `types`
- `markets`
- `mints`: matches swaps and new markets involving any of the listed mints.

For example, `/v1/stream?types=swap&mints=<mint>` streams every trade of a token, on every market.

## Freshness

New router transactions are indexed within a few seconds of finality. Vault balances refresh every 30 seconds; mint facts and strategy program facts every 5 minutes; strategy metadata every hour (every 5 minutes while unreachable, and soon after its URI changes). A market created in the last few minutes may not have its flags yet.

Metadata is fetched over https only (`ipfs://` and `ar://` through public gateways), with a 64 KiB cap and a timeout, and only from public internet addresses. It is shown as published, unvetted.
