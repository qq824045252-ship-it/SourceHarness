use super::{read, research, search_result, Journal};
use crate::budget::{json_byte_len, MAX_RESPONSE_BYTES};
use crate::workspace::{WorkspaceConfig, WorkspaceContext};
use serde_json::json;
use std::fs;

#[test]
fn research_accepts_empty_defaults_for_optional_fields() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("service.cfg"), "timeout=30\n").unwrap();
    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root.path())).unwrap();
    let mut journal = Journal::open(&root.path().join(".sourceharness")).unwrap();
    let list = research(&ctx, &mut journal, 1, &json!({
        "scope":"workspace","path":"","query":"","result_ref":"","offset":0,"limit":20
    })).unwrap();
    assert_eq!(list["status"], "ok");
    assert_eq!(list["presentation"]["matching_items"], 1);
    assert_eq!(list["hits"][0]["source"]["path"], "service.cfg");
    let invalid = research(&ctx, &mut journal, 1, &json!({"scope":"workspace","offset":1})).unwrap();
    assert_eq!(invalid["code"], "INVALID_ARGUMENT");
}

#[test]
fn literal_multiline_hits_report_exact_bytes_and_original_line_ranges() {
    let root = tempfile::tempdir().unwrap();
    let text = "介绍\r\n[primary]\r\ntimeout=30\r\n[secondary]\ntimeout=10\n";
    fs::write(root.path().join("service.cfg"), text).unwrap();
    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root.path())).unwrap();
    let mut journal = Journal::open(&root.path().join(".sourceharness")).unwrap();

    let crlf = "[primary]\r\ntimeout=30";
    let found = research(
        &ctx,
        &mut journal,
        1,
        &json!({"scope":"workspace","query":crlf}),
    )
    .unwrap();
    assert_eq!(found["presentation"]["matching_items"], 1);
    let hit = &found["hits"][0];
    let start = "介绍\r\n".len();
    assert_eq!(
        hit["range"],
        json!({"start_line":2,"end_line":3,"byte_start":start,"byte_end":start+crlf.len()})
    );
    assert_eq!(hit["text"], crlf);

    let lf = "[secondary]\ntimeout=10";
    let found = research(
        &ctx,
        &mut journal,
        1,
        &json!({"scope":"result","result_ref":hit["result_ref"],"query":lf}),
    )
    .unwrap();
    assert_eq!(found["presentation"]["matching_items"], 1);
    let hit = &found["hits"][0];
    assert_eq!(hit["range"]["start_line"], 4);
    assert_eq!(hit["range"]["end_line"], 5);
    assert_eq!(hit["text"], lf);
}

#[test]
fn all_result_hits_are_pageable_after_the_workspace_changes() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("many.txt");
    let text: String = (1..=3000).map(|line| format!("match {line}\n")).collect();
    fs::write(&path, text).unwrap();
    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root.path())).unwrap();
    let mut journal = Journal::open(&root.path().join(".sourceharness")).unwrap();

    let mut page = research(
        &ctx,
        &mut journal,
        1,
        &json!({"scope":"workspace","query":"match","limit":20}),
    )
    .unwrap();
    let result_ref = page["result_ref"].clone();
    fs::write(path, "changed\n").unwrap();
    let mut expected_line = 1;
    loop {
        assert_eq!(page["result_ref"], result_ref);
        assert_eq!(page["coverage"]["scan_complete"], true);
        assert_eq!(page["presentation"]["matching_items"], 3000);
        assert_eq!(page["presentation"]["retained"], 3000);
        assert!(json_byte_len(&page) <= MAX_RESPONSE_BYTES);
        for hit in page["hits"].as_array().unwrap() {
            assert_eq!(hit["range"]["start_line"], expected_line);
            assert_eq!(hit["text"], format!("match {expected_line}\n"));
            expected_line += 1;
        }
        if page.get("next").is_none() {
            assert_eq!(page["presentation"]["complete"], false);
            break;
        }
        assert_eq!(page["next"]["offset"], expected_line - 1);
        page = research(&ctx, &mut journal, 1, &page["next"]).unwrap();
    }
    assert_eq!(expected_line, 3001);
}

#[test]
fn archived_history_pages_keep_exact_argument_references_past_128_hits() {
    let root = tempfile::tempdir().unwrap();
    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root.path())).unwrap();
    let mut journal = Journal::open(&root.path().join(".sourceharness")).unwrap();
    for index in 0..133 {
        let raw = format!("{{ \"query\": \"old argument {index}\" }}\r\n");
        let arguments = journal.put(raw.as_bytes()).unwrap();
        journal
            .append(
                1,
                "tool_attempt",
                json!({"name":"research","arguments":arguments}),
            )
            .unwrap();
    }
    let mut page = research(
        &ctx,
        &mut journal,
        1,
        &json!({"scope":"history","query":"old argument","limit":20}),
    )
    .unwrap();
    assert_eq!(page["coverage"]["items_scanned"], 133);
    assert_eq!(page["presentation"]["retained"], 133);
    let mut next_index = 133;
    let added_after_search = journal.put(b"{ \"query\": \"old argument new\" }").unwrap();
    journal
        .append(
            2,
            "tool_attempt",
            json!({"name":"research","arguments":added_after_search}),
        )
        .unwrap();

    loop {
        for hit in page["hits"].as_array().unwrap() {
            next_index -= 1;
            let args_ref = &hit["arguments_ref"];
            let actual = read(&ctx, &mut journal, 1, &json!({"result_ref":args_ref})).unwrap();
            assert_eq!(
                actual["content"],
                format!("{{ \"query\": \"old argument {next_index}\" }}\r\n")
            );
        }
        if page.get("next").is_none() {
            break;
        }
        page = research(&ctx, &mut journal, 1, &page["next"]).unwrap();
    }
    assert_eq!(next_index, 0);
}

#[test]
fn session_search_result_reuses_exact_regex_counts_on_an_old_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("capture.txt");
    fs::write(&path, "item=A item=A\r\nitem=B\r\n").unwrap();
    let ctx = WorkspaceContext::new(WorkspaceConfig::with_root(root.path())).unwrap();
    let mut journal = Journal::open(&root.path().join(".sourceharness")).unwrap();
    let snapshot = read(&ctx, &mut journal, 1, &json!({"path":"capture.txt"})).unwrap();
    let reference = &snapshot["result_ref"];
    fs::write(path, "item=C\n").unwrap();

    let found = search_result(
        &journal,
        &json!({
            "result_ref":reference,"query":"item=([A-Z])","match_mode":"regex","group_by_capture":1,
            "max_hits":2,
        }),
    )
    .unwrap();
    assert_eq!(found["status"], "ok");
    assert_eq!(found["source"]["path"], "capture.txt");
    assert_eq!(found["revision"], snapshot["revision"]);
    assert_eq!(found["data"]["result_ref"], *reference);
    assert_eq!(found["data"]["matching_lines"], 2);
    assert_eq!(found["data"]["total_occurrences"], 3);
    assert_eq!(
        found["data"]["aggregation"]["value_counts"],
        json!([{"value":"A","count":2},{"value":"B","count":1}])
    );
}
