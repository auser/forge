//! `forge chat` (and bare `forge`) end to end: the compiled binary driven
//! over real pipes (and, on unix, a real pseudo-terminal), not the library.
//!
//! Every other adapter (`acp.rs`, `mcp.rs`, `cli.rs`'s `run`/`serve`) has a
//! process-level suite; the chat's in-process tests (`forge-chat`'s
//! `app.rs`) already cover the driver logic against `ScriptedIo`/`FakeHost`,
//! but two guarantees can only be proved against a real process:
//!
//! * a real `SIGINT` mid-turn must cancel the turn without killing the chat
//!   (piped mode's interrupt path, `PipedIo::interrupted` ->
//!   `tokio::signal::ctrl_c`);
//! * a *typed* Ctrl-C on a real terminal must reach `TerminalIo` as
//!   `ReadOutcome::Interrupt` via `rustyline`'s raw-mode keybinding, with no
//!   process-level `SIGINT` involved at all (§ the module doc in
//!   `terminal_io.rs`) — verified so far only by reading the `rustyline`
//!   source, never against a real PTY.
//!
//! Hermetic exactly like `cli.rs`/`acp.rs`/`mcp.rs`: temp HOME/XDG, FORGE_*
//! scrubbed, autofetch off, offline scripted-mock providers only.
//!
//! # Known limitation found by the pty test, not fixed here
//!
//! `a_typed_ctrl_c_on_a_real_terminal_interrupts_without_killing_the_chat`
//! proves the headline claim — a typed Ctrl-C at an idle prompt reaches the
//! chat as `ReadOutcome::Interrupt`, the process stays alive, the exit hint
//! prints — but it does **not** go on to prove the chat is still usable
//! afterward, because it is not: **the very next `Editor::readline()` call
//! hangs forever.** Reproduced on every run (many, over multiple sessions),
//! isolated to `rustyline`'s external-printer machinery specifically —
//! commenting out `editor.create_external_printer()` in
//! `terminal_io.rs::editor_thread_main` (always using the `StdoutPrinter`
//! fallback instead) made the second `readline()` return normally every
//! time, in the same test, with nothing else changed. Two consecutive
//! *normal* (`Ok(Line(_))`) submissions never trigger it; only a call that
//! returned `Err(ReadlineError::Interrupted)` poisons the next one.
//!
//! Not fixed here: removing `create_external_printer()` unconditionally
//! would silently degrade `notify()` (background job notices, design
//! §10.1) for every session, not only ones that hit Ctrl-C, and the actual
//! defect is inside `rustyline` 18.0.1 or in some assumption this crate's
//! use of it does not satisfy — tracking it down further, or reworking
//! `TerminalIo`'s printer lifecycle to dodge it, is real work in its own
//! right and out of this task's scope (process-level *tests*). This is
//! exactly the gap Task 8's brief flagged as unverified against a real
//! PTY, now verified, and broken.

use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const FORGE_ENV_VARS: &[&str] = &[
    "FORGE_MODEL",
    "FORGE_MODEL_BASE_URL",
    "FORGE_MODEL_KEY_ENV",
    "FORGE_MOCK_SCRIPT",
    "FORGE_ROUTER",
    "FORGE_ROUTER_URL",
    "FORGE_ROUTER_KEY_ENV",
    "FORGE_ROUTER_ESCALATE",
    "FORGE_JEV_URL",
    "FORGE_JEV_KEY_ENV",
    "TYPESAFE_API_KEY",
    "FORGE_EXECUTION",
    "FORGE_APPROVAL",
    "FORGE_LOCAL_ONLY",
    "FORGE_MAX_TURNS",
    "FORGE_SERVER_HOST",
    "FORGE_SERVER_PORT",
    "FORGE_NEEDLE_VARIANT",
    "FORGE_NEEDLE_AUTOFETCH",
    "FORGE_NEEDLE_WEIGHTS_SHA256",
    "FORGE_NEEDLE_BACKEND",
    "FORGE_NEEDLE_WEIGHTS_BASE_URL",
    "FORGE_NEEDLE_TEST_SHA256",
    "FORGE_MOCK_VERBOSE",
    // Test-only mocks are gated; scrubbed then set by `forge()` below.
    "FORGE_TEST_MOCKS",
];

