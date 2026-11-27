# Search scope and answer accuracy

The selected build tells the model to attribute each count to its query and scanned range. It distinguishes the 20-line sample limit from the 50-group aggregation limit. A capture index outside the compiled regex returns the requested index and the valid range.

Two experimental builds repeated the scan scope and counts in a sentence. One did so on every successful search. The other did so only when samples were truncated. The selected build uses the revised tool descriptions and capture error with the existing structured success payload.

## Regrading the archived evidence

The available four-arm archive contains 144 formal observations at default effort. The separate 16-run smoke archive was unavailable. The [smoke review record](smoke-review.json) preserves the three cases and batch totals reported in the user's review.

The formal regrade verified full originals in 31 observations using each snapshot's archived fingerprint. Counting exact card lines in the recovered text supplied an earlier or previously missing proof point in 15 observations. The regrade records the call that produced evidence, calls already issued in the same response, and calls issued in later responses.

The grader checked 91 explicit table counts. In formal case 23215/B, the answer swapped the counts for lines 421–560 and 561–700. The verified counts were 7 and 5, respectively. Two answers disclosed IDs despite a request to omit them. Other free-form claims in the archived answers remain unscored.

## Live comparisons

The tests ran the Windows release CLI through the Magpie Responses gateway on DeepSeek V4.1 Flash at low effort, with inline first-packet delivery. Each pair used the same fixture and prompt. Pair order alternated. The two builds in the last batch sent byte-equivalent first requests with identical tool declarations. The test varies search feedback while keeping read behavior fixed.

| Batch | Build | Strict pass | Calls | Model requests | Median total tokens |
| --- | --- | ---: | ---: | ---: | ---: |
| Sentence on every search | Original | 6/6 | 19 | 14 | 4,146 |
| Sentence on every search | Candidate | 3/6 | 28 | 18 | 8,423 |
| Sentence on truncated samples | Original | 7/8 | 22 | 19 | 4,208 |
| Sentence on truncated samples | Candidate | 6/8 | 31 | 22 | 5,894 |
| Same tool declaration | Sentence on truncated samples | 5/8 | 22 | 19 | 4,141 |
| Same tool declaration | Selected build | 7/8 | 21 | 19 | 3,954 |

All 44 observations gave the correct requested values. Strict passes also require the requested answer format and verified evidence. A fenced JSON object alone meets the format. Extra prose is recorded separately, and explicit additional scoped claims in structured answers can be checked against the fixture.

All 12 aggregation observations obtained complete frequency tables and reported the correct distinct count, duplicate count, and top four groups. All six paging observations returned the requested 136 source lines byte for byte and followed the continuation twice. Those checks establish that the capabilities ran. The sample size and natural variation leave efficiency rankings open. The last batch delivered the truncation sentence once, and total tokens were 47,218 with the sentence option and 48,330 in the selected build.

The runner distinguishes original bytes, verified counts, and the point when the model could use evidence. The independent grader can derive a whole-report total from disjoint section counts and verify an exact original from several reads. A later request may use evidence from the earlier batch to compute its answer.

## Reproduce or inspect

```powershell
python -B -m unittest discover -s scripts -p "test_*.py"
python -B scripts/eval_search_contract.py --baseline before.exe --candidate after.exe --out target/search-comparison --include-samples
python -B scripts/regrade_search_archive.py path/to/SourceHarness-vivion-four-arm-formal.md --out target/formal-regrade.json
```

[Build, tool declaration, and source identities](results.json) · [Exact requests, SSE, scorer results, and experiment patches](live-traces.zip)
