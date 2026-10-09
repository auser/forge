# CONTEXT-2: Sanitized reversible tool artifacts

Status: Implementation authorized; user accepted defaults.
Implements: GitHub #39. Epic: #37. Builds on CONTEXT-1 / PR #50.
Architecture: `docs/superpowers/specs/2026-10-08-context-engine-design.md`.

## Outcome and scope

Preserve sanitized complete oversized tool results in bounded native storage.
Expose their omitted content only through explicit bounded model retrieval.
Keep capped views in provider requests and replay. No content-aware compressor,
observer, external service, raw-secret archive, or automatic expansion.

## Accepted defaults

- 128 MiB project-wide payload capacity, seven-day maximum age, 8 MiB maximum
  complete artifact; configurable limits. Bound metadata/object counts too.
- UTF-8-safe byte ranges or literal search; at most 16 KiB per retrieval response.
  Explicit continuation information; no regex.
- Project-wide content deduplication, but retrieval authorization only from the
  current session's visible history, including fork ancestors before their cut.
  A content hash alone is never authorization.
- Storage failure preserves capped sanitized output with explicit unavailable
  information, never an advertised successful retrieval handle.

## Mandatory reading and patterns

- `crates/forge-context/src/store.rs`: ContextStore and private Unix traversal,
  locking, atomic replacement, sanitized errors and adversarial tests.
- `crates/forge-context/src/store/windows.rs`: pinned handles, reparse/hardlink
  rejection and non-propagating private security operations.
- `crates/forge-session/src/redact.rs` and `store.rs`: existing redaction policy
  and assigned per-run event sequence numbers.
- `crates/forge-runtime/src/service.rs`: normal tool dispatch, Needle direct
  execution, failed-fast reinjection, emit, replay and injected context store.
- `crates/forge-runtime/src/tools.rs`: definitions, policy evaluation, dispatch.
- `crates/forge-runtime/src/replay.rs`: authoritative visible fork history.
- `crates/forge-core/src/events.rs`: output cap and event schema.
- `crates/forge-runtime/src/service/tests.rs`: oversized tool output and privacy.
- `crates/forge-cli/src/commands/service.rs`: production dependency wiring.

Reuse the existing redactor rather than duplicating regexes. Redact complete
strings before truncation, hashing, persistence, live events or provider messages,
including when artifact storage is disabled or unavailable. Retain full shared
redaction behavior for normal, error, approved and Needle results.

## Contract and ordering

The context crate owns artifact types, sanitization boundary, retention and
bounded retrieval. The runtime owns authorization based on authoritative visible
events, provider policy and normal tool execution/quota events.

Content identity is SHA-256 over redaction-policy version plus sanitized bytes.
Source metadata does not enter the content identity. Each manifest carries its
session, run, call, source event sequence, policy version, length and timestamps.
Use a source-specific opaque handle distinct from the deduplicated object hash.

Do not guess or reserve session event sequences. Prefer a source anchor in the
already-persisted tool call event; if result-event anchoring is necessary, use an
explicit two-stage object/manifest API and ensure failures cannot advertise an
uncommitted manifest. Record the chosen anchor meaning explicitly.

Authorization must use trusted event metadata, not handles parsed from arbitrary
model/tool text. Keep a typed artifact reference with its source anchor in the
event stream, backward compatible with older events. Fork visibility is resolved
by existing session replay traversal and its exact cut semantics.

Retrieval accepts an opaque handle plus a nonempty query, bounded range or
positive limit. All forms obey a hard response cap and UTF-8 boundaries. Return
explicit unavailable for absent, evicted or unauthorized artifacts without
disclosing existence across sessions. Do not recursively artifact retrieval
responses. Register retrieval only when usable, route it through normal policy
and quota dispatch, and exclude it from Needle direct execution.

## Tasks

1. **Artifact contracts, storage and retrieval**
   Add artifacts alongside context plans; factor and reuse secure filesystem
   primitives rather than weakening their traversal rules. Add in-memory storage
   for tests. Implement private atomic dedupe/manifests, a bounded cross-process
   retention transaction, age and deterministic LRU eviction, bounded reads and
   metadata. Inject time for deterministic tests. Do not silently store truncated
   bytes as a complete artifact.
   VALIDATE: `cargo test --locked -p forge-context`; Windows-target clippy.

2. **Shared redaction and runtime integration**
   Expose existing redaction safely; use one post-dispatch transformation for
   normal and Needle paths. Preserve exact capped provider/replay views plus
   retrieval markers. Add typed event metadata, authorized retrieval, ordinary
   quota/policy auditing and failed-fast handling. Test fork cut behavior.
   VALIDATE: `cargo test --locked -p forge-runtime --lib`;
   `cargo test --locked -p forge-session`; `cargo test --locked -p forge-core`.

3. **Configuration, production wiring and documentation**
   Wire production artifact storage and validated defaults, retaining optional
   injection for embedders. Document limitations, retention, retrieval semantics
   and sanitized unavailable behavior. Update exhaustive event consumers if
   needed. Keep unrelated `docs/issues/` files untouched.
   VALIDATE: config/CLI tests and workspace clippy.

4. **Adversarial integration and independent review**
   Verify secrets absent from object bytes, manifests, events, live frames and
   captured provider requests; retrieve omitted tails and compare replay views.
   Cover dedupe, cross-session denial, nested fork cuts, evicted/missing artifacts,
   Unicode windows, invalid/unbounded requests, retention ties, independent
   writers, symlink/hardlink substitution, private permissions and Windows ACLs.
   VALIDATE: full CI and actual Windows runtime tests, not only cross-compilation.

## Validation and completion

Run `cargo fmt --all -- --check`, `git diff --check`,
`cargo clippy --workspace --all-targets -- -D warnings`,
`cargo test --workspace`, and CLI BDD. Run Windows context tests in CI.
Obtain independent security review before ready/merge. Record results and any
intentional deviations in the implementation report; never call cross-compilation
Windows runtime proof. Publication/merge is coordinated by the parent agent.

## Risks and implementation decisions to record

- Stored sanitized artifacts cannot be rebuilt from truncated session events.
  Eviction deliberately loses omitted detail and must always report unavailable.
- Retention must bound manifests as well as payload bytes; duplicate output must
  not create unbounded metadata. Reads must not trust corrupt lengths.
- Exact default metadata count limits and deterministic eviction tie breaking
  are engineering choices; document them and test them.
- Redaction is the existing heuristic policy, not a claim to detect every secret.
- No unsupported claims about Win32 query normalization or universal filesystem
  behavior; preserve the tested Windows/NTFS baseline from CONTEXT-1.