/// A hermetic `forge` invocation (see `cli.rs::forge`/`acp.rs::forge` for
/// why each piece is here).
fn forge(tmp: &Path, project: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_forge"));
    for var in FORGE_ENV_VARS {
        cmd.env_remove(var);
    }
    cmd.arg("--project").arg(project);
    cmd.env("HOME", tmp.join("home"));
    cmd.env("XDG_CONFIG_HOME", tmp.join("xdg"));
    cmd.env("FORGE_NEEDLE_AUTOFETCH", "false");
    // The scaffolded projects use `model = "scripted-mock"`.
    cmd.env("FORGE_TEST_MOCKS", "1");
    cmd.env("NO_COLOR", "1");
    cmd
}

/// Drive a chat session by writing lines to the child's stdin, then close
/// it (EOF) and wait for the process to exit on its own. Every line is
/// written up front rather than interleaved with reads: the amounts here
/// are tiny (a handful of short lines), well under a pipe buffer, so this
/// cannot deadlock the way an interleaved read/write over a full pipe
/// could — `child.wait_with_output()` itself drains stdout/stderr
/// concurrently on separate threads, which is what makes this safe at all.
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
    } // dropping stdin is EOF, which ends the chat (§12.3)
    child.wait_with_output().expect("wait")
}

/// A project scaffolded for chat tests: a scripted-mock model, offline, and
/// a small source file so the transcript has something real to reference.
/// `mode` doubles as both the config's `approval` value and the choice of
/// script (an approval round trip needs one that writes a file; everything
/// else just answers).
///
/// `forge-cli`'s `build_service_with` wires a *per-route model factory*
/// (`commands/service.rs`) that re-resolves — for `scripted-mock`,
/// re-parses `mock_script` from disk — once per **run** (i.e. once per
/// user-submitted line in a chat), not once per process. A script's queue
/// therefore advances normally across the several model calls *inside* one
/// run (an approval's tool-call turn, then its answer), but never carries
/// state from one run to the next: a second chat turn gets a brand new
/// model built from the same file, so it always sees the script's first
/// entry again. `auto` mode's script is one reply for exactly that reason.
fn scaffold(tmp: &Path, mode: &str) -> PathBuf {
    let project = tmp.join("project");
    std::fs::create_dir_all(&project).expect("project dir");
    std::fs::write(project.join("alpha.rs"), "fn parse_config() {}\n").expect("write alpha.rs");

    let script = match mode {
        "prompt" => {
            r#"[
                {"tool_calls": [{"id": "call_1", "name": "write_file", "arguments": {"path": "notes.txt", "content": "scripted notes"}}]},
                {"text": "notes written"}
            ]"#
        }
        _ => r#"[{"text": "the answer"}]"#,
    };
    std::fs::write(project.join("script.json"), script).expect("write script");

    std::fs::create_dir_all(project.join(".forge")).expect("mkdir .forge");
    std::fs::write(
        project.join(".forge").join("config.toml"),
        format!(
            "model = \"scripted-mock\"\nmock_script = \"script.json\"\nrouter = \"static\"\napproval = \"{mode}\"\n"
        ),
    )
    .expect("write config");
    project
}

/// A project for slash-command-only tests: no model configured at all (so
/// the real default, `qwen3-coder`, is what is active). Unlike every other
/// scaffold here this deliberately avoids `scripted-mock`: the banner
/// reports the *active* model honestly even when it is a mock
/// (`CliHost::environment`'s documented contract — hiding it there would be
/// a lie about what is running), so a test asserting no line ever names a
/// mock must not configure one in the first place. No turn ever runs here,
/// so no credentials or network are needed either.
fn scaffold_idle(tmp: &Path) -> PathBuf {
    let project = tmp.join("project");
    std::fs::create_dir_all(project.join(".forge")).expect("mkdir .forge");
    std::fs::write(
        project.join(".forge").join("config.toml"),
        "approval = \"prompt\"\n",
    )
    .expect("write config");
    project
}

/// A project whose scripted model requests a `run_command` that sleeps for
/// a real second, long enough to observe the tool call start and deliver a
/// signal before it finishes. `approval = "auto"` so the (at-least-`risky`)
/// command runs without a round trip of its own competing with the signal.
fn scaffold_slow(tmp: &Path) -> PathBuf {
    let project = tmp.join("project");
    std::fs::create_dir_all(&project).expect("project dir");
    std::fs::write(
        project.join("script.json"),
        r#"[
            {"tool_calls": [{"id": "call_1", "name": "run_command", "arguments": {"command": "sleep", "args": ["1"]}}]},
            {"text": "slow thing done"}
        ]"#,
    )
    .expect("write script");
    std::fs::create_dir_all(project.join(".forge")).expect("mkdir .forge");
    std::fs::write(
        project.join(".forge").join("config.toml"),
        "model = \"scripted-mock\"\nmock_script = \"script.json\"\nrouter = \"static\"\napproval = \"auto\"\n",
    )
    .expect("write config");
    project
}

