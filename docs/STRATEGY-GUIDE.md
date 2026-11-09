# Writing a Pipeline strategy

A **strategy** is a Solana program that holds liquidity and decides prices. Pipeline's router sends trades to it, checks the trader got at least their minimum, charges Pipeline's 0.25% fee and publishes a standard swap event. Every market on your strategy is then quotable, tradable and chartable by anything built on Pipeline, with no integration work per market.

Start from the reference implementation: `programs/test-strategy-cp` is a complete constant-product strategy in Anchor. Copy it, change `declare_id!` and the pricing in `Pool::quote`, and keep the structure. `programs/test-strategy-evil` shows the same interface implemented natively without a framework.

## 1. The interface

Implement five instructions. Each instruction's data is an 8-byte discriminator followed by one Borsh-encoded argument. The discriminators equal Anchor's `sha256("global:<name>")[..8]`, so an Anchor function with the same name matches automatically. Types, discriminators and account indices are in the `pipeline-strategy-interface` crate (Apache-2.0).

| Instruction | Argument | Called by | Must |
|---|---|---|---|
| `pipeline_init_market` | `Vec<u8>` (your own setup data) | The router, from `create_market` | Create the market state account (owned by your program) and its vaults |
| `pipeline_market_info` | none | The router, at market creation | Set return data to `MarketInfo { mints, vaults, supports_exact_out }` |
| `pipeline_resolve_accounts` | `ResolveRequest { trade, round }` | Quoters and SDKs, in simulation | Set return data to `ResolveResult { accounts, complete }`: every strategy-specific account a swap needs |
| `pipeline_quote` | `TradeRequest` | Quoters and SDKs, in simulation | Set return data to `QuoteResult { amount_in, amount_out }`, read-only |
| `pipeline_swap` | `SwapRequest { trade, amount_sent, min_amount_out }` | The router, from `swap` | Send output to the trader's output account; optionally refund unused input |

### Fixed accounts

`pipeline_init_market` receives these accounts first, then whatever accounts the creator's transaction passes. The creator's own signature never reaches your program. Other accounts keep their signature only if they are fresh (empty, unfunded), such as a new keypair account you are meant to create. The router can't tell a new keypair from a wallet with no SOL (including one emptied earlier in the same transaction) or a program's PDA signing through a CPI, so anyone creating a market should sign only with new keypairs:

| Index | Account |
|---|---|
| 0 | Market signer: the router's market PDA (signer) |
| 1 | Market state (writable; your program creates it) |
| 2 | Payer (signer, writable): a router PDA funded with the creator's `max_creator_lamports` (plus anything a third party sent it, which also goes back to the creator). Pay rent from it; don't reassign or allocate it, or the router fails the transaction |
| 3 | System program |

`pipeline_market_info` receives the market state first.

`pipeline_swap`, `pipeline_quote` and `pipeline_resolve_accounts` receive these accounts first, then your strategy accounts in the order your `pipeline_resolve_accounts` returned them, then any transfer-hook accounts:

| Index | Account | In `pipeline_swap` |
|---|---|---|
| 0 | Market signer (router market PDA) | Signer |
| 1 | Market state | Writable |
| 2 | Trader's wallet | **Not a signer** |
| 3 | Trader's input token account | Writable (you may refund into it) |
| 4 | Trader's output token account | Writable (you pay into it) |
| 5 | Your input vault for the input mint | Writable; the router has just transferred input here |
| 6 | Input mint | |
| 7 | Output mint | |
| 8 | Input token program | |
| 9 | Output token program | |

In quotes and account resolution, nothing signs and the trader accounts may be placeholders. Don't read them.

### Amounts

Strategy amounts are measured at your vaults:

- **Input** is what arrives in your input vault, after any Token-2022 transfer fee.
- **Output** is what leaves your output vault, before any transfer fee.

Clients convert to the trader's view.

- **Exact-in:** `trade.amount` is what the router sent (`amount_in`, before any transfer fee), the same as `amount_sent`. With a transfer-fee token less arrives, so measure your vault (`credited_input`) and price what actually arrived.
- **Exact-out** (only if `supports_exact_out`): `trade.amount` is the output you must send. You report the input you need and refund the rest.

