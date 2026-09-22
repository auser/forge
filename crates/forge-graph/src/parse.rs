//! Lightweight per-language extraction of symbols, imports, and call
//! sites using line/regex heuristics (no tree-sitter, no network).

use std::sync::OnceLock;

use regex::Regex;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawSymbol {
    pub name: String,
    pub kind: &'static str,
    pub line: u32,
}

#[derive(Debug, Clone, Default)]
pub struct FileParse {
    pub symbols: Vec<RawSymbol>,
    /// (raw import text, line)
    pub imports: Vec<(String, u32)>,
    /// (enclosing symbol name or "" for file scope, callee name, line)
    pub calls: Vec<(String, String, u32)>,
}

fn re(pattern: &str) -> &'static Regex {
    static REGEXES: std::sync::Mutex<Option<std::collections::HashMap<String, &'static Regex>>> =
        std::sync::Mutex::new(None);
    let mut guard = REGEXES.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(std::collections::HashMap::new);
    map.entry(pattern.to_string()).or_insert_with(|| {
        Box::leak(Box::new(Regex::new(pattern).unwrap_or_else(|_| {
            // Patterns are compile-time constants; an invalid one is a bug
            // we surface as a never-matching regex instead of panicking.
            Regex::new("a^").expect("fallback regex is valid")
        })))
    })
}

static CALL_RE: OnceLock<Regex> = OnceLock::new();

fn call_re() -> &'static Regex {
    CALL_RE.get_or_init(|| {
        Regex::new(r"\b([a-zA-Z_][a-zA-Z0-9_]*)\s*\(").expect("call regex is valid")
    })
}

fn stopwords(lang: &str) -> &'static [&'static str] {
    match lang {
        "rust" => &[
            "if", "while", "for", "match", "loop", "return", "fn", "struct", "enum", "impl",
            "trait", "mod", "use", "let", "const", "static", "pub", "where", "unsafe", "async",
            "move", "self", "Self", "super", "crate", "in", "as", "dyn", "box", "Some", "Ok",
            "Err", "None", "vec", "drop", "clone", "into", "from", "new",
        ],
        "python" => &[
            "if", "while", "for", "return", "def", "class", "import", "from", "with", "as", "elif",
            "else", "not", "and", "or", "in", "is", "lambda", "print", "len", "range", "str",
            "int", "list", "dict", "set", "tuple", "super", "self", "None", "True", "False",
        ],
        "javascript" => &[
            "if", "while", "for", "return", "function", "class", "import", "from", "require",
            "const", "let", "var", "new", "typeof", "switch", "case", "catch", "throw", "async",
            "await", "of", "in", "else", "do", "export", "default", "console",
        ],
        "go" => &[
            "if",
            "for",
            "return",
            "func",
            "type",
            "import",
            "package",
            "var",
            "const",
            "range",
            "switch",
            "case",
            "select",
            "go",
            "defer",
            "else",
            "struct",
            "interface",
            "map",
            "make",
            "new",
            "len",
            "cap",
            "append",
            "copy",
            "delete",
        ],
        _ => &[],
    }
}

/// Parse one file; dispatch on extension. Unknown languages yield an empty
/// parse (file node only).
pub fn parse_source(path: &str, content: &str) -> FileParse {
    let ext = path.rsplit('.').next().unwrap_or("");
    match ext {
        "rs" => parse_lang(content, &rust_defs(), &rust_imports(), "rust"),
        "py" => parse_lang(content, &python_defs(), &python_imports(), "python"),
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" => parse_lang(
            content,
            &javascript_defs(),
            &javascript_imports(),
            "javascript",
        ),
        "go" => parse_go(content),
        _ => FileParse::default(),
    }
}

type DefSpec = (&'static str, &'static str);

fn rust_defs() -> Vec<DefSpec> {
    vec![
        (
            r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)",
            "function",
        ),
        (
            r"^\s*(?:pub\s+)?struct\s+([A-Za-z_][A-Za-z0-9_]*)",
            "struct",
        ),
        (r"^\s*(?:pub\s+)?enum\s+([A-Za-z_][A-Za-z0-9_]*)", "enum"),
        (r"^\s*(?:pub\s+)?trait\s+([A-Za-z_][A-Za-z0-9_]*)", "trait"),
        (r"^\s*impl(?:<[^{}]*>)?\s+([A-Za-z_][A-Za-z0-9_:]*)", "impl"),
        (r"^\s*(?:pub\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)", "module"),
    ]
}

