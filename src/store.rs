use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::budget::{json_byte_len, truncate_utf8_bytes, MAX_RESPONSE_BYTES};

static NEXT_STORE_ID: AtomicU64 = AtomicU64::new(1);

/// Configuration for a task-isolated [`FoldStore`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FoldStoreConfig {
    pub max_snapshots: usize,
    pub max_total_bytes: Option<usize>,
    pub inline_threshold_bytes: usize,
    pub preview_max_bytes: usize,
}

impl Default for FoldStoreConfig {
    fn default() -> Self {
        Self {
            max_snapshots: 64,
            max_total_bytes: Some(64 * 1024 * 1024),
            inline_threshold_bytes: 4096,
            preview_max_bytes: 1024,
        }
    }
}

/// Byte offsets for a single 1-based line in a [`Snapshot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineSpan {
    pub raw_start: usize,
    pub raw_end: usize,
    pub text_end: usize,
}

/// A Markdown heading section with a 1-based closed line interval `[start_line, end_line]`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SectionEntry {
    pub name: String,
    pub lines: [usize; 2],
}

/// Lightweight first-packet envelope returned when an output is folded into [`FoldStore`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FoldEnvelope {
    pub status: &'static str,
    pub result_ref: String,
    pub total_lines: usize,
    pub preview: String,
    pub has_sections: bool,
    pub index_status: &'static str,
    pub sections: Vec<SectionEntry>,
}

/// Routing result of [`FoldStore::ingest`]: small payloads pass inline, large payloads fold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestOutcome {
    Inline(String),
    Folded(FoldEnvelope),
}

/// Immutable indexed text snapshot stored inside [`FoldStore`].
#[derive(Debug)]
pub struct Snapshot {
    result_ref: String,
    raw: String,
    lines: Vec<LineSpan>,
    sections: Vec<SectionEntry>,
}

impl Snapshot {
    pub(crate) fn new(result_ref: String, raw: String) -> Self {
        let lines = build_line_spans(&raw);
        let sections = extract_markdown_sections(&raw, &lines);
        Self {
            result_ref,
            raw,
            lines,
            sections,
        }
    }

    pub fn result_ref(&self) -> &str {
        &self.result_ref
    }

    pub fn raw(&self) -> &str {
        &self.raw
    }

    pub fn total_lines(&self) -> usize {
        self.lines.len()
    }

    /// Returns line content without its trailing `\r\n` or `\n` for a 1-based line number.
    pub fn line_text(&self, line_1based: usize) -> Option<&str> {
        let span = self.lines.get(line_1based.checked_sub(1)?)?;
        Some(&self.raw[span.raw_start..span.text_end])
    }

    /// Returns the exact raw byte slice (including original line endings) for 1-based closed `[start_line, end_line]`.
    pub fn raw_range(&self, start_line: usize, end_line: usize) -> Option<&str> {
        if start_line == 0 || end_line < start_line || end_line > self.lines.len() {
            return None;
        }
        let first = &self.lines[start_line - 1];
        let last = &self.lines[end_line - 1];
        Some(&self.raw[first.raw_start..last.raw_end])
    }

    pub fn sections(&self) -> &[SectionEntry] {
        &self.sections
    }
}

#[derive(Debug, Default)]
struct StoreInner {
    entries: HashMap<String, Arc<Snapshot>>,
    order: VecDeque<String>,
    total_bytes: usize,
    next_seq: u64,
}

/// Task-isolated, thread-safe read-only storage for folded outputs.
#[derive(Debug)]
pub struct FoldStore {
    store_id: u64,
    config: FoldStoreConfig,
    inner: RwLock<StoreInner>,
}

impl Default for FoldStore {
    fn default() -> Self {
        Self::new()
    }
}

impl FoldStore {
    pub fn new() -> Self {
        Self::with_config(FoldStoreConfig::default())
    }

    pub fn with_config(config: FoldStoreConfig) -> Self {
        Self {
            store_id: NEXT_STORE_ID.fetch_add(1, Ordering::Relaxed),
            config,
            inner: RwLock::new(StoreInner::default()),
        }
    }

    pub fn config(&self) -> FoldStoreConfig {
        self.config
    }

