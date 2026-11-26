# Qwen tool contract and concurrency experiment

Tested Qwen3.8-27B-GGUF on the supplied endpoint. All inputs were synthetic. Each comparison kept the task and result for the same case fixed except for the format/contract being tested. Changed questions and domains on the second batch, not the system prompt.

- Two in-flight requests: four requests in the same two cohorts took 20.117 s serially and 14.798 s concurrently. All eight returned HTTP 200. This is a 26.4% reduction in batch elapsed time for this small sample, not a service capacity guarantee. The load probe was separate from the decision samples. The next section separately measures four workers.
- Search: explicitly tell the model the family prefix includes the final separator and excludes the last item component. For fresh cases the baseline was 1/6 and the clearer parameter 6/6. Another held-out batch with different data was 0/4 and 4/4. The first batch uses a few of the same fixture shapes as the load probe and is exploratory; the second does not.
- Pagination: under an undefined offset, the original pair of parameters passed 3/6 and one continuation cursor passed 5/6. In a held-out batch, explicitly saying that offset is the returned next_offset gave 4/4, and a random opaque cursor gave 4/4. Keep the legacy byte-offset interface for random access. Improve its parameter definition. The single-cursor implementation stays opt-in for sequential traversal; the held-out sample does not show that it beats a clearly defined offset.
- Both Rust pagination paths passed a live smoke via local Magpie on Windows. The Qwen A/B runs used synthetic tool results, not the Rust executable, and the number of requests is not a production performance estimate. No default provider or system prompt changes.

Evidence: qwen-iteration-vivion-1795665242248057400.jsonl, qwen-holdout-vivion-1795665451633001100.jsonl.

## Four-worker verification

The Rust harness command `check-next-call <samples> [workers]` now defaults to four independent cases at a time. Use `1` to reproduce the old serial schedule or `2` for the previous two-worker cadence. Work is capped at four; dependent tool turns inside one case remain sequential. Evidence retains sample IDs and records worker count.

New Qwen crossover (eight fresh cases with nested asset identifiers and mixed Unicode text, two cohorts, same cases at each concurrency): two workers 31.706 s / 8 of 8 correct, four workers 30.553 s / 8 of 8 correct. All 16 responses HTTP 200. The roughly 3.6% batch improvement does not establish a material speedup or a provider capacity ceiling. The Qwen run tested model decisions with synthetic tool results, separately from native Rust checks. Raw evidence: `qwen-four-gate-vivion-1795666174372569300.jsonl`.
