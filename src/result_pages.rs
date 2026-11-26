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
        let inline = json!({"ok":true,"result":&value});
        if value.len() <= INLINE && inline.to_string().len() <= INLINE {
            return inline;
        }
        let total = value.len();
        if total > MAX_BYTES - self.bytes || self.results.len() >= MAX_ITEMS {
            return json!({"ok":true,"result_status":"not_retained","reason":"memory_limit",
                          "total_bytes":total});
        }
        let mut bytes = [0u8; 16];
        if getrandom::fill(&mut bytes).is_err() {
            return json!({"ok":true,"result_status":"not_retained","reason":"reference_unavailable",
                          "total_bytes":total});
        }
        let reference: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        if self.results.contains_key(&reference) {
            return json!({"ok":true,"result_status":"not_retained","reason":"reference_unavailable",
                          "total_bytes":total});
        }
        self.results.insert(reference.clone(), value);
        self.bytes += total;
        self.page(&reference, 0)
    }

    pub fn page(&self, reference: &str, offset: usize) -> Value {
        let Some(value) = self.results.get(reference) else {
            return json!({"ok":false,"code":"RESULT_NOT_FOUND"});
        };
        if offset >= value.len() || !value.is_char_boundary(offset) {
            return json!({"ok":false,"code":"INVALID_RESULT_OFFSET","field":"offset"});
        }
        let mut end = (offset + PAGE_BYTES).min(value.len());
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        json!({"ok":true,"result_status":"paged","ref":reference,"offset":offset,
               "total_bytes":value.len(),"chunk":&value[offset..end],
               "next_offset":(end<value.len()).then_some(end)})
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
            json!({"ok":true,"result":"small"})
        );
        let text = "\u{0}\r\u{1}\"中🙂".repeat(500);
        let mut page = cache.publish(text.clone());
        let reference = page["ref"].as_str().unwrap().to_owned();
        let mut reconstructed = String::new();
        loop {
            assert_eq!(page["ok"], true);
            assert!(page.to_string().len() <= INLINE);
            reconstructed.push_str(page["chunk"].as_str().unwrap());
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
        assert_eq!(extra["ok"], true);
        assert_eq!(extra["result_status"], "not_retained");
        assert!(extra.get("ref").is_none());
        assert_eq!(cache.page(first["ref"].as_str().unwrap(), 0), first);
    }
}
