# Integrating Pipeline (desks, terminals, bots, indexers)

Integrate once and every Pipeline market, including ones created after you integrate, is discoverable, quotable, tradable and chartable with no per-market or per-strategy code.

Libraries:

| Library | Language | Purpose |
|---|---|---|
| `pipeline-client` | Rust | Addresses, instruction builders, account decoders |
| `pipeline-events` | Rust | Event decoding |
| `pipeline-quoter` | Rust | Embedded-VM quoting |
| `sdk/ts` | TypeScript | The same for TypeScript |

All are Apache-2.0, on crates.io (`cargo add pipeline-client`) and npm (`npm install @pipeline-sol/sdk`). Router program: `PiPEf4fFBpYXCtgfiWgVWzZNXnq2wZ7bnXidJCTwf34`.

## 1. Discover strategies and markets

- **Accounts:** `getProgramAccounts` on the router, filtered by the 8-byte Anchor discriminator:
  - `Strategy`: `sha256("account:Strategy")[..8]`. Holds the program ID and metadata URL.
  - `Market`: `sha256("account:Market")[..8]`. Holds the strategy, market state, mints, vaults and exact-out support.

  Decoders: `pipeline_client::decode_strategy` / `decode_market`, or the TypeScript SDK.
- **Events:** subscribe to the router and decode `MarketCreated` and `StrategyRegistered` (section 4) to learn about new ones as they appear.
- **Metadata:** fetch the strategy's metadata URL for its name, image, links and custom-action forms (see `docs/STRATEGY-GUIDE.md` section 7). Treat metadata as untrusted user content. In TypeScript, `fetchStrategyMetadata` fetches and validates it (https, IPFS and Arweave, with size and time limits).
- **Custom actions without per-strategy code:** metadata describes each action (add liquidity, claim, …) as typed arguments plus account sources. `buildActionInstruction` turns an action, a market and the user's inputs into a ready instruction, and `buildCreateMarketFromMetadata` does the same for creating a market, so one generic form works for every strategy that publishes metadata. The instruction always targets the strategy's own program, but signing it means trusting that strategy with the accounts the action lists, as with any app; show the user which accounts it writes to.

## 2. Quote

Three options, from fastest to simplest:

1. **`pipeline-quoter`, embedded.**
   - Load each strategy's program (from its ProgramData account) and keep the market's accounts updated from your stream.
   - `Quoter::quote` runs the strategy's real `pipeline_quote` in a sandboxed VM and applies Pipeline's fee, your integrator fee and Token-2022 transfer fees: about 10–20 µs for simple strategies.
   - `Quoter::simulate_swap` simulates the whole router swap for exact numbers with any token behaviour.
   - Reload a program when its ProgramData changes, since strategies can be upgraded.
2. **The Pipeline API's quote endpoint** ([`API.md`](API.md)), hosted on devnet at <https://pipeline-devnet.up.railway.app>. It also serves market lists with trust flags and server-side filters (`/v1/markets?exclude=…`), trades, candles, trader history and a live stream.
3. **RPC simulation** of `pipeline_quote` (TypeScript SDK `quoteStrategy`): no infrastructure, 50–300 ms.

