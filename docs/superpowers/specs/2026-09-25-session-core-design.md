# Session core: a decision plane for forge's agent loop

**Status:** design, awaiting implementation plan
**Date:** 2026-09-25
**Scope:** the Session core. See "How this relates to the existing program" below
before reading further — this document was drafted in parallel with work that
had already landed, and its lettering is not the project's only one.

## How this relates to the existing program

This spec was written against a branch that had diverged from `main`, and `main`
meanwhile shipped overlapping work. Reconciling, so the two numbering schemes do
not mislead:

| This document | The program's existing name | Status on `main` |
|---|---|---|
| sub-project B — interactive REPL | **Phase B / sub-project 6b**, [`2026-09-24-interactive-chat-ui-design.md`](2026-09-24-interactive-chat-ui-design.md) | **already designed and planned** — do not re-spec it |
| sub-project A — Session core | this document | partially superseded, see below |
| sub-projects C, E, F | ACP+serve, editor integration, streaming | unclaimed |

Three things this document proposed have since landed on `main` independently,
and `main`'s versions win:

- **Per-platform engine fetching.** `needle-sys/build_support.rs` implements it
  with the same pinned digests, more thoroughly tested. The version drafted
  alongside this spec was discarded.
- **The honest no-engine error.** `BackendError::EngineMissing` with a shared
  `ENGINE_REMEDY` is the same fix this spec described as `BackendUnavailable`.
- **Session substrate** — replay, fork, attach — which §2's `Session` must be
  designed *with* rather than alongside.

What remains genuinely unbuilt from this document: the decision plane and its
trait (§13), the gate and risk taxonomy (§3), egress tiers (§11), graph hygiene
and freshness (§17), the approval contract (§18), budgets (§19), the daemon
(§20), and earned autonomy (§15). The decision log (§4) has landed.

**Still unported, recorded so it is not lost:** `main` has `default = []` for
`forge-cli` and still branches on `cfg!(feature = "needle-ffi")` in seven places
across `init.rs` and `doctor.rs`. Making the engine default-on with graceful
degradation — the §16 single-command property — should be designed against
`main`'s `EngineResolution`/`build_support` rather than transplanted from the
abandoned branch.

## Goal

Make forge good enough to build forge with. The harness surfaces are the
interactive REPL, Zed via the ACP adapter, and `forge serve` — not one-shot
`forge run`, which cannot keep an engine warm and so can never reach the fast
path this design is built around.

The brief, as agreed:

> Forge is fast because the cheap decider runs first. Needle 3 picks and fills
> tool calls on-device. Jev — hosted or self-hosted — makes the structured
> judgment calls: gating tool execution and escalating model choice. Cloud and
> local LLMs do the open-ended generation neither can. No local LLM sits in any
> decision path. Everything degrades to working-but-dumber rather than failing,
> and every decision is logged so it can be learned from later.

## Decisions already settled

Carried in from brainstorming so implementation does not relitigate them:

| Decision | Choice |
|---|---|
| Structure | An explicit `Session` object owning per-conversation state; `AgentService` becomes its factory |
| Engine concurrency | One shared engine, queued; sessions stay independent |
| Gate authority | The gate may narrow but never widen what the approval mode permits |
| Jev dependency | Needle gates; Jev sharpens it when present. Zero credentials still works |
| Decision log | On by default, local-only, decision shape not content |
| Local LLM as decider | Rejected — uncalibrated confidence, and ~6.5 s where needle is ~1.1 s |

## §1 Architecture

### Three planes

Borrowed from the decision-plane pattern, which is the clearest framing of what
we are building:

- **Decision plane** — needle (on-device) and Jev (network). Typed answers, no
  tokens, 100–500 ms.
- **Generation plane** — local or cloud LLMs. Tokens, seconds.
- **Control layer** — ordinary Rust. Thresholds, escalation, retries, audit.
  Lives in `forge-config` and `Session`, never in a prompt. Control flow belongs
  in code that can be tested, versioned and diffed.

### The turn

```
session.turn(prompt)
│
│ 0. SELECT ───── graph → ranked files/symbols → StatePayload at the configured
│                 egress tier (§11). Deterministic, model-free, never source.
│
│ 1. DECIDE ───── two calls fired CONCURRENTLY (they contend for nothing:
│                 needle owns a blocking OS thread, a remote plane is async I/O)
│
│      needle.tool_call(prompt, tools) ─────────────────▶ ~1.1 s   name + arguments
│      plane.ask{tool, safety, model}  ────▶ ~250 ms              choices + probabilities
│
│                 The plane's answers land first and are free by the time needle
│                 returns. With a local plane (§13) they cost no network at all.
│
│ 2. BRANCH ───── control layer, on typed values only
│                 ├ needle confident ────▶ GATE ─▶ dispatch    no LLM, no routing
│                 ├ needle declined, Jev picked a tool
│                 │     └ needle.fill(pinned tool) ─▶ GATE ─▶ dispatch
│                 └ otherwise ───────────▶ route ─▶ LLM
│
│ 3. ROUTE ────── needle.decide → Jev escalation → static   only on LLM paths
│
│ 4. GENERATE ─── chosen provider
│                 └ proposes tool calls ─▶ GATE each ─▶ execute
│
└─ every decision → DecisionLog, with its full probability vector
```

### Why this shape

**Routing moved below the branch.** Choosing a model is only a meaningful
question once a model is known to be answering. The saving is modest in a warm
session (`needle.decide` is ~120 ms) but the structure is honest, and it removes
provider resolution from turns that never reach a provider.

**Needle and the decision plane run concurrently.** This is the main performance
idea. Needle occupies its own thread; a remote plane is network I/O on the tokio
runtime. Running them together hides the plane's latency completely behind
needle's, which makes the fall-back branch cheap: on a decline, the plane's
answer is already in hand and the retry costs only needle's fill, not a
round-trip.

Expected value of the decline retry, using measured numbers (§7):

