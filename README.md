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

```powershell
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
cargo run -- schema
cargo run -- eval --root .
cargo run -- bench --workers 6 --strategy native
cargo run -- agent --root . --prompt "List files in src/ and summarize MAX_RESPONSE_BYTES in src/budget.rs."
cargo run -- chat --root .
cargo run -- smoke --strategy inline
cargo run -- smoke --strategy native
cargo run -- run --file README.md --prompt "Summarize this file."
```
