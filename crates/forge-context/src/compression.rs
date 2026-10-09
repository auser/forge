//! Pure, conservative transformations of already-sanitized tool output.
//! Arguments are classification hints only and are never copied into a view.
use forge_core::{ToolCall, events::MAX_TOOL_OUTPUT_BYTES};
use serde::{Deserialize, Serialize};

use crate::{ArtifactRef, ContextSize, SanitizedOutput};

pub const COMPRESSION_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompressionKind {
    Unknown,
    Log,
    Search,
    Json,
    Jsonl,
    Table,
    Diff,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompressionReason {
    BelowThreshold,
    Unsupported,
    NoSavings,
    Compressed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompressionDecision {
    pub version: u32,
    pub kind: CompressionKind,
    pub reason: CompressionReason,
    pub original: ContextSize,
    pub baseline: ContextSize,
    pub view: ContextSize,
    pub omitted: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompressionResult {
    /// `None` means retain the caller's exact baseline.
    pub view: Option<String>,
    pub decision: CompressionDecision,
}

pub fn compress_tool_output(
    call: &ToolCall,
    output: &SanitizedOutput,
    artifact: &ArtifactRef,
    baseline: &str,
) -> CompressionResult {
    let mut result = compress_at(call, output, artifact, baseline, MAX_TOOL_OUTPUT_BYTES);
    // The checked-in corpus has not qualified structured records or diff for rollout.
    if matches!(
        result.decision.kind,
        CompressionKind::Json
            | CompressionKind::Jsonl
            | CompressionKind::Table
            | CompressionKind::Diff
    ) {
        result.view = None;
        result.decision.reason = CompressionReason::Unsupported;
        result.decision.view = result.decision.baseline;
        result.decision.omitted = 0;
    }
    result
}

fn compress_at(
    call: &ToolCall,
    output: &SanitizedOutput,
    artifact: &ArtifactRef,
    baseline: &str,
    threshold: usize,
) -> CompressionResult {
    let text = output.as_str();
    let mut decision = CompressionDecision {
        version: COMPRESSION_VERSION,
        kind: CompressionKind::Unknown,
        reason: CompressionReason::BelowThreshold,
        original: ContextSize::of_serialized(text),
        baseline: ContextSize::of_serialized(baseline),
        view: ContextSize::of_serialized(baseline),
        omitted: 0,
    };
    if text.len() <= threshold {
        return CompressionResult {
            view: None,
            decision,
        };
    }
    decision.reason = CompressionReason::Unsupported;
    let Some((kind, body)) = candidate(call, text) else {
        return CompressionResult {
            view: None,
            decision,
        };
    };
    decision.kind = kind;
    decision.omitted = if kind == CompressionKind::Diff {
        text.lines()
            .filter(|line| line.starts_with(' '))
            .count()
            .saturating_sub(body.lines().filter(|line| line.starts_with(' ')).count())
    } else {
        0
    };
    let kind_name = match kind {
        CompressionKind::Log => "log",
        CompressionKind::Search => "search",
        CompressionKind::Json => "json",
        CompressionKind::Jsonl => "jsonl",
        CompressionKind::Table => "table",
        CompressionKind::Diff => "diff",
        CompressionKind::Unknown => "unknown",
    };
    // A fixed-point decimal width avoids metadata-size self-reference.
    let render = |tokens: usize| {
        format!(
            "[forge compression v1 kind={kind_name} original_tokens={} baseline_tokens={} view_tokens={tokens:020} omitted_context_lines={}; formatting/repetition summarized; omissions marked inline]\n{body}\n[Complete sanitized output: retrieve_tool_output handle={}]\n",
            decision.original.estimated_tokens,
            decision.baseline.estimated_tokens,
            decision.omitted,
            artifact.handle
        )
    };
    let size = ContextSize::of_serialized(&render(0));
    let view = render(size.estimated_tokens);
    decision.reason = CompressionReason::NoSavings;
    if view.len() <= baseline.len()
        && size.chars <= decision.baseline.chars
        && size.estimated_tokens.saturating_mul(100)
            <= decision.baseline.estimated_tokens.saturating_mul(70)
    {
        decision.reason = CompressionReason::Compressed;
        decision.view = size;
        return CompressionResult {
            view: Some(view),
            decision,
        };
    }
    decision.omitted = 0;
    CompressionResult {
        view: None,
        decision,
    }
}

fn candidate(call: &ToolCall, text: &str) -> Option<(CompressionKind, String)> {
    match call.name.as_str() {
        "graph_grep" => search(text).map(|s| (CompressionKind::Search, s)),
        "read_file" => {
            let path = call.arguments.get("path")?.as_str()?;
            if path.ends_with(".json") {
                json(text).map(|s| (CompressionKind::Json, s))
            } else if path.ends_with(".jsonl") {
                jsonl(text).map(|s| (CompressionKind::Jsonl, s))
            } else if path.ends_with(".csv") || path.ends_with(".tsv") {
                table(text, if path.ends_with(".csv") { b',' } else { b'\t' })
                    .map(|s| (CompressionKind::Table, s))
            } else if path.ends_with(".diff") || path.ends_with(".patch") {
                diff(text).map(|s| (CompressionKind::Diff, s))
            } else {
                None
            }
        }
        "run_command" => {
            // Exact executable and argument vector only: no shell wrappers,
            // pipes, inline environments or arbitrary commands inferred as logs.
            let command = call.arguments.get("command")?.as_str()?;
            let args = call.arguments.get("args")?.as_array()?;
            if command == "cargo" && args.len() == 1 && args[0].as_str() == Some("test") {
                log(text).map(|s| (CompressionKind::Log, s))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// JSON whitespace compaction retains raw lexemes, duplicate keys, integer
/// precision, order and exceptional rows (unlike deserialize/reserialize).
fn json(text: &str) -> Option<String> {
    let _: &serde_json::value::RawValue = serde_json::from_str(text).ok()?;
    if let Ok(records) = serde_json::from_str::<Vec<&serde_json::value::RawValue>>(text) {
        let records: Vec<_> = records.iter().map(|record| record.get()).collect();
        if records.windows(2).any(|pair| pair[0] == pair[1]) {
            return Some(record_groups("JSON array elements", &records));
        }
    }
    let mut result = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for c in text.chars() {
        if quoted {
            result.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                quoted = false;
            }
        } else if c == '"' {
            quoted = true;
            result.push(c);
        } else if !matches!(c, ' ' | '\n' | '\r' | '\t') {
            result.push(c);
        }
    }
    Some(result)
}

/// Framed original records, never an opaque value dictionary. Positions are
/// one-based logical record positions; byte lengths disambiguate record content
/// that happens to resemble the explanatory framing.
fn record_groups(label: &str, records: &[&str]) -> String {
    let mut body = format!(
        "Ordered {label}; total_records={}; original records follow each byte-length header:\n",
        records.len()
    );
    let mut start = 0;
    while start < records.len() {
        let mut end = start + 1;
        while end < records.len() && records[end] == records[start] {
            end += 1;
        }
        body.push_str(&format!(
            "[records {}..={} count={} bytes={}]\n{}\n",
            start + 1,
            end,
            end - start,
            records[start].len(),
            records[start]
        ));
        start = end;
    }
    body
}

fn jsonl(text: &str) -> Option<String> {
    let records: Vec<_> = text.lines().collect();
    if records.is_empty() {
        return None;
    }
    for record in &records {
        // Strict complete-value parsing, including raw numeric/duplicate-key
        // fidelity; blank records and trailing garbage are not guessed away.
        let _: &serde_json::value::RawValue = serde_json::from_str(record).ok()?;
    }
    Some(record_groups("JSONL records", &records))
}

/// Accept a canonical CSV/TSV subset, not the CSV reader's permissive grammar.
/// Exact writer roundtrip rejects unterminated quotes, ignored whitespace and
/// other ambiguous input. LF record separators and an optional final LF only;
/// embedded quoted newlines remain part of the original record.
fn table(text: &str, delimiter: u8) -> Option<String> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .delimiter(delimiter)
        .from_reader(text.as_bytes());
    let mut record = csv::ByteRecord::new();
    let mut records = Vec::new();
    let mut offset = 0;
    while reader.read_byte_record(&mut record).ok()? {
        let end = usize::try_from(reader.position().byte()).ok()?;
        let raw = text.get(offset..end)?;
        let mut writer = csv::WriterBuilder::new()
            .delimiter(delimiter)
            .from_writer(Vec::new());
        writer.write_byte_record(&record).ok()?;
        let canonical = writer.into_inner().ok()?;
        let matches = raw.as_bytes() == canonical
            || (end == text.len()
                && !raw.ends_with('\n')
                && canonical.strip_suffix(b"\n") == Some(raw.as_bytes()));
        if !matches {
            return None;
        }
        records.push(raw.strip_suffix('\n').unwrap_or(raw));
        offset = end;
    }
    if offset != text.len() || records.len() < 2 {
        return None;
    }
    let header = records.remove(0);
    Some(format!(
        "Table delimiter={} header bytes={}:\n{header}\n{}",
        if delimiter == b',' { "comma" } else { "tab" },
        header.len(),
        record_groups("table data records (header excluded)", &records)
    ))
}

/// Group only consecutive rows by path; each original line number and match
/// text is retained verbatim and ordering is unchanged.
fn search(text: &str) -> Option<String> {
    let mut groups: Vec<(String, Vec<(u64, String)>)> = Vec::new();
    for row in text.lines() {
        let (left, matched) = row.split_once(": ")?;
        let (path, line) = left.rsplit_once(':')?;
        let number: u64 = line.parse().ok()?;
        if path.is_empty() || number == 0 || number.to_string() != line {
            return None;
        }
        if groups.last().is_none_or(|(last, _)| last != path) {
            groups.push((path.to_owned(), Vec::new()));
        }
        groups.last_mut()?.1.push((number, matched.to_owned()));
    }
    if groups.is_empty() {
        return None;
    }
    Some(format!(
        "Ordered search groups [path, [[line, exact match], ...]]:\n{}",
        serde_json::to_string(&groups).ok()?
    ))
}

/// Readable exact consecutive repetitions only. Every distinct line remains
/// visible in its original order; repetition counts replace redundant copies.
fn repeated_lines(text: &str) -> Option<String> {
    let mut output = String::new();
    let mut lines = text.split_inclusive('\n').peekable();
    while let Some(line) = lines.next() {
        output.push_str(line);
        let mut repeated = 0;
        while lines.peek() == Some(&line) {
            lines.next();
            repeated += 1;
        }
        if repeated > 0 {
            output.push_str(&format!(
                "[forge: preceding exact line repeated {repeated} additional times]\n"
            ));
        }
    }
    Some(output)
}

fn log(text: &str) -> Option<String> {
    let rest = text.strip_prefix("exit ")?;
    let (exit, rest) = rest.split_once("\nstdout:\n")?;
    exit.parse::<i32>().ok()?;
    let (stdout, stderr) = rest.split_once("\nstderr:\n")?;
    // Accept only libtest output, not arbitrary successful shell logs. Unknown
    // diagnostics decline rather than infer that they can safely be discarded.
    for line in stdout.lines().chain(stderr.lines()) {
        if !(line.is_empty()
            || line.starts_with("running ")
            || line.starts_with("test ")
            || line.starts_with("test result: ")
            || line.starts_with("failures:")
            || line.starts_with("error:")
            || line.starts_with("warning:")
            || line.starts_with("    Finished ")
            || line.starts_with("     Running ")
            || line.starts_with("   Doc-tests "))
        {
            return None;
        }
    }
    repeated_lines(text)
}

/// Ordinary header/hunk unified diff only. Binary/combined/git extended forms
/// are deliberately unsupported. Validate old/new hunk counts before encoding.
fn diff(text: &str) -> Option<String> {
    let lines: Vec<_> = text.lines().collect();
    let mut context = vec![false; lines.len()];
    let mut i = 0;
    while i < lines.len() {
        if !lines.get(i)?.starts_with("--- ") || !lines.get(i + 1)?.starts_with("+++ ") {
            return None;
        }
        i += 2;
        let mut hunks = 0;
        while i < lines.len() && lines[i].starts_with("@@ ") {
            let header = lines[i].strip_prefix("@@ ")?;
            let (ranges, _) = header.split_once(" @@")?;
            let (old, new) = ranges.split_once(' ')?;
            let count = |range: &str, sign| -> Option<usize> {
                let range = range.strip_prefix(sign)?;
                let (start, count) = range.split_once(',').unwrap_or((range, "1"));
                start.parse::<usize>().ok()?;
                count.parse::<usize>().ok()
            };
            let mut old = count(old, '-')?;
            let mut new = count(new, '+')?;
            i += 1;
            while old > 0 || new > 0 {
                let row = *lines.get(i)?;
                match row.as_bytes().first()? {
                    b' ' => {
                        old = old.checked_sub(1)?;
                        new = new.checked_sub(1)?;
                        context[i] = true;
                    }
                    b'-' => old = old.checked_sub(1)?,
                    b'+' => new = new.checked_sub(1)?,
                    _ => return None,
                }
                i += 1;
                if lines.get(i) == Some(&"\\ No newline at end of file") {
                    i += 1;
                }
            }
            hunks += 1;
        }
        if hunks == 0 {
            return None;
        }
    }
    if lines.is_empty() {
        return None;
    }
    let mut output = String::new();
    let mut i = 0;
    while i < lines.len() {
        if !context[i] {
            output.push_str(lines[i]);
            output.push('\n');
            i += 1;
            continue;
        }
        let start = i;
        while i < lines.len() && context[i] {
            i += 1;
        }
        let count = i - start;
        for (offset, line) in lines[start..i].iter().enumerate() {
            if count <= 6 || offset < 2 || offset >= count - 2 {
                output.push_str(line);
                output.push('\n');
            } else if offset == 2 {
                output.push_str(&format!(
                    "[forge: {} unchanged context lines omitted; retrieve original]\n",
                    count - 4
                ));
            }
        }
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ArtifactQuery, ArtifactSource, ArtifactStore, MemoryArtifactStore, SanitizedArtifactSource,
    };
    use forge_core::events::cap_tool_output;
    use forge_session::Redactor;
    use serde_json::{Value, json};

    fn call(kind: CompressionKind) -> ToolCall {
        match kind {
            CompressionKind::Log => ToolCall::new(
                "c",
                "run_command",
                json!({"command":"cargo","args":["test"]}),
            ),
            CompressionKind::Search => {
                ToolCall::new("c", "graph_grep", json!({"pattern":"secret-never-echo"}))
            }
            CompressionKind::Json => {
                ToolCall::new("c", "read_file", json!({"path":"private-never-echo.json"}))
            }
            CompressionKind::Jsonl => {
                ToolCall::new("c", "read_file", json!({"path":"private-never-echo.jsonl"}))
            }
            CompressionKind::Table => {
                ToolCall::new("c", "read_file", json!({"path":"private-never-echo.csv"}))
            }
            CompressionKind::Diff => {
                ToolCall::new("c", "read_file", json!({"path":"private-never-echo.diff"}))
            }
            CompressionKind::Unknown => unreachable!(),
        }
    }

    fn stored(text: &str) -> (MemoryArtifactStore, SanitizedOutput, ArtifactRef) {
        let redactor = Redactor::default();
        let output = SanitizedOutput::new(&redactor, text);
        let store = MemoryArtifactStore::default();
        let source = SanitizedArtifactSource::new(
            ArtifactSource {
                session_id: "s".into(),
                run_id: "r".into(),
                call_id: "c".into(),
                event_seq: 1,
            },
            &redactor,
        )
        .unwrap();
        let reference = store.put(source, &output).unwrap();
        (store, output, reference)
    }

    fn baseline(text: &str, reference: &ArtifactRef) -> String {
        if text.len() <= MAX_TOOL_OUTPUT_BYTES {
            return text.to_owned();
        }
        format!(
            "{}\n[Complete sanitized output: retrieve_tool_output handle={}]\n",
            cap_tool_output(text),
            reference.handle
        )
    }

    // Public, sanitized synthetic workloads, not private session captures.
    // Mix ordinary unique records and repetitive compiler/generated-file output.
    fn corpus(records: usize) -> Vec<(&'static str, CompressionKind, String)> {
        let mut logs = format!("exit 0\nstdout:\nrunning {records} tests\n");
        for n in 0..records {
            logs.push_str(&format!(
                "test integration::storage::case_{n:04}::preserves_authorization ... ok\n"
            ));
        }
        logs.push_str(&format!("test result: ok. {records} passed; 0 failed\n\nstderr:\nwarning: W0042 deprecated fixture mode\n"));
        let warnings = format!(
            "exit 1\nstdout:\nrunning 1 test\ntest validation::exceptional ... FAILED\n\nstderr:\n{}error: E0308 mismatched types\n",
            "warning: generated schema uses deprecated field `legacy_identity`\n".repeat(records)
        );
        let rows: Vec<_> = (0..records)
            .map(|n| {
                json!({
                    "id": n, "status": if n == records - 1 {"exceptional"} else {"ready"},
                    "payload": {"active": true, "label": "café \"safe\""},
                })
            })
            .collect();
        let search = (1..=records)
            .map(|n| {
                format!(
                    "crates/forge-context/src/generated/authorization_registry.rs:{n}: symbol_{n}"
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let mut diff = String::new();
        for n in 0..records {
            diff.push_str(&format!("--- a/generated/schema_{n}.rs\n+++ b/generated/schema_{n}.rs\n@@ -1,4 +1,4 @@\n // generated schema\n pub struct Record {{\n-    pub legacy: bool,\n+    pub enabled: bool,\n }}\n"));
        }
        vec![
            ("unique-tests", CompressionKind::Log, logs),
            ("repeated-warnings", CompressionKind::Log, warnings),
            (
                "nested-json",
                CompressionKind::Json,
                serde_json::to_string_pretty(&rows).unwrap(),
            ),
            ("graph-symbols", CompressionKind::Search, search),
            ("generated-diff", CompressionKind::Diff, diff),
        ]
    }

    #[test]
    fn corpus_report_and_structural_tasks() {
        for (records, name, kind, text) in [120, 300, 600, 1200].into_iter().flat_map(|records| {
            corpus(records)
                .into_iter()
                .map(move |(name, kind, text)| (records, name, kind, text))
        }) {
            let (store, output, reference) = stored(&text);
            let baseline = baseline(output.as_str(), &reference);
            let (actual_kind, body) = candidate(&call(kind), output.as_str()).unwrap();
            assert_eq!(actual_kind, kind);
            match kind {
                CompressionKind::Log | CompressionKind::Diff => {
                    // All distinct original lines are directly readable, with
                    // no test-only decompressor or retrieval expansion.
                    for line in output.as_str().lines() {
                        assert!(body.lines().any(|kept| kept == line));
                    }
                    if name == "repeated-warnings" {
                        assert!(body.contains("error: E0308 mismatched types"));
                        assert!(body.contains("test validation::exceptional ... FAILED"));
                        assert!(body.contains(&format!("{} additional times", records - 1)));
                    }
                }
                CompressionKind::Json => {
                    assert_eq!(
                        serde_json::from_str::<Value>(&body).unwrap(),
                        serde_json::from_str::<Value>(output.as_str()).unwrap()
                    );
                    assert!(body.contains("exceptional"));
                }
                CompressionKind::Search => {
                    let groups: Vec<(String, Vec<(u64, String)>)> =
                        serde_json::from_str(body.split_once('\n').unwrap().1).unwrap();
                    let restored = groups
                        .into_iter()
                        .flat_map(|(path, rows)| {
                            rows.into_iter()
                                .map(move |(line, text)| format!("{path}:{line}: {text}"))
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    assert_eq!(restored, output.as_str());
                }
                _ => unreachable!(),
            }
            for kib in [8, 16, 32, 64] {
                let result = compress_at(&call(kind), &output, &reference, &baseline, kib * 1024);
                assert_eq!(
                    result,
                    compress_at(&call(kind), &output, &reference, &baseline, kib * 1024)
                );
                let measured = result.view.as_deref().unwrap_or(&baseline);
                assert_eq!(result.decision.view, ContextSize::of_serialized(measured));
                assert!(!measured.contains("never-echo"));
                if let Some(view) = result.view {
                    assert!(view.contains(&body));
                    assert!(view.len() <= baseline.len());
                    assert!(
                        result.decision.view.estimated_tokens * 100
                            <= result.decision.baseline.estimated_tokens * 70
                    );
                }
                println!(
                    "{name} records={records} {kind:?} threshold={kib}KiB original={} baseline={} view={} decision={:?}",
                    result.decision.original.estimated_tokens,
                    result.decision.baseline.estimated_tokens,
                    result.decision.view.estimated_tokens,
                    result.decision.reason
                );
            }
            // Script an original-byte question not expressible by normalized
            // formatting; authorized bounded retrieval recovers exact bytes.
            let start = output.as_str().len() - 128;
            let read = store
                .retrieve(
                    &reference.handle,
                    std::slice::from_ref(&reference),
                    ArtifactQuery::Range {
                        start,
                        end: output.as_str().len(),
                    },
                )
                .unwrap()
                .unwrap();
            assert_eq!(read.text, output.as_str()[read.start..read.end]);
        }
    }

    #[test]
    fn default_per_kind_gate_and_rollout_exclusions() {
        for kind in [CompressionKind::Log, CompressionKind::Search] {
            let mut baseline_tokens = 0;
            let mut view_tokens = 0;
            for (_, sample_kind, text) in corpus(1200) {
                if sample_kind != kind {
                    continue;
                }
                let (_, output, reference) = stored(&text);
                let baseline = baseline(output.as_str(), &reference);
                let result = compress_tool_output(&call(kind), &output, &reference, &baseline);
                baseline_tokens += result.decision.baseline.estimated_tokens;
                view_tokens += result.decision.view.estimated_tokens;
            }
            assert!(baseline_tokens > 0);
            assert!(
                view_tokens * 100 <= baseline_tokens * 70,
                "{kind:?} fails per-kind corpus gate"
            );
            println!(
                "DEFAULT GATE {kind:?}: baseline={baseline_tokens} view={view_tokens} savings={}%",
                (baseline_tokens - view_tokens) * 100 / baseline_tokens
            );
        }
        for kind in [CompressionKind::Json, CompressionKind::Diff] {
            let (_, _, text) = corpus(1200)
                .into_iter()
                .find(|(_, k, _)| *k == kind)
                .unwrap();
            let (_, output, reference) = stored(&text);
            let result = compress_tool_output(
                &call(kind),
                &output,
                &reference,
                &baseline(output.as_str(), &reference),
            );
            assert!(result.view.is_none());
        }
    }

    #[test]
    fn diff_keeps_changes_headers_and_markers_retrieves_omitted_context() {
        let context = (0..1000)
            .map(|n| format!(" unchanged context record {n}\n"))
            .collect::<String>();
        let text = format!(
            "--- a/example\n+++ b/example\n@@ -1,1001 +1,1001 @@ section\n{context}-old protected change\n+new protected change\n\\ No newline at end of file\n"
        );
        let (store, output, reference) = stored(&text);
        let body = diff(output.as_str()).unwrap();
        for required in [
            "--- a/example",
            "+++ b/example",
            "@@ -1,1001 +1,1001 @@ section",
            "-old protected change",
            "+new protected change",
            "\\ No newline at end of file",
        ] {
            assert!(body.lines().any(|line| line == required));
        }
        let answer = " unchanged context record 500\n";
        assert!(!body.contains(answer));
        assert!(body.contains("996 unchanged context lines omitted"));
        let read = store
            .retrieve(
                &reference.handle,
                std::slice::from_ref(&reference),
                ArtifactQuery::Search {
                    literal: answer.into(),
                    start: 0,
                    limit: 128,
                },
            )
            .unwrap()
            .unwrap();
        assert!(read.text.contains(answer));
        assert!(read.text.len() <= 128);
        assert!(
            store
                .retrieve(
                    &reference.handle,
                    &[],
                    ArtifactQuery::Search {
                        literal: answer.into(),
                        start: 0,
                        limit: 128
                    }
                )
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn search_unicode_windows_paths_and_pairs_are_directly_visible() {
        let source = "C:\\work\\café.rs:12: fn quoted(\"value\")\nC:\\work\\café.rs:18: error E0042\nsrc/other.rs:7: exceptional\nC:\\work\\café.rs:12: repeated";
        let body = search(source).unwrap();
        let groups: Vec<(String, Vec<(u64, String)>)> =
            serde_json::from_str(body.split_once('\n').unwrap().1).unwrap();
        assert_eq!(groups[0].0, "C:\\work\\café.rs");
        assert_eq!(
            groups[0].1,
            vec![
                (12, "fn quoted(\"value\")".into()),
                (18, "error E0042".into())
            ]
        );
        assert_eq!(
            groups[1],
            ("src/other.rs".into(), vec![(7, "exceptional".into())])
        );
        assert_eq!(groups[2].1, vec![(12, "repeated".into())]);
        let distinct = (1..2000)
            .map(|n| format!("p{n}:{n}: distinct symbol value {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (_, output, reference) = stored(&distinct);
        assert!(
            compress_tool_output(
                &call(CompressionKind::Search),
                &output,
                &reference,
                &baseline(output.as_str(), &reference)
            )
            .view
            .is_none()
        );
    }

    #[test]
    fn strict_origins_and_malformed_inputs_decline() {
        let log = "exit 0\nstdout:\ntest alpha ... ok\n\nstderr:\n";
        for command in [
            "git status",
            "bash",
            "cargo test",
            "cargo test; echo secret",
            "env",
            "cat",
        ] {
            assert!(
                candidate(
                    &ToolCall::new(
                        "c",
                        "run_command",
                        json!({"command":command,"args":["test"]})
                    ),
                    log
                )
                .is_none()
            );
        }
        for path in [
            "source.rs",
            "README.md",
            "output.log",
            "values.csv",
            "data.jsonl",
        ] {
            assert!(
                candidate(&ToolCall::new("c", "read_file", json!({"path":path})), log).is_none()
            );
        }
        for text in [
            "path:zero: x",
            "path:0: x",
            "path:01: x",
            "path:2:x",
            "header\npath:1: x",
            "",
        ] {
            assert!(search(text).is_none());
        }
        for text in ["{", "{} trailing", "{\"x\":NaN}", "{\"x\":1,}", "{}\n{}"] {
            assert!(json(text).is_none());
        }
        for text in [
            "@@@ combined",
            "GIT binary patch",
            "--- a\n+++ b\n@@ -1 +1 @@\n-a\n",
            "--- a\n+++ b\n@@ -1 +1 @@\n-a\n+b\njunk",
        ] {
            assert!(diff(text).is_none());
        }
        assert!(log_fn("exit 0\nstdout:\nfn main() {}\n\nstderr:\n").is_none());
        fn log_fn(text: &str) -> Option<String> {
            super::log(text)
        }
    }

    #[test]
    fn json_raw_fields_escapes_duplicate_keys_and_precision_are_retained() {
        let raw = " { \"dup\": 1, \"dup\": 999999999999999999999999, \"s\": \"é \\\\\\\" \\n x\", \"rows\": [null, false, -1.20e+3] } ";
        let compact = json(raw).unwrap();
        assert_eq!(
            compact,
            "{\"dup\":1,\"dup\":999999999999999999999999,\"s\":\"é \\\\\\\" \\n x\",\"rows\":[null,false,-1.20e+3]}"
        );
        assert_eq!(json(&compact).unwrap(), compact);
    }

    #[test]
    fn structured_records_preserve_raw_values_positions_and_exceptions() {
        let raw = r#"{"dup":1,"dup":999999999999999999999999,"n":1e999,"s":"雪\n\""}"#;
        let exceptional = r#"{"new_schema":null,"n":-0.00e+02}"#;
        for (kind, input) in [
            (
                CompressionKind::Json,
                format!("[{raw},{raw},{exceptional},{raw}]"),
            ),
            (
                CompressionKind::Jsonl,
                format!("{raw}\n{raw}\n{exceptional}\n{raw}\n"),
            ),
        ] {
            let (_, body) = candidate(&call(kind), &input).unwrap();
            assert!(body.contains(raw));
            assert!(body.contains(exceptional));
            assert!(body.contains("[records 1..=2 count=2"));
            assert!(body.contains("[records 3..=3 count=1"));
            assert!(body.contains("[records 4..=4 count=1"));
            assert!(body.contains("total_records=4"));
        }
        for delimiter in *b",\t" {
            let d = char::from(delimiter);
            let header = format!("id{d}value{d}status");
            let row = format!("01{d}\"雪{d}\"\"quoted\"\"\nline\"{d}ready");
            let exception = format!("02{d}{d}exceptional");
            let input = format!("{header}\n{row}\n{row}\n{exception}\n{row}");
            let body = table(&input, delimiter).unwrap();
            for exact in [&header, &row, &exception] {
                assert!(body.contains(exact));
            }
            assert!(body.contains("[records 1..=2 count=2"));
            assert!(body.contains("[records 3..=3 count=1"));
            assert!(body.contains("[records 4..=4 count=1"));
            assert!(body.contains("total_records=4"));
            let path = if delimiter == b',' {
                "rows.csv"
            } else {
                "rows.tsv"
            };
            assert_eq!(
                candidate(
                    &ToolCall::new("c", "read_file", json!({"path":path})),
                    &input
                ),
                Some((CompressionKind::Table, body))
            );
        }
    }

    #[test]
    fn structured_malformed_ambiguous_and_trailing_inputs_decline() {
        for input in ["", "\n", "{}\n\n{}", "{} trailing\n{}", "{\n}", "{}\n{"] {
            assert!(jsonl(input).is_none(), "{input:?}");
        }
        for input in ["[{},{}] trailing", "[{},{} ,]", "[1e,1e]", "[true,false]{}"] {
            assert!(json(input).is_none(), "{input:?}");
        }
        for delimiter in *b",\t" {
            for input in [
                "",
                "id,value",
                "id,value\n1",
                "id,value\n1,2,3",
                "id,value\n1,\"unterminated",
                "id,value\n1,\"x\"junk",
                "id,value\n1,un\"quoted",
                "id,value\n\n1,x",
                "id,value\r\n1,x\r\n",
                "id,value\n1,x\n\n",
                // Valid but noncanonical optional quoting deliberately declines.
                "id,value\n1,\"plain\"",
            ] {
                let input = input.replace(',', &char::from(delimiter).to_string());
                assert!(table(&input, delimiter).is_none(), "{input:?}");
            }
        }
    }

    #[test]
    fn structured_candidate_corpus_report_and_default_exclusion() {
        for records in [120, 300, 600, 1200] {
            for repeated in [false, true] {
                let mut json_rows = Vec::new();
                let mut csv_rows = Vec::new();
                for n in 0..records {
                    let id = if repeated { 0 } else { n };
                    let status = if n == records - 1 {
                        "exceptional"
                    } else {
                        "ready"
                    };
                    json_rows.push(format!(
                        "{{\"id\":{id},\"dup\":1,\"dup\":999999999999999999999999,\"status\":\"{status}\",\"text\":\"雪 quoted \\\"value\\\" preserved payload\"}}"
                    ));
                    csv_rows.push(format!(
                        "{id},\"雪,\"\"quoted\"\"\nall distinct values remain visible\",{status}"
                    ));
                }
                for (name, kind, path, text, original_rows) in [
                    (
                        "array",
                        CompressionKind::Json,
                        "rows.json",
                        format!("[{}]", json_rows.join(",")),
                        &json_rows,
                    ),
                    (
                        "jsonl",
                        CompressionKind::Jsonl,
                        "rows.jsonl",
                        json_rows.join("\n"),
                        &json_rows,
                    ),
                    (
                        "csv",
                        CompressionKind::Table,
                        "rows.csv",
                        format!("id,text,status\n{}\n", csv_rows.join("\n")),
                        &csv_rows,
                    ),
                    (
                        "tsv",
                        CompressionKind::Table,
                        "rows.tsv",
                        format!(
                            "id\ttext\tstatus\n{}\n",
                            csv_rows.join("\n").replace(',', "\t")
                        ),
                        &csv_rows,
                    ),
                ] {
                    let call = ToolCall::new("c", "read_file", json!({"path":path}));
                    let (_, output, reference) = stored(&text);
                    let baseline = baseline(output.as_str(), &reference);
                    let (actual_kind, body) = candidate(&call, output.as_str()).unwrap();
                    assert_eq!(actual_kind, kind);
                    // Direct answer checks, not merely a repetition ratio:
                    // every unique row, final exception, schema and exact lexemes.
                    for row in original_rows {
                        let row = if name == "tsv" {
                            row.replace(',', "\t")
                        } else {
                            row.clone()
                        };
                        assert!(body.contains(&row));
                    }
                    assert!(body.contains("exceptional"));
                    assert!(body.contains("status"));
                    if repeated {
                        assert!(body.contains(&format!(
                            "records 1..={} count={}",
                            records - 1,
                            records - 1
                        )));
                    }
                    for kib in [8, 16, 32, 64] {
                        let result = compress_at(&call, &output, &reference, &baseline, kib * 1024);
                        assert_eq!(
                            result,
                            compress_at(&call, &output, &reference, &baseline, kib * 1024)
                        );
                        if let Some(view) = &result.view {
                            assert!(view.contains(&body));
                            assert_eq!(result.decision.view, ContextSize::of_serialized(view));
                            assert!(
                                result.decision.view.estimated_tokens * 100
                                    <= result.decision.baseline.estimated_tokens * 70
                            );
                        }
                        if !repeated {
                            assert!(result.view.is_none());
                        } else if output.as_str().len() > kib * 1024 {
                            assert!(result.view.is_some());
                        }
                        println!(
                            "{name} repeated={repeated} records={records} threshold={kib}KiB original={} baseline={} view={} decision={:?}",
                            result.decision.original.estimated_tokens,
                            result.decision.baseline.estimated_tokens,
                            result.decision.view.estimated_tokens,
                            result.decision.reason
                        );
                    }
                    assert!(
                        compress_tool_output(&call, &output, &reference, &baseline)
                            .view
                            .is_none()
                    );
                }
            }
        }
    }

    #[test]
    fn threshold_is_strict_and_metadata_can_reject_small_candidates() {
        for bytes in [8192, 16384, 32768, 65536] {
            let text = format!("\"{}\"", "a".repeat(bytes - 2));
            let (_, output, reference) = stored(&text);
            let baseline = baseline(&text, &reference);
            let result =
                compress_tool_output(&call(CompressionKind::Json), &output, &reference, &baseline);
            assert_eq!(result.decision.reason, CompressionReason::BelowThreshold);
            assert!(result.view.is_none());
        }
        let (_, output, reference) = stored("{ \"x\": 1 }");
        let baseline = baseline(output.as_str(), &reference);
        assert_eq!(
            compress_at(
                &call(CompressionKind::Json),
                &output,
                &reference,
                &baseline,
                0
            )
            .decision
            .reason,
            CompressionReason::NoSavings
        );
    }
}
