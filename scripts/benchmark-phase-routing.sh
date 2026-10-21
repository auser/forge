#!/usr/bin/env bash
set -euo pipefail

# Compare Forge's ordinary execution workflow with phase-aware planning on the
# same tiny, disposable Rust task. Results and raw event logs remain in a temp
# directory so runs are auditable without exposing repository source.

FORGE_BIN=${FORGE_BIN:-target/debug/forge}
FORGE_BENCH_ROUTER=${FORGE_BENCH_ROUTER:-static}
FORGE_BENCH_MODEL=${FORGE_BENCH_MODEL:-}
FORGE_BENCH_PROMPT=${FORGE_BENCH_PROMPT:-Add a public function named double that returns twice its i32 argument, with a unit test.}

command -v jq >/dev/null || {
  echo "benchmark requires jq" >&2
  exit 2
}

if [[ ! -x "$FORGE_BIN" ]]; then
  echo "Forge binary is not executable: $FORGE_BIN (build it or set FORGE_BIN)" >&2
  exit 2
fi

bench_dir=$(mktemp -d "${TMPDIR:-/tmp}/forge-phase-benchmark.XXXXXX")
results="$bench_dir/results.jsonl"

make_project() {
  local project=$1
  mkdir -p "$project/src"
  printf '%s\n' '[package]' 'name = "phase-benchmark"' 'version = "0.1.0"' 'edition = "2024"' >"$project/Cargo.toml"
  printf '%s\n' 'pub fn identity(value: i32) -> i32 {' '    value' '}' >"$project/src/lib.rs"
  "$FORGE_BIN" --project "$project" init >/dev/null
}

summarize() {
  local case_name=$1
  local project=$2
  local exit_code=$3
  local latency_seconds=$4
  local stdout_file=$5
  local stderr_file=$6
  local session_id events_file

  session_id=$(
    "$FORGE_BIN" --project "$project" --json session list \
      | jq -r 'last.session_id // empty'
  )
  events_file="$bench_dir/$case_name.events.json"
  if [[ -n "$session_id" ]]; then
    "$FORGE_BIN" --project "$project" --json session show "$session_id" >"$events_file"
  else
    printf '[]\n' >"$events_file"
  fi

  jq -cn \
    --arg case "$case_name" \
    --argjson exit_code "$exit_code" \
    --argjson latency_seconds "$latency_seconds" \
    --arg session_id "$session_id" \
    --arg stdout "$stdout_file" \
    --arg stderr "$stderr_file" \
    --slurpfile events "$events_file" '
      ($events[0] // []) as $all
      | (reduce $all[] as $event
          ({phase: "general", usage: []};
           if $event.type == "routing_decision_made" then
             .phase = ($event.phase // "general")
           elif $event.type == "usage_recorded" then
             .usage += [{
               phase: .phase,
               model: $event.model,
               prompt_tokens: ($event.usage.prompt_tokens // 0),
               completion_tokens: ($event.usage.completion_tokens // 0),
               total_tokens: ($event.usage.total_tokens // 0),
               cost_usd: ($event.cost_usd // 0)
             }]
           else . end)) as $scan
      | {
          case: $case,
          exit_code: $exit_code,
          completed: ($exit_code == 0 and any($all[]; .type == "completed")),
          latency_seconds: $latency_seconds,
          session_id: $session_id,
          routes: [$all[] | select(.type == "routing_decision_made") | {
            phase, router, selected_model, fallback_used, confidence
          }],
          usage_by_phase_and_model: [
            $scan.usage
            | group_by([.phase, .model])[]
            | {
                phase: .[0].phase,
                model: .[0].model,
                prompt_tokens: (map(.prompt_tokens) | add),
                completion_tokens: (map(.completion_tokens) | add),
                total_tokens: (map(.total_tokens) | add),
                cost_usd: (map(.cost_usd) | add)
              }
          ],
          stdout: $stdout,
          stderr: $stderr
        }' >>"$results"
}

run_case() {
  local case_name=$1
  local plan_flag=$2
  local project="$bench_dir/$case_name"
  local stdout_file="$bench_dir/$case_name.stdout.json"
  local stderr_file="$bench_dir/$case_name.stderr.log"
  local started finished exit_code
  local -a command=("$FORGE_BIN" --project "$project" --json --router "$FORGE_BENCH_ROUTER")

  make_project "$project"
  if [[ -n "$FORGE_BENCH_MODEL" ]]; then
    command+=(--model "$FORGE_BENCH_MODEL")
  fi
  command+=(run)
  if [[ "$plan_flag" == "yes" ]]; then
    command+=(--plan)
  fi
  command+=("$FORGE_BENCH_PROMPT")

  started=$SECONDS
  set +e
  "${command[@]}" >"$stdout_file" 2>"$stderr_file"
  exit_code=$?
  set -e
  finished=$SECONDS
  summarize "$case_name" "$project" "$exit_code" "$((finished - started))" "$stdout_file" "$stderr_file"
}

run_case baseline no
run_case planned yes

jq -s '.' "$results" >"$bench_dir/summary.json"
echo "Benchmark artifacts: $bench_dir"
jq '.' "$bench_dir/summary.json"
