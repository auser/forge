# Interactive Chat UI (Phase B) — Design

Date: 2026-09-24
Status: Approved design, pending implementation plan
Scope: Sub-project 6b of the Needle/Jev/editor-integration program —
Phase B of §2 item 6 in
[`2026-09-23-needle-embedded-brain-design.md`](2026-09-23-needle-embedded-brain-design.md)

## 1. Intent

`forge` with no subcommand opens an interactive conversation in the
terminal you are already looking at: a scrolling transcript in normal
scrollback, a rich input line at the bottom, slash commands with Tab
completion, discovered skills as `/name`, inline approvals, and the
on-device fast path visibly landing instantly. It is the fourth front end
over the one `AgentService` — after the CLI, `forge serve`, `forge mcp`
and `forge acp` — and it adds **no** new runtime capability. Everything it
needs already exists: Phase A shipped replay, fork, `attach`/`list_runs`
and `RunState`; this spec is about the *interface*.

Two constraints frame every decision below.

**It must not contradict the README.** The README promises a single
binary, stdout discipline, approval gating, sessions as conversations,
and that mocks are invisible. The chat is held to all of them.

**It must be boring where it can be.** The three existing adapters
already solved event→status mapping, approval round-trips and
cancel-racing-a-turn. Where they did, this design copies them and says so
rather than inventing a fourth answer.

### Decisions taken as given (not re-litigated here)

- **Inline transcript, not a full-screen TUI.** No alternate screen, no
  redraw of scrolled-away content, no layout engine. The feel is Claude
  Code / Kimi Code: output scrolls, the input line is rich.
- **`forge` with no args opens the chat**; `forge chat` is the explicit
  alias. Every existing subcommand keeps working unchanged.
- **Slash commands**: `/model /config /skills /graph /session /approval
  /fork /bg /jobs /attach /help /quit`, plus every discovered skill as
  `/name`. Tab completes all of them.
- **Mocks stay invisible.** `mock-local`, `scripted-mock`, `router =
  "mock"` and `execution = "mock"` are test-only, gated behind
  `FORGE_TEST_MOCKS=1`, and no chat surface — `/model`, `/config`,
  `/help`, the banner, an error message — may name them.
- **No fake token streaming.** The agent loop produces final text. The
  chat renders what genuinely arrives live (routing decisions, skill
  activation, tool-call lifecycle, file changes, per-turn assistant
  messages) and never chops finished text into a pretend stream. Real
  token streaming is recorded as a follow-up (§14).

## 2. The crate stack, and what it costs

### 2.1 What the input line has to do

Non-negotiable, because a REPL without them is worse than no REPL:
cursor/word editing and kill-ring, persistent history with reverse
search, **Tab completion via a callback** (commands and skills are
discovered at runtime, so a static list is useless), **multi-line input**
(a pasted code block must not become five turns), typed `Ctrl-C` and
`Ctrl-D` as *distinct* outcomes, and no alternate screen.

### 2.2 The candidates, measured

The workspace has no TUI or line-editor dependency today, so this is a
real addition and is costed the way `rmcp` (accepted, 13 crates) and
`agent-client-protocol` (rejected, 52 crates) were costed: resolve it
alone, diff the lockfile against this workspace's 300 packages, count
what is genuinely new.

| candidate | version | packages in its tree | **new to this workspace** | verdict |
| --- | --- | --- | --- | --- |
| `rustyline` (`default-features = false`, features `custom-bindings`, `with-file-history`) | 18.0.1 | 18 | **8** | **chosen** |
| `reedline` (default features) | 0.51.0 | 93 | **23** | rejected on cost |
| `ratatui` + a hand-written editor | — | — | 20+ | rejected on shape *and* cost |
| hand-rolled over `crossterm` | — | — | 5–6 | rejected on scope |

Measured 2026-09-24 by `cargo add` into an empty crate and
`comm`-diffing the `name =` lines of the two lockfiles.

The 8 new packages for rustyline: `rustyline`, `nix`, `unicode-segmentation`,
`radix_trie`, `nibble_vec`, `endian-type`, plus `clipboard-win` and
`error-code` (**Windows-only**, pulled by the clipboard integration). On
unix the real addition is 6, of which 3 (`radix_trie`, `nibble_vec`,
`endian-type`) are the tiny trie behind `custom-bindings`. Eleven more of
its deps — `bitflags`, `cfg-if`, `cfg_aliases`, `libc`, `log`, `memchr`,
`smallvec`, `unicode-width`, `utf8parse`, `windows-link`, `windows-sys` —
are already in `Cargo.lock`.

`reedline` is the richer editor and would be defensible on features
alone. What it costs is a second terminal stack (`crossterm`,
`crossterm_winapi`, `winapi`) beside nothing we have, a second
signal-handling stack (`signal-hook`, `signal-hook-mio`) beside tokio's
`signal` feature which is already enabled, `strum`/`derive_more`
proc-macro trees, and another `syn`. That is the same shape of argument
that rejected the ACP SDK, and it is rejected the same way: not on
capability, on cost.

`ratatui` is rejected first on shape — it is built for the full-screen,
owned-viewport application this explicitly is not — and only then on
weight.

Hand-rolling over `crossterm` would be cheaper in crates and much more
expensive in code: history search, kill-ring, unicode width and grapheme
handling, bracketed paste, and per-terminal key decoding are exactly the
things a line editor exists to own.

### 2.3 Why `rustyline` specifically, verified against its source

Each claim below was checked in `rustyline-18.0.1`'s source, not
inferred from prose, because the keybinding semantics are the part of
this design most likely to be wrong in a plausible-sounding way.

| requirement | how rustyline satisfies it |
| --- | --- |
| stable toolchain, no build script | `edition = "2021"`, `build = false`; no `bindgen`, no proc-macros unless the `derive` feature is on (it is off) |
| inline, no alternate screen | never emits `smcup`/`1049`; it draws only the prompt line(s) |
| Tab completion callback | `Completer::complete(&self, line, pos, ctx) -> Result<(usize, Vec<Candidate>)>`; `String` implements `Candidate` |
| multi-line input | `Enter` binds to `Cmd::AcceptOrInsertLine { accept_in_the_middle: true }` (`keymap.rs:1059`), which inserts a newline instead of submitting when the `Validator` returns `Incomplete` |
| explicit newline key | `custom-bindings` → `Editor::bind_sequence(Alt-Enter, Cmd::Newline)` |
| history persistence | `load_history`, `append_history`, `save_history` (feature `with-file-history`) |
| `Ctrl-C` distinct from EOF | `Cmd::Interrupt` → `ReadlineError::Interrupted` (`keymap.rs:1034`) |
| `Ctrl-C` that can tell a typed line from an empty one | `ReadlineError::Interrupted` carries no buffer, so the distinction §6.3 needs is made *inside* the editor: `custom-bindings` allows `EventHandler::Conditional`, whose `handle(.., ctx)` sees `ctx.line()` (`binding.rs:181`) and returns `Cmd::Kill(Movement::WholeBuffer)` for a non-empty line and `Cmd::Interrupt` for an empty one. This is the second reason `custom-bindings` is paid for |
| `Ctrl-D` = EOF | on unix it is the **termios `VEOF` character**, mapped to `Cmd::EndOfFile` (`tty/unix.rs:1641`), which returns `ReadlineError::Eof` only when the line is empty (`command.rs:71`) — so it honours a user's `stty` and never discards a half-typed line |
| type-ahead survives | raw mode is entered with `SetArg::TCSADRAIN` (`tty/unix.rs:1646`), which preserves queued input; `TCSAFLUSH` would have discarded what the user typed during a turn |
| paste is one input | bracketed paste is on by default on unix (`config.rs:266`) |
| out-of-band printing | `Editor::create_external_printer()` prints a line from another thread without corrupting the prompt (`lib.rs:930`) |
| width, if ever needed | `Editor::dimensions()`; this design deliberately does not need it (§12.1) |

One behaviour is deliberately **not** used: rustyline falls back to
`readline_direct` when stdin is not a TTY. The chat decides non-TTY mode
itself (§12.3) so that piped behaviour is forge's, specified and tested,
rather than an internal of a dependency.

### 2.4 Nothing else is added

No styling crate: the palette is a handful of hand-written ANSI escapes
(`\x1b[2m`, `\x1b[31m`, `\x1b[32m`, `\x1b[33m`, `\x1b[36m`, `\x1b[0m`)
behind one `Palette` type, disabled as a unit (§12.2). No `terminal_size`
(§12.1). No pty crate in dev-dependencies (§13.4).

## 3. Architecture

