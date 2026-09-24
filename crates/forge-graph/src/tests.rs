use std::path::Path;

use forge_core::ProjectGraph;

use super::*;

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, content).expect("write");
}

/// Small fixture: two Rust files (one importing the other, with a call),
/// one Python file, one test file, one doc.
fn fixture(root: &Path) {
    write(
        root,
        "src/main.rs",
        "use crate::helper::util;\n\nfn main() {\n    util();\n    compute();\n}\n\nfn compute() -> i32 {\n    42\n}\n",
    );
    write(
        root,
        "src/helper.rs",
        "pub fn util() {\n    println!(\"hi\");\n}\n\npub struct Helper;\n",
    );
    write(
        root,
        "scripts/tool.py",
        "import os\n\ndef run():\n    os.getcwd()\n",
    );
    write(
        root,
        "tests/main_test.rs",
        "#[test]\nfn it_works() {\n    assert!(true);\n}\n",
    );
    write(root, "README.md", "# fixture\n");
}

#[test]
fn build_indexes_files_symbols_and_imports() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fixture(tmp.path());

    let mut graph = LocalGraph::open(tmp.path()).expect("open");
    let stats = graph.build().expect("build");

    assert_eq!(stats.files, 5);
    assert_eq!(stats.tests, 1);
    let names: Vec<&str> = graph
        .state()
        .symbols
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    for expected in ["main", "compute", "util", "Helper", "run", "it_works"] {
        assert!(names.contains(&expected), "missing {expected} in {names:?}");
    }

    let edge = graph
        .state()
        .imports
        .iter()
        .find(|e| e.from == "src/main.rs")
        .expect("import edge");
    assert_eq!(edge.raw, "crate::helper::util");
    assert_eq!(edge.resolved.as_deref(), Some("src/helper.rs"));

    // graph.json was persisted and round-trips.
    let reopened = LocalGraph::open(tmp.path()).expect("reopen");
    assert_eq!(reopened.state().files.len(), 5);
}

#[test]
fn freshness_lifecycle() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fixture(tmp.path());

    let mut graph = LocalGraph::open(tmp.path()).expect("open");
    graph.build().expect("build");
    assert!(graph.is_fresh(), "fresh right after build");
    assert!(ProjectGraph::is_fresh(&graph));

    // Touch content → stale (modified).
    write(
        tmp.path(),
        "src/helper.rs",
        "pub fn util() {\n    println!(\"changed\");\n}\n\npub struct Helper;\n",
    );
    let report = graph.freshness().expect("freshness");
    assert!(!report.fresh);
    assert_eq!(report.modified, vec!["src/helper.rs".to_string()]);

    // New file → stale (added); deletion → stale (removed).
    let mut graph = LocalGraph::open(tmp.path()).expect("open");
    graph.build().expect("rebuild");
    write(tmp.path(), "src/extra.rs", "fn extra() {}\n");
    let report = graph.freshness().expect("freshness");
    assert_eq!(report.added, vec!["src/extra.rs".to_string()]);
    std::fs::remove_file(tmp.path().join("src/extra.rs")).expect("remove");
    std::fs::remove_file(tmp.path().join("README.md")).expect("remove");
    let report = graph.freshness().expect("freshness");
    assert_eq!(report.removed, vec!["README.md".to_string()]);
}

#[test]
fn incremental_build_reparses_only_changed_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fixture(tmp.path());

    let mut graph = LocalGraph::open(tmp.path()).expect("open");
    let (_, report) = graph.build_report().expect("build");
    assert_eq!(report.parsed.len(), 5, "first build parses everything");

    // No changes → everything reused, nothing reparsed.
    let (_, report) = graph.build_report().expect("rebuild");
    assert!(report.parsed.is_empty(), "nothing reparsed: {report:?}");
    assert_eq!(report.reused.len(), 5);

    // Change one file → only it is reparsed.
    write(
        tmp.path(),
        "scripts/tool.py",
        "import os\n\ndef run():\n    os.getcwd()\n\ndef extra_fn():\n    pass\n",
    );
    let (stats, report) = graph.build_report().expect("incremental rebuild");
    assert_eq!(report.parsed, vec!["scripts/tool.py".to_string()]);
    assert_eq!(report.reused.len(), 4);
    assert!(graph.state().symbols.iter().any(|s| s.name == "extra_fn"));
    assert_eq!(stats.files, 5);
}

