"""Run real-model SourceHarness session checks in isolated workspaces."""
import argparse
import concurrent.futures
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time

CONFIG = """# Production configuration
[primary]
timeout=30
retries=2

[archive]
timeout=30
retries=2
"""
FILLER = "".join(f"# preserved settings note {i:04d}\n" for i in range(230))
INITIAL = CONFIG + FILLER
BASE_A = (30, 2, "POL-031")
BASE_B = (60, 3, "POL-072")
HOLDOUT = (45, 5, "POL-083")
POLICY = """# Operations policy {policy}
Only the primary section moves to the policy settings.
Its timeout is {timeout}.
Its retry count is {retries}.
Keep the archive section and every other line as it is.
Use the policy identifier in the evidence note.
"""
VERIFY = r"""import pathlib,re,sys
root = pathlib.Path('.')
cfg = (root/'service.cfg').read_text(encoding='utf8')
policy=(root/'policy.md').read_text(encoding='utf8')
base=(root/'fixture-original.txt').read_text(encoding='utf8')
timeout=int(re.search(r'timeout is (\d+)',policy)[1])
retries=int(re.search(r'retry count is (\d+)',policy)[1])
policy_id=re.search(r'POL-\d+',policy)[0]
wanted=base.replace('[primary]\ntimeout=30\nretries=2',f'[primary]\ntimeout={timeout}\nretries={retries}',1)
note=root/'evidence.md'
ok=cfg==wanted and note.is_file() and policy_id in note.read_text(encoding='utf8')
print(f'policy={policy_id} configuration={"correct" if cfg==wanted else "wrong"} evidence={"correct" if note.is_file() and policy_id in note.read_text(encoding="utf8") else "wrong"}')
sys.exit(0 if ok else 1)
"""
PROMPT = ("Apply the current operations policy in this workspace to the primary service. Preserve the archive and "
          "unrelated content. Deliver evidence.md with the policy ID and the actual values implemented. "
          "Run the configured service acceptance check against the delivered workspace and report its outcome.")
BASELINE_PROMPT = ("Verify the current operations policy and service configuration. Leave all existing files as they "
                   "are and run the configured service acceptance check. Summarize what was verified.")

def sha(data):
    return hashlib.sha256(data).hexdigest()

def set_policy(root, values):
    timeout, retries, policy = values
    (root/"policy.md").write_text(POLICY.format(timeout=timeout,retries=retries,policy=policy),encoding="utf8",newline="\n")

def seed(root):
    root.mkdir()
    (root/"service.cfg").write_text(INITIAL,encoding="utf8",newline="\n")
    (root/"fixture-original.txt").write_text(INITIAL,encoding="utf8",newline="\n")
    (root/"verify.py").write_text(VERIFY,encoding="utf8",newline="\n")
    (root/"evidence.md").write_text("Initial checked configuration POL-031, timeout=30, retries=2.\n",encoding="utf8",newline="\n")
    set_policy(root,BASE_A)

def events(root):
    return [json.loads(line) for line in (root/".sourceharness/sessions/work/events.jsonl").read_text(encoding="utf8").splitlines()]

def blob(root, digest):
    return (root/".sourceharness/sessions/work/blobs"/digest).read_bytes()

def workspace_revision(root):
    entries = {p.name:sha(p.read_bytes()) for p in sorted(root.iterdir()) if p.is_file()}
    return sha(json.dumps(entries,separators=(",",":"),sort_keys=True).encode())

