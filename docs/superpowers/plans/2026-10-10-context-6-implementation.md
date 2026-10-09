# CONTEXT-6: Context and memory controls

Status: Approved. Issue #43 / epic #37. Baseline PR #54 (`6ad3416`).

## Approved semantics

- `/context status`; `/memory status|on|off|show|sources`, with CLI equivalents.
- Memory means session-scoped observation extraction only. Live prompt injection
  remains unavailable and must be explicitly reported as such.
- Project observer configuration/model/complete pricing/egress/budget restrictions
  remain authoritative; session `on` cannot bypass them. Refuse enabling when
  prerequisites are unavailable with a clear safe diagnostic; no implicit model
  calls from status or controls.
- Default session consent is off. Existing observations are not deleted; project
  enablement alone is not session consent after this change.
- `off` stops new scheduling/dispatch and requests cancellation of active work
  where possible. Existing records and incurred/ambiguous provider charges remain.
  Do not promise remote execution or an already committing operation can be undone.
- Policy is a typed append-only session event, replay-ignored. Resume preserves
  it; fork prefix copies inherit only policy through the cut. Child overrides
  never mutate parent state.
- Chat always targets its current session. Every session-scoped CLI command
  requires `--session <id>`. No implicit newest-session selection.
- Status/show/sources are bounded, read-only and usable during active chat turns.
  Controls do not move sessions or alter active conversation messages.
- Consolidated memory is explicitly unavailable (CONTEXT-7); observations, raw
  events and jobs remain separate categories.

## Shared service and data contract

Add a transport-independent query/control facade in forge-runtime, consuming
explicit config, session store and local context stores without constructing
providers or starting observer workers. CLI/chat serialize/render the same DTOs.
Gate `on` using explicit observer configuration and known prices plus centralized
provider-policy readiness supplied by the host; never infer readiness from a
global boolean alone. Document inspection versus live worker status.

Context status: latest available request component estimates, output reservation,
remaining budget, stable-prefix drift; cumulative typed compression savings and
retrieval attempts/results across visible session history; artifact health and
capacity. Treat missing/corrupt/unavailable derived stores explicitly. Count
retrieval from genuine typed tool events, never arbitrary text. Follow typed plan
IDs via trusted validation, not an arbitrary event pathname.

Memory status: desired session consent vs effective eligibility, raw event count,
eligible/active observation count, consolidated unavailable, and session-filtered
observer queue counts/costs. Do not expose project-wide counts as session-local.
Show/source pages use stable ordering, bounded item/content budgets and explicit
continuation. Sources report original session/range/version/fingerprint metadata;
do not automatically dump raw source text.

Add native read-only artifact/observation inspection as necessary. Inspection must
not create absent storage, repair permissions or refresh LRU/expiry. Missing and
unhealthy stores are distinct. Counts must be honest about ledgered versus live
payloads and expiry; avoid a write transaction just for status.

## Policy integration

Use one shared effective-policy function over the target session's persisted
events. Copied parent event.session_id fields are not a reason to ignore inherited
policy. Event publication uses the existing redaction/store path, no synthetic
model turn or provider request.

Observer discovery AND queued dispatch must enforce consent. Recheck after
provider return before commitment and on local policy-change notification.
Off sessions pause eligible pending work rather than silently deleting it;
later on may resume under current budgets. Charge ambiguity conservatively.
Control-only events must not keep a fictitious run active forever in chunking,
pollute replay or become observer content. Add regression for toggles between
completed runs and during a run, including nested fork cuts.

## Tasks and ownership

1. Shared runtime query/control DTOs, source policy event/core consumers, observer
   gates and native read-only inspection APIs in forge-context. Publish facade
   contracts early to UI worker. No chat presentation coupling.
   VALIDATE context/runtime/core/session tests and clippy.
2. Chat parser/help/completion/controller/app/host seam. Pure explicit action
   enums, command argument validation, fake-host tests, no filesystem in editor
   completion. Keep status output via notification while a turn runs.
   VALIDATE forge-chat tests/clippy.
3. CLI context/memory commands and CliHost implementation using shared facade.
   One JSON document on stdout in --json mode, no observer startup/model call;
   TTY activity remains on stderr. Add process/PTY-facing tests and docs.
   VALIDATE CLI tests/BDD and command smoke tests.
4. Independent reviews of policy/fork/races/read-only inspection and UI/JSON.
   Full workspace validation and actual Windows context CI before ready.

## Mandatory context

- forge-chat command.rs COMMANDS/parse/complete; controller.rs actions; host.rs;
  app.rs active-turn output; ScriptedIo/FakeHost tests.
- forge-cli chat/host.rs; commands/observer_cmd.rs; cli.rs/commands/mod.rs.
- forge-core events.rs and all exhaustive event consumers/replay.
- forge-context plans/artifact/observation/observer queue native inspection.
- forge-runtime observer.rs discovery/claim/finish; service.rs fork/run lifecycle.

## Validation and limits

Run fmt/diff checks, workspace all-target clippy, full workspace tests and CLI
BDD. Test fresh absent storage leaves disk unchanged; corrupt/missing store DTOs;
session/default/off/on/fork policies; queued off prevents provider calls; in-flight
off cancels where possible without losing spend; no live memory in requests;
inspection JSON parses with no activity bytes. All fixtures use scripted/local
providers only. No credentials or real session content in committed fixtures.

This is observability/control, not a new planner, observer model evaluation,
consolidation or automatic memory injection feature.
