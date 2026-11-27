"""Run the compact CLI against Qwen3.8 with independent workspace and answer checks."""

import argparse
from collections import Counter
from concurrent.futures import ThreadPoolExecutor, as_completed
from contextlib import contextmanager
import fcntl
import getpass
from hashlib import sha256
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import threading
import time
from urllib import error, parse, request
import uuid

from check_qwen_benchmark import digests, tree, workspace_check, write
from eval_natural_session import interrupt_evidence, reported_validation, resumable_transport_failure
from eval_session import read_events, request_cache, request_cost
from qwen_benchmark_cases import corpus, encode, manifest
from session_wire_audit import request_context


MODEL = "qwen3.8-flash"
BASE_URL = "https://opencode.ai/inference/go/openai/v1"
CHECKER = Path(__file__).with_name("check_qwen_benchmark.py").resolve()
GENERATOR = Path(__file__).with_name("qwen_benchmark_cases.py").resolve()


def save(path, value):
    temporary = path.with_suffix(path.suffix+".new")
    temporary.write_bytes(encode(value)+b"\n")
    temporary.replace(path)


class NoRedirect(request.HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


class Gateway:
    def __init__(self, base, model, key):
        self.endpoint = base+"/responses"
        self.model = model
        self.key = key
        self.blocked = threading.Event()
        self.lock = threading.Lock()
        self.failure = None
        self.sessions = {}

    def block(self, reason):
        with self.lock:
            if not self.blocked.is_set():
                self.failure = reason
                self.blocked.set()

    def session(self, case):
        record = {"id":"ses_"+uuid.uuid4().hex,"phase":0,"requests":[]}
        self.sessions[case] = record
        return record

    def forward(self, handler, run, raw):
        body = json.loads(raw)
        if body["model"] != self.model:
            handler.send_error(400,"Unexpected requested model")
            self.block("model_mismatch")
            return
        row = {"phase":run["phase"],"input_bytes":len(raw),"input_sha256":sha256(raw).hexdigest(),
               "status":None,"model":None,"terminal":None,"usage":None}
        run["requests"].append(row)
        if self.blocked.is_set():
            handler.send_error(503,"Benchmark gateway paused")
            row["failure"] = self.failure
            return
        started = time.monotonic()
        upstream = request.Request(self.endpoint,data=raw,headers={
            "Authorization":"Bearer "+self.key,"Content-Type":"application/json",
            "Accept":"text/event-stream","User-Agent":"SourceHarness/0.1.0",
            "x-opencode-session":run["id"],
        })
        opener = request.build_opener(NoRedirect())
        headers_sent = False
        try:
            with opener.open(upstream,timeout=105) as response:
                row["status"] = response.status
                handler.send_response(response.status)
                handler.send_header("Content-Type",response.headers.get("Content-Type","text/event-stream"))
                handler.end_headers()
                headers_sent = True
                for line in response:
                    if line.startswith(b"data:") and line[5:].strip() not in (b"", b"[DONE]"):
                        event = json.loads(line[5:])
                        state = event.get("response") or {}
                        if state.get("model"):
                            row["model"] = state["model"]
                        if event.get("type") in ("response.completed","response.incomplete","response.failed"):
                            row["terminal"] = event["type"]
                            row["usage"] = state.get("usage")
                            if row["model"] != self.model:
                                row["failure"] = "unexpected_response_model"
                                self.block("unexpected_response_model")
                                return
                    handler.wfile.write(line)
                    handler.wfile.flush()
        except error.HTTPError as exc:
            row["status"] = exc.code
            raw_error = exc.read(4096)
            if exc.code == 403 and b"Model access is disabled" in raw_error:
                row["failure"] = "model_access_disabled"
            else:
                row["failure"] = f"upstream_http_{exc.code}"
            if exc.code in (401,402,403,404,429):
                self.block(row["failure"])
            if not headers_sent:
                handler.send_error(exc.code,row["failure"])
        except (BrokenPipeError,ConnectionResetError):
            row["failure"] = "connection_closed"
        except (OSError,ValueError,KeyError,TypeError):
            row["failure"] = "unusable_upstream_response"
            if not headers_sent:
                handler.send_error(502,"Unusable upstream response")
        finally:
            row["seconds"] = round(time.monotonic()-started,4)

    @contextmanager
    def serve(self):
        gateway = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_):
                pass

            def do_POST(self):
                match = re.fullmatch(r"/case/([a-z0-9.\-]+)/v1/responses",self.path)
                if not match or match[1] not in gateway.sessions:
                    self.send_error(404)
                    return
                try:
                    raw = self.rfile.read(int(self.headers["Content-Length"]))
                    gateway.forward(self,gateway.sessions[match[1]],raw)
                except (ValueError,KeyError):
                    self.send_error(400,"Invalid Responses request")

        with ThreadingHTTPServer(("127.0.0.1",0),Handler) as server:
            thread = threading.Thread(target=server.serve_forever,daemon=True)
            thread.start()
            try:
                yield f"http://127.0.0.1:{server.server_port}"
            finally:
                server.shutdown()
                thread.join()


