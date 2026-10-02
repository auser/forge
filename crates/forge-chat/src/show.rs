//! On-demand rendering of recorded `tool_result` payloads: `/show`.
//!
//! `render.rs` owns the *live* event→line mapping, where `ToolResult` is
//! deliberately silent (§4.2); this module is its on-demand counterpart —
//! selection from a session's recorded events, then the same §4.1 gutters
//! applied to a verbatim payload. Pure: no store, no terminal, no clock.

use std::collections::HashMap;

use forge_core::{Event, EventKind};

use crate::io::Line;

/// One recorded tool result, selected from a session's log for `/show`.
#[derive(Debug)]
pub struct SelectedResult {
    /// 1 = the most recent tool result in the session.
    pub ordinal: usize,
    /// Total tool results recorded in the session.
    pub total: usize,
    pub tool: String,
    /// The friendly argument (`src/main.rs` in `read_file src/main.rs`),
    /// recovered from the assistant message that requested the call.
    pub args_hint: Option<String>,
    pub run_id: String,
    pub output: String,
    pub is_error: bool,
}

/// How many tool results `events` holds — for the out-of-range message.
pub fn count(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e.kind, EventKind::ToolResult { .. }))
        .count()
}

/// The nth most recent `tool_result` in `events` (1 = latest), with the
/// requesting call's friendly argument recovered by `call_id` pairing.
pub fn select(events: &[Event], ordinal: usize) -> Option<SelectedResult> {
    if ordinal == 0 {
        return None;
    }
    // Pair first, walking forward: `call_id` -> the requesting call's name
    // and arguments JSON, from every assistant message. `ToolCallRequested`
    // is *not* a pairing source — it carries no `call_id`.
    let mut calls: HashMap<&str, (&str, String)> = HashMap::new();
    for event in events {
        if let EventKind::AssistantMessage { tool_calls, .. } = &event.kind {
            for call in tool_calls {
                calls.insert(
                    call.id.as_str(),
                    (call.name.as_str(), call.arguments.to_string()),
                );
            }
        }
    }
    let total = count(events);
    let found = events
        .iter()
        .rev()
        .filter_map(|event| match &event.kind {
            EventKind::ToolResult {
                call_id,
                tool,
                output,
                is_error,
            } => Some((event.run_id.as_str(), call_id, tool, output, is_error)),
            _ => None,
        })
        .nth(ordinal - 1)?;
    let (run_id, call_id, tool, output, is_error) = found;
    // A bare tool name is the honest fallback when the requesting call
    // cannot be recovered (a pre-v3 log, or a call id nothing named).
    let args_hint = calls
        .get(call_id.as_str())
        .map(|(name, args)| crate::render::summarize_call(name, args))
        .filter(|hint| !hint.is_empty());
    Some(SelectedResult {
        ordinal,
        total,
        tool: tool.clone(),
        args_hint,
        run_id: run_id.to_string(),
        output: output.clone(),
        is_error: *is_error,
    })
}

