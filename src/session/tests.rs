use super::journal::{revision, Journal};
use super::tools::{modify, read, research, EditMode};
use super::{Options, Session};
use crate::budget::json_byte_len;
use crate::workspace::{WorkspaceConfig, WorkspaceContext};
use serde_json::{json, Value};
use std::fs;

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
        &json!({"scope":"workspace","query":"limit="}),
    )
    .unwrap();
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
        {"role":"user","content":"What is the current limit?"},
        {"type":"function_call_output","call_id":"new","output":new.to_string()}
    ]);
    let changed = session.project_workspace_evidence(&mut request, 2);
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
    assert_eq!(request[3]["output"], new.to_string());
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
    let changed = session.project_workspace_evidence(&mut unchecked, 1);
    assert_eq!(
        changed,
        vec![
            json!({"path":"policy.txt","last_observed_revision":revision(b"limit=30\n"),
                                   "current_revision":revision(b"limit=60\n")})
        ]
    );
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