def oracle(directory, files, step):
    expected = step.expected
    spec = {key:expected[key] for key in ("exact","json","behavior") if key in expected}
    allowed = set(spec.get("exact",{})) | set(spec.get("json",{}))
    if "behavior" in spec:
        allowed.add(spec["behavior"]["path"])
    spec["protected"] = {name:digest for name,digest in digests(files).items() if name not in allowed}
    checks = dict(step.checks)
    if expected.get("validate"):
        checks["acceptance"] = spec
    config = {}
    for name, criteria in checks.items():
        path = directory / f"oracle-{name}.json"
        save(path,criteria)
        config[name] = {"program":sys.executable,"args":["-B",str(CHECKER),str(path)],
                        "inputs":[str(CHECKER),str(GENERATOR),str(path)],"timeout_seconds":25}
    config_path = directory / "checks.json"
    save(config_path,config)
    return spec, config_path


def invoke(exe, root, checks, model, base, prompt, timeout, interrupt=False):
    env = {key:value for key,value in os.environ.items()
           if key not in ("SOURCEHARNESS_OPENCODE_GO_API_KEY","SOURCEHARNESS_QWEN_API_KEY",
                          "SOURCE_HARNESS_TEST_INTERRUPT")}
    env.update(OPENAI_BASE_URL=base,OPENAI_API_KEY="loopback-benchmark")
    if interrupt:
        env["SOURCE_HARNESS_TEST_INTERRUPT"] = "kill_after_file_replace"
    command = [str(exe),"agent","--tool-profile","compact","--session","work","--root",str(root),
               "--checks",str(checks),"--model",model,"--effort","low"]
    command.extend(["--prompt",prompt] if prompt is not None else ["--resume"])
    started = time.monotonic()
    try:
        run = subprocess.run(command,env=env,capture_output=True,text=True,encoding="utf-8",errors="replace",timeout=timeout)
        return {"exit":run.returncode,"final":run.stdout.strip(),"error":run.stderr.strip(),
                "seconds":round(time.monotonic()-started,3)}
    except subprocess.TimeoutExpired:
        return {"exit":None,"final":"","error":f"Evaluation process exceeded {timeout} seconds",
                "seconds":round(time.monotonic()-started,3)}


def parse_answer(final):
    text = final.strip()
    if text.startswith("```") and text.endswith("```"):
        text = text.partition("\n")[2][:-3].strip()
    try:
        return json.loads(text)
    except (ValueError,UnicodeError):
        return None


def historical_evidence(events, final, expected):
    if not isinstance(final,dict) or (final.get("old"),final.get("current")) != (expected["old"],expected["current"]):
        return False
    source = final.get("source")
    if not isinstance(source,(str,int)) or isinstance(source,bool):
        return False
    match = re.fullmatch(r"(r)?([1-9]\d*)",str(source))
    if not match:
        return False
    event = next((event for event in events if event["seq"] == int(match[2])),None)
    if not event or event["kind"] != "artifact":
        return False
    data = event["data"]
    return data["source"].get("path") == expected["path"] and data["revision"] == expected["old_digest"]


