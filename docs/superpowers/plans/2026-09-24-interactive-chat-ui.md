# Interactive Chat UI (Phase B) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `forge` with no subcommand opens an interactive conversation — a scrolling inline transcript, a rich input line with history and Tab-completed slash commands, discovered skills as `/name`, inline approvals, backgroundable turns, and `/fork` — over the same `AgentService` the CLI, server, MCP and ACP adapters already share. No new runtime capability: Phase A shipped replay, fork, `attach`/`list_runs` and `RunState`.

**Architecture:** A new **pure** crate `forge-chat` (no terminal, no `rustyline`, no I/O syscalls) holds slash parsing + completion (`command.rs`), the `Event → Vec<Line>` mapping (`render.rs`), the input/signal state machine (`controller.rs`), and the async driver (`app.rs`) over two seams: `ChatIo` (terminal) and `ChatHost` (everything config-shaped). `forge-cli` implements both — `TerminalIo` over `rustyline` on a dedicated editor thread (the `NeedleEngine` pattern), `CliHost` over the existing `build_run_service` path. This is `forge-acp`'s pure-`dispatch`/impure-`server` split, with the impure half moved out of the crate entirely, so `cargo test -p forge-chat` can never need a TTY. The turn driver copies `forge-acp::server::run_turn` (subscribe-before-start → `select!` → flush → settle), and approvals reuse MCP/ACP's parked-run round trip (`ApprovalRequested` → `send_input("y"/"n")`) rather than adding a second mechanism.

**Tech Stack:** Rust edition 2024, tokio (existing `signal`/`sync`/`time` features), async-trait, thiserror, serde_json, `rustyline` 18 (`default-features = false`, features `custom-bindings` + `with-file-history`; 8 new transitive packages, measured), cucumber BDD, tempfile.

**Spec:** `docs/superpowers/specs/2026-09-24-interactive-chat-ui-design.md`

## Global Constraints

- Typed errors via `thiserror`; **no `unwrap`/`expect` in production code** (tests may).
- Edition 2024: mutating the environment in a test needs `unsafe { std::env::set_var(...) }` and `#[serial]` (`serial_test`), as the existing env-sensitive suites do.
- Hermetic tests: temp `HOME`/`XDG_CONFIG_HOME`, the full `FORGE_*` scrub list, `FORGE_NEEDLE_AUTOFETCH=false`, `NO_COLOR=1`, no network. Process-level tests follow `crates/forge-cli/tests/acp.rs` exactly.
- Stdout discipline: human/transcript output on **stdout**, every diagnostic on **stderr**, even at `-vvv`. `--json` guarantees stdout is a single machine-readable JSON value, which a conversation cannot honour — so chat + `--json` is refused, never reinterpreted.
- Mocks are test-only behind the `FORGE_TEST_MOCKS=1` gate (`forge_config::ensure_test_mocks_allowed`), and **no user-facing chat surface may mention them** — not `/model`, not `/help`, not the banner, not an error message.
- Foreground `just verify` (fmt --check + check + clippy -D warnings + lint-ffi + test + bdd) green **before each commit**. If a task's commit step names a narrower command, `just verify` must still pass at task end.
- `README.md` and `ARCHITECTURE.md` are updated **in the same commit** as the behavior they describe; both describe today, never the roadmap.
- Workspace deps come from root `Cargo.toml` `[workspace.dependencies]`; new crates inherit `edition.workspace = true` / `version.workspace = true` / `license.workspace = true` like every existing crate.
- Config precedence is untouched: no new config keys in this plan. `/model` and `/approval` change the session, never a file.
- Nothing in the transcript may be width-sensitive: no boxes, no tables, no right alignment, no rule longer than 48 characters, ASCII glyphs only.

## Review Focus

The input classes most likely to bite a real user, each pinned to a test:

1. **`Ctrl-C` while a turn is running** must cancel the turn and **not** quit; `Ctrl-C` twice at an empty idle prompt must quit. Getting this backwards is the single most irritating REPL bug. → pure-state-machine table in Task 5, real `SIGINT`-to-the-child test in Task 10.
2. **`Ctrl-C` while an approval is pending** must cancel the turn *without running the operation* and without leaving an unconsumed `"n"` in the input channel for a later approval to swallow. → Task 5 (state machine) + Task 7 (driver, asserting the file was not written).
3. **A risky tool call on a real TTY** must park and be answered through the run's input channel, never by `NativeExecution` reading stdin behind the line editor's back (two readers on one fd, one in raw mode). → Task 6.
4. **A pasted multi-line code block** must be one turn, not five. → validator tests in Task 8; fenced-input parse test in Task 4.
5. **`write_file` with real content**, whose `args_summary` is truncated mid-JSON-string, must still render a readable `* write_file notes.txt`. → Task 3 (shared scan) + Task 4.
6. **Typing during a turn** must not be discarded; a completed line typed mid-turn becomes the next turn. → Task 8 (documented + `TCSADRAIN` assertion note) and Task 10 (piped-mode ordering test).
7. **A turn that fails** (unreachable model endpoint, denied approval, unreadable session log) must print one error line and return to the prompt, never end the chat. → Task 7 and Task 10.
8. **`forge` in a non-TTY** (`printf '…' | forge`, `forge < script.txt`) must behave like `forge run`'s piped stdin: a line while a run is parked is the approval answer, otherwise it is a prompt; EOF exits 0. → Task 9 and Task 10.

---

### Task 1: `forge` with no subcommand, and the `chat` command shell

The smallest independently shippable slice: `forge` stops erroring with no arguments, `forge chat` exists, and both reach a stub that prints the banner and exits. Everything else hangs off this.

**Files:**
- Modify: `crates/forge-cli/src/cli.rs` (`command: Option<Command>`, `Command::Chat`)
- Modify: `crates/forge-cli/src/commands/mod.rs` (`dispatch` handles `None`, adds `Chat`)
- Create: `crates/forge-cli/src/commands/chat_cmd.rs`
- Modify: `crates/forge-cli/tests/cli.rs` (two new tests)

**Interfaces:**
- Consumes: `crate::commands::Context`, `forge_config::ResolvedConfig`.
- Produces: `Command::Chat { prompt: Vec<String>, continue_session: bool, session: Option<String> }`, `chat_cmd::run(&Context, ChatArgs) -> Result<(), ForgeError>` — consumed by Tasks 8, 9.

- [ ] **Step 1: Write the failing tests** in `crates/forge-cli/tests/cli.rs`, in the style of the existing tests there (hermetic `forge()` helper, temp HOME/XDG):

```rust
#[test]
fn bare_forge_opens_the_chat_and_exits_at_eof() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path());
    // stdin is an empty pipe: the chat starts, reads EOF, exits cleanly.
    let out = forge(tmp.path(), &project)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            drop(child.stdin.take());
            child.wait_with_output()
        })
        .expect("bare forge runs");
    assert!(out.status.success(), "bare forge should exit 0, got {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("forge "), "banner missing: {stdout}");
    assert!(stdout.contains("/help"), "banner should point at /help: {stdout}");
    assert!(!stdout.to_lowercase().contains("mock"), "no mock may be named: {stdout}");
}

#[test]
fn chat_refuses_json_output() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path());
    let out = forge(tmp.path(), &project)
        .args(["--json", "chat"])
        .output()
        .expect("runs");
    assert!(!out.status.success(), "--json chat must fail");
    assert!(out.stdout.is_empty(), "nothing may reach stdout: {:?}", out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--json is not supported by the interactive chat"), "{stderr}");
}
```

Reuse the file's existing `forge()` and project-scaffolding helpers; if `scaffold` does not exist under that name, use whatever the file already uses to make a project with `model = "scripted-mock"`.

- [ ] **Step 2: Run** `cargo test -p forge-cli --test cli` → FAIL (`forge` with no subcommand is a clap error today).
- [ ] **Step 3: Implement the CLI change.** In `cli.rs`:

```rust
pub struct Cli {
    #[command(flatten)]
    pub global: GlobalOpts,

    /// No subcommand opens the interactive chat (see `Command::Chat`).
    /// This is `Option` for exactly that reason: `forge` alone must not be
    /// a usage error.
    #[command(subcommand)]
    pub command: Option<Command>,
}
```

and add to `Command`:

```rust
    /// Interactive chat: a scrolling transcript with slash commands.
    /// `forge` with no subcommand is the same thing.
    Chat {
        /// Optional first turn; the chat stays interactive afterwards.
        #[arg(value_name = "PROMPT")]
        prompt: Vec<String>,
        /// Continue the project's most recently active session.
        #[arg(short = 'c', long = "continue")]
        continue_session: bool,
        /// Continue a named session.
        #[arg(long, value_name = "SESSION_ID")]
        session: Option<String>,
    },
```

In `commands/mod.rs::dispatch`, match `cli.command`:

```rust
    match cli.command {
        // No subcommand: the interactive chat. Deliberately the same code
        // path as `forge chat`, so the two can never drift.
        None => chat_cmd::run(&ctx, chat_cmd::ChatArgs::default()).await,
        Some(Command::Chat { prompt, continue_session, session }) => {
            chat_cmd::run(&ctx, chat_cmd::ChatArgs { prompt, continue_session, session }).await
        }
        Some(Command::Init) => init::run(&ctx),
        // ... every existing arm, now wrapped in Some(...)
    }
```

- [ ] **Step 4: Implement the stub** `chat_cmd.rs`: reject `--json` first (typed error with the exact message from the test), resolve the config, print the §7 banner to stdout, and return `Ok(())`. Keep it a stub — Task 9 replaces the body with the real loop.
- [ ] **Step 5: Run** `cargo test -p forge-cli --test cli` → PASS.
- [ ] **Step 6:** `just verify` → PASS. **Commit** — `git add -A && git commit -m "feat(cli): forge with no subcommand opens the chat"`

---

### Task 2: `forge-chat` crate skeleton, `Line`/`Style`, and the two seams

**Files:**
- Create: `crates/forge-chat/Cargo.toml`, `crates/forge-chat/src/lib.rs`, `crates/forge-chat/src/io.rs`, `crates/forge-chat/src/host.rs`
- Modify: root `Cargo.toml` (workspace member + `forge-chat = { path = "crates/forge-chat" }`)
- Modify: `ARCHITECTURE.md` (crate map line)

**Interfaces:**
- Consumes: `forge_core::ForgeError`, `forge_runtime::AgentService`.
- Produces (used by Tasks 3–10): `Line`, `Style`, `Prompt`, `ReadOutcome`, `ChatIo` (incl. `interactivity()`), `ChatHost`, `HostChange`, `Environment`, `NeedleState`, `ModelChoice`, `SkillChoice`, `ConfigLine`, `ContextLine`. `Interactivity` is declared here and re-exported by `controller.rs`'s module (Task 5), so there is one definition.

