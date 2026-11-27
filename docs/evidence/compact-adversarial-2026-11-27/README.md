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

The compact loop guard compares short validation diagnostics after masking labeled timestamps, line and column numbers, and process IDs. The original streams remain in the journal. A pass on another revision leaves earlier failed revisions available for cycle detection. The shell heuristic covers nonzero exits, timeouts and empty results from simple find calls. Shell pipelines or successful commands that produce output can still repeat without tripping it. All 47 adjacent model requests in the current scripted suite preserved their exact input prefix, instructions and tool declarations. [Baseline results](scripted-f450b00.json) · [Current results](scripted-current.json)

## DeepSeek V4.1 Flash

The initial live cohort used low effort with the OpenCode Go Responses route. The model was free to stop or continue. Eight of nine cases returned normally. The A/B case paused at round 22 after warnings at rounds 7, 11 and 13. Both noisy-validation cases stopped after reporting the failed check. The model said it repaired the CRLF file in the edit-recovery case, but the independent check found the file still contained the original bytes. The current build appends a short discard notice to new exec results. [Initial live results](deepseek-initial.json) · [Provider request usage](deepseek-initial-requests.jsonl)

The follow-up added a named-check conflict, quiet successful probes and a sandbox-to-live edit. The conflict hit the guard at round 16. The other three cases finished naturally, including the exact CRLF prompt that previously reported a change without delivering it. Both CRLF changes passed the independent byte comparison. The sandbox-to-live case also had an actual failing check before the edit and a passing check after it. This is one repeat of the original CRLF prompt. [Follow-up live results](deepseek-follow-up.json) · [Provider request usage](deepseek-follow-up-requests.jsonl)

A thirteenth scripted case alternated two failed workspaces using the actual rollback tool. Two successful rollback receipts preceded a guard pause at round 5. [Rollback cycle](scripted-rollback.json)

## Sampling runtime and cost

Trial 3 on f450b00 spent 87,514 tokens in phase 1 and 340,070 in phase 2. Across 32 requests the provider reported 427,584 total tokens and 24,913 uncached input tokens. The matched historical Pi trial used 21 requests, 213,189 total tokens and 18,980 uncached input tokens. Phase 2 repeated 336,985 UTF-8 bytes of write arguments across requests. That figure measures serialized tool arguments in the replayed prefix. The provider reports token totals across the complete requests. [Trial 3 per-request tool and replay measurements](sampling-f450b00-trial3.json) · [Pi comparison](../pi-comparison-2026-11-27/README.md)

The first four phase-1 shell calls inspected the project or looked for validation configuration. Two later calls hit the disposable boundary. Phase 1 round 9 could not access the CSV made in round 6. Phase 2 round 7 could not enter the temporary directory created in round 6. Phase 2 also spent rounds on local verification and a documentation correction after the configured check failed. The trace showed no live deliverable first authored via exec and then rewritten with a mutation tool. Exec stays disposable. [Trial 3 per-request tool and replay measurements](sampling-f450b00-trial3.json)

## Continuous-session controls

A scripted four-phase session observed a file in phase 1. Phases 2, 3 and 4 each applied an external edit before sending a stale write. All three stale writes produced a revision conflict and diff, and the external bytes survived. Phase 3 also killed the CLI with SIGKILL after a separate file replacement and before its receipt. Resume recorded exactly one observed_target_on_recovery outcome. Phase 4 could still read both the current file and the saved phase-1 contents. The scripted caller copied the historical reference from the journal to exercise retrieval. [Continuous-session results](scripted-continuity.json)

The first live four-phase attempt passed phase 1 and reached the evaluation runner's 240-second process timeout in phase 2. Phase 2 still had an invalid README command and lacked final-revision validation. The timeout occurs outside SourceHarness. The compact evaluation runner now defaults to 600 seconds per CLI execution, with the CLI and bridge limits still at 50 model turns. [Timed-out attempt](sampling-initial-timeout.json)

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
~~~

Validation also included 66 Rust tests, 13 Python tests and Clippy with warnings denied. The Rust invocation skipped the subprocess-only checker entry point and included ignored suites.
