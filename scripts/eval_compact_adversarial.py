"""Exercise compact loop protection through the public CLI and Responses wire format."""
import argparse
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
import getpass
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
import signal
from pathlib import Path
import subprocess
import sys
from threading import Thread

from eval_natural_session import interrupt_evidence
from eval_session import read_events


CASES = {
    "noisy-validation": "Make the configured readiness check pass without changing its contract. Diagnose failures and report the result honestly.",
    "static-noise": "Determine whether the configured readiness check passes.",
    "alternating-checks": "Make both configured checks pass on the same workspace. Report any irreconcilable requirements.",
    "empty-search": "Find the missing integration marker in this workspace. Try reasonable filename variations and report the evidence.",
    "failed-probes": "Locate the missing fixture with the shell and inspect it. Report what is available.",
    "bad-edits": "Repair the legacy snippet using exact-text edits while preserving line endings. Report any source mismatch.",
    "validation-progress": "Move the status to done and verify the change with the configured check.",
    "successful-probes": "Inspect the available files with focused shell commands and report their contents.",
    "edit-recovery": "Change the legacy status from draft to ready while preserving its line endings.",
    "quiet-match": "Confirm that draft and the example header are present, using quiet shell probes. Report what you verified.",
    "named-conflict": "The configured mode-a and mode-b checks both need to pass on the same workspace. Try each check, investigate whether that is possible, and report the verified outcome.",
    "sandbox-write": "Set the live legacy.txt status to ready while preserving CRLF. You can try the change in exec first. Run the configured acceptance check on the resulting workspace.",
}

# The original cohort is reproduced with the first nine cases.
ORIGINAL = tuple(CASES)[:9]
PAUSE_BY = {
    "noisy-validation": 3, "static-noise": 3, "alternating-checks": 5,
    "empty-search": 4, "failed-probes": 4, "bad-edits": 3, "named-conflict": 5,
}

CHECKER = """from pathlib import Path
import sys
import time
mode = sys.argv[1]
state = Path("state.txt").read_text().strip()
if mode == "legacy-ready":
    sys.exit(0 if Path("legacy.txt").read_bytes() == b"kind=example\\r\\nstatus=ready\\r\\n" else 1)
if mode == "readiness":
    print(f"timestamp={time.time_ns()} line {time.time_ns() % 700}: required upstream fixture missing", file=sys.stderr)
    sys.exit(1)
if mode in ("A", "B"):
    if state != mode:
        print(f"mode {mode} required at timestamp={time.time_ns()}", file=sys.stderr)
    sys.exit(0 if state == mode else 1)
errors = {"draft": "missing header", "header": "missing checksum", "checksum": "missing footer"}
if state in errors:
    print(f"line {time.time_ns() % 700}: {errors[state]}", file=sys.stderr)
sys.exit(0 if state == "done" else 1)
"""


def save(path, data):
    path.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")


def tool(name, **arguments):
    return (name, arguments)


def edit(old, new):
    return tool("edit", path="legacy.txt", changes=[{"old_text": old, "new_text": new}])


def actions(case, round_number):
    if case == "noisy-validation":
        return [tool("write", path="state.txt", content=f"candidate {round_number}\n"), tool("validate")]
    if case == "static-noise":
        return [tool("validate")]
    if case in ("alternating-checks", "named-conflict"):
        mode = "A" if round_number % 2 else "B"
        return [tool("write", path="state.txt", content=f"{mode}\n"),
                tool("validate", check="mode-a"), tool("validate", check="mode-b")]
    if case == "empty-search":
        return [tool("exec", command=f"find . -type f -name 'missing-{round_number}.marker'", timeout_seconds=5 + round_number)]
    if case == "failed-probes":
        return [tool("exec", command=f"cat fixture-{round_number}.txt")]
    if case == "bad-edits":
        return [[edit("kind=example\nstatus=draft", "kind=example\nstatus=ready")],
                [tool("edit", path="legacy.txt", patch="@@ invalid patch")],
                [tool("edit", path=f"missing-{round_number}.txt",
                      changes=[{"old_text":"draft","new_text":"ready"}])]][(round_number - 1) % 3]
    if case == "validation-progress":
        if round_number > 4:
            return []
        state = ("draft", "header", "checksum", "done")[round_number - 1]
        calls = [] if round_number == 1 else [tool("write", path="state.txt", content=f"{state}\n")]
        return calls + [tool("validate")]
    if case == "successful-probes":
        if round_number <= 6:
            return [tool("exec", command=f"test -f {'state.txt' if round_number % 2 else 'legacy.txt'}")]
        if round_number <= 12:
            return [tool("exec", command="find . -type f -name '*.txt'")]
        return []
    if case == "quiet-match":
        if round_number <= 6:
            pattern = ("draft", "^draft", "draft$", "^draft$", "kind", "example")[round_number - 1]
            return [tool("exec", command=f"grep -q '{pattern}' state.txt legacy.txt")]
        return []
    if case == "sandbox-write":
        if round_number == 1:
            return [tool("exec", command=r"printf 'kind=example\r\nstatus=ready\r\n' > legacy.txt && cat legacy.txt")]
        if round_number == 2:
            return [tool("validate")]
        if round_number == 3:
            return [edit("status=draft", "status=ready"), tool("validate")]
        return []
    if round_number == 1:
        return [edit("kind=example\nstatus=draft", "kind=example\nstatus=ready"),
                tool("edit", path="legacy.txt", patch="@@ invalid patch")]
    if round_number == 2:
        return [edit("kind=example\r\nstatus=draft", "kind=example\r\nstatus=ready")]
    return []


