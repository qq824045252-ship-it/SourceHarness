# F06 tool contract baseline

This baseline records 90 live observations from the tuned SourceHarness build and 90 matching observations from [the previous build](https://github.com/qq824045252-ship-it/SourceHarness/commit/cb4e6ab6a8d63399dd6e911bcdebad58e7a7db72).

- 72 F06 observations pair `inline` and `native` at low effort on the same 36 reports. Each size, 4, 16, and 64 KiB, has 12 reports.
- 12 repeat the 64 KiB F06 cases with `native` at medium effort.
- 6 pair both injection strategies at low effort on three 900-line pagination cases backed by 64 KiB reports.

Both batches used Windows release builds, four workers, and the Magpie Responses gateway route `auto/deepseek-v4.1-flash-1017`. The runner explicitly folded the 4,096-byte reports in both batches to compare the two envelope formats. Each case and configuration was sampled once per build, in separate batches. The figures describe these observations.

## Results

| Metric | Previous | Tuned |
| --- | ---: | ---: |
| Correct requested totals or IDs, all cases | 90/90 | 90/90 |
| Tool rejections, all cases | 13 | 4 |
| Complete 900-line reads | 6/6 | 6/6 |
| Correct totals followed by zero extra calls, 64 KiB inline low | 1/12 | 4/12 |
| Mean calls after sufficient evidence, 64 KiB inline low | 4.33 | 0.75 |
| Mean total tokens, 64 KiB inline low | 11,525 | 4,872 |
| Median total tokens, 64 KiB inline low | 8,822 | 4,434 |

The count score measures the requested total and tracks unsolicited card IDs separately. The stop score requires a correct final answer and zero executed calls after the first verified sufficient evidence, including calls already requested in the same batch. The pagination score checks each returned line against its original document and requires full coverage of lines 1–900 and the correct requested IDs.

All four remaining rejections occurred with `native`. Two used unsupported regex lookaround, one passed response fields as arguments, and one passed a report name as `result_ref`.

## Evidence

- [Comparison report](comparison.json) contains the matched summaries, individual tuned outcomes, error arguments, and build fingerprints.
- [Fixtures and traces](fixtures-and-traces.zip) contains the exact inputs, both batches of raw JSONL, the test runner and analysis scripts, and `SHA256SUMS` for the archive entries. It also includes the tuned CLI smoke result and five live workspace evaluation results.
