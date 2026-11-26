# SourceHarness

Clean-slate Rust implementation of the **SourceHarness (Responses API native)** tool kernel, real workspace tool router, streaming CLI/REPL, and live evaluation harness.

## Architecture

- **Pure 6-Module Tool Kernel (`error`, `budget`, `store`, `search`, `read`, `registry`)**:
  - Unidirectional dependency DAG with zero filesystem or network IO coupling.
  - `FoldStore`: Task-isolated read-only snapshot store with FIFO eviction, unified 1-based CRLF/LF line indexing, Markdown section extraction (ignoring fenced code blocks, preserving duplicate headings), and small-inline vs. large-folded routing.
  - `search_result`: Exact single-pass literal/regex search, capture-group aggregation, and deterministic 8 KiB budget trimming that preserves all scalar ground-truth counts.
  - `read_result`: Whole-line 1-based pagination (`start_line`, `line_count`) within 8 KiB, with `UNREADABLE_SINGLE_LINE` protection.
  - `registry`: Single-source-of-truth Responses API flat function declarations and unified `dispatch()`.
- **Workspace Producer Tools (`workspace/`)**:
  - Sandboxed read-only `list_files`, `read_file`, and `grep_files` routed through `FoldStore::ingest` so small outputs pass inline and large outputs fold automatically for on-demand `search_result` / `read_result`.
- **Responses API Transport (`transport/`)**:
  - Stateless SSE loop (`input` array replay including `reasoning` items, `function_call`, and `function_call_output`; never sends `store` or `previous_response_id`).
  - Incremental `output_text.delta` streaming observer (never logs raw reasoning text).
  - Pluggable fold-envelope injection (`inline` user message vs. `native` synthetic `function_call` + `function_call_output`).
- **Live Verification Suite (`eval`)**:
  - End-to-end live scenarios checking mixed inline+folded workspace execution, `inline` vs. `native` paired A/B comparison, multi-page section reading, and recoverable error self-correction.

## Gateway Configuration

```powershell
$env:OPENAI_BASE_URL = "http://127.0.0.1:3425/v1"
$env:OPENAI_API_KEY  = "magpie"
$env:OPENAI_MODEL    = "auto/deepseek-v4.1-flash-1017"
```

## CLI Commands

Folded input uses `inline` by default. Requests use `low` reasoning effort unless you set `OPENAI_REASONING_EFFORT` or pass `--effort`.

```powershell
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo run -- schema
cargo run -- eval --root .
cargo run -- bench --workers 6
cargo run -- agent --root . --prompt "List files in src/ and summarize MAX_RESPONSE_BYTES in src/budget.rs."
cargo run -- chat --root .
cargo run -- smoke --strategy inline
cargo run -- smoke --strategy native
cargo run -- run --file README.md --prompt "Summarize this file."
```

## Resumable workspace sessions

Add `--session` to `agent` or `chat` to let SourceHarness research, read, edit, and write workspace files. A session stores requests, tool results, file revisions, and checkpoints under `.sourceharness/sessions/<name>` in that workspace. A session has one active writer. Ordinary `agent` and `chat` invocations retain their read-only tools.

```powershell
cargo run --locked -- agent --root . --session config-review --prompt "Apply the current configuration policy and report the changes."
cargo run --locked -- agent --root . --session config-review --resume
cargo run --locked -- agent --root . --session config-review --continue --prompt "Use the updated policy."
cargo run --locked -- history --root . --session config-review
```

Use `history --event <n>` to inspect a request or receipt and `history --ref <ref>` to open its original text. A `result_ref` belongs to its session and retains the old content when a live file changes. A validation receipt includes `workspace_revision`. Use `fork --revision <hash> --to <new-directory>` with the same `--root` and `--session` to open that tested file snapshot in a fresh workspace.

`research` searches the workspace, stored results, and session history. Literal queries can span lines. Pass the returned `next` arguments back to `research` to enumerate the same stored hit set. Open `arguments_ref` with `read` to retrieve the original arguments of a historical tool call. Use `search_result` on a saved reference for exact occurrence counts, regex searches, and capture aggregation.

`read` returns literal UTF-8 text and the source revision, with continuation coordinates when a result spans pages. `edit` matches exact old text by default. `--edit-mode range` and `--edit-mode patch` select alternative contracts. Use `--resume` with the original model, executable, tool options, and validation setup. Use `--continue` to open a linked run under current conditions and optionally supply a correction. It reports the observed state of outstanding calls for the model to decide what remains. A recovery conflict includes the diff from the planned file to the current file when both versions are available.

The default `--history-mode events` attaches a list of related records. `facts` groups the same records by type within the same budget. `conversation` uses the conversation alone by default. The related records link earlier validation reports to the workspace, tool definitions, executable, model, and check configuration in effect when they ran.

Use `--freshness-instructions on|off` and `--stale-projection on|off` to vary the reminder and the treatment of stale file contents independently. A reference to the original snapshot remains available when a result is reduced to a version notice. `--history-budget` sets the auxiliary input budget in bytes. Request events audit the selected facts and bytes. The model provider reports tokens for the whole request.

To give the model a validation tool, pass `--checks <file>` with a map of check names to executable configurations. For example, the following command runs a workspace test script against a captured file snapshot and records the checked revision, program digest, stdout, stderr, and exit status.

```json
{
  "acceptance": {
    "program": "C:/absolute/path/to/python.exe",
    "args": ["verify.py"],
    "description": "Check the workspace deliverables against their acceptance criteria.",
    "timeout_seconds": 60
  }
}
```

Reports distinguish a completed write receipt from a matching target observed during recovery. File revisions guard edits against observed conflicts. External editors share the filesystem, so programs that bypass the workspace lock can still race the file replacement. Session data contains workspace content and provider responses for recovery. Keep it local or review it before sharing.

Run the real-model comparison with the release executable and a fresh output directory.

```powershell
python -B -m unittest discover -s scripts -p "test_*.py"
python -B scripts/eval_session.py --exe target/release/source_harness.exe --out target/session-eval
python -B scripts/eval_session.py --exe target/release/source_harness.exe --out target/session-history --profile history --repeats 2
```

The runner scores the final workspace against fixtures and expected values held outside the model workspace. A passing run also needs a successful model-initiated validation of the delivered revision. The report keeps known token and latency totals and counts requests with missing usage separately.