- [ ] **Step 1: Crate scaffolding.** `crates/forge-chat/Cargo.toml`:

```toml
[package]
name = "forge-chat"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
async-trait = { workspace = true }
forge-core = { workspace = true }
forge-runtime = { workspace = true }
forge-session = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
tokio = { workspace = true }
tracing = { workspace = true }

[dev-dependencies]
forge-config = { workspace = true }
forge-execution = { workspace = true }
forge-providers = { workspace = true }
tempfile = { workspace = true }
```

Add `"crates/forge-chat"` to `members` and `forge-chat = { path = "crates/forge-chat" }` to `[workspace.dependencies]`.

- [ ] **Step 2: Write the failing test** in `crates/forge-chat/src/io.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// A `Line` is semantic, not styled: the writer decides colour, so a
    /// renderer test asserts on text alone and a palette change cannot
    /// break one.
    #[test]
    fn a_line_carries_text_and_a_semantic_style() {
        let line = Line::meta("routing: needle -> qwen3-coder (conf 0.91)");
        assert_eq!(line.style, Style::Meta);
        assert_eq!(line.text, "  - routing: needle -> qwen3-coder (conf 0.91)");
        assert!(line.text.is_ascii(), "the transcript is ASCII only");
    }

    #[test]
    fn every_constructor_uses_its_documented_gutter() {
        assert!(Line::tool("read_file src/main.rs").text.starts_with("  * "));
        assert!(Line::ok("ok (12 ms)").text.starts_with("    -> "));
        assert!(Line::bad("error: boom").text.starts_with("  ! "));
        assert!(Line::warn("approval needed: x (risky)").text.starts_with("  ! "));
        assert!(Line::footer("2 turns, 3 tool calls, 4.2s").text.starts_with("  = "));
        assert!(Line::notice("job 01J finished").text.starts_with("  # "));
        // Assistant text is the one class with no gutter: it is the answer.
        assert_eq!(Line::plain("the parser is recursive-descent").text,
                   "the parser is recursive-descent");
    }
}
```

- [ ] **Step 3: Run** `cargo test -p forge-chat` → FAIL (nothing exists).
- [ ] **Step 4: Implement `io.rs`** — `Style` (`Plain`, `Meta`, `Tool`, `Ok`, `Warn`, `Bad`, `Notice`), `Line` with the constructors above (each prepending its documented gutter exactly once), `Prompt { text, completions, history }`, `ReadOutcome { Line(String), Interrupt, Eof, Failed(String) }`, and the `ChatIo` trait verbatim from the spec's §3.1 including the doc comments (the `interrupted()` rationale is the load-bearing one).
- [ ] **Step 5: Implement `host.rs`** — the `ChatHost` trait and its plain-data types from §3.1. `Environment`, `ModelChoice { name, description, active }`, `SkillChoice { name, description }`, `ConfigLine { key, value, origin }`, `ContextLine { path, score }`, `NeedleState { Active { model_id }, Inactive { reason } }`. Every method returns owned data — that is what lets a `FakeHost` drive the loop.
- [ ] **Step 6: Run** `cargo test -p forge-chat` → PASS. Add the `forge-chat` line to `ARCHITECTURE.md`'s crate map:

```text
forge-chat        interactive chat: pure slash parsing, event->transcript
                  rendering and the input state machine, over ChatIo /
                  ChatHost seams the CLI implements (no terminal here)
```

- [ ] **Step 7:** `just verify` → PASS. **Commit** — `git add -A && git commit -m "feat(chat): forge-chat crate with the ChatIo and ChatHost seams"`

---

### Task 3: `forge_core::tool_arg_field` — one truncated-args scanner

`forge-acp` already has the escape-aware, truncation-tolerant scan for reading a field out of a 120-character `args_summary`. The chat needs it. Two copies would diverge, so it moves down and ACP delegates.