**Before quoting a new market, resolve its accounts** with `pipeline_resolve_accounts` (the quoter's `resolve_accounts`, or the SDK). This tells you which strategy-specific accounts to cache and to include in swaps.

**Keep the quoter's clock current** with `set_clock(slot, epoch, timestamp)`; the epoch decides which Token-2022 transfer fee applies.

**Pass the trader** if you know them. Some strategies price by trader, and anonymous quotes may differ from what the trader actually gets.

## 3. Build a swap

Use `pipeline_client::swap` (or the SDK) with:

- the trader, market state, the trader's input and output token accounts, the market's input vault (`Market::vaults`), both mints and their token programs;
- **strategy accounts**: the result of account resolution, then any transfer-hook accounts for the **output** mint;
- **input hook accounts**: the transfer-hook accounts for the **input** mint, if it has a hook;
- optionally, your integrator fee account (a token account in the input mint) and fee rate.

Arguments:

| Mode | `amount_in` | `amount_out` |
|---|---|---|
| Exact-in | The amount sent to the strategy | Ignored |
| Exact-out (if the market supports it) | The most to send | The strategy-level output from the quote |

**`min_amount_out` is always what must arrive in the trader's output account.** Set it from the quote minus your slippage tolerance. The router enforces it independently of the strategy.

**What the trader pays:** the router charges `ceil(net_sent × 0.25%)` plus your fee on top of the input actually kept by the strategy. The most a swap can cost is `pipeline_strategy_interface::fees::max_total_spend(amount_in, integrator_fee_bps)`. To spend at most a budget, use `fees::amount_for_budget`.

**Multi-hop:** put several `swap` instructions in one transaction. Each hop pays 0.25%. **Give every hop its own `min_amount_out`** (its quote minus your slippage), and **size each later hop from the previous hop's minimum**, not its quote: `amount_in = amount_for_budget(previous min_amount_out, bps)`. Put the trader's end-to-end minimum on the last hop. A later hop spends its `amount_in` from the trader's account whatever the earlier hop delivered, so sizing it from the guaranteed minimum means it can never dip into intermediate tokens the trader already held, even if an earlier hop is sandwiched.

**Before the first swap that sells a mint in a market,** that market's fee account for the mint must exist. Add `init_fee_account` (idempotent) to the transaction if needed.

**Native SOL:** wrap into a WSOL token account before the swap and close it after, as usual.

## 4. Index trades and build charts

Every swap emits one `SwapEvent` as a router self-CPI, so it is not lost to log truncation.

- **Find it:** the event appears in the transaction's inner instructions, as an instruction to the router whose data starts with Anchor's event tag. Decode it with `pipeline_events::decode`.
- **Fields:** market, strategy, trader, input and output mints, mode, `net_sent` (input the swap was charged for, excluding fees), `received` (output that arrived), `pipeline_fee`, the integrator account and fee, slot and timestamp.
- **Price** is `received / net_sent` in the mints' units. Adjust for decimals; Token-2022 transfer fees are already reflected in both numbers.
- **Volume** is `net_sent` in the input mint.
- **`integrator`** is whatever account the trader passed. A non-zero `integrator_fee` means that amount was transferred to it; the field alone proves nothing about who built the trade.
- **When the numbers can differ from what moved:** `net_sent` is what the swap is charged for: the trader's net spend, or what the market's input vault kept if that is more (at most `amount_in`). `received` is what arrived in the trader's output account, so an input mint's transfer hook that tops up that account during the swap inflates it. Both are measured on-chain and can't be faked by the strategy, but a token issuer's hook can move them; treat prices from tokens with hooks accordingly.

The same decoder handles `MarketCreated`, `StrategyRegistered`, `MetadataUpdated`, `FeesSwept` and `ConfigChanged`. Trades made by calling a strategy directly, bypassing Pipeline, do not produce Pipeline events.

## 5. What to show users

Pipeline does not vet strategies, so it's up to front ends what to show and filter. Show
neutral facts and no judgements. The TypeScript SDK collects them for you:

```ts
const market = await inspectMarket(rpc, marketAddress, { cluster: 'mainnet' });
market.flags;          // ['upgradeable', 'verified-build', 'token-2022', 'transfer-fee', …]
const shown = filterMarkets(markets, { exclude: ['permanent-delegate', 'freeze-authority'] });
```

| Facts | Helper | Examples of flags |
|---|---|---|
| The strategy program: upgradeable or frozen, upgrade authority, last deploy, code hash | `inspectProgram` | `upgradeable`, `frozen` |
| Verified build: OtterSec's record matches the deployed code (mainnet only) | `fetchVerifiedBuild` | `verified-build`, `unverified-build` |
| The strategy's metadata: present, reachable, valid; whether its linked conformance report is for the deployed code | `inspectStrategy` | `metadata-missing`, `metadata-invalid`, `conformance-report`, `conformance-stale` |
| Each mint: authorities and Token-2022 extensions | `inspectMint` | `mint-authority`, `freeze-authority`, `transfer-hook`, `transfer-fee`, `permanent-delegate`, `non-transferable`, `default-frozen`, `pausable` |
| Hosted checks: the latest `pipeline-check` run against the strategy's current code | The hosted API's `checks` on each market, and `/v1/markets/{market}/checks` for the report | `checks-passed`, `checks-failed`, `checks-pending` |

A market's flags are its strategy's flags plus every mint's; the hosted API adds exactly one
`checks-*` flag. The full list, with exact meanings, is `FLAGS` and `FLAG_DESCRIPTIONS` in
the SDK.

**Hosted checks.** The hosted API runs `pipeline-check` on every market: when the market is
created, as soon as its strategy is upgraded, and every 15 minutes. `checks-passed` means the
latest run against the strategy's current code had no Fail (warnings, such as a closed market
refusing quotes, are allowed); `checks-failed` that it had at least one; `checks-pending` that
no run has completed for the current code yet. A pass never carries over to new code: an
upgrade makes the market pending at once. Filter with `/v1/markets?require=checks-passed`.

