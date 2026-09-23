# ADR-0002: Cost-Aware Routing and Laya (System One) Support

## Status

Accepted

## Date

2026-09-23

## Context

v0.2/v0.3 Forge routes tasks through static rules, mocks, or a generic
System One-compatible HTTP router. Two gaps:

1. **Cost blindness.** Agent loops make many model calls; without cost
   metadata, Forge cannot prefer the cheapest capable model — the basic
   unit of agentic cost control.
2. **Laya.** Laya (open-source System One decision model) is Python-only
   with no official HTTP server, conflicting with Forge's single-binary,
   no-Python-runtime goal.

## Decision

- A `[models.<name>]` config table carries per-model metadata:
  `description` (routing criteria text), `cost_input_per_mtok` /
  `cost_output_per_mtok` (USD per million tokens, default 0.0 = free),
  optional `base_url`/`key_env`, and capability overrides
  (`tools`/`streaming`/`structured_output`/`vision`/`max_context`).
  The table deep-merges by name across config file layers; env/CLI cannot
  set entries. Entries without capability overrides remain
  "optimistically unknown" for capability filtering.
- New router mode `cheapest`: among capability-satisfying candidates,
  pick the lowest input cost (tie-break: output cost, then name).
  Deterministic; confidence 1.0.
- New router mode `laya`: a System One-compatible HTTP router specialized
  for Laya's typed-questions shape (`state` + `questions.model.choice`
  with per-candidate criteria). Defaults to `http://127.0.0.1:8788/decide`
  (the reference adapter), fully overridable via `router_url`.
  Unknown choices are typed router errors.
- Confidence gating: `router_confidence_threshold` (default 0.7) wraps
  `http` and `laya` primaries in a `ThresholdRouter`; below-threshold
  decisions become router failures and escalate through the existing
  `FallbackRouter` to `router_fallback` (default `static`, may be
  `cheapest`). The low confidence is recorded in the decision reason.
- A reference stdlib-only Python adapter (`adapters/laya-http.py`) runs
  `laya.Router(preload=True)` behind the HTTP contract. It is optional
  and never part of the binary or CI.

## Consequences

### Positive

- Cheapest-capable routing is a one-line config change and works offline.
- Laya is usable without Python in the Forge process or repo toolchain.
- Low-confidence decisions degrade gracefully to deterministic routing.
- The `[models]` table becomes the single place for cost, capabilities,
  and endpoint metadata; the runtime resolves providers per routed model.

### Negative

- Cost metadata is manual; no usage metering feeds it yet.
- The Python adapter is an extra moving part for Laya users.
- Threshold gating is global, not per-task-type.

## Alternatives rejected

### Embedding Python via PyO3

Rejected: it would embed a Python interpreter in the binary, violating
the single-binary / no-Python-runtime goal and blowing up portability.

### Hard-coding a Laya cloud URL

Rejected: no such service exists; Laya is self-hosted. Only a localhost
default is baked in.

### Cost tracking via measured usage

Deferred: useful but orthogonal; static cost tables already enable
cheapest-capable routing.
