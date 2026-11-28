import argparse
from collections import Counter
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
from tempfile import TemporaryDirectory
from threading import Thread
import unittest

from check_qwen_benchmark import run_behavior, workspace_check
from eval_qwen_benchmark import Gateway, MODEL, oracle, parse_answer, run_case
from qwen_benchmark_cases import corpus, manifest


def tool(call_id, name, arguments):
    return {"type":"function_call","call_id":call_id,"name":name,"arguments":json.dumps(arguments)}


def final(text):
    return {"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}


@contextmanager
def scripted_upstream(outputs):
    requests = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_):
            pass

        def do_POST(self):
            requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
            if len(requests) > len(outputs):
                self.send_error(500,"Unexpected scripted request")
                return
            response = {"model":MODEL,"output":outputs[len(requests)-1],
                        "usage":{"input_tokens":64,"output_tokens":4,"total_tokens":68,
                                 "input_tokens_details":{"cached_tokens":12}}}
            event = json.dumps({"type":"response.completed","response":response})
            raw = f"event: response.completed\ndata: {event}\n\n".encode()
            self.send_response(200)
            self.send_header("Content-Type","text/event-stream")
            self.send_header("Content-Length",str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)

    with ThreadingHTTPServer(("127.0.0.1",0),Handler) as server:
        thread = Thread(target=server.serve_forever,daemon=True)
        thread.start()
        try:
            yield f"http://127.0.0.1:{server.server_port}", requests
        finally:
            server.shutdown()
            thread.join()


class CorpusTest(unittest.TestCase):
    def test_unique_content_and_fixed_category_totals(self):
        plan = manifest(list(corpus()))
        self.assertEqual(plan,manifest(list(corpus())))
        self.assertEqual(len(plan),1000)
        self.assertEqual(len({row["sha256"] for row in plan}),1000)
        self.assertEqual(len({(row["category"],row["shape"]) for row in plan}),50)
        self.assertEqual(Counter(row["category"] for row in plan),{
            "inspect":200,"patch":200,"repair":200,"report":160,"workflow":140,"guard":100,
        })

    def test_workspace_preserves_line_endings_and_protected_files(self):
        case = next(case for case in corpus() if case.id == "patch.crlf-lines.00")
        files = {name:value.encode() for name,value in case.files.items()}
        with TemporaryDirectory() as temporary:
            spec,_ = oracle(Path(temporary),files,case.steps[0])
        changed = {**files, **{name:value.encode() for name,value in case.steps[0].expected["exact"].items()}}
        self.assertFalse(workspace_check(changed,spec))
        name = next(iter(case.steps[0].expected["exact"]))
        self.assertTrue(workspace_check({**changed,name:changed[name].replace(b"\r\n",b"\n")},spec))
        self.assertTrue(workspace_check({**changed,"extra.txt":b"noise"},spec))

    def test_json_output_accepts_conventional_fence(self):
        self.assertEqual(parse_answer('```json\n{"ready":true}\n```'),{"ready":True})
        self.assertIsNone(parse_answer('unstructured text {"ready":true}'))


@unittest.skipUnless(os.environ.get("SOURCEHARNESS_QWEN_TEST_CLI"),"compiled CLI optional")
class CLITest(unittest.TestCase):
    def local_case(self, identifier, outputs):
        case = next(case for case in corpus() if case.id == identifier)
        args = argparse.Namespace(exe=Path(os.environ["SOURCEHARNESS_QWEN_TEST_CLI"]).resolve(),
                                  model=MODEL,phase_timeout=30)
        with TemporaryDirectory() as directory, scripted_upstream(outputs) as (upstream,requests):
            gateway = Gateway(upstream,MODEL,"local-stub")
            with gateway.serve() as proxy:
                result = run_case(args,case,Path(directory),gateway,proxy)
        self.assertEqual(len(requests),len(outputs))
        self.assertTrue(result["evaluated"])
        self.assertTrue(result["passed"],result["phases"])
        self.assertEqual(result["prefix_exact_pairs"],result["prefix_pairs"])
        return result

    def test_crlf_edit_receipt_and_independent_validation(self):
        case = next(case for case in corpus() if case.id == "patch.crlf-lines.00")
        path, expected = next(iter(case.steps[0].expected["exact"].items()))
        result = self.local_case(case.id,[
            [tool("edit","write",{"path":path,"content":expected})],
            [tool("check","validate",{})],
            [final("Acceptance passed.")],
        ])
        self.assertEqual(result["cost"]["known_usage"]["total_tokens"],204)
        self.assertTrue(result["phases"][0]["validated"])

    def test_cross_phase_sigkill_recovery(self):
        case = next(case for case in corpus() if case.id == "workflow.kill-recover.00")
        path, expected = next(iter(case.steps[1].expected["exact"].items()))
        result = self.local_case(case.id,[
            [tool("observe","read",{"path":path})],
            [final(json.dumps(case.steps[0].expected["answer"]))],
            [tool("stale","write",{"path":path,"content":expected}),tool("refresh","read",{"path":path})],
            [tool("mutate","write",{"path":path,"content":expected})],
            [tool("check","validate",{})],
            [final("Acceptance passed.")],
        ])
        self.assertEqual(result["phases"][1]["recovery"]["outcomes"],["observed_target_on_recovery"])
        self.assertEqual(result["phases"][1]["errors"],{"REVISION_CONFLICT":1})

    def test_independent_code_check_verifies_json_lines(self):
        candidate = b"""import json, sys
for line in sys.stdin:
    print(json.dumps(json.loads(line)))
"""
        spec={"path":"candidate.py","inputs":[1,[2,3]],"outputs":[1,[2,3]]}
        self.assertIsNone(run_behavior({"candidate.py":candidate},spec))
        self.assertIsNotNone(run_behavior({"candidate.py":candidate},{**spec,"outputs":[9,[2,3]]}))
