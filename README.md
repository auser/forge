# Forge

Forge is a lightweight, single-binary Rust agentic coding harness. It combines a fast
interactive coding agent core, provider-neutral model access, configurable decision
routing, progressive-disclosure skills, a deterministic incremental project graph,
pluggable execution, and both CLI and REST/SSE interfaces over one shared runtime.

No Node.js, Python, database, or daemon is required. Everything works offline with
built-in mock providers.

- Project spec: [`specs/project.md`](specs/project.md)
- Architecture decisions: [`specs/adrs/`](specs/adrs/) (start with `0001-core-architecture.md`)
- Implementation plan: [`specs/implementation-plan.md`](specs/implementation-plan.md)
- Roadmap and future directions: [`specs/roadmap.md`](specs/roadmap.md)
- BDD features: [`tests/features/`](tests/features/)

## Quickstart

```bash
cargo build --release          # or: just release
cd /path/to/your/project
forge init                     # creates .forge/, starter config, gitignore entry, builds graph
forge doctor                   # health report
forge run "Explain this project"
forge serve                    # REST/SSE on http://127.0.0.1:7341
curl http://127.0.0.1:7341/health
```

Out of the box Forge uses the built-in mock model, static router, and native execution,
so every command above works with no accounts, keys, or network.

## Command line

```text
forge init                          Initialize a project (idempotent)
forge run [--max-turns N] <prompt>  Run the multi-turn agent loop
forge serve [--host --port]         Start the REST/SSE server
forge resume <run-or-session-id>    Continue a completed run in its session
forge cancel <run-or-session-id>    Cancel a run (in-flight or recorded)
forge session [list|show <id>]      Inspect sessions (JSONL event logs)
forge graph build|check|map|grep|callers|blast|context
forge skill list|show|test
forge model list|test
forge config show|path|explain <key>
forge doctor                        Environment/config health check
forge version
```

Global flags:

```text
-v / -vv / -vvv       diagnostics at INFO / DEBUG / TRACE (stderr; default WARN)
--config <path>       additional config file, layered after the project config
--project <path>      project directory (default: cwd, root discovered upward)
--model <m>           override the configured model
--router <r>          override the router (static|mock|http)
--execution <p>       override the execution provider (native|mock)
--local-only          restrict to local providers
--approval <mode>     auto | prompt | prompt-dangerous | deny
--json                machine-readable JSON on stdout, nothing else on stdout
--no-color            disable ANSI colors
```

Output discipline: human results go to **stdout**, diagnostics/tracing go to
**stderr**, and `--json` guarantees stdout is a single machine-readable JSON value
even with `-vvv` enabled.

## Configuration

Precedence is strict:

```text
built-in defaults → ~/.config/forge/config.toml → .forge/config.toml → FORGE_* env → CLI flags
```

(`$XDG_CONFIG_HOME/forge/config.toml` is honored when set. `--config <path>` layers
an extra file after the project config.)

Key settings (all optional):

| Key | Default | Env var | Meaning |
|---|---|---|---|
| `model` | `mock-local` | `FORGE_MODEL` | Active model (`scripted-mock` = scripted offline model) |
| `mock_script` | — | `FORGE_MOCK_SCRIPT` | JSON script path for `scripted-mock` (project-relative) |
| `model_base_url` | — | `FORGE_MODEL_BASE_URL` | OpenAI-compatible endpoint (oMLX etc.) |
| `model_key_env` | — | `FORGE_MODEL_KEY_ENV` | Name of the env var holding the API key |
| `router` | `static` | `FORGE_ROUTER` | `static` \| `mock` \| `http` |
| `router_url` | — | `FORGE_ROUTER_URL` | System One-compatible router endpoint |
| `router_key_env` | — | `FORGE_ROUTER_KEY_ENV` | Name of the env var holding the router key |
| `router_timeout_ms` | `5000` | — | HTTP router timeout |
| `execution` | `native` | `FORGE_EXECUTION` | `native` \| `mock` |
| `approval` | `prompt` | `FORGE_APPROVAL` | `auto` \| `prompt` \| `prompt-dangerous` \| `deny` |
| `local_only` | `false` | `FORGE_LOCAL_ONLY` | Restrict to local providers |
| `server_host` | `127.0.0.1` | `FORGE_SERVER_HOST` | Server bind address (loopback default) |
| `server_port` | `7341` | `FORGE_SERVER_PORT` | Server port |
| `max_turns` | `25` | `FORGE_MAX_TURNS` | Agent-loop turn budget |

Unknown keys are tolerated. Inspect the resolved configuration:

```bash
forge config show            # merged effective config
forge config path            # config file locations and which exist
forge config explain model   # winning value + source, e.g. model = "cli-model" (source: cli-flag)
```

## Models, routing, execution

These are the three pluggable seams (traits in `forge-core`):