/// The §4.1 grammar, on demand: a meta header naming what was selected,
/// then the verbatim payload under the result gutter — one `Line` per
/// payload line, `failed` (Bad style) when the call errored. No
/// display-side cap: the store's 64 KiB cap is the bound, and the payload
/// is exactly what was recorded (redaction and the truncation marker
/// included).
pub fn lines(result: &SelectedResult) -> Vec<Line> {
    let mut header = format!(
        "tool result {} of {}: {}",
        result.ordinal, result.total, result.tool
    );
    if let Some(hint) = &result.args_hint {
        header.push(' ');
        header.push_str(hint);
    }
    header.push_str(&format!(" (run {})", result.run_id));
    let mut out = vec![Line::meta(header)];
    let gutter = |line: &str| {
        if result.is_error {
            Line::failed(line)
        } else {
            Line::ok(line)
        }
    };
    // The writer emits line-at-a-time, so a multi-line payload is split
    // here rather than handed over with newlines inside. An empty payload
    // still gets one gutter line, so a header never dangles over nothing.
    if result.output.is_empty() {
        out.push(gutter("(no output)"));
    } else {
        out.extend(result.output.lines().map(gutter));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_core::{EventKind, ToolCall};
    use serde_json::json;

    use crate::io::Style;

    fn ev(run: &str, kind: EventKind) -> Event {
        Event::new(run, "sess-1", kind)
    }

    fn read_call(id: &str, path: &str) -> EventKind {
        EventKind::AssistantMessage {
            text: String::new(),
            tool_calls: vec![ToolCall::new(id, "read_file", json!({"path": path}))],
        }
    }

    fn read_result(call_id: &str, output: &str) -> EventKind {
        EventKind::ToolResult {
            call_id: call_id.into(),
            tool: "read_file".into(),
            output: output.into(),
            is_error: false,
        }
    }

    fn texts(lines: &[Line]) -> Vec<&str> {
        lines.iter().map(|l| l.text.as_str()).collect()
    }

    #[test]
    fn select_picks_the_nth_most_recent_result() {
        let events = vec![
            ev("run-1", read_call("c1", "a.rs")),
            ev("run-1", read_result("c1", "first")),
            ev("run-1", read_call("c2", "b.rs")),
            ev("run-1", read_result("c2", "second")),
        ];
        let latest = select(&events, 1).expect("a latest result");
        assert_eq!(latest.output, "second");
        assert_eq!(latest.total, 2);
        assert_eq!(latest.ordinal, 1);
        let earlier = select(&events, 2).expect("a second result");
        assert_eq!(earlier.output, "first");
        assert_eq!(earlier.total, 2);
        assert!(select(&events, 3).is_none(), "out of range");
        assert!(select(&events, 0).is_none(), "zero is not an ordinal");
        assert!(select(&[], 1).is_none(), "an empty log has nothing");
    }

    #[test]
    fn the_args_hint_is_recovered_by_call_id_pairing() {
        let events = vec![
            ev("run-9", read_call("c1", "src/main.rs")),
            ev("run-9", read_result("c1", "fn main() {}")),
        ];
        let selected = select(&events, 1).expect("selected");
        assert_eq!(selected.args_hint.as_deref(), Some("src/main.rs"));
        assert_eq!(selected.run_id, "run-9");
        let rendered = lines(&selected);
        let out = texts(&rendered);
        assert_eq!(
            out[0],
            "  - tool result 1 of 1: read_file src/main.rs (run run-9)"
        );
        assert_eq!(out[1], "    -> fn main() {}");
    }

    #[test]
    fn an_unpairable_call_id_falls_back_to_the_bare_tool_name() {
        let events = vec![ev("run-1", read_result("nobody-called-this", "data"))];
        let selected = select(&events, 1).expect("selected");
        assert_eq!(selected.args_hint, None);
        let out = lines(&selected);
        assert_eq!(
            texts(&out)[0],
            "  - tool result 1 of 1: read_file (run run-1)"
        );
    }

    #[test]
    fn a_multi_line_payload_renders_one_guttered_line_per_payload_line() {
        let events = vec![
            ev("run-1", read_call("c1", "a.rs")),
            ev("run-1", read_result("c1", "one\ntwo\nthree")),
        ];
        let out = lines(&select(&events, 1).expect("selected"));
        assert_eq!(
            texts(&out),
            vec![
                "  - tool result 1 of 1: read_file a.rs (run run-1)",
                "    -> one",
                "    -> two",
                "    -> three",
            ]
        );
        for line in &out[1..] {
            assert_eq!(line.style, Style::Ok);
            assert!(!line.text.contains('\n'), "one Line per payload line");
        }
    }

    #[test]
    fn an_error_result_renders_its_payload_in_the_bad_style() {
        let events = vec![ev(
            "run-1",
            EventKind::ToolResult {
                call_id: "c1".into(),
                tool: "run_command".into(),
                output: "boom".into(),
                is_error: true,
            },
        )];
        let out = lines(&select(&events, 1).expect("selected"));
        assert_eq!(out[1].text, "    -> boom");
        assert_eq!(out[1].style, Style::Bad);
    }

    #[test]
    fn an_empty_payload_never_leaves_the_header_dangling() {
        let events = vec![ev("run-1", read_result("c1", ""))];
        let out = lines(&select(&events, 1).expect("selected"));
        assert_eq!(texts(&out)[1], "    -> (no output)");
        let events = vec![ev(
            "run-1",
            EventKind::ToolResult {
                call_id: "c1".into(),
                tool: "read_file".into(),
                output: String::new(),
                is_error: true,
            },
        )];
        let out = lines(&select(&events, 1).expect("selected"));
        assert_eq!(texts(&out)[1], "    -> (no output)");
        assert_eq!(out[1].style, Style::Bad);
    }

    #[test]
    fn the_header_is_ascii_and_the_payload_passes_through_verbatim() {
        // The store's truncation marker (`cap_tool_output`) is payload text
        // like any other: it must survive untouched.
        let payload =
            "caf\u{e9} \u{4e2d}\u{6587}\n[forge: tool output truncated, 12 bytes dropped]";
        let events = vec![
            ev("run-1", read_call("c1", "a.rs")),
            ev("run-1", read_result("c1", payload)),
        ];
        let out = lines(&select(&events, 1).expect("selected"));
        assert!(out[0].text.is_ascii(), "header: {}", out[0].text);
        assert_eq!(out[1].text, "    -> caf\u{e9} \u{4e2d}\u{6587}");
        assert_eq!(
            out[2].text,
            "    -> [forge: tool output truncated, 12 bytes dropped]"
        );
    }
}
