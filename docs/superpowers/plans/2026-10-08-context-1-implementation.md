# Feature: CONTEXT-1 deterministic context plans

The following plan is complete, but implementation must still validate code
patterns and task sanity before each change.

## Feature description

Record a deterministic, provider-neutral context plan for every generation
request without changing any request bytes or model behavior. Plans explain
where the context budget went, identify static-prefix drift, and provide the
storage/event contracts that reversible artifacts and observational memory
will extend.

## User story

As a Forge user debugging a long or expensive agent run,  
I want to inspect exactly what categories consumed each model request and
whether its stable prefix changed,  
so that context regressions are measurable instead of inferred from model
behavior.

## Problem statement

Forge budgets replay history, injects system guidance, activated skills, graph
context, prompts, tool schemas, and active-turn tool messages, but does not
record their individual sizes or stable-prefix identity. Provider usage events
show aggregate tokens only after completion. There is therefore no way to
explain a bloated request, detect prompt-cache drift, or establish a stable
contract for later compression and memory work.

## Solution statement

Add a `forge-context` crate containing versioned context-plan data, stable hash
and size estimation, and a dedicated `ContextStore` abstraction with local
filesystem and in-memory implementations. Inject the store into
`AgentService` through a builder so existing constructors remain compatible.
Immediately before every completion request, build and persist a plan from the
exact messages and tool definitions being sent. Emit a compact recorded or
unavailable event, and continue unchanged when accounting fails.

## Out of scope / non-goals

- No tool-output compression or retrieval handles (CONTEXT-2).
- No content classifier or compressor (CONTEXT-3).
- No observations, memory injection, or altered replay policy (CONTEXT-4+).
- No provider tokenizer dependency; use a conservative character estimate.
- No mutation, reordering, normalization, or omission of request content.
- No Headroom dependency, process, proxy, MCP backend, or compatibility seam.
- No user-global or cross-project state.
- No automatic repair of prefix drift; report it only.

## Feature metadata

**Feature type:** New capability / observability foundation  
**Estimated complexity:** Medium-high  
**Primary systems affected:** `forge-context` (new), `forge-runtime`,
`forge-core` events, `forge-cli` service/session inspection  
**Dependencies:** Existing workspace `serde`, `serde_json`, `sha2`, `chrono`;
no external service or new third-party crate

## Related work

