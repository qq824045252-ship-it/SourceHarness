"""Audit what the model received using the actual archived request body."""

from collections import defaultdict
import json
import re

from session_eval_cases import encode, sha


_RECEIPT_FIELDS = ("diff", "before_revision", "revision", "diff_ref", "result_ref")
_JSON_START = re.compile(r'[\[{](?=\s*["\[{])')


def request_body(root, event):
    blob = root / ".sourceharness/sessions/work/blobs" / event["data"]["input"]
    raw = blob.read_bytes()
    return json.loads(raw), len(raw)


def request_context(root, events):
    """Structural size and reuse by request. Provider token usage is measured separately."""
    result = []
    previous = None
    for event in events:
        if event["kind"] != "model_request":
            continue
        body, wire_bytes = request_body(root, event)
        items = body["input"]
        prefix = 0
        if previous is not None:
            for earlier, current in zip(previous["input"], items):
                if earlier != current:
                    break
                prefix += 1
        result.append({
            "event": event["seq"],
            "round": event["data"]["round"],
            "attempt": event["data"]["attempt"],
            "request_sha256": event["data"]["input"],
            "wire_bytes": wire_bytes,
            "instructions_bytes": len(body.get("instructions", "").encode()),
            "input_items": len(items),
            "serialized_input_bytes": len(json.dumps(items, separators=(",", ":"), ensure_ascii=False).encode()),
            "result_bytes": sum(
                len(item["output"].encode()) for item in items
                if item.get("type") == "function_call_output"
            ),
            "shared_input_prefix_items": prefix,
            "instructions_match_previous": previous is not None and previous.get("instructions") == body.get("instructions"),
            "tools_match_previous": previous is not None and previous["tools"] == body["tools"],
            "identical_request_to_previous": previous == body,
        })
        previous = body
    return result


def _objects(value):
    """Find structured data, including JSON embedded in prompt or tool text."""
    if isinstance(value, dict):
        yield value
        for field in value.values():
            yield from _objects(field)
    elif isinstance(value, list):
        for item in value:
            yield from _objects(item)
    elif isinstance(value, str):
        decoder = json.JSONDecoder()
        offset = 0
        while match := _JSON_START.search(value, offset):
            offset = match.start() + 1
            try:
                nested, offset = decoder.raw_decode(value, match.start())
            except ValueError:
                continue
            yield from _objects(nested)


def _receipt_fields(candidate, operation, raw):
    same_operation = candidate.get("operation") == operation
    same_source = candidate.get("source") == raw.get("source")
    matching = {
        key for key in _RECEIPT_FIELDS
        if raw.get(key) is not None and candidate.get(key) == raw[key]
    }
    if same_operation or (same_source and ("diff" in matching or (
        candidate.get("outcome") == raw.get("outcome") and len(matching) >= 2
    ))):
        return matching
    if raw.get("diff_ref") is not None and candidate.get("result_ref") == raw["diff_ref"]:
        return {"diff_ref", "diff"} if candidate.get("content") else {"diff_ref"}
    return set()


def _audit_channel(exposures, request, channel, payload, mutations):
    serialized = json.dumps(payload, ensure_ascii=False)
    for operation, raw in mutations.items():
        diff = raw.get("diff")
        if diff and json.dumps(diff, ensure_ascii=False)[1:-1] in serialized:
            exposures[request, channel, operation].add("diff")
    for candidate in _objects(payload):
        for operation, raw in mutations.items():
            fields = _receipt_fields(candidate, operation, raw)
            if fields:
                exposures[request, channel, operation].update(fields)


def feedback_audit(root, events, mode):
    blobs = root / ".sourceharness/sessions/work/blobs"
    seen_by_call = {}
    direct = defaultdict(list)
    exposures = defaultdict(set)
    tools_sha256 = None
    first_request_sha256 = None
    mutations = {}
    modified = set()
    rereads = 0
    ambiguous = set()
    for sequence, event in enumerate(events, 1):
        data = event["data"]
        if event["kind"] == "model_request":
            body, _ = request_body(root, event)
            seq = event.get("seq", sequence)
            if tools_sha256 is None:
                tools_sha256 = sha(encode(body["tools"]))
                first_request_sha256 = sha(encode(body))
            _audit_channel(exposures, seq, "instructions", body.get("instructions"), mutations)
            _audit_channel(exposures, seq, "request_fields", {
                key: value for key, value in body.items() if key not in ("instructions", "input")
            }, mutations)
            for item in body["input"]:
                if item.get("type") == "function_call_output":
                    operation = seen_by_call.get(item["call_id"])
                    if operation is not None:
                        direct[operation].append((seq, item["output"]))
                    else:
                        _audit_channel(exposures, seq, "other_tool_output", item["output"], mutations)
                else:
                    _audit_channel(exposures, seq, "other_input", item, mutations)
        if event["kind"] != "tool_result":
            continue
        path = data["source"].get("path") if isinstance(data.get("source"), dict) else None
        if data["status"] == "ok" and data["name"] in ("edit", "write"):
            call_id = data["call_id"]
            operation = data["operation"]
            if call_id in seen_by_call:
                ambiguous.add(call_id)
            seen_by_call[call_id] = operation
            mutations[operation] = json.loads((blobs / data["output"]).read_bytes())
            modified.add(path)
        elif data["status"] == "ok" and data["name"] == "read" and path in modified:
            rereads += 1

    receipts = []
    extra_fields = {}
    for operation, raw in mutations.items():
        if mode == "versioned":
            expected = raw
        else:
            expected = {key: raw[key] for key in ("status", "source", "outcome")}
            if mode == "diff":
                expected["diff"] = raw["diff"]
        extra_fields[operation] = set(_RECEIPT_FIELDS) - expected.keys()
        sends = direct[operation]
        receipts.append({
            "operation": operation,
            "path": raw["source"]["path"],
            "sent": bool(sends),
            "matches_mode": bool(sends) and all(json.loads(body) == expected for _, body in sends),
            "first_request": sends[0][0] if sends else None,
            "visible_bytes": len(sends[0][1].encode()) if sends else None,
        })
    side_channels = [
        {
            "request": seq, "channel": channel, "operation": op, "fields": sorted(fields),
            "outside_direct_mode": sorted(fields & extra_fields[op]),
        }
        for (seq, channel, op), fields in sorted(exposures.items())
    ]
    delivery = bool(receipts) and not ambiguous and all(row["matches_mode"] for row in receipts)
    return {
        "tools_sha256": tools_sha256,
        "first_request_sha256": first_request_sha256,
        "made_mutations": len(mutations),
        "post_mutation_reads": rereads,
        "verified_delivery": delivery,
        "isolated_receipt_exposure": delivery and not any(row["outside_direct_mode"] for row in side_channels),
        "ambiguous_call_ids": sorted(ambiguous),
        "side_channels": side_channels,
        "receipts": receipts,
    }