Retry cost is Jev (~250 ms) + needle fill (~1.1 s) = 1.35 s when serial, and
just the fill (~1.1 s) when concurrent, against an avoided LLM turn of ~6.5 s:

| retry succeeds | serial (1.35 s) | concurrent (1.1 s) |
|---|---|---|
| 50% | −1.90 s | **−2.15 s** |
| 30% | −0.60 s | **−0.85 s** |
| 10% | +0.70 s worse | +0.45 s worse |

The concurrent form is what makes this worth shipping rather than hiding behind
a flag.

**All questions go in one request.** The API takes `questions` as a map and
processes them together with shared latency and billing; input tokens are
charged once for the shared `state` and answers are free. Asking tool choice,
safety and model selection separately would be three round-trips for no benefit.
Forge asks one question today; that is the change. Implementations that cannot
batch answer sequentially behind the same trait method (§13).

**The plane's answer is speculative and sometimes discarded.** When needle answers
confidently we throw it away. With a local plane that costs nothing at all; with
a hosted one it costs a `state`'s worth of input tokens (~$0.0001) and no
wall-clock. Accepted deliberately — but note this is *per turn*, which is exactly
why §11 bounds what `state` may contain.

### Batched request shape

```json
{"state": "<StatePayload at the configured egress tier — never source (§11)>",
 "model": "jev-latest",
 "questions": {
   "tool":   {"type": "choice",
              "instructions": "Which tool, if any, satisfies this request?",
              "criteria": {"read_file": "...", "graph_grep": "...", "none": "no tool applies"}},
   "safety": {"type": "choice",
              "instructions": "Classify the risk of executing this.",
              "criteria": {"readonly": "...", "privileged": "...",
                           "destructive": "...", "exfiltration": "..."}},
   "model":  {"type": "choice",
              "instructions": "Which model should handle this software task?",
              "criteria": {"<model>": "<description>"}}
 }}
```

Two constraints the implementation must respect:

- **Canonical option ordering.** Option order alone has been observed to swing
  probabilities by up to 0.17. Every `criteria` map must be emitted in a stable
  sorted order so a decision is reproducible.
- **255 options maximum** per `choice`. Fine at 7 tools; the retrieval seam
  (stage 0) exists for when it is not.

## §2 The `Session` object

`Session` owns everything that is per-conversation and outlives a single turn:

```rust
pub struct Session {
    id: SessionId,
    config: Config,
    history: Vec<Message>,

    /// Handle to the *shared* engine (§20). Warmth is a property of the engine,
    /// not of this session: the tool surface is installed into the engine, so a
    /// second conversation against the same runtime must not pay the ~8 s
    /// install again. `EngineHandle::None` when this build has no engine
    /// (`forge_needle::HAS_EMBEDDED_BACKEND == false`).
    engine: EngineHandle,

    /// The decision plane (§13). Never optional: absence is `NullPlane`, and an
    /// on-device `NeedlePlane` needs no credential and no network.
    plane: Arc<dyn DecisionPlane>,
    /// Builds every `state` payload, enforcing the egress tier (§11). The only
    /// permitted constructor.
    payloads: StatePayloadBuilder,

    gate: Gate,
    /// Handle to the *shared* log (§20). Records carry this session's id, so one
    /// log spans every surface — which is what §15's calibration needs in order
    /// to converge in reasonable time.
    log: DecisionLogHandle,
    /// Session-scoped memo, keyed by a hash of (prompt, ordered tool set): an
    /// identical request reuses its decision instead of re-deciding.
    memo: HashMap<u64, DecisionOutcome>,

    /// Head/tail retention with a compacted middle (§12).
    context: ContextBudget,

    cancel: CancellationToken,
    store: Arc<JsonlSessionStore>,
}

impl Session {
    pub async fn turn(&mut self, prompt: &str) -> Result<TurnOutcome, ForgeError>;
    pub fn cancel(&self);
    pub fn id(&self) -> &SessionId;
}
```

`AgentService` keeps model/router/execution/skills/graph and becomes a factory:

```rust
impl AgentService {
    pub fn session(&self) -> Session;
    pub fn resume(&self, id: &SessionId) -> Result<Session, ForgeError>;
}
```

Every surface holds a `Session` and calls `turn`. That is the point of the
extraction: the gate, the memo, the context budget and the conversation itself
acquire an owner whose lifetime matches a conversation, instead of being
process-global or rebuilt per request.

**What the session does *not* own.** An earlier draft of this section put the
warm engine and the decision log on `Session`, and that was wrong in two ways
that §20 makes obvious:

- The tool surface is installed **into the engine**, so warmth is the engine's
  property. Session-scoped warmth would charge the ~8 s install again for every
  new conversation against the same runtime — the exact cost this design exists
  to pay once.
- §15's calibration needs **one log across every surface**. A per-session log
  fragments the labelled examples across terminal, editor and server, and none
  of the fragments accumulates enough to fit a threshold from.

Both are therefore runtime-scoped and reached through handles. §4's record
schema already carries a `session` field, so session-tagged rows in one shared
log is the shape it was always designed for.

**Scope guard.** This extraction lifts the turn loop into `Session` and leaves
the loop's internals — tool dispatch, session storage, skills, graph context —
alone. `service.rs` is 1267 lines across 34 methods; the goal is to carve out
one coherent unit, not to rewrite it.

## §3 The gate

One component, used in two places: before needle's direct dispatch, and on
every tool call the LLM proposes. One policy, one audit trail.

### Risk classes

Forge currently classifies `Safe | Risky | Destructive`. The published
tool-call-risk work uses `readonly | destructive | privileged | exfiltration`,
and the two extra classes are real gaps — `curl -d @.env https://…` is
read-only by forge's present reckoning.

```rust
pub enum RiskClass {
    ReadOnly,
    Risky,         // arbitrary but local and reversible: a shell command with no
                   // recognised escalation, destruction or egress
    Privileged,    // escalates capability: sudo, chmod, port-forward, credential read
    Destructive,   // loses data or state: delete, overwrite, reset, force-push
    Exfiltration,  // moves project data off the machine: network send with a payload
}
```