def evaluate_phase(spec, expected, step_events, final, current, recovery, all_events):
    failures = workspace_check(current,spec)
    calls = [event["data"] for event in step_events if event["kind"] == "tool_result"]
    receipts = [event["data"] for event in step_events if event["kind"] == "validation_receipt"]
    pauses = [event["data"] for event in step_events if event["kind"] == "paused"]
    revision = sha256(encode(digests(current))).hexdigest()
    validated = any(row["check"] == "acceptance" and row["passed"] and row["workspace_revision"] == revision
                    and row.get("input_revisions_stable") is True and row["workspace_changed_during_check"] is False
                    for row in receipts)
    claims = reported_validation(final["final"],validated,revision)
    answer_value = parse_answer(final["final"])
    guard_pause = any(pause["reason"] == "LOOP_GUARD_TRIGGERED" for pause in pauses)
    required_checks = set(expected.get("required_checks",[]))
    if required_checks - {row["check"] for row in receipts}:
        failures.append("expected checks were not run")
    if "answer" in expected and encode(answer_value) != encode(expected["answer"]):
        if not (expected.get("blocked") and guard_pause):
            failures.append("answer differs")
    if "answer" in expected and not calls:
        failures.append("no workspace tool used")
    if "history" in expected and not historical_evidence(all_events,answer_value,expected["history"]):
        failures.append("old source citation missing or unverifiable")
    if expected.get("failed_check") and not any(not receipt["passed"] for receipt in receipts):
        failures.append("expected failed verification was not recorded")
    if set(expected.get("required_tools",[])) - {call["name"] for call in calls if call["status"] == "ok"}:
        failures.append("required tool outcome absent")
    independent = not failures
    if expected.get("validate") and not validated:
        failures.append("final workspace revision was not validated")
    if claims["unsupported_success_claim"] or claims["mismatched_validation_revisions"]:
        failures.append("unsupported validation claim")
    if recovery is not None and not recovery["passed"]:
        failures.append("SIGKILL recovery contract not satisfied")
    if final["exit"] != 0 and not (expected.get("blocked") and guard_pause):
        failures.append("CLI did not end naturally")
    return {"passed":not failures,"independent_passed":independent,"failures":failures,
            "validated":validated,"claims":claims,"pauses":pauses,
            "validation_results":[{"check":row["check"],"passed":row["passed"]} for row in receipts],
            "tools":dict(Counter(call["name"] for call in calls)),
            "errors":dict(Counter(call["code"] for call in calls if call.get("code"))),
            "guard_warnings":sum((call.get("guard") or {}).get("state") == "warn" for call in calls)}


def run_case(args, case, temporary, gateway, proxy):
    directory = temporary / case.id
    root = directory / "workspace"
    directory.mkdir()
    write(root,case.files)
    record = gateway.session(case.id)
    base = f"{proxy}/case/{case.id}/v1"
    start = time.monotonic()
    phases = []
    for number, step in enumerate(case.steps,1):
        if gateway.blocked.is_set():
            break
        record["phase"] = number
        checkpoint = len(read_events(root))
        write(root,step.external)
        spec, checks = oracle(directory,tree(root),step)
        runs = [invoke(args.exe,root,checks,args.model,base,step.prompt,args.phase_timeout,step.interrupt)]
        recovered = {"passed":False,"triggered":False} if step.interrupt else None
        if step.interrupt and runs[-1]["exit"] == -signal.SIGKILL:
            recovered.update(interrupt_evidence(root,read_events(root)[checkpoint:]))
            runs.append(invoke(args.exe,root,checks,args.model,base,None,args.phase_timeout))
        if resumable_transport_failure(runs[-1]) and not gateway.blocked.is_set():
            runs.append(invoke(args.exe,root,checks,args.model,base,None,args.phase_timeout,
                               step.interrupt and not recovered["triggered"] if recovered is not None else False))
            if step.interrupt and not recovered["triggered"] and runs[-1]["exit"] == -signal.SIGKILL:
                recovered.update(interrupt_evidence(root,read_events(root)[checkpoint:]))
                runs.append(invoke(args.exe,root,checks,args.model,base,None,args.phase_timeout))
        all_events = read_events(root)
        events = all_events[checkpoint:]
        if recovered and recovered["triggered"]:
            recovered["outcomes"] = [event["data"]["outcome"] for event in events
                                     if event["kind"] == "edit_receipt" and event["data"]["operation"] == recovered["operation"]]
            recovered["passed"] = (recovered["candidate_present_before_resume"]
                                   and not recovered["receipt_recorded_before_resume"]
                                   and recovered["outcomes"] == ["observed_target_on_recovery"])
        phase = evaluate_phase(spec,step.expected,events,runs[-1],tree(root),recovered,all_events)
        phase.update(phase=number,cost=request_cost(events),cache=request_cache(root,events),
                     executions=runs,external_paths=sorted(step.external),recovery=recovered)
        phases.append(phase)
        if runs[-1]["exit"] != 0:
            break

    events = read_events(root)
    contexts = request_context(root,events)
    pairs = sum(new["shared_input_prefix_items"] == old["input_items"]
                and new["instructions_match_previous"] and new["tools_match_previous"]
                for old,new in zip(contexts,contexts[1:]))
    observed = [row for row in record["requests"] if row.get("terminal")]
    verified = bool(observed) and all(row["model"] == args.model for row in observed)
    infrastructure_blocked = any(resumable_transport_failure(phase["executions"][-1]) for phase in phases)
    if gateway.blocked.is_set():
        unfinished = not phases or len(phases) < len(case.steps) or phases[-1]["executions"][-1]["exit"] != 0
        infrastructure_blocked |= unfinished and any(row.get("failure") for row in record["requests"])
    return {"case":case.id,"category":case.category,"shape":case.shape,
            "steps":len(case.steps),"phases":phases,"model_confirmed":verified,
            "evaluated":verified and not infrastructure_blocked,
            "passed":verified and len(phases) == len(case.steps) and all(phase["passed"] for phase in phases),
            "cost":request_cost(events),"cache":request_cache(root,events),"provider_requests":record["requests"],
            "prefix_exact_pairs":pairs,"prefix_pairs":max(0,len(contexts)-1),
            "seconds":round(time.monotonic()-start,3)}


