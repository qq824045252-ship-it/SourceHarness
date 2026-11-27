use super::journal::{revision, Journal};
use super::tools::{modify, read, research, search_result, EditMode};
use super::{history, validation, Commands, HistoryMode, Options, Session};
use crate::budget::json_byte_len;
use crate::workspace::{WorkspaceConfig, WorkspaceContext};
use serde_json::{json, Value};
use std::fs;

#[test]
fn restores_receipts_by_operation_in_recorded_completion_order() {
    let root = tempfile::tempdir().unwrap();
    let mut session = Session::open(root.path(), "work", Options::default()).unwrap();
    session.journal.append(1, "user", json!({"text":"Read both files","profile":{}})).unwrap();
    let response = session.journal.store_json(&json!([
        {"type":"function_call","call_id":"read_a","name":"read","arguments":"{}"},
        {"type":"function_call","call_id":"read_b","name":"read","arguments":"{}"}
    ])).unwrap();
    session.journal.append(1, "model_response", json!({"round":1,"output":response})).unwrap();
    for (operation, actual, delivered) in [("1.1.2", "raw b", "visible b"), ("1.1.1", "a", "a")] {
        let raw = session.journal.store_json(&json!({"content":actual})).unwrap();
        let model = session.journal.store_json(&json!({"content":delivered})).unwrap();
        session.journal.append(1, "tool_result", json!({
            "operation":operation,"output":raw,"model_output":model
        })).unwrap();
    }
    let replay = session.checkpoint().unwrap();
    assert!(replay.pending.is_empty());
    assert_eq!(replay.input[3]["call_id"], "read_b");
    assert_eq!(replay.input[3]["output"], json!({"content":"visible b"}).to_string());
    assert_eq!(replay.input[4]["call_id"], "read_a");
}

#[test]
fn create_accepts_an_empty_optional_revision_but_still_requires_an_absent_file() {
    let root = tempfile::tempdir().unwrap();
    let mut session = Session::open(root.path(), "work", Options::default()).unwrap();
    let args = json!({"path":"evidence.md","mode":"create","expected_revision":"","content":"first"});
    let created = modify(&session.ctx, &mut session.journal, 1, "create", "write", EditMode::Text, &args).unwrap();
    assert_eq!(created["status"], "ok");
    let mut args = args;
    args["content"] = json!("should not replace");
    let rejected = modify(&session.ctx, &mut session.journal, 1, "again", "write", EditMode::Text, &args).unwrap();
    assert_eq!(rejected["code"], "REVISION_CONFLICT");
    assert_eq!(fs::read(root.path().join("evidence.md")).unwrap(), b"first");
    args["expected_revision"] = json!(created["revision"]);
    let wrong_mode = modify(&session.ctx, &mut session.journal, 1, "wrong-mode", "write", EditMode::Text, &args).unwrap();
    assert_eq!(wrong_mode["omit"], json!(["expected_revision"]));
}

#[test]
fn research_samples_keep_source_coordinates_and_shared_edit_revision() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("settings.txt");
    fs::write(&path, "open\r\nfirst code=27\r\nsecond code=27\r\n").unwrap();
    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root.path())).unwrap();
    let mut journal = Journal::open(&root.path().join(".sourceharness")).unwrap();
    let found = research(
        &ctx,
        &mut journal,
        1,
        None,
        &json!({"scope":"workspace","query":"code=27","limit":1}),
    )
    .unwrap();
    let hit = &found["hits"][0];
    assert_eq!(found["coverage"]["scan_complete"], true);
    assert_eq!(found["presentation"]["matching_items"], 2);
    assert_eq!(found["presentation"]["complete"], false);
    assert_eq!(hit["source"]["path"], "settings.txt");
    assert_eq!(hit["range"]["start_line"], 2);
    assert_eq!(hit["text"], "first code=27\r\n");

    fs::write(&path, "open\r\nfirst code=12\r\nsecond code=27\r\n").unwrap();
    let conflict = modify(
        &ctx,
        &mut journal,
        1,
        "1.1.1",
        "edit",
        EditMode::Text,
        &json!({"path":"settings.txt","expected_revision":hit["revision"],
                "changes":[{"old_text":"first code=27","new_text":"first code=51"}] }),
    )
    .unwrap();
    assert_eq!(conflict["code"], "REVISION_CONFLICT");
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "open\r\nfirst code=12\r\nsecond code=27\r\n"
    );

    let restored = read(
        &ctx,
        &mut journal,
        1,
        &json!({"path":"settings.txt","result_ref":hit["result_ref"],"start_line":2,"line_count":1}),
    )
    .unwrap();
    assert_eq!(restored["content"], "first code=27\r\n");
    let edited = modify(
        &ctx,
        &mut journal,
        1,
        "1.2.1",
        "edit",
        EditMode::Text,
        &json!({"path":"settings.txt","expected_revision":conflict["current_revision"],
                "changes":[{"old_text":"first code=12","new_text":"first code=51"}] }),
    )
    .unwrap();
    assert_eq!(edited["outcome"], "committed");
    assert_eq!(
        fs::read_to_string(path).unwrap(),
        "open\r\nfirst code=51\r\nsecond code=27\r\n"
    );
}

