# TICKET-1 — Token Streaming: Core Plumbing — Implementation Plan

> **For agentic workers:** implement this plan task-by-task (superpowers:subagent-driven-development or superpowers:executing-plans). Steps use checkbox (`- [ ]`) syntax. Work in the clean worktree `/Users/auser/work/rust/mine/forge/worktrees/t1-streaming-core` (HEAD 5c72985); every `file:line` citation below is against that tree.

## Feature Description

`ModelProvider` gains a streaming completion method with a default
whole-response fallback over `complete()`; `EventKind` gains an additive
`assistant_delta` variant; the runtime emits ordered text deltas on the run
channel and persists them through the one redaction boundary; the scripted
mock streams deterministically so the whole path is provable offline. Nothing
user-facing changes yet: no front end renders deltas in this ticket.

**Goal:** a run over a streaming-capable model emits ordered
`assistant_delta` events and then *exactly* the terminal events it emits
today; a non-streaming provider is byte-identical to before; replay and
`forge resume` keep reconstructing from the final `AssistantMessage` alone.

**Architecture:** one new trait method (default impl = today's behavior), one
new event kind (schema v4, additive per the events.rs:8-22 convention), one
new call helper in `AgentService` used at both `complete()` call sites, one
mock override. The replay/persistence story is untouched by construction:
`emit_assistant_message` (`crates/forge-runtime/src/service.rs:986-1009`)
keeps recording the assembled response verbatim, and deltas are marked
rendering-only in replay's ignore arm (`crates/forge-runtime/src/replay.rs:195-206`).

**Spec:** `specs/tickets/interactive-chat-feel.md` (TICKET-1 section);
`docs/superpowers/specs/2026-09-24-interactive-chat-ui-design.md` §14 bullet 1
("the transcript gains an incremental assistant block and nothing else
changes — the renderer is already per-event" — the *transcript* half is
TICKET-3; this ticket is the "nothing else changes" half).

## User Story

As a forge front end (chat, ACP, SSE), I want the runtime to publish a
streaming model's text as it arrives — ordered, redacted, and recorded — so
that a later ticket can render it live, without any change to what replay,
resume, or a non-streaming provider do today.

## Problem

The agent loop calls `ModelProvider::complete` and waits for the whole answer
(`crates/forge-runtime/src/service.rs:1792` and `:1838`). The
`streaming: bool` capability bit exists but is wired to nothing
(`crates/forge-core/src/model.rs:11`, `crates/forge-core/src/router.rs:10`;
both mocks already advertise `streaming: true` —
`crates/forge-providers/src/model.rs:45`,
`crates/forge-providers/src/scripted.rs:70`). The design doc records "No fake
token streaming" as a deliberate stance and real streaming as the follow-up
(§1 decisions, §14 bullet 1; user-facing: `docs/reference.md:1338-1341`). The
event schema has an additive convention made for exactly this kind of growth
(`crates/forge-core/src/events.rs:8-22`), and the store already redacts and
returns whatever it writes (`crates/forge-session/src/store.rs:266-300`), so
the missing pieces are purely: a way to ask a provider for fragments, an
event kind to carry them, and a runtime path that emits them without
disturbing the replay record.

## Solution

Six design calls, each argued from the code:

**D1 — Method shape.** `stream_complete(&self, request, on_delta) -> Result<CompletionResponse, ForgeError>`
on `ModelProvider`, where `on_delta: &mut (dyn FnMut(&str) + Send)` receives
text fragments and the returned response is the assembled completion —
exactly what `complete()` would have returned, tool calls included. A
callback, not a `BoxStream`: `ModelProvider` is an `async_trait` object used
as `Arc<dyn ModelProvider>` (`crates/forge-core/src/model.rs:158-165`,
`crates/forge-runtime/src/service.rs:39-40`), a `&mut dyn FnMut` parameter
keeps it object-safe with no new dependency (no `futures` in forge-core), and
TICKET-2's SSE decoders map one-to-one onto "call the closure per decoded
fragment". Tool-call fragments are *not* streamed: reassembly from deltas is
a provider-internal concern (TICKET-2), and the calls surface whole in the
final response, as today.

**D2 — Default impl = graceful fallback.** The default body calls
`complete()` and emits no deltas at all. Every existing provider and mock —
including ones that advertise `streaming: true` without overriding
(`MockModel`, and forge-chat's `SlowModel` wrapper at
`crates/forge-chat/src/testing.rs:478-511`) — compiles unchanged and behaves
byte-identically to today. "Advertises streaming but doesn't implement it"
degrading silently to a whole response is the exact fallback TICKET-2 needs
for real servers; the default method is where it lives.

**D3 — One additive event kind, schema v4.** `EventKind::AssistantDelta { text: String }`,
serde tag `assistant_delta` (the enum is `#[serde(tag = "type", rename_all = "snake_case")]`,
`crates/forge-core/src/events.rs:95-97`). `EVENT_SCHEMA_VERSION` bumps 3 → 4
with the doc comment extended — the v2→v3 precedent bumped the constant when
kinds were added, and the convention text already says a newer log "simply
carries event kinds an older reader does not know" (events.rs:16-21). Text
only: no turn number, no index (ordering comes from the store-assigned
`seq`; see Open Questions Q2).

**D4 — The final `AssistantMessage` is retained as the only replay source.**
`emit_assistant_message` (`service.rs:986-1009`) is called with the assembled
response exactly as today, at exactly the same points; `replay.rs` adds the
new kind to its observability-only ignore arm so deltas never enter a
replayed conversation. Deltas are rendering-only: broadcast live, persisted
for inspection, never needed to reconstruct anything.

**D5 — Deltas cross the one redaction boundary like every event.** Emission
goes through `AgentService::emit` (`service.rs:965-976`): `sessions.append`
redacts and returns the stored form (store.rs:281-300), the redacted event is
what gets broadcast and collected — the invariant ARCHITECTURE.md:414-418
states ("One redaction, at one boundary, for every consumer"). A delta that
fails to persist is logged at `warn` and dropped, never a run failure: the
final `AssistantMessage` carries the same text through the same boundary a
beat later, so a delta is only ever a rendering gap. (The `FnMut(&str)`
callback cannot propagate `emit`'s `Result`; this is the deliberate answer.)

**D6 — Hold-back carry before emit, so the boundary can actually see
secrets.** The redactor matches whole patterns per payload
(`crates/forge-session/src/redact.rs:51-75`); a provider whose chunks split
`sk-abcdef123456` across two deltas would defeat per-payload redaction. So
the runtime emits only the prefix of its buffer that ends at a whitespace
boundary and carries the trailing non-whitespace run until more text arrives
or the response completes, flushing the carry before `emit_assistant_message`.
Concatenated, the emitted deltas still equal the response content exactly;
the cost is a one-word rendering lag, imperceptible for the feel this epic
buys. This lives in the runtime helper, not the mock, so TICKET-2's real SSE
providers inherit it.

**Runtime gating (D7).** The helper streams only when
`model.capabilities().streaming` is true — capabilities are "declared, never
assumed" (ARCHITECTURE.md:171) — and D2 covers advertisers that don't
override.

**Mock (D8).** `ScriptedMockModel` overrides `stream_complete` and splits its
scripted reply into deterministic chunks: each chunk is a maximal run of
non-whitespace plus its trailing whitespace, so concatenation reproduces the
reply exactly and a whitespace-free token is never split. No clocks, no
randomness — offline tests assert exact delta sequences. `MockModel` stays on
the default fallback: it is the zero-setup first impression
(`crates/forge-providers/src/model.rs:21-27`), and `forge-cli`'s
process-level suite pins its run at exactly "5 events"
(`crates/forge-cli/tests/cli.rs:466-468`) — leaving it unchanged both
preserves that pin and keeps an in-tree proof of D2's fallback.

## Out of Scope

- **Real provider streaming** (OpenAI-compatible and Anthropic SSE decode,
  tool-call reassembly from deltas, mid-stream errors, `EgressPolicy`
  redirect re-checks, `Capability::Streaming` as a router filter): TICKET-2,
  `crates/forge-providers` only.
- **Rendering deltas anywhere**: forge-chat's incremental block, ACP live
  `agent_message_chunk`s, piped-mode behavior: TICKET-3. In this ticket
  `forge-chat` and `forge-acp` gain *silence* arms so they compile; the
  transcript is byte-identical.
- No new config keys, no feature flag: streaming is on whenever the selected
  provider's declared capability says so.
- No changes to `subscribe`, `attach`, `fork_session`, `RunState`, approval
  flow, or the needle fast path (which never calls the model — no model call,
  no deltas).

## Metadata

- Date: 2026-10-01. Base: worktree `t1-streaming-core`, HEAD 5c72985.
- Ticket: `specs/tickets/interactive-chat-feel.md` TICKET-1. Depends on:
  nothing. Blocks: TICKET-2, TICKET-3 (planned only after this lands).
- Estimate: ~800–1200 lines including tests (per the ticket). Crates touched:
  forge-core, forge-runtime, forge-providers; forced one-arm compile fixes in
  forge-chat, forge-acp, forge-cli; one new BDD feature file.

## CONTEXT REFERENCES

### Files to read first (with why)

| file:line | why |
| --- | --- |
| `crates/forge-core/src/model.rs:153-165` | the `ModelProvider` trait — `name`, `capabilities`, one `complete()`; D1's method goes here |
| `crates/forge-core/src/model.rs:7-28` | `ModelCapabilities.streaming` exists, defaults false, unwired |
| `crates/forge-core/src/events.rs:8-22` | the additive-schema convention and version doc comment — extend for v4 |
| `crates/forge-core/src/events.rs:95-197` | `EventKind`: serde tag `type`, snake_case; the v3 replay-kinds comment block at :166-173 explains the observability/replay split the delta kind joins (observability side) |
| `crates/forge-core/src/router.rs:9-27` | `Capability::Streaming` + `satisfied_by` — **untouched** here; becomes a real filter in TICKET-2 |
| `crates/forge-runtime/src/service.rs:965-976` | `emit()`: append → broadcast the *redacted stored* event → collect. The one boundary; deltas flow through it |
| `crates/forge-runtime/src/service.rs:986-1009` | `emit_assistant_message` — the replay record; called unchanged with the assembled response |
| `crates/forge-runtime/src/service.rs:1788-1818` | single-turn path (`tools.is_empty()`): first `complete()` call site |
| `crates/forge-runtime/src/service.rs:1836-1843` | agent loop: second `complete()` call site |
| `crates/forge-runtime/src/replay.rs:145-211` | `run_messages` — exhaustive match, ignore arm at :195-206 gains the new kind; module table at :9-24 states what is replayed |
| `crates/forge-session/src/store.rs:266-300` | `append` redacts via `redact_value` and returns the redacted event — generic over the new kind, **no change needed** |
| `crates/forge-providers/src/scripted.rs:62-107` | `ScriptedMockModel`'s `ModelProvider` impl — the override site |
| `crates/forge-providers/src/model.rs:28-127` | `MockModel` — deliberately **not** overridden (D8) |

### Forced compile-fix sites (exhaustive matches, no wildcard)

| file:line | change |
| --- | --- |
| `crates/forge-runtime/src/replay.rs:195-206` | add `EventKind::AssistantDelta { .. }` to the ignore arm |
| `crates/forge-chat/src/render.rs:66-70` | add a silent arm (renders `Vec::new()`), comment naming TICKET-3 |
| `crates/forge-acp/src/dispatch.rs:412-422` | add to the ignore list, comment naming TICKET-3 |
| `crates/forge-cli/src/commands/session_cmd.rs:17-79` | `describe` arm: `assistant_delta text={head}` with `head` capped at 40 chars, matching the `assistant_message` style at :49-60 |

Verified non-issues (checked, no change needed): `forge-server` SSE
serializes events generically (`crates/forge-server/src/handlers.rs:304-331`)
and its status mapping has a wildcard (handlers.rs:177); `forge-mcp` has only
targeted matches (`crates/forge-mcp/src/tools.rs:501,707,818,832`);
`RunState::of_events` reads only the last kind (`crates/forge-core/src/run.rs:83-92`),
and a delta is never terminal (`is_terminal`, events.rs:201-206);
`service/tests.rs`'s `event_kinds` has a `_ => "other"` wildcard (:97) — it
compiles, but Task 2 gives it an explicit arm so the new tests can assert
deltas distinctly.

### Tests that pin today's exact shape (know them before changing anything)

- `crates/forge-runtime/src/service/tests.rs:102-133`
  (`full_run_emits_ordered_events`, **MockModel**) — 5 events, seqs 1..=5.
  Stays **unchanged**: it becomes the AC2 byte-identical proof.
- `crates/forge-runtime/src/service/tests.rs:231-278`
  (`scripted_two_turn_run_writes_file_and_emits_full_trail`) — exact 12-event
  sequence; gains 2 deltas (Task 4 updates it).
- `crates/forge-runtime/src/service/tests.rs:2031-2056`
  (`needle_fast_path_absent_engine_changes_nothing`) — exact 5-event
  sequence; gains 2 deltas (Task 4 updates it).
- `crates/forge-cli/tests/cli.rs:466-468` — `session list` shows "5 events"
  for a `mock-local` run; the reason `MockModel` must not stream (D8).
- `crates/forge-core/src/events.rs:330` — hardcodes `assert_eq!(value["v"], 3)`;
  becomes 4 (or better, `EVENT_SCHEMA_VERSION`).

### New files

- `tests/features/streaming.feature` — one BDD scenario (Task 5).

No new source files; everything lands in the modules named above.

### Patterns to follow

The one boundary — `crates/forge-runtime/src/service.rs:965-976`:

```rust
fn emit(
    &self,
    sender: &broadcast::Sender<Event>,
    collected: &mut Vec<Event>,
    event: Event,
) -> Result<(), ForgeError> {
    let stored = self.sessions.append(event)?;
    // No subscribers yet is normal for the CLI; not an error.
    let _ = sender.send(stored.clone());
    collected.push(stored);
    Ok(())
}
```

The replay record, retained unchanged — `service.rs:986-1009`:

```rust
fn emit_assistant_message(
    &self,
    sender: &broadcast::Sender<Event>,
    collected: &mut Vec<Event>,
    run_id: &str,
    session_id: &str,
    response: &forge_core::CompletionResponse,
) -> Result<(), ForgeError> {
    if response.content.is_empty() && response.tool_calls.is_empty() {
        return Ok(());
    }
    self.emit(
        sender,
        collected,
        Event::new(
            run_id,
            session_id,
            EventKind::AssistantMessage {
                text: response.content.clone(),
                tool_calls: response.tool_calls.clone(),
            },
        ),
    )
}
```

The scripted mock's `complete` (pop once, record once — the override shares
this; do not double-record) — `crates/forge-providers/src/scripted.rs:78-106`.

## IMPLEMENTATION PLAN (phases)

- **Phase 1 — the seam (Task 1).** The trait method with its fallback exists;
  nothing calls it; the workspace compiles and every test passes unchanged.
- **Phase 2 — the event kind (Task 2).** `AssistantDelta` + schema v4 + the
  four forced match arms, each landing with a test pinning "deltas are
  silent/ignored here". Workspace green.
- **Phase 3 — a streaming mock (Task 3).** `ScriptedMockModel` streams
  deterministically; inert until Phase 4 because the runtime still calls
  `complete()`. Workspace green.
- **Phase 4 — the runtime wires it (Task 4).** One helper, two call sites,
  the hold-back carry; the two pinned scripted-mock sequences gain their
  deltas; the new behavior tests land. Workspace green.
- **Phase 5 — proof from the binary (Task 5).** One BDD scenario through the
  compiled `forge`; docs that describe today's schema updated.

## STEP-BY-STEP TASKS

### Task 1: `ModelProvider::stream_complete` with a no-op fallback

- [ ] **Step 1: Write the failing tests** in `crates/forge-core/src/model.rs`'s
  test module (create one if absent — the file currently has none, so a small
  `#[cfg(test)] mod tests`):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// A provider that implements only `complete` — every pre-streaming
    /// provider is this. The default `stream_complete` must answer with the
    /// whole response and call the delta callback *never*.
    struct WholeOnly;

    #[async_trait]
    impl ModelProvider for WholeOnly {
        fn name(&self) -> &str { "whole-only" }
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities { streaming: true, ..ModelCapabilities::default() }
        }
        async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, ForgeError> {
            Ok(CompletionResponse {
                model: "whole-only".into(),
                content: format!("answer to {}", request.messages.len()),
                tool_calls: Vec::new(),
                finish_reason: Some("stop".into()),
                usage: None,
            })
        }
    }

    #[tokio::test]
    async fn the_default_stream_is_a_silent_whole_response_fallback() {
        let provider = WholeOnly;
        let mut deltas: Vec<String> = Vec::new();
        let response = provider
            .stream_complete(
                CompletionRequest::new("whole-only", vec![Message::user("hi")]),
                &mut |d: &str| deltas.push(d.to_string()),
            )
            .await
            .expect("stream falls back to complete");
        assert_eq!(response.content, "answer to 1");
        assert!(deltas.is_empty(), "the fallback emits no deltas: {deltas:?}");
    }

    /// An overriding provider receives the fragments it hands out.
    #[tokio::test]
    async fn an_override_delivers_fragments_then_the_assembled_response() {
        struct Chunky;
        #[async_trait]
        impl ModelProvider for Chunky {
            fn name(&self) -> &str { "chunky" }
            fn capabilities(&self) -> ModelCapabilities { ModelCapabilities::default() }
            async fn complete(&self, _: CompletionRequest) -> Result<CompletionResponse, ForgeError> {
                unreachable!("streaming providers are called through stream_complete")
            }
            async fn stream_complete(
                &self,
                _: CompletionRequest,
                on_delta: &mut (dyn FnMut(&str) + Send),
            ) -> Result<CompletionResponse, ForgeError> {
                on_delta("he");
                on_delta("llo");
                Ok(CompletionResponse {
                    model: "chunky".into(),
                    content: "hello".into(),
                    tool_calls: Vec::new(),
                    finish_reason: None,
                    usage: None,
                })
            }
        }
        let mut got = String::new();
        let response = Chunky
            .stream_complete(
                CompletionRequest::new("chunky", vec![Message::user("hi")]),
                &mut |d: &str| got.push_str(d),
            )
            .await
            .expect("streamed");
        assert_eq!(got, "hello");
        assert_eq!(response.content, "hello", "fragments concatenate to the response");
    }
}
```

- [ ] **Step 2: Run** `cargo test -p forge-core` → FAIL (no such method).
- [ ] **Step 3: Implement** in `crates/forge-core/src/model.rs`, after
  `complete` (model.rs:164). The doc comment carries the contract:

```rust
    /// Complete one request, invoking `on_delta` with each text fragment as
    /// it becomes available, then returning the assembled response — exactly
    /// what `complete` would have returned, tool calls included.
    ///
    /// Contract: the fragments passed to `on_delta`, concatenated, equal the
    /// returned response's `content`. A provider that cannot honor that must
    /// not call `on_delta` at all. Tool-call deltas are reassembled inside
    /// the provider and surface whole in the response; only text streams.
    ///
    /// The default is the graceful fallback: answer with `complete` and emit
    /// no deltas, so every provider written before streaming — including one
    /// advertising `streaming: true` — behaves byte-identically to before.
    async fn stream_complete(
        &self,
        request: CompletionRequest,
        on_delta: &mut (dyn FnMut(&str) + Send),
    ) -> Result<CompletionResponse, ForgeError> {
        let _ = on_delta;
        self.complete(request).await
    }