    /// Routes `raw` by byte size: `Inline` when `<= inline_threshold_bytes`, otherwise inserts into store and returns `Folded`.
    pub fn ingest(&self, raw: impl Into<String>) -> IngestOutcome {
        let raw = raw.into();
        if raw.len() <= self.config.inline_threshold_bytes {
            IngestOutcome::Inline(raw)
        } else {
            IngestOutcome::Folded(self.insert(raw))
        }
    }

    /// Always stores `raw` as an immutable snapshot and returns its bounded [`FoldEnvelope`].
    pub fn insert(&self, raw: impl Into<String>) -> FoldEnvelope {
        let mut snapshot = Snapshot::new(String::new(), raw.into());
        let mut inner = self.inner.write().expect("FoldStore lock poisoned");
        inner.next_seq += 1;
        let result_ref = format!("res_{}_{}", self.store_id, inner.next_seq);

        snapshot.result_ref.clone_from(&result_ref);
        let snapshot = Arc::new(snapshot);

        let envelope = build_fold_envelope(&snapshot, self.config.preview_max_bytes);
        let entry_bytes = snapshot.raw.len();

        inner.entries.insert(result_ref.clone(), snapshot);
        inner.order.push_back(result_ref.clone());
        inner.total_bytes += entry_bytes;

        self.evict_if_needed(&mut inner, &result_ref);
        envelope
    }

    /// Looks up a snapshot by `result_ref`. Returns `None` if unknown or evicted.
    pub fn get(&self, result_ref: &str) -> Option<Arc<Snapshot>> {
        let inner = self.inner.read().expect("FoldStore lock poisoned");
        inner.entries.get(result_ref).cloned()
    }

    fn evict_if_needed(&self, inner: &mut StoreInner, keep_ref: &str) {
        let max_snapshots = self.config.max_snapshots.max(1);
        while inner.entries.len() > max_snapshots
            || self
                .config
                .max_total_bytes
                .is_some_and(|limit| inner.total_bytes > limit && inner.entries.len() > 1)
        {
            let Some(oldest) = inner.order.pop_front() else {
                break;
            };
            if oldest == keep_ref {
                inner.order.push_back(oldest);
                if inner.entries.len() <= 1 {
                    break;
                }
                continue;
            }
            if let Some(removed) = inner.entries.remove(&oldest) {
                inner.total_bytes = inner.total_bytes.saturating_sub(removed.raw.len());
            }
        }
    }
}

fn build_fold_envelope(snapshot: &Snapshot, preview_max_bytes: usize) -> FoldEnvelope {
    let (preview_slice, _) = truncate_utf8_bytes(&snapshot.raw, preview_max_bytes);
    let has_sections = !snapshot.sections.is_empty();
    let mut envelope = FoldEnvelope {
        status: "ok",
        result_ref: snapshot.result_ref.clone(),
        total_lines: snapshot.total_lines(),
        preview: preview_slice.to_owned(),
        has_sections,
        index_status: if has_sections { "available" } else { "none" },
        sections: snapshot.sections.clone(),
    };

    // Keep the fold envelope bounded within the 8 KiB response budget.
    while json_byte_len(&envelope) > MAX_RESPONSE_BYTES && !envelope.preview.is_empty() {
        let new_limit = envelope.preview.len() / 2;
        let (shorter, _) = truncate_utf8_bytes(&envelope.preview, new_limit);
        envelope.preview = shorter.to_owned();
    }
    while json_byte_len(&envelope) > MAX_RESPONSE_BYTES && !envelope.sections.is_empty() {
        envelope.sections.pop();
    }
    envelope
}

/// Splits `raw` into 1-based [`LineSpan`] descriptors, preserving CRLF and LF coordinates consistently.
pub fn build_line_spans(raw: &str) -> Vec<LineSpan> {
    if raw.is_empty() {
        return Vec::new();
    }
    let bytes = raw.as_bytes();
    let mut spans = Vec::new();
    let mut start = 0usize;

    for (idx, &b) in bytes.iter().enumerate() {
        if b == b'\n' {
            let text_end = if idx > start && bytes[idx - 1] == b'\r' {
                idx - 1
            } else {
                idx
            };
            spans.push(LineSpan {
                raw_start: start,
                raw_end: idx + 1,
                text_end,
            });
            start = idx + 1;
        }
    }

    if start < bytes.len() {
        spans.push(LineSpan {
            raw_start: start,
            raw_end: bytes.len(),
            text_end: bytes.len(),
        });
    }
    spans
}

