# Forge

Forge is a lightweight, single-binary Rust agentic coding harness. It combines a fast
interactive coding agent core, provider-neutral model access, configurable decision
routing, progressive-disclosure skills, a deterministic incremental project graph,
pluggable execution, and both CLI and REST/SSE interfaces over one shared runtime.

No Node.js, database, or daemon is required. The default stack is an embedded
Needle 3 decision router (on-device, no network calls) in front of a local
oMLX coding model — no mock in the default path, no hosted account needed.
When needle declines or fails, forge can escalate to Jev (TypeSafe's hosted
System One API, or a self-hosted OpenJev server) before falling all the way
back to deterministic static routing — opt-in only when a Jev credential is
configured (`router_escalate = "auto"`, the default, is a no-op without one)
and always skipped under `--local-only`. Laya (open-source System One) and
other HTTP-style routers remain available as alternates. Mock providers
exist for tests and demos but are strictly opt-in (`model = "mock-local"`).

- **How it all fits together** — the decision plane (Needle → Jev/OpenJev →
  static), the generation plane (local → subscription → API-key cloud), the
  fast path, and every trait seam: [`ARCHITECTURE.md`](ARCHITECTURE.md)
- Project spec: [`specs/project.md`](specs/project.md)
- Architecture decisions: [`specs/adrs/`](specs/adrs/) (start with `0001-core-architecture.md`)
- Implementation plan: [`specs/implementation-plan.md`](specs/implementation-plan.md)
- Roadmap and future directions: [`specs/roadmap.md`](specs/roadmap.md)
- Needle/Jev embedded-brain design (next direction: on-device decisions,
  cheapest-first routing, ACP/MCP editor integration):
  [`docs/superpowers/specs/2026-09-23-needle-embedded-brain-design.md`](docs/superpowers/specs/2026-09-23-needle-embedded-brain-design.md)
- BDD features: [`tests/features/`](tests/features/)

## Installation

One line, using the latest prebuilt release (macOS, Linux, and Windows via
Git Bash/MSYS2; detects OS/architecture and verifies the checksum):

```bash
curl -fsSL https://raw.githubusercontent.com/auser/forge/main/install.sh | bash
```

Or with Cargo, no script needed:

```bash
cargo install --git https://github.com/auser/forge forge-cli --locked
```

From a local checkout:

```bash
./install.sh                        # installs to ~/.local/bin
./install.sh --prefix /usr/local/bin
./install.sh --uninstall
# or: cargo install --path crates/forge-cli --locked
```

The script downloads the release asset for your platform
(`forge-<target-triple>.tar.gz` from the latest GitHub release, verified
against its published SHA-256) and falls back to `cargo install` when no
prebuilt asset exists yet. It installs to `~/.local/bin` by default
(`--prefix` or `FORGE_PREFIX` to override), warns if that directory is not
on your `PATH`, and respects `NO_COLOR` and non-interactive terminals.
Release assets are built by CI for every `v*` tag (see
`.github/workflows/release.yml`).

## Quickstart

The default stack routes on-device: embedded Needle 3 → a real local model
via oMLX — no mock anywhere in the default path, no separate router process
to start.

Prereqs: an OpenAI-compatible server running `qwen3-coder` at
`http://127.0.0.1:8080/v1` (oMLX or compatible). After
[installing](#installation) (or with `cargo build --release` and
`./target/release/forge` in place of `forge`):

```bash
cd /path/to/your/project
forge init                     # creates .forge/, starter config, gitignore entry, builds graph, fetches Needle weights
forge doctor                   # probes model + router endpoints and the embedded needle brain (weights, load, decision latency), warns if down
forge run "Explain this project"
forge serve                    # REST/SSE on http://127.0.0.1:7341
curl http://127.0.0.1:7341/health
```

No GPU, no accounts, just evaluating? The mock is one explicit flag away:

```bash
forge --model mock-local --router static run "Explain this project"
```

`forge init` fetches and verifies Needle's weights when `needle.autofetch`
is on (the default) — a one-time ~35 MB download for `needle.variant = "full"`
(the only variant with a hosted, pinned artifact today; `small`/`medium`
report "no pinned weights artifact" and fall back to static routing),
cached under `~/.cache/forge/models/`; re-running `init`
re-verifies the checksum and skips the download if it already matches.
Whenever weights aren't present (no network, `--local-only`, or a variant
with nothing to fetch yet — see below), routing falls back to deterministic
static routing (`fallback_used: true` in the events) and the run proceeds
with the configured model; this is a fully supported, fully offline mode,
not a degraded one. To point at a different endpoint or use an API
key, override per project:

```toml
# .forge/config.toml
model_base_url = "http://127.0.0.1:8080/v1"   # include the /v1 prefix
model_key_env = "MY_API_KEY"   # name of the env var, never the key itself
```

### Drop-in setup for existing projects

`forge init` works in any existing project: at startup Forge loads `.env` and
`.env.local` from the project root (shell env wins over `.env.local`, which
wins over `.env`), and init reports what it found — e.g.
`detected  DEEPSEEK_API_KEY → deepseek-chat routable` (key names only, values
are never printed or written anywhere). With keys in place, the default
embedded Needle router plus the built-in `[models]` registry give you
Jev-style model selection out of the box — no config file needed.

```bash
forge init     # loads .env/.env.local, detects known provider keys, builds the graph
```

Full environment precedence: **shell env** (incl. `FORGE_*` vars) →
`.env.local` → `.env` → project config file → user config file → defaults;
CLI flags beat everything. Values loaded from `.env` files are covered by
session-log secret redaction just like shell-set keys.

Forge ships ready-made config presets — copy one into `.forge/config.toml`
(or `~/.config/forge/config.toml` for all projects) and you're done:

```bash
cp configs/hybrid-laya.toml .forge/config.toml   # from the repo's examples/
```

See [`examples/`](examples/) for `local-first`, `hybrid-laya`,
`budget-hosted`, and `offline-eval` presets plus an `env.example` template
for provider keys.

## Usage

Run the agent loop (multi-turn, tool-using when the model supports it):

```bash
forge run "add a hello function to main.rs"   # edits files via tools
forge run --max-turns 10 "refactor the parser"
forge run --json "summarize this repo" | jq .text
```

Well-defined read-only requests skip the LLM entirely. All of these must
hold: the run's model is tool-capable (a chat-only model's run stays a plain
completion), the embedded Needle brain both picks a tool and fills its
arguments with confidence at least `router_confidence_threshold`, the
operation is **read-only** (`read_file`, `graph_context`, `graph_grep` —
anything that could need approval is excluded by construction, so a fast
path never prompts you), and a second on-device check finds the call
non-destructive. Then Forge dispatches it directly — no model call at all,
and the run records `router: "needle-dispatch"` with `turns: 0`.

Everything else — writes, deletes, commands, prompts the brain declines or
is unsure about, resumed runs — runs the full agent loop exactly as before.
The fast path is purely an optimization: it dispatches through the same
approval-gated execution provider as the loop, so it can never do something
a normal run of the same configuration could not, and without a working
brain (no `ffi` feature, weights missing) it simply never engages.

Approval, when a tool call needs it (`approval = "prompt"`):

```bash
forge run "clean up build artifacts"     # interactive y/N on a terminal
echo y | forge run "delete old logs"     # non-interactive: pipe answers
```

Interrupt and continue:

```bash
forge cancel <run-id>        # works from another terminal while a run is live
forge resume <run-id>        # continues the completed run in its session
forge session list           # what happened, per session
forge session show <id>      # full event history (JSONL, one event per line)
```

Drive it over HTTP:

```bash
forge serve &
curl -s -X POST http://127.0.0.1:7341/v1/runs \
  -H 'content-type: application/json' -d '{"prompt": "explain this project"}'
curl -s http://127.0.0.1:7341/v1/runs/<run-id>          # status + events
curl -N http://127.0.0.1:7341/v1/runs/<run-id>/events   # live SSE stream
curl -s -X POST http://127.0.0.1:7341/v1/runs/<run-id>/input \
  -H 'content-type: application/json' -d '{"input": "y"}'   # approve a pause
```

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
forge router serve [--host --port]  Run the local Laya decision-router adapter
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
--router <r>          override the router (static|mock|cheapest|http|laya|needle)
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

At startup, before config resolution, Forge loads `.env.local` then `.env`
from the project root (never overriding the shell environment, so the chain is
shell env → `.env.local` → `.env`). Parse errors warn and never abort startup.
This is where provider keys like `DEEPSEEK_API_KEY` normally live; `FORGE_*`
variables set in `.env` files behave as environment config (origin
`environment`), below real shell vars and CLI flags.