#[test]
fn overlapping_matches_need_unique_context() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("note.txt");
    fs::write(&path, "caaaa").unwrap();
    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root.path())).unwrap();
    let mut journal = Journal::open(&root.path().join(".sourceharness")).unwrap();
    let prior = read(&ctx, &mut journal, 1, &json!({"path":"note.txt"})).unwrap();
    let ambiguous=modify(&ctx,&mut journal,1,"1.1.1","edit",EditMode::Text,
        &json!({"path":"note.txt","expected_revision":prior["revision"],"changes":[{"old_text":"aaa","new_text":"b"}]})).unwrap();
    assert_eq!(ambiguous["code"], "AMBIGUOUS_MATCH");
    assert_eq!(ambiguous["match_byte_starts"], json!([1, 2]));
    assert_eq!(fs::read_to_string(path).unwrap(), "caaaa");
}

#[test]
fn pages_an_oversized_utf8_line_after_the_live_file_changes() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("output.txt");
    let long = format!("{}\r\n", "字\"".repeat(3400));
    fs::write(&path, format!("start\r\n{long}end\r\n")).unwrap();
    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root.path())).unwrap();
    let mut journal = Journal::open(&root.path().join(".sourceharness")).unwrap();
    let mut reply = read(
        &ctx,
        &mut journal,
        1,
        &json!({"path":"output.txt","start_line":2,"line_count":1}),
    )
    .unwrap();
    assert_eq!(reply["delivery"], "folded");
    fs::write(path, "new version").unwrap();
    let revision = reply["revision"].clone();
    let mut got = String::new();
    for _ in 0..16 {
        assert_eq!(reply["revision"], revision);
        assert!(json_byte_len(&reply) <= 8192);
        got.push_str(reply["content"].as_str().unwrap());
        if reply["coverage"]["complete"] == true {
            break;
        }
        reply = read(&ctx, &mut journal, 1, &reply["next"]).unwrap();
    }
    assert_eq!(got, long);
}

#[test]
fn historical_validation_hit_links_to_the_original_stdout() {
    let root = tempfile::tempdir().unwrap();
    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root.path())).unwrap();
    let mut journal = Journal::open(&root.path().join(".sourceharness")).unwrap();
    let stdout = journal
        .artifact(
            1,
            json!({"kind":"validation","check":"service"}),
            b"policy=POL-031 passed\n",
        )
        .unwrap();
    journal
        .append(
            1,
            "validation_receipt",
            json!({"result_ref":stdout.result_ref,"preview":"policy=POL-031"}),
        )
        .unwrap();
    let found = research(
        &ctx,
        &mut journal,
        2,
        None,
        &json!({"scope":"history","query":"POL-031"}),
    )
    .unwrap();
    let reference = &found["hits"][0]["output_ref"];
    let result = read(&ctx, &mut journal, 2, &json!({"result_ref":reference})).unwrap();
    assert_eq!(result["content"], "policy=POL-031 passed\n");
    assert_eq!(result["source"]["kind"], "validation");
}