def summary(rows, planned):
    evaluated = [row for row in rows if row["evaluated"]]
    category_counts, shape_counts = {}, {}
    for row in planned:
        for counts, label in ((category_counts,row["category"]),
                              (shape_counts,f"{row['category']}.{row['shape']}")):
            counts.setdefault(label,{"planned":0,"evaluated":0,"passed":0})["planned"] += 1
    for row in evaluated:
        for counts, label in ((category_counts,row["category"]),
                              (shape_counts,f"{row['category']}.{row['shape']}")):
            counts[label]["evaluated"] += 1
            counts[label]["passed"] += int(row["passed"])
    tokens = Counter()
    tools, codes, pauses, failures = (Counter() for _ in range(4))
    for row in rows:
        tokens.update(row["cost"]["known_usage"])
        for phase in row["phases"]:
            tools.update(phase["tools"])
            codes.update(phase["errors"])
            pauses.update(pause["reason"] for pause in phase["pauses"])
            failures.update(phase["failures"])
    budgets = sorted(row["cost"]["attempts"] for row in evaluated)
    durations = sorted(row["seconds"] for row in evaluated)
    return {"planned":len(planned),"evaluated":len(evaluated),"passed":sum(row["passed"] for row in evaluated),
            "attempted_cases":len(rows),"categories":category_counts,"shapes":shape_counts,"known_usage":dict(tokens),
            "requests":sum(row["cost"]["attempts"] for row in rows),
            "unmetered_requests":sum(row["cost"]["unknown_usage_attempts"] for row in rows),
            "requests_p50":budgets[(len(budgets)-1)//2] if budgets else None,
            "requests_p95":budgets[(len(budgets)*95-1)//100] if budgets else None,
            "seconds_p95":durations[(len(durations)*95-1)//100] if durations else None,
            "tools":dict(tools),"tool_errors":dict(codes),"pauses":dict(pauses),"failure_reasons":dict(failures),
            "cached_input_tokens":sum(row["cache"]["known_cached_input_tokens"] for row in rows),
            "requests_without_cache_usage":sum(row["cache"]["requests_without_cache_usage"] for row in rows),
            "prefix_pairs":sum(row["prefix_pairs"] for row in rows),
            "prefix_exact_pairs":sum(row["prefix_exact_pairs"] for row in rows)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe",type=Path)
    parser.add_argument("--out",type=Path,required=True)
    parser.add_argument("--base-url",default=BASE_URL)
    parser.add_argument("--model",default=MODEL)
    parser.add_argument("--workers",type=int,default=8)
    parser.add_argument("--limit",type=int,default=1000)
    parser.add_argument("--case",action="append")
    parser.add_argument("--phase-timeout",type=int,default=600)
    parser.add_argument("--plan-only",action="store_true")
    args = parser.parse_args()
    args.out = args.out.resolve()
    args.out.mkdir(parents=True,exist_ok=True)
    with (args.out/"benchmark.lock").open("a") as lock:
        try:
            fcntl.flock(lock.fileno(),fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            parser.error("A benchmark is already using this result directory")
        return benchmark(args,parser)


def benchmark(args, parser):
    all_cases = list(corpus())
    plan = manifest(all_cases)
    if len(plan) != 1000 or len({row["sha256"] for row in plan}) != 1000:
        raise ValueError("Benchmark must contain 1,000 unique workloads")
    plan_path = args.out/"manifest.json"
    if plan_path.is_file() and json.loads(plan_path.read_bytes()) != plan:
        parser.error("The corpus changed. Choose a new result directory.")
    save(plan_path,plan)
    if args.plan_only:
        print(json.dumps(summary([],plan),indent=2))
        return 0
    if args.exe is None or args.workers < 1 or not 1 <= args.limit <= 1000 or args.phase_timeout < 1:
        parser.error("Supply --exe and positive benchmark limits")
    args.exe = args.exe.resolve(strict=True)
    args.base_url = args.base_url.rstrip("/")
    url = parse.urlsplit(args.base_url)
    if (url.scheme != "https" or not url.hostname or url.username is not None or url.query
            or url.fragment or not url.path.endswith("/v1")):
        parser.error("Use an HTTPS API base URL ending in /v1")
    if not re.search(r"qwen[^\d]*3[.\-]8\b",args.model,re.I):
        parser.error("Use an explicit Qwen3.8 model ID")
    selected = [case for case in all_cases if not args.case or case.id in args.case]
    if args.case and set(args.case) != {case.id for case in selected}:
        parser.error("Unknown case ID")
    settings = {"base_url":args.base_url,"model":args.model,"binary_sha256":sha256(args.exe.read_bytes()).hexdigest(),
                "checker_sha256":sha256(CHECKER.read_bytes()).hexdigest(),"phase_timeout_seconds":args.phase_timeout}
    settings_path = args.out/"settings.json"
    if settings_path.is_file() and json.loads(settings_path.read_bytes()) != settings:
        parser.error("Model, gateway, executable, checker and timeout must match earlier results")
    save(settings_path,settings)
    results = args.out/"cases"
    attempts = args.out/"infrastructure"
    work = args.out/"work"
    for directory in (results,attempts,work):
        directory.mkdir(exist_ok=True)
    rows = {path.stem:json.loads(path.read_bytes()) for path in results.glob("*.json")}
    pending = [case for case in selected if case.id not in rows][:args.limit]
    if not pending:
        print(json.dumps(summary(list(rows.values()),plan),indent=2))
        return 0
    key = os.environ.get("SOURCEHARNESS_QWEN_API_KEY")
    if not key and url.hostname == "opencode.ai" and url.port in (None,443):
        key = os.environ.get("SOURCEHARNESS_OPENCODE_GO_API_KEY")
    key = key or getpass.getpass("Qwen benchmark credential (hidden): ")
    if not key or any(char.isspace() for char in key):
        parser.error("A nonempty API credential is required")
    if key.startswith("oc_sk_") and (url.hostname != "opencode.ai" or url.port not in (None,443)):
        parser.error("Use OpenCode Go credentials only with the OpenCode HTTPS endpoint")
    gateway = Gateway(args.base_url,args.model,key)
    run = work/uuid.uuid4().hex
    run.mkdir()
    with gateway.serve() as proxy:
        # Check model access before sending the rest of the cohort.
        ordered = iter(pending)
        pilot = next(ordered)

        def complete(case, data):
            data = json.loads(encode(data).decode().replace(key,"[credential]"))
            if data["evaluated"]:
                save(results/(case.id+".json"),data)
                rows[case.id] = data
            else:
                save(attempts/(case.id+"-"+run.name[:8]+".json"),data)
            report = summary(list(rows.values()),plan)
            report["stopped_by"] = gateway.failure
            rejected = list(attempts.glob("*.json"))
            report["infrastructure_attempts"] = len(rejected)
            report["infrastructure_requests"] = sum(
                json.loads(path.read_bytes())["cost"]["attempts"] for path in rejected)
            save(args.out/"summary.json",report)
            print(f"{len(rows)}/{len(plan)} {case.id} evaluated={data['evaluated']} passed={data['passed']} "
                  f"requests={data['cost']['attempts']}",flush=True)

        complete(pilot,run_case(args,pilot,run,gateway,proxy))
        if not gateway.blocked.is_set():
            with ThreadPoolExecutor(max_workers=args.workers) as pool:
                while not gateway.blocked.is_set():
                    batch = [case for _,case in zip(range(args.workers),ordered)]
                    if not batch:
                        break
                    futures = {pool.submit(run_case,args,case,run,gateway,proxy):case for case in batch}
                    for job in as_completed(futures):
                        complete(futures[job],job.result())
    if gateway.failure:
        print(f"Qwen gateway stopped the batch: {gateway.failure}",file=sys.stderr)
    return int(gateway.blocked.is_set() or any(case.id not in rows for case in pending))


if __name__ == "__main__":
    raise SystemExit(main())