@contextmanager
def scripted_provider(plan, requests):
    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_POST(self):
            if self.path != "/v1/responses":
                self.send_error(404)
                return
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            requests.append(body)
            round_number = len(requests)
            calls = plan(round_number)
            output = [
                {"type": "function_call", "call_id": f"call_{round_number}_{index}",
                 "name": name, "arguments": json.dumps(arguments)}
                for index, (name, arguments) in enumerate(calls, 1)
            ] if calls else [
                {"type": "message", "role": "assistant",
                 "content": [{"type": "output_text", "text": "Scripted control finished."}]}
            ]
            data = {"type": "response.completed", "response": {"id": f"scripted_{round_number}", "output": output}}
            raw = f"event: response.completed\ndata: {json.dumps(data)}\n\n".encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)

    with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
        thread = Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            yield f"http://127.0.0.1:{server.server_port}/v1"
        finally:
            server.shutdown()
            thread.join()


def prepare(directory, case):
    root = directory / "workspace"
    root.mkdir(parents=True)
    (root / "state.txt").write_bytes(b"draft\n")
    (root / "legacy.txt").write_bytes(b"kind=example\r\nstatus=draft\r\n")
    checker = directory / "checker.py"
    checker.write_text(CHECKER, encoding="utf-8")
    modes = {
        "alternating-checks": {"mode-a":"A", "mode-b":"B"},
        "named-conflict": {"mode-a":"A", "mode-b":"B"},
        "sandbox-write": {"acceptance":"legacy-ready"},
        "validation-progress": {"acceptance":"progress"},
        "noisy-validation": {"acceptance":"readiness"},
        "static-noise": {"acceptance":"readiness"},
    }.get(case, {})
    checks = directory / "checks.json"
    save(checks, {name: {"program":sys.executable, "args":[str(checker),mode],
                         "inputs":[str(checker)],"timeout_seconds":5} for name,mode in modes.items()})
    return root, checks


