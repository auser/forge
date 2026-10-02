# Explicit Skill Activation (`RunOptions::activate_skills`) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A run can name the skills it wants, deterministically. `RunOptions` gains `activate_skills: Vec<String>`; the runtime activates exactly those skills (in addition to, never instead of, lexical discovery) and records the same `skill_activated` event. Chat's `/name` stops relying on the lexical `match_task` accident that the skill's name appears in the prompt text, and `forge run`, `forge mcp` and `forge acp` each gain the same option.

**Architecture:** One runtime change (`forge-runtime`), four thin pass-throughs. The `SkillRegistry` trait (`forge-core`) and `FsSkillRegistry` (`forge-skills`) are **untouched** — explicit activation is the *caller* naming entries the registry already knows how to `activate()`. In chat, the skill's identity stops being dissolved into prompt text at the parser and instead travels as data — `Parsed::Skill` → `Action::Prompt(TurnRequest)` → `RunOptions::activate_skills` — through the same queue and driver every other turn uses. No event-schema change: `SkillActivated { name, path }` already exists and replay already ignores system messages.

**Tech Stack:** Rust edition 2024, tokio, thiserror, serde/serde_json; cucumber BDD (`cargo test -p forge-cli --test bdd`). No new dependencies, no new config keys, no event-schema change.

**Spec:** `docs/superpowers/specs/2026-09-24-interactive-chat-ui-design.md` §9.4 (skills as `/name`, and its stated honest limitation) and §14 bullet 4 ("Explicit skill activation — the fix is `RunOptions::activate_skills` in the runtime, which also fixes the same weakness for `forge run`, `forge mcp` and ACP").

All `file:line` citations are against the clean worktree `worktrees/t6-skill-activation` at HEAD `5c72985`.

---

## Feature Description

Today the only way a skill reaches a run is `SkillRegistry::match_task`: a scored lexical match of the prompt's words against skill names/descriptions (`crates/forge-skills/src/registry.rs:236-262`). Chat's `/name` works *through* it — the parser rewrites `/tdd rest` into the prompt `"Use the tdd skill.\n\nrest"` precisely so the literal name appears as a matchable word (`crates/forge-chat/src/command.rs:284-296`). The design doc records the two weaknesses (§9.4): glue words can match the *wrong* skill, and a skill whose name is shorter than 3 characters can never match. This ticket adds the runtime-level explicit path the design deferred: the caller names skills, the runtime activates exactly those, and every front end that starts runs can express it.

## User Story

