pub mod tools;

use std::fs;
use std::path::{Component, Path, PathBuf};

use serde_json::{Map, Value};

use crate::budget::{json_byte_len, to_json_string, MAX_RESPONSE_BYTES};
use crate::error::{ErrorCode, ErrorPayload};
use crate::registry;
use crate::store::{FoldEnvelope, FoldStore, IngestOutcome};
use crate::transport::{ToolExecutionOutput, ToolExecutor};

/// Safety limits for read-only workspace producer tools.
#[derive(Debug, Clone)]
pub struct WorkspaceConfig {
    pub root: PathBuf,
    pub max_file_bytes: usize,
    pub max_list_entries: usize,
    pub max_grep_matches: usize,
    pub max_grep_file_bytes: usize,
}

impl WorkspaceConfig {
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            max_file_bytes: 8 * 1024 * 1024,
            max_list_entries: 5000,
            max_grep_matches: 10_000,
            max_grep_file_bytes: 2 * 1024 * 1024,
        }
    }
}

/// Canonicalized workspace root ensuring strict sandbox containment for all file reads.
#[derive(Debug, Clone)]
pub struct WorkspaceContext {
    root: PathBuf,
    config: WorkspaceConfig,
}

impl WorkspaceContext {
    pub fn new(config: WorkspaceConfig) -> Result<Self, String> {
        let canon = fs::canonicalize(&config.root)
            .map_err(|e| format!("Failed to canonicalize workspace root `{}`: {e}", config.root.display()))?;
        if !canon.is_dir() {
            return Err(format!("Workspace root `{}` is not a directory", canon.display()));
        }
        Ok(Self {
            root: canon,
            config,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config(&self) -> &WorkspaceConfig {
        &self.config
    }

    /// Resolves `rel` strictly inside the workspace root, returning `(canonical_path, normalized_rel_path)`.
    pub fn resolve_existing_path(&self, rel: &str) -> Result<(PathBuf, String), ErrorPayload> {
        let trimmed = rel.trim();
        if trimmed.is_empty() {
            return Err(ErrorPayload::new(
                ErrorCode::InvalidArgument,
                Some("path"),
                "INVALID_PATH",
            ));
        }

        let rel_path = Path::new(trimmed);
        if rel_path.is_absolute() {
            return Err(ErrorPayload::new(
                ErrorCode::InvalidArgument,
                Some("path"),
                "PATH_OUTSIDE_WORKSPACE",
            ));
        }

        let mut candidate = self.root.clone();
        for comp in rel_path.components() {
            match comp {
                Component::CurDir => {}
                Component::Normal(part) => candidate.push(part),
                Component::ParentDir | Component::Prefix(_) | Component::RootDir => {
                    return Err(ErrorPayload::new(
                        ErrorCode::InvalidArgument,
                        Some("path"),
                        "PATH_OUTSIDE_WORKSPACE",
                    ));
                }
            }
        }

        if !candidate.exists() {
            return Err(ErrorPayload::new(
                ErrorCode::InvalidArgument,
                Some("path"),
                "FILE_NOT_FOUND",
            ));
        }

        let canon = fs::canonicalize(&candidate).map_err(|_| {
            ErrorPayload::new(ErrorCode::InvalidArgument, Some("path"), "FILE_NOT_FOUND")
        })?;
        if !canon.starts_with(&self.root) {
            return Err(ErrorPayload::new(
                ErrorCode::InvalidArgument,
                Some("path"),
                "PATH_OUTSIDE_WORKSPACE",
            ));
        }

        let display_rel = self.relative_display(&canon);
        Ok((canon, display_rel))
    }

    pub fn relative_display(&self, canon: &Path) -> String {
        match canon.strip_prefix(&self.root) {
            Ok(rel) if rel.as_os_str().is_empty() => ".".to_owned(),
            Ok(rel) => normalize_slashes(rel),
            Err(_) => normalize_slashes(canon),
        }
    }
}

fn normalize_slashes(path: &Path) -> String {
    let mut parts = Vec::new();
    for comp in path.components() {
        if let Component::Normal(s) = comp {
            parts.push(s.to_string_lossy().into_owned());
        }
    }
    if parts.is_empty() {
        ".".to_owned()
    } else {
        parts.join("/")
    }
}

/// Bridges real workspace producer tools (`list_files`, `read_file`, `grep_files`)
/// with the pure SourceHarness `FoldStore` + `search_result` / `read_result` kernel.
pub struct WorkspaceToolRouter {
    context: WorkspaceContext,
    store: FoldStore,
}

impl WorkspaceToolRouter {
    pub fn new(context: WorkspaceContext) -> Self {
        Self::with_store(context, FoldStore::new())
    }

    pub fn with_store(context: WorkspaceContext, store: FoldStore) -> Self {
        Self { context, store }
    }

    pub fn context(&self) -> &WorkspaceContext {
        &self.context
    }

    pub fn store(&self) -> &FoldStore {
        &self.store
    }

    /// Routes raw producer output through SPEC §1.2:
    /// small -> inline JSON envelope (`<= 8 KiB`), large -> `FoldStore` + `FoldEnvelope`.
    pub fn deliver_tool_output(
        &self,
        tool_name: &str,
        meta: Map<String, Value>,
        raw_text: String,
    ) -> ToolExecutionOutput {
        match self.store.ingest(raw_text) {
            IngestOutcome::Inline(content) => {
                let mut obj = Map::new();
                obj.insert("status".to_owned(), Value::String("ok".to_owned()));
                obj.insert("delivery".to_owned(), Value::String("inline".to_owned()));
                obj.insert("tool".to_owned(), Value::String(tool_name.to_owned()));
                for (k, v) in &meta {
                    obj.insert(k.clone(), v.clone());
                }
                obj.insert("content".to_owned(), Value::String(content.clone()));
                let envelope = Value::Object(obj);
                if json_byte_len(&envelope) <= MAX_RESPONSE_BYTES {
                    ToolExecutionOutput {
                        output_json: to_json_string(&envelope),
                        delivery: Some("inline"),
                        result_ref: None,
                    }
                } else {
                    // JSON escaping expansion pushed an under-threshold payload over 8 KiB: fold safely.
                    let fold_env = self.store.insert(content);
                    folded_output(tool_name, meta, fold_env)
                }
            }
            IngestOutcome::Folded(fold_env) => folded_output(tool_name, meta, fold_env),
        }
    }
}

fn folded_output(tool: &str, meta: Map<String, Value>, mut envelope: FoldEnvelope) -> ToolExecutionOutput {
    loop {
        let mut value = serde_json::to_value(&envelope).expect("FoldEnvelope serialization is infallible");
        let obj = value.as_object_mut().expect("FoldEnvelope is an object");
        obj.extend(meta.clone());
        obj.insert("tool".to_owned(), Value::String(tool.to_owned()));
        obj.insert("delivery".to_owned(), Value::String("folded".to_owned()));
        if json_byte_len(&value) <= MAX_RESPONSE_BYTES {
            return ToolExecutionOutput {
                output_json: to_json_string(&value), delivery: Some("folded"), result_ref: Some(envelope.result_ref),
            };
        }
        if !envelope.preview.is_empty() {
            envelope.preview.clear();
        } else if envelope.sections.pop().is_some() {
            envelope.index_status = "truncated";
        } else {
            return ToolExecutionOutput {
                output_json: ErrorPayload::new(ErrorCode::InvalidArgument, None, "METADATA_EXCEEDS_RESPONSE_BUDGET").to_json_string(),
                delivery: None, result_ref: None,
            };
        }
    }
}

impl ToolExecutor for WorkspaceToolRouter {
    fn tool_definitions(&self) -> Vec<Value> {
        let mut defs = registry::tool_definitions();
        defs.extend(tools::producer_tool_definitions());
        defs
    }

    fn dispatch(&self, name: &str, raw_args: &str) -> ToolExecutionOutput {
        match name {
            registry::SEARCH_RESULT_NAME | registry::READ_RESULT_NAME => ToolExecutionOutput {
                output_json: registry::dispatch(name, raw_args, &self.store),
                delivery: None,
                result_ref: None,
            },
            tools::LIST_FILES_NAME => tools::exec_list_files(raw_args, self),
            tools::READ_FILE_NAME => tools::exec_read_file(raw_args, self),
            tools::GREP_FILES_NAME => tools::exec_grep_files(raw_args, self),
            _ => ToolExecutionOutput {
                output_json: registry::dispatch(name, raw_args, &self.store),
                delivery: None,
                result_ref: None,
            },
        }
    }
}
