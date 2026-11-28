"""Run the Phase 1-4 conformance benchmark against scripted or live model turns."""

import argparse
from collections import Counter, deque
from contextlib import contextmanager
from hashlib import sha256
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import threading
import time
import uuid

from check_qwen_benchmark import tree, workspace_check, write
from eval_session import request_cost
from phase14_benchmark_cases import corpus, encode, manifest
from phase14_checker import markers

MODEL = "qwen3.8-flash"
BASE_URL = "https://opencode.ai/inference/go/openai/v1"
CHECKER = Path(__file__).with_name("phase14_checker.py").resolve()

SMOKE = ("atomic.atomic-rename.00", "atomic.atomic-abort.00", "atomic.atomic-rollback.00",
         "parallel.validate-fanout.00", "branch.branch-rewind.00", "branch.branch-green.00",
         "worker.worker-disjoint.00", "worker.worker-write-skew.00")


def save(path, value):
    temporary = path.with_suffix(path.suffix + ".new")
    temporary.write_bytes(encode(value) + b"\n")
    temporary.replace(path)


def parse_answer(final):
    text = final.strip()
    if text.startswith("```") and text.endswith("```"):
        text = text.partition("\n")[2][:-3].strip()
    try:
        return json.loads(text)
    except (ValueError, UnicodeError):
        return None


class ScriptedGateway:
    """Replay scripted tool turns over SSE so mechanism checks stay offline and deterministic."""

    def __init__(self, model):
        self.model = model
        self.turns = deque()
        self.emitted = []
        self.requests = []
        self.failure = None
        self.lock = threading.Lock()

    def load(self, script):
        self.turns = deque(script)
        self.emitted = []

    def response(self):
        with self.lock:
            if not self.turns:
                self.failure = "script_exhausted"
                raise ValueError("The scripted gateway ran out of turns")
            turn = self.turns.popleft()
            if isinstance(turn, str):
                output = [{"type": "message", "role": "assistant",
                           "content": [{"type": "output_text", "text": turn}]}]
            else:
                output = []
                for name, arguments in turn:
                    call_id = f"c{len(self.emitted) + 1}"
                    self.emitted.append(call_id)
                    output.append({"type": "function_call", "call_id": call_id, "name": name,
                                   "arguments": json.dumps(arguments)})
        return {"model": self.model, "output": output,
                "usage": {"input_tokens": 1024, "output_tokens": 48, "total_tokens": 1072,
                          "input_tokens_details": {"cached_tokens": 0}}}

    @contextmanager
    def serve(self):
        gateway = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                if not self.path.endswith("/v1/responses"):
                    self.send_error(404)
                    return
                try:
                    raw = self.rfile.read(int(self.headers["Content-Length"]))
                    gateway.requests.append({"bytes": len(raw), "sha256": sha256(raw).hexdigest()})
                    body = json.loads(raw)
                    if body.get("model") != gateway.model:
                        gateway.failure = "model_mismatch"
                        self.send_error(400, "Unexpected requested model")
                        return
                    response = gateway.response()
                except (ValueError, KeyError):
                    self.send_error(500, "Scripted turn unavailable")
                    return
                event = json.dumps({"type": "response.completed", "response": response})
                payload = f"event: response.completed\ndata: {event}\n\n".encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

        with ThreadingHTTPServer(("127.0.0.1", 0), Handler) as server:
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                yield f"http://127.0.0.1:{server.server_port}"
            finally:
                server.shutdown()
                thread.join()


def materialize(directory, case):
    config = {}
    for name, check in case.checks.items():
        path = directory / f"check-{name}.json"
        save(path, check)
        config[name] = {"program": sys.executable, "args": ["-B", str(CHECKER), str(path)],
                        "description": name, "inputs": [str(CHECKER), str(path)], "timeout_seconds": 30}
    save(directory / "checks.json", config)
    return directory / "checks.json"


def read_events(root, session):
    path = root / ".sourceharness" / "sessions" / session / "events.jsonl"
    if not path.is_file():
        return [], b""
    raw = path.read_bytes()
    return [json.loads(line) for line in raw.decode("utf-8").splitlines()], raw


