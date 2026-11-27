# Tool-call correctness

The November 27, 2026 run used a Windows release build and real models at low effort. The [results](results.json) tie each batch to the build SHA-256 and record the source hashes, tool calls, and final checks.

| Check | Result |
| --- | --- |
| DeepSeek V4.1 Flash workspace scenarios | 8/8 |
| Qwen3.8 Flash workspace scenarios | 8/8 |
| Cross-line history lookup and exact file delivery | 2/2 |
| Repair from an archived snapshot after a failed check | 1/1 |
| Text, range, and patch editing | 3/3 |
| Original workspace and tool regression suite | 5/5 |
| Rust and Python tests | 50 Rust and 11 Python tests passed |

In the history lookup, DeepSeek first sent literal escape sequences. Qwen first sent LF where the source used CRLF. Each model reused the retry reference, found the original read in the timeline, and passed the final byte checks. Qwen read the original tool output through its history reference. The grader now accepts that route after checking the source event, snapshot revision, and exact bytes.

In the repair run, the first check identified extra prose around a JSON object and a mismatch on line 117. DeepSeek recovered the complete line from the archived report, kept the two correct values, and passed a new check on the delivered files.

The editing batch includes one malformed patch that the tool rejected and the model then corrected. Qwen also named the archived revision as the current revision in one final message. The independent grader confirmed that the current file's bytes were intact.

[Structured results](results.json) · [Requests, responses, and fixtures](trace.zip)
