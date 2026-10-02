//! A hand-rolled incremental Server-Sent Events parser — the grammar both
//! streaming model APIs speak (OpenAI-compatible `data:`-only streams and
//! Anthropic's named-event streams).
//!
//! Zero new dependencies on purpose: the grammar is ~80 lines over
//! `reqwest::Response::chunk()` (base API), and the SDK/crate route was
//! pre-rejected by the house's own precedent (forge-acp transcribed a
//! protocol rather than pay 52 transitive crates).
//!
//! The parser works on *bytes*: lines are cut at `\n` (0x0A can never appear
//! inside a multi-byte UTF-8 sequence, so a complete line is complete
//! UTF-8), one trailing `\r` is stripped (CRLF streams are legal SSE), and a
//! blank line dispatches the pending event. `data:` lines accumulate and
//! join with `\n`; `event:` names the event; `:`-prefixed lines are
//! comments/keepalives; `id:`/`retry:` and unknown fields are ignored.
//! Network chunk boundaries are invisible by construction — the caller feeds
//! arbitrary byte slices and only complete lines are ever examined.

/// One dispatched SSE event: the `event:` name (if any) and the accumulated
/// `data:` payload (multi-line data joined with `\n`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SseEvent {
    pub(crate) event: Option<String>,
    pub(crate) data: String,
}

/// Incremental parser: [`feed`](SseParser::feed) arbitrary byte slices, get
/// the events completed by them; [`finish`](SseParser::finish) at EOF to
/// flush an event whose dispatching blank line never arrived.
#[derive(Default)]
pub(crate) struct SseParser {
    /// Received bytes not yet terminated by `\n` (a partial line).
    pending_line: Vec<u8>,
    /// `data:` payloads accumulated for the pending event.
    data: String,
    /// The pending event's `event:` name.
    event: Option<String>,
}

impl SseParser {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Feed the next network chunk; returns the events it completed.
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.pending_line.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(pos) = self.pending_line.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.pending_line.drain(..pos).collect();
            self.pending_line.drain(..1); // the '\n' itself
            self.process_line(&line, &mut events);
        }
        events
    }

    /// EOF: a trailing partial line is still a line, and a pending event
    /// whose blank line never arrived still dispatches — but an empty
    /// buffer invents nothing.
    pub(crate) fn finish(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        if !self.pending_line.is_empty() {
            let line = std::mem::take(&mut self.pending_line);
            self.process_line(&line, &mut events);
        }
        self.dispatch(&mut events);
        events
    }

    fn process_line(&mut self, line: &[u8], events: &mut Vec<SseEvent>) {
        // One trailing CR: CRLF is a legal line ending in SSE.
        let line = match line.last() {
            Some(b'\r') => &line[..line.len() - 1],
            _ => line,
        };
        if line.is_empty() {
            self.dispatch(events);
            return;
        }
        // A leading ':' is a comment/keepalive line; it dispatches nothing.
        if line.starts_with(b":") {
            return;
        }
        // `field: value` — one optional leading space on the value.
        let (field, value) = match line.iter().position(|&b| b == b':') {
            Some(colon) => {
                let value = &line[colon + 1..];
                let value = match value.first() {
                    Some(b' ') => &value[1..],
                    _ => value,
                };
                (&line[..colon], value)
            }
            None => (line, &[][..]),
        };
        match field {
            b"data" => {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                // A complete line is complete UTF-8 (see the module docs);
                // lossy decode only matters for a server that isn't speaking
                // UTF-8 at all, which serde_json will reject downstream.
                self.data.push_str(&String::from_utf8_lossy(value));
            }
            b"event" => {
                self.event = Some(String::from_utf8_lossy(value).into_owned());
            }
            // `id:`, `retry:`, unknown fields: ignored by both APIs' clients.
            _ => {}
        }
    }

    /// A blank line (or EOF) dispatches the pending event — but only when
    /// data accumulated; a keepalive gap is not an event.
    fn dispatch(&mut self, events: &mut Vec<SseEvent>) {
        if self.data.is_empty() {
            self.event = None;
            return;
        }
        events.push(SseEvent {
            event: self.event.take(),
            data: std::mem::take(&mut self.data),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(chunks: &[&[u8]]) -> Vec<SseEvent> {
        let mut parser = SseParser::new();
        let mut events = Vec::new();
        for chunk in chunks {
            events.extend(parser.feed(chunk));
        }
        events.extend(parser.finish());
        events
    }

    #[test]
    fn openai_style_data_only_events() {
        let events = parse_all(&[b"data: {\"a\":1}\n\ndata: [DONE]\n\n"]);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0],
            SseEvent {
                event: None,
                data: "{\"a\":1}".into()
            }
        );
        assert_eq!(events[1].data, "[DONE]");
    }

    #[test]
    fn anthropic_style_named_events_and_keepalives() {
        let bytes =
            b": keepalive\r\n\r\nevent: message_start\r\ndata: {\"type\":\"message_start\"}\r\n\r\n";
        let events = parse_all(&[bytes]);
        assert_eq!(events.len(), 1, "comments dispatch nothing");
        assert_eq!(events[0].event.as_deref(), Some("message_start"));
    }

    #[test]
    fn multi_line_data_joins_with_newline() {
        let events = parse_all(&[b"data: first\ndata: second\n\n"]);
        assert_eq!(events[0].data, "first\nsecond");
    }

    #[test]
    fn finish_flushes_an_unterminated_final_event() {
        // A stream ending `data: [DONE]\n` (no blank line) still delivers it.
        let events = parse_all(&[b"data: hello\n\ndata: [DONE]\n"]);
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].data, "[DONE]");
    }

    /// An empty stream invents no event, and neither do keepalives alone.
    #[test]
    fn finish_with_nothing_pending_dispatches_nothing() {
        assert!(parse_all(&[]).is_empty());
        assert!(parse_all(&[b": keepalive\n\n"]).is_empty());
    }

    /// The determinism workhorse: network chunk boundaries are invisible.
    /// Splits a transcript at *every byte* — including through a multi-byte
    /// UTF-8 character — and asserts the events never change.
    #[test]
    fn chunk_boundaries_are_invisible() {
        let transcript: &[u8] = "event: content_block_delta\r\ndata: {\"delta\":{\"text\":\"héllo ✨\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".as_bytes();
        let whole = parse_all(&[transcript]);
        for split in 0..transcript.len() {
            let parts = [&transcript[..split], &transcript[split..]];
            assert_eq!(parse_all(&parts), whole, "split at byte {split}");
        }
    }
}