```

- [ ] **Step 4: Run** `cargo test -p forge-core` → PASS.
- [ ] **Step 5:** the full gate (VALIDATION COMMANDS below) → PASS. **Commit** —
  `git commit -m "feat(core): ModelProvider::stream_complete with a whole-response fallback"`

**ACTION** `crates/forge-core/src/model.rs`
**PATTERN** `crates/forge-core/src/model.rs:153-165` (existing trait + capability-contract doc style)
**GOTCHA** the `+ Send` on `dyn FnMut(&str)` is required: `#[async_trait]` boxes a
`Send` future, and the callback crosses the `.await`. Keep the parameter
name `on_delta` with `let _ = on_delta;` (not `_on_delta`) so the trait
*declaration's* docs read cleanly; clippy is satisfied by the `let _`.
**VALIDATE** `cargo test -p forge-core && cargo clippy --workspace --all-targets -- -D warnings`
**SATISFIES** AC5 (every existing provider compiles unchanged — proven by the
workspace build with zero provider edits), half of AC2 (the fallback emits no
deltas).

---

### Task 2: `EventKind::AssistantDelta`, schema v4, and the four forced arms

- [ ] **Step 1: Write the failing tests.** In
  `crates/forge-core/src/events.rs`'s test module:

```rust
    #[test]
    fn assistant_delta_serializes_snake_case_and_roundtrips() {
        let event = Event::new(
            "r",
            "s",
            EventKind::AssistantDelta { text: "hel".into() },
        );
        let value = serde_json::to_value(&event).expect("serialize");
        assert_eq!(value["type"], "assistant_delta");
        assert_eq!(value["v"], EVENT_SCHEMA_VERSION);
        let line = serde_json::to_string(&event).expect("ser");
        let back: Event = serde_json::from_str(&line).expect("de");
        assert!(
            matches!(back.kind, EventKind::AssistantDelta { ref text } if text == "hel")
        );
    }

    /// The additive convention: a v3 log line (which knows no deltas) still
    /// parses under the new binary — deltas only ever *add* lines.
    #[test]
    fn a_v3_log_line_still_parses() {
        let line = "{\"v\":3,\"seq\":4,\"ts\":\"2026-09-24T12:00:00Z\",\"run_id\":\"r\",\"session_id\":\"s\",\"type\":\"assistant_message\",\"text\":\"done\"}";
        let event: Event = serde_json::from_str(line).expect("v3 line parses");
        assert_eq!(event.v, 3);
        assert!(matches!(event.kind, EventKind::AssistantMessage { .. }));
    }
```

