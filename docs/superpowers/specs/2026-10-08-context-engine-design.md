# Forge Context Engine — Architecture

**Status:** Accepted  
**Date:** 2026-10-08  
**Scope:** Native context accounting, deterministic compression, reversible retrieval, and observational memory

## Problem and goals

Forge has an append-only, versioned session log and can replay exact model
conversations, but its context policy is still coarse: recent history is kept
verbatim until the model budget forces older messages out, and oversized tool
results are capped generically. Long coding sessions can therefore lose
decisions and rationale, while current turns spend tokens on repetitive logs,
search results, JSON, and diffs.

The context engine must make long Forge sessions coherent and economical
without weakening the properties that make the harness trustworthy:

- the event log remains the source of truth;
- every derived memory is traceable to source events;
- deterministic work stays local and adds no provider latency;
- compressed material remains retrievable when the model needs detail;
- redacted information can never re-enter provider context through retrieval;
- session forks inherit exactly the memory available at their cut point;
- the default `forge` experience remains one command with no sidecar.

Headroom and observational-memory systems are design references only. Forge
will not run, embed, call, or depend on Headroom.

## Approaches considered

### 1. Native event-derived context engine — selected

Build accounting, content-aware compression, storage, retrieval, observation,
and planning directly in Rust around Forge's existing event and tool
contracts.

**Advantages**

- Preserves the single-binary and local-first product.
- Reuses stable session/run/call/event identities.
- Keeps redaction, approvals, replay, and provider adapters under one policy.
- Makes every transformation testable without a model or network.

**Costs**

- Forge owns compressor quality and storage lifecycle.
- More initial implementation than wrapping another product.

### 2. Inline memory extraction from primary model responses — rejected

Ask every generation model to emit a hidden memory block, strip it from the
answer, and persist it.

**Why rejected**

- Couples memory correctness to every provider and response format.
- Makes ordinary answers carry hidden protocol obligations.
- Cannot be replayed or retried independently.
- Encourages unverified model output to become durable state.

Inline extraction may be reconsidered only as an optimization after an
event-derived implementation proves the contract.

### 3. External compression or memory service — rejected

Run a proxy, MCP service, or sidecar beside Forge.

**Why rejected**

- Breaks the one-command, single-binary experience.
- Creates a second authority over context and secrets.
- Adds installation, availability, and compatibility states.
- Duplicates storage and provider-boundary policy.

The external systems remain sources of patterns and measurements, not runtime
dependencies or planned backends.

## Recommended approach

Add a native context engine between Forge's immutable session/tool artifacts
and model request assembly.

The engine has two independent derivation paths:

1. **Spatial compression** reduces large current artifacts such as logs, JSON,
   search results, diffs, and code before they enter the model context. A
   sanitized complete form is stored locally under a content hash, and the
   compressed view carries a retrieval handle.
2. **Temporal observation** distills fixed, source-anchored ranges of older
   session events into atomic decisions, constraints, outcomes, unresolved
   questions, and work state. Observations are never generated from previous
   observations.

A deterministic planner then assembles a request from stable guidance,
relevant project/session observations, recent raw turns, compressed artifacts,
graph context, and a reserved output budget. The append-only event log remains
authoritative; every other store can be deleted and rebuilt.

## Key decisions

### Stack and libraries

- Implement in existing Rust workspace crates; do not add a service runtime.
- Prefer existing `serde`, `serde_json`, hashing, graph, session, and runtime
  abstractions.
- Use exact or conservative character/token estimates initially. Provider
  usage events calibrate estimates; no tokenizer dependency is required for
  phase one.
- Use deterministic compressors first. Model-based compression is outside the
  initial architecture.
- Store manifests as versioned JSON and large content-addressed payloads as
  bytes. A future encoding change is permitted behind manifest versions.

### Data model

#### ContextArtifact

A provider-safe, complete artifact derived from one tool result or context
source.

- content hash and byte length;
- semantic kind (`log`, `search`, `json`, `diff`, `code`, `text`, or
  `lossless`);
- source anchor: session, run, call, and event sequence;
- redaction-policy version;
- retention timestamps.

#### CompressedView

The bounded representation sent to the model.

- deterministic compressor and version;
- original and compressed size estimates;
- preserved structural summary;
- explicit omission markers;
- retrieval handle for the matching `ContextArtifact`.

#### Observation

An atomic memory item derived from raw events.

- stable ID;
- project/session scope;
- kind (`decision`, `constraint`, `outcome`, `question`, `state`);
- concise content;
- source event range;
- observer version and creation time;
- optional supersession/tombstone relation.

#### ObservationLedger

An append-only projection with committed source watermarks. Out-of-order
observer completion is legal because each result declares the exact event
range it covers.

#### ContextPlan

The deterministic record of what a request includes and omits.

- stable-prefix hash and size;
- selected observations and artifacts;
- recent raw-tail boundary;
- budget by component;
- reserved output budget;
- omission reasons.

The plan is recorded with the run so a context-related failure can be
reproduced and explained.

### Storage

```text
.forge/context/
  objects/<content-hash>
  manifests/<session-id>/<run-id>/<call-id>.json
  observations/<session-id>.jsonl
  watermarks/<session-id>.json
  plans/<run-id>.json
  memory/<session-id>/
    INDEX.md
    decisions.md
    constraints.md
    journey.md
```

- `.forge/` remains ignored.
- Artifacts are content-addressed and deduplicated.
- Store only sanitized complete content; never persist a second raw secret
  archive.
