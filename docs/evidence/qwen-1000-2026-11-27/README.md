# Qwen3.8 compact evaluation

The 1,000-task corpus and independent checker are ready. The real Qwen run has completed zero tasks. The original Gradio deployment returned HTTP 404. OpenCode Go returned HTTP 403 and reported disabled model access for both qwen3.8-flash and qwen3.8-max. The benchmark stopped after the first blocked request with each model. [Connection evidence](access-probes.json)

## Workloads

The corpus has 50 task shapes with 20 seeded instances each. The manifest hashes only the workspace files and phase specifications, so distinct identifiers alone cannot satisfy the uniqueness check. Multi-phase tasks reuse one journal. Input files, prompts and checker vectors can be reproduced from the generator. [Case manifest](manifest.jsonl)

| Family | Tasks | What the checker measures |
| --- | ---: | --- |
| Inspect | 200 | Exact structured answers from configuration, logs and tables |
| Patch | 200 | Scoped changes, Unicode, CRLF, links and protected bytes |
| Report | 160 | Aggregation and ranking from multiple files, with a checked deliverable |
| Repair | 200 | Python CLI behavior on hidden test vectors |
| Workflow | 140 | External edits, historical citations, rollback, validation drift and SIGKILL recovery |
| Guard | 100 | Handling missing inputs, incompatible checks and unsupported mutations |

The runner uses the compact SourceHarness CLI with a 50-turn model limit. Its independent checks distinguish workspace correctness from validation of the final revision. Candidate code used in behavioral checks runs inside Bubblewrap with a read-only workspace and socket creation disabled. Ground truth and validation programs stay outside the tool sandbox. The benchmark separates blocked model access and other infrastructure interruptions from graded tasks. It records model identity, requests, metered usage, cache telemetry, exact-prefix retention, tool errors, guard pauses and failure reasons by task shape.

## Run

Build the CLI from its lockfile on Linux with Bubblewrap and libseccomp available.

~~~bash
cargo build --locked
python3 -B scripts/eval_qwen_benchmark.py --out /tmp/qwen1000-plan --plan-only
python3 -B scripts/eval_qwen_benchmark.py --exe target/debug/source_harness \
  --out /path/to/qwen1000-results --model qwen3.8-flash --limit 1000 --workers 8
~~~

Use the same output directory, corpus, model, and binary to resume. The runner skips already evaluated cases and retries eligible cases interrupted by infrastructure. The API credential is read from a hidden prompt, SOURCEHARNESS_OPENCODE_GO_API_KEY for OpenCode, or SOURCEHARNESS_QWEN_API_KEY for an explicitly configured HTTPS Responses endpoint. OpenCode credentials are bound to opencode.ai. [OpenCode Go endpoint documentation](https://opencode.ai/docs/console/go/#endpoints)

Twenty local Python tests passed, including two SourceHarness CLI replay tests for CRLF validation and recovery from SIGKILL after an external file change. The replay tests use a scripted provider. They establish that the benchmark runs end to end and supply no real Qwen performance data.
