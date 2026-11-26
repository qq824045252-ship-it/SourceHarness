use serde_json::{json, Value};

pub fn ok(data: Value) -> Value {
    json!({ "status": "ok", "data": data })
}
pub fn error(code: &str) -> Value {
    json!({ "status": "error", "code": code })
}
pub fn unknown(code: &str, operation_id: &str) -> Value {
    json!({ "status": "unknown", "code": code, "retryable": false, "operation_id": operation_id })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn absence_is_an_answer_and_uncertain_effect_is_not_retryable() {
        assert_eq!(
            ok(json!({"found":false})),
            json!({"status":"ok","data":{"found":false}})
        );
        assert_eq!(
            error("MISSING_ARGUMENT"),
            json!({"status":"error","code":"MISSING_ARGUMENT"})
        );
        let result = unknown("EXECUTION_TIMEOUT", "op_local");
        assert_eq!(result["status"], "unknown");
        assert_eq!(result["retryable"], false);
        assert_eq!(result["operation_id"], "op_local");
        assert!(result.get("data").is_none());
    }
}
