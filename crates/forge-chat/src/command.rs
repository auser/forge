//! Slash parsing and Tab completion, as pure code (design §9).
//!
//! Two entry points, both pure functions of a line and a
//! [`CompletionSnapshot`]: [`Command::parse`] decides what one submitted
//! line *means*, and [`Command::complete`] decides what `Tab` offers. The
//! snapshot travels inside [`crate::io::Prompt`], so the completer never
//! calls back into the host and never touches the filesystem from the
//! editor thread.
//!
//! The rule that keeps a prompt a prompt: a line is a command only when it
//! starts with `/` **and** its first token names a known command or a
//! discovered skill. "Starts with a slash" is not the test — otherwise
//! `/usr/bin/env is on PATH?` would be sent nowhere useful. An
//! unrecognised `/word` is an error rather than a prompt, because silently
//! sending a mistyped command to a model is how you pay for a typo.

use crate::io::{CompletionSnapshot, Line};

/// The four approval policies (§9.1). One list, so the `/approval`
/// completion candidates and the value the controller accepts cannot drift.
pub const APPROVAL_MODES: &[&str] = &["auto", "prompt", "prompt-dangerous", "deny"];

/// Every command with the one-line description `/help` prints.
///
/// This is the single list: [`Command::complete`] offers these names and
/// [`help_lines`] renders them, so a command cannot exist in one place and
/// be missing from the other.
pub const COMMANDS: &[(&str, &str)] = &[
    ("/help", "every command, then the discovered skills"),
    ("/model", "list models, or /model <name> to switch"),
    ("/approval", "show the approval policy, or /approval <mode>"),
    ("/config", "effective settings, or /config <key> for one"),
    ("/skills", "discovered skills, name and description"),
    ("/graph", "rank project files for a query"),
    ("/session", "this session, or new, or /session <id>"),
    ("/fork", "fork this conversation, optionally --at <pos>"),
    ("/bg", "detach the running turn and keep talking"),
    ("/jobs", "runs and their states"),
    ("/attach", "follow a run again by id"),
    ("/quit", "leave (Ctrl-D does the same)"),
    ("/exit", "leave (Ctrl-D does the same)"),
];

/// Usage of `/fork`, named once: the parser reports it and nothing else
/// spells it out.
const FORK_USAGE: &str = "/fork [--at <pos|run-id>]";

/// What one submitted line means. Deliberately data, not an effect: the
/// controller decides what is *allowed* in the current state (§6.5), and
/// this decides only what was said.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Parsed {
    /// A turn for the model. A `/skill` line is one of these too — it is a
    /// normal prompt naming the skill (§9.4), not a second activation
    /// mechanism.
    Prompt(String),
    /// A `/word` that names nothing. Carries the name *without* its slash.
    Unknown(String),
    /// A known command whose required argument is missing; carries the
    /// usage line to print.
    Usage(&'static str),
    Help,
    /// `/quit` and `/exit`, which are the same request.
    Quit,
    /// `None` lists the candidates; `Some` switches.
    Model(Option<String>),
    /// `None` shows the policy; `Some` sets it.
    Approval(Option<String>),
    /// `None` is `forge config show`; `Some` is `forge config explain`.
    Config(Option<String>),
    Skills,
    Graph(String),
    /// `/session` with no argument: report the current one.
    Session,
    SessionNew,
    SessionSwitch(String),
    /// `/fork`, optionally `--at <pos|run-id>`.
    Fork(Option<String>),
    Background,
    Jobs,
    Attach(String),
}

/// Namespace for the two pure entry points. A unit struct rather than free
/// functions so the call sites read as `Command::parse` / `Command::complete`
/// — and so this `Command` is visibly a different thing from
/// `forge_cli::cli::Command`, the clap subcommand enum.
pub struct Command;