In `crates/forge-runtime/src/replay.rs`'s tests
(`crates/forge-runtime/src/replay/tests.rs`):

```rust
#[test]
fn deltas_are_never_replayed() {
    // A streamed run's log interleaves deltas with the replay record; the
    // reconstructed conversation must be identical to the delta-free log.
    let with_deltas = vec![
        event(EventKind::RunStarted { provider: "p".into(), model: "m".into(), prompt: "hi".into() }),
        event(EventKind::AssistantDelta { text: "the ".into() }),
        event(EventKind::AssistantDelta { text: "answer".into() }),
        event(EventKind::AssistantMessage { text: "the answer".into(), tool_calls: vec![] }),
        event(EventKind::Completed { summary: "the answer".into() }),
    ];
    let without: Vec<_> = with_deltas
        .iter()
        .filter(|e| !matches!(e.kind, EventKind::AssistantDelta { .. }))
        .cloned()
        .collect();
    assert_eq!(
        conversation_from_events(&with_deltas),
        conversation_from_events(&without),
        "deltas are rendering-only; replay reads the final assistant_message"
    );
}
```

(Match the file's existing helpers — `replay/tests.rs` has its own event
constructors; reuse them.)

In `crates/forge-chat/src/render.rs`'s tests, beside the
"renders nothing" table:

