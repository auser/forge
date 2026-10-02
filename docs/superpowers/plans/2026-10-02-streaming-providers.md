# TICKET-2 — Streaming Providers: OpenAI-compatible + Anthropic SSE — Implementation Plan

> **For agentic workers:** implement this plan task-by-task (superpowers:subagent-driven-development or superpowers:executing-plans). Steps use checkbox (`- [ ]`) syntax. Work in the clean worktree `/Users/auser/work/rust/mine/forge/worktrees/t2-streaming-providers` (branch `tui/t2-streaming-providers`, HEAD f5f8235, which includes the merged TICKET-1 streaming core); every `file:line` citation below is against that tree.

## Feature Description

Both real provider families learn to stream. `OpenAiCompatibleModel` (the
oMLX/llama.cpp/Ollama/Moonshot/OpenAI path) and `AnthropicModel` override
`ModelProvider::stream_complete` with real SSE: incremental decode, text
fragments handed verbatim to the runtime's `on_delta`, tool calls reassembled
from their delta grammars and surfaced whole in the returned
`CompletionResponse`, and usage accounting from the stream's terminal frames.
The `EgressPolicy` redirect re-check travels onto the streaming client
unchanged. A server that ignores or rejects `stream: true` degrades
gracefully; a server that dies mid-stream produces a typed error, never a
silent partial answer. Finally, the `streaming` capability bit becomes
truthful per registry entry, so `Capability::Streaming` is a real selection
filter instead of an unwired default.

**Goal:** a run over a streaming-capable real provider emits ordered
`assistant_delta` events through the runtime's existing
`complete_streaming` gate (`crates/forge-runtime/src/service.rs:1185-1263`)
and then exactly today's terminal events; a non-streaming server degrades to
a whole response; replay, redaction, and the egress guarantees are
untouched — they live in TICKET-1's plumbing, which this ticket feeds but
does not modify.