impl Command {
    /// What one submitted line means.
    ///
    /// Leading and trailing whitespace is trimmed first, so `  /quit  ` is
    /// `/quit`. The body of a prompt is otherwise untouched: a pasted
    /// fenced code block keeps its newlines and is one turn.
    pub fn parse(line: &str, snapshot: &CompletionSnapshot) -> Parsed {
        let trimmed = line.trim();
        let Some(name) = command_name(trimmed) else {
            return Parsed::Prompt(trimmed.to_string());
        };
        // `name` is a slice of `trimmed` starting after the `/`, so this is
        // the token's end — `get` rather than an index anyway, because a
        // parser that can panic is a parser that can take the terminal down.
        let rest = trimmed
            .get(name.len() + 1..)
            .unwrap_or_default()
            .trim()
            .to_string();
        let argument = (!rest.is_empty()).then(|| rest.clone());

        match name {
            "help" => Parsed::Help,
            "quit" | "exit" => Parsed::Quit,
            "model" => Parsed::Model(argument),
            "approval" => Parsed::Approval(argument),
            "config" => Parsed::Config(argument),
            "skills" => Parsed::Skills,
            "graph" => match argument {
                Some(query) => Parsed::Graph(query),
                None => Parsed::Usage("/graph <query>"),
            },
            "session" => match argument.as_deref() {
                None => Parsed::Session,
                Some("new") => Parsed::SessionNew,
                Some(id) => Parsed::SessionSwitch(id.to_string()),
            },
            "fork" => parse_fork(argument.as_deref()),
            "bg" => Parsed::Background,
            "jobs" => Parsed::Jobs,
            "attach" => match argument {
                Some(run_id) => Parsed::Attach(run_id),
                None => Parsed::Usage("/attach <run-id>"),
            },
            // Commands win over skills, so a skill cannot shadow `/help`.
            _ if snapshot.skills.iter().any(|skill| skill == name) => {
                Parsed::Prompt(skill_prompt(name, &rest))
            }
            _ => Parsed::Unknown(name.to_string()),
        }
    }

    /// What `Tab` offers at `pos`, and the byte offset the offered items
    /// replace from (§9.2).
    ///
    /// `pos` is a byte offset into `line`, as rustyline reports it; a
    /// nonsensical one is clamped rather than panicking, because this runs
    /// on the editor thread and a completer must never take the process
    /// down.
    pub fn complete(line: &str, pos: usize, snapshot: &CompletionSnapshot) -> (usize, Vec<String>) {
        let head = &line[..clamp_boundary(line, pos)];
        let word_start = head
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map(|(index, c)| index + c.len_utf8())
            .unwrap_or(0);
        let word = &head[word_start..];

        // The first word: every command plus every discovered skill. Only
        // with a leading slash — nothing guesses inside a prompt.
        if word_start == 0 {
            if !word.starts_with('/') {
                return (word_start, Vec::new());
            }
            let candidates = COMMANDS
                .iter()
                .map(|(name, _)| (*name).to_string())
                .chain(snapshot.skills.iter().map(|skill| format!("/{skill}")));
            return (word_start, filtered(candidates, word));
        }

        // An argument, and only the *first* one: `/model a b` completes
        // nothing, because there is no second argument to any command.
        let mut before = head[..word_start].split_whitespace();
        let Some(command) = before.next() else {
            return (word_start, Vec::new());
        };
        if before.next().is_some() {
            return (word_start, Vec::new());
        }
        let candidates: Vec<String> = match command {
            "/model" => snapshot.models.clone(),
            "/approval" => APPROVAL_MODES.iter().map(|m| (*m).to_string()).collect(),
            "/attach" => snapshot.jobs.clone(),
            "/session" => std::iter::once("new".to_string())
                .chain(snapshot.sessions.iter().cloned())
                .collect(),
            // No file-path completion in v1: forge reads files through
            // tools, and half-working path completion is worse than none.
            _ => Vec::new(),
        };
        (word_start, filtered(candidates.into_iter(), word))
    }

    /// Was this line *meant* as a command?
    ///
    /// Deliberately independent of the snapshot: at an approval prompt a
    /// bare word is the answer and anything else that is not `y`/`yes`
    /// denies (§6.5), so if this depended on which skills exist, a mistyped
    /// `/tddd` would silently be read as a denial. Whether a slash word
    /// names something is [`Command::parse`]'s business; whether it was
    /// aimed at forge is this one's.
    pub fn looks_like_a_command(line: &str) -> bool {
        command_name(line).is_some()
    }
}

