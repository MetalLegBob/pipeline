# pipeline-cpi

Call the [Pipeline](https://github.com/MetalLegBob/pipeline) router from an on-chain
program. A vault, treasury, keeper or smart wallet trades through any Pipeline market by
invoking the router's `swap` or `route` with its own PDA as the trader.

- `SwapAccounts` and `RouteAccounts` hold the router's accounts as `AccountInfo`s; their
  `from_router_order` reads them in the router's own order (the order the off-chain
  `pipeline-client` builders and the TypeScript SDK produce), and `instruction` builds the
  router instruction with the trader signing and the input hook account count set.
- `swap` and `route` invoke the router with your signer seeds and return its `SwapResult`
  or `RouteResult`, read from return data that must come from the router.
- `read_market` reads the router's `Market` record on-chain; `MarketView::fee_side` says
  which fee account a swap needs.
- `check_token_account` checks a token account's owner and mint. A program that lets
  someone else build its transactions must check its output account this way: the router
  measures whatever output account it is given.
- `depth`: the call-depth budget. `stack_height` and `log_stack_height` report where your
  program runs; `swap_fits` says whether a swap fits below it. A swap whose output mint has
  a transfer hook needs four levels below the caller, so under Solana's current limit of
  five only a top-level instruction can make one.
- With the `anchor` feature, `anchor::swap` and `anchor::route` take a `CpiContext`.

```rust
let swap = pipeline_cpi::SwapAccounts::from_router_order(router_accounts, input_hook_accounts)?;
pipeline_cpi::check_token_account(swap.trader_output, vault.key, &output_mint)?;
let result = pipeline_cpi::swap(
    &swap,
    &pipeline_cpi::SwapArgs::exact_in(amount, min_out),
    &[&[b"vault", owner.as_ref(), &[bump]]],
)?;
let kept = result.kept(swap.output_mint.key);
```

The guide is `docs/CPI-GUIDE.md` in the repository, and the worked example is
`programs/example-dca-vault`, a DCA vault tested against the real router in
`crates/pipeline-tests/tests/cpi_vault.rs`.

Licensed under Apache-2.0.
