# Ticket Breakdown — Interactive chat: the "Claude Code / pi.dev feel"

## Epic summary

Close the feel-gap between `forge chat` and Claude Code/pi.dev **within the
recorded inline-transcript architecture** — full-screen ratatui was rejected
on shape and cost (`docs/superpowers/specs/2026-09-24-interactive-chat-ui-design.md` §2.2),
and that decision stands. The feel actually comes from four things the
codebase itself names as gaps: **token streaming** (the big one), clean
out-of-band rendering, completion, and on-demand inspection — plus two
runtime-level fixes that every front end inherits. Slash commands, history
replay, and fork & background already shipped with `forge-chat` (so the
"What's next" line in `ARCHITECTURE.md:453` is partially stale; the remaining
work is what this epic slices).

Re-opening full-screen TUI would be an ADR with new evidence, not a ticket.
Cross-process background runs (daemon/socket) stay deferred at Phase A level.

## Tickets

### TICKET-1 — Token streaming: core plumbing

- Scope: `ModelProvider` gains a streaming completion method with a default
  whole-response fallback over `complete()` (every existing provider and mock
  keeps compiling); `EventKind` gains an additive assistant-delta variant
  under the schema-v3 additive convention; the runtime broadcasts deltas on
  the run channel and persists them through the one redaction boundary; the
  mock provider streams deterministically for offline tests.
- Acceptance: a run with a streaming-capable model emits ordered deltas then
  the same terminal events as today; a non-streaming provider behaves
  byte-identically to before; replay/`forge resume` reconstruct from the
  final `AssistantMessage` exactly as today (deltas never needed for replay).
- Context: design doc §14 bullet 1 ("the transcript gains an incremental
  assistant block and nothing else changes — the renderer is already
  per-event"); `crates/forge-core/src/model.rs:158-165` (single `complete()`
  today); `crates/forge-core/src/events.rs:8-22` (additive convention),
  `events.rs:97-197`; `streaming` capability metadata exists unwired
  (`model.rs:11`, `router.rs:10`); streaming stance `docs/reference.md:1354-1357`.
- Files: forge-core (model, events), forge-runtime (agent loop), mock
  provider. ~800–1200 lines incl. tests.
- Depends on: none.

### TICKET-2 — Streaming providers: OpenAI-compatible + Anthropic SSE

- Scope: real streaming for both provider families — SSE decode, tool-call
  reassembly from deltas, usage accounting — honoring the `EgressPolicy`
  redirect re-check on every hop; graceful fallback when a server doesn't
  stream; `CapabilityKind::Streaming` becomes a real selection filter.
- Acceptance: recorded-SSE fixture tests for both families (deltas, tool
  calls, errors mid-stream); oMLX path streams end-to-end against the local
  server; router never selects a non-streaming provider when streaming is
  required.
- Context: `crates/forge-providers` provider clients; `EgressPolicy`
  (ARCHITECTURE.md generation-plane section); `Capability::satisfied_by`.
- Files: forge-providers. ~800–1200 lines incl. tests.
- Depends on: TICKET-1 (plan only after it is *implemented*).

### TICKET-3 — Streaming render: chat incremental block + ACP live chunks

- Scope: `TranscriptState` renders assistant deltas as one growing block
  (not a line per delta); ACP forwards deltas as live
  `agent_message_chunk`s (today: one chunk at turn end); piped mode behavior
  specified; the answer-once rule still holds for the needle fast path.
- Acceptance: chat shows text arriving during a turn (mock-streamed, no TTY
  needed — the render layer is pure); ACP e2e shows chunks before turn end;
  fast-path dispatch renders exactly as today.
- Context: `crates/forge-chat/src/render.rs:34-62,136-147,178-186`;
  `app.rs:443-454` (apply_event); forge-acp turn driver.
- Files: forge-chat, forge-acp. ~600–900 lines incl. tests.
- Depends on: TICKET-1 (TICKET-2 only for real-model feel; mocks prove it).

### TICKET-4 — Path completion in the prompt