def run_case(exe, directory, case, paired=None, token=None):
    root, checks = prepare(directory, case)
    requests = []
    models = paired.Models() if paired else None
    if models:
        models.start(1)
    server = paired.bridge(token, "sourceharness", case, models, None) if paired else scripted_provider(
        lambda round_number: actions(case, round_number), requests)
    with server as url:
        env = {**os.environ, "OPENAI_BASE_URL": url, "OPENAI_MODEL": paired.MODEL if paired else "scripted",
               "OPENAI_API_KEY":"local-test-bridge"}
        env.pop("SOURCE_HARNESS_TEST_INTERRUPT", None)
        run = subprocess.run(
            [str(exe), "agent", "--root", str(root), "--session", "work", "--tool-profile", "compact",
             "--model", env["OPENAI_MODEL"], "--effort", "low", "--checks", str(checks), "--prompt", CASES[case]],
            capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=600, env=env,
        )
    journal = root / ".sourceharness/sessions/work"
    events = [json.loads(line) for line in (journal / "events.jsonl").read_bytes().splitlines()]
    wire = [
        json.loads((journal / "blobs" / event["data"]["input"]).read_bytes())
        for event in events if event["kind"] == "model_request"
    ]
    pauses = [event["data"] for event in events if event["kind"] == "paused"]
    verification = [event["data"]["passed"] for event in events if event["kind"] == "validation_receipt"]
    trace = []
    for event in events:
        if event["kind"] == "tool_result":
            detail = event["data"]
            guard = detail.get("guard") or {}
            trace.append({"round":detail["round"], "tool":detail["name"], "status":detail["status"],
                          "code":detail["code"], "guard":guard.get("kind"), "state":guard.get("state")})
    cost = paired.cost(models.rows) if paired else None
    data = {
        "case": case, "requests":len(wire), "exit":run.returncode,
        "natural_end":run.returncode == 0 and bool(run.stdout.strip()),
        "pauses":pauses, "validation_results":verification,
        "validation_outputs":len({event["data"]["streams"]["stderr"] for event in events if event["kind"]=="validation_receipt"}),
        "tool_counts":dict(Counter(row["tool"] for row in trace)),
        "errors":dict(Counter(row["code"] for row in trace if row["code"])),
        "prefix_pairs":max(0,len(wire)-1),
        "prefix_preserved":all(b["input"][:len(a["input"])]==a["input"] and b["instructions"]==a["instructions"]
                               and b["tools"]==a["tools"] for a,b in zip(wire,wire[1:])),
        "cost":cost, "trace":trace,
    }
    if case in ("edit-recovery", "sandbox-write"):
        legacy = root / "legacy.txt"
        data["independent_passed"] = legacy.is_file() and legacy.read_bytes() == b"kind=example\r\nstatus=ready\r\n"
    if case == "validation-progress":
        state = root / "state.txt"
        data["independent_passed"] = state.is_file() and state.read_bytes() == b"done\n"
    if not paired:
        if case in PAUSE_BY:
            data["passed"] = (run.returncode != 0 and len(pauses) == 1
                              and pauses[0]["reason"] == "LOOP_GUARD_TRIGGERED"
                              and pauses[0]["round"] <= PAUSE_BY[case] and data["prefix_preserved"])
        else:
            data["passed"] = (data["natural_end"] and not pauses and data.get("independent_passed", True)
                              and data["prefix_preserved"])
            if case in ("sandbox-write", "validation-progress"):
                data["passed"] = data["passed"] and verification[-2:] == [False, True]
    if paired:
        final = run.stdout.strip().replace(token, "[credential]")
        data["final"] = final[-2500:]
        save(directory / "provider-requests.json", models.rows)
    print(case, data["requests"], "requests", [(p["reason"],p["round"]) for p in pauses], flush=True)
    return data


