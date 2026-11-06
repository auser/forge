# Local Inference Runtime (rten) — Exploration

Date: 2026-10-30
Status: Exploration — recommendation for decision, not an approved design
Scope: Evaluating [`rten`](https://github.com/robertknight/rten) as forge's
local inference runtime, prompted by the pure-Rust OCR engine
[`rusto-rs`](https://github.com/byrizki/rusto-rs) and its
[author's post](https://medium.com/@byrizki/why-we-built-a-pure-rust-ocr-engine-zero-c-sub-100ms-latency-and-a-5mb-binary-e410269b4bd1)
("Why We Built a Pure-Rust OCR Engine: Zero C++, Sub-100ms Latency, and a 5MB
Binary").

## 1. What the post actually teaches

The post's argument is not "ONNX Runtime is bad". It is that **a native
inference dependency is a distribution tax paid at every install**, and that
the tax can be avoided without giving up speed. RustO replaced an ONNX
Runtime C++ dependency with `rten` (pure Rust) plus `.rten`-format weights,
and kept the OpenCV path as an *optional* backend rather than a requirement.

Five patterns transfer to forge directly.

| # | Pattern | Where it comes from | Forge analogue today |
|---|---------|--------------------|----------------------|
| 1 | **Pluggable session trait** — the engine is swappable behind one trait, so adding a backend never rewrites the pipeline | `src/engine.rs` in rusto-rs defines `pub trait InferenceSession: Send` with `run` / `run_all` / `run_with_inputs`, and an `EngineSession` enum holding `Rten(Box<RtenSession>)` or `Custom(Box<dyn InferenceSession>)` | `NeedleBackend` (`crates/forge-needle/src/backend.rs:77`), `ModelProvider` (`crates/forge-core/src/model.rs:159`), `Embedder` (`crates/forge-core/src/embed.rs:9`) |
| 2 | **Single-file, memory-mapped weights** — one model file per model, `mmap` is a first-class cargo feature, so the OS can share physical pages across every process that loads it. *Caveat from §7: the measured advantage of `.rten` over `.onnx` was nil for size, load time, and RSS.* | `rten` features list: `onnx_format`, `rten_format`, **`mmap`**, `contrib`, `all-ops`, `wasm_api` | `needle3.cact` weights are a single blob, SHA-256 pinned (`crates/forge-needle/src/weights.rs`); laya needs a *directory* of safetensors + JSON (`crates/forge-providers/src/laya_weights.rs:1-9`) |
| 3 | **Static init vs per-request split** — the model is loaded once and shared; per-call knobs are a separate options value | `rten::Model::load_file` once; `model.run(inputs, outputs, options)` per call with `options = None` | `NeedleEngine` loads lazily once and dispatches per-job (`crates/forge-needle/src/engine.rs`) |
| 4 | **No lock on the hot path** — the session is owned per worker; concurrent callers do not contend a mutex | rusto's `InferenceSession: Send` is moved into a worker thread, not shared behind an `Arc<Mutex<_>>` | `LayaRouter` holds `Arc<Mutex<Option<laya::Agent>>>` (`crates/forge-providers/src/router.rs:461`) — a real contention point |
| 5 | **Pure Rust end-to-end** — no CMake, no NDK, no libc++ probing, no per-platform static blobs | rusto-rs: `rten = "0.27.0"` with `features = ["onnx_format", "rten_format"]`, and `opencv` as an *optional* feature | `crates/needle-sys/build.rs` resolves a per-platform `libneedle.a`, probes a C++ runtime, and links it |

The fifth pattern is the one that matters most here, and §2 shows forge is
already paying that tax.

## 2. Where forge stands today

Three planes, three different local stories.

**Generation plane.** HTTP only. `ModelProvider` is implemented by
`OpenAiCompatibleModel`, `AnthropicModel`, `CodexModel`, and mocks
(`crates/forge-providers/src/model.rs`). A "local model" today means a
*separately running server* — Ollama, llama.cpp `llama-server`, LM Studio,
oMLX — reached over `model_base_url`. `automatic_model` ranks a reachable
local endpoint first (`crates/forge-providers/src/model.rs:330-340`).

**Decision plane.** Two implementations:

- **Needle** — an embedded brain behind a C++ FFI boundary
  (`crates/needle-sys/`). `NeedleBackend` exposes `decide`, `embed`, `extract`,
  `tool_call`.
- **Laya** — pure Rust, in-process, via the `laya` crate: candle +
  candle-transformers + tokenizers, a ModernBERT-large encoder with an RL
  decision head (`crates/forge-providers/src/router.rs:453-520`). It answers
  *typed decisions only* — "it never generates text, so there is nothing to
  parse and nothing to hallucinate" (the laya README). It cannot fill
  arguments, extract structured output, or call tools.

**Embedding plane.** The `Embedder` trait (`crates/forge-core/src/embed.rs:9`)
has exactly two implementations: `EngineEmbedder` (the local Needle engine,
`crates/forge-needle/src/engine.rs:217`) and `EmbeddingGemma2` (a hosted HTTP
provider, `crates/forge-providers/src/embedding.rs:108`). There is no
standalone local embedder, so on any build without the native engine, local
semantics require a network round-trip.

### 2.1 The native tax, measured

`PINNED_ENGINES` (`crates/needle-sys/build_support.rs:87-100`) pins exactly
**two** targets. The release matrix ships **five**
(`.github/workflows/release.yml:39-64`):

| Release target | Ships the brain | Why |
|---|---|---|
| `aarch64-apple-darwin` | yes | pinned |
| `x86_64-apple-darwin` | **no** | there is no `macos-x86_64` folder upstream — Intel macOS has no engine to fetch (`crates/needle-sys/build.rs:35`) |
| `x86_64-unknown-linux-gnu` | **no** | its `libneedle.a` leaves `std::__1::__hash_memory` undefined and no distributed libc++ defines it |
| `aarch64-unknown-linux-gnu` | yes | pinned |
| `x86_64-pc-windows-msvc` | **no** | the published engine is a COFF `libneedle.a`, not the `needle.lib` MSVC resolves, and has never been link-tested |

**Three of five shipped binaries have no decision brain.** They degrade to
`UnavailableBackend` (`crates/forge-needle/src/backend.rs:107`) and route with
static rules instead. That is in direct tension with `specs/project.md` §2
("Fast, portable, single-binary coding agent", "without requiring ... a
mandatory daemon") and §12 ("supported macOS and Linux builds").

There is a second, quieter cost: `laya`'s checkpoint is ~847 MB across a
directory of files, whereas an `.rten` model is one mmap'd file.

## 3. What rten actually offers (verified 2026-10-30)

- **rten 0.27.0**, released 2026-10-02. Licence **MIT OR Apache-2.0** —
  compatible with forge's MIT without a licence exception.
- Published crates: `rten`, `rten-tensor`, `rten-text` (tokenizers),
  **`rten-generate`**, `rten-base`, `rten-gemm`, `rten-vecmath`, `rten-cli`.
  (`rten-examples`, `rten-bench`, `rten-testing` are unpublished.)
- **CPU only.** No GPU backend. SIMD via AVX2, AVX-512, Arm Neon and WASM
  SIMD; multi-threaded by default.
- Weights: `f32` and **int8**; `f16` is partially supported (upconverted to
  `f32` at load).
- **Both model formats load**: `.onnx` directly, and `.rten` (FlatBuffers).
  `rten-cli` can convert between them.
- **`rten-generate`** provides the autoregressive stack: `Generator`,
  `GeneratorConfig`, `ModelInputsConfig`, a `LogitsFilter` trait (token
  filters, temperature, chains) for constrained decoding, and a built-in KV
  cache (`KvCacheData`, in both `[batch, seq, channels]` and
  `[batch, heads, seq, channels]` layouts). Its optional `text-decoder`
  feature adds token-ID-to-text decoding on top of `rten-text` tokenizers.
- In-tree examples covering exactly the capabilities forge wants:
  `llama` (Llama 3.2-3B-Instruct), `qwen2_chat`, `gpt2`, **`modernbert`**,
  **`jina_similarity`** (sentence embeddings), **`bert_qa`** (extractive QA),
  `kokoro`, `piper`, `trocr`, `silero`, `clip`, `detr`, `rmbg`,
  `segment_anything`, `depth_anything`.
- API shape (from `rten-examples/src/jina_similarity.rs`):
  `Model::load_file`, `model.node_id(name)`, `model.find_node(name)`,
  `model.run_n(inputs, outputs, options)`, `rten_tensor::NdTensor`,
  `rten_text::tokenizer::Tokenizer::encode`.

Two facts that change the calculus, both verified:

- **`modernbert` is a first-class example.** Laya *is* a ModernBERT encoder,
  so the decision plane's model architecture is known to export and run under
  rten.
- **`rten` has no GPU backend; `candle` (what Laya uses) does.** Porting
  Laya to rten would trade Metal/CUDA acceleration away. That trade is fine
  for a small embedder and wrong for a 847 MB ModernBERT-large.

## 4. Three real options

### Option A — an rten-backed local embedder

Fill the one genuine hole in the embedding plane: a standalone, pure-Rust,
on-device embedder that works on every target.

- **Seam:** implement `Embedder` (`crates/forge-core/src/embed.rs:9`). Nothing
  else in forge changes.
- **De-risked by construction:** `model_id` versions the vectors and the trait
  documents that "any stored index built with a different `model_id` or
  `dimensions` must be rebuilt, never mixed"
  (`crates/forge-core/src/embed.rs:6-7`). Swapping embedders invalidates
  stale indexes instead of corrupting them.
- **Model:** `bge-small-en-v1.5` (384-dim) or `all-MiniLM-L6-v2` (384-dim).
  Both publish ONNX exports *and* int8 variants, verified on Hugging Face.
- **Value:** the semantic project graph works offline on all five release
  targets; no code leaves the machine; one network hop removed from every
  context build.
- **Risk:** low. No new runtime semantics, no new decision model, the model
  is already exported upstream.

### Option B — rten-backed local generation (`forge-llm-embedded`)

The existing design already plans this as sub-project 5
(`docs/superpowers/specs/2026-09-23-needle-embedded-brain-design.md:90-93`):
"in-process generation via llama.cpp or mistral.rs FFI behind the existing
`ModelProvider` trait; removes the last external server from the local stack."

rten is a materially better fit than that plan, for one reason: **llama.cpp is
a second C++ stack.** Forge is trying to *remove* a C++ dependency; adopting
llama.cpp would add one back, with the same per-platform static-blob and
runtime-probing costs that make `libneedle` awkward today.

- **Seam:** implement `ModelProvider` (`crates/forge-core/src/model.rs:159`) —
  `complete` plus `stream_complete`.
- **Stack:** `rten` + `rten-generate` (`text-decoder` feature) + `rten-text`.
- **Constraint:** CPU only. A 3B-class model on CPU is single-digit tokens per
  second. Local generation is therefore a *fallback tier*, not a coding
  model. Its honest jobs: summarise, classify, extract, and keep forge useful
  with no credential and no daemon running.
- **Risk:** medium. Needs its own design — sampling defaults, stop
  conditions, tool-call grammar, context budgeting, and a clear statement of
  what a CPU-tier model is *not* good at.

### Option C — replace the native Needle brain with an rten backend

The highest-value option and the highest-risk one.

- **Why it is attractive:** it removes the C++ tax entirely, restores the
  decision brain on the three targets that ship without one, and potentially
  reaches WASM (rten ships a `wasm_api` feature, and upstream publishes
  `wasm` / `wasm-component` engine folders).
- **The blocker:** Needle 3's weights are `needle3.cact`, a proprietary
  format. rten runs ONNX and `.rten`. **We cannot port Needle 3 to rten
  unless Cactus publishes an ONNX export.** That is a question for upstream,
  not a task we can schedule.
- **The workaround:** reimplement the *capabilities* on models rten can run.
  `NeedleBackend::decide` maps onto a ModernBERT classifier (the `modernbert`
  example); `embed` onto a small encoder (the `jina_similarity` example);
  `extract` onto extractive QA (the `bert_qa` example); `tool_call` onto a
  decoder with constrained decoding via `LogitsFilter`.
- **Risk:** high. This is replacing a trained model, not a runtime. The
  behaviour of the brain would change, and the design's "grammar-guaranteed"
  tool-call contract (`crates/forge-needle/src/backend.rs:84-88`) would need
  to be re-established against a different model.

## 5. Recommendation — Option A, implemented

**Option A is built.** `crates/forge-inference` exists, `embedding.provider =
"rten"` works, and the end-to-end path (config → checksum → model load →
embed → normalised vectors) is covered by a test that runs a real encoder.

```text
crates/forge-inference/
  src/session.rs         # InferenceSession trait (the rusto pattern)
  src/rten_backend.rs    # rten implementation, on its own worker thread
  src/model.rs           # the pinned catalog: files, SHA-256s, pooling
  src/weights.rs         # SHA-256-pinned, on-demand model fetch
  src/cache.rs           # process-wide model cache
  src/embedder.rs        # the adapter onto forge-core's Embedder trait
  tests/e2e.rs           # gated on a real model (`rten-e2e` feature)
```

`weights.rs` follows the pattern already proven twice in this repo —
`crates/forge-needle/src/weights.rs` and
`crates/forge-providers/src/laya_weights.rs`: fetch to a `.part` path, verify
the SHA-256, rename only on success, land in `~/.cache/forge/models/`. The
`.rten` single-file format would simplify laya's directory layout into one
file — but §7 measured no runtime advantage for `.rten` over `.onnx`, so the
format should be chosen on *distribution* grounds (one file vs a directory),
not performance.

**One design decision the implementation forced.** `rten-text`'s `Tokenizer` is
neither `Send` nor `Sync` — it stores its model and normalizer as
`Box<dyn Model>` / `Box<dyn Normalizer>`, and those traits carry no thread
bounds. A tokenizer therefore cannot be shared, and cannot even be moved onto
another thread once it exists; it has to be *created* on the thread that will
use it. So the encoder owns a dedicated worker thread that holds both the
tokenizer and the model, and speaks to it over a channel. That is the same
shape `forge_needle::NeedleEngine` uses for the FFI brain, for the same
reason. `rten::Model` itself *is* `Send + Sync`, so the expensive part is
not what constrains this — the tokenizer is.

A note on ordering: `model_id` already versions vectors, but the *semantic
index* is a stored artefact. Changing the embedder must rebuild it, and that
rebuild is a user-visible event, not a silent one.

Then, in order:

1. **Option A** — `crates/forge-inference` + `RtenEmbedder`, behind
   `embedding.provider = "rten"`. **Done.**
2. **Option B** — `forge-llm-embedded` on rten-generate, superseding the
   llama.cpp/mistral.rs plan in the existing design. Needs its own spec.
3. **Option C** — open the ONNX-export conversation with Cactus, and
   separately prototype a ModernBERT-based `NeedleBackend` to learn what the
   capability set costs to reproduce.

## 6. Open questions

- Does Cactus publish Needle 3 as ONNX? Everything in Option C depends on
  the answer, and it is one email away.
- Is there a published ONNX export of a *code-aware* small embedding model
  good enough for a semantic code graph? Checked and the answer is **no, not
  today**: `jinaai/jina-code-embeddings-1.5b`,
  `nomic-ai/nomic-embed-code`, `Alibaba-NLP/gte-code-en` and
  `Salesforce/SFR-Embedding-Code-400M` all resolve as repositories, but none
  serves an ONNX export at the standard paths. `bge-small-en-v1.5` is general
  domain. Either export one of the code models ourselves (a one-time Python
  step, outside forge) or accept a general-domain embedder.
- What does an `.rten` file save over `.onnx` in practice? **Measured: for
  this model, nothing** — same size, same load time, same peak RSS, same
  latency (§7). The only remaining candidate is mmap page-sharing across
  processes, which is still unverified. rten loads either format, so this is
  no longer a blocker for shipping `.onnx` directly.
- Is `Model` `Sync`? `NeedleBackend` requires `Send + 'static` with `&mut
  self` methods (`crates/forge-needle/src/backend.rs:77`), so a `Send` model
  behind a worker satisfies it, but a `Sync` model behind a lock would
  remove the contention point in §1 pattern 4.

## 7. Appendix — measured spike

A scratch crate (`/tmp/rten-spike`, not part of forge) loads a published
`bge-small-en-v1.5` ONNX encoder under `rten` 0.27.0 on an Apple Silicon Mac
and measures it. It is a single-file model, `rten` with features
`onnx_format`, `rten_format`, `mmap`.

**The headline result: it works.** rten loaded a real 127 MB ONNX encoder,
tokenized with `rten-text`, ran it, and produced 384-dimensional vectors whose
cosine similarity behaves correctly — `cos(code, same code) = 1.0000` and
`cos(code, prose) = 0.3627`. The tensors are semantically meaningful, not
noise.

### Measurements

> **Correction (2026-10-31).** The first run of these measurements was taken
> while the machine had a load average of ~365 from unrelated builds, and the
> absolute latencies were inflated by 4–7×. The table below is a re-measurement
> at load ~196, still busy but far more representative. The earlier figures
> should not be quoted.

Paired runs, alternating `.onnx` and `.rten` back-to-back so both formats
saw the same machine conditions.

| Measurement | `.onnx` | `.rten` |
|---|---|---|
| File size | 133,093,490 B | 133,108,224 B |
| Model load | 54–137 ms | 44–80 ms |
| 1 text (steady state) | 101–190 ms | 49–75 ms |
| 32 texts (batched, total) | 1816–3352 ms | 1379–4577 ms |
| Peak RSS | 166.2 MB | 163.9 MB |
| Dimensions | 384 | 384 |
| `cos(code, same code)` | 1.0000 | 1.0000 |
| `cos(code, prose)` | 0.3627 | — (identical vectors) |

int8 (`model_quantized.onnx`) measured separately: 127 MB → **32 MB**, and
the code-vs-prose separation held (0.3627 → 0.3609, self-similarity 0.9951).
int8 load was *slower* (int8 dequantises at load).

What this actually shows:

- **rten runs real models in-process with no native dependency, no daemon, and
  no Python.** The whole integration was three crates and one `Model::load_file`
  call. This is the load-bearing claim of the exploration, and it is verified.
- **A single interactive query embeds in ~50–190 ms.** That is fast enough to
  sit on an interactive path, not just a background index build. The
  load-365 numbers in the first run of this spike suggested otherwise and were
  wrong.
- **Batching still pays, roughly 2×.** 32 texts took ~1.4–2.1 s together
  (~43–67 ms/text) versus ~100–190 ms for one text alone. Forge should never
  embed one file at a time; the semantic index build should batch.
- **The `.rten` format bought nothing measurable for this model.** Same file
  size (within 14 KB), same load time within noise, same peak RSS, same
  latency. This contradicts the natural assumption that `.rten` is faster to
  load, and it means forge should **not** convert to `.rten` on the strength
  of format alone. The remaining `.rten` argument is mmap page-sharing across
  processes, which a single-process RSS measurement cannot demonstrate.
- **int8 is a 4× size win at no quality cost for this use**, traded against
  load latency.

### The `.rten` artifact pipeline

If forge ships `.rten` models, note who converts them: **the official
converter is Python.** `rten-convert` is a PyPI package (`pip install
rten-convert`, requires Python ≥ 3.10), not a Rust binary, and
`rten-convert` is *not* published on crates.io. `rten-onnx` is Rust, but it is
an ONNX *parser*, not a `.rten` writer.

That is not a blocker, but it relocates the work. Python is a
**maintainer-side, one-time** step — convert the ONNX once, publish the
`.rten`, SHA-256 pin it, and users download a single file. That is exactly the
shape forge already uses for `needle3.cact` and for laya's safetensors, and
it keeps Python off the end-user install path. Versioning rule from the
converter's README: "A model produced by `rten-convert` version X can be
executed by `rten` version X or newer." (`rten-convert` on PyPI is 0.22.0
against `rten` 0.27.0, so it is compatible.)

The measured conversion itself took **1.6 s** for the 127 MB encoder and
produced a byte-identical-size `.rten` with the `RTEN` magic header.

### API notes for whoever implements this

The real API differs from the obvious guesses in three places, all of which
cost a compile cycle to discover:

- `NdTensor<f32, 3>` is a concrete tensor type, not an enum — match on
  `rten::Value` and `try_into()` instead of writing `NdTensor::FloatTensor(..)`.
- `NodeId` is a `NonZero<u32>` newtype with `as_u32()`/`as_usize()` and
  `Display`; there is **no** `as_str()`. Look nodes up by name with
  `model.node_id(name)` / `model.find_node(name)` instead of iterating
  `input_ids()`.
- Tensor dtype must match the model's graph. The upstream rten example uses
  `i32` for `input_ids` and `attention_mask`; `rten_text::tokenizer::TokenId`
  is `u32`, so the cast is `tid as i32`.

### Not yet measured

- Whether the `mmap` feature actually shares pages across processes, which is
  the property that would make running `forge serve` and `forge chat` side by
  side cheap, and the only remaining argument for `.rten` over `.onnx`.
  Peak RSS was the same for both formats in a single process.
- Whether a `bge-small` encoder is good enough for a *semantic code* graph.
  It was trained on general text, and no code-specific alternative ships an
  ONNX export yet (see §6).
- rten as an **LLM** runtime. `rten-generate` is published with a KV cache and
  a `LogitsFilter` trait, and upstream ships `llama` / `qwen2_chat` examples,
  but no generation was benchmarked here. This is the single largest unverified
  assumption behind Option B.

