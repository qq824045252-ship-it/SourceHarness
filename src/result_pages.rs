use crate::wire;
use serde_json::{json, Value};
use std::collections::BTreeMap;

const INLINE: usize = 1024;
const PAGE_BYTES: usize = 128;
const MAX_BYTES: usize = 64 * 1024;
const MAX_ITEMS: usize = 8;

/// Holds results only for one run. References are not file paths and don't
/// survive this store, a process exit, or a new task.
#[derive(Default)]
pub struct ResultPages {
    results: BTreeMap<String, String>,
    bytes: usize,
}

impl ResultPages {
    pub fn publish(&mut self, value: String) -> Value {
        if value.len() <= INLINE {
            let inline = wire::ok(json!({"text":&value}));
            if inline.to_string().len() <= INLINE {
                return inline;
            }
        }
        let total = value.len();
        if total > MAX_BYTES - self.bytes || self.results.len() >= MAX_ITEMS {
            return json!({"status":"ok","data":{},"result_status":"not_retained","reason":"memory_limit",
                          "total_bytes":total});
        }
        let mut bytes = [0u8; 16];
        if getrandom::fill(&mut bytes).is_err() {
            return json!({"status":"ok","data":{},"result_status":"not_retained","reason":"reference_unavailable",
                          "total_bytes":total});
        }
        let reference: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        if self.results.contains_key(&reference) {
            return json!({"status":"ok","data":{},"result_status":"not_retained","reason":"reference_unavailable",
                          "total_bytes":total});
        }
        self.results.insert(reference.clone(), value);
        self.bytes += total;
        self.page(&reference, 0)
    }

    pub fn page(&self, reference: &str, offset: usize) -> Value {
        let Some(value) = self.results.get(reference) else {
            return wire::error("RESULT_NOT_FOUND");
        };
        if offset >= value.len() || !value.is_char_boundary(offset) {
            return json!({"status":"error","code":"INVALID_RESULT_OFFSET","field":"offset"});
        }
        let mut end = (offset + PAGE_BYTES).min(value.len());
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        json!({"status":"ok","data":{"chunk":&value[offset..end]},
               "truncated":end<value.len(),"output_ref":reference,"offset":offset,
               "total_bytes":value.len(),"next_offset":(end<value.len()).then_some(end)})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_inline_or_complete_bounded_unicode_pages() {
        let mut cache = ResultPages::default();
        assert_eq!(
            cache.publish("small".into()),
            wire::ok(json!({"text":"small"}))
        );
        let text = "\u{0}\r\u{1}\"中🙂".repeat(500);
        let mut page = cache.publish(text.clone());
        let reference = page["output_ref"].as_str().unwrap().to_owned();
        let mut reconstructed = String::new();
        loop {
            assert_eq!(page["status"], "ok");
            assert!(page.to_string().len() <= INLINE);
            reconstructed.push_str(page["data"]["chunk"].as_str().unwrap());
            let Some(next) = page["next_offset"].as_u64() else {
                break;
            };
            page = cache.page(&reference, next as usize);
        }
        assert_eq!(reconstructed, text);
        assert_eq!(
            cache.page(&reference, text.len())["code"],
            "INVALID_RESULT_OFFSET"
        );
        assert_eq!(cache.page(&reference, 5)["code"], "INVALID_RESULT_OFFSET");
        assert_eq!(
            ResultPages::default().page(&reference, 0)["code"],
            "RESULT_NOT_FOUND"
        );
    }

    #[test]
    fn capacity_preserves_success_and_older_references() {
        let mut cache = ResultPages::default();
        let first = cache.publish("a".repeat(MAX_BYTES));
        let extra = cache.publish("b".repeat(2500));
        assert_eq!(extra["status"], "ok");
        assert_eq!(extra["result_status"], "not_retained");
        assert!(extra.get("output_ref").is_none());
        assert_eq!(cache.page(first["output_ref"].as_str().unwrap(), 0), first);
    }
}
