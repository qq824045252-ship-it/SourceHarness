# Compact validation-first follow-up

The batch-read cohort reduced Phase 1 rereads. Its logs still showed repeated sandbox copies and local checks before acceptance, especially in Phase 2. This iteration directs the model to run the configured `validate` first once the requested changes are in place and to use `exec` for discovery and focused diagnosis. [Batch-read cohort](../compact-batch-budget-2026-11-27/README.md)

The three reruns used the same published sampling-report prompts, checker, model and 16-request limit for each phase.

| Trial | Strict phases | Requests by phase | Total tokens | Cached share of input |
| --- | ---: | ---: | ---: | ---: |
| 1 | 1/2 | 13 + 16 = 29 | 353,883 | 92.7% |
| 2 | 2/2 | 11 + 12 = 23 | 258,227 | 91.3% |
| 3 | 2/2 | 9 + 9 = 18 | 156,441 | 88.8% |

Across the three trials, Phase 2 `exec` rounds fell from 17 to 9. Total requests fell from 77 to 70 and total tokens from 904,440 to 768,551. Both cohorts completed two of three trials strictly. In trial 1, the final files passed the independent check and the final model-initiated validation. The phase reached its turn limit before a natural final response. In trial 2, the model still ran an extra `exec` after the final verification of each phase.

Trial 3 finished in 18 requests at 156,441 tokens and 88.8% cached input. The full gate still requires three strict passes, at most 21 requests and 149,232 tokens per trial, and at least 90% cached input. The validation-first guidance stays on the experiment branch. All 67 adjacent request comparisons preserved the exact prefix. Usage was reported for all 70 requests.

## Data and reproduction

- [Phase outcomes and tool-round breakdowns](results.json)
- [Per-request measurements](requests.jsonl)

Prepare the published runner and fixtures using the [historical comparison instructions](../pi-comparison-2026-11-27/README.md).

~~~bash
cargo build --locked
python3 -B docs/evidence/compact-tools-2026-11-27/compare_compact.py \
  --runner-dir "$bench/pi-comparison" --exe target/debug/source_harness \
  --out "$bench/compact-validate-first" --case sample-report --repeats 3
~~~
