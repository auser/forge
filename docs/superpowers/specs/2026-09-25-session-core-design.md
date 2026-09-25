# Session core: a decision plane for forge's agent loop

**Status:** design, awaiting implementation plan
**Date:** 2026-09-25
**Scope:** sub-project A of three. B (interactive REPL) and C (ACP + serve alignment) get their own specs.

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
│ 0. RETRIEVE ─── needle.embed → top-K tools      seam only; no-op while ≤10 tools
│
│ 1. DECIDE ───── two calls fired CONCURRENTLY (they contend for nothing:
│                 needle owns a blocking OS thread, Jev is async network I/O)
│
│      needle.tool_call(prompt, tools) ─────────────────▶ ~1.1 s   name + arguments
│      jev.batch{tool, safety, model}  ────▶ ~250 ms              choices + probabilities
│
│                 Jev's answers land first and are free by the time needle returns.
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

**Needle and Jev run concurrently.** This is the main performance idea. Needle
occupies its own thread; Jev is network I/O on the tokio runtime. Running them
together hides Jev's latency completely behind needle's, which makes the
fall-back-to-Jev branch cheap: on a decline, Jev's answer is already in hand and
the retry costs only needle's fill, not a round-trip.

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

**All Jev questions go in one request.** The API takes `questions` as a map and
processes them together with shared latency and billing; input tokens are
charged once for the shared `state` and answers are free. Asking tool choice,
safety and model selection separately would be three round-trips for no benefit.
Forge asks one question today; that is the change.

**Jev is speculative and sometimes discarded.** When needle answers confidently
we throw Jev's reply away. That costs one `state`'s worth of input tokens
(~$0.0001) and no wall-clock. Accepted deliberately.

### Batched request shape

```json
{"state": "<prompt + relevant context>",
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

    /// Shared, queued. `None` when this build has no engine
    /// (`forge_needle::HAS_EMBEDDED_BACKEND == false`).
    engine: Option<Arc<NeedleEngine>>,
    /// Whether the engine's tool surface has been installed. Session-scoped, so
    /// the ~8 s cold install is paid once per conversation rather than per turn.
    warmed: Arc<AtomicBool>,

    /// Decision-plane client. `None` without a credential; everything still works.
    jev: Option<Arc<JevClient>>,

    gate: Gate,
    log: DecisionLog,
    /// Session-scoped memo, keyed by a hash of (prompt, ordered tool set): an
    /// identical request reuses its decision instead of re-deciding.
    memo: HashMap<u64, DecisionOutcome>,

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

All three surfaces hold a `Session` and call `turn`. That is the whole point of
the extraction: the warm engine, the gate, the memo and the log acquire an owner
whose lifetime matches a conversation, instead of being process-global or
rebuilt per request.

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
3. **A model signal may widen only under all of:** the class is `ReadOnly`, the
   mode already permits auto for `ReadOnly`, and **Jev's** `safety` answer has
   confidence exactly **1.000**. Needle's confidence never widens (§3, "Without
   Jev").
4. **A model signal may always narrow.** Low confidence on a call the mode would
   have auto-run escalates it to a prompt — including in `auto` mode.

Rules 1 and 2 are deliberately separate, and the distinction matters: the floor
constrains what a *model* may do, not what the operator configured. `approval =
"auto"` is a human accepting the consequences, and the gate does not overrule it
on a destructive call; it may still narrow to a prompt under rule 4 when the
decision plane is unsure. A model can only ever make forge quieter than `prompt`
for read-only work, or noisier than `auto` when it has doubts.

### Why exactly 1.000

From a 60-case benchmark on this precise task (34 clear, 14 ambiguous, 12
adversarial): overall accuracy 91.7%; the 0.9–1.0 bin held 50 of 60 predictions
at 98.0% accuracy; **every incorrect answer carried confidence below 1.000**,
and all 40 answers at exactly 1.000 were correct.

Accuracy was also non-monotonic in confidence — the 0.4–0.5 bin scored 100%
where 0.1–0.2 scored 0% — which is what an uncalibrated signal looks like. A
tunable slider invites a value the evidence does not support, so the threshold
is a constant, not a config knob. Gating at 0.9 would auto-approve a
misclassification roughly 2% of the time.

Thresholds are **per question**, never shared: a threshold calibrated for
"which tool?" does not transfer to "is this safe?", even when the questions are
logically equivalent.

### Without Jev

The gate uses the static floor plus needle's confidence and **never
auto-approves** — needle's confidence head is not calibrated for this task and
no benchmark supports a cutoff for it. Behaviour matches today's, minus the
misdiagnosis fixed earlier. Gaining autonomy requires a credential; losing the
credential loses autonomy, not function.

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
| No Jev credential | No batched call. Gate never auto-approves; route escalation skipped |
| Jev `429` / `529` | Exponential backoff; on exhaustion treat as absent for this turn |
| Jev `422` | Log and treat as absent. Never retried — the request is wrong, not the service |
| Jev `401` | Log once per session, then treat Jev as absent |
| Jev slower than needle | Ignored for this turn; needle's answer stands |
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

- Interactive REPL — sub-project B.
- ACP and `forge serve` alignment — sub-project C.
- Any training or fine-tuning pipeline. This spec produces the substrate only.
- Migration to `needle-infer`, and Needle 2 support. Both previously assessed;
  neither is needed here.

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
