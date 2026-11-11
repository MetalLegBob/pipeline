# pipeline-check

Self-service quality checks for a Pipeline strategy market. The checker quotes and trades a
market through the real router in a local Solana VM (LiteSVM) and reports, per check,
**Pass**, **Warn**, **Fail** or **Skip** with a plain-English message and the numbers behind it.

It is a tool for strategy developers and a signal they can publish. It is never an
admission gate: Pipeline lists every registered strategy whatever its report says.

## From your own tests

Set up the router, your strategy and a market with liquidity in LiteSVM the way your tests
already do, then:

```rust
use pipeline_check::{check_market, CheckOptions};

let report = check_market(&svm, market, &CheckOptions::default());
println!("{}", report.to_text());
assert_eq!(report.summary.fail, 0, "{}", report.to_text());
```

`market` is the router's `Market` account (`pipeline_client::pda::market(program, state)`),
not your market state. `check_market` takes `&LiteSVM` and never modifies it: every trade
runs in a clone. It funds its own traders by writing token accounts straight into the clone
(SPL Token, Token-2022 with the extensions the mint requires, and wrapped SOL), so you
don't need a mint authority.

`CheckOptions`:

| Field | Default | Meaning |
|---|---|---|
| `amounts` | `None` | Exact-in sizes (smallest units) to use in every direction instead of sizes derived from vault balances. Use it if your liquidity isn't held in the vaults (virtual reserves, for example) |
| `extra` | empty | `TradeRequest::extra` bytes for every quote and swap |
| `quote_compute_limit` | 200,000 | Compute cap for `pipeline_quote`, as `pipeline-quoter` applies it for integrators |
| `compute_warn_units` | 200,000 | Warn when a router swap uses more |

If you only use the library, `default-features = false` drops the RPC client.

## Against a deployed market

```
pipeline-check --url devnet --list-markets
pipeline-check --url devnet --market <market address> [--json] [--amounts 1000,250000] \
    [--extra <hex>] [--router-so programs/target/deploy/pipeline.so] [--fail-on-warn]
```

`--url` takes `devnet`, `mainnet`, `testnet`, `localnet` or an http(s) RPC URL. The tool
clones everything the market's trades touch into a local VM and runs the same checks there.
**It only reads from the cluster** (`getMultipleAccounts`, `getProgramAccounts`, `getSlot`,
`getEpochInfo`, `getBlockTime`, `getGenesisHash`) and never signs or sends anything.

Exit status: 0 if no check failed, 1 if one did (or warned, with `--fail-on-warn`), 2 if
the market couldn't be checked.

What is cloned: the router program (unless `--router-so` supplies a local build) and its
config; the Market; the strategy's registration, program and ProgramData; the market state,
mints, vaults and fee accounts; each transfer hook's program, extra-account-meta list and
the extra accounts it resolves to; and every account `pipeline_resolve_accounts` lists, round
by round, for every direction and size the checks use, including programs among them. The
VM's clock is set to the cluster's slot, epoch and block time. LiteSVM's own System, SPL
Token, Token-2022, Associated Token and Compute Budget programs and sysvars are used rather
than the cluster's.

Example (devnet, 2026-11-09):

```
Pipeline check 0.1.0
  market    2u5ep2aLFzp2SACkG5ZLUrAkEY7vCzm2vL56Q6YiQxb9
  strategy  9mAV3pMKXx67poy98jHizWM6TEeKJ5g48ZeD83xFa1UF  sha256 062e4c0bf1c8fa0151f2d49599ae577b3fc923affb466f39a05a5fb23e8b1cfa
  router    PiPEf4fFBpYXCtgfiWgVWzZNXnq2wZ7bnXidJCTwf34  sha256 2558d884788d5bb8df3113836dc937858249ed4f4823852b251208f5eb32237e
  source    cloned from devnet at slot 520458820
  mint 0    So11111111111111111111111111111111111111112 (9 decimals), vault holds 624115677
  mint 1    9cHCWqcMhg8sZetwAF4MJJPQqVjqR72U7EhEQTa6H3VK (9 decimals), vault holds 235840688950091

  PASS  market_account      Market of registered strategy 9mAV3pMKXx67poy98jHizWM6TEeKJ5g48ZeD83xFa1UF with 2 mints; router config found.
  PASS  market_info         The strategy reports the same mints, vaults and exact-out support the router recorded.
  PASS  fee_accounts        Every mint has its Pipeline fee account.
  PASS  mint_extensions     No Token-2022 extensions.
  PASS  resolve_accounts    Resolves in at most 1 of 4 rounds in every direction; swaps need up to 4 strategy account(s).
  PASS  transaction_size    The largest swap is 750 of 1232 bytes with 18 accounts, so it fits in a legacy transaction.
  PASS  quotes              All 6 quotes succeeded through pipeline-quoter using only the resolved accounts (up to 40307 compute units each).
  PASS  quote_matches_swap  All 6 router swaps delivered exactly the quoted output and charged exactly the quoted input and fee.
  PASS  min_out_enforced    In 2 direction(s), asking for one unit more than the quote makes the swap fail.
  PASS  determinism         Repeated quotes agree, and two different traders get the anonymous quote.
  PASS  compute             Router swaps used 45484–72772 compute units, of which the strategy used up to 46480.
  SKIP  exact_out_refund    The market does not support exact-out.
  PASS  round_trip          In 1 pair(s), buying and immediately selling back returned less than was put in.
  PASS  dust_and_zero       Zero-input quotes give no output, zero-input swaps are refused, and 1-unit swaps either fail cleanly or deliver what was quoted.

Result: PASS (13 pass, 0 warn, 0 fail, 1 skip)
```

