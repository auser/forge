# TICKET-4 — Path Completion in the Prompt — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** In `forge chat` on a TTY, a word starting with `@` Tab-completes against the project's real files — `explain @src/ma` + Tab offers `@src/main.rs` — anywhere in the line, bounded on huge repos, with zero behavior change for any other token. Nothing else moves: slash completion, piped mode, and the `forge-chat` purity rule all stay exactly as they are.

**Architecture:** The existing completion pipeline is already the right shape and is not altered in kind: `forge-chat` decides *what* Tab offers as a pure function of a `CompletionSnapshot` (`Command::complete`, `crates/forge-chat/src/command.rs:180`); the snapshot travels inside `Prompt` so the editor thread never touches the filesystem or calls back into the host (`crates/forge-chat/src/io.rs:110-130`); `forge-cli`'s rustyline `ChatHelper` delegates wholesale (`crates/forge-cli/src/chat/terminal_io.rs:413-428`) and needs **no change**. This ticket adds one field to the snapshot (`paths`), one method to the `ChatHost` seam (`project_files()`, the shell does the I/O), one branch in the pure completer (the `@` rule), and one shell implementation reading the project graph's file index. It is the smallest possible change that meets the design doc's bar: *"half-working path completion is worse than none"* (`docs/superpowers/specs/2026-09-24-interactive-chat-ui-design.md` §14, line 1128-1129).

