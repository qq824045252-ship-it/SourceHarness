# Repair from an archived snapshot

One Windows release session with DeepSeek V4.1 Flash at low effort completed the repair. The runner reused the prior paging answer with prose outside its JSON. It also seeded a controlled mismatch on line 117. The first model run archived the old report, and the runner then advanced the workspace report to a different revision.

| Journal event | Actual result |
| --- | --- |
| 44 | Acceptance failed. The checker identified extra text around the object and a mismatch in `line_117`. It confirmed that `line_44` and `line_179` were already correct. |
| 46 | The next model request contained the failed check and its field names. |
| 49 | The model used `read` with the archived reference `r10` to retrieve only line 117. |
| 50 | The next model request contained the exact archived line. |
| 56 | The model rewrote the submission once. The two correct values were preserved. |
| 63 | Acceptance passed. The tested workspace revision matches the final files. |

The expected values lived outside the workspace. The check returned the failing field without the expected value. The repair requests before the archived read did not contain the exact corrected line. The model made six tool calls in the repair run, including two acceptance checks. The single historical read returned one line and the model made no reads from the live report path. The independent grader confirmed that the live report stayed unchanged and all three submitted values were correct.

The scenario uses an explicit request to validate the draft and final submission. It demonstrates one repair using existing tools and prior session evidence.

```powershell
python -B scripts/eval_repair_loop.py --exe target/release/source_harness.exe --archive docs/evidence/search-scope-2026-11-27/live-traces.zip --out target/timeline-repair
```

[Run summary](summary.json) · [Requests, responses, and checked files](trace.zip)
