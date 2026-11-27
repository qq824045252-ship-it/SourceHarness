use super::journal::{revision, Event, Journal};
use super::validation::{workspace_version, Commands};
use super::HistoryMode;
use crate::workspace::WorkspaceContext;
use serde_json::{json, Map, Value};
use std::collections::HashSet;

pub const DEFAULT_BUDGET: usize = 8 * 1024;
pub const MIN_BUDGET: usize = 1024;
pub const MAX_BUDGET: usize = 64 * 1024;

const FRESHNESS_INSTRUCTIONS: &str = "Use fresh workspace evidence for requirements that can change. When workspace_changes identifies an earlier read as stale, inspect the current source before relying on that earlier content. Earlier validations and snapshots remain available as historical evidence.";
const HISTORY_HEADER: &str = "\nRelated recorded evidence. For changes since an earlier check, use validation_provenance with scope=previous_runs. For what this run tested, use scope=current_run. A validation receipt establishes the workspace and conditions it actually tested.\n";

pub struct Auxiliary {
    pub static_suffix: String,
    pub dynamic_context: String,
    pub notice_revision: Option<String>,
    pub audit: Value,
}

pub fn related(
    ctx: &WorkspaceContext,
    journal: &Journal,
    run: usize,
    profile: &Value,
    commands: &Commands,
) -> Vec<Value> {
    let mut records = Vec::new();
    let mut receipts = Vec::new();
    for current_run in [false, true] {
        let mut checks = HashSet::new();
        receipts.extend(
            journal
                .events
                .iter()
                .rev()
                .filter(|e| {
                    e.kind == "validation_receipt"
                        && (e.run == run) == current_run
                        && checks.insert(e.data["check"].as_str())
                })
                .take(2),
        );
    }
    if let Some(latest) = receipts.iter().max_by_key(|event| event.seq) {
        let workspace = json!(workspace_version(ctx, journal).ok());
        for receipt in &receipts {
            let mut fact = provenance(journal, receipt, profile, commands, &workspace);
            fact["scope"] = json!(if receipt.run == run { "current_run" } else { "previous_runs" });
            records.push(fact);
        }
        for receipt in &receipts {
            records.push(json!(receipt));
        }
        records.extend(
            journal
                .events
                .iter()
                .rev()
                .filter(|e| e.seq > latest.seq && e.kind == "edit_receipt")
                .take(4)
                .map(|e| json!(e)),
        );
    }
    records.extend(
        journal
            .events
            .iter()
            .rev()
            .find(|e| e.run == run && e.kind == "tool_result")
            .filter(|e| e.data["status"] == "error")
            .map(|e| json!(e)),
    );
    records
}

fn provenance(
    journal: &Journal,
    receipt: &Event,
    current: &Value,
    commands: &Commands,
    workspace: &Value,
) -> Value {
    let old = journal
        .events
        .iter()
        .find(|e| e.kind == "user" && e.run == receipt.run)
        .map(|e| &e.data["profile"])
        .unwrap_or(&Value::Null);
    let check = receipt.data["check"].as_str().unwrap_or("");
    let intent = journal.events.iter().rev().find(|e| {
        e.kind == "validation_intent"
            && e.run == receipt.run
            && !receipt.data["operation"].is_null()
            && e.data["operation"] == receipt.data["operation"]
    });
    let old_config = intent
        .map(|e| &e.data["config"])
        .unwrap_or(&old["validation"][check]);
    let new_config = &current["validation"][check];
    let mut comparisons = Map::new();
    for field in [
        "tools_revision",
        "binary_revision",
        "model",
        "effort",
        "gateway",
    ] {
        comparisons.insert(field.to_owned(), compare(&old[field], &current[field]));
    }
    comparisons.insert(
        "validation_config_revision".to_owned(),
        compare(&config_revision(old_config), &config_revision(new_config)),
    );
    comparisons.insert(
        "validation_suite_revision".to_owned(),
        compare(
            &config_revision(&old["validation"]),
            &config_revision(&current["validation"]),
        ),
    );
    let current_program = json!(commands.program_revision(check));
    comparisons.insert(
        "validation_program_revision".to_owned(),
        compare(&receipt.data["program_revision"], &current_program),
    );
    let current_inputs = json!(commands.input_revisions(check));
    comparisons.insert(
        "validation_input_revisions".to_owned(),
        compare(&receipt.data["input_revisions"], &current_inputs),
    );
    let model_request = journal
        .events
        .iter()
        .find(|e| {
            e.kind == "tool_attempt"
                && e.run == receipt.run
                && !receipt.data["operation"].is_null()
                && e.data["operation"] == receipt.data["operation"]
        })
        .and_then(|attempt| {
            journal.events.iter().find(|e| {
                e.kind == "model_response"
                    && e.run == receipt.run
                    && e.data["round"] == attempt.data["round"]
            })
        })
        .map(|e| &e.data["request"]);
    json!({
        "kind":"validation_provenance",
        "validation_event":receipt.seq,
        "validation_run":receipt.run,
        "tested_model_request":model_request,
        "check":receipt.data["check"],
        "result_ref":receipt.data["result_ref"],
        "check_configured_now":!new_config.is_null(),
        "validation_inputs_matched_after_check":receipt.data["input_revisions_stable"],
        "workspace_revision_comparison":compare(&receipt.data["workspace_revision"], workspace),
        "comparisons":comparisons,
    })
}

