# pipeline-strategy-interface

The interface between the [Pipeline](https://github.com/MetalLegBob/pipeline) router and
strategy programs: instruction discriminators, account layouts, request and result types,
and the fee maths the router uses. Strategy programs depend on it on-chain; quoters, SDKs
and indexers use the same types off-chain.

```toml
[dependencies]
pipeline-strategy-interface = "0.1"
```

- `TradeRequest`, `SwapRequest`, `QuoteResult`, `MarketInfo`: what the router sends and
  expects back.
- `discriminator`, `trade_accounts`, `init_accounts`, `info_accounts`: the instruction
  bytes and fixed account layouts.
- `fees`: Pipeline's 0.25% fee, integrator fees and `max_total_spend`; `credited_input`;
  `settlement::settle`, the router's swap measurement. The fee arithmetic is proven
  correct for every input (Kani and Z3; see `docs/VERIFICATION.md` in the repository).

Start from the strategy template and `docs/STRATEGY-GUIDE.md` in the repository.

Licensed under Apache-2.0.
