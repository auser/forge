# Needle 3 Embedded Brain — Design

Date: 2026-09-23
Status: Approved design, pending implementation plan
Scope: Sub-project 1 of the Needle/Jev/editor-integration program (decomposition below)

## 1. Intent

Make forge incredibly easy and fast, with on-device functionality as the
default. Ease of use (UX/DX) is the highest-priority decision criterion.
Forge remains an AI harness itself and must integrate with editors
(VS Code, Cursor, Zed) and other harnesses via standard protocols.

Agreed direction:

- **Needle 3** (Cactus Compute; 8–29 MB on-device model for tool calling,
  structured extraction, and embeddings) is embedded **in-process** and
  becomes forge's default decision brain. No external processes, no pip
  installs in the default path.
- **Jev** (TypeSafe AI; cloud System One model returning typed decisions)
  is an **opt-in escalation tier** for low-confidence decisions, active
  only when a TypeSafe credential is configured.
- **Offline by default**: a fresh install works 100% on-device with zero
  accounts. `--local-only` hard-blocks all network.
- Editors and harnesses integrate via **ACP + MCP** only — no bespoke
  per-editor extensions.

### Decision plane vs generation plane

Needle 3 and Jev cannot generate text or code. They form the *decision*
ladder; LLM providers form the *generation* tier the decisions route to.

Decision plane: **Needle 3 (embedded, ~ms, free) → Jev (opt-in cloud,
70–500 ms) → static rules (deterministic, always succeeds)**.

Generation plane: **local model first** (any OpenAI-compatible endpoint:
oMLX, llama.cpp `llama-server`, Ollama, LM Studio — configurable via
`model_base_url`) → **hosted LLM providers only when** the routing
decision requires capabilities the local model lacks **and** a
credential exists. Credentials include both **API keys and existing
cloud subscriptions** — Claude Code OAuth tokens, Codex CLI auth, Kimi/
Moonshot, and similar CLI credential stores (detection already landing
in `forge-providers/src/credentials.rs`; `forge auth status` reports
what was found). Providers with detected credentials join the router's
candidate list; without credentials, cloud candidates do not exist. Any
decision that selects a cloud model must carry a human-readable
`reason` in the session event.

**Subscription and/or API key — both first-class.** Each cloud
provider is usable through either credential kind, whichever is
present: subscription alone, API key alone, or both (explicit
`key_env` → conventional env vars → CLI subscription stores, per the
documented precedence). `forge auth status` reports source and kind
per provider. Forge surfaces the terms caveat for subscription OAuth
tokens (providers intend them for their own CLIs) but does not
privilege one kind over the other.

**Positioning**: forge should be the premier way to make AI work
**cheapest, fastest, and local-first** — free embedded decisions, free
local generation, subscriptions you already pay for next, metered API
keys last — with automatic, transparent fallback up that ladder only
when a task demands it.

Needle can genuinely *call functions* (select + fill arguments,
grammar-guaranteed). Jev can only *select among options* (choice /
score / probability), not fill free-form arguments.

## 2. Program decomposition

Each sub-project gets its own spec → plan → implementation cycle:

1. **`forge-needle` embedded brain** — this spec.
2. **Jev escalation tier** — `JevRouter` (TypeSafe API) + credential
   gating; composes into the existing router stack.
3. **`forge acp`** — Agent Client Protocol adapter over stdio (Zed,
   JetBrains, neovim, other ACP clients get forge as an in-editor agent).
4. **`forge mcp`** — MCP server exposing graph search, skills, and runs
   as tools (VS Code, Cursor, Claude Code, other harnesses).
5. **`forge-llm-embedded`** — in-process generation via llama.cpp or
   mistral.rs FFI behind the existing `ModelProvider` trait; removes the
   last external server from the local stack. (Weights are GBs; needs
   its own design.)

