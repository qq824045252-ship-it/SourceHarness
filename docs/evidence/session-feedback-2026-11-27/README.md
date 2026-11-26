# Session feedback and runtime follow-up

The runs used one Windows release build based on `2bd93f3`, isolated workspaces, and the Magpie Responses gateway. [Build identity and results](results.json).

## Tool and runtime checks

| Scenario | Result |
| --- | --- |
| Search a saved report whose last 172 of 300 hits match a new query | DeepSeek and Qwen each returned 172 from the full report reference |
| Change the external oracle at the same path | DeepSeek received the input digest change while the command, tools, and workspace stayed the same. The new check failed as expected |
| Change the tools and check configuration with workspace files unchanged | DeepSeek retained the previous-run comparison in subsequent requests and ran the new check |
| Send a correction during an active chat run | The next request included the correction in the same run. An unstarted research call was skipped. The workspace was unchanged |
| Compare exact text, line ranges, and unified patches | DeepSeek passed 2/2 for each mode. Patch still produced two tool rejections |
| Local regression | 42 Rust tests and 8 Python tests passed. Clippy was clean |

The report fixture distinguishes 300 hits from the line count of their serialized JSON. Qwen used the correct 172 count but described the serialized report's total line count imprecisely in its explanation.

The patch review found two concrete failures in the earlier archive. Two malformed strings parsed into zero hunks and yielded an unchanged-success receipt. Valid-looking hunks with missing final patch delimiters consumed an unrelated blank separator. The regressions exercise both cases. The final patch comparison is a small end-to-end retest with other runtime changes present.

## Same-model receipt comparison

DeepSeek low completed three tasks twice in each mode. Each matched block used the same initial files, prompt, and tool declarations. The tasks covered nested delivery, CLI and documentation repair, and recovery after an external edit. The scorer checks final files and fresh validation. For the CLI fixture, it runs the application and checks the public function on several inputs.

| Success receipt | Passed | Mean total tokens | Read calls on previously modified files |
| --- | ---: | ---: | ---: |
| Summary | 6/6 | 67,228 | 12 |
| Summary plus diff | 6/6 | 48,406 | 4 |
| Versioned receipt plus diff and references | 6/6 | 50,378 | 4 |

The runner verified the actual outgoing tool payloads. It also preserved full original receipts for each mode. These counts describe six observations per arm. The archive retains cost ranges, cache telemetry, failed checks, interrupted calls, and tool rejects. The runs held the placement of instructions constant.

[Summary and source hashes](results.json) · [Fixtures, requests, journals, and checksums](fixtures-and-traces.zip)
