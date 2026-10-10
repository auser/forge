# Beta qualification

Qualification run: 2026-10-09 on macOS arm64. The tested code commit was
`01ac8b0a4103b14d466a13c11ad106ed74b52ddc`; only documentation changes follow
that commit in this branch.

## Candidate installation

`cargo install --locked --path crates/forge-cli` installed Forge into an
isolated temporary prefix. The installed binary reported:

```text
forge 0.2.6 (commit 01ac8b0a4103, target aarch64-apple-darwin)
```

The offline installer verification in `tests/install-path.sh` also passed,
including checksum verification and PATH-shadow diagnostics. The qualification
run initially warned that the locked `yoke-derive 0.8.3` release was yanked.
On 2026-10-10 the lockfile was refreshed narrowly to `yoke-derive 0.8.4`, and a
fresh `cargo install --locked --path crates/forge-cli` completed without the
yanked-release warning.

## Development workflow

The installed binary initialized a fresh disposable Git project in
`--local-only` mode, built its graph, and then ran the deterministic
inspect/edit/check/review acceptance workflow through the explicitly gated
scripted provider.

- Task `01M4J4S7NNFP8DXEGYDMFXE3K2` finished in `succeeded` state.
- Session `01M4J4S7PCREACAKAYZ5M66AEV` retained the route, tool policy
  decisions, tool results, changed path, check result, review, and diff.
- The workflow read and edited `main.rs`, passed `sh check.sh`, and returned a
  reviewable Git diff.
- The task view reported 80 output tokens and `cost_usd: null`, preserving the
  distinction between an unknown price and known-free execution.
- `provider_limit_parks_and_resumes_the_same_durable_task` passed against the
  candidate, confirming that a provider limit parks the durable task and resume
  completes the same task without replaying completed effects.

## Provider and routing evidence

Only access already authorized on the qualification machine was used.

| Provider path | Result |
| --- | --- |
| Codex subscription (`gpt-5.6-sol`) | Passed `doctor --live` in 9,770 ms and two turns. The session recorded 2,216 tokens across two unpriced calls. Static routing handled the pinned canary after Needle declined; Jev was not armed. |
| Claude Code subscription (`claude-sonnet`) | Credential detected. No new call was made because the preceding release-gate canary received HTTP 429; Forge did not retry or route around the limit. |
| Kimi Code subscription (`k3`) | Credential unavailable; no live claim. |
| Local OpenAI-compatible model | Configured loopback endpoint unreachable; no live claim. |
| API-key providers | Credentials unavailable; no live claim. |

Subscription models now carry no per-token price unless an operator configures
one. `forge model list` reports `null` costs for Claude, Codex, and Kimi
subscription entries. `forge doctor` and `forge session decisions` report known
spend separately from unpriced calls; the Codex canary reported `$0.0000` known
spend plus 2,216 unpriced tokens, rather than describing the subscription as
free. Explicitly zero-priced local models remain known-free.

## Status and limitations

This candidate is qualified for a Codex-backed beta after its required CI
passes. It is not evidence for live Claude, Kimi, local-model, or API-key
support on this machine. A release claiming any of those paths must run the
documented authorized canary first. No account rotation, quota pooling,
credential sharing, publication, tagging, or deployment occurred.

Serena-style code intelligence remains outside this milestone.
