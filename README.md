# forge

A one-command, local-first AI coding harness that combines native Laya,
Needle 3, a semantic project graph, and your choice of local or hosted
generation models. It is built around two planes:

- a **decision plane** — Laya selects the least-expensive capable generation
  model; Needle remains an on-device safe-tool and guardrail fast path. Every
  decision is written to a local log
  (`forge session decisions`).
- a **generation plane** — your model: local first, cloud when you
  choose. The same agent loop runs either way.

The point: fast, deterministic plumbing around whichever model you bring.
Tool dispatch, routing, project context and approvals are decided on
device and logged; the big model only does what actually needs a big
model.

## Install

```bash
cargo install --locked --git https://github.com/auser/forge forge-cli
```

The on-device engine links by default on macOS/Linux arm64 and on x86_64
Linux (fetched and checksum-verified at build time; x86_64 Linux also needs
LLVM's libc++ 21+, see
[Embedded Needle brain](docs/reference.md#embedded-needle-brain)). Other
platforms build engine-less and route with static rules — `forge doctor` says
which you have, which resolution path the build took, and what your CPU means
for the engine. Prebuilt
binaries and an install script are in
[Installation](docs/reference.md#installation).

### Through a local model proxy

Running a Claude/OpenAI-compatible proxy on this machine (or tunnelled to it)?
`forge proxy` sends every model request there, and keeps subscription
credentials out of reach:

```bash
forge proxy init                       # once: write the proxy's [models], local_only, [proxy]
forge proxy doctor                     # URL, reachability, dialects, isolation, needle
forge proxy                            # chat
forge proxy run "fix the failing test" # one-shot
forge proxy --model sonnet run "…"     # any forge command and flags pass through
```

`init` discovers models from `GET {url}/v1/models` (Anthropic-dialect entries
use the URL without `/v1`, OpenAI-dialect ones with it), is idempotent, and
keeps the rest of your config. The URL comes from `--url`, then
`$FORGE_PROXY_URL`, then `[proxy] url`, then `http://127.0.0.1:8001`; the
client key from `$FORGE_PROXY_API_KEY` (default `dummy`). For a proxy on
another host, tunnel it first: `ssh -N -L 8001:127.0.0.1:8001 host`. Details:
[Through a local proxy](docs/reference.md#through-a-local-proxy-forge-proxy).

## Quickstart

One-command quickstart (recommended):

```bash
cd your-project
forge                                # auto-bootstraps this project and opens the chat
```

On the first run Forge creates its local state, builds the project graph,
prepares the native Laya checkpoint when it is first needed, and prints the
generation model it selected. Selection is deterministic: a reachable local
model wins, followed by an authenticated CLI subscription, then an API-key
hosted provider. Models without working endpoints or credentials are never
offered to the router.

The graph's semantic index is embedded independently of the generation model.
`embedding.provider` defaults to `needle`; the other valid values are
`embeddinggemma2` and `rten`. `rten` runs on-device in pure Rust with no
daemon and no native dependency, so it also works on hosts where the Needle
engine cannot link, and it fetches SHA-256-pinned weights on first use.

```toml
[embedding]
provider = "rten"          # or needle (the default), embeddinggemma2
# autofetch = false        # supply the weights yourself, for offline use
```

The full embedding configuration is in
[Configuration](docs/reference.md#configuration).

No model yet? Sign in with a subscription you already have, then run `forge`
again:

```bash
forge auth login claude
forge auth login codex
forge auth login kimi
```

Each command delegates the browser/device flow to the provider's official CLI;
Forge never asks for or stores your password. `forge auth status` reports what
is ready without printing credential values. Inside chat, use `/auth` to list
the same providers or `/auth codex` (and similarly `claude`/`kimi`) to sign in
and switch without restarting Forge.

Or the explicit steps:

```bash
cd your-project
forge init                            # starter config, project graph, brain weights
forge run "Explain this project"      # the agent loop (non-interactive)
forge run --plan "Refactor auth"      # route planning separately from execution
forge chat                            # the same loop, interactive
```

To route each phase through a named model policy, initialize the ready-made
profile and use it for a planned run:

```bash
forge init --preset opusplan
forge --profile opusplan run --plan "Refactor auth"
forge doctor                          # reports the active profile
```

A profile declares `plan`, `execute`, `review`, and `general` under
`[profiles.<name>]`; each phase accepts one model name or a list:

```toml
[profiles.opusplan]
plan = "claude-sonnet"
execute = ["deepseek-chat", "qwen3-coder"]
review = "claude-sonnet"
general = "deepseek-chat"
```

You can activate it with `profile = "opusplan"` in config,
`FORGE_PROFILE=opusplan` in the environment, or `--profile opusplan` on the
command line, in that precedence order. The preset matches
[`examples/configs/opusplan.toml`](examples/configs/opusplan.toml). Naming an
undefined profile is a configuration error, and the run refuses to start.

Forge also keeps durable project knowledge alongside the graph. Add a concise,
reviewable decision linked to the files you changed, then inspect it before a
commit:

```bash
forge learn add "Keep payment tokens out of logs" --kind guardrail
forge learn check
```

These Markdown records live in `.forge/learnings/`, are intentionally
trackable, and are injected only when the project graph finds related files.
After a verified `forge run`, Forge also refreshes its separate private queue
of redacted learning candidates; review it with `forge learn list` before any
guidance affects later prompts.

When Forge auto-bootstraps it chooses a reachable local model or a
credentialed hosted model for you; use `forge model list` to inspect
what it detected and `forge model add` / `.forge/config.toml` to pin one.

```toml
model = "..."   # or any name from `forge model list`
```

Anything looks wrong: **`forge doctor`** — it probes the model, the
router, the brain, and your credentials, and tells you which line to
change.

## Release checks

`forge version --build` prints the semver, source commit, and compilation
target. Release artifacts embed the exact GitHub commit, so two builds with the
same semver can still be identified.

The offline acceptance check
`cargo test -p forge-cli --test cli compiled_forge_initializes_then_edits_and_validates_a_project`
starts with an uninitialized disposable project, runs the compiled Forge
binary, and proves its deterministic mock provider can edit a file and execute
a validation command. `bash tests/install-path.sh` separately checks installer
verification and PATH-shadow handling with a local fake release asset.

`just harness-gate` is the complete release gate: the workspace checks plus
named hermetic wire-contract tests for Claude, Codex, Kimi, and local
OpenAI-compatible endpoints, and the inspect/edit/check/review acceptance
workflow. It makes no provider calls.

The current candidate evidence and its provider-specific limitations are in
[Beta qualification](docs/beta-qualification.md).

Before publishing a release, a **manual canary is still required**: install the
candidate artifact on a clean machine and complete one real edit/test run with
each supported subscription provider. The hermetic mock check validates the
agent and tool loop, but cannot validate provider authentication, subscription
entitlements, or upstream API behavior. Run `just harness-canary MODEL` for
each model the release claims (for example `claude-sonnet`, `gpt-5.6-sol`, and
`k3`). Each canary uses only the credential already authorized on that machine;
it does not rotate accounts or retry around a provider limit.

## Use from your editor

**Zed** (or any ACP editor) — forge as a native agent, in
`~/.zed/settings.json`:

```json
{
  "agent_servers": {
    "forge": {
      "command": "forge",
      "args": ["acp"]
    }
  }
}
```

**Cursor** (or any MCP client) — forge's project tools, in
`~/.cursor/mcp.json` under `mcpServers`:

```json
"forge": {
  "command": "forge",
  "args": ["mcp"]
}
```

Run `forge graph build` in the project first so the graph tools have an
index to search.

For on-demand tool discovery, use `"args": ["mcp", "--compact"]`. This
advertises search/schema/invoke tools instead of every schema up front,
with the same execution and approval policy. See
[compact discovery](docs/reference.md#compact-discovery-opt-in) for the workflow
and tradeoffs.

## What you get

- `forge run` — a bounded inspect/edit/check/review workflow over the
  multi-turn agent loop, with durable task evidence and a reviewable diff.
- Routing profiles — a named policy per phase: plan with the strong model,
  build with the cheap one (`[profiles.<name>]`).
- `forge run --plan` — routes a read-only design pass, economical execution,
  and—when implementation changes something—a read-only independent review.
  The generated plan and review are durable task evidence.
- `forge resume <task-id>` — validates the working tree and effect journal,
  then continues a safely parked task in its existing session.
- `forge task [list]` / `forge task show <task-id>` — inspect the generated
  plan, current node, route, spend, checks, changed files, review, parked
  reason, and terminal result; add `--json` for the shared machine-readable
  projection.
- `forge chat` — the same tool-using runtime interactively, with approvals
  (`y`/`n` inline; safe reads never ask).
- `forge graph` — a deterministic project graph plus an on-device
  semantic index: `graph build`, `graph context "auth flow"
  [--steer "prefer tests"]`, `graph grep`, `graph map`.
- `forge serve` / `forge mcp` / `forge acp` — REST server, MCP tool
  server, ACP agent: the same runtime over every transport.
- A decision log — every dispatch/routing decision on disk, readable with
  `forge session decisions`.

## Configuration, in one breath

`.forge/config.toml` per project, `~/.config/forge/config.toml` for all
projects. `FORGE_*` env vars override files; CLI flags override
everything. The three keys you'll actually touch:

```toml
model = "..."            # from `forge model list`
router = "laya-native"   # default; chooses the least-expensive capable model
router_fallback = "cheapest"
approval = "prompt-dangerous" # ordinary edits run; destructive operations ask
```

Presets live in [`examples/configs/`](examples/configs/) — or let init
write one: `forge init --preset claude` (also `codex`, `kimi`, `kev`,
`decider`, `jev`, `opusplan`, `local-first`, `hybrid-needle`, `budget-hosted`,
`hybrid-laya`, `hybrid-laya-native`).

A **routing profile** says which models may serve which phase — the
`opusplan` idea, per phase instead of per vendor. One model in a tier is a
decision (no inference, no extra cost); several make a menu the router picks
from:

```toml
profile = "opusplan"           # or FORGE_PROFILE=opusplan, or --profile opusplan

[profiles.opusplan]
plan    = "claude-sonnet"           # one model: a decision
execute = ["deepseek-chat", "gpt-5"] # several: a menu
```

`forge init --preset opusplan` writes that shape with the models forge
already declares. `forge doctor` reports the active profile and marks any
tier model that cannot be served — a name that is not wired up would
otherwise be ignored at run time, silently.

## Where to read more

- **[docs/reference.md](docs/reference.md)** — the full manual: models,
  routers, execution, skills, the graph and semantic index, the server,
  the MCP/ACP surfaces, and every known limitation.
- **[ARCHITECTURE.md](ARCHITECTURE.md)** — the crate map and the design
  invariants.
- **[docs/superpowers/specs/](docs/superpowers/specs/)** — the design
  specs this was built from.
