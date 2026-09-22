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
}
