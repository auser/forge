# TICKET-3 — Streaming Render: chat incremental block + ACP live chunks — Implementation Plan

> **For agentic workers:** implement this plan task-by-task (superpowers:subagent-driven-development or superpowers:executing-plans). Steps use checkbox (`- [ ]`) syntax. Work in the clean worktree `/Users/auser/work/rust/mine/forge/worktrees/t3-streaming-render` (HEAD f5f8235 — includes all of Wave 1: TICKET-1 streaming core, TICKET-4 path completion, TICKET-5 `/show`, TICKET-6 skill activation); every `file:line` citation below is against that tree.

## Feature Description

TICKET-1 gave the runtime a streaming spine: a provider that implements
`ModelProvider::stream_complete` has its text broadcast and persisted as
ordered `assistant_delta` events ahead of the verbatim `assistant_message`
(`docs/superpowers/plans/2026-10-01-streaming-core.md`; merged code at
`crates/forge-core/src/events.rs:187-194`,
`crates/forge-runtime/src/service.rs:1186-1267`). No front end renders those
deltas yet: forge-chat has a silent arm (`crates/forge-chat/src/render.rs:152-153`)
and forge-acp ignores them (`crates/forge-acp/src/dispatch.rs:402-425`). This
ticket is the render half the design doc promised in §14 bullet 1 — "the
transcript gains an incremental assistant block and nothing else changes —
the renderer is already per-event"
(`docs/superpowers/specs/2026-09-24-interactive-chat-ui-design.md:1113-1117`):

- **forge chat** renders `assistant_delta` events as one growing assistant
  block — text appended to the line in flight as it arrives, never a line
  per delta — and the closing `assistant_message` prints only what the
  stream has not already shown (so: no double-print, ever).
- **forge acp** forwards deltas as live `agent_message_chunk` notifications
  during the turn (today the answer goes out as one chunk after the turn,
  `crates/forge-acp/src/server.rs:580-593`).
- Piped (non-TTY) mode's behavior is specified and pinned: the same
  fragments, appended plainly, so a captured transcript is byte-identical
  to today's.
- The answer-once rule (§4.3) and the needle fast path's shape render
  exactly as today.

**Goal:** a chat run over the scripted mock shows its answer arriving during
the turn (no TTY needed to prove it — the render layer is pure); an ACP
client sees chunks before the `session/prompt` response; a non-streaming
provider (the `stream_complete` default fallback, TICKET-1 D2) behaves
byte-identically to before on both surfaces.

**Architecture to inherit:** `forge-chat` is the pure core / thin shell
split (ARCHITECTURE.md:279-287): `render.rs` decides what the transcript
*is* as a pure function and `forge-cli`'s writers only paint it. `forge-acp`
keeps every protocol decision pure in `dispatch.rs` while `server.rs` owns
the I/O (ARCHITECTURE.md:364-372). Both halves of this ticket put the
decision in the pure layer and only the byte-writing in the shell.

## User Story

As a person watching forge answer — in my terminal, or in Zed over ACP — I
want the answer to appear as it is written instead of landing in one lump at
the end of the turn, so a long answer reads the way Claude Code / pi.dev
feels, and so I can start reading (or hit Ctrl-C) before the model has
finished. When the model does not stream, nothing about what I see changes.

## Problem

A streaming run today emits, in order: `assistant_delta` ×N, then the
assembled `assistant_message`. The chat's silent arm drops the deltas and
prints the message as one blank-wrapped block
(`crates/forge-chat/src/render.rs:136-147`); ACP's ignore arm drops the
deltas and the server sends `run_result.text` as one chunk after the loop
settles. So the text arrives at the *end* no matter how early the runtime
knew it — the single biggest feel-gap the epic names.

The honest constraint that shapes the whole design: **the transcript is an
inline, append-only scrollback.** `ChatIo::write(&Line)`
(`crates/forge-chat/src/io.rs:197`) writes one whole line — `println!` in
both real writers (`crates/forge-cli/src/chat/terminal_io.rs:185-187`,
`crates/forge-cli/src/chat/piped_io.rs:258-264`). There is no in-place
redraw, no cursor addressing, no region ownership; the full-screen TUI that
could redraw a growing block was rejected on shape and cost (design §2.2)
and that decision stands. So "one growing block" cannot mean "repaint a
region". What it *can* mean, and what the line paradigm honestly supports,
is: **append text to the line currently on screen without terminating it**
— `print!` with no `\n` — which is exactly how a streamed answer looks when
it types itself out in scrollback. The render layer stays pure: it emits a
new *kind of `Line`* (a fragment), and the writers learn one new trick
(write without the newline, then flush).

Two correctness rules ride along, and both are about the delta stream being
a *prefix* of the answer rather than the answer:

1. **No double-print.** The closing `assistant_message` carries the whole
   text the deltas already showed. The renderer must print only the unshown
   suffix (a lagged broadcast can drop deltas; design §5's promise is
   "lagging degrades the transcript, never the answer",
   `docs/superpowers/specs/2026-09-24-interactive-chat-ui-design.md:362-364`).
2. **The answer-once rule survives** (§4.3): the driver prints
   `RunOutcome.text` only when nothing textual rendered
   (`crates/forge-chat/src/app.rs:521-536`), which is the fast path's shape
   (an `AssistantMessage` with empty text and one tool call). Deltas alone
   must not arm or disarm that rule wrongly in either direction.

## Solution

Eight design calls, each argued from the code.

**D1 — A `Line` can be a fragment.** `Line` gains
`pub fragment: bool` (`crates/forge-chat/src/io.rs:38-42`) and one
constructor:

```rust
/// A piece of a streamed assistant answer: appended to the line in
/// flight, with no trailing newline. Only `render`'s incremental block
/// produces these; every other line terminates itself.
pub fn fragment(text: impl Into<String>) -> Self {
    Self { style: Style::Plain, text: text.into(), fragment: true }
}
```

Plain style, no gutter — the assistant text class is the one class with no
gutter (§4.1), and the palette never decorates `Plain`
(`crates/forge-cli/src/chat/palette.rs:58-64`), so a fragment paints as its
bytes. The `gutter()` constructor and `Line::plain` set `fragment: false`;
nothing else constructs `Line` by literal (checked: only `io.rs:102-108`'s
`gutter`). The alternative — a separate `ChatIo::write_fragment` method —
was rejected: `on_event`'s contract is `Vec<Line>` end to end, and a second
output channel would give the driver two orderings to keep consistent.

**D2 — The block is opened by the first delta and closed by whatever ends
it.** `TranscriptState` (`crates/forge-chat/src/render.rs:34-45`) gains one
field: `stream: Option<String>` — the concatenated, already-rendered delta
text of the in-flight response, `Some` exactly while the block is open.

- `AssistantDelta { text }`: empty text renders nothing and opens nothing.
  Otherwise append to `stream` and emit `vec![Line::fragment(text)]` — or,
  for the *first* delta of a block, `vec![Line::plain(""), Line::fragment(text)]`:
  the blank line above is §4.1's assistant-block grammar, emitted once.
- Closing emits `close_lines(terminated)`: if the on-screen text ends with
  `\n` the line is already terminated and only the blank below is owed
  (`vec![Line::plain("")]`); otherwise two empties — the first terminates
  the text line, the second is the blank below. Byte-for-byte this is what
  today's `AssistantMessage` arm produces for the same text: open `"\n"` +
  fragments + close `"\n\n"` == `["", <text lines>, ""]` through the writer.
  A render test pins that byte-equivalence (Task 2).
- `AssistantMessage { text, .. }` owns the close when deltas preceded it
  (D3). *Any other event kind* that arrives with a stream open closes it
  first — implemented as a prefix step at the top of `on_event`, so a tool
  line, an error, or `  ! cancelled` never lands on the half-written line:

```rust
let mut lines = match &event.kind {
    EventKind::AssistantDelta { .. } | EventKind::AssistantMessage { .. } => Vec::new(),
    _ => self.close_stream(),
};
lines.extend(/* the existing match, with the two arms below */);
lines
```

A tool call never interleaves *within* one response's stream — the runtime
emits deltas during the model call and the `AssistantMessage` before
dispatching tools — so a run that interleaves text and tools renders as
alternating closed blocks, exactly today's shape with the text arriving
incrementally. The prefix close is the defensive half: it covers a cancelled
or failed run whose message never comes.

**D3 — The closing message prints only the unshown suffix.** The
`AssistantMessage` arm becomes:

```rust
EventKind::AssistantMessage { text, .. } => {
    if text.trim().is_empty() {
        // The fast path's shape (or a tool-call-only response). Contract
        // says no stream is open; close defensively, print nothing.
        return self.close_stream();
    }
    self.rendered_assistant_text = true;
    match self.stream.take() {
        None => assistant_block(text),              // today's behavior, extracted
        Some(streamed) => match text.strip_prefix(&streamed) {
            // Normal case, and lag recovery: the stream showed a prefix.
            Some(suffix) => {
                let mut lines = Vec::new();
                if !suffix.is_empty() {
                    lines.push(Line::fragment(suffix));
                }
                lines.extend(close_lines(text.ends_with('\n')));
                lines
            }
            // The stream is not a prefix of the message: the provider
            // broke its concat contract (the runtime already warned,
            // service.rs:1256-1263), or a lagged broadcast dropped deltas
            // from the middle. Close the partial line and print the whole
            // message as a fresh block — the answer whole and once wins
            // over tidy.
            None => {
                let mut lines = close_lines(streamed.ends_with('\n'));
                lines.extend(assistant_block(text));
                lines
            }
        },
    }
}
```

`strip_prefix`, not `starts_with` + slice: it is char-boundary-safe by
construction. `assistant_block(text)` is today's block extracted verbatim
(`render.rs:140-146`: blank, `text.lines()` as `Line::plain`, blank), made
`pub(crate)` so `app.rs`'s defensive branch can share it (Task 3).

**D4 — Deltas do NOT set `rendered_assistant_text`; the outcome fallback
becomes stream-aware.** The flag keeps its exact meaning — "the answer has
been *fully* rendered" — set only by a non-empty `AssistantMessage`, as
today. A delta does not set it, which is what keeps design §5's lag promise
true: if deltas arrived but their message was lost to broadcast lag, the
flag is still false and the driver's outcome print still fires — completed
with only the unshown tail rather than reprinted:

```rust
/// The §4.3 fallback, stream-aware: the lines to print for the run
/// outcome when no `AssistantMessage` rendered the answer — the fast
/// path's shape (no text, one tool call) *and* the lagged-stream shape
/// (deltas arrived, their message did not). Empty when the answer
/// already rendered.
pub fn outcome_lines(&mut self, text: &str) -> Vec<Line> {
    if self.rendered_assistant_text || text.trim().is_empty() {
        return Vec::new();
    }
    match self.stream.take() {
        None => assistant_block(text),
        Some(streamed) => {
            let tail = text.strip_prefix(&streamed).unwrap_or(text);
            let mut lines = Vec::new();
            if !tail.is_empty() {
                lines.push(Line::fragment(tail));
            }
            lines.extend(close_lines(text.ends_with('\n')));
            lines
        }
    }
}
```

`finish_run`'s `Ok(Ok(outcome))` arm (`app.rs:520-536`) becomes a call to
`outcome_lines(&outcome.text)`; the `None`-transcript defensive branch keeps
today's whole-text block via `assistant_block`. For a non-streaming run
(no stream open) this is byte-identical to today. For the fast path —
no deltas, empty message, flag false — the outcome prints whole, exactly as
today (pinned by the unedited `a_turn_with_no_assistant_text_falls_back_to_the_run_outcome`,
`app.rs:1241-1251`).

**D5 — Lines that did not come from the transcript close an open stream
before printing.** Mid-turn slash commands render through `Action::Write`
→ `App::emit` (`app.rs:579-585`) without touching `TranscriptState` — a
`/show` answer or an error line would otherwise land on the half-written
line. The guard lives in `emit`, the one funnel:

```rust
fn emit(&mut self, line: Line) {
    // A line that did not come from the transcript may arrive while a
    // streamed answer's block is open; close the block first. Fragment
    // lines are the block, so they never trigger this (and `close_stream`
    // is empty when nothing is open, so transcript-produced lines — which
    // already closed it inside `on_event` — cost nothing here).
    if !line.fragment {
        if let Some(transcript) = self.transcript.as_mut() {
            let closing = transcript.close_stream();
            for close in closing {
                self.emit_inner(close);
            }
        }
    }
    self.emit_inner(line);
}
```

(`emit_inner` is today's `emit` body — the `notify`-while-attached /
`write`-between-turns choice — extracted.) One deliberate exception:
background-watch notices go straight to `io.notify` (`app.rs:437-447`) and
keep today's documented may-land-mid-line behavior (README/reference known
limitation); a notice interrupting a streamed line is the same accepted
artifact class, not a new one. Also fixed here: `settle_attached_run`
(`app.rs:564-574`) closes before `take()`, so a followed run whose stream
died mid-block does not leave the footer to land on the half-written line.

**D6 — Piped mode writes the same fragments, flushed.** No coalescing, no
delay, no second render path. §6.2 already promises "the transcript is
byte-identical either way; only the mechanism differs" and §12.2 "identical
modulo escape sequences"; `PipedIo`'s two writers
(`crates/forge-cli/src/chat/piped_io.rs:258-264`) and `TerminalIo`'s
(`terminal_io.rs:185-194`) each become:

```rust
let text = self.palette.paint(line.style, &line.text);
if line.fragment {
    print!("{text}");
    // stdout is line-buffered (a `LineWriter`); without the flush a
    // fragment shows nothing until the block closes — the whole point
    // is that it shows now.
    let _ = std::io::stdout().flush();
} else {
    println!("{text}");
}
```

(`TerminalIo::notify` passes the painted text to its `StdoutPrinter`
without appending the `\n` when `line.fragment`; `StdoutPrinter::print`
already flushes, `terminal_io.rs:396-401`.) A captured piped transcript
then holds the answer assembled in place — byte-identical to today's whole
block — and a `forge chat < script` run reads exactly as before. The
process-level proof is cheap because the bytes are the assertion (Task 5).

**D7 — ACP: deltas are live `agent_message_chunk`s; each
`assistant_message` flushes its own remainder; the end-of-turn send becomes
a tail.** Three pieces, the first two pure:

- `TurnState` (`crates/forge-acp/src/dispatch.rs:230-239`) gains
  `streamed: String` (chunk text sent for the response currently streaming)
  and `accounted: Option<String>` (the text of the last `AssistantMessage`
  seen — that response is fully sent). The delta arm:

```rust
EventKind::AssistantDelta { text } => {
    if self.accounted.is_some() {
        // A new response is streaming; the previous one was fully sent.
        self.accounted = None;
        self.streamed.clear();
    }
    self.streamed.push_str(text);
    vec![TurnAction::Notify(SessionUpdate::AgentMessageChunk {
        content: ContentBlock::text(text.clone()),
    })]
}
```

- `AssistantMessage` leaves the ignore arm (`dispatch.rs:402-425`, whose
  "one `agent_message_chunk`" comment at :409 and the `// TICKET-3…` note at
  :413 are both rewritten) and becomes the per-response flush:

```rust
// The replay record doubles as the flush point: whatever of this
// response's text the stream did not deliver (a lagged broadcast's gap)
// goes out now, as one chunk. Non-streaming providers take the same path
// with an empty `streamed` — their whole text, here, at message time.
EventKind::AssistantMessage { text, .. } => {
    let suffix = text.strip_prefix(&self.streamed).unwrap_or(text).to_string();
    self.streamed.clear();
    self.accounted = Some(text.clone());
    if suffix.is_empty() {
        Vec::new()
    } else {
        vec![TurnAction::Notify(SessionUpdate::AgentMessageChunk {
            content: ContentBlock::text(suffix),
        })]
    }
}
```

  Flushing here rather than only at turn end is what covers *intermediate*
  responses (a tool-loop run's "Let me read that file" before its tool
  calls): their text never appears in `RunOutcome.text`, so a turn-end-only
  flush could never recover their lagged gaps.
- `server.rs`'s end-of-turn chunk (`server.rs:580-593`) becomes the tail,
  via one pure method:

```rust
/// The part of the run's final text the client has not been sent: empty
/// when the final `AssistantMessage` was seen (its chunks plus the flush
/// covered it whole); the outcome's unstreamed tail when that message was
/// lost to lag; the whole outcome when the answer was never a message at
/// all — the needle fast path's text is a tool result, carried by no
/// `AssistantMessage` (its empty-text message sets `accounted` to `""`,
/// which never equals a non-empty outcome, so the tail is the whole text).
pub fn unsent_tail(&self, outcome_text: &str) -> String {
    if self.accounted.as_deref() == Some(outcome_text) {
        return String::new();
    }
    outcome_text
        .strip_prefix(&self.streamed)
        .unwrap_or(outcome_text)
        .to_string()
}
```

  and in `run_turn`, keeping the existing non-blank guard:

```rust
if let Ok(text) = run_result.as_ref()
    && !text.trim().is_empty()
{
    let tail = state.unsent_tail(text);
    if !tail.is_empty() {
        self.notify_update(
            session_id,
            SessionUpdate::AgentMessageChunk { content: ContentBlock::text(tail) },
        )
        .await;
    }
}
```

The case analysis, exhaustively: non-streaming provider → message flush
sends the whole text at message time (one chunk, as today, slightly earlier
— the drain at `server.rs:570-573` still precedes the response, so the
wire ordering "chunks before the `session/prompt` response" is unchanged);
streaming, no loss → deltas, suffix `""`, no tail; lagged deltas → message
flush sends the gap; lagged message → `unsent_tail` sends the gap; provider
contract violation (prefix mismatch) → flush sends the whole text, the
runtime's own `warn` (`service.rs:1256-1263`) explains the duplication; fast
path → whole outcome text, as today; blank outcome → the outer guard sends
nothing, as today.

**D8 — The "renderer is already per-event" promise holds.** Nothing about
the driver's loop changes: `apply_event` (`app.rs:449-460`) still maps one
event to lines and emits them; ACP's `apply` (`server.rs:606-638`) still
maps one event to actions. The whole ticket is one new `Line` shape, two
match arms, two small driver touch points (`emit`'s guard, `finish_run`'s
fallback), and one accounting method. That is the design doc's §14 bullet 1
landed as written.

## Out of Scope

- **Provider SSE** (OpenAI-compatible and Anthropic decode, tool-call
  reassembly from deltas, `EgressPolicy` redirect re-checks, mid-stream
  errors): TICKET-2, `forge-providers` only. After this ticket lands, real
  providers still take TICKET-1's default fallback (no deltas) until
  TICKET-2 implements their override — the chat and ACP then light up with
  no further changes here. The docs edits (Task 5) say exactly this.
- **Full-screen TUI / in-place redraw**: rejected on shape and cost by the
  design doc (§2.2); this ticket deliberately does not reopen it. The
  incremental block is appended text in inline scrollback (D1/D6), which is
  the honest mechanism the recorded architecture supports.
- **The `rustyline` `ExternalPrinter` restoration** (TICKET-7): streamed
  fragments mid-turn ride the existing `StdoutPrinter` path, inheriting its
  documented may-interleave-with-a-half-typed-line trade; nothing here makes
  that better or worse.
- **The whitespace-spanning redaction gap found while planning** (NOTES):
  `Bearer\s+\S+`-style patterns can leak in a delta stream when a provider
  splits a chunk at the pattern's internal whitespace. That is the runtime's
  carry rule (TICKET-1's `complete_streaming`), not the render layer — this
  ticket's renderer tolerates the resulting prefix mismatch (D3/D7's
  fallback) but the fix belongs to the runtime/TICKET-2 follow-up.
- No changes to `forge serve` (SSE forwards deltas generically already),
  `forge mcp`, replay, `forge session show`, the controller, or the command
  set. No new config keys, no feature flags.

## Metadata

- Date: 2026-10-02. Base: worktree `t3-streaming-render`, HEAD f5f8235.
- Ticket: `specs/tickets/interactive-chat-feel.md` TICKET-3 (lines 58-70).
  Depends on: TICKET-1 (implemented; merged in this tree). Independent of
  TICKET-2 (mocks prove the whole path; real providers light it up).