**Files:**
- Modify: `crates/forge-core/src/events.rs` (or a new `src/tool_args.rs` + `pub mod`; follow whichever the crate's existing layout makes tidier)
- Modify: `crates/forge-acp/src/dispatch.rs` (`Args::field` delegates)
- Modify: `crates/forge-acp/src/dispatch/tests.rs` (unchanged assertions must still pass)

**Interfaces:**
- Produces: `forge_core::tool_arg_field(args_summary: &str, key: &str) -> Option<String>` — consumed by Task 4 and by `forge-acp`.

- [ ] **Step 1: Write the failing test** next to the new function:

```rust
#[cfg(test)]
mod tool_arg_tests {
    use super::tool_arg_field;

    #[test]
    fn reads_a_field_from_valid_json() {
        let summary = r#"{"path":"src/main.rs","content":"fn main() {}"}"#;
        assert_eq!(tool_arg_field(summary, "path").as_deref(), Some("src/main.rs"));
        assert_eq!(tool_arg_field(summary, "missing"), None);
    }

    /// The whole reason this is not `serde_json::from_str`: the most
    /// interesting call is the one whose summary was cut mid-string.
    #[test]
    fn reads_a_field_from_json_truncated_after_it() {
        let summary = r#"{"path":"notes.txt","content":"a very long body that got cu"#;
        assert_eq!(tool_arg_field(summary, "path").as_deref(), Some("notes.txt"));
    }

    #[test]
    fn a_value_cut_before_its_closing_quote_is_none_not_a_lie() {
        let summary = r#"{"path":"src/very/long/pa"#;
        // Half a path points at a file that does not exist; no answer is
        // better than a wrong one.
        assert_eq!(tool_arg_field(summary, "path"), None);
    }

    #[test]
    fn honours_backslash_escapes_inside_the_value() {
        let summary = r#"{"path":"a\"b/c.rs","content":"x"}"#;
        assert_eq!(tool_arg_field(summary, "path").as_deref(), Some("a\"b/c.rs"));
    }

    #[test]
    fn an_empty_value_is_none() {
        assert_eq!(tool_arg_field(r#"{"path":""}"#, "path"), None);
    }
}
```

- [ ] **Step 2: Run** `cargo test -p forge-core tool_arg` → FAIL.
- [ ] **Step 3: Implement** by moving the body of `forge-acp::dispatch::Args::field` down: try `serde_json::from_str::<Value>` filtered to an object first, then fall back to the escape-aware textual scan for `"{key}":"`. Document at the definition *why* it is not just a JSON parse (truncation), because that is the fact a future reader will otherwise "simplify" away.
- [ ] **Step 4: Make `forge-acp` delegate** — `Args::field` becomes: parsed JSON if present (keeping its existing `Value`-based path for `raw_input`), else `forge_core::tool_arg_field(self.raw, key)`. Run `cargo test -p forge-acp` → PASS with **no test edits**; if an ACP test needs changing, the move changed behaviour and must be corrected instead.
- [ ] **Step 5:** `just verify` → PASS. **Commit** — `git add -A && git commit -m "refactor(core): share the truncated tool-args field scan with forge-acp"`

---

### Task 4: `render.rs` — events to transcript, pure

**Files:**
- Create: `crates/forge-chat/src/render.rs` (+ `pub mod render;` and re-exports in `lib.rs`)

**Interfaces:**
- Consumes: `forge_core::{Event, EventKind, RiskLevel}`, `forge_core::tool_arg_field` (Task 3), `Line`/`Style` (Task 2).
- Produces: `TranscriptState::{new, on_event, rendered_assistant_text, footer}`, `pub fn summarize_call(tool, args_summary) -> String` — consumed by Tasks 7, 9.

- [ ] **Step 1: Write the failing tests** in `render.rs`. These are the spec's §4.2 table, one assertion per row; the helpers keep them short:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use forge_core::{Event, EventKind, RiskLevel, ToolCall};

    fn ev(kind: EventKind) -> Event {
        Event::new("run-1", "sess-1", kind)
    }

    fn texts(state: &mut TranscriptState, kind: EventKind) -> Vec<String> {
        state.on_event(&ev(kind)).into_iter().map(|l| l.text).collect()
    }

    #[test]
    fn routing_reads_as_a_routing_line() {
        let mut s = TranscriptState::new();
        let out = texts(&mut s, EventKind::RoutingDecisionMade {
            router: "needle".into(),
            selected_model: "qwen3-coder".into(),
            confidence: 0.913,
            fallback_used: false,
            reason: String::new(),
        });
        assert_eq!(out, vec!["  - routing: needle -> qwen3-coder (conf 0.91)"]);
    }

    #[test]
    fn a_fallback_is_marked_and_a_reason_is_kept() {
        let mut s = TranscriptState::new();
        let out = texts(&mut s, EventKind::RoutingDecisionMade {
            router: "static".into(),
            selected_model: "qwen3-coder".into(),
            confidence: 0.5,
            fallback_used: true,
            reason: "needle declined".into(),
        });
        assert_eq!(out, vec![
            "  - routing: static -> qwen3-coder (conf 0.50) [fallback] - needle declined"
        ]);
    }

    /// The product's headline claim must read as itself, not as routing.
    #[test]
    fn the_needle_fast_path_reads_as_an_on_device_call() {
        let mut s = TranscriptState::new();
        let out = texts(&mut s, EventKind::RoutingDecisionMade {
            router: "needle-dispatch".into(),
            selected_model: "none".into(),
            confidence: 0.94,
            fallback_used: false,
            reason: "needle filled and dispatched `read_file` on device; no model call".into(),
        });
        assert_eq!(out, vec![
            "  - on-device: needle called read_file directly (conf 0.94, no model call)"
        ]);
    }

    #[test]
    fn a_tool_call_renders_its_friendly_arguments() {
        let mut s = TranscriptState::new();
        let out = texts(&mut s, EventKind::ToolCallRequested {
            tool: "read_file".into(),
            args_summary: r#"{"path":"src/main.rs"}"#.into(),
        });
        assert_eq!(out, vec!["  * read_file src/main.rs"]);
    }

    /// Review Focus 5: the summary is cut mid-content, and the path still
    /// has to appear.
    #[test]
    fn a_truncated_write_still_names_the_file() {
        let mut s = TranscriptState::new();
        let out = texts(&mut s, EventKind::ToolCallRequested {
            tool: "write_file".into(),
            args_summary: r#"{"path":"notes.txt","content":"a very long body that got c"#.into(),
        });
        assert_eq!(out, vec!["  * write_file notes.txt"]);
    }

    #[test]
    fn an_unreadable_summary_is_ellipsized_not_dropped() {
        let mut s = TranscriptState::new();
        let long = "x".repeat(120);
        let out = texts(&mut s, EventKind::ToolCallRequested {
            tool: "mystery_tool".into(),
            args_summary: long,
        });
        assert_eq!(out.len(), 1);
        assert!(out[0].starts_with("  * mystery_tool xxx"), "{:?}", out[0]);
        assert!(out[0].ends_with("..."), "{:?}", out[0]);
        assert!(out[0].len() <= 4 + "mystery_tool".len() + 1 + 60, "{:?}", out[0]);
    }

    #[test]
    fn tool_started_is_silent_and_completion_reports_elapsed_time() {
        let mut s = TranscriptState::new();
        assert!(texts(&mut s, EventKind::ToolStarted { name: "read_file".into() }).is_empty());
        let out = texts(&mut s, EventKind::ToolCompleted { name: "read_file".into(), success: true });
        assert_eq!(out.len(), 1);
        assert!(out[0].starts_with("    -> ok ("), "{:?}", out[0]);
        assert!(out[0].ends_with(" ms)"), "{:?}", out[0]);
    }

    #[test]
    fn a_failed_tool_says_so() {
        let mut s = TranscriptState::new();
        let out = texts(&mut s, EventKind::ToolCompleted { name: "run_command".into(), success: false });
        assert!(out[0].starts_with("    -> failed ("), "{:?}", out[0]);
    }

    #[test]
    fn approval_and_its_verdict_are_both_visible() {
        let mut s = TranscriptState::new();
        let asked = texts(&mut s, EventKind::ApprovalRequested {
            command: "write notes.txt".into(),
            risk: RiskLevel::Risky,
        });
        assert_eq!(asked, vec![
            "  ! approval needed: write notes.txt (risky) \
             - y to approve, anything else denies"
        ]);
        let decided = texts(&mut s, EventKind::ApprovalDecided {
            command: "write notes.txt".into(),
            approved: false,
        });
        assert_eq!(decided, vec!["    -> denied"]);
    }

    #[test]
    fn assistant_text_is_a_bare_block_and_is_recorded_as_rendered() {
        let mut s = TranscriptState::new();
        assert!(!s.rendered_assistant_text());
        let out = texts(&mut s, EventKind::AssistantMessage {
            text: "the parser is recursive-descent".into(),
            tool_calls: Vec::new(),
        });
        assert_eq!(out.iter().map(String::as_str).collect::<Vec<_>>(),
                   vec!["", "the parser is recursive-descent", ""]);
        assert!(s.rendered_assistant_text(), "the answer-once rule depends on this");
    }

    /// The fast path's assistant record has no text, so the run outcome is
    /// what the driver prints (see `rendered_assistant_text`).
    #[test]
    fn an_empty_assistant_message_renders_nothing_and_counts_as_nothing() {
        let mut s = TranscriptState::new();
        let out = texts(&mut s, EventKind::AssistantMessage {
            text: String::new(),
            tool_calls: vec![ToolCall::new("c1", "read_file", serde_json::json!({"path": "x"}))],
        });
        assert!(out.is_empty());
        assert!(!s.rendered_assistant_text());
    }

    #[test]
    fn replay_and_bookkeeping_kinds_render_nothing() {
        let mut s = TranscriptState::new();
        for kind in [
            EventKind::RunStarted { provider: "p".into(), model: "m".into(), prompt: "x".into() },
            EventKind::ToolResult { call_id: "c1".into(), tool: "read_file".into(),
                                    output: "a".repeat(4096), is_error: false },
            EventKind::TurnCompleted { turn: 1 },
            EventKind::InputReceived { message: "y".into() },
            EventKind::Completed { summary: "truncated to eighty chars".into() },
        ] {
            assert!(texts(&mut s, kind).is_empty(), "this kind must stay silent");
        }
    }

    #[test]
    fn errors_and_cancellation_are_loud() {
        let mut s = TranscriptState::new();
        assert_eq!(texts(&mut s, EventKind::Error { message: "model endpoint returned 401".into() }),
                   vec!["  ! error: model endpoint returned 401"]);
        assert_eq!(texts(&mut s, EventKind::Cancelled { reason: "cancelled by user".into() }),
                   vec!["  ! cancelled"]);
    }

    #[test]
    fn no_rendered_line_is_ever_non_ascii() {
        let mut s = TranscriptState::new();
        for kind in [
            EventKind::SkillActivated { name: "tdd".into(), path: "/p/SKILL.md".into() },
            EventKind::FileChanged { path: "src/main.rs".into() },
            EventKind::SessionForked { from_session: "sess-0".into(), at_position: 18 },
        ] {
            for line in s.on_event(&ev(kind)) {
                assert!(line.text.is_ascii(), "non-ascii in {:?}", line.text);
            }
        }
    }
}
```

- [ ] **Step 2: Run** `cargo test -p forge-chat render` → FAIL.
- [ ] **Step 3: Implement `render.rs`** — the §4.2 table with a `match` over `EventKind` that is **exhaustive with no wildcard arm**, so a future event kind cannot be silently dropped. `TranscriptState` holds: the elapsed-time anchor set by `ToolStarted` (falling back to `ToolCallRequested`, so a missing `ToolStarted` still reports a plausible duration rather than panicking), a `rendered_assistant_text: bool`, and the tool-call count/turn count for `footer()`. `summarize_call` prefers `tool_arg_field` for the field each tool is about (`path` for the file tools, `command` for `run_command`, `query`/`pattern` for the graph tools) and otherwise ellipsizes the raw summary to 60 characters with a trailing `...`. Adapt the exact `EventKind` variant/field spellings to `forge-core`'s real definitions — read `crates/forge-core/src/events.rs` first; the table's *content* is fixed, its Rust spelling follows the crate.
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5:** `just verify` → PASS. **Commit** — `git add -A && git commit -m "feat(chat): pure event-to-transcript rendering"`

---

### Task 5: `command.rs` + `controller.rs` — parsing, completion, and the Ctrl-C table

**Files:**
- Create: `crates/forge-chat/src/command.rs`, `crates/forge-chat/src/controller.rs`

**Interfaces:**
- Consumes: `Line` (Task 2), `HostChange` (Task 2).
- Produces: `Parsed`, `Command::{parse, complete}`, `COMMANDS`, `CompletionSnapshot`, `Action`, `Signal`, `ChatState`, `Interactivity::{Interactive, Batch}`, and `Controller::{new, state, on_line, on_signal, on_approval_requested, on_run_settled, on_drained, take_queued, arm_window}` — consumed by Tasks 7, 9.

- [ ] **Step 1: Write the failing parse/completion tests** in `command.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> CompletionSnapshot {
        CompletionSnapshot {
            skills: vec!["tdd".into(), "code-reviewer".into()],
            models: vec!["qwen3-coder".into(), "deepseek-chat".into()],
            jobs: vec!["01JCF4ABC".into()],
            sessions: vec!["01JCF3XYZ".into()],
        }
    }

    #[test]
    fn a_plain_line_is_a_prompt() {
        assert_eq!(Command::parse("explain the parser", &snapshot()),
                   Parsed::Prompt("explain the parser".into()));
    }

    /// A path is not a command. This is why the first token is matched
    /// against known names instead of "starts with a slash".
    #[test]
    fn a_line_starting_with_a_path_is_a_prompt() {
        assert_eq!(Command::parse("/usr/bin/env is on PATH?", &snapshot()),
                   Parsed::Prompt("/usr/bin/env is on PATH?".into()));
    }

    #[test]
    fn an_unknown_slash_word_is_an_error_not_a_prompt() {
        assert_eq!(Command::parse("/wat", &snapshot()), Parsed::Unknown("wat".into()));
    }

    #[test]
    fn commands_parse_with_and_without_arguments() {
        let s = snapshot();
        assert_eq!(Command::parse("/help", &s), Parsed::Help);
        assert_eq!(Command::parse("  /quit  ", &s), Parsed::Quit);
        assert_eq!(Command::parse("/exit", &s), Parsed::Quit);
        assert_eq!(Command::parse("/model", &s), Parsed::Model(None));
        assert_eq!(Command::parse("/model deepseek-chat", &s),
                   Parsed::Model(Some("deepseek-chat".into())));
        assert_eq!(Command::parse("/approval deny", &s), Parsed::Approval(Some("deny".into())));
        assert_eq!(Command::parse("/config model", &s), Parsed::Config(Some("model".into())));
        assert_eq!(Command::parse("/graph auth flow", &s), Parsed::Graph("auth flow".into()));
        assert_eq!(Command::parse("/session new", &s), Parsed::SessionNew);
        assert_eq!(Command::parse("/session 01JCF3XYZ", &s),
                   Parsed::SessionSwitch("01JCF3XYZ".into()));
        assert_eq!(Command::parse("/fork", &s), Parsed::Fork(None));
        assert_eq!(Command::parse("/fork --at 18", &s), Parsed::Fork(Some("18".into())));
        assert_eq!(Command::parse("/bg", &s), Parsed::Background);
        assert_eq!(Command::parse("/jobs", &s), Parsed::Jobs);
        assert_eq!(Command::parse("/attach 01JCF4ABC", &s),
                   Parsed::Attach("01JCF4ABC".into()));
    }

    /// The prompt template matters: `SkillRegistry::match_task` matches
    /// whitespace-separated words of >=3 chars against the skill name, so
    /// the bare name must appear as its own word.
    #[test]
    fn a_skill_becomes_a_prompt_that_activates_it() {
        let s = snapshot();
        assert_eq!(Command::parse("/tdd write the failing test", &s),
                   Parsed::Prompt("Use the tdd skill.\n\nwrite the failing test".into()));
        assert_eq!(Command::parse("/code-reviewer", &s),
                   Parsed::Prompt("Use the code-reviewer skill.".into()));
    }

    #[test]
    fn a_multiline_prompt_survives_parsing_intact() {
        let s = snapshot();
        let input = "fix this:\n```rust\nfn main() {}\n```";
        assert_eq!(Command::parse(input, &s), Parsed::Prompt(input.into()));
    }

    #[test]
    fn completion_offers_commands_and_skills_by_prefix() {
        let s = snapshot();
        let (start, items) = Command::complete("/s", 2, &s);
        assert_eq!(start, 0);
        assert!(items.contains(&"/session".to_string()));
        assert!(items.contains(&"/skills".to_string()));
        assert!(!items.contains(&"/help".to_string()));
        let (_, skills) = Command::complete("/t", 2, &s);
        assert!(skills.contains(&"/tdd".to_string()), "skills complete too: {skills:?}");
    }

    #[test]
    fn completion_offers_arguments_per_command() {
        let s = snapshot();
        assert_eq!(Command::complete("/model ", 7, &s).1,
                   vec!["qwen3-coder".to_string(), "deepseek-chat".to_string()]);
        assert_eq!(Command::complete("/attach ", 8, &s).1, vec!["01JCF4ABC".to_string()]);
        assert!(Command::complete("/approval ", 10, &s).1.contains(&"prompt-dangerous".to_string()));
        // No path completion in v1, and no guessing inside a prompt.
        assert!(Command::complete("explain the ", 12, &s).1.is_empty());
    }

    /// The mock gate is upstream (`ChatHost::models`), and this asserts the
    /// completer adds nothing of its own.
    #[test]
    fn completion_never_invents_a_candidate() {
        let s = snapshot();
        for (line, pos) in [("/model ", 7), ("/m", 2), ("/", 1)] {
            for item in Command::complete(line, pos, &s).1 {
                assert!(!item.to_lowercase().contains("mock"), "offered {item}");
            }
        }
    }
}
```

- [ ] **Step 2: Write the failing controller tests** in `controller.rs` — this is Review Focus 1 and 2, as a table:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn idle() -> Controller {
        Controller::new(CompletionSnapshot::default(), Interactivity::Interactive)
    }

    fn batch() -> Controller {
        Controller::new(CompletionSnapshot::default(), Interactivity::Batch)
    }

    #[test]
    fn a_prompt_starts_a_turn() {
        let mut c = idle();
        assert_eq!(c.on_line("explain the parser"),
                   vec![Action::Prompt("explain the parser".into())]);
        assert_eq!(c.state(), ChatState::Running);
    }

    #[test]
    fn an_empty_line_does_nothing() {
        let mut c = idle();
        assert_eq!(c.on_line("   "), vec![]);
        assert_eq!(c.state(), ChatState::Idle);
    }

    // --- Review Focus 1: the Ctrl-C table ------------------------------

    #[test]
    fn interrupt_while_running_cancels_the_turn_and_never_quits() {
        let mut c = idle();
        c.on_line("do something slow");
        let actions = c.on_signal(Signal::Interrupt);
        assert!(actions.contains(&Action::CancelRun), "{actions:?}");
        assert!(!actions.iter().any(|a| matches!(a, Action::Quit(_))), "{actions:?}");
    }

    #[test]
    fn interrupt_while_awaiting_approval_cancels_and_does_not_answer() {
        let mut c = idle();
        c.on_line("delete the logs");
        c.on_approval_requested("rm -rf logs (destructive)");
        assert_eq!(c.state(), ChatState::AwaitingApproval);
        let actions = c.on_signal(Signal::Interrupt);
        assert!(actions.contains(&Action::CancelRun), "{actions:?}");
        // Review Focus 2: no denial may be queued as well — an unconsumed
        // "n" could be swallowed by a later approval in the same run.
        assert!(!actions.iter().any(|a| matches!(a, Action::Approve(_))), "{actions:?}");
    }

    #[test]
    fn one_interrupt_at_an_idle_prompt_only_hints() {
        let mut c = idle();
        let actions = c.on_signal(Signal::Interrupt);
        assert!(!actions.iter().any(|a| matches!(a, Action::Quit(_))), "{actions:?}");
        let hint = actions.iter().find_map(|a| match a {
            Action::Write(line) => Some(line.text.clone()),
            _ => None,
        }).expect("a hint is printed");
        assert!(hint.contains("Ctrl-D"), "{hint}");
        assert!(hint.contains("/quit"), "{hint}");
    }

    #[test]
    fn a_second_interrupt_at_an_idle_prompt_quits_with_130() {
        let mut c = idle();
        c.on_signal(Signal::Interrupt);
        let actions = c.on_signal(Signal::Interrupt);
        assert!(actions.contains(&Action::Quit(130)), "{actions:?}");
    }

    /// Typing between the two interrupts means the user is still working.
    #[test]
    fn a_line_between_two_interrupts_resets_the_quit_arming() {
        let mut c = idle();
        c.on_signal(Signal::Interrupt);
        c.on_line("still here");
        c.on_run_settled();
        let actions = c.on_signal(Signal::Interrupt);
        assert!(!actions.contains(&Action::Quit(130)), "{actions:?}");
    }

    #[test]
    fn eof_quits_zero() {
        let mut c = idle();
        assert_eq!(c.on_signal(Signal::Eof), vec![Action::Quit(0)]);
    }

    /// Piped stdin's EOF means "no more input", not "stop now": the queue
    /// still runs, and there is nobody to ask for the live-job
    /// confirmation. `printf 'a\nb\n' | forge` must run both turns.
    #[test]
    fn in_batch_mode_eof_drains_the_queue_before_quitting() {
        let mut c = batch();
        c.on_line("first");
        c.on_line("second");
        let actions = c.on_signal(Signal::Eof);
        assert!(!actions.iter().any(|a| matches!(a, Action::Quit(_))),
                "the queued prompt has not run yet: {actions:?}");
        c.on_run_settled();
        assert_eq!(c.take_queued(), Some("second".to_string()));
        c.on_run_settled();
        assert_eq!(c.take_queued(), None);
        assert_eq!(c.on_drained(), vec![Action::CancelAllJobs, Action::Quit(0)]);
    }

    // --- approvals ------------------------------------------------------

    #[test]
    fn approval_answers_map_y_to_approve_and_everything_else_to_deny() {
        for (answer, approved) in [("y", true), ("Y", true), ("yes", true),
                                   ("n", false), ("", false), ("maybe", false)] {
            let mut c = idle();
            c.on_line("delete the logs");
            c.on_approval_requested("rm -rf logs (destructive)");
            assert_eq!(c.on_line(answer), vec![Action::Approve(approved)],
                       "answer {answer:?}");
            assert_eq!(c.state(), ChatState::Running, "the turn continues");
        }
    }

    // --- refusals and quit with work in flight --------------------------

    #[test]
    fn switching_the_model_is_refused_while_a_run_is_live() {
        let mut c = idle();
        c.on_line("something long");
        let actions = c.on_line("/model deepseek-chat");
        assert!(!actions.iter().any(|a| matches!(a, Action::Host(_))), "{actions:?}");
        let msg = actions.iter().find_map(|a| match a {
            Action::Write(line) => Some(line.text.clone()),
            _ => None,
        }).expect("an explanation is printed");
        assert!(msg.contains("/jobs"), "{msg}");
    }

    #[test]
    fn quitting_with_a_live_job_asks_once_then_abandons() {
        let mut c = idle();
        c.on_line("long job");
        c.on_line("/bg");
        let first = c.on_line("/quit");
        assert!(!first.iter().any(|a| matches!(a, Action::Quit(_))), "{first:?}");
        let second = c.on_line("/quit");
        assert!(second.contains(&Action::Quit(0)), "{second:?}");
        assert!(second.contains(&Action::CancelAllJobs), "{second:?}");
    }

    /// A line typed during a turn becomes the next turn; it is never lost.
    /// (The driver keeps a read outstanding during a turn — spec §6.2 —
    /// which is what makes this reachable, and `/bg` reachable at all.)
    #[test]
    fn prompts_submitted_while_running_queue_in_order() {
        let mut c = idle();
        c.on_line("first");
        assert!(c.on_line("second").is_empty(), "nothing happens yet");
        assert!(c.on_line("third").is_empty());
        c.on_run_settled();
        assert_eq!(c.take_queued(), Some("second".to_string()));
        assert_eq!(c.take_queued(), Some("third".to_string()), "FIFO, nothing dropped");
        assert_eq!(c.take_queued(), None);
    }

    /// The commands whose whole purpose is a turn that is taking too long.
    #[test]
    fn during_turn_commands_act_immediately() {
        let mut c = idle();
        c.on_line("long job");
        assert_eq!(c.on_line("/bg"), vec![Action::Background]);
        c.on_line("another long job");
        assert_eq!(c.on_line("/jobs"), vec![Action::ListJobs]);
    }

    /// Review Focus 2's neighbour: a command typed while an approval is
    /// pending must not be read as "anything else", which would deny.
    #[test]
    fn a_command_while_an_approval_is_pending_does_not_answer_it() {
        let mut c = idle();
        c.on_line("delete the logs");
        c.on_approval_requested("rm -rf logs (destructive)");
        let actions = c.on_line("/jobs");
        assert_eq!(actions, vec![Action::ListJobs]);
        assert_eq!(c.state(), ChatState::AwaitingApproval, "still waiting for an answer");
        assert_eq!(c.on_line("n"), vec![Action::Approve(false)]);
    }
}
```

