# Forge as a drop-in AI development harness

Status: repository assessment and staged implementation plan, 2026-10-09.

## Assessment

Forge already has the shape of a coding harness. It is a 16-crate Rust
workspace whose stable center is `forge-runtime::AgentService`, shared by the
batch CLI, interactive chat, REST/SSE, MCP, and ACP. `forge-core` defines the
closed seams the rest of the workspace implements: `ModelProvider`,
`DecisionRouter`, `ExecutionProvider`, `SkillRegistry`, `ProjectGraph`, and
`SessionStore` (`ARCHITECTURE.md`, `crates/forge-core/src/{model,router,
execution,graph,session}.rs`). This should be extended, not replaced.

Implemented behavior is further along than the original v0.2 plan:

- The generation plane has OpenAI-compatible, Anthropic Messages, and Codex
  subscription adapters, plus local and deterministic test providers. Provider
  construction filters unreachable or unauthenticated candidates and enforces
  `local_only` (`crates/forge-providers/src/{model,anthropic,codex,
  local_only}.rs`). Claude, Codex, Kimi, API-key, and local credentials are
  discovered without logging their values (`credentials.rs`).
- The decision plane composes Needle, Jev, Laya/HTTP, cheapest, static, and
  fallback routers. Decisions and completion usage/cost are written separately
  from prompt/tool contents (`forge-needle/src/router.rs`,
  `forge-providers/src/{jev,router}.rs`,
  `forge-session/src/decisions.rs`).
- Tool calls go through one dispatcher and `ExecutionProvider`; risk,
  disposition, and approval are recorded before execution. Per-run tool quotas,
  turn limits, project-bound file operations, cancellation, and
  `prompt-dangerous` already exist (`forge-runtime/src/{service,tools}.rs`,
  `forge-execution/src/native.rs`).
- Append-only, redacted JSONL is the durable conversation record. Resume
  reconstructs provider-valid messages, repairs orphaned tool calls, and fits
  history to the selected model's context window
  (`forge-session/src/store.rs`, `forge-runtime/src/replay.rs`). Token and USD
  budgets are rebuilt from decision logs, so a new process sees earlier spend
  (`forge-runtime/src/budget.rs`).
- `forge-graph` is a deterministic source/project graph and semantic retrieval
  index. It is not yet a durable task-dependency graph. The repository has no
  typed plan/task/checkpoint contract that says which development step is ready,
  running, blocked, verified, or safely repeatable.

The main gap is therefore orchestration state, not another agent loop. Today a
session can resume its conversation, but it cannot resume a multi-step coding
plan from a durable node boundary. Before the Phase 1 slice below, provider
failures were also flattened into mostly textual `ForgeError::Provider` values.
In particular, the OpenAI-compatible and Anthropic streaming paths retried any
rejected streaming request as a whole-response request. A `429` could therefore
cause an immediate second request while hiding the provider's `Retry-After`
signal.

The recovered Delta worktree at
`.delta/worktrees/mwqmxena4r5m/forge` explored research routing, typed 429s,
routing descriptors, and a Serena subprocess. It is based on older main and
mixes several independent projects. Only the provider-interruption concept is
appropriate for the first slice. Public research is a separate product surface,
and Serena remains future work.

## Smallest useful end-to-end workflow

The first complete workflow should remain one command:

1. `forge run "request"` resolves project instructions and configuration,
   creates a session/run, and records the request.
2. A small deterministic planner produces a short typed plan: inspect, edit,
   check, review. Each node declares dependencies, required capabilities, and
   its verification condition. The human-readable rendering is stored beside
   the typed form.
3. `DecisionRouter` receives task metadata plus an allowlisted set of available
   models. Needle handles local decisions; Jev may rank candidates when
   configured; a deterministic router always remains available. The chosen
   model, reason, confidence, prices, and observed availability are recorded.
4. The selected `ModelProvider` proposes tool calls. Needle may fill or validate
   calls, but never grants authority. The existing dispatcher and
   `ExecutionProvider` enforce project boundaries, quotas, and approvals.
