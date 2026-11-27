# Compact loop protection

The scripted provider drives the normal compact CLI through Responses requests with a 50-turn limit. It keeps sending the prescribed calls until the guard or turn limit stops the run. The six original adverse cases exercise failed evidence across model rounds. Positive controls require natural completion, and the repair controls check file bytes or the required validation transitions.

| Scripted case | f450b00 | Current |
| --- | ---: | ---: |
| Changing workspace, same failure with timestamps and line numbers | Turn limit at 50 | Guard at 3 |
| Unchanged workspace, noisy validation | Guard at 3 | Guard at 3 |
| Alternating incompatible checks | Turn limit at 50 | Guard at 5 |
| Empty find with different arguments | Turn limit at 50 | Guard at 4 |
| Failed file probes with different arguments | Turn limit at 50 | Guard at 4 |
| CRLF mismatch, bad patch and missing target | Guard at 7 | Guard at 3 |
| Validation diagnostics advance, then pass | Finishes at 5 | Finishes at 5 |
| Silent tests and nonempty find lookups | Finishes at 13 | Finishes at 13 |
| Failed edits followed by a correct CRLF edit | Finishes at 3 | Finishes at 3 |
| Six successful quiet grep probes | | Finishes at 7 |
| Incompatible checks named in the request | | Guard at 5 |
| Sandbox write, failing validation, live edit, passing validation | | Finishes at 4 |
| Real rollback revisits a failed workspace | | Guard at 5 |
| Empty rollback between changing failed exec calls | | Guard at 4 |

The compact loop guard compares short validation diagnostics after masking labeled timestamps, line and column numbers, and process IDs. The original streams remain in the journal. A pass on another revision leaves earlier failed revisions available for cycle detection. The shell heuristic covers nonzero exits, timeouts and empty results from simple find calls. Shell pipelines or successful commands that produce output can still repeat without tripping it. All 54 adjacent model requests in the current scripted suite preserved their exact input prefix, instructions and tool declarations. [Baseline results](scripted-f450b00.json) · [Current results](scripted-current.json)

## DeepSeek V4.1 Flash

The initial live cohort used low effort with the OpenCode Go Responses route. The model was free to stop or continue. Eight of nine cases returned normally. The A/B case paused at round 22 after warnings at rounds 7, 11 and 13. Both noisy-validation cases stopped after reporting the failed check. The model said it repaired the CRLF file in the edit-recovery case, but the independent check found the file still contained the original bytes. The current build appends a short discard notice to new exec results. [Initial live results](deepseek-initial.json) · [Provider request usage](deepseek-initial-requests.jsonl)

The follow-up added a named-check conflict, quiet successful probes and a sandbox-to-live edit. The conflict hit the guard at round 16. The other three cases finished naturally, including the exact CRLF prompt that previously reported a change without delivering it. Both CRLF changes passed the independent byte comparison. The sandbox-to-live case also had an actual failing check before the edit and a passing check after it. This is one repeat of the original CRLF prompt. [Follow-up live results](deepseek-follow-up.json) · [Provider request usage](deepseek-follow-up-requests.jsonl)

The real-rollback case alternated two failed workspaces using the actual rollback tool. Two successful rollback receipts preceded a guard pause at round 5. A separate case returned an empty rollback before each changing failed lookup. Before the empty-rollback fix it reached the 50-turn limit. The current guard counts the unchanged workspace across all four empty rollbacks and pauses at round 4. [Rollback cycle](scripted-rollback.json) · [Empty rollback before the fix](scripted-empty-rollback-before.json)

## Sampling runtime and cost

Trial 3 on f450b00 spent 87,514 tokens in phase 1 and 340,070 in phase 2. Across 32 requests the provider reported 427,584 total tokens and 24,913 uncached input tokens. The matched historical Pi trial used 21 requests, 213,189 total tokens and 18,980 uncached input tokens. Phase 2 repeated 336,985 UTF-8 bytes of write arguments across requests. That figure measures serialized tool arguments in the replayed prefix. The provider reports token totals across the complete requests. [Trial 3 per-request tool and replay measurements](sampling-f450b00-trial3.json) · [Pi comparison](../pi-comparison-2026-11-27/README.md)

The first four phase-1 shell calls inspected the project or looked for validation configuration. Two later calls hit the disposable boundary. Phase 1 round 9 could not access the CSV made in round 6. Phase 2 round 7 could not enter the temporary directory created in round 6. Phase 2 also spent rounds on local verification and a documentation correction after the configured check failed. The trace showed no live deliverable first authored via exec and then rewritten with a mutation tool. Exec stays disposable. [Trial 3 per-request tool and replay measurements](sampling-f450b00-trial3.json)