- [ ] **Step 3: Run** `cargo test -p forge-chat` → FAIL.
- [ ] **Step 4: Implement `command.rs`** — `Parsed` as an enum with `PartialEq`, `Command::parse` (trim, require a leading `/` **and** a first token in the command set or the snapshot's skills, else `Prompt`; `Unknown` for an unmatched `/word` whose first token has no `/` in it — a token containing a second `/` is a path and therefore a prompt), `Command::complete` per the §9.2 rules, and a `pub const COMMANDS: &[(&str, &str)]` name/description table that `/help` also renders, so there is one list.
- [ ] **Step 5: Implement `controller.rs`** — `ChatState { Idle, Running, AwaitingApproval, Detached }`, `Signal { Interrupt, Eof }`, `Interactivity { Interactive, Batch }`, `Action` (the spec's §3-level list plus `CancelAllJobs`), and the transitions the tests pin. The quit-arming timestamp uses `std::time::Instant` with a 2-second window; expose `Controller::arm_window()` so the test can reason about it without sleeping. `Interactivity::Batch` is what piped stdin passes: an `Eof` there drains the queue and needs no confirmation, because there is nobody to confirm with (spec §12.3).
- [ ] **Step 6: Run** → PASS.
- [ ] **Step 7:** `just verify` → PASS. **Commit** — `git add -A && git commit -m "feat(chat): slash commands, completion and the input state machine"`

---

### Task 6: an explicit approval channel in `forge-execution`

Review Focus 3. Today `NativeExecution` decides whether to prompt by asking whether stdin is a TTY, which inside a chat means two readers on one file descriptor — one of them in raw mode on another thread. The channel becomes explicit.

**Files:**
- Modify: `crates/forge-execution/src/native.rs`
- Modify: `crates/forge-execution/src/lib.rs` (re-export)
- Modify: `crates/forge-cli/src/commands/service.rs` (`ServiceOptions`, `build_service_with`, `build_run_service_with`)
- Modify: `README.md` (ExecutionProvider paragraph)

**Interfaces:**
- Produces: `forge_execution::ApprovalChannel::{InlineTty, Parked}`, `NativeExecution::with_channel(policy, root, channel)`, `forge_cli::commands::service::{ServiceOptions, build_service_with, build_run_service_with}` — consumed by Task 9.
- Unchanged: `NativeExecution::new`, `build_execution`, `build_service`, `build_run_service` — all four keep today's behaviour and today's call sites (`run_cmd`, `serve_cmd`, `mcp_cmd`, `acp_cmd`, `session_cmd`, `skill_cmd`, `router_cmd`) compile untouched.

- [ ] **Step 1: Write the failing tests** in `native.rs`'s existing test module (which already asserts `!std::io::stdin().is_terminal()` for the inline path, so the *parked* path is the one that can be tested unconditionally):

```rust
    /// The chat owns stdin through a line editor in raw mode on another
    /// thread. A provider that read stdin itself would fight it, and the
    /// transcript would never show `approval_requested`. So `Parked`
    /// always defers to the run's input channel.
    #[test]
    fn parked_never_prompts_and_always_asks_the_caller() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let exec = NativeExecution::with_channel(
            ApprovalPolicy::Prompt,
            tmp.path(),
            ApprovalChannel::Parked,
        );
        let err = exec
            .check_approval("write notes.txt", RiskLevel::Risky)
            .expect_err("risky work under `prompt` must not be allowed silently");
        assert!(matches!(err, ForgeError::ApprovalRequired { .. }), "{err:?}");
    }

    #[test]
    fn parked_leaves_safe_auto_and_deny_exactly_as_they_were() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let parked = |policy| {
            NativeExecution::with_channel(policy, tmp.path(), ApprovalChannel::Parked)
        };
        // Safe work is never gated, whatever the channel.
        assert!(parked(ApprovalPolicy::Prompt)
            .check_approval("read main.rs", RiskLevel::Safe)
            .is_ok());
        assert!(parked(ApprovalPolicy::Auto)
            .check_approval("write notes.txt", RiskLevel::Risky)
            .is_ok());
        let denied = parked(ApprovalPolicy::Deny)
            .check_approval("write notes.txt", RiskLevel::Risky)
            .expect_err("deny blocks");
        assert!(!matches!(denied, ForgeError::ApprovalRequired { .. }),
                "deny is a refusal, not a question");
        // prompt-dangerous still only asks about destructive work.
        assert!(parked(ApprovalPolicy::PromptDestructive)
            .check_approval("write notes.txt", RiskLevel::Risky)
            .is_ok());
        assert!(matches!(
            parked(ApprovalPolicy::PromptDestructive)
                .check_approval("rm -rf logs", RiskLevel::Destructive)
                .expect_err("destructive asks"),
            ForgeError::ApprovalRequired { .. }
        ));
    }

    #[test]
    fn the_default_constructor_is_still_inline() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let exec = NativeExecution::new(ApprovalPolicy::Prompt, tmp.path());
        assert_eq!(exec.approval_channel(), ApprovalChannel::InlineTty);
    }
```

`check_approval` is private today; make it `pub(crate)` (or `#[cfg(test)]`-visible) rather than routing these through `execute`, so the test is about the gate and not about spawning processes.

- [ ] **Step 2: Run** `cargo test -p forge-execution` → FAIL.
- [ ] **Step 3: Implement** — add `ApprovalChannel` (derive `Debug, Clone, Copy, PartialEq, Eq`), store it on `NativeExecution`, default it to `InlineTty` in `new`, add `with_channel` and `approval_channel()`. In `check_approval`, the two prompting arms become: `InlineTty` → today's `prompt_for_approval`, `Parked` → `Err(ForgeError::ApprovalRequired { description, risk })`. Document at the enum *why* it is explicit rather than inferred, naming the chat's raw-mode editor thread.
- [ ] **Step 4: Thread it through the CLI** in `commands/service.rs`:

```rust
/// Non-default choices a front end makes about its runtime.
///
/// `model`/`approval` hold the same strings the `--model`/`--approval`
/// flags take and are applied into `CliOverrides` before `Config::load`,
/// so the chat's `/model` and `/approval` go through the *one* override
/// path rather than a second one that could resolve differently.
#[derive(Debug, Clone, Default)]
pub struct ServiceOptions {
    /// How a risky operation asks. Default: today's behaviour.
    pub approvals: forge_execution::ApprovalChannel,
    pub model: Option<String>,
    pub approval: Option<String>,
}
```

`ApprovalChannel` therefore needs `Default` (= `InlineTty`), which is what keeps `ServiceOptions::default()` equal to today's behaviour. Keep `build_service(ctx)` / `build_run_service(ctx)` as one-line wrappers over `*_with(ctx, ServiceOptions::default())`, so no existing call site changes.

- [ ] **Step 5: Run** `cargo test -p forge-execution -p forge-cli` → PASS.
- [ ] **Step 6: README** — in the `ExecutionProvider` paragraph, after the existing non-interactive sentence: "Front ends that own stdin themselves — `forge mcp`, `forge acp`, and the interactive chat — take the *parked* channel explicitly: a risky operation always returns an `approval required` pause and is answered through the run's input channel, so nothing ever reads stdin behind the protocol's or the line editor's back."
- [ ] **Step 7:** `just verify` → PASS. **Commit** — `git add -A && git commit -m "feat(execution): explicit parked approval channel for stdin-owning front ends"`

---

### Task 7: `app.rs` — the async driver, tested in-process

The turn driver, the approval round trip, and the loop that survives a failed turn. All of it exercised with a `ScriptedIo` and a `FakeHost` — no terminal, no process, no network.

**Files:**
- Create: `crates/forge-chat/src/app.rs`, `crates/forge-chat/src/testing.rs` (`#[cfg(any(test, feature = "testing"))]` — `ScriptedIo` + `FakeHost`)

**Interfaces:**
- Consumes: `ChatIo`, `ChatHost` (Task 2), `TranscriptState` (Task 4), `Controller`/`Action` (Task 5), `AgentService::{subscribe, start_run_with_options, send_input, cancel, attach, list_runs, fork_session, sessions}`.
- Produces: `pub async fn run(io: impl ChatIo, host: impl ChatHost, start: Start) -> Result<i32, ForgeError>`, `pub struct Start { pub session: SessionStart, pub first_prompt: Option<String> }` with `Start::{fresh, continue_latest, named}`, `pub enum SessionStart { Fresh, Continue, Named(String) }` — consumed by Task 9.

- [ ] **Step 1: Write the failing tests** in `app.rs`. The helpers build a real `AgentService` over the scripted mock model and `MockExecution`, exactly as `forge-runtime`'s own service tests do — read `crates/forge-runtime/src/service/tests.rs` and reuse its construction shape:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{FakeHost, ScriptedIo};

    /// One turn: the transcript shows the routing line, the tool call, and
    /// the answer exactly once.
    #[tokio::test]
    async fn a_turn_renders_and_answers_once() {
        let (host, _tmp) = FakeHost::with_script(r#"[
            {"tool_calls": [{"id": "c1", "name": "read_file", "arguments": {"path": "alpha.rs"}}]},
            {"text": "alpha.rs defines parse_config"}
        ]"#);
        let mut io = ScriptedIo::new(["explain alpha.rs", "/quit"]);
        let code = run(io.handle(), host, Start::fresh()).await.expect("chat runs");
        assert_eq!(code, 0);
        let out = io.output();
        assert!(out.contains("  * read_file alpha.rs"), "{out}");
        assert_eq!(out.matches("alpha.rs defines parse_config").count(), 1,
                   "the answer is printed exactly once:\n{out}");
        assert!(out.contains("  = 2 turns"), "the footer reports the turns:\n{out}");
    }

    /// The fast path has no assistant text, so the outcome is the answer —
    /// and still only once.
    #[tokio::test]
    async fn a_turn_with_no_assistant_text_falls_back_to_the_run_outcome() {
        let (host, _tmp) = FakeHost::with_script(r#"[{"text": ""}]"#);
        let mut io = ScriptedIo::new(["say nothing", "/quit"]);
        run(io.handle(), host, Start::fresh()).await.expect("chat runs");
        // Nothing textual rendered and nothing to fall back to: no blank
        // block, no panic, and the loop kept going.
        assert!(io.output().contains("  = "), "{}", io.output());
    }

    /// Review Focus 1, in the driver: cancel the run, survive, stay usable.
    #[tokio::test]
    async fn an_interrupt_mid_turn_cancels_the_run_and_the_chat_continues() {
        let (host, _tmp) = FakeHost::with_slow_script();
        let mut io = ScriptedIo::new(["something slow", "still here", "/quit"]);
        io.interrupt_after_first_prompt();
        let code = run(io.handle(), host, Start::fresh()).await.expect("chat runs");
        assert_eq!(code, 0, "an interrupt must not change the exit code");
        let out = io.output();
        assert!(out.contains("  ! cancelled"), "{out}");
        assert!(out.contains("still here") || out.contains("  = "),
                "the chat accepted another turn afterwards:\n{out}");
    }

    /// Review Focus 3 + §8: the parked mechanism, answered from the chat.
    #[tokio::test]
    async fn an_approval_is_asked_in_the_transcript_and_answered_from_the_prompt() {
        let (host, tmp) = FakeHost::writing_project();  // parked approvals, `prompt`
        let mut io = ScriptedIo::new(["write the notes", "y", "/quit"]);
        run(io.handle(), host, Start::fresh()).await.expect("chat runs");
        let out = io.output();
        assert!(out.contains("  ! approval needed:"), "{out}");
        assert!(out.contains("    -> approved"), "{out}");
        assert!(tmp.path().join("notes.txt").exists(), "approved work must happen");
    }

    #[tokio::test]
    async fn a_denied_approval_leaves_the_file_alone_and_the_turn_continues() {
        let (host, tmp) = FakeHost::writing_project();
        let mut io = ScriptedIo::new(["write the notes", "n", "/quit"]);
        run(io.handle(), host, Start::fresh()).await.expect("chat runs");
        assert!(io.output().contains("    -> denied"), "{}", io.output());
        assert!(!tmp.path().join("notes.txt").exists(), "denied work must not happen");
    }

    /// Review Focus 2: interrupting the approval must not run the operation.
    #[tokio::test]
    async fn an_interrupt_during_an_approval_cancels_without_running_it() {
        let (host, tmp) = FakeHost::writing_project();
        let mut io = ScriptedIo::new(["write the notes", "/quit"]);
        // The approval question is a transcript line, not a prompt, so the
        // interrupt is scheduled on the output rather than on a prompt.
        io.interrupt_when_output_contains("approval needed");
        run(io.handle(), host, Start::fresh()).await.expect("chat runs");
        assert!(io.output().contains("  ! cancelled"), "{}", io.output());
        assert!(!tmp.path().join("notes.txt").exists(),
                "the pending operation must not have run");
    }

    /// Review Focus 7: a turn that fails is a line, not the end.
    #[tokio::test]
    async fn a_failing_turn_prints_an_error_and_keeps_the_chat_alive() {
        let (host, _tmp) = FakeHost::with_unreachable_model();
        let mut io = ScriptedIo::new(["anything", "/help", "/quit"]);
        let code = run(io.handle(), host, Start::fresh()).await.expect("chat runs");
        assert_eq!(code, 0);
        let out = io.output();
        assert!(out.contains("  ! error:"), "{out}");
        assert!(out.contains("/model"), "/help still worked afterwards:\n{out}");
    }

    /// One session across turns, which is what makes history real (§7).
    #[tokio::test]
    async fn every_turn_lands_in_one_session() {
        let (host, _tmp) = FakeHost::with_script(r#"[{"text": "ok"}]"#);
        let service = host.service();
        let mut io = ScriptedIo::new(["first", "second", "/quit"]);
        run(io.handle(), host, Start::fresh()).await.expect("chat runs");
        let sessions = service.sessions().list_sessions().expect("sessions");
        assert_eq!(sessions.len(), 1, "two turns, one session: {sessions:?}");
    }

    /// `/bg` detaches *and* moves the conversation to a fork, because two
    /// runs writing into one session log would replay interleaved (spec
    /// §10.1.1). This is the test that pins that.
    #[tokio::test]
    async fn bg_detaches_and_continues_in_a_fork() {
        let (host, _tmp) = FakeHost::with_slow_script();
        let service = host.service();
        let mut io = ScriptedIo::new(["long job", "/bg", "/jobs", "/quit", "/quit"]);
        run(io.handle(), host, Start::fresh()).await.expect("chat runs");
        let out = io.output();
        assert!(out.contains("  - detached run"), "{out}");
        assert!(out.contains("continues in fork"), "the fork is announced:\n{out}");
        assert!(out.contains("running"), "/jobs shows its state:\n{out}");
        assert!(out.contains("job still running"), "quit asks once:\n{out}");
        assert_eq!(service.sessions().list_sessions().expect("sessions").len(), 2,
                   "the job keeps its session; the conversation moved to a fork");
    }

    /// The same rule from the other side: a turn is never started in a
    /// session that already has a live run.
    #[tokio::test]
    async fn fork_is_refused_while_a_turn_is_attached_and_points_at_bg() {
        let (host, _tmp) = FakeHost::with_slow_script();
        let mut io = ScriptedIo::new(["long job", "/fork", "/quit", "/quit"]);
        run(io.handle(), host, Start::fresh()).await.expect("chat runs");
        let out = io.output();
        assert!(out.contains("/bg"), "the refusal names the way forward:\n{out}");
        assert!(!out.contains("  - forked to session"), "no fork happened:\n{out}");
    }

    #[tokio::test]
    async fn fork_continues_in_the_new_session_and_leaves_the_source_alone() {
        let (host, _tmp) = FakeHost::with_script(r#"[{"text": "ok"}]"#);
        let service = host.service();
        let mut io = ScriptedIo::new(["first", "/fork", "second", "/quit"]);
        run(io.handle(), host, Start::fresh()).await.expect("chat runs");
        let out = io.output();
        assert!(out.contains("  - forked to session"), "{out}");
        assert!(out.contains("is untouched"), "{out}");
        let sessions = service.sessions().list_sessions().expect("sessions");
        assert_eq!(sessions.len(), 2, "the fork is a second session: {sessions:?}");
    }

    #[tokio::test]
    async fn resuming_a_session_rerenders_its_transcript_through_one_renderer() {
        let (host, _tmp) = FakeHost::with_script(r#"[{"text": "the first answer"}]"#);
        let service = host.service();
        let mut first = ScriptedIo::new(["first", "/quit"]);
        run(first.handle(), host.clone(), Start::fresh()).await.expect("first chat");
        let session = service.sessions().list_sessions().expect("sessions")[0].session_id.clone();

        let mut second = ScriptedIo::new(["/quit"]);
        run(second.handle(), host, Start::named(&session)).await.expect("second chat");
        let out = second.output();
        assert!(out.contains("  - resumed session"), "{out}");
        assert!(out.contains("the first answer"), "history is re-rendered:\n{out}");
    }
}
```

- [ ] **Step 2: Run** `cargo test -p forge-chat app` → FAIL.
- [ ] **Step 3: Implement `testing.rs`** — `ScriptedIo` (a queue of input lines, an interrupt trigger with the two scheduling helpers the tests use (`interrupt_after_first_prompt`, `interrupt_when_output_contains`), an `Arc<Mutex<String>>` output buffer shared with the `handle()` it hands to `run`, `interrupted()` resolving from a `tokio::sync::Notify` and otherwise pending forever, `interactivity()` reporting `Interactive` by default with a `ScriptedIo::batch(..)` constructor for the queue-draining case) and `FakeHost` (**`Clone`**, since one test runs two chats over one runtime: a `tempfile::TempDir` project, a `scripted-mock` model built from the given script — providers are constructed directly, which the `FORGE_TEST_MOCKS` gate does not restrict, since it gates what *configuration* may select — `MockExecution` or `NativeExecution::with_channel(.., Parked)` for the writing case, a `JsonlSessionStore` under the temp project, `StaticRouter`, and `ChatHost` methods returning fixed data).
- [ ] **Step 4: Implement `app.rs`:**
  - the entry point: resolve `Start` into a session id (fresh / most recent / named), re-render a resumed transcript (bounded at 200 lines, with a `... n earlier lines` note), print the banner, then loop;
  - the loop: **one `io.read(...)` is always outstanding**, including while a turn runs (spec §6.2 — without it `/bg` is unreachable), so the driver is a single `select!` over the read, the attached run's events, the run's `JoinHandle`, and `io.interrupted()`; each line goes to the pure `Controller` and each resulting `Action` is executed against host/service/io, then the next read is issued;
  - transcript lines go to `io.write` between turns and `io.notify` while a turn is running (the terminal implementation puts the latter through rustyline's `ExternalPrinter`, which redraws the prompt underneath). Both take the same `Line`, so the transcript is identical either way;
  - the turn driver, copied from `forge-acp::server::run_turn`: `subscribe` **before** `start_run_with_options`; drain `try_recv` after settling; print the answer per §4.3; print the footer; then submit the first queued prompt. On cancel, `service.cancel(run_id)` then await the handle with a 2-second grace before giving up on it;
  - the approval round trip: on `ApprovalRequested`, render the question as a transcript line and let the **already-outstanding read** carry the answer (`service.send_input(run_id, "y"|"n")`). Nothing swaps the prompt text and no second read path exists — unlike ACP, which has to spawn the ask because its reader must stay free;
  - **never start a turn in a session that already has a live run** (spec §5): `Action::Background` forks the session for the foreground and leaves the detached run in the original, and `/fork` while a turn is attached is refused with the message that names `/bg`;
  - the background watcher: a task per detached run consuming its stream and emitting only the two notice kinds of §10.1;
  - `/attach`: `service.attach(run_id)`, render the backlog, then stream while `is_live()`, else print the cross-process note and return.
- [ ] **Step 5: Run** → PASS.
- [ ] **Step 6:** `just verify` → PASS. **Commit** — `git add -A && git commit -m "feat(chat): the async chat driver, with in-process end-to-end tests"`

---

### Task 8: `TerminalIo` — rustyline on a dedicated editor thread

**Files:**
- Modify: root `Cargo.toml` (`rustyline` in `[workspace.dependencies]`), `crates/forge-cli/Cargo.toml`
- Create: `crates/forge-cli/src/chat/mod.rs`, `crates/forge-cli/src/chat/terminal_io.rs`, `crates/forge-cli/src/chat/palette.rs`, `crates/forge-cli/src/chat/piped_io.rs`
- Modify: `crates/forge-cli/src/main.rs` or `lib.rs` (`mod chat;`)

**Interfaces:**
- Consumes: `forge_chat::{ChatIo, Line, Style, Prompt, ReadOutcome}`, `forge_chat::Command::complete`.
- Produces: `chat::terminal_io::TerminalIo::new(palette, history_path) -> Result<Self, ForgeError>`, `chat::piped_io::PipedIo::new(palette)`, `chat::palette::Palette::detect(no_color_flag) -> Palette` — consumed by Task 9.

- [ ] **Step 1: Add the dependency** to the root `Cargo.toml`, with the cost comment the repo uses for `rmcp`:

```toml
# Inline line editor for the interactive chat: history, completion,
# multi-line input and typed Ctrl-C/Ctrl-D, without taking over the
# screen. default-features off drops `derive` (a proc-macro tree) and
# `with-dirs` (`home`); what is left adds 8 packages to this workspace,
# of which 2 are Windows-only. reedline was measured at 23 and rejected
# on cost (see the Phase B design, §2.2).
rustyline = { version = "18", default-features = false, features = [
    "custom-bindings",
    "with-file-history",
] }
```

- [ ] **Step 2: Write the failing tests** in `palette.rs` and `terminal_io.rs`. Only the palette and the line-validator are unit-testable here (an `Editor` needs a terminal), and they are exactly the TTY-only logic that is ours:

```rust
// palette.rs
#[cfg(test)]
mod tests {
    use super::*;
    use forge_chat::Style;
    use serial_test::serial;

    fn with_env(vars: &[(&str, Option<&str>)], f: impl FnOnce()) {
        // edition 2024: env mutation is unsafe, and #[serial] keeps it sane.
        let saved: Vec<_> = vars.iter().map(|(k, _)| (*k, std::env::var(k).ok())).collect();
        for (key, value) in vars {
            match value {
                Some(v) => unsafe { std::env::set_var(key, v) },
                None => unsafe { std::env::remove_var(key) },
            }
        }
        f();
        for (key, value) in saved {
            match value {
                Some(v) => unsafe { std::env::set_var(key, v) },
                None => unsafe { std::env::remove_var(key) },
            }
        }
    }

    #[test]
    #[serial]
    fn no_color_disables_every_style() {
        with_env(&[("NO_COLOR", Some("1")), ("TERM", Some("xterm-256color"))], || {
            let p = Palette::detect(false);
            assert!(!p.enabled());
            assert_eq!(p.paint(Style::Bad, "  ! error: boom"), "  ! error: boom");
        });
    }

    #[test]
    #[serial]
    fn a_dumb_terminal_disables_every_style() {
        with_env(&[("NO_COLOR", None), ("TERM", Some("dumb"))], || {
            assert!(!Palette::detect(false).enabled());
        });
    }

    #[test]
    #[serial]
    fn the_flag_alone_disables_every_style() {
        with_env(&[("NO_COLOR", None), ("TERM", Some("xterm-256color"))], || {
            assert!(!Palette::detect(true).enabled());
        });
    }

    #[test]
    fn an_enabled_palette_wraps_and_always_resets() {
        let p = Palette::forced_on_for_tests();
        let painted = p.paint(Style::Bad, "  ! error: boom");
        assert!(painted.starts_with("\x1b["), "{painted:?}");
        assert!(painted.ends_with("\x1b[0m"), "{painted:?}");
        assert!(painted.contains("  ! error: boom"));
        // Plain text is never decorated, so an answer is never coloured.
        assert_eq!(p.paint(Style::Plain, "the answer"), "the answer");
    }
}
```

```rust
// terminal_io.rs
#[cfg(test)]
mod tests {
    use super::*;

    /// Review Focus 4: a pasted fenced block is one input, not five turns.
    #[test]
    fn an_unterminated_fence_is_incomplete_input() {
        assert!(!is_complete("fix this:\n```rust\nfn main() {}"));
        assert!(is_complete("fix this:\n```rust\nfn main() {}\n```"));
    }

    #[test]
    fn a_trailing_backslash_continues_the_line() {
        assert!(!is_complete("first line \\"));
        assert!(is_complete("first line"));
        // An escaped backslash is not a continuation.
        assert!(is_complete("a path c:\\\\tmp\\\\"));
    }

    #[test]
    fn an_ordinary_line_is_complete() {
        assert!(is_complete("explain the parser"));
        assert!(is_complete(""));
    }

    /// `ReadlineError::Interrupted` carries no buffer, so the distinction
    /// the spec's Ctrl-C table needs is made here: a typed line is cleared
    /// inside the editor and the chat is never told, while an empty line
    /// interrupts. Without this, two Ctrl-Cs used to clear two typed lines
    /// would look like a request to exit.
    #[test]
    fn ctrl_c_clears_a_typed_line_and_interrupts_an_empty_one() {
        assert_eq!(ctrl_c_command("half a thought"), CtrlC::ClearLine);
        assert_eq!(ctrl_c_command(""), CtrlC::Interrupt);
        assert_eq!(ctrl_c_command("   "), CtrlC::ClearLine, "whitespace is typed text");
    }
}
```

- [ ] **Step 3: Run** `cargo test -p forge-cli palette` → FAIL.
- [ ] **Step 4: Implement `palette.rs`** — `Palette::detect(no_color_flag)`: enabled iff `std::io::stdout().is_terminal()` && `NO_COLOR` unset && `!no_color_flag` && `TERM` is set and not `dumb`; `paint(style, text)` wrapping in the §2.4 escapes (`Plain` never decorated); `forced_on_for_tests()` behind `#[cfg(test)]`.
- [ ] **Step 5: Implement `terminal_io.rs`:**
  - `is_complete(input)` as tested, used by a `Validator` returning `Incomplete` otherwise;
  - a `ChatHelper` implementing `Completer` (delegating to `forge_chat::Command::complete` over the snapshot carried in the current `Prompt`), `Validator` (above), and empty `Hinter`/`Highlighter`, plus `impl Helper` — hand-written, since the `derive` feature is off;
  - one dedicated `std::thread` owning the `Editor`, taking `(Prompt, oneshot::Sender<ReadOutcome>)` over an `mpsc`, mapping `Ok(line)` → `Line`, `Err(Interrupted)` → `Interrupt`, `Err(Eof)` → `Eof`, anything else → `Failed(msg)`; `append_history` after each accepted line (debug-log failures, never fatal); `save_history` on shutdown. The comment must name the pattern it copies: `NeedleEngine`'s dedicated engine thread, for the same reason (a blocking, non-`Send`-friendly resource behind a channel);
  - `ctrl_c_command(line) -> CtrlC { ClearLine, Interrupt }` as tested, wrapped in an `EventHandler::Conditional` bound to `Ctrl-C` — `ClearLine` returns `Some(Cmd::Kill(Movement::WholeBuffer))`, `Interrupt` returns `Some(Cmd::Interrupt)`; the handler reads the buffer from `EventContext::line()`;
  - `Editor` config: `ColorMode` from the palette, `max_history_size(1000)`, `history_ignore_dups(true)`, `auto_add_history(true)`, `completion_type(List)`, `bind_sequence(Alt-Enter, Cmd::Newline)`, and the `Ctrl-C` handler above;
  - `write` → stdout via the palette (used between turns); `notify` → the `ExternalPrinter` via the palette (used while a turn runs, and for background notices). A read is outstanding whenever `notify` is called except for the microseconds between two reads, so a notice is never noticeably delayed. If `create_external_printer()` fails (a dumb or unsupported terminal), fall back to plain stdout and log at debug — a notice printed slightly awkwardly beats a notice lost. `interrupted()` → `tokio::signal::ctrl_c()`, which on a TTY is the *secondary* path (a typed `Ctrl-C` arrives as `ReadOutcome::Interrupt` instead) and in piped mode is the only one;
  - a module comment recording the two verified termios facts the design rests on: raw mode is entered with `TCSADRAIN`, so type-ahead during a turn is preserved (Review Focus 6), and `Ctrl-D` is the terminal's `VEOF`, so it honours the user's `stty`.
- [ ] **Step 6: Implement `piped_io.rs`** — §12.3: read stdin lines with `BufRead` on a blocking task, echo each with the `> ` gutter, `Eof` at end of input, no history, no completion, `interrupted()` → `tokio::signal::ctrl_c()` (so Ctrl-C works in a pipe too, which is what Task 10 tests).
- [ ] **Step 7: Run** `cargo test -p forge-cli` → PASS; `cargo tree -p forge-cli -i rustyline` to confirm the feature set resolved as intended.
- [ ] **Step 8:** `just verify` → PASS. **Commit** — `git add -A && git commit -m "feat(cli): rustyline-backed terminal io for the chat"`

---

### Task 9: `CliHost`, and the real `chat` command

The stub from Task 1 becomes the real thing.

**Files:**
- Create: `crates/forge-cli/src/chat/host.rs`
- Modify: `crates/forge-cli/src/commands/chat_cmd.rs`
- Modify: `crates/forge-cli/Cargo.toml` (`forge-chat`)

**Interfaces:**
- Consumes: `forge_chat::{app, ChatHost, Start, SessionStart}`, `chat::{TerminalIo, PipedIo, Palette}`, `commands::service::{build_run_service_with, ServiceOptions}`, `forge_config::Config`, `forge_graph::query`, `forge_needle::engine_if_available`.
- Produces: `chat::host::CliHost::new(ctx) -> Result<Self, ForgeError>` implementing `ChatHost`.

- [ ] **Step 1: Write the failing tests** in `host.rs` — the mock filter is the one piece with real logic and a real hazard:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// The single place mocks are filtered out (§9.3). Everything the chat
    /// *offers* comes from here, so this test is the gate.
    #[test]
    fn model_choices_never_include_a_test_only_mock() {
        let names = visible_models(
            &["qwen3-coder", "mock-local", "scripted-mock", "deepseek-chat", "mock"],
            "qwen3-coder",
        );
        assert_eq!(
            names.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            vec!["deepseek-chat", "qwen3-coder"],
        );
        assert!(names.iter().any(|m| m.name == "qwen3-coder" && m.active));
    }

    /// ...but a user who really is running one still sees their own
    /// configuration reported honestly; hiding it would be a lie.
    #[test]
    fn the_active_model_is_reported_even_when_it_is_a_mock() {
        let names = visible_models(&["qwen3-coder", "scripted-mock"], "scripted-mock");
        assert!(names.iter().all(|m| m.name != "scripted-mock"),
                "a mock is never offered as a choice");
        assert_eq!(active_model_label("scripted-mock"), "scripted-mock",
                   "`/config` and the banner report what is actually configured");
    }
}
```

- [ ] **Step 2: Run** → FAIL. **Step 3: Implement `host.rs`** — `visible_models` (sorted, mocks removed via one `const MOCK_MODELS: &[&str]` list co-located with the filter), `environment()` (project root, model, router, approval, and `NeedleState` from `forge_needle::engine_if_available`, phrased with `forge doctor`'s vocabulary), `models`, `skills` (`FsSkillRegistry::list`), `config_summary` (the same data `forge config show`/`explain` print — reuse `config_cmd`'s accessors rather than re-deriving origins), `graph_context` (`forge_graph::query`, the shared ranked-context implementation), and `switch` (rebuild through `build_run_service_with` + `ServiceOptions`, keeping the old service on error).
- [ ] **Step 4: Implement `chat_cmd.rs`** — refuse `--json`; build `CliHost`; pick the io by `std::io::stdin().is_terminal()` (`TerminalIo` with the history path `<project>/.forge/chat-history`, else `PipedIo`); map `ChatArgs` to `Start`; `forge_chat::app::run(...)`; `io.shutdown()`; `std::process::exit(code)` only for the non-zero case, returning `Ok(())` otherwise so the normal error path is untouched.
- [ ] **Step 5: Run** `cargo test -p forge-cli` → PASS. Manual smoke (not a test, but do it once): `cargo run -p forge-cli` in a scratch project — banner, `/help`, Tab completion, a turn against a local model or `FORGE_TEST_MOCKS=1` + `scripted-mock`, `Ctrl-C` mid-turn, `Ctrl-D` to exit.
- [ ] **Step 6:** `just verify` → PASS. **Commit** — `git add -A && git commit -m "feat(cli): the interactive chat, wired to the shared runtime"`

---

### Task 10: process-level tests — `crates/forge-cli/tests/chat.rs`

Every other adapter has them; the interrupt guarantee in particular can only be proved against a real process and a real signal.

**Files:**
- Create: `crates/forge-cli/tests/chat.rs`
- Modify: `crates/forge-cli/Cargo.toml` (only if a dev-dependency is needed to send a signal — prefer `libc`, already in the lock via rustyline, over a new crate)

**Interfaces:**
- Consumes: the compiled binary via `env!("CARGO_BIN_EXE_forge")`, hermetic env per `tests/acp.rs`.

- [ ] **Step 1: Write the tests.** Copy `tests/acp.rs`'s `FORGE_ENV_VARS` list and `forge()` helper verbatim (temp HOME/XDG, scrub, `FORGE_NEEDLE_AUTOFETCH=false`, `FORGE_TEST_MOCKS=1`, `NO_COLOR=1`), then:

```rust
/// Drive a chat session by writing lines to the child's stdin.
fn chat(tmp: &Path, project: &Path, lines: &[&str]) -> std::process::Output {
    let mut child = forge(tmp, project)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn forge");
    {
        let mut stdin = child.stdin.take().expect("stdin");
        for line in lines {
            writeln!(stdin, "{line}").expect("write");
        }
    } // dropping stdin is EOF, which ends the chat
    child.wait_with_output().expect("wait")
}

#[test]
fn a_piped_conversation_runs_two_turns_in_one_session() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path(), "auto");
    let out = chat(tmp.path(), &project, &["explain alpha.rs", "and again"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("> explain alpha.rs"), "piped input is echoed:\n{stdout}");
    assert_eq!(stdout.matches("  = ").count(), 2, "two turn footers:\n{stdout}");
    let sessions = std::fs::read_dir(project.join(".forge").join("sessions"))
        .expect("sessions dir")
        .count();
    assert_eq!(sessions, 1, "two turns share one session");
}

#[test]
fn slash_commands_answer_and_never_name_a_mock() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path(), "auto");
    let out = chat(tmp.path(), &project, &["/help", "/model", "/skills", "/session", "/jobs"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    for expected in ["/fork", "/attach", "/approval"] {
        assert!(stdout.contains(expected), "/help lists {expected}:\n{stdout}");
    }
    // `/model` lists candidates; none of them may be a test-only mock.
    for line in stdout.lines().filter(|l| !l.starts_with("> ")) {
        assert!(!line.to_lowercase().contains("mock"), "mock leaked: {line}");
    }
}

#[test]
fn an_approval_is_answered_from_the_conversation() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path(), "prompt");   // scripted model writes notes.txt
    let out = chat(tmp.path(), &project, &["write the notes", "y"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("approval needed"), "{stdout}");
    assert!(project.join("notes.txt").exists(), "approved work happened");
    let log = session_log(&project);
    assert!(log.contains("\"approval_requested\""), "the parked mechanism was used:\n{log}");
    assert!(log.contains("\"approved\":true"), "{log}");
}

#[test]
fn a_denied_approval_leaves_the_file_alone() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path(), "prompt");
    let out = chat(tmp.path(), &project, &["write the notes", "n"]);
    assert!(out.status.success());
    assert!(!project.join("notes.txt").exists());
    assert!(session_log(&project).contains("\"approved\":false"));
}

