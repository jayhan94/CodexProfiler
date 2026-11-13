use crate::model::{self, Session, Snapshot};
use eframe::egui;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{value::RawValue, Value};
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};
use walkdir::WalkDir;

const MAX_SESSIONS: usize = 32;
const CHUNK_BYTES: u64 = 8 * 1024 * 1024;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// One collector serves all chats; bounded channels prevent a hidden UI backlog.
pub fn start(home: PathBuf, db_path: PathBuf, ctx: egui::Context) -> mpsc::Receiver<Snapshot> {
    let (tx, rx) = mpsc::sync_channel(1);
    thread::spawn(move || {
        if let Err(error) = run(home, db_path, &ctx, &tx) {
            let _ = tx.try_send(Snapshot {
                sessions: Vec::new(),
                error: Some(error.to_string()),
                updated_ms: model::now_ms(),
            });
            ctx.request_repaint();
        }
    });
    rx
}

#[derive(Deserialize)]
struct Envelope<'a> {
    #[serde(rename = "type")]
    kind: String,
    timestamp: Option<String>,
    timestamp_ms: Option<i64>,
    thread_id: Option<String>,
    #[serde(borrow)]
    payload: Option<&'a RawValue>,
}

// Unknown fields (including tool output and full prompts) are skipped by the deserializer.
#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct ThinPayload {
    #[serde(rename = "type")]
    kind: Option<String>,
    id: Option<String>,
    cwd: Option<String>,
    model: Option<String>,
    turn_id: Option<String>,
    thread_id: Option<String>,
    response_id: Option<String>,
    call_id: Option<String>,
    name: Option<String>,
    tool_use_id: Option<String>,
    tool_name: Option<String>,
    hook_event_name: Option<String>,
    status: Option<String>,
    started_at_ms: Option<i64>,
    completed_at_ms: Option<i64>,
    info: Option<ThinInfo>,
    usage: Option<ThinUsage>,
    turn_token_usage: Option<ThinUsage>,
    thread_token_usage: Option<ThinUsage>,
    token_usage: Option<ThinUsage>,
    item: Option<ThinItem>,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct ThinInfo {
    total_token_usage: Option<ThinUsage>,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct ThinUsage {
    output_tokens: Option<u64>,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct ThinItem {
    #[serde(rename = "type")]
    kind_type: Option<String>,
    id: Option<String>,
    kind: Option<String>,
    #[serde(deserialize_with = "command_summary")]
    command: Option<String>,
    server: Option<String>,
    tool: Option<String>,
    exit_code: Option<i64>,
    content: Option<Vec<ThinContent>>,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct ThinContent {
    #[serde(deserialize_with = "short_text")]
    text: Option<String>,
}

// Keep at most a short display title, never an additional copy of a full user prompt.
fn short_text<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error> {
    struct Text;
    impl<'de> serde::de::Visitor<'de> for Text {
        type Value = Option<String>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("optional text")
        }
        fn visit_str<E: serde::de::Error>(
            self,
            value: &str,
        ) -> std::result::Result<Self::Value, E> {
            Ok(Some(value.chars().take(160).collect()))
        }
        fn visit_none<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
            Ok(None)
        }
        fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Self::Value, E> {
            Ok(None)
        }
    }
    deserializer.deserialize_any(Text)
}

// Codex command items use an argv array; older records may contain a string.
fn command_summary<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error> {
    let value = Value::deserialize(deserializer)?;
    let command = match value {
        Value::String(text) => text,
        Value::Array(parts) => parts
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" "),
        _ => return Ok(None),
    };
    Ok(Some(command.chars().take(160).collect()))
}

/// Parse only monitored fields. RawValue borrows the original line instead of cloning its payload.
fn thin_record(line: &[u8]) -> std::result::Result<Option<Value>, serde_json::Error> {
    let envelope: Envelope<'_> = serde_json::from_slice(line)?;
    if !matches!(
        envelope.kind.as_str(),
        "session_meta"
            | "turn_context"
            | "event_msg"
            | "response_item"
            | "token_usage_record"
            | "monitor_hook"
            | "hook_event"
    ) {
        return Ok(None);
    }
    let Some(raw_payload) = envelope.payload else {
        return Ok(None);
    };
    let mut payload: ThinPayload = serde_json::from_str(raw_payload.get())?;
    if envelope.kind == "response_item"
        && !matches!(
            payload.kind.as_deref(),
            Some(
                "function_call"
                    | "custom_tool_call"
                    | "function_call_output"
                    | "custom_tool_call_output"
            )
        )
    {
        return Ok(None);
    }
    // Agent message and reasoning text are never used by the monitor.
    if let Some(item) = &mut payload.item {
        if item.kind_type.as_deref() != Some("UserMessage") {
            item.content = None;
        }
    }
    Ok(Some(
        serde_json::json!({"type":envelope.kind, "timestamp":envelope.timestamp,
        "timestamp_ms":envelope.timestamp_ms, "thread_id":envelope.thread_id,
        "payload":payload}),
    ))
}

