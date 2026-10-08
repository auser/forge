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

use crate::io::{CompletionCandidate, CompletionSnapshot, Line};

/// The four approval policies (§9.1). One list, so the `/approval`
/// completion candidates and the value the controller accepts cannot drift.
pub const APPROVAL_MODES: &[&str] = &["auto", "prompt", "prompt-dangerous", "deny"];
pub const AUTH_PROVIDERS: &[&str] = &["claude", "codex", "kimi"];

/// Every command with the one-line description `/help` prints.
///
/// This is the single list: [`Command::complete`] offers these names and
/// [`help_lines`] renders them, so a command cannot exist in one place and
/// be missing from the other.
pub const COMMANDS: &[(&str, &str)] = &[
    ("/help", "every command, then the discovered skills"),
    ("/model", "list models, or /model <name> to switch"),
    ("/auth", "sign in: /auth <claude|codex|kimi>"),
    ("/approval", "show the approval policy, or /approval <mode>"),
    ("/config", "effective settings, or /config <key> for one"),
    ("/skills", "discovered skills, name and description"),
    ("/graph", "rank project files for a query"),
    ("/queue", "list queued messages, or remove <n>, or clear"),
    ("/session", "this session, or new, or /session <id>"),
    ("/fork", "fork this conversation, optionally --at <pos>"),
    ("/bg", "detach the running turn and keep talking"),
    ("/jobs", "runs and their states"),
    ("/attach", "follow a run again by id"),
    (
        "/show",
        "re-render a recorded tool result: /show [n], latest first",
    ),
    ("/quit", "leave (Ctrl-D does the same)"),
    ("/exit", "leave (Ctrl-D does the same)"),
];

/// Usage of `/fork`, named once: the parser reports it and nothing else
/// spells it out.
const FORK_USAGE: &str = "/fork [--at <pos|run-id>]";

/// At most this many path candidates per completion. The listing is
/// `CompletionType::List`, which is unusable past a screenful; typing one
/// more character narrows further, so a cap hides nothing reachable.
const MAX_PATH_CANDIDATES: usize = 100;