def read_body(root, session, event):
    blob = root / ".sourceharness" / "sessions" / session / "blobs" / event["data"]["input"]
    return json.loads(blob.read_bytes())


def prefix_stats(root, session, events):
    pairs = exact = 0
    previous = None
    for event in (e for e in events if e["kind"] == "model_request"):
        body = read_body(root, session, event)
        if previous is not None:
            pairs += 1
            items = body.get("input", [])
            if (items[:len(previous["input"])] == previous["input"]
                    and body.get("instructions") == previous.get("instructions")
                    and body.get("tools") == previous.get("tools")):
                exact += 1
        previous = body
    return pairs, exact


def invoke(args, root, session, checks, base, prompt):
    env = {key: value for key, value in os.environ.items()
           if key not in ("SOURCEHARNESS_QWEN_API_KEY", "SOURCEHARNESS_OPENCODE_GO_API_KEY",
                          "OPENAI_API_KEY", "SOURCE_HARNESS_TEST_INTERRUPT")}
    env.update(OPENAI_BASE_URL=base, OPENAI_API_KEY="loopback-benchmark")
    command = [str(args.exe), "agent", "--session", session, "--root", str(root),
               "--checks", str(checks), "--model", args.model, "--effort", "low"]
    command.extend(["--prompt", prompt] if prompt is not None else ["--resume"])
    started = time.monotonic()
    try:
        run = subprocess.run(command, env=env, capture_output=True, text=True,
                             encoding="utf-8", errors="replace", timeout=args.action_timeout)
        return {"exit": run.returncode, "final": run.stdout.strip(), "error": run.stderr.strip(),
                "seconds": round(time.monotonic() - started, 3)}
    except subprocess.TimeoutExpired:
        return {"exit": None, "final": "", "error": f"Action exceeded {args.action_timeout} seconds",
                "seconds": round(time.monotonic() - started, 3)}


def run_cli(args, argv):
    started = time.monotonic()
    try:
        run = subprocess.run([str(args.exe), *argv], capture_output=True, text=True,
                             encoding="utf-8", errors="replace", timeout=args.action_timeout)
        return {"exit": run.returncode, "out": run.stdout.strip(), "err": run.stderr.strip(),
                "seconds": round(time.monotonic() - started, 3)}
    except subprocess.TimeoutExpired:
        return {"exit": None, "out": "", "err": f"Command exceeded {args.action_timeout} seconds",
                "seconds": round(time.monotonic() - started, 3)}


def resolve_event(events, spec):
    if spec != "green":
        return int(spec)
    receipt = next((e for e in reversed(events)
                    if e["kind"] == "validation_receipt" and e["data"]["passed"] is True), None)
    if receipt is None:
        raise ValueError("This case has no passing validation receipt to branch from")
    return receipt["seq"]


def concurrency_ratio(events):
    """Wall time of a multi-check turn against the sum of the checks' own durations."""
    ratios = []
    for response in (event for event in events if event["kind"] == "model_response"):
        round_number = response["data"]["round"]
        batch = [event for event in events
                 if event["kind"] == "tool_result" and event["data"]["round"] == round_number
                 and event["data"]["name"] == "validate"]
        if len(batch) < 2:
            continue
        wall = max(event["ms"] for event in batch) - response["ms"]
        elapsed = sum(event["data"].get("elapsed_ms") or 0 for event in batch)
        if wall > 0 and elapsed:
            ratios.append(round(elapsed / wall, 2))
    return max(ratios) if ratios else None


def validated_checks(events):
    latest = {}
    for event in events:
        if event["kind"] == "validation_receipt":
            latest[event["data"]["check"]] = event["data"].get("passed") is True
    return sorted(name for name, passed in latest.items() if passed)


