# Streaming model output

**Status:** design, awaiting implementation plan
**Date:** 2026-09-25
**Scope:** sub-project F. Required by the REPL (B) and editor integration (E);
independent of the Session core's decision plane.

## The problem

`ModelProvider` has exactly one method for generation:

```rust
async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, ForgeError>;
```

Request in, whole response out. `ModelCapabilities` advertises
`streaming: bool` and `ModelEntry` lets an operator set it, but **nothing can
consume it** — there is no streaming API to call.

The measured consequence: a local Qwen3-Coder turn takes 6.5–7.5 s. Today that
is 7 seconds of silence followed by a wall of text. Acceptable in a terminal you
have learned to wait on; unacceptable in an editor, and the single largest
perceived-latency problem forge has that the decision plane does not fix.

Worth being precise about that last point. The Session core makes forge *faster*
— skipping the model entirely on dispatched turns. Streaming does not make
anything faster; it makes the remaining slow path *feel* fast. Both matter, and
neither substitutes for the other.

## Design

### The API

Add a second method rather than changing `complete`, so providers that cannot
stream stay simple and callers that do not care are unaffected:

```rust
pub enum CompletionDelta {
    /// Assistant prose, incremental.
    Text(String),
    /// A tool call became fully formed. Emitted once per call, not per token:
    /// a half-parsed argument object is not useful to anyone downstream.
    ToolCall(ToolCall),
    /// Terminal. Carries the assembled response so callers that want the whole
    /// thing do not have to reassemble it themselves.
    Done(CompletionResponse),
}

pub trait ModelProvider: Send + Sync {
    fn name(&self) -> &str;
    fn capabilities(&self) -> ModelCapabilities;

    async fn complete(&self, request: CompletionRequest)
        -> Result<CompletionResponse, ForgeError>;

    /// Default: call `complete` and emit a single `Done`. A provider that cannot
    /// stream is therefore correct without implementing anything.
    fn complete_streaming(&self, request: CompletionRequest)
        -> BoxStream<'_, Result<CompletionDelta, ForgeError>>;
}
```

The defaulted method is what keeps this from being a breaking change across
every provider, and it means `capabilities().streaming` becomes a statement
about *quality* — whether deltas arrive incrementally — rather than about
whether the call is legal.

### Tool calls are not streamed token-by-token

A partially parsed `{"path": "src/ma` is worse than nothing: it cannot be
gated, cannot be dispatched, and cannot be previewed in a diff. So tool calls
buffer until complete and are emitted whole. Text streams; structure does not.

This also keeps the gate's contract intact — `ApprovalRequest` (core §18)
requires a complete `ToolCall` to build a preview from.

### Where deltas go

Forge already has an event stream: `Event` / `EventKind`, broadcast per run, and
already consumed by `forge-mcp` and `forge-server`. Streaming text becomes
another event kind rather than a parallel channel, so every surface that already
subscribes gets it for free and ordering against tool events is preserved.

### Cancellation

The `Session`'s existing `CancellationToken` must abort an in-flight stream, not
merely stop reading it — an abandoned HTTP body still costs tokens on a metered
provider. Dropping the stream must close the connection.

This matters more with streaming than without: a user who sees the first
sentence going the wrong way will cancel, and that is exactly the case where
forge should stop paying for the rest.

### Interaction with the fast path

A turn that needle dispatches (core §1) produces no model call and therefore no
stream. The surface shows the tool result directly.

That asymmetry is worth surfacing in the UI rather than hiding: a dispatched
turn completing in ~1.1 s with no streamed text is forge working *well*, and it
should not look like a stall. The REPL and the editor extension should indicate
"answered on-device" explicitly.

## Provider support

| Provider | Mechanism | Notes |
|---|---|---|
| Anthropic | SSE | `anthropic.rs` |
| OpenAI-compatible | SSE, `stream: true` | Covers oMLX, llama.cpp, vLLM, Moonshot, and the rest of `model.rs` |
| Scripted / mock | synthesized deltas | Needed so tests can assert streaming without a network |

`model.rs` already reads a per-entry `streaming` override, so an endpoint that
claims SSE and does not deliver can be turned off without code changes.

## Testing

- **Delta ordering** — text deltas arrive before the `ToolCall` that follows
  them, and `Done` is always last and always present, including on the
  non-streaming default path.
- **The default implementation** — a provider that implements only `complete`
  must yield exactly one `Done` and nothing else.
- **Cancellation closes the connection**, asserted against a `wiremock` server
  that observes the disconnect rather than inferring it.
- **Partial tool calls never escape** — a provider emitting a tool call across
  several SSE frames yields exactly one `ToolCall` delta, never a fragment.
- **Mid-stream errors** surface as a stream error and leave the session usable
  for the next turn.

## Open questions

- **Reasoning/thinking blocks.** Some providers stream a separate reasoning
  channel. Needle 3 also produces a `<think>` trace. Whether these become a
  distinct `CompletionDelta` variant or are folded into `Text` is unresolved,
  and the answer should be the same for both so surfaces have one rule.
- **Token accounting.** Budget enforcement (core §19) needs usage numbers, and
  some providers only report them in the final SSE frame. A cancelled stream may
  therefore have consumed tokens forge cannot account for.
- **Retry semantics.** A stream that fails halfway has already shown the user
  text. Retrying duplicates it; not retrying loses the turn. Probably: surface
  the failure, never silently retry a partially-shown response.
