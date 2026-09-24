# Forge Architecture

How Needle, Jev, and LLM providers (local and cloud) fit together into one
harness. This document describes what is implemented today; design direction
lives in [`docs/superpowers/specs/`](docs/superpowers/specs/) and
[`specs/roadmap.md`](specs/roadmap.md).

## The two planes

Forge separates **deciding** from **generating**. Small, fast, cheap models
make decisions; large models generate text and code. Neither plane depends on
any single vendor, and every tier degrades gracefully to the one below it.

```
              DECISION PLANE                        GENERATION PLANE
   (which model? is this call safe?           (the model that actually
    which tool? which skill?)                  writes code and text)

  ┌───────────────────────────────┐        ┌──────────────────────────────┐
  │ 1. Needle 3 — embedded        │        │ 1. Local model (free)        │
  │    in-process, on-device      │        │    any OpenAI-compatible     │
  │    ~ms, free, offline         │ ─────▶ │    server: oMLX, llama.cpp,  │
  │    (forge-needle + needle-sys)│ selects│    Ollama, LM Studio…        │
  ├───────────────────────────────┤        ├──────────────────────────────┤
  │ 2. Jev protocol — opt-in      │        │ 2. Cloud models (opt-in)     │
  │    TypeSafe hosted Jev, or    │        │    candidates exist only     │
  │    self-hosted OpenJev        │        │    when a credential is      │
  │    (~70–500 ms, credential-   │        │    detected: API keys or     │
  │    gated)                     │        │    CLI subscriptions (Claude │
  ├───────────────────────────────┤        │    Code OAuth, Codex, Kimi)  │
  │ 3. Static rules — always      │        └──────────────────────────────┘
  │    succeeds, deterministic,   │
  │    fully offline              │
  └───────────────────────────────┘
```

The positioning this buys: **cheapest, fastest, local-first** — free embedded
decisions, free local generation, subscriptions you already pay for next,
metered API keys last — with automatic, transparent fallback up the ladder
only when a task demands it. Every escalation is recorded in the session log
with a reason; nothing leaves the device silently.

## Decision plane

### Tier 1: Needle 3, embedded

[Needle 3](https://huggingface.co/Cactus-Compute/needle3) (Cactus Compute,
Apache-2.0) is a ~35 MB on-device model that does three things well: pick and
fill tool calls, extract structured data, and produce embeddings. It cannot
generate text — which is exactly why it is safe to run on every request.

- `needle-sys` — six hand-written `extern "C"` declarations against
  `libneedle` (no bindgen, no libclang; a unit test pins the committed
  `needle.h` against the declarations). Built only with the `ffi` feature.
- `forge-needle` — the safe layer. `NeedleEngine` owns the model on one
  dedicated OS thread (mpsc jobs, oneshot replies): lazy load, panic
  containment (`catch_unwind`), abandoned-job skip (a caller that timed out
  doesn't waste engine time), and sticky-vs-retryable load failures (missing
  weights retry — they may appear after `forge init`; anything else fails
  fast). One engine is shared per process (router, fast path, and graph
  indexing all use the same cached instance).
- Weights are fetched once by `forge init` (SHA-256-pinned, atomic rename,
  refetch-once; `[needle] weights_sha256` lets an operator pin their own).
  Builds without the `ffi` feature skip the fetch and say so.

Needle answers four kinds of question in forge:

1. **Routing** — which model should handle this task (`NeedleRouter`,
   implementing the `DecisionRouter` trait).
