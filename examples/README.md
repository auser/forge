# Drop-in configuration presets

Copy a preset into your project and Forge runs with it immediately:

```bash
forge init                                                   # creates .forge/
cp examples/configs/local-first.toml .forge/config.toml      # pick your preset
forge doctor                                                 # verify the setup
```

Or install one globally for all projects:

```bash
cp examples/configs/local-first.toml ~/.config/forge/config.toml
```

Remember precedence: CLI flags > shell env / `FORGE_*` > `.env.local` > `.env`
> project config (`.forge/config.toml`) > user config (`~/.config/forge/`) >
built-in defaults. Check where any value comes from with
`forge config explain <key>`.

## Presets

| Preset | Use it when | Needs running |
|---|---|---|
| [`configs/local-first.toml`](configs/local-first.toml) | **Recommended.** You have a GPU/Apple Silicon and want fully local, free inference | a local model server (oMLX/Ollama/LM Studio/llama.cpp) |
| [`configs/hybrid-needle.toml`](configs/hybrid-needle.toml) | Local model for easy tasks, hosted models when the on-device brain says the task is hard | a local model server; hosted keys optional |
| [`configs/budget-hosted.toml`](configs/budget-hosted.toml) | No local GPU; always route to the cheapest capable hosted model | A provider API key in `.env` |
| [`configs/offline-eval.toml`](configs/offline-eval.toml) | Trying Forge with zero dependencies, CI, demos | nothing |
| [`configs/hybrid-laya.toml`](configs/hybrid-laya.toml) | You specifically want the Laya adapter doing the routing instead of the embedded brain | `pip install laya` + `forge router serve` |

`router = "laya"` is a legacy setting: the built-in default is now the
embedded needle brain. Only `hybrid-laya.toml` sets it, and only because
that preset is *about* the adapter.

Inside a `[models.<name>]` entry the endpoint/key fields are `base_url` and
`key_env` — **not** `model_base_url`/`model_key_env`, which are top-level
keys. Forge rejects the wrong spelling at load time and names the right
field, rather than letting it silently do nothing.

## Environment keys

Copy [`env.example`](env.example) to `.env` in your project root and fill in
what you have — Forge loads `.env`/`.env.local` at startup, detects known
provider keys during `forge init`, and redacts their values from all logs:

```bash
cp examples/env.example .env
```

Only keys with a value set become active; a model whose `key_env` is unset is
never called (hosted models are only contacted when routing selects them).
