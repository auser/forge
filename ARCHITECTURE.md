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
  `needle.h` against the declarations). The engine is resolved in three steps:
  `NEEDLE_LIB_DIR` → `vendor/<target>/` → download at build time against a
  pinned SHA-256, the last only under the `ffi` feature, so a default build
  never reaches for the network. Downloads are cached by content hash, so
  once per machine. `NEEDLE_NO_DOWNLOAD=1` opts out (offline/packaging);
  `NEEDLE_REQUIRE_ENGINE=1` turns "no engine" from a warning into a build
  failure, which is how release builds guarantee a brain-labelled binary has
  one. Linked only with the `ffi` feature.
- The engine's C++ runtime is chosen from the **artifact**, not the OS: every
  published `libneedle.a` is clang/libc++ (`_ZNSt3__1…` undefined symbols, no
  libstdc++ `__cxx11`), so Linux links libc++ — statically, plus the `-L` that
  `rustc`'s `static=` lookup needs, so a release asset keeps the same runtime
  dependencies as a brain-less build. `NEEDLE_CXX_RUNTIME` overrides it.
  Choosing by OS instead is what broke the first brain-enabled Linux build.
  Linkability is per *architecture*, not per OS: the x86_64 archives need a
  libc++ symbol no distribution ships, so only the arm64 engines are pinned.
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
- **One story about why the brain is off.** Two distinct causes, never
  conflated: `BackendError::EngineMissing` ("no engine in this build", fixed by
  a reinstall) and `BackendError::WeightsMissing` ("no weights on disk", fixed
  by `forge init`). `forge-needle::ENGINE_REMEDY` is the single string the
  first case quotes everywhere it surfaces — a failed route, `forge init`'s
  skip note, and `forge doctor`'s `needle engine`/`needle brain` line-pair — so
  the three cannot drift into telling a user two incompatible things (which is
  exactly what they did: init said "build with the feature", the router said
  "run `forge init`", and neither exit was reachable from the other).

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
prunes the tier entirely (unconditionally, even for a loopback OpenJev;
`http`/`laya` are pruned by endpoint instead — see the generation plane's
`local_only` note). `JevRouter` never fabricates a decision — any
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

**`local_only` is enforced here, at provider construction.**
`model_from_config` is the one place a configured (or *routed*) model name
becomes a client, so it is the one place the restriction can actually hold:
with `local_only` set, an endpoint that is not on this machine — loopback,
`localhost`, or a hostless `unix:`/`file:` socket path; deliberately not
private-range LAN addresses — yields a typed `ForgeError::Config` naming the
model, the URL and the config field that set it, and no client is built.

Checking the configured string is necessary but not sufficient, so an
`EgressPolicy` travels with every HTTP client forge builds: under
`local_only` its redirect policy re-checks each hop, because reqwest's
default (`Policy::limited(10)`) has no host restriction and a `307` from an
approved loopback endpoint would otherwise re-POST the prompt verbatim to an
authority nothing inspected. A custom policy replaces that default whole, so
it re-imposes the 10-hop bound too: locality alone would follow a loopback
server that redirects to itself until the request timed out.

Provider construction also refuses combinations that cannot work at all — an
`anthropic`-family model whose endpoint already ends in `/v1` (the client
appends `/v1/messages`), or an entry whose declared `provider` the endpoint
contradicts — naming both settings rather than building a client whose only
symptom is a 404. The decision plane uses the same
`endpoint_is_local` predicate to prune off-device routers, so "local" has one
definition (`forge_providers::local_only`) and `forge doctor` reports it
rather than restating it.

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
forge-graph       deterministic project graph + embeddings index format;
                  `query` = the one ranked-context/semantic-search
                  implementation, taking an Embedder the caller built
forge-skills      SKILL.md discovery, progressive disclosure
forge-session     append-only JSONL event store, secret redaction —
                  the harness's memory: `forge resume` reconstructs the
                  model conversation from it (forge-runtime::replay)
forge-server      axum REST/SSE adapter over the same AgentService
forge-mcp         Model Context Protocol (stdio) adapter over the same
                  AgentService: tool registry + schemas + dispatch
forge-acp         Agent Client Protocol (stdio) adapter over the same
                  AgentService: forge *as the agent* — v1 wire types,
                  pure event→update dispatch, prompt-turn driver
forge-chat        interactive chat: pure slash parsing, event->transcript
                  rendering and the input state machine, over ChatIo /
                  ChatHost seams the CLI implements (no terminal here)