```rust
#[test]
fn a_delta_renders_nothing_until_ticket_3() {
    let mut s = TranscriptState::new();
    let out = s.on_event(&ev(EventKind::AssistantDelta { text: "hel".into() }));
    assert!(out.is_empty(), "TICKET-3 grows the incremental block; the core plumbing stays silent");
    assert!(!s.rendered_assistant_text(), "a delta is not the answer-once record");
}
```

- [ ] **Step 2: Run** `cargo test -p forge-core -p forge-runtime -p forge-chat` → FAIL.
- [ ] **Step 3: Implement the variant.** In `crates/forge-core/src/events.rs`:

  - `EVENT_SCHEMA_VERSION` becomes `4`; extend the doc comment
    (events.rs:8-22) with: "* v4 adds `assistant_delta` — ordered text
    fragments of a streaming model's in-flight answer. Rendering-only: replay
    ignores them and reads the final `assistant_message`, so a v4 log replays
    exactly as its delta-free equivalent. Purely additive, like v3."
  - Add the variant, placed right after `AssistantMessage` with a comment
    tying it to the v3 stream-split comment at :166-173:

```rust
    /// One text fragment of a streaming model's in-flight answer (v4).
    /// Ordered by `seq`, always followed by the response's verbatim
    /// `AssistantMessage`. Rendering-only — a third, optional strand beside
    /// the observability and replay streams above: replay ignores it, and a
    /// non-streaming run records none.
    AssistantDelta {
        text: String,
    },
```

- [ ] **Step 4: The forced arms** (compile errors point at each; fix exactly
  these four, nothing else):
  1. `crates/forge-runtime/src/replay.rs:195-206` — add
     `| EventKind::AssistantDelta { .. }` to the ignore arm.
  2. `crates/forge-chat/src/render.rs:70` — new arm
     `EventKind::AssistantDelta { .. } => Vec::new(),` with the comment
     `// TICKET-3 renders these as one growing block; silent here.`
  3. `crates/forge-acp/src/dispatch.rs:412-422` — add
     `EventKind::AssistantDelta { .. }` to the ignored list with
     `// TICKET-3 forwards these as live agent_message_chunks.`
  4. `crates/forge-cli/src/commands/session_cmd.rs:78` — new arm before
     `Completed`: `EventKind::AssistantDelta { text } => { let head: String = text.chars().take(40).collect(); format!("assistant_delta text={head}") }`.
