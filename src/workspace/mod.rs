use std::fs;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone)]
pub struct WorkspaceConfig {
    pub root: PathBuf,
    pub max_file_bytes: usize,
    pub max_list_entries: usize,
}

impl WorkspaceConfig {
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into(), max_file_bytes: 8 * 1024 * 1024, max_list_entries: 20_000 }
    }
}

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
        Ok(Self { root: canon, config })
    }
    pub fn root(&self) -> &Path { &self.root }
    pub fn config(&self) -> &WorkspaceConfig { &self.config }
    pub fn relative_display(&self, canon: &Path) -> String {
        match canon.strip_prefix(&self.root) {
            Ok(rel) if rel.as_os_str().is_empty() => ".".to_owned(),
            Ok(rel) => normalize_slashes(rel),
            Err(_) => normalize_slashes(canon),
        }
    }
}

fn normalize_slashes(path: &Path) -> String {
    let parts: Vec<String> = path.components().filter_map(|c| match c {
        Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
        _ => None,
    }).collect();
    if parts.is_empty() { ".".to_owned() } else { parts.join("/") }
}
