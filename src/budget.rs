use std::io::{self, Write};

use serde::Serialize;

/// Hard 8 KiB response payload budget enforced across tool outputs.
pub const MAX_RESPONSE_BYTES: usize = 8192;

/// Maximum byte length of a single hit line snippet in `search_result`.
pub const MAX_HIT_TEXT_BYTES: usize = 240;

/// Truncates `s` to at most `max_bytes` bytes on a valid UTF-8 character boundary.
/// Returns `(slice, truncated)`.
pub fn truncate_utf8_bytes(s: &str, max_bytes: usize) -> (&str, bool) {
    if s.len() <= max_bytes {
        return (s, false);
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (&s[..end], true)
}

struct ByteCounter(usize);

impl Write for ByteCounter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Computes the exact compact JSON serialized byte length of `value` without allocating a `String`.
pub fn json_byte_len<T: Serialize>(value: &T) -> usize {
    let mut counter = ByteCounter(0);
    serde_json::to_writer(&mut counter, value).expect("JSON serialization is infallible");
    counter.0
}

/// Serializes `value` into a compact JSON string.
pub fn to_json_string<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).expect("JSON serialization is infallible")
}