## 2. Rules

The router does not enforce these, but quoting and trading depend on them.

1. **Quote and swap must agree.** Compute both from the same function. The reference strategy's `pipeline_swap` measures the input, calls the same `Pool::quote` used by `pipeline_quote`, then settles. If they disagree, trades fail the trader's minimum and quoters see the mismatch.
2. **Credit `min(vault − reserve, amount_sent)` as input.** Use `pipeline_strategy_interface::credited_input`. Measuring the vault handles transfer-fee tokens. Capping at `amount_sent` stops a trader from depositing most of their input straight into your vault earlier in the same transaction, then swapping a token amount through Pipeline to dodge the fee and print a fake price. Treat anything beyond the cap as a donation to the pool, as the reference strategy does.
3. **Be deterministic.** Results may depend only on the accounts passed, the instruction data and the Clock and Rent sysvars. Slots are about 200 ms; under Alpenglow, timestamps are set by the block leader within bounds. Neither is precise.
4. **Check the market signer** if you only want trades routed through Pipeline. It must equal `PDA(["market", your_program_id, market_state], PIPELINE_PROGRAM_ID)`. Store it at init, as the reference strategy does. Accepting direct calls is your choice; direct calls pay no Pipeline fee. Also check that the market state really is your market type (for example an Anchor discriminator), because anyone can call `register_existing_market` on any account your program owns.
5. **List every account a swap needs** in `pipeline_resolve_accounts`. If you need to read some accounts before you know the rest, return what you know with `complete: false`. You'll be called again with those accounts appended, up to 4 rounds, about 30 accounts per round.
6. **Forward transfer-hook accounts** to your token transfers. The reference strategy passes every account after its own to `transfer_checked`.
7. **Keep quotes under the quoter's compute cap** (200,000 units by default). Heavier strategies still trade, but can't be quoted off-chain.
8. **Don't let markets be blocked or hijacked.**
   - Create vaults idempotently (`init_if_needed`), because anyone can create an associated token account in advance.
   - If market setup has parameters, include them in the market state's address, so a market with bad parameters can't occupy the address of a good one. The reference strategy keys pools by mints and fee tier.
9. **Refuse to price an empty side.** A pool with zero reserves on either side should fail quotes and swaps, and its first deposit should need both tokens, as the reference strategy does.
10. **Keep a swap's input in the input vault you declared** until the swap ends. The router charges its fee on the larger of what the trader spent and what that vault kept; moving input out of it during the call (combined with something refunding the trader) can only shrink your own market's fee base, and integrators can't see where the input went.

## 3. What your strategy cannot do in a Pipeline swap

- **Take more from the trader.** You never get the trader's signature, and the router fails any swap where the trader parts with more input than `amount_in`, even through a token's permanent delegate. Attempts fail the whole transaction (see `crates/pipeline-tests/tests/adversarial.rs` and `security.rs`).
- **Deliver output without keeping any input.** The router rejects it.
- **Touch Pipeline's fees.** Fee accounts belong to a separate PDA you never receive.
- **Call back into the router.** Solana forbids re-entrancy.
- **Pull a second token or collateral inside a swap.** Use a custom action called directly.

## 4. Solana limits that shape strategy design

- **Accounts must be listed in advance.** Mechanisms that touch an open-ended set of things in one trade, such as matching many orders, paying every holder or scanning history, need to be bounded per trade or handled by keepers across transactions.
- **Call depth is 4 nested levels.** Router → your strategy → token program → transfer hook uses all four. Calling one other protocol from your strategy works with ordinary tokens. Calling something that itself calls further programs fails with hooked tokens, and traders using smart wallets use one more level.
- **Compute.** About 1.3M units remain for your strategy per transaction, and multi-hop routes share one budget. The reference strategy uses about 9–10k units per swap.
- **Write locks.** Every trade writes your market state. Avoid a strategy-wide account written on every trade, because it serialises all your markets.
- **State size.** Accounts max out at 10 MB and return data at 1 KB.

## 5. Custom actions

Anything that isn't a swap, such as adding liquidity, claims or keeper jobs, is an ordinary instruction on your program. Users call it directly; it costs no Pipeline fee and is not routed through the router. Describe custom actions in your metadata so desks can show generic forms (section 7).