#[test]
fn model_request_marks_stale_history_and_keeps_the_archived_snapshot_available() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("policy.txt");
    fs::write(&file, "limit=30\n").unwrap();
    let mut session = Session::open(root.path(), "work", Options::default()).unwrap();
    let old = read(
        &session.ctx,
        &mut session.journal,
        1,
        &json!({"path":"policy.txt"}),
    )
    .unwrap();
    let old_hit = research(
        &session.ctx,
        &mut session.journal,
        1,
        None,
        &json!({"scope":"workspace","query":"limit="}),
    )
    .unwrap();
    let old_count = search_result(
        &session.journal,
        &json!({"result_ref":old["result_ref"],"query":"limit=30"}),
    )
    .unwrap();
    assert_eq!(old_count["data"]["total_occurrences"], 1);
    fs::write(&file, "limit=60\n").unwrap();
    let new = read(
        &session.ctx,
        &mut session.journal,
        2,
        &json!({"path":"policy.txt"}),
    )
    .unwrap();
    let mut request = json!([
        {"type":"function_call_output","call_id":"old","output":old.to_string()},
        {"type":"function_call_output","call_id":"old_hit","output":old_hit.to_string()},
        {"type":"function_call_output","call_id":"old_count","output":old_count.to_string()},
        {"role":"user","content":"What is the current limit?"},
        {"type":"function_call_output","call_id":"new","output":new.to_string()}
    ]);
    let changed = session.project_workspace_evidence(&mut request, 3, true, true);
    assert!(
        changed.is_empty(),
        "The current read supersedes the old observation."
    );
    let older: Value = serde_json::from_str(request[0]["output"].as_str().unwrap()).unwrap();
    assert_eq!(older["historical"], true);
    assert_eq!(older["current_revision"], revision(b"limit=60\n"));
    assert!(older.get("content").is_none());
    let search: Value = serde_json::from_str(request[1]["output"].as_str().unwrap()).unwrap();
    assert_eq!(search["hits"][0]["historical"], true);
    assert!(search["hits"][0].get("text").is_none());
    assert_eq!(search["hits"][0]["result_ref"], old["result_ref"]);
    let count: Value = serde_json::from_str(request[2]["output"].as_str().unwrap()).unwrap();
    assert_eq!(count["historical"], true);
    assert!(count.get("data").is_none());
    assert_eq!(count["result_ref"], old["result_ref"]);
    assert_eq!(request[4]["output"], new.to_string());
    let archived = read(
        &session.ctx,
        &mut session.journal,
        2,
        &json!({"result_ref":older["result_ref"]}),
    )
    .unwrap();
    assert_eq!(archived["content"], "limit=30\n");

    let mut unchecked =
        json!([{"type":"function_call_output","call_id":"old","output":old.to_string()}]);
    let changed = session.project_workspace_evidence(&mut unchecked, 1, true, true);
    assert_eq!(
        changed,
        vec![
            json!({"path":"policy.txt","last_observed_revision":revision(b"limit=30\n"),
                                   "current_revision":revision(b"limit=60\n")})
        ]
    );
    let original = json!([{"type":"function_call_output","call_id":"old","output":old.to_string()}]);
    let mut notices_only = original.clone();
    let notices = session.project_workspace_evidence(&mut notices_only, 1, false, true);
    assert_eq!(notices, changed);
    assert_eq!(notices_only, original);
    let mut projection_only = original;
    assert!(session.project_workspace_evidence(&mut projection_only, 1, true, false).is_empty());
    let redacted: Value = serde_json::from_str(projection_only[0]["output"].as_str().unwrap()).unwrap();
    assert_eq!(redacted["historical"], true);
    assert!(redacted.get("current_revision").is_none());
    assert!(redacted.get("content").is_none());
}

#[test]
fn recovered_conflict_exposes_the_exact_difference_from_the_intended_version() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("settings.txt");
    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root.path())).unwrap();
    let mut journal = Journal::open(&root.path().join(".sourceharness")).unwrap();
    let before = journal.put(b"timeout=30\n").unwrap();
    let after = journal.put(b"timeout=60\n").unwrap();
    journal
        .append(
            1,
            "edit_intent",
            json!({"operation":"1.1.1","path":"settings.txt","before":before,"after":after}),
        )
        .unwrap();
    fs::write(&path, "timeout=60\n# hand edited\n").unwrap();
    let response = modify(
        &ctx,
        &mut journal,
        1,
        "1.1.1",
        "edit",
        EditMode::Text,
        &Value::Null,
    )
    .unwrap();
    assert_eq!(response["code"], "REVISION_CONFLICT");
    assert_eq!(response["planned_revision"], after);
    let observed = read(
        &ctx,
        &mut journal,
        1,
        &json!({"result_ref":response["planned_to_current"]["result_ref"]}),
    )
    .unwrap();
    let delta = observed["content"].as_str().unwrap();
    assert!(delta.contains("\n+# hand edited\n"));
    assert!(delta.contains("\n timeout=60\n"));
    assert_eq!(
        fs::read_to_string(path).unwrap(),
        "timeout=60\n# hand edited\n"
    );
}