#[test]
fn callers_blast_grep_map_context() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fixture(tmp.path());
    let mut graph = LocalGraph::open(tmp.path()).expect("open");
    graph.build().expect("build");

    let callers = graph.callers("util");
    assert_eq!(callers.len(), 1);
    assert_eq!(callers[0].name, "main");

    let blast = graph.blast("src/helper.rs");
    assert_eq!(blast, vec!["src/main.rs".to_string()]);

    let matches = ProjectGraph::grep(&graph, "compute").expect("grep");
    assert!(
        matches.iter().any(|m| m.file == *"src/main.rs"),
        "matches: {matches:?}"
    );

    let map = graph.map();
    let src = map.iter().find(|d| d.dir == "src").expect("src dir");
    assert_eq!(src.files.get("source"), Some(&2));
    assert!(src.symbols >= 4);
    let tests = map.iter().find(|d| d.dir == "tests").expect("tests dir");
    assert_eq!(tests.files.get("test"), Some(&1));

    let hits = graph.context("helper utility function", 10);
    assert!(!hits.is_empty());
    assert_eq!(hits[0].path, "src/helper.rs");
}

#[test]
fn embedding_candidates_key_and_text_format_disambiguate_same_named_symbols() {
    let tmp = tempfile::tempdir().expect("tempdir");
    write(tmp.path(), "src/a.rs", "pub fn run() {}\n");
    write(tmp.path(), "src/b.rs", "pub fn run() {}\n");

    let mut graph = LocalGraph::open(tmp.path()).expect("open");
    graph.build().expect("build");

    let candidates = graph.embedding_candidates();
    let a = candidates
        .iter()
        .find(|(key, _, _)| key == "src/a.rs::run")
        .expect("src/a.rs::run present");
    let b = candidates
        .iter()
        .find(|(key, _, _)| key == "src/b.rs::run")
        .expect("src/b.rs::run present");

    assert_eq!(a.2, "function run in src/a.rs");
    assert_eq!(b.2, "function run in src/b.rs");
    // Same symbol name, different files: distinct keys, distinct hashes
    // (the embedded text differs by path).
    assert_ne!(a.0, b.0);
    assert_ne!(a.1, b.1);
}

/// One bad byte in one file must never take down the whole graph build.
/// A binary-content `.rs` file (source-like extension, non-UTF-8 bytes)
/// and a binary file with no extension at all must both be indexed
/// (hashed, counted, classified) without crashing the build; since their
/// content isn't valid source text, symbol/import extraction is skipped
/// for exactly those two files while every other file parses normally.
#[test]
fn build_tolerates_non_utf8_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fixture(tmp.path());

    // Every byte value 0..=255, repeated to 512 bytes: guaranteed to
    // contain invalid UTF-8 sequences (e.g. a lone 0xFF), deterministic
    // and reproducible (not `rand`, no flakiness).
    let binary: Vec<u8> = (0u8..=255u8).cycle().take(512).collect();
    std::fs::write(tmp.path().join("src/notes.rs"), &binary).expect("write binary .rs");
    std::fs::write(tmp.path().join("blob"), &binary).expect("write binary no-ext");

    let mut graph = LocalGraph::open(tmp.path()).expect("open");
    let stats = graph
        .build()
        .expect("build must succeed despite non-UTF-8 file content");

    // Binary files are still indexed as files (fixture's 5 + these 2).
    assert_eq!(stats.files, 7);
    assert!(graph.state().files.contains_key("src/notes.rs"));
    assert!(graph.state().files.contains_key("blob"));

    // ...but contribute no symbols: their content isn't parseable source
    // text, so symbol/import extraction is skipped for them.
    assert!(
        graph
            .state()
            .symbols
            .iter()
            .all(|s| s.file != "src/notes.rs" && s.file != "blob"),
        "binary files must not yield symbols: {:?}",
        graph.state().symbols
    );
    assert!(
        graph
            .state()
            .imports
            .iter()
            .all(|i| i.from != "src/notes.rs" && i.from != "blob")
    );

    // Every other (valid UTF-8) file still parses normally alongside them.
    let names: Vec<&str> = graph
        .state()
        .symbols
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    for expected in ["main", "compute", "util", "run"] {
        assert!(names.contains(&expected), "missing {expected} in {names:?}");
    }

    // `forge graph build` is idempotent: rebuilding reuses everything,
    // including the binary files, without re-crashing.
    let (_, report) = graph.build_report().expect("rebuild must also succeed");
    assert!(
        report.parsed.is_empty(),
        "second build reuses everything: {report:?}"
    );
    assert_eq!(report.reused.len(), 7);
}

#[test]
fn skips_common_directories() {
    let tmp = tempfile::tempdir().expect("tempdir");
    fixture(tmp.path());
    write(tmp.path(), "target/generated.rs", "fn junk() {}\n");
    write(
        tmp.path(),
        "node_modules/pkg/index.js",
        "function junk() {}\n",
    );
    write(tmp.path(), ".git/ignored.rs", "fn junk2() {}\n");

    let mut graph = LocalGraph::open(tmp.path()).expect("open");
    let stats = graph.build().expect("build");
    assert_eq!(stats.files, 5);
    assert!(
        graph
            .state()
            .symbols
            .iter()
            .all(|s| !s.name.starts_with("junk"))
    );
}
