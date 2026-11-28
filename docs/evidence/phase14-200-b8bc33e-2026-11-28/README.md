# DeepSeek V4.1 Flash on the 200-task Phase 1-4 evaluation (definitive run)

This is the definitive run of the two-track 200-task evaluation on `v2-clean`, recorded at commit
`b8bc33e73e8b146b15c061ff855f102b73aeb5f8`. All three configurations below ran the **same binary**
(`cargo build --locked`, 7,435,264 bytes, `sha256 = 7aae8231490267c03c5ece3fb5aff14fa0a5ba7f0f25572af3638465cc9e4628`),
each into its own fresh result directory:

- **Track A**: 100 unseen instances of the existing 1,000-task compact corpus (`variant 02–03`, all 50 shapes), live model calls, 4 workers.
- **Track B live**: 100 tasks written for the Phase 1–4 mechanisms, live model calls.
- **Track B scripted**: the same 100 tasks with the model turns replaced by fixed tool calls through a local SSE gateway, so only the mechanisms are measured.

The model is `auto/deepseek-v4.1-flash-1017` (gateway label: *DeepSeek V4.1 Flash 1017 · routing group*) on a loopback Responses gateway. The routing group may land on different upstream backends between requests; this is the same configuration the 2026-11-28 baseline used, so the comparison is like-for-like. Every tool call, workspace mutation and external check is real; only the scripted driver substitutes the model turns. Per-case data, the summary each archive entry was built from, and the binary digest are bound together in `provenance.json`.

## Results

| Configuration | Passes | 2026-11-28 baseline (`b8ee49e`) |
| --- | ---: | ---: |
| A · unseen compact tasks (live) | **96/100** | 95/100 |
| B · Phase 1–4 tasks (live) | **95/100** | 77/100 |
| B · Phase 1–4 tasks (scripted) | **100/100** | 100/100 |

The live Track B gain is the subject of the change list below: worker rebase closed the write-skew
family, naming the configured checks closed the write barrier, and the prompt clarifications removed
the remaining read-level ambiguity. Track B scripted stays at 100/100, which is the evidence that the
mechanism wiring itself did not change behavior.

### Track A · unseen compact tasks

| Metric | This run | Baseline |
| --- | ---: | ---: |
| Evaluated / passed | 100 / **96** | 100 / 95 |
| Requests (`p50 / p95`) | 613 (`6 / 11`) | 612 (`6 / 10`) |
| `seconds_p95` | 19.40s | 15.98s |
| Metered tokens | 1,372,651 (input 1,304,846 / output 67,805 / reasoning 23,935) | 1,373,952 |
| Cached input | 1,154,944 (**88.5%**) | 1,150,976 (88.2%) |
| Static prefix pairs | **513 / 513 (100%)** | 512 / 512 |
| Loop Guard pauses | 0 | 0 |
| Tools | `read 270`, `exec 115`, `validate 99`, `edit 41`, `write 34`, `rollback 6` | — |

| Category | Passes |
| --- | ---: |
| guard | 7/10 |
| inspect | 20/20 |
| patch | 20/20 |
| repair | 20/20 |
| report | 16/16 |
| workflow | 13/14 |

The four failures: `guard.must-delete.02/.03` deleted a protected file through `exec` (the case
assumes no delete capability, and `exec` has it) and also replied with a different answer;
`guard.noisy-oracle.02` replied with a different answer; `workflow.history-audit.02` cited a
historical source that cannot be verified. The model also recovered from 14 visible tool errors
(`EXEC_FAILED 7`, `EXEC_TIMEOUT 3`, `FILE_NOT_FOUND 1`, `REVISION_CONFLICT 1`, `UNKNOWN_CHECK 2`)
without losing a single request prefix.

### Track B · Phase 1–4 mechanisms

| Metric | Live | Scripted |
| --- | ---: | ---: |
| Evaluated / passed | 100 / **95** | 100 / **100** |
| Requests (`p50 / p95`) | 1,155 (`10 / 22`) | 522 |
| `seconds_p95` | 25.62s | 0.89s |
| Metered tokens | 2,387,988 (input 2,295,065 / output 92,923) | — |
| Static prefix pairs | **584 / 584 (100%)** | **218 / 218 (100%)** |
| Merge gate | `ok 24` / `MERGE_VALIDATION_FAILED 10` / `MERGE_CONFLICT 3` | `ok 25` / `VALIDATION_FAILED 8` / `CONFLICT 4` |
| Tools | `read 590`, `validate 225`, `edit 224`, `exec 99`, `write 7`, `rollback 5` | `edit 192`, `validate 165`, `read 48`, `rollback 7` |
| `validate` fan-out speedup (`p50 / max`) | 1.38 / 3.13 (16 samples) | 2.65 / 2.85 |

| Shape | Live | Scripted | Shape | Live | Scripted |
| --- | ---: | ---: | --- | ---: | ---: |
| `atomic-rename` | 10/10 | 10/10 | `branch-rewind` | 10/10 | 10/10 |
| `atomic-abort` | 8/8 | 8/8 | `branch-green` | 5/5 | 5/5 |
| `atomic-rollback` | 6/7 | 7/7 | `branch-ab` | 10/10 | 10/10 |
| `validate-fanout` | 12/13 | 13/13 | `worker-disjoint` | 8/8 | 8/8 |
| `write-barrier` | 11/12 | 12/12 | `worker-shared-file` | 5/5 | 5/5 |
| `worker-write-skew` | **8/8** | 8/8 | `worker-conflict` | 2/4 | 4/4 |

