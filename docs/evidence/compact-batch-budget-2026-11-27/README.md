# Compact batch reads and resumed turn budgets

The baseline audit joins the published request measurements at b19be89 with the tool events and their argument blobs. The round data records tool names, error codes and argument sizes.

| Baseline trial | Phase 1 exec rounds | Phase 1 write rounds | Rounds with multiple writes |
| --- | ---: | ---: | ---: |
| 1 | 6 | 5 | 4 |
| 2 | 7 | 4 | 3 |
| 3 | 8 | 5 | 1 |

The models already grouped several writes in one response. The `exec` rounds included directory discovery, attempts to locate the external checker, and repeated local tests. All three trials tried to replace at least one existing file after inspecting it through `exec`. Those calls received `READ_REQUIRED`. Trial 1 also retried after supplying absolute sandbox paths. [Baseline round data](baseline-phase1.json) · [Prior cohort](../compact-convergence-2026-11-27/README.md)

The new CLI offers `read(paths)` and records separate revisions for each file. It uses the recorded round for budget reminders at three, two and one remaining turns. The CLI regression suite includes a process stop after an edit in round 13, a resume through the original limit, then an explicit resume after the resulting pause.

| New trial | Strict phases | Requests by phase | Total tokens | Cached share of input |
| --- | ---: | ---: | ---: | ---: |
| 1 | 2/2 | 8 + 13 = 21 | 250,476 | 90.6% |
| 2 | 1/2 | 14 + 16 = 30 | 356,686 | 93.0% |
| 3 | 2/2 | 12 + 14 = 26 | 297,278 | 92.4% |

Phase 1 passed in all three trials. The new runs used four Phase 1 `read` calls overall, three of them batches, compared with 15 before. There were zero Phase 1 `READ_REQUIRED` errors. The number of Phase 1 requests fell from 48 to 34 across the cohort.

Phase 2 continued to use repeated local tests on copies of the disposable workspace. Trial 2 exhausted its budget with a README example that the independent checker could not parse. All 74 adjacent pairs preserved the request prefix, and the provider recorded usage for all 77 requests. All three trials met the cache target. The strict, request and token targets were not met together.

The rerun used the published sampling prompt, checker and 16-request phase cap with DeepSeek V4.1 Flash at low effort. Pi's historical reference was 21 requests and 213,189 tokens. The merge target uses 21 requests, 149,232 tokens or fewer and at least 90% cached input for each of three strict passes. [Historical comparison](../pi-comparison-2026-11-27/README.md)

## Data and reproduction

- [Phase results and tool-round breakdowns](results.json)
- [Request measurements and prefix checks](requests.jsonl)

Use the published runner prepared from the historical comparison and build the tested CLI.

~~~bash
cargo build --locked
python3 -B docs/evidence/compact-tools-2026-11-27/compare_compact.py \
  --runner-dir "$bench/pi-comparison" --exe target/debug/source_harness \
  --out "$bench/compact-batch" --case sample-report --repeats 3
~~~