**Implements:** [CONTEXT-1 / #38](https://github.com/auser/forge/issues/38)  
**Epic:** [#37](https://github.com/auser/forge/issues/37)

**Back-references**

- `docs/superpowers/specs/2026-10-08-context-engine-design.md` — accepted
  architecture and security boundaries.
- `docs/superpowers/plans/2026-10-08-context-engine-tickets.md` — dependency
  graph and acceptance criteria.
- `crates/forge-mcp/src/compact.rs` — precedent that accounting must measure
  the actual advertised tool surface, not the full registry.

**Forward-references**

- CONTEXT-2 will reuse `ContextStore`, plan IDs, source anchors, and
  accounting.
- CONTEXT-4 will add the currently zero-valued memory component.

---

## Context references

### Relevant codebase files

- `crates/forge-runtime/src/service.rs` around request assembly and
  `complete_streaming` — both the no-tools and iterative tool-loop request
  paths must record plans.
- `crates/forge-runtime/src/replay.rs` — current character/token approximation
  and replay-budget semantics.
- `crates/forge-core/src/model.rs` — exact provider-neutral `Message`,
  `ToolDefinition`, and `CompletionRequest` structures to hash and size.
- `crates/forge-core/src/events.rs` — versioned tagged event schema and
  backward-compatible defaults.
- `crates/forge-core/src/session.rs` — precedent for injected persistence
  traits; context storage remains a separate trait.
- `crates/forge-session/src/store.rs` — redaction, append, atomicity, and
  filesystem-test patterns. Context plans must never bypass provider-safe
  inputs, though CONTEXT-1 stores only sizes/hashes.
- `crates/forge-cli/src/commands/service.rs` — production runtime composition;
  attach the local context store here.
- `crates/forge-cli/src/commands/session_cmd.rs` — human-readable event
  inspection.
- `crates/forge-runtime/src/service/tests.rs` — scripted multi-request runs and
  request recording patterns.
- `crates/forge-cli/tests/cli.rs` — compiled-binary `.forge/` and JSON/session
  assertions.

### New files to create

- `crates/forge-context/Cargo.toml`
- `crates/forge-context/src/lib.rs` — public contracts and exports.
- `crates/forge-context/src/plan.rs` — component sizes, stable-prefix hashes,
  drafts, and recorded plans.
- `crates/forge-context/src/store.rs` — `ContextStore`, `FsContextStore`, and
  in-memory test store.

### Relevant documentation

- Rust `std::fs::rename` documentation — atomic replacement expectations and
  platform differences.
- `sha2` workspace usage in graph/session code — hashing convention.
- Accepted context-engine architecture, especially data model, security,
  prompt-cache stability, and rollout principles.

### Patterns to follow

**Constructor compatibility**

`AgentService::new` is used widely. Keep it unchanged and add:

```rust
service.with_context_store(Some(store))
```

The default remains no context store for focused unit adapters; every
production CLI runtime receives the local store.

**Fail-open observability**

Plan failure must follow best-effort catalogue/diagnostic behavior, not tool or
session-log failure behavior:

```rust
match context_store.record(draft) {
    Ok(plan) => emit(ContextPlanRecorded { ... }),
    Err(error) => {
        tracing::warn!(%error, "context plan unavailable");
        emit(ContextPlanUnavailable { ... });
    }
}
```

The unchanged request is sent in either branch.

**Provider-neutral exactness**

Hash canonical `serde_json` serialization of the exact ordered system-message
slice and exact ordered offered tool definitions. Do not hash registries,
catalogues, unsupported tools, timestamps, session IDs, graph context, skills,
replay, or the current prompt into the stable prefix.

**Per-request identity**

Use:

```text
<run-id>/<request-ordinal>.json
```

Ordinals start at 1 and increment for every provider completion attempt within
the run, including tool-loop continuations.

---

## Implementation plan

### Task 1 — Add the `forge-context` workspace crate and plan model

Create the crate and workspace dependency. Define:

- `CONTEXT_PLAN_VERSION = 1`;
- `ContextSize { chars, estimated_tokens }`;
- `ContextComponents` for system guidance, skill instructions, graph context,
  replay/active history, memory, current prompt, tool schemas, and total;
- `StablePrefix` with system hash, tool hash, combined hash, and
  `changed_from_previous`;
- `ContextPlanDraft` and `ContextPlan`;
- `ContextPlanSummary` for session events.

Use the existing four-characters-per-token estimate and saturating arithmetic.
Serialize with explicit version fields and stable snake-case names.

Tests:

- exact deterministic size arithmetic;
- UTF-8 uses character counts consistently rather than byte slicing;
- serialization round trip;
- empty memory component is represented honestly as zero.

### Task 2 — Implement exact stable-prefix hashing

Add helpers that accept the exact static system messages and exact offered
tools.

- Serialize each surface canonically through its typed `serde` representation.
- Preserve ordering; reordering tools must change the tool/combined hashes.
- Dynamic messages must not affect the stable hash.
- Prefix hashes contain no original prompt text.
- Hash input includes an internal version/domain separator to make future
  changes explicit.

Tests:

- same inputs yield byte-identical hashes;
- content or order changes alter only the expected hashes;
- dynamic history/prompt changes leave stable hashes unchanged;
- empty tool surface hashes deterministically.

### Task 3 — Add dedicated context stores

Define synchronous:

```rust
pub trait ContextStore: Send + Sync {
    fn record(&self, draft: ContextPlanDraft) -> Result<ContextPlan, ForgeError>;
    fn plan(&self, run_id: &str, ordinal: u32) -> Result<Option<ContextPlan>, ForgeError>;
}
```

`record` owns prefix-change comparison and persistence so it can make the
operation atomic under one lock.

`FsContextStore`:

- root is `.forge/context`;
- writes plans beneath `plans/<run-id>/<ordinal>.json`;
- keeps a small per-session latest-prefix record beneath `latest/`;
- discovers the prior hash after process restart;
- writes temporary files then replaces;
- rejects path components not derived from validated Forge IDs;
- creates parent directories and user-only permissions where supported.

`MemoryContextStore`:

- deterministic and cloneable for runtime tests;
- supports injected failure for fail-open coverage.

Tests:

- first plan reports no prior change;
- same prefix remains unchanged;
- changed prefix is reported;
- behavior survives constructing a fresh filesystem store;
- concurrent records do not produce partial JSON;
- malformed prior state degrades to a typed store error;
- stored plans contain sizes/hashes only, never message text.

### Task 4 — Add versioned plan events

Bump the event schema version and add:

```text
context_plan_recorded
context_plan_unavailable
```

Recorded event fields:

- plan ID;
- request ordinal;
- stable-prefix combined hash;
- prefix-changed flag;
- total estimated input tokens;
- optional explicit reserved output tokens;
- project-relative plan path.

Unavailable event fields:

- request ordinal;
- sanitized error category/message.

Update:

- event serialization and backward-compatibility tests;
- session command formatting;
- any exhaustive render/replay matches (replay ignores these events);
- BDD/session fixtures if schema assertions require the new version.

Do not place full component plans in JSONL.

### Task 5 — Account for actual request components without changing requests

Refactor request assembly only enough to retain component boundaries:

- base system guidance;
- activated skills;
- graph context;
- replay plus active-turn assistant/tool history;
- memory (zero in CONTEXT-1);
- original current prompt;
- exact offered tools.

Create the `CompletionRequest` exactly as today. Build the draft from borrowed
request/component data immediately before calling the provider.

For iterative tool runs:

- increment request ordinal per completion;
- count newly appended assistant/tool messages as active/replay history;
- do not double-count the original current prompt;
- use the routed provider's actual tool capability and offered tool list.

Reserved output:

- record `request.max_tokens` when explicit;
- otherwise record `None` as provider-default/unknown;
- also record remaining estimated context headroom, without inventing a
  provider output limit.

Stable prefix:

- system hash uses only `self.system_context`;
- tools hash uses `request.tools`;
- skills and graph context remain dynamic components.

### Task 6 — Persist and emit plans fail-open

Add optional context store state and a `with_context_store` builder to
`AgentService`.

Before each provider request:

1. build draft;
2. ask store to record;
3. emit recorded/unavailable event;
4. send the original request unmodified.

The event itself still uses the authoritative `SessionStore`, so a context
store failure is visible. If emitting the event fails, preserve existing
session-store failure semantics; do not special-case authoritative event-log
failure.

Production CLI composition attaches:

```text
.forge/context/
```

All frontends using the shared service automatically receive plans.

### Task 7 — Add inspection and end-to-end coverage

Update `forge session show` to print concise plan events. The event's relative
path allows direct JSON inspection; no new CLI command is required in this
ticket.

Add runtime tests proving:

- a one-shot completion records exactly one plan;
- a tool-call/response cycle records one plan per provider request with
  ordinals 1 and 2;
- recorded model requests are exactly equal to the pre-feature expected
  requests;
- tool schemas are absent when the selected provider lacks tool support;
- store failure emits unavailable and the run still completes;
- dynamic prompt/history changes do not report stable-prefix drift;
- actual system/tool changes do report drift.

Add compiled CLI coverage:

- fresh project creates `.forge/context/plans/...`;
- plan JSON contains no prompt or tool-result content;
- `session show` names the plan event;
- JSON run output remains valid and includes only the compact event summary.

### Task 8 — Documentation and validation

Update reference documentation with:

- accounting is always on in production;
- location and rebuildable nature of plans;
- stable-prefix definition;
- fail-open behavior;
- no compression or memory behavior change yet.

Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
NEEDLE_NO_DOWNLOAD=1 cargo clippy -p forge-needle --features needle-e2e --all-targets -- -D warnings
cargo test --workspace
bash tests/install-path.sh
cargo test -p forge-cli --test bdd
git diff --check
```

## Testing strategy

### Unit

- Context size and hash determinism.
- Store atomicity, restart behavior, and failure injection.
- Event serialization/version compatibility.
- Component classification and ordinal arithmetic.

### Integration

- Runtime request equality before/after accounting.
- Multi-request tool loop.
- CLI filesystem layout and session inspection.
- JSON output remains machine-readable.

### Regression

- No provider request content/order changes.
- No additional model calls.
- No failure when `.forge/context` is unwritable.
- No prompt/tool content persisted in plan JSON.
- Explicit compact MCP discovery is measured according to the tools actually
  offered by that frontend; registry-only tools are not counted.

## Acceptance checklist

- [ ] Every production generation request records a plan or unavailable event.
- [ ] Plans are deterministic, versioned, and source-linked.
- [ ] Stable prefix reports system/tool drift independently.
- [ ] Request ordinals cover iterative tool turns.
- [ ] Context failure never blocks an otherwise valid generation.
- [ ] Existing request and answer fixtures remain byte/structurally equal.
- [ ] Plans contain sizes/hashes, not secret-bearing content.
- [ ] Full validation passes.

## Open questions / assumptions

No open product questions. Accepted defaults:

- dedicated injected `ContextStore`;
- compact recorded/unavailable events;
- system/tool/combined stable hashes;
- fail-open accounting;
- explicit output limit recorded only when known.