forge-cli         clap command tree, doctor, init, the forge binary
```

## Editors and harnesses

`forge mcp` is the stdio sibling of `forge serve`: the same `AgentService`,
built by the same `build_run_service` path (needle seam included), exposed as
MCP tools instead of HTTP routes. Editors and agent harnesses (Claude Code,
VS Code, Cursor) launch it as a subprocess and get the project graph
(`forge_graph_*`), skills (`forge_skill_*`), the doctor report, and the agent
loop itself (`forge_run*`) as tools — so another agent can use forge's
project intelligence without reimplementing any of it.

Three properties are load-bearing. **stdout is the protocol channel**, so
every diagnostic goes to stderr and nothing in the process may print.
**stdin is too**, which means the approval path cannot prompt: a risky
operation under `approval = "prompt"` parks the run
(`status: "waiting_for_approval"`) and the client answers with
`forge_run_input` — the same pause/deliver mechanism the REST adapter's
`POST /v1/runs/:id/input` uses. And **protocol work is the SDK's**: `forge-mcp`
wraps the official `rmcp` crate, which serves both the `initialize`-handshake
revisions (`2025-11-25` and earlier) and the current stateless `2026-07-28`
revision (per-request `_meta`, mandatory `server/discover`) from one process.

Doctor is the one capability the adapter cannot reach downward for: its checks
span providers, credentials, graph, skills and needle weights, a combination
only `forge-cli` sees. So `forge-mcp` declares a `Diagnostics` seam and the CLI
implements it from `commands::doctor::collect_checks` — one definition of
"healthy" for `forge doctor`, `forge doctor --json` and the `forge_doctor`
tool, and never a subprocess.

`forge acp` is the *other* half of that story, and the distinction is worth
stating precisely: **MCP exposes forge's capabilities as tools for someone
else's agent; ACP exposes forge as the agent.** Same `AgentService`, same
`build_run_service`, same stdio discipline — a different question being
answered. An ACP client (Zed and friends) drives `initialize` → `session/new` →
`session/prompt`, and gets the turn back as `session/update` notifications:
tool calls with kinds, statuses and file locations, routing decisions as
thoughts, and the final text as one `agent_message_chunk`.

Three seams carry it. **The ACP session id *is* the forge session id**, so a
turn driven from the editor is inspectable with `forge session show <id>` and
continuable with `forge resume <id>` — no second identity space. **The client
chooses the project root per session** (`session/new`'s `cwd`), so the runtime
is built per session rather than per process: `forge-acp` declares a
`ServiceFactory` seam and the CLI implements it by overriding `--project`,
which routes the choice through the same config discovery and needle probe as
every other subcommand. And **approval becomes a protocol request**: stdin is
the protocol channel, so the parked-run mechanism that MCP answers with
`forge_run_input` is answered here by `session/request_permission` —
`ApprovalRequested` → a permission request naming the tool call → the chosen
option mapped back to `send_input("y"/"n")`. The result is a permission prompt
rendered by the editor, on the tool call it belongs to.

Unlike `forge-mcp`, this adapter does **not** wrap an SDK. The official
`agent-client-protocol` crate was evaluated and rejected on cost, not
capability: 52 new transitive crates (against `rmcp`'s 13), including a second
async reactor beside tokio and two more datetime libraries beside chrono, to
supply a dozen message types and one newline-delimited JSON-RPC loop. So
`forge-acp::protocol` carries the v1 types, transcribed from the authoritative
schema crate, and `forge-acp::dispatch` keeps every protocol decision pure —
which is both why the whole forge→ACP mapping is unit-testable without a
process, and where an SDK would be dropped in if the subset stops keeping up.
The crate docs record the verification evidence.

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

**Continuing a session** runs the same path with one difference: before the
loop starts, `forge-runtime::replay` reads the session's log and rebuilds the
model conversation from it — `run_started` prompts, `assistant_message`
records (text + tool calls, verbatim), `tool_result` records — across every
prior run of the session, fitted to a character budget derived from the
model's context window. This is not special to `forge resume`: every entry
point that names an existing session (`run_with_options`,
`start_run_with_options`, and so `POST /v1/runs`, `forge_run`, and each ACP
turn) continues the conversation it names. A fresh session replays nothing.

The log is therefore not just a trace: it is the only place the conversation
lives between runs, which is why the v3 replay events are written even though
no adapter displays them. Reconstruction also *repairs* the conversation — a
run that died between announcing a tool call and recording its result leaves a
call with no answer, and every chat API rejects that — so replay synthesizes
the missing result rather than emitting a dangling call.

Because the store is now read back into the model's context, `SessionStore::append`
returns **the redacted event it wrote**, not the one it was handed: the runtime
broadcasts and collects whatever `append` returns, so anything less would let
secrets reach SSE subscribers and `--json` outcomes while the log on disk
stayed clean. One redaction, at one boundary, for every consumer.

`forge session fork` branches a session by copying its log prefix, so two
conversations can continue from one shared past without either being able to
disturb the other.

Every run's in-memory tracking — input channel, broadcast sender,
cancellation token — is keyed by run id and **pruned when the run reaches a
terminal state** (`AgentService::finish_run`), with a bounded tombstone so a
pruned run is still recognisably finished. What a caller can still want about
a finished run comes from the session store instead: `attach(run_id)` returns
its whole backlog, and a live run additionally gets a gap-free, duplicate-free
stream (subscribe first, read the log second, filter the overlap by `seq`).
`RunState` in `forge-core` is the one typed discriminant the ACP and MCP
adapters classify a run's ending by.

## Failure ladder (what never breaks)

| Missing / failing            | Behavior                                            |
|------------------------------|-----------------------------------------------------|
| Needle weights absent        | static routing, `fallback_used: true`, run proceeds |
| Needle low confidence        | escalate to Jev (if credentialed) else static       |
| Jev unreachable / no key     | static routing, recorded, run proceeds              |
| Local model server down      | typed error with doctor-style hint                  |
| Cloud credential absent      | cloud candidates simply don't exist                 |
| `--local-only`               | off-device routers degrade, non-local providers refused |
| Engine panic / hung call     | contained; timeout-bounded; degrade to fallback     |

`forge doctor` reports each layer's actual state (weights, backend presence,
credential detection, endpoints) without ever failing the check for a
degradable condition.

## What's next (per the program spec)

The ACP adapter (the other half of editor interop), the interactive TUI with
slash commands / history replay / fork & background, and in-process
generation (`forge-llm-embedded`) — see
[`docs/superpowers/specs/2026-09-23-needle-embedded-brain-design.md`](docs/superpowers/specs/2026-09-23-needle-embedded-brain-design.md) §2.