- Enforce user-only permissions where the platform supports them.
- Apply bounded size/age retention with LRU-style eviction.
- Missing artifacts degrade to an explicit unavailable marker, never a
  fabricated reconstruction.

### Compression contracts

Compression is activated automatically only above a conservative threshold.
Content below the threshold remains byte-faithful.

Initial policies:

- **Logs:** failures, warnings, command summary, first/last windows, and
  bounded context around error clusters.
- **Search results:** preserve paths and line numbers; group by file and cap
  repeated matches.
- **JSON/tabular:** preserve shape, exceptional records, distinct values, and
  deterministic samples.
- **Diffs:** preserve file headers and changed hunks; bound unchanged context.
- **Code:** preserve imports, declarations, signatures, and explicitly
  requested ranges.
- **Git status, approvals, user requirements, commands, error codes, and test
  names:** lossless.
- **Unknown text:** retain current conservative cap behavior with a retrieval
  handle.

Every view states what was omitted and how to retrieve it. Compression that
does not reduce estimated tokens is discarded.

### Retrieval contract

Expose one bounded tool:

```text
retrieve_tool_output(handle, query?, start?, end?, limit?)
```

- A request must be bounded by range, query, or result limit.
- Retrieval returns sanitized content only.
- Retrieval events are recorded and become quality signals for compressor
  tuning.
- The model never receives a hidden automatic expansion that can unexpectedly
  overflow context.

### Memory and scope

Initial scopes are **project** and **session**.

- A session fork inherits observations whose source range is at or before the
  fork cut.
- Later observations remain branch-local.
- Project memory is promoted only through explicit consolidation; ordinary
  session observations do not silently become project rules.
- User-global and cross-project memory are deferred.

Observation extraction is opt-in in its first release. Accounting,
deterministic compression above threshold, and retrieval handles are on by
default.

### Security and privacy boundaries

- Redact before hashing and persistence.
- Include the redaction-policy version in the artifact identity.
- Never allow retrieval to bypass local-only or provider egress policy.
- Never put credentials, environment values, or approval secrets into memory.
- Treat observations as untrusted derived data with source links, not as
  instructions equal to project guidance.
- Learning can propose guidance changes but cannot silently modify committed
  instruction files.

### Prompt-cache stability

Forge reports rather than silently repairs prefix drift.

For each request record:

- stable-prefix byte/hash estimate;
- whether it changed since the prior turn;
- sizes for guidance, tools, memory, replay, graph context, and prompt;
- compression savings and retrieval counts.

Static guidance and tool schemas precede dynamic session material and remain
byte-identical whenever their inputs are unchanged.

## Missing pieces

- A shared context-artifact and context-plan contract in the core/runtime
  boundary.
- Provider-safe artifact storage and eviction.
- Deterministic content classifiers and compressors.
- A bounded retrieval tool wired through normal tool events.
- Context accounting around current request assembly.
- Stable-prefix metrics.
- Observation schema, ledger, chunking, and fork projection.
- Observer-job scheduling and provider/cost policy.
- Memory inspection commands and chat slash commands.
- Consolidation and learning proposal workflows.

## Spikes and experiments

### Compressor fidelity corpus

**Question:** Can deterministic compression materially reduce real Forge tool
outputs without changing task outcomes?

**Spike:** Capture a sanitized corpus of logs, search results, JSON, diffs, and
code from existing test fixtures and sessions. Compare baseline versus
compressed runs on error identification, referenced paths/lines, and validation
success.

**Decision rule:** Enable a compressor by default only if it reduces estimated
tokens by at least 30%, preserves all required structural assertions, and does
not reduce deterministic task success.

### Retrieval usefulness

**Question:** Are compressed views sufficient, and are retrieval handles
discoverable by models?

**Spike:** Script tasks where the answer is present both inside and outside the
compressed view.

**Decision rule:** Retrieval must recover the omitted answer reliably, remain
bounded, and avoid repeated full-artifact expansion.

### Token accounting calibration

**Question:** Is Forge's character-based estimator sufficient for planning
across Codex, Claude, Kimi, and local models?

**Spike:** Compare estimates to recorded provider usage across representative
requests.

**Decision rule:** Keep the estimator if the conservative error stays within
15%; otherwise add provider-family token estimators behind the same contract.

### Observation quality

**Question:** Which observer model/prompt preserves decisions and constraints
without inventing durable facts?

**Spike:** Run observers against source-anchored session chunks and grade
coverage, unsupported claims, source accuracy, and cost.

**Decision rule:** Observational memory remains opt-in until unsupported durable
claims are below the agreed review threshold and every observation has valid
source anchors.

## Open questions

- Exact automatic compression threshold and artifact-store capacity; settle
  using the corpus rather than intuition.
- Observer provider and budget policy; settle through the observation-quality
  spike.
- Whether project-memory promotion is manual only or may be proposed
  automatically; default is manual.
- Whether semantic retrieval over observations earns its cost beyond lexical
  and recency ranking.
- Retention policy for sessions explicitly marked private or disposable.
- UI shape beyond `/context status`, `/memory status`, source inspection, and
  retrieval statistics.

## Rollout principles

- Ship accounting before compression, and compression before model-derived
  memory.
- Every phase must be removable without damaging the event log.
- Compare task outcomes, not token savings alone.
- Default-on behavior must be deterministic, local, reversible, and visibly
  measurable.
- No phase may add a required setup step to bare `forge`.
