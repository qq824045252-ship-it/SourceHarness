use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Entry {
    pub id: usize,
    pub text: String,
    pub received_ms: u64,
    pub steer_run: Option<usize>,
}

pub(super) struct State {
    file: File,
    pub entries: Vec<Entry>,
    pub active_run: Option<usize>,
    write_failed: bool,
}

#[derive(Clone)]
pub struct InputInbox(Arc<Mutex<State>>);

impl InputInbox {
    pub(super) fn open(dir: &Path) -> Result<Self, String> {
        let path = dir.join("inbox.jsonl");
        match fs::symlink_metadata(&path) {
            Ok(meta) if !meta.is_file() || fs::canonicalize(&path).map_err(error)? != path => {
                return Err("Session inbox cannot be redirected".to_owned());
            }
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(error(err)),
        }
        let mut file = OpenOptions::new().create(true).truncate(false).read(true).write(true)
            .open(path).map_err(error)?;
        let mut raw = Vec::new();
        file.read_to_end(&mut raw).map_err(error)?;
        let end = raw.iter().rposition(|&b| b == b'\n').map_or(0, |last| last + 1);
        let mut entries = Vec::new();
        for line in raw[..end].split_inclusive(|&b| b == b'\n') {
            let entry: Entry = serde_json::from_slice(line).map_err(error)?;
            if entry.id != entries.len() + 1 {
                return Err("Missing or out-of-order session inbox entry".to_owned());
            }
            entries.push(entry);
        }
        if end != raw.len() {
            file.set_len(end as u64).map_err(error)?;
        }
        file.seek(SeekFrom::End(0)).map_err(error)?;
        Ok(Self(Arc::new(Mutex::new(State { file, entries, active_run: None, write_failed: false }))))
    }

    pub(super) fn lock(&self) -> Result<MutexGuard<'_, State>, String> {
        self.0.lock().map_err(|_| "Session inbox lock was interrupted".to_owned())
    }

    /// Accept a terminal message. A correction received during a run stays tied to that run.
    pub fn accept(&self, text: String, steering: bool) -> Result<(), String> {
        let received_ms = SystemTime::now().duration_since(UNIX_EPOCH)
            .unwrap_or_default().as_millis() as u64;
        let mut state = self.lock()?;
        if state.write_failed {
            return Err("Session inbox cannot accept more input. Reopen the session first.".to_owned());
        }
        let entry = Entry {
            id: state.entries.len() + 1,
            text,
            received_ms,
            steer_run: if steering { state.active_run } else { None },
        };
        let mut bytes = serde_json::to_vec(&entry).map_err(error)?;
        bytes.push(b'\n');
        if let Err(err) = state.file.write_all(&bytes).and_then(|()| state.file.sync_data()) {
            state.write_failed = true;
            return Err(error(err));
        }
        state.entries.push(entry);
        Ok(())
    }
}

fn error(err: impl std::fmt::Display) -> String {
    format!("Session inbox: {err}")
}
