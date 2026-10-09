use regex::Regex;

const REDACTED: &str = "[REDACTED]";

/// Redacts secret-looking values from strings before they hit the session
/// log. Two sources: (a) values of environment variables whose names
/// contain KEY/TOKEN/SECRET/PASSWORD (non-empty, length ≥ 8), snapshotted
/// at construction; (b) common token shapes such as `sk-…` keys and
/// `Bearer …` headers.
pub struct Redactor {
    env_secrets: Vec<String>,
    patterns: Vec<Regex>,
}

impl Default for Redactor {
    fn default() -> Self {
        Self::new()
    }
}

impl Redactor {
    pub fn new() -> Self {
        let env_secrets = std::env::vars()
            .filter(|(name, value)| {
                let upper = name.to_uppercase();
                ["KEY", "TOKEN", "SECRET", "PASSWORD"]
                    .iter()
                    .any(|marker| upper.contains(marker))
                    && value.len() >= 8
            })
            .map(|(_, value)| value)
            .collect();

        let patterns = [
            r"sk-[A-Za-z0-9]{8,}",
            r"sk-ant-[A-Za-z0-9_-]{8,}",
            r"Bearer\s+\S+",
            r"ghp_[A-Za-z0-9]{8,}",
            r"xox[baprs]-[A-Za-z0-9-]{8,}",
        ]
        .iter()
        .filter_map(|p| Regex::new(p).ok())
        .collect();

        Self {
            env_secrets,
            patterns,
        }
    }

    pub fn redact(&self, text: &str) -> String {
        let mut out = text.to_string();
        for secret in &self.env_secrets {
            if out.contains(secret) {
                out = out.replace(secret, REDACTED);
            }
        }
        for pattern in &self.patterns {
            out = pattern.replace_all(&out, REDACTED).into_owned();
        }
        out
    }

    /// Byte index from which a trailing fragment of `text` could still grow
    /// into a redactable secret, if any: `text` ends with the *start* of one
    /// of the whitespace-spanning patterns — `Bearer` followed only by
    /// whitespace, its token not yet arrived.
    ///
    /// A streaming caller holds everything from this index back until more
    /// text arrives, so the token is never emitted without the pattern's
    /// start: `redact` matches `Bearer\s+\S+` only whole, and a fragment
    /// boundary between `Bearer` and its token would let the token leave
    /// unredacted (the fragment alone matches nothing).
    ///
    /// Scope, honestly stated: the whole-token shapes (`sk-…`, `ghp_…`,
    /// `xox…`) need no help — a whitespace-based hold-back already keeps a
    /// token whole. The env-snapshot secrets are whitespace-free tokens in
    /// practice and ride the same rule; a whitespace-*containing* env secret
    /// is out of scope. If a new whitespace-spanning pattern is added to
    /// `patterns` above, extend this method — `bearer_prefix_is_held_back`
    /// in the tests is the drift tripwire.
    pub fn secret_prefix_start(&self, text: &str) -> Option<usize> {
        let head = text.trim_end();
        let prefix = head.strip_suffix("Bearer")?;
        // The pattern needs no word boundary (`fooBearer x` redacts too), so
        // the hold-back needs none either — a false positive costs one held
        // word, a false negative leaks a token.
        Some(prefix.len())
    }

    /// Retrieval results are a JSON envelope, not free text: redact its text
    /// field without letting a pattern consume quotes or continuation metadata.
    /// Unknown/malformed envelopes retain the ordinary free-text policy.
    pub fn redact_tool_output(&self, tool: &str, output: &str) -> String {
        if tool == "retrieve_tool_output"
            && let Ok(mut value) = serde_json::from_str::<serde_json::Value>(output)
            && let Some(object) = value.as_object_mut()
            && object.len() == 5
            && object.get("text").is_some_and(|v| v.is_string())
            && ["start", "end", "total_bytes"]
                .iter()
                .all(|key| object.get(*key).is_some_and(|v| v.is_u64()))
            && object
                .get("next_start")
                .is_some_and(|v| v.is_null() || v.is_u64())
        {
            self.redact_value(&mut value);
            return value.to_string();
        }
        self.redact(output)
    }

    /// Deep-redact every string inside a JSON value.
    pub fn redact_value(&self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(s) => *s = self.redact(s),
            serde_json::Value::Array(items) => {
                for item in items {
                    self.redact_value(item);
                }
            }
            serde_json::Value::Object(map) => {
                for v in map.values_mut() {
                    self.redact_value(v);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_redaction_is_limited_to_retrieval_envelopes() {
        let redactor = Redactor::new();
        for (tool, output) in [
            ("retrieve_tool_output", "sk-abcdefghijk {broken"),
            ("retrieve_tool_output", r#"{"text":"sk-abcdefghijk"}"#),
            ("read_file", "Bearer token"),
        ] {
            assert_eq!(
                redactor.redact_tool_output(tool, output),
                redactor.redact(output)
            );
        }
    }

    #[test]
    fn redacts_common_token_patterns() {
        let redactor = Redactor::new();
        let out = redactor.redact("key is sk-abcdef123456 and auth Bearer tok.en-123");
        assert!(!out.contains("sk-abcdef123456"), "got: {out}");
        assert!(!out.contains("tok.en-123"), "got: {out}");
        assert!(out.contains(REDACTED));
    }

    #[test]
    fn keeps_short_and_benign_strings() {
        let redactor = Redactor::new();
        assert_eq!(redactor.redact("hello world"), "hello world");
    }

    /// The hold-back rule a streaming caller applies: a text ending in
    /// `Bearer` + whitespace is a pattern start whose token hasn't arrived.
    #[test]
    fn bearer_prefix_is_held_back() {
        let redactor = Redactor::new();
        assert_eq!(redactor.secret_prefix_start("auth: Bearer "), Some(6));
        assert_eq!(redactor.secret_prefix_start("Bearer "), Some(0));
        assert_eq!(redactor.secret_prefix_start("Bearer\n"), Some(0));
        // No boundary required — the regex matches mid-word too.
        assert_eq!(redactor.secret_prefix_start("fooBearer "), Some(3));
        // A completed pair is not a prefix: the pattern matches it whole.
        assert_eq!(redactor.secret_prefix_start("Bearer tok.en-123 "), None);
        assert_eq!(redactor.secret_prefix_start("Bearer\ntok "), None);
        assert_eq!(redactor.secret_prefix_start("plain text "), None);
        // Partial spellings of "Bearer" are whitespace-held upstream; this
        // method only guards the pattern's internal whitespace seam.
        assert_eq!(redactor.secret_prefix_start("x Bea"), None);
    }
}
