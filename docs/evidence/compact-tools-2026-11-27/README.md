# Compact tools and the Pi comparison

The compact profile combines disposable shell execution, numbered text reads, implicit write revisions and checkpoint rollback. The tuned CLI used the same published fixtures, prompts, acceptance checker, DeepSeek V4.1 Flash model, low reasoning effort and 16-request phase limit as pair 1 of the earlier benchmark. The classic and Pi rows are historical measurements from that pair. A strict pass requires the independent checker to pass, a passing model-initiated validation of the final revision and a natural end. The runner stops a session when a phase fails to end naturally.

| Fixture | Arm | Strict phases | Model requests | Total tokens | Cached share of input |
| --- | --- | ---: | ---: | ---: | ---: |
| Markdown links | Classic SourceHarness | 2/2 | 11 | 87,838 | 54.1% |
| Markdown links | Pi 1.6.1 | 2/2 | 15 | 73,406 | 87.7% |
| Markdown links | Compact pilot | 2/2 | 15 | 41,418 | 86.7% |
| Markdown links | Compact with grouped edits | 2/2 | 14 | 39,654 | 86.9% |
| Sampling report | Classic SourceHarness | 2/2 | 17 | 316,010 | 45.3% |
| Sampling report | Pi 1.6.1 | 2/2 | 21 | 213,189 | 90.6% |
| Sampling report | Compact pilot | 1/2 | 32 | 275,119 | 92.9% |
| Sampling report | Compact with grouped edits | 0/2 | 16 | 96,446 | 88.6% |
| Sampling report | Compact with grouped edits, repeat | 1/2 | 28 | 377,329 | 93.2% |

The tuned Markdown run used 54.9% fewer tokens than the classic arm and 46.0% fewer than Pi. All 55 adjacent request pairs in the tuned runs preserved the preceding input array as a prefix, including across task phases, with identical instructions and tools. The provider reported usage for all 58 tuned requests.

In the sampling pilot, the final files passed the independent check after phase two. The run reached the request limit before calling `validate` on those files. The tuned build grouped disjoint substitutions in one call and clarified the scope of disposable execution and live reads. In its first sampling run, the model used the default read mode when opening an output file and spent its remaining turns investigating paths. Phase two did not start. In the repeat, phase one passed in 12 requests. Phase two passed a model-initiated check, then the model edited the usage document. The independent checker accepted the final state. The model reached the request limit without validating the final revision or ending naturally.

The sampling repeat averaged 18,871 input tokens per request in phase two, compared with 26,607 for classic SourceHarness and 15,208 for Pi. Its additional requests raised total token use above both historical arms. The large fixture still has a convergence and final-verification gap.

## Evidence

- [Tuned phase results](results.json) and [per-request measurements](requests.jsonl).
- [Sampling repeat results](sample-repeat-results.json) and [per-request measurements](sample-repeat-requests.jsonl). Both tuned runs used the same executable digest.
- [Initial pilot results](pilot-results.json) and [per-request measurements](pilot-requests.jsonl).
- [Historical SourceHarness and Pi data](../pi-comparison-2026-11-27/README.md).

The per-request files include input size, input digest, prefix comparisons, usage and provider status. The phase results include acceptance, exact-revision validation and natural completion separately.

## Reproduce

Prepare the prior benchmark runner and its published fixtures using the historical comparison instructions. Build this branch and use the same Linux Bubblewrap environment. The runner reads the test credential at a hidden prompt.

~~~bash
cargo build --locked
python3 -B docs/evidence/compact-tools-2026-11-27/compare_compact.py \
  --runner-dir "$bench/pi-comparison" --exe target/debug/source_harness \
  --out "$bench/compact"
python3 -B docs/evidence/compact-tools-2026-11-27/compare_compact.py \
  --runner-dir "$bench/pi-comparison" --exe target/debug/source_harness \
  --out "$bench/compact-sample-repeat" --case sample-report
~~~

The Rust suite covers prefix replay through the CLI, implicit revision conflicts, grouped edits, oracle isolation, socket denial, output folding, timeouts, rollback recovery, external changes and loop pause with steering. The sandbox case requires a working Bubblewrap namespace backend.

~~~bash
cargo test --locked --all-targets -- --include-ignored --skip run_test_checker
cargo clippy --locked --all-targets -- -D warnings
python3 -B -m unittest discover -s scripts -p 'test_*.py'
~~~