/// Review Focus 1, for real: SIGINT mid-turn cancels the turn, the process
/// survives, and EOF still exits 0. This is the piped path's interrupt
/// (`ChatIo::interrupted`); a *typed* Ctrl-C on a TTY arrives as
/// `ReadOutcome::Interrupt` instead, which the conditional-binding test in
/// Task 8 and the controller table in Task 5 cover between them.
#[cfg(unix)]
#[test]
fn sigint_cancels_the_turn_without_killing_the_chat() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // The scripted model's tool call runs `sleep`, so the turn is reliably
    // in flight when the signal arrives.
    let project = scaffold_slow(tmp.path());
    let mut child = forge(tmp.path(), &project)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .spawn().expect("spawn");
    let mut stdin = child.stdin.take().expect("stdin");
    writeln!(stdin, "start the slow thing").expect("write");
    // Wait for the turn to be visibly under way before signalling, rather
    // than sleeping a guessed interval.
    wait_for_stdout_line(&mut child, "  * run_command");
    unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    writeln!(stdin, "/quit").expect("the chat must still be listening");
    drop(stdin);
    let out = child.wait_with_output().expect("wait");
    assert!(out.status.success(), "SIGINT must not change the exit status: {:?}", out.status);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("  ! cancelled"), "{stdout}");
    assert!(session_log(&project).contains("\"cancelled\""), "the run recorded it");
}