- Scope: `@path` / in-prompt token completion against project files (graph
  as the source), wired into the rustyline helper next to slash completion;
  no half-working states (design doc's explicit bar).
- Acceptance: Tab on `@src/ma` completes to real project paths; completion
  is bounded on huge repos; no behavior change for non-path tokens.
- Context: design doc §14 ("half-working path completion is worse than
  none"); `crates/forge-chat/src/command.rs:180` (`Command::complete`);
  `crates/forge-cli/src/chat/terminal_io.rs:404-453` (helper).
- Files: forge-chat, forge-cli chat shell. ~400–700 lines incl. tests.
- Depends on: none.

### TICKET-5 — On-demand tool-result rendering (`/show`)

- Scope: `/show <n>` (or `/last`) renders recorded `tool_result` payloads
  from the session log into the transcript — the data is already on disk
  (≤64 KiB verbatim payloads); parse + pure render + driver wiring.
- Acceptance: `/show 2` re-renders that run's tool results through the same
  visual grammar; works on continued sessions (backlog), not just live runs.
- Context: design doc §14 ("The data is in the log; `forge session show`
  reads it today"); `ToolResult` in `crates/forge-core/src/events.rs`;
  render grammar §4.1–4.2 of the design doc.
- Files: forge-chat. ~300–500 lines incl. tests.
- Depends on: none.

### TICKET-6 — Explicit skill activation (`RunOptions::activate_skills`)

- Scope: runtime-level explicit activation so `/name` in chat stops relying
  on lexical `match_task` — and `forge run`, `forge mcp`, and ACP inherit
  the same determinism.
- Acceptance: `/name` activates exactly that skill (activation event
  recorded); the other three surfaces gain the same option; lexical
  matching stays as the default discovery path.
- Context: design doc §14 bullet 4; `crates/forge-chat/src/command.rs:163-166`
  (skill-as-slash today); `SkillRegistry` trait.
- Files: forge-runtime, forge-chat, mcp/acp pass-through. ~500–800 lines
  incl. tests.
- Depends on: none.

### TICKET-7 — Clean out-of-band printing (rustyline bump)

- Scope: when a rustyline release >18.0.1 ships with upstream `f2bbcc5`,
  bump it and restore the coordinating `ExternalPrinter`, removing the
  notify-prints-mid-line artifact; interim `StdoutPrinter` stays until then.
- Acceptance: `/bg` watcher notices never corrupt a half-typed line in a
  pty test; `docs/reference.md:665-694` limitation 1 removed.
- Context: `crates/forge-cli/src/chat/terminal_io.rs:292-302` (bug +
  upstream commit reference).
- Files: forge-cli chat shell, Cargo.toml. ~100–200 lines.
- Depends on: none (but **externally blocked** on the upstream release).

### TICKET-8 — Bounded/incremental `list_runs`

- Scope: runtime-side bounded/incremental session-run listing — today one
  submitted line re-reads and re-parses every session's whole JSONL log —
  and completion-snapshot reads move off the executor.
- Acceptance: completion-refresh cost stops scaling with total session-log
  size; `/jobs` and session pickers stay correct; every front end gets the
  improvement through the runtime, not per-adapter patches.
- Context: design doc §14 final bullet; `App::refresh_completions`
  (`crates/forge-chat/src/app.rs`).
- Files: forge-runtime, forge-session, chat call sites. ~400–700 lines
  incl. tests.
- Depends on: none.

## Dependency graph

```
TICKET-1 (streaming core) ──▶ TICKET-2 (real provider SSE)
                        └──▶ TICKET-3 (chat + ACP render)

TICKET-4, TICKET-5, TICKET-6, TICKET-8   fully independent
TICKET-7                                  independent, blocked on upstream rustyline release
```

## Suggested execution order

- **Wave 1 (parallel worktrees):** TICKET-1, TICKET-4, TICKET-5, TICKET-6
  (TICKET-8 also independent — add if a fifth lane exists).
- **Wave 2:** TICKET-2 and TICKET-3, planned only after TICKET-1 is
  *implemented* (its shape informs theirs).
- **Opportunistic:** TICKET-7 when rustyline >18.0.1 ships.

Each ticket enters its own PIV loop via `/piv-plan-implementation`; the
per-ticket context above is what makes that possible without re-reading
this epic.