def execute(exe,checks,root,prompt,mode="text",history="facts",interrupt=False):
    env=os.environ | {"OPENAI_REASONING_EFFORT":"low"}
    if interrupt:
        env["SOURCE_HARNESS_TEST_INTERRUPT"]="after_file_replace"
    else:
        env.pop("SOURCE_HARNESS_TEST_INTERRUPT",None)
    args=[str(exe),"agent","--root",str(root),"--session","work","--checks",str(checks)]
    if mode!="default":
        args+= ["--edit-mode",mode]
    if history!="default":
        args+= ["--history-mode",history]
    args+=["--resume"] if prompt is None else ["--prompt",prompt]
    started=time.monotonic()
    result=subprocess.run(args,capture_output=True,text=True,encoding="utf8",env=env,timeout=240)
    step = f"{root.name}-{'resume' if prompt is None else 'start'}"
    (root.parent/(step+".stdout.txt")).write_text(result.stdout,encoding="utf8")
    (root.parent/(step+".stderr.txt")).write_text(result.stderr,encoding="utf8")
    return dict(exit=result.returncode, seconds=round(time.monotonic()-started,2), final=result.stdout.strip(), error=result.stderr.strip())

def result(root, values, old_count, executions, extra=None):
    newer=events(root)[old_count:]
    codes=[e["data"]["code"] for e in newer if e["kind"]=="tool_result" and e["data"]["status"]=="error"]
    results=[e["data"] for e in newer if e["kind"]=="validation_receipt"]
    import collections
    tool_counts=collections.Counter(e["data"]["name"] for e in newer if e["kind"]=="tool_result")
    workspace_revision_after=workspace_revision(root)
    expected=(root/"fixture-original.txt").read_text(encoding="utf8").replace(
        "[primary]\ntimeout=30\nretries=2",f"[primary]\ntimeout={values[0]}\nretries={values[1]}",1)
    unchanged=(root/"service.cfg").read_bytes()==expected.encode()
    evidence=root/"evidence.md"
    note=evidence.read_text(encoding="utf8") if evidence.exists() else ""
    independently_checked = subprocess.run([sys.executable,"verify.py"],cwd=root,capture_output=True,
                                            text=True,encoding="utf8",timeout=10)
    validated = any(r["passed"] and r["workspace_revision"]==workspace_revision_after for r in results)
    responses=[e["data"] for e in newer if e["kind"]=="model_response"]
    known=all(r["usage"] is not None for r in responses)
    return dict(name=root.name,passed=bool(executions[-1]["exit"]==0 and independently_checked.returncode==0
                and unchanged and validated), independent=independently_checked.stdout.strip(), file_exact=unchanged,
                policy_in_evidence=values[2] in note, active_revision_validated=validated,errors=codes,tool_calls=tool_counts,
                validated_revisions=[r["workspace_revision"] for r in results],
                model_rounds=len(responses), tokens=sum(r["usage"]["total_tokens"] for r in responses) if known else None,
                executions=executions, **(extra or {}))