fn rust_imports() -> Vec<&'static str> {
    vec![r"^\s*(?:pub\s+)?use\s+([^;]+);"]
}

fn python_defs() -> Vec<DefSpec> {
    vec![
        (
            r"^\s*(?:async\s+)?def\s+([A-Za-z_][A-Za-z0-9_]*)",
            "function",
        ),
        (r"^\s*class\s+([A-Za-z_][A-Za-z0-9_]*)", "class"),
    ]
}

fn python_imports() -> Vec<&'static str> {
    vec![
        r"^\s*import\s+([A-Za-z_][A-Za-z0-9_\.]*)",
        r"^\s*from\s+([A-Za-z_][A-Za-z0-9_\.]*)\s+import",
    ]
}

fn javascript_defs() -> Vec<DefSpec> {
    vec![
        (
            r"^\s*(?:export\s+)?(?:async\s+)?function\s+([A-Za-z_][A-Za-z0-9_]*)",
            "function",
        ),
        (
            r"^\s*(?:export\s+)?class\s+([A-Za-z_][A-Za-z0-9_]*)",
            "class",
        ),
        (
            r"^\s*(?:export\s+)?(?:const|let|var)\s+([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(?:async\s*)?(?:\([^)]*\)|[A-Za-z_][A-Za-z0-9_]*)\s*=>",
            "function",
        ),
    ]
}

fn javascript_imports() -> Vec<&'static str> {
    vec![
        r#"^\s*import\s+.*?from\s+['"]([^'"]+)['"]"#,
        r#"\brequire\(\s*['"]([^'"]+)['"]\s*\)"#,
    ]
}