2. **Guardrails** — is this tool call safe to run without waking a model
   (the fast path's safe/risky decide).
3. **Tool filling** — for well-defined prompts, pick the tool and fill its
   arguments directly (the direct-dispatch fast path).
4. **Embeddings** — the semantic graph index (`Embedder` trait).

### Tier 2: the Jev protocol (hosted or self-hosted)

When Needle declines or scores below `router_confidence_threshold`, forge can
escalate the *decision* (never the content-generation) to a System One
service speaking the Jev wire protocol:

- **TypeSafe's hosted Jev** — the reference implementation
  (`https://api.typesafe.ai/v1/systemone`, bearer `TYPESAFE_API_KEY`).
- **Self-hosted OpenJev** — wire-compatible open servers
  ([razorback16/openjev](https://github.com/razorback16/openjev),
  [GitHub30/OpenJev](https://github.com/GitHub30/OpenJev)); point `jev_url`
  (and, if needed, `jev_key_env`) at yours and escalation stays on your
  infrastructure. Deliberately **not** `router_url`/`router_key_env`: those
  belong to the `http`/`laya` routers, and the escalation tier never
  consults them — a leftover value from an unrelated router setup must
  never be able to receive the Jev credential or redirect it elsewhere.

Escalation is **opt-in by credential**: `router_escalate = "auto"` (the
default) does nothing until the key env var is set, and `--local-only`
prunes the tier entirely. `JevRouter` never fabricates a decision — any
error, timeout, or unknown choice is an `Err` that falls through.

### Tier 3: static rules

`StaticRouter` — deterministic keyword rules and a default model. It cannot
fail, needs no network, no weights, no account. Every decision that reaches
it is marked `fallback_used: true` in the session events, so degradation is
visible, never silent.

### Composition

The tiers are not special-cased; they are composed from two generic
combinators in `forge-providers`:

```
FallbackRouter(
    ThresholdRouter(NeedleRouter, confidence_threshold),   // tier 1
    FallbackRouter(
        ThresholdRouter(JevRouter, confidence_threshold),  // tier 2 (iff credential)
        StaticRouter,                                      // tier 3
    ),
)
```

`ThresholdRouter` rejects low-confidence decisions; `FallbackRouter` catches
rejections and errors. `router = "laya"` / `"http"` / `"cheapest"` / `"mock"`
slot into the same seam — the ladder is configuration, not architecture.

## Generation plane

The `ModelProvider` trait abstracts text/code generation. Selection between
providers is the decision plane's job; the registry defines the candidates:

- **Local first.** The default model is whatever OpenAI-compatible server
  you run (`model_base_url`): oMLX, llama.cpp's `llama-server`, Ollama,
  LM Studio. Cost 0 in the registry; the router prefers it unless the task
  demands capabilities it lacks.
- **Cloud, credential-gated.** Entries in the `[models]` registry carry
  costs and capabilities (tools, streaming, context size). A hosted entry
  becomes a routing *candidate* only when its credential resolves — an API
  key (`key_env`, conventional env vars) **or** an existing CLI subscription
  (Claude Code OAuth token, Codex CLI auth, Kimi/Moonshot). No credential →
  the candidate does not exist. Hosted models are never called implicitly:
  a router selects them (with a recorded reason) or you set `model`
  explicitly.

Capabilities are declared, never assumed: `Capability::satisfied_by` filters
candidates before any router sees them, so a vision task never routes to a
text-only model and a tool-needing loop never selects a provider without
tool support.

## The direct-dispatch fast path

The payoff of an embedded decision model: for well-defined, read-only
requests, forge answers **without any LLM at all**. On a fresh prompt,
Needle attempts a tool call; it dispatches only when *all* gates hold:

1. the model in play is tool-capable (fast path never exceeds what a normal
   run could do),
2. Needle proposed a call with confidence ≥ threshold,
3. the operation's risk classification is `Safe` (read-only — writes,
   edits, deletes, and anything that could require approval always go to
   the full loop),
4. the guardrail decide agrees it's safe, and
5. the arguments parse as a JSON object.

Any gate failing falls through silently to the normal agent loop. Dispatch
goes through the same `ToolDispatcher`/`ExecutionProvider` path as the loop,
so approval policy and risk classification are enforced by construction, and
the session log shows the same event sequence (`routing_decision_made` with
`router_name: "needle-dispatch"`, `tool_*`, `completed`).

## Semantic graph

`forge graph build` embeds every symbol (via the `Embedder` trait — Needle's
embeddings when the engine is available, skipped silently otherwise) into
`.forge/graph/embeddings.bin`, incrementally by content hash. `graph grep
--semantic` and `graph context` blend lexical and cosine scores. The graph
crate itself stays model-free: the index is pure data; embedding happens in
the CLI layer. The index fully rebuilds if the embedding model or dimensions
change (the header records both).

## Crate map and trait seams

```
forge-core        traits + events: ModelProvider, DecisionRouter,
                  ExecutionProvider, Embedder, SkillRegistry, ProjectGraph,
                  SessionStore; typed errors; risk classification
forge-config      config loading, precedence, provenance, [models]/[needle]
forge-providers   model providers (OpenAI-compatible, Anthropic, mocks) and
                  routers (needle, jev, laya, http, static, cheapest, mock
                  + Threshold/Fallback combinators)
forge-needle      NeedleEngine, backends (ffi/hash/unavailable), weights,
                  NeedleRouter, EngineEmbedder, engine cache
needle-sys        raw FFI declarations + link configuration (feature ffi)
forge-execution   native + mock execution; approval gating lives here
forge-runtime     AgentService: the one runtime (agent loop, fast path,
                  tool dispatch) shared by CLI and server
forge-graph       deterministic project graph + embeddings index format
forge-skills      SKILL.md discovery, progressive disclosure
forge-session     append-only JSONL event store, secret redaction
forge-server      axum REST/SSE adapter over the same AgentService
forge-cli         clap command tree, doctor, init, the forge binary
```

Every integration in this document sits behind one of the `forge-core`
traits. That is the load-bearing design decision: Needle could be replaced
by a different embedded model, Jev by a different decision API, oMLX by any
generation server — each is an adapter, none is a foundation.

## Life of a run

```
forge run "explain the parser"
  │
  ├─ config resolve (defaults → files → env → flags) + credential detection
  ├─ AgentService.start_run
  │    ├─ events: run_started, routing_decision_made (needle → jev → static)
  │    ├─ skills matched, graph context seeded
  │    ├─ FAST PATH? (fresh prompt + engine + gates) ── yes ─▶ dispatch tool,
  │    │                                                       events, done
  │    └─ no ─▶ agent loop on the selected ModelProvider
  │              └─ per tool call: risk classification → approval policy
  │                 → ExecutionProvider → events
  └─ every event appended to .forge/sessions/<id>.jsonl (redacted, replayable)
```

## Failure ladder (what never breaks)

| Missing / failing            | Behavior                                            |
|------------------------------|-----------------------------------------------------|
| Needle weights absent        | static routing, `fallback_used: true`, run proceeds |
| Needle low confidence        | escalate to Jev (if credentialed) else static       |
| Jev unreachable / no key     | static routing, recorded, run proceeds              |
| Local model server down      | typed error with doctor-style hint                  |
| Cloud credential absent      | cloud candidates simply don't exist                 |
| `--local-only`               | every network tier pruned at construction           |
| Engine panic / hung call     | contained; timeout-bounded; degrade to fallback     |

`forge doctor` reports each layer's actual state (weights, backend presence,
credential detection, endpoints) without ever failing the check for a
degradable condition.

## What's next (per the program spec)

ACP and MCP adapters (editors and harness interop), the interactive TUI with
slash commands / history replay / fork & background, and in-process
generation (`forge-llm-embedded`) — see
[`docs/superpowers/specs/2026-09-23-needle-embedded-brain-design.md`](docs/superpowers/specs/2026-09-23-needle-embedded-brain-design.md) §2.