```
crates/forge-chat          pure: no terminal, no rustyline, no I/O syscalls
  command.rs               slash parsing + Tab completion            (pure)
  render.rs                Event -> Vec<Line>, + TranscriptState     (pure)
  controller.rs            input/signal -> Vec<Action> state machine (pure)
  app.rs                   the async driver: ChatIo x ChatHost x AgentService
  io.rs                    the ChatIo trait + Line/Style/Prompt types
  host.rs                  the ChatHost trait + its plain-data types

crates/forge-cli
  src/chat/terminal_io.rs  ChatIo over rustyline (the only rustyline user)
  src/chat/host.rs         ChatHost over the existing config/service plumbing
  src/commands/chat_cmd.rs the subcommand entry point
```

This is `forge-acp`'s split, applied again: **`dispatch.rs` is pure and
`server.rs` owns the I/O**, which is why the whole forge→ACP mapping is
unit-testable without a process. Here the pure half is three modules
instead of one, and the I/O half is additionally *outside the crate* —
`forge-chat` does not depend on `rustyline` at all, so it cannot
accidentally grow a terminal dependency, and `cargo test -p forge-chat`
can never need a TTY.

`forge-chat`'s dependencies: `forge-core`, `forge-runtime`,
`forge-session`, `async-trait`, `serde_json`, `thiserror`, `tokio`,
`tracing`. Dev: `forge-config`, `forge-execution`, `forge-providers`,
`tempfile`.

### 3.1 The two seams

Both are the same idea as `forge-mcp`'s `Diagnostics` and `forge-acp`'s
`ServiceFactory`: the adapter declares what it needs; `forge-cli`, the
only crate that can see config resolution, provider construction and
credential detection, implements it.

```rust
/// Everything the chat needs from the terminal. One implementation over
/// rustyline (TTY), one over plain stdin (piped), one scripted (tests).
#[async_trait]
pub trait ChatIo: Send {
    /// Ask for one line of input. Returns the typed outcome, never a raw
    /// error string.
    async fn read(&mut self, prompt: Prompt) -> ReadOutcome;
    /// A transcript line (stdout).
    fn write(&mut self, line: &Line);
    /// An out-of-band line (a background job's state change) that may
    /// arrive while a prompt is on screen.
    fn notify(&mut self, line: &Line);
    /// Resolves when the user interrupts *without* it arriving as a read
    /// outcome: `SIGINT` in piped mode, and a channel the test fires in
    /// tests. On a TTY a typed Ctrl-C normally arrives as
    /// `ReadOutcome::Interrupt` instead (§6.2); both funnel into one pure
    /// `Controller::on_signal`, which is what makes "Ctrl-C cancels the
    /// turn" testable with no terminal and no signal.
    async fn interrupted(&mut self);
    /// Can this io ask the user a follow-up question? A terminal can (so
    /// an exit with a live job is confirmed); piped stdin cannot (so EOF
    /// drains the queue and leaves). §12.3.
    fn interactivity(&self) -> Interactivity;
    /// Flush and restore the terminal. Called once, on the way out.
    fn shutdown(&mut self);
}

pub enum ReadOutcome {
    Line(String),
    /// Ctrl-C at the prompt (rustyline `Interrupted`).
    Interrupt,
    /// Ctrl-D on an empty line, or stdin EOF.
    Eof,
    /// The editor itself failed; the message is already user-facing.
    Failed(String),
}

/// The prompt to draw, plus the completion candidates *as data* — so
/// completion needs no callback back into the host, and the completer is
/// a pure function of a snapshot.
pub struct Prompt {
    /// Always `"> "` today: the approval question is a transcript line,
    /// not a second prompt (§8), so nothing swaps this mid-turn.
    pub text: String,
    /// Candidates as data (commands, skills, model names, job ids,
    /// session ids), so the completer is a pure function of a snapshot and
    /// never calls back into the host from the editor thread.
    pub completions: CompletionSnapshot,
}
```

```rust
/// Everything the chat needs from forge that is not the runtime itself.
#[async_trait]
pub trait ChatHost: Send + Sync {
    fn service(&self) -> Arc<AgentService>;
    /// Rebuild the runtime with one setting overridden. On error the old
    /// runtime is kept and the error is returned unchanged.
    async fn switch(&mut self, change: HostChange) -> Result<(), ForgeError>;
    fn environment(&self) -> Environment;
    /// User-visible model candidates only: this is where mock entries are
    /// filtered out, in one place (§9.3).
    fn models(&self) -> Vec<ModelChoice>;
    fn skills(&self) -> Vec<SkillChoice>;
    /// `forge config show`/`explain` data: key, value, origin.
    fn config_summary(&self, key: Option<&str>) -> Vec<ConfigLine>;
    fn graph_context(&self, query: &str, limit: usize) -> Result<Vec<ContextLine>, ForgeError>;
}

pub enum HostChange { Model(String), Approval(String) }

pub struct Environment {
    pub project_root: PathBuf,
    pub model: String,
    pub router: String,
    pub approval: String,
    pub needle: NeedleState,   // Active { model_id } | Inactive { reason }
}
```

Every method returns plain data, so a `FakeHost` in `forge-chat`'s own
tests drives the entire loop with no config files, no terminal, and a
runtime built from mock providers.

## 4. Events → transcript

`render.rs` is the counterpart of `forge-acp::dispatch`: synchronous,
side-effect free, given `&Event` returns `Vec<Line>`.

```rust
pub struct Line { pub style: Style, pub text: String }
pub enum Style { Plain, Meta, Tool, Ok, Warn, Bad, Notice }
```

Styles are *semantic*. Colour is applied by the writer (§12.2), so every
render test asserts on `text` and is unaffected by palette changes.

### 4.1 The visual grammar

ASCII only — no box drawing, no arrows, no emoji. A Windows console, a
`TERM=dumb` session, a CI log and a `NO_COLOR` terminal all render
identically, and no glyph can be replaced by a `?`.

| class | prefix | style | example |
| --- | --- | --- | --- |
| user line, piped mode only (§12.3) | `> ` | Plain | `> explain the parser` |
| meta: routing, skills, session notes | `  - ` | Meta | `  - routing: needle -> qwen3-coder (conf 0.91)` |
| tool requested | `  * ` | Tool | `  * read_file src/main.rs` |
| tool result / file change | `    -> ` | Ok/Bad | `    -> ok (12 ms)` |
| approval question | `  ! ` | Warn | `  ! approval needed: write notes.txt (risky)` |
| assistant text | none, blank line above and below | Plain | the model's answer, verbatim |
| error, cancellation | `  ! ` | Bad | `  ! error: model endpoint ... returned 401 ...` |
| turn footer | `  = ` | Meta | `  = 2 turns, 3 tool calls, 4.2s` |
| background notice | `  # ` | Notice | `  # job 01JC... finished` |

### 4.2 The mapping, exhaustively

Every `EventKind` in the v3 schema is accounted for. "nothing" is a
decision, not an omission — the same discipline `forge-acp::dispatch`
applies.

| `EventKind` | rendered |
| --- | --- |
| `RunStarted` | nothing (the user just typed it; in piped mode the echo already showed it) |
| `RoutingDecisionMade` | `  - routing: {router} -> {model} (conf {c:.2})`, `+ " [fallback]"` when `fallback_used`, `+ " - {reason}"` when the reason is non-empty. **Special case:** `router == "needle-dispatch"` renders `  - on-device: needle called {tool} directly (conf {c:.2}, no model call)` — the fast path is the product's headline claim and must read as itself, not as a routing line |
| `SkillActivated` | `  - skill: {name}` |
| `ToolCallRequested` | `  * {tool} {summary}` where `summary` is the friendly argument (§4.3) |
| `ToolStarted` | nothing; it always follows `ToolCallRequested` for the same call, and a second line per call buys nothing. The *timer* starts here |
| `ToolCompleted` | `    -> ok ({ms} ms)` / `    -> failed ({ms} ms)` |
| `FileChanged` | `    -> wrote {path}` |
| `ApprovalRequested` | `  ! approval needed: {command} ({risk}) - y to approve, anything else denies`, and the driver enters `AwaitingApproval` (§8) |
| `ApprovalDecided` | `    -> approved` / `    -> denied` |
| `AssistantMessage` | the `text`, as a Plain block with blank lines around it, when non-empty; the `tool_calls` are already narrated by the `tool_*` events, so they add nothing |
| `ToolResult` | nothing. It is the replay record of what the *model* saw, capped at 64 KiB; dumping it would bury the transcript. The one-line `ToolCompleted` result is the user-facing summary, and `forge session show` is where the payload lives |
| `TurnCompleted` | nothing at default verbosity; `  - turn {n} complete` at `-v` |
| `InputReceived` | nothing — the user is the one who sent it |
| `Note` | `  - {text}` (v1 compatibility) |
| `SessionForked` | `  - forked from {from_session} at position {at}` |
| `Error` | `  ! error: {message}` (forge's messages already carry their hints) |
| `Cancelled` | `  ! cancelled` |
| `Completed` | nothing. Its `summary` is truncated to 80 characters by the store; the answer comes from `AssistantMessage` or the run outcome. This is exactly why `forge-acp` ignores it too |

### 4.3 Two rules that are easy to get wrong

**The final answer is printed exactly once.** `TranscriptState` records
whether any non-empty `AssistantMessage` text was rendered for the run.
After the run settles, the driver prints `RunOutcome.text` **only if
nothing textual was rendered**. That covers the fast path (which emits an
`AssistantMessage` with empty text and a tool call, so the tool's output
is printed once, from the outcome) without double-printing an ordinary
turn (whose last `AssistantMessage` *is* the answer, already on screen).
`TranscriptState::rendered_assistant_text() -> bool` is public so a unit
test asserts the rule directly.

**Argument summaries are read, not parsed hopefully.** The event carries
`args_summary`: the call's JSON arguments truncated to 120 characters,
which cuts the most interesting call (`write_file` with real content)
mid-string. `forge-acp` already solved this with an escape-aware,
truncation-tolerant field scan. That scan moves into
`forge_core::tool_arg_field(summary, key) -> Option<String>`, `forge-acp`
delegates to it, and `forge-chat` calls it: one definition of "read a
field out of a truncated args summary", rather than two that will
diverge. The chat's friendly form (`read_file src/main.rs`,
`run_command cargo test`, `graph_grep "parse"`) is its own — ACP's titles
are imperative editor labels tied to absolute `ToolCallLocation` paths —
but the fragile part is shared. A field that cannot be recovered falls
back to the summary, ellipsized to 60 characters.