def scripted_invariants(invariants, events, results, emitted, run):
    failures = []
    receipts = [e["data"] for e in events if e["kind"] == "changeset_receipt" and e["data"]["status"] == "ok"]
    edits = [e["data"] for e in events if e["kind"] == "edit_receipt" and e["data"]["status"] == "ok"]
    codes = [row.get("code") for row in results if row.get("code")]
    rollback = next((e["data"] for e in events if e["kind"] == "rollback_receipt"), None)
    for key, value in invariants.items():
        if key == "changesets" and len(receipts) != value:
            failures.append(f"expected {value} atomic changesets, observed {len(receipts)}")
        elif key == "tx_files" and any(len(row["files"]) != value for row in receipts):
            failures.append(f"expected {value} files per atomic changeset")
        elif key == "no_untracked_edits" and any("tx_id" not in row for row in edits):
            failures.append("an edit outside the atomic changeset was committed")
        elif key == "no_edits" and edits:
            failures.append("a file edit was committed")
        elif key == "errors":
            missing = [code for code in value if code not in codes]
            if missing:
                failures.append(f"expected tool error codes {missing}, observed {codes}")
        elif key == "tool_errors" and value == 0 and any(row.get("status") == "error" for row in results):
            failures.append("an unexpected tool error was recorded")
        elif key == "validated":
            missing = [name for name in value if name not in validated_checks(events)]
            if missing:
                failures.append(f"checks not passed: {missing}")
        elif key == "order" and emitted != [row["call_id"] for row in results]:
            failures.append("tool results were reordered against their call order")
        elif key == "parallel":
            ratio = concurrency_ratio(events) or 0
            if ratio < value["min_ratio"]:
                failures.append(f"validate concurrency ratio {ratio:.2f} below {value['min_ratio']}")
        elif key == "rollback_checkpoint" and (not rollback or rollback.get("checkpoint_event") != value):
            failures.append(f"rollback checkpoint was {rollback and rollback.get('checkpoint_event')}, expected {value}")
        elif key == "rollback_paths" and (not rollback or len(rollback.get("paths") or []) != value):
            failures.append("rollback did not restore the whole changeset")
    return failures


def observable_invariants(invariants, events):
    failures = []
    if "validated" in invariants:
        missing = [name for name in invariants["validated"] if name not in validated_checks(events)]
        if missing:
            failures.append(f"checks not passed: {missing}")
    return failures


def accepted_state(options, before, after):
    """A case may accept more than one correct end state; partial writes are never one."""
    for option in options:
        errors = workspace_check(after, option["workspace"]) if "workspace" in option else []
        if option.get("unchanged") and after != before:
            errors.append("workspace changed")
        if not errors:
            return []
    return ["workspace matches none of the accepted end states"]


def score_agent(action, run, events, before, after, emitted, driver):
    expect = action["expect"]
    failures = []
    if "any_of" in expect:
        failures.extend(accepted_state(expect["any_of"], before, after))
    else:
        if "workspace" in expect:
            failures.extend(workspace_check(after, expect["workspace"]))
        if expect.get("unchanged") and after != before:
            failures.append("workspace changed")
    if "markers" in expect:
        failures.extend(markers(after, {"files": expect["markers"]}))
    if "answer" in expect and encode(parse_answer(run["final"])) != encode(expect["answer"]):
        failures.append("answer differs")
    if run["exit"] != 0:
        failures.append("CLI did not end naturally")
    results = [e["data"] for e in events if e["kind"] == "tool_result"]
    invariants = expect.get("invariants", {})
    failures.extend(scripted_invariants(invariants, events, results, emitted, run) if driver == "scripted"
                    else observable_invariants(invariants, events))
    return {"action": "agent", "session": action["session"], "passed": not failures, "failures": failures,
            "exit": run["exit"], "final": run["final"][:400], "seconds": run["seconds"],
            "tools": dict(Counter(row["name"] for row in results)),
            "errors": dict(Counter(row["code"] for row in results if row.get("code"))),
            "validated": validated_checks(events), "cost": request_cost(events),
            "concurrency": concurrency_ratio(events)}