- [ ] **Step 5:** In `crates/forge-core/src/events.rs:330`, the hardcoded
  `assert_eq!(value["v"], 3)` becomes `assert_eq!(value["v"], EVENT_SCHEMA_VERSION)`
  (the test's intent is "written at the current schema", not "v3 forever").
  In `crates/forge-runtime/src/service/tests.rs:79-97`, give `event_kinds`
  an explicit `EventKind::AssistantDelta { .. } => "assistant_delta"` arm
  above the `_ => "other"` wildcard.
- [ ] **Step 6: Run** `cargo test --workspace` → PASS.
- [ ] **Step 7:** full gate → PASS. **Commit** —
  `git commit -m "feat(core): additive assistant_delta event kind (schema v4)"`

**ACTION** `crates/forge-core/src/events.rs`; arms in the four files above
**PATTERN** variant style per `events.rs:174-196`; the additive convention per
`events.rs:8-22`; "deliberately silent" arm style per `crates/forge-chat/src/render.rs:151-159`
**GOTCHA** `is_terminal` (events.rs:201-206) needs no arm — its `matches!` is
exhaustive-by-construction over terminal kinds only; do not add one.
`forge-server` and `forge-mcp` need no edits (wildcards/generic
serialization) — verify with the workspace build rather than touching them.
**VALIDATE** `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings`
**SATISFIES** AC3's compile half (replay ignores deltas), and keeps every
adapter's output byte-identical (AC2 for the render layer).

---

### Task 3: `ScriptedMockModel` streams deterministically

- [ ] **Step 1: Write the failing tests** in
  `crates/forge-providers/src/scripted.rs`'s test module:

```rust
#[tokio::test]
async fn streaming_splits_a_text_reply_into_word_chunks() {
    let model = ScriptedMockModel::new(vec![ScriptedReply {
        text: Some("the answer is ready".to_string()),
        tool_calls: Vec::new(),
    }]);
    let mut deltas: Vec<String> = Vec::new();
    let response = model
        .stream_complete(
            CompletionRequest::new("scripted-mock", vec![Message::user("x")]),
            &mut |d: &str| deltas.push(d.to_string()),
        )
        .await
        .expect("streamed");
    // Each chunk is a word plus its trailing whitespace; concatenation is exact.
    assert_eq!(deltas, vec!["the ", "answer ", "is ", "ready"]);
    assert_eq!(deltas.concat(), response.content);
    assert_eq!(response.content, "the answer is ready");
}

#[tokio::test]
async fn streaming_a_tool_call_reply_emits_no_deltas() {
    let model = ScriptedMockModel::new(vec![ScriptedReply {
        text: None,
        tool_calls: vec![ToolCall::new("call_1", "read_file", serde_json::json!({"path": "a.rs"}))],
    }]);
    let mut deltas = 0;
    let response = model
        .stream_complete(
            CompletionRequest::new("scripted-mock", vec![Message::user("x")]),
            &mut |_: &str| deltas += 1,
        )
        .await
        .expect("streamed");
    assert_eq!(deltas, 0, "tool calls surface whole, in the response");
    assert_eq!(response.tool_calls.len(), 1);
}

#[tokio::test]
async fn streaming_pops_the_queue_once_and_records_one_request() {
    let model = ScriptedMockModel::from_json(r#"[{"text": "one here"}, {"text": "two"}]"#)
        .expect("parse");
    let mut sink = |_: &str| {};
    let first = model
        .stream_complete(CompletionRequest::new("scripted-mock", vec![Message::user("a")]), &mut sink)
        .await
        .expect("first");
    assert_eq!(first.content, "one here");
    let second = model
        .stream_complete(CompletionRequest::new("scripted-mock", vec![Message::user("b")]), &mut sink)
        .await
        .expect("second");
    assert_eq!(second.content, "two");
    assert_eq!(model.recorded().len(), 2, "exactly one recorded request per call");
}

/// A whitespace-free token is never split — a key-shaped fragment must
/// reach the redaction boundary whole (the runtime additionally holds back
/// partial trailing tokens; see Task 4).
#[tokio::test]
async fn a_token_without_whitespace_is_one_chunk() {
    let model = ScriptedMockModel::new(vec![ScriptedReply {
        text: Some("key sk-abcdef123456 end".to_string()),
        tool_calls: Vec::new(),
    }]);
    let mut deltas: Vec<String> = Vec::new();
    let _ = model
        .stream_complete(
            CompletionRequest::new("scripted-mock", vec![Message::user("x")]),
            &mut |d: &str| deltas.push(d.to_string()),
        )
        .await
        .expect("streamed");
    assert!(deltas.iter().any(|d| d == "sk-abcdef123456 "), "{deltas:?}");
}
```

- [ ] **Step 2: Run** `cargo test -p forge-providers scripted` → FAIL.
- [ ] **Step 3: Implement** in `crates/forge-providers/src/scripted.rs`:
  - Factor the pop/record/assemble body of `complete` into
    `fn next_reply(&self, request: CompletionRequest) -> CompletionResponse`
    (recording + queue pop + the "script exhausted" default +
    `CompletionResponse` assembly), so `complete` and `stream_complete` are
    two one-liners over it and a request is never recorded twice.
  - Add `fn word_chunks(text: &str) -> Vec<&str>` (crate-private):
    walk `char_indices`, cutting after each maximal `non-whitespace+
    whitespace*` run. Doc comment: "Each chunk is a word plus its trailing
    whitespace, so concatenation reproduces the input exactly and a
    whitespace-free token is never split."
  - `stream_complete`: `let response = self.next_reply(request); for chunk in word_chunks(&response.content) { on_delta(chunk); } Ok(response)`.
    No sleeps, no timers: determinism is the whole point, and forge-chat's
    `SlowModel` tests own the slow-turn timing
    (`crates/forge-chat/src/testing.rs:478-511`).
- [ ] **Step 4: Run** `cargo test -p forge-providers` → PASS.
- [ ] **Step 5:** full gate → PASS (the runtime doesn't call the method yet,
  so nothing else moves). **Commit** —
  `git commit -m "feat(providers): scripted mock streams deterministic word chunks"`

**ACTION** `crates/forge-providers/src/scripted.rs`
**PATTERN** `crates/forge-providers/src/scripted.rs:78-106` (existing `complete`)
**GOTCHA** do **not** override `MockModel` (`crates/forge-providers/src/model.rs:28-127`):
it must stay on the Task 1 fallback — `crates/forge-cli/tests/cli.rs:466-468`
pins a mock-local run at exactly "5 events", and it doubles as the living
proof that advertising `streaming: true` without an override is a safe no-op.
Also: `scripted-mock`'s capability already says `streaming: true`
(scripted.rs:70); from Task 4 on, every scripted run emits deltas — that is
AC1 working as intended, and the two tests it invalidates are updated there,
not here.
**VALIDATE** `cargo test -p forge-providers && cargo clippy --workspace --all-targets -- -D warnings`
**SATISFIES** AC6.

---

### Task 4: the runtime emits deltas — one helper, two call sites

- [ ] **Step 1: Write the failing tests** in
  `crates/forge-runtime/src/service/tests.rs`. First the streaming run:

```rust
#[tokio::test]
async fn a_streaming_run_emits_ordered_deltas_then_the_same_terminal_events() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let service = scripted_service(
        tmp.path(),
        vec![text_reply("the answer")],
        forge_core::ApprovalPolicy::Deny,
    );
    let outcome = service.run("question").await.expect("run");

    let kinds = event_kinds(&outcome);
    assert_eq!(
        kinds,
        [
            "run_started",
            "routing_decision_made",
            "assistant_delta",
            "assistant_delta",
            // the replay record, retained and unchanged
            "assistant_message",
            "turn_completed",
            "completed"
        ],
        "deltas arrive in order, then exactly today's terminal events"
    );

    // The deltas concatenate to the final message, and the message is the
    // same one a delta-free log would carry.
    let deltas: String = outcome
        .events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::AssistantDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, "the answer");
    let final_text = outcome
        .events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::AssistantMessage { text, .. } => Some(text.clone()),
            _ => None,
        })
        .expect("final message");
    assert_eq!(final_text, "the answer");
    assert_eq!(outcome.text, "the answer", "RunOutcome.text is unchanged");

    // Persisted and sequenced like every event, through the one boundary.
    let persisted = service
        .sessions()
        .events_for(&outcome.session_id)
        .expect("read");
    let seqs: Vec<u64> = persisted.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (1..=persisted.len() as u64).collect::<Vec<_>>());
}
```

The byte-identical non-streaming run (with the *capability* off — the gate
half of D7; `full_run_emits_ordered_events` already covers the default
MockModel):

```rust
#[tokio::test]
async fn a_non_streaming_provider_records_no_deltas() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let model = MockModel::new().with_capabilities(forge_core::ModelCapabilities {
        streaming: false,
        ..MockModel::new().capabilities()
    });
    let service = AgentService::new(
        Arc::new(model),
        Arc::new(MockRouter::selecting("mock-local")),
        Arc::new(MockExecution::new(tmp.path())),
        Arc::new(NullSkillRegistry),
        Arc::new(JsonlSessionStore::new(tmp.path().join(".forge").join("sessions"))),
        Config::default(),
    );
    let outcome = service.run("hi").await.expect("run");
    assert_eq!(
        event_kinds(&outcome),
        ["run_started", "routing_decision_made", "assistant_message", "turn_completed", "completed"],
        "byte-identical to before"
    );
}

/// A provider that advertises streaming but only implements `complete`
/// (every pre-existing provider) takes the fallback: no deltas, same events.
#[tokio::test]
async fn advertising_streaming_without_an_override_is_a_silent_fallback() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let service = test_service(tmp.path()); // MockModel: streaming: true, no override
    let outcome = service.run("hi").await.expect("run");
    assert!(
        !event_kinds(&outcome).contains(&"assistant_delta"),
        "the default stream_complete emits nothing"
    );
    assert_eq!(outcome.text, "mock response to: hi");
}
```

The redaction proof — a provider whose chunks split a secret mid-token,
which per-payload redaction alone cannot catch:

```rust
/// Splits its answer at fixed byte offsets — including through a secret —
/// to prove the runtime's hold-back, not the provider's chunking, is what
/// lets the redaction boundary see whole tokens.
struct SplittingModel;

#[async_trait::async_trait]
impl ModelProvider for SplittingModel {
    fn name(&self) -> &str { "splitting" }
    fn capabilities(&self) -> forge_core::ModelCapabilities {
        forge_core::ModelCapabilities { streaming: true, tools: false, ..Default::default() }
    }
    async fn complete(&self, _: forge_core::CompletionRequest) -> Result<forge_core::CompletionResponse, ForgeError> {
        unreachable!()
    }
    async fn stream_complete(
        &self,
        _: forge_core::CompletionRequest,
        on_delta: &mut (dyn FnMut(&str) + Send),
    ) -> Result<forge_core::CompletionResponse, ForgeError> {
        for piece in ["the key is sk-abc", "def123456 ok", " bye"] {
            on_delta(piece);
        }
        Ok(forge_core::CompletionResponse {
            model: "splitting".into(),
            content: "the key is sk-abcdef123456 ok bye".into(),
            tool_calls: Vec::new(),
            finish_reason: None,
            usage: None,
        })
    }
}

#[tokio::test]
async fn a_secret_split_across_provider_chunks_is_still_redacted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let service = AgentService::new(
        Arc::new(SplittingModel),
        Arc::new(MockRouter::selecting("splitting")),
        Arc::new(MockExecution::new(tmp.path())),
        Arc::new(NullSkillRegistry),
        Arc::new(JsonlSessionStore::new(tmp.path().join(".forge").join("sessions"))),
        Config::default(),
    );
    let outcome = service.run("tell me").await.expect("run");

    let raw = std::fs::read_to_string(
        tmp.path().join(".forge").join("sessions")
            .join(format!("{}.jsonl", outcome.session_id)),
    )
    .expect("log");
    assert!(!raw.contains("sk-abcdef123456"), "secret leaked split across deltas: {raw}");
    assert!(raw.contains("[REDACTED]"), "the boundary saw the whole token: {raw}");
    // And what was broadcast/collected is the redacted form, like every event.
    let deltas: String = outcome.events.iter().filter_map(|e| match &e.kind {
        EventKind::AssistantDelta { text } => Some(text.as_str()),
        _ => None,
    }).collect();
    assert!(deltas.contains("[REDACTED]"), "{deltas}");
}
```

And the replay guarantee, end to end (resume reconstructs from the final
message; deltas change nothing the model is shown):

```rust
#[tokio::test]
async fn resume_after_a_streamed_run_replays_the_final_message_only() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let service = scripted_service(
        tmp.path(),
        vec![text_reply("first streamed answer"), text_reply("second")],
        forge_core::ApprovalPolicy::Auto,
    );
    let first = service.run("start").await.expect("first");
    let resumed = service.resume(&first.run_id).await.expect("resume");
    assert_eq!(resumed.text, "second");
    // The model saw the final message, not a pile of fragments.
    let model_requests = /* read the scripted model's recorded() — see GOTCHA */;
    let history_text: String = model_requests[1]
        .messages
        .iter()
        .map(|m| m.content.as_str())
        .collect();
    assert!(history_text.contains("first streamed answer"));
    assert!(!history_text.contains("first "), "fragments never replay: {history_text}");
}
```

- [ ] **Step 2: Run** `cargo test -p forge-runtime` → FAIL.
- [ ] **Step 3: Implement** in `crates/forge-runtime/src/service.rs`:

```rust
/// One model call, streamed when the provider supports it.
///
/// Fragments become `assistant_delta` events — rendering-only: the replay
/// record is the assembled `AssistantMessage` emitted by
/// [`emit_assistant_message`](Self::emit_assistant_message) from the
/// returned response, exactly as for a non-streaming call.
///
/// A delta is emitted only up to its last whitespace boundary; the trailing
/// partial token is carried until more text arrives and flushed before this
/// returns. The redactor matches whole patterns per payload
/// (`forge_session`'s one boundary), and a provider chunk can split a
/// secret mid-token — the carry is what makes "deltas pass through the one
/// redaction boundary" true rather than vacuous.
async fn complete_streaming(
    &self,
    sender: &broadcast::Sender<Event>,
    collected: &mut Vec<Event>,
    run_id: &str,
    session_id: &str,
    model: &Arc<dyn ModelProvider>,
    request: CompletionRequest,
) -> Result<forge_core::CompletionResponse, ForgeError> {
    if !model.capabilities().streaming {
        return model.complete(request).await;
    }
    let mut carry = String::new();
    let mut streamed = String::new();
    {
        let mut on_delta = |delta: &str| {
            carry.push_str(delta);
            // Emit the prefix that ends at whitespace; keep the partial token.
            let Some(split) = carry.rfind(char::is_whitespace).map(|i| i + 1) else {
                return;
            };
            let tail = carry.split_off(split);
            streamed.push_str(&carry);
            let event = Event::new(
                run_id,
                session_id,
                EventKind::AssistantDelta { text: std::mem::take(&mut carry) },
            );
            carry = tail;
            // Best-effort: a delta that fails to persist is a rendering gap,
            // never a run failure — the final AssistantMessage carries the
            // same text through the same boundary a moment later.
            if let Err(e) = self.emit(sender, collected, event) {
                tracing::warn!(run_id, error = %e, "assistant delta not persisted");
            }
        };
        let response = model.stream_complete(request, &mut on_delta).await?;
        // The tail: provider contract says fragments concatenate to
        // `content`, so flush whatever is still carried.
        streamed.push_str(&carry);
        if !carry.is_empty() {
            let event = Event::new(run_id, session_id, EventKind::AssistantDelta { text: carry });
            if let Err(e) = self.emit(sender, collected, event) {
                tracing::warn!(run_id, error = %e, "assistant delta not persisted");
            }
        }
        if streamed != response.content {
            // A provider violating the fragment contract must not corrupt
            // the replay record — the response is what gets recorded.
            tracing::warn!(
                run_id,
                "streamed fragments do not concatenate to the response content; \
                 the final assistant_message is unaffected"
            );
        }
        Ok(response)
    }
}
```

Then replace `model.complete(request).await` with
`self.complete_streaming(&sender, &mut collected, &run_id, &session_id, &model, request).await`
at the two call sites — the single-turn path (service.rs:1792) and the loop
(service.rs:1838). `emit_assistant_message` keeps being called with the
response exactly where it is today.

- [ ] **Step 4: Update the two pinned scripted-mock sequences** (they now
  legitimately include deltas — AC1):
  - `scripted_two_turn_run_writes_file_and_emits_full_trail`
    (service/tests.rs:255-277): the final text reply "created main.rs" adds
    `assistant_delta` ×2 (`"created "`, `"main.rs"`) immediately before the
    second `assistant_message`; the sequence grows to 14 events, seqs
    `(1..=14)`. Keep the comments' v3-replay-record wording accurate.
  - `needle_fast_path_absent_engine_changes_nothing`
    (service/tests.rs:2046-2055): "plain loop" adds `assistant_delta` ×2
    (`"plain "`, `"loop"`) before `assistant_message`; 7 events.
- [ ] **Step 5: Run** `cargo test -p forge-runtime -p forge-providers -p forge-core` → PASS,
  then `cargo test --workspace` → PASS.
- [ ] **Step 6:** full gate → PASS. **Commit** —
  `git commit -m "feat(runtime): stream assistant text as assistant_delta events"`

**ACTION** `crates/forge-runtime/src/service.rs`, `crates/forge-runtime/src/service/tests.rs`
**PATTERN** call-site shape per `service.rs:1791-1796` / `:1836-1843`;
emission per `service.rs:965-976`; best-effort-with-warn per the broadcast
comment at `service.rs:972-973`
**GOTCHA** (a) The closure borrows `carry`/`streamed` mutably while
`model.stream_complete(...).await` is in flight — keep them outside the
closure's captures by scoping as written, and do not hold any other mutable
borrow of `collected` across the await. (b) The resume test's
`model_requests` — `scripted_service` doesn't expose the model; either build
the service inline with a visible `Arc<ScriptedMockModel>` (pattern:
`needle_fast_path_never_attempts_a_non_read_only_operation`,
service/tests.rs:2065-2068) or assert against the second run's
`run_started`+replay via a fresh `conversation_from_events` call on the
stored events. Prefer the inline service. (c) `SplittingModel`'s `complete`
is `unreachable!()` — the gate sends capability-streaming providers to
`stream_complete` only; if the test hits `unreachable!()` the gate is wrong.
(d) `carry.rfind(char::is_whitespace).map(|i| i + 1)` — `rfind` with a
predicate returns a byte index, so `+ 1` is wrong for multi-byte whitespace;
use `i + carry[i..].chars().next().map_or(1, char::len_utf8)`. (e) Broadcast
capacity is 64 (service.rs:471): a long answer bursts deltas; lagging
subscribers see `Lagged`, which every consumer already handles
(service.rs:219-225).
**VALIDATE** `cargo test -p forge-core -p forge-runtime -p forge-providers && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings`
**SATISFIES** AC1, AC2, AC3, AC4.

---

### Task 5: BDD proof through the binary + docs that describe today

- [ ] **Step 1: Write `tests/features/streaming.feature`:**

```gherkin
Feature: Token streaming core
  A streaming-capable model's answer is recorded as ordered assistant
  deltas followed by the verbatim assistant message — the replay record
  every front end already reads.

  Scenario: A scripted run records ordered deltas before the final message
    Given a scripted mock model that answers "the answer is ready"
    When I run an agent task
    Then the session events include assistant deltas before the final assistant message
    And the assistant deltas concatenate to the final assistant message text
    And the session events include run started and completed
```

- [ ] **Step 2: Run** `cargo test -p forge-cli --test bdd` → the new scenario
  FAILS (undefined steps).
- [ ] **Step 3: Implement the steps** in `crates/forge-cli/tests/bdd/steps.rs`:
  - `#[given(expr = "a scripted mock model that answers {string}")]` — write
    the project's `.forge/config.toml` with `model = "scripted-mock"` and a
    `mock_script` of `[{"text": "<answer>"}]`; pattern:
    the existing `a scripted mock model that writes {string}`
    (steps.rs:824) and `I run an agent task` (steps.rs:738, already exists —
    reuse it unchanged).
  - `#[then("the session events include assistant deltas before the final assistant message")]` —
    parse the session log exactly as the existing `session events` steps do
    (steps.rs:889-941); assert: at least one `"type": "assistant_delta"`,
    every delta's position precedes the final `assistant_message`, and each
    delta carries a non-empty `text`.
  - `#[then("the assistant deltas concatenate to the final assistant message text")]` —
    `concat(delta texts) == assistant_message.text` for the run.
- [ ] **Step 4: Run** `cargo test -p forge-cli --test bdd` → PASS.
- [ ] **Step 5: Docs.** `docs/reference.md:1354-1372`: `"v": 3` → `"v": 4`;
  add `assistant_delta` to the observability bullet with "(v4) ordered text
  fragments of a streaming model's in-flight answer — rendering-only; replay
  reads the final `assistant_message`". In `ARCHITECTURE.md:407-412` (the
  "log is not just a trace" paragraph), append one sentence: "Schema v4 adds
  `assistant_delta`: a streaming model's text as it arrives, broadcast and
  redacted like every event but never replayed — the final
  `assistant_message` remains the replay record." Do **not** touch the "No
  token-by-token streaming yet" notes (reference.md:1338-1341, README Known
  limitations): no front end renders deltas until TICKET-3, so those
  statements are still true.
