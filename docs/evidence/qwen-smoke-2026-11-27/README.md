# Qwen Chat Completions smoke

The SourceHarness CLI completed a two-request tool smoke with unsloth/Qwen3.8-27B-GGUF using HTTPS Chat Completions and Bearer authentication. Both responses reported the configured model. The first emitted a read function call. The CLI executed it against a temporary synthetic file and returned the result under the same call ID. Qwen returned the exact fixture text and the CLI exited successfully.

The runner makes up to two upstream requests for one synthetic read and returns complete Responses SSE events to the CLI. Upstream responses use non-streaming JSON. [Measured result](results.json)

## Run

Build the SourceHarness CLI and create a local JSON profile with the gateway base URL and model ID. The base URL includes the /v1 suffix.

~~~json
{
  "base_url": "https://example.test/v1",
  "model": "unsloth/Qwen3.8-27B-GGUF"
}
~~~

~~~bash
cargo build --locked
python3 -B scripts/smoke_qwen.py --exe target/debug/source_harness --profile /path/to/profile.json
~~~

Supply the credential at the hidden prompt or through SOURCEHARNESS_QWEN_API_KEY for that process. The profile stores the base URL and model ID. The runner reports only status, timing, tool round-trip checks, and provider token counts.
