# Forge Implementation Plan (v0.2 vertical slice)

Source specs: `specs/project.md`, `specs/adrs/0001-core-architecture.md`, `specs/v0.2-prompt.md`.
Toolchain confirmed: rustc/cargo 1.97.1, `just` available.

## Crate layout (Cargo workspace)

| Crate | Responsibility |
|---|---|
| `forge-core` | Core traits (`ModelProvider`, `DecisionRouter`, `ExecutionProvider`, `SkillRegistry`, `ProjectGraph`, `SessionStore`), shared types, versioned event protocol, typed errors. No heavy deps. |
| `forge-config` | Config loading with precedence defaults → files (`~/.config/forge/config.toml`, `.forge/config.toml`) → env (`FORGE_*`) → CLI flags; `explain <key>` with winning source. |
| `forge-execution` | `NativeExecution` and `MockExecution` behind `ExecutionProvider`; approval gating hook. |
| `forge-providers` | `MockModel` and OpenAI-compatible HTTP provider (oMLX-compatible), explicit `ModelCapabilities`; static/mock/HTTP `DecisionRouter` impls (System One-compatible, arbitrary base URL + env-selected API key, timeout, fallback). |
| `forge-session` | Append-only JSONL session/run store under `.forge/sessions/`, secret redaction. |
| `forge-skills` | `SKILL.md` discovery (`.forge/skills/`, `.agents/skills/`, `.claude/skills/`, `~/.config/forge/skills/`, `~/.agents/skills/`), frontmatter metadata-first loading, on-demand activation, scripts via `ExecutionProvider`. |
| `forge-graph` | Deterministic incremental project graph (files, dirs, symbols, imports, tests, basic calls) stored under `.forge/graph/graph.json`, freshness via mtime+hash. |
| `forge-runtime` | `AgentService` shared by CLI and server: routing → model → tools/skills → execution, emits events. Transport-neutral. |
| `forge-server` | axum REST/SSE adapter over `AgentService`, loopback default (`127.0.0.1:7341`). |
| `forge-cli` | Clap derive command tree, tracing verbosity mapping, stdout/stderr/JSON discipline, `init`, doctor, wiring. |

## Increments

1. **A — Foundation**: workspace, `forge-core` traits/types/errors/events, `forge-config` with precedence + explain, `forge-cli` full clap tree (all commands present), tracing `-v/-vv/-vvv`, Justfile.
2. **B — Runtime plumbing**: `forge-execution` (native+mock, approval), `forge-providers` (mock + OpenAI-compatible model; static/mock/HTTP routers with confidence, capability filtering, timeout, fallback), `forge-session` (JSONL, redaction), `forge-runtime` `AgentService`.
3. **C — Graph & skills**: deterministic incremental graph + graph CLI commands; skill discovery/activation/logging + skill CLI commands.
4. **D — Server**: axum REST endpoints + SSE run events; `forge serve`.
5. **E — BDD**: cucumber-rs harness in `forge-cli/tests/`, step definitions driving the built binary in temp dirs, making all 7 existing `.feature` files executable and green (offline mocks).
6. **F — Verification**: `cargo fmt --check`, `cargo check --workspace --all-targets`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-features`, `just bdd`, plus acceptance commands (`init`, `config show`, `config explain model`, `graph build`, `graph map`, `skill list`, `doctor`).

## Key decisions

- Event protocol: `{"v":1,"type":"...","ts":"...","run_id","session_id",...}` JSONL, append-only.
- Secrets redacted via pattern + env-value matching before event write.
- Router config: `router = "static|mock|http"`, HTTP router takes `router_url`, `router_key_env`, `router_timeout_ms`; no hard-coded TypeSafe/Kev URLs or model names.
- Model capabilities explicit (`streaming`, `tools`, `structured_output`, `vision`, `max_context`); router filters candidates by capability.
- Approval modes: `auto`, `prompt`, `deny` for risky commands; mock provider records requests for BDD.
- Graph stored as `.forge/graph/graph.json`; per-file mtime + blake3/sha hash for incremental rebuild; no model calls.
- BDD: cucumber-rs integration test in `forge-cli`, invoking the compiled `forge` binary (via `CARGO_BIN_EXE_forge`) in temp project dirs; server scenario spawns `forge serve --port 0`-style ephemeral port.

## Engineering rules honored

No pseudocode/placeholders, typed errors (`thiserror`), no `unwrap`/`expect` in production code, tests per feature, `just` recipes: check, fmt, lint, test, bdd, verify, build, release, clean.
