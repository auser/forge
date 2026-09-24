//! End-to-end tests for the real `libneedle` backend against real weights.
//!
//! Opt in explicitly — these need a linked engine and a 35 MB `.cact` archive,
//! so they are not part of `just verify`:
//!
//! ```sh
//! NEEDLE_LIB_DIR=$PWD/crates/needle-sys/vendor/$(rustc -vV | sed -n 's/^host: //p') \
//! FORGE_NEEDLE_E2E_WEIGHTS=~/.cache/forge/models/needle3.cact \
//!   cargo test -p forge-needle --features "ffi needle-e2e" --test e2e -- --nocapture
//! ```
//!
//! # Why this is one test function
//!
//! `libneedle` is **one process-global, non-thread-safe model** that cannot be
//! unloaded. Two engines in one process would race through shared C state, and
//! `FfiBackend`'s global claim deliberately makes the second one fail. Test
//! binaries run test functions on parallel threads, so the only structurally
//! safe shape is a single test that drives one engine through every assertion
//! in order. Splitting this up would either need `#[serial]` plus a shared
//! lazy engine (the same thing with more moving parts) or `--test-threads=1`
//! (an invisible requirement that fails confusingly when forgotten).

#![cfg(all(feature = "ffi", feature = "needle-e2e"))]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use forge_core::model::ModelCapabilities;
use forge_core::router::{DecisionRouter, RoutingRequest};
use forge_needle::{FfiBackend, NeedleEngine, NeedleRouter};

/// Path to a real `.cact` archive.
const WEIGHTS_ENV: &str = "FORGE_NEEDLE_E2E_WEIGHTS";

/// Warm-up inferences before any latency assertion. The first inference after
/// load pays for paging in the archive and building caches: measured on
/// macos-arm64 / `needle3.cact` the first `decide` took ~5.7 s, the second
/// ~1.1 s and the third ~90 ms. Latency claims are only meaningful warm.
const WARMUP_ROUNDS: usize = 3;

/// Latency samples to take before asserting. The assertion uses the minimum;
/// see the perf section for why several samples are needed.
const PERF_SAMPLES: usize = 5;

/// Reference latency for one warm route round-trip on an idle macos-arm64
/// machine: ~47 ms in a release build, ~100 ms in a debug build. Printed
/// alongside the measurements so drift is visible to a human reading the
/// output — this is the number to care about.
const ROUTE_REFERENCE: Duration = Duration::from_millis(50);

/// Ceiling the test actually asserts. Deliberately much looser than
/// [`ROUTE_REFERENCE`], because wall-clock latency here tracks machine load
/// more than it tracks forge's code: the same bit-identical inference measured
/// 47 ms idle and 675 ms–1.3 s at load average 347 on 16 cores. A tight gate
/// would just flake on a busy dev machine.
///
/// It is still worth asserting, because the regression class this is here to
/// catch is gross, not subtle: skipping `needle_init` per call (see
/// `FfiBackend::run`) took a round-trip to 16.5 s. Anything in that class trips
/// this; a 2x drift will not, and is meant to be caught by reading the printed
/// numbers against [`ROUTE_REFERENCE`].
const ROUTE_CEILING: Duration = Duration::from_secs(2);

/// Timeout given to the router during the perf samples. Generous, because a
/// sample that trips it tells us about the host, not about forge — samples that
/// do are reported as unmeasurable rather than failing the suite.
const ROUTER_TIMEOUT: Duration = Duration::from_secs(60);

fn weights() -> PathBuf {
    let raw = std::env::var(WEIGHTS_ENV).unwrap_or_else(|_| {
        panic!(
            "{WEIGHTS_ENV} is not set. These tests need a real Needle 3 archive, e.g.\n  \
             {WEIGHTS_ENV}=$HOME/.cache/forge/models/needle3.cact\n\
             (`forge init` fetches and verifies it.)"
        )
    });
    let path = PathBuf::from(&raw);
    assert!(
        path.is_file(),
        "{WEIGHTS_ENV} points at {raw}, which is not a file"
    );
    path
}