#[test]
fn historical_validation_distinguishes_changed_conditions_from_unchanged_workspace() {
    let root = tempfile::tempdir().unwrap();
    let programs = tempfile::tempdir().unwrap();
    let executable = programs.path().join("check.exe");
    fs::write(&executable, b"validator v1").unwrap();
    fs::write(root.path().join("settings.txt"), b"timeout=30\n").unwrap();
    let mut session = Session::open(root.path(), "work", Options::default()).unwrap();
    let old_command = json!({"program":executable,"args":[],"description":"Check v1",
                             "artifacts":[],"timeout_seconds":30});
    let new_command = json!({"program":executable,"args":[],"description":"Check v2",
                             "artifacts":[],"timeout_seconds":30});
    let commands: Commands = serde_json::from_value(json!({"sample":new_command})).unwrap();
    let workspace = validation::workspace_version(&session.ctx, &session.journal).unwrap();
    session.journal.append(1, "user", json!({"profile":{
        "model":"model-a","effort":"low","gateway":"gateway",
        "tools_revision":"tools-a","binary_revision":"binary-a",
        "validation":{"sample":old_command}
    }})).unwrap();
    session.journal.append(1, "validation_intent", json!({
        "operation":"1.1.1","config":old_command,"workspace_revision":workspace
    })).unwrap();
    let receipt = session.journal.append(1, "validation_receipt", json!({
        "check":"sample","operation":"1.1.1","workspace_revision":workspace,
        "program_revision":revision(b"validator v1"),"result_ref":"r2","passed":true
    })).unwrap();
    fs::write(executable, b"validator v2").unwrap();
    let profile = json!({"model":"model-b","effort":"low","gateway":"gateway",
        "tools_revision":"tools-b","binary_revision":"binary-b",
        "validation":{"sample":new_command}
    });
    let records = history::related(&session.ctx, &session.journal, 2, &profile, &commands);
    let provenance = &records[0];
    assert_eq!(provenance["validation_event"], receipt);
    assert_eq!(provenance["validation_run"], 1);
    assert_eq!(provenance["workspace_revision_comparison"]["status"], "same");
    for field in ["tools_revision","binary_revision","validation_config_revision","validation_program_revision","model"] {
        assert_eq!(provenance["comparisons"][field]["status"], "changed");
    }
    assert_eq!(provenance["comparisons"]["validation_program_revision"]["current"], revision(b"validator v2"));
    assert_eq!(provenance["comparisons"]["effort"]["status"], "same");
    assert_eq!(provenance["result_ref"], "r2");

    session.journal.append(2, "user", json!({"profile":profile})).unwrap();
    let current = session.journal.append(2, "validation_receipt", json!({
        "check":"sample","operation":"2.1.1","workspace_revision":workspace,
        "program_revision":revision(b"validator v2"),"result_ref":"r3","passed":true
    })).unwrap();
    let records = history::related(&session.ctx, &session.journal, 2, &profile, &commands);
    let auxiliary = history::prepare(HistoryMode::Events, false, false, 8192, Vec::new(), records);
    assert!(auxiliary.static_suffix.is_empty());
    let sent: Value = serde_json::from_str(auxiliary.dynamic_context.lines().last().unwrap()).unwrap();
    assert_eq!(sent[0]["validation_event"], receipt);
    assert_eq!(sent[0]["scope"], "previous_runs");
    assert_eq!(sent[0]["comparisons"]["tools_revision"]["status"], "changed");
    assert_eq!(sent[1]["validation_event"], current);
    assert_eq!(sent[1]["scope"], "current_run");
    assert_eq!(sent[1]["comparisons"]["tools_revision"]["status"], "same");
}

#[test]
fn history_representations_send_the_same_records_under_the_same_budget() {
    let records = vec![
        json!({"kind":"validation_provenance","validation_event":4,"check":"build"}),
        json!({"kind":"validation_receipt","seq":4,"data":{"passed":true}}),
        json!({"kind":"edit_receipt","seq":8,"data":{"detail":"a".repeat(1900)}}),
    ];
    let options = |mode| history::prepare(mode, false, false, 1024, Vec::new(), records.clone());
    let events = options(HistoryMode::Events);
    let facts = options(HistoryMode::Facts);
    let sent_events: Value = serde_json::from_str(events.dynamic_context.lines().last().unwrap()).unwrap();
    let sent_facts: Value = serde_json::from_str(facts.dynamic_context.lines().last().unwrap()).unwrap();
    for record in sent_events.as_array().unwrap() {
        assert!(sent_facts[record["kind"].as_str().unwrap()]
            .as_array().unwrap().contains(record));
    }
    assert_eq!(sent_facts.as_object().unwrap().values().map(|v| v.as_array().unwrap().len()).sum::<usize>(), 2);
    assert_eq!(events.audit["fact_count"], 2);
    assert_eq!(events.audit["facts_revision"], facts.audit["facts_revision"]);
    for prepared in [events, facts] {
        assert!(prepared.static_suffix.is_empty());
        assert!(prepared.dynamic_context.len() <= 1024);
        assert_eq!(prepared.audit["bytes"], prepared.dynamic_context.len());
        assert_eq!(prepared.audit["freshness_instructions"], false);
        assert_eq!(prepared.audit["stale_projection"], false);
        assert!(prepared.audit["tokens"].is_null());
    }
}