## 6. Registering and creating markets

1. Deploy your program with the upgradeable loader.
2. **Register** it with the router's `register_strategy`, signed by the program's upgrade authority. This pays the one-time listing fee (0.01 SOL on devnet). An immutable program registers by signing through a CPI with its own `["pipeline-register"]` PDA (see `forward_to_router` in the hostile test strategy). Upgrades are free; your registration persists.
3. **Create markets through the router, or record them straight after creating them** (below).
4. Point your registration at a **metadata JSON file** (`update_metadata` changes it).

### Every market must be recorded

Registering your program does not make its markets visible. The router, indexers, desks and
quoters only know about markets that have a router `Market` account. A pool your program
creates on its own is invisible to all of them, and can't be traded through Pipeline, until
someone records it. There are two ways to get a market recorded, and both should happen in the
transaction that creates the pool:

- **Create it through the router (recommended).** Anyone calls `create_market` with your
  setup data; the router calls your `pipeline_init_market`, then `pipeline_market_info`, and
  records the market in the same instruction. The creator sets `max_creator_lamports`: the
  router lends exactly that much to your setup through the payer account and returns whatever
  you don't spend. Setup can't touch the creator's wallet or token accounts, so seed liquidity
  with a separate custom action the creator signs directly, in the same transaction.
- **Create it yourself, then record it.** If your program creates pools with its own
  instruction, put the router's `register_existing_market` **in the same transaction, right
  after it**. Your program can't make that call itself through a CPI: the router calls back
  into your program to read `pipeline_market_info`, and Solana doesn't allow a program to be
  re-entered through another program. So it has to be a separate instruction, built by your
  front end or SDK.

Either way, also add `init_fee_account` for each of the market's mints (idempotent; anyone can
pay). A mint can't be sold through Pipeline until its fee account exists. A complete "new
pool" transaction therefore looks like:

```
[create_market]                     or   [your_init_pool, register_existing_market]
[init_fee_account × each mint]           [init_fee_account × each mint]
[your deposit / seed action]             (liquidity, if your init didn't add it)
```

If a market was created without being recorded, anyone can record it later with
`register_existing_market`; nothing is lost except the time it wasn't visible.

Things worth knowing:

- **A fresh signer can be pre-funded.** If someone sends lamports to a new keypair account before the creator's `create_market` lands, it is no longer fresh, the router drops its signature, and setup fails. The creator can retry with another keypair; a market state at a PDA you derive avoids this.
- **The `["pipeline-register"]` PDA is your program's registration authority.** If your program exposes an instruction that signs with it for anyone, anyone can register your program (paying the fee) or change its metadata. Only sign with it deliberately.
- **The router doesn't tell you who created a market.** If a market's address comes only from public seeds (mints, fee tier), whoever creates it first gets it. Put the creator in the seeds if that matters.
- **Anyone can register any account your program owns** with `register_existing_market`, so `pipeline_market_info` must check that the account is really one of your markets (rule 4).

## 7. Metadata JSON (version 1)

Point your registration's `metadata_uri` at a JSON file on IPFS (`ipfs://`), Arweave (`ar://`) or any `https://` URL. Desks use it for names, links, trust facts you declare, and forms for creating markets and calling your custom actions. Treat it as public: anyone can read it, and desks treat it as untrusted.

The JSON Schema is [`sdk/ts/schema/strategy-metadata.v1.json`](../sdk/ts/schema/strategy-metadata.v1.json). The TypeScript SDK reads and uses the file:

| Helper | Does |
|---|---|
| `parseStrategyMetadata(json)` | Validates a document. Returns `{ ok, value, warnings }` or `{ ok: false, errors }` |
| `fetchStrategyMetadata(uri)` | Fetches `https://`, `ipfs://` or `ar://` (64 KiB limit, 10 s timeout), then validates |
| `buildActionInstruction(...)` | Turns one of your `actions` into an instruction to your program |
| `buildCreateMarketFromMetadata(...)` | Turns your `init` into the router's `create_market` |

This example describes the constant-product test strategy (`programs/test-strategy-cp`). The SDK's tests build its `init` and `deposit` from this exact document and check the result against the program's accounts.

