# OpenRouter as a model catalogue

**Status:** design, awaiting implementation plan
**Date:** 2026-09-25
**Scope:** its own sub-project. Independent of the Session core, but it feeds
the spend ceiling in that document's §19.

## What already works, so nobody rebuilds it

OpenRouter needs **no code today**. The OpenAI-compatible provider in
`forge-providers/src/model.rs` is generic over `base_url` (it posts to
`{base_url}/chat/completions`), and provider-family inference classifies any
non-Anthropic base URL as the `openai` family. So this is a working
configuration right now:

```toml
[models."anthropic/claude-sonnet-4.5"]
base_url = "https://openrouter.ai/api/v1"
key_env  = "OPENROUTER_API_KEY"
tools = true
max_context = 200000
cost_input_per_mtok  = 3.0
cost_output_per_mtok = 15.0
```

`forge init` already reports `OPENROUTER_API_KEY` when it is set —
`KNOWN_PROVIDER_KEYS` in `commands/init.rs` lists it with `None` for its
built-in entry, meaning the key is detected but unlocks nothing usable.

Giving it a built-in entry, the way `DEEPSEEK_API_KEY → deepseek-chat` and
`MOONSHOT_API_KEY → kimi-k2.7-code` already work, is a separate one-line
change and is **not** what this document is about.

## The actual problem

`router = "cheapest"` cannot see prices it was not told.

`build_cheapest` reads `config.model_entries()` and calls `entry.costs()` —
hand-typed `cost_input_per_mtok` / `cost_output_per_mtok` values. Two
consequences:

- **It is only as accurate as the config.** A stale number silently mis-ranks,
  and nothing detects the drift. Prices move; the file does not.
- **It cannot consider a model nobody declared.** `CheapestRouter`'s pool is the
  requested candidates or `self.registry` — both derived from configured
  entries. OpenRouter brokers hundreds of models; forge can only rank the
  handful someone typed out.

So "cheapest" today means "cheapest among the models you already thought to
list, priced as of whenever you last looked". That is a weaker claim than the
router's name makes.

The spend ceiling planned in the Session core spec (§19) inherits the same
weakness: it computes spend from those same hand-typed figures, so a budget is
enforced against prices that may not be real.

## Design

### A catalogue source, not a provider

OpenRouter publishes `GET /api/v1/models`: id, context length, and per-token
input/output pricing for every model it brokers. This design consumes that as a
**catalogue** — a source of model *facts* — while leaving generation on the
existing OpenAI-compatible provider. The two concerns stay separate: one answers
"what models exist and what do they cost", the other answers "run this prompt".

```
                  ┌─ catalogue: GET /api/v1/models ─▶ ids, context, prices
  OpenRouter ─────┤
                  └─ generation: POST /chat/completions ─▶ existing provider
```

### Cache, with an explicit staleness contract

The catalogue is fetched once and cached at
`~/.cache/forge/openrouter/models.json` with its fetch timestamp.

- Refreshed on `forge bootstrap` / `forge init`, and on explicit
  `forge model refresh`.
- A cache older than a configurable TTL (default 7 days) is still **used**, with
  a warning naming its age. Stale prices beat no prices, and a routing decision
  must not block on a network call.
- No catalogue at all — never fetched, or `--local-only` — falls back to exactly
  today's behaviour: hand-typed costs from config. This is the degradation rule
  the rest of the project follows, and it means the feature cannot make forge
  worse than it is now.

### Config precedence, most specific first

1. An explicit `cost_*_per_mtok` in a `[models.<name>]` entry. An operator who
   typed a number meant it — perhaps they have negotiated pricing — and the
   catalogue must not overrule it.
2. The cached catalogue, matched on model id.
3. Nothing: the model is unpriced, and `cheapest` must not rank it above a
   priced one. Treating unknown as free is how a router picks the most expensive
   model in the pool by accident.

That third rule is the one worth testing hardest, because the naive
implementation — `unwrap_or(0.0)` — inverts the router's entire purpose.

### Candidate pool

Catalogue entries do **not** silently join the routing pool. Forge would
otherwise start proposing models the operator never opted into, with
capabilities it cannot verify and a bill it cannot predict.

Instead: `forge model list --catalogue` shows what is available with prices, and
`forge model add <id>` writes a `[models.<id>]` entry pre-filled from the
catalogue. The operator's config stays the sole declaration of what forge may
route to; the catalogue makes declaring it accurate and one command.

### Capabilities

OpenRouter reports context length, which maps onto `max_context`. It does not
reliably report tool support in a form forge can trust across every brokered
model, so `tools` is **not** inferred — an entry added from the catalogue gets
`max_context` filled and `tools` left to the operator. Guessing tool support
wrong means the agent loop hands tools to a model that ignores them, which
surfaces as a baffling empty response rather than a clear error.

## Testing

- **Unpriced models never rank as cheap.** A pool mixing priced and unpriced
  entries must never select an unpriced one on price. This is the inverted-logic
  trap; assert it directly.
- **Precedence**: a config `cost_input_per_mtok` beats a catalogue price for the
  same id, and the catalogue beats absence.
- **Stale cache is used, and warns** — with its age in the message.
- **Absent cache is not an error**: `cheapest` falls back to config-only ranking
  and still routes.
- **Catalogue fetch is offline in tests** — `wiremock`, following the pattern in
  `jev.rs`'s tests. No test may reach `openrouter.ai`.
- **Malformed or partial catalogue JSON** degrades to config-only rather than
  failing a run; a broker changing its response shape must not break routing.

## Risks

| Risk | Mitigation |
|---|---|
| `unwrap_or(0.0)` on a missing price inverts `cheapest` | Unpriced is unrankable, asserted by its own test |
| Catalogue pricing drifts from what is actually billed | Prices are advisory; the §19 budget measures real usage, and the cache's age is always visible |
| A network fetch on the routing hot path | Never: routing reads the cache only. Fetch happens at bootstrap or on explicit command |
| Auto-adding brokered models to the pool | Rejected by design — the catalogue informs `forge model add`, it does not expand the pool |
| `--local-only` leaking a catalogue fetch | Pruned like every other network source |

## Out of scope

- OpenRouter's own routing/fallback features. Forge has a decision plane; layering
  another router underneath it would make the reason for a model choice
  unattributable, which is exactly what the decision log exists to prevent.
- The built-in `OPENROUTER_API_KEY` model entry — a separate small change.
- Catalogues from other brokers. The seam here is a catalogue source; a second
  implementation is a later, easy addition if one is wanted.
