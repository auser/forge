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
   gating; composes into the existing router stack. **Implemented** — see
   the §8 amendment for the verified wire contract, endpoint, and
   OpenJev-compatibility notes.
3. **`forge acp`** — Agent Client Protocol adapter over stdio (Zed,
   JetBrains, neovim, other ACP clients get forge as an in-editor agent).
   **Implemented** — stdio adapter in `crates/forge-acp` over the shared
   `AgentService`, speaking ACP protocol version 1; see the §8 amendment
   for the verified schema version, the SDK decision (we carry the v1
   wire subset rather than depend on the official crate) and the two
   recorded follow-ups.
4. **`forge mcp`** — MCP server exposing graph search, skills, and runs
   as tools (VS Code, Cursor, Claude Code, other harnesses).
   **Implemented** — stdio adapter in `crates/forge-mcp` over the shared
   `AgentService`; see the §8 amendment for the verified protocol reality
   (the current revision replaced the `initialize` handshake) and the SDK
   decision that followed from it.
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

   **Phase A (session substrate): implemented** (sub-project 6a, branch
   `tui-substrate`). The runtime half of the table stakes is done, with
   no UI:

   - **Full conversation replay.** Event schema v3 adds the verbatim
     replay kinds `assistant_message`, `tool_result` and
     `session_forked` (additively — v1/v2 logs stay readable), and
     `forge-runtime::replay` rebuilds the model's `messages` from a
     session's log across every prior run, fitted to a budget derived
     from the model's `max_context`. The v0.3 "resume seeds a truncated
     summary" limitation is gone. Every entry point that names an
     existing session continues it — `forge resume`, and also
     `POST /v1/runs`, `forge_run` and each ACP turn after the first, all
     of which reuse one session id and previously started from an empty
     history while claiming otherwise. Replay also repairs a run that
     died between announcing a tool call and recording its result, which
     would otherwise replay as a dangling call every chat API rejects.
   - **Fork.** `AgentService::fork_session` + `forge session fork <id>
     [--at <position|run-id>]`: a prefix copy into a new session with a
     `session_forked` provenance marker, snapped to a run boundary, the
     source never touched.
   - **Background/attach primitives.** `attach(run_id)` (backlog + live
     stream, gap-free and duplicate-free by `seq`) and `list_runs()`
     (live runs plus a bounded tail of finished ones). Cross-process
     detach/reattach remains out of scope and is recorded as a known
     limitation.
   - **Ledgered leak closed.** The never-pruned
     `inputs`/`broadcasters`/`cancel_tokens` maps are pruned on terminal
     state; `send_input` to a finished run is a typed error instead of
     resurrecting its channel.
   - **Typed run-outcome discriminant.** `forge_core::RunState` replaces
     ACP's `message.contains("cancelled")` turn-end classification and
     MCP's `&'static str` status hops; `ForgeError::Cancelled` makes
     cancellation readable from the type. Both adapters' existing test
     suites pass unmodified.

   Phase B (the interactive UI itself) still needs its own design.

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

  **Amendment (2026-09-24, `ffi-default`): resolution step 3 implemented; the
  feature stays opt-in.** §3's third resolution step — download at build time
  with checksum verification — now exists in
  `crates/needle-sys/build_support.rs` (`PINNED_ENGINES` +
  `ensure_cached_engine`), gated on a `needle-sys/fetch` feature that only
  `forge-needle/ffi` turns on, so a default build still touches no network.
  Cache is content-addressed under `$CARGO_HOME/needle-engine/<sha256>/`;
  `NEEDLE_NO_DOWNLOAD=1` opts out for offline/packaging builds;
  `NEEDLE_REQUIRE_ENGINE=1` makes an unresolvable engine fatal (release/CI use
  it so a brain-less binary cannot ship brain-labelled); an unresolvable
  engine is otherwise a `cargo:warning`, not a failure, so `just lint-ffi`'s
  link-free coverage of `ffi_backend.rs` keeps working on machines with no
  engine.

  **Engine checksums, all downloaded and hashed locally 2026-09-24** (each
  also matches the `x-linked-etag` Hugging Face serves, and macos-arm64
  matches the value recorded above from the earlier session):

  | folder | sha256 | bytes | wired up |
  | --- | --- | --- | --- |
  | `macos-arm64` | `60cc14f1a2eda8da72b75f8f228fb72cadc2850b38702370f43e9660b74e951a` | 1 158 184 | yes (`aarch64-apple-darwin`) |
  | `linux-x86_64` | `2581e7d46acd4f66c5839bcfb06b0af11c157c8775636875beb0af5ca35ded54` | 1 675 104 | yes (`x86_64-unknown-linux-gnu`) |
  | `linux-arm64` | `b36c214437b5230bae89291f684de571dceb0922834a09ceeb09a8e21464a481` | 1 539 978 | yes (`aarch64-unknown-linux-gnu`) |
  | `windows-x86_64` | `6fb0b9bccfa9f54d46e05a279273c15021570a53a8b3945613d80d299ca1f634` | 1 808 664 | no |
  | `windows-arm64` | `3a945065225cb383cab9b75333ebe0195d25c7e7c815f032d47857b354056d75` | 1 650 954 | no |
  | `linux-armv7` | `b1c3cf3ac526cb01314529da2094b8e5b38f41acd5b4a956fc05f22fb4b99346` | 1 334 534 | no (etag only) |
  | `linux-riscv64` | `11e0eea3d8dff6826171a702f6e741c3cbedde4e42a1ca1959d3712092adbc53` | 1 548 596 | no (etag only) |

  **Why `needle-ffi` is still not a default feature.** Three findings, each
  independently sufficient:

  1. **Intel macOS has no engine.** The HF `siblings` listing has no
     `macos-x86_64` folder (only a Python wheel). `x86_64-apple-darwin` is a
     release target, so a default-on feature would turn "builds, routes
     statically" into "does not link" on every Intel Mac.
  2. **Windows is unverified.** Both Windows folders publish `libneedle.a` —
     an `ar` archive of a COFF `needle.cpp.obj` — not the `needle.lib` an MSVC
     `-lneedle` resolves. A rename is probably enough, but neither the rename
     nor the C++ runtime pairing has been link-tested, and an unverified
     default is not a default.
  3. **Offline builds would break.** With `ffi` on and nothing cached, the
     link fails. Today those builds succeed and route statically. Converting
     graceful degradation into a build failure is a worse default than the
     bug it would fix.

  The user-visible fix for "the brain doesn't work out of the box" therefore
  runs through the *release* artifacts and through coherent messaging, not
  through the default feature set. Revisit if Cactus publishes an Intel macOS
  engine, or once a Windows link is verified on a Windows runner.

  **Amendment (2026-09-24): the distribution path, which was the real cause.**
  `.github/workflows/release.yml` built `-p forge-cli` with no features for
  every target, so *every* prebuilt binary was brain-less — the embedded brain
  was effectively unreachable for anyone who installed forge the way the
  README recommends. Now:

  - The three verified targets build with `--features needle-ffi` and
    `NEEDLE_REQUIRE_ENGINE=1`, so a job that cannot resolve a checksummed
    engine fails instead of publishing a brain-less asset under a
    brain-enabled label. A post-build step runs the artifact's own `forge
    doctor` and greps for `needle engine: backend built in`, so the claim is
    checked against the binary rather than against the build command.
  - Intel macOS and Windows keep the engine-less build (reasons above) and
    report it honestly at runtime.
  - `install.sh` needed no change for the download path — it fetches whatever
    the release published. Its `cargo install` *fallback* did: it now adds
    `--features needle-ffi` on the three verified targets and retries without
    it if the engine cannot be fetched or linked, so a source install matches
    the asset install without letting an upstream outage cost the user their
    install.
  - CI gained an advisory `verify-ffi` job that links and *runs* the ffi
    backend on x86_64 Linux. `just lint-ffi`'s link-free guarantee is
    unchanged and still in the required `verify` job (now with
    `NEEDLE_NO_DOWNLOAD=1`, so it stays network-free as well as link-free);
    the new job is what would catch a pinned checksum going stale or the
    engine ceasing to link — neither of which a type-check can see.
  - Brain-enabled Linux assets now link `libstdc++.so.6` (libneedle is C++).
    Noted in the workflow; minimal containers may need `libstdc++6`.
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

- **Amendment (sub-project 2, Jev escalation tier, implemented 2026-09-24).**
  `JevRouter` (`crates/forge-providers/src/jev.rs`) plus credential-gated
  escalation wiring in `router_from_config`
  (`crates/forge-providers/src/router.rs`) landed as a bounded follow-up to
  sub-project 1.

  **Verified wire contract** (network-checked before writing any code, per
  three independent, mutually-corroborating sources fetched 2026-09-24):
  TypeSafe's own API reference (`docs.typesafe.ai/api.md`), LiteLLM's
  TypeSafe pass-through docs (`docs.litellm.ai/docs/pass_through/typesafe`),
  and both OpenJev READMEs (`github.com/razorback16/openjev`,
  `github.com/GitHub30/OpenJev` — both explicitly documented as
  wire-compatible with TypeSafe's official `typesafe-sdk`). All four agree:
  `POST /v1/systemone`, `Authorization: Bearer <key>`,
  `{state: "<free text>", model: "<inference model id>", questions: {<id>:
  {type: "choice"|"noul"|"score", instructions, criteria}}}` →
  `{model, answers: {<id>: {type, choice, probabilities, confidence}},
  usage}`. The hosted endpoint is `https://api.typesafe.ai/v1/systemone`
  (the brief's suggested `console.typesafe.ai` was not it — the actual host
  is `api.typesafe.ai`). This **genuinely differs** from `LayaRouter`'s
  assumed shape in this codebase (`state` as a structured object with no
  top-level `model` field, no `usage`), so `JevRouter` stays self-contained
  rather than sharing request/response types with `LayaRouter`. Model alias
  sent: `jev-latest`, documented by OpenJev as accepted by both TypeSafe and
  OpenJev servers ("so TypeSafe SDK defaults work"), so one alias
  round-trips against either backend.

  **OpenJev as the self-hosted path**: both `razorback16/openjev` (Python/
  vLLM/MLX, DiffusionGemma-26B) and `GitHub30/OpenJev` (any HF instruct
  model) implement the identical `/v1/systemone` contract, so `router_url`
  pointed at either serves as a credential-free (or self-issued-credential)
  self-hosted alternative to TypeSafe's hosted API — the same
  `JevRouter`/config plumbing serves both without a separate code path.

  **Router semantics implemented as specced**: `JevRouter::new(url,
  key_env, timeout, registry)`, capability filtering via `filter_candidates`
  (unlike `LayaRouter`, which doesn't filter), unknown-choice rejected as an
  error, and — a deliberate departure from `HttpRouter`/`LayaRouter` — a
  missing credential is itself a typed `Err` naming the env var (no
  silent unauthenticated request), since the brief named "missing key" as
  an explicit error case.

  **Escalation composition**: `router = "needle"` + `router_escalate =
  "auto"` (default) + `!local_only` + a non-empty credential at
  build time composes `Fallback(Threshold(needle), Fallback(Threshold(jev),
  router_fallback))`; any missing condition leaves the pre-existing
  `Fallback(Threshold(needle), router_fallback)` stack unchanged. `router =
  "jev"` as primary gets the same threshold wrap as `http`/`laya`/`needle`.

  **`--local-only` finding, as the brief asked to report**: `http` and
  `laya` are, today, *not* specially pruned under `local_only` in
  `router_from_config` — §3's "`--local-only` prunes any network router
  from the stack at construction time" describes intended behavior that
  isn't actually implemented for those two (a pre-existing gap, left
  unfixed here — out of this bounded change's scope; `local_only` already
  independently blocks the *generation*-plane network calls those routers'
  decisions would otherwise lead to, and `forge init` separately skips
  needle's own network fetch under `local_only`, so no build is actually
  network-silent through a router alone today). `jev` gets the stricter,
  brief-mandated behavior instead: pruned in **both** roles it could play.
  As an escalation tier, `!local_only` is one of the direct gating
  conditions above, so it's simply never wired in. As primary
  (`router = "jev"` with `--local-only`), `router_from_config` degrades to
  `static` with a `tracing::warn!`, rather than erroring the whole build —
  chosen to match the rest of this stack's established philosophy (`needle`
  with no weights doesn't error either; it degrades to a working, if less
  capable, router) over hard-failing a misconfiguration that has an obvious
  safe substitute.

  **Doctor**: a `jev` check line (`crates/forge-cli/src/commands/doctor.rs`)
  reports credential-detected (env var name only) + endpoint when jev could
  actually be contacted (primary or active escalation), the brief's exact
  "jev escalation: no credential (TYPESAFE_API_KEY) — on-device only"
  message when escalation is configured but uncredentialed, and an
  informational line otherwise (including under `--local-only`). No network
  probe, matching the needle probe's philosophy; never `Level::Fail`.

  **Tests**: wiremock unit tests for `JevRouter` (decision round-trip incl.
  bearer header, unknown-choice rejection, missing-credential typed error
  with zero network calls, timeout, capability filtering);
  `#[serial]`/`unsafe` env-gated composition tests in `router.rs` covering
  all three brief-specified escalation scenarios (available+credentialed,
  no-credential, `--local-only`) plus the jev-as-primary local-only and
  low-confidence/timeout-fallback cases; a BDD scenario
  (`tests/features/jev_escalation.feature`) proving an unreachable jev
  endpoint still completes a run with `fallback_used: true`; a
  `forge-config` validate/env/explain suite for the new `router_escalate`
  key; and `jev_check` unit tests in `doctor.rs`. `TYPESAFE_API_KEY` was
  added to the BDD harness's env-hygiene scrub list
  (`crates/forge-cli/tests/bdd/world.rs`) so a developer's shell can't leak
  a real credential into an otherwise-hermetic scenario and cause a live
  escalation attempt against `api.typesafe.ai`.

  **Amendment (Task 10, `forge mcp` shipped 2026-09-24).** Two findings
  worth recording, because both contradicted assumptions the brief was
  written with:

  - **The MCP spec moved out from under §2 item 4.** The current revision
    is **`2026-07-28`**, and it *removed the handshake*: there is no
    `initialize`/`notifications/initialized` lifecycle any more. Every
    request instead declares its own version in
    `_meta["io.modelcontextprotocol/protocolVersion"]`, servers **MUST**
    implement a new `server/discover` RPC, results carry a `resultType`
    discriminator, and an unsupported version is answered with
    `UnsupportedProtocolVersionError` (`-32022`, `data.supported`).
    Revisions `2025-11-25` and earlier ("legacy") still use the
    handshake, and the spec explicitly blesses **dual-era** servers.
    Verified at
    `modelcontextprotocol.io/specification/versioning`,
    `/2026-07-28/basic/versioning`, `/2026-07-28/server/discover`,
    `/2026-07-28/basic/transports/stdio`, `/2026-07-28/server/tools`.
    Framing was confirmed to be what the brief assumed: newline-delimited
    JSON-RPC, one message per line, no `Content-Length` headers, and the
    server "MUST NOT write anything to its `stdout` that is not a valid
    MCP message". `forge mcp` serves **both** eras from one process,
    which is not optional in practice: current Claude Code probes with
    `server/discover` and (per `anthropics/claude-code` issue #96183) can
    then still send `initialize`.

  - **SDK: the official `rmcp` crate, not a hand-roll.** The brief's
    fallback ("a few hundred lines with serde_json") was costed against
    the old three-method handshake; dual-era negotiation is materially
    more protocol to own. `rmcp` 3.4.1 passes every criterion the brief
    set — stable edition-2024 (MSRV 1.88), 13 new transitive crates for
    the `server` + `transport-io` slice with `macros`/`schemas` off, and
    full `initialize` + `server/discover` + `tools/*` coverage — and its
    stdio serve loop already picks the era from how the client opens.
    Tool schemas stay hand-written `serde_json` as specified. The
    protocol logic remains testable without a process: `ForgeTools::call`
    takes a tool name plus a `serde_json::Value` and returns a
    `ToolOutcome`, with process-level handshake tests
    (`crates/forge-cli/tests/mcp.rs`) on top for both eras, stdout purity,
    and the approval round-trip.

  **Sharing, not duplicating.** Two extractions kept the adapter from
  re-implementing CLI behavior: `forge-graph`'s new `query` module is now
  the single lexical+semantic ranking implementation (it takes an
  `Embedder` the caller built, so the crate stays model-free) used by
  `forge graph context`, `forge graph grep --semantic`,
  `forge_graph_context` and `forge_graph_grep`; and `doctor`'s checks were
  split into `collect_checks` + `report_json`, which `forge doctor`,
  `forge doctor --json` and the `forge_doctor` tool all render — the tool
  reaches them through a `Diagnostics` seam implemented in `forge-cli`
  (the checks span providers/graph/skills/needle, a combination no lower
  crate can see), never by shelling out. `AgentService` gained
  `start_run_with_options` so the adapter can honour a per-call
  `max_turns`; `start_run` delegates to it, so the REST adapter is
  unchanged.

  **Amendment (`forge acp` shipped 2026-09-24).** §2 item 3 is
  implemented, in `crates/forge-acp` + `crates/forge-cli/src/commands/acp_cmd.rs`.
  Four things are worth recording:

  - **Protocol version: 1, and it is stable.** Unlike MCP, ACP did *not*
    move out from under the brief. Verified against the authoritative
    schema source (`agent-client-protocol-schema` **1.9.1**, which the
    SDK pins with `=`): `ProtocolVersion` is a single integer "only
    bumped for breaking changes", `V1` is `LATEST`, and `V2` exists only
    behind an `unstable_protocol_v2` feature as a draft. Framing is
    newline-delimited JSON-RPC 2.0 over stdio. The method names, the
    `session/update` nesting (`{sessionId, update:{sessionUpdate, …}}`),
    the `ToolKind`/`ToolCallStatus` vocabularies, the five stop reasons,
    and the `outcome`-tagged permission outcomes are all as the brief
    assumed. Docs: `agentclientprotocol.com/protocol/{overview,
    initialization,session-setup,prompt-turn,tool-calls}`.

  - **SDK: rejected on cost, not capability — the opposite call to MCP's.**
    `agent-client-protocol` 2.2.0 (2026-09-18, the ACP org's official Rust
    SDK) builds fine on our toolchain and does cover the agent side. It
    also adds **52** new transitive crates to this workspace, measured by
    resolving it alone and diffing against `Cargo.lock` — against
    `rmcp`'s 13 — including a second async reactor (`async-io`,
    `async-process`, `async-signal`, `polling`, `blocking`) beside tokio,
    two more datetime libraries (`jiff`, `time`) beside chrono, `defmt`
    (an embedded logging framework), and the
    `darling`/`strum`/`serde_with`/`derive_more` proc-macro trees. The
    2.x surface is a framework (roles, components, proxy chains, protocol
    routers, MCP-over-ACP) moving fast (1.0 → 2.2 in three months), where
    what this adapter needs is one stdio loop and a dozen message types.
    So `forge-acp::protocol` carries the v1 subset, transcribed
    field-by-field from the schema crate's source rather than from prose,
    and the SDK stays the documented escape hatch behind the same module
    boundary. The brief's premise that forge-mcp already ships a
    hand-rolled JSON-RPC stdio loop to share turned out to be false —
    `rmcp` owns all of that — so there was nothing to factor out and no
    shared plumbing module was created.

  - **Two seams, mirroring MCP's `Diagnostics`.** An ACP client picks the
    project root *per session* (`session/new`'s `cwd`), so the runtime
    cannot be built once at startup: `forge-acp` declares a
    `ServiceFactory` and `forge-cli` implements it by overriding
    `--project`, which keeps every session on the same
    `build_run_service` path (needle seam, config discovery, provider
    resolution) as every other subcommand. And the **ACP session id *is*
    the forge session id**, so an editor-driven turn is inspectable with
    `forge session show <id>` and continuable with `forge resume <id>` —
    asserted by a test, because it is the kind of property that silently
    stops being true.

  - **Approval is the same parked-run mechanism, answered differently.**
    stdin is the protocol channel, so the loop cannot prompt; a risky
    operation under `approval = "prompt"` parks on `ApprovalRequested`,
    which becomes a `session/request_permission` request naming the tool
    call already on the client's screen, and the chosen option maps back
    to `send_input("y"/"n")`. Only "once" options are offered:
    forge's gate is per-operation with nowhere to persist a standing
    decision, so `allow_always` would be a lie. A client that cancels
    instead of answering (`{"outcome":"cancelled"}`) is treated as a
    denial *and* still unblocks the run — a parked run that nobody
    answers is the one failure mode that hangs a turn.

  **Recorded follow-ups** (deliberately not in v1, both advertised
  honestly in `initialize`): an **editor-filesystem bridge** — v1 ignores
  the client's `fs`/`terminal` capabilities and executes through forge's
  own `ExecutionProvider` in the project root, so unsaved editor buffers
  are invisible and edits land on disk; and **token streaming** — the
  agent loop produces final text rather than a token stream, so the
  answer is one `agent_message_chunk` rather than a faked stream. Tool
  calls *are* streamed live.

- **Session-substrate follow-ups** (recorded from sub-project 6a, Phase A).
  Deliberately not in scope there:

  - **Cross-process background runs.** `attach`/`list_runs` are
    in-process: a run started by another `forge` process attaches to its
    stored history, but its live events arrive only as the session log
    grows. A real detach/reattach needs a daemon or a socket the runtime
    does not have, and the chat UI (Phase B) drives runs in its own
    process, so it does not need one yet.
  - **Replay fidelity has two honest floors.** Tool output stored in
    `tool_result` is capped (64 KiB, with an explicit truncation marker),
    and the history is fitted to a character budget estimated from the
    model's token context window at four characters per token. Both are
    approximations that report themselves rather than failing silently;
    a real tokenizer per provider would be the next step, if it ever
    matters.
  - **`forge resume` continues; it cannot branch in place.** A resume
    replays everything up to the target run. Keeping two continuations of
    the same past is what `forge session fork` is for. The cost of
    copying is that a run id is no longer unique across sessions, which
    `resume <run-id>` resolves in favour of the older session.
  - **Forked-session ACP/MCP surface.** Neither adapter exposes forking
    yet; `forge session fork` is CLI-only, and a fork is just a session
    afterwards, so the adapters need no change to work with one.
  - **Session logs are now sensitive.** `tool_result` persists what each
    tool returned, so an agent that reads a credentials file writes it to
    `.forge/sessions/*.jsonl`. The redactor is shape-based (`sk-…`,
    `Bearer …`, `ghp_…`, `xox…`) plus this process's
    `*KEY*`/`*TOKEN*`/`*SECRET*`/`*PASSWORD*` env values, so anything it
    does not recognise lands in the log. Recorded as a known limitation
    rather than solved: the alternatives (not storing tool output, or
    content-classifying it) each cost more than they buy at this stage —
    the first removes the memory layer's whole point, the second is a
    guess dressed as a guarantee.
  - **`subscribe`/`send_input` still create state for an unknown run id.**
    Both must serve an id that has been handed out but not yet started
    (ACP subscribes before `start_run_with_options`; `forge run` queues
    piped stdin before `run_with_options`), so an id nothing is known
    about gets a real channel. Entries for *finished* runs and for runs
    live in another process are refused, and a started run's entries are
    pruned when it ends — but an id that is never run leaves one behind.
    Every production caller passes an id it just generated; a UI that
    subscribed to arbitrary strings would need a bound.