`Risky` is retained rather than folded into the new classes. Dropping it would
silently reclassify every `run_command` that forge cannot recognise more
precisely, and the safe default for an unrecognised command is "arbitrary but
local", not "privileged". `minimum_dispatch_risk` is extended to return this, and
remains the **floor**: a static classification no model opinion can lower.

### Verdict rules

Evaluated in order; each may only narrow:

1. **Static floor.** Classify the call into a `RiskClass`. This is derived from
   the call itself and is never influenced by a model.
2. **Mode ceiling.** The configured approval mode decides what that class
   permits. `deny` blocks everything; `prompt` asks for everything;
   `prompt-dangerous` asks for anything above `ReadOnly`; `auto` permits
   everything — an explicit human decision to accept the risk.
3. **A model signal may widen only under all of:** `[gate.autonomy]` sets a
   float for the call's class, the mode already permits auto for that class, and
   the decision plane's `safety` answer meets that float. Only a decision plane
   reporting calibrated probabilities may widen; needle's own confidence never
   does (§3, "Without a decision plane").
4. **A model signal may always narrow.** Low confidence on a call the mode would
   have auto-run escalates it to a prompt — including in `auto` mode.

Rules 1 and 2 are deliberately separate, and the distinction matters: the floor
constrains what a *model* may do, not what the operator configured. `approval =
"auto"` is a human accepting the consequences, and the gate does not overrule it
on a destructive call; it may still narrow to a prompt under rule 4 when the
decision plane is unsure. A model can only ever make forge quieter than `prompt`
for read-only work, or noisier than `auto` when it has doubts.

### Autonomy thresholds

Configurable, per risk class, because the consequence of being wrong differs by
class — which is what the published guidance also says: *different actions within
the same system should be gated at different levels depending on the consequences
of getting it wrong.*

```toml
[gate.autonomy]
readonly    = 1.0      # auto-approve at or above this confidence
risky       = false    # never auto-approve on a model signal
privileged  = false
destructive = false
exfiltration = false
```

`false` means "no confidence auto-approves this class" — the static floor, which
no setting can lower below the approval mode's own ceiling. A float sets the bar
for that class. Defaults are the conservative end of the evidence below;
operators who want 0.95 or 0.75 can say so, and the decision log records the
confidence of every auto-approval so the cost of that choice is measurable rather
than theoretical.

### What the evidence says about where to set them

From a 60-case benchmark on this precise task (34 clear, 14 ambiguous, 12
adversarial): overall accuracy 91.7%; the 0.9–1.0 bin held 50 of 60 predictions
at 98.0% accuracy; **every incorrect answer carried confidence below 1.000**,
and all 40 answers at exactly 1.000 were correct.

Accuracy was also non-monotonic in confidence — the 0.4–0.5 bin scored 100%
where 0.1–0.2 scored 0% — which is what an uncalibrated signal looks like. So
lowering a threshold does not trade accuracy smoothly for autonomy; at 0.9 the
observed error rate is ~2%, and below that the signal stops ordering reliably.
That is the number to weigh when setting these, and it is why the defaults sit at
1.0 and `false` rather than somewhere more permissive.

**A caveat on transfer, stated plainly:** that benchmark used its own taxonomy
and its own prompt wording. Published calibration work is explicit that a
threshold does not transfer between question formulations, even logically
equivalent ones — so forge's own numbers will differ. The defaults are chosen to
be maximally conservative precisely because they are borrowed: erring this way
means asking too often, not approving too readily. Re-derive them from
`.forge/sessions/*.decisions.jsonl` once there is traffic; that is what the log
is for.

Thresholds are also **per question**, never shared: one calibrated for "which
tool?" does not transfer to "is this safe?".

### Without a decision plane

The gate uses the static floor alone and **never auto-approves** — needle's
confidence head is not calibrated for this task and no benchmark supports a
cutoff for it. Behaviour matches today's, minus the
misdiagnosis fixed earlier. Gaining autonomy requires a decision plane — which
may be entirely local (§13); losing it costs autonomy, not function.

## §4 Decision log

Append-only JSONL at `.forge/sessions/<session-id>.decisions.jsonl`, beside the
existing transcript and already gitignored. On by default.

```json
{"ts":"2026-09-25T03:14:07.113Z","session":"01M3B5BG88KCD5QBAG92VCZNKG","turn":3,
 "stage":"decide","decider":"needle","question":"tool",
 "choice":"graph_grep","confidence":0.91,
 "probabilities":{"graph_grep":0.91,"graph_context":0.06,"read_file":0.02,"none":0.01},
 "candidates":["graph_context","graph_grep","none","read_file"],
 "outcome":"dispatched","elapsed_ms":1104,"speculative":false}
```

- `stage`: `retrieve | decide | gate | route | generate`
- `decider`: `needle | jev | static | llm`
- `outcome`: `dispatched | declined | approved | prompted | blocked | errored`
- `speculative`: true when the answer was fetched concurrently and discarded
- `plane`: which `DecisionPlane` answered, and `egress`: the tier its payload
  used — so an audit can show what left the machine, not just what was decided

**No prompt text, no tool arguments, no file contents.** Records join to the
existing transcript by `session` + `turn`, so full context is recoverable
locally without duplicating it — and without the log inheriting the sensitivity
of any secret that appeared in a file the agent read.

Recording the whole `probabilities` vector is deliberate: it is what allows
thresholds to be re-derived from real traffic instead of inherited from a
benchmark, which every source on calibration insists is necessary.

## §5 Degradation and error handling

Nothing here may fail a turn that could otherwise proceed.