- [ ] **Step 6:** full gate → PASS. **Commit** —
  `git commit -m "test(bdd): streaming core scenario; schema v4 docs"`

**ACTION** `tests/features/streaming.feature`, `crates/forge-cli/tests/bdd/steps.rs`, `docs/reference.md`, `ARCHITECTURE.md`
**PATTERN** feature style per `tests/features/sessions.feature`; step style
per `crates/forge-cli/tests/bdd/steps.rs:824-860,889-941`
**GOTCHA** the world's session-log helper excludes `*.decisions.jsonl`
(`crates/forge-cli/tests/bdd/world.rs:707-726`) — reuse it rather than
re-reading the directory, or every "all events" assertion miscounts. The
scripted answer in the feature must contain whitespace (single-token answers
stream as one held-back delta flushed at the end — still correct, but the
multi-delta assertion wants a sentence).
**VALIDATE** `cargo test -p forge-cli --test bdd`
**SATISFIES** AC1 through the compiled binary; keeps user-facing docs honest.

---

## TESTING STRATEGY

**Unit (deterministic, offline — no clocks, no network):**

- forge-core: the fallback emits no deltas and returns `complete()`'s
  response; an override's fragments reach the callback; `assistant_delta`
  serde tag/roundtrip; a literal v3 log line still parses under v4.