def score_branch(action, run, root, source_raw, event, after):
    expect = action["expect"]
    failures = []
    info = None
    try:
        info = json.loads(run["out"])
    except ValueError:
        failures.append("branch did not print its metadata")
    if run["exit"] != 0:
        failures.append("branch command failed")
    target_events, target_raw = read_events(root, action["to"])
    source_prefix = b"".join(source_raw.splitlines(keepends=True)[:event])
    target_prefix = b"".join(target_raw.splitlines(keepends=True)[:event])
    if target_prefix != source_prefix:
        failures.append("branch did not reproduce the 0..=K event prefix byte for byte")
    appended = target_events[event:]
    if len(appended) > 1 or (appended and appended[0]["kind"] != "steer"):
        failures.append(f"branch appended unexpected events: {[e['kind'] for e in appended]}")
    if "prefix_events" in expect and len(target_events) < expect["prefix_events"]:
        failures.append("branch dropped part of the event prefix")
    if info and info.get("fork_event") != event:
        failures.append("branch metadata reports another fork event")
    if info and info.get("lesson") != (action["lesson"].strip() or None):
        failures.append("branch lesson was not recorded")
    if "restored" in expect:
        failures.extend(workspace_check(after, expect["restored"]))
    return {"action": "branch", "session": action["to"], "passed": not failures, "failures": failures,
            "exit": run["exit"], "fork_event": event, "events": len(target_events), "seconds": run["seconds"]}


def compare_prefix(root, pending):
    """The first branch request must reuse the mainline request prefix byte for byte."""
    source_events, _ = read_events(root, pending["source"])
    target_events, _ = read_events(root, pending["target"])
    ahead = next((e for e in source_events if e["seq"] > pending["event"] and e["kind"] == "model_request"), None)
    first = next((e for e in target_events if e["seq"] > pending["event"] and e["kind"] == "model_request"), None)
    if ahead is None or first is None:
        return None
    expected = read_body(root, pending["source"], ahead)
    observed = read_body(root, pending["target"], first)
    return (observed.get("instructions") == expected.get("instructions")
            and observed.get("tools") == expected.get("tools")
            and observed.get("input", [])[:len(expected.get("input", []))] == expected.get("input"))


def score_merge(action, run, before, after):
    expect = action["expect"]
    failures = []
    report = None
    try:
        report = json.loads(run["out"])
    except ValueError:
        failures.append("merge did not print a report")
    if report is not None:
        for key in ("status", "code", "head_changed_paths", "worker_changed_paths", "changed_paths", "conflicts"):
            if key in expect and report.get(key) != expect[key]:
                failures.append(f"merge {key} was {report.get(key)}, expected {expect[key]}")
        if "failed_checks" in expect:
            observed = sorted(row["check"] for row in report.get("failed_checks") or [])
            if observed != sorted(expect["failed_checks"]):
                failures.append(f"merge failed checks were {observed}")
    if expect.get("unchanged") and after != before:
        failures.append("mainline workspace was polluted by a rejected merge")
    if "workspace" in expect:
        failures.extend(workspace_check(after, expect["workspace"]))
    if expect.get("status") == "ok" and run["exit"] != 0:
        failures.append("merge command failed")
    return {"action": "merge", "worker": action["from"], "passed": not failures, "failures": failures,
            "status": report and report.get("status"), "code": report and report.get("code"),
            "changed": report and report.get("changed_paths"), "seconds": run["seconds"]}


