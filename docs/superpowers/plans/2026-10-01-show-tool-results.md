# TICKET-5 — On-demand tool-result rendering (`/show`) — Implementation Plan

> **For agentic workers:** implement this plan task-by-task, in order, with
> the validation command of each step green before moving on. Steps use
> checkbox (`- [ ]`) syntax for tracking. Run everything in the worktree
> (`worktrees/t5-show-tool-results`), never in the main checkout.

## Metadata

- **Ticket:** `specs/tickets/interactive-chat-feel.md`, TICKET-5 (epic:
  *Interactive chat: the "Claude Code / pi.dev feel"*). Fully independent —
  no other ticket gates it.
- **Architecture inherited:**
  `docs/superpowers/specs/2026-09-24-interactive-chat-ui-design.md` —
  §4.1/§4.2 (events→transcript visual grammar; `ToolResult` is deliberately
  silent live), §9.1 (slash surface), §14 (the follow-up bullet: *"Rendering
  `tool_result` payloads on demand (a `/last` or `/show <n>` command). The
  data is in the log; `forge session show` reads it today."*), plus
  `ARCHITECTURE.md:255-313` (the pure-core/thin-shell adapter shape).
- **Code baseline:** worktree `worktrees/t5-show-tool-results` @ `5c72985`.
  All `file:line` citations below are against that checkout.
- **Estimated size:** ~300–500 lines including tests (ticket's own estimate;
  this plan lands inside it).
- **Crates touched:** `forge-chat` only, plus test/doc surfaces in
  `forge-cli` (process test, BDD), `docs/reference.md`, and the design doc.

## Feature Description

A new slash command, `/show [n]`, re-renders the **nth most recent recorded
`tool_result`** of the current session into the chat transcript — the
verbatim, ≤64 KiB, already-redacted payload the runtime wrote to the session
log when the tool finished. `/show` with no argument is `/show 1`, the most
recent result, which is the `/last` reading of design §14. The data is read
from the session store at command time, so it works identically on a live
turn's results, a settled turn's, and a continued session's backlog.

## User Story

As a forge chat user, after watching `  * read_file src/parser.rs` /
`    -> ok (4 ms)` scroll by, I want to type `/show` and see what the tool
actually returned — without leaving the chat, re-running the tool, or opening
`forge session show <id>` in another terminal and counting JSON lines.

## Problem

`ToolResult` is the replay record of what the *model* saw, and the live
renderer deliberately renders it as nothing
(`crates/forge-chat/src/render.rs:148-151`:

```rust
// The replay record of what the *model* saw, capped at 64 KiB:
// dumping it would bury the transcript. `ToolCompleted` is the
// user-facing summary and `forge session show` has the payload.
EventKind::ToolResult { .. } => Vec::new(),
```

That is the right default for the live stream, but it leaves the payload —
the very thing a user needs when a tool's one-line `ok` summary isn't enough
("what did the read return?", "what did the command print?") — reachable
only through a different command in a different terminal. The payloads are
already on disk (`crates/forge-runtime/src/service.rs:2007-2020` records
each result verbatim, capped, into the session log as it happens); the chat
just has no window onto them. Design §14 filed exactly this as a follow-up;
this ticket ships it.

## Solution

Four pieces, three of them pure functions, all inside the existing
pure-core/thin-shell split:

1. **Parse** (`command.rs`): `/show [n]` joins `COMMANDS`, `Parsed`, and
   `Command::parse`, following the `/fork` pattern (optional argument,
   `Usage` on garbage).
2. **Select + render** (new pure module `crates/forge-chat/src/show.rs`):
   given a session's events and an ordinal, pick the nth most recent
   `ToolResult`, recover the requesting call's friendly argument by pairing
   `call_id` against the preceding `AssistantMessage`'s `tool_calls`, and
   render the block through the existing §4.1 gutters.
3. **Wire** (`controller.rs`, `app.rs`): a read-only `Action::Show` —
   immediate in every chat state — executed by `App::do_show`, which reads
   the current session's log through `AgentService::sessions().events_for()`
   (the exact call `/session` and session resume already make) and emits the
   rendered lines.
4. **Prove** (`forge-chat` unit + driver tests, one `forge-cli` process
   test, one BDD scenario) and **document** (`docs/reference.md` slash
   table, design-doc §14 amendment) in the same commits.

### Design call 1 — command shape: `/show [n]`, n indexing *tool results*, most-recent-first

`/show <n>` selects the **nth most recent tool result in the current
session** (`1` = the latest); `/show` with no argument means `/show 1`. No
separate `/last` command.

Rationale, from §14 and the `forge session show` precedent:

- §14 offers "a `/last` **or** `/show <n>` command" — one command whose
  argument defaults to 1 *is* both. `COMMANDS`
  (`crates/forge-chat/src/command.rs:28-42`) is the single list feeding
  `/help` and Tab completion; two names for one action doubles that surface
  for nothing.
- The indexable unit the user can actually see on screen is the tool call —
  `  * read_file src/main.rs` (`render.rs:87-98`). Nothing numbers *runs* on
  screen (the footer doesn't; `/jobs` shows 26-char ULIDs), so "run 2" is
  not an address a user can form. Most-recent-first matches the question the
  command answers: "that tool call I just watched".
- `forge session show`'s 1-based log positions
  (`crates/forge-cli/src/commands/session_cmd.rs:91-97`) were considered and
  rejected as the index: that numbering exists to feed `forge session fork
  --at <N>` (the comment at `session_cmd.rs:91-94` says so), it shifts as
  the session grows, and the chat never displays it.
- The ticket's acceptance phrasing ("`/show 2` re-renders that run's tool
  results") is read in its loose sense — "the run you just watched" — not as
  run-ordinal indexing. This is Open Question 1; the recommended default
  (per-call indexing) is what is specified here, and if a reviewer reads the
  ticket literally, the change is confined to `show::select`'s grouping.

### Design call 2 — the data source is the session store, for live and continued sessions alike

The in-memory `TranscriptState` retains **no** payloads — its fields are a
timing anchor, an answer-once flag, and two counters
(`render.rs:34-45`), and the `ToolResult` arm stores nothing. Adding
retention would duplicate the log and break the design's "the chat keeps
exactly two things in memory" rule (§5). It also isn't needed:

- The runtime records each `ToolResult` into the session log *as the tool
  finishes* (`service.rs:2007-2020`), through `JsonlSessionStore::append`,
  which redacts **before** writing
  (`crates/forge-session/src/store.rs:281-301`). So the store is complete
  and current mid-turn, after a turn, and across processes.
- The driver already reaches it the same way in two places:
  `do_show_session` (`crates/forge-chat/src/app.rs:889-902`) and
  `render_session_transcript` (`app.rs:989-995`) both call
  `self.host.service().sessions().events_for(&self.session_id)`.
  `AgentService::sessions()` returns the shared `Arc<JsonlSessionStore>`
  (`crates/forge-runtime/src/service.rs:458`); `events_for` returns the log
  in file order (`store.rs:62-81`), so "most recent" is "from the end".
- Two properties come free and are worth stating in tests: what `/show`
  prints is the **redacted** payload (it can never un-redact a secret), and
  the store's truncation marker — `[forge: tool output truncated, N bytes
  dropped]`, appended by `cap_tool_output`
  (`crates/forge-core/src/events.rs:36-49`) — passes through verbatim.
- After `/bg`, the conversation continues in a fork whose log is a verbatim
  prefix *copy* (`store.rs:103-135`), so `/show` still reaches the
  backgrounded turn's results-so-far; after `/session <id>` it reads the
  newly current session. Both are consequences of "read the current
  session's log", not special cases.

The read is a synchronous filesystem call on the executor — the same class
as `/session`, resume, and `/fork` already perform, and covered by the
crate's stated rule (no terminal; filesystem only through `AgentService`,
`crates/forge-chat/src/lib.rs:1-17`). Moving it behind `spawn_blocking` is
**not** part of this ticket (see Out of Scope and Notes).

### Design call 3 — the rendering grammar: meta header + `    -> ` payload lines

```text
  - tool result 1 of 3: read_file src/main.rs (run 01JCF4...)
    -> fn parse_config() {}
    -> ...
```

- **Header** — one `Line::meta` (`  - `, the class §4.1 gives to chat-side
  annotations): ordinal and total, the tool name, the friendly argument when
  recoverable (recovered with the existing `summarize_call`,
  `render.rs:257-269`, over the *full* arguments JSON from the paired
  `ToolCall` — `forge_core::tool_arg_field`,
  `crates/forge-core/src/events.rs:228-260`, parses valid JSON first), and
  the owning run id, so results in a multi-run session are attributable the
  same way `forge session show` attributes every event (`[run_id]`,
  `session_cmd.rs:80`).
- **Payload** — verbatim, one `Line` per payload line (the writer writes one
  line at a time, so multi-line content is split by the renderer — the
  `AssistantMessage` pattern, `render.rs:141-146`), each under the result
  gutter: `Line::ok` (`    -> `) for success, `Line::failed` (same gutter,
  `Style::Bad`) when `is_error` — precisely §4.1's "tool result / file
  change | `    -> ` | Ok/Bad" class, multi-line.
- **No blank-line framing** (that is the assistant answer's class) and **no
  display-side truncation**: the store's 64 KiB cap
  (`forge_core::MAX_TOOL_OUTPUT_BYTES`, `events.rs:31`) is the bound, and
  §12.1 leaves wrapping to the terminal. Payload lines are verbatim tool
  output and may be non-ASCII — the ASCII-only rule governs forge's own
  chrome (gutters, header), exactly as it does for assistant answers; the
  header is ASCII.
- **Empty payload** renders one `    -> (no output)` line, so a header is
  never left dangling over nothing.
- **Nothing-to-show states** are informational meta lines, not errors:
  no results at all → `  - no tool results in this session yet`; ordinal out
  of range → `  - no tool result {n} in this session - {total} recorded (1
  is the most recent)`. This follows `/bg`'s "nothing is running" precedent
  (`controller.rs:331-336`). A non-numeric argument is a *usage* error
  (`  ! usage: /show [n]`), the controller's existing convention
  (`controller.rs:228`).

### Design call 4 — placement: a new pure module `crates/forge-chat/src/show.rs`

Selection (walk the log backwards, pair by `call_id`) is not the live
event→line mapping, so it does not go into `render.rs`'s
`TranscriptState::on_event` — whose `ToolResult` arm **must stay silent**
(AC5). A focused third pure module mirrors the crate's existing layout
(`command` = what a line says, `controller` = what may happen now, `render`
= the live stream). `lib.rs` gains `pub mod show;` and one crate-doc bullet.
(The alternative — two more functions in `render.rs` — was rejected: it
would blur the one thing `render.rs`'s module doc promises, the *live*
mapping's exhaustiveness.)

### Design call 5 — state gating: read-only and immediate in every state

`/show` mutates nothing: no turn, no session switch, no runtime rebuild. The
controller maps it to its action unconditionally, exactly like
`Parsed::Session => vec![Action::ShowSession]`
(`controller.rs:247-248`, commented "Read-only, so it answers in every
state, mid-turn included"). Mid-turn, `App::emit` routes the lines through
`ChatIo::notify` like every other mid-turn print (`app.rs:573-579`). Because
`/show` starts with a slash, the approval rule at `controller.rs:213-216`
already keeps it from being read as an approval answer.

## Out of Scope

- **Any change to live rendering.** The `ToolResult` arm of
  `TranscriptState::on_event` stays `Vec::new()`; TICKET-3 owns streaming
  render, and live payload dumps remain rejected by design §4.2.
- **Rendering other event kinds on demand** (`/show` for assistant messages,
  routing decisions, approvals). The ticket is tool results only; the
  `show.rs` shape generalizes later if wanted.
- **A paging UI**, incremental reveal, or `/show` with a run-id/event-pos
  address. One block, printed whole; the 64 KiB store cap is the bound.
- **`/last` as a separate command** (covered by `/show` with no argument —
  see Design call 1 and Open Question 2).
- **Cross-session inspection** (`/show` in a session other than the current
  one; that is `forge session show <id>`'s job, and `/session <id>` +
  `/show` composes).
- **Moving the store read off the executor** (`spawn_blocking`) or making
  `events_for` incremental. `/show` adds one more call site of the existing
  shape; the bounded-read fix is TICKET-8's, in the runtime, where every
  front end gets it.
- **Changes to `forge-cli`'s terminal/io layer** — no new `ChatIo`/`ChatHost`
  methods; the command is carried entirely by the compiled-in command table.

## CONTEXT REFERENCES

### Files to modify (all paths relative to the worktree root)

| file | what changes | why there |
| --- | --- | --- |
| `crates/forge-chat/src/command.rs` | `COMMANDS` entry (28-42), `Parsed::Show(Option<usize>)` (52-83), parse arm in `Command::parse` (97-169), tests | The one place slash parsing and the `/help`/completion table live. |
| `crates/forge-chat/src/show.rs` | **New.** `select` + `lines` + their unit tests | Design call 4. |
| `crates/forge-chat/src/lib.rs` | `pub mod show;` + crate-doc bullet (26-41) | Module list and the "what each module is" index. |
| `crates/forge-chat/src/controller.rs` | `Action::Show(Option<usize>)` (85-119), one `on_line` arm (next to 247-248), tests | One variant per effect; the driver stays a policy-free `match`. |
| `crates/forge-chat/src/app.rs` | `execute` arm (583-626), `do_show` (next to `do_show_session`, 889-909), driver tests | The only module that may touch the store. |
| `crates/forge-chat/src/testing.rs` | `FakeHost` constructor variant with canned read content | Driver tests need a non-empty tool payload; see Task 3's GOTCHA. |
| `crates/forge-cli/tests/chat.rs` | one process-level test + a scaffold script mode | The compiled binary over pipes proves the whole path. |
| `crates/forge-cli/tests/bdd/steps.rs` + `tests/features/chat.feature` | one scenario, one Given, one Then | BDD conventions below. |
| `docs/reference.md` | `/show` row in the slash table (498-514) + a short paragraph in Interactive chat | User-facing command reference. |
| `docs/superpowers/specs/2026-09-24-interactive-chat-ui-design.md` | §14 bullet marked shipped (one-line amendment, in the §16/§17 tradition) | The follow-up list must stay honest. |

### Patterns to follow (with the real code)

**Parse an optional-argument command — `/fork` (`command.rs:305-315`):**

```rust
fn parse_fork(argument: Option<&str>) -> Parsed {
    match argument {
        None => Parsed::Fork(None),
        Some(rest) => match rest.strip_prefix("--at") { ... },
    }
}
```

`/show` is simpler (bare optional number), but the shape — `None` maps to a
default, unparseable maps to `Parsed::Usage(&'static str)` — is this one.

**A read-only command, controller to driver (`controller.rs:247-248`,
`app.rs:889-902`):**

```rust
// Read-only, so it answers in every state, mid-turn included.
Parsed::Session => vec![Action::ShowSession],
```

```rust
fn do_show_session(&mut self) {
    let runs = self
        .host
        .service()
        .sessions()
        .events_for(&self.session_id)
        ...
}
```

`do_show` is this function with selection and rendering between the read
and the emit.

**Re-render recorded events from the store (`app.rs:989-1010`):**
`render_session_transcript` proves the resumed/continued path — the store,
not memory, is the history — and `do_attach`'s backlog loop
(`app.rs:854-859`) shows the same events→lines fan-out.

**The result gutter and its styles (`crates/forge-chat/src/io.rs:64-84`):**
`Line::ok` / `Line::failed` share `    -> ` and differ only in style — the
multi-line payload uses exactly these two constructors.

**The recorded payload's shape (`crates/forge-core/src/events.rs:183-190`):**

```rust
/// A tool's output as the model saw it, keyed by the call it answers.
/// Capped by [`cap_tool_output`].
ToolResult {
    call_id: String,
    tool: String,
    output: String,
    is_error: bool,
},
```

Pairing source — `crates/forge-core/src/tool.rs:31-35` (`ToolCall { id,
name, arguments }`) carried on `EventKind::AssistantMessage`
(`events.rs:178-182`). **Note:** `EventKind::ToolCallRequested` has *no*
`call_id` (`events.rs`, and as consumed at `render.rs:87-98`) — pairing goes
through the assistant message, not the request event.

**The existing human reader of these payloads** — `forge session show`
(`session_cmd.rs:61-69`) renders `tool_result call_id=… tool=… error=…
output=<80 chars>`; `/show` is its chat-native, full-payload counterpart.

**Testing harness** — `crates/forge-chat/src/testing.rs`: `ScriptedIo`
(lines 79-298: a line queue + captured output + scheduled interrupts) and
`FakeHost` (312-435: a real `AgentService` over `ScriptedMockModel` +
`MockExecution` + a temp `JsonlSessionStore`). Driver tests follow
`app.rs:1109-1138` (one turn, transcript assertions) and the two-chat
continued-session pattern at `app.rs:1409-1430`
(`resuming_a_session_rerenders_its_transcript_through_one_renderer`).

**Process-level conventions** — `crates/forge-cli/tests/chat.rs`: the
hermetic `forge()` helper (106-119: temp HOME/XDG, `FORGE_*` scrub,
`FORGE_TEST_MOCKS=1`, `NO_COLOR=1`), the piped `chat()` driver (134-165:
write all lines, close stdin, bounded wait), and `scaffold` (182-207) which
writes `alpha.rs` with known content `fn parse_config() {}\n`. **Load-bearing
property** (174-181): the per-run model factory re-parses the script from
disk once per *run*, and `/show` is a command, not a run — so a script with
one tool call followed by `/show` behaves deterministically.

**BDD conventions** — `tests/features/chat.feature` (imperative scenarios,
one behavior each) over `crates/forge-cli/tests/bdd/steps.rs`: the
`run_forge_with_stdin(&["chat"], &[...])` When steps (1950-1967), the
scripted-model Given to mirror (1936-1948, "a scripted mock model that
writes"), and a Then asserting on `world.last_stdout` (e.g. 2080-2087).

## IMPLEMENTATION PLAN

Four phases, one commit each, strictly ordered — each lands green and the
feature is useless-but-harmless until Task 3 wires it:

- **Phase 1 — the word exists:** `/show [n]` parses, appears in `/help` and
  completion, and garbage arguments get a usage line. Pure, `command.rs`
  only.
- **Phase 2 — selection and rendering exist:** `show.rs`, pure, fully
  unit-tested against hand-built event logs. Nothing calls it yet.
- **Phase 3 — the command does something:** `Action::Show` + `do_show` +
  driver tests over `ScriptedIo`/`FakeHost`, including the continued-session
  proof. The feature is complete in-process at the end of this phase.
- **Phase 4 — the product surface agrees:** process-level test, BDD
  scenario, `docs/reference.md`, design-doc amendment.

## STEP-BY-STEP TASKS

### Task 1: `/show [n]` parses and is advertised

**Files:** Modify `crates/forge-chat/src/command.rs`.

- [ ] **Step 1 — write the failing parse tests.**

  ACTION: extend `command.rs`'s `commands_parse_with_and_without_arguments`
  (406-454) and `a_command_missing_its_argument_reports_its_usage`
  (640-663), or add a focused new test, asserting:

  ```rust
  assert_eq!(Command::parse("/show", &s), Parsed::Show(None));
  assert_eq!(Command::parse("/show 2", &s), Parsed::Show(Some(2)));
  assert_eq!(Command::parse("  /show  3 ", &s), Parsed::Show(Some(3)));
  assert_eq!(Command::parse("/show abc", &s), Parsed::Usage("/show [n]"));
  assert_eq!(Command::parse("/show -1", &s), Parsed::Usage("/show [n]"));
  ```

  IMPLEMENT: nothing yet.
  PATTERN: `command.rs:159-162` (`/attach`'s optional-arg arm),
  `command.rs:305-315` (`parse_fork`).
  GOTCHA: the existing `every_advertised_command_parses` (679-688) iterates
  `COMMANDS` and asserts none parse as `Unknown`/`Prompt` — it fails the
  moment the `COMMANDS` entry lands without the parse arm, which is the TDD
  hook doing its job; `help_renders_one_line_per_command_and_nothing_else`
  (665-674) adapts on its own.
  VALIDATE: `cargo test -p forge-chat command` → FAIL (`Parsed::Show` does
  not exist).
  SATISFIES: part of AC7.

- [ ] **Step 2 — implement the parse surface.**

  ACTION:
  - `COMMANDS` (28-42) gains, positioned after `/attach`:
    `("/show", "re-render a recorded tool result: /show [n], latest first"),`
  - `Parsed` (52-83) gains, documented like its siblings:
    `/// `/show`, optionally `/show <n>`: the nth most recent tool result (1 = latest).`
    `Show(Option<usize>),`
  - `Command::parse` gains, next to the `"attach"` arm (159-162):

    ```rust
    "show" => match argument {
        None => Parsed::Show(None),
        Some(text) => match text.parse::<usize>() {
            Ok(n) => Parsed::Show(Some(n)),
            Err(_) => Parsed::Usage("/show [n]"),
        },
    },
    ```

  IMPLEMENT: exactly the above; no completion change.
  PATTERN: `command.rs:156-162`.
  GOTCHA: `Parsed::Usage` carries a `&'static str` — a literal, not a
  `format!`. `/show 0` deliberately parses (`0` is a `usize`); the *driver*
  answers it with the informational out-of-range line, which teaches the
  numbering — do not special-case it here. No `/show` argument completion:
  the `_ => Vec::new()` arm (230) already covers it, and inventing number
  candidates is noise.
  VALIDATE: `cargo test -p forge-chat command` → PASS.
  SATISFIES: AC7 (partially — `/help` and completion now carry it).

- [ ] **Step 3 — commit.**
  VALIDATE: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p forge-chat` → PASS.
  Commit: `git add -A && git commit -m "feat(chat): parse /show [n]"`.

### Task 2: `crates/forge-chat/src/show.rs` — pure selection and rendering

**Files:** Create `crates/forge-chat/src/show.rs`; modify
`crates/forge-chat/src/lib.rs`.

- [ ] **Step 1 — write the failing tests** (in-module `#[cfg(test)] mod
  tests`, the crate's convention).

  ACTION: create the module with the public surface sketched below and tests
  asserting:

  - `select` returns the most recent `ToolResult` for `n == 1`, the second
    most recent for `n == 2`, `None` for `n > total` and for an empty log.
  - the args hint is recovered by `call_id` pairing: an
    `AssistantMessage { tool_calls: [ToolCall::new("c1", "read_file",
    json!({"path": "src/main.rs"}))] }` followed by
    `ToolResult { call_id: "c1", .. }` renders a header containing
    `read_file src/main.rs`; an unpairable `call_id` falls back to the bare
    tool name.
  - a multi-line `output` renders one `Line` per payload line, each starting
    `    -> `; `is_error: true` renders those lines with `Style::Bad`
    (`Line::failed`) and success with `Style::Ok`.
  - an empty `output` renders the header plus `    -> (no output)`.
  - the header is ASCII; a non-ASCII payload passes through verbatim; the
    store's truncation marker (`cap_tool_output`'s `[forge: tool output
    truncated, …]`) survives untouched.

  PATTERN: test style of `render.rs`'s module (303-313's `ev()`/`texts()`
  helpers); event construction as in `render.rs:679-701`.

  IMPLEMENT: the module itself —

  ```rust
  //! On-demand rendering of recorded `tool_result` payloads: `/show`.
  //!
  //! `render.rs` owns the *live* event→line mapping, where `ToolResult` is
  //! deliberately silent (§4.2); this module is its on-demand counterpart —
  //! selection from a session's recorded events, then the same §4.1 gutters
  //! applied to a verbatim payload. Pure: no store, no terminal, no clock.

  use forge_core::{Event, EventKind};

  use crate::io::Line;

  /// One recorded tool result, selected from a session's log for `/show`.
  #[derive(Debug)]
  pub struct SelectedResult {
      /// 1 = the most recent tool result in the session.
      pub ordinal: usize,
      /// Total tool results recorded in the session.
      pub total: usize,
      pub tool: String,
      /// The friendly argument (`src/main.rs` in `read_file src/main.rs`),
      /// recovered from the assistant message that requested the call.
      pub args_hint: Option<String>,
      pub run_id: String,
      pub output: String,
      pub is_error: bool,
  }

  /// How many tool results `events` holds — for the out-of-range message.
  pub fn count(events: &[Event]) -> usize {
      events
          .iter()
          .filter(|e| matches!(e.kind, EventKind::ToolResult { .. }))
          .count()
  }

  /// The nth most recent `tool_result` in `events` (1 = latest), with the
  /// requesting call's friendly argument recovered by `call_id` pairing.
  pub fn select(events: &[Event], ordinal: usize) -> Option<SelectedResult> {
      // pair: walk forward, call_id -> (name, arguments-json-string) from
      // every AssistantMessage's tool_calls;
      // pick: walk backward, nth ToolResult;
      // hint: paired call -> crate::render::summarize_call(&name, &args_json),
      //       empty string -> None (a bare tool name is the honest fallback).
      ...
  }

  /// The §4.1 grammar, on demand: a meta header naming what was selected,
  /// then the verbatim payload under the result gutter — one `Line` per
  /// payload line, `failed` (Bad style) when the call errored.
  pub fn lines(result: &SelectedResult) -> Vec<Line> {
      ...
  }
  ```

  Header text: `tool result {ordinal} of {total}: {tool}[ {hint}] (run
  {run_id})`. Payload: `result.output.lines().map(|l| if is_error {
  Line::failed(l) } else { Line::ok(l) })`; empty output → one
  `Line::ok("(no output)")`/`Line::failed("(no output)")` to match.

  PATTERN: `io.rs:64-84` (the two result constructors),
  `render.rs:136-147` (split multi-line text into one `Line` per line),
  `render.rs:257-269` (`summarize_call` — `pub(crate)`, same crate, call it
  directly).
  GOTCHA: pair via `AssistantMessage.tool_calls`, **not**
  `ToolCallRequested` — the request event carries no `call_id`
  (`render.rs:87-98` shows its two fields). A `Line`'s text must not contain
  `\n` (the writer emits line-at-a-time): split with `str::lines()`. Do not
  ellipsize or cap the payload — the store cap is the bound (Design call 3).
  VALIDATE: `cargo test -p forge-chat show` → PASS.
  SATISFIES: AC1, AC6 (rendering half).

- [ ] **Step 2 — register the module.**

  ACTION: `lib.rs` gains `pub mod show;` (in the alphabetical module block,
  45-52) and one bullet in the crate-doc module list (26-41), e.g.
  `//! * [`show`] — `/show`: on-demand rendering of a recorded `tool_result`
  payload, selected from the session log (the live mapping in [`render`]
  stays silent for it).`
  IMPLEMENT: also extend the crate doc's executor-read enumeration (9-13:
  "`refresh_completions`' `list_runs()`…, `events_for`, and
  `fork_session`") to name `/show`'s `events_for` call, keeping the honesty
  note true.
  GOTCHA: the doc change rides this commit — the house rule is docs in the
  same commit as the behavior.
  VALIDATE: `cargo test -p forge-chat` → PASS; `cargo clippy --workspace
  --all-targets -- -D warnings` → PASS.
  SATISFIES: AC7 (partially).

- [ ] **Step 3 — commit.**
  VALIDATE: `cargo fmt --all --check && cargo test -p forge-chat` → PASS.
  Commit: `git add -A && git commit -m "feat(chat): select and render a recorded tool result"`.

### Task 3: wire it — `Action::Show` and `App::do_show`

**Files:** Modify `crates/forge-chat/src/controller.rs`,
`crates/forge-chat/src/app.rs`, `crates/forge-chat/src/testing.rs`.

- [ ] **Step 1 — controller: the failing tests, then the variant.**

  ACTION: tests first, in `controller.rs`'s style:
  - idle `/show` → `vec![Action::Show(None)]`;
  - `/show` while a turn is attached → the same action immediately (extend
    `during_turn_commands_act_immediately`, 961-967);
  - `/show` while an approval is pending does not answer it (pattern:
    `a_command_while_an_approval_is_pending_does_not_answer_it`, 972-984);
  - `/show abc` → one `Action::Write` containing `usage: /show [n]`.

  IMPLEMENT: `Action` (85-119) gains
  `/// `/show`, optionally `/show <n>`. Read-only: immediate in every state.`
  `Show(Option<usize>),` and `on_line` gains, next to `Parsed::Session`
  (247-248):

  ```rust
  // Read-only, so it answers in every state, mid-turn included.
  Parsed::Show(n) => vec![Action::Show(n)],
  ```

  PATTERN: `controller.rs:247-248`.
  GOTCHA: no approval-pending special case is needed — `/show` starts with
  `/`, so `looks_like_a_command` (`command.rs:254-256`) already routes it
  past the answer rule (213-216). Add it to the sweep in
  `nothing_this_module_writes_is_width_sensitive` (1035-1059) only if new
  `Write` lines were introduced by the controller — they are not (usage
  lines come from the existing `Parsed::Usage` arm).
  VALIDATE: `cargo test -p forge-chat controller` → PASS.
  SATISFIES: AC4 (state-machine half).

- [ ] **Step 2 — testing harness: a `FakeHost` with a known payload.**

  ACTION: `testing.rs` gains a constructor variant so a driver test can know
  what the tool "returned": generalize `FakeHost::build` (390-435) to take
  the canned read content, or add

  ```rust
  /// A scripted model over a `MockExecution` whose `read_file` returns
  /// `content` — the fixture for `/show`, which needs a known payload.
  pub fn with_script_and_read_content(json: &str, content: &str) -> (Self, TempDir)
  ```

  IMPLEMENT: use `MockExecution::with_read_content`
  (`crates/forge-execution/src/mock.rs:43-46`) in place of
  `MockExecution::new` on that path.
  PATTERN: `testing.rs:324-332` (`with_script`).
  GOTCHA: this is the ticket's one real fixture trap — `MockExecution::new`
  serves `Some("")` for reads (`mock.rs:91-94`), so without this constructor
  a `/show` driver test asserts on an empty payload and proves almost
  nothing (it exercises only the `(no output)` branch).
  VALIDATE: `cargo test -p forge-chat` → PASS (existing tests unaffected).
  SATISFIES: enables AC1/AC3's driver proofs.

- [ ] **Step 3 — driver: `do_show` and its tests.**

  ACTION: tests first, in `app.rs`'s test module:
  - `show_rerenders_the_most_recent_tool_result_after_a_turn` — host from
    `with_script_and_read_content` (script: a `read_file alpha.rs` call then
    a closing text; content e.g. `fn parse_config() {}`), lines
    `["explain alpha.rs", "/show", "/quit"]`; assert the output contains
    `  - tool result 1 of 1: read_file alpha.rs (run ` and
    `    -> fn parse_config() {}`.
  - `show_works_on_a_continued_session` — the two-chat pattern
    (`app.rs:1409-1430`): first chat runs the tool-call turn; second chat
    `Start::named(&session)` with lines `["/show", "/quit"]`; assert the
    payload appears — proving the store/backlog path, not memory.
  - `show_reports_when_there_is_nothing_to_show` — fresh session,
    `["/show", "/quit"]` → `no tool results in this session yet`.
  - `show_reports_an_out_of_range_ordinal` — one result recorded,
    `/show 9` → `no tool result 9 in this session - 1 recorded (1 is the
    most recent)`; `/show 0` gets the same shape.
  - `show_mid_turn_does_not_disturb_the_turn` — `with_slow_script`, batch
    lines `["something slow", "/show"]`: the show line (a "no results yet"
    or a result, depending on timing — assert on whichever the scripted
    fixture makes deterministic) prints, and the turn still completes with
    its answer and footer. Prefer asserting the deterministic half: exit 0,
    answer present, and the show line present.

  IMPLEMENT: in `execute` (583-626),
  `Action::Show(n) => self.do_show(n),` and beside `do_show_session`:

  ```rust
  /// `/show [n]` (design §14): re-render the nth most recent recorded
  /// tool result of the current session from its log — the one place the
  /// payloads live, since `TranscriptState` retains none (§5). The same
  /// synchronous store read `/session` and resume already make; mid-turn
  /// the lines go through `notify` like every other print (`emit`).
  fn do_show(&mut self, n: Option<usize>) {
      let ordinal = n.unwrap_or(1);
      match self.host.service().sessions().events_for(&self.session_id) {
          Ok(events) => match crate::show::select(&events, ordinal) {
              Some(selected) => {
                  for line in crate::show::lines(&selected) {
                      self.emit(line);
                  }
              }
              None => {
                  let total = crate::show::count(&events);
                  self.emit(Line::meta(if total == 0 {
                      "no tool results in this session yet".to_string()
                  } else {
                      format!(
                          "no tool result {ordinal} in this session - {total} recorded (1 is the most recent)"
                      )
                  }));
              }
          },
          Err(e) => self.emit(Line::bad(format!("error: {e}"))),
      }
  }
  ```

  PATTERN: `app.rs:889-909` (the store read), `app.rs:824-843` (`do_fork`'s
  error-as-bad-line shape), `app.rs:573-579` (`emit`).
  GOTCHA: do **not** read from `self.transcript` or add payload retention to
  `TranscriptState` (Design call 2); do not touch `render.rs`'s `ToolResult`
  arm (AC5); keep the read synchronous — `spawn_blocking` is TICKET-8's
  conversation, not this ticket's.
  VALIDATE: `cargo test -p forge-chat` → PASS.
  SATISFIES: AC1, AC2, AC3, AC4, AC5, AC6.

- [ ] **Step 4 — commit.**
  VALIDATE: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p forge-chat` → PASS.
  Commit: `git add -A && git commit -m "feat(chat): /show re-renders a recorded tool result"`.

### Task 4: process proof, BDD, and the docs sweep

**Files:** Modify `crates/forge-cli/tests/chat.rs`,
`crates/forge-cli/tests/bdd/steps.rs`, `tests/features/chat.feature`,
`docs/reference.md`,
`docs/superpowers/specs/2026-09-24-interactive-chat-ui-design.md`.

- [ ] **Step 1 — the process-level test.**

  ACTION: `chat.rs` gains a scaffold script mode (alongside `"prompt"` in
  `scaffold`, 187-195) whose script is a `read_file alpha.rs` call followed
  by a closing text reply — `alpha.rs` already exists with known content
  (`fn parse_config() {}\n`, written at 185). Then:

  ```rust
  #[test]
  fn show_rerenders_a_recorded_tool_result_from_piped_input() {
      // lines: ["read alpha", "/show"], piped; EOF drains and exits 0.
      // assert exit 0;
      // assert stdout contains "  - tool result 1 of 1: read_file alpha.rs (run ";
      // assert stdout contains "    -> fn parse_config() {}";
  }
  ```

  IMPLEMENT: drive it with the existing `chat()` helper (134-165).
  PATTERN: any existing test in the file (e.g. the approval round trip) for
  the `tempfile` + `scaffold` + `chat` + assert shape.
  GOTCHA: the script file is re-parsed once per *run* (174-181) and `/show`
  is not a run — do not write a two-entry script expecting the second entry
  to be consumed by `/show`. Stdout-only assertion: diagnostics never leave
  stderr (house rule), so no stderr assertions are needed.
  VALIDATE: `cargo test -p forge-cli --test chat show` → PASS.
  SATISFIES: AC1, AC6 (against the real binary).

- [ ] **Step 2 — the BDD scenario.**

  ACTION: `tests/features/chat.feature` gains:

  ```gherkin
  Scenario: /show re-renders a recorded tool result on demand
    Given an initialized project with a scripted mock model that reads "alpha.txt"
    When I chat with the lines "read alpha" and "/show"
    Then the chat output shows the recorded tool result
  ```

  `steps.rs` gains, mirroring 1936-1948 exactly:

  ```rust
  #[given(expr = "an initialized project with a scripted mock model that reads {string}")]
  // script: [{"tool_calls": [{"id": "call_1", "name": "read_file",
  //   "arguments": {"path": "<path>"}}]}, {"text": "read it"}]
  // plus world.write_file(<path>, known content), model/router config as
  // the "writes" step does.
  ```

  and a Then asserting `world.last_stdout` contains the `    -> `-guttered
  known content.

  IMPLEMENT: reuse the existing "I chat with the lines {string} and
  {string}" When (1950-1955) unchanged.
  PATTERN: `steps.rs:1936-1948`, `steps.rs:2080-2087`.
  GOTCHA: the scenario keeps BDD at the user-story level — the
  continued-session and out-of-range proofs already live in `app.rs`'s
  driver tests; do not add `--continue` plumbing to the BDD world for this
  ticket.
  VALIDATE: `cargo test -p forge-cli --test bdd` → PASS.
  SATISFIES: AC1 (end to end, user-visible).

- [ ] **Step 3 — docs.**

  ACTION:
  - `docs/reference.md`: one row in the slash-command table (498-514), in
    the same two-column shape —
    `/show       re-render a recorded tool result: /show [n], latest first`
    — and two or three sentences in the Interactive chat section: payloads
    come from the session log (so continued sessions work), are the redacted
    ≤64 KiB recorded form, and `forge session show <id>` remains the
    cross-session reader.
  - Design doc §14: strike the *"Rendering `tool_result` payloads on
    demand"* bullet into a one-line amendment in the §16/§17 tradition
    ("shipped 2026-10-01 as `/show [n]`; `ToolResult` stays silent in the
    live mapping").
  - `ARCHITECTURE.md` and `README.md`: no change — the crate-map line
    (262-264) stays true ("event->transcript rendering"), and the README
    lists no slash commands. Verify this claim while editing; if either
    names the command set, update it in this commit.

  GOTCHA: house rule — docs land in the same commit as the behavior they
  describe; do not leave reference.md for a later PR.
  VALIDATE: `cargo test -p forge-cli --test bdd` → PASS (no behavior change;
  guards accidental edits).
  SATISFIES: AC7, AC8 (partially).

- [ ] **Step 4 — full gate, then commit.**
  VALIDATE: the full VALIDATION COMMANDS block below, in the worktree → all
  PASS.
  Commit: `git add -A && git commit -m "test(chat): /show end to end, plus docs"`.

## TESTING STRATEGY

The ticket's three layers, mapped onto the harnesses that already exist:

- **Parse (pure, `command.rs`)** — mirrors the existing parse-table tests:
  `/show`, `/show 2`, whitespace, garbage → usage. The
  `every_advertised_command_parses` sweep (679-688) turns the `COMMANDS`
  entry into a failing test for free until the parse arm exists.
- **Selection + rendering (pure, `show.rs`)** — mirrors `render.rs`'s test
  module: hand-built `Vec<Event>` (`Event::new("run-1", "sess-1", kind)`),
  assertions on `Line.text`/`Line.style` only (styles are semantic; color is
  the writer's, `io.rs:9-11`). Covers: most-recent-first selection,
  out-of-range and empty logs, `call_id` pairing and its fallback,
  multi-line split, `is_error` styling, empty payload, non-ASCII and
  truncation-marker passthrough.
- **Driver wiring (in-process, `app.rs` + `testing.rs`)** —
  `ScriptedIo`/`FakeHost` over a real `AgentService` with a temp
  `JsonlSessionStore`, exactly the harness §13.1 describes. The continued-
  session test (two chats over one service, `app.rs:1409-1430`'s pattern)
  is the ticket's "works on continued sessions (backlog), not just live
  runs" acceptance, proven in-process.
- **Process level (`forge-cli/tests/chat.rs`)** — the compiled binary over
  pipes, hermetic per the file's conventions; proves the `CliHost`/
  `PipedIo` path carries the command with no new seam code.
- **BDD (`tests/features/chat.feature`)** — one scenario in the existing
  cucumber harness (`cargo test -p forge-cli --test bdd`), user-story level
  only.

Deliberately absent (and why): no pty test (the command renders through the
same `write`/`notify` path every other line uses — §13.4's reasoning
applies), no new `ChatIo`/`ChatHost` surface to fake, no snapshot/golden
files.

## VALIDATION COMMANDS

Run in the worktree (`worktrees/t5-show-tool-results`), in this order, all
green before the final commit (the umbrella is `just verify`,
`Justfile:48-49`):

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p forge-chat -p forge-cli
cargo test --workspace
cargo test -p forge-cli --test bdd
```

## ACCEPTANCE CRITERIA

- **AC1** — After any turn with tool calls, `/show` (≡ `/show 1`) re-renders
  the most recent recorded `tool_result` verbatim through the §4.1 grammar:
  a `  - ` meta header (`tool result 1 of N: <tool>[ <friendly-arg>] (run
  <id>)`) followed by the payload as `    -> ` lines (`Style::Ok`, or
  `Style::Bad` via `Line::failed` when `is_error`). Ticket: "re-renders …
  through the same visual grammar".
- **AC2** — `/show <n>` selects the nth most recent result; an out-of-range
  or zero ordinal prints one informational line naming the count and the
  numbering — never an error, never a turn, never a panic. A non-numeric
  argument prints `  ! usage: /show [n]`.
- **AC3** — It works on continued sessions: `forge chat --continue`,
  `--session <id>`, or `/session <id>` followed by `/show` renders backlog
  payloads identically, because the source is the session log, not memory
  (ticket: "works on continued sessions (backlog), not just live runs").
  After `/bg`, `/show` in the fork still reaches the backgrounded turn's
  recorded results-so-far.
- **AC4** — `/show` is read-only and immediate in every chat state (idle,
  mid-turn, approval pending, detached jobs live): it never queues behind a
  turn, never answers an approval, never rebuilds the runtime.
- **AC5** — Live rendering is unchanged: the `ToolResult` arm of
  `TranscriptState::on_event` still returns `Vec::new()`, and
  `render.rs`'s `replay_and_bookkeeping_kinds_render_nothing` test (676-701)
  stays green.
- **AC6** — What `/show` prints is exactly the recorded payload — redacted
  at append (`store.rs:281-301`), capped at 64 KiB with the truncation
  marker verbatim (`events.rs:36-49`). No second redaction path, no
  un-redaction, no display-side cap.
- **AC7** — `/show` appears in `/help`, Tab completion, and
  `docs/reference.md`'s slash table; the design doc's §14 follow-up bullet
  is amended as shipped; `lib.rs`'s crate doc lists the module. All in the
  same commits as the behavior.
- **AC8** — Every VALIDATION COMMANDS line passes in the worktree.

## OPEN QUESTIONS / ASSUMPTIONS

1. **Does `/show <n>` index tool calls or runs?** The ticket's acceptance
   says "`/show 2` re-renders *that run's* tool results". This plan reads
   that loosely and indexes **tool results, most-recent-first** (Design
   call 1): runs have no on-screen ordinal, and a run can hold arbitrarily
   many calls. *Recommended default:* as specified. If the epic owner meant
   run ordinals literally, the change is confined to `show::select`'s
   grouping plus header wording — confirm before Task 1 lands if possible.
2. **Should `/last` exist as its own command?** §14 says "a `/last` **or**
   `/show <n>` command". *Recommended default:* no separate `/last` —
   `/show` with no argument is that command, and one entry keeps the
   `/help`/completion surface minimal. If a reviewer wants the alias, it is
   one parse arm (`"last" => Parsed::Show(None)`) and one `COMMANDS` row,
   deliberately not pre-built here.
3. **Should the rendered payload be display-capped below the store's 64
   KiB?** *Recommended default:* no second cap — the command exists to see
   the payload, the store cap already bounds it, and §12.1 leaves wrapping
   to the terminal. If real use finds 64 KiB dumps hostile, a `/show n
   --head N` style option is a follow-up, not a retrofit of this ticket.

Assumptions (settled upstream, stated so a cold agent need not re-derive
them): the data source is the store for live *and* continued sessions —
there is no in-memory payload retention to consult (Design call 2); the
state gating is "read-only, immediate" per the existing `/session`
precedent (Design call 5); and the rendering grammar is the §4.1 result
gutter, not a new visual class (Design call 3).

## NOTES

- `events_for` re-reads and re-parses the whole session log on every
  `/show` — the same shape `/session`, resume, and `/fork` already have, and
  one more data point for TICKET-8's bounded/incremental runtime-side read.
  Do not fix it here.
- A fork's log is a verbatim prefix copy, so `/show` in a fork cannot see
  results the source session records *after* the fork point. That is the
  documented nature of forks, not a `/show` gap.
- `forge session show <id>` remains the cross-session, JSON-capable reader;
  `/show` deliberately composes with `/session <id>` instead of growing a
  session argument.
- The `--json` refusal is untouched: `/show` is a transcript command inside
  the chat, which already refuses `--json` outright; nothing here changes
  stdout discipline (human output on stdout, diagnostics on stderr).
- Merge-order note for the epic: TICKET-1/TICKET-3 add a streaming
  assistant-delta kind and touch `render.rs`; this ticket adds a sibling
  module and one `command.rs`/`controller.rs`/`app.rs` arm each. Conflicts
  should be textual at worst (`COMMANDS`, the `Parsed`/`Action` enums, the
  `execute` match) and semantic nowhere — `/show` reads recorded events, so
  it is indifferent to how the live stream grows.

## AMENDMENTS

(none yet — append here, dated, if the plan changes in flight)