fn config_revision(config: &Value) -> Value {
    if config.is_null() {
        Value::Null
    } else {
        json!(revision(config.to_string().as_bytes()))
    }
}

fn compare(tested: &Value, current: &Value) -> Value {
    let status = if tested.is_null() || current.is_null() {
        "unknown"
    } else if tested == current {
        "same"
    } else {
        "changed"
    };
    json!({"tested":tested,"current":current,"status":status})
}

fn grouped(records: &[Value]) -> Value {
    let mut buckets: Map<String, Value> = Map::new();
    for record in records {
        let kind = record["kind"]
            .as_str()
            .expect("Related records must carry a kind");
        let bucket = buckets.entry(kind.to_owned()).or_insert_with(|| json!([]));
        bucket.as_array_mut().unwrap().push(record.clone());
    }
    Value::Object(buckets)
}

pub fn prepare(
    mode: HistoryMode,
    freshness: bool,
    projection: bool,
    budget: usize,
    workspace_changes: Vec<Value>,
    mut records: Vec<Value>,
) -> Auxiliary {
    let static_suffix = if freshness {
        format!("\n{FRESHNESS_INSTRUCTIONS}")
    } else {
        String::new()
    };
    let mut dynamic_context = String::new();
    let all_changes = workspace_changes.len();
    let mut changes = Vec::new();
    for item in workspace_changes {
        changes.push(item);
        if changes_section(all_changes, &changes).len() > budget / 4 {
            changes.pop();
            break;
        }
    }
    if all_changes > 0 {
        dynamic_context.push_str(&changes_section(all_changes, &changes));
    }
    let available = records.len();
    while !records.is_empty() {
        let events = json!(records).to_string();
        let facts = grouped(&records).to_string();
        if dynamic_context.len() + HISTORY_HEADER.len() + events.len().max(facts.len()) <= budget {
            dynamic_context.push_str(HISTORY_HEADER);
            dynamic_context.push_str(match mode {
                HistoryMode::Facts => &facts,
                HistoryMode::Events => &events,
                HistoryMode::Conversation => unreachable!("Conversation has no additional records"),
            });
            break;
        }
        records.pop();
    }
    let facts_revision = revision(json!(records).to_string().as_bytes());
    let notice_revision = (!dynamic_context.is_empty()).then(|| revision(dynamic_context.as_bytes()));
    Auxiliary {
        audit: json!({
            "representation":mode.label(),
            "delivery":"input_tail",
            "fact_count":records.len(),
            "available_facts":available,
            "facts_revision":facts_revision,
            "notice_revision":notice_revision,
            "freshness_instructions":freshness,
            "stale_projection":projection,
            "workspace_changes_shown":changes.len(),
            "workspace_changes_available":all_changes,
            "bytes":dynamic_context.len(),
            "budget_bytes":budget,
            "tokens":null,
        }),
        static_suffix,
        dynamic_context,
        notice_revision,
    }
}

fn changes_section(available: usize, changes: &[Value]) -> String {
    format!(
        "\nWorkspace observations: {}",
        json!({"workspace_changes":changes,"total":available})
    )
}
