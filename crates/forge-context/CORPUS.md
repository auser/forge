# Pure compression corpus (CONTEXT-3 candidate implementation)

Run the deterministic report with:

```sh
cargo test --locked -p forge-context compression -- --nocapture
```

Fixtures are sanitized public synthetic workload generators in
`src/compression.rs`, not private sessions or live model runs. Four workload
sizes (120, 300, 600, 1200 records) are compared at 8/16/32/64 KiB thresholds.
Below 64 KiB the baseline is the entire input, not an artificially capped
input. Above 64 KiB it is the actual existing capped text and opaque retrieval
marker. All estimates serialize the complete string, including escaping,
metadata, and retrieval instructions. The rollout threshold remains strictly
greater than 64 KiB.

## Default-enabled, narrowly supported transformations

- **Log:** exact executable `cargo` with argument vector `["test"]`, the Forge
  exit/stdout/stderr envelope, and recognized libtest/compiler line prefixes.
  Only consecutive byte-identical lines become one original readable line plus
  an explicit additional-repetition count. This is a repeated-line compactor,
  not a general semantic log summary. Unknown diagnostics or command wrappers
  decline. Unique test names, warnings, errors and exit codes remain visible.
- **Search:** `graph_grep` rows with canonical positive line numbers, grouped
  by consecutive identical path. The visible representation is an ordered JSON
  list `[path, [[line, exact match], ...]]`; every match and path/line pair is
  retained. Unicode and Windows paths, escaping, ordering and repeated rows
  have direct structural checks.

Each individual accepted view must save at least 30% estimated tokens against
its real baseline and cannot exceed its byte or character budget.

Representative report at 1200 records, 64 KiB threshold (estimated tokens):

| Fixture | Original | Capped baseline | Selected view | Decision |
|---|---:|---:|---:|---|
| unique-tests | 21033 | 16658 | 16658 | NoSavings |
| repeated-warnings | 20130 | 16669 | 132 | Compressed |
| nested-json | 44726 | 19729 | 19729 | NoSavings |
| graph-symbols | 23447 | 16633 | 6450 | Compressed |
| generated-diff | 52846 | 17203 | 17203 | NoSavings |

The two log fixtures aggregate to baseline 33327 / selected 16790 (~49.6%
savings); this is driven by repeated warnings and does **not** establish a
general log benefit. Search baseline 16633 / selected 6450 is ~61.2%.
Distinct short-path search results additionally assert no-savings fallback.
These are small synthetic eligibility checks, not evidence of model quality
or representative production savings.

## Implemented but not qualified for default rollout

- **JSON:** `.json` file origin and strict raw JSON parsing. Top-level arrays
  with consecutive byte-identical elements receive readable record grouping;
  other JSON receives raw-lexeme whitespace compaction. This preserves key
  order, duplicate keys, numeric precision/spelling (including numbers outside
  machine floating-point range), all distinct values and exceptional records.
  Nested arrays are not recursively grouped. Default disabled.
- **JSONL:** `.jsonl` origin and one complete strict raw JSON value per nonempty
  physical line. Consecutive exact records become an original record with
  one-based inclusive positions and count. Different records stay visible in
  order, including schema changes. Blank lines, multiline JSON values and
  trailing garbage decline. Default disabled.
- **CSV/TSV:** `.csv`/`.tsv` origins, explicit comma/tab delimiter, a retained
  header and consistent field count. The `csv` crate parses records; an exact
  per-record writer roundtrip restricts acceptance to a canonical subset.
  Necessary quoted fields, doubled quotes, embedded quoted newlines, empty
  fields and Unicode work. LF record separators and optional final LF work;
  CRLF separators, optional unnecessary quotes, blank rows, ragged records
  and malformed/permissively parsed syntax decline. This intentionally rejects
  some valid CSV rather than repairing ambiguous input. Header and original
  records are retained verbatim, with data-record positions/counts. Default
  disabled; no new handwritten CSV parser.
- **Diff:** `.diff`/`.patch` origin, ordinary unified file/hunk headers, validated
  old/new line counts. Candidate context elision preserves two unchanged lines
  on each side of long runs, every changed line, every file/hunk header and
  no-newline markers. Omitted context is explicitly counted. Normal short-hunk
  diffs do not save enough; default disabled. Extended Git/binary/combined
  formats decline rather than silently dropping metadata.

The candidate-only diff test verifies changed-line/header/no-newline visibility,
asserts a particular unchanged-context answer is absent, then explicitly
retrieves that answer through an authorized bounded artifact search. This is
not a test of a default-selected diff view. JSON semantic/raw-lexeme tests
likewise validate the experimental candidate, not default activation.

### Structured-record candidate report

The additional matrix covers JSON arrays, JSONL, CSV and TSV at all four
thresholds and workload sizes. Each has distinct-record negative fixtures and
repetition-positive fixtures with a final exceptional record. Direct assertions
check every original record, schema, exception, raw duplicate keys and numeric
lexemes, ordering positions and counts, Unicode, quotes and embedded newlines.
No default activation follows from these synthetic repetition-positive fixtures.

At 1200 records / 64 KiB (complete serialized estimates; these are experimental
candidate decisions, **not** default-selected views):

| Format / workload | Original | Capped baseline | Experimental selected view | Decision |
|---|---:|---:|---:|---|
| JSON array / distinct | 39325 | 18704 | 18704 | NoSavings |
| JSON array / repeated | 38703 | 18740 | 183 | Compressed |
| JSONL / distinct | 39624 | 18846 | 18846 | NoSavings |
| JSONL / repeated | 39002 | 18884 | 182 | Compressed |
| CSV / distinct | 20429 | 18006 | 18006 | NoSavings |
| CSV / repeated | 19806 | 18058 | 169 | Compressed |
| TSV / distinct | 21329 | 18800 | 18800 | NoSavings |
| TSV / repeated | 20707 | 18877 | 170 | Compressed |

Grouping is an explicitly labeled view, not a valid replacement JSON/CSV file.
Byte-length headers distinguish original record content from framing. Record
separators and inter-element JSON whitespace are formatting, not reconstructed
bytes; authorized original retrieval remains the source for byte-level answers.
No opaque dictionaries or hidden distinct values are introduced.

## Assertions and limitations

- Direct candidate structural checks and exact deterministic decisions.
- Selected views must contain the same checked readable candidate and match
  exact full serialized size accounting.
- Strict threshold boundaries, no-savings fallback, unknown/protected origins,
  malformed input, raw argument non-echoing and metadata overhead.
- Bounded original-byte retrieval and unauthorized retrieval rejection.
- No filesystem, model, clock, randomness, network or retrieval expansion in
  classification/compression. Store use occurs only in tests/caller.

The pure module now has candidates for all requested format families. This does
not claim every format is qualified for rollout, that the whole CONTEXT-3
runtime/configuration ticket is complete, or that deterministic scripted
assertions establish live model task quality.
