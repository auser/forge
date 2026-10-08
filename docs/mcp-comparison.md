# Full vs compact MCP comparison

`tools/mcp_comparison.py` is a separate Python 3.10+ standard-library harness.
It launches real `forge mcp` subprocesses, using full discovery or `--compact`.
It does not alter the user's project, configuration, or credentials.

## Offline protocol smoke test (default)

From the repository root:

```sh
python3 -m unittest discover -s tools -p 'test_mcp_comparison.py'
cargo build -p forge-cli
python3 tools/mcp_comparison.py --forge target/debug/forge --trials 2 --output target/comparison.json
```

The binary path is resolved before changing directories. The default is
`target/debug/forge`. JSON is always written to stdout; `--output` additionally
writes it to a file. Exit status is **1 if any task/arm/trial fails**, otherwise 0.
CLI validation errors exit 2.

This mode is explicitly **protocol-only** (`evaluation_kind: "protocol_only"`).
Its scripted client already knows the native operations; compact calls search,
schema, then invoke. Its final lookup/read output is the returned data, not a
model-generated answer. It checks protocol correctness and exposes payload/call
overhead, but cannot establish better model selection, token usage, answer
quality, or end-to-end model performance. It does not access a model endpoint.
Do not infer a recommendation from tools/list bytes alone.

## Opt-in outer-model evaluation

```sh
python3 tools/mcp_comparison.py --mode model \
  --forge target/debug/forge \
  --base-url http://localhost:8080/v1 --model YOUR_MODEL \
  --api-key-env MCP_COMPARISON_API_KEY \
  --trials 3 --timeout 60 --max-turns 40 --output target/model-comparison.json
```

Supply `MCP_COMPARISON_API_KEY` in the invoking shell through your normal secret
manager/environment workflow, **not in source files, command-line literals, or
committed reports**. Omit `--api-key-env` for endpoints requiring no key. An unset
or empty explicitly requested key variable is an error.

For a hosted endpoint, replace the URL and add `--allow-remote`. Without that
flag only literal loopback IPs and `localhost` are accepted; hostname lookalikes
are not. The URL must end in `/v1` and may not include userinfo, query parameters,
or a fragment. Ambient HTTP proxies are disabled. Redirects are never followed,
so requests and credentials cannot be forwarded to another origin by a redirect.
Use HTTPS for remote endpoints; it is required when sending a key to a
non-loopback endpoint.

This is nonstreaming OpenAI-compatible **Chat Completions**, not the Responses
API. Compatibility and tool-calling ability depend on the endpoint/model.
The actual server's `initialize.instructions` and all `tools/list` definitions
are supplied to the model. Each model-selected advertised call is routed over
real MCP. No search query, native tool path, or approval sequence is preselected
in this mode. A fresh conversation is used for every task/arm/trial.

Tool replies sent to the model retain `isError` and use `structuredContent` as
`data` when available, otherwise `content`. The MCP compatibility mirror is not
sent twice. This rendering policy is identical in both arms.

Model mode evaluates the **outer agent**, not a nested coding model:
`forge_run` intentionally uses `scripted-mock`, with `FORGE_TEST_MOCKS=1` and
per-project `approval = "prompt"`. This removes nested generation variability
while still exercising real delegation, parking, approval input, and execution.
No hosted/model run is implied by the offline tests.

## Tasks, isolation, and verification

Every run creates fresh temporary project, HOME, XDG, and temporary directories.
Only a small OS environment allowlist reaches Forge; inherited `FORGE_*` values,
provider credentials, and arbitrary custom key variables are not passed through.
Fixtures include two Rust symbols, a built project graph, a skill, and a scripted
write. Fixture construction and graph build run locally with autofetch disabled.
The project and all associated state are deleted after each run.

Both arms receive identical user prompts:

1. Find the synthetic `authenticate_session` symbol and report its source file.
2. Read the `reviewing` skill and report a random marker present only in its body.
3. Delegate a write; wait for approval, explicitly **APPROVE**, wait for completion.
4. Delegate the same write; wait for approval, explicitly **DENY**, wait for completion.

Prompts do not disclose native MCP tool names, run IDs, the hidden skill marker,
or the expected graph filename. The marker is shared between paired arms in a
trial but randomly generated anew for the next trial. Arm order alternates:
full then compact on odd trials, compact then full on even trials.

Independent verifiers check successful returned graph/skill data **and** the
final answer. Delegation must show a real run, an observed
`waiting_for_approval` state while the output is absent, a subsequent delivered
decision for that run, and observed completion. Session events must corroborate
the attempted write's success or explicit approval denial; actual file state
and content must agree. Doing nothing cannot pass denial. Compact invocation is
unwrapped to native names for these checks.

The harness is isolation from your normal configuration, **not a security
sandbox for untrusted binaries**. The supplied Forge executable runs with your
user privileges. Use a trusted local build.

## Reading the report

Each task/arm/trial records:

- Success, safe error category, and a concrete verification failure reason.
- Latency from immediately before MCP spawn through initialization, initial
  tool listing, agent execution, and verification. Fixture/graph build and
  shutdown are excluded. Failed runs are included; failures before spawn have
  null latency and are counted in the summary.
- Attempted MCP `tools/call` count (including discovery and failed calls), error
  count, and search/schema discovery count. Initialization and `tools/list` are
  not tool calls. Unknown model tool names are rejected locally, not counted
  as MCP calls.
- Initial `tools/list` result bytes: compact UTF-8 JSON summed over all initial
  pages, excluding JSON-RPC envelopes. This is **not** a token estimate.
- Model request attempts and provider-reported prompt/completion token totals.
  Missing usage, failed requests, and partial usage coverage produce **null**,
  not fabricated totals. `usage_reported_calls` shows coverage separately for
  input and output. Protocol mode has zero model calls and null token counts.
- A small control trace (advertised/native tool names, errors, run statuses).
  Prompts, raw tool results, final answers, secrets, and HTTP error bodies are
  not written to the report. Raw Forge stderr is drained and discarded.

Per-arm summaries include **all** runs, not just successes. They show success/
failure counts, mean observed latency and its coverage, total call counts, and
byte/token totals only when all runs report those metrics. Model name, binary
path/version, limits, and `evaluation_kind` are recorded at the report root.
Compare success rates alongside costs; small synthetic trials are not a
statistically strong claim about arbitrary real-world tasks.

`--timeout` bounds MCP requests, setup commands, and model requests. HTTP uses
both a socket timeout and a wall-clock watchdog, shutting down a connected socket
on timeout. An outstanding OS DNS lookup cannot be interrupted, but its daemon
worker cannot keep the harness alive or send a request after cancelled resolution.
`--max-turns` bounds outer model responses, protocol polling phases,
and initial list pagination. Each model response may request at most 32 calls.
MCP reader/writer threads match request IDs, ignore notifications, decline
unsupported server requests, and drain stderr without printing it. Shutdown
first closes stdin, then terminates/kills and reaps the harness's own child if
it does not exit. A timeout/failure does not leave a Forge child running.

Unit tests use fake models/transports plus a tiny local subprocess to test
timeouts, IDs, notification handling, and stderr draining. They do not contact
any hosted endpoint. Run the real-binary protocol command above as the separate
integration smoke test.