/// Extracts Markdown ATX headings outside fenced code blocks (` ``` ` / `~~~`),
/// preserving duplicate heading names with their individual 1-based closed line intervals.
pub fn extract_markdown_sections(raw: &str, lines: &[LineSpan]) -> Vec<SectionEntry> {
    if lines.is_empty() {
        return Vec::new();
    }

    struct RawHeading {
        level: usize,
        name: String,
        line_1based: usize,
    }

    let mut headings = Vec::new();
    let mut open_fence: Option<(u8, usize)> = None;

    for (idx, span) in lines.iter().enumerate() {
        let line = &raw[span.raw_start..span.text_end];
        let line_no = idx + 1;

        if let Some((fence_char, fence_len)) = open_fence {
            if is_closing_fence(line, fence_char, fence_len) {
                open_fence = None;
            }
            continue;
        }

        if let Some(fence) = parse_opening_fence(line) {
            open_fence = Some(fence);
            continue;
        }

        if let Some((level, name)) = parse_atx_heading(line) {
            headings.push(RawHeading {
                level,
                name,
                line_1based: line_no,
            });
        }
    }

    let total_lines = lines.len();
    let mut sections = Vec::with_capacity(headings.len());
    for i in 0..headings.len() {
        let start_line = headings[i].line_1based;
        let level = headings[i].level;
        let mut end_line = total_lines;
        for next in &headings[(i + 1)..] {
            if next.level <= level {
                end_line = next.line_1based - 1;
                break;
            }
        }
        sections.push(SectionEntry {
            name: headings[i].name.clone(),
            lines: [start_line, end_line],
        });
    }
    sections
}

fn strip_up_to_three_spaces(line: &str) -> Option<&str> {
    let bytes = line.as_bytes();
    let mut spaces = 0usize;
    while spaces < bytes.len() && bytes[spaces] == b' ' {
        spaces += 1;
        if spaces > 3 {
            return None;
        }
    }
    Some(&line[spaces..])
}

fn parse_opening_fence(line: &str) -> Option<(u8, usize)> {
    let trimmed = strip_up_to_three_spaces(line)?;
    let first = *trimmed.as_bytes().first()?;
    if first != b'`' && first != b'~' {
        return None;
    }
    let count = trimmed.bytes().take_while(|&b| b == first).count();
    if count < 3 {
        return None;
    }
    let info = &trimmed[count..];
    if first == b'`' && info.contains('`') {
        return None;
    }
    Some((first, count))
}

fn is_closing_fence(line: &str, fence_char: u8, min_len: usize) -> bool {
    let Some(trimmed) = strip_up_to_three_spaces(line) else {
        return false;
    };
    let count = trimmed.bytes().take_while(|&b| b == fence_char).count();
    if count < min_len {
        return false;
    }
    trimmed[count..].bytes().all(|b| b == b' ' || b == b'\t')
}

fn parse_atx_heading(line: &str) -> Option<(usize, String)> {
    let trimmed = strip_up_to_three_spaces(line)?;
    let level = trimmed.bytes().take_while(|&b| b == b'#').count();
    if level == 0 || level > 6 {
        return None;
    }
    let rest = &trimmed[level..];
    if !rest.is_empty() && !rest.starts_with(' ') && !rest.starts_with('\t') {
        return None;
    }
    let mut title = rest.trim();
    if title.is_empty() {
        return None;
    }
    // Strip optional closing `#` sequence if preceded by whitespace (CommonMark ATX rule).
    let without_trailing_hashes = title.trim_end_matches('#');
    if without_trailing_hashes.len() < title.len()
        && (without_trailing_hashes.is_empty()
            || without_trailing_hashes.ends_with(' ')
            || without_trailing_hashes.ends_with('\t'))
    {
        title = without_trailing_hashes.trim_end();
    }
    if title.is_empty() {
        None
    } else {
        Some((level, title.to_owned()))
    }
}
