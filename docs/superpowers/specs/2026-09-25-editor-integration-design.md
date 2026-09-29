# Editor integration: forge as the agent in Cursor, VS Code and Zed

**Status:** design, awaiting implementation plan
**Date:** 2026-09-25
**Scope:** sub-project E. Depends on the Session core for the approval
contract and process model, and on streaming (F) for output. The terminal REPL
it sits beside is Phase B / sub-project 6b, already designed on `main`
(`2026-09-24-interactive-chat-ui-design.md`).

## The problem

The goal is to use forge as the harness inside an editor. Forge ships
`forge-acp`, which reaches **Zed**. Cursor and VS Code do not speak ACP.

They speak MCP — and forge already has `forge-mcp`. But read what it is: a
**server**, "exposing forge's project intelligence (graph, skills, doctor) and
agent runs as MCP tools to any MCP client (Claude Code, VS Code, Cursor, other
harnesses)."

That inverts the relationship. Over MCP, **Cursor's agent is the harness and
forge is a tool provider**. Useful, and not what "switch over to using forge"
means.

So there are two distinct products hiding under one phrase, and they need
separating before either is built.

## Two integration models

**Forge-as-intelligence.** The editor's own agent runs the loop; forge supplies
the graph, semantic search, skills and project facts as MCP tools. Already built.
Zero new work for Cursor, VS Code, Claude Code and anything else speaking MCP.
The editor's model does the reasoning, so none of forge's routing, gating or
on-device decisions apply.

**Forge-as-agent.** Forge runs the loop; the editor is a view. Needle dispatch,
the gate, routing, the decision log and the whole Session core apply. This is
what ACP gives in Zed, and what Cursor and VS Code need an extension for.

These are complementary, not competing. Both should ship, clearly labelled, so a
user knows which one they are getting.

## Recommendation

| Editor | Protocol | Model | Work |
|---|---|---|---|
| Zed | ACP | forge-as-agent | Exists; align to the Session core |
| VS Code | extension → daemon | forge-as-agent | **New** |
| Cursor | the same extension | forge-as-agent | Free — Cursor is a VS Code fork |
| Claude Code, others | MCP | forge-as-intelligence | Exists |

**One extension covers both VS Code and Cursor**, because Cursor is a VS Code
fork with a compatible extension API. That is what makes forge-as-agent worth
building for that pair rather than one.

## Architecture

The extension is a **thin view over the daemon** (Session core §20), not a
reimplementation of the loop:

```
  VS Code / Cursor extension (TypeScript)
        │  JSON-RPC over the local socket
        ▼
     forged  ──▶ Session ──▶ needle │ decision plane │ LLM
```

The extension owns presentation only: a chat panel, diff review, approval
prompts, and progress. Every decision stays in Rust, which is what keeps the
three surfaces behaving identically and the decision log comparable across them.

### What the extension must render

Directly from the Session core's approval contract (A §18) — it exists so this
is mechanical rather than invented per surface:

- `ApprovalPreview::Diff { path, before, after }` → the editor's native diff
  view, with accept/reject. This is the single biggest reason to build an
  extension rather than live in a terminal.
- `ApprovalPreview::Command { program, args, cwd }` → a confirmation showing
  exactly what will run.
- `ApprovalPreview::Read { path }` → usually auto-approved; shown when not.

Responses map onto the fixed `ApprovalResponse` vocabulary, so an approval in
Cursor and an approval in the terminal are the same labelled example for §15's
calibration.

### Transport

The extension attaches to the daemon's local socket, with the same rules as any
other client: auto-start, version-matched, degrade to spawning an embedded
forge if the daemon is unavailable.

It deliberately does **not** use `forge serve` over TCP. The daemon socket is
already local-only and already the thing every other surface uses; adding an
HTTP hop would mean a second auth story for no benefit.

## Multi-root workspaces

Editors open several folders at once; forge is `--project <path>` and its graph
is per-project. The daemon holds graphs per project (A §20), so the extension
resolves the project for a turn from the active file's workspace folder and
names it per request. A workspace with three roots gets three graphs and one
engine — which is the split A §20 already makes.

## What this does not do

- **Inline completions.** Forge is a task harness, not a completion engine.
  Needle is 121M parameters and tuned for tool calls, not for FIM.
- **Replacing the editor's own agent.** Both can coexist; forge-as-intelligence
  over MCP is the supported way to combine them.
- **Remote or web editors.** Local socket only. A remote story would need
  `forge serve` and an auth model that does not exist yet.

## Open questions

- **Extension distribution.** VS Code Marketplace and Open VSX (which Cursor
  uses) are separate publishing targets with separate review.
- **Version skew.** The extension and the daemon must match (A §20 refuses
  mismatches). Who prompts to update whom, given `forge update` manages the
  binary but the marketplace manages the extension?
- **Zed alignment.** `forge-acp` predates the Session core; it needs auditing
  against the approval contract so Zed and the extension agree.

## Sequencing

1. Session core — the approval contract and daemon must exist first.
2. Streaming (F) — an extension that cannot stream is a worse terminal.
3. This spec.

Building the extension before A §18 and F means inventing a presentation
contract in TypeScript and then rewriting it, which is the specific failure this
sequencing avoids.
