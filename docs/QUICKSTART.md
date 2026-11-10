# Quick start: your first Pipeline strategy

This takes you from an empty folder to a strategy that is registered, has a market, and trades through Pipeline. It covers your machine first, then devnet once Pipeline is deployed there (see `docs/DEVNET.md` for status).

You need:
- Rust;
- `cargo-build-sbf` 4.4 or later, which builds sBPF v3 programs (`cargo install cargo-build-sbf --version 4.6.0 --locked`), and the Solana CLI 4.4;
- Node 22.18 or later, for the TypeScript SDK and scripts.

## 1. Build and test everything

```sh
git clone <this repository> && cd <it>
scripts/build-programs.sh
cargo test --workspace
```

You should see every test pass, including:
- **router tests** — swaps, fees, pause, sweeps, two-hop routes;
- **Token-2022 tests** — transfer fees and transfer hooks;
- **adversarial tests** — a hostile strategy can't harm traders or fees;
- **quoter tests** — quotes match execution exactly.

## 2. Start from the template

`templates/strategy` is a complete strategy project: a constant-product pool in Anchor, LiteSVM tests against the real router, a metadata file and a devnet script. Copy it out with a new name and program key:

```sh
scripts/new-strategy.sh my-amm ~/code/my-amm
cd ~/code/my-amm
```

The script creates your program's keypair at `~/.config/pipeline/keys/my-amm.json` (outside the project) and writes its address into `declare_id!`. Then:

1. Change the pricing in `programs/my-amm/src/pricing.rs`. `pipeline_quote` and `pipeline_swap` both call it, which keeps quotes and swaps identical.
2. Keep the rest of the structure. Its comments mark the rules that matter; `docs/STRATEGY-GUIDE.md` sections 2 and 4 explain them.
3. Build:

   ```sh
   cargo build-sbf --manifest-path programs/my-amm/Cargo.toml
   ```

## 3. Test it against the real router

```sh
scripts/fetch-router.sh     # the devnet router, for the tests
cargo test
```

The tests create a market through the router, add fee accounts and liquidity, swap through the router, then run every `pipeline-check` check on the market: quotes match swaps in every direction and size, minimums hold, zero input buys nothing, and more. Run them after every pricing change. `crates/pipeline-tests` has more examples of router tests.

## 4. Rehearse on a local validator

`docs/DEVNET.md` ("Running Pipeline on a local validator") starts `solana-test-validator` with Pipeline loaded. Add `--upgradeable-program <your id> target/deploy/my_amm.so <your wallet>` to it, then run your project's `scripts/deploy-devnet.ts` with `RPC_URL` pointing at the validator.

## 5. On devnet

Pipeline is live on devnet ([`DEVNET.md`](DEVNET.md)):

1. **Deploy your program:**

   ```sh
   solana program deploy target/deploy/my_amm.so --program-id ~/.config/pipeline/keys/my-amm.json --url devnet
   ```

2. **Publish `metadata.json`** (`docs/STRATEGY-GUIDE.md` section 7) at an `https://`, `ipfs://` or `ar://` URL.
3. **Register, create a market and trade** with the project's script:

   ```sh
   cd scripts && npm install
   node deploy-devnet.ts https://<where you published>/metadata.json
   ```

   It registers the strategy (`register_strategy`, signed by your upgrade authority, 0.01 devnet SOL), points it at your metadata, creates a market and records it with `init_fee_account` for each mint in one transaction, adds liquidity with the `deposit` action from your metadata, then quotes and swaps through the router. A market the router hasn't recorded is invisible to everything built on Pipeline (`docs/STRATEGY-GUIDE.md` section 6).
4. **Check it** the way integrators will, and publish the report:

   ```sh
   cargo install pipeline-check
   pipeline-check --url devnet --market <market address> --json > report.json
   ```

   Put the report's URL and SHA-256 in your metadata's `conformance`.

## Where to read next

- `docs/STRATEGY-GUIDE.md`: the interface, the rules and Solana's limits for strategies.
- `docs/INTEGRATOR-GUIDE.md`: if you're building a desk, terminal, bot or chart on top.
- `docs/ROUTER.md`: how the router works.