**Tech Stack:** Rust edition 2024, existing crates only — **no new dependencies** (the graph's `walkdir` use stays inside `forge-graph`; the shell only *reads* the built graph state).

**Ticket:** `specs/tickets/interactive-chat-feel.md` TICKET-4 (lines 72-83).
**Inherited design:** `docs/superpowers/specs/2026-09-24-interactive-chat-ui-design.md` §9.2 (Completion, lines 698-716 — whose "No file-path completion in v1" bullet this ticket retires) and §14 (path-completion follow-up, lines 1128-1129).

## Feature Description

Tab completion of project file paths in the chat prompt. A whitespace-delimited word beginning with `@`, at **any** position in the line (first word of a prompt, mid-prompt, or a command argument such as `/graph @src/ma`), completes by exact case-sensitive prefix against the project-relative, `/`-separated file list of the built project graph. The replacement re-inserts the `@` (the buffer keeps what the user typed); the display is the value itself, matching how `/model` and `/attach` argument candidates already display. With multiple matches, rustyline's `CompletionType::List` (`terminal_io.rs:376`) inserts the longest common prefix first — `@src/forge-ch` + Tab lands on `@src/forge-chat/` — so directory drill-down works with files-only candidates and no directory entries. The file list is refreshed once per submitted line through the existing `App::refresh_completions` (`crates/forge-chat/src/app.rs:338-366`), gated in the shell by the graph file's mtime so a steady-state refresh is one `stat`.

## User Story

As a forge chat user, I want to type `@src/ma` and hit Tab so the prompt fills in `@src/main.rs`, so that I can point the model at real project files without leaving the prompt or misremembering paths — and I never want Tab to do something surprising on a word that is not a path reference.

## Problem

The design deliberately shipped no path completion: `Command::complete` returns nothing for anything that is not a slash command or a known command argument (`command.rs:228-230`: *"No file-path completion in v1: forge reads files through tools, and half-working path completion is worse than none"*), and `docs/reference.md:1811-1813` records the limitation. That was the right call for v1, and it is now the recorded follow-up this epic slice implements. The gap is real: referencing files is the most common thing a chat user does, and today the only help is typing from memory. The hard part is not matching strings — it is doing it without a half-working state: a trigger that only sometimes fires, a source that invents paths, or an unbounded scan that stalls the prompt on a monorepo.

## Solution

Three design calls, made explicitly:

**1. Trigger: a leading `@` on the current word — never an unmarked path-like token.**
The ticket's acceptance names the `@` shape ("Tab on `@src/ma`"), and it is the only trigger that meets the no-half-working bar. Any-token completion needs a heuristic for "does this word mean a path?" (contains `/`? starts with `./`? a bare word like `main`?) and every heuristic both fires where it wasn't wanted (changing behavior for non-path tokens, which the ticket forbids) and stays silent where a user expected it — the definition of half-working. A leading `@` is unambiguous intent, so completing is *always* safe: the rule is total (`@`-word → project-path candidates, at any word position) rather than contextual. It is also the convention peer agent CLIs have already taught users. The `@` check runs before the existing slash-command branch in `Command::complete`, so `@` works at word 0 (a prompt opening with a file reference) and after commands uniformly. Words are whitespace-delimited exactly as today's completer defines them (`command.rs:186-192`); `email@host` does not start with `@` and never triggers.

**2. Data source: the built project graph's file index, reached through the `ChatHost` seam — not a filesystem walk, and not the runtime's private graph handle.**
`LocalGraph::open(root)` (`crates/forge-graph/src/graph.rs:64-81`) reads `.forge/graph/graph.json`; `ProjectGraph::files()` (`crates/forge-core/src/graph.rs:52`, implemented at `graph.rs:624-626`) returns its `BTreeMap` keys — project-relative, `/`-separated, already sorted. This is the harness's single definition of "project files": the graph's `SKIP_DIRS` policy (`graph.rs:13-24`) already excludes `.git`, `.forge`, `target`, `node_modules`, `dist`, `build` and friends — a filesystem walk would need to re-derive that policy and would then be a *second* definition that can disagree with the first (two sources of "what files exist" is its own half-working state). The terminal-free crate law holds: `forge-chat` gains `ChatHost::project_files(&self) -> Vec<String>` returning plain owned data (the same shape as `models()`/`skills()`, `crates/forge-chat/src/host.rs:27-28`), and `CliHost` — the shell, which already opens the graph per call for `/graph` (`crates/forge-cli/src/chat/host.rs:232`) — does the reading. Deliberately *not* `AgentService`: the runtime holds `Option<Arc<dyn ProjectGraph>>` (`crates/forge-runtime/src/service.rs:357`) but exposes no getter (`service.rs:446-464` has `model`/`execution`/`skills`/`sessions`/`config` only), and growing one would hand every front end a mutable-build API to serve a read-only listing the CLI already knows how to make. Degradation matches the established precedent: no built graph → no candidates, empty-not-error — the same contract `/graph` has (`host.rs:375-385` tests it) and the runtime's own wiring has ("Wire the project graph when one has been built; absence never blocks a run", `crates/forge-cli/src/commands/service.rs:148-153`). A *corrupt* graph file also degrades to empty (logged at debug): `project_files` returns `Vec`, not `Result`, because completion must never break the prompt — the one deliberate contrast with `graph_context`'s `Result`.

**3. Bounds: three, each with a named home.**
- *Candidate bound* (pure layer): at most `MAX_PATH_CANDIDATES = 100` matches per completion, taken from the front of the sorted match run — a longer listing is unusable in `CompletionType::List` anyway, and typing one more char narrows further.
- *Snapshot bound* (shell): at most `MAX_PROJECT_PATHS = 50_000` paths loaded into the snapshot, truncated in the graph's sorted order. This caps what `Prompt` clones carry through the per-`readline` `set_helper` (`terminal_io.rs:335-337`).
- *Refresh bound* (shell): `CliHost` caches the loaded list keyed by the graph file's `mtime` (with "file absent" as a first-class cached state), so `refresh_completions` — which runs at startup and after every submitted line (`app.rs:159`, `app.rs:180`, `app.rs:378`) — costs one `stat` in the steady state and re-parses only after a `forge graph build`. Measured stakes: this repo's own `.forge/graph/graph.json` is ~0.9 MB; a 50k-file monorepo's is ~10 MB of JSON, and an unguarded per-line reparse would be exactly the "bounded on huge repos" failure the ticket's acceptance forbids. (This cache is also what keeps TICKET-4 from adding to the per-line executor cost TICKET-8 exists to remove.)

Two smaller rules, both consequences of the whitespace tokenization the completer already has: paths **containing whitespace are never offered** (they cannot round-trip the word-splitting — an inserted space would end the token the next Tab reads), and matching is **case-sensitive `starts_with`**, consistent with the existing `filtered` helper (`command.rs:317-321`). Both are stated in code comments and tests so they read as decisions, not accidents.

What `@path` *means* after submission is unchanged: it is prompt text, verbatim. Models handle file references in this shape natively, and forge's tools read by path. Expanding `@path` into attached file contents is a runtime/prompt-assembly feature, not completion, and is out of scope below.

## Out of Scope

- **Fuzzy or substring matching UI.** Prefix-only, like every other candidate list in this completer. Fuzzy finders are a different feature (and a different dependency).
- **Quote-aware completion.** The completer is whitespace-tokenized and quote-blind; a quoted `"@src/ma` does not start with `@`, so nothing fires — degradation is to *no candidates*, never to wrong ones. No quoting grammar is added.
- **`@`-expansion at submission** (attaching file contents, Claude Code-style). The submitted line is unchanged by this ticket.
- **Paths outside the project root, absolute paths, `~`.** The graph is project-relative by construction; there is nothing to complete *from*.
- **Directory candidates.** rustyline's longest-common-prefix insertion already drills down one segment at a time.
- **Piped/non-TTY mode.** §12.3: "no history file is written and no completion exists — there is nobody to complete for." `PipedIo` is untouched.
- **`.gitignore` beyond the graph's policy.** Completion inherits the graph's file set (SKIP_DIRS), the one definition the whole harness uses; a second ignore engine here would drift from it.
- **Building or refreshing the graph from the chat.** Absence degrades to no candidates; the user runs `forge graph build` (incremental). The chat never writes graph state.
- **BDD scenarios.** Completion is TTY-only and the cucumber harness drives piped stdin; the end-to-end proof is the pty test (Task 4).

## Metadata

- **Ticket:** TICKET-4 of `specs/tickets/interactive-chat-feel.md` (independent; Wave 1).
- **Estimate:** ~450-650 lines including tests (ticket: 400-700). 5 tasks.
- **Crates touched:** `forge-chat` (pure rule + seam), `forge-cli` (shell implementation + e2e test). No new crates, no new dependencies, no config keys.
- **Docs touched:** `docs/reference.md` (two spots), `crates/forge-chat/src/lib.rs` crate doc, the design doc (amendment).
- **Run all commands in:** the ticket worktree (`worktrees/t4-path-completion`).

## CONTEXT REFERENCES

### Files to read first (worktree paths; line numbers at HEAD 5c72985)

- `crates/forge-chat/src/command.rs:180-244` — `Command::complete`, the function this ticket extends. Note the structure: clamp `pos` (`351-357`), find the current whitespace-delimited word (`186-192`), first-word slash branch (`196-210`), then the first-argument-per-command branch (`212-243`).
- `crates/forge-chat/src/command.rs:228-230` — the "No file-path completion in v1" comment this ticket replaces, and `535-537` — the test whose comment says the same ("No path completion in v1, and no guessing inside a prompt"). The *assertion* stays green (a non-`@` word still completes nothing); only its comment moves.
- `crates/forge-chat/src/io.rs:110-139` — `CompletionSnapshot` (add `paths` here) and `CompletionCandidate { replacement, display }`.
- `crates/forge-chat/src/host.rs:19-45` — the `ChatHost` trait; every method returns owned plain data ("which is what lets a `FakeHost` drive the whole loop"). `project_files` joins this list.
- `crates/forge-chat/src/app.rs:328-366` — `refresh_completions`, the one place the snapshot is rebuilt (startup `app.rs:159`, after session resolution `app.rs:180`, and after each submitted line `app.rs:378`). Its doc comment enumerates the synchronous filesystem reads it performs — extend the enumeration, don't hide the new one. `app.rs:359-364` is the struct literal that gains the field; `app.rs:320-326` (`prompt()`) clones the snapshot into every `Prompt`.
- `crates/forge-cli/src/chat/host.rs:203-248` — `graph_context` + its extracted free function `context_lines`: the established pattern for "the shell opens the graph and maps it to plain data", including the extracted-for-testability shape (`context_lines` exists "so it is testable without a full `CliHost`"). Task 3 copies this shape exactly.
- `crates/forge-cli/src/chat/host.rs:350-385` — the real-graph test pattern: tempdir, write files, `LocalGraph::open` + `ProjectGraph::build`, assert on results.
- `crates/forge-graph/src/graph.rs:64-81` (`open`: missing file → empty state; wrong version → empty state; parse error → `Err`), `13-24` (`SKIP_DIRS`), `624-626` (`files()` over sorted `BTreeMap` keys), `96-128` (`walk` — the policy owner; read-only context, **not** reused here).
- `crates/forge-cli/src/commands/service.rs:148-153` — the runtime's own "graph only when built" wiring; the degradation precedent.
- `crates/forge-cli/src/chat/terminal_io.rs:404-453` — `ChatHelper`/`Completer`/`DisplayCandidate`. **Verify no change is needed**: the helper delegates to `Command::complete` with the whole snapshot, so path candidates flow through unchanged. If you find yourself editing this file beyond a doc touch, the design has leaked — stop and re-read the seam.
- `crates/forge-cli/tests/chat.rs:692-792` — the `pty` module (`libc::openpty` + `setsid` + `TIOCSCTTY`, no new crate), and `816-880` — the existing pty test whose structure Task 4 copies: `pty::spawn` → `tail_stream` → `wait_for(&output, marker, 30s)` → byte-write → assert. Helpers to reuse: `forge()` (`106-119`), `scaffold()` (`182-207`), `tail_stream` (`291`), `wait_for` (used at `832`).
- `crates/forge-chat/src/testing.rs:79-81, 257` — `ScriptedIo`/`ScriptedIoHandle::read(&mut self, _prompt: Prompt)`: the prompt is currently **ignored**, so Task 2 adds capture of `prompt.completions` for the wiring test. `317-322, 439-471` — `FakeHost` fields and its `ChatHost` impl, which gains `project_files`.
- `docs/reference.md:551-553` (the "Tab completes a slash command, a skill name, or a `/attach` job id" sentence) and `1802-1813` (the Known-limitations bullet ending "there is no path completion … nothing filesystem-shaped") — both updated in Task 5.
- `docs/superpowers/specs/2026-09-24-interactive-chat-ui-design.md:698-716` (§9.2) and `1128-1129` (§14 bullet) — amended, not rewritten (Task 5).

### New files

None. Every change lands in an existing module.

### Patterns to copy (short real excerpts)

The candidate-construction style for *values* (display == replacement), `command.rs:234-243`:

```rust
// Argument candidates display as themselves — they are values, not
// menu entries with something to explain.
(
    word_start,
    filtered(candidates.into_iter(), word)
        .into_iter()
        .map(|replacement| CompletionCandidate {
            display: replacement.clone(),
            replacement,
        })
        .collect(),
)
```

The extracted-free-function testability pattern, `crates/forge-cli/src/chat/host.rs:213-214` (doc on `context_lines`):

```rust
/// [`ChatHost::graph_context`]'s body, pulled out so it is testable without
/// a full `CliHost` (which needs a whole runtime to construct).
```

The pty test's bounded-wait discipline, `tests/chat.rs:826-841`: wait for the banner marker, then wait for the actual `"> "` prompt — *"The prompt text only exists once `readline()` is genuinely blocked in raw mode on the other end of this pty"* — with 30 s bounds and the reason written down.

## Global Constraints

- Typed errors via `thiserror`; **no `unwrap`/`expect` in production code** (tests may). The completer runs on the editor thread — a panic there takes the terminal down (`command.rs:597-599` says so on the existing test); the new branch is slice-and-iterator only.
- Foreground `just verify` (`check` + `lint` + `lint-ffi` + `test` + `bdd` + `cargo fmt --all --check`, per the `Justfile`) green **before each commit**.
- `forge-chat` keeps its stated rule (**no terminal; filesystem access only through `AgentService`** — everything else arrives as plain data through `ChatHost`). The crate doc at `crates/forge-chat/src/lib.rs:9-17` is amended in Task 2 to name the new host-side read honestly, as that paragraph demands of itself.
- Mocks stay invisible; no chat surface changes wording. No new dependencies, no new config keys, no pty crate (the `libc` pty harness already exists in `tests/chat.rs`).
- ASCII-only, no width-sensitive output: path candidates are plain text values; nothing is drawn.
- Docs that describe behavior are updated in the same commit series as the behavior (Task 5), never left describing v1.

## Review Focus

The input classes most likely to bite a real user, each pinned to a test:

1. **A non-`@` word must complete exactly as before** — the ticket's "no behavior change for non-path tokens". The entire existing `Command::complete` test table (`command.rs:480-613`) stays unedited and green; Task 1 adds only new tests. If an existing assertion needs editing, the branch leaked.
2. **The `@` rule must be total, not contextual** — word 0, mid-prompt, and command-argument positions all behave identically (one test each in Task 1). A trigger that fires only in some positions is the half-working state the design doc forbids.
3. **Huge repos must not stall the prompt** — the two caps and the mtime gate: candidate cap (pure test with >100 matches), snapshot cap (host test with a parameterized cap), reload-only-on-change (host test: rebuild after adding a file → new path appears; second call with unchanged mtime → same data).
4. **An unbuilt or corrupt graph must degrade to silence, never an error in the transcript** — host tests for the missing-file and garbage-file cases (`LocalGraph::open`'s `Err` path, `graph.rs:69-70`, becomes `vec![]` + a `tracing::debug!`, not a panic or a user-facing line).
5. **The replacement keeps the `@`** — inserting `src/main.rs` bare would delete the user's trigger character and change what the prompt says. One pure test asserts `replacement.starts_with('@')` and equality with `format!("@{path}")`.
6. **Whitespace-containing paths never appear** — they cannot round-trip the completer's own word-splitting. Pure test.

---

## IMPLEMENTATION PLAN (phases)

- **Phase 1 — the pure rule (Task 1).** The `@` branch and the snapshot field in `forge-chat`, fully unit-tested with no I/O. After this task the feature exists as logic; nothing feeds it yet.
- **Phase 2 — the seam and the shell (Tasks 2-3).** `ChatHost::project_files` + `App::refresh_completions` wiring + test doubles (Task 2, forge-chat); `CliHost`'s mtime-gated graph reader (Task 3, forge-cli). After this, the feature works end to end in-process.
- **Phase 3 — proof and docs (Tasks 4-5).** The real-pty Tab test through the compiled binary (Task 4); the docs sweep retiring the recorded limitation (Task 5).

---

## STEP-BY-STEP TASKS

### Task 1: the `@` path-completion rule in `Command::complete`

**ACTION** `crates/forge-chat/src/io.rs`, `crates/forge-chat/src/command.rs`

**IMPLEMENT**

1. `io.rs`: add to `CompletionSnapshot` (at `io.rs:118-130`):

```rust
    /// Project-relative file paths from the built project graph, sorted,
    /// `/`-separated, capped by the host. The source of `@`-word
    /// completion (TICKET-4); empty when no graph is built, which degrades
    /// `@`-completion to silence — never to an error.
    pub paths: Vec<String>,
```

2. `command.rs`: in `Command::complete`, immediately after `word` is computed (`command.rs:192`) and **before** the `word_start == 0` slash branch, add the `@` rule:

```rust
/// At most this many path candidates per completion. The listing is
/// `CompletionType::List`, which is unusable past a screenful; typing one
/// more character narrows further, so a cap hides nothing reachable.
const MAX_PATH_CANDIDATES: usize = 100;
```

```rust
// A word starting with `@` is a file reference and completes project
// paths, at any position — first word of a prompt, mid-prompt, or a
// command argument. The trigger is explicit intent, which is what makes
// offering always safe: unmarked words never complete as paths (the
// design doc's bar: half-working path completion is worse than none).
// Paths containing whitespace are never offered — they cannot round-trip
// this completer's own whitespace word-splitting.
if let Some(prefix) = word.strip_prefix('@') {
    let candidates = snapshot
        .paths
        .iter()
        .filter(|path| !path.chars().any(char::is_whitespace))
        .filter(|path| path.starts_with(prefix))
        .take(MAX_PATH_CANDIDATES)
        .map(|path| {
            // The replacement keeps the `@`: inserting the bare path
            // would delete the user's trigger character.
            let replacement = format!("@{path}");
            CompletionCandidate { display: replacement.clone(), replacement }
        })
        .collect();
    return (word_start, candidates);
}
```

3. Replace the now-stale comment at `command.rs:228-230` ("No file-path completion in v1…") with one line: path completion lives in the `@` branch above and is deliberately the only in-prompt completion. Fix the stale comment on the test at `command.rs:535-537` the same way (assertion unchanged).
4. Update the test helper `snapshot()` (`command.rs:363-373`) with a `paths` field — **in sorted order**, which is the host's contract (the graph's `BTreeMap` keys arrive sorted; the completer preserves order rather than re-sorting): `vec!["docs/guide.md".into(), "src/lib.rs".into(), "src/main.rs".into(), "with space.rs".into()]`.

**PATTERN** `crates/forge-chat/src/command.rs:234-243` (value-style candidates), `command.rs:601-613` (the never-panic discipline).

**GOTCHA** The `word_start == 0` early return (`command.rs:196-210`) requires a leading `/`; placing the `@` branch *after* it would silently disable `@` at the start of a line — the most common position. The branch order in step 2 is the feature. Also: `word.strip_prefix('@')`, not `word.starts_with('@')` + re-slice — one source for the prefix, no index arithmetic.

**Steps:**

- [ ] **Step 1: Write the failing tests** in `command.rs`'s existing test module:

```rust
    /// Review Focus 2: the rule is total — word 0, mid-prompt, and after a
    /// command all behave identically.
    #[test]
    fn an_at_word_completes_project_paths_anywhere_in_the_line() {
        let s = snapshot();
        for (line, pos) in [
            ("@src/ma", 7),                  // word 0
            ("explain @src/ma", 15),         // mid-prompt
            ("/graph @src/ma", 14),          // a command argument
        ] {
            let (_, items) = Command::complete(line, pos, &s);
            assert_eq!(
                replacements(&items),
                vec!["@src/main.rs"],
                "completing {line:?}"
            );
        }
    }

    /// Review Focus 5: the replacement keeps the trigger character.
    #[test]
    fn path_replacements_keep_the_at_sign() {
        let s = snapshot();
        let (start, items) = Command::complete("explain @src/li", 15, &s);
        assert_eq!(start, 8, "the whole @-word is replaced");
        assert_eq!(replacements(&items), vec!["@src/lib.rs"]);
        assert_eq!(items[0].display, "@src/lib.rs", "a value displays as itself");
    }

    /// Review Focus 1: unmarked words never complete as paths.
    #[test]
    fn a_bare_path_like_word_completes_nothing() {
        let s = snapshot();
        assert!(Command::complete("explain src/ma", 14, &s).1.is_empty());
        assert!(Command::complete("src/ma", 6, &s).1.is_empty());
    }

    /// Review Focus 6: a path with a space cannot round-trip the word
    /// splitting, so it is never offered.
    #[test]
    fn paths_with_whitespace_are_never_offered() {
        let s = snapshot();
        assert!(Command::complete("@with", 5, &s).1.is_empty());
    }

    /// Review Focus 3 (pure half): the listing is capped.
    #[test]
    fn path_candidates_are_capped() {
        let mut s = snapshot();
        s.paths = (0..150).map(|i| format!("src/file{i:03}.rs")).collect();
        let (_, items) = Command::complete("@src/", 5, &s);
        assert_eq!(items.len(), MAX_PATH_CANDIDATES);
        // Sorted order, taken from the front: deterministic, and one more
        // typed character narrows further.
        assert_eq!(items[0].replacement, "@src/file000.rs");
    }

    /// A bare `@` offers the first page of the project, sorted.
    #[test]
    fn a_bare_at_offers_the_first_candidates() {
        let s = snapshot();
        let (_, items) = Command::complete("@", 1, &s);
        assert_eq!(
            replacements(&items),
            vec!["@docs/guide.md", "@src/lib.rs", "@src/main.rs"]
        );
    }
```

Extend the existing nonsense-position test (`command.rs:601-613`) with one line: `Command::complete("@caf\u{e9}", 5, &s)` must not panic and must return a char-boundary `start`.

- [ ] **Step 2: Run** `cargo test -p forge-chat command` → FAIL (no `paths` field, no `@` branch).
- [ ] **Step 3: Implement** per IMPLEMENT above.
- [ ] **Step 4: Run** `cargo test -p forge-chat` → PASS, with **zero edits to pre-existing assertions**.
- [ ] **Step 5:** `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings` → PASS. **Commit** — `git add -A && git commit -m "feat(chat): @-triggered path completion in the pure completer"`

**VALIDATE** `cargo test -p forge-chat`

**SATISFIES** AC1 (pure half), AC3, Review Focus 1/2/5/6 + candidate-cap half of 3.

---

### Task 2: the `ChatHost::project_files` seam and snapshot wiring

**ACTION** `crates/forge-chat/src/host.rs`, `crates/forge-chat/src/app.rs`, `crates/forge-chat/src/testing.rs`, `crates/forge-chat/src/lib.rs`

**IMPLEMENT**

1. `host.rs`: add to the `ChatHost` trait (after `skills()`, `host.rs:28`):

```rust
    /// Project-relative file paths for `@`-completion: sorted,
    /// `/`-separated, and already capped by the implementation. Empty when
    /// no project graph has been built — `@`-completion degrades to
    /// silence, never to an error, so this returns data, not a `Result`.
    fn project_files(&self) -> Vec<String>;
```

2. `app.rs` `refresh_completions` (`app.rs:338-366`): add `let paths = self.host.project_files();` and the `paths` field to the `CompletionSnapshot` literal (`app.rs:359-364`). Extend the method's doc comment: the snapshot now also reads the graph's file index *through the host*, and the host is required to keep that read bounded (the `CliHost` implementation mtime-gates it; Task 3) — one honest sentence, matching how the existing doc names `list_runs()`' cost.
3. `testing.rs`: `FakeHost` gains a `paths: Vec<String>` field (default empty) plus a builder-style `with_paths(mut self, paths: Vec<String>) -> Self`; implement `project_files` returning `self.paths.clone()`. `ScriptedIo`'s `Shared` gains `last_completions: Mutex<CompletionSnapshot>`; `ScriptedIoHandle::read` (`testing.rs:257`) stores `prompt.completions` before answering; `ScriptedIo` gains `pub fn last_completions(&self) -> CompletionSnapshot`.
4. `lib.rs` crate doc (`lib.rs:9-17`): add the new read to the honest-I/O paragraph — `refresh_completions` now also takes the project file list through `ChatHost::project_files` (a graph-state read the shell performs and bounds), keeping the rule: no terminal here; filesystem only through `AgentService` or behind the host seam as plain data.

**PATTERN** `crates/forge-chat/src/host.rs:25-28` (plain-data method style); `crates/forge-chat/src/app.rs:328-337` (the refresh-doc's way of naming costs); `crates/forge-chat/src/testing.rs:168-186` (`eof_reads`/`output` — the Shared-field-plus-getter shape `last_completions` copies).

**GOTCHA** `CompletionSnapshot` struct literals exist at `command.rs:364` (updated in Task 1), `app.rs:359` (this task), and `controller.rs:1021` (uses `..CompletionSnapshot::default()` — compiles untouched). `controller.rs:191-193` (`set_completions`) and `app.rs:146-147` need no change — the controller carries the snapshot opaquely and `Command::parse` never reads `paths`.

**Steps:**

- [ ] **Step 1: Write the failing test** in `app.rs`'s test module:

```rust
    /// The host's file list reaches the editor's prompt: after one
    /// submitted line (which refreshes the snapshot), the next read is
    /// handed completions that carry the paths.
    #[tokio::test]
    async fn the_hosts_project_files_reach_the_prompt_snapshot() {
        let (host, _tmp) = FakeHost::with_script(r#"[{"text": "ok"}]"#)
            .with_paths(vec!["src/main.rs".to_string()]);
        let mut io = ScriptedIo::new(["anything", "/quit"]);
        run(io.handle(), host, Start::fresh()).await.expect("chat runs");
        assert_eq!(
            io.last_completions().paths,
            vec!["src/main.rs".to_string()],
            "the prompt the editor saw carries the host's paths"
        );
    }
```

- [ ] **Step 2: Run** `cargo test -p forge-chat` → FAIL (trait method missing → `FakeHost` no longer implements `ChatHost`).
- [ ] **Step 3: Implement** per IMPLEMENT above.
- [ ] **Step 4: Run** `cargo test -p forge-chat` → PASS.
- [ ] **Step 5:** `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings` → PASS. **Commit** — `git add -A && git commit -m "feat(chat): ChatHost::project_files seam feeding the completion snapshot"`

**VALIDATE** `cargo test -p forge-chat`

**SATISFIES** AC1 (wiring half); keeps the crate-doc honesty constraint.

---

### Task 3: `CliHost::project_files` — the bounded, mtime-gated graph reader

**ACTION** `crates/forge-cli/src/chat/host.rs`

**IMPLEMENT**

1. `CliHost` gains one field:

```rust
    /// `project_files`' cache: the loaded path list keyed by the graph
    /// file's mtime (`None` = the file was absent at load). `refresh_
    /// completions` calls this once per submitted line, so the steady
    /// state must be one `stat`, not a re-parse of a graph file that can
    /// run to megabytes on a large repo. Interior mutability because the
    /// seam is `&self`.
    paths_cache: std::sync::Mutex<Option<(Option<std::time::SystemTime>, Vec<String>)>>,
```

initialized `Mutex::new(None)` in `CliHost::new` (`host.rs:94-101`).

2. The trait impl delegates to a free function (the `context_lines` shape, `host.rs:213-248`):

```rust
    fn project_files(&self) -> Vec<String> {
        project_files_at(&self.root, MAX_PROJECT_PATHS, &self.paths_cache)
    }
```

3. The free function and its bound:

```rust
/// The most project paths ever loaded into a completion snapshot. Bounds
/// the per-`readline` snapshot clones on huge repos; truncation is in the
/// graph's sorted order, so it is deterministic, and a truncated repo
/// completes its alphabetically-first 50k files — a bound, not a stall.
const MAX_PROJECT_PATHS: usize = 50_000;

/// `project_files`' body, free-standing so tests need no `CliHost` (the
/// `context_lines` shape, this module). Re-reads the graph only when the
/// file's mtime changed (or it appeared); any read/parse failure —
/// including a corrupt `graph.json`, which `LocalGraph::open` turns into
/// an `Err` — degrades to *no candidates* with a debug log, because a
/// completion source must never break the prompt.
fn project_files_at(
    root: &Path,
    cap: usize,
    cache: &std::sync::Mutex<Option<(Option<std::time::SystemTime>, Vec<String>)>>,
) -> Vec<String> { /* stat graph.json; hit → clone cached; miss → open + files() + truncate + store */ }
```

Conversion note: `ProjectGraph::files()` returns `PathBuf`s built from the graph's already-`/`-normalized keys (`crates/forge-graph/src/graph.rs:624-626`, normalization at walk time `graph.rs:114`); map with `to_string_lossy().into_owned()` — do **not** re-join or re-separator them.

**PATTERN** `crates/forge-cli/src/chat/host.rs:225-248` (free function + thin trait method), `350-385` (real tempdir graph tests), `crates/forge-graph/src/graph.rs:64-81` (`open`'s three cases: missing → empty, version mismatch → empty, parse error → `Err` — the last is the one this function must catch).

**GOTCHA** Two. (a) `LocalGraph::open` returns `Err` on an unparseable `graph.json` (`graph.rs:69-70`) — an `.ok()?`-style early return must *not* poison the cache in a way that retries a multi-MB parse on every submitted line; on parse error, cache `(mtime, vec![])` so the failure is paid once per file version. (b) Do not hold the `Mutex` guard across the `LocalGraph::open` call — take it, check, drop it, do the I/O, re-take to store. A completion refresh must never block a concurrent `graph_context` caller behind a file parse. (If clippy's `significant_drop_tightening` or a borrow fight makes the two-phase lock awkward, an `RwLock` is overkill — keep `Mutex` and structure the code as check → drop → load → store.)

**Steps:**

- [ ] **Step 1: Write the failing tests** in this module's test module, next to the existing real-graph tests (`host.rs:350-385`):

```rust
    fn fresh_cache() -> std::sync::Mutex<Option<(Option<std::time::SystemTime>, Vec<String>)>>
    { std::sync::Mutex::new(None) }

    /// A built graph's files complete: sorted, project-relative, and
    /// excluding what the graph's policy excludes (`.forge/` itself).
    #[test]
    fn project_files_lists_the_graphs_files() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(tmp.path().join("src")).expect("mkdir");
        std::fs::write(tmp.path().join("src/main.rs"), "fn main() {}\n").expect("write");
        std::fs::write(tmp.path().join("guide.md"), "# hi\n").expect("write");
        let mut graph = forge_graph::LocalGraph::open(tmp.path()).expect("open");
        graph.build().expect("build");

        assert_eq!(
            project_files_at(tmp.path(), MAX_PROJECT_PATHS, &fresh_cache()),
            vec!["guide.md".to_string(), "src/main.rs".to_string()],
        );
    }

    /// Review Focus 4: no graph, or a corrupt one, is silence — not an
    /// error, not a panic, and (for the corrupt case) not a re-parse on
    /// every call.
    #[test]
    fn project_files_degrades_to_empty_without_a_built_graph() {
        let tmp = tempfile::tempdir().expect("tempdir");
        assert!(project_files_at(tmp.path(), MAX_PROJECT_PATHS, &fresh_cache()).is_empty());
        std::fs::create_dir_all(tmp.path().join(".forge/graph")).expect("mkdir");
        std::fs::write(tmp.path().join(".forge/graph/graph.json"), "not json").expect("write");
        let cache = fresh_cache();
        assert!(project_files_at(tmp.path(), MAX_PROJECT_PATHS, &cache).is_empty());
        assert!(cache.lock().expect("cache").is_some(), "the failure is cached, not re-paid");
    }

    /// Review Focus 3 (shell half): the cap is honored, and a rebuild is
    /// picked up through the mtime gate — a file added after a rebuild
    /// appears without restarting the chat.
    #[test]
    fn project_files_is_capped_and_reloads_on_rebuild() {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join("a.rs"), "fn a() {}\n").expect("write");
        let mut graph = forge_graph::LocalGraph::open(tmp.path()).expect("open");
        graph.build().expect("build");
        let cache = fresh_cache();
        assert_eq!(project_files_at(tmp.path(), 1, &cache), vec!["a.rs".to_string()]);

        std::fs::write(tmp.path().join("b.rs"), "fn b() {}\n").expect("write");
        let mut graph = forge_graph::LocalGraph::open(tmp.path()).expect("reopen");
        graph.build().expect("rebuild bumps the graph file's mtime");
        assert_eq!(
            project_files_at(tmp.path(), 50, &cache),
            vec!["a.rs".to_string(), "b.rs".to_string()],
            "the mtime gate reloaded"
        );
    }
