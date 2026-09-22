# ADR-0001: Pluggable, Traceable Agent Runtime

## Status

Accepted

## Date

2026-09-22

## Context

Forge must be a fast, single-binary Rust coding harness that supports local and hosted models, intelligent routing, skills, project understanding, safe execution, CLI use, and server integrations. It must remain useful without TypeSafe Jev, MVM, Node.js, a database, or a specific model vendor.

## Decision

Forge will use a small, strongly typed runtime composed of:

```rust
ModelProvider
DecisionRouter
ExecutionProvider
SkillRegistry
ProjectGraph
SessionStore
```

CLI, TUI, REST/SSE, JSONL, and future gRPC adapters will call the same `AgentService`.

The decision abstraction is named `DecisionRouter`, not `JevProvider`. TypeSafe Jev, Kev, local Jev-like services, remote gateways, and deterministic rules may implement it.

The execution abstraction is named `ExecutionProvider`. Native execution, MVM, containers, remote workers, and mocks may implement it. MVM is optional.

Every run emits append-only events. Sessions, tracing, replay, inspection, and server streaming derive from that event stream.

The initial project graph is deterministic, local, incremental, and regenerable. Model-generated summaries are optional enrichment.

Configuration precedence is:

```text
defaults → configuration files → environment variables → CLI flags
```

## Consequences

### Positive

- Offline and local-only operation remains possible.
- Providers can be added without changing the agent loop.
- MVM can evolve independently.
- REST and gRPC can share behavior.
- BDD tests can use deterministic mocks.
- Traces provide auditability and replay foundations.
- The default binary remains small and portable.

### Negative

- More interfaces and capability negotiation are required.
- Event schemas require versioning.
- Dynamic native plugins are deferred.
- Provider-specific feature differences must be handled explicitly.

## Alternatives rejected

### Make TypeSafe Jev mandatory

Rejected because it prevents offline use and experimentation with Kev and other implementations.

### Make MVM mandatory

Rejected because native, container, mock, and remote execution are useful independently.

### Separate CLI and server runtimes

Rejected because behavior would drift and require duplicated tests.

### Use Graft as a runtime dependency

Rejected for v0.1 because its current Node/TypeScript implementation conflicts with the single-binary Rust goal. Its graph and context ideas will influence the Rust-native implementation.

## Follow-up

- Version event and transport schemas.
- Decide whether gRPC lands in v0.2 or v0.3.
- Benchmark BDD framework choices.
- Define the System One-compatible decision protocol.
