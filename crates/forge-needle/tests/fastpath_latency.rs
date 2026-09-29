//! What the agent loop's needle fast path actually costs.
//!
//! `forge-runtime`'s `needle_fast_path` asks needle to pick a tool before
//! falling back to the chat model. Whether that is a win depends entirely on a
//! number nobody had measured: how long `tool_call` takes with forge's real
//! tool surface. A fast path slower than the model it is trying to avoid is a
//! tax on every turn, and its budget cannot be chosen without this figure.
//!
//! The tools JSON below mirrors `forge_runtime::tools::tool_definitions()`
//! exactly (7 tools, same names/descriptions/schemas). It is duplicated rather
//! than imported because `forge-needle` sits *below* `forge-runtime` in the
//! dependency graph; `needle_fastpath_measurement_uses_this_tool_surface` in
//! `crates/forge-runtime/src/tools.rs` fails if the two drift.
//!
//! ```sh
//! FORGE_NEEDLE_E2E_WEIGHTS=~/.cache/forge/models/needle3.cact \
//!   cargo test --release -p forge-needle --features "ffi needle-e2e" \
//!   --test fastpath_latency -- --nocapture
//! ```
//!
//! Release build only: debug-build inference numbers say nothing about what a
//! user experiences.

#![cfg(all(feature = "ffi", feature = "needle-e2e"))]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use forge_needle::{FfiBackend, NeedleBackend};

/// Byte-for-byte the surface `forge_runtime::tools::tool_definitions()`
/// serializes to. Size matters: `needle_init` installs and tokenizes this on
/// every call, so a measurement against a toy two-tool catalogue would
/// understate the real cost.
const TOOLS_JSON: &str = r#"[
{"name":"read_file","description":"Read a file's contents (project-root-relative path).","parameters":{"type":"object","properties":{"path":{"type":"string","description":"file path"}},"required":["path"]}},
{"name":"write_file","description":"Write (create or overwrite) a file. Project-root-relative path.","parameters":{"type":"object","properties":{"path":{"type":"string","description":"file path"},"content":{"type":"string","description":"full new file content"}},"required":["path","content"]}},
{"name":"edit_file","description":"Replace an exact string in a file. Fails when `old` is absent or ambiguous.","parameters":{"type":"object","properties":{"path":{"type":"string","description":"file path"},"old":{"type":"string","description":"exact text to replace"},"new":{"type":"string","description":"replacement text"}},"required":["path","old","new"]}},
{"name":"delete_file","description":"Delete a file. Destructive.","parameters":{"type":"object","properties":{"path":{"type":"string","description":"file path"}},"required":["path"]}},
{"name":"run_command","description":"Run a shell command. Risk is at least `risky` regardless of the hint; the hint can only raise it to `destructive`.","parameters":{"type":"object","properties":{"command":{"type":"string","description":"command to run"},"args":{"type":"array","items":{"type":"string"}},"risk":{"type":"string","description":"risk hint: safe|risky|destructive"}},"required":["command"]}},
{"name":"graph_context","description":"Select the most relevant project files for a query (project graph).","parameters":{"type":"object","properties":{"query":{"type":"string","description":"relevance query"}},"required":["query"]}},
{"name":"graph_grep","description":"Search project graph symbols and imports by pattern.","parameters":{"type":"object","properties":{"pattern":{"type":"string","description":"substring or regex"}},"required":["pattern"]}}
]"#;

/// Prompts spanning what the fast path actually meets: two that map cleanly to
/// a read-only tool (the only kind it may dispatch), one ambiguous, and one
/// that should be declined. A budget tuned only on the easy cases would be
/// wrong, because a decline costs a full generation too.
const PROMPTS: &[(&str, &str)] = &[
    ("clean read", "Read the file src/main.rs"),
    ("clean read", "Search the project for the symbol serve_http"),
    ("ambiguous", "Tell me about this project"),
    ("should decline", "What is the capital of France?"),
];

fn weights() -> PathBuf {
    PathBuf::from(
        std::env::var("FORGE_NEEDLE_E2E_WEIGHTS")
            .expect("set FORGE_NEEDLE_E2E_WEIGHTS to a .cact archive"),
    )
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx]
}

/// One test function, not several: `libneedle` is a single process-global,
/// non-thread-safe model and the test harness runs functions on parallel
/// threads. See `e2e.rs` for the longer version of this reasoning.
#[test]
fn fast_path_tool_call_latency() {
    let mut backend = FfiBackend::new(weights());

    let load_started = Instant::now();
    backend.load().expect("weights load");
    let load = load_started.elapsed();

    // Separated from the samples below on purpose. The very first call pays
    // for the static-prefix tokenization; the agent loop pays that once per
    // process, but every *subsequent* turn pays only the warm cost, and it is
    // the warm cost that a per-turn budget has to cover.
    let cold_started = Instant::now();
    let _ = backend.tool_call(PROMPTS[0].1, TOOLS_JSON);
    let cold = cold_started.elapsed();

    let mut all = Vec::new();
    println!("\n=== needle tool_call latency (7 tools, release) ===");
    println!("weights load       {load:>10.1?}");
    println!("first call (cold)  {cold:>10.1?}");
    println!();

    for (kind, prompt) in PROMPTS {
        let mut samples = Vec::new();
        for _ in 0..3 {
            let started = Instant::now();
            let outcome = backend.tool_call(prompt, TOOLS_JSON);
            let elapsed = started.elapsed();
            samples.push(elapsed);
            all.push(elapsed);
            let picked = match &outcome {
                Ok(Some(call)) => format!("{} (conf {:.2})", call.name, call.confidence),
                Ok(None) => "declined".to_string(),
                Err(e) => format!("error: {e}"),
            };
            println!("{elapsed:>10.1?}  [{kind}] {prompt:.40} -> {picked}");
        }
        samples.sort();
        println!(
            "           median {:.1?} for [{kind}]\n",
            percentile(&samples, 0.5)
        );
    }

    all.sort();
    let p50 = percentile(&all, 0.5);
    let p90 = percentile(&all, 0.9);
    let max = *all.last().expect("samples");
    println!("--- warm tool_call: p50 {p50:.1?}  p90 {p90:.1?}  max {max:.1?} ---");
    println!(
        "A fast path is only worth having if it beats the chat model it avoids.\n\
         Compare p90 against the observed local-model turn cost (~6.5-7.5 s on\n\
         Qwen3-Coder-Next-4bit via oMLX) and against router_timeout_ms, which is\n\
         what `needle_fast_path` currently uses as its budget.\n"
    );

    // Not a perf gate — the numbers above are the output. This only catches the
    // engine being outright broken, which would make every number meaningless.
    assert!(
        max < Duration::from_secs(120),
        "tool_call wedged: {max:.1?}"
    );
}