- **ModelProvider** — `mock` (offline, deterministic) or any OpenAI-compatible
  server (oMLX and friends) via `model_base_url`. Capabilities (streaming, tools,
  structured output, vision, context size) are explicit per provider, never assumed.
- **DecisionRouter** — chooses the model per task and records the decision with a
  confidence score. Five modes:
  - `static` (deterministic rules; default),
  - `mock` (preset decision, for tests),
  - `cheapest` (lowest-cost candidate from the `[models]` cost table;
    tie-breaks by output cost then name),
  - `http` (System One-compatible: POST `{task, candidates, required_capabilities}`
    to `router_url`, bearer token from `router_key_env`),
  - `laya` (Laya typed-questions shape; defaults to
    `http://127.0.0.1:8788/decide`, the reference adapter below).

  `http`/`laya` decisions below `router_confidence_threshold` (default 0.7) are
  rejected and escalate through the fallback chain: any router is wrapped in a
  fallback (`router_fallback`, default `static`, may be `cheapest`), so an
  unreachable, timing-out, or unconfident router degrades to deterministic
  routing with `fallback_used: true`. TypeSafe Jev / Kev services work through
  the `http` backend — nothing is hard-coded.

### Model registry with costs

```toml
# .forge/config.toml
router = "cheapest"           # or "laya"
router_fallback = "cheapest"  # used when the primary fails / is unconfident

[models.mock-fast]
description = "fast local mock for trivial tasks"
cost_input_per_mtok = 0.0

[models.deepseek-coder]
description = "strong coding model"
base_url = "http://127.0.0.1:8080"   # oMLX / OpenAI-compatible endpoint
key_env = "DEEPSEEK_API_KEY"          # env var holding the API key
cost_input_per_mtok = 0.14
cost_output_per_mtok = 0.28
tools = true                          # capability overrides are explicit
```