struct Store {
    db: Connection,
    sessions: HashMap<String, Session>,
    error: Option<String>,
}

impl Store {
    /// Source cursors and reduced session state share a transaction on every batch.
    fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }
        let db = Connection::open(path)?;
        db.busy_timeout(Duration::from_secs(2))?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA cache_size=-4096;
             CREATE TABLE IF NOT EXISTS sources(
                 path TEXT PRIMARY KEY, offset INTEGER NOT NULL, thread_id TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS sessions(
                 id TEXT PRIMARY KEY, last_ms INTEGER NOT NULL, state TEXT NOT NULL);",
        )?;
        let sessions = {
            let mut statement =
                db.prepare("SELECT id,state FROM sessions ORDER BY last_ms DESC LIMIT 32")?;
            let rows = statement
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            let mut sessions = HashMap::new();
            for row in rows {
                let (id, state) = row?;
                sessions.insert(id, serde_json::from_str(&state)?);
            }
            sessions
        };
        Ok(Self {
            db,
            sessions,
            error: None,
        })
    }

    fn snapshot(&self) -> Snapshot {
        let mut sessions: Vec<_> = self.sessions.values().cloned().collect();
        sessions.sort_by_key(|s| std::cmp::Reverse(s.last_ms));
        sessions.truncate(MAX_SESSIONS);
        Snapshot {
            sessions,
            error: self.error.clone(),
            updated_ms: model::now_ms(),
        }
    }

    fn ensure_session(&mut self, id: &str) -> Result<()> {
        if !self.sessions.contains_key(id) {
            let state: Option<String> = self
                .db
                .query_row("SELECT state FROM sessions WHERE id=?1", [id], |r| r.get(0))
                .optional()?;
            let session = match state {
                Some(state) => serde_json::from_str(&state)?,
                None => Session {
                    id: id.to_owned(),
                    ..Session::default()
                },
            };
            self.sessions.insert(id.to_owned(), session);
        }
        Ok(())
    }

    /// Replaying or resuming a file never counts previously committed tokens twice.
    fn ingest(&mut self, path: &Path) -> Result<bool> {
        let key = path.to_string_lossy().into_owned();
        let saved: Option<(u64, String)> = self
            .db
            .query_row(
                "SELECT offset,thread_id FROM sources WHERE path=?1",
                [&key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (mut offset, mut thread_id) = saved.unwrap_or_default();
        let mut file = File::open(path)?;
        let length = file.metadata()?.len();
        if offset > length {
            offset = 0;
            if !thread_id.is_empty() {
                self.sessions.insert(
                    thread_id.clone(),
                    Session {
                        id: thread_id.clone(),
                        ..Session::default()
                    },
                );
            }
            self.error = Some(format!(
                "{}: source truncated; rebuilding its state",
                path.display()
            ));
        }
        if offset == length {
            return Ok(false);
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = Vec::new();
        (&mut file).take(CHUNK_BYTES).read_to_end(&mut bytes)?;
        let Some(end) = bytes.iter().rposition(|b| *b == b'\n') else {
            return self.handle_incomplete(
                path,
                offset,
                length,
                &mut file,
                bytes.len(),
                &thread_id,
            );
        };
        let mut changed = HashSet::new();
        for line in bytes[..=end]
            .split(|b| *b == b'\n')
            .filter(|s| !s.is_empty())
        {
            self.apply_line(line, &mut thread_id, &mut changed, path)?;
        }
        let next = offset + end as u64 + 1;
        if let Err(error) = self.commit(&key, next, &thread_id, &changed) {
            // Discard uncommitted reductions before the file is retried.
            for id in &changed {
                self.sessions.remove(id);
                self.ensure_session(id)?;
            }
            return Err(error);
        }
        self.trim_cache();
        Ok(next < length && bytes.len() as u64 == CHUNK_BYTES)
    }

    fn apply_line(
        &mut self,
        line: &[u8],
        thread_id: &mut String,
        changed: &mut HashSet<String>,
        path: &Path,
    ) -> Result<()> {
        let row: Value = match thin_record(line) {
            Ok(Some(row)) => row,
            Ok(None) => return Ok(()),
            Err(error) => {
                self.error = Some(format!("{}: invalid JSON record: {error}", path.display()));
                return Ok(());
            }
        };
        let payload = &row["payload"];
        if row["type"] == "session_meta" {
            if let Some(id) = payload["id"].as_str() {
                *thread_id = id.to_owned();
            }
        }
        let routed = payload["thread_id"]
            .as_str()
            .or_else(|| row["thread_id"].as_str())
            .unwrap_or(thread_id)
            .to_owned();
        if routed.is_empty() {
            self.error = Some(format!("{}: record has no chat identity", path.display()));
            return Ok(());
        }
        self.ensure_session(&routed)?;
        if let Some(session) = self.sessions.get_mut(&routed) {
            model::apply(session, &row);
        }
        changed.insert(routed);
        Ok(())
    }

    /// A normal unfinished JSONL tail is retried only after the next file change.
    fn handle_incomplete(
        &mut self,
        path: &Path,
        offset: u64,
        length: u64,
        file: &mut File,
        read: usize,
        thread_id: &str,
    ) -> Result<bool> {
        if read as u64 != CHUNK_BYTES || offset + read as u64 >= length {
            return Ok(false);
        }
        // Oversized output rows are skipped without allocating their entire contents.
        let mut reader = BufReader::new(file);
        let mut skipped = read as u64;
        loop {
            let available = reader.fill_buf()?;
            if available.is_empty() {
                return Ok(false);
            }
            let count = available.iter().position(|b| *b == b'\n').map(|n| n + 1);
            let consumed = count.unwrap_or(available.len());
            skipped += consumed as u64;
            reader.consume(consumed);
            if count.is_some() {
                break;
            }
        }
        self.error = Some(format!(
            "{}: skipped record larger than 8 MiB",
            path.display()
        ));
        self.commit(
            &path.to_string_lossy(),
            offset + skipped,
            thread_id,
            &HashSet::new(),
        )?;
        Ok(offset + skipped < length)
    }

    fn commit(
        &mut self,
        path: &str,
        offset: u64,
        thread_id: &str,
        changed: &HashSet<String>,
    ) -> Result<()> {
        let transaction = self.db.transaction()?;
        for id in changed {
            let session = &self.sessions[id];
            transaction.execute(
                "INSERT INTO sessions(id,last_ms,state) VALUES(?1,?2,?3)
                 ON CONFLICT(id) DO UPDATE SET last_ms=excluded.last_ms,state=excluded.state",
                params![id, session.last_ms, serde_json::to_string(session)?],
            )?;
        }
        transaction.execute(
            "INSERT INTO sources(path,offset,thread_id) VALUES(?1,?2,?3)
             ON CONFLICT(path) DO UPDATE SET offset=excluded.offset,thread_id=excluded.thread_id",
            params![path, offset, thread_id],
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn trim_cache(&mut self) {
        if self.sessions.len() <= MAX_SESSIONS {
            return;
        }
        let mut ids: Vec<_> = self
            .sessions
            .values()
            .map(|s| (s.last_ms, s.id.clone()))
            .collect();
        ids.sort();
        for (_, id) in ids.into_iter().take(self.sessions.len() - MAX_SESSIONS) {
            self.sessions.remove(&id);
        }
    }
}

/// Discovery walks once on startup; ongoing updates arrive from OS file notifications.
fn recent_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut recent = BinaryHeap::new();
    for entry in WalkDir::new(root).into_iter() {
        let entry = entry?;
        if is_rollout(entry.path()) && entry.file_type().is_file() {
            recent.push(std::cmp::Reverse((
                entry.metadata()?.modified()?,
                entry.into_path(),
            )));
            if recent.len() > MAX_SESSIONS {
                recent.pop();
            }
        }
    }
    let mut files: Vec<_> = recent.into_iter().map(|entry| entry.0).collect();
    files.sort_by_key(|(time, _)| std::cmp::Reverse(*time));
    Ok(files
        .into_iter()
        .take(MAX_SESSIONS)
        .map(|(_, path)| path)
        .collect())
}

fn is_rollout(path: &Path) -> bool {
    path.extension().is_some_and(|s| s == "jsonl")
        && path
            .file_name()
            .is_some_and(|s| s.to_string_lossy().starts_with("rollout-"))
}

fn enqueue(path: PathBuf, queue: &mut VecDeque<PathBuf>, queued: &mut HashSet<PathBuf>) {
    if path.is_file() && queued.insert(path.clone()) {
        queue.push_back(path);
    }
}

fn publish(
    store: &Store,
    ctx: &egui::Context,
    tx: &mpsc::SyncSender<Snapshot>,
) -> std::result::Result<bool, ()> {
    match tx.try_send(store.snapshot()) {
        Ok(()) => {
            ctx.request_repaint();
            Ok(true)
        }
        Err(mpsc::TrySendError::Full(_)) => Ok(false),
        Err(mpsc::TrySendError::Disconnected(_)) => Err(()),
    }
}

fn watch_sources(
    root: &Path,
    hooks_dir: &Path,
    event_tx: mpsc::SyncSender<notify::Result<notify::Event>>,
    signal: Arc<AtomicBool>,
) -> Result<RecommendedWatcher> {
    let mut watcher = RecommendedWatcher::new(
        move |event| {
            if event_tx.try_send(event).is_err() {
                signal.store(true, Ordering::Relaxed);
            }
        },
        notify::Config::default(),
    )?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    if hooks_dir.exists() {
        watcher.watch(hooks_dir, RecursiveMode::NonRecursive)?;
    }
    Ok(watcher)
}

/// File events are coalesced; overflow triggers a bounded source rediscovery.
fn run(
    home: PathBuf,
    db_path: PathBuf,
    ctx: &egui::Context,
    tx: &mpsc::SyncSender<Snapshot>,
) -> Result<()> {
    let root = home.join("sessions");
    let hooks_dir = home.join("monitor");
    let hooks = hooks_dir.join("hooks.jsonl");
    let mut store = Store::open(&db_path)?;
    let (event_tx, events) = mpsc::sync_channel(64);
    let overflow = Arc::new(AtomicBool::new(false));
    let _watcher = watch_sources(&root, &hooks_dir, event_tx, overflow.clone())?;
    let mut queue = VecDeque::new();
    let mut queued = HashSet::new();
    for path in recent_files(&root)?
        .into_iter()
        .chain(std::iter::once(hooks.clone()))
    {
        enqueue(path, &mut queue, &mut queued);
    }
    let mut last_publish = Instant::now();
    let mut dirty = true;
    loop {
        if let Some(path) = queue.pop_front() {
            queued.remove(&path);
            match store.ingest(&path) {
                Ok(true) => enqueue(path, &mut queue, &mut queued),
                Ok(false) => (),
                Err(error) => store.error = Some(format!("{}: {error}", path.display())),
            }
            dirty = true;
        }
        if dirty && (queue.is_empty() || last_publish.elapsed() >= Duration::from_millis(250)) {
            match publish(&store, ctx, tx) {
                Ok(sent) => dirty = !sent,
                Err(()) => return Ok(()),
            }
            last_publish = Instant::now();
        }
        let event = if queue.is_empty() {
            let wait = if dirty {
                Duration::from_millis(250)
            } else {
                Duration::from_secs(5)
            };
            events.recv_timeout(wait).ok()
        } else {
            events.try_recv().ok()
        };
        if let Some(event) = event {
            handle_event(event, &hooks, &mut queue, &mut queued, &mut store);
        }
        if overflow.swap(false, Ordering::Relaxed) {
            store.error = Some("File event queue overflow: rediscovered recent sources".into());
            for path in recent_files(&root)?
                .into_iter()
                .chain(std::iter::once(hooks.clone()))
            {
                enqueue(path, &mut queue, &mut queued);
            }
            dirty = true;
        }
    }
}

fn handle_event(
    event: notify::Result<notify::Event>,
    hooks: &Path,
    queue: &mut VecDeque<PathBuf>,
    queued: &mut HashSet<PathBuf>,
    store: &mut Store,
) {
    match event {
        Ok(event) if !event.kind.is_access() => {
            for path in event.paths {
                if is_rollout(&path) || path == hooks {
                    enqueue(path, queue, queued);
                }
            }
        }
        Ok(_) => (),
        Err(error) => store.error = Some(format!("File watcher: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn fixture(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "codex-monitor-{name}-{}-{}",
            std::process::id(),
            model::now_ms()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    /// A partial final record must survive restart without advancing its cursor.
    #[test]
    fn partial_tail_and_restart() {
        let dir = fixture("partial");
        let file = dir.join("rollout-test.jsonl");
        let db = dir.join("state.sqlite");
        let meta = "{\"type\":\"session_meta\",\"payload\":{\"id\":\"chat-a\"}}\n";
        fs::write(&file, format!("{meta}{{\"type\":\"turn_context\"")).unwrap();
        let mut store = Store::open(&db).unwrap();
        store.ingest(&file).unwrap();
        let offset: u64 = store
            .db
            .query_row("SELECT offset FROM sources", [], |r| r.get(0))
            .unwrap();
        assert_eq!(offset, meta.len() as u64);
        drop(store);
        File::options()
            .append(true)
            .open(&file)
            .unwrap()
            .write_all(b",\"payload\":{\"model\":\"test-model\"}}\n")
            .unwrap();
        let mut store = Store::open(&db).unwrap();
        store.ingest(&file).unwrap();
        assert_eq!(store.sessions["chat-a"].model, "test-model");
        assert!(!store.ingest(&file).unwrap());
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }

    /// Chats sharing a root session stay isolated, including an in-flight request after restart.
    #[test]
    fn mixed_chat_hooks_survive_restart() {
        let dir = fixture("mixed");
        let file = dir.join("hooks.jsonl");
        let db = dir.join("state.sqlite");
        let hook = |chat: &str, event: &str, time: i64, tokens: u64| {
            serde_json::json!({"type":"monitor_hook", "timestamp_ms":time,
                "payload":{"thread_id":chat,"session_id":"shared-root",
                    "turn_id":"turn-1","hook_event_name":event,
                    "token_usage":{"output_tokens":tokens}}})
            .to_string()
                + "\n"
        };
        let rows = hook("chat-a", "UserPromptSubmit", 100, 0)
            + &hook("chat-b", "UserPromptSubmit", 200, 0)
            + &hook("chat-a", "PreModelRequest", 1000, 0)
            + &hook("chat-b", "PreModelRequest", 2000, 0)
            + &hook("chat-a", "PostModelRequest", 3000, 100);
        fs::write(&file, rows).unwrap();
        let mut store = Store::open(&db).unwrap();
        store.ingest(&file).unwrap();
        assert_eq!(store.sessions["chat-a"].request_tps, Some(50.0));
        assert_eq!(store.sessions["chat-b"].request_tps, None);
        drop(store);
        let mut store = Store::open(&db).unwrap();
        assert_eq!(store.sessions["chat-b"].request_started_ms, Some(2000));
        File::options()
            .append(true)
            .open(&file)
            .unwrap()
            .write_all(hook("chat-b", "PostModelRequest", 6000, 200).as_bytes())
            .unwrap();
        store.ingest(&file).unwrap();
        assert_eq!(store.sessions["chat-b"].request_tps, Some(50.0));
        assert_eq!(store.sessions["chat-a"].request_tps, Some(50.0));
        assert!(!store.ingest(&file).unwrap());
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }

    /// Large outputs/instructions stay out of reduced records while command times remain exact.
    #[test]
    fn thin_parser_discards_bulk_outputs() {
        let row = serde_json::json!({"type":"event_msg","timestamp_ms":4000,
            "payload":{"type":"item_completed","thread_id":"chat-a","turn_id":"turn-a",
                "started_at_ms":1000,"completed_at_ms":4000,
                "item":{"type":"CommandExecution","id":"call-a","command":["cargo","test"],
                    "aggregated_output":"x".repeat(1024 * 1024),"exit_code":0}}});
        let normalized = thin_record(&serde_json::to_vec(&row).unwrap())
            .unwrap()
            .unwrap();
        assert!(serde_json::to_vec(&normalized).unwrap().len() < 2000);
        assert_eq!(normalized["payload"]["started_at_ms"], 1000);
        assert_eq!(normalized["payload"]["item"]["command"], "cargo test");
        assert!(normalized["payload"]["item"]
            .get("aggregated_output")
            .is_none());
        assert!(thin_record(br#"{"type":"world_state","large":"ignored"}"#)
            .unwrap()
            .is_none());
    }
}