Mechanism trigger rates (`triggered` counts cases where the run actually exercised the mechanism;
`evaluated` counts cases designed to allow it):

| Mechanism | Live | Scripted |
| --- | ---: | ---: |
| Atomic multi-file changeset | 25/25 | 17/25 |
| Parallel `validate` (2+ checks in one turn) | 16/25 | 25/25 |
| Explicit rollback to a checkpoint | 1/7 | 7/7 |
| Worker `rebase` before the second merge | `ok 8` | `ok 8` |

The five live Track B failures are strictness, not mechanism, failures: `write-barrier.07` and
`validate-fanout.00` produced a semantically valid `app/service.py` that differs byte-for-byte from
the expected file; `atomic-rollback.06` failed the case's `acceptance` check; `worker-conflict.02/.03`
reworded `notes.md` so the marker strings were absent, and in `.03` a reworded mainline edit removed
the same-line conflict the case depends on (`MERGE_VALIDATION_FAILED` instead of `MERGE_CONFLICT`).
The fix direction for this class is the oracle relaxation already listed in the plan (markers /
`any_of` instead of byte-exact), not a mechanism change.

## What changed since the 2026-11-28 baseline

| Commit | Change | Effect here |
| --- | --- | --- |
| `c7f41bd` | `worker rebase` three-way merge + `worker.json` `base_revision` update | worker family 15/25 → 23/25 |
| `7927734` | Benchmark scores the rebase step; write-skew case rebases before the fix | write-skew 0/8 → 8/8 |
| `10b016f` | Write-barrier prompt names the configured `lint` and `types` checks | write-barrier 0/12 → 11/12 |
| `7242ca1` | Write-skew prompt states `2x` / `scale=2 → 4x`, default `1`, and "only the signature and return line" | removed the no-op-`scale` and docstring readings (8/8 live) |
| `b8bc33e` | `changeset_intent` pre-write + `Session::open` forward recovery + `CHANGESET_CONFLICT` on external edits | keeps the interrupt tests honest; no benchmark case depends on a crash |

Only `10b016f` and `7242ca1` touch task text, and neither relaxes the oracle: `exact`, invariants and
configured checks are unchanged. Every commit keeps `instructions` and `tools` static and preserves
the append-only prompt prefix.

## The rejected alternative, for the record

Before changing the write-barrier prompt, we tried telling the model which checks exist by appending
`[Available validate checks: lint, types]` to the same user turn (no extra message, no instruction
change). Live, on the eight write-skew cases, that version scored **0/7** with 637,831 metered tokens
(7.5× the baseline), 91 `validate` calls and five `REBASE_CONFLICT`s: the mainline chased checks only
the worker could satisfy, edited `pkg/service_b.py`, and then could not merge. The experiment was
reverted cleanly and no code from it remains; the fix moved to the task text instead, where the
ambiguity actually lived.

## Reproduce

~~~bash
git checkout b8bc33e
cargo build --locked
export SOURCEHARNESS_QWEN_API_KEY=...   # loopback gateway credential

python -B scripts/eval_qwen_benchmark.py --exe target/debug/source_harness \
  --variant-range 2-3 --workers 4 --base-url http://127.0.0.1:3425/v1 \
  --model auto/deepseek-v4.1-flash-1017 --out /path/to/fresh-trackA

python -B scripts/phase14_benchmark.py --exe target/debug/source_harness \
  --driver live --base-url http://127.0.0.1:3425/v1 \
  --model auto/deepseek-v4.1-flash-1017 --out /path/to/fresh-live-b

python -B scripts/phase14_benchmark.py --exe target/debug/source_harness \
  --out /path/to/fresh-scripted
~~~

Each result directory refuses to mix drivers, models or binaries, so use a fresh directory per
configuration. Track A is live by design and has no scripted mode.

## Files

Each `summary-*.json` below was written by the run whose per-case rows are in the adjacent
`cases-*.jsonl.gz` (one JSON object per line, ordered by case id, 100 rows each); the pairing,
row count, pass count and digests are recorded in `provenance.json`.

| Track | Summary | Cases (`sha256`) |
| --- | --- | --- |
| A | [summary-trackA.json](summary-trackA.json) | [cases-trackA.jsonl.gz](cases-trackA.jsonl.gz) `b26d0832f2e55e90…` |
| B live | [summary-live-b.json](summary-live-b.json) | [cases-live-b.jsonl.gz](cases-live-b.jsonl.gz) `cefe53c65a75174e…` |
| B scripted | [summary-scripted.json](summary-scripted.json) | [cases-scripted.jsonl.gz](cases-scripted.jsonl.gz) `ab3e1628731551e8…` |

Settings captured by each run are in `settings-trackA.json`, `settings-live-b.json` and
`settings-scripted.json`. The compressed data can be read with gzip or zcat. Evaluation plan and
mechanism definitions: [docs/phase14-200-task-evaluation-plan.md](../../phase14-200-task-evaluation-plan.md).
The earlier baseline this run is compared against stays at
[docs/evidence/phase14-200-2026-11-28/](../phase14-200-2026-11-28/README.md).
