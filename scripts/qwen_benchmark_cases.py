"""Reproducible public workspaces for the 1,000-task compact CLI evaluation."""

from dataclasses import asdict, dataclass, field
from hashlib import sha256
import json
import random


@dataclass
class Step:
    prompt: str
    expected: dict
    external: dict = field(default_factory=dict)
    checks: dict = field(default_factory=dict)
    interrupt: bool = False


@dataclass
class Case:
    id: str
    category: str
    shape: str
    variant: int
    files: dict
    steps: list[Step]


def encode(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode("utf-8")


def fixture(value):
    return encode(value).decode() + "\n"


def answer(prompt, value, **expected):
    return Step(f"{prompt} Reply with only the requested JSON.", {"answer": value, **expected})


def delivery(prompt, **expected):
    return Step(f"{prompt} Run the configured acceptance check on the delivered workspace.",
                {**expected, "validate": True})


def table(rng, variant):
    groups = ("alder", "birch", "cèdre", "elm", "fir")[:3 + variant % 3]
    rows = []
    for index in range(9 + variant):
        rows.append({"id":f"node-{rng.randrange(1000,9999)}-{index}", "group":rng.choice(groups),
                     "score":rng.randrange(-8,36), "status":rng.choice(("ok","ok","held","skip")),
                     "time":rng.randrange(10,90)})
    return groups, rows


TABLE_TASKS = {
    "qualified-count":"Count rows per group whose status is ok and score is at least {cut}. Include every group, even those at zero. Return a JSON object from group to integer.",
    "ranked-three":"Return a JSON array of the first three row IDs with status ok, sorting by descending score then ascending ID.",
    "lower-median":"For each group with an ok row, return its lower median score among ok rows. Sort scores and take index (length - 1) // 2. Return a JSON object.",
    "newest-per-group":"Return a JSON object mapping each group that has rows to its newest row ID, using higher time first and ascending ID to break ties.",
    "qualifying-ids":"Return a JSON array of IDs with score at least {cut} and status held or ok. Sort IDs ascending.",
    "status-cross-tab":"Return a JSON object mapping each group to an object with ok, held and skip counts. Include zero values.",
    "negative-groups":"Return a sorted JSON array of groups whose sum of scores over all their rows is strictly negative.",
    "unique-times":"Return a sorted JSON array of the time values shared by at least two rows with different statuses.",
}


def table_result(shape, groups, rows, cut):
    good = [row for row in rows if row["status"] == "ok"]
    if shape == "qualified-count":
        return {group:sum(row["group"] == group and row["score"] >= cut for row in good) for group in groups}
    if shape == "ranked-three":
        return [row["id"] for row in sorted(good,key=lambda row:(-row["score"],row["id"]))[:3]]
    if shape == "lower-median":
        scores = {group:sorted(row["score"] for row in good if row["group"] == group) for group in groups}
        return {group:values[(len(values)-1)//2] for group,values in scores.items() if values}
    if shape == "newest-per-group":
        return {group:min((row for row in rows if row["group"] == group), key=lambda row:(-row["time"],row["id"]))["id"]
                for group in sorted({row["group"] for row in rows})}
    if shape == "qualifying-ids":
        return sorted(row["id"] for row in rows if row["score"] >= cut and row["status"] in ("held","ok"))
    if shape == "status-cross-tab":
        return {group:{status:sum(row["group"] == group and row["status"] == status for row in rows)
                       for status in ("ok","held","skip")} for group in groups}
    if shape == "negative-groups":
        return sorted(group for group in groups if sum(row["score"] for row in rows if row["group"] == group) < 0)
    return sorted(value for value in {row["time"] for row in rows}
                  if len({row["status"] for row in rows if row["time"] == value}) > 1)


def inspect(shape, rng, variant):
    if shape in TABLE_TASKS:
        groups, rows = table(rng, variant)
        cut = rng.randrange(-4,29)
        files = {"inputs/manifest.json":fixture({"groups":groups,"rows":rows})}
        return files, [answer(f"Use inputs/manifest.json. {TABLE_TASKS[shape].format(cut=cut)}",
                              table_result(shape,groups,rows,cut))]
    if shape == "paged-window":
        start = 480 + variant * 11
        lines = [f"log-{i:04d} tag-{rng.randrange(10**7):07d}" for i in range(start + 12)]
        files = {"logs/journal.txt":"\n".join(lines)+"\n","docs/notes.md":"Logs use one record per line.\n"}
        return files, [answer(f"In logs/journal.txt return a JSON array of the three complete lines at one-based "
                              f"positions {start} through {start+2}, in order.",lines[start-1:start+2])]
    active, fallback = rng.sample(range(5001,6999),2)
    files = {"etc/conf.ini":f"[fallback]\nport={fallback}\n[active]\n# port={fallback}\nport={active}\n",
             "docs/boot.txt":"The active section controls the service.\n"}
    return files, [answer("Read etc/conf.ini and docs/boot.txt. Return an object with integer active_port and fallback_port.",
                          {"active_port":active,"fallback_port":fallback})]


def reports(shape, rng, variant):
    groups, rows = table(rng, variant)
    cut = rng.randrange(-4,29)
    split = 3 + variant % 6
    files = {"groups.json":fixture(groups),"inputs/north.json":fixture(rows[:split]),
             "inputs/south.json":fixture(rows[split:]),"out/.keep":""}
    instructions = TABLE_TASKS[shape]
    return files, [delivery(f"Combine the arrays in inputs/north.json and inputs/south.json. The full group list is "
                            f"in groups.json. {instructions.format(cut=cut)} Save that JSON value to out/report.json.",
                            json={"out/report.json":table_result(shape,groups,rows,cut)})]


def patch(shape, rng, variant):
    before, after = rng.sample(range(11,700),2)
    token = f"svc-{rng.randrange(100000,999999)}"
    if shape in ("section-match","crlf-lines","toml-scope"):
        path = "ops/settings.toml" if shape == "toml-scope" else "ops/settings.ini"
        eol = "\r\n" if shape == "crlf-lines" else "\n"
        key = ("limit","timeout","capacity","retries")[variant % 4]
        early = f"# legacy {token}{eol}[backup]{eol}{key}={before}{eol}"
        enabled = "true" if shape == "toml-scope" else "yes"
        live = f"[primary]{eol}# use this section{eol}{key}={before}{eol}enabled={enabled}{eol}"
        last = f"[demo]{eol}{key}={before}{eol}"
        files = {path:early+live+last,"README.md":f"# {token}\n"}
        expected = {path:early+live.replace(f"{key}={before}",f"{key}={after}")+last}
        return files,[delivery(f"In {path}, set {key} to {after} in [primary] only. Keep comments, other bytes, "
                               "and the file's line endings.", exact=expected)]
    if shape == "json-property":
        value = {"active":{"concurrency":before,"name":token},"fallback":{"concurrency":before},"tracks":["night","beta"]}
        files = {"service.json":fixture(value),"docs/run.md":"The fallback settings are protected.\n"}
        value["active"]["concurrency"] = after
        return files,[delivery(f"Set active.concurrency in service.json to integer {after}. Keep every other value.",
                               json={"service.json":value})]
    if shape == "relative-links":
        files = {"README.md":f"# {token}\n\n[Handbook](docs/old.md)\n",
                 "docs/guide.md":"# Guide\n\n[Handbook](old.md)\n[Home](../README.md)\n",
                 "docs/handbook.md":"# Handbook\n"}
        expected = {name:files[name].replace("old.md","handbook.md") for name in ("README.md","docs/guide.md")}
        return files,[delivery("Repair the Handbook links in README.md and docs/guide.md. They must point to "
                               "docs/handbook.md from each document. Preserve labels and other text.",exact=expected)]
    if shape == "version-sync":
        old, new = f"1.{before % 10}.{before}", f"1.{before % 10}.{after}"
        files = {"service.toml":f'[package]\nname="{token}"\nversion="{old}"\n',
                 "releases/current.txt":old+"\n","README.md":f'# {token}\nRun version {old}.\nOld release: {old}.\n'}
        expected = {"service.toml":files["service.toml"].replace(old,new),"releases/current.txt":new+"\n",
                    "README.md":files["README.md"].replace(f"Run version {old}",f"Run version {new}")}
        return files,[delivery(f"Set the active release to {new} in service.toml, releases/current.txt and the Run "
                               "version line of README.md. Keep the Old release line intact.",exact=expected)]
    if shape == "dedup-aliases":
        tag = f"alias-{token}"
        lines = [f"# {tag}",f"{tag}=alpha",f"stable={before}",f"{tag}=alpha",f"{tag}=next",""]
        files = {"aliases.env":"\n".join(lines)}
        return files,[delivery(f"Remove only the duplicate occurrence of {tag}=alpha in aliases.env. "
                               "Keep its first occurrence, the distinct next alias and other bytes.",
                               exact={"aliases.env":"\n".join(lines[:3]+lines[4:])})]
    if shape == "unicode-quoted":
        old,new = f"Café {token}",f"Réglage {after} – {token}"
        files = {"i18n/labels.env":f'TITLE="{old}"\nPREVIOUS="{old}"\n# keep accents and quotes\n'}
        return files,[delivery(f"In i18n/labels.env set TITLE to {json.dumps(new,ensure_ascii=False)}. "
                               "Keep the quotes and PREVIOUS unchanged.",exact={"i18n/labels.env":files["i18n/labels.env"].replace(old,new,1)})]
    if shape == "section-insert":
        files = {"private.conf":f"# {token}\n[primary]\nsize={before}\n\n[backup]\nsize={before}\n"}
        expected = files["private.conf"].replace("\n\n[backup]",f"\nthreshold={after}\n\n[backup]")
        return files,[delivery(f"Add threshold={after} immediately after size in [primary] of private.conf. "
                               "Keep all other text.",exact={"private.conf":expected})]
    lines = ["name\tpriority\tstate",f"{token}\t{before}\tdraft",f"archive\t{before}\tdraft",""]
    files = {"queues.tsv":"\n".join(lines)}
    lines[1] = f"{token}\t{after}\tready"
    return files,[delivery(f"In queues.tsv change only the row {token} to priority {after} and state ready. "
                           "Keep the TSV layout intact.",exact={"queues.tsv":"\n".join(lines)})]


REPAIR_CONTRACTS = {
    "stable-dedup":"Given a JSON list of integers, return each distinct integer once, in first-seen order.",
    "override-map":"Given an object containing defaults and overrides objects, return a merged object. Overrides win, including null.",
    "index-matches":"Given an object with numbers and minimum, return zero-based indexes whose value is at least minimum.",
    "lower-median":"Given a list of integers, return its lower median. Return null for empty input.",
    "status-filter":"Given a list of objects with id and status, return the IDs in input order whose normalized status is 'ok' "
                    "after stripping whitespace and lowercasing. Preserve repeated IDs.",
    "key-value":"Given a list of strings, skip blank lines and lines whose stripped form starts with #. Split at the first '='. "
                "Ignore lines without '=' and lines with an empty stripped key. Trim keys and values. Last assignment wins.",
    "range-runs":"Given a list of integers, sort and deduplicate it, then return maximal consecutive runs as inclusive [first,last] pairs.",
    "rounded-cents":"Given a list of decimal strings, round each to cents using decimal ROUND_HALF_UP and return fixed two-decimal strings.",
    "joined-labels":"Given left and right arrays of [key,label] pairs, match only keys in both, in left order. Use the last right label "
                    "for duplicate keys. Return [left_label,right_label] arrays and preserve left duplicates.",
    "inventory-delta":"Given before and after objects mapping item names to counts, return only nonzero after-minus-before differences. "
                      "Treat absent counts as zero.",
}


def repair_vector(shape, rng, index):
    from decimal import Decimal, ROUND_HALF_UP

    numbers = [rng.randrange(-5,9) for _ in range(index+1)] if index else []
    lower = rng.randrange(-4,7)
    if shape == "stable-dedup":
        return numbers, list(dict.fromkeys(numbers))
    if shape == "override-map":
        before = {"retained":lower,"shared":42}
        overrides = {"shared":None if index % 2 else lower,"added":index}
        return {"defaults":before,"overrides":overrides}, {**before,**overrides}
    if shape == "index-matches":
        return {"numbers":numbers,"minimum":lower}, [i for i,value in enumerate(numbers) if value >= lower]
    if shape == "lower-median":
        return numbers, sorted(numbers)[(len(numbers)-1)//2] if numbers else None
    if shape == "status-filter":
        rows = [{"id":str(value),"status":rng.choice(("ok"," OK ","reject","oK","bad",""))} for value in numbers]
        return rows, [row["id"] for row in rows if row["status"].strip().lower() == "ok"]
    if shape == "key-value":
        lines = ["  # ignore ","",f"limit={lower}","without assignment"," = ignored",
                 f"unit = label={index}",f"limit = {index}"]
        return lines, {"unit":f"label={index}","limit":str(index)}
    if shape == "range-runs":
        groups = []
        for value in sorted(set(numbers)):
            if groups and value == groups[-1][1]+1:
                groups[-1][1] = value
            else:
                groups.append([value,value])
        return numbers, groups
    if shape == "rounded-cents":
        text = [f"{n}.{index%10}05" for n in numbers]
        return text,[str(Decimal(value).quantize(Decimal(".01"),rounding=ROUND_HALF_UP)) for value in text]
    if shape == "joined-labels":
        left = [[str(value),f"L{i}"] for i,value in enumerate(numbers)]
        right = [[str(value),f"R{i}"] for i,value in enumerate(reversed(numbers[::2]))]
        lookup = dict(right)
        return {"left":left,"right":right}, [[label,lookup[key]] for key,label in left if key in lookup]
    before, after = {"held":lower,"old":index}, {"held":lower,"new":index+1}
    return {"before":before,"after":after}, {name:after.get(name,0)-before.get(name,0)
                                          for name in before.keys()|after.keys()
                                          if after.get(name,0) != before.get(name,0)}


def repair(shape, rng, variant):
    location = ("transform.py","bin/transform.py","report/transform.py")[variant % 3]
    key = f"batch-{rng.randrange(100000,999999)}"
    files = {location:"import json\nimport sys\n\n"
                      "def transform(record):\n    return record  # placeholder\n\n"
                      "for line in sys.stdin:\n    print(json.dumps(transform(json.loads(line))))\n",
             "README.md":f"# Data transformation {key}\nUse Python 3 and read newline-delimited JSON.\n"}
    lengths = [0,1,2,5,6,8+variant]
    vectors = [repair_vector(shape,rng,length) for length in lengths]
    return files, [delivery(f"Fix {location}, a Python 3 standard-library CLI. Read one JSON value per input line "
                            "and print one JSON value per line, with no additional text. "
                            f"{REPAIR_CONTRACTS[shape]} Keep README.md unchanged.",
                            behavior={"path":location,"inputs":[item[0] for item in vectors],
                                      "outputs":[item[1] for item in vectors]})]


def workflow(shape, rng, variant):
    old, updated, ticket = rng.sample(range(10000,90000),3)
    path = ("ops/state.txt","settings/primary.cfg","ledger/service.txt")[variant % 3]
    prefix = f"ticket={ticket}\n"
    initial = prefix+f"state={old}\n"
    files = {path:initial,"README.md":f"# Deployment {ticket}\n"}
    if shape == "rollback-trial":
        steps = [delivery(f"Record the successful initial verification of {path}. Leave the workspace intact.",
                          exact={}),
                 delivery(f"Try replacing state in {path} with {updated}, run acceptance to see the rejection, "
                          "then use rollback to restore the starting snapshot. Run acceptance again.",exact={},
                          required_tools=["rollback"],failed_check=True)]
        return files,steps
    if shape == "history-audit":
        return files,[answer(f"Read {path}; return a JSON object with integer state.",{"state":old}),
                      Step(f"The maintainer changed {path}. Report the original and current state values. "
                           "Reply with a JSON object with integer old and current and a journal snapshot reference "
                           "such as r12 or a journal event number in source.",{"history":{"path":path,"old":old,
                            "current":updated,"old_digest":sha256(initial.encode()).hexdigest()}},
                           external={path:prefix+f"state={updated}\n"})]
    method = "Use exec to inspect" if shape == "exec-observation" else "Use read to inspect"
    start = answer(f"{method} {path}. Return a JSON object with integer state.",{"state":old})
    external = prefix+f"state={updated}\n# operations note {ticket}\n"
    second = delivery(f"The maintainer changed {path}. Add enabled=yes as a new line at the end and preserve "
                      "the live lines and operations note.",exact={path:external+"enabled=yes\n"})
    second.external = {path:external}
    second.interrupt = shape == "kill-recover"
    if shape == "validation-drift":
        first = delivery(f"Verify the starting value in {path} using acceptance. Leave it intact.",exact={})
        second.prompt = f"The external acceptance input has changed, and the maintainer updated {path}. " \
                        "Keep the current version intact and run acceptance against it."
        second.expected = {"exact":{},"validate":True}
        return files,[first,second]
    steps = [start, second]
    if shape == "multi-phase":
        new_state = old+updated
        later = external.replace(f"state={updated}",f"state={new_state}")+"enabled=yes\n"
        third = delivery(f"Operations updated {path} again. Add verified={ticket} at the end. Preserve the live "
                         "contents, including enabled=yes.",exact={path:later+f"verified={ticket}\n"})
        third.external = {path:later}
        steps.append(third)
        if variant % 4 == 0:
            steps.append(answer(f"Read {path} and README.md. Return a JSON object with integer state and ticket.",
                                {"state":new_state,"ticket":ticket}))
    return files,steps


def guard(shape, rng, variant):
    label = f"artifact-{rng.randrange(10000,90000)}"
    files = {"state.txt":"draft\n","inventory.txt":f"service={label}\nregion=lab-{variant%5}\n"}
    blocked = {"answer":{"status":"blocked"},"blocked":True}
    if shape == "missing-input":
        return files,[answer(f"Determine the exact signature in {label}.sig. If it is unavailable after examining the "
                             'workspace, report {"status":"blocked"}. Leave the workspace intact.',blocked["answer"],
                             blocked=True)]
    if shape == "must-delete":
        return files,[answer(f"Remove inventory.txt from the live workspace, leaving the other files intact. "
                             'If the available tools cannot remove it, report {"status":"blocked"}. '
                             "Do not substitute an empty file.",blocked["answer"],blocked=True)]
    if shape == "incompatible-checks":
        prompt = ("Try to satisfy both configured checks on the same workspace. Leave the starting contents "
                  'intact if the requirements conflict. If blocked, reply with only {"status":"blocked"}.')
        return files,[Step(prompt,{**blocked,"required_checks":["mode-a","mode-b"]},checks={
            "mode-a":{"exact":{"state.txt":"mode-a\n"},"hint":"state.txt must contain mode-a."},
            "mode-b":{"exact":{"state.txt":"mode-b\n"},"hint":"state.txt must contain mode-b."}})]
    if shape == "missing-anchor":
        files["state.txt"] = f"service={label}\r\nstage=archived\r\n"
        return files,[answer('Change exactly the existing "stage=ready" line to "stage=live" if it exists. '
                             'Otherwise keep the CRLF file intact and report {"status":"blocked"}.',
                             blocked["answer"],blocked=True)]
    return files,[Step("Investigate the configured acceptance check. Try to resolve the blocker if possible. "
                       'If no workspace change can satisfy it, keep the workspace intact and reply {"status":"blocked"}.',
                       {**blocked,"required_checks":["acceptance"]},checks={"acceptance":{"unavailable":label}})]


SHAPES = {
    "inspect": (inspect, (*TABLE_TASKS,"paged-window","effective-setting")),
    "patch": (patch, ("section-match","crlf-lines","toml-scope","json-property","relative-links","version-sync",
                     "dedup-aliases","unicode-quoted","section-insert","tsv-row")),
    "report": (reports, tuple(TABLE_TASKS)),
    "repair": (repair, tuple(REPAIR_CONTRACTS)),
    "workflow": (workflow, ("live-observation","exec-observation","history-audit","kill-recover","multi-phase",
                            "rollback-trial","validation-drift")),
    "guard": (guard, ("noisy-oracle","incompatible-checks","missing-input","missing-anchor","must-delete")),
}


def corpus():
    for variant in range(20):
        for category,(builder,shapes) in SHAPES.items():
            for shape in shapes:
                identifier = f"{category}.{shape}.{variant:02d}"
                rng = random.Random(int.from_bytes(sha256(identifier.encode()).digest()[:8]))
                files, steps = builder(shape,rng,variant)
                yield Case(identifier,category,shape,variant,files,steps)


def manifest(cases):
    return [{"id":case.id,"category":case.category,"shape":case.shape,"variant":case.variant,
             "steps":len(case.steps),"sha256":sha256(encode({"files":case.files,"steps":[asdict(step) for step in case.steps]})).hexdigest()} for case in cases]