The `[models]` table deep-merges by name across user/project config files
(env/CLI flags don't set entries). The runtime builds routing candidates from
it, and when the router selects a model with a `base_url`, Forge constructs the
OpenAI-compatible provider for it automatically. `forge model list` shows the
cost table.

### Laya via the reference adapter

Laya (open-source System One decision model) is a Python SDK with no official
server. `adapters/laya-http.py` is a small stdlib-only bridge:

```bash
pip install laya
python3 adapters/laya-http.py 8788   # listens on 127.0.0.1:8788
```

then set `router = "laya"` (and optionally `router_url`). Forge POSTs
`{"state": {"task", "required_capabilities"}, "questions": {"model": {"type":
"choice", "instructions": ..., "criteria": {name: description}}}}` and expects
`{"answers": {"model": {"choice", "confidence"}}}`. The adapter is optional;
Forge never requires Python.
- **ExecutionProvider** — all command/script execution AND file
  reads/writes/edits/deletes go through this trait (the runtime never spawns
  processes or touches files directly). Risk classification: reads are `Safe`,
  in-project writes/edits are `Risky`, deletes and out-of-project paths are
  `Destructive`. `native` runs locally with approval gating: `Risky` operations
  pause for approval under `approval = "prompt"`, while `prompt-dangerous` asks
  only for `Destructive` ones (non-interactive stdin → typed "approval required"
  error, which the agent loop treats as a pause: answer via piped stdin lines,
  e.g. `echo y | forge run ...`). `auto` runs, `deny` blocks. `mock` records
  requests for tests. MVM/container/remote executors plug into the same trait
  later.

## Skills

Skills are directories containing a `SKILL.md` with YAML frontmatter
(`name`, `description`) and a markdown body of instructions. Discovery roots, in
order (project shadows user on name collision):

```text
.forge/skills/  .agents/skills/  .claude/skills/  ~/.config/forge/skills/  ~/.agents/skills/
```

Progressive disclosure: `forge skill list` reads only frontmatter metadata;
`forge skill show <name>` loads the full instructions; references/scripts load on
demand. During `forge run`, a prompt matching a skill activates it — the
instructions are injected into the model context and a `skill_activated` event is
appended to the session log. `forge skill test <name>` runs the skill's
`test.sh`/`test.py` through the configured execution provider.

## Project graph

`forge graph build` (also run by `forge init`) builds a deterministic, local graph
— no model calls, no network — stored at `.forge/graph/graph.json` (git-ignored).
It indexes files, directories, symbols, imports, tests, and basic call sites for
Rust, Python, JS/TS, and Go (regex-based extraction). Rebuilds are incremental:
only files whose mtime+hash changed are re-parsed.

```bash
forge graph build              # build / incrementally refresh
forge graph check              # fresh (exit 0) or stale (exit 1, lists changes)
forge graph map                # per-directory structural summary
forge graph grep <pattern>     # search symbols and imports
forge graph callers <symbol>   # who calls this symbol
forge graph blast <path>       # direct + second-hop importers
forge graph context <query>    # ranked files/symbols for agent context
```

## Server

`forge serve` exposes the same `AgentService` the CLI uses (transport-neutral by
design; gRPC can be added as another adapter). Binds to `127.0.0.1:7341` by
default.

```text
GET  /health                   liveness + version
GET  /v1/capabilities          server/model/router/execution capabilities
GET  /v1/models                known models with capabilities
POST /v1/runs                  {"prompt": "..."} → 202 {"run_id","session_id"}
GET  /v1/runs/:id              status + events so far; status is one of
                               running | waiting_for_approval | completed |
                               failed | cancelled
POST /v1/runs/:id/input        deliver input to a run (202): a run parked in an
                               approval wait consumes it ("y" approves, anything
                               else denies). Records an `input_received` event.
                               404 unknown run, 409 terminal/closed run
POST /v1/runs/:id/cancel       cancel mid-loop: runtime cancellation token +
                               `.forge/runs/<id>.cancel` marker + task abort +
                               `cancelled` event (404 if unknown)
GET  /v1/runs/:id/events       SSE: replays stored events, streams live, ends
                               after a terminal (completed/cancelled/error) event
GET  /v1/skills                discovered skill metadata
GET  /v1/project/graph         graph stats + freshness
POST /v1/project/context       {"query": "..."} → ranked context selection
```

The server tracks at most 1024 in-flight/recent runs in memory
(`MAX_TRACKED_RUNS`); oldest terminal entries are evicted first and remain fully
retrievable from the session store (the source of truth).

## Sessions and events

Every run appends versioned events (`"v": 2`, with a monotonic per-run `seq`
assigned by the session store on append) to
`.forge/sessions/<session_id>.jsonl` — one JSON object per line, append-only.
v1 logs (no `seq`, f32 confidence) remain readable. Event kinds: `run_started`,
`routing_decision_made`, `skill_activated`, `tool_call_requested`,
`tool_started`, `tool_completed`, `file_changed`, `approval_requested`,
`approval_decided`, `turn_completed`, `input_received`, `note` (v1 compat),
`error`, `cancelled`, `completed`. Events carry run/session IDs, provider,
model, routing confidence, and fallback flags. Secret-looking values (API-key
patterns, `Bearer` tokens, values of `*KEY*`/`*TOKEN*`/`*SECRET*`/`*PASSWORD*`
env vars) are redacted to `[REDACTED]` before anything is written.

```bash
forge session list        # sessions with event counts
forge session show <id>   # full event history
forge resume <id>         # continue a completed run (new run, same session,
                          # seeded with the original prompt + prior outcome)
```

## Development

Requires a recent stable Rust (developed on 1.97) and [`just`](https://just.systems).

```bash
just check     # cargo check --workspace --all-targets
just fmt       # cargo fmt --all
just lint      # clippy, warnings denied
just test      # unit + integration tests
just bdd       # cucumber BDD suite (drives the compiled binary)
just verify    # fmt --check + check + lint + test + bdd — run before committing
just build     # debug build
just release   # release build
just clean
```

Layout:

```text
crates/
  forge-core        traits, event protocol, typed errors (no heavy deps)
  forge-config      config loading, precedence, provenance
  forge-execution   native + mock execution providers
  forge-providers   mock + OpenAI-compatible models; static/mock/HTTP routers
  forge-session     append-only JSONL store + secret redaction
  forge-skills      SKILL.md discovery, progressive disclosure
  forge-graph       deterministic incremental project graph
  forge-runtime     AgentService — the one runtime shared by CLI and server
  forge-server      axum REST/SSE adapter
  forge-cli         clap command tree, tracing, the forge binary
tests/features/     Gherkin scenarios (executable via just bdd)
specs/              project spec, ADRs, implementation plan
```

Engineering rules: typed errors (`thiserror`); no `unwrap`/`expect` in production
code; no placeholder modules; tests land with each feature; architecture decisions
go in `specs/adrs/`.

## Testing

- Unit/integration tests: `just test` (all crates; HTTP router covered with
  wiremock; server covered with tower oneshot + a real ephemeral-port roundtrip).
- BDD: `just bdd` runs cucumber against `tests/features/` using the compiled
  `forge` binary in hermetic temp dirs (isolated `HOME`/`XDG_CONFIG_HOME`), with
  mock providers — fully offline. Currently 16 features / 24 scenarios / 93 steps.

## Known limitations (v0.3)

- `forge resume` seeds the new run with the original prompt and the prior
  (truncated) completion summary; full conversation replay is future work.
- Symbol/call extraction is regex-based; `graph blast` covers two hops.
- Server run-status state is in-memory (bounded at `MAX_TRACKED_RUNS` = 1024,
  terminal-first eviction); the session store persists across restarts.
- Input delivery is in-process: `POST /v1/runs/:id/input` for a run owned by
  another process records the event but that loop does not consume it.