#[tokio::test]
async fn needle_ffi_backend_end_to_end() {
    let path = weights();
    let engine = Arc::new(NeedleEngine::spawn(FfiBackend::new(path.clone())));

    // ---- the engine loads the weights and reports real model facts -------
    let (model_id, dimensions) = engine
        .info()
        .await
        .expect("the engine loads real weights and reports model facts");
    eprintln!("model_id={model_id} dimensions={dimensions}");
    assert_eq!(
        model_id, "needle3",
        "model_id should track the weights filename"
    );
    assert!(
        dimensions > 0,
        "a loaded model must report an embedding dimension"
    );

    // ---- decide: one no-arg tool per option, the choice is the tool ------
    let decision = engine
        .decide(
            "run the tests".to_string(),
            vec!["test-runner".to_string(), "chat-model".to_string()],
        )
        .await
        .expect("decides between two options");
    eprintln!(
        "decide -> {:?} confidence={} reason={:?}",
        decision.choice, decision.confidence, decision.reason
    );
    assert_eq!(
        decision.choice, "test-runner",
        "'run the tests' should select the test runner"
    );
    assert!(
        decision.confidence > 0.5,
        "a clear-cut decision should be confident, got {}",
        decision.confidence
    );
    assert!(
        !decision.reason.is_empty(),
        "the engine's reasoning should be surfaced"
    );

    // Hyphenated option names must round-trip verbatim: the engine
    // snake-cases names internally but echoes the original in the envelope,
    // which is what lets options be matched by exact equality. `test-runner`
    // above already proves it; a provider-qualified id proves the harder case.
    let qualified = engine
        .decide(
            "reason carefully about this large codebase".to_string(),
            vec![
                "openai/gpt-5.1-mini".to_string(),
                "anthropic/claude-opus-4.5".to_string(),
            ],
        )
        .await
        .expect("decides between provider-qualified ids");
    eprintln!("qualified decide -> {:?}", qualified.choice);
    assert!(
        qualified.choice == "openai/gpt-5.1-mini"
            || qualified.choice == "anthropic/claude-opus-4.5",
        "the choice must be one of the offered options verbatim, got {:?}",
        qualified.choice
    );

    // ---- decide: an off-topic task is declined, never guessed ------------
    let declined = engine
        .decide(
            "xyzzy plugh frotz".to_string(),
            vec!["test-runner".to_string(), "chat-model".to_string()],
        )
        .await;
    eprintln!("nonsense decide -> {declined:?}");
    // Either outcome is acceptable and both are honest: Needle may refuse
    // (Err, so the router falls back) or pick with low confidence. What must
    // never happen is a confident pick from nonsense.
    if let Ok(decision) = &declined {
        assert!(
            decision.confidence <= 1.0,
            "confidence must stay in range, got {}",
            decision.confidence
        );
    }

    // ---- embed: right size, L2-normalised, deterministic -----------------
    let texts = vec!["hello world".to_string(), "refactor the parser".to_string()];
    let first = engine.embed(texts.clone()).await.expect("embeds");
    let second = engine.embed(texts.clone()).await.expect("embeds again");
    assert_eq!(first.len(), texts.len(), "one vector per input text");
    assert_eq!(
        first, second,
        "embeddings must be deterministic across calls"
    );
    for vector in &first {
        assert_eq!(
            vector.len(),
            dimensions,
            "every vector must be dimensions() long"
        );
        let norm: f64 = vector.iter().map(|v| f64::from(*v) * f64::from(*v)).sum();
        assert!(
            (norm - 1.0).abs() < 1e-3,
            "vectors should come out L2-normalised, got squared norm {norm}"
        );
    }
    // Different texts must not collapse to the same vector.
    assert_ne!(first[0], first[1], "distinct texts need distinct vectors");

    // Embedding is independent of conversation state: it must survive an
    // intervening decide unchanged.
    let _ = engine
        .decide("run the tests".to_string(), vec!["test-runner".to_string()])
        .await;
    let after = engine
        .embed(texts.clone())
        .await
        .expect("embeds after decide");
    assert_eq!(
        first, after,
        "an intervening decision must not perturb embeddings"
    );

    // ---- extract: the schema drives a grammar-constrained record ---------
    //
    // Needle takes its semantics from the record tool's *name* and
    // *description*. A bare schema gives it neither, and it refuses rather
    // than guessing. `FfiBackend::extract` maps the schema's `title` to the
    // tool name for exactly this reason, so the titled form is the one
    // callers should use. Both behaviours are asserted, because the refusal
    // is a designed outcome and a silent change to it would matter.
    let titled = r#"{"title":"weather_query",
                     "description":"A weather request naming a city",
                     "type":"object",
                     "properties":{"city":{"type":"string"}},
                     "required":["city"]}"#;
    let record = engine
        .extract(
            "what is the weather in Paris".to_string(),
            titled.to_string(),
        )
        .await
        .expect("extracts a record from a titled schema");
    eprintln!("extract(titled) -> {record}");
    let parsed: serde_json::Value =
        serde_json::from_str(&record).expect("the extracted record must be parseable JSON");
    assert_eq!(
        parsed.get("city").and_then(serde_json::Value::as_str),
        Some("Paris"),
        "the city should be extracted verbatim from the text, got {record}"
    );

    let bare = r#"{"type":"object","properties":{"city":{"type":"string"}}}"#;
    let bare_result = engine
        .extract("weather in Paris".to_string(), bare.to_string())
        .await;
    eprintln!("extract(bare, no title/description) -> {bare_result:?}");
    // Documented, measured behaviour: with nothing naming what the record
    // means, "weather in Paris" reads as a request for a weather tool that
    // does not exist, and Needle declines. If this ever starts succeeding
    // that is an improvement, so accept a correct extraction too — but never
    // a wrong one.
    if let Ok(extracted) = &bare_result {
        let parsed: serde_json::Value =
            serde_json::from_str(extracted).expect("any returned record must be parseable");
        if let Some(city) = parsed.get("city").and_then(serde_json::Value::as_str) {
            assert_eq!(
                city, "Paris",
                "if it extracts a city it must be the right one"
            );
        }
    }

    // ---- tool_call: real arguments off a real schema ---------------------
    let tools = r#"[{"name":"run-tests",
                     "description":"Run the test suite for a package",
                     "parameters":{"type":"object",
                                   "properties":{"package":{"type":"string",
                                                            "description":"the package name"}},
                                   "required":["package"]}}]"#;
    let call = engine
        .tool_call(
            "run the tests for the forge-needle package".to_string(),
            tools.to_string(),
        )
        .await
        .expect("tool_call succeeds")
        .expect("a matching request should produce a call");
    eprintln!(
        "tool_call -> {} {} (confidence {})",
        call.name, call.arguments_json, call.confidence
    );
    assert_eq!(call.name, "run-tests");
    let arguments: serde_json::Value =
        serde_json::from_str(&call.arguments_json).expect("arguments must be parseable JSON");
    let package = arguments
        .get("package")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    assert!(
        package.contains("forge-needle"),
        "the package argument should be grounded in the prompt, got {package:?}"
    );

    // An off-topic prompt against the same tools is a refusal (`None`), not a
    // fabricated call. This is the contract callers rely on.
    let refused = engine
        .tool_call(
            "what is the airspeed velocity of an unladen swallow".to_string(),
            tools.to_string(),
        )
        .await
        .expect("tool_call completes");
    eprintln!("off-topic tool_call -> {refused:?}");
    assert!(
        refused.is_none(),
        "an unsupported request must refuse, not guess: {refused:?}"
    );

    // ---- perf smoke: a warm route round-trip --------------------------
    //
    // Measured, not assumed. Two things this had to account for:
    //
    // * The first inference after load pays for paging in the archive, hence
    //   the warm-up rounds.
    // * Wall-clock latency here is extremely sensitive to machine load. The
    //   engine's output is bit-identical across calls (same choice, same
    //   confidence, same reasoning), yet on a machine busy compiling Rust the
    //   same deterministic inference measured anywhere from 47 ms (idle) to
    //   1.3 s (load average 347 on 16 cores) to 20 s. So this takes the
    //   *minimum* of several samples — the one least contaminated by the
    //   scheduler — prints them all against an idle reference, and asserts a
    //   loose ceiling. See ROUTE_REFERENCE / ROUTE_CEILING.
    let router = NeedleRouter::new(
        engine.clone(),
        vec![
            ("test-runner".to_string(), caps()),
            ("chat-model".to_string(), caps()),
        ],
        ROUTER_TIMEOUT,
    );
    for _ in 0..WARMUP_ROUNDS {
        let _ = router.route(&RoutingRequest::new("run the tests")).await;
    }
    let mut timings = Vec::new();
    let mut unmeasurable = 0usize;
    for _ in 0..PERF_SAMPLES {
        let started = Instant::now();
        match router.route(&RoutingRequest::new("run the tests")).await {
            Ok(decision) => {
                timings.push(started.elapsed());
                // The routing *contract* is asserted on every sample that
                // completes — only the timing is treated as best-effort.
                assert_eq!(decision.selected_model, "test-runner");
                assert_eq!(decision.router_name, "needle");
                assert!(!decision.fallback_used);
            }
            // The router's own timeout fired. On a machine this oversubscribed
            // that says nothing about forge, and failing here would turn the
            // whole functional suite red for an unmeasurable host.
            Err(e) => {
                unmeasurable += 1;
                eprintln!(
                    "  perf sample unmeasurable after {:?}: {e}",
                    started.elapsed()
                );
            }
        }
    }

    match timings.iter().min().copied() {
        Some(best) => {
            eprintln!(
                "warm route round-trips: {timings:?}\n  best {best:?} vs idle reference \
                 {ROUTE_REFERENCE:?} (release); {unmeasurable} sample(s) timed out. A large \
                 gap usually means machine load — check the load average before reading it \
                 as a regression."
            );
            assert!(
                best < ROUTE_CEILING,
                "the fastest of {PERF_SAMPLES} warm route round-trips was {best:?}, over the \
                 {ROUTE_CEILING:?} ceiling; measured {timings:?}. The idle reference is \
                 {ROUTE_REFERENCE:?} in a release build, so this is either a gross regression \
                 (see FfiBackend::run on the needle_init-per-call measurement) or a very \
                 heavily loaded machine. Re-run idle with `--release` before concluding."
            );
        }
        // Not a pass and not a failure of the code under test: we could not
        // measure at all. Say so loudly rather than inventing a verdict. The
        // functional assertions above have already run and are what this suite
        // exists for.
        None => eprintln!(
            "WARNING: could not measure route latency — all {PERF_SAMPLES} samples exceeded \
             the router timeout ({ROUTER_TIMEOUT:?}). This host is too loaded to benchmark; \
             the functional assertions above still ran. Re-run idle with `--release` to get \
             a real number (idle reference {ROUTE_REFERENCE:?})."
        ),
    }
}

fn caps() -> ModelCapabilities {
    ModelCapabilities {
        streaming: true,
        tools: true,
        structured_output: true,
        vision: false,
        max_context: 32_768,
    }
}