```

(The rebuild test may need a one-second sleep if the filesystem's mtime granularity hides the rewrite — coarse mtimes are a real platform property. Prefer asserting on behavior first; add `std::thread::sleep(Duration::from_millis(1100))` before the rebuild only if it flakes, with a comment saying why.)

- [ ] **Step 2: Run** `cargo test -p forge-cli chat::host` → FAIL.
- [ ] **Step 3: Implement** per IMPLEMENT above.
- [ ] **Step 4: Run** `cargo test -p forge-cli` → PASS (this module's existing tests — mock filtering, `context_lines`, the `CliHost` construction trio — must pass unedited).
- [ ] **Step 5:** `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings` → PASS. **Commit** — `git add -A && git commit -m "feat(cli): project file completion source over the graph, mtime-gated"`

**VALIDATE** `cargo test -p forge-cli`

**SATISFIES** AC1 (data source), AC2 (bounded), Review Focus 3/4.

---

### Task 4: end-to-end proof on a real pty — Tab on `@` in the compiled binary

**ACTION** `crates/forge-cli/tests/chat.rs`

**IMPLEMENT** One new `#[cfg(unix)]` test at the end of the file, beside `a_typed_ctrl_c_on_a_real_terminal_interrupts_without_killing_the_chat` (`tests/chat.rs:816-880`), reusing `pty::spawn` (`723`), `tail_stream` (`291`), `wait_for`, and `scaffold` (`182-207` — which already writes `alpha.rs`). The test's own doc comment must say why this needs a pty (completion only exists on a TTY; §12.3 gives piped mode none) and why no new dependency was added (the `libc` pty harness exists for the Ctrl-C test).