- forge-providers: the scripted mock's chunking is a pure function of the
  reply (`word_chunks`); concatenation is exact; a tool-call reply emits no
  deltas; the queue pops exactly once per call; a whitespace-free token is
  never split. This is what makes every higher layer deterministic: tests
  assert exact delta sequences, never timing.
- forge-runtime (the load-bearing layer): ordered deltas then today's exact
  terminal events; concat == final message == `RunOutcome.text`; deltas are
  persisted with monotonic seqs; capability-off providers are
  byte-identical; advertise-but-don't-override is a silent fallback; a
  chunk-split secret is still `[REDACTED]` in the log *and* in the broadcast
  copy (the hold-back carry is what this test proves); resume replays the
  final message and never a fragment.
- replay: a log with interleaved deltas reconstructs *identically* to the
  same log with deltas removed.
- forge-chat: a delta renders nothing and does not trip the answer-once
  record (TICKET-3 changes this deliberately).

**BDD:** one scenario in a new `tests/features/streaming.feature`, through
the compiled binary with `FORGE_TEST_MOCKS=1` and a scripted mock — the
existing convention (`sessions.feature`, `privacy.feature`): steps parse
`.forge/sessions/*.jsonl` and assert on the log. It proves the config →
provider → runtime → store path end to end. Redaction of streamed secrets
stays a unit test (privacy.feature already covers the boundary generically;
the split-chunk case needs a hand-built provider no config can express).

