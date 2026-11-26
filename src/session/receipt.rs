use serde_json::{json, Value};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ReceiptMode {
    Summary,
    Diff,
    #[default]
    Versioned,
}

impl ReceiptMode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Summary => "summary",
            Self::Diff => "diff",
            Self::Versioned => "versioned",
        }
    }
}

pub(super) fn project(mode: ReceiptMode, name: &str, actual: &Value) -> Option<Value> {
    if mode == ReceiptMode::Versioned
        || !matches!(name, "edit" | "write")
        || actual["status"] != "ok"
    {
        return None;
    }
    let mut visible = json!({
        "status": actual["status"],
        "source": actual["source"],
        "outcome": actual["outcome"],
    });
    if mode == ReceiptMode::Diff {
        visible["diff"] = actual["diff"].clone();
    }
    Some(visible)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_successful_mutation_views_are_reduced() {
        let actual = json!({
            "status":"ok",
            "source":{"kind":"workspace","path":"src/file.txt"},
            "outcome":"observed_target_on_recovery",
            "revision":"v2",
            "before_revision":"v1",
            "result_ref":"r1",
            "diff_ref":"r2",
            "diff":"-old\n+current\n"
        });
        let summary = json!({
            "status":"ok",
            "source":{"kind":"workspace","path":"src/file.txt"},
            "outcome":"observed_target_on_recovery"
        });
        assert_eq!(project(ReceiptMode::Summary, "edit", &actual), Some(summary.clone()));
        let mut diff = summary;
        diff["diff"] = actual["diff"].clone();
        assert_eq!(project(ReceiptMode::Diff, "write", &actual), Some(diff));
        assert_eq!(project(ReceiptMode::Versioned, "edit", &actual), None);
        assert_eq!(project(ReceiptMode::Diff, "read", &actual), None);
        let conflict = json!({"status":"error","code":"REVISION_CONFLICT","current_revision":"v3"});
        assert_eq!(project(ReceiptMode::Summary, "edit", &conflict), None);
    }
}
