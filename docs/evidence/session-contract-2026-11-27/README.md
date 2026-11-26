# Session contract review

These runs extend the review of `65f53fa`. They used Windows release executables, the Magpie Responses gateway, and isolated fixture workspaces. [Batch and executable identities](results.json).

| Final release check | Result |
| --- | --- |
| DeepSeek low, eight acceptance scenarios repeated twice | 16/16 |
| DeepSeek low, five history configurations repeated twice | 10/10 |
| DeepSeek low, exact text / range / unified patch edit | 2/2, 2/2, 0/2 |
| Qwen3.8 Flash low, eight acceptance scenarios | 8/8 |
| DeepSeek low, old-to-new executable continuation with user correction | 2/2 |
| Legacy workspace live regression | 5/5 |
| Rust tests, Python tests | 30/30, 4/4 |

The Qwen exploration exposed an old-report association failure. After a new check completed, the next request replaced the earlier check's comparison with the current one's comparison. The final release keeps previous-run and current-run checks separately. The outgoing provenance on every request of the three final association trials retained the earlier comparison.

An intermediate 64 KiB recovery hit the turn limit after repeated empty optional revision arguments for `write`. A separate Qwen run hit the limit after several `research` argument rejections, even though its final files and validation were correct. The final release clarifies the field scopes and treats placeholder empty values as omission where they carry no precondition. The archive includes both earlier failures.

## What the runner checks

The runner keeps the initial bytes, expected changes, and expected evidence fields in its own process. It compares the delivered workspace against those expectations. A passing run also needs a successful model-initiated check of the final workspace revision. The wrong-value and mutable-oracle reproductions are covered by local tests.

- **Policy update and file creation.** Update only the primary section and report the actual policy values.
- **64 KiB file.** Find and change the primary section in the middle of a 65,536-byte file. The runner checks the remaining bytes.
- **Interrupted writes.** Stop after file replacement, before the receipt. Check normal recovery and recovery after a maintainer changes the same file.
- **Executable upgrade.** Interrupt the old executable, change the policy, and provide a correction to the new executable using `--continue`. Check the linked run, the actual edited file, and the final validation.
- **Two-file CLI repair.** Start with a failing Python CLI. Repair its public function and example documentation, keep the caller, and validate the actual program output.
- **Evidence provenance.** Leave the workspace intact and change the edit schema and validation configuration. Check the old and new conditions in the actual outgoing requests and in the model's report.

## Experiment boundaries

The history experiment starts each arm from a copy of one verified checkpoint. The runner compares the actual facts sent in the first request for the event-list and grouped representations. The freshness instruction and old-content projection each have a separate arm. Later requests follow each arm's own execution. Auxiliary size is measured in UTF-8 bytes. Model token usage covers the whole request, and missing usage is counted separately for every attempt.

The two repetitions per DeepSeek condition probe sampling variation in these fixtures. The Qwen run adds a second model for the selected acceptance tasks. These sample sizes support reproducing contract failures and checking the tested paths. They leave the relative performance of the history strategies open.

## Evidence files

- [Results](results.json) lists the current release SHA-256, source file digests, per-case counts and known costs, and the observed failures.
- [Fixtures and traces](fixtures-and-traces.zip) contains the source snapshot for the final build, summaries, workspace inputs and deliverables, and raw session journals and referenced blobs. Earlier batches and failed attempts are included. Each archived file is listed in `SHA256SUMS`.
