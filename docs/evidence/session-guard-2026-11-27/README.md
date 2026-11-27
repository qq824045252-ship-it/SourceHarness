# Session evidence delivery and validation feedback

The original patch omitted a stable history notice from later stateless requests. The fix checks the actual request and records the delivered notice bytes. Repeated tool operations now produce a warning. A passing validation receipt can compare the checked file manifest with the manifest captured at the start of the run. The file list marks whether it is complete.

## Live Markdown link repair

The original patch and this fix each ran the two-phase `markdown-links` scenario through OpenCode Go Responses using DeepSeek V4.1 Flash at low effort. The second phase moves an existing file between user turns. Both arms used a 600-second per-phase evaluator deadline and isolated workspaces. The forwarding process supplied a stable session header for each arm.

| Build | Accepted | Natural pass | Natural pass with zero calls after check | Calls after check | History omissions | Requests | Known total tokens | Requests missing usage |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| original-2ef1770 | 2/2 | 2/2 | 2/2 | 0 | 3 | 11 | 80,059 | 0 |
| fixed | 2/2 | 2/2 | 1/2 | 3 | 0 | 14 | 122,023 | 0 |

Natural pass requires the independent check, current-revision validation, and one successful execution. Calls after check count tool attempts following the first passing validation of the delivered revision for the phase. History omissions count requests with selected history facts missing from the actual request. Both arms passed both tasks. The fixed build delivered every selected history notice. It made three reads after validation in the second phase. Two returned the same revision and byte range previously delivered for the reference and the guide. The index read retrieved the edited file at a new revision. The original arm had no post-check calls in this pair. The comparison provides one sample per arm.

## Earlier sampling-report diagnostic

The four-phase `sample-report` comparison used the same provider, model, and effort. Each arm hit the 240-second evaluator deadline once and finished after resuming the same run. The recovery evaluator used a 600-second deadline.

| Build | Accepted | Natural pass | Natural pass with zero calls after check | Calls after check | History omissions | Requests | Known total tokens | Requests missing usage |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| baseline-119711a | 4/4 | 3/4 | 0/4 | 9 | 0 | 39 | 1,364,312 | 1 |
| patch-a922af5 | 4/4 | 3/4 | 2/4 | 1 | 6 | 41 | 1,389,710 | 1 |

The original guard was also evaluated against 314 archived tool results from the existing continuous-session [traces](../natural-session-2026-11-27/README.md). It raised zero warnings and pauses. The replay applies the guard to the recorded tools and outcomes and measures whether it would have signaled on them.

## Reproduce

Build each version with `cargo build --locked`. Point the executable at a Responses endpoint for the selected model. OpenCode Go requires a stable `x-opencode-session` header and client user agent. Run the [scenario evaluator](../../../scripts/eval_natural_session.py) for each arm with the same settings and separate output directories.

```bash
python3 -B scripts/eval_natural_session.py --exe target/debug/source_harness \
  --case markdown-links --model deepseek-v4.1-flash --effort low --out target/markdown-session
```

[Phase scores, validation evidence, and build digests](results.json) · [Per-request data](requests.jsonl) · [Guard replay](guard-replay.json)