| Condition | Behaviour |
|---|---|
| No engine linked (`HAS_EMBEDDED_BACKEND == false`) | No reflex, no needle routing. Static routing, gate is static-only |
| Engine present, weights missing | As above; `WeightsMissing` is retried per job so a concurrent fetch recovers without restart |
| `provider = "none"`, or a remote plane with no credential | `NullPlane`. Gate never auto-approves; route escalation skipped |
| `local_only = true` with a remote provider configured | Provider pruned with a warning; falls back to `NeedlePlane` if available, else `NullPlane` |
| Secret scan trips on the payload | Refuse to send, log the refusal, proceed as if the plane were absent |
| Remote plane `429` / `529` | Exponential backoff; on exhaustion treat as absent for this turn |
| Remote plane `422` | Log and treat as absent. Never retried — the request is wrong, not the service |
| Remote plane `401` | Log once per session, then treat as absent |
| Plane slower than needle | Ignored for this turn; needle's answer stands |
| Context budget exceeded | Compact the middle (§12) before the turn; never truncate the head |
| needle declines, no Jev answer | Passthrough to the LLM |
| Cancellation | In-flight decisions dropped; the engine's existing abandon check discards queued jobs |
| Reflex disabled by config | Skip stages 0–2 entirely; behave as the pre-existing loop |

## §6 Testing

**The gate is safety-critical and gets exhaustive unit coverage** — a truth
table over every `RiskClass` × approval mode × confidence band, asserting the
narrow-only property directly: no combination of model signals may produce a
verdict wider than the mode permits.

Additionally:

- **Ordering**, with the existing `HashBackend` (instant) and `SlowBackend`
  (delays past the probe) to cover both the answer-immediately and
  cold-engine branches deterministically.
- **Jev**, via `wiremock`, following the pattern already in `jev.rs` tests.
  Every scenario runs twice: credential present and absent.
- **Batching**, asserting one HTTP request carries all three questions and that
  `criteria` maps are emitted in canonical order.
- **Log redaction**, asserting no prompt text, tool argument or file content
  ever appears in a decision record — property-style over generated turns, since
  an example-based test cannot establish absence.
- **BDD**, one scenario for a dispatched turn (zero model calls) and one for an
  LLM turn, driving the compiled binary.

## §7 Measured baselines

Recorded so future changes can be compared rather than guessed at. Apple
silicon, release build, 7 tools.

| Quantity | Value | Source |
|---|---|---|
| Weights load | 29 ms | `fastpath_latency.rs` |
| needle `tool_call`, cold | 8.0 s | `fastpath_latency.rs` |
| needle `tool_call`, warm | p50 1.1 s, p90 2.2 s | `fastpath_latency.rs` |
| needle `decide` | ~121 ms idle; 388 ms–1.7 s loaded | `doctor`, `e2e.rs` |
| Jev hosted round-trip | 236–276 ms | JevBench |
| Extra Jev questions per request | ≈ free, latency and billing | TypeSafe API docs |
| Local Qwen3-Coder-4bit turn | 6.5–7.5 s | observed trace |
| Gate autonomy threshold | 1.000 | tool-call-risk benchmark |

## §8 Risks

| Risk | Mitigation |
|---|---|
| Jev confidence is distribution shape, not empirical correctness | Hard 1.000 cutoff; static floor; log vectors and re-derive from own traffic |
| Option reordering swings probabilities by up to 0.17 | Canonical sorted `criteria` ordering, asserted by test |
| Thresholds do not transfer between question formulations | Per-question thresholds; never share a constant |
| Reflex routing helped debugging but hurt some feature work in published benchmarks | Config switch plus decision log, so the win rate is measured on this project's own work and is reversible |
| Endpoint drift (`/v1/systemone` vs an observed `/api/alpha/decisions`) | `jev_url` stays configurable; response parsing kept versionable |
| Single engine thread contention across sessions | Accepted; queued with per-session fairness. Worst case N × 1.1 s |
| `choice` capped at 255 options; a community-reported 32 k window | Retrieval seam at stage 0, disabled until the surface grows |
| `laya` scores 34.1% on JevBench's hard tier, last in field | Documented as not recommended; never permitted in the gate |
| Source code leaking to a hosted decision plane | §11: `StatePayload` is the only constructor, `source` is not a remote tier, secret scan refuses before send, `local_only` prunes, and `NeedlePlane` needs no network at all |
| Borrowed thresholds do not transfer between question formulations | Conservative defaults (1.0 / `false`), every auto-approval's confidence logged, thresholds re-derived from own traffic |
| Long sessions exceeding the context window | §12: head/tail retention with a compacted middle; tool results demoted to graph references |
| A deep graph pass sends source to a model once at build time | Opt-in, separate from the per-turn path, cached by content hash; `structure` tier needs no pass at all |

## §9 Considered and rejected

- **Jev first, before needle.** Jev cannot fill arguments, so needle still runs
  afterwards — two hops where one suffices. Measured: Jev ~250 ms + needle fill
  ~1.1 s against needle alone at ~1.1 s.
- **A local LLM as the decider.** Generated confidence is not calibrated, and a
  local turn is ~6.5 s against needle's ~1.1 s. This is the category error both
  Jev and Needle 3 exist to avoid.
- **Inner-loop action selection** (a Jev call per action, as in the published
  Minecraft agent). Works for tiny actions with fast-changing state; forge's
  tool calls are coarse and need planning between them.
- **One engine per session.** The FFI layer forbids a second bind by design, and
  each copy costs real memory. Would require migrating to the pure-Rust
  `needle-infer` backend, which has no embedding API.
- **Self-hosted OpenJev as the default.** Its 94 ms median is a warm datacentre
  GPU over loopback; JevBench's own correction puts it near 338 ms against
  hosted 236–276 ms, and its calibration is 64.8 against Jev's 82.7 — worst
  exactly where the gate depends on it. Remains a reasonable cost/ownership
  choice for tool selection.
- **Four-mode cascade** (`direct`/`forced`/`hint`/`passthrough`). `hint` and
  LLM-side `forced` are deferred: forge can fill arguments on-device, which is
  what those modes exist to work around. The mode enum is structured so they can
  be added without reshaping the branch.

## §10 Out of scope

- Interactive REPL — already designed and planned on `main` as Phase B /
  sub-project 6b (`2026-09-24-interactive-chat-ui-design.md`). Not this
  document's to specify.
