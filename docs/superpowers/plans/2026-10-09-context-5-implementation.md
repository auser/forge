# CONTEXT-5: Bounded asynchronous observer jobs

Status: Approved. Issue #42 / epic #37; baseline PR #53 (`68e27b8`).
Architecture: `docs/superpowers/specs/2026-10-08-context-engine-design.md`.

## Approved product policy

- Disabled by default. Enabling requires an explicit observer model, never an
  automatic active-model fallback.
- Hosted session-derived egress additionally requires `allow_remote = true`.
  Existing local-only restrictions and redirect/endpoint policy always win.
- Dedicated observer ceilings: $0.05/session and $0.25/project/UTC day,
  configurable, separate from interactive spend. Reserve estimated maximum
  cost durably before a call. Unknown prices refuse dispatch; explicitly
  zero-priced local models are allowed.
- One observer job in flight per project by default, enforced across
  cooperating processes. Durable pending work resumes on later startup.
- Active turns and CLI exit never await observer provider completion.
- Two retries (three total attempts). Failed/unknown attempts may still incur
  cost; charge conservatively when usage is missing. At-most-once ledger
  commitment is guaranteed, not exactly-once external provider invocation.
- Keep CONTEXT-4's version-independent overlap rejection. Same committed job
  reconciliation is safe; other overlapping versions conflict until an explicit
  future rebuild/migration.
- Reviewed evaluation fixtures require zero unsupported durable claims. Scripted
  tests do not qualify any live model; live evaluation requires separate consent.
- No live memory injection or automatic project promotion in this ticket.

## Design

### Pure chunking and observer contract (forge-context)

Read durable session events, not transient provider output. Project a minimal
explicit allowlist of conversational input/output kinds and redact each field
before encoding. Never serialize arbitrary events or raw tool arguments into a
provider request. Commands/approvals/credentials require careful exclusion;
source records remain available locally for anchor validation.

Use inclusive session-log positions and canonical serialized request sizes.
Only source prefixes belonging to completed runs are eligible. Chunk boundaries
are deterministic for a frozen source prefix, fixed observer version and limits;
do not let append arrival timing alter already-enqueued ranges. Greedy chunks
are bounded by byte and conservative token budgets, including framing, with
an explicit blocked/unsupported status for a single event that cannot fit.
Do not silently truncate a source event while claiming complete observation.

Source fingerprints and deterministic job IDs bind project-local session,
range, observer version and model/prompt policy. Fork jobs address child-local
ranges only, beginning after copied prefix plus provenance marker. Parent jobs
cannot target child ledgers. Source mutation is a conflict, never an inferred
new version of the same job.

Observer completion is a strict, bounded JSON envelope: reject unknown fields,
unsupported scope/kinds, tool calls, malformed/oversized output and anchors not
equal to the job's source range. Permit an empty observations array. Build the
final batch only through ValidatedObservationBatch with the session redactor
and freshly verified target snapshot.

### Durable job state and spending (forge-context)

Reuse private pinned native filesystem primitives, bounded atomic index,
content-free errors and memory implementation. Keep a dedicated namespace;
do not weaken existing ledger/artifact locks or paths.

State includes pending, leased, committed, retryable/failed/blocked; bounded
attempt count, deterministic identity, source fingerprint, version, target
session, next-attempt time, lease token/expiry and per-attempt reservation.
Claim/reserve is one atomic project transaction. Never advance observed coverage
until the ledger commit succeeds. Reconcile a crash after ledger commit but
before queue completion only against the exact matching range/version/source.
Stale workers cannot finalize another lease.

Persist cost/reservations with overflow-safe arithmetic and bounded cardinality.
Reported missing usage/price is unknown, never free. Retry reservations are new
charges; do not release ambiguous timeout/crash reservations as if no call
happened. Failed persistence refuses dispatch rather than silently bypassing
spending bounds. Provider-reported usage/cost is evidence, not a promise that
the estimate perfectly predicts external billing.

### Runtime supervisor

Use ModelProvider::complete with no tools and a fixed prompt/schema/version;
reuse the production provider factory and egress enforcement. Never route
observers through the interactive agent loop, approvals or tool execution.
Provider concurrency, per-call timeout and shutdown cancellation are bounded.
Avoid unbounded task-per-event spawning or blocking filesystem work on async
executors. A coalesced session notification and startup discovery can recreate
pending chunks after a CLI exits before durable enqueue.

Readiness/status exposes model, remote policy, ceilings, pending/running/failed/
blocked/committed counts, reserved/known/unknown cost and version conflicts,
without source content. Do not mix observer costs into interactive decision logs
without a typed purpose distinction.

Freeze observation fork inheritance at the actual session fork operation when
enabled, before allowing child-local observer jobs. A failed derived fork must
fail closed for child observation, not abort ordinary conversation or later
silently import parent observations that arrived after the fork.

## Tasks / ownership

1. **Context jobs/chunks/schema**
   Add pure bounded source projection/chunk/parser/evaluation fixtures and durable
   queue/budget APIs; factor existing native namespace transaction as necessary.
   Publish API early to runtime worker. Add memory/filesystem and independent-
   process tests for duplicate claims, spend reservation, retries/crash recovery,
   corruption, source mutation and fork targets.
   VALIDATE: context tests/clippy and Windows cross-target.

2. **Runtime supervisor and lifecycle**
   Add separate observer module/tests and optional AgentService injection.
   Startup recovery plus non-blocking completed-run notifications; clean abort
   without waiting on providers. Reconcile durable queue with committed ledger.
   Expose content-free status API. Add scripted provider schema/timeout/retry/
   budget/egress-failure tests, active-turn timing barrier and fork races.
   VALIDATE: runtime/session/core tests and clippy.

3. **Configuration, provider wiring and status**
   `[observer]` explicit enabled/model/allow_remote/session_usd/daily_usd.
   Defaults off, absent model, remote false, 0.05/0.25. Reject invalid/nonfinite
   budgets. Production wiring uses same registry/factory and local-only policy;
   no credentials in committed fixtures. Add status CLI entry using runner state.
   VALIDATE: config/CLI tests, local-only fixtures, workspace checks.

4. **Independent review and actual CI**
   Security review privacy/source/fork/lease correctness; spend and lifecycle
   review; fix regressions. Actual Windows queue/ledger execution required.
   No live or paid provider calls in validation. Record evaluation limitations.

## Mandatory context

- forge-context observation.rs: snapshots, fork and overlap contract.
- forge-context store.rs/windows.rs: native bounded namespace transactions.
- forge-session store.rs: persisted redaction, prefix copy, event positions.
- forge-runtime service.rs: run_tracked/start_run/fork_session/model factory.
- forge-runtime budget.rs and forge-session decisions.rs: known vs unknown usage.
- forge-providers model.rs/local_only.rs: centralized egress enforcement.
- forge-config ModelEntry/CostBook and CLI commands/service.rs.

## Verification

`cargo fmt --all -- --check`; `git diff --check`;
`cargo test --locked -p forge-context -p forge-runtime -p forge-config`;
`cargo clippy --workspace --all-targets -- -D warnings`;
`cargo test --workspace`; `cargo test -p forge-cli --test bdd`.
Corpus must measure supported vs unsupported claims using reviewed fixture
annotations; rejecting all claims is not quality success. Report empty/rejected
output separately from supported-claim precision and useful coverage.

## Limits to report

Session logs are authoritative; snapshots are trusted caller input. Sanitization
is heuristic. Idempotent commitment does not remove provider retry charges.
Budget estimates do not prove vendor billing ceilings. New observer versions
do not rewrite existing ledger coverage. No default-on observations or injection.