/// What one submitted line means. Deliberately data, not an effect: the
/// controller decides what is *allowed* in the current state (§6.5), and
/// this decides only what was said.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Parsed {
    /// A turn for the model.
    Prompt(String),
    /// A `/skill` line (§9.4): the skill's name travels as data so the
    /// runtime activates exactly it, never a lexical look-alike. `prompt`
    /// stays the user-turn text the model sees.
    Skill {
        name: String,
        prompt: String,
    },
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
    /// `None` shows the providers; `Some` starts subscription sign-in.
    Auth(Option<String>),
    /// `None` shows the policy; `Some` sets it.
    Approval(Option<String>),
    /// `None` is `forge config show`; `Some` is `forge config explain`.
    Config(Option<String>),
    Skills,
    /// `/graph <query>`, optionally `/graph <query> -- <steering>`.
    Graph(String, Option<String>),
    /// Inspect or change prompts waiting behind the active turn.
    Queue(QueueCommand),
    /// `/session` with no argument: report the current one.
    Session,
    SessionNew,
    SessionSwitch(String),
    /// `/fork`, optionally `--at <pos|run-id>`.
    Fork(Option<String>),
    Background,
    Jobs,
    Attach(String),
    /// `/show`, optionally `/show <n>`: the nth most recent tool result (1 = latest).
    Show(Option<usize>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QueueCommand {
    List,
    Remove(usize),
    Clear,
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
            "auth" => Parsed::Auth(argument),
            "approval" => Parsed::Approval(argument),
            "config" => Parsed::Config(argument),
            "skills" => Parsed::Skills,
            "graph" => match argument {
                Some(text) => {
                    let text = text.trim();
                    // `/graph <query> -- <steering>`: the first ` -- `
                    // separates the query from the free-text steering.
                    // Steering with no query (`/graph -- x`) is a usage
                    // error either way it is spaced.
                    if text.starts_with("-- ") || text == "--" {
                        Parsed::Usage("/graph <query>")
                    } else {
                        let (query, steering) = match text.split_once(" -- ") {
                            Some((query, steering)) => {
                                (query.trim().to_string(), steering.trim().to_string())
                            }
                            None => (text.to_string(), String::new()),
                        };
                        if query.is_empty() {
                            Parsed::Usage("/graph <query>")
                        } else {
                            Parsed::Graph(
                                query,
                                if steering.is_empty() {
                                    None
                                } else {
                                    Some(steering)
                                },
                            )
                        }
                    }
                }
                None => Parsed::Usage("/graph <query>"),
            },
            "queue" => match argument.as_deref() {
                None => Parsed::Queue(QueueCommand::List),
                Some("clear") => Parsed::Queue(QueueCommand::Clear),
                Some(text) => match text
                    .strip_prefix("remove ")
                    .and_then(|position| position.trim().parse::<usize>().ok())
                {
                    Some(position) if position > 0 => Parsed::Queue(QueueCommand::Remove(position)),
                    _ => Parsed::Usage("/queue [remove <n>|clear]"),
                },
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
            "show" => match argument {
                None => Parsed::Show(None),
                Some(text) => match text.parse::<usize>() {
                    Ok(n) => Parsed::Show(Some(n)),
                    Err(_) => Parsed::Usage("/show [n]"),
                },
            },
            // Commands win over skills, so a skill cannot shadow `/help`.
            _ if snapshot.skills.iter().any(|(skill, _)| skill == name) => Parsed::Skill {
                name: name.to_string(),
                prompt: skill_prompt(name, &rest),
            },
            _ => Parsed::Unknown(name.to_string()),
        }
    }

    /// What `Tab` offers at `pos`, and the byte offset the offered items
    /// replace from (§9.2). Candidates carry a display string beside the
    /// replacement, so the listing shows what each command *does* — the
    /// difference between a list of names and a menu.
    ///
    /// `pos` is a byte offset into `line`, as rustyline reports it; a
    /// nonsensical one is clamped rather than panicking, because this runs
    /// on the editor thread and a completer must never take the process
    /// down.
    pub fn complete(
        line: &str,
        pos: usize,
        snapshot: &CompletionSnapshot,
    ) -> (usize, Vec<CompletionCandidate>) {
        let head = &line[..clamp_boundary(line, pos)];
        let word_start = head
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map(|(index, c)| index + c.len_utf8())
            .unwrap_or(0);
        let word = &head[word_start..];

        // A word starting with `@` is a file reference and completes project
        // paths, at any position — first word of a prompt, mid-prompt, or a
        // command argument. The trigger is explicit intent, which is what
        // makes offering always safe: unmarked words never complete as
        // paths (the design doc's bar: half-working path completion is
        // worse than none). Paths containing whitespace are never offered —
        // they cannot round-trip this completer's own whitespace
        // word-splitting.
        if let Some(prefix) = word.strip_prefix('@') {
            let candidates = snapshot
                .paths
                .iter()
                .filter(|path| !path.chars().any(char::is_whitespace))
                .filter(|path| path.starts_with(prefix))
                .take(MAX_PATH_CANDIDATES)
                .map(|path| {
                    // The replacement keeps the `@`: inserting the bare
                    // path would delete the user's trigger character.
                    let replacement = format!("@{path}");
                    CompletionCandidate {
                        display: replacement.clone(),
                        replacement,
                    }
                })
                .collect();
            return (word_start, candidates);
        }

        // The first word: every command plus every discovered skill. Only
        // with a leading slash — nothing guesses inside a prompt.
        if word_start == 0 {
            if !word.starts_with('/') {
                return (word_start, Vec::new());
            }
            let candidates = COMMANDS
                .iter()
                .map(|(name, description)| (name.to_string(), description.to_string()))
                .chain(
                    snapshot
                        .skills
                        .iter()
                        .map(|(name, description)| (format!("/{name}"), description.clone())),
                );
            return (word_start, described(candidates, word));
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
            "/auth" => AUTH_PROVIDERS
                .iter()
                .map(|provider| (*provider).to_string())
                .collect(),
            "/approval" => APPROVAL_MODES.iter().map(|m| (*m).to_string()).collect(),
            "/attach" => snapshot.jobs.clone(),
            "/session" => std::iter::once("new".to_string())
                .chain(snapshot.sessions.iter().cloned())
                .collect(),
            // Path completion lives in the `@` branch above, deliberately
            // the only in-prompt completion: nothing here guesses at paths.
            _ => Vec::new(),
        };
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

/// The prompt a `/skill` line carries (§9.4).
///
/// This is model context, not the activation mechanism: activation is the
/// [`Parsed::Skill`] name travelling to `RunOptions::activate_skills`,
/// which works for any name — including one shorter than the three
/// characters `SkillRegistry::match_task`'s tokenizer requires. The
/// template stays because it tells the model the user invoked the skill by
/// name (the same text a working lexical match used to produce, so that
/// case is unchanged), and because it gives a bare `/name` a non-empty
/// prompt.
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

/// Prefix-filter (name, description) pairs, then align the descriptions
/// into a column: `/help   every command, then the discovered skills`.
fn described(
    candidates: impl Iterator<Item = (String, String)>,
    word: &str,
) -> Vec<CompletionCandidate> {
    let matches: Vec<(String, String)> = candidates
        .filter(|(name, _)| name.starts_with(word))
        .collect();
    let width = matches
        .iter()
        .map(|(name, _)| name.len())
        .max()
        .unwrap_or(0);
    matches
        .into_iter()
        .map(|(name, description)| CompletionCandidate {
            display: if description.is_empty() {
                name.clone()
            } else {
                format!("{name:<width$}  {description}")
            },
            replacement: name,
        })
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
            skills: vec![
                ("tdd".into(), "test-driven development".into()),
                ("code-reviewer".into(), "review the diff".into()),
            ],
            models: vec!["qwen3-coder".into(), "deepseek-chat".into()],
            jobs: vec!["01JCF4ABC".into()],
            sessions: vec!["01JCF3XYZ".into()],
            // Sorted, which is the host's contract: the graph's `BTreeMap`
            // keys arrive sorted and the completer preserves that order
            // rather than re-sorting.
            paths: vec![
                "docs/guide.md".into(),
                "src/lib.rs".into(),
                "src/main.rs".into(),
                "with space.rs".into(),
            ],
        }
    }

    fn replacements(items: &[CompletionCandidate]) -> Vec<&str> {
        items.iter().map(|c| c.replacement.as_str()).collect()
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
        assert_eq!(Command::parse("/auth", &s), Parsed::Auth(None));
        assert_eq!(
            Command::parse("/auth codex", &s),
            Parsed::Auth(Some("codex".into()))
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
            Parsed::Graph("auth flow".into(), None)
        );
        assert_eq!(
            Command::parse("/graph auth flow -- prefer tests", &s),
            Parsed::Graph("auth flow".into(), Some("prefer tests".into()))
        );
        // A bare separator is not steering; an empty query is still a usage
        // error.
        assert_eq!(
            Command::parse("/graph -- prefer tests", &s),
            Parsed::Usage("/graph <query>")
        );
        assert_eq!(
            Command::parse("/queue", &s),
            Parsed::Queue(QueueCommand::List)
        );
        assert_eq!(
            Command::parse("/queue remove 2", &s),
            Parsed::Queue(QueueCommand::Remove(2))
        );
        assert_eq!(
            Command::parse("/queue clear", &s),
            Parsed::Queue(QueueCommand::Clear)
        );
        assert_eq!(
            Command::parse("/queue remove 0", &s),
            Parsed::Usage("/queue [remove <n>|clear]")
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
        assert_eq!(Command::parse("/show", &s), Parsed::Show(None));
        assert_eq!(Command::parse("/show 2", &s), Parsed::Show(Some(2)));
        assert_eq!(Command::parse("  /show  3 ", &s), Parsed::Show(Some(3)));
        // Zero parses (it is a `usize`); the *driver* answers it with the
        // informational out-of-range line, which teaches the numbering.
        assert_eq!(Command::parse("/show 0", &s), Parsed::Show(Some(0)));
    }

    /// A `/skill` line keeps the skill's name as data — the runtime
    /// activates exactly it — while the template stays the prompt text.
    #[test]
    fn a_skill_parses_as_a_skill_with_its_prompt() {
        let s = snapshot();
        assert_eq!(
            Command::parse("/tdd write the failing test", &s),
            Parsed::Skill {
                name: "tdd".into(),
                prompt: "Use the tdd skill.\n\nwrite the failing test".into(),
            }
        );
        assert_eq!(
            Command::parse("/code-reviewer", &s),
            Parsed::Skill {
                name: "code-reviewer".into(),
                prompt: "Use the code-reviewer skill.".into(),
            }
        );
    }

    /// The case lexical matching could never serve: `match_task` drops
    /// words under 3 characters, so a two-letter skill was unreachable as a
    /// slash command until the name became data.
    #[test]
    fn a_two_character_skill_name_parses_as_a_skill() {
        let s = CompletionSnapshot {
            skills: vec![("xy".into(), "the two-letter skill".into())],
            ..CompletionSnapshot::default()
        };
        assert_eq!(
            Command::parse("/xy do the thing", &s),
            Parsed::Skill {
                name: "xy".into(),
                prompt: "Use the xy skill.\n\ndo the thing".into(),
            }
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
        let names = replacements(&items);
        assert!(names.contains(&"/session"));
        assert!(names.contains(&"/skills"));
        assert!(!names.contains(&"/help"));
        let (_, skills) = Command::complete("/t", 2, &s);
        assert!(
            replacements(&skills).contains(&"/tdd"),
            "skills complete too: {skills:?}"
        );
    }

    /// The listing shows what a command does; only the name is inserted.
    #[test]
    fn completion_displays_descriptions_but_replaces_with_names() {
        let s = snapshot();
        let (_, items) = Command::complete("/skills", 7, &s);
        let skills = items
            .iter()
            .find(|c| c.replacement == "/skills")
            .expect("the /skills candidate");
        assert_eq!(skills.replacement, "/skills");
        assert!(
            skills.display.starts_with("/skills"),
            "display leads with the name: {skills:?}"
        );
        assert!(
            skills.display.contains("discovered skills"),
            "display carries the description: {skills:?}"
        );
        let (_, items) = Command::complete("/tdd", 4, &s);
        let tdd = items
            .iter()
            .find(|c| c.replacement == "/tdd")
            .expect("the /tdd candidate");
        assert_eq!(tdd.display, "/tdd  test-driven development");
    }

    #[test]
    fn completion_offers_arguments_per_command() {
        let s = snapshot();
        assert_eq!(
            replacements(&Command::complete("/model ", 7, &s).1),
            vec!["qwen3-coder", "deepseek-chat"]
        );
        assert_eq!(
            replacements(&Command::complete("/attach ", 8, &s).1),
            vec!["01JCF4ABC"]
        );
        assert!(
            replacements(&Command::complete("/approval ", 10, &s).1).contains(&"prompt-dangerous")
        );
        assert_eq!(
            replacements(&Command::complete("/auth ", 6, &s).1),
            vec!["claude", "codex", "kimi"]
        );
        // Unmarked words complete nothing — path completion is the `@`
        // branch's job, and guessing inside a prompt is still forbidden.
        assert!(Command::complete("explain the ", 12, &s).1.is_empty());
    }

    /// Review Focus 2: the rule is total — word 0, mid-prompt, and after a
    /// command all behave identically.
    #[test]
    fn an_at_word_completes_project_paths_anywhere_in_the_line() {
        let s = snapshot();
        for (line, pos) in [
            ("@src/ma", 7),          // word 0
            ("explain @src/ma", 15), // mid-prompt
            ("/graph @src/ma", 14),  // a command argument
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
        assert_eq!(
            items[0].display, "@src/lib.rs",
            "a value displays as itself"
        );
        assert!(items[0].replacement.starts_with('@'));
        assert_eq!(items[0].replacement, format!("@{}", "src/lib.rs"));
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

    /// The completer invents nothing: the argument candidates are exactly the
    /// snapshot's (`ChatHost::models` is the one place test-only entries are
    /// filtered, §9.3) and the command candidates are exactly the compiled-in
    /// table plus the snapshot's skills. Set equality with the sources is the
    /// property that matters; the absence of any particular substring is not.
    #[test]
    fn completion_offers_only_the_snapshot_and_the_command_table() {
        let s = snapshot();
        assert_eq!(
            replacements(&Command::complete("/model ", 7, &s).1),
            ["qwen3-coder", "deepseek-chat"]
        );
        assert_eq!(
            replacements(&Command::complete("/attach ", 8, &s).1),
            ["01JCF4ABC"]
        );
        assert_eq!(
            replacements(&Command::complete("/approval ", 10, &s).1),
            APPROVAL_MODES.to_vec()
        );
        let expected: Vec<String> = COMMANDS
            .iter()
            .map(|(name, _)| (*name).to_string())
            .chain(s.skills.iter().map(|(name, _)| format!("/{name}")))
            .collect();
        let names: Vec<String> = replacements(&Command::complete("/", 1, &s).1)
            .into_iter()
            .map(str::to_string)
            .collect();
        assert_eq!(names, expected);
    }

    #[test]
    fn session_completion_offers_new_and_the_recent_sessions() {
        let s = snapshot();
        assert_eq!(
            replacements(&Command::complete("/session ", 9, &s).1),
            vec!["new", "01JCF3XYZ"]
        );
        let (start, items) = Command::complete("/session 01J", 12, &s);
        assert_eq!(start, 9);
        assert_eq!(replacements(&items), vec!["01JCF3XYZ"]);
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
        // …and the same survival holds inside an `@` word.
        let line = "@caf\u{e9}";
        let (start, _) = Command::complete(line, line.len() - 1, &s);
        assert!(line.is_char_boundary(start), "start {start} splits a char");
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
        // `/show`'s argument is optional, but a non-numeric one is a usage
        // error rather than a guess at which result was meant.
        assert_eq!(Command::parse("/show abc", &s), Parsed::Usage("/show [n]"));
        assert_eq!(Command::parse("/show -1", &s), Parsed::Usage("/show [n]"));
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