**Architecture:** one new crate-private module (`sse.rs`, a hand-rolled
incremental SSE parser — zero new dependencies; reqwest's
[`Response::chunk()`](https://docs.rs/reqwest/0.12/reqwest/struct.Response.html#method.chunk)
is base API, so not even a cargo feature is added), one new
`EgressPolicy::streaming_client` constructor (no total deadline — see D7),
one `stream_complete` override per family sharing its request-building and
response-parsing with `complete()`, one registry-entry flip
(`claude-sonnet` declares `streaming: Some(true)`), and router-filter
proofs. The trait contract, the runtime, and the event schema are all
TICKET-1's and do not change.

**Spec:** `specs/tickets/interactive-chat-feel.md` (TICKET-2 section);
`docs/superpowers/specs/2026-09-25-streaming-design.md` (Provider support
table at :104-113 — this ticket implements exactly those two rows);
TICKET-1's plan `docs/superpowers/plans/2026-10-01-streaming-core.md`
(D1/D2 settled the callback contract this ticket implements against).

## User Story

As someone running forge against a real model — the local oMLX server the
default config points at, or Claude through a subscription — I want the
model's answer to arrive off the wire as it generates, so that a 7-second
local turn (the design doc's measurement,
`docs/superpowers/specs/2026-09-25-streaming-design.md:20-23`) shows up in
the run's event stream as it happens instead of as one wall of text — and I
want that with no new failure modes: my secrets redacted exactly as today, a
misbehaving server degrading instead of hanging, and every redirect hop of a
streamed answer re-checked under `local_only` exactly like a buffered one.

## Problem

The TICKET-1 plumbing is live, but nothing real feeds it:

1. **Both real providers implement only `complete()`.**
   `OpenAiCompatibleModel` (`crates/forge-providers/src/model.rs:302-467`)
   and `AnthropicModel` (`crates/forge-providers/src/anthropic.rs:105-289`)
   have no `stream_complete` override, so the runtime's capability gate
   (`service.rs:1207`) sends them into TICKET-1's silent whole-response
   fallback — `streaming: true` is advertised, nothing streams.
2. **The capability metadata is untruthful in both directions.**
   `model_from_config` defaults `streaming: true` for every
   OpenAI-compatible entry (`model.rs:758-764`) over a client that cannot
   stream, and the built-in `claude-sonnet` entry declares
   `streaming: Some(false)` (`crates/forge-config/src/lib.rs:389`) over an
   API whose primary mode is SSE. `Capability::Streaming`
   (`crates/forge-core/src/router.rs:9-27`) filters candidates through
   `filter_candidates` (`crates/forge-providers/src/router.rs:14-24`), but a
   filter over wrong metadata is worse than none.
3. **The wire work is unbuilt.** Neither client's request shape can express
   `stream: true`; neither response path can read SSE; both clients carry a
   120 s *total* deadline (`model.rs:831` for the Anthropic branch,
   `model.rs:851` for the OpenAI-compatible one) that would guillotine any
   stream outliving it; and nothing reassembles tool calls from either
   family's delta grammar.

## Solution

Ten design calls, each argued from the code or the wire docs.

**D1 — Streaming lives in the providers, behind the existing gate; the
runtime is untouched.** The merged trait method is
`stream_complete(&self, request, on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send))`
(`crates/forge-core/src/model.rs:178-185` — the higher-ranked bound is TICKET-1's
merged deviation 1; the in-tree override example is
`crates/forge-providers/src/scripted.rs:137-147` and implementations must
match that signature exactly). The runtime already gates on
`capabilities().streaming` (`service.rs:1207`), holds back partial trailing
tokens so a chunk-split secret still reaches the redactor whole
(`service.rs:1212-1238`), and warns if fragments don't concatenate to the
response (`service.rs:1253-1261`). **Redaction is not re-implemented here**:
providers forward decoded text fragments verbatim, unbuffered, never
re-chunked "for secrecy" — the one boundary
(`ARCHITECTURE.md:417-421`) and the runtime carry do the rest.

**D2 — One hand-rolled SSE parser, no new dependency.** New crate-private
module `crates/forge-providers/src/sse.rs`: buffer bytes, cut complete lines
at `\n` (0x0A can never appear inside a multi-byte UTF-8 sequence, so a
complete line is complete UTF-8), strip one trailing `\r`, dispatch on a
blank line; `data:` lines accumulate (multi-line data joins with `\n`),
`event:` names the event, `:`-prefixed lines are comments/keepalives,
`id:`/`retry:`/unknown fields are ignored. The SDK/crate route
(eventsource-stream, reqwest-eventsource) was pre-rejected by the house's
own precedent: `forge-acp` transcribed a protocol rather than pay 52
transitive crates (`ARCHITECTURE.md:364-372`); this grammar is ~80 lines and
needs only `Response::chunk()`, which is base reqwest API on non-wasm
([docs.rs](https://docs.rs/reqwest/0.12/reqwest/struct.Response.html#method.chunk)
— only `bytes_stream()` is gated behind the `stream` feature). Body reads
loop on `response.chunk().await`, so chunk boundaries are invisible to the
parser by construction (proven by the split-at-every-byte test).

**D3 — When the client streams: the caller asked, and the capability allowed
— that's the whole rule.** The runtime calls `stream_complete` only when
`capabilities().streaming` (`service.rs:1207`); the bit comes from the
`[models]` entry over a truthful default (D8). The override itself streams
unconditionally when called — no second-guessing inside the provider. No new
config keys, no flags: the per-entry `streaming = false` off-ramp already
exists (`model.rs:770-772`, and the 09-25 design doc named it as the escape
hatch, `2026-09-25-streaming-design.md:112-113`).

**D4 — Graceful degradation, exactly two shapes, both delta-free and
therefore safe.** (a) *Server ignores `stream: true`* → 200 with a non-SSE
`Content-Type`: read the body, parse it with the *same* parser `complete()`
uses (factored per family so the paths cannot drift), emit zero deltas —
contract-safe ("a provider that cannot honor that must not call `on_delta`
at all", `model.rs:166-177`). (b) *Server rejects the streaming request*
(non-2xx before any delta): fall back to `self.complete(request)` **once**,
logging at `warn` with the off-ramp named (`streaming = false` in
`[models.<name>]`). This is safe precisely because nothing was shown — the
09-25 doc's "never silently retry a partially-shown response"
(`2026-09-25-streaming-design.md:137-140`) is honored by construction. A
genuine 500 pays one extra round-trip on a cold error path; a 400 from a
server that doesn't know `stream_options` (older llama.cpp, older LM Studio)
degrades instead of failing.

**D5 — Mid-stream failures are typed errors; partial text is discarded from
the record.** SSE-level error payloads (Anthropic
`event: error` / `{"type":"error",...}` — [error events](https://docs.anthropic.com/en/api/messages-streaming#error-events);
OpenAI-family `data: {"error":{...}}`), a reqwest chunk error, and
truncation (EOF with no terminal marker *and* no finish reason) all become
`ForgeError::provider` naming the phase and how much had arrived. The
runtime's `fail()` records an `Error` event (`service.rs:1733-1743`), and
`emit_assistant_message` never runs for the failed call, so the replay
record carries no half-answer; deltas already persisted are rendering-only
orphans that replay ignores (`replay.rs:194-206`) — the log stays
consistent. **Tolerance nuance:** EOF *after* a terminal signal without the
final sentinel — OpenAI `finish_reason` seen but no `data: [DONE]`;
Anthropic `message_delta.stop_reason` seen but no `message_stop` — is
accepted (debug-logged), because real servers omit sentinels; EOF with no
terminal signal at all is the truncation error.

**D6 — Tool-call reassembly is per-family, index-keyed, and never streams
arguments.** Only text is streamed (the TICKET-1 contract); argument
fragments buffer inside the provider. **OpenAI:** `delta.tool_calls[]`
entries carry `index`; `id`/`function.name` arrive on that index's first
chunk, `function.arguments` string fragments concatenate
([ChatCompletionChunk](https://platform.openai.com/docs/api-reference/chat/streaming));
at stream end, sort by index, parse each accumulated string as JSON —
invalid JSON degrades to `Value::String`, the exact tolerance `complete()`
already has (`model.rs:442-443`); a call whose id or name never arrived is
dropped, same as the non-streaming `filter_map` (`model.rs:435-445`).
**Anthropic:** `content_block_start` opens a `tool_use` block with `id` +
`name` and an empty JSON buffer; `input_json_delta.partial_json` fragments
concatenate; `content_block_stop` closes and parses (empty buffer → `{}`)
([input JSON delta](https://docs.anthropic.com/en/api/messages-streaming#input-json-delta)
— partial JSON strings, parse once at block end). Multiple text blocks are
joined with `\n` exactly as `complete()` does (`anthropic.rs:243-249`), and
the separator is emitted *as a delta* so concatenated deltas still equal
`content`. Unknown block/delta/event types are skipped — Anthropic's
versioning policy explicitly adds event types
([other events](https://docs.anthropic.com/en/api/messages-streaming#other-events));
that covers `ping`, `thinking_delta` (forge never enables thinking), and
server-side-fallback blocks.

**D7 — Streaming gets its own client: connect + read timeouts, no total
deadline.** The existing clients carry `.timeout(120 s)`, and reqwest's
`timeout` "is applied from when the request starts connecting until the
response body has finished"
([docs.rs](https://docs.rs/reqwest/0.12/reqwest/struct.ClientBuilder.html#method.timeout))
— a total deadline that would cut any stream outliving 120 s. So
`EgressPolicy` gains `streaming_client(timeout)`:
`.connect_timeout(timeout) + .read_timeout(timeout)`, no total bound;
`read_timeout` "applies to each read operation, and resets after a
successful read... more appropriate for detecting stalled connections when
the size isn't known beforehand"
([docs.rs](https://docs.rs/reqwest/0.12/reqwest/struct.ClientBuilder.html#method.read_timeout)).
A stalled server dies in 120 s; Anthropic's `ping` keepalives and ordinary
deltas are reads, so a healthy stream never trips it — including Anthropic's
documented tool-input pauses
([input JSON delta](https://docs.anthropic.com/en/api/messages-streaming#input-json-delta)).
The custom redirect policy is factored so both constructors apply the
identical `LocalOnly` arm (`local_only.rs:159-178` today) — the redirect
re-check per hop (`ARCHITECTURE.md:184-191`) holds on streams unchanged.
Each provider builds both clients at construction; `EgressPolicy` is already
a constructor parameter (`model.rs:208-228`, `anthropic.rs:35-58`), so no
constructor signature changes.

**D8 — Capability metadata becomes truthful; the filter becomes real;
nothing at runtime starts *requiring* streaming.** Both families actually
stream after this ticket, so: the `model_from_config` OpenAI-compatible
default `streaming: true` (`model.rs:758-764`) becomes true in fact; the
built-in `claude-sonnet` entry flips `streaming: Some(false)` →
`Some(true)` (`forge-config/src/lib.rs:389`); `AnthropicModel`'s "streaming
is not implemented" doc (`anthropic.rs:21-22`) is rewritten. Router-side,
`filter_candidates` (`router.rs:14-24`) is pinned by new
`StaticRouter`/`CheapestRouter` tests requiring `Capability::Streaming`
against a `streaming: false` registry entry. But
`RoutingRequest.required_capabilities` stays **empty**
(`service.rs:1976-1980`): degradation beats refusal across the whole failure
ladder (`ARCHITECTURE.md:439-451`), a non-streaming provider still answers
correctly (TICKET-1's D2), and a pinned model (`--model`) must never be
refused for lacking it. Unknown models keep `optimistic_caps`
(`router.rs:421-429`, `streaming: true`) — the static router must be able to
select unregistered models; that's its documented job
(`router.rs:76-80`).

**D9 — Usage accounting: ask for it, tolerate its absence.** OpenAI-family:
send `stream_options: {"include_usage": true}` on every streaming request;
the usage chunk arrives last with `choices: []`
([ChatCompletionStreamOptions](https://platform.openai.com/docs/api-reference/chat/create#chat-create-stream_options));
oMLX supports it ([oMLX README](https://github.com/jundot/omlx/blob/main/README.md));
servers that don't know the field ignore it, and one that *rejects* it is
covered by D4b. Absent → `usage: None` → the existing None-cost posture
("a response with no usage stays `None`, never 0.0",
`service.rs:1070-1091`). OpenAI documents that interrupted streams lose the
usage chunk — acceptable: the response is lost too (D5). Anthropic:
`message_start.message.usage.input_tokens` plus the cumulative
`message_delta.usage.output_tokens`; `Usage` is assembled only when both are
known — never fabricated.

**D10 — Cancellation is structural, not new machinery.** Dropping the
`stream_complete` future drops the `Response` and closes the connection —
the 09-25 doc's requirement (`2026-09-25-streaming-design.md:84-92`). The
runtime cancels at turn/tool checkpoints (`service.rs:2191-2194`) and
`start_run`'s task abort (`service.rs:1437-1448`) drops the in-flight
future. Nothing to build; one sentence in the crate docs to keep it true.

## Out of Scope

- **Rendering deltas anywhere**: forge-chat's incremental block, ACP live
  `agent_message_chunk`s, piped-mode behavior — TICKET-3. The silence arms
  TICKET-1 added (`forge-chat/src/render.rs`, `forge-acp/src/dispatch.rs`)
  stay silent, and the "no token-by-token streaming" user-facing notes
  (`docs/reference.md:1376-1379`, `:1846`) stay **untouched** — they remain
  true until TICKET-3.
- **needle / graph / embeddings**: untouched.
- **No trait-shape change**: the callback contract is settled (TICKET-1 D1);
  the 09-25 doc's `BoxStream` sketch is superseded (recorded in Task 5's
  doc update, not re-litigated).
- **No stream resume/capture-and-continue** after a mid-stream failure
  (Anthropic documents the pattern; a follow-up if wanted) — this ticket
  surfaces the failure typed.
- **No reasoning/thinking channel**: OpenAI-family `delta.reasoning_content`
  (DeepSeek, llama.cpp) and Anthropic `thinking_delta` are ignored; see Open
  Questions Q1.
- **No BDD feature file**: provider SSE is proven against wiremock below the
  process boundary, and the binary-level streaming proof already exists
  (TICKET-1's `tests/features/streaming.feature`, scripted-mock).
- **forge-server / forge-mcp / forge-acp adapters**: they consume the event
  stream generically (TICKET-1 verified); untouched.

## Metadata

- Date: 2026-10-02. Base: worktree `t2-streaming-providers`, HEAD f5f8235
  (includes merged TICKET-1).
- Ticket: `specs/tickets/interactive-chat-feel.md` TICKET-2. Depends on:
  TICKET-1 (merged). Blocks: TICKET-3 (only for real-model feel — mocks
  already prove the rendering).
- Estimate: ~1,300–1,700 lines including tests — above the ticket's
  800–1200; the two families' SSE test matrices and the parser are the bulk,
  and there is no architectural surprise (see NOTES). Crates touched:
  forge-providers (one new module, two clients, router tests), forge-config
  (one literal + one test), forge-runtime (one dev-dep + two e2e tests),
  docs. **Zero new dependencies** (D2).

## CONTEXT REFERENCES

### Files to read first (with why)

| file:line | why |
| --- | --- |
| `crates/forge-core/src/model.rs:153-186` | the `ModelProvider` trait as merged — `stream_complete`'s `for<'a>` HRTB callback (:178-185) and the contract its doc comment states (:166-177) |
| `crates/forge-core/src/model.rs:133-151` | `Usage` / `CompletionResponse` — the shapes the stream assembles |
| `crates/forge-runtime/src/service.rs:1185-1263` | `complete_streaming` — the gate (:1207), the hold-back carry (:1212-1238), the contract warn (:1253-1261). **Read-only this ticket**; it defines what a provider's fragments flow through |
| `crates/forge-runtime/src/service.rs:1070-1091` | `account_completion` — `response.usage` is all the budget machinery reads; `None` stays `None` (D9) |
| `crates/forge-runtime/src/service.rs:1976-1980` | the `RoutingRequest` construction — `required_capabilities: Vec::new()` stays (D8) |
| `crates/forge-providers/src/model.rs:190-300` | `OpenAiCompatibleModel` + the request wire structs `stream_complete` extends |
| `crates/forge-providers/src/model.rs:302-467` | `complete()` — the error mapping (:368-410) and the response parsing (:411-465) the streaming path factors and shares |
| `crates/forge-providers/src/model.rs:707-858` | `model_from_config` — capability defaults (:758-782), the anthropic branch (:807-834), both client timeouts (:831, :851) |
| `crates/forge-providers/src/anthropic.rs:21-30` | the struct and the "streaming is not implemented" doc comment this ticket makes false |
| `crates/forge-providers/src/anthropic.rs:105-289` | `complete()` — auth headers (:186-201), error mapping (:203-233), response parsing (:234-287, text-block `\n` join at :243-249) |
| `crates/forge-providers/src/local_only.rs:130-198` | `EgressPolicy` + `client()` + `error_detail` — `streaming_client` lands beside them (D7) |
| `crates/forge-providers/src/router.rs:14-24` | `filter_candidates` — the filter the new tests pin |
| `crates/forge-providers/src/router.rs:76-90,421-429` | `capabilities_of` / `optimistic_caps` — unchanged; the tests must respect their semantics |
| `crates/forge-providers/src/scripted.rs:137-147` | the in-tree `stream_complete` override — exact signature to match, including `for<'a>` |
| `crates/forge-config/src/lib.rs:74-91` | `capabilities_if_known` — unset fields default `false`; the registry flip is one literal |
| `crates/forge-config/src/lib.rs:376-394` | the built-in `claude-sonnet` entry — `streaming: Some(false)` at :389 flips to `Some(true)` |
| `crates/forge-cli/src/commands/service.rs:134-164` | how routed model names become providers (the factory calls `model_from_config`) — read to confirm no CLI change is needed |

### Wire formats (the ground truth for Tasks 2–3)

**OpenAI-compatible** — [chat.create `stream`](https://platform.openai.com/docs/api-reference/chat/create#chat-create-stream), [`stream_options`](https://platform.openai.com/docs/api-reference/chat/create#chat-create-stream_options), [chunk object](https://platform.openai.com/docs/api-reference/chat/streaming):

- `POST /v1/chat/completions` with `"stream": true`; response is
  `Content-Type: text/event-stream`, one `data: <json>` per chunk,
  terminated by `data: [DONE]`.
- Each chunk: `choices[i].delta` carries `content` (text fragment, may be
  absent/null), `role` (first chunk only), and `tool_calls[]` entries of
  `{index, id?, function{name?, arguments?}}` — `arguments` arrives as
  *string fragments* to concatenate per `index`. `finish_reason` is null
  until the terminal content chunk.
- With `stream_options.include_usage`, one extra chunk precedes `[DONE]`
  with `choices: []` and `usage` populated; all earlier chunks carry
  `usage: null`. Absent when unsupported or the stream died.

**Anthropic** — [messages streaming](https://docs.anthropic.com/en/api/messages-streaming) ([event types](https://docs.anthropic.com/en/api/messages-streaming#event-types), [delta types](https://docs.anthropic.com/en/api/messages-streaming#content-block-delta-types)):

- `POST /v1/messages` with `"stream": true`; SSE with both `event:` names
  and a matching `"type"` inside each JSON payload (key off the payload's
  `type` — robust if a proxy drops event names).
- Order: `message_start` (carries `usage.input_tokens`) → per content block
  `content_block_start` / `content_block_delta`* / `content_block_stop` →
  `message_delta` (`delta.stop_reason`, cumulative `usage.output_tokens`) →
  `message_stop`. `ping` may interleave.
- `text_delta.text` streams prose; `input_json_delta.partial_json` streams a
  tool_use block's input as partial JSON strings — accumulate, parse at
  `content_block_stop`.
- `event: error` carries `{"type":"error","error":{"type":"overloaded_error","message":...}}`
  mid-stream.
- Unknown event types must be handled gracefully (versioning policy).

**Where "OpenAI-compatible" deviates (oMLX / llama.cpp / LM Studio /
DeepSeek) and how the client tolerates it:** usage chunk may never arrive
(D9: `None`); `[DONE]` may be missing after a final `finish_reason` chunk
(D5: accepted); `content` may be `null` or absent on role/finish chunks
(skip); unknown fields (`timings`, `reasoning_content`) are ignored;
keepalive comment lines (`:`) and CRLF endings are legal SSE (the parser
handles both); a server may answer a streaming request with a plain JSON
body (D4a) or a 4xx (D4b). oMLX specifically supports both wire families and
`include_usage` ([oMLX README](https://github.com/jundot/omlx/blob/main/README.md)),
so the default `qwen3-coder` entry streams with usage on a current oMLX.

### Tests that pin today's shape (know them before changing anything)

- `crates/forge-providers/src/model.rs:978-1012`
  (`openai_compatible_maps_chat_completion_response`) and :1304-1369
  (`openai_compatible_sends_tools_and_parses_tool_calls`) — the non-streaming
  request/response shape. `stream: false` must serialize **identically to
  today** (`skip_serializing_if`), so these pass unedited.
- `crates/forge-providers/src/anthropic.rs:325-462` — the three Anthropic
  wire tests (auth headers, tool round-trip, error mapping); same rule.
- `crates/forge-providers/src/model.rs:1928-2094` — the `local_only`
  redirect suite against `EgressPolicy::client`; `client()` must stay
  byte-identical (the redirect policy is *factored*, not rewritten).
- `crates/forge-providers/src/scripted.rs:192-281` — TICKET-1's streaming
  contract tests; unaffected, they are the contract's in-tree statement.
- `crates/forge-runtime/src/service/tests.rs` — TICKET-1's runtime streaming
  tests (deltas, redaction carry, resume); the runtime gains two tests and
  loses none.
- `crates/forge-config/src/tests.rs` — nothing pins `claude-sonnet`'s
  streaming flag today (checked: only cost/explain lookups at :80, :166,
  :429-433); the flip is safe.

### New files

- `crates/forge-providers/src/sse.rs` — the parser (D2). Crate-private
  (`mod sse;` in `lib.rs`, no re-export).

No other new files: SSE transcripts live as `const &str` fixtures in the two
providers' existing test modules — the house convention is inline wire
fixtures (`anthropic.rs:332-339`, `model.rs:982-989`), and `include_str!` is
reserved for shipped config presets.

### Patterns to follow

The override signature, verbatim from the scripted mock
(`crates/forge-providers/src/scripted.rs:137-147`):

```rust
    async fn stream_complete(
        &self,
        request: CompletionRequest,
        on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send),
    ) -> Result<CompletionResponse, ForgeError> {
```

The wiremock fixture pattern, from `anthropic.rs:325-358`:

```rust
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "sk-ant-test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({...})))
            .expect(1)
            .mount(&server)
            .await;
```

(SSE variants use `.insert_header("content-type", "text/event-stream")` +
`.set_body_string(SSE_TRANSCRIPT)`; the server delivers the whole body and
closes — `chunk()` sees EOF cleanly. Arbitrary-split robustness is proven at
the parser level, so the mock does not need real chunked encoding.)

The transport-error mapping, from `model.rs:368-391` — reused for the
streaming `send()` and extended with a mid-stream variant:

```rust
        let response = http.send().await.map_err(|e| {
            if e.is_timeout() {
                ForgeError::provider(format!("model request to {url} timed out"))
            } else if e.is_redirect() {
                // `local_only` refusing a hop lands here...
                ForgeError::provider(format!(
                    "model request to {url} was not completed: {}",
                    crate::local_only::error_detail(&e)
                ))
            } else if e.is_connect() { ... } else { ... }
        })?;
```

The streaming `EgressPolicy` constructor, beside `client()`
(`local_only.rs:159-178`):

```rust
    /// An HTTP client for *streaming* requests: no total deadline (reqwest's
    /// `timeout` runs connect → body-end, which would guillotine a long
    /// stream); stalls are bounded per read instead, and SSE keepalives are
    /// reads. The redirect policy is identical to `client()`'s — every hop
    /// is still re-checked.
    pub fn streaming_client(self, timeout: Duration) -> Result<reqwest::Client, reqwest::Error> {
        let builder = reqwest::Client::builder()
            .connect_timeout(timeout)
            .read_timeout(timeout);
        match self {
            Self::Unrestricted => builder,
            Self::LocalOnly => builder.redirect(local_redirect_policy()),
        }
        .build()
    }
```

(`local_redirect_policy()` is the existing custom closure at
`local_only.rs:163-175`, factored so both constructors share it verbatim.)

## IMPLEMENTATION PLAN (phases)

- **Phase 1 — shared plumbing (Task 1).** `sse.rs` exists with its
  split-proof tests; `EgressPolicy::streaming_client` exists with its
  redirect tests. Nothing calls either yet; the workspace is green.
- **Phase 2 — the OpenAI-compatible family streams (Task 2).** Request
  gains `stream`/`stream_options`; parsing is factored; `stream_complete`
  decodes, reassembles, falls back, and errors per D4/D5/D6; the full
  wiremock matrix lands. Workspace green.
- **Phase 3 — the Anthropic family streams (Task 3).** Same shape against
  the event/block grammar; partial-JSON tool_use reassembly; the wiremock
  matrix lands. Workspace green.
- **Phase 4 — truthful capabilities, real filter (Task 4).** The
  `claude-sonnet` entry flips, doc comments catch up, and the router tests
  pin `Capability::Streaming` as a selection filter.
- **Phase 5 — end-to-end proof and docs (Task 5).** The runtime drives a
  wiremock SSE server through `AgentService` and the log shows
  `assistant_delta`s; docs say what is now true.

## STEP-BY-STEP TASKS

### Task 1: the SSE parser and the streaming egress client

- [ ] **Step 1: Write the failing tests.** New file
  `crates/forge-providers/src/sse.rs` starting as the module skeleton with
  `#[cfg(test)] mod tests` (the parser itself doesn't exist yet — write the
  tests against the API sketched in D2):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(chunks: &[&[u8]]) -> Vec<SseEvent> {
        let mut parser = SseParser::new();
        let mut events = Vec::new();
        for chunk in chunks {
            events.extend(parser.feed(chunk));
        }
        events.extend(parser.finish());
        events
    }

    #[test]
    fn openai_style_data_only_events() {
        let events = parse_all(&[b"data: {\"a\":1}\n\ndata: [DONE]\n\n"]);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], SseEvent { event: None, data: "{\"a\":1}".into() });
        assert_eq!(events[1].data, "[DONE]");
    }

    #[test]
    fn anthropic_style_named_events_and_keepalives() {
        let bytes = b": keepalive\r\n\r\nevent: message_start\r\ndata: {\"type\":\"message_start\"}\r\n\r\n";
        let events = parse_all(&[bytes]);
        assert_eq!(events.len(), 1, "comments dispatch nothing");
        assert_eq!(events[0].event.as_deref(), Some("message_start"));
    }

    #[test]
    fn multi_line_data_joins_with_newline() {
        let events = parse_all(&[b"data: first\ndata: second\n\n"]);
        assert_eq!(events[0].data, "first\nsecond");
    }

    #[test]
    fn finish_flushes_an_unterminated_final_event() {
        // A stream ending `data: [DONE]\n` (no blank line) still delivers it.
        let events = parse_all(&[b"data: hello\n\ndata: [DONE]\n"]);
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].data, "[DONE]");
    }

    /// The determinism workhorse: network chunk boundaries are invisible.
    /// Splits a transcript at *every byte* — including through a multi-byte
    /// UTF-8 character — and asserts the events never change.
    #[test]
    fn chunk_boundaries_are_invisible() {
        let transcript: &[u8] = "event: content_block_delta\r\ndata: {\"delta\":{\"text\":\"héllo ✨\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".as_bytes();
        let whole = parse_all(&[transcript]);
        for split in 0..transcript.len() {
            let parts = [&transcript[..split], &transcript[split..]];
            assert_eq!(parse_all(&parts), whole, "split at byte {split}");
        }
    }
}
```

  In `crates/forge-providers/src/local_only.rs`'s test module, mirroring
  `the_local_only_client_refuses_an_off_device_redirect`
  (`crates/forge-providers/src/model.rs:1985-2007`):

```rust
    #[tokio::test]
    async fn the_streaming_client_refuses_an_off_device_redirect() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .respond_with(
                wiremock::ResponseTemplate::new(302)
                    .insert_header("location", "https://evil.example.com/x"),
            )
            .mount(&server)
            .await;
        let client = EgressPolicy::LocalOnly
            .streaming_client(Duration::from_secs(5))
            .expect("client");
        let err = client.get(server.uri()).send().await.expect_err("refused");
        assert!(err.is_redirect(), "{err}");
        let detail = error_detail(&err);
        assert!(detail.contains("evil.example.com"), "{detail}");
        assert!(detail.contains("local_only refused"), "{detail}");
    }

    #[test]
    fn both_policies_build_a_streaming_client() {
        assert!(EgressPolicy::LocalOnly.streaming_client(Duration::from_secs(1)).is_ok());
        assert!(EgressPolicy::Unrestricted.streaming_client(Duration::from_secs(1)).is_ok());
    }
```

- [ ] **Step 2: Run** `cargo test -p forge-providers sse` → FAIL (no such
  module); `cargo test -p forge-providers local_only` → FAIL (no such
  method).
- [ ] **Step 3: Implement `sse.rs`** per D2 (the `SseParser`/`SseEvent`
  sketched in the Solution: byte buffer, `\n` line cuts, `\r` strip,
  blank-line dispatch, `data:`/`event:`/comment/ignored fields,
  `finish()` flushing a pending event). Add `mod sse;` to
  `crates/forge-providers/src/lib.rs:10-17` (crate-private — no `pub use`).
- [ ] **Step 4: Implement `streaming_client`** in
  `crates/forge-providers/src/local_only.rs` per the PATTERN excerpt: factor
  the `LocalOnly` redirect closure (`local_only.rs:163-175`) into
  `fn local_redirect_policy() -> reqwest::redirect::Policy`, call it from
  both `client()` and `streaming_client()`. `client()`'s behavior must be
  byte-identical — the existing redirect suite
  (`model.rs:1928-2094`) is the proof.
- [ ] **Step 5: Run** `cargo test -p forge-providers` → PASS; full gate
  (VALIDATION COMMANDS below) → PASS. **Commit** —
  `git commit -m "feat(providers): SSE parser + streaming egress client (no total deadline)"`

**ACTION** `crates/forge-providers/src/sse.rs` (new),
`crates/forge-providers/src/lib.rs`, `crates/forge-providers/src/local_only.rs`
**PATTERN** `local_only.rs:159-178` (constructor shape), `local_only.rs:228-339`
(test style); doc-comment voice per `local_only.rs:113-129`
**GOTCHA** (a) cut lines out of the *byte* buffer, never decode-then-split —
a multi-byte char split across chunks is legal on the wire and only safe
because `0x0A` can't be a continuation byte; (b) `finish()` must dispatch a
pending event (a server may end `data: [DONE]\n` with no trailing blank
line) but must NOT invent one from an empty buffer; (c) blank line with no
accumulated data dispatches nothing (keepalive gaps); (d) the redirect
closure today is inline in `client()` — factor, don't copy: two copies of
the locality rule is how they drift; (e) `wiremock` is already a dev-dep
(`crates/forge-providers/Cargo.toml:19-24`) — `local_only.rs`'s tests gain
`use wiremock::...` like `model.rs`'s do.
**VALIDATE** `cargo test -p forge-providers && cargo clippy --workspace --all-targets -- -D warnings`
**SATISFIES** the parser half of the ticket's SSE scope; AC5's mechanism
(the streaming client carries the redirect re-check).

---

### Task 2: `OpenAiCompatibleModel::stream_complete`

- [ ] **Step 1: Write the failing tests** in
  `crates/forge-providers/src/model.rs`'s test module. Recorded-transcript
  fixtures as `const &str` (annotate each "recorded shape, OpenAI chat
  completions streaming"). The matrix (each its own `#[tokio::test]`):

  1. `streaming_emits_deltas_then_the_assembled_response_and_usage` — SSE
     transcript: role chunk, two content chunks (`"The answer"`,
     `" is ready"`), a `finish_reason: "stop"` chunk, the `include_usage`
     chunk (`choices: []`, full usage), `data: [DONE]`. Assert: deltas are
     exactly `["The answer", " is ready"]`; `concat == response.content`;
     `finish_reason == "stop"`; `usage.total_tokens == Some(16)`; and from
     `server.received_requests()`: `body["stream"] == true`,
     `body["stream_options"]["include_usage"] == true` (D9).
  2. `streaming_reassembles_tool_calls_from_argument_fragments` — one text
     delta, then `tool_calls` chunks: index 0 opens with `id`/`name`/empty
     arguments, arguments arrive in three fragments; index 1 (a second
     parallel call) interleaved; `finish_reason: "tool_calls"`; `[DONE]`.
     Assert: exactly one text delta; two `ToolCall`s in index order with
     parsed argument objects; no tool JSON in the deltas (D6).
  3. `streaming_without_a_usage_chunk_leaves_usage_none` — transcript minus
     the usage chunk (a server ignoring `include_usage`, e.g. older LM
     Studio). Assert `usage.is_none()` and the response is otherwise whole.
  4. `a_server_ignoring_stream_returns_a_whole_json_body_with_no_deltas` —
     200 with `Content-Type: application/json` and a normal chat-completion
     body (D4a). Assert: response parsed, zero deltas, `.expect(1)` — no
     second request.
  5. `a_server_rejecting_stream_falls_back_to_complete_once` — first POST
     400 `{"error":{"message":"unknown field stream_options"}}`, second POST
     200 JSON (D4b). Assert: response delivered; **two** requests seen, the
     second body's JSON has no `stream` key; the first has
     `"stream": true`.
  6. `an_error_payload_mid_stream_is_a_typed_error` — one content chunk,
     then `data: {"error":{"message":"overloaded","type":"server_error"}}`,
     then EOF (D5). Assert `ForgeError::Provider` naming `overloaded`; the
     partial text is not in any response (there is no response).
  7. `a_truncated_stream_without_a_terminal_marker_errors` — content chunk,
     then EOF: no `finish_reason`, no `[DONE]`. Assert `ForgeError::Provider`
     whose message says the stream ended early and names the byte count.
  8. `a_stream_ending_after_finish_reason_without_done_is_accepted` — full
     chunks, `finish_reason: "stop"`, EOF without `[DONE]` (D5 tolerance).
     Assert success, debug-log only.
  9. `openai_compat_tolerances` — one transcript containing: a `:` keepalive
     line, CRLF endings, a chunk with `"timings": {...}` (llama.cpp), a
     chunk with `delta.reasoning_content` (DeepSeek — ignored, Q1), and a
     `content: null` delta. Assert the streamed text is exactly the real
     content fragments.
  10. `streaming_requests_honor_the_local_only_redirect_recheck` — provider
      built with `EgressPolicy::LocalOnly` against a wiremock 307 to an
      `api.localhost:<port>` URL (the established trick,
      `model.rs:1928-1981`): `stream_complete` errs, the message contains
      `local_only refused to follow a redirect` and `api.localhost`, and the
      second server saw zero requests. (Ticket: "honoring the EgressPolicy
      redirect re-check on every hop".)

  Also assert in test 1's shadow: `concat(deltas) == response.content`
  (the runtime's contract warn at `service.rs:1253-1261` must never fire).

- [ ] **Step 2: Run** `cargo test -p forge-providers` → FAIL (no
  `stream_complete` behavior).
- [ ] **Step 3: Implement** in `crates/forge-providers/src/model.rs`:
  - `OpenAiCompatibleModel` gains `stream_client: reqwest::Client`; `new()`
    builds it via `egress.streaming_client(timeout)` beside the existing
    `egress.client(timeout)` (:216-218). Constructor signature unchanged.
  - `ChatRequest` (:241-250) gains
    `#[serde(skip_serializing_if = "std::ops::Not::not")] stream: bool` and
    `#[serde(skip_serializing_if = "Option::is_none")] stream_options: Option<ChatStreamOptions>`;
    `ChatStreamOptions { include_usage: bool }`. With `stream: false` the
    wire body is byte-identical to today (the pinned tests prove it).
  - Factor: `fn chat_body(&self, request: &CompletionRequest, stream: bool) -> ChatRequest<'_>`
    (the mapping at :314-351), `fn parse_chat_completion(model: &str, url: &str, body: &serde_json::Value) -> Result<CompletionResponse, ForgeError>`
    (the tail at :416-465), and `fn parse_usage(value: &serde_json::Value) -> Option<Usage>`
    (:458-464). `complete()` becomes reject → `chat_body(false)` → send via
    `self.client` → status map → `parse_chat_completion` — no behavior change.
  - Add the `OpenAiStream` assembly (D6): `content: String`,
    `calls: BTreeMap<u64, PartialToolCall{id, name, arguments: String}>`,
    `finish_reason`, `usage`, `saw_done`; `apply(&mut self, event: &SseEvent, on_delta: &mut (dyn for<'a> FnMut(&'a str) + Send)) -> Result<bool, ForgeError>`
    handling `[DONE]`, `error` payloads, the usage chunk, `delta.content`
    (skip empty strings — no empty deltas), `delta.tool_calls` index
    accumulation, `finish_reason`; `into_response(self, model) -> CompletionResponse`
    sorting calls by index and parsing argument strings with the
    invalid-JSON → `Value::String` tolerance (:442-443), empty string → `{}`.
  - `stream_complete`: reject → `chat_body(true)` → send via
    `self.stream_client` with the same auth and the same error mapping →
    D4b fallback on non-2xx (`warn!` naming `streaming = false` in
    `[models.<name>]`, then `self.complete(request).await`) → D4a
    content-type check (`text/event-stream` prefix) →
    `loop { response.chunk().await }` through the parser → `parser.finish()`
    flush → D5 truncation check (`!saw_done && finish_reason.is_none()` →
    typed error naming how many answer bytes arrived) → `into_response`.
- [ ] **Step 4: Run** `cargo test -p forge-providers` → PASS (including the
  unedited non-streaming tests — byte-identical wire proof); then
  `cargo test --workspace` → PASS.
- [ ] **Step 5:** full gate → PASS. **Commit** —
  `git commit -m "feat(providers): OpenAI-compatible SSE streaming with graceful fallback"`

**ACTION** `crates/forge-providers/src/model.rs`
**PATTERN** `complete()` at :312-466 (send, error mapping, parsing — now
factored); override signature per `scripted.rs:137-147`; fallback warn style
per `tracing::warn!` at :360-365
**GOTCHA** (a) `chat_body(&request, true)` borrows `request`; the D4b
fallback moves `request` into `self.complete(request)` — NLL ends the
body's borrow at `.json(&body)`, so this compiles only if `body` is never
used after; keep the fallback before the SSE branch. (b) `data` trimming:
compare `[DONE]` against the *trimmed* payload, but JSON-parse the raw
`event.data` (serde tolerates whitespace anyway; never trim into a string
you pass on). (c) `choices: []` on the usage chunk — guard with
`and_then(as_array)` and never index `[0]`. (d) `index` may be absent on
degenerate servers — default 0. (e) `response.chunk()` needs
`mut response`; its error is a reqwest error — map with
`crate::local_only::error_detail` like the send path. (f) Do not emit
deltas for `content: ""` or `content: null` — the runtime would persist
empty `assistant_delta` noise. (g) `#[serde(skip_serializing_if = "std::ops::Not::not")]`
works because std implements `Not` for `&bool` — don't "simplify" it away.
**VALIDATE** `cargo test -p forge-providers && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings`
**SATISFIES** AC1 (OpenAI family), AC2, AC4, AC5, AC6.

---

### Task 3: `AnthropicModel::stream_complete`

- [ ] **Step 1: Write the failing tests** in
  `crates/forge-providers/src/anthropic.rs`'s test module. Fixtures as
  `const &str` ("recorded shape, Anthropic messages streaming" — the
  [full HTTP stream](https://docs.anthropic.com/en/api/messages-streaming#full-http-stream-response)
  examples are the reference). The matrix:

  1. `streaming_emits_text_deltas_and_usage` — `message_start` (usage
     `input_tokens: 25`), a `ping`, `content_block_start` (text), three
     `text_delta`s, `content_block_stop`, `message_delta`
     (`stop_reason: "end_turn"`, `usage.output_tokens: 15`),
     `message_stop`. Assert deltas exact and concatenating to content;
     `usage == Some(Usage{ prompt: 25, completion: 15, total: 40 })`;
     `finish_reason == Some("end_turn")`; request body has
     `"stream": true` and the `x-api-key`/`anthropic-version` headers are
     asserted as today (:330-331).
  2. `streaming_reassembles_tool_use_from_partial_json` — text block, then a
     `tool_use` block (`id`, `name`, `input: {}`) whose
     `input_json_delta.partial_json` arrives in five fragments
     (`"{\"path\":"`, `" \"a.rs\""`, …), `stop_reason: "tool_use"` (D6).
     Assert one `ToolCall` with the parsed object, the text streamed as
     deltas, no JSON in deltas.
  3. `an_error_event_mid_stream_is_a_typed_error` — after one delta,
     `event: error` /
     `data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}`
     (D5). Assert `ForgeError::Provider` naming `overloaded_error`.
  4. `a_stream_without_message_stop_and_no_stop_reason_errors` — truncation
     after a text delta; assert the typed truncation error.
  5. `stop_reason_without_message_stop_is_accepted` — `message_delta` seen,
     EOF before `message_stop` (D5 tolerance).
  6. `unknown_events_and_delta_types_are_skipped` — interleave
     `event: citation` with an unknown payload, a `thinking_delta`, and a
     `signature_delta`; assert only the text streams (versioning policy,
     D6).
  7. `two_text_blocks_join_with_a_newline_delta` — two text blocks; assert
     the separator `\n` is emitted as a delta and
     `concat(deltas) == content` matches `complete()`'s join (:243-249).
  8. `a_non_2xx_streaming_request_falls_back_to_complete_once` — D4b for
     this family; assert two requests, the second without `"stream"`.
  9. `an_oauth_streaming_request_carries_bearer_and_beta_header` — the
     OAuth arm (:196-200) on the streaming path (header matcher, as
     :361-381).

- [ ] **Step 2: Run** `cargo test -p forge-providers anthropic` → FAIL.
- [ ] **Step 3: Implement** in `crates/forge-providers/src/anthropic.rs`:
  - `AnthropicModel` gains `stream_client`; `new()` builds both (D7).
  - `MessagesRequest` (:61-72) gains
    `#[serde(skip_serializing_if = "std::ops::Not::not")] stream: bool`.
  - Factor: `fn messages_body(&self, request: &CompletionRequest, stream: bool) -> MessagesRequest<'_>`
    (:118-184), `fn authed(&self, http: reqwest::RequestBuilder) -> reqwest::RequestBuilder`
    (the credential match at :192-201), and
    `fn parse_message_body(model: &str, url: &str, body: &serde_json::Value) -> Result<CompletionResponse, ForgeError>`
    (:234-287). `complete()` reuses all three, unchanged in behavior.
  - Add the `AnthropicStream` assembly (D6): `content: String`,
    `blocks: BTreeMap<u64, AnthropicBlock>` where
    `enum AnthropicBlock { Text, ToolUse{id, name, json: String}, Other }`,
    `tool_calls: Vec<ToolCall>`, `input_tokens`/`output_tokens`,
    `stop_reason`, `saw_stop`; `apply(...)` keying on the payload's `"type"`
    (`message_start`, `content_block_start` — emitting the `\n` separator
    delta when a text block opens over non-empty content,
    `content_block_delta`, `content_block_stop`, `message_delta`,
    `message_stop`, `ping`, `error`, unknown → skip).
  - `stream_complete` mirrors Task 2's shape: reject → `messages_body(true)`
    → `self.stream_client` + `authed` + same error mapping (:203-233) → D4b
    on non-2xx → D4a on non-SSE content-type → chunk loop → truncation check
    (`!saw_stop && stop_reason.is_none()`) → assemble (usage only when both
    token counts are known — D9, never fabricated).
- [ ] **Step 4: Run** `cargo test -p forge-providers` → PASS;
  `cargo test --workspace` → PASS.
- [ ] **Step 5:** full gate → PASS. **Commit** —
  `git commit -m "feat(providers): Anthropic SSE streaming with tool_use reassembly"`

**ACTION** `crates/forge-providers/src/anthropic.rs`
**PATTERN** `complete()` at :115-288; the OAuth/API-key header split at
:192-201; Task 2's `stream_complete` shape
**GOTCHA** (a) key on the payload's `"type"`, not the `event:` name —
equal today, but the payload survives a proxy that drops event names; the
parser keeps `event` available regardless. (b) `output_tokens` in
`message_delta.usage` is *cumulative* — replace, don't add; `message_start`
also carries an initial `output_tokens` (usually 1) that the final
`message_delta` supersedes. (c) `content_block_delta` for a block whose
start was `Other`/unknown must be skipped — look the block up by `index`,
never assume. (d) an empty `input_json_delta` accumulation is `{}`, not an
error — a tool may legitimately take no arguments. (e) the Anthropic docs
note tool input can pause between events — D7's read timeout is 120 s per
read, generous for this. (f) `stop_reason` passes through verbatim as
`finish_reason` (`end_turn`, `tool_use`, `max_tokens`), matching
`complete()` (:274-277).
**VALIDATE** `cargo test -p forge-providers && cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings`
**SATISFIES** AC1 (Anthropic family), AC4, AC6.

---

### Task 4: truthful capability metadata + the router filter is real

- [ ] **Step 1: Write the failing tests.**
  In `crates/forge-config/src/tests.rs` (near the registry tests, :80):

```rust
#[test]
fn the_builtin_anthropic_entry_declares_streaming() {
    // The Anthropic client streams (forge-providers, TICKET-2); the
    // registry must say so, or `Capability::Streaming` filters lie.
    let config = Config::default();
    let entry = &config.model_entries()["claude-sonnet"];
    assert_eq!(entry.streaming, Some(true));
}
```

  In `crates/forge-providers/src/router.rs`'s test module (the `caps`
  helper at :1117 takes `tools`; add a sibling `fn caps_streaming(streaming: bool)`):

```rust
#[tokio::test]
async fn static_router_never_selects_a_non_streaming_model_when_streaming_is_required() {
    let router = StaticRouter::new("silent").with_registry(vec![
        ("silent".to_string(), caps_streaming(false)),
        ("live".to_string(), caps_streaming(true)),
    ]);
    let request = RoutingRequest {
        task: "x".to_string(),
        required_capabilities: vec![Capability::Streaming],
        candidates: vec!["silent".to_string(), "live".to_string()],
    };
    let decision = router.route(&request).await.expect("routes");
    assert_eq!(decision.selected_model, "live");
    assert!(decision.fallback_used, "the default was filtered out");

    // And with no capable candidate at all, the router says so (the
    // `errors_when_nothing_is_capable` shape at :1181-1193).
    let only_silent = RoutingRequest {
        task: "x".to_string(),
        required_capabilities: vec![Capability::Streaming],
        candidates: vec!["silent".to_string()],
    };
    let err = router.route(&only_silent).await.expect_err("must fail");
    assert!(matches!(err, ForgeError::Router(_)));
}

#[tokio::test]
async fn cheapest_router_filters_non_streaming_candidates_too() {
    let costs = [(String::from("silent"), (0.0, 0.0)), (String::from("live"), (1.0, 1.0))]
        .into_iter()
        .collect();
    let router = CheapestRouter::new(
        costs,
        vec![
            ("silent".to_string(), caps_streaming(false)),
            ("live".to_string(), caps_streaming(true)),
        ],
    );
    let decision = router
        .route(&RoutingRequest {
            task: "x".to_string(),
            required_capabilities: vec![Capability::Streaming],
            candidates: vec!["silent".to_string(), "live".to_string()],
        })
        .await
        .expect("routes");
    // Cheaper but silent must lose to the streaming candidate.
    assert_eq!(decision.selected_model, "live");
}
```

  In `crates/forge-providers/src/model.rs`'s tests, next to
  `an_anthropic_family_model_on_a_local_proxy_still_builds` (:1856-1872),
  same `#[serial]` env pattern:

```rust
#[test]
#[serial_test::serial]
fn an_anthropic_family_model_reports_a_truthful_streaming_capability() {
    // SAFETY: test-only env mutation, serialized via #[serial].
    unsafe { std::env::set_var("ANTHROPIC_API_KEY", "test-key-not-a-real-secret") };
    let config = Config {
        model: "claude-sonnet".to_string(),
        model_base_url: Some("http://127.0.0.1:11434".to_string()),
        local_only: true,
        ..Config::default()
    }
    .with_explicit([forge_config::keys::MODEL_BASE_URL]);
    let built = model_from_config(&config, std::path::Path::new("."));
    unsafe { std::env::remove_var("ANTHROPIC_API_KEY") };
    let model = built.expect("builds");
    assert!(model.capabilities().streaming, "the client streams; the bit must say so");
}
```

- [ ] **Step 2: Run** `cargo test -p forge-config -p forge-providers` →
  FAIL (the registry flip; the router tests may already pass mechanically —
  `filter_candidates` works — they exist to *pin* it; run to confirm).
- [ ] **Step 3: Implement** —
  `crates/forge-config/src/lib.rs:389`: `streaming: Some(false)` →
  `Some(true)` for `claude-sonnet`.
  `crates/forge-providers/src/anthropic.rs:21-22`: rewrite the struct doc —
  "Capabilities are explicit; both whole-response and SSE streaming
  (`stream: true`) requests are supported."
  `crates/forge-providers/src/model.rs:688-696`: extend
  `model_from_config`'s doc with one sentence — "Both real families stream
  (`stream: true` on the wire) unless an entry sets `streaming = false`,
  which keeps the runtime on whole responses for that endpoint."
- [ ] **Step 4: Run** `cargo test --workspace` → PASS (the
  `local_only_leaves_no_configured_model_with_a_remote_endpoint` sweep at
  :1746-1784 and `forge model list` printing, `model_cmd.rs:64-70`, are
  unaffected in shape — the value changed, not the structure).
- [ ] **Step 5:** full gate → PASS. **Commit** —
  `git commit -m "feat(config): claude-sonnet declares streaming; pin Capability::Streaming as a router filter"`

**ACTION** `crates/forge-config/src/lib.rs`, `crates/forge-config/src/tests.rs`,
`crates/forge-providers/src/router.rs` (tests only),
`crates/forge-providers/src/model.rs` (tests + doc), `crates/forge-providers/src/anthropic.rs` (doc)
**PATTERN** router tests at :1165-1193; the serial env test at :1856-1872
**GOTCHA** (a) do **not** add `Capability::Streaming` to any
`RoutingRequest` — D8: required stays empty; these tests exercise the filter
directly. (b) `MockRouter` bypasses filtering by design (it records and
answers, :181-190) — don't "fix" it. (c) `optimistic_caps` keeps
`streaming: true` for unknown models (:421-429) — deliberate; the static
router must reach unregistered models (:76-80).
**VALIDATE** `cargo test -p forge-config -p forge-providers && cargo test --workspace`
**SATISFIES** AC3; the metadata half of AC2.

---

### Task 5: end-to-end proof through the runtime + docs

- [ ] **Step 1: Write the failing tests** in
  `crates/forge-runtime/src/service/tests.rs` (helpers at :13-100). First
  add `wiremock.workspace = true` to `crates/forge-runtime/Cargo.toml`'s
  `[dev-dependencies]` (:20-26). Then:

```rust
/// The oMLX path, end to end: a real OpenAI-compatible client against a
/// local SSE server, driven by the runtime — deltas in the log, then the
/// same terminal events as any other run.
#[tokio::test]
async fn an_openai_compatible_sse_server_streams_deltas_end_to_end() {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .respond_with(
            wiremock::ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\n\
                     data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"the \"},\"finish_reason\":null}]}\n\n\
                     data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"answer\"},\"finish_reason\":null}]}\n\n\
                     data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":null}\n\n\
                     data: {\"choices\":[],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2,\"total_tokens\":7}}\n\n\
                     data: [DONE]\n\n",
                ),
        )
        .mount(&server)
        .await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let model = forge_providers::OpenAiCompatibleModel::new(
        server.uri(),
        "qwen3-coder",
        None,
        forge_core::ModelCapabilities {
            streaming: true,
            tools: false, // single-turn path: the simplest streaming run
            ..Default::default()
        },
        std::time::Duration::from_secs(5),
        forge_providers::EgressPolicy::default(),
    )
    .expect("construct");
    let service = AgentService::new(
        Arc::new(model),
        Arc::new(forge_providers::MockRouter::selecting("qwen3-coder")),
        Arc::new(MockExecution::new(tmp.path())),
        Arc::new(NullSkillRegistry),
        Arc::new(JsonlSessionStore::new(tmp.path().join(".forge").join("sessions"))),
        Config::default(),
    );
    let outcome = service.run("hi").await.expect("run");

    assert_eq!(
        event_kinds(&outcome),
        [
            "run_started",
            "routing_decision_made",
            "assistant_delta", // "the " — the runtime's hold-back emits at whitespace
            "assistant_delta", // "answer" — flushed at stream end
            "assistant_message",
            "turn_completed",
            "completed"
        ],
        "the runtime streams the real provider exactly like the scripted mock"
    );
    let deltas: String = outcome
        .events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::AssistantDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, "the answer");
    assert_eq!(outcome.text, "the answer");
    // Persisted, not just broadcast.
    let raw = std::fs::read_to_string(
        tmp.path().join(".forge").join("sessions").join(format!("{}.jsonl", outcome.session_id)),
    )
    .expect("log");
    assert!(raw.contains("\"assistant_delta\""), "{raw}");
}
```

- [ ] **Step 2: Run** `cargo test -p forge-runtime` → PASS if Tasks 1–4
  landed (this is an integration proof, not TDD — it fails only if the
  wiring is wrong).
- [ ] **Step 3: Docs.**
  `docs/reference.md:935-941` (the `ModelProvider` section): after the
  capabilities sentence, add — "Both real provider families stream over SSE
  (`stream: true` for OpenAI-compatible servers; Anthropic's native event
  stream), and the runtime records the fragments as `assistant_delta`
  events (v4). A `[models.<name>]` entry may set `streaming = false` for an
  endpoint whose SSE misbehaves; that endpoint then answers whole, exactly
  as before streaming landed."
  `ARCHITECTURE.md:453-460` ("What's next"): the TICKET-2 mention becomes
  past tense — provider SSE landed (both families, graceful fallback);
  chat/ACP rendering remains TICKET-3.
  `docs/superpowers/specs/2026-09-25-streaming-design.md`: add a status
  line under the title — "Provider support (the table below) landed in
  TICKET-2 (2026-10-02), on TICKET-1's callback contract rather than this
  doc's `BoxStream` sketch; tool calls surface whole, as §'Tool calls are
  not streamed' specifies. Reasoning channels remain open (see Open
  Questions)."
  **Do not touch** `docs/reference.md:1376-1379` or `:1846` — "no
  token-by-token streaming" stays true at the UI until TICKET-3.
- [ ] **Step 4:** full gate → PASS, including `just verify`
  (Justfile:48). **Commit** —
  `git commit -m "test(runtime): real SSE provider end-to-end; docs for landed provider streaming"`

**ACTION** `crates/forge-runtime/Cargo.toml`,
`crates/forge-runtime/src/service/tests.rs`, `docs/reference.md`,
`ARCHITECTURE.md`, `docs/superpowers/specs/2026-09-25-streaming-design.md`
**PATTERN** `test_service`/`scripted_service` (:13-37) for service
construction; the persisted-log assertion per
`a_secret_split_across_provider_chunks_is_still_redacted` (TICKET-1)
**GOTCHA** (a) `tools: false` keeps the run on the single-turn path
(`service.rs:2109-2177`) — no execution provider involvement; (b) the
runtime's hold-back carry shapes the deltas ("the " then "answer") — don't
assert the provider's raw chunks here, that's Task 2's job; (c) wiremock
delivers the whole body then closes — `chunk()` EOFs cleanly, so `[DONE]`
must be in the body or the truncation check fires (that's D5 working).
**VALIDATE** `cargo test -p forge-runtime && cargo test --workspace && just verify`
**SATISFIES** AC2 (end-to-end), AC7's final proof.

---

## TESTING STRATEGY

**Offline only** — no clocks, no network beyond loopback wiremock, following
the established provider-test pattern (`model.rs:978+`, `anthropic.rs:325+`).

- **Parser level (pure, deterministic):** the SSE grammar tests of Task 1,
  anchored by `chunk_boundaries_are_invisible` — every recorded transcript
  re-parsed at *every* byte split. This is what makes the wiremock layer
  trustworthy: wiremock delivers a whole body in one or two chunks, so
  split-robustness would otherwise be untested.
- **Provider level (wiremock, recorded transcripts):** the Task 2/3
  matrices — text deltas, tool-call reassembly from fragments (both
  grammars), usage present/absent, `ping`/keepalive/unknown-type tolerance,
  oMLX-class deviations (CRLF, `timings`, `reasoning_content`, null
  content), both fallback shapes (D4a JSON-200, D4b one-shot retry), and the
  mid-stream failures: SSE error payloads, truncation without a terminal
  marker, and (Task 2 #10) the `local_only` redirect re-check on the
  streaming path.
- **Router level:** `Capability::Streaming` pinned as a real filter for
  `StaticRouter` and `CheapestRouter` (Task 4); the metadata flip pinned at
  the registry (`forge-config`) and at the built provider (`model.rs`).
- **Runtime level:** Task 5's end-to-end — `AgentService` driving a real
  `OpenAiCompatibleModel` against a wiremock SSE server, asserting the event
  sequence, delta concatenation, and persistence. This is the ticket's
  "oMLX path streams end-to-end against the local server" with the local
  server hermetic.
- **Regression proof:** every pre-existing provider test passes unedited —
  `stream: false` serializes byte-identically (Task 2's existing-body
  assertions), `EgressPolicy::client()` is unchanged (the redirect suite),
  and TICKET-1's runtime/replay/redaction tests are untouched.

**Deliberately not tested here:** a live oMLX/Anthropic call (manual
verification — see NOTES), rendering (TICKET-3), stream resume (out of
scope), and TCP-abort mid-body (wiremock can't force it; the chunk-error
mapping is one line and shared).

## VALIDATION COMMANDS

Run in the worktree
(`/Users/auser/work/rust/mine/forge/worktrees/t2-streaming-providers`), in
this order, all green before each commit and at the end:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p forge-providers -p forge-core -p forge-runtime
cargo test --workspace
cargo test -p forge-cli --test bdd
```

(`just verify` is the umbrella — `check + lint + lint-ffi + test + bdd`;
run it at least at Task 5's end.)

Known pre-existing failure, **baseline, not this ticket's**:
`forge-cli --test chat` `a_typed_ctrl_c_on_a_real_terminal_interrupts_without_killing_the_chat`
is red/flaky on this machine independent of these changes (real-pty timing;
its own comments record the flakes,
`crates/forge-cli/tests/chat.rs:879+`). Compare against a pre-change run;
do not chase it here.

## ACCEPTANCE CRITERIA

- **AC1 — recorded-SSE fixture tests for both families:** text deltas,
  tool-call/tool_use reassembly from fragments, and mid-stream error events
  are covered by wiremock transcripts for OpenAI-compatible (Task 2, tests
  1–3, 6) and Anthropic (Task 3, tests 1–3); truncation covered for both
  (Task 2 #7–8, Task 3 #4–5).
- **AC2 — the oMLX path streams end-to-end against a local server:** the
  OpenAI-compatible client streams from a local SSE server with correct
  deltas, assembly, and usage (Task 2 #1), and the runtime drives the same
  path into the session log (Task 5). Capability metadata is truthful
  (Task 4).
- **AC3 — the router never selects a non-streaming provider when streaming
  is required:** pinned for `StaticRouter` and `CheapestRouter` (Task 4) —
  the candidate known to lack the capability loses even when cheaper or
  default; `required_capabilities` itself stays empty at runtime (D8).
- **AC4 — the streaming contract holds:** for every fixture,
  `concat(deltas) == response.content`, tool calls surface whole in the
  response, and no empty/argument JSON leaks into deltas — so the runtime's
  contract warn (`service.rs:1253-1261`) never fires.
- **AC5 — egress is enforced on streams:** the `LocalOnly` redirect
  re-check refuses an off-device hop on the streaming client (Task 1) and
  through the provider's streaming request (Task 2 #10).
- **AC6 — graceful degradation:** a server ignoring `stream` yields a whole
  response with no deltas and no retry (Task 2 #4, Task 3 implicitly via the
  shared shape); a server rejecting it gets exactly one non-streaming
  fallback with a warn naming the `streaming = false` off-ramp (Task 2 #5,
  Task 3 #8); a mid-stream failure is a typed `ForgeError::Provider`, never
  a silent partial answer (Task 2 #6–7, Task 3 #3–4).
- **AC7 — non-streaming is byte-identical:** every existing provider test
  passes unedited; `complete()`'s wire body is unchanged
  (`skip_serializing_if`); the runtime, event schema, and redaction path
  are untouched.

## OPEN QUESTIONS / ASSUMPTIONS

Two genuine questions; everything else is settled by the cited code, the
wire docs, and TICKET-1.

1. **Should reasoning/thinking channels surface?** OpenAI-family servers
   stream `delta.reasoning_content` (DeepSeek, llama.cpp); Anthropic streams
   `thinking_delta` when thinking is enabled (forge never enables it). The
   09-25 design doc lists this as unresolved and notes needle's `<think>`
   trace should share the answer
   (`2026-09-25-streaming-design.md:131-134`). *Recommended default: ignore
   both* (debug-log once per response at most) — reasoning is not the
   answer, replay has no place for it, and a channel is an additive
   follow-up (new event kind, same convention) if a front end wants it.
2. **Should anything ever *require* `Capability::Streaming`?** The ticket's
   acceptance is conditional ("when streaming is required") and today
   nothing requires it — the runtime degrades instead (D8). TICKET-3's ACP
   chunk forwarding might genuinely want it (a client built for chunks).
   *Recommended default: no requirement now*; if TICKET-3 wants one, it
   belongs in `RunOptions`, not in the shared `RoutingRequest`
   construction.

Assumptions stated plainly: (a) `stream_options.include_usage` is sent on
every streaming request — free where supported, ignored where not, and a
server rejecting it falls under D4b (Q: none — settled by the fallback).
(b) No BDD feature file: the binary-level streaming proof exists
(TICKET-1's `tests/features/streaming.feature`) and provider SSE is proven
below the process boundary. (c) The 120 s value doubles as the streaming
connect/read timeout — no new config; Anthropic's documented tool-input
pauses fit well inside it.

## NOTES

- **Estimate honesty:** the ticket's 800–1200 lines undercounts the two
  test matrices and the parser; ~1,300–1,700 with tests is the realistic
  landing zone. No architectural surprise drives it — it is fixture volume.
- **The merged callback contract** (`for<'a> FnMut(&'a str) + Send`,
  TICKET-1 deviation 1) is what both overrides implement; copy the
  `scripted.rs:137-147` signature verbatim or the impl won't match.
- **Usage on a cancelled stream** is lost with the response (OpenAI
  documents the usage chunk doesn't survive interruption) — consistent with
  D9's never-fabricate rule and with today's failure accounting.
- **`forge model list`** (`crates/forge-cli/src/commands/model_cmd.rs:64-70`)
  prints `streaming=true` for `claude-sonnet` after Task 4 — truthful for
  the first time; no test pins the old value (checked).
- **D7's client split** leaves `complete()` on the 120 s total deadline
  deliberately: a buffered response that hasn't finished in 120 s is stuck,
  not slow — the two failure shapes deserve different bounds.
- **Manual verification after landing** (not a gate): `forge chat` against
  a live oMLX with `-v` shows `assistant_delta` events in the session log
  during a turn; TICKET-3 makes them visible in the transcript.
- The 09-25 design doc's "Provider support" table
  (`2026-09-25-streaming-design.md:104-113`) is fully implemented by this
  ticket; Task 5 records that and the callback-contract deviation in the
  doc itself.

## AMENDMENTS

(none yet)
