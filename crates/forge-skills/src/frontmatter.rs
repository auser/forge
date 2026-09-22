//! Minimal YAML-frontmatter handling for `SKILL.md` files. Tolerates
//! missing frontmatter, quoted values, and extra keys.

/// Split a `SKILL.md` into (frontmatter fields, body). When no
/// frontmatter block exists, fields are empty and the body is the whole
/// content.
pub fn split(content: &str) -> (Vec<(String, String)>, &str) {
    let trimmed = content.trim_start_matches(['\u{feff}']);
    let Some(rest) = trimmed.strip_prefix("---") else {
        return (Vec::new(), content);
    };
    // Opening fence must be alone on its line.
    let rest = match rest
        .strip_prefix('\n')
        .or_else(|| rest.strip_prefix("\r\n"))
    {
        Some(r) => r,
        None => return (Vec::new(), content),
    };
    let mut fields = Vec::new();
    let mut offset = 0usize;
    for line in rest.lines() {
        let line_len = line.len() + 1; // include '\n'
        if line.trim() == "---" || line.trim() == "..." {
            let body = &rest[offset + line_len..];
            return (fields, body);
        }
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim().to_string();
            let value = value
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string();
            if !key.is_empty() {
                fields.push((key, value));
            }
        }
        offset += line_len;
    }
    // Unterminated frontmatter: treat everything as body.
    (Vec::new(), content)
}

pub fn field<'a>(fields: &'a [(String, String)], key: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_str())
}

/// First non-heading, non-empty body line; fallback description for
/// skills without frontmatter.
pub fn first_prose_line(body: &str) -> Option<String> {
    body.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.chars().take(200).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_and_body() {
        let (fields, body) =
            split("---\nname: demo\ndescription: \"Does things\"\n---\n# Demo\n\nBody text.\n");
        assert_eq!(field(&fields, "name"), Some("demo"));
        assert_eq!(field(&fields, "description"), Some("Does things"));
        assert!(body.contains("# Demo"));
        assert!(body.contains("Body text."));
    }

    #[test]
    fn tolerates_missing_frontmatter() {
        let (fields, body) = split("# Just a title\n\nSome prose here.\n");
        assert!(fields.is_empty());
        assert_eq!(first_prose_line(body).as_deref(), Some("Some prose here."));
    }

    #[test]
    fn tolerates_unterminated_frontmatter() {
        let (fields, body) = split("---\nname: broken\n# never closed");
        assert!(fields.is_empty());
        assert!(body.contains("broken"));
    }
}