/// Every event the chat's session logged, across every session in the
/// project (a chat test's project has at most one, but reading them all is
/// simpler than threading the session id out of the transcript).
fn session_log(project: &Path) -> String {
    let mut log = String::new();
    let dir = project.join(".forge").join("sessions");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return log;
    };
    for entry in entries.flatten() {
        if entry.path().extension().and_then(|e| e.to_str()) == Some("jsonl") {
            log.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
        }
    }
    log
}

/// Drain `stream` line by line on a background thread into a shared buffer,
/// so a test can poll for a marker without either blocking on a read that
/// may never come or stealing the pipe out from under a later
/// `wait_with_output`/`wait` (the thread is the only reader from here on;
/// everyone else reads the buffer). Returns once the stream hits EOF, so
/// joining the handle after the child exits guarantees the buffer holds
/// everything the process ever wrote.
fn tail_stream<R: Read + Send + 'static>(
    stream: R,
) -> (Arc<Mutex<String>>, std::thread::JoinHandle<()>) {
    let buf = Arc::new(Mutex::new(String::new()));
    let shared = Arc::clone(&buf);
    let handle = std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stream);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => shared
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_str(&line),
            }
        }
    });
    (buf, handle)
}

/// Poll `buffer` until it contains `marker`, or panic with what was
/// actually captured once `timeout` elapses — a bounded wait, not a
/// sleep-and-hope, and never an unbounded one: every test using this must
/// still terminate on its own if the chat never says what is expected.
fn wait_for(buffer: &Mutex<String>, marker: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        let snapshot = buffer.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if snapshot.contains(marker) {
            return snapshot;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {marker:?}; captured so far:\n{snapshot}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Submit `prompt`, wait for it to actually raise the approval question,
/// then answer it — never send the answer up front.
///
/// Piped input is read eagerly (§12.3's `Interactivity::Batch`, and the
/// main loop's own always-live `io.read` arm, app.rs's module doc): if
/// `answer` were written before the run has reached the point of asking,
/// it can be popped off stdin and misread as an unrelated second prompt
/// instead of the pending approval's answer — a real race against the
/// compiled binary's actual (multi-threaded) tokio runtime that a fixed
/// two-line script cannot reliably win. Waiting for the question in the
/// transcript first removes the race instead of hoping to beat it.
fn answer_one_approval(tmp: &Path, project: &Path, prompt: &str, answer: &str) -> (bool, String) {
    let mut child = forge(tmp, project)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn forge");
    let mut stdin = child.stdin.take().expect("stdin");
    let (stdout_buf, stdout_thread) = tail_stream(child.stdout.take().expect("stdout"));
    let (_stderr_buf, stderr_thread) = tail_stream(child.stderr.take().expect("stderr"));

    writeln!(stdin, "{prompt}").expect("write prompt");
    wait_for(&stdout_buf, "approval needed", Duration::from_secs(10));
    writeln!(stdin, "{answer}").expect("write answer");
    drop(stdin); // EOF right after the answer; nothing else to say

    let status = child.wait().expect("wait");
    stdout_thread.join().expect("stdout reader thread");
    stderr_thread.join().expect("stderr reader thread");
    let stdout = stdout_buf.lock().unwrap_or_else(|e| e.into_inner()).clone();
    (status.success(), stdout)
}

#[test]
fn a_piped_conversation_runs_two_turns_in_one_session() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path(), "auto");
    let out = chat(tmp.path(), &project, &["explain alpha.rs", "and again"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("> explain alpha.rs"),
        "piped input is echoed:\n{stdout}"
    );
    assert!(stdout.contains("> and again"), "{stdout}");
    // Each submitted line is a separate *run*, and the per-route model
    // factory (see `scaffold`'s doc) resolves a fresh model per run, so
    // both turns answer with the script's one entry rather than advancing
    // through a queue — the thing this test actually pins is two runs in
    // one session, not two different answers.
    assert_eq!(
        stdout.matches("the answer").count(),
        2,
        "both turns answered:\n{stdout}"
    );
    assert_eq!(
        stdout.matches("  = ").count(),
        2,
        "two turn footers:\n{stdout}"
    );
    let sessions = std::fs::read_dir(project.join(".forge").join("sessions"))
        .expect("sessions dir")
        .count();
    assert_eq!(sessions, 1, "two turns share one session");
}

#[test]
fn slash_commands_answer_and_never_name_a_mock() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold_idle(tmp.path());
    let out = chat(
        tmp.path(),
        &project,
        &["/help", "/model", "/skills", "/session", "/jobs"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    for expected in ["/fork", "/attach", "/approval"] {
        assert!(
            stdout.contains(expected),
            "/help lists {expected}:\n{stdout}"
        );
    }
    // `/model` would list candidates; the configured model is itself a
    // mock, so it must be filtered rather than offered as a choice, and no
    // other line (echoed input aside) may name a mock either.
    for line in stdout.lines().filter(|l| !l.starts_with("> ")) {
        assert!(!line.to_lowercase().contains("mock"), "mock leaked: {line}");
    }
}

#[test]
fn an_approval_is_answered_from_the_conversation() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path(), "prompt"); // scripted model writes notes.txt
    let (ok, stdout) = answer_one_approval(tmp.path(), &project, "write the notes", "y");
    assert!(ok, "{stdout}");
    assert!(stdout.contains("approval needed"), "{stdout}");
    assert!(project.join("notes.txt").exists(), "approved work happened");
    let log = session_log(&project);
    assert!(
        log.contains("\"approval_requested\""),
        "the parked mechanism was used:\n{log}"
    );
    assert!(log.contains("\"approved\":true"), "{log}");
}