## 5. The turn driver

One turn is one run in the current session. Copied from
`forge-acp::server::run_turn`, including the ordering comments, because
the ordering is the part that is load-bearing:

1. `run_id = new_run_id()`.
2. `let mut events = service.subscribe(&run_id);` — **before** starting,
   so an approval request from a fast first tool call cannot be emitted
   with nobody listening.
3. `service.start_run_with_options(prompt, RunOptions { run_id, session_id: Some(current), max_turns: None })`.
4. `select!` over `events.recv()`, the `JoinHandle`, and two arms the ACP
   driver does not have: the outstanding `io.read(...)` (§6.2 — the user
   can type during a turn, which is what makes `/bg` reachable) and
   `io.interrupted()`.
   - `Lagged(n)`: warn and continue. The final text does not come from the
     event stream, so lagging degrades the transcript, never the answer.
   - `Closed`: no more events; await the handle.
   - a line arrives: hand it to the pure `Controller`, which applies the
     §6.5 table — answer a pending approval, act on a during-turn command,
     refuse a runtime-rebuilding one, or queue a prompt as the next turn.
     Then issue the next read.
5. When the run settles, drain `events.try_recv()` so the last tool's
   result line precedes the answer.
6. Print the answer (§4.3), then the footer, then submit the first queued
   prompt if there is one. The queue is FIFO and unbounded: what the user
   typed is never dropped.

**One live run per session, and the runtime is what enforces it.** Two
runs writing into one session concurrently corrupt the *next* turn's
replayed history in a way no error reports (§10.1.1). That guard belongs
in `AgentService`, not here — it is being added there as a typed refusal
of a second *concurrent* run on a live session (sequential reuse of a
session id is unchanged and still the normal case). The chat's job is
therefore not to police it but to avoid provoking it: `/bg` moves the
foreground conversation into a fork, so the detached run keeps its session
and the next thing you type lands somewhere else. If the runtime's refusal
ever does surface — a race, or another process — it arrives as a typed
`ForgeError` and is rendered as one `  ! error:` line like any other
failed turn (§6.3).

**Naming the session is what makes history real.** Phase A made every
entry point that names an existing session continue it: the runtime
replays `assistant_message`/`tool_result` records into the model's
`messages`, fitted to a budget from `max_context`. The chat therefore
needs *no* conversation state of its own — it passes the same
`session_id` every turn, and the model's memory is the session log. The
chat keeps exactly two things in memory: the session id and the id of the
run it is attached to.

**Cancellation** is `service.cancel(&run_id)` and then *awaiting the
handle*, not aborting the task: the loop polls the token at every
turn/tool checkpoint and at the approval wait, and cancelling it from
underneath would rob it of the chance to record its own `Cancelled`
event. If the handle has not settled 2 seconds after the token fired, the
driver prints `  ! cancelled (the turn is still unwinding)` and returns
to the prompt anyway; the run's own events keep being written to the log.
A second `Ctrl-C` in that window does not escalate — there is nothing
left to cancel — it prints the exit hint (§6.3).

## 6. Input, keys, signals

### 6.1 The editor lives on its own thread

`rustyline::Editor` is synchronous, blocking, and not `Send`-friendly
across an await. So one dedicated OS thread owns it for the life of the
session, fed by an mpsc channel of prompt requests and replying on
oneshots — **the same pattern `NeedleEngine` uses for the FFI handle**,
for the same reason: a non-async, non-shareable resource is easier to
reason about behind a channel than behind a mutex. The async side never
blocks, so the event stream and the signal handler keep being polled.

That thread also owns the history file (`.forge/chat-history`, capped at
1000 entries, `append_history` after every accepted line so a crash does
not lose the session, failures logged at debug and never fatal). The file
is as sensitive as `.forge/sessions/` — it is what the user typed — and
is covered by the same gitignored `.forge/` directory.

### 6.2 The prompt is up *during* a turn, too

This is the structural decision the rest of the chapter depends on, and
getting it wrong would make `/bg` unreachable: a read is outstanding
**almost all the time**, including while a turn is running. Otherwise
there would be no moment at which a user could type `/bg`, `/jobs` or
`/attach` — the very commands whose reason to exist is a turn that is
taking too long.

So the driver always has one `io.read(...)` in flight, and while a turn is
running the transcript's lines are emitted through `ChatIo::notify` —
rustyline's `ExternalPrinter`, whose entire purpose is printing a line
from elsewhere while a prompt is displayed, redrawing the prompt
afterwards. Lines printed between turns go through `write` (plain stdout).
Both take the same `Line`, so the transcript is byte-identical either way;
only the mechanism differs.

Two consequences:

- **Raw mode is effectively always on** on a TTY, so `Ctrl-C` is a *byte*
  and reaches the conditional binding of §2.3 (buffer non-empty → clear
  it; empty → `Cmd::Interrupt` → `ReadOutcome::Interrupt`). `SIGINT` is
  not normally generated at all.
- **`ChatIo::interrupted()` is still required**, for the two contexts
  where no editor is reading bytes: piped/non-TTY mode (where a terminal's
  `Ctrl-C` reaches the process as `SIGINT` through the process group) and
  tests. The terminal implementation therefore *also* watches
  `tokio::signal::ctrl_c()`, so a `SIGINT` from `kill` behaves like a
  typed `Ctrl-C`.

Either delivery funnels into the same pure
`Controller::on_signal(Signal::Interrupt)`, which is exhaustively
unit-tested, so the paths cannot drift.

### 6.3 Ctrl-C, completely specified

The single most irritating REPL bug is Ctrl-C doing the wrong one of
"cancel" and "quit", so this is a table, not prose.

The first row wins over the rest: the editor resolves it before the chat is
told anything.

