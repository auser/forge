# CONTEXT-3: Deterministic content-aware compression

Status: Accepted for implementation.
Issue: #40; epic #37; prerequisite: merged PR #51 / CONTEXT-2.
Architecture: `docs/superpowers/specs/2026-10-08-context-engine-design.md`.

## Outcome and approved decisions

Pure native compressors may replace the existing capped sanitized tool view
when they preserve required structure and reduce actual provider context.
Complete sanitized artifacts and explicit authorized retrieval remain unchanged.

- Initial default activation: strictly above 64 KiB, matching existing capping.
- Measure 8, 16, 32 and 64 KiB thresholds in the corpus; do not enable lower
  thresholds by default merely because a synthetic case succeeds.
- Each compressor independently requires at least 30% net estimated savings
  against the current provider view, including metadata and retrieval markers,
  with all structural and scripted-task assertions passing.
- Below the current cap, experimental comparisons use full input, not a capped
  artificial baseline. Use the existing conservative character/token estimator.
- Protected oversized outputs, including git status, retain existing
  cap-and-retrieval behavior without additional compression. This is not a
  promise of full losslessness in an already-capped provider prompt.
- Reject a candidate that drops required protected details, exceeds the existing
  view budget, or does not save context; retain CONTEXT-2 fallback.
- No code compression, generic history compression, paid-provider calls or
  private session corpus. Use sanitized fixtures and deterministic scripted tasks.

## Mandatory context

- `crates/forge-runtime/src/service.rs`: `prepare_tool_output`, normal and Needle
  paths, `emit_stored`; provider text must be the exact persisted output.
- `crates/forge-context/src/artifact.rs`: SanitizedOutput, source wrappers,
  manifest grants, bounded retrieval; do not weaken privacy or authorization.
- `crates/forge-session/src/redact.rs`: structured retrieval redaction.
- `crates/forge-core/src/events.rs`: cap, event schema and typed artifact grants.
- `crates/forge-context/src/plan.rs`: common ContextSize estimator.
- `crates/forge-runtime/src/tools.rs`: graph output formats and command envelope.
- Existing artifact runtime tests and native storage adversarial suites.

## Design and boundaries

Put classification, candidate generation and measurement in a pure context
module. Inputs are the originating ToolCall and sanitized output. Never echo raw
tool arguments into summaries or metadata: arguments may contain secrets.
Return a stable reason, version, original/baseline/view estimates and omission
description. No filesystem, clock, model, randomness or retrieval expansion in
classification/compression.

Classify only known origin plus strict parse: graph search rows, JSON/JSONL or
tables from recognized file types, ordinary unified diffs, and a conservative
known command-log grammar. Unknown/malformed/ambiguous formats are successful
fallbacks. Shell wrappers, unsupported diff forms, arbitrary code and protected
commands must never be guessed into a lossy compressor.

Preserve file/line references in search; JSON/table shape and distinct values
plus exceptional rows and deterministic samples; all diff file/hunk headers,
changed lines and special markers; recognized log errors, warning lines, test
names, exit codes and first/last/context windows. When complete preservation
cannot be established, decline compression rather than silently dropping fields.
Approvals, user requirements, protocol IDs and commands outside tool-result
content are not transformation inputs and remain unchanged.

Runtime compresses only after full sanitization and successful artifact storage,
so every compressed omission has a retrievable original. Retrieval results are
excluded. Render deterministic visible classification/version/size/omission
metadata with the existing opaque retrieval handle; compare the complete
serialized view against the existing complete serialized capped baseline.
Record compact classification/decision metadata in versioned events without raw
content, and preserve exact persisted/provider/replay text.

The default-on eligible compressor set must be justified by per-kind corpus
tests. If a format cannot pass honestly, leave that format conservative rather
than tuning fixtures or hiding a failing gate. Record measured limitations.

## Implementation tasks

1. **Pure classification/compression and corpus**
   Add a module in forge-context, exports, sanitized representative fixtures and
   a deterministic report for each format and each threshold. Include negative
   classifiers, Unicode/escaping/metadata overhead, difficult protected fields,
   no-savings fallback and unsupported/malformed inputs.
   VALIDATE: context tests and clippy, including Windows cross-target if available.

2. **Runtime integration and decision events**
   Use a single prepare boundary for model and Needle paths. Preserve source
   validation, fail-open unavailable behavior and exact persisted output.
   Add typed bounded decision metadata and event consumers as needed. Test
   compressed successes, disabled mode, source/store failures, unknown/protected
   fallbacks, privacy, replay parity and model-loop omitted-answer retrieval.
   VALIDATE: runtime/core/session tests and clippy.

3. **Configuration and production docs**
   Provide `context_compression.enabled` (default true) with explicit disable for
   comparison/rollback. Keep the 64 KiB activation threshold fixed for rollout;
   corpus exploration is not a user-facing automatic threshold tuner.
   Document per-kind supported grammars, unknown/protected fallback and savings
   definition. Preserve unrelated untracked docs/issues.
   VALIDATE: config tests and CLI integration/workspace checks.

4. **Independent review and CI**
   Review fidelity, classification conservatism, privacy and budget comparisons.
   Fix findings with regressions. Run full workspace tests/clippy/fmt, CLI BDD
   and actual Windows CI before ready status.

## Verification and report

`cargo fmt --all -- --check`; `git diff --check`;
`cargo test --locked -p forge-context -p forge-runtime -p forge-session`;
`cargo clippy --workspace --all-targets -- -D warnings`;
`cargo test --workspace`; `cargo test -p forge-cli --test bdd`.

Keep a report of per-kind original, capped-baseline and compressed estimates;
savings include formatting and source retrieval instructions, not just payload.
Every required field and scripted answer assertion must pass. No real model
quality claim follows from deterministic scripted tests.

## Intentional conservative limitations

Existing capping can already omit protected data; this ticket adds no new lossy
transformation to protected results and does not solve the general budget policy.
Arbitrary natural-language requirements embedded inside an unknown file are not
reliably classifiable, so unknown content keeps the existing path.