def main():
    parser=argparse.ArgumentParser()
    parser.add_argument("--exe",type=Path,required=True)
    parser.add_argument("--out",type=Path,required=True)
    parser.add_argument("--workers",type=int,default=4)
    parser.add_argument("--profile",choices=["all","recovery","selected"],default="all")
    cli=parser.parse_args()
    exe,output=cli.exe.resolve(),cli.out.resolve()
    output.mkdir(parents=True)
    checks=output/"checks.json"
    checks.write_text(json.dumps({"service":{"program":sys.executable,"args":["verify.py"],
                              "description":"Check primary settings against the policy, exact preservation of other lines, and the evidence note.",
                              "timeout_seconds":20}}),encoding="utf8")
    baseline=output/"baseline"
    seed(baseline)
    initial=execute(exe,checks,baseline,BASELINE_PROMPT)
    baseline_report=result(baseline,BASE_A,0,[initial])
    assert baseline_report["passed"], baseline_report
    old_history=len(events(baseline))
    print("Original version validated by live model",flush=True)
    experiments=[
        ("history-conversation","text","conversation",BASE_B),
        ("history-events","text","events",BASE_B),
        ("history-facts","text","facts",BASE_B),
        ("edit-range","range","facts",BASE_B),
        ("edit-patch","patch","facts",BASE_B),
        ("holdout-text","text","facts",HOLDOUT),
        ("holdout-range","range","facts",HOLDOUT),
        ("holdout-patch","patch","facts",HOLDOUT),
        ("resume-after-replace","text","facts",BASE_B),
        ("resume-with-external-change","text","facts",HOLDOUT),
    ]
    if cli.profile=="recovery":
        experiments=[case for case in experiments if case[0].startswith("resume")]
    if cli.profile=="selected":
        experiments=[("default-current-version","default","default",BASE_B),
                     ("holdout-default-create","default","default",HOLDOUT),
                     ("resume-after-replace","default","default",BASE_B),
                     ("resume-with-external-change","default","default",HOLDOUT)]
    def experiment(case):
        label,edit,history,values=case
        root=output/label
        shutil.copytree(baseline,root,ignore=shutil.ignore_patterns("workspace.lock"))
        set_policy(root,values)
        if label.startswith("holdout"):
            primary,archive = CONFIG.split("[archive]",1)
            content = ("[archive]" + archive + FILLER + primary).encode()
            for path in ("service.cfg","fixture-original.txt"):
                (root/path).write_bytes(content)
        if label=="holdout-default-create":
            (root/"evidence.md").unlink()
        recovery=label.startswith("resume")
        first=execute(exe,checks,root,PROMPT,edit,history,recovery)
        attempts=[first]
        extra=dict(edit_mode=edit,history_mode=history)
        if recovery and first["exit"]==86:
            int_event=next(e["data"] for e in reversed(events(root)) if e["kind"]=="edit_intent")
            target=root/int_event["path"]
            extra["interrupted_path"]=int_event["path"]
            extra["candidate_present_before_resume"]=sha(target.read_bytes())==int_event["after"]
            if label=="resume-with-external-change":
                marker="\n# Maintainer note, preserve it.\n"
                if target.name=="service.cfg":
                    target.write_bytes(target.read_bytes()+marker.encode())
                    original=root/"fixture-original.txt"
                    original.write_bytes(original.read_bytes()+marker.encode())
                else:
                    target.write_bytes(target.read_bytes()+marker.encode())
            attempts.append(execute(exe,checks,root,None,edit,history))
            rows=events(root)[old_history:]
            receipts=[e["data"] for e in rows if e["kind"]=="edit_receipt" and e["data"]["operation"]==int_event["operation"]]
            extra["interrupted_receipts"]=receipts
            extra["conflict_detected"]=any(e["kind"]=="tool_result" and e["data"]["operation"]==int_event["operation"]
                                          and e["data"]["code"]=="REVISION_CONFLICT" for e in rows)
            extra["maintainer_note_preserved"]=label!="resume-with-external-change" or marker.encode() in target.read_bytes()
        row=result(root,values,old_history,attempts,extra)
        if recovery:
            row["passed"] &= bool(first["exit"]==86 and row.get("candidate_present_before_resume"))
            if label=="resume-with-external-change":
                row["passed"] &= bool(row.get("conflict_detected") and row.get("maintainer_note_preserved"))
            else:
                receipts=row.get("interrupted_receipts") or []
                row["passed"] &= len(receipts)==1 and receipts[0]["outcome"]=="observed_target_on_recovery"
        print(row["name"],row["passed"],row["model_rounds"],row["tool_calls"],row["errors"],flush=True)
        return row
    with concurrent.futures.ThreadPoolExecutor(max_workers=cli.workers) as pool:
        rows=list(pool.map(experiment,experiments))
    report={"model_route":os.environ.get("OPENAI_MODEL","auto/deepseek-v4.1-flash-1017"),
            "effort":"low","baseline":baseline_report,"observations":rows,"pass":sum(r["passed"] for r in rows)}
    (output/"summary.json").write_text(json.dumps(report,indent=2),encoding="utf8")
    return int(any(not row["passed"] for row in rows))

if __name__=="__main__":
    raise SystemExit(main())
