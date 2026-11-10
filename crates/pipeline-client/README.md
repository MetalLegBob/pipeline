# pipeline-client

Rust client for the [Pipeline](https://github.com/MetalLegBob/pipeline) router: program
addresses and PDAs, instruction builders, account decoders, and trust facts.

```toml
[dependencies]
pipeline-client = "0.1"
```

- `pda`, `decode_strategy`, `decode_market`, `decode_config`: find and read every
  strategy and market.
- `swap`, `create_market`, `register_existing_market`, `init_fee_account`,
  `register_strategy`, `update_metadata`, `sweep_fees`: build router instructions.
- `wallet::swap_with_setup`: a complete swap transaction, with token-account setup and
  SOL wrapping.
- `facts`: Token-2022 mint extensions and program facts (upgradeable, frozen, code hash,
  verified build) as the same flags the TypeScript SDK uses.
- `metadata`: validates strategy metadata (version 1).

See `docs/INTEGRATOR-GUIDE.md` in the repository.

Licensed under Apache-2.0.