def run_case(args, case, temporary, gateway, proxy):
    directory = temporary / case.id
    directory.mkdir()
    checks = materialize(directory, case)
    root = directory / "main"
    write(root, case.files)
    token = "case-" + re.sub(r"[^a-z0-9.\-]", "-", case.id)
    base = f"{proxy}/case/{token}/v1"
    if args.driver == "live":
        gateway.session(token)
    records = []
    pending = None
    started = time.monotonic()
    for action in case.actions:
        kind = action["kind"]
        if kind == "agent":
            target = directory / action["root"] if action["root"] != "main" else root
            before = tree(target)
            if args.driver == "scripted":
                gateway.load(action["script"])
            run = invoke(args, target, action["session"], checks, base,
                         None if action["resume"] else action["prompt"])
            emitted = list(gateway.emitted) if args.driver == "scripted" else []
            events, _ = read_events(target, action["session"])
            after = tree(target)
            record = score_agent(action, run, events, before, after, emitted, args.driver)
            if pending and pending["target"] == action["session"]:
                record["prefix_request"] = compare_prefix(root, pending)
                if action["expect"].get("invariants", {}).get("prefix_request") and record["prefix_request"] is False:
                    record["failures"].append("branch request prefix was not reproduced byte for byte")
                    record["passed"] = False
                pending = None
            records.append(record)
        elif kind == "branch":
            source_events, source_raw = read_events(root, action["from"])
            event = resolve_event(source_events, action["event"])
            run = run_cli(args, ["branch", "--root", str(root), "--from", action["from"], "--to", action["to"],
                                 "--event", str(event), "--lesson", action["lesson"]])
            records.append(score_branch(action, run, root, source_raw, event, tree(root)))
            pending = {"source": action["from"], "target": action["to"], "event": event}
        elif kind == "spawn":
            run = run_cli(args, ["worker", "spawn", "--root", str(root), "--name", action["name"],
                                 "--to", str(directory / action["name"])])
            records.append({"action": "spawn", "worker": action["name"], "passed": run["exit"] == 0,
                            "failures": [] if run["exit"] == 0 else [run["err"][:200]],
                            "seconds": run["seconds"]})
        else:
            before = tree(root)
            run = run_cli(args, ["worker", "merge", "--root", str(root), "--from", str(directory / action["from"]),
                                 "--session", action["session"], "--checks", str(checks)])
            records.append(score_merge(action, run, before, tree(root)))
    events, _ = read_events(root, "main")
    pairs, exact = prefix_stats(root, "main", events)
    return {"case": case.id, "category": case.category, "shape": case.shape,
            "evaluated": True, "passed": all(record["passed"] for record in records),
            "actions": records, "requests": sum(record.get("cost", {}).get("attempts", 0) for record in records),
            "cost": request_cost(events), "prefix_pairs": pairs, "prefix_exact": exact,
            "seconds": round(time.monotonic() - started, 3)}