6. **Interactive CLI (TUI) with slash commands** — a fully-featured
   interactive mode (`forge` with no subcommand, or `forge chat`)
   matching the UX users know from Claude Code, Kimi Code, and similar
   harnesses: a persistent conversational session with `/` commands
   (e.g. `/model`, `/config`, `/skills`, `/graph`, `/session`,
   `/approval`, `/help`, `/quit`) mapping onto the same `AgentService`
   the CLI and server already share. Skills discovered from the
   existing roots surface as `/name` commands (the `.claude/skills/`
   root means Claude Code skills just work). Needle's direct-dispatch
   fast path (§5) makes slash/plain-text intent routing instant and
   on-device. Modern-harness table stakes are in scope: forking a
   conversation into a new session, backgrounding a running task and
   reattaching to it, and listing/switching live runs — the append-only
   session store and `forge resume`/`cancel` are the substrate. Needs
   its own design (TUI framework, streaming render, keybindings,
   approval UX, fork/background semantics).

Parallel track (in progress on main): **cloud subscription support** —
credential detection for Claude Code OAuth, Codex, Kimi/Moonshot and
friends (`forge auth status`), extending the generation-plane candidate
list. Remaining gaps tracked there: OAuth-only Codex (ChatGPT Responses
backend), macOS Keychain lookup, and additional subscription providers
as they expose usable credentials.

**Documentation requirement**: every sub-project keeps `README.md`
accurate in the same change that lands behavior — the README describes
what forge does today, never the roadmap; design direction lives in
this spec and `specs/roadmap.md`.

## 3. Architecture (sub-project 1)

Two new crates, `-sys`/safe-wrapper convention:

```
crates/needle-sys      # bindgen FFI over needle.h; links libneedle per platform
crates/forge-needle    # safe async wrapper + trait impls
```

- **`needle-sys`**: raw unsafe bindings only. `build.rs` resolves the
  static library in order: `NEEDLE_LIB_DIR` env var → vendored
  `vendor/needle/<target>/` → download at build time with checksum
  verification. Targets: macOS ARM64, Linux x86-64/ARM64, Windows x86-64.
- **`forge-needle`**: owns `NeedleEngine` and implements forge-core
  traits. Public capabilities:
  1. `NeedleRouter: DecisionRouter` — model routing and guardrail
     decisions ("is this tool call destructive / off-task?").
  2. `NeedleEmbedder: Embedder` — local embeddings (new trait, §4).
  3. `extract()` — grammar-guaranteed structured extraction (also used
     to repair malformed tool-call args from local models).
  4. `fill_tool_call()` — tool selection + argument filling for the
     direct-dispatch fast path (§5).

**Weights**: fetched once by `forge init` to
`~/.cache/forge/models/needle3-<variant>.bin`, SHA-256 verified.
Variants follow Needle's intelligence ladder: `small` (~8 MB) /
`medium` / `full` (default; ~35 MB) — `full` is the default because it's
currently the only variant Cactus-Compute hosts as a downloadable
artifact (see §8); revert to a smaller rung once one is hosted. An
operator can override the expected checksum via `needle.weights_sha256`
to run their own weights without recompiling. Optional cargo feature
`embed-weights` bakes weights into the binary via `include_bytes!` for
single-file distribution.

**Default router change**: `router = "needle"` replaces `"laya"` as the
built-in default. The stack composed by `router_from_config` becomes:

```
ThresholdRouter(NeedleRouter, router_confidence_threshold)
  → [JevRouter, iff TypeSafe credential configured — sub-project 2]
  → StaticRouter
```

Existing combinators (`ThresholdRouter`, `FallbackRouter`) are reused
unchanged. When Needle declines to guess (a designed behavior), the
decision falls through deterministically with `fallback_used: true`.
Laya and HTTP routers remain selectable via `router = ...`; nothing is
removed. `--local-only` prunes any network router from the stack at
construction time, so no code path can attempt network.

## 4. Components & interfaces

New trait in `forge-core` (`src/embed.rs`):

```rust
#[async_trait]
pub trait Embedder: Send + Sync {
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, ForgeError>;
    fn dimensions(&self) -> usize;
    fn model_id(&self) -> &str; // index invalidates when this changes
}
```

`NeedleRouter` implements the existing `DecisionRouter` unchanged;
guardrail checks ride the same trait with a distinct task prefix
(matching how `LayaRouter` phrases criteria today).

