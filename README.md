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
cargo run --locked -- chat --root . --session config-review
cargo run --locked -- history --root . --session config-review
```

Session chat streams model text and tool progress. Enter `:steer <instruction>` to send a correction during an active run. The session finishes any tool call already in progress, records skipped calls that had yet to start, and includes the correction in the next model request. Chat saves a message or correction to the session inbox before confirming receipt. Reopen the session to recover accepted input after a process stop. Plain input typed during a run queues a follow-up task until that run finishes.

Use `history --event <n>` to inspect a request or receipt and `history --ref <ref>` to open its original text. A `result_ref` belongs to its session and retains the old content when a live file changes. A validation receipt includes `workspace_revision`. Use `fork --revision <hash> --to <new-directory>` with the same `--root` and `--session` to open that tested file snapshot in a fresh workspace.

`research` requires `scope` to select the workspace, stored results, session history, a page, or a retry. Literal queries can span lines. In workspace and result searches, `context_lines` adds up to six complete lines on each side of a hit. Context over 1,200 bytes includes a `read` request for the same range and snapshot. Pass the returned `next` arguments back to `research` to enumerate the same stored hit set. The report's `result_ref` refers to the complete set. `read`, `search_result`, and `research` in result scope expand reports up to 8 MiB. Larger reports return `RESEARCH_PAGED` with a cursor. A `preview_ref` covers only its labeled subset. Open `arguments_ref` with `read` to retrieve the original arguments of a historical tool call.

`read` returns literal UTF-8 text and the source revision, with continuation coordinates when a result spans pages. `edit` matches exact old text by default. `--edit-mode range` and `--edit-mode patch` select alternative contracts. Use `--resume` with the original model, executable, tool options, and validation setup. Use `--continue` to open a linked run under current conditions and optionally supply a correction. It reports the observed state of outstanding calls for the model to decide what remains. A recovery conflict includes the diff from the planned file to the current file when both versions are available.

`write` in create mode creates missing parent directories inside the workspace. Use `--receipt-mode summary|diff|versioned` to compare the model-visible success receipt. `versioned` is the default. The journal retains the full receipt in each mode.

The default `--history-mode events` attaches a list of related records. `facts` groups the same records by type within the same budget. `conversation` uses the conversation alone by default. The related records link earlier validation reports to the workspace, tool definitions, executable, model, and check configuration in effect when they ran.

Use `--freshness-instructions on|off`, `--stale-projection on|off`, and `--workspace-change-notices on|off` to vary the reminder, stale-body projection, and per-file change notices independently. By default, per-file notices follow the projection setting. A reference to the original snapshot remains available when a result is reduced to a version notice. `--history-budget` sets the auxiliary input budget in bytes. Request events audit the selected facts and bytes. The model provider reports tokens for the whole request.

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

Use the optional `inputs` array to declare absolute paths to external dependencies such as a check script and its rules file. Receipts include the declared file digests observed before and after execution. A successful exit counts as a passing check only when those observations agree. The runner executes with the configured paths and records any drift it observes.

Reports distinguish a completed write receipt from a matching target observed during recovery. File revisions guard edits against observed conflicts. External editors share the filesystem, so programs that bypass the workspace lock can still race the file replacement. Session data contains workspace content and provider responses for recovery. Keep it local or review it before sharing.

### Compact session tools

Select the experimental profile when starting a session. Use the same flag when reopening it.

~~~bash
source_harness agent --root ./workspace --session compact-task --tool-profile compact \
  --checks ./checks.json --prompt "Apply the requested changes and verify the result."
source_harness chat --root ./workspace --session compact-task --tool-profile compact --checks ./checks.json
~~~

The compact profile sends a fixed instruction and tool prefix and appends to the conversation. Automatic history projection and notices are disabled. `read` delivers numbered text. `edit` applies a batch of exact, disjoint replacements to a single file. `write` creates a new file or replaces an observed one. Edits and replacements use the latest revision presented by an earlier live `read` or a successful `edit` or `write`, including from earlier tasks in the session. A rollback invalidates observations for restored paths. Read a missing file before recreating it if the session previously observed it. Conflicts return a diff when both file versions exist. The session journal retains revisions, raw responses and receipts.

`exec` runs shell commands on a bounded, disposable workspace copy. This profile uses Bubblewrap on x86-64 or AArch64 Linux with user namespaces and `openat2`. The command sees system binaries and libraries, a clean environment and temporary storage. Network sockets are disabled. Commands default to 15 seconds and can request up to 60. Outputs over 2 KiB are folded into a reference that `read` can open, with up to 512 KiB saved for each stream. Changes on the copy expire after each call.

Keep the checks file, acceptance scripts and declared oracle inputs outside the workspace and the system runtime mounts. `validate` runs the configured checker on a separate captured snapshot. A passing result verifies the tested revision. Run `validate` again after further edits. `rollback` restores file bytes to the latest passing snapshot in the current run, or the run's starting snapshot if no check passed. It verifies that the tracked files match the checkpoint plus recorded edits before restoring. An interrupted rollback can resume from its recorded intent.

With two model turns left, compact adds a finish reminder to the last result in the current tool batch. With one turn left, it asks for the final report on the next response. Both reminders are recorded with the delivered result and replayed verbatim.

The compact loop guard warns after two rounds of repeated failure and pauses after three. It closes pending tool calls from that response and records the pause and fingerprints. Give a correction with `:steer` in chat or use `agent --continue --prompt <correction>`. Use the session's `--resume` command to continue under the same configuration.

Run the real-model comparison with the release executable and a fresh output directory.

```powershell
python -B -m unittest discover -s scripts -p "test_*.py"
python -B scripts/eval_session.py --exe target/release/source_harness.exe --out target/session-eval
python -B scripts/eval_session.py --exe target/release/source_harness.exe --out target/session-history --profile history --repeats 2
python -B scripts/eval_session.py --exe target/release/source_harness.exe --out target/session-feedback --profile feedback --repeats 2
python -B scripts/eval_session.py --exe target/release/source_harness.exe --out target/session-feedback-isolated --profile feedback-isolated --repeats 2
python -B scripts/eval_session.py --exe target/release/source_harness.exe --out target/session-context --profile context
```

The runner scores the final workspace against fixtures and expected values held outside the model workspace. A passing run also needs a successful model-initiated validation of the delivered revision. The report keeps known token and latency totals and counts requests with missing usage separately.

`feedback` compares direct tool receipts with automatic history enabled. `feedback-isolated` disables automatic history for all three modes and checks each archived request for receipt fields delivered through other channels. `context` runs four policy validations and a recap in one session per setting. It records actual request sizes and shared input items as the conversation grows. Provider usage records supply the token and cache counts.

The [continuous-session evaluation](docs/evidence/natural-session-2026-11-27/README.md) runs successive user tasks in one workspace and includes an interrupted-write recovery run.

## License

SourceHarness is released under the [MIT License](LICENSE).
