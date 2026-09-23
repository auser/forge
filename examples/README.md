# Drop-in configuration presets

Copy a preset into your project and Forge runs with it immediately:

```bash
forge init                                          # creates .forge/
cp configs/hybrid-laya.toml .forge/config.toml      # pick your preset
forge doctor                                        # verify the setup
```

Or install one globally for all projects:

```bash
cp configs/hybrid-laya.toml ~/.config/forge/config.toml
```

Remember precedence: CLI flags > shell env / `FORGE_*` > `.env.local` > `.env`
> project config (`.forge/config.toml`) > user config (`~/.config/forge/`) >
built-in defaults. Check where any value comes from with
`forge config explain <key>`.

## Presets

| Preset | Use it when | Needs running |
|---|---|---|
| [`configs/local-first.toml`](configs/local-first.toml) | You have a GPU/Apple Silicon and want fully local, free inference | oMLX (or compatible) + Laya adapter |
| [`configs/hybrid-laya.toml`](configs/hybrid-laya.toml) | **Recommended.** Local model for easy tasks, hosted models when Laya says the task is hard | oMLX + Laya adapter; hosted keys optional |
| [`configs/budget-hosted.toml`](configs/budget-hosted.toml) | No local GPU; always route to the cheapest capable hosted model | A provider API key in `.env` |
| [`configs/offline-eval.toml`](configs/offline-eval.toml) | Trying Forge with zero dependencies, CI, demos | nothing |

## Environment keys

Copy [`env.example`](env.example) to `.env` in your project root and fill in
what you have — Forge loads `.env`/`.env.local` at startup, detects known
provider keys during `forge init`, and redacts their values from all logs:

```bash
cp env.example .env
```

Only keys with a value set become active; a model whose `key_env` is unset is
never called (hosted models are only contacted when routing selects them).
