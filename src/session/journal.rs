use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn revision(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub seq: usize,
    pub ms: u64,
    pub run: usize,
    pub kind: String,
    pub data: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Artifact {
    pub result_ref: String,
    pub source: Value,
    pub revision: String,
    pub bytes: usize,
}

type StatEntry = (u64, u128, String, bool);

pub struct Journal {
    dir: PathBuf,
    file: File,
    _lock: File,
    pub events: Vec<Event>,
    stat_cache: RefCell<HashMap<PathBuf, StatEntry>>,
}

impl Journal {
    pub fn open(dir: &Path) -> Result<Self, String> {
        fs::create_dir_all(dir.join("blobs")).map_err(io_error)?;
        let lock = OpenOptions::new().create(true).truncate(false).read(true).write(true).open(dir.join("session.lock")).map_err(io_error)?;
        lock.try_lock().map_err(|err| format!("Session is already open or cannot be locked: {err}"))?;
        let mut file = OpenOptions::new().create(true).truncate(false).read(true).write(true).open(dir.join("events.jsonl")).map_err(io_error)?;
        let (events, length) = read_events(dir)?;
        if file.metadata().map_err(io_error)?.len() != length {
            file.set_len(length).map_err(io_error)?;
        }
        file.seek(SeekFrom::End(0)).map_err(io_error)?;
        Ok(Self { dir: dir.to_path_buf(), file, events, _lock: lock, stat_cache: RefCell::new(HashMap::new()) })
    }

    pub fn append(&mut self, run: usize, kind: &str, data: Value) -> Result<usize, String> {
        let seq = self.events.len() + 1;
        let ms = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
        let event = Event { seq, ms, run, kind: kind.to_owned(), data };
        let mut line = serde_json::to_vec(&event).map_err(|e| e.to_string())?;
        line.push(b'\n');
        self.file.write_all(&line).map_err(io_error)?;
        self.file.sync_data().map_err(io_error)?;
        self.events.push(event);
        Ok(seq)
    }

    pub fn put(&self, bytes: &[u8]) -> Result<String, String> {
        let hash = revision(bytes);
        let folder = self.dir.join("blobs");
        let path = folder.join(&hash);
        if !path.exists() {
            let mut temp = tempfile::NamedTempFile::new_in(folder).map_err(io_error)?;
            temp.write_all(bytes).map_err(io_error)?;
            temp.as_file().sync_all().map_err(io_error)?;
            temp.persist_noclobber(path).map_err(|e| io_error(e.error))?;
        }
        Ok(hash)
    }

    pub fn get(&self, hash: &str) -> Result<Vec<u8>, String> {
        get_blob(&self.dir, hash)
    }

    pub fn store_json(&self, value: &Value) -> Result<String, String> {
        self.put(&serde_json::to_vec(value).map_err(|e| e.to_string())?)
    }

    pub fn load_json(&self, hash: &Value) -> Result<Value, String> {
        serde_json::from_slice(&self.get(hash.as_str().ok_or("Missing payload hash")?)?).map_err(|e| e.to_string())
    }

    pub fn artifact(&mut self, run: usize, source: Value, bytes: &[u8]) -> Result<Artifact, String> {
        let hash = self.put(bytes)?;
        for event in self.events.iter().rev().filter(|e| e.kind == "artifact") {
            if event.data["revision"] == hash && event.data["source"] == source {
                return serde_json::from_value(event.data.clone()).map_err(|e| e.to_string());
            }
        }
        let result = Artifact { result_ref: format!("r{}", self.events.len() + 1), source, revision: hash, bytes: bytes.len() };
        self.append(run, "artifact", json!(result))?;
        Ok(result)
    }

    pub fn resolve(&self, reference: &str) -> Option<Artifact> {
        let seq = reference.strip_prefix('r')?.parse::<usize>().ok()?;
        let event = self.events.get(seq.checked_sub(1)?)?;
        if event.kind != "artifact" || event.data["result_ref"] != reference {
            return None;
        }
        serde_json::from_value(event.data.clone()).ok()
    }

    pub fn hash_cached_file(&self, path: &Path, meta: &fs::Metadata, retain: bool) -> Result<String, String> {
        let len = meta.len();
        let mtime_ns = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0);
        let now_ns = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        // Racy-clean guard: skip re-reading only when mtime is at least 2s older than now.
        let racy_clean = mtime_ns > 0 && now_ns.saturating_sub(mtime_ns) >= 2_000_000_000;
        let prev_retained = {
            let cache = self.stat_cache.borrow();
            if let Some((c_len, c_mtime, c_hash, c_ret)) = cache.get(path) {
                if racy_clean && *c_len == len && *c_mtime == mtime_ns && (!retain || *c_ret) {
                    return Ok(c_hash.clone());
                }
                (*c_ret, Some(c_hash.clone()))
            } else { (false, None) }
        };
        let bytes = fs::read(path).map_err(io_error)?;
        let hash = if retain { self.put(&bytes)? } else { revision(&bytes) };
        let kept = retain || (prev_retained.0 && prev_retained.1.as_deref() == Some(hash.as_str()));
        self.stat_cache.borrow_mut().insert(path.to_path_buf(), (len, mtime_ns, hash.clone(), kept));
        Ok(hash)
    }
}

pub fn read_events(dir: &Path) -> Result<(Vec<Event>, u64), String> {
    let raw = fs::read(dir.join("events.jsonl")).map_err(io_error)?;
    let len = raw.iter().rposition(|&b| b == b'\n').map_or(0, |last| last + 1);
    let mut events = Vec::new();
    for line in raw[..len].split_inclusive(|&b| b == b'\n') {
        let event: Event = serde_json::from_slice(line).map_err(|e| format!("Journal entry {}: {e}", events.len() + 1))?;
        if event.seq != events.len() + 1 {
            return Err(format!("Missing or out-of-order journal entry {}", events.len() + 1));
        }
        events.push(event);
    }
    Ok((events, len as u64))
}

pub fn get_blob(dir: &Path, hash: &str) -> Result<Vec<u8>, String> {
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("Invalid content hash".to_owned());
    }
    let bytes = fs::read(dir.join("blobs").join(hash)).map_err(io_error)?;
    if revision(&bytes) != hash {
        return Err(format!("Content hash mismatch: {hash}"));
    }
    Ok(bytes)
}

fn io_error(err: io::Error) -> String {
    err.to_string()
}
