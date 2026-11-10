# Pipeline

Pipeline is a permissionless router for experimental Solana markets. Developers write any
AMM or market mechanism they like as a **strategy program**, register it once, and every
market created from it is immediately discoverable, quotable and tradable through one
router program, with one swap instruction and one swap event. Desks, terminals, bots and
chart sites integrate Pipeline once and see every new market without per-market code.

- **Strategies hold their own liquidity.** Pipeline never custodies pool funds; it moves
  the trader's input into the strategy, calls it, and checks what the trader received.
- **One fee:** 0.25% of each swap's input through Pipeline, measured on-chain. Every hop of
  a multi-hop route pays it. Nothing else is charged.
- **Open listing:** registering a strategy program costs a one-time SOL fee (0.01 SOL on
  devnet). There is no whitelist and no approval step. Markets are free to create.
- **The trader is protected by the router**, not by trusting strategies: the trader never
  signs for the strategy, never pays more than their input plus the stated fees, and gets
  at least their minimum output or the swap fails.

> **Status: live on devnet.** Not on mainnet yet. The router's maths is formally verified
> ([`docs/VERIFICATION.md`](docs/VERIFICATION.md)) and it has been security-audited
> ([`docs/SECURITY-AUDIT.md`](docs/SECURITY-AUDIT.md)); there is no external audit.
> Packages: `@pipeline-sol/sdk` on npm; `pipeline-strategy-interface`, `pipeline-client`,
> `pipeline-events`, `pipeline-quoter` and `pipeline-check` on crates.io.

## Live on devnet

| | Address |
|---|---|
| Router | [`PiPEf4fFBpYXCtgfiWgVWzZNXnq2wZ7bnXidJCTwf34`](https://orbmarkets.io/address/PiPEf4fFBpYXCtgfiWgVWzZNXnq2wZ7bnXidJCTwf34/history?cluster=devnet) |
| Hosted API | <https://pipeline-devnet.up.railway.app> ([`docs/API.md`](docs/API.md)) |
| Example strategy (constant product) | [`yRptCdde1du1JYR2DcNFtLC1fkp5Asd6rE3XSvZoUFx`](https://orbmarkets.io/address/yRptCdde1du1JYR2DcNFtLC1fkp5Asd6rE3XSvZoUFx/history?cluster=devnet) |

[`docs/DEVNET.md`](docs/DEVNET.md) gets you from nothing to a swap through the router on
devnet in a few minutes.

## Where to start

| You want to | Read |
|---|---|
| Try it on devnet | [`docs/DEVNET.md`](docs/DEVNET.md) |
| Build your own AMM or market | [`docs/QUICKSTART.md`](docs/QUICKSTART.md), then [`docs/STRATEGY-GUIDE.md`](docs/STRATEGY-GUIDE.md) |
| Build a desk, terminal, bot or chart on top | [`docs/INTEGRATOR-GUIDE.md`](docs/INTEGRATOR-GUIDE.md) |
| Use the hosted API (markets, trust flags, quotes, swap transactions, trades, candles, live stream) | [`docs/API.md`](docs/API.md) |
| Check a strategy's market (yours or anyone's) | [`crates/pipeline-check`](crates/pipeline-check/README.md) |
| Show and filter trust facts (upgradeable, verified build, token powers) | [`docs/INTEGRATOR-GUIDE.md`](docs/INTEGRATOR-GUIDE.md) section 5, [`sdk/ts`](sdk/ts/README.md) |
| Understand the router | [`docs/ROUTER.md`](docs/ROUTER.md) |
| See how it's verified and audited | [`docs/VERIFICATION.md`](docs/VERIFICATION.md), [`docs/SECURITY-AUDIT.md`](docs/SECURITY-AUDIT.md) |

## Repository

| Path | What | Licence |
|---|---|---|
| `programs/pipeline` | The router program (Anchor) | BSL 1.1, becomes GPL-2.0-or-later on 2028-11-08 |
| `templates/strategy` | Strategy template: a constant-product pool with LiteSVM tests, metadata and a devnet script. Start here (`scripts/new-strategy.sh`) | Apache-2.0 |
| `programs/test-strategy-cp` | Example strategy: a constant-product pool, with optional extras such as `pipeline_seed_liquidity` | Apache-2.0 |
| `programs/test-strategy-evil`, `test-hook`, `test-hook-refill` | Hostile strategy and transfer hooks used by the router's security tests | Apache-2.0 |
| `programs/pipeline-strategy-interface` | The strategy interface: types, discriminators, account layouts, fee maths | Apache-2.0 |
| `crates/pipeline-client` | Rust addresses, instruction builders and account decoders | Apache-2.0 |
| `crates/pipeline-events` | Event decoding for indexers | Apache-2.0 |
| `crates/pipeline-quoter` | Quotes any strategy by running its real code in an embedded Solana VM | Apache-2.0 |
| `crates/pipeline-check` | Quality checks for a strategy market, from your tests or against a deployed market | Apache-2.0 |
| `crates/pipeline-tests` | LiteSVM integration and security tests | Apache-2.0 |
| `sdk/ts` | TypeScript SDK (`@solana/kit`) | Apache-2.0 |
| `scripts/devnet` | End-to-end script: create tokens and a market, quote, swap, decode the event | Apache-2.0 |

## Install

```sh
npm install @pipeline-sol/sdk          # TypeScript: instructions, quotes, metadata, trust flags
cargo add pipeline-client              # Rust: addresses, instructions, decoders, trust facts
cargo add pipeline-strategy-interface  # in a strategy program: the interface and fee maths
cargo add pipeline-quoter              # quote any strategy in an embedded Solana VM
cargo add pipeline-events              # decode router events for an indexer
cargo install pipeline-check           # check any market: pipeline-check --url devnet --market <address>
```

To write a strategy, start from the template: `scripts/new-strategy.sh my-amm ~/code/my-amm`.

## Build and test

You need Rust (the version in `rust-toolchain.toml` is selected automatically),
`cargo-build-sbf` 4.4 or later for sBPF v3 output, and Node 22.18 or later for the
TypeScript SDK and scripts.

```sh
cargo install cargo-build-sbf --version 4.6.0 --locked
scripts/build-programs.sh                 # each program on its own -> programs/target/deploy
cargo test --workspace                    # LiteSVM integration and unit tests
(cd sdk/ts && npm install && npm test)    # TypeScript SDK
scripts/test-template.sh                  # the strategy template, against these sources
```

CI runs the same on every push. Keypairs are never committed: `scripts/check-no-keypairs.sh`
and the pre-commit hook (`git config core.hooksPath .githooks`) enforce it.

## Licence

The router (`programs/pipeline`) is under the Business Source License 1.1: read, modify and
use it freely outside production; on 2028-11-08 it converts to GPL-2.0-or-later. Everything
else is Apache-2.0. See [`LICENSING.md`](LICENSING.md).