Shape:

1. `scaffold(tmp.path(), "auto")`, then build the graph through the real binary: `forge(tmp.path(), &project).args(["graph", "build"]).output()` — assert success. (The graph must be built *before* the chat starts; `refresh_completions` at startup, `app.rs:159`, is what loads it.)
2. `pty::spawn(forge(tmp.path(), &project))`; `tail_stream(master)`; clone the master for writing.
3. `wait_for(&output, "/help for commands", 30s)`, then `wait_for(&output, "> ", 30s)` — the two-stage wait, for the documented reason (`tests/chat.rs:826-841`: only the prompt proves `readline()` is blocked in raw mode).
4. `writer.write_all(b"@al\t")` — with exactly one matching file (`alpha.rs`), `CompletionType::List` completes immediately.
5. `wait_for(&output, "@alpha.rs", 30s)` — the completed text appears in rustyline's redraw of the input line. Assert on the completed path, not on redraw escapes (the existing test's reasoning at `tests/chat.rs:868-872` applies: never substring-match across ANSI redraw sequences for the *prompt text*; `@alpha.rs` appears as a contiguous fragment because the completion is inserted in one edit).
6. `writer.write_all(b"\n")` to submit, then `wait_for(&output, "the answer", 30s)` — the completed line ran as a real turn against the scripted mock, proving the completed text is what the prompt held.
7. Kill the child rather than `/quit` (same documented reason as the existing pty test, `tests/chat.rs:810-815` — do not race the unrelated outstanding-read hazard), join the reader thread.