Key settings (all optional):

| Key | Default | Env var | Meaning |
|---|---|---|---|
| `model` | `qwen3-coder` | `FORGE_MODEL` | Active model (`mock-local`/`scripted-mock` = opt-in offline mocks) |
| `mock_script` | — | `FORGE_MOCK_SCRIPT` | JSON script path for `scripted-mock` (project-relative) |
| `model_base_url` | `http://127.0.0.1:8080/v1` | `FORGE_MODEL_BASE_URL` | OpenAI-compatible endpoint (oMLX etc.) |
| `model_key_env` | — | `FORGE_MODEL_KEY_ENV` | Name of the env var holding the API key |
| `router` | `needle` | `FORGE_ROUTER` | `needle` \| `laya` \| `static` \| `cheapest` \| `mock` \| `http` \| `jev` |
| `router_url` | — | `FORGE_ROUTER_URL` | System One-compatible router endpoint (laya default: `http://127.0.0.1:8788/decide`; jev default: `https://api.typesafe.ai/v1/systemone`, or a self-hosted OpenJev server's `/v1/systemone`) |
| `router_key_env` | — | `FORGE_ROUTER_KEY_ENV` | Name of the env var holding the router key (jev default: `TYPESAFE_API_KEY`) |
| `router_escalate` | `auto` | `FORGE_ROUTER_ESCALATE` | `auto` \| `off` — when `router = "needle"`, escalate to the Jev tier before the static fallback once a Jev credential is present (`auto`, the default) or never (`off`); no-op unless a credential exists and `--local-only` is off |
| `router_timeout_ms` | `5000` | — | HTTP/needle/jev router timeout |
| `router_confidence_threshold` | `0.7` | `FORGE_ROUTER_CONFIDENCE_THRESHOLD` | Below this, http/laya/needle/jev decisions escalate to the fallback |
| `router_fallback` | `static` | `FORGE_ROUTER_FALLBACK` | Fallback router (`static` \| `cheapest`) |
| `router_autostart` | `true` | `FORGE_ROUTER_AUTOSTART` | `forge serve` auto-starts the Laya adapter when `router = "laya"` |
| `execution` | `native` | `FORGE_EXECUTION` | `native` \| `mock` |
| `approval` | `prompt` | `FORGE_APPROVAL` | `auto` \| `prompt` \| `prompt-dangerous` \| `deny` |
| `local_only` | `false` | `FORGE_LOCAL_ONLY` | Restrict to local providers |
| `server_host` | `127.0.0.1` | `FORGE_SERVER_HOST` | Server bind address (loopback default) |
| `server_port` | `7341` | `FORGE_SERVER_PORT` | Server port |
| `max_turns` | `25` | `FORGE_MAX_TURNS` | Agent-loop turn budget |
| `needle.variant` | `full` | `FORGE_NEEDLE_VARIANT` | Needle 3 weights ladder (small \| medium \| full); **only `full` has a downloadable artifact today** — Cactus-Compute publishes one 20-layer file, `needle build --layers N` slices smaller ones locally, so `small`/`medium` currently report "no pinned weights artifact" and fall back to static routing. `full` is the default precisely because it's the one that actually fetches; revisit once a smaller rung is hosted |
| `needle.weights_path` | — | — | Weights override; empty → ~/.cache/forge/models/ |
| `needle.autofetch` | `true` | `FORGE_NEEDLE_AUTOFETCH` | `forge init` downloads + verifies weights (~35 MB for `full`) |
| `needle.weights_sha256` | — | `FORGE_NEEDLE_WEIGHTS_SHA256` | Operator override for the expected weights checksum (64 hex chars); empty → use the compiled-in pin. Pairs with `weights_path`/a custom base URL to run your own weights without recompiling |

Unknown keys are tolerated. Inspect the resolved configuration:

```bash
forge config show            # merged effective config
forge config path            # config file locations and which exist
forge config explain model   # winning value + source, e.g. model = "cli-model" (source: cli-flag)
```

## Authentication

Forge uses your existing credentials, in this order:

1. **`key_env` env var** from the model's `[models]` entry (e.g. `DEEPSEEK_API_KEY`).
2. **Conventional env vars** per provider (`ANTHROPIC_API_KEY`,
   `CLAUDE_CODE_OAUTH_TOKEN`, `OPENAI_API_KEY`, `MOONSHOT_API_KEY` /
   `KIMI_API_KEY`) — including ones loaded from `.env`/`.env.local` at startup.
3. **CLI credential stores**: `~/.claude/.credentials.json` (Claude Code OAuth)
   and `~/.codex/auth.json` (its `OPENAI_API_KEY` field).

`forge auth status` shows what was detected — provider, usable models, source,
and kind (api-key/oauth) — never any values. `forge doctor` summarizes the
same in one line.

**Claude subscription**: run `claude login` (or `claude setup-token`) once;
Forge picks up the stored OAuth token automatically and uses it for the
built-in `claude-sonnet` entry (`provider = "anthropic"`). Codex CLI:
`~/.codex/auth.json` with an API key works out of the box; **OAuth-only Codex
subscriptions are not usable yet** (they target the ChatGPT Responses backend,
which is unimplemented — set `OPENAI_API_KEY` for API access). macOS Keychain
credential lookup is not implemented yet.

> **Terms note:** subscription OAuth tokens are intended by providers for
> their own CLIs; using them elsewhere may violate provider terms. API keys
> are the supported path.

**Jev escalation** (decision plane, not generation): `TYPESAFE_API_KEY`
enables the Jev tier (see [DecisionRouter](#decisionrouter) below) — set it
and `router_escalate = "auto"` (the default) starts escalating there once
needle declines or fails. `forge doctor` reports credential detection (env
var name only, never the value) the same way it does for model providers.
No terms caveat applies here: it's a metered API key, not a subscription
OAuth token repurposed from another CLI.

A provider entry looks like:

```toml
[models.claude-sonnet]
provider = "anthropic"
description = "Anthropic Claude (subscription via Claude Code)"
base_url = "https://api.anthropic.com"
key_env = "ANTHROPIC_API_KEY"   # used when set; CLI OAuth store is the fallback
tools = true
max_context = 200000
```

## Models, routing, execution

These are the three pluggable seams (traits in `forge-core`).

### ModelProvider

The default is `qwen3-coder` via the OpenAI-compatible endpoint at
`model_base_url` (oMLX convention). `mock` (offline, deterministic) and
`scripted-mock` (JSON-scripted replies incl. tool calls) exist for tests,
demos, and CI — always explicitly requested. Capabilities (streaming, tools,
structured output, vision, context size) are explicit per provider, never
assumed; a provider without `tools` receives single-turn requests only.

### DecisionRouter

Chooses the model per task and records the decision with a confidence score.
Seven modes:

- `needle` (embedded on-device Needle 3 decision model, no network calls
  once weights are on disk; **default**; `forge init` fetches/verifies
  weights for `needle.variant = "full"`, the one variant Cactus-Compute
  currently publishes as a standalone artifact; real inference needs a build
  with the `needle-ffi` feature — see [Embedded Needle brain
  (`ffi`)](#embedded-needle-brain-ffi) — and falls back to static without
  it, or when weights are unavailable: unpinned variant, `--local-only`,
  no network),
- `jev` (Jev/OpenJev System One decision model — TypeSafe's hosted API at
  `https://api.typesafe.ai/v1/systemone` by default, or a self-hosted
  [OpenJev](https://github.com/razorback16/openjev) server via `router_url`;
  bearer token from `router_key_env`, default `TYPESAFE_API_KEY`; falls back
  to static when unreachable or uncredentialed),
- `laya` (open-source System One decision model via the reference adapter;
  falls back to static when the adapter is down),
- `static` (deterministic rules),
- `mock` (preset decision, for tests),
- `cheapest` (lowest-cost candidate from the `[models]` cost table;
  tie-breaks by output cost then name),
- `http` (System One-compatible: POST `{task, candidates, required_capabilities}`
  to `router_url`, bearer token from `router_key_env`).

`http`/`laya`/`needle`/`jev` decisions below `router_confidence_threshold`
(default 0.7) are rejected and escalate through the fallback chain: any
router is wrapped in a fallback (`router_fallback`, default `static`, may be
`cheapest`), so an unreachable, timing-out, unconfident, or (for `needle`)
not-yet-loaded router degrades to deterministic routing with
`fallback_used: true`.

#### Escalation ladder: needle → Jev → static

With the default `router = "needle"` and `router_escalate = "auto"`, forge
composes a three-tier decision ladder: **needle** (embedded, on-device,
free) tries first; if it declines, errors, or falls below
`router_confidence_threshold`, forge escalates to **Jev** — but *only* when
a `TYPESAFE_API_KEY` (or `router_key_env` override) is actually present at
startup and `--local-only` is off; otherwise escalation is skipped entirely
and today's needle → static behavior is unchanged. If Jev is also
unreachable, uncredentialed-after-all, or unconfident, forge falls through
to **static** (or `router_fallback`, if set to `cheapest`). Every hop is
recorded in the session's routing-decision event (`router`, `confidence`,
`fallback_used`), so a run never blocks on the escalation tier being down.

Setting `router = "jev"` directly makes Jev the primary router (still
threshold-gated, still falling back to `router_fallback`) instead of an
escalation tier behind needle. Either way, `--local-only` prunes Jev
entirely — as primary, it degrades to static with a warning instead of
erroring the build; as an escalation tier, it's simply never wired in. Jev
and Kev-family services also work through the plain `http` backend if
you'd rather speak the flat contract yourself — nothing is hard-coded.

### Model registry with costs

Five entries ship as built-in defaults (`qwen3-coder`, `deepseek-chat`,
`claude-sonnet`, `gpt-5`, `kimi-k2.7-code`; prices as of September 2026 —
prices change; check provider pages). A representative few, shown below
(`claude-sonnet` is shown in [Authentication](#authentication) above):

```toml
[models.qwen3-coder]      # local default, free
description = "local coding model via oMLX (Qwen3-Coder)"
base_url = "http://127.0.0.1:8080/v1"
cost_input_per_mtok = 0.0
tools = true

[models.deepseek-chat]    # DeepSeek V4-class, very low cost
description = "DeepSeek V4-class chat/coding model, very low cost"
base_url = "https://api.deepseek.com/v1"
key_env = "DEEPSEEK_API_KEY"
cost_input_per_mtok = 0.14
cost_output_per_mtok = 0.28
tools = true

[models.kimi-k2.7-code]   # Moonshot, frontier-quality coding
description = "Moonshot Kimi K2.7 Code, frontier-quality coding"
base_url = "https://api.moonshot.ai/v1"
key_env = "MOONSHOT_API_KEY"
cost_input_per_mtok = 0.95
cost_output_per_mtok = 4.00
tools = true
```

The `[models]` table deep-merges by name across user/project config files — a
same-named project entry replaces the built-in entirely (env/CLI flags don't
set entries). The runtime builds routing candidates from it, and when the
router selects a model with a `base_url`, Forge constructs the
OpenAI-compatible provider for it automatically. **Hosted entries are never
called implicitly**: the default active model is local `qwen3-coder`, and
hosted models only run when a router selects them or you set `model`
explicitly. `forge model list` shows the cost table. Cheapest routing with the
built-ins prefers the free local model unless it's capability-ineligible.

### Laya via the reference adapter

Laya (open-source System One decision model) is a Python SDK with no official
server. The easiest way to run it is built into Forge (the adapter script is
embedded in the binary — no repo checkout needed):

```bash
pip install laya
forge router serve                 # 127.0.0.1:8788, foreground, Ctrl-C to stop
forge router serve --port 9000     # custom port/host via --host/--port
```

then set `router = "laya"` (and optionally `router_url`). `forge router serve`
checks prerequisites (`python3`, the `laya` package) through the configured
execution provider and reports actionable typed errors.

Without Forge, the same adapter ships as a plain script:
`python3 adapters/laya-http.py [port] [--host ...]`. Forge POSTs
`{"state": {"task", "required_capabilities"}, "questions": {"model": {"type":
"choice", "instructions": ..., "criteria": {name: description}}}}` and expects
`{"answers": {"model": {"choice", "confidence"}}}`. The adapter is optional;
Forge never requires Python.

### ExecutionProvider

All command/script execution AND file reads/writes/edits/deletes go through
this trait (the runtime never spawns processes or touches files directly).
Risk classification: in-project reads are `Safe`; in-project writes/edits are
`Risky`; deletes, and any operation (including a read) whose path escapes the
project root, are `Destructive` — a read outside the project can exfiltrate a
secret just as effectively as a write can overwrite one, so it gets the same
gating rather than the free pass `Safe` gives every approval policy. `native`
runs locally with approval gating: `Risky` operations pause for approval under
`approval = "prompt"`, while `prompt-dangerous` asks only for `Destructive`
ones (non-interactive stdin → typed "approval required" error, which the agent
loop treats as a pause: answer via piped stdin lines, e.g.
`echo y | forge run ...`). `auto` runs, `deny` blocks. `mock` records requests
for tests. MVM/container/remote executors plug into the same trait later.

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
only files whose mtime+hash changed are re-parsed. The graph itself stays
model-free by design — this never changes even when a needle engine is available.

```bash
forge graph build              # build / incrementally refresh
forge graph check              # fresh (exit 0) or stale (exit 1, lists changes)
forge graph map                # per-directory structural summary
forge graph grep <pattern>     # search symbols and imports
forge graph grep --semantic <query>  # search a local semantic embedding index
forge graph callers <symbol>   # who calls this symbol
forge graph blast <path>       # direct + second-hop importers
forge graph context <query>    # ranked files/symbols for agent context
```

### Semantic index

When a needle engine is genuinely available (weights loaded and answering, not
just constructible — see `forge_needle::engine_if_available`), `forge graph
build` additionally embeds every symbol locally and stores the vectors at
`.forge/graph/embeddings.bin`. Embedding text is `"<kind> <name> in <path>"`;
the index key is `"<path>::<name>"`, so two symbols with the same name in
different files are both independently searchable. Rebuilds are incremental
and content-hash keyed: only symbols whose embedded text actually changed are
re-embedded, in batches of 32; symbols removed from the graph are dropped from
the index too. An index built with a different model or embedding
dimensionality is discarded and rebuilt wholesale rather than mixed with new
vectors. Without a working needle engine, this step is skipped silently — the
build still succeeds, and no `embeddings.bin` is touched.

`embeddings.bin` is a single `serde_json` blob today (an 8-byte `FRGEMB01`
magic prefix + one JSON object), parsed in full on every load. That is fine
at the hash backend's 64 dimensions, but the real `ffi` backend embeds at
3072 dimensions (see [Embedded Needle brain
(`ffi`)](#embedded-needle-brain-ffi)); a project with ~1,000 embedded symbols
would produce a ~45 MB index under that format. A raw little-endian-`f32`
format bump is planned — safe to do later because the magic prefix makes a
version change non-silent (mismatch → clean rebuild, never a misread) — see
the spec's §8 amendment for the follow-up.

`forge graph grep --semantic <query>` embeds the query and returns the top 20
matches by cosine similarity (`score  path::symbol` lines); without a working
engine it fails with `semantic search needs needle weights (run forge init)`
(exit 1) rather than silently falling back to literal search. `forge graph
context <query>` blends the two signals when both an engine and a matching
index exist: `final = 0.5 * (1 / (1 + lexical_rank)) + 0.5 * cosine`; otherwise
its output is exactly the lexical ranking as before. For `--json` consumers:
`score` is always a float — a blended 0-1 value when a needle engine and
matching index both exist, otherwise the raw lexical rank count — whereas the
`POST /v1/project/context` server endpoint always returns the raw lexical
count as an integer today (see [Server](#server); it has not been wired to
the semantic blend yet).

## Server

`forge serve` exposes the same `AgentService` the CLI uses (transport-neutral by
design; gRPC can be added as another adapter). Binds to `127.0.0.1:7341` by
default.

When `router = "laya"` and the router endpoint is unreachable, `forge serve`
auto-starts the embedded Laya adapter as a managed child (`router_autostart`,
default `true`): the adapter script is materialized from the binary, python/laya
prerequisites are checked, the server waits for adapter liveness before
printing the listening line, and Ctrl-C kills the adapter first — one command
brings up the whole stack. The adapter binds its HTTP port immediately and
preloads its model in the background, so routing requests while it loads get a
503 that the fallback router absorbs. Set `router_autostart = false` (or
`FORGE_ROUTER_AUTOSTART=false`) for the old behavior, or use
`forge router serve` to run the adapter standalone.

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

`POST /v1/project/context` is lexical-only today: it calls the same
structural ranking as `forge graph context` but does not (yet) blend in
the semantic index the way the CLI command does — see [Known
limitations](#known-limitations-v03) and the `forge graph context`
paragraph under [Semantic index](#semantic-index) for the score-shape
difference this implies for `--json`/API consumers.

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

### Embedded Needle brain (`ffi`)

`just verify` runs with default features, where the needle router has no
inference engine and degrades to static routing. Real on-device inference is
behind a feature flag because it needs a per-platform native engine that this
repo does not carry:

| crate | feature | effect |
| --- | --- | --- |
| `forge-needle` | `ffi` | `FfiBackend` over `libneedle` instead of `UnavailableBackend` |
| `forge-needle` | `needle-e2e` | enables `tests/e2e.rs` (needs `ffi` + real weights) |
| `forge-cli` | `needle-ffi` | builds the `forge` binary with the above |

The engine ships per platform in the same Apache-2.0 Hugging Face repo as the
weights, [`Cactus-Compute/needle3`](https://huggingface.co/Cactus-Compute/needle3).
Fetch `libneedle.a` for your target once:

```bash
TRIPLE=$(rustc -vV | sed -n 's/^host: //p')      # e.g. aarch64-apple-darwin
mkdir -p crates/needle-sys/vendor/$TRIPLE
curl -L -o crates/needle-sys/vendor/$TRIPLE/libneedle.a \
  https://huggingface.co/Cactus-Compute/needle3/resolve/main/macos-arm64/libneedle.a

# Verify against the pinned checksum (macos-arm64; see the spec's §8 for
# other platforms as they get verified) before trusting the download:
echo "60cc14f1a2eda8da72b75f8f228fb72cadc2850b38702370f43e9660b74e951a  crates/needle-sys/vendor/$TRIPLE/libneedle.a" | shasum -a 256 -c -
```

Substitute the platform folder for your target (`macos-arm64`,
`linux-x86_64`, `linux-arm64`, `linux-armv7`, `linux-riscv64`,
`linux-mipsel`, `windows-x86_64`, `windows-arm64`, `android-arm64`, ...; the
full list is in `crates/needle-sys/build.rs`). `NEEDLE_LIB_DIR=/path/to/dir`
overrides the vendored location. `crates/needle-sys/vendor/` is gitignored;
`needle.h` is committed as the contract of record — `needle-sys` hand-writes
its six `extern "C"` declarations rather than generating them (no `bindgen`, so
no libclang needed to build forge), and a unit test fails if the committed
header ever stops matching those declarations.

Then:

```bash
just verify-ffi   # clippy + unit tests with `ffi` on
just e2e          # real-weights end-to-end suite (release build)

# build the forge binary itself against the real engine
cargo build --release -p forge-cli --features needle-ffi
```

`just verify` already type- and lint-checks the `ffi` code on every run via
`just lint-ffi` — `cargo clippy` never links, so that needs no engine binary.
The recipes above are what additionally *run* it.

`just e2e` needs weights as well as the engine:

```bash
FORGE_NEEDLE_E2E_WEIGHTS=~/.cache/forge/models/needle3.cact just e2e
```

`forge init` puts that file there (35 MB, SHA-256 pinned). The suite asserts
the engine loads, that `decide` picks `test-runner` for "run the tests" with
calibrated confidence, that embeddings are 3072-dimensional, L2-normalised
and deterministic, that `extract` pulls `{"city":"Paris"}` out of prose, that
an unsupported request refuses instead of guessing, and that a warm route
round-trip is not pathologically slow.

On latency: a warm round-trip measures **~47 ms** in a release build on an
idle macos-arm64 machine (~100 ms debug). The suite prints every sample
against that reference but asserts only a loose 2 s ceiling, because
wall-clock latency here tracks machine load far more than it tracks forge —
the same bit-identical inference measured 47 ms idle and 1.3 s at load average
347. Read the printed numbers for drift; the assertion exists to catch gross
regressions (skipping `needle_init` per call once took a round-trip to 16.5 s).

The "no network calls once weights are on disk" claim holds for this backend:
`libneedle.a` has no network-capable symbols at all (`nm -u` shows only libc
maths/memory/stdio, `mmap`, `pthread` and `sysctlbyname`). Needle's own README
mentions engine telemetry, but that lives in its Python SDK and standalone CLI
runner, neither of which forge uses.

Two things worth knowing about the C API, because they shape the code:
`libneedle` is **one process-global, non-thread-safe model that cannot be
unloaded**, so `NeedleEngine` keeps it on a single dedicated thread and
`FfiBackend` takes a process-wide claim (a second engine fails loudly instead
of racing); and for `extract`, Needle takes its semantics from the record's
name and description, so give extraction schemas a `title` (it becomes the
tool name) or a meaningful `description` — a bare `{"type":"object",
"properties":{...}}` is often declined rather than guessed at.

Layout:

```text
crates/
  forge-core        traits, event protocol, typed errors (no heavy deps)
  forge-config      config loading, precedence, provenance
  forge-execution   native + mock execution providers
  forge-providers   mock/scripted + OpenAI-compatible models; static, mock,
                    cheapest, HTTP, and Laya routers
  forge-session     append-only JSONL store + secret redaction
  forge-skills      SKILL.md discovery, progressive disclosure
  forge-graph       deterministic incremental project graph
  forge-needle      embedded Needle brain: decide/embed/extract/tool-call,
                    weights lifecycle, engine thread, needle router
  forge-runtime     AgentService — the one runtime shared by CLI and server
  forge-server      axum REST/SSE adapter
  forge-cli         clap command tree, tracing, the forge binary
  needle-sys        raw FFI declarations for libneedle + its link config
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
  mock providers — fully offline. Currently 20 features / 38 scenarios / 141 steps.
- Needle FFI: `just verify-ffi` and `just e2e` are opt-in and excluded from
  `just verify` — they need a native engine and real weights. See [Embedded
  Needle brain (`ffi`)](#embedded-needle-brain-ffi).

## Known limitations (v0.3)

- `forge resume` seeds the new run with the original prompt and the prior
  (truncated) completion summary; full conversation replay is future work.
- Symbol/call extraction is regex-based; `graph blast` covers two hops.
- Server run-status state is in-memory (bounded at `MAX_TRACKED_RUNS` = 1024,
  terminal-first eviction); the session store persists across restarts.
- Input delivery is in-process: `POST /v1/runs/:id/input` for a run owned by
  another process records the event but that loop does not consume it.
- The needle direct-dispatch fast path is read-only by design (`read_file`,
  `graph_context`, `graph_grep` only); writes, edits, deletes, and commands
  always go through the full agent loop and its approval gating.
- `forge init` builds the project graph's structure but never embeds it (no
  model calls from `init`, ever); run `forge graph build` afterwards to
  populate the semantic index once needle weights are available.
- Needle extraction and fast-path tool-call quality depend on the weights
  variant loaded; only `full` ships a downloadable artifact today, so
  `small`/`medium` quality is untested until Cactus-Compute hosts them.
- Jev-tier escalation (routing across a ladder of models by task difficulty,
  with `extract()`-based argument repair in the agent loop) is not
  implemented yet; it needs real-model quality data first and is deferred to
  a later spec sub-project.
- Default/prebuilt builds don't include the inference engine yet: the
  `needle-ffi` feature is off by default and in release builds, so `forge
  init` skips fetching needle weights entirely in such builds (there is no
  backend to use them) and reports the skip rather than downloading ~35 MB
  that would just sit unused. Build with `--features needle-ffi` (see
  [Embedded Needle brain (`ffi`)](#embedded-needle-brain-ffi)) to get real
  fetch-on-init behavior.
- `POST /v1/project/context` is lexical-only: the semantic blend that
  `forge graph context`/`graph grep --semantic` apply (needle engine +
  embedding index, when both exist) has not been ported to the server
  handler yet. Follow-up: share one `semantic_blend` implementation between
  the CLI and `forge-server` via `forge-runtime`.
- `embeddings.bin`'s whole-file `serde_json` format has a known scale limit
  for `ffi` builds: fine at the hash backend's 64 dimensions, but the real
  engine embeds at 3072 dimensions, where a project with ~1,000 symbols
  would produce a ~45 MB index parsed in full on every load. A raw
  little-endian-`f32` format bump is planned; deferred for now because the
  `FRGEMB01` magic prefix makes that change safe to land later (a version
  bump triggers a clean rebuild, never a misread).
- `forge model test` reports generation-plane (`ModelProvider`) health only;
  it has no needle/decision-plane status yet (`forge doctor`'s needle probe
  is the current way to check that). Skill selection
  (`SkillRegistry::match_task`) is lexical word/substring matching, not the
  pre-embedded, task-embedding-ranked selection the design describes —
  both are deferred to the same follow-up as the semantic-blend sharing
  above.