fn parse_lang(content: &str, defs: &[DefSpec], imports: &[&'static str], lang: &str) -> FileParse {
    let stop = stopwords(lang);
    let mut out = FileParse::default();
    for (idx, line) in content.lines().enumerate() {
        let line_no = (idx + 1) as u32;
        for (pattern, kind) in defs {
            if let Some(caps) = re(pattern).captures(line)
                && let Some(name) = caps.get(1)
            {
                out.symbols.push(RawSymbol {
                    name: name.as_str().to_string(),
                    kind,
                    line: line_no,
                });
                break;
            }
        }
        for pattern in imports {
            if let Some(caps) = re(pattern).captures(line)
                && let Some(raw) = caps.get(1)
            {
                out.imports.push((raw.as_str().trim().to_string(), line_no));
                break;
            }
        }
        for caps in call_re().captures_iter(line) {
            let Some(name) = caps.get(1) else { continue };
            let name_str = name.as_str();
            if stop.contains(&name_str) {
                continue;
            }
            // Skip call matches that are actually this line's definition.
            if out
                .symbols
                .iter()
                .any(|s| s.line == line_no && s.name == name_str)
            {
                continue;
            }
            let enclosing = out
                .symbols
                .iter()
                .rfind(|s| s.line <= line_no)
                .map(|s| s.name.clone())
                .unwrap_or_default();
            out.calls.push((enclosing, name_str.to_string(), line_no));
        }
    }
    out
}

/// Go needs import-block handling, so it gets its own pass.
fn parse_go(content: &str) -> FileParse {
    let stop = stopwords("go");
    let func_re = re(r"^func\s+(?:\([^)]*\)\s*)?([A-Za-z_][A-Za-z0-9_]*)");
    let type_re = re(r"^type\s+([A-Za-z_][A-Za-z0-9_]*)");
    let import_line_re = re(r#"^import\s+"([^"]+)""#);
    let import_block_start = re(r"^import\s*\($");
    let quoted_re = re(r#"^\s*"([^"]+)""#);

    let mut out = FileParse::default();
    let mut in_import_block = false;
    for (idx, line) in content.lines().enumerate() {
        let line_no = (idx + 1) as u32;
        if import_block_start.is_match(line) {
            in_import_block = true;
            continue;
        }
        if in_import_block {
            if line.trim() == ")" {
                in_import_block = false;
                continue;
            }
            if let Some(caps) = quoted_re.captures(line)
                && let Some(raw) = caps.get(1)
            {
                out.imports.push((raw.as_str().to_string(), line_no));
            }
            continue;
        }
        if let Some(caps) = import_line_re.captures(line)
            && let Some(raw) = caps.get(1)
        {
            out.imports.push((raw.as_str().to_string(), line_no));
            continue;
        }
        let def = func_re
            .captures(line)
            .map(|c| ("function", c))
            .or_else(|| type_re.captures(line).map(|c| ("type", c)));
        if let Some((kind, caps)) = def
            && let Some(name) = caps.get(1)
        {
            out.symbols.push(RawSymbol {
                name: name.as_str().to_string(),
                kind,
                line: line_no,
            });
        }
        for caps in call_re().captures_iter(line) {
            let Some(name) = caps.get(1) else { continue };
            let name_str = name.as_str();
            if stop.contains(&name_str) {
                continue;
            }
            if out
                .symbols
                .iter()
                .any(|s| s.line == line_no && s.name == name_str)
            {
                continue;
            }
            let enclosing = out
                .symbols
                .iter()
                .rfind(|s| s.line <= line_no)
                .map(|s| s.name.clone())
                .unwrap_or_default();
            out.calls.push((enclosing, name_str.to_string(), line_no));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_symbols_imports_and_calls() {
        let src = "use crate::helper::util;\n\npub fn main_fn() {\n    util();\n    helper_fn();\n}\n\nfn helper_fn() {}\n\npub struct Thing;\n";
        let parsed = parse_source("src/main.rs", src);
        let names: Vec<&str> = parsed.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"main_fn"));
        assert!(names.contains(&"helper_fn"));
        assert!(names.contains(&"Thing"));
        assert_eq!(parsed.imports.len(), 1);
        assert_eq!(parsed.imports[0].0, "crate::helper::util");
        let callees: Vec<&str> = parsed.calls.iter().map(|(_, c, _)| c.as_str()).collect();
        assert!(callees.contains(&"util"), "calls: {:?}", parsed.calls);
        // Calls inside main_fn are attributed to it.
        assert!(
            parsed
                .calls
                .iter()
                .any(|(enc, callee, _)| enc == "main_fn" && callee == "helper_fn")
        );
    }

    #[test]
    fn python_symbols_imports_and_calls() {
        let src = "import os\nfrom pkg.mod import thing\n\ndef top():\n    helper()\n\nclass Klass:\n    pass\n";
        let parsed = parse_source("a/b.py", src);
        let names: Vec<&str> = parsed.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"top"));
        assert!(names.contains(&"Klass"));
        let raws: Vec<&str> = parsed.imports.iter().map(|(r, _)| r.as_str()).collect();
        assert!(raws.contains(&"os"));
        assert!(raws.contains(&"pkg.mod"));
        assert!(
            parsed
                .calls
                .iter()
                .any(|(enc, callee, _)| enc == "top" && callee == "helper")
        );
    }

    #[test]
    fn javascript_symbols_and_imports() {
        let src = "import { x } from './dep';\nconst dep2 = require('./dep2');\n\nexport function doIt() {\n  x();\n}\n\nconst arrow = (a) => a + 1;\nclass Widget {}\n";
        let parsed = parse_source("src/app.ts", src);
        let names: Vec<&str> = parsed.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"doIt"), "{names:?}");
        assert!(names.contains(&"arrow"), "{names:?}");
        assert!(names.contains(&"Widget"), "{names:?}");
        let raws: Vec<&str> = parsed.imports.iter().map(|(r, _)| r.as_str()).collect();
        assert!(raws.contains(&"./dep"), "{raws:?}");
        assert!(raws.contains(&"./dep2"), "{raws:?}");
    }

    #[test]
    fn go_symbols_and_block_imports() {
        let src = "package main\n\nimport (\n\t\"fmt\"\n\t\"os\"\n)\n\nfunc main() {\n\tfmt.Println(\"x\")\n}\n\ntype Server struct{}\n";
        let parsed = parse_source("cmd/main.go", src);
        let names: Vec<&str> = parsed.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"main"), "{names:?}");
        assert!(names.contains(&"Server"), "{names:?}");
        let raws: Vec<&str> = parsed.imports.iter().map(|(r, _)| r.as_str()).collect();
        assert!(raws.contains(&"fmt"), "{raws:?}");
        assert!(raws.contains(&"os"), "{raws:?}");
    }

    #[test]
    fn unknown_language_yields_empty_parse() {
        let parsed = parse_source("notes.txt", "fn notRust() {}");
        assert!(parsed.symbols.is_empty());
        assert!(parsed.imports.is_empty());
    }
}