## Continuous-session controls

A scripted four-phase session observed a file in phase 1. Phases 2, 3 and 4 each applied an external edit before sending a stale write. All three stale writes produced a revision conflict and diff, and the external bytes survived. Phase 3 also killed the CLI with SIGKILL after a separate file replacement and before its receipt. Resume recorded exactly one observed_target_on_recovery outcome. Phase 4 could still read both the current file and the saved phase-1 contents. The scripted caller copied the historical reference from the journal to exercise retrieval. [Continuous-session results](scripted-continuity.json)

A separate check inspected a path only through sandbox stdout. After an external edit, a direct write based on that output succeeded and replaced the external bytes. The observation lock currently records live path reads and completed writes. An exec snapshot alone does not record that path as observed. [Exec-only stale write](scripted-exec-only-stale-write.json)

The first live four-phase attempt passed phase 1 and reached the evaluation runner's 240-second process timeout in phase 2. Phase 2 still had an invalid README command and lacked final-revision validation. The timeout occurs outside SourceHarness. The compact evaluation runner now defaults to 600 seconds per CLI execution, with the CLI and bridge limits still at 50 model turns. [Timed-out attempt](sampling-initial-timeout.json)

The real Markdown session passed both phases, including independent acceptance and final-revision validation. Phase 2 externally renamed the schema reference and injected SIGKILL after the first model file replacement. Resume recorded a single recovered receipt and completed normally. The session used 17 requests and 67,712 total tokens. All 16 adjacent pairs kept the exact instructions, tools and input prefix. [Real Markdown continuity](markdown-natural-sigkill.json)

The resumed Sampling evaluation completed all four phases with the original CLI and journal. Phase 2 recovered from two gateway 502 responses. Phase 3 recorded one recovered file-replacement receipt after SIGKILL, and two attempted README updates produced revision conflicts and diffs before the model reread the file. All 66 adjacent requests preserved the exact prefix.

| Sampling phase | Requests | Known total tokens | Strict outcome |
| --- | ---: | ---: | --- |
| 1 | 14 | 79,488 | Passed |
| 2 | 21 | 408,431 | Passed after transport recovery |
| 3 | 20 | 699,142 | Passed after SIGKILL recovery |
| 4 | 12 | 612,771 | Historical citation check failed |
| Total | 67 | 1,799,832 | 3 of 4 phases passed |

Usage was available for 65 requests. The provider reused 95.5% of the metered input tokens. Phase 4 passed independent content and final-revision validation. The two source digests quoted by the model matched the exact bytes in the first-release manifest. The model cited reconstructed MD5 digests, and neither source had a journal snapshot reference or event citation in the delivered note. Both original files existed in the phase-1 workspace archive. The compact model view lacked a way to discover those archived paths. The strict historical citation check remains open. [Four-phase Sampling results](sampling-natural-results.json) · [Provider request usage](sampling-natural-requests.jsonl)

## Reproduction

Build the CLI with Cargo using the lockfile and use fresh output directories. The live test uses the comparison runner in [reproduce.zip](../pi-comparison-2026-11-27/reproduce.zip) and reads a test credential without echoing it.

~~~bash
python3 -B scripts/eval_compact_adversarial.py \
  --exe target/debug/source_harness --out "$bench/compact-guard" --verify
python3 -B scripts/eval_compact_adversarial.py \
  --exe target/debug/source_harness --out "$bench/compact-continuity" --session-check
python3 -B scripts/eval_compact_adversarial.py \
  --exe target/debug/source_harness --out "$bench/compact-live" \
  --live-runner "$bench/pi-comparison"
python3 -B scripts/eval_natural_session.py \
  --exe target/debug/source_harness --out "$bench/compact-natural" \
  --tool-profile compact --interrupt-phase 3 --sigkill \
  --live-runner "$bench/pi-comparison"
~~~

An evaluation interrupted by a network error or wall-clock timeout can resume with the same output directory and the resume-output flag. The runner preserves the original gateway address and verifier inputs, records the failed execution, and keeps the CLI's turn limit and guard pauses visible.

Validation also included 66 Rust tests, 14 Python tests and Clippy with warnings denied. The Rust invocation skipped the subprocess-only checker entry point and included ignored suites.
