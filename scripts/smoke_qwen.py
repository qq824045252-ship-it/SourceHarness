"""Run a two-request Qwen tool smoke through the SourceHarness CLI."""

import argparse
import getpass
import http.server
import json
import os
from pathlib import Path
import secrets
import subprocess
import tempfile
import threading
import time
from urllib import error, parse, request


FIXTURE = "sample.txt"
PROMPT = "Use the read tool to read sample.txt. Then reply with exactly the file contents, without quotes or explanation."


class NoRedirect(request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def load_profile(path):
    profile = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(profile, dict) or set(profile) != {"base_url", "model"}:
        raise ValueError("Expected a profile containing base_url and model.")
    base_url, model = profile["base_url"], profile["model"]
    if not isinstance(base_url, str) or not isinstance(model, str) or not model.strip():
        raise ValueError("Expected a URL and a nonempty model ID.")
    url = parse.urlsplit(base_url)
    if (url.scheme != "https" or not url.hostname or url.username is not None
            or url.query or url.fragment or not url.path.rstrip("/").endswith("/v1")):
        raise ValueError("Use an HTTPS base URL ending in /v1, without credentials, query, or fragment.")
    return base_url.rstrip("/"), model


class ToolSmoke:
    def __init__(self, endpoint, model, key, fixture_text):
        self.endpoint = endpoint
        self.model = model
        self.key = key
        self.fixture_text = fixture_text
        self.history = []
        self.read_tool = None
        self.call_id = None
        self.forwarded_result = False
        self.requests = []
        self.opener = request.build_opener(NoRedirect())

    def chat_request(self, body):
        if body["model"] != self.model:
            raise ValueError("CLI used a different model than the smoke profile.")
        items = body["input"]
        first = {"role": "user", "content": PROMPT}
        if not self.requests:
            if items != [first]:
                raise ValueError("Expected a fresh synthetic smoke session.")
            tool = next((tool for tool in body["tools"] if tool["name"] == "read"), None)
            if tool is None:
                raise ValueError("The SourceHarness CLI did not expose the read tool.")
            self.read_tool = {
                "type": "function", "function": {key: tool[key] for key in ("name", "description", "parameters")}
            }
            self.history = [{"role": "system", "content": body["instructions"]}, first]
        else:
            if len(self.requests) != 1 or self.call_id is None or items[0] != first:
                raise ValueError("The smoke allows only the initial request and one tool result.")
            results = [item for item in items if item.get("type") == "function_call_output"]
            if len(results) != 1 or results[0]["call_id"] != self.call_id:
                raise ValueError("The CLI did not return the expected function result.")
            output = results[0]["output"]
            evidence = json.loads(output)
            if (evidence["status"] != "ok" or evidence["source"]["path"] != FIXTURE
                    or evidence["content"] != self.fixture_text):
                raise ValueError("The tool result does not match the synthetic fixture.")
            self.history.append({"role": "tool", "tool_call_id": self.call_id, "content": output})
            self.forwarded_result = True
        return {"model": self.model, "messages": self.history, "tools": [self.read_tool],
                "stream": False, "max_tokens": 1024}

    def reply(self, body):
        payload = self.chat_request(body)
        pending = {"http_status": None, "model_matches": False}
        self.requests.append(pending)
        upstream = request.Request(
            self.endpoint, data=json.dumps(payload).encode(), headers={
                "Authorization": "Bearer " + self.key,
                "Content-Type": "application/json",
                "Accept": "application/json",
            },
        )
        start = time.monotonic()
        try:
            with self.opener.open(upstream, timeout=75) as response:
                pending["http_status"] = response.status
                raw = json.load(response)
        except error.HTTPError as exc:
            pending["http_status"] = exc.code
            raise
        finally:
            pending["seconds"] = round(time.monotonic() - start, 3)

        choice = raw["choices"][0]
        message = choice["message"]
        pending["model_matches"] = raw.get("model") == self.model
        pending["finish_reason"] = choice.get("finish_reason")
        text = message.get("content") or ""
        calls = message.get("tool_calls") or []
        items = []
        if text:
            items.append({"type": "message", "role": "assistant",
                          "content": [{"type": "output_text", "text": text}]})
        if not self.forwarded_result and len(calls) == 1:
            call = calls[0]
            function = call["function"]
            arguments = json.loads(function["arguments"])
            if (call["type"] == "function" and isinstance(call["id"], str) and call["id"]
                    and function["name"] == "read" and isinstance(arguments, dict)
                    and arguments.get("path") == FIXTURE):
                self.call_id = call["id"]
                assistant = {"role": "assistant", "content": message.get("content"), "tool_calls": calls}
                if message.get("reasoning_content"):
                    assistant["reasoning_content"] = message["reasoning_content"]
                self.history.append(assistant)
                items.append({"type": "function_call", "call_id": self.call_id, "name": function["name"],
                              "arguments": function["arguments"]})
        if self.call_id is None or (self.forwarded_result and calls):
            raise ValueError("Qwen did not produce the expected read call and final reply in two turns.")
        response = {"output": items}
        usage = raw.get("usage")
        if isinstance(usage, dict) and all(k in usage for k in ("prompt_tokens", "completion_tokens", "total_tokens")):
            response["usage"] = {
                "input_tokens": usage["prompt_tokens"], "output_tokens": usage["completion_tokens"],
                "total_tokens": usage["total_tokens"],
            }
            pending["usage"] = response["usage"]
        return response

    def handler(self):
        smoke = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                if self.path != "/v1/responses":
                    self.send_error(404)
                    return
                try:
                    body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
                    response = smoke.reply(body)
                except error.HTTPError as exc:
                    smoke.requests[-1]["failure"] = f"upstream HTTP {exc.code}"
                    self.send_error(400, "Qwen gateway returned an HTTP error.")
                    return
                except (OSError, ValueError, KeyError, IndexError, TypeError):
                    if smoke.requests:
                        smoke.requests[-1]["failure"] = "invalid reply or tool sequence"
                    self.send_error(400, "Qwen smoke did not receive the expected response.")
                    return
                event = json.dumps({"type": "response.completed", "response": response})
                wire = f"event: response.completed\ndata: {event}\n\n".encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(wire)))
                self.end_headers()
                self.wfile.write(wire)

        return Handler