- Estimate: ~700–950 lines including tests (the ticket said 600–900; the
  ACP tail accounting and the emit guard are the growth). Crates touched:
  forge-chat (`io`, `render`, `app`, `testing`), forge-cli
  (`chat/terminal_io.rs`, `chat/piped_io.rs`, `tests/chat.rs`,
  `tests/acp.rs`, `tests/bdd/steps.rs`), forge-acp (`dispatch.rs`,
  `dispatch/tests.rs`, `server.rs`, `lib.rs` docs); docs: `docs/reference.md`,
  `ARCHITECTURE.md`, the design doc's amendment section. One touched feature
  file: `tests/features/streaming.feature`. No new source files, no new
  dependencies.

## CONTEXT REFERENCES

### Files to read first (with why)

| file:line | why |
| --- | --- |
| `crates/forge-chat/src/render.rs:34-62` | `TranscriptState` today: the answer-once flag and the counts. D2's `stream` field joins it |
| `crates/forge-chat/src/render.rs:136-147` | the `AssistantMessage` arm — the block shape (`["", …text.lines, ""]`) `assistant_block` extracts |
| `crates/forge-chat/src/render.rs:152-153` | T1's silent delta arm (`// TICKET-3 renders these as one growing block`) — the integration point |
| `crates/forge-chat/src/render.rs:180-202` | `rendered_assistant_text()` and `footer()` — the two other places state surfaces |
| `crates/forge-chat/src/io.rs:38-108` | `Line`/`Style` and the constructors — D1's field and `Line::fragment` go here; the doc at :30-37 ("the gutter is applied by the constructor, exactly once") is the invariant to preserve |
| `crates/forge-chat/src/app.rs:449-460` | `apply_event`: event → `transcript.on_event` → `emit` per line |
| `crates/forge-chat/src/app.rs:514-555` | `finish_run`: the answer-once outcome print at :521-536 (D4 rewrites it), footer at :545-547 |
| `crates/forge-chat/src/app.rs:564-574` | `settle_attached_run` — D5's close-before-`take()` fix |
| `crates/forge-chat/src/app.rs:579-585` | `emit` — the one funnel D5 guards |
| `crates/forge-chat/src/app.rs:856-885` | `do_attach`'s backlog walk — deltas in a backlog render through the same `on_event`, and a mid-stream attach simply continues the open block |
| `crates/forge-chat/src/app.rs:1026-1063` | `render_session_transcript` (resume) — same one-renderer path; the 200-line bound at :1048 (see Q1) |
| `crates/forge-chat/src/testing.rs:236-243,306-312` | `ScriptedIoHandle::record`/`write`/`notify` — the captured-output writer that must learn fragments |
| `crates/forge-chat/src/testing.rs:601-642` | `SlowModel` — note it does **not** override `stream_complete`, so `with_slow_script` turns emit no deltas (T1's D2 fallback, live in-tree). This is why the arrival test needs a new `TrickleModel`, not the slow script |
| `crates/forge-cli/src/chat/terminal_io.rs:185-194` | `TerminalIo::write`/`notify` — `println!` today; D6's fragment branch and the flush |
| `crates/forge-cli/src/chat/terminal_io.rs:394-402` | `StdoutPrinter` — already flushes; `notify` hands it text without the `\n` for fragments |
| `crates/forge-cli/src/chat/piped_io.rs:258-264` | `PipedIo::write`/`notify` — same change; piped mode is D6 |
| `crates/forge-cli/src/chat/palette.rs:58-64` | `paint` never decorates `Plain` — fragments carry no escapes, so piped and TTY transcripts stay byte-identical modulo the *other* styles |
| `crates/forge-acp/src/dispatch.rs:230-287` | `TurnState` and its doc ("every event kind that has no honest ACP slot maps to nothing") — D7's two fields and the doc's extension |
| `crates/forge-acp/src/dispatch.rs:290-300,402-425` | `on_event` and the ignore arm with the TICKET-3 note — the two arms that leave the list |
| `crates/forge-acp/src/server.rs:507-603` | `run_turn`: subscribe-before-start, the `select!` loop, the drain at :570-573, and the one-chunk send at :580-593 (D7's tail) |
| `crates/forge-acp/src/server.rs:986-1128` | the in-crate server test harness (`server()`, `prompt()`, `next()`) and `session_new_then_prompt_drives_a_run_and_ends_the_turn` — the non-streaming pin (stays green unedited) |
| `crates/forge-acp/src/protocol.rs:342-367,422-430` | `ContentBlock::text` and `SessionUpdate` (internally tagged `sessionUpdate`, snake_case) — the chunk shape; the wire test at `dispatch/tests.rs:885-900` pins it |
| `crates/forge-runtime/src/service.rs:1186-1267` | `complete_streaming` (merged T1): what a delta *is* by the time the front ends see it — hold-back at whitespace, redacted through the one boundary, contract warning at :1256-1263 |
| `crates/forge-core/src/events.rs:187-194` | the `AssistantDelta` variant and its "rendering-only" doc |
| `crates/forge-providers/src/scripted.rs:95-148` | `word_chunks` + the scripted mock's `stream_complete` — the deterministic delta source every test here rides |
| `docs/superpowers/specs/2026-09-24-interactive-chat-ui-design.md` §4.1–4.3, §5, §6.2, §12.2–12.3, §14 bullet 1 | the visual grammar, the answer-once rule, write-vs-notify, the piped rules, and the bullet this ticket ships |

### Tests that pin today's exact shape (know them before changing anything)

- `crates/forge-chat/src/render.rs:705-717` (`a_delta_renders_nothing_until_ticket_3`)
  — T1's silence pin. **Replaced** in Task 2 by the new delta suite (its
  second assertion, "a delta is not the answer-once record", survives as its
  own test: deltas still do not set the flag, D4).
- `crates/forge-chat/src/render.rs:619-676` — the three `AssistantMessage`
  block tests (bare block, multi-line, empty-counts-as-nothing). Stay
  **unedited**: they are the non-streaming path's pin.
- `crates/forge-chat/src/app.rs:1166-1194` (`a_turn_renders_and_answers_once`)
  — counts the answer exactly once in the captured transcript. Passes
  **unedited** while exercising the streamed path (its `with_script` model
  streams since T1) — the byte-equivalence proof at the driver level.
- `crates/forge-chat/src/app.rs:1241-1251`
  (`a_turn_with_no_assistant_text_falls_back_to_the_run_outcome`) — the
  fast-path shape. **Unedited** (AC3).
- `crates/forge-chat/src/app.rs:1507-1529`
  (`resuming_a_session_rerenders_its_transcript_through_one_renderer`) — the
  resumed log now contains deltas; passes **unedited** because backlog
  deltas + the message render byte-identically to the message alone.
- `crates/forge-acp/src/server.rs:1090-1128`
  (`session_new_then_prompt_drives_a_run_and_ends_the_turn`) — `MockModel`
  (no `stream_complete` override): exactly one whole-text chunk, now sent by
  the message flush instead of the end-of-turn send. **Unedited** (AC3's ACP
  half).
- `crates/forge-acp/src/dispatch/tests.rs:494-524`
  (`bookkeeping_events_produce_no_updates`) — does not list
  `AssistantMessage`/`AssistantDelta`; stays **unedited**.
- `crates/forge-cli/tests/acp.rs:443-450` — asserts one chunk's text equals
  `"all done"`. **Updated** in Task 5 to "chunks concatenate to the answer,
  in ≥2 chunks" — the scripted answer now streams.
- `crates/forge-cli/tests/bdd/steps.rs:2066-2094`
  (`acp_client_saw_tool_call_and_message`) — same single-chunk assertion on
  the same `"all done"` script (`scripted_mock_writes`, steps.rs:910-922).
  **Updated** the same way.
- `crates/forge-cli/tests/chat.rs:405-447`
  (`a_piped_conversation_runs_two_turns_in_one_session`) — counts
  `stdout.matches("the answer") == 2` over a scripted model that now
  streams: passes **unedited** because fragments assemble contiguously in
  captured stdout (D6's whole point).

### New files

None. Everything lands in the modules named above; the one edited Gherkin
file is `tests/features/streaming.feature` (a new ACP scenario appended to
TICKET-1's feature).

### Patterns to follow

The one funnel — `crates/forge-chat/src/app.rs:579-585`:

```rust
    /// A transcript line: `notify` while a turn is attached (it may land
    /// while a prompt is up), `write` between turns. Both take the same
    /// [`Line`], so the transcript is identical either way (§6.2).
    fn emit(&mut self, line: Line) {
        if self.events.is_some() {
            self.io.notify(&line);
        } else {
            self.io.write(&line);
        }
    }
```

The current one-chunk send this ticket replaces —
`crates/forge-acp/src/server.rs:580-593`:

```rust
        // The model's answer, as one chunk. forge's loop produces final
        // text rather than a token stream, so streaming it token by token
        // would be theatre; one honest chunk is what the protocol gets.
        if let Ok(text) = run_result.as_ref()
            && !text.trim().is_empty()
        {
            self.notify_update(
                session_id,
                SessionUpdate::AgentMessageChunk {
                    content: ContentBlock::text(text.clone()),
                },
            )
            .await;
        }
```

The scripted mock's chunking — `crates/forge-providers/src/scripted.rs:137-147`
(the runtime's hold-back at `service.rs:1214-1224` then re-cuts these at
whitespace, so e.g. `"all done"` reaches the front ends as `"all "` +
`"done"`):

```rust
    async fn stream_complete(
        &self,
        request: CompletionRequest,
        on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<CompletionResponse, ForgeError> {
        let response = self.next_reply(request);
        for chunk in word_chunks(&response.content) {
            on_delta(chunk);
        }
        Ok(response)
    }
```

## IMPLEMENTATION PLAN (phases)

- **Phase 1 — the fragment primitive (Task 1).** `Line::fragment` exists and
  all four writers (TerminalIo, PipedIo, ScriptedIo) honor it; nothing emits
  one yet; the workspace is green and every transcript byte-identical.
- **Phase 2 — the pure renderer (Task 2).** The incremental block, the close
  rules, the suffix rule, `outcome_lines`; T1's silence test becomes the new
  suite. `cargo test -p forge-chat render` needs no TTY and proves every
  byte-level claim.
- **Phase 3 — the chat driver (Task 3).** `emit`'s guard, `finish_run` via
  `outcome_lines`, `settle_attached_run`'s close-before-take; the
  `TrickleModel` arrival test. Existing driver tests pass unedited — over
  streamed deltas.
- **Phase 4 — ACP (Task 4).** D7's two arms, `unsent_tail`, the end-of-turn
  tail; pure dispatch tests plus one in-crate streaming turn test.
- **Phase 5 — proof from the binaries + honest docs (Task 5).** The two
  process/BDD expectation updates (acp.rs, steps.rs), one new piped chat
  process test, one new BDD scenario; `reference.md`, `ARCHITECTURE.md`,
  `forge-acp`'s crate doc, and the design doc's amendment all stop saying
  "no token streaming" where that is now false.

## STEP-BY-STEP TASKS

### Task 1: `Line::fragment` and the four writers

- [ ] **Step 1: Write the failing tests.** In `crates/forge-chat/src/io.rs`'s
  test module:

```rust
/// A streamed answer's unit: no gutter, Plain style, and marked so the
/// writer appends it to the line in flight instead of terminating it.
#[test]
fn a_fragment_is_plain_text_that_does_not_end_the_line() {
    let fragment = Line::fragment("the ans");
    assert_eq!(fragment.style, Style::Plain);
    assert_eq!(fragment.text, "the ans");
    assert!(fragment.fragment);
    assert!(
        !Line::plain("the answer").fragment && !Line::meta("note").fragment,
        "every other line terminates itself"
    );
}
```

- [ ] **Step 2: Run** `cargo test -p forge-chat` → FAIL (no such field/constructor).
- [ ] **Step 3: Implement.**
  - `crates/forge-chat/src/io.rs`: add `pub fragment: bool` to `Line` with the
    doc comment from D1; set `fragment: false` in `gutter()` and
    `Line::plain`; add `Line::fragment`. Extend the `Line` struct doc
    (:30-37) with one sentence: a fragment is the one shape that is not a
    whole line — the writer appends it to the line in flight.
  - `crates/forge-cli/src/chat/terminal_io.rs:185-194`: D6's branch in
    `write`; in `notify`, hand `StdoutPrinter` the painted text *without* the
    trailing `\n` when `line.fragment`.
  - `crates/forge-cli/src/chat/piped_io.rs:258-264`: the same branch in both
    `write` and `notify` (identical today, identical after).
  - `crates/forge-chat/src/testing.rs:236-243,306-312`: `record` takes
    whether to terminate the line; `write`/`notify` pass `!line.fragment`.
    The captured `output()` is then exactly the bytes a terminal's
    scrollback would show — which is what every app-level assertion reads.
- [ ] **Step 4: Run** `cargo test -p forge-chat -p forge-cli` → PASS
  (nothing emits fragments yet, so all captured output is unchanged).
- [ ] **Step 5:** full gate (VALIDATION COMMANDS below) → PASS. **Commit** —
  `git commit -m "feat(chat): Line::fragment — text that continues the line in flight"`

**ACTION** `crates/forge-chat/src/io.rs`, `crates/forge-chat/src/testing.rs`,
`crates/forge-cli/src/chat/terminal_io.rs`, `crates/forge-cli/src/chat/piped_io.rs`
**PATTERN** constructor discipline per `io.rs:44-108`; the flush per
`StdoutPrinter` (`terminal_io.rs:396-401`)
**GOTCHA** `print!` alone does not flush: Rust's stdout is a `LineWriter`,
so a fragment without `\n` sits in the buffer until one — the explicit
`std::io::stdout().flush()` is the feature, not a nicety. Keep
`fragment: false` in *every* existing constructor; the field defaults are
the bug otherwise.
**VALIDATE** `cargo test -p forge-chat -p forge-cli && cargo clippy --workspace --all-targets -- -D warnings`
**SATISFIES** the mechanism half of AC1/AC5; nothing user-visible yet.

---

### Task 2: `TranscriptState` renders the incremental block

- [ ] **Step 1: Write the failing tests** in `crates/forge-chat/src/render.rs`'s
  test module. Replace `a_delta_renders_nothing_until_ticket_3` (:705-717)
  with the suite below. The writer simulation used twice is:

```rust
/// What a writer's captured bytes would be: fragments append, everything
/// else terminates its line.
fn written(lines: &[Line]) -> String {
    let mut out = String::new();
    for line in lines {
        out.push_str(&line.text);
        if !line.fragment {
            out.push('\n');
        }
    }
    out
}
```

```rust
#[test]
fn deltas_grow_one_block_from_the_first_blank_line() {
    let mut s = TranscriptState::new();
    let first = s.on_event(&ev(EventKind::AssistantDelta { text: "the ".into() }));
    assert_eq!(first, vec![Line::plain(""), Line::fragment("the ")]);
    let second = s.on_event(&ev(EventKind::AssistantDelta { text: "answer".into() }));
    assert_eq!(second, vec![Line::fragment("answer")]);
    assert!(s.stream_open(), "the block stays open until its message");
    assert!(
        !s.rendered_assistant_text(),
        "deltas alone do not disarm the outcome fallback (D4)"
    );
}

/// The whole point of the fragment shape: streamed and non-streamed
//  renderings of one answer are the same bytes.
#[test]
fn a_streamed_answer_is_byte_identical_to_the_same_answer_printed_whole() {
    let mut streamed = TranscriptState::new();
    let mut lines = Vec::new();
    for text in ["the ", "answer ", "is\n", "ready"] {
        lines.extend(streamed.on_event(&ev(EventKind::AssistantDelta { text: text.into() })));
    }
    lines.extend(streamed.on_event(&ev(EventKind::AssistantMessage {
        text: "the answer is\nready".into(),
        tool_calls: Vec::new(),
    })));

    let mut whole = TranscriptState::new();
    let whole_lines = whole.on_event(&ev(EventKind::AssistantMessage {
        text: "the answer is\nready".into(),
        tool_calls: Vec::new(),
    }));
    assert_eq!(written(&lines), written(&whole_lines));
    assert!(streamed.rendered_assistant_text());
}

/// A lagged broadcast drops deltas; the message prints only what the
/// stream has not shown.
#[test]
fn a_message_after_partial_deltas_prints_only_the_suffix() {
    let mut s = TranscriptState::new();
    let _ = s.on_event(&ev(EventKind::AssistantDelta { text: "the ".into() }));
    let lines = s.on_event(&ev(EventKind::AssistantMessage {
        text: "the answer".into(),
        tool_calls: Vec::new(),
    }));
    assert_eq!(
        lines,
        vec![Line::fragment("answer"), Line::plain(""), Line::plain("")],
        "suffix, then the close"
    );
    assert_eq!(written(&lines), "answer\n\n");
}

/// A provider that broke the fragment contract gets its answer printed
/// whole, once, after the partial line is closed.
#[test]
fn a_message_that_does_not_extend_its_deltas_prints_whole() {
    let mut s = TranscriptState::new();
    let _ = s.on_event(&ev(EventKind::AssistantDelta { text: "gar".into() }));
    let lines = s.on_event(&ev(EventKind::AssistantMessage {
        text: "whole text".into(),
        tool_calls: Vec::new(),
    }));
    assert_eq!(
        written(&lines),
        "gar\n\n\nwhole text\n\n",
        "partial line closed, then the whole message as a fresh block"
    );
    assert!(s.rendered_assistant_text());
}

/// The defensive close: a turn that fails or is cancelled mid-stream
/// never leaves the next line on the half-written one.
#[test]
fn an_error_or_cancellation_closes_the_open_block_first() {
    for kind in [
        EventKind::Error { message: "boom".into() },
        EventKind::Cancelled { reason: "cancelled by user".into() },
    ] {
        let mut s = TranscriptState::new();
        let _ = s.on_event(&ev(EventKind::AssistantDelta { text: "partial ".into() }));
        let lines = s.on_event(&ev(kind));
        assert!(lines[0].text.is_empty() && !lines[0].fragment, "closed first: {lines:?}");
        assert!(!s.stream_open());
    }
}

/// Text and tools alternate as closed blocks: the stream's close precedes
/// the tool gutter, and the next response opens a fresh block.
#[test]
fn a_tool_call_between_two_responses_sits_between_two_closed_blocks() {
    let mut s = TranscriptState::new();
    let mut out = String::new();
    for kind in [
        EventKind::AssistantDelta { text: "reading ".into() },
        EventKind::AssistantDelta { text: "it".into() },
        EventKind::AssistantMessage {
            text: "reading it".into(),
            tool_calls: vec![ToolCall::new("c1", "read_file", serde_json::json!({"path": "a.rs"}))],
        },
        EventKind::ToolCallRequested { tool: "read_file".into(), args_summary: r#"{"path":"a.rs"}"#.into() },
        EventKind::AssistantDelta { text: "done".into() },
        EventKind::AssistantMessage { text: "done".into(), tool_calls: Vec::new() },
    ] {
        out.push_str(&written(&s.on_event(&ev(kind))));
    }
    assert_eq!(out, "\nreading it\n\n  * read_file a.rs\n\ndone\n\n");
}

/// A text that already ends in a newline is owed only the blank below.
#[test]
fn a_stream_ending_in_a_newline_closes_with_one_blank() {
    let mut s = TranscriptState::new();
    let _ = s.on_event(&ev(EventKind::AssistantDelta { text: "line\n".into() }));
    let lines = s.on_event(&ev(EventKind::AssistantMessage {
        text: "line\n".into(),
        tool_calls: Vec::new(),
    }));
    assert_eq!(lines, vec![Line::plain("")]);
}

#[test]
fn an_empty_delta_renders_nothing_and_opens_nothing() {
    let mut s = TranscriptState::new();
    assert!(s.on_event(&ev(EventKind::AssistantDelta { text: String::new() })).is_empty());
    assert!(!s.stream_open());
}

/// A resumed session's log holds deltas and the message; re-rendering the
/// backlog is the same bytes as the message alone — §7's one-renderer rule.
#[test]
fn a_backlog_of_deltas_and_the_message_rerenders_as_the_message() {
    let mut s = TranscriptState::new();
    let mut lines = Vec::new();
    for kind in [
        EventKind::AssistantDelta { text: "the ".into() },
        EventKind::AssistantDelta { text: "first ".into() },
        EventKind::AssistantDelta { text: "answer".into() },
        EventKind::AssistantMessage { text: "the first answer".into(), tool_calls: Vec::new() },
    ] {
        lines.extend(s.on_event(&ev(kind)));
    }
    assert_eq!(written(&lines), "\nthe first answer\n\n");
}

/// D4: deltas arrived, the message was lost to lag — the outcome completes
/// the open block with only the unshown tail.
#[test]
fn the_outcome_fallback_completes_a_lagged_stream_with_its_tail() {
    let mut s = TranscriptState::new();
    let _ = s.on_event(&ev(EventKind::AssistantDelta { text: "the ".into() }));
    let lines = s.outcome_lines("the answer");
    assert_eq!(written(&lines), "answer\n\n");
}

/// …and the two shapes the fallback was built for are unchanged.
#[test]
fn the_outcome_fallback_is_today_for_the_fast_path_and_stays_silent_after_a_rendered_answer() {
    let mut fast_path = TranscriptState::new();
    let _ = fast_path.on_event(&ev(EventKind::AssistantMessage {
        text: String::new(),
        tool_calls: vec![ToolCall::new("c1", "read_file", serde_json::json!({"path": "x"}))],
    }));
    assert_eq!(written(&fast_path.outcome_lines("fn x() {}")), "\nfn x() {}\n\n");

    let mut answered = TranscriptState::new();
    let _ = answered.on_event(&ev(EventKind::AssistantMessage {
        text: "the answer".into(),
        tool_calls: Vec::new(),
    }));
    assert!(answered.outcome_lines("the answer").is_empty(), "never twice");
    // A blank outcome prints nothing either way.
    assert!(TranscriptState::new().outcome_lines("").is_empty());
}
```

- [ ] **Step 2: Run** `cargo test -p forge-chat render` → FAIL.
- [ ] **Step 3: Implement** in `crates/forge-chat/src/render.rs`, per D2–D4:
  the `stream` field; `close_lines(terminated: bool)`; `pub fn
  close_stream(&mut self) -> Vec<Line>`; `pub fn stream_open(&self) ->
  bool`; the `pub(crate) fn assistant_block(text: &str) -> Vec<Line>`
  extraction (today's :140-146 verbatim); the prefix close at the top of
  `on_event`; the two rewritten arms (D2/D3); `pub fn outcome_lines(&mut
  self, text: &str) -> Vec<Line>` (D4). Update the module doc (:1-9) with
  the one new sentence: deltas render as fragments of one growing block;
  the closing message prints only the unshown suffix. Update the
  `AssistantMessage` arm's existing comment (:131-135) to describe the
  suffix accounting.
- [ ] **Step 4: Run** `cargo test -p forge-chat render` → PASS, then
  `cargo test -p forge-chat` → PASS (`app.rs` does not compile until Task
  3 — keep this task's `app.rs` build green by leaving `finish_run`'s
  existing body calling the *existing* pieces; if the extraction forces a
  touch there, land the one-line `assistant_block` call now and the
  `outcome_lines` swap in Task 3).
- [ ] **Step 5:** full gate → PASS. **Commit** —
  `git commit -m "feat(chat): render assistant deltas as one growing block"`

**ACTION** `crates/forge-chat/src/render.rs`
**PATTERN** the block shape per `render.rs:136-147`; "silent is a decision"
comment style per `render.rs:148-159`
**GOTCHA** (a) `strip_prefix`, never `starts_with` + manual slicing — the
boundary must be a char boundary by construction. (b) Do **not** set
`rendered_assistant_text` in the delta arm (D4): a lagged message must
leave `outcome_lines` armed. (c) The prefix-close at the top of `on_event`
excludes exactly two kinds — `AssistantDelta` (extends the stream) and
`AssistantMessage` (owns the close, D3). (d) `close_stream` must be
idempotent (empty when closed): `emit`'s guard calls it around lines that
already closed the stream inside `on_event`. (e) `Line` does not implement
`PartialEq` against `&str` — the tests above compare `Vec<Line>` to
constructed `Line`s, which needs `Line: PartialEq` (it derives it, io.rs:38).
**VALIDATE** `cargo test -p forge-chat && cargo clippy --workspace --all-targets -- -D warnings`
**SATISFIES** AC1's pure half, AC4, AC6's byte-equivalence.

---

### Task 3: the chat driver — guard, fallback, and the arrival proof

- [ ] **Step 1: Write the failing tests.** First the fixture, in
  `crates/forge-chat/src/testing.rs` — a model that streams its scripted
  reply with a real delay *between* chunks, so "the text is on screen while
  the turn is still running" is observable with no TTY:

```rust
/// Streams its scripted reply one chunk at a time with a real delay
/// between chunks — the fixture that makes "the answer arrives during the
/// turn" observable from a test (`with_slow_script`'s `SlowModel` does not
/// override `stream_complete`, so its turns emit no deltas at all; TICKET-1
/// D2). The scripted mock answers synchronously, so the chunks are
/// collected first and replayed with sleeps — the staggering is what a
/// test observes, not the collection.
struct TrickleModel {
    inner: ScriptedMockModel,
    chunk_delay: Duration,
}

#[async_trait]
impl ModelProvider for TrickleModel {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn capabilities(&self) -> ModelCapabilities {
        self.inner.capabilities()
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, ForgeError> {
        self.inner.complete(request).await
    }

    async fn stream_complete(
        &self,
        request: CompletionRequest,
        on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<CompletionResponse, ForgeError> {
        let mut chunks = Vec::new();
        let response = self
            .inner
            .stream_complete(request, &mut |d: &str| chunks.push(d.to_string()))
            .await?;
        for chunk in chunks {
            tokio::time::sleep(self.chunk_delay).await;
            on_delta(&chunk);
        }
        Ok(response)
    }
}
```

plus `FakeHost::with_trickled_script(json: &str, chunk_delay: Duration) ->
(Self, TempDir)` built like `with_slow_script` (`testing.rs:435-453`). Then
the test, in `crates/forge-chat/src/app.rs`'s test module:

```rust
/// The ticket's headline: the answer is on screen *while the turn is still
/// running*, and the whole answer still prints exactly once.
#[tokio::test]
async fn a_streamed_answer_arrives_during_the_turn_and_prints_once() {
    let (host, _tmp) = FakeHost::with_trickled_script(
        r#"[{"text": "the answer arrives word by word"}]"#,
        Duration::from_millis(50),
    );
    let mut io = ScriptedIo::batch(["what is the answer"]);
    let chat = tokio::spawn(run(io.handle(), host, Start::fresh()));

    // Half-way through the trickle the partial answer is visible and the
    // turn has not closed: no footer yet.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !io.output().contains("the answer arrives") {
        assert!(
            std::time::Instant::now() < deadline,
            "no partial answer ever appeared:\n{}",
            io.output()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        !io.output().contains("  = "),
        "the turn is still streaming, so there is no footer yet:\n{}",
        io.output()
    );

    let code = tokio::time::timeout(Duration::from_secs(10), chat)
        .await
        .expect("the chat exits at EOF")
        .expect("the chat task did not panic")
        .expect("chat runs");
    assert_eq!(code, 0);
    let out = io.output();
    assert_eq!(
        out.matches("the answer arrives word by word").count(),
        1,
        "assembled in place, printed once:\n{out}"
    );
    assert!(out.contains("  = "), "a footer closes the turn:\n{out}");
}
```

(The deadline-poll pattern is `bg_after_attach…`'s, `app.rs:1441-1450`.) And
the mid-turn guard test:

```rust
/// A `/show` answer typed mid-stream must not land on the half-written
/// line: `emit` closes the open block before any line that did not come
/// from the transcript.
#[tokio::test]
async fn a_command_answered_mid_stream_closes_the_block_first() {
    let (host, _tmp) = FakeHost::with_trickled_script(
        r#"[{"text": "the answer arrives word by word"}]"#,
        Duration::from_millis(60),
    );
    let mut io = ScriptedIo::new(["what is the answer"]);
    let chat = tokio::spawn(run(io.handle(), host, Start::fresh()));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !io.output().contains("the answer") {
        assert!(std::time::Instant::now() < deadline, "stream never started:\n{}", io.output());
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    io.push_line("/show");
    io.push_line("/quit");
    let code = tokio::time::timeout(Duration::from_secs(10), chat)
        .await.expect("exits").expect("no panic").expect("runs");
    assert_eq!(code, 0);
    let out = io.output();
    let show_at = out.find("no tool results in this session yet").expect("the /show answer");
    // The /show line starts at a line boundary — never mid-answer.
    assert!(out[..show_at].ends_with('\n'), "the block was closed first:\n{out}");
    assert_eq!(out.matches("the answer arrives word by word").count(), 1, "{out}");
}
```

- [ ] **Step 2: Run** `cargo test -p forge-chat` → FAIL (no
  `with_trickled_script`; and the driver pieces are not yet wired).
- [ ] **Step 3: Implement** in `crates/forge-chat/src/app.rs`:
  - `emit` gains D5's guard; extract the notify/write choice into
    `fn emit_inner(&mut self, line: Line)` (today's body verbatim).
  - `finish_run`'s `Ok(Ok(outcome))` arm (:520-536) becomes:

```rust
            Ok(Ok(outcome)) => {
                // §4.3: print the outcome's text only if nothing textual
                // was rendered live — the fast path's shape, and the rule
                // that keeps an ordinary turn's answer from printing
                // twice. `outcome_lines` is the stream-aware form: a
                // lagged final message is completed with its unshown tail,
                // not repeated.
                let lines = match self.transcript.as_mut() {
                    Some(transcript) => transcript.outcome_lines(&outcome.text),
                    // Not reached in practice (start_turn always installs
                    // the transcript); keep today's shape for it.
                    None if outcome.text.trim().is_empty() => Vec::new(),
                    None => crate::render::assistant_block(&outcome.text),
                };
                for line in lines {
                    self.emit(line);
                }
            }
```

  - `settle_attached_run` (:564-574): close before `take()` —

```rust
        if let Some(transcript) = self.transcript.as_mut() {
            for line in transcript.close_stream() {
                self.emit(line);
            }
        }
        if let Some(transcript) = self.transcript.take() {
            self.emit(transcript.footer());
        }
```

- [ ] **Step 4: Run** `cargo test -p forge-chat` → PASS. Confirm the
  *unedited* pins still pass and now exercise the streamed path:
  `a_turn_renders_and_answers_once`,
  `resuming_a_session_rerenders_its_transcript_through_one_renderer`,
  `a_turn_with_no_assistant_text_falls_back_to_the_run_outcome`.
- [ ] **Step 5:** full gate → PASS. **Commit** —
  `git commit -m "feat(chat): stream-aware answer-once fallback and mid-stream line guard"`

**ACTION** `crates/forge-chat/src/app.rs`, `crates/forge-chat/src/testing.rs`
**PATTERN** the one-funnel `emit` per `app.rs:576-585`; the deadline poll
per `app.rs:1432-1450`; `SlowModel`'s provider-wrapper shape per
`testing.rs:601-634`
**GOTCHA** (a) The guard fires only for non-fragment lines — a fragment IS
the open block, and guarding it would close the stream it is extending.
(b) `settle_cancelled_run`'s timeout arm (:501-510) is already safe: its
`cancelled (the turn is still unwinding)` line goes through `emit` before
the transcript is cleared, so the guard closes the block first — keep that
order. (c) `TrickleModel` must NOT poll the cancel marker (that is
`SlowModel`'s trick for a different job); these tests never interrupt a
trickle — interrupt-mid-stream transcript behavior is covered purely at
render level (`an_error_or_cancellation_closes_the_open_block_first`).
(d) `bg` notices bypass `emit` (`on_bg`, app.rs:437-447) by design — see
Q2.
**VALIDATE** `cargo test -p forge-chat && cargo clippy --workspace --all-targets -- -D warnings`
**SATISFIES** AC1 (driver half), AC3, AC4, AC5's in-process half.

---

### Task 4: ACP — live chunks, the per-message flush, and the end-of-turn tail

- [ ] **Step 1: Write the failing tests** in
  `crates/forge-acp/src/dispatch/tests.rs` (helpers: `event`,
  `updates(state, kinds)` at :24/:254):

```rust
#[test]
fn a_delta_becomes_a_live_message_chunk() {
    let mut state = TurnState::for_run(RUN, ROOT);
    let out = updates(&mut state, vec![EventKind::AssistantDelta { text: "the ".into() }]);
    assert_eq!(out.len(), 1);
    assert!(
        matches!(&out[0], SessionUpdate::AgentMessageChunk { content }
            if matches!(content, ContentBlock::Text { text } if text == "the ")),
        "{out:?}"
    );
}

/// A fully streamed response flushes nothing at its message: the chunks
/// were the text.
#[test]
fn a_fully_streamed_message_flushes_nothing() {
    let mut state = TurnState::for_run(RUN, ROOT);
    let out = updates(
        &mut state,
        vec![
            EventKind::AssistantDelta { text: "all ".into() },
            EventKind::AssistantDelta { text: "done".into() },
            EventKind::AssistantMessage { text: "all done".into(), tool_calls: Vec::new() },
        ],
    );
    assert_eq!(out.len(), 2, "two chunks, no flush: {out:?}");
    assert!(state.unsent_tail("all done").is_empty(), "fully sent");
}

/// A lagged stream's gap is flushed by the message…
#[test]
fn a_message_flushes_the_suffix_its_deltas_missed() {
    let mut state = TurnState::for_run(RUN, ROOT);
    let out = updates(
        &mut state,
        vec![
            EventKind::AssistantDelta { text: "all ".into() },
            EventKind::AssistantMessage { text: "all done".into(), tool_calls: Vec::new() },
        ],
    );
    assert_eq!(out.len(), 2);
    assert!(
        matches!(&out[1], SessionUpdate::AgentMessageChunk { content }
            if matches!(content, ContentBlock::Text { text } if text == "done")),
        "the suffix, not the whole text again: {out:?}"
    );
    assert!(state.unsent_tail("all done").is_empty());
}

/// …and a message lost to lag is covered by the end-of-turn tail.
#[test]
fn the_tail_is_what_a_lost_message_never_sent() {
    let mut state = TurnState::for_run(RUN, ROOT);
    let _ = updates(&mut state, vec![EventKind::AssistantDelta { text: "all ".into() }]);
    assert_eq!(state.unsent_tail("all done"), "done");
}

/// No deltas at all: the message sends the whole text (today's one chunk,
/// moved earlier) — and the tail then has nothing left.
#[test]
fn a_non_streaming_response_is_one_chunk_at_message_time() {
    let mut state = TurnState::for_run(RUN, ROOT);
    let out = updates(
        &mut state,
        vec![EventKind::AssistantMessage { text: "all done".into(), tool_calls: Vec::new() }],
    );
    assert_eq!(out.len(), 1);
    assert!(state.unsent_tail("all done").is_empty());
}

/// The fast path: the answer is a tool result carried by no message text,
/// so the tail is the whole outcome — exactly today's send.
#[test]
fn the_fast_paths_outcome_is_the_whole_tail() {
    let mut state = TurnState::for_run(RUN, ROOT);
    let _ = updates(
        &mut state,
        vec![EventKind::AssistantMessage {
            text: String::new(),
            tool_calls: vec![forge_core::ToolCall::new("c1", "read_file", json!({"path": "a.rs"}))],
        }],
    );
    assert_eq!(state.unsent_tail("fn a() {}"), "fn a() {}");
}

/// Two responses in one run (a tool loop): each flushes itself, and the
/// second response's deltas do not append to the first's accounting.
#[test]
fn each_response_streams_and_flushes_on_its_own() {
    let mut state = TurnState::for_run(RUN, ROOT);
    let out = updates(
        &mut state,
        vec![
            EventKind::AssistantDelta { text: "reading ".into() },
            EventKind::AssistantDelta { text: "it".into() },
            EventKind::AssistantMessage { text: "reading it".into(), tool_calls: vec![
                forge_core::ToolCall::new("c1", "read_file", json!({"path": "a.rs"})),
            ] },
            EventKind::AssistantDelta { text: "done".into() },
            EventKind::AssistantMessage { text: "done".into(), tool_calls: Vec::new() },
        ],
    );
    let texts: Vec<&str> = out
        .iter()
        .filter_map(|u| match u {
            SessionUpdate::AgentMessageChunk { content: ContentBlock::Text { text } } => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["reading ", "it", "done"], "{texts:?}");
    assert!(state.unsent_tail("done").is_empty());
}

/// A provider that broke the concat contract gets its whole text sent at
/// the message — duplication in the editor, explained by the runtime's own
/// warning, rather than a lost answer.
#[test]
fn a_contract_violation_sends_the_whole_text() {
    let mut state = TurnState::for_run(RUN, ROOT);
    let _ = updates(&mut state, vec![EventKind::AssistantDelta { text: "gar".into() }]);
    let out = updates(
        &mut state,
        vec![EventKind::AssistantMessage { text: "whole text".into(), tool_calls: Vec::new() }],
    );
    assert!(
        matches!(&out[0], SessionUpdate::AgentMessageChunk { content }
            if matches!(content, ContentBlock::Text { text } if text == "whole text")),
        "{out:?}"
    );
    assert_eq!(state.unsent_tail("whole text"), "", "accounted now");
}
```

And the in-crate streaming-turn test in `crates/forge-acp/src/server.rs`'s
test module (alongside `session_new_then_prompt_drives_a_run_and_ends_the_turn`,
:1090-1128 — which must keep passing **unedited**: `MockModel` has no
override, so its one whole-text chunk now comes from the message flush):

```rust
#[tokio::test]
async fn a_streaming_turn_sends_the_answer_as_live_chunks_before_responding() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let (tx, mut rx) = mpsc::channel(64);
    let factory = Arc::new(FnFactory(|root: &Path| {
        Ok(Arc::new(AgentService::new(
            Arc::new(
                forge_providers::ScriptedMockModel::from_json(r#"[{"text": "all done here"}]"#)
                    .expect("script"),
            ),
            Arc::new(forge_providers::MockRouter::selecting("scripted-mock")),
            Arc::new(forge_execution::MockExecution::new(root)),
            Arc::new(forge_skills::FsSkillRegistry::with_roots(vec![], None)),
            Arc::new(forge_session::JsonlSessionStore::new(
                root.join(".forge").join("sessions"),
            )),
            forge_config::Config::default(),
        )))
    }));
    let server = Arc::new(ForgeAcpServer::new(factory, tx));

    let created = server
        .new_session(json!({ "cwd": tmp.path(), "mcpServers": [] }))
        .await
        .expect("session created");
    let session_id = created["sessionId"].as_str().expect("session id").to_string();

    // `prompt` resolves with the turn's response; every chunk was already
    // in the channel — i.e. on the wire — before it (run_turn drains its
    // events before answering, server.rs:570-573).
    let response = prompt(
        &server,
        json!({ "sessionId": session_id, "prompt": [{ "type": "text", "text": "hi" }] }),
    )
    .await
    .expect("turn completed");
    assert_eq!(response["stopReason"], "end_turn", "{response}");

    let mut chunks = Vec::new();
    while let Ok(message) = rx.try_recv() {
        let value = serde_json::to_value(&message).expect("serialize");
        if value["params"]["update"]["sessionUpdate"] == "agent_message_chunk" {
            chunks.push(value["params"]["update"]["content"]["text"].as_str().expect("text").to_string());
        }
    }
    assert_eq!(chunks, ["all ", "done ", "here"], "streamed live, in order: {chunks:?}");
    assert_eq!(chunks.concat(), "all done here");
}
```

- [ ] **Step 2: Run** `cargo test -p forge-acp` → FAIL.
- [ ] **Step 3: Implement** per D7: the two `TurnState` fields (extend the
  struct doc at `dispatch.rs:217-228`); the two new arms replacing their
  entries in the ignore arm (`dispatch.rs:402-425` — rewrite the comment
  that says the text arrives "as one `agent_message_chunk`"); `unsent_tail`;
  `server.rs`'s end-of-turn tail with its rewritten comment. Update the
  `Lagged` arm's comment at `server.rs:551-556` to name the recovery: lost
  deltas are flushed by the message or the tail.
- [ ] **Step 4: Run** `cargo test -p forge-acp` → PASS.
- [ ] **Step 5:** full gate → PASS. **Commit** —
  `git commit -m "feat(acp): forward assistant deltas as live message chunks"`

**ACTION** `crates/forge-acp/src/dispatch.rs`,
`crates/forge-acp/src/dispatch/tests.rs`, `crates/forge-acp/src/server.rs`
**PATTERN** arm style per `dispatch.rs:290-300`; the ignore arm's "no honest
slot" discipline per `dispatch.rs:217-228`; the in-crate harness per
`server.rs:986-1003`
**GOTCHA** (a) `accounted` must be set on *every* message, including the
empty-text one — the fast path's `""` never equaling a non-empty outcome is
precisely what keeps the fast-path chunk flowing (D7). (b) The lazy reset
belongs to the delta arm (`if self.accounted.is_some() { … clear … }`), not
the message arm alone: a lagged message must leave `streamed` intact for
`unsent_tail`. (c) Keep the `!text.trim().is_empty()` guard around the tail
send — a whitespace-only outcome sends nothing, as today. (d) Do not touch
`prompt_text`, permissions, or the writer task; stdout purity is a pinned
invariant (`acp.rs:460-469`).
**VALIDATE** `cargo test -p forge-acp && cargo clippy --workspace --all-targets -- -D warnings`
**SATISFIES** AC2's in-crate half, AC3's ACP half, AC4's ACP half.

---

### Task 5: process proof, BDD, and honest docs

- [ ] **Step 1: The two expectation updates** (both are the scripted mock
  now streaming `"all done"` as `"all "` + `"done"`):
  - `crates/forge-cli/tests/acp.rs:443-450` — replace the single-chunk
    assertion with:

```rust
    // The model's answer arrives as live message chunks that concatenate
    // to it — the scripted mock streams since TICKET-1, and every chunk
    // was collected before the prompt response (see `request`).
    let chunks = client.updates_of("agent_message_chunk");
    let text: String = chunks
        .iter()
        .filter_map(|c| c["content"]["text"].as_str())
        .collect();
    assert_eq!(text, "all done", "chunks assemble to the answer: {chunks:?}");
    assert!(chunks.len() >= 2, "streamed live, not one lump: {chunks:?}");
```

  - `crates/forge-cli/tests/bdd/steps.rs:2087-2093`
    (`acp_client_saw_tool_call_and_message`) — same change: the
    `agent_message_chunk` assertion becomes `chunks.len() >= 2` and
    concatenation equals `"all done"`.
- [ ] **Step 2: The new piped chat process test** in
  `crates/forge-cli/tests/chat.rs` (after
  `a_piped_conversation_runs_two_turns_in_one_session`, :405-447):

```rust
/// TICKET-3's piped-mode rule (design D6): a streamed answer's fragments
/// are written plainly, and the captured transcript is byte-identical to a
/// non-streamed one — the answer assembled in place, exactly once.
#[test]
fn a_piped_chat_assembles_a_streamed_answer_once() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path(), "auto"); // script: [{"text": "the answer"}]
    let out = chat(tmp.path(), &project, &["what is it"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(
        stdout.matches("the answer").count(),
        1,
        "the streamed answer, once:\n{stdout}"
    );
    assert!(stdout.contains("  = "), "the footer still closes the turn:\n{stdout}");
    // The stream is on the log, not just the screen.
    let log = session_log(&project);
    assert!(
        log.contains("\"assistant_delta\""),
        "the turn recorded deltas:\n{log}"
    );
}
```

- [ ] **Step 3: The BDD scenario.** Append to
  `tests/features/streaming.feature`:

```gherkin
  Scenario: An ACP client sees the answer stream as it is written
    Given a project with a built graph
    And a scripted mock model that answers "the answer is ready"
    When an ACP client starts a session over stdio
    And the ACP client prompts "what is the answer"
    Then the ACP turn ends with stop reason "end_turn"
    And the ACP client saw the answer stream in more than one chunk
```

  and add the step to `crates/forge-cli/tests/bdd/steps.rs`, next to the
  other ACP steps (:2066-2110):

```rust
#[then("the ACP client saw the answer stream in more than one chunk")]
fn acp_client_saw_the_answer_stream(world: &mut BddWorld) {
    let chunks: Vec<&str> = world
        .acp_updates
        .iter()
        .filter(|u| u["sessionUpdate"] == "agent_message_chunk")
        .filter_map(|u| u["content"]["text"].as_str())
        .collect();
    assert!(
        chunks.len() >= 2,
        "the answer streamed live, not as one lump: {chunks:?}"
    );
    assert_eq!(chunks.concat(), "the answer is ready", "{chunks:?}");
}
```

  (The updates land in `world.acp_updates` from inside
  `BddWorld::acp_request`'s read loop, `world.rs:602-628` — i.e. before the
  prompt response by construction, which is what "stream, before turn end"
  means on the wire.) Adjust the feature file's header comment to say the
  stream is proven end to end: recorded in the log (TICKET-1's scenario)
  and forwarded live by a front end (this one).
- [ ] **Step 4: Docs that stop saying "no streaming" where that is now
  false:**
  - `docs/reference.md:1376-1379` (the ACP "Notes and current limits"
    bullet) — replace with: streaming *is* live when the provider streams;
    today the scripted test double is the one implementation and real
    OpenAI-compatible/Anthropic SSE is the recorded follow-up
    (TICKET-2); a non-streaming provider's answer still arrives as one
    `agent_message_chunk`; tool calls are live either way. Keep the
    "honest version, never chopped finished text" sentence's spirit — the
    never-faked rule still holds.
  - `docs/reference.md:707-710` — drop "no token-by-token streaming" from
    the parenthetical list of shared limitations.
  - `docs/reference.md:1844-1848` — drop "no token-by-token streaming (the
    loop returns a finished answer, exactly like `forge acp`)" from the
    interactive-chat bullet.
  - `ARCHITECTURE.md:348` — "the final text as one `agent_message_chunk`"
    becomes: the answer as live `agent_message_chunk`s when the provider
    streams (the final `assistant_message` flushes any gap), one chunk when
    it does not.
  - `ARCHITECTURE.md:455-461` — the "What's next" streaming clause:
    TICKET-3 has landed; remaining is provider SSE (TICKET-2).
  - `crates/forge-acp/src/lib.rs:97-100` — the "It does not stream tokens"
    bullet becomes "It streams when the provider streams": live chunks for
    a `stream_complete` provider (today the scripted test double; real SSE
    is the follow-up), one chunk otherwise — and never a faked stream of
    chopped-up finished text.
  - `docs/superpowers/specs/2026-09-24-interactive-chat-ui-design.md` —
    append a §19 amendment, in §16/§17/§18's style: §14 bullet 1 has
    shipped — the incremental assistant block is `Line::fragment` appended
    in inline scrollback (not a redrawn region, which the recorded
    architecture does not support), the closing `assistant_message` prints
    only the unshown suffix, ACP forwards live chunks, and §1's "no fake
    token streaming" stance is untouched (the stream is real; a
    non-streaming provider changes nothing). Add the `AssistantDelta` row
    to §4.2's mapping table ("appended to the in-flight assistant block;
    the closing `assistant_message` prints only the unshown suffix") and
    strike/annotate §14 bullet 1 as shipped, mirroring how §14's `/show`
    bullet was annotated by §18.
- [ ] **Step 5: Run** `cargo test -p forge-cli` → PASS (includes acp.rs and
  chat.rs), `cargo test -p forge-cli --test bdd` → PASS.
- [ ] **Step 6:** full gate, `just verify` included → PASS. **Commit** —
  `git commit -m "test+docs: streaming render proven at the binaries; streaming docs made true"`

**ACTION** `crates/forge-cli/tests/acp.rs`, `crates/forge-cli/tests/chat.rs`,
`crates/forge-cli/tests/bdd/steps.rs`, `tests/features/streaming.feature`,
`docs/reference.md`, `ARCHITECTURE.md`, `crates/forge-acp/src/lib.rs`,
`docs/superpowers/specs/2026-09-24-interactive-chat-ui-design.md`
**PATTERN** the piped harness per `chat.rs:122-161`; the BDD ACP steps per
`steps.rs:2037-2110`; the amendment style per the design doc's §16-§18
**GOTCHA** (a) No new pty tests in this ticket — the fragment path is
identical code in `write` and `notify`, and the known-red pty baseline
(`a_typed_ctrl_c_on_a_real_terminal_interrupts_without_killing_the_chat`,
`chat.rs:879`) stays out of scope; if a later ticket adds pty coverage it
must pin `TERM=xterm-256color` exactly as T4's test does (`chat.rs:968-975`).
(b) `scaffold(tmp, "auto")`'s one-entry script is re-parsed per run
(`chat.rs:166-183` explains the per-route model factory) — a single turn is
all this test gets, which is all it needs. (c) The reference.md edits are
deletions/rewordings of specific sentences — do not restructure either
section.
**VALIDATE** `cargo test -p forge-cli && cargo test -p forge-cli --test bdd && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --all --check`
**SATISFIES** AC2's process half, AC5's process half, AC6.

---

## TESTING STRATEGY

**Unit (pure, offline, deterministic — no clocks except the two bounded
deadline polls, no TTY, no process):**

- forge-chat `io.rs`: the fragment shape (style, no gutter, the flag).
- forge-chat `render.rs` (the load-bearing layer): open/extend/close;
  byte-equivalence of a streamed answer with the same answer printed whole
  (via a 6-line writer simulation); the suffix rule after partial deltas;
  the contract-violation fallback; error/cancel closing the block first;
  tool lines between two closed blocks; the trailing-newline close; empty
  deltas; backlog (resume/attach) re-rendering byte-identically; the flag
  rule (deltas never set it, the message does); `outcome_lines` in all
  three shapes (fast path whole, lagged tail, already-rendered silence).
  Every assertion is on `Line.text` — palette-immune (§4).
- forge-chat `app.rs` (driver, in-process over `ScriptedIo` + `FakeHost`):
  the arrival test (partial text visible, no footer yet, then exactly-once)
  and the mid-stream `/show` guard. The existing suite passes unedited and
  now exercises streamed deltas — `a_turn_renders_and_answers_once` is the
  byte-equivalence proof through the whole loop.
- forge-acp `dispatch/tests.rs`: the delta→chunk mapping, the per-message
  flush (full / suffix / whole / empty), `unsent_tail` in all four shapes
  (accounted, lagged message, non-streaming, fast path), the two-response
  run, the contract violation. Pure: no process, no runtime.
- forge-acp `server.rs`: one in-crate streaming turn over the scripted mock
  (chunks `["all ", "done ", "here"]`, in order, before the response — the
  harness reads the response only after `run_turn` returns, and `run_turn`
  drains its events first, so ordering is structural, not raced);
  `session_new_then_prompt_drives_a_run_and_ends_the_turn` passes unedited
  (the non-streaming pin).

**Process-level (`forge-cli/tests/`, hermetic like the rest of the suite):**

- `acp.rs`: the updated chunk assertion (concatenate + ≥2) — ACP streaming
  over the real binary's stdio.
- `chat.rs`: the new piped test — the real `PipedIo` fragment writer
  assembling the answer exactly once, with deltas on the session log. (The
  in-process tests use `ScriptedIo`; this is the only place the real
  writers' fragment branches run end to end.)
- Deliberately **no new pty test**: `TerminalIo`'s fragment branch is the
  same two lines as `PipedIo`'s behind the same flag, the palette never
  decorates `Plain`, and the pty suite's remaining gap (a typed Ctrl-C
  under a real terminal) has nothing to do with output. The known-red
  pty baseline is `chat.rs`'s
  `a_typed_ctrl_c_on_a_real_terminal_interrupts_without_killing_the_chat`
  under a `TERM=dumb` shell; T4's pty test pins `TERM=xterm-256color`
  (`chat.rs:968-975`) and any future pty test must do the same.

**BDD (`tests/features/streaming.feature`):** one new scenario — the ACP
client sees the answer stream in more than one chunk — beside TICKET-1's
log-level scenario, so the feature tells the whole story: the stream is
recorded, and a front end forwards it live. The existing `acp.feature`
scenario keeps passing through the updated `acp_client_saw_tool_call_and_message`
step. No chat-side scenario is added: the piped transcript is byte-identical
by design (D6), so there is nothing a Gherkin assertion could see that the
process test does not already pin — chat.feature's existing scenarios run
over streamed deltas now and pin that invariance.

**Deliberately not tested here:** real provider SSE (TICKET-2, with recorded
fixtures); redaction of streamed secrets (TICKET-1's runtime suite owns it;
the renderer only ever sees the redacted stored events); interrupt-mid-stream
beyond the render-level close test (a mid-trickle cancel needs the
cancel-marker polling `SlowModel` has and `TrickleModel` deliberately does
not — see Task 3's GOTCHA c).

## VALIDATION COMMANDS

Run in the worktree (`/Users/auser/work/rust/mine/forge/worktrees/t3-streaming-render`),
in this order, all green before each commit and at the end:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p forge-chat -p forge-acp -p forge-cli
cargo test --workspace
cargo test -p forge-cli --test bdd
```

(`just verify` is the umbrella — `check + lint + lint-ffi + test + bdd +
fmt --check`; run it at Task 5's end.)

Known baseline: the pty test
`crates/forge-cli/tests/chat.rs::a_typed_ctrl_c_on_a_real_terminal_interrupts_without_killing_the_chat`
is red under a `TERM=dumb` shell (the suite's own comment at `chat.rs:968-975`
records it; T4's pty test pins `TERM=xterm-256color` there). This ticket
adds no pty tests; treat that one test's pre-existing state as the baseline
and do not "fix" it here.

## ACCEPTANCE CRITERIA

- **AC1 — chat shows text arriving during a turn.** Over the scripted mock,
  with no TTY: a partial answer is visible in the transcript while the turn
  is still running (no footer yet), and the finished answer appears exactly
  once, as one blank-wrapped block byte-identical to a non-streamed
  rendering of the same text. Proven: Task 3's arrival test + Task 2's
  byte-equivalence test.
- **AC2 — ACP shows chunks before turn end.** A streaming turn's answer
  arrives as ≥2 `agent_message_chunk` notifications, in order,
  concatenating to the final text, all before the `session/prompt`
  response. Proven: the in-crate server test (Task 4), the updated
  `acp.rs` process assertion and the new BDD scenario (Task 5).
- **AC3 — non-streaming is byte-identical, and the fast path renders
  exactly as today.** A provider on TICKET-1's default fallback produces
  the same transcript bytes and the same one-chunk ACP turn as before;
  the fast path (empty-text message + tool call) still answers from the
  run outcome exactly once. Proven by the unedited pins:
  `render.rs:619-676`, `app.rs:1166-1194,1241-1251`,
  `server.rs:1090-1128`, plus `dispatch/tests.rs`'s new
  `the_fast_paths_outcome_is_the_whole_tail`.
- **AC4 — the answer-once rule holds under loss.** Deltas alone never set
  `rendered_assistant_text`; a lagged final message is completed with its
  unshown tail (`outcome_lines` / `unsent_tail`), never reprinted and never
  lost. Proven: Task 2's `the_outcome_fallback_completes_a_lagged_stream_with_its_tail`,
  Task 4's `the_tail_is_what_a_lost_message_never_sent`.
- **AC5 — piped mode is specified and proven.** The same fragments,
  appended plainly and flushed per write; a captured piped transcript is
  byte-identical to today's. Proven: Task 5's
  `a_piped_chat_assembles_a_streamed_answer_once` plus the unedited
  `a_piped_conversation_runs_two_turns_in_one_session` (which now runs over
  deltas).
- **AC6 — the docs tell the truth.** `docs/reference.md:1376` and its two
  sibling mentions, `ARCHITECTURE.md:348,455-461`, `forge-acp`'s crate doc,
  and the design doc all describe streaming-as-shipped (provider-gated,
  never faked), and the design doc records the §19 amendment. Proven: Task
  5, reviewed in the diff.

## OPEN QUESTIONS / ASSUMPTIONS

Settled in the Solution, stated here so nobody re-litigates them: the
fragment mechanism instead of redraw (D1/D6 — the recorded inline
architecture offers nothing else); deltas not arming the answer-once record
(D4 — design §5's lag promise decides it); per-delta chunks on the ACP
wire rather than line-coalesced (D7 — the runtime's whitespace hold-back
already coalesces, and ACP clients are built for high-frequency chunks);
backlog deltas rendered rather than skipped (D2 — §7's one-renderer rule,
and it is what makes a mid-stream `/attach` continue the open block).
Two genuine questions remain:

1. **The 200-line resume window can cut a streamed block mid-text.**
   `render_session_transcript` bounds re-rendered history at 200 *lines*
   (`app.rs:1048-1054`), and a fragment is a line — so the tail window of a
   very long session can begin with the middle of a streamed answer. *The
   whole answer still appears* (the fragments concatenate within the
   window); what is lost is the block's leading blank line and some leading
   words, behind the honest `... N earlier lines` note. *Recommended
   default: accept it* — the boundary is a line count by design, the note
   already says history was elided, and snapping to block boundaries would
   special-case the one renderer for a cosmetic edge. Revisit only if a
   real session shows it.
2. **Background-job notices can land mid-stream.** `on_bg` sends watcher
   notices straight to `io.notify` (`app.rs:437-447`), bypassing `emit`'s
   new close guard, so a `/bg` notice can interleave with a half-written
   streamed line on a TTY. *Recommended default: accept it* — it is exactly
   the documented may-land-mid-line artifact the chat already carries for
   notices (reference.md:686-698's known limitation, born of the rustyline
   `ExternalPrinter` wedge), not a new class; routing notices through the
   guard would couple the watcher to transcript state for a case the
   project already judged acceptable. TICKET-7's `ExternalPrinter` return
   is where mid-line interleave gets properly fixed, for streams and
   notices alike.

Assumption stated plainly: nothing outside the named files pins the
*number* of `agent_message_chunk`s or transcript lines for a scripted run
(checked: `cli.rs` counts session events, not chunks; `bdd/steps.rs`'s
other ACP steps assert presence, not counts; `mcp.rs` and the SSE handler
never see deltas as anything but generic events).

## NOTES

- **A pre-existing redaction gap surfaced while planning (not this
  ticket's to fix).** The redactor's `Bearer\s+\S+` pattern spans
  whitespace (`crates/forge-session/src/redact.rs:31-38`), while TICKET-1's
  hold-back splits deltas *at* whitespace (`service.rs:1214-1224`): a
  provider whose chunks split between `Bearer` and its token leaks the
  token in a delta (the final message still redacts correctly, and the
  renderer's prefix-mismatch fallback then shows the whole redacted text —
  duplication, honestly explained by the runtime's contract warning). Whole
  token shapes (`sk-…`, `ghp_…`) are safe by construction. The fix belongs
  to the runtime's carry rule (hold back from a pattern *start*, or carry
  the last whitespace-separated pair) and should land with TICKET-2's real
  SSE splitters, where arbitrary split points are the norm. File it as a
  follow-up against the interactive-chat-feel epic.
- **Broadcast capacity is 64** (`service.rs`'s channel): a burst of more
  than 64 deltas — a long answer from a synchronous-streaming provider —
  lags a current-thread consumer, and a lagged *middle* makes the streamed
  buffer a non-prefix, so the front ends show surviving fragments followed
  by the whole text (D3/D7's fallback). That is design §5's documented lag
  trade extended honestly: visible, bounded, never answer-losing. If real
  providers make it common, the knob is the channel capacity, not the
  renderer.
- **The ticket's estimate holds**: the diff lands at ~700–950 lines
  including tests; the two biggest single pieces are the render suite and
  the dispatch accounting.
- **What the writers inherit unchanged**: streamed fragments during a TTY
  turn go through the `StdoutPrinter` path like every other mid-turn line
  — they can interleave with a half-typed input line, the artifact the
  README already documents. Streaming makes the artifact *more frequent*
  (a turn's text now prints during the prompt instead of after it) but not
  *new*; TICKET-7's `ExternalPrinter` restoration lifts it for everything
  at once.
- **Why `TrickleModel` collects then replays**: the scripted mock's
  `stream_complete` is synchronous, so the staggering a test observes must
  be inserted between its chunks — collecting first keeps one recorded
  request and one queue pop (`scripted.rs`'s own invariant), and the delay
  is in the replay, where the test can see it.
- TICKET-1's open question 1 ("should `MockModel` stream too?") stays
  answered no — and this ticket relies on it: `forge-acp`'s
  `session_new_then_prompt_drives_a_run_and_ends_the_turn` is the
  non-streaming ACP pin precisely because `MockModel` takes the fallback.

## AMENDMENTS

(none yet)
