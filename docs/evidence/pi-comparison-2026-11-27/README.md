# SourceHarness and Pi paired benchmark

This benchmark compares SourceHarness at e2818d2 with Pi 1.6.1 on the published Markdown link and sampling report fixtures. Both arms use DeepSeek V4.1 Flash at low effort through the OpenCode Go Responses endpoint. Each phase uses the same prompt, acceptance logic, and 16-request limit, with a 600-second process timeout. A pass requires the final workspace to satisfy the independent checker, a successful acceptance check of that exact revision, and a natural end.

The measured runs used isolated Linux workspaces. SourceHarness used its session tools and Pi used its built-in tools with a shell command that called the acceptance checker through a local broker. The broker gave Pi the checker result. Pi's writable workspace and tool sandbox did not mount the checker source or its oracle configuration. The sampling report comparison covers the first two phases.

| Scenario | Arm | Natural passes | Model requests | Total tokens | Cached share of input tokens | Calls after final check |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| Markdown links, 3 pairs | SourceHarness | 6/6 | 36 | 284,793 | 58.6% | 2 |
| Markdown links, 3 pairs | Pi | 6/6 | 41 | 169,061 | 87.0% | 1 |
| Sampling report, 1 pair | SourceHarness | 2/2 | 17 | 316,010 | 45.3% | 0 |
| Sampling report, 1 pair | Pi | 2/2 | 21 | 213,189 | 90.6% | 0 |

Pi used 36.4% fewer total tokens in the combined sample. Mean input per request in phase two was 10,969 tokens for SourceHarness and 5,407 for Pi in Markdown, and 26,607 for SourceHarness and 15,208 for Pi in sampling reports. The provider reported usage for all 115 model requests. Counts include cached input tokens. The data describe four paired sessions across two fixtures.

## Evidence and reproduction

- [Paired scores and build details](results.json) includes per-phase correctness, final-validation receipts, post-check calls, and the SourceHarness executable digest.
- [Per-request measurements](requests.jsonl) includes wire size and digest, provider usage, and response status for every measured request.
- [Reproduction bundle](reproduce.zip) contains the recorded runner, check adapters, aggregator, and npm lockfile.

Use the same Linux Codex environment, including Bubblewrap, Node, and Rust. From a SourceHarness checkout containing this evidence, create an isolated worktree at the tested revision.

~~~bash
bench="$(mktemp -d)"
git worktree add --detach "$bench/SourceHarness" e2818d2991119ad46b36f6ee40a68d46c8f944a1
cargo build --locked --manifest-path "$bench/SourceHarness/Cargo.toml"
unzip docs/evidence/pi-comparison-2026-11-27/reproduce.zip -d "$bench/pi-comparison"
npm ci --prefix "$bench/pi-comparison" --ignore-scripts --no-audit --no-fund
mkdir "$bench/pi-comparison/public-fixtures"
git -C "$bench/SourceHarness" archive HEAD scripts/check_natural_session.py scripts/natural_session_cases.py scripts/session_eval_cases.py \
  | tar --strip-components=1 -x -C "$bench/pi-comparison/public-fixtures"
chmod 755 "$bench/pi-comparison/acceptance" "$bench/pi-comparison/pi-acceptance"
cd "$bench/pi-comparison"
python3 -B compare_pi_public.py
python3 -B consolidate.py
~~~

Supply the OpenCode Go test credential at the runner's hidden prompt. The runner writes the paired scores and per-request measurements in the working directory.