As a forge user (in chat, in scripts, or through an editor harness), when I say which skill a turn should use — `/tdd write the failing test`, `forge run --skill tdd ...`, `forge_run { "skills": ["tdd"] }`, or an ACP prompt carrying the extension metadata — exactly that skill activates (recorded as `skill_activated`, instructions injected into the model's system context), regardless of how its name would score lexically; and when I name nothing, discovery behaves exactly as before.

## Problem

1. **`/name` is probabilistic where it should be exact.** Activation depends on the *text* of the generated prompt matching (`command.rs:290-296` + `registry.rs:236-262`), not on the name the user typed. A skill named `go` (2 chars) is unreachable by slash command: `tokenize` drops words under 3 chars (`registry.rs:279-285`).
2. **The wrong skill can win.** `match_task` scores description-token hits too; the "Use the X skill." template still carries words that can pull in an unrelated skill alongside or instead of the intended one (design §9.4).
3. **The other three run-starting surfaces have no way to say it at all.** `forge run` (`run_cmd.rs:30-34`), MCP's `forge_run` (`tools.rs:567-571`) and ACP's `run_turn` (`server.rs:523-529`) all start runs with no skill input, so a harness that *knows* which skill it wants must smuggle the name into the prompt and hope the lexical matcher agrees.
4. **An explicit-but-wrong name is silently ignored.** A typo'd `/tddd`-style name at a surface that *meant* a skill should fail loudly, not run without it — the same principle as chat's "an unrecognised `/word` is an error, not a prompt" (`command.rs:12-15`).

## Solution

`RunOptions` (`crates/forge-runtime/src/service.rs:117-125`) gains:

```rust
/// Skill names to activate explicitly, in addition to any the task
/// matches lexically. An unknown name is a typed error at the entry
/// point — the caller asked for it, so never run without it.
pub activate_skills: Vec<String>,
```

Every existing construction site uses `..RunOptions::default()` (`service.rs:1110-1113`, `run_cmd.rs:30-34`, `tools.rs:567-571`, `server.rs:523-529`, `app.rs:656-660`, plus test sites), so the field is source-compatible everywhere.

**Design calls (settled here, not left to the implementer):**

1. **Field shape:** `Vec<String>` of skill *names* (not `Skill` structs, not `SkillMeta`). Names are what every caller has — the chat snapshot, a CLI flag, a JSON array — and the registry resolves name → instructions at run time through the one path that already exists (`SkillRegistry::activate`). `Vec` because a turn may want several; order is the caller's and is preserved in event order.
2. **Unknown-skill behavior: fail fast, synchronously, typed.** Both entry points that take `RunOptions` (`run_with_options` `service.rs:1055`, `start_run_with_options` `service.rs:1129`) validate every name against `skills().list()` **before** claiming the session or spawning anything, returning `ForgeError::Skill("unknown skill: {name} (available: a, b, c)")`. Rationale: (a) the caller explicitly asked — running without the skill is paying for a turn nobody wanted, the same "don't silently reinterpret" rule as the unknown-slash-command error; (b) the refusal must reach the caller *synchronously* — the same argument `claim_session`'s doc makes for `SessionBusy` (`service.rs:1011-1038`): a transport that has already answered "202, here is your run id" has nowhere left to report it; (c) validating at the entry leaves **zero** events in the log, so a typo pollutes no session. Lexical activation keeps its existing warn-and-continue (`service.rs:1747-1749`) — discovery is speculative, explicit is requested; the two failure policies differ *because the two contracts differ*. If a skill passes entry validation but its `activate()` read then fails inside the run (deleted between the two), the run fails with the typed error rather than continuing without the requested instructions.
3. **Precedence: explicit adds, never replaces discovery.** In `run_inner` the explicit names activate first (caller order, duplicates collapsed), each emitting `SkillActivated` and pushing its system message; then `match_task(task)` runs exactly as today, skipping any name already explicit (no double event, no double system message). The lexical cap `MAX_MATCHED_SKILLS = 3` (`registry.rs:267`) still applies only to lexical matches — explicit skills are not budgeted against it; the user asked for them. With `activate_skills` empty the behavior is byte-identical to today.
4. **How `/name` switches to the explicit path:** the skill's identity stays *data* end-to-end instead of being dissolved into prompt text. `Command::parse` gains `Parsed::Skill { name, prompt }` (`prompt` keeps the existing `skill_prompt(name, rest)` template — see NOTES on why the template stays); the controller gains `TurnRequest { prompt, activate_skills: Vec<String> }` carried by `Action::Prompt` and by the mid-turn **queue** (so `/tdd x` typed during a turn keeps its skill when it runs next); `App::start_turn` passes `activate_skills` into `RunOptions`. Parsing rules are unchanged: a skill becomes a command only via the completion snapshot (`command.rs:163-166`), so `/name` can never name an unknown skill at parse time, and the runtime's entry validation is the backstop for the delete-between-refresh race.
5. **The needle fast path is skipped for explicit-skill turns.** The fast path (`service.rs:1490-1644`) returns before the skill block, so a turn that dispatched would record no activation and inject no instructions — silently dishonoring the one thing the caller asked for. Gate it alongside the existing `resumed_from.is_none()` condition (`service.rs:1493`): `resumed_from.is_none() && activate_skills.is_empty()`. No `Decide` record is written for such runs, consistent with resumed runs today (which skip the whole block, record included).

## Out of Scope

- **Changing lexical discovery.** `match_task`'s scoring, stopwords, and `MAX_MATCHED_SKILLS` are untouched; it stays the default path for prompts that name nothing. (Embedding-ranked selection remains the separately-recorded follow-up, `docs/reference.md:1813-1817`.)
- **Skill authoring format.** `SKILL.md`, frontmatter parsing, discovery roots, progressive disclosure (`list()` = metadata, `activate()` = full body) are all unchanged. This ticket adds no frontmatter fields and no new skill kinds.
- **`forge serve` (REST).** Its handler calls the options-free `start_run` (`crates/forge-server/src/handlers.rs:136`); giving REST callers the option means a request-body schema change, which the ticket does not ask for. Recorded as a follow-up (Open Question 3).
- **Recording explicit skills for `resume`.** Activation is per-run; `resume()` re-runs lexical discovery on the original task as today (Open Question 1).
- **`forge chat --skill` flag.** `/name` covers the interactive surface; the first-prompt arg stays a plain prompt.

## Metadata

- **Ticket:** `specs/tickets/interactive-chat-feel.md`, TICKET-6 (epic: interactive-chat-feel). Depends on: none.
- **Design inheritance:** `docs/superpowers/specs/2026-09-24-interactive-chat-ui-design.md` §9.4, §14 bullet 4; `ARCHITECTURE.md` (crate map: forge-skills progressive disclosure; `AgentService` as the one runtime every front end shares).
- **Crates touched:** forge-runtime (core), forge-chat (parser/controller/driver), forge-cli (`run` flag + dispatch), forge-mcp (tool schema + handler), forge-acp (wire type + turn claim). Tests: same crates + `crates/forge-cli/tests/` (process + BDD). Docs: `docs/reference.md`, `ARCHITECTURE.md`.
- **Estimate:** ~600–800 lines including tests (ticket estimate 500–800), across 6 tasks. Complexity: medium — each change is small and additive, but the same idea must land correctly in five places and the chat change crosses three modules.

## Global Constraints

- Typed errors via `thiserror`; **no `unwrap`/`expect` in production code** (tests may).
- Hermetic tests: temp `HOME`/`XDG_CONFIG_HOME`, the full `FORGE_*` scrub list, `FORGE_TEST_MOCKS=1` where mocks are used, no network. Process-level tests follow `crates/forge-cli/tests/mcp.rs` / `acp.rs` / `chat.rs`.
- `docs/reference.md` and `ARCHITECTURE.md` are updated **in the same commit** as the behavior they describe; both describe today, never the roadmap.
- No new dependencies, no new config keys, no event-schema change (`SkillActivated { name, path }` at `crates/forge-core/src/events.rs:114-117` is already the record).
- `just verify` (fmt --check + check + clippy -D warnings + lint-ffi + test + bdd) green before each commit; run in the worktree.
- Stdout discipline is untouched: nothing here prints; the surfaces render existing events.

## CONTEXT REFERENCES

### Runtime core — where the change lands

- `crates/forge-runtime/src/service.rs:117-125` — `RunOptions` today:
  ```rust
  #[derive(Debug, Default)]
  pub struct RunOptions {
      pub run_id: Option<String>,
      pub session_id: Option<String>,
      pub max_turns: Option<u32>,
  }
  ```
  Add the field here. `Default` keeps every existing caller compiling.
- `crates/forge-runtime/src/service.rs:130-158` — `RunPlan` ("everything one run needs beyond the ids") and `RunPlan::new_prompt(prompt, history)`. The explicit names ride the plan: add `activate_skills: Vec<String>` and take it as `new_prompt`'s third parameter. `resume()` builds the struct literally at `service.rs:2236-2247` and passes `Vec::new()` — resume has no options and is unchanged.
- `crates/forge-runtime/src/service.rs:1055-1072` (`run_with_options`) and `:1129-1161` (`start_run_with_options`) — the two entry points taking `RunOptions`. Validation goes **first**, before `claim_session` (`:1062` / `:1136`), so a refusal leaves nothing behind and reaches the caller synchronously. `run()` (`:1041-1043`) and `start_run()` (`:1103-1115`) delegate and inherit the behavior.
- `crates/forge-runtime/src/service.rs:1726-1751` — the skill block in `run_inner`, the insertion point. Today:
  ```rust
  let mut messages = Vec::new();
  for meta in self.skills.match_task(task) {
      match self.skills.activate(&meta.name) {
          Ok(skill) => {
              self.emit(..., EventKind::SkillActivated {
                  name: skill.meta.name.clone(),
                  path: skill.meta.path.clone(),
              })?;
              messages.push(Message::system(format!(
                  "Active skill `{}` instructions:\n{}",
                  skill.meta.name, skill.instructions
              )));
          }
          Err(e) => {
              tracing::warn!(skill = %meta.name, error = %e, "skill activation failed")
          }
      }
  }
  ```
  Explicit names run the same emit/push body **first**, then this loop gains a skip for already-activated names. Factor the emit+push into one closure/helper so the two paths cannot drift (the event shape and the system-message template are one definition — same discipline as `forge_core::tool_arg_field`).
- `crates/forge-runtime/src/service.rs:1493` — the fast-path gate `if resumed_from.is_none() {`; becomes `if resumed_from.is_none() && activate_skills.is_empty() {`.
- `crates/forge-runtime/src/service.rs:26-36` — `NullSkillRegistry`: `list()` is empty, so any `activate_skills` on a skills-less runtime fails validation with "unknown skill". That is the correct, testable behavior.
- `crates/forge-core/src/skill.rs:25-34` — the `SkillRegistry` trait (`list` / `activate` / default `match_task` returning nothing). **Unchanged** — the seam already says everything this ticket needs.
- `crates/forge-core/src/error.rs:22,85-86` — `ForgeError::Skill(String)` and its `ForgeError::skill(...)` constructor. Reuse; no new variant.
- `crates/forge-skills/src/registry.rs:202-208` — `FsSkillRegistry::find` already errors `"unknown skill: {name}"`; entry validation composes with it (validate via `list()` for the *available-names* message; `activate()` re-resolves and can still fail, which the run then surfaces).
- `crates/forge-skills/src/lib.rs:1-4` — "The registry itself never logs; callers append the `SkillActivated` event." This ticket keeps that rule: only `run_inner` emits.

### Chat — `/name` stops being a text trick

- `crates/forge-chat/src/command.rs:163-166` — the skill arm today:
  ```rust
  // Commands win over skills, so a skill cannot shadow `/help`.
  _ if snapshot.skills.iter().any(|(skill, _)| skill == name) => {
      Parsed::Prompt(skill_prompt(name, &rest))
  }
  ```
  becomes `Parsed::Skill { name: name.to_string(), prompt: skill_prompt(name, &rest) }`. The commands-win rule and the snapshot gate are unchanged.
- `crates/forge-chat/src/command.rs:284-296` — `skill_prompt`. Keep the function; rewrite its doc comment: the template is now *model context*, not the activation mechanism (the comment currently says the opposite — sweep it).
- `crates/forge-chat/src/controller.rs:85-119` — `Action`; `:87` `Prompt(String)` becomes `Prompt(TurnRequest)`. `:149` the queue `VecDeque<String>` becomes `VecDeque<TurnRequest>`; `:315-319` `take_queued` returns it; `:321-329` `on_prompt` takes it; `:224` the `Parsed::Prompt` arm wraps `TurnRequest::plain(text)`; the new `Parsed::Skill` arm wraps `TurnRequest { prompt, activate_skills: vec![name] }`. Define `TurnRequest` in this module (beside `Action`, its only consumer) and re-export from `lib.rs:56` next to `Action`.
- `crates/forge-chat/src/app.rs:641-682` — `start_turn`; `:654-661` the `RunOptions` literal gains `activate_skills: turn.activate_skills`. `:586` the `Action::Prompt` arm passes the whole `TurnRequest`. The queued-turn handoffs at `:503`, `:547`, `:566` are type-driven only.
- `crates/forge-chat/src/testing.rs:317-436` — `FakeHost` builds its service over `NullSkillRegistry` (`:411`) with a fixed `skills` list (`:432`). Add a skill-capable constructor: a small test-local `SkillRegistry` impl (the trait is `forge-core`; **no new dev-dependency** — do not add `forge-skills` here) with one skill whose name never lexically matches (e.g. `xy`, two chars), plus `skills` entries so the snapshot offers it.
- `crates/forge-chat/src/lib.rs:54-56` — re-export additions (`TurnRequest`, and `Parsed` already exported).

### `forge run` — the flag

- `crates/forge-cli/src/cli.rs:97-103` — `Command::Run { prompt, max_turns }`; add `#[arg(long = "skill", value_name = "NAME")] skills: Vec<String>` (clap `Vec` = repeatable).
- `crates/forge-cli/src/commands/mod.rs:106` — dispatch arm passes `skills` through.
- `crates/forge-cli/src/commands/run_cmd.rs:16-53` — `run(ctx, prompt, max_turns)` gains `skills: Vec<String>`; the `RunOptions` literal at `:30-34` gains `activate_skills: skills`. An unknown name surfaces as the entry-point `ForgeError::Skill` on stderr with a non-zero exit — the CLI's existing error path, no new printing.

### `forge mcp` — the tool parameter

- `crates/forge-mcp/src/tools.rs:157-178` — `run_schema()`; add:
  ```json
  "skills": { "type": "array", "items": { "type": "string" },
              "description": "Skill names to activate explicitly (as forge_skill_list reports them), in addition to any the task matches automatically." }
  ```
- `crates/forge-mcp/src/tools.rs:533-575` — `run()`; parse with a new `opt_str_array(args, "skills")` helper modeled exactly on `opt_u64`/`opt_bool` (`:852-874`), then pass into the `RunOptions` literal at `:567-571`. The start-error mapping at `:573-575` currently labels everything `session_busy`; match the variant — `ForgeError::Skill` → `ToolOutcome::invalid_params` (a bad argument, honestly classified), anything else unchanged.
- `crates/forge-mcp/src/tools.rs:487-514` — `skill_show` shows the existing discipline for a name lookup failing (`unknown_skill` tool error) and for recording an activation outside a run; consistent with, and unchanged by, this ticket.

### `forge acp` — the `_meta` extension

- `crates/forge-acp/src/protocol.rs:322-329` — `PromptRequest`; add `#[serde(default, rename = "_meta")] pub meta: Option<serde_json::Value>`. Serde's default ignores unknown fields, so older/newer clients interoperate; the ACP spec's per-request `_meta` is the sanctioned extension point (design doc §Editors: "the current stateless 2026-07-28 revision (per-request `_meta` …)").
- `crates/forge-acp/src/server.rs:80-85` — `TurnClaim` gains `activate_skills: Vec<String>`.
- `crates/forge-acp/src/server.rs:453-481` — `claim_turn` parses `request.meta.get("forge.activateSkills")`: absent → `vec![]`; an array of strings → the names; anything else → `RpcError::invalid_params` (the same refusal shape as an empty prompt, `dispatch.rs:145`'s `prompt_text` path). This keeps protocol validation synchronous on the reader task, before the turn is claimed.
- `crates/forge-acp/src/server.rs:521-533` — `run_turn`'s `RunOptions` literal gains `activate_skills: claim.activate_skills` (threaded through `drive_turn`, `:484-499`). The `Err` arm at `:531-532` (`RpcError::invalid_request(e.to_string())`) already carries the runtime's "unknown skill: …" message to the client.

### Conventions to copy

- Runtime tests: `crates/forge-runtime/src/service/tests.rs:13-37` (service builders), `:75-100` (`event_kinds` helper — already maps `SkillActivated` at `:82`); assert instructions reached the model via `MockModel::recorded()` / `ScriptedMockModel::recorded()` (`crates/forge-providers/src/model.rs:65-66`, `scripted.rs:54-56`).
- Chat driver tests: `crates/forge-chat/src/app.rs:1102-1160` (`FakeHost::with_script` + `ScriptedIo` pattern).
- Process tests: `crates/forge-cli/tests/mcp.rs:330` (`client.call_tool("forge_run", json!({...}))`), `crates/forge-cli/tests/acp.rs:289-291` (`session/prompt` shape), `crates/forge-cli/tests/cli.rs` (hermetic `forge()` helper).
- BDD: `tests/features/skills.feature` (13 lines, two scenarios), steps at `crates/forge-cli/tests/bdd/steps.rs:394-476` (skill fixture + `skill_activated` log assertion), chat lines via `world.rs:243-315` (`run_forge_with_stdin`).

### New files

None. Every change lands in an existing module; tests go in the existing test modules/files listed above.

## IMPLEMENTATION PLAN

Six tasks, each independently green under `just verify`. Tasks 2–5 depend on Task 1 (the runtime field); 3, 4, 5 are mutually independent and could be reordered or parallelized after 1 and 2.

- **Phase 1 (Task 1):** the runtime — field, entry validation, explicit-first activation, fast-path gate, unit tests.
- **Phase 2 (Task 2):** the chat — `Parsed::Skill`, `TurnRequest`, queue, driver; `/name` is exact.
- **Phase 3 (Tasks 3–5):** the pass-throughs — `forge run --skill`, MCP `skills`, ACP `_meta`.
- **Phase 4 (Task 6):** BDD scenarios + docs sweep (reference.md, ARCHITECTURE.md).

## STEP-BY-STEP TASKS

### Task 1: `RunOptions::activate_skills` in the runtime

The core. Everything else is plumbing into this.

**Files:**
- Modify: `crates/forge-runtime/src/service.rs` (RunOptions, RunPlan, both entry points, `run_inner` skill block, fast-path gate)
- Modify: `crates/forge-runtime/src/service/tests.rs` (new tests)

**Interfaces:**
- Consumes: `SkillRegistry::list` / `activate` (unchanged trait), `ForgeError::skill`.
- Produces: `RunOptions::activate_skills: Vec<String>` — consumed by Tasks 2–5.

- [ ] **Step 1: Write the failing tests** in `service/tests.rs`. Add a test-local registry (beside the builders at `:13-37`):
  ```rust
  /// Two skills: `beta` lexically matches the prompt, `xy` never can
  /// (two chars — `match_task` tokenizes >=3). `recorded` style assertions
  /// on activation go through the session log's skill_activated events.
  struct StubSkills { /* metas + instructions for "xy" and "beta" */ }
  impl SkillRegistry for StubSkills {
      fn list(&self) -> Vec<SkillMeta> { /* both */ }
      fn activate(&self, name: &str) -> Result<Skill, ForgeError> { /* find or ForgeError::skill */ }
      fn match_task(&self, prompt: &str) -> Vec<SkillMeta> { /* beta iff prompt contains "beta" */ }
  }
  ```
  Tests:
  1. `explicit_skill_activates_without_lexical_match` — `run_with_options("unrelated words", RunOptions { activate_skills: vec!["xy"], ..})`: exactly one `skill_activated`, named `xy`; `MockModel::recorded()`'s request contains a system message `Active skill \`xy\` instructions:` with the body; the run completes normally.
  2. `explicit_adds_to_lexical_never_replaces` — prompt mentions `beta`, explicit names `xy`: both activate, `xy`'s event first.
  3. `explicit_and_lexical_same_name_activates_once` — explicit `beta` + matching prompt: one event, one system message.
  4. `unknown_explicit_skill_is_a_synchronous_error_and_writes_nothing` — both `run_with_options` and `start_run_with_options` return `ForgeError::Skill` naming the bad name; the session store holds **no** `run_started` for the attempt (entry validation precedes claim and spawn).
  5. `explicit_skill_skips_the_fast_path` — build `needle_service` (`:43-59`), prompt in the one `HashBackend`-dispatchable shape, `activate_skills: ["xy"]`: the run goes through the model loop and the `skill_activated` event exists.
  6. `no_explicit_skills_is_byte_identical` — the existing `full_run_emits_ordered_events` (`:102-133`) already pins this; keep it green untouched.
- [ ] **Step 2: Run** `cargo test -p forge-runtime` → FAIL (no such field).
- [ ] **Step 3: Implement.** `RunOptions` field; `RunPlan::activate_skills` (via `new_prompt`'s new third parameter; `resume()` passes `Vec::new()` at `:2236-2247`); a private `validate_activate_skills(&self, names: &[String]) -> Result<(), ForgeError>` called first in both `run_with_options` and `start_run_with_options` (message: `unknown skill: {name} (available: {comma-separated list, or "none discovered"})`); in `run_inner`, destructure the new field, activate explicit names first (caller order, dedup by name, failure → `fail(...)` typed error, never warn-and-continue), then the existing `match_task` loop with an `activated` set skip; factor emit+push into one helper used by both paths; gate the fast path at `:1493` with `&& activate_skills.is_empty()`.
  - **GOTCHA:** validate **before** `claim_session` (`:1062`, `:1136`) — a refused run must leave no claim, no broadcaster, no events.
  - **GOTCHA:** `start_run_with_options` moves `options` fields before the `tokio::spawn` (`:1134-1155`); move `activate_skills` into the `RunPlan::new_prompt` call inside the spawn the same way `max_turns` travels.
  - **GOTCHA:** dedup explicit names *before* activating (caller passing `["tdd", "tdd"]` gets one event); preserve first-occurrence order.
  - **PATTERN:** `service.rs:1726-1751` (the block being extended); `claim_session`'s doc (`:1011-1038`) is the stated precedent for synchronous entry refusal.
- [ ] **Step 4: Run** `cargo test -p forge-runtime` → PASS.
- [ ] **Step 5:** `just verify` → PASS. **Commit** — `git commit -m "feat(runtime): RunOptions::activate_skills for explicit skill activation"`
- VALIDATE: `cargo test -p forge-runtime`
- SATISFIES: AC1, AC2, AC3, AC8 (partially — the byte-identical half)

### Task 2: Chat `/name` carries the skill explicitly

`/name` stops depending on the template's words matching.

**Files:**
- Modify: `crates/forge-chat/src/command.rs` (`Parsed::Skill`, skill arm, `skill_prompt` doc)
- Modify: `crates/forge-chat/src/controller.rs` (`TurnRequest`, `Action::Prompt`, queue, `take_queued`, `on_prompt`, both parse arms)
- Modify: `crates/forge-chat/src/app.rs` (`start_turn`, `Action::Prompt` arm)
- Modify: `crates/forge-chat/src/lib.rs` (re-export `TurnRequest`)
- Modify: `crates/forge-chat/src/testing.rs` (skill-capable `FakeHost`)
- Modify: the three modules' test suites (mechanical `Action::Prompt("...")` → `TurnRequest::plain("...")` updates at `controller.rs:494,640,936-941,956,1027` and `command.rs:459-470`)

**Interfaces:**
- Consumes: Task 1's `RunOptions::activate_skills`.
- Produces: `Parsed::Skill { name: String, prompt: String }`, `TurnRequest { prompt: String, activate_skills: Vec<String> }` with `TurnRequest::plain(prompt)`.

- [ ] **Step 1: Write the failing tests.**
  - `command.rs`: `a_skill_becomes_a_prompt_that_activates_it` (`:459-470`) becomes an assertion on `Parsed::Skill { name: "tdd", prompt: "Use the tdd skill.\n\nwrite the failing test" }`; add `a_two_character_skill_name_parses_as_a_skill` (the case lexical matching could never serve).
  - `controller.rs`: `/tdd write the failing test` → `vec![Action::Prompt(TurnRequest { prompt: "Use the tdd skill.\n\nwrite the failing test", activate_skills: vec!["tdd"] })]`; a `/tdd` typed **during** a turn pops from `take_queued` with `activate_skills` intact (extends `prompts_submitted_while_running_queue_in_order`, `:929-943`).
  - `app.rs`: with the skill-capable `FakeHost` (Step 3), drive `["/xy do the thing", "/quit"]`: the transcript shows `  - skill: xy`, the session log has `skill_activated` named `xy`, and `ScriptedMockModel::recorded()` shows the instructions system message — all with a prompt text that never lexically matches `xy`.
- [ ] **Step 2: Run** `cargo test -p forge-chat` → FAIL.
- [ ] **Step 3: Implement.**
  - `command.rs`: add the `Parsed::Skill` variant; switch the arm at `:163-166`; rewrite `skill_prompt`'s doc (`:284-296`) — the template is kept as model context and as the non-empty prompt for a bare `/name`; activation no longer depends on it.
  - `controller.rs`: `TurnRequest` beside `Action` (`:85`); `on_prompt` takes one; the `Parsed::Prompt` arm wraps `TurnRequest::plain`; the new `Parsed::Skill` arm wraps `TurnRequest { prompt, activate_skills: vec![name] }`; queue and `take_queued` carry `TurnRequest`.
  - `app.rs`: `start_turn(turn: TurnRequest)`; the `RunOptions` literal (`:656-660`) gains `activate_skills: turn.activate_skills`; prompt moves into `start_run_with_options(turn.prompt, ...)`.
  - `testing.rs`: add `FakeHost::with_skill_and_script(...)` — the same `build` factored to accept a `SkillRegistry`; a test-local `StubSkills` (name `xy`, two chars, plus a normally-matching one if convenient) and a `skills` snapshot entry so `/xy` parses.
  - **GOTCHA:** the queue carrying `String` is the silent way to lose the skill mid-turn — a queued `/tdd` must not degrade to a plain prompt. This is the whole reason `TurnRequest` exists instead of a second queue.
  - **GOTCHA:** `Action` derives `Eq` (`:84`); `TurnRequest` must too.
  - **GOTCHA:** `forge chat "/tdd x"` as a first prompt already routes through `controller.on_line` (`app.rs:182-185`), so it inherits the explicit path for free — assert it or at least do not break it.
  - **PATTERN:** `controller.rs:321-329` (`on_prompt`), `app.rs:641-682` (`start_turn`).
- [ ] **Step 4: Run** `cargo test -p forge-chat` → PASS.
- [ ] **Step 5:** `just verify` → PASS. **Commit** — `git commit -m "feat(chat): /name activates its skill explicitly, not via lexical matching"`
- VALIDATE: `cargo test -p forge-chat`
- SATISFIES: AC4 (and AC1 through the chat surface)

### Task 3: `forge run --skill <NAME>`

**Files:**
- Modify: `crates/forge-cli/src/cli.rs` (flag)
- Modify: `crates/forge-cli/src/commands/mod.rs` (dispatch)
- Modify: `crates/forge-cli/src/commands/run_cmd.rs` (pass-through)
- Modify: `crates/forge-cli/tests/cli.rs` (process tests)

- [ ] **Step 1: Write the failing tests** in `cli.rs`'s existing hermetic style: scaffold a project with `.forge/skills/demo/SKILL.md` (the BDD fixture at `bdd/steps.rs:394-400` shows the exact file), `forge run --skill demo "zz unrelated qq"` → exit 0 and the session log contains `skill_activated` named `demo` (read `.forge/sessions/*.jsonl` like `bdd/steps.rs:446-452`); `forge run --skill nosuch "..."` → non-zero exit, stderr contains `unknown skill: nosuch`.
- [ ] **Step 2: Run** `cargo test -p forge-cli --test cli` → FAIL.
- [ ] **Step 3: Implement** the flag, dispatch, and pass-through per CONTEXT REFERENCES.
  - **GOTCHA:** `prompt: Vec<String>` is positional (`cli.rs:98-99`); a `Vec<String>` flag is unambiguous to clap, but keep the flag declared before/after consistently with the file's style and confirm `--skill demo prompt words` and `prompt words --skill demo` both parse in the test.
  - **GOTCHA:** the unknown-skill error must reach stderr unchanged — do not wrap it in run_cmd; the CLI's top-level error printer does the work.
  - **PATTERN:** `cli.rs:97-103`, `mod.rs:106`, `run_cmd.rs:16-53`.
- [ ] **Step 4: Run** `cargo test -p forge-cli --test cli` → PASS.
- [ ] **Step 5:** `just verify` → PASS. **Commit** — `git commit -m "feat(cli): forge run --skill activates a skill explicitly"`
- VALIDATE: `cargo test -p forge-cli --test cli`
- SATISFIES: AC5

### Task 4: MCP `forge_run` gains `skills`

**Files:**
- Modify: `crates/forge-mcp/src/tools.rs` (schema, helper, handler, error mapping; unit tests in its `#[cfg(test)]` module at `:887`)
- Modify: `crates/forge-cli/tests/mcp.rs` (process test)

- [ ] **Step 1: Write the failing tests.** Unit (`tools.rs` tests): `opt_str_array` accepts absent/null/`["a","b"]`, rejects `"a"` and `[1]` with `invalid_params`. Process (`mcp.rs`, pattern at `:330`): scaffold the demo skill (the test file's existing project-scaffold helper — mirror how `skills.feature`'s step writes `.forge/skills/demo/SKILL.md`), `call_tool("forge_run", {"prompt": "zz unrelated qq", "skills": ["demo"]})` → completed run whose session log shows `skill_activated: demo`; then `{"prompt": "...", "skills": ["nosuch"]}` → a tool error classified invalid-params, and **no** run appears in `forge_run_status`/the log.
- [ ] **Step 2: Run** `cargo test -p forge-mcp` and `cargo test -p forge-cli --test mcp` → FAIL.
- [ ] **Step 3: Implement** per CONTEXT REFERENCES: schema property, `opt_str_array` beside `opt_u64` (`:852`), pass into `RunOptions` (`:567-571`), and the start-error mapping (`:573-575`) distinguishing `ForgeError::Skill` → `invalid_params` from the existing `session_busy`.
  - **GOTCHA:** `additionalProperties: false` in `run_schema` (`:176`) means the schema *is* the contract — forget it and every client call with `skills` is rejected before the handler runs.
  - **GOTCHA:** an invalid-params refusal must happen **before** `subscribe`/`start_run_with_options` side effects where possible (schema/argument errors first, then subscribe at `:557-558` — mirroring the existing order: prompt validation `:534-540` precedes subscription).
  - **PATTERN:** `tools.rs:839-874` (argument helpers), `:560-575` (start + refusal handling).
- [ ] **Step 4: Run** `cargo test -p forge-mcp` → PASS; `cargo test -p forge-cli --test mcp` → PASS.
- [ ] **Step 5:** `just verify` → PASS. **Commit** — `git commit -m "feat(mcp): forge_run accepts explicit skills"`
- VALIDATE: `cargo test -p forge-mcp && cargo test -p forge-cli --test mcp`
- SATISFIES: AC6

### Task 5: ACP `session/prompt` gains `_meta.forge.activateSkills`

**Files:**
- Modify: `crates/forge-acp/src/protocol.rs` (`PromptRequest.meta`)
- Modify: `crates/forge-acp/src/server.rs` (`TurnClaim`, `claim_turn`, `drive_turn`, `run_turn`; unit tests if the module has them, else `dispatch/tests.rs` style)
- Modify: `crates/forge-cli/tests/acp.rs` (process test)

- [ ] **Step 1: Write the failing tests.** Unit: `PromptRequest` deserializes with no `_meta` (back-compat), with `"_meta": {"forge.activateSkills": ["demo"]}`, and rejects a non-array/non-string value at `claim_turn` with `invalid_params`. Process (`acp.rs`, `session/prompt` shape at `:289-291`): scaffold the demo skill in the session's project root, send a prompt with the `_meta` key → the turn completes and the forge session log (the ACP session id *is* the forge session id, so it is inspectable) shows `skill_activated: demo`; a bad name → an error response, no run.
- [ ] **Step 2: Run** `cargo test -p forge-acp` and `cargo test -p forge-cli --test acp` → FAIL.
- [ ] **Step 3: Implement** per CONTEXT REFERENCES: the `meta` field with `rename = "_meta"`; extraction in `claim_turn` (synchronous, before the slot is claimed); `TurnClaim::activate_skills`; pass into `run_turn`'s `RunOptions` (`:523-529`).
  - **GOTCHA:** `PromptRequest` has `rename_all = "camelCase"` (`protocol.rs:323`) — without the explicit `rename = "_meta"` the field would read `meta`, and nobody's `_meta` would ever land.
  - **GOTCHA:** validate in `claim_turn`, not in the spawned `drive_turn`: protocol errors belong to the reader task's synchronous refusal (the same reason the turn slot is claimed there, `:443-452`).
  - **PATTERN:** `server.rs:453-481` (`claim_turn`), `dispatch.rs:145` (`prompt_text`'s `invalid_params` shape).
- [ ] **Step 4: Run** `cargo test -p forge-acp` → PASS; `cargo test -p forge-cli --test acp` → PASS.
- [ ] **Step 5:** `just verify` → PASS. **Commit** — `git commit -m "feat(acp): session/prompt accepts forge.activateSkills via _meta"`
- VALIDATE: `cargo test -p forge-acp && cargo test -p forge-cli --test acp`
- SATISFIES: AC7

### Task 6: BDD scenarios + docs sweep

**Files:**
- Modify: `tests/features/skills.feature` (two new scenarios)
- Modify: `crates/forge-cli/tests/bdd/steps.rs` (new steps, mirroring `:394-476`)
- Modify: `docs/reference.md` (`:530-533` chat `/name` paragraph — the lexical reliance sentence goes; `:1105-1107` Skills section — explicit activation + `--skill`; `:1825-1827` known-limitations bullet — the `/name` lexical clause is removed, the discovery-is-lexical clause at `:1813-1817` stays true)
- Modify: `ARCHITECTURE.md` ("Life of a run": `skills matched, graph context seeded` → `skills activated (explicit, then matched), graph context seeded`)
- Verify-only: `README.md` (its single skill mention, `:111`, is a doctor list — confirm no change needed)

- [ ] **Step 1: Write the scenarios:**
  ```gherkin
  Scenario: A slash-named skill activates explicitly, never by matching luck
    Given a project contains a SKILL.md skill
    When I chat with the lines "/demo" and "/quit"
    Then the chat output shows the skill activated
    And the session events include a skill activation for "demo"

  Scenario: forge run --skill activates without a matching prompt
    Given a project contains a SKILL.md skill
    When I run a task that does not match the skill with --skill "demo"
    Then its instructions are activated and logged
  ```
  Steps mirror the existing skill steps: the chat one uses `run_forge_with_stdin` (`world.rs:243-315`); the `--skill` one uses `run_forge(&["run", "--skill", "demo", "zz unrelated qq"])` with the model set to `mock-local` as `task_matches_skill` does (`steps.rs:417-428`); the log assertion reuses the `skill_activated` parse at `:446-452`. The prompt must share no ≥3-char token with the skill's name/description (`demo skill description`) — `"zz unrelated qq"` qualifies; keep it that way, since the scenario's entire point is that activation did *not* come from matching.
- [ ] **Step 2: Run** `cargo test -p forge-cli --test bdd` → FAIL (steps undefined).
- [ ] **Step 3: Implement** the steps, then the docs edits.
  - **GOTCHA:** the chat scenario's `/quit` line: one prompt, then quit — batch-mode EOF drains the queue, so no second `/quit` is needed (§12.3; `chat.feature:6-10` shows the shape).
  - **GOTCHA:** keep both existing `skills.feature` scenarios green **unmodified** — they are the lexical-default regression net (AC8).
  - **PATTERN:** `steps.rs:394-476`, `chat.feature:6-16`.
- [ ] **Step 4: Run** `cargo test -p forge-cli --test bdd` → PASS.
- [ ] **Step 5:** `just verify` → PASS. **Commit** — `git commit -m "feat(skills): BDD + docs for explicit skill activation"`
- VALIDATE: `cargo test -p forge-cli --test bdd`
- SATISFIES: AC8, AC9

## TESTING STRATEGY

- **Runtime level (mock providers, no process):** the six `service/tests.rs` tests of Task 1 are the heart — they prove activation is exact, additive, dedup'd, validated synchronously with a clean log, and that the fast path cannot bypass an explicit request. Instructions-reaching-the-model is asserted on `MockModel::recorded()` / `ScriptedMockModel::recorded()` (the seam `model.rs:65-66` and `scripted.rs:54-56` already provide), not on output text.
- **Chat wiring:** pure parse tests (`command.rs`), pure state-machine tests including the queued-`/skill` case (`controller.rs`), and one driver test (`app.rs` + skill-capable `FakeHost`) proving `/xy` → `skill_activated` with a name that cannot lexically match. No TTY anywhere (the crate's standing rule).
- **Per-surface coverage:** each pass-through gets one process test of the happy path and one of the unknown-name refusal (CLI: `tests/cli.rs`; MCP: `tests/mcp.rs` + `tools.rs` unit tests for the argument helper; ACP: `tests/acp.rs` + protocol/claim unit tests). The refusal tests matter as much as the happy paths: they are what distinguishes "explicit" from "best effort".
- **BDD fit:** the two new `skills.feature` scenarios extend the existing file and reuse its fixture/log-assertion steps; the two existing scenarios are the untouched regression net proving lexical discovery is unchanged (AC8). No new world helper is needed (`run_forge_with_stdin` already exists, `world.rs:243`).
- **Not tested, deliberately:** rustyline rendering of the activation line (already covered by the generic `SkillActivated → "  - skill: {name}"` render test, `render.rs:742`); real-model behavior (mocks are the contract); REST (out of scope).

## VALIDATION COMMANDS

Run in the worktree (`worktrees/t6-skill-activation`), in this order, all green before each commit and at completion:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p forge-runtime -p forge-skills -p forge-chat
cargo test --workspace
cargo test -p forge-cli --test bdd
```

`just verify` covers all of the above plus `lint-ffi`; it is the per-commit gate.

## ACCEPTANCE CRITERIA

- **AC1 — exact activation:** a run whose `RunOptions.activate_skills` names an existing skill records exactly one `skill_activated` event for it and injects its instructions as the `Active skill \`<name>\` instructions:` system message — with a prompt that shares no matchable token with the skill (proven with the two-character name `xy`).
- **AC2 — additive, never replacing:** lexical `match_task` still activates its own matches in the same run; a skill reached both ways activates exactly once (one event, one system message); explicit names are not bounded by `MAX_MATCHED_SKILLS`.
- **AC3 — loud on typos, clean log:** an unknown name in `activate_skills` is a synchronous, typed `ForgeError::Skill` from `run_with_options`/`start_run_with_options` naming the skill and the available ones; no `run_started` or any other event is written; an `activate()` failure of an explicit name inside a run fails the run rather than continuing without it.
- **AC4 — chat `/name` is deterministic:** `/name [text]` activates exactly that skill through the explicit path (transcript shows `  - skill: name`); a queued mid-turn `/name` keeps its skill; a two-character skill name works; parsing/completion rules (snapshot-gated, commands-win) are unchanged.
- **AC5 — CLI:** `forge run --skill <name>` (repeatable) activates; unknown name exits non-zero naming it.
- **AC6 — MCP:** `forge_run` accepts optional `skills: string[]`; unknown name → invalid-params-class tool error and no started run; schema keeps `additionalProperties: false` with `skills` included.
- **AC7 — ACP:** `session/prompt` with `"_meta": {"forge.activateSkills": ["name"]}` activates; absent `_meta` behaves exactly as before; malformed values are `invalid_params`.
- **AC8 — lexical default untouched:** with no explicit skills, runs are byte-identical to before — the existing `skills.feature` scenarios and `full_run_emits_ordered_events` pass unmodified.
- **AC9 — docs in the same commits:** `docs/reference.md` (chat `/name` paragraph, Skills section, known-limitations bullet) and `ARCHITECTURE.md` (life-of-a-run line) describe the new behavior; `README.md` verified unchanged or updated.

Ticket acceptance mapping: "`/name` activates exactly that skill (activation event recorded)" = AC1+AC4; "the other three surfaces gain the same option" = AC5+AC6+AC7; "lexical matching stays as the default discovery path" = AC2+AC8.

## OPEN QUESTIONS / ASSUMPTIONS

1. **Should explicit activations be recorded on the run (e.g. in `run_started`) so `forge resume` re-activates them?** Not settled upstream — the design doc does not address resume. **Recommended default (assumed by this plan): no.** Activation is per-run on every surface, exactly as lexical activation is per-run today; `resume()` re-runs discovery on the original task as it already does. Recording would be an additive event-schema change with replay consequences; if wanted, it is a follow-up of its own.
2. **The ACP `_meta` key spelling.** The design doc sanctions per-request `_meta` but names no key. **Recommended default (pinned): `"forge.activateSkills"`** — namespaced, camelCase to match the adapter's wire convention (`protocol.rs:323`). A client that sends nothing is unaffected; a different key is a one-line change before this ships anywhere documented.
3. **Does `forge serve` (REST `POST /v1/runs`) get the option here?** Its handler uses the options-free `start_run` (`handlers.rs:136`), so inclusion means a request-body schema change the ticket does not ask for. **Recommended default (assumed): out of scope** — follow-up alongside the REST body's next change; the runtime field makes it a five-line diff when wanted.

Assumptions (no action needed unless falsified): all `RunOptions` construction sites use `..RunOptions::default()` (verified by workspace-wide grep — the field is source-compatible); `ForgeError::Skill` and its constructor exist (`error.rs:22,85-86`); the chat snapshot is the only gate for a `/name` parse, so chat cannot produce an unknown explicit name at parse time.

## NOTES

- **Why the `skill_prompt` template stays.** With explicit activation it is no longer *load-bearing*, but it remains the right user-turn text: it tells the model the user invoked the skill by name (mirroring today's behavior when lexical matching works, so the working case is byte-identical), and it gives a bare `/name` a non-empty prompt for free. Removing it would change `run_started` history for no correctness gain. Its doc comment, which currently says it exists *for* `match_task`, is rewritten.
- **Why validation compares against `list()`, then activates again in the run.** `list()` is the cheap frontmatter read that produces the "available: …" message at a point where the caller can still be told synchronously; `activate()` in `run_inner` is the same single load path every activation uses, so explicit and lexical skills cannot drift apart in how instructions are read. The double resolution is a few microseconds of frontmatter parsing; the failure mode it leaves (delete-between) fails the run loudly, which is the honest answer for an explicit request.
- **Why the fast path skips explicit-skill turns** (design call 5): AC1 says the activation event is recorded; the fast path's early return (`service.rs:1527-1644`) precedes the skill block, so an engine-enabled build could otherwise dispatch a `/name` turn with no activation and no instructions. The gate makes the feature hold on brain-enabled builds, not only in mocks.
- **Nothing about the fast path's *own* skill use changes** — it never had one.
- The ticket's file list ("forge-runtime, forge-chat, mcp/acp pass-through") plus `forge-cli` for the `run` flag matches the tasks above exactly; the ticket's ~500–800 line estimate holds.

## AMENDMENTS

(none yet)
