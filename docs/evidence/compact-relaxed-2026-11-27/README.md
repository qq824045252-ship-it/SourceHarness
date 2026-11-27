# Relaxed compact profile

This run uses the published sampling-report and Markdown scenarios with DeepSeek V4.1 Flash at low effort. It uses the same task prompts and independent checker as the original comparison. Both the CLI and provider bridge allow up to 50 requests per phase.

The CLI records revisions at the start of unobserved mutations and checks recorded observations when available. The sandbox uses a disposable workspace copy. A strict phase pass requires independent acceptance, model-initiated validation of the final revision, and natural completion. Requests, tokens and cache share are measured alongside those outcomes.

| Trial | Strict phases | Requests by phase | Total tokens | Cached share of input |
| --- | ---: | ---: | ---: | ---: |
| Sampling 1 | 2/2 | 12 + 11 = 23 | 261,438 | 91.6% |
| Sampling 2 | 2/2 | 13 + 15 = 28 | 307,451 | 92.9% |
| Sampling 3 | 2/2 | 14 + 18 = 32 | 427,584 | 93.9% |
| Markdown | 2/2 | 7 + 5 = 12 | 29,367 | 84.9% |

All three sampling trials passed both phases. In trial 3 the second phase passed its final validation on request 17 and ended naturally on request 18. Across the four trials there were no recorded pauses. The sampling trials each made four direct overwrites of existing files without a prior live observation. The Markdown trial made two direct exact-text edits. These counts come from the tool-result observations and edit receipts in the test journals.

The sampling cohort used 83 requests and 996,473 tokens. The preceding 16-turn cohort at d4d5311 used 70 requests and 768,551 tokens and produced two strict passes among three trials. The current Markdown run used 29,367 tokens. The earlier grouped-edit run used 39,654 and the historical Pi pair used 73,406. [Previous sampling cohort](../compact-validate-first-2026-11-27/README.md) · [Earlier Markdown and Pi comparison](../compact-tools-2026-11-27/README.md)

All 91 adjacent request pairs kept the exact input prefix, instructions and tool declarations. The provider supplied usage for all 95 requests. Newly appended tool outputs contained zero turn-budget reminders.

## Data and reproduction

- [Sampling results](sample-results.json) and [request measurements](sample-requests.jsonl).
- [Markdown results](markdown-results.json) and [request measurements](markdown-requests.jsonl).

Use the published runner and fixtures from the historical comparison, and build the tested CLI.

~~~bash
cargo build --locked
python3 -B docs/evidence/compact-tools-2026-11-27/compare_compact.py \
  --runner-dir "$bench/pi-comparison" --exe target/debug/source_harness \
  --out "$bench/relaxed-sample" --case sample-report --repeats 3 --request-limit 50
python3 -B docs/evidence/compact-tools-2026-11-27/compare_compact.py \
  --runner-dir "$bench/pi-comparison" --exe target/debug/source_harness \
  --out "$bench/relaxed-markdown" --case markdown-links --request-limit 50
~~~

The Rust tests exercise unobserved exact edits and overwrites, same-turn read and edit, observed absence, external revision conflicts, sandbox boundaries and accepted command limits. They also cover interruption and recovery across the former 16-turn limit, loop guard pauses, and rollback recovery.