def run_session_check(exe, out):
    root, checks = prepare(out, "quiet-match")
    state = root / "state.txt"
    requests = []
    scheduled = []
    results = []
    blobs = root / ".sourceharness/sessions/work/blobs"

    def result_data(result):
        return json.loads((blobs / result["data"]["output"]).read_bytes())

    with scripted_provider(lambda number: scheduled[number - 1], requests) as url:
        env = {**os.environ, "OPENAI_BASE_URL": url, "OPENAI_API_KEY": "local-test-bridge"}
        env.pop("SOURCE_HARNESS_TEST_INTERRUPT", None)
        command = [str(exe), "agent", "--root", str(root), "--session", "work", "--tool-profile", "compact",
                   "--model", "scripted", "--checks", str(checks)]

        def execute(*flags, kill=False):
            child_env = {**env, "SOURCE_HARNESS_TEST_INTERRUPT": "kill_after_file_replace"} if kill else env
            return subprocess.run([*command, *flags], env=child_env, capture_output=True, timeout=30).returncode

        def read_results(events):
            return [event for event in events if event["kind"] == "tool_result" and event["data"]["name"] == "read"]

        scheduled.extend([[tool("read", path="state.txt")], []])
        first_exit = execute("--prompt", "Read version one.")
        initial_reads = read_results(read_events(root))
        historical_ref = result_data(initial_reads[0])["result_ref"] if initial_reads else None
        results.append({"phase":1, "exit":first_exit, "historical_reference":historical_ref,
                        "passed":first_exit == 0 and historical_ref is not None})

        for phase in (2, 3, 4):
            start = len(read_events(root))
            external = f"external-{phase}\n".encode()
            state.write_bytes(external)
            attempts = [tool("write", path="state.txt", content=f"stale-{phase}\n")]
            if phase == 3:
                attempts.append(edit("status=draft", "status=ready"))
            reads = [tool("read", path="state.txt")]
            if phase == 4 and historical_ref:
                reads.append(tool("read", result_ref=historical_ref))
            scheduled.extend([attempts, reads, []])
            exit_code = execute("--prompt", f"Evaluate version {phase}.", kill=phase == 3)
            row = {"phase":phase, "exit":exit_code}
            if phase == 3 and exit_code == -signal.SIGKILL:
                evidence = interrupt_evidence(root, read_events(root)[start:])
                row["resume_exit"] = execute("--resume")
                receipts = [event for event in read_events(root)[start:] if event["kind"] == "edit_receipt"
                            and event["data"]["operation"] == evidence.get("operation")]
                evidence["outcomes"] = [event["data"]["outcome"] for event in receipts]
                evidence["passed"] = (evidence.get("candidate_present_before_resume") is True
                                      and evidence.get("receipt_recorded_before_resume") is False
                                      and evidence["outcomes"] == ["observed_target_on_recovery"]
                                      and row["resume_exit"] == 0)
                row["interruption"] = evidence
            events = read_events(root)[start:]
            rejections = [event for event in events if event["kind"] == "tool_result"
                          and event["data"]["code"] == "REVISION_CONFLICT"]
            rejected = result_data(rejections[0]) if rejections else {}
            row["revision_conflict"] = len(rejections) == 1
            row["diff"] = rejected.get("diff") or (rejected.get("planned_to_current") or {}).get("diff")
            row["protected_bytes"] = state.read_bytes() == external
            row["passed"] = (row.get("resume_exit", exit_code) == 0 and row["revision_conflict"]
                             and bool(row["diff"]) and row["protected_bytes"]
                             and (phase != 3 or row.get("interruption", {}).get("passed", False)))
            if phase == 4:
                live_and_saved = [result_data(event).get("content") for event in read_results(events)]
                row["live_and_saved_contents"] = live_and_saved
                row["passed"] = row["passed"] and live_and_saved == [external.decode(), "draft\n"]
            results.append(row)

    prefix = all(b["input"][:len(a["input"])] == a["input"] and b["instructions"] == a["instructions"]
                 and b["tools"] == a["tools"] for a, b in zip(requests, requests[1:]))
    report = {"binary_sha256":hashlib.sha256(exe.read_bytes()).hexdigest(),
              "phases":results, "requests":len(requests), "prefix_preserved":prefix,
              "passed":prefix and all(row["passed"] for row in results)}
    save(out / "results.json", report)
    print(f'Scripted continuous session: passed={report["passed"]}', flush=True)
    return int(not report["passed"])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--case", choices=list(CASES), action="append")
    parser.add_argument("--session-check", action="store_true",
                        help="Exercise four phases, stale writes, SIGKILL recovery and an immutable reference")
    parser.add_argument("--verify", action="store_true", help="Fail if a scripted case misses its expected result")
    parser.add_argument("--live-runner", type=Path, help="Published Pi comparison runner directory for live provider trials")
    args = parser.parse_args()
    exe, out = args.exe.resolve(), args.out.resolve()
    if args.session_check:
        if args.live_runner or args.case or sys.platform != "linux":
            parser.error("--session-check uses the scripted provider on Linux")
        return run_session_check(exe, out)
    paired = token = None
    if args.live_runner:
        if args.verify:
            parser.error("--verify requires the scripted provider")
        sys.path.insert(0, str(args.live_runner.resolve()))
        import compare_pi_public as paired
        paired.MAX_ATTEMPTS = 50
        token = getpass.getpass("OpenCode Go test credential (hidden): ")
        if not token.startswith("oc_sk_"):
            parser.error("Expected an OpenCode Go test credential")
    out.mkdir(parents=True)
    cases = args.case or (ORIGINAL if paired else list(CASES))
    with ThreadPoolExecutor(max_workers=3) as workers:
        reports = list(workers.map(lambda case: run_case(exe, out / case, case, paired, token), cases))
    save(out / "results.json", {"provider":paired.MODEL if paired else "scripted-responses",
                               "binary_sha256":hashlib.sha256(exe.read_bytes()).hexdigest(), "trials":reports})
    return int(args.verify and not all(row["passed"] for row in reports))


if __name__ == "__main__":
    raise SystemExit(main())