**`NeedleEngine` internals**: one dedicated OS thread owns the FFI
handle (contexts not assumed thread-safe), fed by an `mpsc` channel;
async callers await oneshot replies. Embedding requests batch up to 32
texts per FFI call. Lazy initialization — `forge --help` never pays
model-load cost.

**Configuration** (defaults shown):

```toml
router = "needle"                 # new default (was "laya")

[needle]
variant = "full"                  # small | medium | full — full is the only hosted artifact today
weights_path = ""                 # override; empty → ~/.cache/forge/models/
autofetch = true                  # forge init downloads + verifies weights
weights_sha256 = ""               # operator override for the expected checksum; empty → compiled-in pin
```

Reused as-is: `router_confidence_threshold` (0.7), `router_fallback`
(`"static"`), `router_timeout_ms`. `forge config explain` covers the
new keys.

**CLI surface** (additions only, no breaking changes):

- `forge init` — fetches/verifies weights when `autofetch = true`
  (skipped under `--local-only`, with a clear message if weights absent).
- `forge doctor` — "needle" probe: weights present + checksum, load
  time, 1-token smoke inference, tokens/sec.
- `forge model test` — includes needle status.
- `forge graph grep --semantic "<query>"` — embedding-backed search (§5).
- `forge router serve` (Laya) unchanged; relevant only when
  `router = "laya"`.

**Events**: routing decisions already emit to the session store; Needle
adds `router_name: "needle"` and confidence — no schema change.

## 5. Data flow

**Run lifecycle** (`forge run "..."`):

1. CLI → `AgentService` builds `RoutingRequest` (task, required
   capabilities, candidate models from config + credential detection).
2. Router stack decides; decision + confidence + `fallback_used`
   emitted as a session event.