#[test]
fn stdout_carries_the_transcript_and_stderr_carries_diagnostics() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path(), "auto");
    let mut cmd = forge(tmp.path(), &project);
    cmd.arg("-vvv");
    // ... spawn as in `chat`, one prompt, then EOF
    // stdout: the transcript only; stderr: tracing output, and nothing
    // that looks like a transcript line.
}
```

`wait_for_stdout_line` reads the child's stdout on a thread until the marker appears or a generous timeout elapses — the same shape the ACP/MCP tests use to avoid sleeping. Prefer `libc::kill` (already in the dependency graph) over adding a signal crate; if that is unacceptable, shell out to `kill -INT <pid>`.

- [ ] **Step 2: Run** `cargo test -p forge-cli --test chat` → FAIL where behaviour is missing; fix the *implementation*, not the test, unless the test is wrong about the spec.
- [ ] **Step 3: Run** the whole suite: `cargo test -p forge-cli` → PASS.
- [ ] **Step 4:** `just verify` → PASS. **Commit** — `git add -A && git commit -m "test(cli): process-level chat tests, including SIGINT mid-turn"`

---

### Task 11: BDD coverage, then the README/ARCHITECTURE/spec sweep

**Files:**
- Create: `tests/features/chat.feature`
- Modify: `crates/forge-cli/tests/bdd/world.rs` (a stdin-carrying variant of `run_forge`), `crates/forge-cli/tests/bdd/steps.rs`
- Modify: `README.md`, `ARCHITECTURE.md`, `docs/superpowers/specs/2026-09-24-interactive-chat-ui-design.md` (closeout amendment), `docs/superpowers/specs/2026-09-23-needle-embedded-brain-design.md` (§2 item 6 Phase B status)

- [ ] **Step 1: Write the feature file**, in the style of the existing ones:

```gherkin
Feature: Interactive chat
  `forge` with no subcommand is a conversation: one session across turns,
  slash commands, inline approvals, and forking — over the same runtime
  every other front end uses.

  Scenario: A conversation keeps one session across turns
    Given an initialized project with a mock model
    When I chat with the lines "explain this project" and "and again"
    Then the chat exits successfully
    And one session holds both runs

  Scenario: Slash commands answer without naming a mock
    Given an initialized project with a mock model
    When I chat with the lines "/help" and "/model"
    Then the chat output lists the chat commands
    And the chat output never mentions a mock

  Scenario: An approval denied in the chat leaves the file unwritten
    Given an initialized project with a scripted mock model that writes "notes.txt"
    And approval mode "prompt"
    When I chat with the lines "write the notes" and "n"
    Then the file "notes.txt" does not exist
    And the session events include an approval decision that was denied

  Scenario: Forking from the chat creates a second session
    Given an initialized project with a mock model
    When I chat with the lines "first" and "/fork" and "second"
    Then two sessions exist
    And the chat output says the source session is untouched