- Editor integration for Cursor and VS Code, which do not speak ACP —
  sub-project E, specified separately.
- Streaming model output — sub-project F, specified separately. `ModelProvider`
  exposes only a request/response `complete`, so no surface can stream today.
- ACP and `forge serve` alignment — sub-project C.
- The `bootstrap` / `update` command surface and its relationship to
  `install.sh` — sub-project D. §16 records the requirements it must satisfy,
  because the Session core must not assume a setup step that will not exist.
- Any training or fine-tuning pipeline. This spec produces the substrate only;
  §15's local calibration is a threshold fit over the decision log, not training.
- An in-process Rust port of a calibrated decision model via Candle. Recorded in
  §13 as the roadmap target for keeping calibrated probabilities single-command;
  the model choice must be settled first.
- Migration to `needle-infer`, and Needle 2 support. Both previously assessed;
  neither is needed here.

## §11 Egress budget: what may leave the machine

**Source code is never sent to a remote decision plane.** Not per turn, not
ever. This section is normative and overrides any convenience elsewhere in the
design.

The mechanism is the project graph, used the way `graft` uses its own: graph
operations are deterministic, local, and model-free, and what gets sent is an
*abstraction over* the code rather than the code.

### The only constructor

A `StatePayload` builder is the sole way the `state` field of any decision-plane
request may be constructed. No call site assembles `state` by hand; that is what
makes the policy auditable rather than aspirational.

### Tiers

| Tier | Contents | Model needed | May go to a remote plane |
|---|---|---|---|
| `none` | Prompt text and tool names/descriptions only. No project content. | no | yes |
| `structure` | Plus ranked file paths, symbol names, kinds, and **signatures** — `graft skeleton`'s trick, roughly a tenth the tokens of the bodies. | no | yes (default) |
| `summaries` | Plus cached per-symbol summaries and crux excerpts (the few lines carrying the logic). | yes, once at build time | only when explicitly enabled |
| `source` | File contents. | — | **never** |

`source` reaches only the generation-plane model the operator configured, for
files that model asked for. It is not a decision-plane tier.

### Config

```toml
[decision]
provider = "needle"        # names an entry in [decision.providers] (§13)
egress   = "structure"     # none | structure | summaries
```

- `local_only = true` prunes any non-local provider entirely, as it already does
  for routing.
- A provider reporting `is_local() == true` makes the tier moot — nothing
  leaves — so `summaries` is a reasonable default there and a deliberate
  opt-in for a hosted one.
- Before any egress, the payload passes a secret scan (the same patterns
  `forge auth` already knows) and refuses rather than redacts on a hit: a
  redacted payload that still describes a credential's location is not obviously
  safe.

### What the graph must gain

Forge's `SymbolInfo` is `{name, kind, file, line}` today — no signature, so the
`structure` tier cannot be built from it. Two additions, both deterministic:

- `signature: Option<String>` on `SymbolInfo`, captured during the existing parse.
- `summary: Option<String>` and `crux: Option<String>`, populated only by an
  opt-in deep pass and cached by content hash so a rebuild touches only changed
  files.

Ranking reuses `context(query, limit)`, which already returns scored hits with
reasons. `graft`'s refinement is worth copying: rank by in-edge coupling and
rank each scope separately before fusing, so one large subtree cannot crowd out a
small one.

### Why this is also the performance story

The same pruning that keeps source off the wire is what shrinks prompts to the
generation plane. `graft` reports 42% fewer tokens, 46% fewer tool calls and 60%
less wall-clock from exactly this (23/25/32% on SWE-bench Verified). Privacy and
speed are the same change here, not a trade.

## §12 Context management

A session used to build forge for hours will exceed any context window.
`history: Vec<Message>` cannot grow unbounded.

### Retention shape

Framing and recency are what matter; the middle is what compacts.

```
[ first K messages ]  [ ...... compacted ...... ]  [ last N messages ]
   the task, the         a running summary of        current working
   constraints, the      what was tried and           state, open
   plan — never          what it changed              threads
   evicted
```

- **Head, always retained.** The opening messages carry the task and its
  constraints. Losing them is how an agent forgets what it was asked.
- **Tail, always retained.** The live working set.
- **Middle, compacted** into a running summary when the budget is approached,
  oldest first.

Budget is enforced against the routed model's `max_context`, which
`ModelEntry` already carries.

### Where the graph replaces history

This is the part that makes the problem tractable rather than merely deferred.
Tool results are the bulk of transcript growth, and file contents are the bulk of
tool results. So a completed `read_file` is compacted to a **graph reference** —
path, symbol, content hash — not its bytes. The content is re-resolvable from the
graph on demand, at the tier the situation calls for.

The transcript therefore stops being the store of project knowledge. The graph is
the store; the transcript holds the conversation. That is what keeps a long
session viable, and it is the same abstraction §11 needs, so both are served by
one addition to the graph rather than two mechanisms.

Compaction is deterministic and model-free wherever possible: dropping a file
body in favour of a reference needs no model. Only the prose summary of the
compacted middle does, and it is produced once per compaction, not per turn.

## §13 The `DecisionPlane` trait

Jev must not be a hard dependency, and a self-hosted or on-device implementation
must be a first-class citizen rather than a fallback. So the batched-question
capability is a trait, and Jev is one implementation of it.

```rust
#[async_trait]
pub trait DecisionPlane: Send + Sync {
    fn name(&self) -> &str;

    /// True when answering involves no network egress. The egress policy in §11
    /// is enforced against this, so the guarantee lives in the type rather than
    /// in a comment.
    fn is_local(&self) -> bool;

    /// Answer every question in one call. Implementations that cannot batch
    /// answer sequentially; callers must not care.
    async fn ask(
        &self,
        state: &StatePayload,
        questions: &Questions,
    ) -> Result<Answers, ForgeError>;
}
```

`Questions`/`Answers` model the System One primitives — `choice` (with
`probabilities` and `confidence`), `noul`, `score` — because that is the richest
contract of the candidates and the others are expressible within it.