```json
{
  "schema": "pipeline-strategy-metadata/1",
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
    "telegram": "https://t.me/example"
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
  "actions": [
    {
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
        { "name": "depositor_b", "source": "wallet_token_account:1", "writable": true },
        { "name": "vault_a", "source": "vault:0", "writable": true },
        { "name": "vault_b", "source": "vault:1", "writable": true },
        { "name": "mint_a", "source": "mint:0" },
        { "name": "mint_b", "source": "mint:1" },
        { "name": "token_program_a", "source": "token_program:0" },
        { "name": "token_program_b", "source": "token_program:1" }
      ]
    }
  ],
  "conformance": {
    "report": "https://example.com/report.json",
    "sha256": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"
  },
  "security": {
    "contact": "security@example.com",
    "policy": "https://example.com/security",
    "audits": [{ "by": "Example Audits", "url": "https://example.com/audit.pdf", "date": "2026-10-01" }]
  }
}
```

### Fields

Only `schema` and `name` are required. Unknown keys are allowed everywhere and ignored, so later additions don't break older readers.

| Field | Meaning |
|---|---|
| `schema` | Always `pipeline-strategy-metadata/1` |
| `name` | Up to 64 characters |
| `description` | Up to 2,000 characters |
| `image` | Logo or icon URL |
| `category` | `amm`, `launchpad`, `order-book`, `auction`, `lbp` or `other` |
| `tags` | Up to 16 free tags, 32 characters each |
| `links` | `website`, `docs`, `source` (a link for people), `idl` (your Anchor IDL or Codama file); social accounts `twitter`, `github`, `discord`, `telegram`, `farcaster`; and any other named URL. A social link on an unexpected site (a `twitter` link not on x.com or twitter.com, say) is accepted with a warning |
| `source` | `{ repository, commit?, path? }`: the code a verified build is made from. `path` is the program's directory |
| `init` | How to create a market (below) |
| `actions` | Your custom instructions (below) |
| `conformance` | `{ report, sha256? }`: a `pipeline-check` JSON report and its SHA-256 in hex |
| `security` | `{ contact, policy?, audits? }`. `contact` is an email address, URL or handle; each audit is `{ by, url?, date? }` with `date` as `YYYY-MM-DD` |

Every URL must start with `https://`, `ipfs://` or `ar://`. Other schemes, including `http://`, make the document invalid.

An unknown `category` or action `kind` is a warning, not an error, so new values can be added later.

### `init`: creating a market

`init` describes the `create_market` call.

| Field | Meaning |
|---|---|
| `args` | Encoded in order with Borsh, with no discriminator, to form `init_data` (the `data` your `pipeline_init_market` receives) |
| `accounts` | Your own setup accounts. They are passed after the router's four fixed accounts (market signer, market state, payer, system program) |
| `market_state` | Where the market state lives. `new_keypair` (the default): the creator generates a keypair, which signs. Or an account source, usually a `pda:` |
| `idl_type` | The IDL type `init_data` decodes to, if you publish an IDL |
| `description` | Shown on the create form |

`init` can't use the sources that need an existing market (`mint:i`, `vault:i`, `token_program:i`, `wallet_token_account:i`). Use `input:` for the mints instead, as in the example. `market_state` can't be `market_state` or `market`.

### `actions`: custom instructions

| Field | Meaning |
|---|---|
| `name` | The instruction's name. For Anchor programs, use the IDL instruction name |
| `label` | Button text, up to 64 characters |
| `kind` | `add_liquidity`, `remove_liquidity`, `claim`, `create_market` or `other` |
| `description` | Shown with the form |
| `discriminator` | 8 bytes as 16 hex digits. Optional: the default is Anchor's `sha256("global:<name>")[..8]` |
| `args` | Encoded in order with Borsh after the discriminator |
| `accounts` | The instruction's accounts, in order |

Users sign actions directly. They don't go through the router and pay no Pipeline fee.

### Arguments

Each argument is `{ name, type, label?, description?, decimals_of? }`. `name` is an identifier (letters, digits, `_`). `decimals_of` is an account source naming the mint whose decimals a form should use for the amount, such as `mint:0`.