#[test]
fn a_denied_approval_leaves_the_file_alone() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path(), "prompt");
    let (ok, stdout) = answer_one_approval(tmp.path(), &project, "write the notes", "n");
    assert!(ok, "{stdout}");
    assert!(!project.join("notes.txt").exists());
    assert!(session_log(&project).contains("\"approved\":false"));
}

/// Review Focus 1, for real: `SIGINT` mid-turn cancels the turn, the
/// process survives, and `/quit` right after still exits 0. This is the
/// piped path's interrupt (`ChatIo::interrupted`, `PipedIo` ->
/// `tokio::signal::ctrl_c`); a *typed* Ctrl-C on a real terminal is a
/// different path entirely (`ReadOutcome::Interrupt` via `rustyline`, no
/// process-level signal at all) — see the pty-backed test below.
#[cfg(unix)]
#[test]
fn sigint_cancels_the_turn_without_killing_the_chat() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // The scripted model's tool call runs a real `sleep`, so the turn is
    // reliably in flight (and reliably still running a second later, when
    // the runtime next checks for cancellation) when the signal arrives.
    let project = scaffold_slow(tmp.path());
    let mut child = forge(tmp.path(), &project)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut stdin = child.stdin.take().expect("stdin");
    let (stdout_buf, stdout_thread) = tail_stream(child.stdout.take().expect("stdout"));
    let (_stderr_buf, stderr_thread) = tail_stream(child.stderr.take().expect("stderr"));

    writeln!(stdin, "start the slow thing").expect("write");
    // Wait for the turn to be visibly under way before signalling, rather
    // than sleeping a guessed interval.
    wait_for(&stdout_buf, "  * run_command", Duration::from_secs(10));

    // SAFETY: `child.id()` is a live pid this process owns until `wait`,
    // and `SIGINT` on a running process is always a defined operation.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
    // Wait for the cancellation to actually land in the transcript before
    // asking to leave. Sent too early, `/quit` races it: in
    // `Interactivity::Batch`, `Controller::request_exit` skips the
    // "job still running, ask again" confirmation entirely (nobody
    // interactive is there to answer it) and goes straight to
    // `Action::CancelAllJobs` + `Action::Quit` — which cancels and exits
    // immediately, without ever draining/rendering the turn's own
    // `Cancelled` event the way `Action::CancelRun` (this signal's own
    // path, `settle_cancelled_run`) does. Waiting here keeps the two
    // cancellation paths from racing instead of relying on this signal's
    // one winning it.
    wait_for(&stdout_buf, "  ! cancelled", Duration::from_secs(10));
    writeln!(stdin, "/quit").expect("the chat must still be listening");
    drop(stdin);

    let status = child.wait().expect("wait");
    stdout_thread.join().expect("stdout reader thread");
    stderr_thread.join().expect("stderr reader thread");
    let stdout = stdout_buf.lock().unwrap_or_else(|e| e.into_inner()).clone();

    assert!(
        status.success(),
        "SIGINT must not change the exit status: {status:?}"
    );
    assert!(stdout.contains("  ! cancelled"), "{stdout}");
    assert!(
        session_log(&project).contains("\"cancelled\""),
        "the run recorded it"
    );
}