def summary(rows, planned):
    evaluated = [row for row in rows if row["evaluated"]]
    categories, shapes = {}, {}
    for row in planned:
        for counts, label in ((categories, row["category"]), (shapes, f"{row['category']}.{row['shape']}")):
            counts.setdefault(label, {"planned": 0, "evaluated": 0, "passed": 0})["planned"] += 1
    for row in evaluated:
        for counts, label in ((categories, row["category"]), (shapes, f"{row['category']}.{row['shape']}")):
            counts[label]["evaluated"] += 1
            counts[label]["passed"] += int(row["passed"])
    failures, tools, merge_gate, ratios = Counter(), Counter(), Counter(), []
    for row in rows:
        for record in row["actions"]:
            failures.update(record["failures"])
            tools.update(record.get("tools", {}))
            if record["action"] == "merge":
                merge_gate[record["status"] or "unreported"] += 1
            if record.get("concurrency"):
                ratios.append(record["concurrency"])
    budgets = sorted(row["requests"] for row in evaluated)
    durations = sorted(row["seconds"] for row in evaluated)
    usage = {key: sum(row["cost"]["known_usage"].get(key, 0) for row in rows)
             for key in ("input_tokens", "output_tokens", "total_tokens")}
    return {"planned": len(planned), "evaluated": len(evaluated), "passed": sum(row["passed"] for row in evaluated),
            "attempted_cases": len(rows), "categories": categories, "shapes": shapes,
            "actions": sum(len(row["actions"]) for row in rows),
            "requests": sum(row["requests"] for row in rows),
            "requests_p50": budgets[(len(budgets) - 1) // 2] if budgets else None,
            "requests_p95": budgets[(len(budgets) * 95 - 1) // 100] if budgets else None,
            "seconds_p95": durations[(len(durations) * 95 - 1) // 100] if durations else None,
            "tools": dict(tools), "merge_gate": dict(merge_gate), "failure_reasons": dict(failures),
            "known_usage": usage,
            "validate_concurrency": {"samples": len(ratios), "p50": sorted(ratios)[(len(ratios) - 1) // 2] if ratios else None,
                                     "max": max(ratios) if ratios else None},
            "prefix_pairs": sum(row["prefix_pairs"] for row in rows),
            "prefix_exact_pairs": sum(row["prefix_exact"] for row in rows)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe", type=Path)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--driver", choices=("scripted", "live"), default="scripted")
    parser.add_argument("--base-url", default=BASE_URL)
    parser.add_argument("--model", default=MODEL)
    parser.add_argument("--limit", type=int, default=100)
    parser.add_argument("--case", action="append")
    parser.add_argument("--smoke", action="store_true")
    parser.add_argument("--plan-only", action="store_true")
    parser.add_argument("--action-timeout", type=int, default=300)
    args = parser.parse_args()
    args.out = args.out.resolve()
    args.out.mkdir(parents=True, exist_ok=True)
    cases = list(corpus())
    plan = manifest(cases)
    if len(plan) != 100 or len({row["sha256"] for row in plan}) != 100:
        raise ValueError("The Phase 1-4 corpus must contain 100 unique workloads")
    plan_path = args.out / "manifest.json"
    if plan_path.is_file() and json.loads(plan_path.read_bytes()) != plan:
        parser.error("The corpus changed. Choose a new result directory.")
    save(plan_path, plan)
    if args.plan_only:
        print(json.dumps(summary([], plan), indent=2))
        return 0
    if args.exe is None:
        parser.error("Supply --exe")
    args.exe = args.exe.resolve(strict=True)
    selected = [case for case in cases if case.id in set(args.case or ())] if args.case else cases
    if args.smoke:
        selected = [case for case in cases if case.id in SMOKE]
    if args.case and set(args.case) != {case.id for case in selected}:
        parser.error("Unknown case ID")
    selected = selected[:args.limit]
    if not 1 <= args.limit <= 100:
        parser.error("--limit must be within 1..100")
    settings = {"driver": args.driver, "model": args.model, "binary_sha256": sha256(args.exe.read_bytes()).hexdigest(),
                "checker_sha256": sha256(CHECKER.read_bytes()).hexdigest()}
    settings_path = args.out / "settings.json"
    if settings_path.is_file() and json.loads(settings_path.read_bytes()) != settings:
        parser.error("Driver, model, executable and checker must match earlier results")
    save(settings_path, settings)
    results, attempts, work = args.out / "cases", args.out / "attempts", args.out / "work"
    for directory in (results, attempts, work):
        directory.mkdir(exist_ok=True)
    rows = {path.stem: json.loads(path.read_bytes()) for path in results.glob("*.json")}
    pending = [case for case in selected if case.id not in rows]
    if not pending:
        print(json.dumps(summary(list(rows.values()), manifest(selected)), indent=2))
        return 0
    run = work / uuid.uuid4().hex
    run.mkdir()
    if args.driver == "live":
        from eval_qwen_benchmark import Gateway
        key = os.environ.get("SOURCEHARNESS_QWEN_API_KEY") or os.environ.get("OPENAI_API_KEY")
        if not key:
            parser.error("Live mode needs an API credential in the environment")
        gateway = Gateway(args.base_url.rstrip("/"), args.model, key)
        server = gateway.serve()
    else:
        gateway = ScriptedGateway(args.model)
        server = gateway.serve()
    with server as proxy:
        for case in pending:
            try:
                row = run_case(args, case, run, gateway, proxy)
            except Exception as error:  # A broken case must not discard the rest of the cohort.
                save(attempts / (case.id + "-" + run.name[:8] + ".json"),
                     {"case": case.id, "error": str(error)})
                print(f"{case.id} failed to run: {error}", file=sys.stderr)
                continue
            save(results / (case.id + ".json"), row)
            rows[case.id] = row
            report = summary(list(rows.values()), manifest(selected))
            report["stopped_by"] = gateway.failure
            save(args.out / "summary.json", report)
            print(f"{len(rows)}/{len(selected)} {case.id} passed={row['passed']} "
                  f"actions={len(row['actions'])} seconds={row['seconds']}", flush=True)
            if gateway.failure:
                print(f"Gateway stopped the batch: {gateway.failure}", file=sys.stderr)
                break
    return int(any(case.id not in rows for case in selected))


if __name__ == "__main__":
    raise SystemExit(main())
