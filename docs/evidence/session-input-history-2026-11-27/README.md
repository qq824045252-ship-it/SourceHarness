# Durable chat input and receipt feedback

The chat tests use a local fixture server so the test can stop the CLI after it confirms input and before the model request returns. After reopening the session, the pending run receives its correction and the queued prompt starts once. Separate tests cover the model round limit and the exact positions of corrections in a linked continuation.

## Receipt comparison

DeepSeek V4.1 Flash at low effort ran three tasks twice per mode through the Magpie Responses gateway. The six matched blocks used the same initial files and first request. The runner checked the final files, fresh validation, and the expected recovery path when the task included an interruption.

| Direct receipt | Passed | Mean total tokens | Reads of previously modified files |
| --- | ---: | ---: | ---: |
| Summary | 5/6 | 97,379 | 9 |
| Summary and diff | 4/6 | 112,530 | 8 |
| Versioned receipt | 5/6 | 100,194 | 1 |

Two failures reached the model round limit in the nested-delivery task. In two recovery trials, the model wrote the evidence file before the requested configuration change. Both later delivered the expected files and obtained passing validation, but neither exercised the intended recovery path.

The runner disabled automatic history, freshness instructions, stale projection, and workspace change notices for all three modes. It compared every direct receipt against the archived request. It also scanned each request for structured receipt fields and exact diff text in instructions, other inputs, and tool results. In two trials, the model retrieved omitted version and reference fields with `research(scope=history)`. Both trials already failed their task checks. Normal workspace reads remained available to the model.

The earlier experiment kept automatic history enabled. Re-auditing its archived requests found omitted fields in the auxiliary instructions in all six summary trials and all six diff trials. The new sample shows fewer reads with versioned receipts, while token totals vary widely between trials. Neither sample establishes a default receipt mode on its own. [Earlier experiment](../session-feedback-2026-11-27/README.md)

## Context over several user turns

Two sessions validated four successive policies in a 64 KiB configuration file. Both ended with a correct recap of all four policies and their values. Both used automatic facts and workspace change notices, starting from identical files and model input. One enabled the existing projection that removes stale portions of earlier tool outputs.

| Stale projection | Requests | Total tokens | Largest request in bytes | Reported cached input tokens |
| --- | ---: | ---: | ---: | ---: |
| Off | 24 | 314,272 | 78,577 | 70,656 |
| On | 20 | 182,116 | 52,575 | 34,048 |

Each row is one session. The request paths diverged as soon as the first policy validation, so the difference includes natural model variation. The replay still grows in both runs. The archive records each request's size, input items, and shared item prefix, alongside provider token counts.

[Build identities and totals](results.json) · [Requests, responses, fixtures, and checksums](fixtures-and-traces.zip)