/// The third stream, checked directly: stdout carries only the transcript,
/// stderr carries only diagnostics, and `-vvv` (which puts real content on
/// stderr) does not blur that line.
#[test]
fn stdout_carries_the_transcript_and_stderr_carries_diagnostics() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path(), "auto");
    let mut child = forge(tmp.path(), &project)
        .arg("-vvv")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    {
        let mut stdin = child.stdin.take().expect("stdin");
        writeln!(stdin, "explain alpha.rs").expect("write");
    }
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        stdout.contains("the answer"),
        "the transcript reached stdout:\n{stdout}"
    );
    assert!(
        stderr.contains("chat requested"),
        "-vvv tracing reached stderr:\n{stderr}"
    );
    // Nothing that reads as a transcript line (tool/footer/error gutters)
    // leaked into stderr, and the tracing line above did not leak into
    // stdout.
    for gutter in ["  * ", "  - ", "  ! ", "  = "] {
        assert!(
            !stderr.contains(gutter),
            "a transcript line leaked into stderr:\n{stderr}"
        );
    }
    assert!(
        !stdout.contains("chat requested"),
        "a diagnostic leaked into stdout:\n{stdout}"
    );
}

// --- a real pseudo-terminal, `libc` only (no new crate) --------------------
//
// Every test above drives the binary over plain pipes, which exercises
// `PipedIo` end to end but never `TerminalIo` at all (`forge`'s own
// `std::io::stdin().is_terminal()` check in `chat_cmd.rs` picks `PipedIo`
// whenever stdin is not a real tty). The one guarantee this whole phase
// left unverified — Task 8's note, carried forward twice now — is whether a
// *typed* Ctrl-C on a real terminal actually reaches `rustyline`'s
// `bind_sequence` override as `ReadOutcome::Interrupt`, with no process
// `SIGINT` involved (raw mode clears `ISIG`), rather than killing the
// process the way an un-augmented terminal's default `VINTR` mapping would.
// That needs a real controlling terminal, which a pipe cannot give.
//
// `libc::openpty`/`setsid`/`ioctl(TIOCSCTTY)` are the same three calls
// every pty-attaching crate (`portable-pty`, `rexpect`, glibc's own
// `login_tty`) makes; `libc` is already resolved in the lockfile as a
// transitive dependency of `rustyline`, so this adds no new crate.
#[cfg(unix)]
mod pty {
    use std::fs::File;
    use std::os::unix::io::FromRawFd;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};

    /// Open a pty pair and spawn `cmd` with the slave end as its
    /// controlling terminal — `setsid` detaches it from this test
    /// process's own session, and `ioctl(..., TIOCSCTTY, ...)` then makes
    /// the slave the *new* session's controlling tty, which is what makes
    /// a byte written to the master reach the child exactly as a real
    /// keypress would (default termios: canonical-ish line discipline
    /// with `ISIG` on until `rustyline` puts it in raw mode on its own).
    pub fn spawn(mut cmd: Command) -> std::io::Result<(Child, File)> {
        let mut master: libc::c_int = -1;
        let mut slave: libc::c_int = -1;
        // A real terminal always has a nonzero size; a null `winsize` here
        // leaves the pty at 0x0, which is not a condition any real
        // terminal `rustyline` draws to would ever present it with.
        let mut winsize: libc::winsize = unsafe { std::mem::zeroed() };
        winsize.ws_row = 24;
        winsize.ws_col = 80;
        // SAFETY: `master`/`slave` are valid out-pointers; `winsize` is a
        // valid, initialized struct; the name/termios pointers are null,
        // which `openpty` treats as "use the defaults" on every platform
        // this crate declares it for (apple, linux, the bsds).
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut winsize,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }

        // SAFETY: `slave` is a valid, open fd for the whole body of this
        // function (closed explicitly below); `dup` gives each `Stdio`
        // its own fd, so std's spawn machinery can own and close each one
        // independently once it has dup2'd it onto 0/1/2 in the child.
        unsafe {
            cmd.stdin(std::process::Stdio::from_raw_fd(libc::dup(slave)));
            cmd.stdout(std::process::Stdio::from_raw_fd(libc::dup(slave)));
            cmd.stderr(std::process::Stdio::from_raw_fd(libc::dup(slave)));
        }
        // SAFETY: this closure runs in the child, after `fork` and before
        // `exec`, and calls only `setsid`/`ioctl` — both async-signal-safe
        // and exactly what `login_tty` does to attach a controlling
        // terminal. `slave` is still valid in the child (fds survive
        // `fork`).
        unsafe {
            cmd.pre_exec(move || {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(slave, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let child = cmd.spawn()?;
        // SAFETY: this function's only remaining copy of `slave` — the
        // three fds handed to `cmd` above were independent `dup`s, and
        // `cmd.spawn()` (having forked) has no further use for this one.
        unsafe { libc::close(slave) };
        // SAFETY: `master` is a valid, open fd this function owns from
        // here on; wrapping it in a `File` makes `Drop` close it exactly
        // once.
        let master = unsafe { File::from_raw_fd(master) };
        Ok((child, master))
    }
}

/// The one thing a pipe cannot prove (see the module doc above): a typed
/// Ctrl-C on a real terminal survives as `ReadOutcome::Interrupt`, not a
/// process-killing `SIGINT` — proved here by the controller's own idle-Ctrl-C
/// contract (`on_interrupt`'s `None` arm, `forge-chat::controller`): one
/// interrupt at an empty prompt only *hints* that a second one exits, so
/// seeing that hint and then a live, still-responsive process is only
/// possible if the byte reached the chat as `ReadOutcome::Interrupt` and
/// not as a real `SIGINT` (whose default disposition would have killed the
/// process outright, before it could print anything at all).
///
/// Deliberately does not go on to prove the chat is still fully usable by
/// typing a further line: it is not. See the big finding in this file's
/// module doc — one typed Ctrl-C over a real pty leaves the *next*
/// `Editor::readline()` call hung forever inside `rustyline` itself,
/// something no pipe-backed test could ever exercise (`PipedIo` has no
/// `rustyline` in it at all) and the in-process `forge-chat` suite cannot
/// either (its `ScriptedIo` has no real terminal or real `rustyline`
/// underneath it). This test ends by killing the child rather than
/// asking it to `/quit`, which is exactly the gap: a real user hitting
/// Ctrl-C once at an idle prompt cannot be asked to do that.
#[cfg(unix)]
#[test]
fn a_typed_ctrl_c_on_a_real_terminal_interrupts_without_killing_the_chat() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let project = scaffold(tmp.path(), "auto");
    let cmd = forge(tmp.path(), &project);
    let (mut child, master) = pty::spawn(cmd).expect("spawn on pty");
    let mut writer = master.try_clone().expect("clone master for writing");
    let (output, reader_thread) = tail_stream(master);

    // Wait for the idle prompt: the banner's last line, printed once
    // `App::drive` is blocked on its first `io.read` — i.e. `rustyline` is
    // genuinely mid-`readline()`, in raw mode, on the other end of this
    // pty.
    wait_for(&output, "/help for commands", Duration::from_secs(10));

    // A single typed Ctrl-C: on a real terminal in raw mode this is just
    // byte 0x03 arriving on the child's stdin, exactly as it would from a
    // real keyboard — never a kernel-generated `SIGINT` (that would need
    // `ISIG` still enabled, which raw mode has already cleared by the time
    // `readline()` is blocked waiting for it).
    writer.write_all(&[0x03]).expect("write ctrl-c byte");

    // The idle-interrupt hint (`EXIT_HINT` in `forge-chat::controller`)
    // only prints if the byte was read back out as `ReadOutcome::Interrupt`
    // by a chat that is still running its main loop — a real `SIGINT`
    // reaching an unprotected process instead would simply end it, with no
    // further output at all.
    wait_for(&output, "press Ctrl-C again", Duration::from_secs(10));
    assert!(
        matches!(child.try_wait(), Ok(None)),
        "the chat must still be alive after one Ctrl-C"
    );

    // No further interaction attempted — see this test's doc and the
    // module doc's "Known limitation" section. Killed, not asked to
    // `/quit`, because asking is exactly what does not currently work.
    drop(writer);
    child.kill().ok();
    child.wait().ok();
    reader_thread.join().expect("pty reader thread");
}
