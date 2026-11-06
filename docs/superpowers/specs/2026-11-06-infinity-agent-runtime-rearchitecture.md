# Agent runtime: what to adopt from Infinity, Hydro and tellus

**Date:** 2026-11-06
**Status:** proposed
**Spike:** [`../spikes/infinity-runtime/`](../spikes/infinity-runtime/)

Three projects were put forward for a rearchitecture:
[Infinity](https://infinity.hydro.run/), [Hydro](https://hydro.run/) and
[tellus](https://tellus.rs). The goal they are being measured against:
forge should *contain* the easiest extensible AI framework — future-proof,
fast, and as cheap as possible to run.

## The verdict

| Project | Version | Verdict |
|---|---|---|
| `infinity-agent-core` | 0.2.0 | **Idea, not dependency** — drop per-turn state; keep forge's runtime |
| `hydro_lang` | 0.17.0-alpha.6 | **Idea, not dependency** — put stream guarantees in the type |
| `tellus` | 0.5.3 | **Reject** — wholly subsumed |

**No new framework goes under `AgentService`.** Forge's own trait seams stay
the framework. The one real gap — concurrent, model-callable fan-out — is
built on primitives forge already owns.

### Correction to the first draft of this spec

The first version of this document recommended adopting Infinity as the
execution engine, on the claim that *"forge has no subagent primitive at
all."* That claim is wrong, and it was the load-bearing one. Checking the
tree afterwards:

- `AgentService::fork_session(session, at)` already copies an event-log
  prefix into a self-contained session with a `session_forked` provenance
  marker. That **is** `SpawnContext::InheritUpTo(n)` — forge just exposes it
  as the user-facing `/fork` instead of as a tool a model can call. A fresh
  session is `SpawnContext::Fresh`.
- `forge-task` (1563 lines) is a durable task graph: `TaskPlan`, `TaskNode`,
  `TaskState`, `TransitionRequest`, `CapabilityNeed`, `EffectClass`,
  `EffectRequest`, `WorkspaceCheckpoint`, `Verification`, `JsonlTaskStore`,
  and `DurableTaskState::{Queued, Running, Parked, Failed, Succeeded}` as the
  "stable, transport-neutral lifecycle shown by every Forge front end".
- `forge-runtime::workflow` (1571 lines) already orchestrates a
  plan→build→verify→review development workflow over that graph.
- Runs already stream per-run (`subscribe`/`attach`, cross-process) and
  `forge-server` already spawns them concurrently.

So forge is not missing an agent runtime. It has an event-sourced store with
replay, durable parking, context-cutoff forking, and an orchestrator — and a
*more* opinionated model than Infinity's, because it knows about workspace
checkpoints, effect classes and verification. Infinity knows about none of
those and would not replace them.

## Why that flips the call

Measured against the five goals, adopting Infinity loses on four of them:

- **Easiest** — a 6-slice engine migration running two runtimes side by side,
  versus a spawn tool over a function that already exists.
- **Future-proof** — 0.2.0, 49% doc coverage, a single 220 KB
  `event_processor.rs`. The insurance forge actually has is its own
  `forge-core` seams; putting a 0.2.0 crate *underneath* them trades that away
  and adds an adapter to maintain forever.
- **Fast** — the spike's hard finding: Infinity's local driver spawns `!Send`
  futures and needs a `LocalSet`, so its concurrency is cooperative within one
  thread. Forge is already multi-threaded Tokio with no such constraint.
  Adopting Infinity would make fan-out *slower*, not faster.
- **Cheapest** — a wash at best. Forge's decision plane (embedded Needle free
  → local models free → subscriptions → metered APIs last) already *is* the
  cheapest-possible answer, and Infinity has no opinion about it. Any
  migration must preserve it, so it cannot improve it.

**Extensible** is the one it would not lose on — Infinity's seams really are
pluggable, which the spike proved. But forge's seams are too, and they are
already load-bearing for five front ends.

The spike was still worth running: it is what established the `!Send`
constraint and the provider-seam isomorphism, and it is what makes this
recommendation evidence rather than taste. It stays in-tree as the record of
*why* the answer is no.

## What is actually missing

One thing: **concurrent, model-callable fan-out.** Every primitive it needs
exists; nothing composes them, and no tool exposes them to a model. That is
the whole gap, and it is small.

## Plan

Three slices, all additive, no new dependency, each shippable.

| # | Slice | Done when |
|---|---|---|
| 1 | A `spawn` tool over `fork_session` (inherit-to-cutoff) and a fresh session (severed), returning a child run id | A model fans out one child and the parent's log records the provenance |
| 2 | Bounded concurrent scheduling of child runs — `JoinSet` plus a semaphore, per `async-joinset-structured` / `async-bounded-channel` | N children run in parallel across cores, backpressured, cancellation propagating from the parent |
| 3 | A `join` primitive collecting child outcomes into the parent's event log | A fan-out/rejoin round-trips through `forge resume` |

Then, independently and cheaply:

| # | Slice | Done when |
|---|---|---|
| 4 | **Infinity's idea:** stop holding run state in the loop across turns — load from the store, run a slice, drop it | A parked run holds no task; `attach` is a store read |
| 5 | **Hydro's idea:** ordering/retry/duplication guarantees in the *type* of a run's event stream | The foreign-broadcaster class of bug is unrepresentable, not just tested |

Slice 4 is where Infinity's actual insight lands, and forge is most of the way
there already: the JSONL store plus `replay` can reconstruct a conversation
today. The loop simply does not let go between turns.

Slice 5 pays a debt with a name. The committed test
`subscribing_to_a_run_owned_by_another_process_creates_no_broadcaster` exists
because a run live in *another process* is not terminal, so `subscribe`
registered a broadcaster nothing ever published to and nothing pruned. Typed
stream metadata is what makes that a compile error instead of a regression
test.

## Why not tellus

Each of its offers, against forge:

- **Event sourcing with replay/snapshots** — this is `forge-session` plus
  `forge-runtime::replay`, shipped and used by `forge resume`.
- **Typed actors and mailboxes** — forge's runs already have ordered input
  delivery (`send_input`), per-run streams, and durable parking.
- **Supervision trees** — forge's failure model is `catch_unwind` on the
  engine thread plus typed errors, which fits a single-process CLI. Nothing
  asks for restart-with-backoff of a supervised hierarchy.
- **QUIC clustering with mutual TLS** — distributes actors across nodes.
  Forge is local-first by design.

Plus 0.5.3 and 619 lifetime downloads. A pre-1.0 dependency should carry
weight no existing code does; this one carries none.

## Why not Hydro as a dependency

Hydro's value is correctness *across machines*: distribution in the types,
eventual determinism enforced at compile time, for consensus and replication.
Forge is one process on one machine; `forge serve` is an adapter over the same
in-process `AgentService`. There is no protocol here to make correct, and
`0.17.0-alpha.6` plus a `nondet!`/algebraic-proof obligation on every stream
is a large tax for guarantees about a topology that does not exist.

Worth noting Infinity is itself a Hydro project (UC Berkeley Sky Computing Lab
with AWS), so slices 4 and 5 bank the Hydro thinking at the layer forge can
use. If Hydro-the-crate is ever right, it is for a multi-machine `forge serve`
fan-out — a separate effort, after it leaves alpha.

## Where the money actually is

"As cheap as possible" is a token question, not a framework question. No
runtime swap moves it. The levers forge already has:

- **The fast path** — Needle filling a tool call directly without waking a
  generation model is the single largest saving available, and it is already
  built. Widening what it covers is cheaper than any architectural change.
- **Compaction** — Infinity's `load_history_with_ancestors` walks ancestors to
  the latest compaction summary. Forge has `ContextArtifactsConfig` and an
  observer/budget; ancestor-aware compaction becomes relevant exactly when
  slice 1 lands and child contexts start nesting.
- **The ladder itself** — free embedded decisions, free local generation, then
  subscriptions, then metered keys. Keep it; it is the differentiator.

## Risks

- **Slices 1–3 add concurrency to a runtime whose approval gating assumes one
  foreground run.** `RiskLevel` gating and `waiting_for_approval` are the
  sharp edge: a parked child must not be able to consume the parent's approval
  channel. This is the thing to design before slice 2, not after.
- **`service.rs` is 3787 lines** and is where this lands. The honest risk is
  that fan-out makes it worse. Slices 1–3 should extract a scheduling module
  rather than grow the file.
- **Not adopting Infinity means forge keeps building its own.** That is the
  cost of this recommendation, stated plainly: if `infinity-agent-core` reaches
  1.0 with a `Send` driver and forge has meanwhile hand-rolled fan-out,
  revisit. The spike is kept in-tree so re-testing is cheap.
