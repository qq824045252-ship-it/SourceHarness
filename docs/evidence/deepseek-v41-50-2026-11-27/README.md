# DeepSeek V4.1 Flash on 50 compact tasks

This test uses the first 50 entries from the existing 1,000-task corpus. These entries cover all 50 task shapes with one instance per shape. The CLI uses the compact profile with a 50-turn model limit per phase. OpenCode Go serves the exact deepseek-v4.1-flash model through the Responses API with low effort. The binary and manifest digests are recorded in the summary. [Corpus manifest](../qwen-1000-2026-11-27/manifest.jsonl) · [Measured summary](summary.json)

A strict pass requires the expected answer or workspace behavior, final-revision validation where requested, and a valid stop. An expected guard pause can satisfy an otherwise blocked task if the independent checks pass. The workflow group also checks historical citation and recovery requirements where applicable. Tool errors and guard pauses are counted by actual journal events. The read and patch groups alone cannot establish performance on the rest of the corpus.

The first run used eight workers. An upstream 429 interrupted five in-flight cases after 20 results had been saved. OpenCode Go's usage meter showed quota available. The next invocation restricted scheduling to the selected 50 IDs and used three workers. Cases with confirmed model responses and completed evaluation are recorded in compressed JSON Lines files. Interrupted attempts stay outside the pass-rate denominator and appear separately in the summary.

## Reproduce

Use the Qwen benchmark runner with the DeepSeek model ID on Linux.

~~~bash
cargo build --locked
python3 -B scripts/eval_qwen_benchmark.py \
  --exe target/debug/source_harness --out /path/to/fresh-results \
  --model deepseek-v4.1-flash --limit 50 --workers 3
~~~

The command above selects the first 50 in an empty result directory. For recovery in a populated directory, select the same 50 case IDs with repeated case flags. The runner skips already evaluated cases. The compressed data can be read with gzip or zcat.