/// The command name a line names, without its `/`, or `None` if the line is
/// a prompt.
///
/// The one place the "is this a command at all?" rule lives, so
/// [`Command::parse`] and [`Command::looks_like_a_command`] cannot disagree
/// about it. A second slash makes the token a path, and a path is a prompt:
/// `/usr/bin/env is on PATH?` is a question.
fn command_name(line: &str) -> Option<&str> {
    let name = line.split_whitespace().next()?.strip_prefix('/')?;
    (!name.contains('/')).then_some(name)
}

/// `/help`'s command list, from [`COMMANDS`]. The caller appends the
/// discovered skills, which only the host knows.
///
/// Left-padded to one column: nothing is right-aligned and there is no
/// rule, so a narrow terminal wraps the description and loses nothing
/// (§12.1).
pub fn help_lines() -> Vec<Line> {
    COMMANDS
        .iter()
        .map(|(name, description)| Line::meta(format!("{name:<10} {description}")))
        .collect()
}

/// The prompt a `/skill` line becomes (§9.4).
///
/// The template matters: `SkillRegistry::match_task` matches
/// whitespace-separated words of >= 3 characters against the skill's name
/// and description, so the bare name has to appear as its own word for the
/// existing activation path to fire.
fn skill_prompt(name: &str, rest: &str) -> String {
    if rest.is_empty() {
        format!("Use the {name} skill.")
    } else {
        format!("Use the {name} skill.\n\n{rest}")
    }
}

/// `/fork` takes `--at <pos|run-id>` or nothing. A bare argument is a usage
/// error rather than a guess: `/fork 18` could plausibly mean a position or
/// a session id, and forking at the wrong point is not a cheap mistake.
///
/// The flag has to *end* at `--at`: matching it as a bare prefix read
/// `/fork --atomic` as `--at omic` and forked at a position nobody typed.
/// An unrecognised flag is a usage error, like any other.
fn parse_fork(argument: Option<&str>) -> Parsed {
    match argument {
        None => Parsed::Fork(None),
        Some(rest) => match rest.strip_prefix("--at") {
            Some(at) if at.starts_with(char::is_whitespace) && !at.trim().is_empty() => {
                Parsed::Fork(Some(at.trim().to_string()))
            }
            _ => Parsed::Usage(FORK_USAGE),
        },
    }
}

fn filtered(candidates: impl Iterator<Item = String>, word: &str) -> Vec<String> {
    candidates
        .filter(|candidate| candidate.starts_with(word))
        .collect()
}