**Conformance reports.** A strategy can publish its own `pipeline-check` report and link it
from its metadata (`conformance`). The SDK and the hosted API fetch it (1 MiB at most, checked
against `conformance.sha256` when given) and compare its `strategy.program_id` and
`strategy.executable_sha256` with the deployed program: `conformance-report` when they match,
`conformance-stale` when the report is for other code (after an upgrade, say). A report that
can't be fetched or read sets neither, and `conformance.reason` says why. Also worth showing: liquidity
(vault balances), market age and volume.

`inspectProgram` downloads the program's code to hash it (up to a few MB); cache results per
program and pass `hashExecutable: false` when you only need the upgrade status.

## 6. Limits worth knowing

- **Restrictive transfer hooks.** A mint whose hook rejects transfers into the market's fee account cannot be sold through Pipeline, though it can be bought.
- **Transfer-fee input tokens.** Pipeline's fee transfer and later sweeps pay the token's transfer fee like any transfer, so the issuer collects a cut of Pipeline's fee in that token. This is the issuer's power over its own token.
- **Frozen fee accounts.** Mints whose accounts start frozen (Token-2022 DefaultAccountState), or whose freeze authority freezes a market's fee account, cannot be sold through that market.
- **CPI Guard.** Traders with Token-2022 CPI Guard enabled on their input account cannot swap through the router, because it moves their tokens in a cross-program call.
- **Permanent delegates.** A mint's permanent delegate can move Pipeline's collected fees in that mint, so they should be swept often. Traders are protected: the router never lets a swap take more than `amount_in` plus the two fees.
- **Creating markets.** A strategy's setup never receives the creator's signature. It spends from a router payer funded with `max_creator_lamports` (unused SOL is returned), so set that to the rent you expect. Any other account that signs and is empty, unfunded and system-owned keeps its signature for the strategy (so it can create it); a wallet with no SOL, including one emptied earlier in the same transaction, or a program's PDA signing through a CPI, looks the same. Sign `create_market` only with new keypairs.
- **Exact-out with transfer-fee input tokens.** Unused input refunded by the strategy pays the token's transfer fee again, and Pipeline's fee is measured on the trader's real loss. Keep `amount_in` close to the quote for such tokens.
- **Call depth.** Calls nest at most 4 levels. A trader using a smart wallet, combined with a hooked token and a strategy that calls another protocol, can exceed it.
- **Compute.** A typical swap costs 40–55k compute units in total. Strategies vary.
- **Heap with many hook accounts.** With the default 32 KiB heap, a swap handles about 28 transfer-hook accounts; beyond that the router runs out of memory and the whole transaction reverts (nothing moves). Add a ComputeBudget `RequestHeapFrame` instruction (up to 256 KiB) for swaps with very long hook account lists. Plain strategy accounts hit Solana's 64-account limit first.
