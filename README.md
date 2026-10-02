# forge

An AI agent harness for your projects, built around two planes:

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

```bash
cd your-project
forge init                          # starter config, project graph, brain weights
forge run "Explain this project"    # the agent loop
forge chat                          # the same loop, interactive
```

Then point forge at a model you have — `forge model list` shows what your
keys unlock (it reads `.env`, your shell, and the usual credential
files). Set it in `.forge/config.toml`:

```toml
model = "qwen3-coder"   # or any name from `forge model list`
```

Anything looks wrong: **`forge doctor`** — it probes the model, the
router, the brain, and your credentials, and tells you which line to
change.

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

## What you get

- `forge run` / `forge chat` — the multi-turn, tool-using agent loop with
  approvals (`y`/`n` inline; safe reads never ask).
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
approval = "prompt"      # or "auto" (never asks), "deny" (never runs risky)
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