**Deliberately not tested here:** real SSE wire formats (TICKET-2, with
recorded fixtures); any rendered output of deltas (TICKET-3 — there is none
yet to assert).

## VALIDATION COMMANDS

Run in the worktree (`/Users/auser/work/rust/mine/forge/worktrees/t1-streaming-core`),
in this order, all green before each commit and at the end:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p forge-core -p forge-runtime -p forge-providers
cargo test --workspace
cargo test -p forge-cli --test bdd
```

(`just verify` is the umbrella — `check + lint + lint-ffi + test + bdd +
fmt --check`; run it at least at Task 5's end.)

## ACCEPTANCE CRITERIA

- **AC1 — streaming run:** a run over a streaming-capable provider emits
  ordered `assistant_delta` events (each `seq`-ordered, immediately preceding
  its response's `assistant_message`) and then exactly today's terminal
  events (`assistant_message`, `turn_completed`, `completed`); concatenated
  deltas equal the final message text and `RunOutcome.text`. Proven:
  Task 4's first test, Task 5's BDD scenario.
- **AC2 — non-streaming byte-identity:** a capability-off provider, and every
  provider that doesn't override `stream_complete` (advertised or not),
  produces an event sequence identical to today's.
  `full_run_emits_ordered_events` (service/tests.rs:102-133) passes
  *unedited*, and `forge-cli/tests/cli.rs:466-468`'s "5 events" still holds.
- **AC3 — replay unchanged:** `conversation_from_events` and `forge resume`
  reconstruct from the final `AssistantMessage` exactly as today; deltas are
  never replayed. Proven: replay unit test (Task 2), resume test (Task 4).
- **AC4 — one boundary:** deltas are broadcast on the run channel and
  persisted redacted; a secret split across provider chunks is `[REDACTED]`
  in both the log and the broadcast copy. Proven: Task 4's redaction test.
- **AC5 — nothing else moved:** the workspace compiles with zero changes to
  any existing provider's code (only `ScriptedMockModel` gains an override),
  and `cargo test --workspace` is green with exactly two edited test
  expectations (the two pinned scripted-mock sequences, updated in Task 4).
- **AC6 — deterministic mock:** `ScriptedMockModel` streams a fixed,
  reply-derived chunk sequence with no clocks or randomness.

## OPEN QUESTIONS / ASSUMPTIONS

Everything else the ticket touches is settled by the cited code and design
docs: the method shape (D1/D2, forced by `async_trait` + object safety + no
new deps), the additive variant and v4 bump (D3, the events.rs:8-22
convention and the v2→v3 precedent), the retained replay record (D4,
replay.rs's whole reason to exist), the persistence/redaction path (D5/D6,
store.rs:266-300), the capability gate (D7, ARCHITECTURE.md:171). Two genuine
questions remain:

1. **Should `MockModel` (`mock-local`, the zero-setup demo) also stream?**
   *Recommended default: no.* Its run is pinned at exactly "5 events"
   (`crates/forge-cli/tests/cli.rs:466-468`), it is the documented
   first-impression path, and leaving it on the fallback keeps an in-tree
   proof that advertising `streaming: true` without an override degrades
   silently. If TICKET-3 wants a streaming demo without a script file, the
   override is five lines (`word_chunks` over the echo) and that ticket can
   add it with its own rendering tests.
2. **Should `assistant_delta` carry a `turn` (or message index) field?**
   *Recommended default: no.* Deltas are store-`seq`-ordered within a run and
   always precede the `assistant_message` they belong to, so a renderer
   groups by adjacency; a reattaching client gets the same order from the
   backlog. The additive convention makes a later field cheap
   (`#[serde(default)]`) if TICKET-3's ACP mapping turns out to want it.

Assumption stated plainly: `forge run --json` responses gain `assistant_delta`
entries in their `events` array for streaming runs. That is additive for JSON
consumers and consistent with "the run's events"; no test pins the array's
exact contents (checked: cli.rs uses targeted lookups, bdd/steps.rs filters
by `"type"`).

## NOTES

- Ticket effort estimate holds: the diff is ~700–1000 lines including tests
  (the runtime helper + its tests are the bulk).
- The design doc's "No fake token streaming" stance
  (`docs/superpowers/specs/2026-09-24-interactive-chat-ui-design.md` §1) is
  not contradicted: this is real streaming, and §14 bullet 1 names it as the
  follow-up being implemented. No design-doc amendment until TICKET-3 lands
  the transcript half.
- `forge serve`'s SSE endpoint forwards deltas with no code change
  (generic serialization, handlers.rs:304-331) — noted so nobody "adds" it;
  TICKET-3 decides whether ACP/chat surface them.
- Cancellation mid-stream is unchanged from cancellation mid-`complete`: the
  loop polls the token at turn/tool checkpoints (service.rs:1831-1834), so a
  hung model call — streamed or not — unwinds the same way. No new hazard.
- The per-session concurrency guard and `RunState` classification are
  untouched: a delta is never a terminal event, and
  `RunState::of_events` reads only the last kind.
- If a follow-up wants deltas *not* persisted (broadcast-only), that is a
  policy change against D5, not a bug fix — the log is what makes
  `forge session show` and attach backlogs show the stream faithfully.

## AMENDMENTS

(none yet)