Planned implementations:

### Two independent axes

A provider is described by **what protocol it speaks** and **how it is reached**.
These are orthogonal, and conflating them was a mistake in an earlier draft:
tinyjev can equally be a sidecar forge supervises or an endpoint the operator
already runs, and the same is true of OpenJev and laya.

**`kind`** — the protocol, which selects the implementation:

| `kind` | Protocol | Notes |
|---|---|---|
| `needle` | in-process FFI | The default. No egress, no credential, nothing to install. Its confidence is not calibrated for risk classification, so it does not widen the gate until §15 earns that locally. |
| `systemone` | `POST /v1/systemone` | Serves hosted Jev **and** self-hosted OpenJev — one wire contract, differing only by URL and credential. |
| `tinyjev` | tinyjev's HTTP surface | MIT, Qwen3 + pointer head. Choice/Noul/Score with calibrated probabilities; ~65 ms per question, ~110 ms for three batched in one forward pass. Costs a Python runtime and 1.2 GB of weights. **Whether its HTTP shape matches `systemone` is unverified** — if it does, this collapses into that kind. |
| `laya` | laya's HTTP surface | Already implemented in forge. Scores 34.1% on JevBench's hard tier, last in field; permitted but documented as not recommended, and barred from the gate (§8). |
| `null` | — | Explicit "no decision plane". Keeps the absent case a normal code path rather than an `Option` threaded everywhere. |

**`mode`** — how it is reached:

| `mode` | Meaning |
|---|---|
| `in-process` | Linked into forge. Only `needle` today; a Candle port would join it. |
| `sidecar` | Forge spawns and supervises it (§14), bound to loopback. |
| `endpoint` | Something the operator already runs, or a hosted service. Forge only calls the URL. |

**`is_local()` is derived, never declared** — true for `in-process`, and for a
URL that resolves to loopback. That is what §11's egress policy keys on, so a
provider cannot mislabel itself: a tinyjev sidecar on `127.0.0.1` is local, and
the same tinyjev on a colleague's GPU box is not.

### Configuration

Mirrors the existing `[models.<name>]` shape, so it reads like the rest of
forge's config:

```toml
[decision]
provider = "needle"            # which of the below is active

[decision.providers.needle]
kind = "needle"
mode = "in-process"

[decision.providers.tinyjev]
kind    = "tinyjev"
mode    = "sidecar"            # forge starts it and cleans it up
command = "tinyjev serve --port 8077"
url     = "http://127.0.0.1:8077"

[decision.providers.openjev]
kind = "systemone"
mode = "endpoint"              # already running; forge just calls it
url  = "http://127.0.0.1:8088/v1/systemone"

[decision.providers.jev]
kind    = "systemone"
mode    = "endpoint"
url     = "https://api.typesafe.ai/v1/systemone"
key_env = "TYPESAFE_API_KEY"
```

Switching provider is a one-line change, and the same software can be run either
way without touching code. Adding SemIf, djev, or `openJev-verdict-2.0` (151M,
claimed ECE 0.0144 — small enough to be a serious candidate for the in-process
Rust port) means a new `kind` plus a config entry, with no change to `Session`
or the gate.

**Accuracy claims across these are not comparable.** tinyjev's 88% / 94.8% are
its author's own held-out set with its author's harness; Jev's 74.1% is
JevBench's *hard* tier. Different exams. The only cross-comparable figures are
JevBench's own (Jev 74.1, djev 69.5, OpenJev 65.5, SemIf 59.5, laya 34.1). Treat
every self-reported number as a reason to evaluate, not a ranking.

**This resolves the credential question:** a default forge has a working decision
plane with zero keys, zero egress and nothing to install. Everything above it is
an upgrade the operator chooses.

### The in-process Rust path

