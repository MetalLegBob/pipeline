# pipeline-events

Decodes the events the [Pipeline](https://github.com/MetalLegBob/pipeline) router emits
(`SwapEvent`, `MarketCreated`, `StrategyRegistered`, `MetadataUpdated`, `FeesSwept`,
`ConfigChanged`) from the router's self-CPI instruction data, for indexers and charts.

```toml
[dependencies]
pipeline-events = "0.1"
```

```rust
if let Some(event) = pipeline_events::decode(&inner_instruction_data) {
    // match on PipelineEvent::Swap(..), PipelineEvent::MarketCreated(..), …
}
```

Every swap through the router produces one `SwapEvent` with the amounts the router
measured. See `docs/INTEGRATOR-GUIDE.md` in the repository.

Licensed under Apache-2.0.