5. Forge records file changes, runs repository-defined focused checks, then asks
   the provider for a bounded review of the diff and check results. It returns
   reviewable working-tree changes without publishing them.
6. After every externally visible side effect and node transition, Forge
   commits task state atomically. A provider limit or process exit parks the
   active node with the last safe checkpoint and retry metadata. `forge resume
   <id>` revalidates the working tree and continues the incomplete node without
   replaying a completed side effect.

This matches established harness conventions without copying one system.
Claude Code exposes a familiar interactive/print CLI with continue/resume,
model selection, turn limits, and permission modes
([Claude Code CLI reference](https://code.claude.com/docs/en/cli-usage)). The
OpenAI managed Codex harness keeps sessions across turns, streams progress,
supports steering, tools/MCP, sandboxed execution, and resume
([Agents API overview](https://developers.openai.com/api/docs/guides/agents-api/overview),
[sessions](https://developers.openai.com/api/docs/guides/agents-api/sessions)).
GitHub Copilot uses lifecycle and pre/post-tool hooks for policy and audit
([GitHub hooks](https://docs.github.com/en/copilot/concepts/agents/hooks)).
LangGraph separates thread checkpoints from longer-lived stores and persists
interrupt state before resume
([persistence](https://langchain-ai.github.io/langgraph/concepts/durable_execution/),
[interrupts](https://langchain-ai.github.io/langgraph/how-tos/create-react-agent-hitl/)).
Forge should keep its local JSONL and Rust traits while adopting the proven
properties: explicit sessions, policy before tools, durable step boundaries,
observable events, and a reviewable diff.

## Component contracts

`jev` is a decision adapter. Input is a bounded task descriptor, required
capabilities, eligible model IDs, configured prices, and observed availability.
Output is one eligible choice plus reason/confidence. It cannot add a provider,
read credentials, authorize a tool, or claim quota remaining.

Needle is the local decision and structured-call layer. It may classify work,
select/fill a tool call, and embed graph content. Its result is advisory until
the tool registry, schema validation, quota, risk, and approval checks accept
it. Low confidence and backend failure degrade visibly.

Graph should become two explicit concepts behind separate types: the existing
regenerable `ProjectGraph` for source context, and a new append-only `TaskGraph`
for plan nodes and execution state. A task node owns stable ID, dependencies,
state, attempt count, capability needs, checkpoint, and verification result.
Graph stores facts and transitions; it does not call models or tools.

Providers implement generation only. A provider receives normalized messages,
tools, and limits, then returns content/tool calls/usage or a typed failure.
Credentials and endpoints are resolved before routing. Provider errors must
distinguish authentication, transient transport, rate limit with observed retry
time, invalid request, and terminal account/entitlement failure without
inventing capacity or switching accounts.

`AgentService` remains the orchestrator. It owns ordering and events: load
checkpoint, route, call provider, validate/dispatch tools, checkpoint effects,
run checks, and finish or park. Front ends continue to be thin adapters.

## Requirements

- Safety: one policy path for every tool surface; deny unknown tools; canonical
  project boundaries; no secret-bearing events; approval decisions durable;
  retries never repeat a non-idempotent effect unless the checkpoint proves it
  did not complete.
- Reliability: bounded network calls and response bodies; typed provider
  failures; respect observed `Retry-After`; no automatic cross-account action;
  atomic state replacement or append plus fsync at task boundaries; tolerate a
  truncated final record.
- Cost: capability filtering before price ranking; unknown price remains
  unknown; record provider-reported usage; pre-call local ceilings and
  post-call reconciliation; never describe separate provider quotas as pooled.
- Resumability: stable session/task/node IDs; persist inputs needed to replay a
  pure step; record working-tree identity and changed paths; revalidate before
  resume; park with an actionable reason and earliest observed retry time.
- Inspectability: human and JSON forms for route choice, usage, task graph,
  policy decision, checks, and final diff. Existing stdout/stderr discipline
  remains unchanged.

Anthropic currently documents `429` plus `Retry-After` for ordinary rate limits
and warns that retrying earlier fails
([Claude rate limits](https://platform.claude.com/docs/en/api/rate-limits)).
Forge should surface that evidence and stop; it must not infer credits or bypass
provider limits.

## Staged implementation

### Phase 1: interruption-safe providers

Add a typed rate-limit failure with provider/model and observed retry seconds.
All generation adapters detect it before reading or retrying a response. The
streaming fallback must not issue a second request for a rate limit.

Acceptance: deterministic provider tests prove one request per streamed `429`,
preserve valid `Retry-After`, ignore malformed timing, and keep existing error
redaction. `cargo test -p forge-providers -p forge-core` and formatting pass.

### Phase 2: provider health and routing transparency

Introduce an in-memory observed-availability table fed only by typed failures,
and show it in `forge model list`, `doctor`, route events, and JSON output. Keep
explicit model pins binding. Add no automatic mid-turn provider switch; a
fresh/resumed node may re-route only under an explicit routing policy.

Acceptance: routing cannot select an ineligible provider; cooldown expiry uses
an injected clock; explicit pins fail visibly; no secret or raw prompt enters a
decision record; live canaries remain opt-in.

### Phase 3: durable task graph

Add a small `forge-task` crate or a narrowly scoped module only after its event
schema is agreed. Define `TaskPlan`, `TaskNode`, `TaskState`, `Checkpoint`, and
`Verification`. Persist under `.forge/tasks/` using versioned append-only events
and derived snapshots. Keep it separate from `ProjectGraph`.

Acceptance: dependency validation rejects cycles/missing nodes; transitions are
closed enums and validated; a process killed after each transition resumes at
the correct node; completed side effects are not repeated; corrupt/truncated
tails degrade to the last valid checkpoint.

### Phase 4: one-command plan/edit/check/review

Connect the task graph to `AgentService`. Begin with four node kinds and the
existing tools: inspect, edit, check, review. Use repository instructions and
existing check commands; report the diff and verification, leaving commit/push
to an explicit later action.

Acceptance: an offline scripted provider starts in a disposable project, edits
one file, runs a real check, reviews the diff, then resumes successfully from
forced interruption at every node boundary. CLI, chat, REST, MCP, and ACP share
the same events and result.

### Phase 5: hardened multi-provider operation

Add provider-specific contract tests and opt-in live canaries for Claude,
Codex, Kimi, and local OpenAI-compatible endpoints. Document supported auth and
wire contracts separately from model aliases. Add backoff only for retry-safe
generation calls and only from provider evidence or bounded policy.

Acceptance: hermetic contract fixtures pass in CI; manual release canaries cover
each supported subscription provider; failures name whether auth, endpoint,
capability, quota, or response shape failed; resume works after each category.

## Explicitly deferred

- Serena or another language-server/symbol subprocess, symbol-aware edits, and
  claims about token savings. The native graph must first support the vertical
  workflow and be measured on real failures.
- General workflow DSLs, dynamic native plugins, distributed schedulers,
  multi-agent delegation, speculative parallel provider calls, and automatic
  PR/push/deploy behavior.
- Automatic quota pooling, credential sharing, account rotation, or attempts to
  bypass provider limits. Forge may route only among explicitly configured,
  authorized providers.
- The recovered Delta branch's public-research runner. It has a different trust
  boundary and should be assessed as a separate product feature.

## Risks and open decisions

- Task checkpoints need a precise side-effect rule. The default should be
  at-least-once computation with explicitly idempotent tools, and exactly-once
  claims only for local state transitions Forge controls.
- A single selected model for a whole run is simple, but node-level routing can
  save money. Decide only after route records make the choice explainable and
  explicit pins remain binding.
- Provider subscription endpoints and credential stores are less stable than
  published APIs. Keep them isolated, contract-tested, and release-canary-only;
  do not make the durable task format provider-specific.
- JSONL is appropriate for the first local task graph, but concurrent writers
  require the existing one-live-run-per-session invariant or a real locking
  protocol. Avoid adopting a database until measured contention requires it.
- Planning quality is uncertain. Start with a bounded four-node plan and store
  its human-readable form; do not let a planner create arbitrary executable
  node kinds.