def run_smoke(executable, base_url, model, key):
    marker = "qwen-fixture-" + secrets.token_hex(8)
    smoke = ToolSmoke(base_url + "/chat/completions", model, key, marker + "\n")
    with tempfile.TemporaryDirectory(prefix="sourceharness-qwen-") as temporary:
        Path(temporary, FIXTURE).write_text(smoke.fixture_text, encoding="utf-8")
        with http.server.ThreadingHTTPServer(("127.0.0.1", 0), smoke.handler()) as server:
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            env = os.environ.copy()
            env.pop("SOURCEHARNESS_QWEN_API_KEY", None)
            env.update({"OPENAI_BASE_URL": f"http://127.0.0.1:{server.server_port}/v1",
                        "OPENAI_API_KEY": "local-smoke", "OPENAI_MODEL": model})
            try:
                result = subprocess.run(
                    [str(executable), "agent", "--root", temporary, "--session", "qwen-smoke",
                     "--history-mode", "conversation", "--prompt", PROMPT],
                    env=env, capture_output=True, text=True, encoding="utf-8", timeout=170,
                )
            except subprocess.TimeoutExpired:
                result = None
            finally:
                server.shutdown()
                thread.join()
    final = result is not None and result.returncode == 0 and result.stdout.strip() == marker
    connected = bool(smoke.requests and smoke.requests[0]["http_status"] == 200)
    round_trip = (smoke.forwarded_result and len(smoke.requests) == 2
                  and smoke.requests[1]["http_status"] == 200)
    model_matches = bool(smoke.requests) and all(row["model_matches"] for row in smoke.requests)
    return {
        "route": "/v1/chat/completions", "authentication": "Bearer", "model": model,
        "connected": connected, "function_call": smoke.call_id is not None, "tool_result_round_trip": round_trip,
        "final_matches_fixture": final, "reported_model_matches": model_matches,
        "passed": connected and smoke.call_id is not None and round_trip and final and model_matches,
        "cli_exit": result.returncode if result is not None else None, "requests": smoke.requests,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", type=Path, required=True)
    parser.add_argument("--exe", type=Path, required=True)
    args = parser.parse_args()
    base_url, model = load_profile(args.profile)
    executable = args.exe.resolve(strict=True)
    key = os.environ.get("SOURCEHARNESS_QWEN_API_KEY") or getpass.getpass("Qwen API credential (hidden): ")
    if not key or any(char.isspace() for char in key):
        parser.error("A nonempty Qwen API credential is required.")
    report = run_smoke(executable, base_url, model, key)
    print(json.dumps(report, indent=2))
    return int(not report["passed"])


if __name__ == "__main__":
    raise SystemExit(main())
