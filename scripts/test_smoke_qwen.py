import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from smoke_qwen import FIXTURE, PROMPT, ToolSmoke, load_profile


class UpstreamResponse(io.BytesIO):
    status = 200


class ToolSmokeTests(unittest.TestCase):
    def test_read_result_keeps_call_id_and_replays_assistant_state(self):
        tool = {"name": "read", "description": "Read a file", "parameters": {"type": "object"}}
        args = json.dumps({"path": FIXTURE, "start_line": 1})
        call = {"type": "function", "id": "call_fixture",
                "function": {"name": "read", "arguments": args}}
        first = {"model": "fixture", "choices": [{
            "message": {"content": None, "reasoning_content": "test reasoning", "tool_calls": [call]},
            "finish_reason": "tool_calls",
        }]}
        last = {"model": "fixture", "choices": [{"message": {"content": "hello"}, "finish_reason": "stop"}]}
        smoke = ToolSmoke("https://example.test/v1/chat/completions", "fixture", "integration-test", "hello\n")
        start = {"role": "user", "content": PROMPT}
        body = {"model": "fixture", "input": [start], "instructions": "Use tools", "tools": [tool]}
        output = {
            "type": "function_call_output", "call_id": "call_fixture",
            "output": json.dumps({"status": "ok", "source": {"path": FIXTURE}, "content": "hello\n"}),
        }
        with patch.object(smoke.opener, "open", side_effect=[
            UpstreamResponse(json.dumps(value).encode()) for value in (first, last)
        ]) as upstream:
            native = smoke.reply(body)
            self.assertEqual(native["output"], [{
                "type": "function_call", "call_id": "call_fixture", "name": "read", "arguments": args,
            }])
            with self.assertRaisesRegex(ValueError, "expected function result"):
                smoke.reply({**body, "input": [start, {**output, "call_id": "other"}]})
            self.assertEqual(upstream.call_count, 1)
            final = smoke.reply({**body, "input": [start, *native["output"], output]})
            self.assertEqual(final["output"][0]["content"][0]["text"], "hello")
            sent = json.loads(upstream.call_args.args[0].data)
            self.assertEqual(sent["messages"][-2]["tool_calls"], [call])
            self.assertEqual(sent["messages"][-2]["reasoning_content"], "test reasoning")
            self.assertEqual(sent["messages"][-1], {
                "role": "tool", "tool_call_id": "call_fixture", "content": output["output"],
            })

    def test_profile_contains_configuration_only(self):
        with tempfile.TemporaryDirectory() as directory:
            profile = Path(directory, "profile.json")
            profile.write_text(json.dumps({"base_url": "https://example.test/v1/", "model": "fixture"}))
            self.assertEqual(load_profile(profile), ("https://example.test/v1", "fixture"))
            profile.write_text(json.dumps({"base_url": "https://user@example.test/v1", "model": "fixture"}))
            with self.assertRaisesRegex(ValueError, "without credentials"):
                load_profile(profile)


if __name__ == "__main__":
    unittest.main()