```

- [ ] **Step 2: Run** `just bdd` → the new scenarios FAIL (no steps).
- [ ] **Step 3: Implement the steps.** `world.rs` needs `run_forge_with_stdin(&mut self, args: &[&str], lines: &[&str])`: identical to `run_forge` (same scrub, same hermetic HOME/XDG, same `FORGE_TEST_MOCKS`) except `Stdio::piped()` for stdin, writing each line and then dropping the handle. Keep `run_forge` unchanged so no existing scenario's behaviour moves. The session assertions parse `.forge/sessions/*.jsonl` exactly as the existing steps do.
- [ ] **Step 4: Run** `just bdd` → PASS.
- [ ] **Step 5: README.** A new `## Interactive chat` section after `## Usage`, describing today's behavior only: `forge` / `forge chat` / `forge chat --continue` / `forge chat --session <id>`; the slash-command table; the keybinding table including the complete Ctrl-C rule; approvals inline; `/bg`, `/jobs`, `/attach` and the explicit "background is this process only, and closing the terminal ends it"; `/fork`; piped stdin; `NO_COLOR`/dumb terminals; `--json` refused. Add `forge chat` (and the bare `forge`) to the **Command line** block. Add to **Known limitations**: no token-by-token streaming in the chat (the loop returns final text); background jobs do not survive the process and cannot be attached from another one; `/name` skill invocation relies on lexical skill matching; no path completion.
- [ ] **Step 6: ARCHITECTURE.md.** Update "Where it runs"/"Editors and harnesses" to name five front ends, and add a paragraph: the chat is the fourth adapter over `AgentService`; its pure half (`forge-chat`) is the same split as `forge-acp::dispatch`; the editor thread is `NeedleEngine`'s pattern; the parked approval channel is now explicit for every stdin-owning front end; the turn driver is `run_turn`'s ordering.
- [ ] **Step 7: Specs.** In the Phase B design doc, append a short **Amendment (implemented …)** recording anything that turned out differently from this plan (the measured dependency count as it landed, any keybinding that behaved differently on a real terminal, and whichever of §14's follow-ups moved). In the needle spec's §2 item 6, change the Phase B sentence to point at the shipped state.
- [ ] **Step 8:** `just verify` → PASS. **Commit** — `git add -A && git commit -m "test(bdd): chat scenarios; README/ARCHITECTURE/spec sweep"`

---

## Self-Review

- **Spec coverage:** §2 crate stack → Task 8 (dependency, features, the cost comment); §3 architecture and seams → Tasks 2, 7, 9; §4 rendering → Tasks 3, 4; §5 turn driver → Task 7; §6 input/keys/signals → Tasks 5, 8, 10; §7 session continuity → Tasks 1, 7, 9; §8 approvals → Tasks 6, 7, 10; §9 slash commands → Tasks 5, 9; §10 background/reattach → Tasks 5, 7; §11 fork → Tasks 5, 7, 11; §12 terminal reality → Tasks 8, 9, 1 (`--json`); §13 testing → every task plus 10 and 11; §14 follow-ups → Task 11's README/known-limitations sweep.
- **Review Focus, all pinned:** 1 → Task 5 table + Task 10 real SIGINT; 2 → Task 5 + Task 7 (file not written); 3 → Task 6; 4 → Task 8 validator + Task 4 multi-line parse; 5 → Tasks 3 and 4; 6 → Task 8 (documented, with the `TCSADRAIN` evidence) + Task 10 ordering; 7 → Tasks 7 and 10; 8 → Tasks 9 and 10.
- **Placeholder scan:** Task 1's `chat_cmd` stub is replaced in Task 9 with the signature fixed in Task 1 — a deliberate two-phase, not a placeholder module (the same shape as the needle plan's `engine_from_config`). `ServiceOptions` gains no field it does not use. No task ends without a green `just verify` and an independently observable deliverable: after Task 1 `forge` opens (and exits); after Task 4 the renderer is complete and tested; after Task 6 the parked channel is usable by MCP/ACP as well; after Task 7 the whole loop is provable in-process; after Task 9 a human can hold a conversation; after Task 10 the interrupt guarantee is machine-checked.
- **Type consistency:** `Line`/`Style` (2 → 4, 7, 8), `ChatIo`/`ReadOutcome`/`Prompt` (2 → 7, 8), `ChatHost`/`HostChange`/`Environment` (2 → 7, 9), `tool_arg_field` (3 → 4 and `forge-acp`), `TranscriptState` (4 → 7), `Parsed`/`Action`/`Signal`/`CompletionSnapshot` (5 → 7, 8), `ApprovalChannel`/`ServiceOptions` (6 → 9), `Start`/`SessionStart` (7 → 9) all checked for one spelling across their producing and consuming tasks.
- **Ordering:** Tasks 1–6 are independent of each other except that 4 needs 3 and 5 needs 2; 7 needs 2, 4, 5; 8 needs 2; 9 needs 6, 7, 8; 10 needs 9; 11 needs 10. Nothing later edits an earlier task's tests to make itself pass.