3. **Direct-dispatch fast path**: for well-defined requests ("run the
   tests", a skill invocation), Needle tool-calls against forge's own
   tool/skill registry. High confidence AND non-destructive verdict AND
   grammar-valid args → execute immediately through
   `ExecutionProvider`, without waking any generation LLM. Sub-100 ms,
   fully on-device. Needle declines or confidence low → full agent loop.
4. Agent loop on the selected `ModelProvider`. Per tool call: Needle
   guardrail check gates auto-approval under the existing `--approval`
   policy; `extract()` repairs malformed tool-call args from local
   models instead of burning a retry round-trip.
5. Skill activation: skill descriptions pre-embedded; task embedding
   ranks candidates, top-k metadata joins the routing decision —
   progressive disclosure stays cheap and local.

**Semantic graph index**:

- `forge graph build` (and incremental updates) embeds each
  symbol/doc-chunk via `NeedleEmbedder`; vectors stored in
  `.forge/graph/embeddings.bin` — flat little-endian f32 plus a
  ULID-keyed sidecar index. No vector DB; brute-force dot product over
  a few thousand symbols is sub-millisecond.
- Incremental by content hash; index header records `model_id` and
  `dimensions`, full rebuild when either changes.
- `forge graph grep --semantic` and `POST /v1/project/context` blend
  lexical + semantic scores. This is the surface editors/harnesses hit
  later via MCP for "find relevant code".

## 6. Error handling & offline guarantees

- **Weights missing/corrupt**: checksum failure → one automatic
  refetch → else an actionable error naming the path and the
  `forge init` fix. Under `--local-only` with no weights: clear
  message, static routing continues. Forge never refuses to run
  because Needle is unavailable.
- **FFI safety**: all needle calls on the dedicated engine thread; a
  panic there poisons the engine → router reports
  `ForgeError::Router`, stack falls through to static, session event
  records the degradation, `forge doctor` surfaces it.
- **Latency budget**: in-process timeout on needle decisions
  (`router_timeout_ms`, default 500 ms); breach → fallback, never a
  hung run.
- **Escalation failures** (sub-project 2): network error/timeout →
  skip tier, fall to static.
- **Determinism**: same input + same weights → same decision
  (temperature 0); decisions replayable from session events.

## 7. Testing

- **Unit**: a `NeedleBackend` trait inside `forge-needle` separates FFI
  from logic; router/embedder/dispatch logic tested against a scripted
  backend, no weights needed.
- **Integration** (cargo feature `needle-e2e`): real `small` weights
  (~8 MB) cached in CI; asserts load, route, embed, extract,
  decline-to-guess, and fast-path dispatch end-to-end.
- **BDD** (`tests/features/needle_routing.feature`): offline default
  run with no network; low-confidence fallback marks `fallback_used`;
  doctor probe output; `--local-only`; semantic grep results.
- **Index tests**: incremental re-embed on content change; full
  rebuild on `model_id` change; corrupt index → rebuild, not crash.
- **Perf smoke**: route p50 < 50 ms and embed-batch throughput on dev
  hardware, reported by `forge doctor`; asserted loosely in e2e
  (generous bounds to avoid CI flakes).

## 8. Risks & open questions

- **Licensing/redistribution of weights: resolved, Apache-2.0** (verified
  2026-09-23, Task 6). `Cactus-Compute/needle3` on Hugging Face carries
  `cardData.license: apache-2.0` and ships a `LICENSE` file with the
  standard Apache License 2.0 text — permissive, redistribution and
  autofetch-from-origin are both fine. `libneedle` itself (the C
  API/engine binaries under each platform folder in the same repo) is
  covered by the same repo license; still worth a second look before
  vendoring binaries into forge's own release artifacts, since forge
  currently only fetches weights at runtime (autofetch), not the engine.
  The `NEEDLE_LIB_DIR`/vendored/download-at-build resolution order still
  keeps forge shippable under any outcome there.

  **Amendment (Task 8, engine acquired and verified 2026-09-23).** The
  engine is real, obtained, and now the basis of `FfiBackend`:

  - **Source.** `https://huggingface.co/Cactus-Compute/needle3` ships a
    folder per platform, each holding `libneedle.a` + `needle.h` (native
    runners `needle`/`needle.exe` too, which forge does not use):
    `macos-arm64`, `linux-{x86_64,arm64,armv7,riscv64,mipsel}`,
    `windows-{x86_64,arm64}`, `android-{arm64,armv7,riscv64}`,
    `ios-arm64`, `ios-sim-arm64`, `tvos-arm64`, `watchos-arm64`, `wasm`,
    `wasm-component`. Discovered via the HF API `siblings` listing after
    `cactus-compute/needle` (the Apache-2.0 Python SDK repo, PyPI package
    `cactus-needle` 3.0.1) documented a C API and a `needle build
    --platform <folder>` fetch path. Engine version per the SDK's
    `ENGINE_VERSIONS[3]`: `3.0.2`.
  - **License: Apache-2.0, same repo, redistributable.** Confirmed for the
    *library* as well as the weights — the HF repo-root `LICENSE` covers
    every platform folder, and the SDK repo is independently Apache-2.0
    (`pyproject.toml` `license = { text = "Apache-2.0" }`, GitHub
    `license.spdx_id: Apache-2.0`). Vendoring is therefore *permitted*; the
    repo still does not commit `libneedle.a` (1.1 MB × 17 targets, and
    nothing in `just verify` needs it) but `needle.h` — the API contract —
    is committed so `cargo check` works on machines that never link it.
  - **Pinned artifacts** (macos-arm64, verified by `shasum -a 256`):
    `libneedle.a` = `60cc14f1a2eda8da72b75f8f228fb72cadc2850b38702370f43e9660b74e951a`
    (1 158 184 bytes); `needle.h` =
    `3aa713942528d944598458cecb4a262f2cc49349bec63355f91df0b159964e55`
    (1 187 bytes, committed at `crates/needle-sys/needle.h`).
  - **It is C++ behind an `extern "C"` facade.** `nm` shows libc++ symbols
    plus `__cxa_*`/`__gxx_personality_v0`, so `build.rs` links the C++
    runtime (`c++` on Apple/FreeBSD, `stdc++` on Linux, `c++_static` +
    `c++abi` on Android, nothing extra on MSVC).
  - **The offline guarantee holds: `libneedle.a` cannot make network
    calls.** Needle's README warns that "telemetry is turned on in the
    binary" by default, which would contradict forge's "no network calls
    once weights are on disk" claim for the `needle` router. It does not
    apply to us: that telemetry is in the Python SDK
    (`needle/_telemetry.py`) and the standalone `needle` CLI runner, not in
    the static library forge links. `nm -u libneedle.a` shows **zero**
    network-capable undefined symbols — no `socket`, `connect`,
    `getaddrinfo`, `gethostby*`, DNS, TLS/SSL, HTTP or curl. The complete
    external surface is libc maths/memory/stdio (`fopen`/`fread`/`fwrite`/
    `remove` — expected, `needle_init` takes an optional tool-index path)
    plus `mmap`/`munmap`, `pthread_create`, `getrusage` and
    `sysctlbyname`. No `NEEDLE_TELEMETRY=0` workaround is required.
- **Artifact layout differs from the original plan: only one variant is
  a downloadable file.** The design assumed three separately-hosted
  weight variants (`small`/`medium`/`full`). In reality Cactus-Compute
  publishes a single 20-layer file per release (`needle3.cact`, ~35 MB,
  mapped to `needle.variant = "full"`); `small` and `medium` are produced
  locally by slicing that file with the `needle build --layers N` CLI
  (part of the `cactus-needle` Python package), not hosted separately.
  `forge-needle`'s `weights::spec_for` therefore only pins `"full"` (SHA-256
  verified independently with both `shasum -a 256` and Python's
  `hashlib.sha256`); asking for `"small"` or `"medium"` returns a typed
  error naming `"full"` as the available variant, and `ensure_weights`
  degrades that to `WeightsStatus::Missing` (static routing continues) —
  it never blocks `forge init` or the router.

  **Amendment (controller ruling, Task 6 amendment round)**:
  `NeedleConfig::default().variant` was originally left at `"medium"`
  (following the design's stated default), which meant a fresh `forge
  init` fetched nothing out of the box — silently inert. The default is
  now `"full"`, the one variant that is actually hosted, so `forge init`
  with no config overrides fetches real, working weights immediately.
  Revert the default to a smaller rung once Cactus hosts one (or forge
  builds its own via the CLI) — `full` is a stopgap driven by artifact
  availability, not a statement that `full` is the right default ladder
  rung long-term. Also added `[needle].weights_sha256`: an operator
  override for the expected checksum (empty → compiled-in pin), so
  someone running their own weights build (paired with `weights_path`
  and/or a custom base URL) doesn't need to recompile forge to change
  the trust anchor.
- **C API stability**: Needle 3 shipped 2026-09-18; pin an exact
  library + weights version, verify by checksum.

  **Amendment (Task 8): the C API is six functions, and smaller than the
  plan assumed.** `needle.h` declares only `needle_load`, `needle_init`,
  `needle_complete`, `needle_embed`, `needle_reset`, `needle_last_error`.
  There is no `decide`, no `extract` and no separate tool-call entry point:
  `needle_complete` is the single generation primitive, and the *tool
  surface installed by `needle_init` decides what it means*. So
  `FfiBackend` maps three trait methods onto it — `decide` installs one
  no-argument tool per option (the selected tool is the choice), `extract`
  installs the caller's JSON Schema as one record tool, `tool_call` passes
  the caller's tools JSON through — while `embed` uses `needle_embed`
  directly. The `NeedleBackend` trait did not change.

  Behaviours measured against the real library that the header does *not*
  state, each of which the wrapper now defends against:

  - **`needle_init` does not validate its tools JSON.** `"{not json"`
    returns success (10), as does `"[]"` (7). Malformed tool surfaces are
    therefore silent; `FfiBackend` parses every JSON string with
    `serde_json` before handing it over, and builds JSON with `serde_json`
    rather than string formatting.
  - **`needle_complete` truncates silently.** With a 64-byte buffer it
    wrote 63 bytes + NUL and still returned success. It does respect
    `out_capacity` (verified by placing the buffer against an
    `mprotect(PROT_NONE)` guard page — no fault), so this is a correctness
    not a safety problem: the wrapper uses a 256 KiB buffer and reports a
    full buffer as a typed truncation error.
  - **The return value of `needle_complete` is a token count, not a byte
    count.** The NUL terminator is the only length signal.
  - **`needle_load` copies the archive.** Verified by revoking the source
    pages with `mprotect(PROT_NONE)` after loading and continuing to infer
    successfully, so the weights `Vec` is dropped immediately rather than
    leaked for the process lifetime.
  - **Names round-trip verbatim.** The engine snake-cases tool names
    internally (its `reasoning` shows `qwen3_coder`) but echoes the
    original in `function_calls[].name` — `qwen3-coder` and
    `openai/gpt-5.1-mini` both come back exactly as given. No sanitisation
    or name-mapping table is needed; options are matched by string
    equality.
  - **`needle_embed`** reports 3072 dimensions for `needle3.cact`, is
    L2-normalised, needs `needle_load` but not `needle_init`, is
    independent of conversation state, and rejects an undersized output
    buffer with `-1` rather than overflowing.
  - **`needle_last_error`** points at `""` (not null) when nothing failed,
    and is invalidated by the next call, so it is copied out immediately.
  - **`needle_init` per call is a performance *win*, not a cost.** It
    tokenizes and caches the static prefix. Caching it and skipping it for
    an unchanged tool surface — the obvious optimisation — made a warm
    route round-trip ~30x *worse* (0.5 s → 16.5 s). `FfiBackend::run`
    therefore resets and re-inits on every call, with a comment saying so.

- **Process-global, non-thread-safe, cannot unload.** The header's first
  sentence, and the strongest constraint on the design. `NeedleEngine`
  already serialises everything onto one dedicated thread, which satisfies
  it for a single engine; `FfiBackend::load` additionally takes a
  process-wide claim so a *second* engine in the same process fails with a
  typed error instead of racing through shared C state, and records which
  archive was bound so a request for different weights fails loudly rather
  than being silently answered by the first ones. It also means the
  real-weights e2e suite has to be a single sequential test function —
  parallel test threads would otherwise collide.

- **Latency is real but noisy.** Warm route round-trip on an idle
  macos-arm64 machine: ~47 ms in a release build, ~100 ms in a debug
  build; first inference after load ~5.7 s (paging in the archive).
  Identical deterministic inferences measured anywhere from 93 ms to 20 s
  on a machine busy compiling Rust, so the e2e perf assertion takes the
  minimum of five samples and the README points at release builds for any
  real measurement.
- **Quality bar**: Needle's routing/guardrail accuracy on forge's
  decision phrasing must be validated during implementation; the
  confidence threshold and static fallback bound the blast radius of
  wrong-but-confident decisions.

- **Amendment (Task 11, plan closeout, 2026-09-23).** All 11 tasks are
  merged; this sub-project (embedded on-device decision routing) is
  complete, with BDD coverage in `tests/features/needle_routing.feature`
  (default no-weights fallback, hash-backend on-device routing, the
  doctor probe, `--local-only` init, and semantic grep) added alongside
  the final README/spec sweep.

  Final verification surfaced one real safety gap worth recording here
  rather than just in a commit message: `FileOp::risk` documented "any
  path escaping the project root is Destructive", but its `Read` arm
  early-returned `Safe` *before* the escape check ran, so reading
  `../../secret` or `/etc/passwd` classified `Safe` — the one risk level
  every `ApprovalPolicy` (including `Deny`) lets through unconditionally,
  and the one the needle fast path's gate 5 (`minimum_dispatch_risk`)
  uses to decide it may dispatch without the call ever reaching
  `check_approval`. Fixed in both places: `FileOp::risk` now runs the
  escape check for `Read` too (an escaping read is now `Destructive`,
  gated exactly like an escaping write), and `minimum_dispatch_risk`'s
  sentinel root — previously an empty path, which made
  `path_escapes_root` structurally unable to observe *any* escape,
  because every path trivially "starts with" an empty path — is now a
  single-component absolute path, which correctly rejects both absolute
  and relative-walk-up reads without the tool layer ever knowing the
  real project root. Unit tests cover both layers (`forge-core`'s
  `execution::tests` and `forge-runtime`'s `tools::tests`).

  Known limitations that shipped as part of this plan (see the README's
  "Known limitations" section for the current, authoritative list): the
  needle direct-dispatch fast path is read-only by design, and never
  attempts writes/edits/deletes/commands; `forge init` builds the
  project graph's structure but never embeds it (no model calls from
  `init`, ever), so `forge graph build` is the documented follow-up once
  weights are available; and the e2e latency reference (~47 ms idle,
  release build) is asserted only against a loose 2 s ceiling, because
  wall-clock latency here tracks machine load far more than it tracks
  forge (see "Latency is real but noisy" above). Jev-tier escalation,
  ACP, MCP, a TUI, and embedded generation remain explicitly out of
  scope for this sub-project (see the plan's self-review) and are
  deferred to later spec sub-projects.

  **Amendment (final-review fix wave, 2026-09-23).** Four gaps between
  this spec's stated behavior and what actually shipped, recorded here
  per controller ruling rather than fixed now — each is scoped to a
  concrete follow-up rather than left ambiguous:

  - **`POST /v1/project/context` never got the semantic blend §5
    promises.** §5 says "`forge graph grep --semantic` and `POST
    /v1/project/context` blend lexical + semantic scores"; only the CLI
    side does. `forge-server/src/handlers.rs::project_context` calls
    `LocalGraph::context` directly and echoes its raw lexical score
    (`ContextHit::score`, a `u32` rank-derived count) verbatim as JSON.
    `semantic_blend` — needle-engine lookup, embedding-index load, `final
    = 0.5 * lexical_rank_score + 0.5 * cosine` — lives only in
    `forge-cli/src/commands/graph_cmd.rs`, coupled to the CLI's `Context`
    (config resolution) type. Controller ruling: defer moving the blend
    into a shared location rather than duplicate it ad hoc into the
    server handler under review pressure. Follow-up: lift `semantic_blend`
    (and the `ScoredHit` type it returns) into `forge-runtime` — or
    another crate both `forge-cli` and `forge-server` already depend on —
    behind the existing `Embedder`/graph traits, then have both the CLI
    command and the handler call the one implementation. Until that
    lands: `forge graph context`/`graph grep --semantic` return blended
    0-1 floats when a needle engine and matching index both exist (else
    the unchanged lexical ranking); `POST /v1/project/context` always
    returns the raw lexical count as an integer, needle engine or not.
  - **`embeddings.bin`'s whole-file `serde_json` format has a known scale
    ceiling for real (non-hash) weights.** The hash backend's 64-dim
    vectors keep the index small; the real ffi backend's `needle_embed`
    reports 3072 dimensions (§8 above), and each vector serializes to
    roughly 35-45 KB of JSON — a project with ~1,000 embedded symbols
    would produce a ~45 MB `embeddings.bin` that `EmbeddingIndex::load`
    (`forge-graph/src/embed_index.rs`) fully parses on every semantic
    query. Controller ruling: defer the format bump; it is safe to defer
    because the `FRGEMB01` magic prefix already makes a future format
    change non-silent — a version bump there simply fails to match and
    triggers a clean full rebuild rather than misreading old bytes as the
    new layout. Planned follow-up: a raw little-endian-`f32` vector body
    (no per-entry JSON) behind a new magic revision, keyed by a compact
    offset table instead of a `BTreeMap<String, Entry>` that has to
    deserialize every vector to find one.
  - **`forge model test` has no needle status.** §4 lists it as an
    addition: "`forge model test` — includes needle status." The
    implemented command (`forge-cli/src/commands/model_cmd.rs::test`)
    only pings the configured `ModelProvider` (the generation plane) and
    reports latency/sample text; it never touches `forge-needle` or
    reports engine/weights health. `forge doctor`'s needle probe remains
    the only surface that reports engine state today. Follow-up: fold a
    needle load/route smoke check into `forge model test`, or update this
    spec to name `forge doctor` as the intended surface instead.
  - **Skill selection is lexical, not embedding-ranked.** §5 step 5
    describes pre-embedded skill descriptions with task-embedding-ranked
    candidates. `SkillRegistry::match_task`
    (`forge-skills/src/registry.rs`) is word/substring matching against
    lowercased name + description — no embedder is involved, and there is
    no top-k ranking. Rolls to the same follow-up as the two items above:
    once `forge-runtime`/`forge-skills` have a shared path to a needle
    embedder (the blend refactor above is the natural place to introduce
    it), skill matching can move onto it instead of duplicating an ad hoc
    lookup per call site.
