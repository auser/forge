# Forge: Project Specification

## 1. Summary

Forge is a lightweight, single-binary Rust agentic coding harness. It combines a Pi-like interactive coding experience, DeepSeek-style composable capabilities, configurable Jev-style decision routing, graph-aware project context, reusable skills, and pluggable execution providers.

The binary must work with hosted and local model backends without requiring Node.js, Python, a database server, or a mandatory daemon.

## 2. Goals

- Fast, portable, single-binary coding agent.
- Any model backend through provider adapters.
- Configurable decision routing using TypeSafe Jev, Kev, faster/local variants, or deterministic rules.
- Skills using `SKILL.md` with progressive disclosure.
- Deterministic, incremental project graph.
- CLI, TUI, REST/SSE, JSONL, and future gRPC access.
- Pluggable execution; MVM is optional.
- Append-only, inspectable session events.
- BDD coverage for user-visible behavior.

## 3. Core traits

```rust
ModelProvider
DecisionRouter
ExecutionProvider
SkillRegistry
ProjectGraph
SessionStore
```

`ModelProvider` generates language, structured output, and tool calls.

`DecisionRouter` chooses models, workflows, skills, context budgets, and escalation paths. Implementations include TypeSafe Jev, Kev, local Jev-like HTTP services, static rules, and mocks.

`ExecutionProvider` runs commands and mutations. Implementations include native, mock, container, MVM, and remote execution. The runtime never invokes processes directly outside this trait.

`SkillRegistry` discovers and activates skills. `ProjectGraph` maintains local structural context. `SessionStore` persists events for resume and replay.

## 4. CLI

Use Clap derive:

```text
forge init
forge run
forge serve
forge resume
forge cancel
forge session
forge graph build|check|map|grep|callers|blast|context
forge skill list|show|test
forge model list|test
forge config show|path|explain
forge doctor
forge version
```

Global options include `-v`, `-vv`, `-vvv`, `--config`, `--project`, `--model`, `--router`, `--execution`, `--local-only`, `--approval`, `--json`, and `--no-color`.

## 5. Configuration

Precedence is strictly:

```text
built-in defaults → configuration files → environment variables → CLI flags
```

Support `~/.config/forge/config.toml` and `.forge/config.toml`. `forge config explain <key>` must identify the winning value and source.

## 6. Project initialization

`forge init` is idempotent. It finds the project root, creates `.forge/`, creates a starter config only when absent, discovers instructions and manifests, builds the structural graph, updates `.gitignore` without duplicates, and reports all changes. It must preserve existing files.

## 7. Server

CLI and server use the same `AgentService` and runtime.

v0.1 REST/SSE endpoints:

```text
GET  /health
GET  /v1/capabilities
GET  /v1/models
POST /v1/runs
GET  /v1/runs/:id
POST /v1/runs/:id/input
POST /v1/runs/:id/cancel
GET  /v1/runs/:id/events
GET  /v1/skills
GET  /v1/project/graph
POST /v1/project/context
```

The server binds to loopback by default. gRPC must be addable as a transport adapter without changing runtime semantics.

## 8. Skills

Discover skills from `.forge/skills/`, `.agents/skills/`, `.claude/skills/`, `~/.config/forge/skills/`, and `~/.agents/skills/`. Load metadata first; activate full instructions only when relevant. References and scripts are loaded on demand. Scripts execute through `ExecutionProvider`. Activation is logged.

## 9. Project graph

The initial graph is deterministic, local, incremental, and regenerable. It covers files, directories, symbols, imports, tests, searchable relationships, and basic calls where available. It is stored below `.forge/graph/` and ignored by Git. Semantic summaries are optional enrichment and never required for structural builds.

## 10. Tracing and events

Use Rust `tracing`:

```text
default → WARN
-v      → INFO
-vv     → DEBUG
-vvv    → TRACE
```

Human output goes to stdout; diagnostics go to stderr; JSON mode keeps stdout machine-readable. Events include run/session IDs, provider/model, routing decisions, skill activation, tool start/end, file changes, errors, cancellation, and completion. Secrets must not be logged.

## 11. BDD

Executable Gherkin scenarios live under `tests/features/`. Required features are configuration, initialization, graph, skills, routing, execution, server, tracing, and sessions. Scenarios use deterministic mock providers and run offline in CI.

## 12. v0.1 completion

These must work from a clean checkout:

```bash
forge init
forge graph map
forge run "Explain this project"
forge serve
curl http://127.0.0.1:7341/health
```

All unit, integration, and BDD tests must pass on supported macOS and Linux builds.
