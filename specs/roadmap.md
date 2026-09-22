# Forge roadmap and future directions

Items here are agreed future goals, not committed scope for the current version.
When one is scheduled, move it into a versioned prompt under `specs/prompts/` and
record the design decisions as an ADR under `specs/adrs/`.

## Desktop UI (menu bar / toolbar app) via Tauri

**Goal:** a desktop interface for Forge that lives in the system menu bar /
toolbar, similar to how oMLX ships a small always-available macOS menu bar app.

**Why it fits well when the time comes:**

- Forge is already Rust end to end, so a [Tauri](https://tauri.app) shell can
  embed the harness directly — link the `forge-*` crates in-process or manage
  the single `forge` binary as a sidecar.
- The transport-neutral `AgentService` boundary and the REST/SSE server
  (`forge serve`, loopback by default) were designed for exactly this kind of
  adapter: the UI can talk to `http://127.0.0.1:7341` (runs, SSE event streams,
  skills, graph) without changing runtime semantics.
- SSE run events map naturally onto a live UI (turns, tool calls, approval
  prompts, completion), and the `waiting_for_approval` status + run input
  endpoint give a clean path for approval dialogs in the UI.

**Prerequisites before starting this:**

- Agent loop hardened against real models (v0.4 work).
- Server lifecycle management story (auto-start/stop of a per-user
  `forge serve` instance, or in-process embedding).

**Out of scope for that effort:** replacing the CLI; the CLI and server remain
first-class interfaces.
