# pipeline-quoter

Quotes any [Pipeline](https://github.com/MetalLegBob/pipeline) strategy by running its
deployed program in an embedded, sandboxed Solana VM, on account data you supply. It
applies Pipeline's fee, your integrator fee and Token-2022 transfer fees, so quotes match
what the router will charge and deliver. About 10–20 µs per quote for simple strategies.

```toml
[dependencies]
pipeline-quoter = "0.1"
```

```rust
let mut quoter = pipeline_quoter::Quoter::new();
quoter.load_program_from_program_data(strategy_id, &program_data.data)?;
// Load the market's accounts with `set_account`. Account resolution asks for any more
// the strategy needs; `fetch` loads them (from your stream or an RPC).
let accounts = quoter.resolve_accounts(&market, &request, |quoter, keys| fetch(quoter, keys))?;
let quote = quoter.quote(&market, &request, &accounts)?;
```

`simulate_swap` runs the whole router swap for exact numbers with any token behaviour.
Reload a strategy's program when its ProgramData changes. See
`docs/INTEGRATOR-GUIDE.md` in the repository.

Licensed under Apache-2.0.