| Type | Borsh encoding |
|---|---|
| `u8`, `u16`, `u32`, `u64` | Little-endian unsigned integer |
| `i64` | Little-endian signed integer |
| `bool` | One byte, 0 or 1 |
| `pubkey` | 32 bytes |
| `bytes` | u32 length, then the bytes |
| `string` | u32 length, then UTF-8 |

### Accounts

Each account is `{ name, source, signer?, writable? }`. `signer` and `writable` default to `false`.

| Source | Account |
|---|---|
| `wallet` | The user's wallet. For `init`, the market creator |
| `market_state` | The market's state account |
| `market` | The router's market account |
| `strategy_program` | Your program |
| `system_program` | The system program |
| `associated_token_program` | The associated token account program |
| `mint:i` | The market's mint at index `i` (0–3) |
| `vault:i` | The market's vault at index `i` |
| `token_program:i` | The token program of mint `i` |
| `wallet_token_account:i` | The user's associated token account for mint `i` |
| `fixed:<address>` | A fixed address |
| `input:<name>` | An address the user enters |
| `pda:<program>:<seed>,<seed>…` | A PDA. `<program>` is an address, or `strategy` for your program |
| `ata:<owner>,<mint>` | The associated token account of `<owner>` for `<mint>`; both are sources |
| `token_program_of:<mint>` | The token program that owns the mint given by a source |

A `pda:` seed is one of:

| Seed | Bytes |
|---|---|
| `str:<text>` | UTF-8 text, at most 32 bytes, with no commas |
| `hex:<bytes>` | Raw bytes as hex, at most 32 |
| `key:<source>` | The 32 bytes of another source's address |
| `arg:<name>` | The Borsh encoding of the named argument. The example's `arg:fee_bps` is the two little-endian bytes the test strategy uses as its fee seed |

Sources nested inside `pda:`, `ata:` and `token_program_of:` can't themselves be `pda:` or `ata:` sources. A PDA has at most 15 seeds (Solana allows 16, including the bump the SDK adds).

For `ata:` and `token_program_of:` on a mint that isn't one of the market's, the SDK needs the mint's token program. Pass it in, or give the SDK an RPC client to look it up.

## 8. Testing

Test your strategy against the real router in LiteSVM: `crates/pipeline-tests` shows the setup (`tests/common/mod.rs`) and the kinds of checks worth copying, including quote-versus-execution equivalence (`tests/quoter.rs`). Build programs with `scripts/build-programs.sh`, then run `cargo test`.

## 9. Optional: accepting graduations

A strategy can let another program create and fund a new market in one call. A launchpad can use this to graduate a token straight into your AMM. Implement `pipeline_seed_liquidity` (discriminator `sha256("global:pipeline_seed_liquidity")[..8]`, argument `SeedRequest { amounts, data }`). Types and account indices are in `pipeline_strategy_interface::seed_accounts`.

| Index | Account |
|---|---|
| 0 | Seeder: a PDA of the calling program (signer). It owns the escrows |
| 1 | New market state (writable; you create it) |
| 2 | Payer (signer, writable): a system account with a bounded amount of SOL. Pay rent from it; don't reassign or allocate it, or the caller fails the transaction |
| 3 | System program |
| 4 | LP recipient: owner for any LP tokens or position you issue (ignore it if you have none) |
| 5… | For each mint: mint, the seeder's escrow (writable), token program |
| then | Your own accounts (vaults and so on), then transfer-hook accounts |

**Rules:**

- **Derive the new market state's address from the seeder.** For example, the reference strategy uses `["seeded", seeder, mint_a, mint_b, fee]`, so nobody can create or fund the market first at a bad price.
- **Take at most `amounts[i]` from each escrow**, and open the market at the price those amounts imply. `data` carries your setup parameters, such as a fee tier.
- **Store the router's market signer** for the new market state, as `pipeline_init_market` does.
- **Set return data** to `SeedResult { taken, lp_amount }`. Callers measure their escrows rather than trust it.

The caller then records the market with the router's `register_existing_market`. See `pipeline_seed_liquidity` in `programs/test-strategy-cp` (about 60 lines). Describe your `data` format in your metadata so launchers can fill it in.
