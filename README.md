# SourceHarness

SourceHarness is a cross-platform (Windows, Linux, macOS) Responses API coding agent kernel and resumable workspace session runner written in Rust.

## Architecture

- **Compact Tool Kernel (`src/session/`)**:
  - `read`: Read numbered UTF-8 text (6 KiB pages with line/byte/EOL/`rN` ref headers), list directories (`path="."`), search text/regex (`pattern`, `regex=true`) across files/directories/`result_ref`, batch up to 8 live files (`paths`), page stored outputs (`result_ref`, `start_line`/`end_line`/`line_count`/`start_byte`/`end_byte`), or inspect the causal session timeline (`history=true`, or `history=true` + `path` incl. run-start snapshots), automatically folding large responses over 8 KiB via `src/budget.rs`.
  - `edit` & `write`: Workspace-contained mutations with implicit optimistic revision locking (`observed_revision`), auto-CRLF preservation in `edit`, trailing-newline drop warnings, `REVISION_CONFLICT` unified diffs (`diffy`), `TEXT_NOT_FOUND` anchor diagnostics, and `NO_OP_EDIT` protection.
  - `exec`: Cross-platform shell execution in the workspace (`/bin/sh -c` on Unix, `cmd.exe /D /S /C` on Windows) with 32 KiB command limit, optional `stdin` up to 64 KiB (avoiding temporary helper scripts), 1–300s timeout (default 60s) with full process-tree cleanup on timeout, 512 KiB stream capture, >2 KiB output folding, and `before_workspace_revision`/`workspace_revision` tracking.
  - `validate`: Runs external configured checkers (`--checks <file>`) against captured workspace snapshots with process-group cleanup on timeout, recording program and declared input revisions and surfacing changed files since the last checkpoint plus >2 KiB conditional output refs on failure.
  - `rollback`: Reverts files touched by `edit`, `write`, or `exec` back to the latest passing `validate` snapshot, run start, or an explicit `to_event`, with optional `paths` scoping and `dry_run=true` diff preview while tolerating unrelated external files.
- **Durable WAL & Guard (`journal.rs`, `inbox.rs`, `guard.rs`)**:
  - Append-only JSONL journal (`events.jsonl`) and SHA-256 content-addressed `blobs/` store supporting crash recovery (`--resume`, `--continue`) and interactive chat steering (`:steer`).
  - Loop guard detecting `REPEATED_FAILURE`, `EDIT_ERROR_STREAK`, `EXEC_WITHOUT_PROGRESS`, and `VALIDATION_OSCILLATION` while preserving static `instructions`, `tools`, and append-only `input` prefixes for prompt caching.
- **Responses Transport (`src/transport/`)**:
  - Stateless SSE client over the OpenAI Responses API with `reasoning`, `function_call`, and `function_call_output` replay and incremental terminal streaming.

## Gateway Configuration

```powershell
$env:OPENAI_BASE_URL = "http://127.0.0.1:3425/v1"
$env:OPENAI_API_KEY  = "magpie"
$env:OPENAI_MODEL    = "auto/deepseek-v4.1-flash-1017"
```

## CLI Subcommands

```powershell
source_harness agent --root <dir> [--session <name>] (--prompt <text> | --resume | --continue [--prompt <correction>]) [--checks <file>] [--model <m>] [--effort <e>] [--max-turns <n>]
source_harness chat --root <dir> [--session <name>] [--checks <file>] [--model <m>] [--effort <e>] [--max-turns <n>]
source_harness history --root <dir> --session <name> [--ref <result_ref> | --event <seq>]
source_harness fork --root <dir> --session <name> --revision <workspace_revision> --to <new-directory>
```

- Interactive `chat` accepts `:steer <instruction>` to redirect an active run, `:resume` to resume a paused run, and `:quit`/`:exit` to leave.
- Pass `--checks <file>` (placed outside the workspace) mapping check names to `{ "program", "args", "description", "artifacts", "inputs", "timeout_seconds" }`.

## Verification

```powershell
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
python scripts/eval_compact_adversarial.py --exe <source_harness_exe> --out <dir> --verify
python scripts/eval_compact_adversarial.py --exe <source_harness_exe> --out <dir> --session-check
```

## License

Released under the [MIT License](LICENSE).