| state | `Ctrl-C` does | exits? |
| --- | --- | --- |
| **input line non-empty**, in any state | clears the input line. The chat never sees it — the conditional binding turns it into `Cmd::Kill(WholeBuffer)` inside the editor (§2.3) | no |
| empty line, turn running | cancels the turn (§5), prints `  ! cancelled`, returns to the prompt | no |
| empty line, turn running, approval pending | cancels the turn; the pending operation **does not run** (the loop's approval wait selects on the token), prints `  ! cancelled - the operation was not run` | no |
| empty line, idle | prints `(press Ctrl-C again, or Ctrl-D, or /quit, to exit)` | no |
| empty line, idle, **within 2s of the previous one** | requests an exit (§10.2's confirmation still applies if a job is live) | yes, code 130 |
| empty line, idle, only detached jobs live (§10) | nothing is attached, so there is nothing to cancel: the two rows above apply. `/attach <id>` then Ctrl-C is how you cancel a background run | as above |

Submitting a line resets the arming, so "clear a line, then interrupt
once" can never exit.

`Ctrl-D` on an empty line requests an exit in every state — a prompt is up
during a turn too (§6.2) — and exits with code 0. (Piped stdin's EOF is a
different thing wearing the same name: it means "no more input", drains the
queue, and cannot be confirmed with anybody — see §12.3.) `/quit` and `/exit`
exit 0. **Exiting is one action, however it was requested**, so the
live-job confirmation in §10.2 applies identically to `Ctrl-D`, `/quit`
and the second `Ctrl-C`: the first request prints the warning, the next
one abandons the jobs and leaves. Nothing else exits: **a failed turn
never ends the chat** — a bad model endpoint, a denied approval, an
unreadable session log all print a `  ! error:` line and return to the
prompt.

Mid-approval Ctrl-C deliberately does *not* send `"n"` as well as
cancelling. The runtime's `await_approval` already selects on the
cancellation token, so cancelling alone unblocks it with
`ForgeError::Cancelled` and the tool is never dispatched. Sending a
denial too would put an unordered answer in a channel that a *later*
approval in the same run could consume if cancellation lost the race.
(This is the one place the chat differs from `forge-acp`, which *does*
send `"n"` on a cancelled permission request — correctly, because there
the client cancelled the *request*, not the turn.)

### 6.4 The rest of the keys

| key | effect |
| --- | --- |
| `Tab` | complete a slash command, a skill name, or a `/attach` job id (§9.2) |
| `Enter` | submit, unless the `Validator` says the input is incomplete, in which case it inserts a newline |
| `Alt-Enter` | always insert a newline (bound explicitly; `Shift-Enter` is undetectable in most terminals, so it is not offered) |
| `Up`/`Down`, `Ctrl-R` | history, reverse search (rustyline defaults) |
| `Ctrl-A/E/K/U/W`, `Alt-B/F`, … | emacs editing (rustyline defaults, unmodified) |
| `Ctrl-L` | clear the screen (rustyline default). It clears the *screen*, not the session; the transcript is still in scrollback and the conversation is untouched |

Input is **incomplete** — so `Enter` continues it — when it ends in a
single backslash, or when it contains an odd number of ```` ``` ```` fences.
Pasting a fenced code block is therefore one turn, and bracketed paste
(on by default) means a multi-line paste is one edit rather than N
submissions.

### 6.5 Typing during a turn

Because a read is outstanding during a turn (§6.2), typing while forge
works goes to the line editor — with full editing, completion and history —
and the transcript keeps scrolling above it. What happens on `Enter`
depends only on what was typed:

| typed during a turn | effect |
| --- | --- |
| a prompt | queued; submitted as the next turn when this one ends (FIFO, never dropped) |
| `/bg`, `/jobs`, `/attach`, `/help`, `/config`, `/session`, `/skills`, `/graph`, `/quit` | acted on immediately |
| `/model`, `/approval`, `/fork` | refused, with the reason and the way forward (§9.1, §11) |
| `y` / `n` while an approval is pending | the approval answer (§8) |

rustyline additionally enters raw mode with `TCSADRAIN` (verified, §2.3),
so even input typed in the gap between two reads is preserved rather than
flushed. forge **never discards** what you typed.

## 7. Session start and continuity

`forge` / `forge chat` starts a **fresh session**. It does not silently
resume: a prompt landing in an existing session replays that
conversation into the model's context, which is exactly right when you
asked for it and exactly wrong when you did not (an unrelated
conversation, silently paid for in context).

```text
forge                       # fresh session
forge chat                  # the same thing, spelled out
forge chat "fix the build"  # fresh session, first turn pre-filled, then interactive
forge chat --continue       # the project's most recently active session
forge chat --session <id>   # a named session
```

When a session is resumed (`--continue`, `--session`, `/session <id>`),
the chat **re-renders its transcript** from `sessions.events_for(id)`
through the same `render.rs` used live, under a `  - resumed session
<id> (<n> runs)` header. Two properties follow: what you see above the
prompt is the same history the model is about to be given, and there is
exactly one renderer, so a resumed transcript cannot look different from
a live one. History older than the last 200 rendered lines is summarised
as `  - ... <n> earlier lines (forge session show <id>)` rather than
flooding the scrollback.

The banner, once, on entry:

```text
forge 0.1.0  ~/code/myproject
model qwen3-coder  router needle  approval prompt  brain active
session 01JCF3...  /help for commands
```

`brain active` / `brain off (static routing)` comes from
`Environment::needle`, phrased with the same vocabulary `forge doctor`
uses so two surfaces cannot tell a user different stories.

## 8. Approvals

Under `approval = "prompt"` (the default), a risky tool call stops and
asks, in the transcript, on the operation it belongs to:

```text
  * write_file notes.txt
  ! approval needed: write notes.txt (risky) - y to approve, anything else denies
> y
    -> approved
    -> ok (3 ms)
```

- The question is a **transcript line**, and the answer is typed at the
  normal prompt — which is already up (§6.2). Nothing swaps the prompt
  text mid-flight, nothing cancels an outstanding read, and the answer
  path is the one the chat uses for everything else. The question line
  carries the whole contract, so there is no hidden convention.
- `y`, `yes` → approve. Anything else, including an empty line → deny.
  The comparison is the runtime's, not a second one: the chat sends
  `send_input(run_id, "y")` or `"n"` and the loop's existing
  `await_approval` interprets it (`y`/`yes`/`approve` approve, everything
  else denies) and records `approval_decided`.
- A line that is a *slash command* while an approval is pending is acted
  on per §6.5 and the approval stays pending — so `/jobs` does not
  accidentally deny a write. Only a non-command line answers.
- A denial is not an error: the loop turns it into a tool result of
  `approval denied`, the model sees it, and the turn continues.
- Because a prompt is up, `Ctrl-C` here arrives as `Interrupt` from the
  editor rather than as a signal, which is what makes the mid-approval
  cancellation of §6.3 one code path rather than two.

### 8.1 This is the existing mechanism, with one gap closed

`forge mcp` and `forge acp` both park the run (`ApprovalRequested` →
answer → `send_input`) because stdin is their protocol channel. The chat
does the same for a different reason: stdin is *its* channel too, owned
by the line editor.

Today `NativeExecution::prompt_for_approval` writes `approve …? [y/N]`
to stderr and calls `std::io::stdin().lock().read_line()` **whenever
stdin is a TTY**. Inside a chat that is a genuine bug, not a preference:
two readers on one file descriptor, one of them in raw mode on another
thread. The prompt would be drawn by the wrong writer, the answer might
be consumed by the editor, and `ApprovalRequested`/`ApprovalDecided`
would never be emitted, so the transcript would not show what was asked.

So the approval channel becomes explicit rather than inferred from
`is_terminal()`:

```rust
pub enum ApprovalChannel {
    /// Prompt inline on the terminal when stdin is a TTY, park otherwise.
    /// Today's behaviour, unchanged, and the default.
    InlineTty,
    /// Never prompt: always return `ForgeError::ApprovalRequired` and let
    /// the caller answer through the run's input channel.
    Parked,
}
```

`NativeExecution::new` keeps today's semantics (`InlineTty`); the chat
builds its runtime with `Parked`. `forge run`, `forge serve`,
`forge mcp`, `forge acp`, `forge skill test` and `forge router serve` are
untouched — MCP and ACP already get parking for free from their non-TTY
stdin, and now they get it by construction as well if they ever run on
one.

### 8.2 A note on the fast path

The needle direct-dispatch fast path is read-only by construction and
therefore cannot produce an approval prompt (gate 5,
`minimum_dispatch_risk == Safe`). The chat inherits that: an instant
on-device answer never interrupts you. This is worth stating because it
is the only place in forge where a tool runs before the transcript
mentions it — the fast path emits its `tool_*` events just after the
work rather than just before, so in the chat a fast-path read appears as
a complete, already-finished block.

## 9. Slash commands

### 9.1 The surface

Parsing is pure: `Command::parse(line, &snapshot) -> Parsed` (the
snapshot is what makes a discovered skill a command). A line is a command
only when it starts with `/` **and** the first token matches a known
command or a discovered skill; anything else — including a line starting
with a path like `/usr/bin/env` — is a prompt. An unrecognised `/word`
is an error, not a prompt: `  ! unknown command /wat - /help
lists them`, because silently sending a mistyped command to a model is how you
pay for a typo.

| command | effect | built on |
| --- | --- | --- |
| `/help` | every command, one line each, then discovered skills | `ChatHost::skills` |
| `/quit`, `/exit` | leave (code 0) | — |
| `/model` | list user-visible candidates, marking the active one | `ChatHost::models` |
| `/model <name>` | use it for subsequent turns | `HostChange::Model` |
| `/config` | effective settings with their origins | `ChatHost::config_summary(None)` — the `forge config show` data |
| `/config <key>` | one key: value + origin | `config_summary(Some(key))` — the `forge config explain` data |
| `/approval` | show the current policy | `Environment::approval` |
| `/approval <mode>` | set it for subsequent turns (`auto`/`prompt`/`prompt-dangerous`/`deny`) | `HostChange::Approval` |
| `/skills` | discovered skills, name + description | `ChatHost::skills` |
| `/graph <query>` | ranked project files for a query | `ChatHost::graph_context` → `forge_graph::query`, the one implementation `forge graph context`, `graph grep --semantic` and `forge_graph_context` already share |
| `/session` | current session id, run count, project root | `AgentService::sessions` |
| `/session new` | start a fresh session; the transcript gets a `  - new session <id>` divider | — |
| `/session <id>` | switch to it and re-render its transcript (§7) | — |
| `/fork [--at <pos|run-id>]` | fork and continue in the fork (§11) | `AgentService::fork_session` |
| `/bg` | detach the running turn; the conversation continues in a fork (§10.1.1) | `AgentService::fork_session` |
| `/jobs` | runs and their states | `AgentService::list_runs` |
| `/attach <run-id>` | re-attach to a run (§10) | `AgentService::attach` |
| `/<skill-name> [text]` | a turn that asks for that skill | see §9.4 |

`/model <name>` and `/approval <mode>` rebuild the runtime — the same
thing the corresponding CLI flag would have done — and therefore are
**refused while any run is live** in this process:
`  ! finish or cancel the running turn first (/jobs, /attach <id>,
Ctrl-C)`. Rebuilding would orphan the live runs' input channels and
cancellation tokens, and a `/bg` job you could no longer answer or
cancel is worse than a command you have to retype. Neither writes to any
config file; both last for the session, and `/config <key>` keeps
reporting the real origin of the underlying value.

### 9.2 Completion

`Command::complete(line, pos, snapshot) -> (usize, Vec<String>)`, pure,
unit-tested. Rules:

- at position 0 with a leading `/`: every command plus every discovered
  skill, filtered by prefix;
- after `/model `: user-visible model names;
- after `/approval `: the four policies;
- after `/attach `: run ids from the last `/jobs` snapshot;
- after `/session `: `new`, plus the project's recent session ids;
- anywhere else: nothing. No file-path completion in v1 — forge reads
  files through tools, and half-working path completion is worse than
  none. Recorded as a follow-up.

The snapshot travels *inside* `Prompt` (`CompletionSnapshot`: commands are
compiled in, skills/models/jobs/sessions come from the host), so the
completer never calls back into the host and never touches the filesystem
on the editor thread.

### 9.3 Mocks are invisible, in exactly one place

`ChatHost::models` is the only source of the `/model` listing and the
only source of `/model`'s completion candidates, and it filters
test-only entries there — so there is no second list to forget. `/help`
and the banner name no provider at all. A user who has set
`FORGE_TEST_MOCKS=1` and `model = "scripted-mock"` still sees their
active model reported by `/config`, because that is their configuration
and hiding it would be a lie; what never happens is forge *offering* a
mock. A process-level test asserts that `mock` appears nowhere in
`/model`, `/help` or the banner output.

### 9.4 Skills as `/name`

`/tdd write the failing test first` submits the prompt:

```text
Use the tdd skill.

write the failing test first
```

That is a normal turn. The runtime's existing `SkillRegistry::match_task`
matches on whitespace-separated words of ≥3 characters against the
lowercased skill name and description, so the literal skill name in the
first line activates it, a `skill_activated` event is recorded, and the
instructions reach the model through the one path that already exists.
No runtime change, no second activation mechanism, and the transcript
shows `  - skill: tdd` as proof it worked.

The honest limitation: matching is lexical, so the words `use`, `the`
and `skill` can also match *another* skill whose description contains
them, and a skill whose name is shorter than 3 characters cannot be
matched this way. Both are pre-existing properties of `match_task`
(already recorded in the needle spec's follow-ups as "skill selection is
lexical, not embedding-ranked"). The fix is a runtime-level explicit
activation (`RunOptions::activate_skills`), recorded as a follow-up in
§14 rather than bolted onto the UI.

## 10. Background and reattach

### 10.1 What "background" means here

A turn is a tokio task in **this** process. `/bg` detaches the *view*,
not the work:

```text
> refactor the parser
  - routing: needle -> qwen3-coder (conf 0.88)
  * read_file src/parser.rs
    -> ok (4 ms)
/bg
  - detached run 01JCF4... - /jobs to list, /attach 01JCF4... to follow
  - this conversation continues in fork 01JCFB... (the job keeps writing to 01JCF3...)
>
```

### 10.1.1 Why `/bg` forks the conversation

The second line is not decoration. It keeps the chat clear of a real
corruption, whose failure mode is worth stating precisely because the
obvious guess about it is wrong.

`forge-runtime::replay::conversation_from_events` walks a session's log in
**file order**, explicitly documented as valid because "runs of one
session are appended sequentially". Two runs in one session break that
assumption — but the result is **not** a provider error.
`replay::repair_tool_pairs` keeps the history API-valid either way: for an
assistant message with tool calls it consumes only the immediately
following `Role::Tool` messages, keeps those whose id is in `expected`,
**silently drops the rest as orphans** (`replay.rs:199`), synthesizes an
`UNANSWERED_TOOL` result for every expected id it did not see
(`replay.rs:202-204`), and drops a bare tool message with no assistant
call in front of it (`replay.rs:208`). There is no 400.

What actually happens is quieter and worse. With runs A and B interleaved
— A's assistant tool call, then B's assistant message, then A's
`tool_result` — A's real, *successful* result is discarded as an orphan
and A's call is replayed to the model as
`[forge: run ended before this tool answered]`. **A model told that its
tool call went unanswered can legitimately retry it**, and the call it
retries may have written a file, deleted one, or run a command. So the
hazard is a corrupted history inviting a duplicate side effect, reported
by nothing: no error, no warning, no `degraded` flag.

This is reachable on `main` today and has nothing to do with the chat:
`AgentService` has no per-session concurrency guard, and
`forge-server/src/handlers.rs:124` passes a caller-supplied `session_id`
straight into `start_run`, so two overlapping runs on one session are
creatable over REST. It is being fixed in the runtime, in two layers:
replay will group events by `run_id` so logs that are *already*
interleaved replay correctly, and `AgentService` will refuse a second
concurrent run on a live session with a typed error (sequential reuse
unchanged). **This design is written against that fixed world** — the
invariant is the runtime's to enforce, and the chat relies on it rather
than being the only thing upholding it (§5).

`/bg` still forks, for two reasons that both survive the fix:

- **UX.** Backgrounding a long turn and then typing something else are two
  continuations of one past, which is exactly what `fork_session` is for
  ("keeping two continuations of the same past is what `forge session
  fork` is for"). Without the fork the user's next sentence would have
  nowhere to go until the job finished.
- **Defence in depth.** Forking means the chat never even asks the runtime
  to start a second concurrent run on a live session, so its behaviour
  does not depend on which side of that fix a given binary is on.

The detached run keeps the session it started in. Consequences, all stated
plainly in the transcript and the README:

- the fork contains the backgrounded run's events *so far*, so its history
  ends mid-run; replay already repairs an unanswered tool call the same way
  it repairs an aborted run;
- the background run's **result is never folded into the foreground
  conversation**. `/attach <run-id>` to watch it, or `/session <its-id>` to
  continue the conversation it belongs to;
- if the session has no events yet (you backgrounded the very first turn
  before anything was written), there is nothing to copy, so the
  foreground simply starts a fresh session and says so.

The prompt comes back immediately. The run keeps going, keeps writing
events to the session log, and a small **watcher** task consumes its
event stream and emits exactly two kinds of notice through
`ChatIo::notify` (which uses rustyline's `ExternalPrinter` while a prompt
is up, and plain stdout otherwise):

- `  # job 01JCF4... needs approval - /attach 01JCF4...` on
  `ApprovalRequested`;
- `  # job 01JCF4... completed` / `failed` / `cancelled` on a terminal
  event.

Nothing else from a detached run is printed. A detached run's output
belongs to its transcript, and interleaving two transcripts into one
scrollback would make both unreadable.

`/jobs` is `AgentService::list_runs()` rendered: run id, state, session,
age, and `(this conversation)` on the attached one. It shows every live
run plus the 20 most recent finished ones, newest first — Phase A's
bounds, unchanged.

`/attach <run-id>` is `AgentService::attach()`: render the backlog
through the same renderer, then stream live until the run settles, `/bg`
again, or Ctrl-C cancels it. Attaching to a run in *another* session also
switches the chat's session to that run's session, because continuing to
type into a conversation you are not watching is a trap — and the run
becomes the foreground run, so a prompt typed while it finishes queues
behind it and the one-live-run-per-session invariant (§5) still holds. The
fork you were in is not lost; `/session <id>` goes back to it, and
`/session` prints the id you are in at all times.

### 10.2 What you get, and what you do not

You get, inside one `forge` process: a turn that keeps running while you
type the next thing, several concurrent runs, notices when they finish or
need you, and reattachment with full history.

You do not get:

- **Survival of the process.** Close the terminal, or exit the chat, and
  the runs die with it. There is no daemon and no socket. So the first
  exit request — `/quit`, `Ctrl-D`, or the second `Ctrl-C` — asks once:
  `  ! 1 job still running - ask again to abandon it`; the next exit
  request cancels every live run (recording each cancellation) and leaves.
- **Cross-process attach.** `/attach` on a run another `forge` process
  owns renders its recorded history and then says
  `  - run 01JCF4... belongs to another forge process; showing its
  recorded history only` and returns to the prompt. `Attachment::is_live()`
  is the flag; this is Phase A's documented limit, restated where a user
  meets it.
- **Answering a detached run's approval without attaching.** A parked
  background run waits; the notice tells you to `/attach`. Offering
  `/approve <run-id>` would mean approving an operation whose context is
  off screen.

## 11. Forking

`/fork` calls `fork_session(current, at)` and **continues in the fork**:

```text
> /fork
  - forked to session 01JCF9... (42 events copied)
  - this conversation continues in the fork; 01JCF3... is untouched
>
```

With no `--at`, the fork's history is identical to what is already on
screen, so nothing is re-rendered — re-printing 42 events the user just
watched would be noise.

With `--at <pos|run-id>`, the fork is *shorter* than the screen, so the
screen would lie. The chat prints a divider and the fork's tail — the
last user prompt and the last assistant answer of the fork — so the
boundary is visible without re-dumping history:

```text
  - forked to session 01JCFA... at position 18 (run 01JCF5...)
  - the conversation from here continues at that point:
> explain the parser
(the parser is a recursive-descent ...)
  - ------------------------------------------------
>
```

Everything above that divider remains a true transcript of the *source*
session; everything below belongs to the fork. Nothing is erased,
because an inline transcript never rewrites scrollback.

`/fork` is refused while a turn is **attached**, because the live run
belongs to the source session and its remaining events would land in a
log the chat had stopped following. That is exactly the trade `/bg` makes
deliberately, so the refusal names it rather than just saying no:
`  ! a turn is still running - /bg detaches it and continues in a
fork, or Ctrl-C cancels it`. `/fork` with only *detached* jobs live is fine:
they are in other sessions already.

## 12. The terminal is not always a terminal

### 12.1 Narrow terminals

Nothing in the transcript is width-sensitive **by construction**: forge
draws no boxes, no tables, no rules longer than 48 characters, and
nothing right-aligned. Long content — an assistant answer, an error
message, a path — is emitted as-is and wrapped by the terminal, which
knows the width better than we do. The only fixed-width artifacts are
the 2–6 character gutters of §4.1 and the 48-character `/fork` divider,
so the design holds down to a 40-column terminal. rustyline handles the
input line's own wrapping and cursor arithmetic (it reads the real width
via `TIOCGWINSZ`), including a resize mid-edit.

The one place a limit is applied is the ellipsized argument summary (60
characters) and the `/jobs`/`/model` listings (one item per line, no
column alignment). `Editor::dimensions()` is available if a future
feature genuinely needs the width; no current line does, and not needing
it is cheaper than handling it.

### 12.2 `NO_COLOR` and dumb terminals

One `Palette`, decided once at startup, applied only in the writer:

colour is on **iff** stdout is a TTY **and** `NO_COLOR` is unset **and**
`--no-color` was not passed **and** `TERM` is neither unset nor `dumb`.

Otherwise every `Style` renders as plain text with the same ASCII
prefixes, so the transcript is identical modulo escape sequences. The
same decision is passed to rustyline as `ColorMode::Disabled`. On
`TERM=dumb` rustyline reports the terminal unsupported and falls back to
a plain line read: editing, history recall and Tab completion are
unavailable, everything else works, and the banner says so once —
`  - dumb terminal: line editing and completion are off`.

### 12.3 Non-TTY stdin: piped chat

`forge chat < script.txt` and `printf '…' | forge` must work, and must
work the way `forge run` already works with piped stdin. The rule
`forge run` established is: **a line arriving while a run waits for
input is the approval answer.** The chat generalises it rather than
contradicting it:

- stdin is read line by line, in order;
- a line arriving while no run is parked is a prompt or a slash command;
- a line arriving while a run *is* parked on approval is the approval
  answer;
- **EOF means "no more input", not "stop now"**: the in-flight turn
  finishes, every queued prompt runs in order, and then the chat exits 0.
  `printf 'first\nsecond\n' | forge` therefore runs two turns, in one
  session, in order — which is the only reading of a piped script that does
  not silently drop work;
- there is nobody to ask, so EOF does **not** get the live-job
  confirmation of §10.2: any still-detached job is cancelled with a
  `  # job ... cancelled at exit` notice. This is the same principle as
  `NativeExecution`'s non-interactive approval — when a question cannot be
  asked, take the safe, stated action instead of blocking;
- each input line is echoed to stdout with the `> ` gutter, so the
  captured stdout is a readable transcript — this is the one place the
  transcript echoes input, because there is no terminal echo to rely on;
- colour follows **stdout**, per the unchanged §12.2 rule, not stdin: so
  `forge chat < script.txt` in a terminal is coloured and
  `printf '…' | forge | cat` is plain. The gutters and wording are
  identical either way;
- no history file is written and no completion exists — there is nobody
  to complete for.

`Ctrl-C` still works in piped mode: SIGINT reaches the process and the
turn is cancelled exactly as on a terminal. That is what makes the most
important keybinding testable without a pty (§13.2). *(Corrected: this
paragraph originally said "the `interrupted()` arm fires". It does not —
`App::drive` has no `interrupted()` arm, only a peek taken after
`select!` resolves for some other reason, which a running tool call can
starve indefinitely. `PipedIo::read` races the SIGINT listener itself;
see §16.)*

### 12.4 `--json`

`forge --json` with no subcommand, and `forge chat --json`, **fail** with
`--json is not supported by the interactive chat; use forge run --json,
or forge serve for a machine-readable stream`. The flag's contract is
"stdout is a single machine-readable JSON value", which a conversation
cannot honour. Inventing a JSONL event stream here would duplicate
`forge serve`'s SSE endpoint with a second, untested schema.

### 12.5 Windows

rustyline supports Windows consoles, and everything drawn is ASCII, so
the transcript is portable. Two differences are accepted and documented:
`Ctrl-D` on Windows is a keybinding rather than a termios `VEOF`
character (rustyline handles this), and the clipboard integration pulls
two Windows-only crates (§2.2). No Windows-specific code is written.

## 13. Testability

Every other adapter in this repo has process-level integration tests, and
this one must too — but the bulk of the coverage is cheaper than that.

### 13.1 Unit: the pure layers, no terminal, no process

`cargo test -p forge-chat` covers, with no TTY and no model:

- **`command.rs`** — the parse table (command, command + args, skill,
  unknown `/word`, a prompt that starts with `/usr/...`, `/` alone,
  trailing whitespace); completion at each position; that a mock name is
  never a candidate.
- **`render.rs`** — one test per `EventKind` including the two special
  cases (`needle-dispatch` phrasing, `ToolResult` rendering nothing); a
  truncated `write_file` args summary still yielding the path; the
  print-the-answer-once rule in both directions.
- **`controller.rs`** — the entire §6.3 Ctrl-C table as a parameterised
  test; approval answers, including that a slash command typed while an
  approval is pending does not deny it; the FIFO prompt queue; `Batch`
  mode's EOF draining that queue; `/model` refused while running; the
  two-step quit with a live job.
- **`app.rs`** — the driver itself, against a `ScriptedIo` (a `Vec` of
  queued `ReadOutcome`s, a captured output buffer, and a channel that
  fires `interrupted()`) and a `FakeHost` wrapping a real `AgentService`
  built from `MockExecution` + the scripted mock model + a temp session
  store. This proves the loop end to end *in-process*: a turn renders
  and answers; an interrupt mid-turn cancels the run and the loop
  survives; an approval round-trip approves and denies; `/bg` + `/jobs` +
  `/attach` do what §10 says; a failed turn leaves the loop running.

### 13.2 Process-level: `crates/forge-cli/tests/chat.rs`

Hermetic exactly like `cli.rs`, `mcp.rs` and `acp.rs`: temp
`HOME`/`XDG_CONFIG_HOME`, the full `FORGE_*` scrub list,
`FORGE_NEEDLE_AUTOFETCH=false`, `NO_COLOR=1`, `FORGE_TEST_MOCKS=1`, a
scripted mock model, no network. stdin is a pipe, so this drives the
piped protocol of §12.3:

- `forge` with no arguments runs a turn from piped stdin and exits 0 —
  which is also the test that `command: Option<Command>` works;
- slash commands (`/help`, `/model`, `/config`, `/session`, `/skills`,
  `/jobs`) produce their output and never the word `mock`;
- an approval is answered `y` and then `n` on separate runs, and the
  session log shows `approval_requested` + `approval_decided` with the
  right verdict — proving the parked mechanism, not a second one;
- two piped lines run as two turns **in order, in one session**, proving
  the queue rather than a concurrent second run (§5);
- **SIGINT mid-turn**: send `SIGINT` to the child while a scripted model
  is mid-run, then assert the child is still alive, the transcript shows
  `cancelled`, the session log has a `cancelled` event, and the process
  exits 0 at EOF. This is the Ctrl-C-cancels-the-turn-and-does-not-quit
  guarantee, tested for real against the real binary. It exercises the
  `interrupted()` path (the only one piped mode has); a *typed* Ctrl-C on a
  TTY goes through the conditional binding, whose decision is a pure
  function tested in §13.1;
- `--json` is refused with a non-zero exit and nothing on stdout;
- stdout carries the transcript and nothing else; all diagnostics are on
  stderr even with `-vvv`.

### 13.3 BDD: `tests/features/chat.feature`

The user-visible story, through the compiled binary, in the existing
cucumber harness (which needs one new world helper: today's `run_forge`
hardwires `Stdio::null()` for stdin): a conversation across two turns
shares one session and its log shows both runs; `/fork` creates a second
session whose log is a prefix copy; an approval denied from the chat
leaves the file unwritten; `/model` output contains no mock.

### 13.4 Deliberately no pty dependency

A pty crate would test rustyline's line editing, which is rustyline's
job. What is ours and TTY-only is: the keybinding map, the completer, the
validator and the palette decision — all of which are pure functions
tested directly (§13.1) — plus the interrupt path, which is tested
against the real process with a real signal (§13.2). Adding
`portable-pty` to dev-dependencies to re-test someone else's readline,
flakily, is not worth the crates. Recorded as a follow-up should the raw
mode path ever regress in a way the above cannot see.

## 14. Non-goals and recorded follow-ups

Deliberately out of scope, each with the reason and the shape of the fix:

- **Token streaming.** The loop returns final text; the chat renders it
  when it lands. When `ModelProvider` grows a streaming method, the
  transcript gains an incremental assistant block and nothing else
  changes — the renderer is already per-event. (Same follow-up as
  `forge acp`'s.)
- **Cross-process background runs.** Needs a daemon or a socket the
  runtime does not have (Phase A's recorded limitation). Until then the
  chat's jobs live and die with its process, and it says so.
- **The per-session concurrency guard** (`AgentService` refusing a second
  concurrent run on a live session, and replay grouping events by
  `run_id`) is a runtime fix landing separately, not part of this
  sub-project. The chat is written to rely on it and not to provoke it
  (§10.1.1).
- **Explicit skill activation.** `/name` relies on lexical
  `match_task`. The fix is `RunOptions::activate_skills` in the runtime,
  which also fixes the same weakness for `forge run`, `forge mcp` and
  ACP.
- **Path completion after `@` or inside a prompt.** Useful, and
  orthogonal; half-working path completion is worse than none.
- **Rendering `tool_result` payloads on demand** (a `/last` or
  `/show <n>` command). The data is in the log; `forge session show`
  reads it today.
- **Editing the config from the chat.** `/model` and `/approval` change
  the session, never a file. Writing config from a REPL needs a
  provenance story (which file? what about the origin `/config` reports?)
  that is not worth designing here.
- **A `/doctor` command.** `forge doctor` is one process away and its
  checks span crates the chat would need a third seam for. The banner's
  `brain active/off` line carries the one fact a conversation needs.
- **Images, audio, non-text input.** No provider behind this chat takes
  them (the ACP adapter advertises the same).
- **Taking the completion snapshot's filesystem listings off the
  executor.** `App::refresh_completions` calls `list_runs()` (which
  re-reads and re-parses every session's whole JSONL log) and
  `list_sessions()` synchronously, once per submitted line, on the
  executor — as do `events_for` and `fork_session` at their own call
  sites. The multi-thread runtime `forge-cli` builds keeps this off the
  critical path, so it is latency, not a stall, and moving *these two*
  behind `spawn_blocking` would leave the others exactly as they are. The
  fix worth having is a bounded/incremental `list_runs`, in the runtime,
  where every front end gets it. `forge-chat`'s crate doc states the
  honest rule in the meantime: no terminal, filesystem access only
  through `AgentService`.

## 15. Self-review

- **Every question the brief posed has exactly one answer here**: crate
  stack (§2, with measured costs), event→render mapping and where it
  lives (§3, §4), approval UX including Ctrl-C mid-approval and the
  `NativeExecution` change (§8), keybindings and the complete Ctrl-C
  table (§6), background/reattach with explicit non-goals (§10), fork and
  what the transcript shows (§11), narrow/non-TTY/`NO_COLOR` (§12),
  session continuity (§7), testability end to end (§13).
- **No TBDs, no placeholders.** Every "later" is in §14 with a reason
  and a shape.
- **Reuse, stated**: the turn driver copies `forge-acp::server::run_turn`
  (subscribe-before-start, select, flush, settle); the pure/impure split
  copies `forge-acp::dispatch`; the parked-approval round trip is MCP's
  and ACP's mechanism, not a new one; the editor thread copies
  `NeedleEngine`'s dedicated-thread pattern; `/graph` calls
  `forge_graph::query`, the shared ranking implementation; the truncated
  args scan moves into `forge-core` so ACP and the chat share one.
- **Contradiction check against the README**: single binary (one new
  dependency, no runtime), stdout discipline (§12.3, §12.4), approval
  gating unchanged (§8, and the fast path still cannot prompt), sessions
  as conversations (§7), mocks invisible (§9.3), `--json` contract
  honoured by refusal rather than reinterpretation (§12.4).
- **Three decisions a reviewer may want to push back on**, all argued
  rather than assumed: starting a *fresh* session by default instead of
  resuming (§7); refusing `/model`/`/approval`/`/fork` while a run is live
  instead of hot-swapping (§9.1, §11); and `/bg` moving the foreground
  conversation into a fork (§10.1.1) — the one place the design does
  something the user did not literally ask for. It is justified as UX (two
  continuations of one past need two sessions) plus defence in depth; the
  concurrency invariant itself is the runtime's to enforce, not the UI's.

## 16. Amendment (implemented 2026-09-25)

Recorded after all eleven tasks landed and `just verify` passed, per §15's
own contract that no "later" go unrecorded.

**§2.2's measured dependency count landed exactly as predicted.** Diffing
`Cargo.lock` immediately before and after `rustyline` was added
(`git show b164b37~1:Cargo.lock` vs `git show b164b37:Cargo.lock`, `comm
-13` on the sorted `name = "..."` lines) shows exactly the 8 packages §2.2's
table predicted: `rustyline`, `nix`, `unicode-segmentation`, `radix_trie`,
`nibble_vec`, `endian-type`, and the two Windows-only clipboard crates
(`clipboard-win`, `error-code`). No surprise transitive growth.

**One §14 follow-up moved before this design shipped, not after.** The
per-session concurrency guard and replay's group-by-`run_id` fix — §14
explicitly filed as "landing separately, not part of this sub-project," and
§10.1.1 as the invariant `/bg`'s fork relies on rather than upholds itself —
landed in `ba31374` ("replay groups by run, and a session refuses a second
concurrent run") on 2026-09-24, an ancestor of every commit in this
sub-project's history. So by the time `/bg` shipped, `ForgeError::SessionBusy`
was already real, not merely assumed; the design's own defence-in-depth
argument for forking regardless (§10.1.1's second bullet) is what made that
timing not load-bearing either way. Every other §14 item (token streaming,
cross-process background runs, explicit skill activation, path completion,
on-demand `tool_result` rendering, editing config from the chat, `/doctor`,
non-text input) remains exactly as scoped — none of them shipped, and the
README's Known limitations say so.

**One real-terminal finding changed §6.2's own mechanism, not just its
proof.** §6.2 specifies `ChatIo::notify` uses rustyline's
`ExternalPrinter` while a prompt is up, falling back to plain stdout only
on a dumb/unsupported terminal (§12.2). Testing against a real pty
(`forge-cli/tests/chat.rs`) found that keeping an `ExternalPrinter` alive
routes *every* keystroke's wait through `rustyline` 18.0.1's
`PosixRawReader::select`, whose sibling `poll` guards its blocking read with
a buffer-length check that `select` is missing — so any multi-byte burst in
one kernel read (a paste, fast type-ahead, or a line typed right after
Ctrl-C) permanently wedges the editor thread on a byte the OS has already
delivered and will never redeliver. `TerminalIo::editor_thread_main` no
longer calls `create_external_printer` at all: `notify()` now always uses
the plain-stdout path §12.2 described as the dumb-terminal fallback, on
every terminal. The tradeoff is real and shipped deliberately (interleaved
output instead of a hang) rather than vendoring a three-line patch to a
third-party crate — see the module doc in `terminal_io.rs` and the README's
[Interactive chat](../../../README.md#interactive-chat) section for the
user-facing statement of it.

**One cancel-safety gap surfaced by the same real-terminal testing is
still open.** §6.5 promises "forge never discards what you typed," argued
from rustyline's `TCSADRAIN` behaviour alone. A real pty test found a
second, independent hazard the design did not anticipate: `App::drive`'s
`select!` reconstructs `io.read()` fresh every loop iteration and drops
whichever branch does not win, which is safe for `PipedIo` (a receive-only
channel) but not for `TerminalIo::read`, whose first poll *sends* a
`Job::Read` to the editor thread before awaiting the reply — a send that
cannot be un-sent by dropping the future that issued it. A turn that emits
several events (routing line, answer, footer — three, for one turn) can
therefore abandon several `Job::Read`s in a row, and the editor thread has
no way to tell an abandoned job from a live one: it answers whichever it
dequeues next, leaving the caller that is genuinely still waiting stuck on
a receiver that will now never fire. Documented as a known, not-yet-fixed
gap in `terminal_io.rs`'s `Job` doc and in the README's Known limitations,
rather than closed here — the right fix (a persistent loop reading the
*latest* prompt off a `watch` channel, mirroring `PipedIo`'s persistent
producer thread) is real, separate work.

**`--json`'s refusal message differs in wording from §12.4's, not in
effect.** §12.4 quotes `--json is not supported by the interactive chat;
use forge run --json, or forge serve for a machine-readable stream`. The
shipped message (`chat_cmd.rs`) is `--json is not supported by the
interactive chat; use \`forge run --json\` for machine-readable output` —
the same refusal, one fewer alternative named. Not corrected here to avoid
touching a message an existing test may already match verbatim; worth a
one-line fix next time that file is open for another reason.

**Verified, not merely re-asserted, before this amendment was written**:
`cargo test -p forge-cli --test bdd` (26 features / 54 scenarios / 215
steps, including the four new `chat.feature` scenarios) and `just verify`
both green — see `task-11-report.md` for the exact commands and output.

## 17. Whole-branch review fixes (2026-09-25)

A review of the whole branch, rather than of any one task, found five
defects that a per-task review structurally could not see — each of them
spanning two tasks' worth of code. All are fixed; the two that changed
what §12.3 describes are recorded here as the appendix §15 asks for.

**Batch mode busy-waited for the whole length of every turn.**
`Controller::on_drained` — the method whose entire purpose is "the driver
has nothing left to run, decide whether that is the end" — was never
called by the driver at all. Batch mode terminated anyway, by accident:
every `ChatIo` re-signals EOF on each read once input has ended, the
`read` arm of `App::drive`'s `select!` was unguarded, so that arm resolved
in zero time on every iteration and the loop span at full tilt
(`read -> Eof -> on_eof -> on_drained -> [] -> yield_now -> repeat`) for
the whole duration of every in-flight and queued turn. Measured on one
turn whose tool call was `sleep 3`: stdin closed, 2.34 s of user CPU;
stdin held open, 0.03 s — ~78x, scaling linearly with turn length, in
`PipedIo`'s own stated primary use case. Fixed by *re-aiming* the `read`
arm at EOF (`read_or_wait_for_interrupt`, selected on `App::input_ended`):
it awaits `io.interrupted()`, which pends, instead of `io.read()`, which
resolves instantly. Disabling the arm outright — the obvious fix, and the
first one written — silently removed piped `Ctrl-C` for the whole drain,
because `PipedIo::read` *is* the piped SIGINT listener (§12.3 as corrected
above); a process-level test that closes stdin **before** signalling now
guards that, the case the pre-existing SIGINT test could not reach because
it holds stdin open to send `/quit`. The end-of-input decision also gets
an explicit home, `App::settle_ended_input`, re-taken after every loop
iteration — which is every point the state behind it can change: a run
settling, a cancelled run settling, an attach settling, a queued turn
starting, and an `ApprovalRequested` arriving *after* EOF (the case the
spin was load-bearing for: §12.3's auto-denial). `select!` also gained an
`else` arm, so an all-arms-disabled state would end the chat rather than
panic. `Controller`'s three `on_drained` tests are load-bearing now
instead of vacuous, and `ScriptedIo::eof_reads` lets a test assert the
*absence of a spin* — the transcript is identical either way, so nothing
else could have caught it.

**`/bg` after `/attach` permanently corrupted the live-work count.**
`do_background` spawned a watcher only for `RunEvents::Live`, so a run
detached after `/attach` had none — while `Controller::on_background` had
already counted it and the only thing that ever un-counts a job is a
watcher's `BgMsg::Settled`. `live_work()` therefore stayed above zero for
the rest of the process with nothing running: `/model` and `/approval`
refused for ever with "finish or cancel the running turn first", every
`/quit`, `Ctrl-D` and second `Ctrl-C` needing two requests for ever,
`ChatState::Detached` for ever. Fixed by making the watcher take a
`RunEvents` and drive it through the same `recv_events` both origins
already share — a detached-but-followed run is exactly what `/jobs` and
`/attach` are for. The watcher now also reports `Settled` when the stream
simply ends, not only on a terminal event, so no future shape of this can
strand the count either.

**§12.3's `  # job ... cancelled at exit` notice is now implemented.**
It was specified and never written: `do_cancel_all_jobs` cancelled
silently. It now prints one notice per detached job, in every mode rather
than only in piped mode — a terminal user who confirmed the abandonment
has no less right to know which jobs it took.

**§12.3's piped-`Ctrl-C` mechanism is not the one that shipped.** The
section said "SIGINT reaches the process, the `interrupted()` arm fires".
There is no `interrupted()` arm: `read` and `interrupted` both take
`&mut self`, so `App::drive` can only peek at `interrupted()` *after*
`select!` has resolved for some other reason — which a running tool call
starves indefinitely, making the cancellation a coin flip (measured: 3
passes in 5, 2 hangs). `PipedIo::read` races the SIGINT listener inside
itself instead, which is a live arm every iteration. §12.3's paragraph is
corrected in place; `piped_io.rs`'s module doc carries the full argument.

**`TerminalIo::shutdown` could hang the process after `/quit`.** It joined
the editor thread unconditionally. If that thread is inside `readline()`
serving an orphaned `Job` — the cancel-safety hazard §16 records as still
open — `readline` returns only when a key is pressed, so the join is a
process that has printed nothing, drawn no prompt, and will not exit.
Reachable today: a turn running with an orphaned job outstanding, then
enough `kill -INT`s to reach `Quit(130)`. The thread is now detached
rather than joined (`append_history` already ran for every accepted line,
so the thread-exit `save_history` was never the durability mechanism), and
`App::start` calls `io.shutdown()` unconditionally on `drive`'s return
rather than on one path inside it.

**A queued prompt could be stranded by a turn that failed to start.**
`start_turn`'s error path called `on_run_settled` without `take_queued`,
unlike every other settlement path (`finish_run`, `settle_attached_run`,
`settle_cancelled_run`). Pre-existing, but worse after the EOF fix: the
old spin re-took the end-of-input decision regardless, so the symptom was
wasted CPU; with the loop correctly idle it is a silent hang. `start_turn`
is now a loop over the queue rather than a single attempt, which also
avoids boxing a recursive `async fn`.

**`forge-chat`'s crate doc claimed a purity it does not have.** "no
terminal, no `rustyline`, no I/O syscalls" — but `ChatHost::service()`
hands the crate the whole runtime, and `app` takes synchronous filesystem
reads on the executor (`refresh_completions`' `list_runs()` +
`list_sessions()` after every submitted line, `events_for`,
`fork_session`, `latest_session`). The claim is corrected rather than the
code: moving the two listings behind `spawn_blocking` would leave the
other three exactly as they are and still not make the sentence true,
while the fix worth having — a bounded, incremental `list_runs` — belongs
in the runtime, where every front end gets it (filed in §14). The rule the
crate actually keeps, and now states, is: no terminal; filesystem access
only through `AgentService`.
