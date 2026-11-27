# Continuous SourceHarness sessions

The sampling-report task runs four successive user turns in one workspace and one session. The runner creates the project once, applies the specified external changes, and retains the journal and prior result references between turns. A separate run interrupts the third turn after a file replacement and resumes the same run.

Each phase has three checks. The independent checker runs the delivered CLI in a temporary directory. The runner checks that the model validated the delivered revision against the current fixture. A separate report tracks validation claims and, for the final audit, the historical references available to the model. Generated report CSVs stay in the check directory.

The Markdown follow-up uses the same runner. It repairs local links, moves a referenced file between user turns, and verifies the new destinations and preserved documents.

## Results

| Run | Scenario | Independent checks | Strict initial acceptance | Model requests | Known total tokens |
| --- | --- | ---: | ---: | ---: | ---: |
| A | Sampling report | 4/4 | 4/4 | 35 | 1,074,882 |
| B | Sampling report, repeat | 4/4 | 3/4 | 43 | 1,650,718 |
| C | Sampling report, interrupted write in turn three | 4/4 | 4/4 | 54 | 2,052,047 |
| D | Sampling report, full repeat after reviewing B | 4/4 | 4/4 | 55 | 1,986,763 |
| Markdown | Link repair and file move | 2/2 | 2/2 | 13 | 82,934 |

Known totals sum the reported usage of each model request. Usage gaps and the separately scored follow-up are recorded in the JSON.

These runs used the same Windows release executable and the `auto/deepseek-v4.1-flash-1017` Responses route at low effort. The session used text edits, versioned receipts, event history, freshness instructions, stale-result projection, workspace change notices, and an 8,192-byte history budget. The fixture changes between turns. Each acceptance receipt identifies the exact fixture and the workspace revision it checked.

In all four sampling sessions, the fourth turn began with the old WARMUP row still visible in prior input. Each run also reopened the original data and policy snapshots. The archived requests show those results reaching the model before it delivered the note. The independent check confirmed that the current program, data, and existing documentation stayed intact.

Run B passed the independent check in turn four and used verifiable historical citations. It edited the note after a successful check, then cited that check as validation of the final version. The last request presented the earlier tested revision and marked the workspace as changed. B therefore failed strict acceptance on its initial delivery. A separate follow-up validated the final version with all files unchanged. D repeated the four tasks from the same initial project with the same executable and passed all four.

In run C, operation `210.4.1` created `reporting/policy.py`. The file was present when the injected interruption stopped the process before its receipt was saved. The same run resumed and recorded one `observed_target_on_recovery` receipt for the operation. The third turn and the final audit both passed.

The [structured results](results.json) contain phase and session costs, including unsuccessful requests, requests with unknown usage, tool rejections, request sizes, cache counts, and the first and later sources of historical evidence. The [archive](trace.zip) contains the original scored runs, the follow-up, raw model requests and responses, final workspaces, the source snapshot, and checksums. This small batch is a diagnostic evaluation.

## Reproduce

Build the release executable and set the Responses endpoint and credentials for the model you want to test.

~~~powershell
cargo build --release --locked
python -B scripts/eval_natural_session.py --exe target/release/source_harness.exe --case sample-report --out target/natural-session-baseline --repeats 2
python -B scripts/eval_natural_session.py --exe target/release/source_harness.exe --case sample-report --out target/natural-session-recovery --interrupt-phase 3
python -B scripts/eval_natural_session.py --exe target/release/source_harness.exe --case markdown-links --out target/natural-session-markdown
~~~
