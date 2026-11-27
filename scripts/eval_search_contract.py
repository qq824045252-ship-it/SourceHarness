"""Paired live evaluation of the kernel CLI with exact Responses request and SSE archives."""

import argparse
from collections import Counter
from concurrent.futures import ThreadPoolExecutor
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import random
import re
import subprocess
from threading import Thread
import time
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen

from search_evidence import grade_answer, new_calls, trace_evidence
from session_eval_cases import encode, sha


def scope_case(size, seed):
    rng = random.Random(seed)
    lines = []
    review = []
    spans = {}
    for section, length, count in zip(
        ("Release Review", "Setup Notes", "Legacy Archive"),
        (size // 3, size // 4, size - size // 3 - size // 4),
        (11 + seed, 6 + seed % 3, 8 + seed % 5),
    ):
        first = len(lines) + 1
        selected = set(rng.sample(range(7, length), count))
        lines.append(f"## {section}\n")
        for index in range(length):
            state = "REVIEW" if index in selected else "CLEARED"
            card_id = len(lines)
            lines.append(f"card CARD-{card_id:05d}: {state} source=deployment zone=west note=sync inventory ledger\n")
            if state == "REVIEW":
                review.append(len(lines))
        spans[section] = [first, len(lines)]
    expected = {"total": len(review)}
    for key, name in (("release", "Release Review"), ("setup", "Setup Notes"), ("archive", "Legacy Archive")):
        start, end = spans[name]
        expected[key] = sum(start <= line <= end for line in review)
    expected["excluding_archive"] = expected["total"] - expected["archive"]
    prompt = (
        "Count cards marked REVIEW in the full report and separately in Release Review, Setup Notes, "
        "and Legacy Archive. Give the count across the report excluding Legacy Archive. "
        "Do not list individual card IDs. Return one JSON object with integer values for keys "
        "total, release, setup, archive, and excluding_archive. Do not add commentary."
    )
    return "".join(lines), prompt, {
        "kind": "scope", "line_count": len(lines), "expected": expected,
        "review_rows": review, "count_ranges": [[1, len(lines)], *spans.values()],
    }


def group_case(distinct, seed):
    rng = random.Random(seed)
    frequencies = {f"BATCH-{i:03d}": 1 + ((i + seed) * 17 % 9) for i in range(1, distinct + 1)}
    records = list(Counter(frequencies).elements())
    rng.shuffle(records)
    lines = ["## Delivery log\n"]
    lines.extend(
        f"delivered batch={batch} audit_sequence={i:05d} node=warehouse inventory event posted successfully\n"
        for i, batch in enumerate(records)
    )
    top = sorted(frequencies.items(), key=lambda group: (-group[1], group[0]))[:4]
    expected = {
        "total_records": len(records), "distinct_ids": distinct,
        "duplicate_ids": sum(count > 1 for count in frequencies.values()),
        "most_frequent": [[name, count] for name, count in top],
    }
    prompt = (
        "Across the whole delivery log count delivered records, distinct batch IDs, and the number of "
        "distinct batch IDs that occur at least twice. List the four most frequent IDs in descending "
        "frequency, breaking ties by ID in ascending order. Return one JSON object with keys "
        "total_records, distinct_ids, duplicate_ids, and most_frequent (an array of [ID, count] pairs)."
    )
    return "".join(lines), prompt, {
        "kind": "group", "line_count": len(lines), "expected": expected, "frequencies": frequencies,
    }


def samples_case(size, seed):
    document, _, source = scope_case(size, seed)
    bounds = source["count_ranges"][1]
    selected = [line for line in source["review_rows"] if bounds[0] <= line <= bounds[1]]
    prompt = (
        "Count all cards marked REVIEW in the Release Review section. Give the source line numbers of "
        "the first two cards marked REVIEW in that section as examples. Do not list individual card IDs. "
        "Return one JSON object with keys total and first_two_lines. Do not add commentary."
    )
    return document, prompt, source | {
        "count_ranges": [bounds], "sample_rows": selected[:2],
        "expected": {"total": len(selected), "first_two_lines": selected[:2]},
    }


def pages_case():
    visible = [
        f'Row {i:03d} audit "run\\{i:04X}" {("verified checksum envelope " * 4)}end-{(0x3142 + i * 1879) % 65535:04X}'
        for i in range(1, 221)
    ]
    document = "".join(line + ("\r\n" if i % 2 else "\n") for i, line in enumerate(visible))
    prompt = (
        "Read and audit the entire original line interval 44 through 179 inclusive. Follow the read "
        "tool's continuation when a page cannot fit. Then return one JSON object with keys line_44, "
        "line_117, and line_179. Each value must be the exact original line text without its line ending."
    )
    expected = {f"line_{i}": visible[i - 1] for i in (44, 117, 179)}
    return document, prompt, {
        "kind": "pages", "line_count": len(visible), "expected": expected, "read_range": [44, 179],
    }


def forwarder(output, base_url, token):
    class CaptureHandler(BaseHTTPRequestHandler):
        def do_POST(self):
            trial, endpoint = self.path.lstrip("/").split("/")
            if endpoint != "responses" or not re.fullmatch(r"[a-z0-9-]+", trial):
                self.send_error(404)
                return
            path = output / trial
            request = self.rfile.read(int(self.headers["Content-Length"]))
            number = len(list(path.glob("*.request.json"))) + 1
            (path / f"{number:02}.request.json").write_bytes(request)
            upstream = Request(
                f"{base_url}/responses", data=request,
                headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json", "Accept": "text/event-stream"},
            )
            try:
                with urlopen(upstream, timeout=125) as response:
                    status = response.status
                    content_type = response.headers["Content-Type"]
                    payload = response.read()
            except HTTPError as error:
                status, content_type, payload = error.code, error.headers["Content-Type"], error.read()
            except (URLError, OSError) as error:
                status, content_type, payload = 502, "text/plain", str(error).encode()
            (path / f"{number:02}.response.sse").write_bytes(payload)
            (path / f"{number:02}.status").write_text(str(status), encoding="ascii")
            self.send_response(status)
            self.send_header("Content-Type", content_type or "text/plain")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            try:
                self.wfile.write(payload)
            except (ConnectionAbortedError, ConnectionResetError, BrokenPipeError):
                pass

        def log_message(self, *_args):
            pass

    return ThreadingHTTPServer(("127.0.0.1", 0), CaptureHandler)


def completed_response(raw):
    completed = {}
    items = {}
    for packet in re.split(rb"\r?\n\r?\n", raw):
        data = b"\n".join(line[5:].lstrip(b" ") for line in packet.splitlines() if line.startswith(b"data:"))
        if not data or data == b"[DONE]":
            continue
        event = json.loads(data)
        if event.get("type") == "response.completed" or packet.splitlines()[0] == b"event: response.completed":
            completed = event.get("response", event)
        if event.get("type") == "response.output_item.done" and "item" in event:
            items[event["output_index"]] = event["item"]
    if completed and not completed.get("output") and items:
        completed["output"] = [items[index] for index in sorted(items)]
    return completed


def score(path, case, document, result):
    requests = [json.loads(p.read_bytes()) for p in sorted(path.glob("*.request.json"))]
    responses = [
        completed_response(p.read_bytes()) if (path / (p.name.split(".")[0] + ".status")).read_text() == "200" else {}
        for p in sorted(path.glob("*.response.sse"))
    ]
    calls = []
    for index in range(1, len(requests)):
        calls.extend(new_calls(requests[index - 1]["input"], requests[index]["input"], index))
    answer = result["stdout"].partition("--- FINAL ANSWER ---\n")[2].strip()
    initial = requests[0] if requests else {}
    source = initial["input"][0]["content"] if initial else ""
    ref = re.search(r"Use `result_ref` = `([^`]+)`", source)
    evidence = trace_evidence(calls, case, document, ref[1] if ref else None)
    grade = grade_answer(answer, case)
    total_issued = sum(
        item.get("type") == "function_call" for response in responses for item in response.get("output", [])
    )
    usage = [response.get("usage", {}).get("total_tokens") for response in responses]
    return {
        "exit": result["exit"], "elapsed_ms": result["elapsed_ms"],
        "answer": answer, "grade": grade, "evidence": evidence,
        "accepted": result["exit"] == 0 and grade["passed"] and "sufficient" in evidence["points"],
        "requests": len(requests), "calls_issued": total_issued,
        "undelivered_call_results": max(0, total_issued - len(calls)),
        "known_total_tokens": sum(count for count in usage if count is not None),
        "unknown_usage_requests": len(requests) - sum(count is not None for count in usage),
        "provider_reported_models": sorted({response["model"] for response in responses if response.get("model")}),
        "tools_sha256": sha(encode(initial["tools"])) if initial else None,
        "first_request_except_tools_sha256": sha(encode({k: v for k, v in initial.items() if k != "tools"})),
        "calls": calls,
    }


def run_trial(root, name, case, document, prompt, fixture, binary, model, effort, port):
    folder = root / name
    folder.mkdir()
    env = os.environ | {"OPENAI_BASE_URL": f"http://127.0.0.1:{port}/{name}", "OPENAI_API_KEY": "local-capture"}
    command = [str(binary), "run", "--file", str(fixture), "--prompt", prompt, "--strategy", "inline",
               "--model", model, "--effort", effort]
    start = time.monotonic()
    try:
        run = subprocess.run(command, capture_output=True, text=True, encoding="utf8", env=env, timeout=320)
        result = {"exit": run.returncode, "stdout": run.stdout, "stderr": run.stderr}
    except subprocess.TimeoutExpired as error:
        result = {"exit": None, "stdout": (error.stdout or b"").decode("utf8", "replace"), "stderr": str(error)}
    result["elapsed_ms"] = round(1000 * (time.monotonic() - start))
    for field in ("stdout", "stderr"):
        (folder / f"{field}.txt").write_text(result[field], encoding="utf8")
    return score(folder, case, document, result)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--model", default="auto/deepseek-v4.1-flash-1017")
    parser.add_argument("--effort", default="low")
    parser.add_argument("--workers", type=int, default=3)
    parser.add_argument("--include-samples", action="store_true")
    args = parser.parse_args()
    root = args.out.resolve()
    root.mkdir(parents=True)
    cases = [*(scope_case(size, seed) for size, seed in ((60, 1), (200, 2), (810, 3))),
             group_case(29, 4), group_case(38, 5), pages_case()]
    if args.include_samples:
        cases.extend((samples_case(170, 6), samples_case(760, 7)))
    binaries = {"baseline": args.baseline.resolve(), "candidate": args.candidate.resolve()}
    (root / "fixtures").mkdir()
    base = os.environ.get("OPENAI_BASE_URL", "http://127.0.0.1:3425/v1").rstrip("/")
    token = os.environ.get("OPENAI_API_KEY", "magpie")
    with forwarder(root, base, token) as server:
        thread = Thread(target=server.serve_forever, daemon=True)
        thread.start()

        def pair(index, fixture):
            document, prompt, case = fixture
            name = f'{case["kind"]}-{index+1}'
            file = root / "fixtures" / f"{name}.txt"
            file.write_bytes(document.encode())
            (root / "fixtures" / f"{name}.json").write_text(json.dumps(case | {"prompt": prompt}, indent=2), encoding="utf8")
            order = list(binaries) if index % 2 == 0 else list(reversed(binaries))
            rows = {}
            for mode in order:
                rows[mode] = run_trial(root, f"{name}-{mode}", case, document, prompt, file, binaries[mode],
                                       args.model, args.effort, server.server_port)
                print(name, mode, rows[mode]["accepted"], rows[mode]["requests"], rows[mode]["known_total_tokens"], flush=True)
            return {
                "case": name, "kind": case["kind"], "order": order, "fixture_sha256": sha(file.read_bytes()),
                "same_first_request_except_tools": len({r["first_request_except_tools_sha256"] for r in rows.values()}) == 1,
                "arms": rows,
            }

        try:
            with ThreadPoolExecutor(max_workers=args.workers) as pool:
                pairs = list(pool.map(lambda entry: pair(*entry), enumerate(cases)))
        finally:
            server.shutdown()
            thread.join()
    result = {
        "base_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip(),
        "binaries": {mode: sha(path.read_bytes()) for mode, path in binaries.items()},
        "model": args.model, "effort": args.effort, "strategy": "inline", "pairs": pairs,
    }
    (root / "summary.json").write_text(json.dumps(result, indent=2), encoding="utf8")
    return int(any(not item["same_first_request_except_tools"] for item in pairs))


if __name__ == "__main__":
    raise SystemExit(main())
