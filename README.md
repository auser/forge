# Forge

Forge is a single-binary Rust coding agent that separates **deciding** from
**generating**. The fast, cheap, typed decisions — which model should handle
this task, which tool to call and with what arguments, whether that call is
safe to run — are made by a small calibrated decision model running
on-device; a general language model is asked only for the work that actually
needs one. No Node.js, no database, no daemon: one binary, one config file,
and whatever model endpoints you already have.

Splitting the two is the whole point. A chat model asked to choose is slow,
expensive, and unaccountable about its own uncertainty; a decision model
returns a typed choice with a confidence score in milliseconds, for free,
without leaving the machine.

## The decision plane: Needle 3, then Jev, then rules

Every routing and guardrail question forge asks is a System One decision — a
choice from a fixed set, with a confidence, never generated prose. Three tiers
answer, cheapest first:

1. **Needle 3, on-device.** The default (`router = "needle"`): an embedded
   decision model from [Cactus Compute](https://huggingface.co/Cactus-Compute/needle3)
   (Apache-2.0, ~35 MB of weights) running inside the forge process, with no
   network calls once its weights are on disk. Free, and fast enough to be
   invisible — the end-to-end suite's reference for a warm route round-trip is
   ~47 ms in a release build on an idle macos-arm64 machine. Needle is also
   what *fills* tool calls: name and arguments, against the same tool schemas
   the language model would have been handed.
2. **Jev**, when the on-device brain is unsure. TypeSafe's hosted System One
   API, or a self-hosted [OpenJev](https://github.com/razorback16/openjev)
   speaking the same wire protocol. It is consulted only when needle declines,
   errors, or lands below `router_confidence_threshold` **and** a Jev
   credential (`TYPESAFE_API_KEY`, or `jev_key_env`) is actually present and
   `--local-only` is off. With no credential, `router_escalate = "auto"` — the
   default — is a no-op and nothing leaves the machine.
3. **Deterministic static rules**, as the floor that cannot fail. They need no
   model, no weights, and no network. So an absent, unloaded, unreachable or
   unconfident brain degrades to rules (`fallback_used: true` in the event log)
   and the run continues. Laya and any other System One-compatible HTTP router
   remain available as alternates.

What this buys is two things you can point at:

**Faster answers.** A well-defined read-only request is answered with *no LLM
call at all*. Needle picks the tool and fills its arguments, a second
on-device check confirms the call is non-destructive, and forge dispatches it
directly through the normal approval-gated execution path — the run records
`router: "needle-dispatch"` with `turns: 0`. The exact gates are in
[Usage](#usage); the path is read-only by construction, so it can never
prompt you and can never do something a normal run could not.

**Better choices.** Which model runs, and whether a tool call is safe, are
decided by a model calibrated to emit a choice plus a confidence — not by a
chat model guessing in prose. Anything under the threshold is rejected and
escalated rather than acted on.

(Skill activation is still lexical matching over skill names and
descriptions, not a needle decision. See [Skills](#skills).)

**Whether you actually have a brain depends on the build.** Real on-device
inference needs the native engine, which sits behind the `needle-ffi` cargo
feature: the prebuilt release binaries for `aarch64-apple-darwin` and
`aarch64-unknown-linux-gnu` are built with it (so the installed default on those
platforms is a working brain), while a plain `cargo build`, Intel macOS, x86_64
Linux and Windows get a statically-routing binary.
That is a supported configuration and fully usable, not a degraded one: you
get deterministic static routing instead of the on-device model, and you give
up the direct-dispatch fast path and the local semantic index
(`graph grep --semantic`) — the agent loop, tools, approvals and sessions are
unchanged. `forge doctor` tells you which you have on its
`needle engine` / `needle brain` lines, and names the one command that
changes it. Details: [Installation](#installation) and [Embedded Needle brain
(`ffi`)](#embedded-needle-brain-ffi).

## The generation plane: local first, cloud when it is earned

Generation goes to any OpenAI-compatible server you already run — oMLX,
llama.cpp, Ollama, LM Studio — and that is the default out of the box
(`qwen3-coder` at `http://127.0.0.1:8080/v1`). Cloud models sit in the
candidate registry but are **never called implicitly**: one runs only when the
decision plane selects it or you name it with `--model`, and only when a
credential for it actually exists.

Those credentials are taken from where they already live: API-key environment
variables, including ones loaded from `.env`/`.env.local` (`DEEPSEEK_API_KEY`,
`MOONSHOT_API_KEY`/`KIMI_API_KEY`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`), and
the CLI credential stores — `~/.claude/.credentials.json` for a Claude
subscription you logged into with `claude login`, and `~/.codex/auth.json`
when it holds an API key. See [Authentication](#authentication) for the full
order and the caveats (OAuth-only Codex subscriptions are not usable yet).

`--local-only` means what it says: forge refuses to build a model provider
whose endpoint is off this machine (including one a router picked), prunes the
Jev tier in both roles, degrades an off-device `http`/`laya` router to static,
and skips the weights fetch. It is not a sandbox — tools and hooks you run are
still your own. [What `--local-only`
restricts](#what---local-only-restricts) has the exact line between "local"
and "remote".

## Where it runs

One shared runtime, four front ends: the `forge` CLI, a REST/SSE server
(`forge serve`), an MCP tool server (`forge mcp` — Claude Code, VS Code,
Cursor), and a native in-editor agent over the Agent Client Protocol
(`forge acp` — Zed). Same routing, same approval policy, same session log,
whichever one you use.

## Where to read more

- **How it all fits together** — the decision plane (Needle → Jev/OpenJev →
  static), the generation plane (local → subscription → API-key cloud), the
  fast path, and every trait seam: [`ARCHITECTURE.md`](ARCHITECTURE.md)
- Project spec: [`specs/project.md`](specs/project.md)
- Architecture decisions: [`specs/adrs/`](specs/adrs/) (start with `0001-core-architecture.md`)
- Implementation plan: [`specs/implementation-plan.md`](specs/implementation-plan.md)
- Roadmap and future directions: [`specs/roadmap.md`](specs/roadmap.md)
- Needle/Jev embedded-brain design (on-device decisions, cheapest-first
  routing; MCP editor integration is [live](#mcp-server-editors-and-agent-harnesses),
  as is [ACP](#in-your-editor-acp)):
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

**The embedded brain comes with it** on `aarch64-apple-darwin` and
`aarch64-unknown-linux-gnu` — the two platforms whose on-device engine forge has
both checksum-verified *and* link-verified. Release assets for those targets are
built with `needle-ffi`, CI asserts each one really has the engine before
publishing, and the `cargo install` fallback adds the feature too (retrying
without it if the engine cannot be fetched, so a bad network never costs you the
install). Intel macOS, x86_64 Linux and Windows get a statically-routing binary,
because no linkable engine exists for them yet (see [Embedded Needle brain
(`ffi`)](#embedded-needle-brain-ffi) for exactly why each); `forge doctor` says
which one you have on its `needle engine` line.

Brain-enabled assets need nothing extra installed to run — the C++ runtime is
linked statically on Linux and ships with the OS on macOS, so they depend on
exactly what a brain-less build does. Full detail:
[Embedded Needle brain (`ffi`)](#embedded-needle-brain-ffi).

## Quickstart

Both planes have working defaults, so setting forge up is picking a
generation model and nothing else.

### Three commands to a working agent

```bash
curl -fsSL https://raw.githubusercontent.com/auser/forge/main/install.sh | bash   # 1. install
cd /path/to/your/project && forge init                                            # 2. set up
forge run "Explain this project"                                                  # 3. go
```

What each does:

1. **install** — one static binary into `~/.local/bin` (see
   [Installation](#installation) for Cargo/local-checkout variants).
2. **`forge init`** — writes `.forge/config.toml` (starter config: local
   model + embedded on-device router), builds the project graph at
   `.forge/graph/` (deterministic, no model calls), adds `.forge/` to
   `.gitignore`, and — on a build that has the inference backend, which the
   release binaries for macOS arm64 and Linux arm64 do — fetches +
   checksum-verifies the ~35 MB brain weights into `~/.cache/forge/models/`.
   On a build without the backend it skips that fetch (nothing could use the
   weights) and prints the one command that gets you one. Idempotent — safe
   to re-run any time.
3. **`forge run "…"`** — the multi-turn agent loop against whatever model
   you picked below.

Then, any time something looks wrong: **`forge doctor`** — it probes the
model endpoint, the router, the embedded brain, and every credential env
var your config names, and tells you which line to change.

### Pick your model (10 seconds)

Exactly one of these, whichever you already have:

**(a) A local server you already run** — Ollama, LM Studio, llama.cpp, oMLX:

```toml
# .forge/config.toml
model = "qwen3-coder"                          # the name your server serves
model_base_url = "http://127.0.0.1:8080/v1"    # include the /v1 prefix
# model_key_env = "MY_API_KEY"   # ONLY if your server requires a key
```

Add `model_key_env` **only** if your server actually demands a key: it names
an env var, never holds a key, and naming one that is unset is the single
most common first-run failure.

**(b) A subscription you already pay for** — Claude:

```bash
claude login      # once; forge picks up the stored token automatically
forge --model claude-sonnet run "Explain this project"
```

Nothing to configure: the `claude-sonnet` entry ships built in, and
`forge auth status` shows what was detected (never any values).

**(c) An API key** — DeepSeek, Moonshot, OpenAI, …:

```bash
echo 'DEEPSEEK_API_KEY=sk-...' >> .env    # loaded at startup, redacted from logs
forge --model deepseek-chat run "Explain this project"
```

`forge init` reports the keys it found by name; built-in `[models]` entries
exist for `deepseek-chat`, `kimi-k2.7-code`, `gpt-5`, and `claude-sonnet`.

**Just evaluating?** A lot of forge needs no model at all. With zero setup:

```bash
forge init                         # builds the project graph
forge graph context "auth flow"    # ranked files for a task
forge graph map                    # what's in this repo
forge skill list                   # discovered skills
forge doctor                       # what's configured, what's missing
forge mcp                          # serve those as tools to your editor
forge acp                          # or be the agent in your editor (Zed)
```

Running an agent loop (`forge run`) does need a model — that is the
ten-second menu above.

### Troubleshooting

`forge doctor` is always the first move — it names the config line to change.
`forge version` is the second, if the installed binary might be stale.

**`no credential found for model …` then `401 Unauthorized: API key required`**

```text
WARN no credential found for model Qwen3-Coder-Next-4bit; tried env vars and CLI credential
     stores (see `forge auth status`) — hint: set OMLX_API_KEY in your shell or .env, or
     remove model_key_env if the endpoint needs no key
error: model provider error: model endpoint http://127.0.0.1:8080/v1/chat/completions
       returned 401 Unauthorized: {"error":{"message":"API key required",...}} — hint: set
       OMLX_API_KEY in your shell or .env, or remove model_key_env if the endpoint needs no key
```

Cause: your config names a key env var (`model_key_env`, or `key_env` on the
active `[models]` entry) that is unset, so requests go out unauthenticated.
Two fixes, pick the one that's true:

```bash
export OMLX_API_KEY=...                       # the server does want a key
# or delete the `model_key_env` line          # the server wants none
```

**`primary router failed; using fallback … laya router endpoint … returned 500`**

```text
WARN primary router failed; using fallback error=router error: laya router endpoint
     http://127.0.0.1:8788/decide returned 500 Internal Server Error — hint: laya is
     no longer the default — delete the `router` line to use the embedded needle brain,
     or run `forge router serve` to serve laya
```

Cause: `router = "laya"` is a legacy setting — the default is now the
embedded needle brain, and nothing is serving the laya endpoint. The run
still worked (the fallback is by design), but you're paying a failed request
per decision. Two fixes:

```bash
# delete the `router = "laya"` line from .forge/config.toml   # use the built-in brain
forge router serve                                            # or keep laya, and serve it
```

**`[models.x] has model_base_url, which does nothing inside a model entry`** —
inside a `[models.<name>]` entry the fields are `base_url` and `key_env`; the
`model_`-prefixed spellings are top-level keys only. Rename them.

**Weights/brain notes.** `forge init` fetches and verifies Needle's weights
when `needle.autofetch` is on (the default) and the build has the
`needle-ffi` feature — a one-time ~35 MB download for `needle.variant =
"full"` (the only variant with a hosted, pinned artifact today;
`small`/`medium` report "no pinned weights artifact"), cached under
`~/.cache/forge/models/`; re-running `init` re-verifies the checksum and
skips the download if it already matches. Whenever weights aren't present
(no network, `--local-only`, a build without `needle-ffi`, or a
variant with nothing to fetch), routing falls back to deterministic static
routing (`fallback_used: true` in the events) and the run proceeds with the
configured model — a fully supported, fully offline mode, not a degraded one.

**Which of those it is, forge tells you in one place.** `forge doctor` reports
the brain as a line-pair — the backend and the weights on the first line, the
verdict and the single command that changes it on the second:

```text
[warn] needle engine: backend not in this build (`needle-ffi` off); weights not fetched (…) — nothing here could use them
[warn] needle brain: inactive — falling back to static routing; install a build with the brain: `cargo install …`
```

A build *with* a backend and no weights says `run \`forge init\`` instead,
because there that is the fix. The two never both fire: the remedy always
matches the precondition that actually failed, so `forge init`, a failed route
and `forge doctor` cannot send you around in a circle.

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
cp examples/configs/local-first.toml .forge/config.toml
```

See [`examples/configs/`](examples/configs/) for `local-first` (the default
stack, made explicit), `hybrid-needle` (local first, hosted escalation),
`budget-hosted`, `offline-eval`, and `hybrid-laya` (the Laya-adapter
example) presets, plus an [`env.example`](examples/env.example) template for
provider keys. A unit test parses every preset against the current config
schema, so a preset never drifts out of date.

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
forge resume <run-id>        # continues the run, replaying its conversation
forge session list           # what happened, per session
forge session show <id>      # full event history (JSONL, one event per line)
forge session fork <id>      # branch the conversation into a new session
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
forge mcp                           Serve MCP over stdio (editors, agents)
forge acp                           Serve ACP over stdio (forge as the agent
                                    in Zed and other ACP editors)
forge resume <run-or-session-id>    Continue a completed run in its session
forge cancel <run-or-session-id>    Cancel a run (in-flight or recorded)
forge session [list|show <id>]      Inspect sessions (JSONL event logs)
forge session fork <id> [--at X]    Branch a session into a new one
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
--router <r>          override the router (needle|jev|laya|http|static|cheapest)
--execution <p>       override the execution provider (native)
--local-only          refuse any provider or router endpoint that is not on
                      this machine (see "What --local-only restricts")
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
| `model` | `qwen3-coder` | `FORGE_MODEL` | Active model |
| `model_base_url` | `http://127.0.0.1:8080/v1` | `FORGE_MODEL_BASE_URL` | OpenAI-compatible endpoint (oMLX etc.). **Setting this overrides *every* model's endpoint**, including hosted `[models]` entries like `claude-sonnet` and `gpt-5` — setting it to the default value still counts as setting it. Per-model endpoints belong in `[models.<name>] base_url`. Combinations that cannot work are refused at startup, naming both settings: an `anthropic`-family model whose endpoint ends in `/v1` (the client appends `/v1/messages`, so it would 404), or an entry whose `provider` contradicts the endpoint (`provider = "anthropic"` pointed at `api.openai.com`) |
| `model_key_env` | — | `FORGE_MODEL_KEY_ENV` | Name of the env var holding the API key |
| `router` | `needle` | `FORGE_ROUTER` | `needle` \| `jev` \| `laya` \| `http` \| `static` \| `cheapest` |
| `router_url` | — | `FORGE_ROUTER_URL` | System One-compatible router endpoint (laya default: `http://127.0.0.1:8788/decide`). Also used by `router = "jev"` as primary if `jev_url` is unset (backwards-compat only — **never** consulted by the Jev escalation tier; see `jev_url`) |
| `router_key_env` | — | `FORGE_ROUTER_KEY_ENV` | Name of the env var holding the router key. Same primary-only fallback rule as `router_url` — the escalation tier never uses it (see `jev_key_env`) |
| `router_escalate` | `auto` | `FORGE_ROUTER_ESCALATE` | `auto` \| `off` — when `router = "needle"`, escalate to the Jev tier before the static fallback once a Jev credential is present (`auto`, the default) or never (`off`); no-op unless a credential exists and `--local-only` is off |
| `jev_url` | — | `FORGE_JEV_URL` | Jev/OpenJev endpoint, default `https://api.typesafe.ai/v1/systemone` (or a self-hosted OpenJev server's `/v1/systemone`). Scoped separately from `router_url` so a leftover http/laya `router_url` can never be hijacked into carrying the Jev credential to the wrong host — the escalation tier resolves **only** `jev_url` (or the default); `router = "jev"` as primary also accepts `router_url` as a secondary fallback |
| `jev_key_env` | — | `FORGE_JEV_KEY_ENV` | Name of the env var holding the Jev credential, default `TYPESAFE_API_KEY`. Same scoping as `jev_url`: the escalation tier never falls back to `router_key_env` |
| `router_timeout_ms` | `5000` | — | HTTP/needle/jev router timeout |
| `router_confidence_threshold` | `0.7` | `FORGE_ROUTER_CONFIDENCE_THRESHOLD` | Below this, http/laya/needle/jev decisions escalate to the fallback |
| `router_fallback` | `static` | `FORGE_ROUTER_FALLBACK` | Fallback router (`static` \| `cheapest`) |
| `router_autostart` | `true` | `FORGE_ROUTER_AUTOSTART` | `forge serve` auto-starts the Laya adapter when `router = "laya"` |
| `execution` | `native` | `FORGE_EXECUTION` | `native` |
| `approval` | `prompt` | `FORGE_APPROVAL` | `auto` \| `prompt` \| `prompt-dangerous` \| `deny` |
| `local_only` | `false` | `FORGE_LOCAL_ONLY` | Restrict to local providers — a model whose endpoint is not local is refused at construction, and network decision routers are pruned. See [What `--local-only` restricts](#what---local-only-restricts) |
| `server_host` | `127.0.0.1` | `FORGE_SERVER_HOST` | Server bind address (loopback default) |
| `server_port` | `7341` | `FORGE_SERVER_PORT` | Server port |
| `max_turns` | `25` | `FORGE_MAX_TURNS` | Agent-loop turn budget |
| `needle.variant` | `full` | `FORGE_NEEDLE_VARIANT` | Needle 3 weights ladder (small \| medium \| full); **only `full` has a downloadable artifact today** — Cactus-Compute publishes one 20-layer file, `needle build --layers N` slices smaller ones locally, so `small`/`medium` currently report "no pinned weights artifact" and fall back to static routing. `full` is the default precisely because it's the one that actually fetches; revisit once a smaller rung is hosted |
| `needle.weights_path` | — | — | Weights override; empty → ~/.cache/forge/models/ |
| `needle.autofetch` | `true` | `FORGE_NEEDLE_AUTOFETCH` | `forge init` downloads + verifies weights (~35 MB for `full`) |
| `needle.weights_sha256` | — | `FORGE_NEEDLE_WEIGHTS_SHA256` | Operator override for the expected weights checksum (64 hex chars); empty → use the compiled-in pin. Pairs with `weights_path`/a custom base URL to run your own weights without recompiling |

Unknown keys are tolerated, with one deliberate exception: inside a
`[models.<name>]` entry, `model_base_url`/`model_key_env` are rejected by
name (they are top-level keys; the in-entry fields are `base_url`/`key_env`).
Tolerating them would mean an endpoint that silently never applies.

Inspect the resolved configuration:

```bash
forge config show            # merged effective config
forge config path            # config file locations and which exist
forge config explain model   # winning value + source, e.g. model = "cli-model" (source: cli-flag)
```

### What `--local-only` restricts

`local_only = true` (`FORGE_LOCAL_ONLY=1`, `--local-only`) is enforced where
a configured endpoint becomes an HTTP client, not merely where routing
decisions are made:

- **Model providers.** A model whose resolved endpoint is not local is
  refused at construction with a typed config error naming the model, the
  URL and the config line that set it — including a model a *router* picked,
  since routed names resolve through the same code. So the hosted `[models]`
  entries (`claude-sonnet`, `gpt-5`, `deepseek-chat`, `kimi-k2.7-code`) simply
  cannot be used while it is on, unless you point one at a local endpoint.
- **Every redirect hop.** Checking the configured URL alone would not be
  worth much: an approved loopback endpoint that answers `307` with a
  `Location` elsewhere would otherwise make forge re-POST your prompt, body
  intact, to a host nothing ever checked. Under `local_only` every hop is
  re-checked and a non-local one is refused, naming the host it declined.
  Loopback-to-loopback redirects still work, and a local endpoint redirecting
  in a circle stops after 10 hops exactly as it does with the setting off.
- **Decision routers.** `router = "jev"` degrades to `static`, and the Jev
  escalation tier behind `needle` is pruned — in both roles, unconditionally,
  since a decision router is handed your task text. `http` and `laya` degrade
  to `static` too **when their endpoint is off-device**; a loopback laya
  adapter (its default, `http://127.0.0.1:8788/decide`, the one `forge serve`
  auto-starts) keeps working, because it sends nothing off the machine. A
  `router_fallback` pointed off-device degrades the same way rather than
  failing the command.
- **`forge init`** skips the Needle weights download, and `forge serve` does
  not probe or auto-start an adapter at an off-device address.

"Local" means **this machine**: loopback (`127.0.0.0/8`, `::1`, and the
IPv4-mapped form `::ffff:127.0.0.1`), the unspecified addresses `0.0.0.0` and
`::`, the exact name `localhost`, or a hostless `unix:`/`file:` socket path.
Deliberately *not* local: private-range LAN addresses (`10/8`, `172.16/12`,
`192.168/16`, `169.254/16`), mDNS `*.local` names, subdomains of `localhost`,
a `file://host/…` URL that carries an authority, and anything that does not
parse as a URL with a host. A LAN address is off-device — another host,
another administrator, usually a plaintext wire — and `local_only` is read as
"my code stays on my machine", so the stricter reading is the honest one. To
use the GPU box down the hall, leave `local_only` off.

What it does **not** do:

- **It is not a sandbox.** Tools, hooks, MCP servers and build commands that
  *you* run can still reach the network; `local_only` governs where forge
  itself sends your code.
- **It does not filter routing candidates.** A router may still *select* a
  hosted `[models]` entry; the provider then refuses and that run fails with
  a typed config error. Loud, not leaky — but it is a failed run, and until
  candidate pruning lands, `model = "<a local model>"` plus
  `router = "static"` is the configuration that never hits it.
- **It trusts `localhost` by name.** Forge does not resolve it, so a modified
  `/etc/hosts`, `HOSTALIASES`, or NSS resolver module can point it off-device
  unnoticed. Accepting the name is a usability call (editing those needs
  root); use `127.0.0.1` if you need the guarantee to survive a hostile
  resolver.
- **It cannot see through a local proxy.** If the loopback server you point
  at forwards upstream, that is outside forge's control.

`forge doctor` reports the setting, and its `model provider` line fails —
with the same wording the run would fail with — when your configured model
contradicts it.

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

**Jev escalation** (decision plane, not generation): `TYPESAFE_API_KEY` (or
`jev_key_env` override) enables the Jev tier (see
[DecisionRouter](#decisionrouter) below) — set it and `router_escalate =
"auto"` (the default) starts escalating there once needle declines or
fails. This credential env var, and the Jev endpoint (`jev_url`), are
scoped separately from the generic `router_url`/`router_key_env` so a
leftover `http`/`laya` router configuration can never receive the Jev
credential or redirect it to the wrong host. `forge doctor` reports
credential detection (env var name only, never the value) the same way it
does for model providers. No terms caveat applies here: it's a metered API
key, not a subscription OAuth token repurposed from another CLI.

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
`model_base_url` (oMLX convention).
Capabilities (streaming, tools,
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
  [OpenJev](https://github.com/razorback16/openjev) server via `jev_url`;
  bearer token from `jev_key_env`, default `TYPESAFE_API_KEY`; as primary,
  also accepts `router_url`/`router_key_env` as a fallback; falls back to
  static when unreachable or uncredentialed),
- `laya` (open-source System One decision model via the reference adapter;
  falls back to static when the adapter is down),
- `static` (deterministic rules),
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
a `TYPESAFE_API_KEY` (or `jev_key_env` override) is actually present at
startup and `--local-only` is off; otherwise escalation is skipped entirely
and today's needle → static behavior is unchanged. The escalation tier's
endpoint/credential come from `jev_url`/`jev_key_env` (or the compiled-in
defaults) **only** — never from `router_url`/`router_key_env`, which in
`needle` mode belong to no router at all and, if left over from an earlier
`http`/`laya` setup, would otherwise silently carry the Jev credential to
the wrong host. If Jev is also unreachable, uncredentialed-after-all, or
unconfident, forge falls through to **static** (or `router_fallback`, if
set to `cheapest`). Every hop is recorded in the session's
routing-decision event (`router`, `confidence`, `fallback_used`, `reason`
— the fallback chain's `reason` names which tier actually failed), so a
run never blocks on the escalation tier being down.

Setting `router = "jev"` directly makes Jev the primary router (still
threshold-gated, still falling back to `router_fallback`) instead of an
escalation tier behind needle. Either way, `--local-only` prunes Jev
entirely — as primary, it degrades to static with a warning instead of
erroring the build; as an escalation tier, it's simply never wired in. That
holds even for a loopback self-hosted OpenJev: the promise shipped
unconditional, and `local_only` is not where a confidentiality promise gets
relaxed. An `http`/`laya` router, by contrast, is judged on its endpoint —
see [What `--local-only` restricts](#what---local-only-restricts). Jev
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
server. It is **not** the default any more — the embedded needle brain is —
so `router = "laya"` always needs a process serving the endpoint, and
`forge init`/`forge doctor` both flag the setting if they find it. The
easiest way to run it is built into Forge (the adapter script is embedded in
the binary — no repo checkout needed):

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
`echo y | forge run ...`). `auto` runs, `deny` blocks. The test-only `mock`
*execution* provider records requests
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

## MCP server (editors and agent harnesses)

One command wires forge into Claude Code:

```bash
claude mcp add forge -- forge mcp
```

That's it. Your agent can now search this project's graph, read its skills, and
run forge tasks as tools.

Any other MCP client wants the same two facts — command `forge`, argument `mcp`:

```json
{
  "mcpServers": {
    "forge": {
      "command": "forge",
      "args": ["mcp"]
    }
  }
}
```

(VS Code: `.vscode/mcp.json`. Cursor: `.cursor/mcp.json` or the global
`~/.cursor/mcp.json`. Add `"args": ["--project", "/path/to/repo", "mcp"]` if the
client does not launch the server in your repository.)

### The tools

```text
forge_graph_context {query, limit?}     ranked files for a task (semantic blend
                                        when needle weights + index exist)
forge_graph_grep    {pattern, semantic?} symbol matches by regex or by meaning
forge_graph_map     {}                  per-directory structure summary
forge_skill_list    {}                  skill names + descriptions (metadata only)
forge_skill_show    {name}              a skill's full instructions
forge_doctor        {}                  the `forge doctor` checks as JSON
forge_run           {prompt, max_turns?, timeout_ms?}
                                        run the agent loop; returns
                                        {run_id, status, text}
forge_run_status    {run_id}            status + final text + recent events
forge_run_input     {run_id, input}     answer a waiting run (approvals)
forge_run_cancel    {run_id}            cancel a run
```

Results come back as one text item of compact JSON, mirrored in
`structuredContent`. Failures a model can act on (unbuilt graph, unknown run,
missing weights) are tool errors with the fix in the message, not protocol
errors.

### Approvals

`forge mcp` is non-interactive by definition: stdin is the protocol channel, so
nothing can prompt on it. Under `approval = "prompt"`, a risky operation parks
the run instead — `forge_run` (or `forge_run_status`) reports
`status: "waiting_for_approval"`, and the client approves with

```json
{ "name": "forge_run_input", "arguments": { "run_id": "...", "input": "y" } }
```

Anything other than `"y"` denies. `forge_run` waits `timeout_ms` (default
120000) for a run to finish; if the run outlives that, it keeps going and you
poll `forge_run_status`.

### Notes

* Same runtime as everything else: one `AgentService`, the same routing,
  execution, approval policy and session recording. Runs started here appear in
  `forge session list` and are redacted like any other.
* Global flags work as usual (`--project`, `--model`, `--router`, `--local-only`,
  `--approval`). There is no host/port: stdio only, so the trust boundary is the
  process — same machine, same user.
* stdout carries the protocol and nothing else; all logs go to stderr (`-v`,
  `-vv`, `-vvv` are safe to add). `--json` is meaningless here.
* Both MCP eras are served from one process: the `initialize` handshake
  (revisions `2025-11-25` and earlier) and the current `2026-07-28` stateless
  style with per-request `_meta` and `server/discover`. Framing is
  newline-delimited JSON-RPC, per the stdio binding.

## In your editor (ACP)

Drop this in Zed's `settings.json` and forge shows up in the agent panel:

```json
{
  "agent_servers": {
    "Forge": {
      "type": "custom",
      "command": "forge",
      "args": ["acp"],
      "env": {}
    }
  }
}
```

That's it. Open the agent panel, pick Forge, and type.

Where `forge mcp` hands *your* editor's agent a box of forge tools, `forge acp`
hands the editor **forge itself as the agent** — over the
[Agent Client Protocol](https://agentclientprotocol.com), which Zed, and a
growing set of other editors, speak natively.

### What you get

* **Answers as they land.** The reply arrives as soon as the turn ends, and
  forge's own thinking shows up along the way: which model the router picked
  (including the instant on-device fast path, which reads as
  `Routing via needle`), and which skill activated.
* **Tool calls you can watch.** Every read, edit, command and search appears as
  its own entry with a live status — pending, running, done, failed. Calls that
  touch a file carry its path, so Zed can follow along and open what forge is
  working on.
* **Permission prompts in the editor.** Under `approval = "prompt"`, a risky
  operation stops and asks *in Zed's UI* — Allow or Reject, on the tool call
  itself. Nothing to type in a terminal you can't see.
* **Cancel that works.** Hit stop and the turn ends; the run is cancelled in
  forge too, marker file and all, so nothing keeps running behind your back.
* **The same sessions as everywhere else.** An editor session *is* a forge
  session: the id Zed uses is the one `forge session show <id>` reads and
  `forge resume <id>` continues.

### Notes and current limits

* **forge reads and writes files itself**, in the project directory the editor
  opened the session for — not through the editor. So edits land on disk, which
  means unsaved buffers in Zed are not visible to forge, and you'll want to save
  before asking about a file. (Bridging editor buffers is a planned follow-up;
  `initialize` honestly advertises that we don't use the client's filesystem.)
* **No token-by-token streaming yet.** forge's loop produces a finished answer
  rather than a token stream, so the reply arrives as one message rather than
  typing itself out. Tool calls, by contrast, *are* live. We'd rather ship the
  honest version than chop up finished text to imitate a stream.
* **Text prompts only** — no images or audio, and `initialize` says so rather
  than accepting them and dropping them on the floor. File mentions work
  either way: whether your editor sends a link or the file's contents, forge
  reads it.
* Global flags work as usual, in `args`: `["--project", "/path/to/repo", "acp"]`,
  `--model`, `--router`, `--local-only`, `--approval`. stdio only, so the trust
  boundary is the process — same machine, same user, no network listener and no
  authentication.
* stdout carries the protocol and nothing else; logs go to stderr (`-v`, `-vv`,
  `-vvv` are safe to add). `--json` is meaningless here.
* Protocol version 1, framed as newline-delimited JSON-RPC 2.0.

## Sessions and events

Every run appends versioned events (`"v": 3`, with a monotonic per-run `seq`
assigned by the session store on append) to
`.forge/sessions/<session_id>.jsonl` — one JSON object per line, append-only.
v1 logs (no `seq`, f32 confidence) and v2 logs remain readable.

The log carries two streams, deliberately separated:

* **Observability** — short, human- and editor-facing:
  `run_started`, `routing_decision_made`, `skill_activated`,
  `tool_call_requested`, `tool_started`, `tool_completed`, `file_changed`,
  `approval_requested`, `approval_decided`, `turn_completed`,
  `input_received`, `note` (v1 compat), `error`, `cancelled`, `completed`.
* **Replay** (v3) — the model conversation, verbatim, so it can be
  reconstructed later: `assistant_message` (one per model response: its text
  and the tool calls it requested), `tool_result` (each tool's output as the
  model saw it, capped at 64 KiB with an explicit truncation marker), and
  `session_forked` (fork provenance).

Events carry run/session IDs, provider, model, routing confidence, and
fallback flags. Secret-looking values (API-key patterns, `Bearer` tokens,
values of `*KEY*`/`*TOKEN*`/`*SECRET*`/`*PASSWORD*` env vars) are redacted to
`[REDACTED]` before anything is written — replay payloads included.

```bash
forge session list        # sessions with event counts
forge session show <id>   # full event history, numbered for --at
forge session fork <id>   # branch: a new session holding a copy of this
                          # session's history (--at cuts it short)
forge resume <id>         # continue a completed run: a new run in the same
                          # session, with the session's whole conversation
                          # replayed as the model's history
```

### Forking a session

```bash
forge session fork <session-id>             # branch from the whole history
forge session fork <session-id> --at 12     # 1-based log position
forge session fork <session-id> --at <run>  # after a particular run
```

A fork is a **prefix copy**: the new session file holds the source's lines
verbatim (original `v`, `seq` and timestamps included) plus one
`session_forked` marker. The source is never touched, and the fork is a
normal session afterwards — resumable, cancellable, forkable again — with no
reference back to its parent. The price is disk, paid once per fork; the
gain is that no session can be broken by anything happening to another.

* `--at` inside a run **snaps forward** to that run's end (`forge session
  show` numbers its output with the positions `--at` takes). A half-run prefix
  would replay as an assistant tool call with no result, which is not a state
  any model should be handed.
* Copying means a run id can exist in two sessions. `forge resume <run-id>`
  then resolves to the older session (the source); name the fork's *session*
  id to continue the fork.

### Attaching to a run

`AgentService` exposes the runtime primitives an interactive front end needs
(no CLI surface yet — Phase B):

* `attach(run_id)` — a run's events so far **and** the ones still to come,
  in one call. The live subscription is taken before the stored backlog is
  read and the overlap is removed by `seq`, so joining late loses nothing and
  sees nothing twice. A finished run attaches to its backlog alone.
* `list_runs()` — every live run (`running` / `waiting_for_approval`) plus
  the 20 most recent finished ones, newest activity first.
* `RunState` (in `forge-core`) is the typed discriminant the ACP and MCP
  adapters classify by, instead of matching error text:
  `running`, `waiting_for_approval`, `completed`, `cancelled`,
  `awaiting_approval` (the loop stopped because a risky operation needed an
  answer that could not arrive), `failed`.

Per-run tracking (input channels, broadcast senders, cancellation tokens) is
now pruned when a run reaches a terminal state, so a long-lived `forge
serve` / `forge mcp` / `forge acp` no longer grows by three map entries per
run. `send_input` to a finished run is a typed error rather than a silently
recreated channel; `attach` still serves that run's whole history from the
session store.

### A session is a conversation

**Anything that names an existing session continues it.** The session's
conversation is rebuilt from the event log — every prior run's prompts,
assistant messages, tool calls and tool results, in order — and becomes the
model's history for the new run. That covers:

* `forge resume <id>` — replays up to the run being resumed and appends a
  continuation instruction (there is no new prompt to send).
* A **new prompt in an existing session** — `POST /v1/runs` with a
  `session_id`, `forge_run` with a `session_id`, and every ACP turn after the
  first (an editor session *is* a forge session) — replays the history and
  the prompt is the next turn.

A fresh session starts with nothing, so plain `forge run` is unaffected.

How reconstruction behaves at the edges:

* Runs recorded before v3 have no verbatim payloads, so their turns replay
  from the truncated `completed` summary. It still works; the log line says
  `degraded=true`.
* A run that died between asking for a tool and getting its result (cancelled,
  dispatch failed, approval unanswered) leaves a call with no answer. Replay
  answers it with `[forge: run ended before this tool answered]` rather than
  sending a dangling call, which every chat API rejects.
* Reconstruction is fitted to a character budget derived from the model's
  advertised context window (half of `max_context`, at four characters per
  token). The **first** message is always kept — it is the session's original
  ask — and the **most recent** messages fill the rest; anything dropped from
  the middle is replaced by one `[forge: earlier conversation omitted…]`
  system note, and a dropped assistant message takes its tool results with
  it.
* On a resume, routing, skill matching and graph context key on the session's
  original ask, so a continuation is routed like the work it continues. A new
  prompt routes on itself.
* A log that cannot be read is logged and the run starts fresh — losing
  history must not lose the run.

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

Real on-device inference needs a per-platform native engine (`libneedle`) that
this repo does not carry, so it sits behind one feature flag — **and enabling
that flag is the whole job**, because the build fetches and checksum-verifies
the engine for you:

```bash
cargo build --release -p forge-cli --features needle-ffi
```

That is the single command. No `curl`, no manual checksum step.

| crate | feature | effect |
| --- | --- | --- |
| `forge-needle` | `ffi` | `FfiBackend` over `libneedle` instead of `UnavailableBackend`; turns on `needle-sys/fetch` |
| `forge-needle` | `needle-e2e` | enables `tests/e2e.rs` (needs `ffi` + real weights) |
| `forge-cli` | `needle-ffi` | builds the `forge` binary with the above |

Prebuilt release binaries for the supported platforms below already have the
engine linked in, and CI refuses to publish one that claims the feature and
does not — see [Installation](#installation). You only need this section to
build one yourself.

#### How the engine gets there

`crates/needle-sys/build.rs` resolves it in three steps, first hit wins:

1. `NEEDLE_LIB_DIR=/path/to/dir` — an engine you supplied.
2. `crates/needle-sys/vendor/<target-triple>/` — a vendored engine
   (gitignored).
3. **Download, verified against a pinned SHA-256** — only when the `ffi`
   feature is on. A default build never reaches this step and never touches
   the network.

The engine ships per platform in the same Apache-2.0 Hugging Face repo as the
weights, [`Cactus-Compute/needle3`](https://huggingface.co/Cactus-Compute/needle3).
Downloads are cached by content hash under `$CARGO_HOME/needle-engine/`
(override with `NEEDLE_ENGINE_CACHE_DIR`), so it is fetched once per machine,
not once per build, and a cache entry is re-hashed on every use.

Targets forge will fetch automatically — the ones whose checksum has been
verified and whose link has been exercised:

| Rust target | artifact folder |
| --- | --- |
| `aarch64-apple-darwin` | `macos-arm64` |
| `aarch64-unknown-linux-gnu` | `linux-arm64` |

The pinned checksums live in `PINNED_ENGINES` in
`crates/needle-sys/build_support.rs`, together with the collected-but-unwired
checksums for the rest and the reason each is held back:

- **Intel macOS: no engine exists.** There is no `macos-x86_64` folder in the
  repo at all. This is the main reason `needle-ffi` is not a default feature —
  making it one would turn "forge builds and routes statically" into "forge does
  not build" on those machines.
- **x86_64 Linux and x86_64 Windows: the archive cannot be linked.** Both leave
  `std::__1::__hash_memory` undefined, and no distributed libc++ defines it
  (checked across libc++ 18 and 20, dev and runtime, static and shared). That
  symbol lives only inside Cactus's own libc++ build, which they ship
  pre-linked inside their Python wheel's `.so` and do not publish separately.
  The arm64 archives have no such problem.
- **Windows also** publishes `libneedle.a` (a COFF `ar` archive) rather than the
  `needle.lib` an MSVC `-lneedle` resolves, and needs a libc++ MSVC has not got.
- **armv7/riscv64:** no forge target builds them, so the link is unexercised.

On any other target, `--features needle-ffi` warns that no verified engine
exists and links nothing; supply one yourself via step 1 or 2 if you have one
you trust.

#### The C++ runtime (Linux build prerequisite)

`libneedle` is C++ built with clang against **libc++** — on every platform, not
just macOS. (`nm --undefined-only` over each published artifact shows
`_ZNSt3__1…`, libc++'s inline namespace, and zero libstdc++ `__cxx11` symbols.)
So on Linux, building with `needle-ffi` needs libc++'s development files:

```bash
sudo apt-get install libc++-dev libc++abi-dev     # Debian/Ubuntu
sudo dnf install libcxx-devel libcxxabi-devel     # Fedora
```

forge links them **statically**, so the binary you get has no libc++ runtime
dependency — it needs only glibc, `libgcc_s` and `libm`, exactly like a
brain-less build. That is what makes a downloaded release asset work on a
machine that has never heard of libc++. If the static archives are missing the
build falls back to a dynamic link and says so loudly; the release workflow
additionally asserts with `ldd` that no published asset was built that way.

macOS needs nothing installed: `libc++.1.dylib` is part of the OS.

Environment knobs:

| variable | effect |
| --- | --- |
| `NEEDLE_LIB_DIR` | use the engine in this directory (step 1) |
| `NEEDLE_NO_DOWNLOAD=1` | never download — offline, air-gapped and packaging builds |
| `NEEDLE_REQUIRE_ENGINE=1` | fail the build instead of continuing engine-less (CI/release use this) |
| `NEEDLE_ENGINE_BASE_URL` | fetch from a mirror instead of Hugging Face (same bytes: the checksum is not overridable) |
| `NEEDLE_ENGINE_CACHE_DIR` | where verified engines are cached |
| `NEEDLE_CXX_RUNTIME` | `static-libc++` (default), `libc++` (dynamic — for distro packages that must share the system runtime), `libstdc++` (escape hatch for a rebuilt engine), `none` (add no C++ runtime — for an engine that already carries its own) |

`needle.h` is committed as the contract of record — `needle-sys` hand-writes
its six `extern "C"` declarations rather than generating them (no `bindgen`, so
no libclang needed to build forge), and a unit test fails if the committed
header ever stops matching those declarations.

#### Running it

```bash
just verify-ffi   # clippy + unit tests with `ffi` on
just e2e          # real-weights end-to-end suite (release build)
```

`just verify` already type- and lint-checks the `ffi` code on every run via
`just lint-ffi` — `cargo clippy` never links, so that needs no engine binary.
The recipes above are what additionally *run* it.

**If `ffi` is on and no engine could be resolved**, the build prints a
`cargo:warning` naming the one thing to do next and carries on without link
flags; a binary that actually calls into the engine then fails at link time
with undefined `_needle_*` symbols, the warning still visible above it. It
warns rather than stopping because `just lint-ffi` and CI compile the `ffi`
code on machines with no engine on purpose, and that coverage is worth more
than pre-empting a link error whose cause is already on screen. Set
`NEEDLE_REQUIRE_ENGINE=1` when you would rather it stop — which is exactly
what the release workflow does, so a brain-less binary can never ship
labelled brain-enabled.

A default build — no `ffi` — never links the engine and says nothing about it:
`needle-sys` prints a note only under `cargo build -vv`, deliberately not a
`cargo:warning`, because the crate compiles on every workspace build whether
or not anything needs the engine.

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
  forge-execution   native execution (+ a test-only mock)
  forge-providers   OpenAI-compatible/Anthropic models and the needle, jev,
                    laya, http, static and cheapest routers
                    (+ test-only mocks)
  forge-session     append-only JSONL store + secret redaction
  forge-skills      SKILL.md discovery, progressive disclosure
  forge-graph       deterministic incremental project graph
  forge-needle      embedded Needle brain: decide/embed/extract/tool-call,
                    weights lifecycle, engine thread, needle router
  forge-runtime     AgentService — the one runtime shared by CLI and server
  forge-server      axum REST/SSE adapter
  forge-mcp         Model Context Protocol (stdio) adapter: tool registry,
                    schemas, dispatch
  forge-acp         Agent Client Protocol (stdio) adapter: forge as the
                    in-editor agent — session/prompt turns, streamed
                    updates, editor permission prompts
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
  mock providers — fully offline. Currently 25 features / 49 scenarios / 193 steps.
- Mocks are **test-only**. `model = "mock-local"`, `model = "scripted-mock"`,
  `router = "mock"` and `execution = "mock"` are all refused by configuration
  unless `FORGE_TEST_MOCKS=1` is
  set, which every forge test harness does. They answer
  `mock response to: <prompt>`, which is useful for asserting the agent loop
  and actively misleading as a product — so users never see them offered.
  (`FORGE_MOCK_VERBOSE=1` additionally makes the mock echo a snippet of the
  assembled system context, when you want that plumbing visible in a test.)
- Needle FFI: `just verify-ffi` and `just e2e` are opt-in and excluded from
  `just verify` — the first downloads a native engine, the second also needs
  real weights. `just verify` still type- and lint-checks all the `ffi` code
  link-free via `just lint-ffi`. In CI the same split is two jobs: the required
  `verify`, and an advisory `verify-ffi` that links and runs the backend for
  real (so a stale engine checksum or a broken link cannot go unnoticed) but
  cannot block a merge when the artifact host is down. See [Embedded Needle
  brain (`ffi`)](#embedded-needle-brain-ffi).

## Known limitations (v0.3)

- Symbol/call extraction is regex-based; `graph blast` covers two hops.
- Server run-status state is in-memory (bounded at `MAX_TRACKED_RUNS` = 1024,
  terminal-first eviction); the session store persists across restarts.
- Input delivery is in-process: `POST /v1/runs/:id/input` for a run owned by
  another process records the event but that loop does not consume it.
- Background runs are in-process only: `attach`/`list_runs` see the live
  events of runs *this* process started. A run in another process attaches to
  its stored history, but its live events reach you only as the session log
  grows — there is no cross-process detach/reattach (no daemon, no socket).
- `forge session fork` copies a prefix, so a run id can exist in more than
  one session; `resume <run-id>` picks the older one.
- **Session logs now persist tool output, and redaction is best-effort.**
  Replay (`tool_result`) stores what each tool returned — including file
  contents, up to 64 KiB per call — in `.forge/sessions/*.jsonl`. The redactor
  only catches *known* secret shapes (`sk-…`, `Bearer …`, `ghp_…`, `xox…`) and
  the values of this process's `*KEY*`/`*TOKEN*`/`*SECRET*`/`*PASSWORD*` env
  vars, so an agent that reads a credentials file, a `.env` that was not in
  this process's environment, or a private key lands in the log largely
  unredacted. Treat `.forge/sessions/` as sensitive: it is already covered by
  the repo's own `.gitignore` for `.forge/`, but back-ups, bug reports and
  pasted logs are not.
- `local_only` restricts forge's own egress, not the process: it refuses
  non-local model endpoints (and redirect hops) at provider construction and
  prunes off-device decision routers, but tools, hooks and MCP servers you run
  are not sandboxed, `localhost` is trusted by name without resolving it, and
  a local proxy that forwards upstream is outside forge's control. It also
  does not *filter* candidates — a router that selects a hosted `[models]`
  entry under `local_only` fails that run with a typed config error rather
  than quietly picking something else, which is the loud-but-correct behavior
  until candidate pruning lands. See [What `--local-only`
  restricts](#what---local-only-restricts).
- A global `model_base_url` overrides hosted `[models]` entries too, verbatim.
  The two combinations that provably cannot work are refused at startup with
  both settings named — an `anthropic`-family model on a `/v1` endpoint, and a
  `provider` the endpoint contradicts — but forge cannot tell in general
  whether a URL speaks the wire protocol a family expects. Use
  `[models.<name>] base_url` to redirect one model rather than all of them.
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
- `needle-ffi` is not a default cargo feature, so a plain `cargo build` still
  produces a brain-less binary that routes statically (and `forge init` skips
  the weights fetch in it, since there would be no backend to use them). It
  cannot be a default: Cactus publishes no engine for Intel macOS at all, the
  x86_64 archives need a libc++ nobody distributes, the Windows one is
  link-untested, and offline builds would fail to link rather than degrade.
  **Prebuilt release binaries for `aarch64-apple-darwin` and
  `aarch64-unknown-linux-gnu` do have the engine**, so the installed default on
  those platforms is a working brain; building it yourself is one flag (see
  [Embedded Needle brain (`ffi`)](#embedded-needle-brain-ffi)). Whichever build
  you have, `forge doctor`'s `needle engine` / `needle brain` line-pair states
  the backend, the weights, the verdict and the one command that changes it.
  **x86_64 Linux is the notable gap** — the most common server platform, and it
  stays brain-less until Cactus publishes an archive that links against a stock
  libc++ (or forge learns to link their self-contained `.so` instead of the
  `.a`).
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

## Contributing

`main` is protected: changes land via pull request only (direct pushes,
force pushes, and branch deletion are rejected). Every PR must pass the
`verify` CI job (`cargo fmt --check`, clippy with `-D warnings`, all tests,
and the BDD suite — the same as `just verify` locally). The `verify-ffi` job
runs alongside it and links the real engine; it is advisory, since it depends
on an upstream artifact download, but a red one is worth reading before you
merge. No approvals are required for now; keep PRs small and green.
