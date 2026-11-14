use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const MAX_PENDING: usize = 1_000;
const MAX_RECORD_BYTES: u64 = 16_384;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActiveExec {
    version: u32,
    exec_id: String,
    pub session_id: String,
    turn_id: String,
    pub node_repl_pid: u32,
    pub kernel_pid: u32,
    pub started_at_ms: u64,
}

#[derive(Default)]
struct Pending {
    records: HashMap<String, ActiveExec>,
    error: Option<String>,
}

pub struct ReplActivity {
    directory: PathBuf,
    watcher: Option<RecommendedWatcher>,
    pending: Arc<Mutex<Pending>>,
}

impl ReplActivity {
    /// Native execution records are brief; capture their IDs before the runtime removes the file.
    pub fn new(codex_home: &Path) -> Self {
        let mut activity = Self {
            directory: codex_home.join("node_repl").join("active_execs"),
            watcher: None,
            pending: Arc::new(Mutex::new(Pending::default())),
        };
        activity.ensure_watching();
        activity
    }

    /// Only the active registry is watched; historical rollouts and databases are never rescanned.
    pub fn drain(&mut self) -> Vec<ActiveExec> {
        self.ensure_watching();
        self.pending
            .lock()
            .unwrap()
            .records
            .drain()
            .map(|(_, record)| record)
            .collect()
    }

    pub fn last_error(&self) -> Option<String> {
        self.pending.lock().unwrap().error.clone()
    }

    /// A not-yet-created registry is normal and is retried on the next process sample.
    fn ensure_watching(&mut self) {
        if self.watcher.is_some() || !self.directory.is_dir() {
            return;
        }
        let pending = Arc::clone(&self.pending);
        let directory = self.directory.clone();
        let result = notify::recommended_watcher(move |event| {
            capture_event(event, &directory, &pending);
        })
        .and_then(|mut watcher| {
            watcher.watch(&self.directory, RecursiveMode::NonRecursive)?;
            Ok(watcher)
        });
        match result {
            Ok(watcher) => {
                self.watcher = Some(watcher);
                self.scan_existing();
            }
            Err(error) => {
                self.pending.lock().unwrap().error =
                    Some(format!("无法监听 REPL 执行标识：{error}"));
            }
        }
    }

    /// Subscribe before this initial scan so records created during startup are also captured.
    fn scan_existing(&self) {
        match fs::read_dir(&self.directory) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    capture_path(&entry.path(), &self.directory, &self.pending);
                }
            }
            Err(error) => {
                self.pending.lock().unwrap().error =
                    Some(format!("无法读取 REPL 执行标识目录：{error}"));
            }
        }
    }
}

fn capture_event(event: notify::Result<Event>, directory: &Path, pending: &Mutex<Pending>) {
    match event {
        Ok(event) if matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_)) => {
            for path in event.paths {
                capture_path(&path, directory, pending);
            }
        }
        Ok(_) => {}
        Err(error) => {
            pending.lock().unwrap().error = Some(format!("REPL 执行标识监听失败：{error}"));
        }
    }
}

/// Retain just the runtime's process/session IDs; partial writes and removed records are skipped.
fn capture_path(path: &Path, directory: &Path, pending: &Mutex<Pending>) {
    if path.parent() != Some(directory) || path.extension().is_none_or(|ext| ext != "json") {
        return;
    }
    let Some(record) = read_record(path) else {
        return;
    };
    let mut pending = pending.lock().unwrap();
    if pending.records.len() >= MAX_PENDING && !pending.records.contains_key(&record.exec_id) {
        pending.error = Some("REPL 执行标识队列已满，部分进程可能保持未归属。".into());
        return;
    }
    pending.records.insert(record.exec_id.clone(), record);
}

fn read_record(path: &Path) -> Option<ActiveExec> {
    if fs::metadata(path).ok()?.len() > MAX_RECORD_BYTES {
        return None;
    }
    let record: ActiveExec = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    (record.version == 1
        && !record.exec_id.is_empty()
        && !record.turn_id.is_empty()
        && record.node_repl_pid > 0
        && record.kernel_pid > 0
        && record.started_at_ms > 0)
        .then_some(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    // A callback retains a native record even when its short-lived file is gone before sampling.
    #[test]
    fn captures_record_before_runtime_removes_it() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("codex-repl-record-{}-{suffix}", std::process::id()));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("execution.json");
        fs::write(&path, br#"{"version":1,"execId":"exec","sessionId":"session","turnId":"turn","nodeReplPid":10,"kernelPid":11,"startedAtMs":2000}"#).unwrap();
        let pending = Mutex::new(Pending::default());
        capture_path(&path, &directory, &pending);
        fs::remove_file(&path).unwrap();
        let records = &pending.lock().unwrap().records;
        assert_eq!(records.len(), 1);
        assert_eq!(records["exec"].node_repl_pid, 10);
        fs::remove_dir(&directory).unwrap();
    }
}
