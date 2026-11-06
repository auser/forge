# Spike: Infinity as forge's execution engine

Throwaway, like `../rten-embed`. It answers one question and then stops
being interesting: **can forge keep its own provider stack and still run on
Infinity's agent runtime?**

Run it:

```
cargo test --manifest-path docs/superpowers/spikes/infinity-runtime/Cargo.toml
```

## What it proves

A `ForgeProviderAdapter` implementing `infinity_provider_protocol::ModelProvider`
drives a real `AgentSystemBuilder::new_local(...)` thread end to end:
`send_user_text` in, the adapter's streamed chunks out. Nothing is mocked on
the Infinity side.

## What it found

1. **`infinity-agent-core` 0.2.0 and `infinity-provider-protocol` 0.2.0 build
   on this workspace's toolchain** (rustc 1.98.1, edition 2024) and pull in
   neither `rig-core` nor the AWS SDK — those live in sibling provider crates
   forge never has to depend on.
2. **The provider seam fits.** `StaticModel::new` takes
   `Arc<dyn ModelProvider>`, so forge-providers plugs in as an adapter and the
   whole decision plane (Needle, Jev, routing profiles, cost books, credential
   detection) survives the move intact.
3. **The local driver spawns `!Send` futures.** Without a `LocalSet` the
   system panics at the first thread launch:
   `spawn_local called from outside of a task::LocalSet or runtime::LocalRuntime`.
   So the Infinity system has to live on one dedicated thread behind a job
   channel — which is the pattern `NeedleEngine` already uses for `libneedle`
   and `TerminalIo` for `rustyline`. This is the single most load-bearing
   constraint on the integration, and only running the spike surfaced it.