tinyjev is Qwen3 plus a custom pointer head, MIT-licensed, weights in
safetensors. Candle (HuggingFace's Rust ML framework) has Qwen support and reads
safetensors directly, so a plane running in-process with no Python and no sidecar
is feasible: load the backbone, reimplement the pointer head, one forward pass,
no generation.

That is the only route to calibrated probabilities that keeps the single-command
property, so it is the roadmap target rather than an idle option. The bounded
risk is reimplementing a custom head against a Python reference with no written
spec, and the model choice should be settled first — a 151M model is far more
shippable than a 596M one. Not planned here; recorded so it is not rediscovered.

## §14 Sidecar supervision

Opt-in planes may need a child process. A forge that leaves orphaned servers
behind is worse than one that never started them, so supervision is specified
rather than improvised.

`kill_on_drop` alone is insufficient: it only fires if `Drop` runs, so a
`SIGKILL`'d forge orphans the child. Cleanup is therefore layered, and the
portable backstop does not depend on forge running any code at exit:

| Platform | Mechanism |
|---|---|
| Linux | `prctl(PR_SET_PDEATHSIG, SIGTERM)` — the kernel signals the child when the parent dies |
| macOS | No PDEATHSIG equivalent. The child inherits a pipe and holds the read end; the parent's death closes the write end, the child reads EOF and exits |
| Windows | Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` |
| all | Spawn into a dedicated process group so the whole tree is signalled, not just the direct child |

Beyond teardown, a supervised sidecar must: bind loopback only, be health-checked
before the first request is routed to it, never block a turn while starting
(the turn proceeds on `NeedlePlane` until the sidecar is ready), and surface its
state through `forge doctor`.

This machinery is not specific to decision planes — `router_autostart` needs
exactly the same thing for OpenJev and laya today — so it lands as a reusable
supervisor rather than inside any one plane.

## §15 Earned autonomy: local calibration

The single-command default has no calibrated probabilities, so by §3 it never
auto-approves. That is safe and it is also the friction this project set out to
remove. The resolution is to earn the threshold instead of borrowing one.

**Every approval prompt is a labelled example.** When the operator answers `y` or
`n`, that is ground truth about whether the decision plane's classification at
that confidence was correct. §4 already records the confidence, the class and the
outcome of every gate decision.

So:

1. Ship prompting always. Nothing auto-approves.
2. Accumulate `(risk_class, confidence, approved?)` from real use.
3. Once a class has enough samples, fit a local calibration and compute the
   confidence at which observed approval was unanimous across a meaningful
   window.
4. **Offer** that threshold to the operator — show the evidence, let them accept.
   Never enable autonomy silently.

This is what every calibration source says to do anyway: *do not assume a
threshold calibrated for one question formulation transfers to another*, and
*re-derive on your own production distribution*. It is also the only mechanism
here that makes autonomy available without a credential, a sidecar, or a
borrowed benchmark — and it turns the learning goal into something that pays off
in the first release rather than a future one.

Defaults stay as §3 states; this only ever proposes moving them, with evidence,
and only upward in autonomy for classes the static floor already permits.

## §16 Bootstrap and lifecycle

The product requirement is one command. Concretely:

```
$ cargo install --git https://github.com/auser/forge forge-cli    # or install.sh
$ forge
  first run: building graph (396 files)… fetching needle weights (35 MB)… ready
› implement the DecisionPlane trait
```

- **No credential, no Python, no server, no separate init** on the default path.
- `forge bootstrap` exists as an explicit, idempotent command for CI and for
  re-running after an upgrade; `install.sh` invokes it so an interactive first
  run has nothing left to do. A bare `forge` with no `.forge/` performs the same
  work inline with visible progress rather than failing or silently degrading.
- `forge update` / `forge upgrade` manages the installed binary: resolve the
  latest release, download the asset for the host triple, **verify the published
  SHA-256** (`release.yml` already emits `forge-<triple>.tar.gz.sha256`), replace
  atomically, then re-run bootstrap for any new assets.

**Note this reverses an earlier decision in this document's history.** Weight
fetching was to stay gated behind `forge init`; the single-command requirement
overrides that. `init` becomes an alias for `bootstrap` rather than a
prerequisite.

The command surface itself — `bootstrap`, `update`, and their interaction with
`install.sh` — is **sub-project D** and gets its own spec. Requirements are
recorded here so the Session core does not assume a setup step that will not
exist.

## §17 Graph hygiene and freshness

Sections 11 and 12 both rest on the project graph — one sends graph-derived
payloads off the machine, the other replaces transcript content with graph
references. Two properties of today's graph make that unsafe as written.

### The graph indexes secrets

`GraphBuilder::walk` filters on a fixed `SKIP_DIRS` list and nothing else: no
`.gitignore`, no dotfile rule. A `.env` at the project root is walked, hashed and
classified as `Config` — the same file this repository just had to add ignore
rules for.

That makes the graph the wrong place for §11's guarantee to *start*. A symbol
named `STRIPE_SECRET_KEY` reaches the `structure` tier entirely legitimately,
because nothing upstream decided it should not exist.

So exclusion moves to the walk:

- Respect `.gitignore` (and nested ones) — if it is not in version control, it is
  not project knowledge.
- A built-in denylist independent of git, because `.gitignore` is not a security
  boundary: `.env*`, `*.pem`, `*.key`, `id_rsa*`, `credentials*`, `.netrc`,
  `.npmrc`, `.pypirc`.
- `.forgeignore` for project-specific additions.

**A file excluded from the graph is excluded from every egress tier by
construction**, and from `graph grep`, `graph context` and semantic search as
well. That last part is intended: an agent that cannot see `.env` cannot read it
into a transcript either. §11's payload scan stays as the second line of
defence, not the first.

### Nothing keeps the graph fresh

There is no file watcher anywhere in the workspace, and `forge doctor` already
reports `project graph: stale (4 added, 39 modified)` on this repository. In an
editor, files change every few seconds. A stale graph means the agent reasons
about code that no longer exists — and both §11 and §12 hand it that graph as
ground truth.

`ProjectGraph::is_fresh()` and an incremental `build()` already exist, so:

- **Check at turn start.** `is_fresh()` compares stored mtimes and hashes; it is
  cheap and correct.
- **Rebuild incrementally when stale**, within a budget. `build()` already only
  touches changed files.
- **If the rebuild exceeds its budget, proceed on the stale graph and finish the
  rebuild in the background.** A turn must never block indefinitely on indexing;
  a slightly stale graph degrades answer quality, while a stalled turn degrades
  the product.
- Record staleness in the decision log, so a bad decision made against a stale
  graph is diagnosable rather than mysterious.

A `notify`-based watcher is a later optimisation. It becomes clearly worthwhile
under §20, where one long-lived process serves several clients and can amortise
the watch across all of them.

## §18 Approval presentation contract

The gate yields `Approve | Prompt | Block`, and the spec has so far said nothing
about what `Prompt` *means* to a surface. A terminal asks `y/N`; an editor should
render a diff with per-hunk accept; `forge serve` returns a pending state a
client polls. Without a contract each surface invents its own — and because §15
treats every prompt as a labelled training example, inconsistent semantics
corrupt the calibration signal at its source.

So the request is typed, and the response vocabulary is fixed:

```rust
pub struct ApprovalRequest {
    pub id: ApprovalId,
    pub call: ToolCall,
    pub class: RiskClass,
    /// Present only when a decision plane answered; `None` under `NullPlane`.
    pub confidence: Option<f64>,
    pub preview: ApprovalPreview,
}

/// Enough for any surface to render the decision without re-deriving it.
pub enum ApprovalPreview {
    Diff { path: PathBuf, before: String, after: String },  // write_file / edit_file
    Command { program: String, args: Vec<String>, cwd: PathBuf },
    Read { path: PathBuf },
}

pub enum ApprovalResponse {
    Approve,
    /// Remember for this tool/command shape until the session ends.
    ApproveForSession,
    Reject,
    Cancel,
}
```

Surfaces differ in presentation and must not differ in vocabulary. `Diff` is
what makes editor integration possible at all: the editor renders its own diff UI
from `before`/`after` rather than forge trying to describe a change in prose.

`ApproveForSession` is the session-scoped trust discussed during design. It is
also the strongest calibration label available — an operator who trusts a shape
for a whole session is making a stronger statement than one who approves once.

## §19 Budget ceiling

`max_turns` bounds turns, not spend. A harness running all day against cloud
models needs a ceiling, and every input it requires already exists: `ModelEntry`
carries `cost_input_per_mtok` and `cost_output_per_mtok`, and §4 records usage
per decision.

```toml
[budget]
session_tokens = 500_000
session_usd    = 5.00
daily_usd      = 25.00
on_exceeded    = "prompt"   # prompt | stop
```

- Local models cost zero, so a local-only configuration is effectively
  unbounded — which is correct, and is the configuration the single-command
  default produces.
- A hosted decision plane's per-turn input tokens count toward the budget.
  Firing it speculatively on every turn (§1) is cheap, not free, and the budget
  is where that becomes visible.
- `on_exceeded = "prompt"` asks before continuing; `stop` fails the turn with a
  clear error. Neither silently truncates work.
- Spend is recorded in the decision log so overruns are attributable to the
  turns that caused them.

## §20 Process model: one engine, many clients

The warm engine in §2 is the design's main performance idea, and it quietly
assumes one long-lived process. Editors break that assumption: open Zed, Cursor
and a terminal and you get three forge processes, three engines, three copies of
35 MB of weights, three 8 s tool-surface installs, and three decision logs that
§15 cannot calibrate from because none of them sees the whole picture.

### Shape

One **daemon per machine**, holding what is genuinely singular, with clients
attaching over a local socket:

```
  forge (REPL) ─┐
  forge-acp ────┼──▶ unix socket ──▶ forged ──┬── the one needle engine
  editor ext ───┤    (named pipe            ├── per-project graphs
  forge serve ──┘     on Windows)            ├── sessions + decision log
                                             └── supervised sidecars (§14)
```

The split follows what is scarce. The **engine** is a process-global,
non-thread-safe singleton (§2), so exactly one may exist per machine. **Graphs**
are per project, and one daemon holding several is what lets a multi-root editor
workspace work at all. **Sessions** belong to clients but outlive any single
connection, so reconnecting an editor resumes rather than restarts.

### Rules

- **Auto-start, never a prerequisite.** The first client to find no daemon
  starts one. The user still types one command (§16).
- **Idle shutdown** after a configurable period with no attached clients, so a
  forgotten daemon does not hold 35 MB and a thread forever.
- **Degrade to embedded.** If the daemon cannot start or the socket is
  unavailable, the client runs the core in-process exactly as it does today.
  This is the same principle as everywhere else in the document: worse, never
  broken.
- **Loopback-equivalent only.** A unix socket in the user's runtime directory at
  mode `0600`; a named pipe with a matching ACL on Windows. No TCP by default —
  `forge serve` remains the deliberate, separately-configured network surface.
- **Version-matched.** A client refuses a daemon built from a different version
  rather than negotiating; `forge update` (§16) stops the old daemon as part of
  replacing the binary.

### What this makes possible

One decision log across every surface, which is what §15's calibration needs to
converge in reasonable time. One warm engine, so the second editor window costs
nothing. One graph watcher (§17) amortised across clients. And one place for
sidecars to live, so a tinyjev process is started once rather than per editor.

## §21 Delivery phases

This document is 20 sections and describes considerably more than one
implementation plan's worth of work. It is split into phases that each ship
something usable, and each gets its own plan.

| Phase | Contents | Why here |
|---|---|---|
| **A1** | `Session` extraction, dispatch-before-routing, engine and log ownership (§2, §20's runtime split), the decision log (§4) | Smallest shippable improvement — and it begins collecting the data every later phase is designed against |
| **A2** | `DecisionPlane` trait and impls (§13), risk taxonomy and gate (§3), approval contract (§18) | Designed against A1's real numbers rather than a published benchmark |
| **A3** | Egress tiers (§11), graph signatures and summaries, ignore rules and freshness (§17) | Different crate, its own testing story; nothing in B/C/E depends on it |
| **A4** | Daemon, sidecar supervision, bootstrap and update (§20, §14, §16) | Lifecycle. B, C and E all attach to this |
| **A5** | Context budget (§12), earned autonomy (§15), spend ceiling (§19) | Needs A1's log to have accumulated real traffic |

**A1 is deliberately first because it is the measurement.** Two assumptions in
this document are unvalidated: the decline-retry's hit rate, and the behaviour of
any remote decision plane, which has never executed in this project. A1 ships the
decision log, which measures the decline rate on real work — so A2 is specified
from observed numbers instead of from `jev-gateway`'s, whose own conclusion was
*measure on your own work*.

Streaming (sub-project F) is **not** deferred behind all of these. It is
independent of the decision plane and blocks both the REPL (B) and editor
integration (E), so it runs in parallel from A2 onward. A surface that cannot
stream is a worse terminal, and the editors are the goal.

## Sources

- TypeSafe API reference — <https://docs.typesafe.ai/api.md>
- Tool-call risk benchmark — <https://webofmike.com/jev-benchmark/>
- Decision-plane architecture — <https://www.vincirufus.com/en/posts/jev-decision-plane-agent-architecture/>
- Calibration and production trade-offs — <https://agentunicorn.ai/research/jev-architecture-open-source>
- `jev-gateway`, pre-LLM tool routing for coding agents — <https://github.com/vinilana/jev-gateway>
- Tool-call gating pattern — <https://openrouter.ai/docs/cookbook/building-agents/gate-tool-calls-with-jev>
- Harness middleware and question batching — <https://www.langchain.com/blog/building-a-harness-with-jev>
- Decision-model comparison — <https://huggingface.co/blog/sora-2/jev-ai-vs-djev-vs-laya-vs-openjev-vs-semif-which-d>
- Open-Jev benchmarks — <https://zefan-cai.github.io/open-jev/benchmarks/>
- Needle 3 — <https://github.com/cactus-compute/needle>
- Graft, graph-first context reduction for coding agents — <https://github.com/nanonets/graft>