## Published results

- **Hosted runs.** The hosted Pipeline API runs these checks on every market, cloned from the
  cluster exactly as above: as soon as a market is created, as soon as its strategy is
  upgraded, and every 15 minutes. Each market gets one flag: `checks-passed` (the latest run
  against the strategy's current code had no Fail; warnings and skips are allowed),
  `checks-failed` (it had at least one) or `checks-pending` (no run for the current code has
  completed yet: a pass never carries over to new code). The full latest report is at
  `GET /v1/markets/{market}/checks`.
- **Your own report.** Publish `--json` output and link it from your strategy metadata's
  `conformance` (`{ "report": "<url>", "sha256": "<hex of the file>" }`). The TypeScript SDK
  and the hosted API compare its `strategy.program_id` and `strategy.executable_sha256` with
  the deployed program: the strategy gets `conformance-report` when they match and
  `conformance-stale` when they don't. Run the checks again and republish after every
  upgrade, or the report goes stale.

## How the checks trade

- **Directions:** every ordered pair of the market's mints.
- **Sizes:** dust (1 unit); then small, medium and large exact-in sizes of 1/100,000,
  1/1,000 and 1/20 of the input vault's balance (or 0.01, 1 and 100 whole tokens when the
  vault holds under 2,000,000 units); exact-out sizes of 1/1,000 and 1/20 of the output
  vault's balance, if the market supports exact-out.
- **Quotes** go through `pipeline-quoter` in a separate VM that holds only the market's fixed
  accounts and what `pipeline_resolve_accounts` listed, which is exactly what an integrator
  streaming account data has. Quotes include Pipeline's fee and Token-2022 transfer fees.
- **Swaps** go through the router's real `swap`, each in a fresh clone of the market, with a
  1.4M compute limit, by a trader holding exactly the most the swap may cost
  (`fees::max_total_spend`). Traders get fresh random keys on every run, so a strategy can't recognise the checker's traders and treat them specially.
- **Transfer-hook accounts** are resolved from the mint's extra-account-meta list for the
  trader-to-vault and vault-to-trader transfers.

## Checks

Check ids are stable. Fail means quoting or trading through Pipeline misbehaves; Warn means
it works but integrators or traders should know something.