**PATTERN** `crates/forge-cli/tests/chat.rs:816-880` wholesale — structure, waits, and the discipline about what may be asserted on.

**GOTCHA** Three, all learned from the existing pty test's scars: (a) wait for the prompt, not the banner, before typing; (b) 30-second bounds, not 10 (loaded shared machines flaked the tighter bound, `tests/chat.rs:826-831`); (c) kill, don't `/quit`. A fourth, new one: the `@al\t` write must happen only *after* step 3's prompt wait — a Tab landing before `readline()` starts is just a byte in the tty buffer with no completer attached yet.

**Steps:**

- [ ] **Step 1: Write the test** per IMPLEMENT.
- [ ] **Step 2: Run** `cargo test -p forge-cli --test chat` → PASS on the first green run only if Tasks 1-3 are correct; if it fails, fix the implementation, not the test, unless the test contradicts this plan.
- [ ] **Step 3: Run** the whole file again: `cargo test -p forge-cli --test chat` → PASS (the pre-existing pty test must stay green — they share the harness).
- [ ] **Step 4:** `just verify` → PASS. **Commit** — `git add -A && git commit -m "test(cli): @-path completion end to end on a real pty"`

**VALIDATE** `cargo test -p forge-cli --test chat`

**SATISFIES** AC1 (the ticket's literal acceptance: "Tab on `@src/ma` completes to real project paths", against the compiled binary on a real terminal).

---

### Task 5: docs sweep — retire the recorded limitation

**ACTION** `docs/reference.md`, `docs/superpowers/specs/2026-09-24-interactive-chat-ui-design.md`

**IMPLEMENT**

1. `docs/reference.md:551-553` — extend the Tab sentence: `Tab` completes a slash command, a skill name, a `/attach` job id, **and any word starting with `@` against the project graph's file list** (prefix, case-sensitive; needs `forge graph build` to have been run; capped). One sentence, no table.
2. `docs/reference.md:1802-1813` — remove "and there is no path completion — `Tab` completes commands, skill names and `/attach` job ids, nothing filesystem-shaped" from the Known-limitations bullet; if any limitation genuinely remains (paths with whitespace are never offered; an unbuilt graph completes nothing), state it there in one clause — that is what the section is for.
3. Design doc: §9.2's "anywhere else: nothing. No file-path completion in v1…" bullet (`2026-09-24-interactive-chat-ui-design.md:709-711`) is behavior-describing and now false — correct it in place to describe the `@` rule in one line with a pointer to this plan, following the §16 amendment's own "corrected in place" precedent for §12.3; and in §14, mark the path-completion bullet (`1128-1129`) as shipped-by TICKET-4 with the date. Do not rewrite the sections' history beyond that.
4. `README.md` needs no change (it does not document completion — verified by grep at planning time); `ARCHITECTURE.md` needs no change (the crate map's `forge-chat` line covers completion under "slash parsing"; no new crate or seam *crate* appeared). If the implementer finds either statement false, correct it in this task and say so in the commit message.

**Steps:**

- [ ] **Step 1:** Make the edits. **Step 2:** `just verify` → PASS (docs are checked by nothing, but the workspace must stay green). **Step 3:** **Commit** — `git add -A && git commit -m "docs: @-path completion ships; retire the recorded limitation"`

**VALIDATE** `just verify`

**SATISFIES** the docs-describe-today constraint; closes the epic's bookkeeping for TICKET-4.

---

## TESTING STRATEGY

Layered exactly as the codebase already layers completion:

- **Pure completer logic, no TTY, no process** (`cargo test -p forge-chat`): the whole `@` rule — trigger positions (word 0 / mid-prompt / command argument), prefix matching, `@`-preserving replacements, display-is-the-value, the 100-candidate cap, whitespace exclusion, bare-`@` paging, char-boundary survival — as direct `Command::complete` calls, mirroring the existing table at `command.rs:480-613`. This is the bulk of the coverage and it can never need a terminal.
- **Seam wiring, in process** (`cargo test -p forge-chat`): `FakeHost::with_paths` → driver → the prompt `ScriptedIo` captured carries the paths (Task 2). Proves `refresh_completions` moves the data without a filesystem.
- **The shell's graph reader** (`cargo test -p forge-cli`): real `LocalGraph` builds in tempdirs (the module's existing pattern) — sorted relative paths, graph-policy exclusions, unbuilt → empty, corrupt → empty *and cached*, the cap, and reload-on-rebuild through the mtime gate (Task 3).
- **End to end on a real pty** (`cargo test -p forge-cli --test chat`, unix): `forge graph build` then Tab on `@al` in the compiled binary; the completed `@alpha.rs` appears and the line submits as a real turn (Task 4). This is the only layer that can prove rustyline actually calls the helper with the new snapshot.
- **Boundedness on huge repos** is tested structurally, not by building 50k files: the candidate cap with 150 paths (pure), the snapshot cap with `cap = 1` (parameterized), and the mtime gate (reload on change; cache-populated-on-error). The unbounded input — a giant graph file — is bounded by construction (one `stat` per refresh; parse only on change), which the tests pin.
- **No BDD scenario** (`cargo test -p forge-cli --test bdd` must stay green but gains nothing): the cucumber harness drives piped stdin, where §12.3 states no completion exists; a completion scenario there would test nothing. The pty test holds the end-to-end guarantee instead.
- **Regression blanket:** every pre-existing completion, controller, app, host, and chat test passes unedited — that suite *is* the "no behavior change for non-path tokens" acceptance.

## VALIDATION COMMANDS

Run all in the ticket worktree. Per-task gates are named in each task; the full gate before finishing:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p forge-chat -p forge-cli
cargo test --workspace
cargo test -p forge-cli --test bdd
```

(`just verify` covers `check` + `lint` + `lint-ffi` + `test` + `bdd` + `fmt --check` and must be green before each commit, per house rule.)

## ACCEPTANCE CRITERIA

- **AC1 — the ticket's headline:** on a TTY, Tab on `@src/ma` completes to real project paths from the built graph, at any word position; the replacement preserves the `@`. Proven pure (Task 1), wired (Task 2), sourced (Task 3), and end-to-end on a real pty against the compiled binary (Task 4).
- **AC2 — bounded on huge repos:** candidate cap 100 (pure), snapshot cap 50 000 (shell), steady-state refresh = one `stat` via the mtime gate; corrupt/unbuilt graph degrades to silence and never errors in the transcript.
- **AC3 — no behavior change for non-path tokens:** the entire pre-existing `Command::complete` test table passes unedited; bare path-like words still complete nothing; slash completion, history, validation, piped mode untouched (`terminal_io.rs`, `piped_io.rs`, `controller.rs` behaviorally unchanged — `terminal_io.rs` gains no diff at all).
- **AC4 — the seam law holds:** `forge-chat` still has no terminal and no new I/O; the file list arrives as plain data through `ChatHost`; `cargo test -p forge-chat` still cannot need a TTY.
- **AC5 — docs describe today:** `reference.md` no longer lists path completion as a limitation; the design doc's §9.2/§14 are corrected/amended in the same series.

## OPEN QUESTIONS / ASSUMPTIONS

Settled upstream (no longer open): the trigger (`@` — the ticket's own acceptance shape), the source (the graph — the ticket says so), the transport (`ChatHost`/`CompletionSnapshot` — the crate's law), piped mode (§12.3 forbids completion there). Two remain:

1. **When no graph is built, should the shell fall back to a bounded filesystem walk?** Recommended default: **no.** Two sources of "project files" would disagree (the walk would need its own exclusion policy beside the graph's `SKIP_DIRS`), which is itself a half-working state; `/graph` and the runtime's wiring both already treat an unbuilt graph as empty-not-error; and the remedy is one incremental command (`forge graph build`). If real usage shows the empty state confusing users, the fix is a hint line once per session (`  - no project graph yet; forge graph build enables @-completion`), not a second data source.
2. **Should directories be offered as candidates too?** Recommended default: **no.** rustyline's longest-common-prefix insertion (`CompletionType::List`) already drills down segment by segment — `@src/forge-ch` + Tab completes to `@src/forge-chat/` — so directory entries would only double the listing. Revisit only if the drill-down feel tests poorly with users.

Assumption recorded: the mtime gate relies on `forge graph build` rewriting `graph.json` (it does — `LocalGraph::save`, `crates/forge-graph/src/graph.rs:301-309`); coarse filesystem mtimes are handled by the test note in Task 3, and in production a same-second rebuild is picked up on the following refresh at worst.

## NOTES

- The whole feature rides the existing snapshot pipeline: `Prompt` is rebuilt per `read()` and the editor thread takes the newest (`terminal_io.rs:320-337`), so a refreshed path list reaches the completer on the very next prompt with no extra plumbing.
- A free consequence of the uniform rule: `/graph @src/ma` and `/session`-style command lines complete `@`-paths too. That is intended (the rule is positional-blind by design), not scope creep.
- TICKET-8 adjacency: `refresh_completions` already pays `list_runs()` per submitted line (the cost TICKET-8 exists to remove). This ticket deliberately adds *only* an mtime-gated read so it contributes nothing to that problem; do not "simplify" Task 3 by dropping the cache.
- This repo's own `.forge/graph/graph.json` measures ~0.9 MB at planning time — the scale at which an un-gated per-line reparse would already be noticeable, and the evidence for the mtime gate.

## AMENDMENTS

(none)