/// The largest byte offset <= `pos` that is a char boundary of `line`.
fn clamp_boundary(line: &str, pos: usize) -> usize {
    let mut pos = pos.min(line.len());
    while !line.is_char_boundary(pos) {
        pos -= 1;
    }
    pos
}

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
        assert_eq!(
            Command::parse("explain the parser", &snapshot()),
            Parsed::Prompt("explain the parser".into())
        );
    }

    /// A path is not a command. This is why the first token is matched
    /// against known names instead of "starts with a slash".
    #[test]
    fn a_line_starting_with_a_path_is_a_prompt() {
        assert_eq!(
            Command::parse("/usr/bin/env is on PATH?", &snapshot()),
            Parsed::Prompt("/usr/bin/env is on PATH?".into())
        );
    }

    #[test]
    fn an_unknown_slash_word_is_an_error_not_a_prompt() {
        assert_eq!(
            Command::parse("/wat", &snapshot()),
            Parsed::Unknown("wat".into())
        );
    }

    #[test]
    fn commands_parse_with_and_without_arguments() {
        let s = snapshot();
        assert_eq!(Command::parse("/help", &s), Parsed::Help);
        assert_eq!(Command::parse("  /quit  ", &s), Parsed::Quit);
        assert_eq!(Command::parse("/exit", &s), Parsed::Quit);
        assert_eq!(Command::parse("/model", &s), Parsed::Model(None));
        assert_eq!(
            Command::parse("/model deepseek-chat", &s),
            Parsed::Model(Some("deepseek-chat".into()))
        );
        assert_eq!(
            Command::parse("/approval deny", &s),
            Parsed::Approval(Some("deny".into()))
        );
        assert_eq!(
            Command::parse("/config model", &s),
            Parsed::Config(Some("model".into()))
        );
        assert_eq!(
            Command::parse("/graph auth flow", &s),
            Parsed::Graph("auth flow".into())
        );
        assert_eq!(Command::parse("/session new", &s), Parsed::SessionNew);
        assert_eq!(
            Command::parse("/session 01JCF3XYZ", &s),
            Parsed::SessionSwitch("01JCF3XYZ".into())
        );
        assert_eq!(Command::parse("/fork", &s), Parsed::Fork(None));
        assert_eq!(
            Command::parse("/fork --at 18", &s),
            Parsed::Fork(Some("18".into()))
        );
        assert_eq!(Command::parse("/bg", &s), Parsed::Background);
        assert_eq!(Command::parse("/jobs", &s), Parsed::Jobs);
        assert_eq!(
            Command::parse("/attach 01JCF4ABC", &s),
            Parsed::Attach("01JCF4ABC".into())
        );
    }

    /// The prompt template matters: `SkillRegistry::match_task` matches
    /// whitespace-separated words of >=3 chars against the skill name, so
    /// the bare name must appear as its own word.
    #[test]
    fn a_skill_becomes_a_prompt_that_activates_it() {
        let s = snapshot();
        assert_eq!(
            Command::parse("/tdd write the failing test", &s),
            Parsed::Prompt("Use the tdd skill.\n\nwrite the failing test".into())
        );
        assert_eq!(
            Command::parse("/code-reviewer", &s),
            Parsed::Prompt("Use the code-reviewer skill.".into())
        );
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
        assert!(
            skills.contains(&"/tdd".to_string()),
            "skills complete too: {skills:?}"
        );
    }

    #[test]
    fn completion_offers_arguments_per_command() {
        let s = snapshot();
        assert_eq!(
            Command::complete("/model ", 7, &s).1,
            vec!["qwen3-coder".to_string(), "deepseek-chat".to_string()]
        );
        assert_eq!(
            Command::complete("/attach ", 8, &s).1,
            vec!["01JCF4ABC".to_string()]
        );
        assert!(
            Command::complete("/approval ", 10, &s)
                .1
                .contains(&"prompt-dangerous".to_string())
        );
        // No path completion in v1, and no guessing inside a prompt.
        assert!(Command::complete("explain the ", 12, &s).1.is_empty());
    }

    /// The completer invents nothing: the argument candidates are exactly the
    /// snapshot's (`ChatHost::models` is the one place test-only entries are
    /// filtered, §9.3) and the command candidates are exactly the compiled-in
    /// table plus the snapshot's skills. Set equality with the sources is the
    /// property that matters; the absence of any particular substring is not.
    #[test]
    fn completion_offers_only_the_snapshot_and_the_command_table() {
        let s = snapshot();
        assert_eq!(Command::complete("/model ", 7, &s).1, s.models);
        assert_eq!(Command::complete("/attach ", 8, &s).1, s.jobs);
        assert_eq!(
            Command::complete("/approval ", 10, &s).1,
            APPROVAL_MODES
                .iter()
                .map(|m| m.to_string())
                .collect::<Vec<_>>()
        );
        let expected: Vec<String> = COMMANDS
            .iter()
            .map(|(name, _)| name.to_string())
            .chain(s.skills.iter().map(|skill| format!("/{skill}")))
            .collect();
        assert_eq!(Command::complete("/", 1, &s).1, expected);
    }

    #[test]
    fn session_completion_offers_new_and_the_recent_sessions() {
        let s = snapshot();
        assert_eq!(
            Command::complete("/session ", 9, &s).1,
            vec!["new".to_string(), "01JCF3XYZ".to_string()]
        );
        assert_eq!(
            Command::complete("/session 01J", 12, &s),
            (9, vec!["01JCF3XYZ".to_string()])
        );
    }

    /// Completion is for the *current* word only: a third token completes
    /// nothing, and neither does a command with no candidates of its own.
    #[test]
    fn completion_stops_after_the_first_argument() {
        let s = snapshot();
        assert!(
            Command::complete("/model qwen3-coder ", 19, &s)
                .1
                .is_empty()
        );
        assert!(Command::complete("/config ", 8, &s).1.is_empty());
        assert!(Command::complete("/graph auth ", 12, &s).1.is_empty());
    }

    /// A completer runs on the editor thread, where a panic would take the
    /// terminal with it. A position past the end, or inside a multi-byte
    /// char, has to be survivable.
    #[test]
    fn completion_survives_a_nonsense_position() {
        let s = snapshot();
        assert_eq!(
            Command::complete("/s", 99, &s).1,
            Command::complete("/s", 2, &s).1
        );
        // A position inside the 2-byte `é` clamps down to its start.
        let line = "/model caf\u{e9}";
        let (start, items) = Command::complete(line, line.len() - 1, &s);
        assert!(line.is_char_boundary(start), "start {start} splits a char");
        assert!(items.is_empty(), "no model is called caf...: {items:?}");
        assert!(Command::complete("", 0, &s).1.is_empty());
    }

    /// A slash command may be typed at an approval prompt, where a bare
    /// word is the answer. Whether a line was *meant* as a command must not
    /// depend on which skills exist, or a mistyped `/tdd` would read as a
    /// denial (§6.5).
    #[test]
    fn a_command_is_told_apart_from_an_answer_without_the_snapshot() {
        assert!(Command::looks_like_a_command("/jobs"));
        assert!(Command::looks_like_a_command("  /wat  "));
        assert!(Command::looks_like_a_command("/tddd write a test"));
        assert!(Command::looks_like_a_command("/"));
        assert!(!Command::looks_like_a_command("n"));
        assert!(!Command::looks_like_a_command(""));
        assert!(!Command::looks_like_a_command("/usr/bin/env is on PATH?"));
    }

    #[test]
    fn a_bare_slash_is_an_unknown_command_not_a_prompt() {
        assert_eq!(
            Command::parse("/", &snapshot()),
            Parsed::Unknown(String::new())
        );
    }

    /// A required argument that is missing gets a usage line, not a turn
    /// sent to a model.
    #[test]
    fn a_command_missing_its_argument_reports_its_usage() {
        let s = snapshot();
        assert_eq!(
            Command::parse("/attach", &s),
            Parsed::Usage("/attach <run-id>")
        );
        assert_eq!(
            Command::parse("/graph", &s),
            Parsed::Usage("/graph <query>")
        );
        assert_eq!(Command::parse("/fork --at", &s), Parsed::Usage(FORK_USAGE));
        assert_eq!(Command::parse("/fork 18", &s), Parsed::Usage(FORK_USAGE));
        // The flag ends at `--at`: matched as a bare prefix, this parsed as
        // `Fork(Some("omic"))` and forked at a position nobody typed.
        assert_eq!(
            Command::parse("/fork --atomic", &s),
            Parsed::Usage(FORK_USAGE)
        );
        assert_eq!(
            Command::parse("/fork --at-18", &s),
            Parsed::Usage(FORK_USAGE)
        );
    }

    #[test]
    fn help_renders_one_line_per_command_and_nothing_else() {
        let lines = help_lines();
        assert_eq!(lines.len(), COMMANDS.len());
        for (line, (name, description)) in lines.iter().zip(COMMANDS) {
            assert!(line.text.contains(name), "{}", line.text);
            assert!(line.text.contains(description), "{}", line.text);
            assert!(line.text.is_ascii(), "{}", line.text);
        }
    }

    /// Every command the `/help` table names has to parse, or `/help` is
    /// advertising something that does not work.
    #[test]
    fn every_advertised_command_parses() {
        let s = CompletionSnapshot::default();
        for (name, _) in COMMANDS {
            let parsed = Command::parse(name, &s);
            assert!(
                !matches!(parsed, Parsed::Unknown(_) | Parsed::Prompt(_)),
                "{name} parses as {parsed:?}"
            );
        }
    }
}