| Id | Passes when | Warn | Fail |
|---|---|---|---|
| `market_account` | The Market decodes, the strategy is registered and deployed, the market state is owned by it, mints are token mints and vaults exist | The router is paused (checks run with the pause lifted locally) | Anything missing; later checks are skipped if the market can't be loaded |
| `market_info` | `pipeline_market_info` (given only the market state) reports the mints, vaults and exact-out flag the router recorded | It fails with only the market state | It reports something different, or malformed data |
| `fee_accounts` | Every mint has its fee account and none is frozen | A fee account is missing (selling that mint fails until anyone sends `init_fee_account`; the checks create it locally and carry on) or frozen | — |
| `mint_extensions` | No extension stops trading (extensions are listed) | Accounts start frozen, the mint is non-transferable, or a hook's program or account list is missing | — |
| `resolve_accounts` | Resolution completes within 4 rounds for every case | It fails for some sizes | It fails for every size in some direction |
| `transaction_size` | One swap plus a compute-limit instruction fits in 1,232 bytes | It needs a v0 transaction with a lookup table | More than 64 accounts, or too big even with a lookup table |
| `quotes` | Every non-dust quote succeeds | Some fail, or the strategy refuses every quote (expected for closed or not-yet-open markets) | Every quote fails and at least one ran out of the quote compute cap or returned malformed data |
| `quote_matches_swap` | Each swap's `received`, `net_sent` and fee from the router, and the trader's balance changes, equal the quote | — | Any difference, or a quoted trade whose swap fails |
| `min_out_enforced` | Swapping with `min_amount_out = quote + 1` fails in every direction | — | It succeeds |
| `determinism` | The same quote twice agrees, and two traders get the anonymous quote. Clock-dependent pricing is noted, not judged | Quotes depend on the trader (allowed) | The same quote in the same state differs |
| `compute` | Router swaps stay under `compute_warn_units` (the strategy's share is shown) | Above it | A swap runs out of the 1.4M maximum |
| `exact_out_refund` | Allowing twice the quoted input, the strategy keeps only what the quote needs and the fee is charged on that | — | It keeps more, or the swap fails |
| `round_trip` | Buying and immediately selling back returns less than was put in | It returns more than was put in (with or without Pipeline's fee, since direct calls skip the fee), or the purchase can't be sold back | — |
| `dust_and_zero` | Zero input quotes no output and can't be swapped; 1-unit swaps fail cleanly or deliver the quote | A 1-unit swap delivers something other than its quote | Output quoted or delivered for zero input |

## Report JSON (`pipeline-check-report/1`)

`report.to_json()` or `--json`. The schema string changes only for incompatible changes;
new fields and new checks may be added within version 1. Addresses are base58 strings.

| Field | Meaning |
|---|---|
| `schema` | `"pipeline-check-report/1"` |
| `checker_version` | This crate's version |
| `generated_at` | Unix seconds when the report was made |
| `source` | `kind` (`"svm"` for a caller's VM, `"rpc"` for a clone), `cluster` (`"devnet"`, `"mainnet-beta"`, `"testnet"` or `null`), `genesis_hash`, `slot`, `unix_timestamp`. The RPC URL is not recorded, since URLs often carry API keys |
| `router`, `strategy` | `program_id`, `loader`, `executable_sha256`, `executable_len`, `upgrade_authority` (`null` if frozen), `deploy_slot` |
| `strategy_metadata_uri` | The registered metadata URL, or `null` |
| `market` | `address`, `market_state`, `supports_exact_out`, and per mint: `address`, `token_program`, `decimals`, `mint_authority`, `freeze_authority`, `extensions`, `vault`, `vault_balance`, `fee_account`, `fee_account_exists` |
| `options` | `amounts`, `extra` (hex), `quote_compute_limit` |
| `summary` | `result` (the most serious status), and counts `pass`, `warn`, `fail`, `skip` |
| `checks` | In the order above: `id`, `title`, `status` (`"pass"`, `"warn"`, `"fail"`, `"skip"`), `message`, `details` (check-specific numbers; informational, fields may be added) |

### `executable_sha256`

SHA-256, as lowercase hex, of the program's executable with **trailing zero bytes removed**:

- upgradeable loader: the ProgramData account's data after its 45-byte header (state tag,
  deploy slot, upgrade authority), which is the ELF plus any zero padding reserved for
  upgrades;
- loader v4: the program account's data after its 48-byte header;
- loaders 1 and 2: the program account's data.

This is meant to follow the convention of `solana-verify get-program-hash` (which also trims
trailing zeros from the ProgramData executable), so it can be compared with a verified build;
for a local `.so` file, hash the file with its trailing zero bytes removed. `executable_len` is the number of
bytes hashed.

## Limits

- Accounts are read at `confirmed` over several requests. Each request asks for at least
  the slot the previous one saw (`minContextSlot`), so the clone never mixes in older state,
  but a busy market can still move a few slots between requests; the CLI prints the range.
- When cloning, hook accounts that depend on the transfer amount are fetched for an amount
  of zero; a hook that derives accounts from the amount may then fail its swaps (a false
  Fail, never a false Pass). Library runs on your own VM aren't affected.
- Trades use fresh traders. Accounts a strategy lists only for a particular trader (a
  per-user PDA, say) are not cloned; for a new trader they wouldn't exist anyway.
- Trader accounts are written as initialized even for mints whose accounts start frozen; the
  market's own accounts keep their real state (and `mint_extensions` warns).
- A hook whose extra accounts depend on the destination gets the list for the main
  transfer; the router passes one list for every transfer of a mint.
- `pipeline_market_info` is called with only the market state.
