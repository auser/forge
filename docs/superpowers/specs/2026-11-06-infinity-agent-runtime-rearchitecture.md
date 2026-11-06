# Rearchitecting forge's runtime onto Infinity

**Date:** 2026-11-06
**Status:** proposed — spike landed, sequencing not yet approved
**Spike:** [`../spikes/infinity-runtime/`](../spikes/infinity-runtime/)

Three projects were put forward for this rearchitecture:
[Infinity](https://infinity.hydro.run/), [Hydro](https://hydro.run/) and
[tellus](https://tellus.rs). This spec takes each one as far as the evidence
supports and no further. **One is adopted, two are not** — reasons below, and
the cost of overriding either rejection is stated so the call stays reversible.

## The verdict

| Project | Version | Verdict |
|---|---|---|
| `infinity-agent-core` | 0.2.0 (crates.io) | **Adopt** as the execution engine beneath `AgentService` |
| `hydro_lang` | 0.17.0-alpha.6 | **Not as a dependency** — adopt one idea from it |
| `tellus` | 0.5.3 | **Reject** — wholly subsumed |

## Why Infinity, specifically

Forge and Infinity are solving the same problem, and Infinity is further along
on the part forge has no answer for at all. Its own builder doc states the
thesis:

> Analogous to an actor system: threads (root agents and their subagents) are
> the actors, and the `InputSender` is how messages are delivered to them.
> `AgentSystem` itself holds no running state — each `step()` loads the
> thread's state from the stores, runs one slice, and drops it.

That last sentence is the whole value. Forge's agent loop holds a run's state
*in the loop*; a parked run (`waiting_for_approval`) is a live task waiting on
a channel. Infinity's run state is **data in a store between turns**, so an
agent costs ~103 KB while it waits and waking it is a store read. Everything
forge wants and cannot currently express follows from that one property:
parallel subagents, sleeping without polling, and thousands of concurrent
agents in one process.

What forge gains that it genuinely does not have today:

- **Typed subagent spawning.** `SpawnContext::{InheritUpTo(n), Fresh}` — a
  child either inherits history to a cutoff or is severed from it. Forge has
  no subagent primitive at all.
- **`DeferQueue`** — sleep/wake with no polling loop.
- **Compaction across ancestors.** `load_history_with_ancestors` walks the
  ancestor chain to the most recent compaction summary.
- **A serverless step API** for free, should `forge serve` ever want it.

### What the spike proved, and what it cost

The adoption is cheap because **forge keeps its own stack at every seam that
matters**. Verified by the spike, not assumed:

1. `infinity-provider-protocol` is deliberately SDK-free ("no HTTP stack, no
   LLM SDKs"). Its `ModelProvider` trait is near-isomorphic to forge's own,
   and `StaticModel::new` takes `Arc<dyn ModelProvider>`. So forge-providers
   plugs in behind an adapter and **the entire decision plane survives
   untouched** — Needle, Jev escalation, static rules, routing profiles, cost
   books, credential detection, the local-first ladder. That plane is forge's
   actual differentiator and Infinity has no opinion about it.
2. `ConversationStore` and `StateStore` are pluggable traits — the crate ships
   in-memory ones and notes "DSQL in Lambda, in-memory for CLI". **forge-session's
   append-only JSONL store becomes a `ConversationStore`**, so redaction,
   `forge resume` and the on-disk format all stay.
3. Neither `rig-core` nor the AWS SDK is pulled in. Those live in sibling
   provider crates forge never depends on.
4. Infinity's `ErrorClass::{Transient, Throttled, ContextOverflow, Fatal}` is
   forge's `ProviderFailureKind` by another name, and its `ToolDefinition` is
   forge's. These are adapter-shaped differences, not redesigns.

**The one hard constraint the spike found:** Infinity's local driver spawns
`!Send` futures. Without a `LocalSet` it panics on first thread launch
(`spawn_local called from outside of a task::LocalSet`). So the Infinity system
must own one dedicated thread behind a job channel.

That is not a new pattern here — it is the *third* instance of one
ARCHITECTURE.md already names: `NeedleEngine` owns `libneedle` on a dedicated
OS thread (mpsc jobs, oneshot replies), and `TerminalIo` owns `rustyline` the
same way. The integration therefore has a precedent in-tree to copy rather
than a design to invent. Only running the spike surfaced this; it type-checks
fine.

## Why not Hydro (as a dependency)

Hydro is a genuinely beautiful system solving a problem **forge does not
have**. Its value is location-oriented programming across machines: a single
program spanning processes and clusters, with distribution in the types and
eventual determinism enforced at compile time. Forge is one process on one
developer's machine; `forge serve` is an adapter over the same in-process
`AgentService`. There is no consensus protocol, no replication, no
cross-region coordination to make correct.

Against that, the costs are concrete: `0.17.0-alpha.6` is alpha, and the
determinism discipline is not free — code whose result could vary with timing
or duplication does not compile without algebraic proofs or explicit
`nondet!` markers. That is a large tax levied on every stream in the codebase
to buy guarantees about a topology forge does not have.

**The idea worth taking, though, is cheap and real.** Hydro's core insight is
that *ordering, duplication and retry semantics belong in the type, not in a
comment*. Forge's event streams carry exactly those hazards today and express
none of them — reattach replays a backlog, retries can duplicate, and
cross-process `attach` interleaves store reads with a live broadcast. Forge
already has a bug-shaped hole here: the committed test
`subscribing_to_a_run_owned_by_another_process_creates_no_broadcaster` exists
because a foreign run registered a broadcaster nothing published to. Stream
metadata newtypes on forge's run events would have made that unrepresentable.

Worth noting: **Infinity is itself a Hydro project**, co-led out of UC
Berkeley's Sky Computing Lab. Adopting Infinity already banks the Hydro
thinking at the layer where forge can use it. If Hydro-the-crate is wanted
anyway, the honest place for it is a future multi-machine `forge serve`
fan-out — a separate effort, after Infinity lands, and not before `hydro_lang`
is out of alpha.

## Why not tellus

tellus offers typed actors, supervision trees, death watch, event sourcing and
QUIC clustering. Checked one at a time against forge:

- **Event sourcing with replay/snapshots** — this is `forge-session` plus
  `forge-runtime::replay`, shipped and in use by `forge resume`.
- **Mailboxes and typed actors** — this is what Infinity provides, in a crate
  whose actors are *agents*, not generic state machines. Adopting both means
  **two actor runtimes in one binary**, which is strictly worse than one.
- **Supervision trees** — forge's failure model today is `catch_unwind` on the
  engine thread plus typed errors, and it fits a single-process CLI. Nothing
  in the backlog asks for restart-with-backoff of a supervised hierarchy.
- **QUIC clustering with mutual TLS** — solves distributing actors across
  nodes. Forge is local-first by design; this is the Hydro objection again.

So every offer is either already built, better served by the crate being
adopted, or aimed at a problem forge does not have. Added to that:
`0.5.3`, 619 lifetime downloads, first published three months ago. Taking a
pre-1.0 dependency is reasonable when it carries weight no existing code
does — Infinity clears that bar, tellus does not.

**If tellus goes in anyway**, the one defensible slice is supervision: restart
policy and death-watch ordering around the Infinity thread. That is roughly
50 lines over what exists, and should be written as 50 lines before it is
taken as a dependency.

## Sequencing

Deliberately staged so forge is shippable and green at every step, and so the
hand-rolled loop is deleted only once the replacement has proven parity. No
front end changes until step 5 — all five (`forge run`, chat, `serve`, `mcp`,
`acp`) sit over `AgentService` and keep sitting over it.

| # | Slice | Done when |
|---|---|---|
| 1 | `forge-agents` crate: `ModelProvider` and `ConversationStore` adapters over forge-providers and forge-session | Spike's test passes against forge's real provider and real JSONL store |
| 2 | `InfinityEngine`: the system on a dedicated thread behind a job channel, copying `NeedleEngine`'s shape | A run completes through it; panic containment covered |
| 3 | Tool dispatch and approval: forge tools as Infinity tools, `RiskLevel` gating mapped onto `UserChoice`/`ChoiceId` | `waiting_for_approval` round-trips via `forge_run_input` as it does now |
| 4 | Config switch, default off (`[runtime] engine = "legacy" \| "infinity"`), `forge doctor` reports which answers | `just verify` green under both engines |
| 5 | Subagents — the capability that motivated the move | A run fans out and rejoins, recorded in the session log |
| 6 | Delete the legacy loop | Parity proven; one engine in the tree |

The engine switch in step 4 follows the pattern forge already uses for
`needle` vs `rten` backends: two implementations behind one seam, config
chooses, `doctor` explains the choice. It is also the rollback.

## Risks, stated plainly

- **`infinity-agent-core` is 0.2.0** with 49% doc coverage, and
  `event_processor.rs` is a single 220 KB file. The API will move under us.
  Mitigated by steps 1 and 4: everything crosses an adapter forge owns, and
  the legacy engine stays until step 6.
- **`!Send` driver** constrains the system to one thread. Known, and the
  in-tree pattern is proven twice over — but it does mean Infinity's
  concurrency is cooperative within that thread, not spread across cores.
  Worth measuring before step 5 is relied on for throughput.
- **`rhai` is a transitive dependency** of `infinity-agent-core` — an embedded
  scripting engine, which is new attack surface in a process that already
  executes tools under approval gating. Needs a look at whether forge's path
  can reach it at all before step 4.
- **Approval parking is the integration's sharpest edge.** Forge parks a run
  mid-tool-call awaiting `y`; Infinity models user input as `UserChoice`. If
  the mapping is lossy, risk gating is what breaks — so step 3 lands before
  the config switch, not after.
