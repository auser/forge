# CONTEXT-4: Source-anchored observation ledger and renderer

Status: Approved for implementation.
Issue #41 / epic #37. Architecture:
`docs/superpowers/specs/2026-10-08-context-engine-design.md`.
Baseline: PR #52 merged, `70dec01`.

## Approved contract

- Source ranges use inclusive one-based session-log positions, not per-run seq.
- Commit ranges are batches: one source range can produce multiple atomic items.
- Reject duplicate/overlapping ranges regardless of observer version.
- Accept disjoint ranges in any arrival order; rendering order is source order.
- A fork inherits only whole batches ending at or before its cut; preserve
  original source identities. Later changes stay branch-local.
- Exclude a whole observation batch intersecting the recent raw tail. Do not
  split summaries or truncate/rebuild tool messages.
- Renderer accepts already replay-normalized tail messages. No runtime replay
  duplication or dependency cycle.
- No observer model, scheduling, automatic prompt injection, production memory
  configuration or changes to current memory-zero accounting in this phase.

## Models and invariants

Add observation APIs in forge-context. Records contain deterministic IDs, scope,
kind (decision/constraint/outcome/question/state), sanitized content, observer
version, exact source range and deterministic source-derived creation metadata.
Include source-content fingerprints so changed supplied snapshots cannot silently
validate an old anchor. Do not infer source-log identity from Event.session_id:
fork copies preserve parent event fields.

Construct validated batches through a private boundary using an explicit source
session identity, its authoritative supplied Event snapshot and shared Redactor.
Check zero/reversed/out-of-range anchors, malformed identifiers, metadata secrets,
empty/oversized content and bounds before hashing/persistence. Source validation
rejects unsupported data; error messages contain no source text or private paths.
Derived content is untrusted data, never an instruction tier.

IDs derive only from stable canonical sanitized inputs; no ULIDs or wall-clock
values in IDs/rendered output. Ledger commit order is append-only. Watermarks
track covered intervals and contiguous coverage, never simply max end when gaps
remain. Empty observation batches may still represent a valid observed range;
they must not silently justify dropping source messages.

Session/project scope is represented, but project promotion/automatic cross-session
selection is deferred to explicit consolidation. Session rendering must not leak
another session's records. Fork projection is a frozen prefix snapshot, not a
live read of later parent changes.

## Renderer and lineage

A visible ledger projection has one target session coordinate system. Parent
anchors retain original identities, but their positions in copied prefixes are
also positions in the child. Apply raw-tail exclusion to *all* visible inherited
batches by those positions, not just batches whose source_session_id equals the
child ID. Validate nested forks, copied prefix markers and local append ranges.

Render selected observations in canonical source-position/ID order under a fixed
derived/untrusted-data header. Keep content escaped/structured so payload text
cannot fabricate outer source labels or observation delimiters. Return selected
and omitted IDs/reasons and component ContextSize estimates; append the supplied
normalized raw messages unchanged. Never convert observations into system/project
instructions. Deterministic output and stable field order are required.

## Implementation and file ownership

1. **Observation models, ledger, secure persistence**
   Create `crates/forge-context/src/observation.rs` and tests. Export APIs.
   Memory/Fs storage share validation and append-only batch semantics. Reuse
   descriptor-relative/pinned native primitives and bounded locking from
   `store.rs` / `store/windows.rs`; no unsafe pathname fallback.
   A bounded atomic ledger file may rewrite an unchanged prefix plus one append,
   but never replace or mutate committed records. Document logical vs physical
   append semantics. Bound files/records/reads and fail closed on corruption.
   VALIDATE: context tests; native and Windows-target clippy.

2. **Pure renderer and fork projection tests**
   Create `crates/forge-context/src/observation_render.rs`, tests and exports.
   Depend on the ledger public projection API; no store/provider side effects.
   Test parent/nested fork cut adjacency/intersection, different source IDs at
   inherited positions, canonical ordering, tool-message preservation, no double
   representation, stable bytes after ledger deletion/rebuild.
   VALIDATE: context tests and clippy.

3. **Replay integration tests and documentation**
   Add fixture-only runtime tests using actual session prefix copies and existing
   replay conversion to supply normalized tails. No live injection. Document
   source position meaning, frozen fork behavior, bounds and unsupported anchors.
   VALIDATE: runtime/context/core/session tests and full workspace checks.

4. **Independent security/correctness review and Windows CI**
   Review source verification, scope/fork isolation, deterministic rendering,
   append-only bounds and persistence. Fix findings with regressions. No ready
   status until actual Windows execution, not just cross-compilation, passes.

## Mandatory reading

- forge-core events: positions vs seq, SessionForked, serialized events.
- forge-session store: append, raw_lines, copy_prefix, redactor snapshot.
- forge-runtime replay: group interleaved runs, repair tool pairs, fit_to_budget.
- forge-runtime service fork_session: normalize cut to complete run boundaries.
- forge-context artifact/store: private constructors, native transaction safety.
- forge-context plan: ContextSize and unchanged memory-zero accounting.

## Validation

`cargo fmt --all -- --check`; `git diff --check`;
`cargo test --locked -p forge-context -p forge-runtime -p forge-session`;
`cargo clippy --workspace --all-targets -- -D warnings`;
`cargo test --workspace`; `cargo test -p forge-cli --test bdd`.
Native Windows context workflow must exercise new ledger filesystem tests.

## Risks and explicit limits

The caller supplies trusted session snapshots in this phase; an Event slice is
not cryptographic proof of an actual disk log. No model-produced anchors are
accepted unchecked by a future observer. Validation proves source existence and
scope, not the semantic truth of an observation. Rebuild fixtures prove byte
determinism, not that a future model repeats its answers. Source logs remain
authoritative; derived ledgers may be deleted.
