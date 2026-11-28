# DeepSeek V4.1 Flash on the 200-task Phase 1-4 evaluation

> This is the early baseline at `b8ee49e`, kept for comparison. The definitive run, at `b8bc33e`,
> is in [docs/evidence/phase14-200-b8bc33e-2026-11-28/](../phase14-200-b8bc33e-2026-11-28/README.md).

This run evaluates two tracks on the `v2-clean` branch. Track A replays 100 unseen instances of the existing 1,000-task compact corpus (`variant 02–03`, all 50 shapes). Track B runs 100 new tasks written for the Phase 1–4 mechanisms: atomic multi-file edits with arbitrary-node rollback, single-turn parallel `validate` behind a write barrier, timeline branching with prefix-cache reuse, and isolated workers behind a merge gate. Track B also has a scripted driver that replays fixed tool calls through a local SSE gateway, which measures the mechanisms without a model.

The model is `auto/deepseek-v4.1-flash-1017` served by a loopback Responses gateway with low effort. Every tool call, workspace mutation and external check is real; only the scripted driver substitutes the model turns. Binary, checker and corpus digests are recorded in each `settings.json` and `manifest.json` in the result directories, and per-case data is published below.

A strict pass for Track A requires the expected answer or workspace behavior, final-revision validation where requested, and a valid stop. A strict pass for Track B requires every action in the case script to meet its own contract: the expected end state, no rejected-merge pollution, byte-identical branch prefixes, and the declared mechanism invariants.

## Results

| Track | Passes |
| --- | ---: |
| A · unseen compact tasks | 95/100 |
| B · Phase 1-4 tasks, scripted driver | 100/100 |
| B · Phase 1-4 tasks, live model | 77/100 |

Track A recorded 612 requests, 1,373,952 metered tokens and 88.2% cached input. Requests per case were 6 at the median and 10 at the 95th percentile; all 512 adjacent requests preserved the exact input prefix and the loop guard never paused. Track B (live) recorded 1,157 requests and 2,523,190 metered tokens, 10 requests per case at the median and 22 at the 95th percentile, with all 584 adjacent request pairs exact. The merge gate accepted 16 merges, rejected 16 on validation and 5 on conflicts.

| Track B family | Live passes | Scripted passes |
| --- | ---: | ---: |
| Atomic multi-file edits and rollback | 25/25 | 25/25 |
| Parallel `validate` and write barrier | 13/25 | 25/25 |
| Timeline branch and rewind | 24/25 | 25/25 |
| Worker isolation and merge gate | 15/25 | 25/25 |

The three Track A failures that are not the known guard contract are two answers that did not match and one unverifiable historical citation; the two `guard.must-delete` cases assume the CLI cannot delete files, which `exec` can. Track A therefore matches the earlier 94/100 on `variant 00–01`, which is the intended evidence that the tool changes did not overfit the older seeds.

Track B isolates three causes. First, the twelve write-barrier cases all ran `mypy` or `ruff` through `exec` instead of the configured `validate` checks and left `.mypy_cache/` and `.ruff_cache/` in the workspace; only two of the twelve also called `validate`. Second, all eight write-skew cases were rejected by the merge gate as designed with the mainline left byte-identical, but the worker then could not see the new `compute` signature because it still sat on its original `base_revision`, so the second merge failed too. Third, one branch case and one conflict case failed on evaluation strictness rather than mechanism: the model reworded the target file, and a reworded mainline edit removed the same-line conflict the case depends on.

## Reproduce

~~~bash
cargo build --locked
export OPENAI_API_KEY=...   # loopback gateway credential

python -B scripts/phase14_benchmark.py --exe target/debug/source_harness \
  --driver live --base-url http://127.0.0.1:3425/v1 \
  --model auto/deepseek-v4.1-flash-1017 --out /path/to/fresh-results

python -B scripts/phase14_benchmark.py --exe target/debug/source_harness \
  --out /path/to/fresh-scripted-results

python -B scripts/eval_qwen_benchmark.py --exe target/debug/source_harness \
  --variant-range 2-3 --workers 4 --model auto/deepseek-v4.1-flash-1017 \
  --out /path/to/fresh-trackA-results
~~~

The scripted driver needs no credential and no network. Each result directory refuses to mix drivers, models or binaries, so use a fresh directory per configuration. The compressed data can be read with gzip or zcat.

[Track A summary](summary-trackA.json) · [Track A cases](cases-trackA.jsonl.gz) · [Track B live summary](summary-live-b.json) · [Track B live cases](cases-live-b.jsonl.gz) · [Track B scripted summary](summary-scripted.json) · [Track B scripted cases](cases-scripted.jsonl.gz) · [Evaluation plan](../../phase14-200-task-evaluation-plan.md)
