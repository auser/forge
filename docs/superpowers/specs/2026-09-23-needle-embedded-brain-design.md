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
`medium` (default) / `full` (~29 MB). Optional cargo feature
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
variant = "medium"                # small | medium | full
weights_path = ""                 # override; empty → ~/.cache/forge/models/
autofetch = true                  # forge init downloads + verifies weights
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
  it never blocks `forge init` or the router. Since `medium` is
  `NeedleConfig::default().variant`, **a fresh `forge init` with no
  config overrides does not fetch anything today**; a project must set
  `needle.variant = "full"` to get real weights. Either vendor a
  small/medium cut later (once Cactus hosts one, or forge builds its own
  via the CLI) or reconsider the default variant in a follow-up task.
- **C API stability**: Needle 3 shipped 2026-09-18; pin an exact
  library + weights version, verify by checksum.
- **Quality bar**: Needle's routing/guardrail accuracy on forge's
  decision phrasing must be validated during implementation; the
  confidence threshold and static fallback bound the blast radius of
  wrong-but-confident decisions.
