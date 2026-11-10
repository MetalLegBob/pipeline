# my-strategy: a Pipeline strategy

A Solana program that holds liquidity and sets prices, listed on
[Pipeline](https://github.com/MetalLegBob/pipeline). Pipeline's router sends trades to it,
checks the trader gets at least their minimum, charges Pipeline's 0.25% fee and publishes a
standard swap event, so every market you create is quotable, tradable and chartable by
anything built on Pipeline, with no integration work per market.

It starts as a working constant-product pool. Change the pricing, keep the plumbing.

```
programs/my-strategy/
  src/pricing.rs      the pricing: the file to change
  src/lib.rs          Pipeline's five instructions, the pool account and a deposit action
  tests/strategy.rs   LiteSVM tests through the real router, including pipeline-check
  tests/common/       test setup: the router, your program, mints, markets, swaps
metadata.json         what desks show and how they create markets and call deposit
scripts/
  fetch-router.sh     copies the devnet router into the tests' fixtures
  deploy-devnet.ts    registers the strategy, creates a market, adds liquidity, swaps
```

## The 10-minute path

You need Rust ([rustup](https://rustup.rs); `rust-toolchain.toml` picks the version),
`cargo-build-sbf` 4.4 or later (`cargo install cargo-build-sbf --version 4.6.0 --locked`),
the Solana CLI 4.4, and Node 22.18 or later.

### 1. Generate your project

From a clone of the Pipeline repository:

```sh
git clone https://github.com/MetalLegBob/pipeline && cd pipeline
scripts/new-strategy.sh my-amm ~/code/my-amm
```

This copies the template, renames it, creates your program's keypair at
`~/.config/pipeline/keys/my-amm.json` and writes its address into `declare_id!` and the
scripts. The keypair decides your program's address: keep it out of the project and back it
up.

### 2. Change the pricing

Edit `quote` in `programs/my-strategy/src/pricing.rs`. Both `pipeline_quote` and
`pipeline_swap` call it, which keeps quotes and swaps identical. Its comment lists what it
must guarantee. Add any state it needs to `Pool` in `src/lib.rs`.

### 3. Build and test

```sh
scripts/fetch-router.sh     # once, and again when the router is upgraded
cargo build-sbf --manifest-path programs/my-strategy/Cargo.toml   # target/deploy/my_strategy.so
cargo test
```

The tests create a market through the router's `create_market`, add fee accounts, deposit,
quote, swap through the router, and then run every
[`pipeline-check`](https://github.com/MetalLegBob/pipeline/tree/main/crates/pipeline-check)
check on the market: quotes match swaps in every direction and size, minimums are enforced,
zero input buys nothing, swaps fit in a transaction and in the compute budget, and more. A
failing check prints the whole report. Run them after every pricing change.

### 4. Deploy to devnet

```sh
solana config set --url devnet
solana airdrop 2              # twice, or use https://faucet.solana.com
solana program deploy target/deploy/my_strategy.so \
  --program-id ~/.config/pipeline/keys/my-strategy.json
```

Deploying needs about 2.5 devnet SOL while it runs: about 1.1 SOL stays as the program's
rent and the upload buffer's share comes back. Always pass `--program-id`:
`cargo build-sbf` also writes a `target/deploy/my_strategy-keypair.json`, which is not
your program's key. Your wallet (`~/.config/solana/id.json`) becomes the program's upgrade
authority. Upgrade later with the same command; your Pipeline registration and markets
carry over.

### 5. Publish your metadata

Fill in `metadata.json`: name, description, image, links and a security contact. Publish it
at an `https://`, `ipfs://` or `ar://` URL. The raw URL of the file in your public Git
repository works.

### 6. Register, create a market and trade

```sh
cd scripts && npm install
node deploy-devnet.ts https://<where you published>/metadata.json
```

Step by step, the script:

1. checks the program is deployed and your wallet is its upgrade authority;
2. checks `metadata.json`, and the published copy;
3. registers the strategy with `register_strategy`, paying the one-time 0.01 SOL listing fee
   (or, if it is already registered, points it at the new metadata URL with
   `update_metadata`);
4. creates two test tokens;
5. creates a market and records it with the router, plus a fee account for each token, in
   one transaction (`create_market` + `init_fee_account`, built from your metadata's `init`);
6. adds liquidity with the `deposit` action described in your metadata;
7. quotes a swap and swaps through the router.

Your market is then live: anything built on Pipeline can find, quote and trade it. The SDK
isn't on npm yet, so `scripts/package.json` points at the SDK in your clone of the Pipeline
repository.

### 7. Check it and publish the report

```sh
cargo install --git https://github.com/MetalLegBob/pipeline pipeline-check --locked
pipeline-check --url devnet --market <market address printed by the script>
pipeline-check --url devnet --market <market address> --json > report.json
shasum -a 256 report.json
```

`pipeline-check` clones the market from devnet into a local VM, read-only, and runs the same
checks as your tests. Publish `report.json` next to your metadata and add it:

```json
"conformance": {
    "report": "https://<where you published>/report.json",
    "sha256": "<the hash>"
}
```

Desks can show the report; it is a signal you choose to publish, never an admission gate.
If the metadata URL changed, run `deploy-devnet.ts` again with the new URL (it also creates
another test market), or send `update_metadata` yourself.

## Rules that matter

The router protects traders whatever your program does, but quoting and trading only work
if the strategy follows these. Each is marked in `src/lib.rs`; the Pipeline repository's
`docs/STRATEGY-GUIDE.md` explains them all.

- **Same code for quote and swap.** `pipeline_swap` measures the input, calls
  `pricing::quote`, then settles. Never price a swap any other way.
- **Credit input from the vault with `credited_input`.** Price what arrived in the vault
  beyond your recorded reserve, capped at what the router sent. That handles Token-2022
  transfer fees and stops tokens sent to the vault beforehand from counting as fee-free
  input.
- **Check the market signer.** `pipeline_swap` only accepts the router's market PDA stored
  at creation, so every trade pays Pipeline's fee and appears in its events. Remove the
  check if you want direct calls too (they pay no Pipeline fee).
- **Check the market state's type.** `Account<Pool>` checks the owner and the account type
  in every instruction. That matters most in `pipeline_market_info`: anyone can ask the
  router to record any account your program owns.
- **List every account a swap needs** in `pipeline_resolve_accounts`.
- **Refuse to price an empty side**, and make the first deposit add both tokens.
- **Be deterministic** and keep quotes under 200,000 compute units, the quoter's default
  cap.

## Changing more than the pricing

- **More state:** add fields to `Pool` (and to `InitParams` if creators choose them). If
  creators choose a parameter, put it in the pool's seeds too, as `fee_bps` is, so a pool
  with bad parameters can't take a good pool's address. Update `metadata.json`'s `init` to
  match.
- **More accounts per swap:** return them from `pipeline_resolve_accounts`; they arrive in
  `pipeline_swap` as remaining accounts in that order.
- **No exact-out:** set `SUPPORTS_EXACT_OUT` to `false` in `pricing.rs` and return an error
  for exact-out trades.
- **More actions:** a withdrawal, claims or keeper jobs are ordinary instructions that users
  call directly. They pay no Pipeline fee. Describe each in `metadata.json`'s `actions` so
  desks can show a form for it. This starter pool has no LP shares and no withdrawal:
  add them before real users deposit.
- **Launchpads graduating tokens into your pools:** implement the optional
  `pipeline_seed_liquidity` (guide section 9; `programs/test-strategy-cp` in the Pipeline
  repository has an example).

Keep `programs/my-strategy/tests/common/mod.rs` (the deposit's accounts and the pool's
address) and `metadata.json` in step with the program.

## Dependencies

The Pipeline crates come from crates.io (`pipeline-strategy-interface`, `pipeline-client`,
`pipeline-check`, version `0.1`), set once in the root `Cargo.toml`, and the scripts use
`@pipeline-sol/sdk` from npm. Commit `Cargo.lock` and `scripts/package-lock.json` to keep
builds reproducible.
