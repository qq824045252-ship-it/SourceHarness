# Compact sampling-report convergence

This experiment repeats the published two-phase sampling-report fixture three times in separate sessions. It uses the same prompts, independent checker, DeepSeek V4.1 Flash at low effort, OpenCode Go Responses endpoint and 16-request limit per phase. The historical Pi 1.6.1 reference completed both phases in 21 requests, using 213,189 total tokens and 90.6% cached input. [Historical paired benchmark](../pi-comparison-2026-11-27/README.md)

A phase passes when the independent checker accepts the final files, a model-initiated validation covers that exact revision and the model ends naturally. The merge criteria are three complete passing trials, each within 21 requests and 70% of Pi's token total, with at least 90% cached input. Lower token totals also satisfy the cost target.

The compact profile reuses the latest live observation across tasks. The CLI regression tests exercise cross-task edits, an external-change conflict with diff, a confirmed deletion followed by safe creation, and rollback invalidation. The budget fixture checks that the final two reminders appear only on the newest tool result, preserve read pagination, and leave every previous input item unchanged.

## Results

| Trial | Strict phases | Model requests | Total tokens | Cached share of input |
| --- | ---: | ---: | ---: | ---: |
| 1 | 2/2 | 16 + 10 = 26 | 306,521 | 91.7% |
| 2 | 2/2 | 16 + 8 = 24 | 308,701 | 91.3% |
| 3 | 0/2 | 16 = 16 | 84,945 | 87.9% |

Two of three trials passed both phases. Trial 3 reached the limit in phase one. Its final files passed the independent checker, and there was no model-initiated validation. The runner did not start its second phase.

Trials 1 and 2 ran their final phase-one validation on request 15 and finished on request 16. Both ended phase two immediately after verifying the final revision, in 10 and 8 requests respectively. In trial 2, five file replacements in phase two reused observations from phase one. The model made no phase-two `read` calls in that trial.

All 63 adjacent request pairs preserved the preceding input and identical instructions and tools. Each first phase received one new budget reminder on request 15 and one on request 16. The provider reported usage for all 66 requests.

The completed trials met the cache target. Both exceeded the 21-request and 149,232-token ceilings. The full merge gate remains unmet.

## Evidence

- [Per-trial and per-phase results](results.json)
- [Per-request usage and prefix checks](requests.jsonl)
- [Benchmark runner](../compact-tools-2026-11-27/compare_compact.py)

Prepare the published runner and its fixtures using the historical benchmark instructions. The CLI tests require a working local Bubblewrap backend for the sandbox case.

~~~bash
cargo build --locked
python3 -B docs/evidence/compact-tools-2026-11-27/compare_compact.py \
  --runner-dir "$bench/pi-comparison" --exe target/debug/source_harness \
  --out "$bench/compact-convergence" --case sample-report --repeats 3
cargo test --locked --all-targets -- --include-ignored --skip run_test_checker
cargo clippy --locked --all-targets -- -D warnings
python3 -B -m unittest discover -s scripts -p 'test_*.py'
~~~
