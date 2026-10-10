# forge

A one-command, local-first AI coding harness that combines Needle 3,
a semantic project graph, optional Jev escalation, and your choice of local
or hosted generation models. It is built around two planes:

- a **decision plane** — a small on-device model (needle) that answers
  "which tool, which model, if any" in milliseconds, locally, before any
  big model is called. Every decision is written to a local log
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

The on-device engine links by default on macOS/Linux arm64 (fetched and
checksum-verified at build time). Other platforms build engine-less and
route with static rules — `forge doctor` says which you have. Prebuilt
binaries and an install script are in
[Installation](docs/reference.md#installation).

## Quickstart

One-command quickstart (recommended):

```bash
cd your-project
forge                                # auto-bootstraps this project and opens the chat
```

On the first run Forge creates its local state, builds the project graph,
prepares Needle 3 when the embedded engine is available, and prints the
generation model it selected. Selection is deterministic: a reachable local
model wins, followed by an authenticated CLI subscription, then an API-key
hosted provider. Models without working endpoints or credentials are never
offered to the router.

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
forge chat                            # the same loop, interactive
```

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
- `forge resume <task-id>` — validates the working tree and effect journal,
  then continues a safely parked task in its existing session.
- `forge task [list]` / `forge task show <task-id>` — inspect the current
  node, route, spend, checks, changed files, parked reason, and terminal result;
  add `--json` for the shared machine-readable projection.
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
router = "needle"        # default; also static, cheapest, jev, laya
approval = "prompt-dangerous" # ordinary edits run; destructive operations ask
```

Presets live in [`examples/configs/`](examples/configs/) — or let init
write one: `forge init --preset claude` (also `codex`, `kimi`, `kev`,
`decider`, `jev`, `local-first`, `hybrid-needle`, `budget-hosted`,
`hybrid-laya`).

## Where to read more

- **[docs/reference.md](docs/reference.md)** — the full manual: models,
  routers, execution, skills, the graph and semantic index, the server,
  the MCP/ACP surfaces, and every known limitation.
- **[ARCHITECTURE.md](ARCHITECTURE.md)** — the crate map and the design
  invariants.
- **[docs/superpowers/specs/](docs/superpowers/specs/)** — the design
  specs this was built from.
