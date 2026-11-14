use crate::coverage_migration;
use crate::model::{self, CostMember, CostSummary, Session, Snapshot};
use eframe::egui;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{value::RawValue, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use walkdir::WalkDir;

const MAX_SESSIONS: usize = 96;
const CACHE_VERSION: i64 = 8;
const DETAILS_CHARS: usize = 32 * 1024;
const CHUNK_BYTES: u64 = 8 * 1024 * 1024;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// One collector serves all chats; bounded channels prevent a hidden UI backlog.
pub fn start(home: PathBuf, db_path: PathBuf, ctx: egui::Context) -> mpsc::Receiver<Snapshot> {
    let (tx, rx) = mpsc::sync_channel(1);
    thread::spawn(move || {
        if let Err(error) = run(home, db_path, &ctx, &tx) {
            let _ = tx.try_send(Snapshot {
                sessions: Vec::new(),
                costs: Vec::new(),
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
    #[serde(
        deserialize_with = "observed_tier",
        skip_serializing_if = "Option::is_none"
    )]
    service_tier: Option<Option<String>>,
    trigger: Option<String>,
    settings: Option<ThinSettings>,
    thread_settings: Option<ThinSettings>,
    source: Option<Value>,
    agent_role: Option<String>,
    agent_path: Option<String>,
    agent_nickname: Option<String>,
    parent_thread_id: Option<String>,
    forked_from_id: Option<String>,
    attached_thread: Option<bool>,
    #[serde(deserialize_with = "detail_text")]
    input: Option<String>,
    #[serde(deserialize_with = "detail_text")]
    arguments: Option<String>,
    #[serde(deserialize_with = "bounded_input")]
    tool_input: Option<Value>,
    turn_id: Option<String>,
    thread_id: Option<String>,
    agent_id: Option<String>,
    response_id: Option<String>,
    call_id: Option<String>,
    name: Option<String>,
    namespace: Option<String>,
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

// Missing tier is unknown; an explicit JSON null is a recorded reset to standard mode.
fn observed_tier<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(deserializer).map(Some)
}

// Settings are sampled from each recorded transition, never from today's global config.
#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct ThinSettings {
    model: Option<String>,
    #[serde(
        deserialize_with = "observed_tier",
        skip_serializing_if = "Option::is_none"
    )]
    service_tier: Option<Option<String>>,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct ThinInfo {
    total_token_usage: Option<ThinUsage>,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct ThinUsage {
    input_tokens: Option<u64>,
    cached_input_tokens: Option<u64>,
    cache_write_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct ThinItem {
    #[serde(rename = "type")]
    kind_type: Option<String>,
    id: Option<String>,
    kind: Option<String>,
    agent_thread_id: Option<String>,
    agent_path: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<String>,
    status: Option<String>,
    sender_thread_id: Option<String>,
    receiver_thread_ids: Option<Vec<String>>,
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
    Ok(Some(command.chars().take(DETAILS_CHARS).collect()))
}

// Input is retained for selectable command details, with a fixed per-call memory bound.
fn detail_text<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error> {
    struct Text;
    impl<'de> serde::de::Visitor<'de> for Text {
        type Value = Option<String>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("optional command text")
        }
        fn visit_str<E: serde::de::Error>(
            self,
            value: &str,
        ) -> std::result::Result<Self::Value, E> {
            Ok(Some(value.chars().take(DETAILS_CHARS).collect()))
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

// Hook inputs already exclude tool results; shrink unexpectedly large argument objects as well.
fn bounded_input<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Value>, D::Error> {
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.map(|v| {
        let text = v.to_string();
        if text.len() <= DETAILS_CHARS {
            v
        } else {
            Value::String(text.chars().take(DETAILS_CHARS).collect())
        }
    }))
}

/// Parse only monitored fields. RawValue borrows the original line instead of cloning its payload.
fn thin_record(line: &[u8]) -> std::result::Result<Option<Value>, serde_json::Error> {
    let envelope: Envelope<'_> = serde_json::from_slice(line)?;
    if !matches!(
        envelope.kind.as_str(),
        "session_meta"
            | "compacted"
            | "turn_context"
            | "event_msg"
            | "response_item"
            | "token_usage_record"
            | "monitor_hook"
            | "hook_event"
    ) {
        return Ok(None);
    }
    let mut payload: ThinPayload = match envelope.payload {
        Some(raw_payload) => serde_json::from_str(raw_payload.get())?,
        None if envelope.kind == "compacted" => ThinPayload::default(),
        None => return Ok(None),
    };
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
    titles: HashMap<String, String>,
    costs: HashMap<String, CostMember>,
    catalog: HashMap<PathBuf, CatalogEntry>,
    source_offsets: HashMap<PathBuf, (u64, String)>,
    member_sources: HashMap<String, HashSet<PathBuf>>,
    failed_sources: HashSet<PathBuf>,
    attempted_sources: HashSet<PathBuf>,
}

/// Historical ownership and costs stay resident without retaining historical tool bodies.
#[derive(Default, Deserialize)]
#[serde(default)]
struct CachedCost {
    id: String,
    parent_id: String,
    thread_kind: String,
    title: String,
    agent_name: String,
    cost: Option<CostSummary>,
    cost_coverage_incomplete: Option<bool>,
}

impl CachedCost {
    fn member(self) -> CostMember {
        CostMember {
            id: self.id,
            parent_id: self.parent_id,
            thread_kind: self.thread_kind,
            title: self.title,
            agent_name: self.agent_name,
            partial: self.cost.is_none() || self.cost_coverage_incomplete != Some(false),
            cost: self.cost,
        }
    }
}

struct CatalogEntry {
    member: CostMember,
    length: u64,
    modified: SystemTime,
}

/// Another rollout may have sparse or conflicting metadata; known ownership remains authoritative.
fn supplement_cost_metadata(member: &mut CostMember, observed: &CostMember) {
    let repair_kind = member.parent_id.is_empty()
        && matches!(member.thread_kind.as_str(), "" | "conversation")
        && !observed.parent_id.is_empty()
        && matches!(observed.thread_kind.as_str(), "subagent" | "side_chat");
    if member.parent_id.is_empty()
        && (member.thread_kind.is_empty()
            || member.thread_kind == observed.thread_kind
            || repair_kind)
    {
        member.parent_id = observed.parent_id.clone();
    }
    if member.thread_kind.is_empty() || repair_kind {
        member.thread_kind = observed.thread_kind.clone();
    }
    if member.agent_name.is_empty() {
        member.agent_name = observed.agent_name.clone();
    }
}

/// Only the first metadata record establishes source ownership, including copied child histories.
fn catalog_entry(path: &Path) -> Result<Option<CatalogEntry>> {
    let file = File::open(path)?;
    let metadata = file.metadata()?;
    let mut first = String::new();
    BufReader::new(file.take(1024 * 1024)).read_line(&mut first)?;
    let Ok(row) = serde_json::from_str::<Value>(&first) else {
        return Ok(None);
    };
    if row["type"] != "session_meta" || row["payload"]["id"].as_str().is_none() {
        return Ok(None);
    }
    let mut session = Session::default();
    model::apply(&mut session, &row);
    let mut member = CostMember::from_session(&session);
    member.cost = None;
    member.partial = true;
    Ok(Some(CatalogEntry {
        member,
        length: metadata.len(),
        modified: metadata.modified()?,
    }))
}

fn load_cost_index(db: &Connection) -> Result<HashMap<String, CostMember>> {
    let mut statement = db.prepare("SELECT id,state FROM sessions")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut costs = HashMap::new();
    for row in rows {
        let (id, state) = row?;
        let mut member = serde_json::from_str::<CachedCost>(&state)?.member();
        member.id = id.clone();
        costs.insert(id, member);
    }
    Ok(costs)
}

fn load_source_offsets(db: &Connection) -> Result<HashMap<PathBuf, (u64, String)>> {
    let mut statement = db.prepare("SELECT path,offset,thread_id FROM sources")?;
    let rows = statement.query_map([], |row| {
        Ok((
            PathBuf::from(row.get::<_, String>(0)?),
            (row.get::<_, u64>(1)?, row.get::<_, String>(2)?),
        ))
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
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
        coverage_migration::ensure_schema(&db)?;
        if db.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))? != CACHE_VERSION
        {
            // These tables are a reproducible cache; Codex source logs are never changed.
            db.execute_batch(
                "BEGIN; DELETE FROM sources; DELETE FROM sessions; PRAGMA user_version=8; COMMIT;",
            )?;
        }
        let sessions = {
            let mut statement =
                db.prepare("SELECT id,state FROM sessions ORDER BY last_ms DESC LIMIT 96")?;
            let rows = statement
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            let mut sessions = HashMap::new();
            for row in rows {
                let (id, state) = row?;
                sessions.insert(id, serde_json::from_str(&state)?);
            }
            sessions
        };
        let costs = load_cost_index(&db)?;
        let source_offsets = load_source_offsets(&db)?;
        let mut member_sources: HashMap<String, HashSet<PathBuf>> = HashMap::new();
        for (path, (_, id)) in &source_offsets {
            if !id.is_empty() && is_rollout(path) {
                member_sources
                    .entry(id.clone())
                    .or_default()
                    .insert(path.clone());
            }
        }
        Ok(Self {
            db,
            sessions,
            error: None,
            titles: HashMap::new(),
            costs,
            catalog: HashMap::new(),
            source_offsets,
            member_sources,
            failed_sources: HashSet::new(),
            attempted_sources: HashSet::new(),
        })
    }

    // Codex's title index is authoritative; appended updates also cover renamed chats.
    fn load_titles(&mut self, path: &Path) -> Result<()> {
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        let length = file.metadata()?.len();
        let offset = length.saturating_sub(2 * 1024 * 1024);
        file.seek(SeekFrom::Start(offset))?;
        let mut reader = BufReader::new(file);
        let mut line = String::new();
        if offset > 0 {
            reader.read_line(&mut line)?;
        }
        self.titles.clear();
        loop {
            line.clear();
            if reader.read_line(&mut line)? == 0 {
                break;
            }
            if let Ok(row) = serde_json::from_str::<Value>(&line) {
                if let (Some(id), Some(title)) = (row["id"].as_str(), row["thread_name"].as_str()) {
                    self.titles
                        .insert(id.into(), title.chars().take(256).collect());
                }
            }
        }
        self.apply_titles()?;
        Ok(())
    }

    fn apply_titles(&mut self) -> Result<()> {
        for (id, member) in &mut self.costs {
            if let Some(title) = self.titles.get(id) {
                member.title = title.clone();
            }
        }
        let transaction = self.db.transaction()?;
        for (id, session) in &mut self.sessions {
            if let Some(title) = self.titles.get(id) {
                session.title = title.clone();
                transaction.execute(
                    "UPDATE sessions SET state=?2 WHERE id=?1",
                    params![id, serde_json::to_string(session)?],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    fn snapshot(&self) -> Snapshot {
        let mut sessions: Vec<_> = self.sessions.values().cloned().collect();
        sessions.sort_by_key(|s| std::cmp::Reverse(s.last_ms));
        sessions.truncate(MAX_SESSIONS);
        Snapshot {
            sessions,
            costs: self.cost_snapshot(),
            error: self.error.clone(),
            updated_ms: model::now_ms(),
        }
    }

    fn cost_snapshot(&self) -> Vec<CostMember> {
        let mut costs: Vec<_> = self
            .costs
            .values()
            .cloned()
            .map(|mut member| {
                member.partial |= !self.sources_complete(&member.id);
                member
            })
            .collect();
        costs.sort_by(|left, right| left.id.cmp(&right.id));
        costs
    }

    /// A cached row is not proof that every source reached EOF, including copied rollout files.
    fn sources_complete(&self, id: &str) -> bool {
        self.member_sources.get(id).is_some_and(|paths| {
            !paths.is_empty()
                && paths.iter().all(|path| {
                    !self.failed_sources.contains(path)
                        && self.catalog.get(path).is_some_and(|entry| {
                            self.source_offsets
                                .get(path)
                                .is_some_and(|(offset, owner)| {
                                    owner == id && *offset == entry.length
                                })
                        })
                })
        })
    }

    /// Repair old metadata without replaying or replacing committed billing state and cursors.
    fn register_source(&mut self, path: &Path, mut entry: CatalogEntry) -> Result<()> {
        let id = entry.member.id.clone();
        let previous = self.costs.get(&id);
        let mut member = previous.cloned().unwrap_or_else(|| entry.member.clone());
        supplement_cost_metadata(&mut member, &entry.member);
        let changed = previous.is_some_and(|old| {
            old.parent_id != member.parent_id
                || old.thread_kind != member.thread_kind
                || old.agent_name != member.agent_name
        });
        if changed {
            self.db.execute(
                "UPDATE sessions SET state=json_set(state,
                '$.parent_id',?2,'$.thread_kind',?3,'$.agent_name',?4) WHERE id=?1",
                params![id, member.parent_id, member.thread_kind, member.agent_name],
            )?;
        }
        if let Some(title) = self.titles.get(&id) {
            member.title = title.clone();
        }
        if let Some(session) = self.sessions.get_mut(&id) {
            session.parent_id = member.parent_id.clone();
            session.thread_kind = member.thread_kind.clone();
            session.agent_name = member.agent_name.clone();
            session.is_internal |= matches!(member.thread_kind.as_str(), "internal" | "subagent");
        }
        entry.member.parent_id = member.parent_id.clone();
        entry.member.thread_kind = member.thread_kind.clone();
        entry.member.agent_name = member.agent_name.clone();
        self.costs.insert(id.clone(), member);
        self.member_sources
            .entry(id)
            .or_default()
            .insert(path.to_owned());
        self.catalog.insert(path.to_owned(), entry);
        Ok(())
    }

    fn observe_source(&mut self, path: &Path) -> Result<()> {
        if !is_rollout(path) {
            return Ok(());
        }
        if let Some(entry) = self.catalog.get_mut(path) {
            let metadata = path.metadata()?;
            entry.length = metadata.len();
            entry.modified = metadata.modified()?;
        } else if let Some(entry) = catalog_entry(path)? {
            self.register_source(path, entry)?;
        }
        Ok(())
    }

    /// Full discovery occurs only at startup/overflow; ordinary updates inspect their own source.
    fn discover(&mut self, root: &Path) -> Result<Vec<PathBuf>> {
        self.catalog.clear();
        self.attempted_sources.clear();
        self.failed_sources.clear();
        if root.exists() {
            for entry in WalkDir::new(root) {
                let entry = entry?;
                if is_rollout(entry.path()) && entry.file_type().is_file() {
                    match catalog_entry(entry.path()) {
                        Ok(Some(source)) => self.register_source(entry.path(), source)?,
                        Ok(None) => (),
                        Err(error) => {
                            self.error = Some(format!("{}: {error}", entry.path().display()))
                        }
                    }
                }
            }
        }
        let mut recent = recent_catalog_paths(&self.catalog);
        let roots: Vec<_> = recent
            .iter()
            .filter_map(|path| self.catalog.get(path))
            .map(|entry| entry.member.id.clone())
            .collect();
        recent.extend(self.family_paths(roots));
        Ok(recent)
    }

    fn pending_family_paths(&self) -> Vec<PathBuf> {
        self.family_paths(self.sessions.keys().cloned())
    }

    /// Metadata owns the edges: references and ordinary forks cannot transfer another chat's costs.
    fn family_paths(&self, roots: impl IntoIterator<Item = String>) -> Vec<PathBuf> {
        let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
        for member in self.costs.values() {
            if matches!(member.thread_kind.as_str(), "subagent" | "side_chat") {
                children
                    .entry(&member.parent_id)
                    .or_default()
                    .push(&member.id);
            }
        }
        let mut queue: VecDeque<_> = roots.into_iter().collect();
        let mut seen = HashSet::new();
        let mut paths = HashSet::new();
        while let Some(id) = queue.pop_front() {
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(descendants) = children.get(id.as_str()) {
                queue.extend(descendants.iter().map(|id| (*id).to_owned()));
            }
            if let Some(sources) = self.member_sources.get(&id) {
                for path in sources {
                    let unfinished = self.catalog.get(path).is_some_and(|entry| {
                        !self
                            .source_offsets
                            .get(path)
                            .is_some_and(|(offset, owner)| owner == &id && *offset == entry.length)
                    });
                    if unfinished
                        && !self.failed_sources.contains(path)
                        && !self.attempted_sources.contains(path)
                    {
                        paths.insert(path.clone());
                    }
                }
            }
        }
        let mut paths: Vec<_> = paths.into_iter().collect();
        paths.sort();
        paths
    }

    fn ensure_session(&mut self, id: &str) -> Result<()> {
        if !self.sessions.contains_key(id) {
            let state: Option<String> = self
                .db
                .query_row("SELECT state FROM sessions WHERE id=?1", [id], |r| r.get(0))
                .optional()?;
            let is_new = state.is_none();
            let mut session = match state {
                Some(state) => serde_json::from_str(&state)?,
                None => Session {
                    id: id.to_owned(),
                    ..Session::default()
                },
            };
            if is_new {
                model::reset_cost_coverage(&mut session);
            }
            if is_new || session.cost_coverage_incomplete.is_none() {
                coverage_migration::restore(&self.db, &mut session, is_new)?;
            }
            if let Some(member) = self.costs.get(id) {
                session.parent_id = member.parent_id.clone();
                session.thread_kind = member.thread_kind.clone();
                session.agent_name = member.agent_name.clone();
                session.is_internal |=
                    matches!(session.thread_kind.as_str(), "internal" | "subagent");
            }
            self.sessions.insert(id.to_owned(), session);
        }
        Ok(())
    }

    /// Replaying or resuming a file never counts previously committed tokens twice.
    fn ingest(&mut self, path: &Path) -> Result<bool> {
        self.observe_source(path)?;
        self.attempted_sources.insert(path.to_owned());
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
        let mixed_hooks = matches!(row["type"].as_str(), Some("monitor_hook" | "hook_event"));
        // A child/fork transcript can include the parent's copied session metadata.
        // Only its first metadata record establishes this source file's identity.
        if row["type"] == "session_meta" {
            if let Some(id) = payload["id"].as_str() {
                if thread_id.is_empty() {
                    *thread_id = id.to_owned();
                } else if id != thread_id {
                    return Ok(());
                }
            }
        }
        let explicit_id = payload["thread_id"]
            .as_str()
            .or_else(|| row["thread_id"].as_str());
        let routed = if mixed_hooks {
            explicit_id.unwrap_or_default().to_owned()
        } else {
            // Copied parent events must not update either the parent or child session.
            if explicit_id.is_some_and(|id| id != thread_id) {
                return Ok(());
            }
            thread_id.clone()
        };
        if routed.is_empty() {
            self.error = Some(format!("{}: record has no chat identity", path.display()));
            return Ok(());
        }
        self.ensure_session(&routed)?;
        if let Some(session) = self.sessions.get_mut(&routed) {
            model::apply(session, &row);
            if row["type"] == "session_meta" {
                if let Some(member) = self.costs.get(&routed) {
                    session.parent_id = member.parent_id.clone();
                    session.thread_kind = member.thread_kind.clone();
                    session.agent_name = member.agent_name.clone();
                    session.is_internal |=
                        matches!(member.thread_kind.as_str(), "internal" | "subagent");
                }
            }
            if let Some(title) = self.titles.get(&routed) {
                session.title = title.clone();
            }
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
        self.source_offsets
            .insert(PathBuf::from(path), (offset, thread_id.to_owned()));
        self.failed_sources.remove(Path::new(path));
        for id in changed {
            self.costs
                .insert(id.clone(), CostMember::from_session(&self.sessions[id]));
        }
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

/// Choose recent full timelines while preserving ancestors; cost discovery has no such cap.
fn recent_catalog_paths(catalog: &HashMap<PathBuf, CatalogEntry>) -> Vec<PathBuf> {
    let mut files: Vec<_> = catalog
        .iter()
        .filter(|(_, entry)| entry.member.thread_kind != "internal")
        .map(|(path, entry)| {
            (
                entry.modified,
                path.clone(),
                entry.member.id.clone(),
                entry.member.parent_id.clone(),
            )
        })
        .collect();
    files.sort_by_key(|(time, _, _, _)| std::cmp::Reverse(*time));
    let mut chosen = HashSet::new();
    let mut pending: Vec<_> = files
        .iter()
        .take(MAX_SESSIONS)
        .map(|(_, _, id, _)| id.clone())
        .collect();
    while let Some(id) = pending.pop() {
        if chosen.insert(id.clone()) {
            if let Some((_, _, _, parent)) = files.iter().find(|(_, _, own, _)| own == &id) {
                if !parent.is_empty() && !chosen.contains(parent) {
                    pending.push(parent.clone());
                }
            }
        }
    }
    let mut selected: Vec<_> = files
        .into_iter()
        .filter(|(_, _, id, _)| chosen.contains(id))
        .collect();
    selected.sort_by_key(|(_, _, _, parent)| !parent.is_empty());
    selected
        .into_iter()
        .take(MAX_SESSIONS)
        .map(|(_, path, _, _)| path)
        .collect()
}

#[cfg(test)]
fn recent_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut catalog = HashMap::new();
    for entry in WalkDir::new(root) {
        let entry = entry?;
        if is_rollout(entry.path()) && entry.file_type().is_file() {
            if let Some(source) = catalog_entry(entry.path())? {
                catalog.insert(entry.into_path(), source);
            }
        }
    }
    Ok(recent_catalog_paths(&catalog))
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
    if root.is_dir() {
        watcher.watch(root, RecursiveMode::Recursive)?;
    }
    if let Some(home) = root.parent() {
        let existing = home.ancestors().find(|path| path.is_dir()).unwrap_or(home);
        watcher.watch(existing, RecursiveMode::NonRecursive)?;
    }
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
    match coverage_migration::migrate(&mut store.db, &hooks) {
        Ok(true) => {
            drop(store);
            store = Store::open(&db_path)?;
        }
        Ok(false) => (),
        Err(error) => store.error = Some(format!("历史费用覆盖回填未完成：{error}")),
    }
    let index = home.join("session_index.jsonl");
    store.load_titles(&index)?;
    let (event_tx, events) = mpsc::sync_channel(64);
    let overflow = Arc::new(AtomicBool::new(false));
    let mut watcher = watch_sources(&root, &hooks_dir, event_tx, overflow.clone())?;
    let mut queue = VecDeque::new();
    let mut queued = HashSet::new();
    for path in store
        .discover(&root)?
        .into_iter()
        .chain(std::iter::once(hooks.clone()))
    {
        enqueue(path, &mut queue, &mut queued);
    }
    let mut sessions_watched = root.is_dir();
    let mut last_publish = Instant::now();
    let mut dirty = true;
    loop {
        if !sessions_watched && root.is_dir() {
            watcher.watch(&root, RecursiveMode::Recursive)?;
            sessions_watched = true;
            for path in store.discover(&root)? {
                enqueue(path, &mut queue, &mut queued);
            }
        }
        if let Some(path) = queue.pop_front() {
            queued.remove(&path);
            process_queued(&mut store, path, &index, &mut queue, &mut queued);
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
            if let Ok(change) = &event {
                if change.paths.iter().any(|path| path == &hooks_dir) && hooks_dir.exists() {
                    if let Err(error) = watcher.watch(&hooks_dir, RecursiveMode::NonRecursive) {
                        store.error = Some(format!("Hook watcher: {error}"));
                    }
                    enqueue(hooks.clone(), &mut queue, &mut queued);
                }
            }
            dirty |= handle_event(event, &hooks, &mut queue, &mut queued, &mut store);
        }
        if overflow.swap(false, Ordering::Relaxed) {
            store.error = Some("File event queue overflow: rediscovered recent sources".into());
            for path in store
                .discover(&root)?
                .into_iter()
                .chain(std::iter::once(hooks.clone()))
            {
                enqueue(path, &mut queue, &mut queued);
            }
            dirty = true;
        }
    }
}

/// Background history catch-up uses the same transactional cursor path as live file updates.
fn process_queued(
    store: &mut Store,
    path: PathBuf,
    index: &Path,
    queue: &mut VecDeque<PathBuf>,
    queued: &mut HashSet<PathBuf>,
) {
    let result = if path == index {
        store.load_titles(&path).map(|_| false)
    } else {
        store.ingest(&path)
    };
    match result {
        Ok(true) => enqueue(path, queue, queued),
        Ok(false) => (),
        Err(error) => {
            store.error = Some(format!("{}: {error}", path.display()));
            store.failed_sources.insert(path);
        }
    }
    for path in store.pending_family_paths() {
        enqueue(path, queue, queued);
    }
}

fn handle_event(
    event: notify::Result<notify::Event>,
    hooks: &Path,
    queue: &mut VecDeque<PathBuf>,
    queued: &mut HashSet<PathBuf>,
    store: &mut Store,
) -> bool {
    let mut changed = false;
    match event {
        Ok(event) if !event.kind.is_access() => {
            for path in event.paths {
                if is_rollout(&path)
                    || path == hooks
                    || path
                        .file_name()
                        .is_some_and(|name| name == "session_index.jsonl")
                {
                    store.failed_sources.remove(&path);
                    store.attempted_sources.remove(&path);
                    if !path.is_file() {
                        store.catalog.remove(&path);
                    }
                    enqueue(path, queue, queued);
                    changed = true;
                }
            }
        }
        Ok(_) => (),
        Err(error) => {
            store.error = Some(format!("File watcher: {error}"));
            changed = true;
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // A migrated agent hook replay must keep its agent identity and not create a duplicate request.
    #[test]
    fn migrated_agent_hook_replay_keeps_one_coverage_request() {
        let dir = fixture("agent-coverage-replay");
        let path = dir.join("hooks.jsonl");
        let db = dir.join("state.sqlite");
        let row = |name: &str, time: i64| {
            serde_json::json!({"type":"monitor_hook","timestamp_ms":time,"payload":{
                "thread_id":"child","turn_id":"turn","agent_id":"child-agent",
                "hook_event_name":name,"status":"completed"}})
            .to_string()
                + "\n"
        };
        fs::write(
            &path,
            row("PreModelRequest", 1000) + &row("PostModelRequest", 2000),
        )
        .unwrap();
        let mut store = Store::open(&db).unwrap();
        assert!(coverage_migration::migrate(&mut store.db, &path).unwrap());
        store.ingest(&path).unwrap();
        let child = &store.sessions["child"];
        assert_eq!(child.cost_coverage.len(), 1);
        assert_eq!(child.cost_coverage_incomplete, Some(true));
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }

    fn fixture(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "codex-monitor-{name}-{}-{}",
            std::process::id(),
            model::now_ms()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn write_cost_rollout(
        dir: &Path,
        file: &str,
        id: &str,
        parent: &str,
        stamp: u64,
        output: u64,
    ) -> PathBuf {
        let path = dir.join(format!("rollout-{file}.jsonl"));
        let source = if parent.is_empty() {
            serde_json::json!("vscode")
        } else {
            serde_json::json!({"subagent":{"parent_thread_id":parent}})
        };
        let records = [
            serde_json::json!({"type":"session_meta","timestamp_ms":stamp,"payload":{"id":id,"source":source}}),
            serde_json::json!({"type":"token_usage_record","timestamp_ms":stamp+1,"payload":{
                "thread_id":id,"response_id":file,"model":"gpt-6.1-sol","turn_id":"turn",
                "usage":{"input_tokens":10,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":output}}}),
        ];
        fs::write(
            &path,
            records
                .into_iter()
                .map(|record| record.to_string() + "\n")
                .collect::<String>(),
        )
        .unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(stamp))
            .unwrap();
        path
    }

    // Historical child fees survive timeline eviction and restart; discovery catches uncached old children.
    #[test]
    fn historical_costs_outlive_the_full_session_limit() {
        let dir = fixture("cost-history");
        let child = write_cost_rollout(&dir, "child", "child", "parent", 100, 7);
        let parent = write_cost_rollout(&dir, "parent", "parent", "", 10000, 3);
        for index in 0..100 {
            let id = format!("other-{index}");
            write_cost_rollout(&dir, &id, &id, "", 1000 + index, 1);
        }
        let db = dir.join("state.sqlite");
        let mut store = Store::open(&db).unwrap();
        let paths = store.discover(&dir).unwrap();
        assert!(paths.contains(&child));
        assert!(paths.contains(&parent));
        assert!(store.costs["child"].cost.is_none());
        assert!(
            store
                .cost_snapshot()
                .iter()
                .find(|member| member.id == "child")
                .unwrap()
                .partial
        );
        for path in paths.into_iter().collect::<HashSet<_>>() {
            store.ingest(&path).unwrap();
        }
        assert!(store.sessions.len() <= MAX_SESSIONS);
        assert!(!store.sessions.contains_key("child"));
        assert_eq!(store.costs["child"].cost.as_ref().unwrap().output_tokens, 7);
        assert!(store.sources_complete("child"));
        let offset = store.source_offsets[&child].0;
        drop(store);
        let mut store = Store::open(&db).unwrap();
        assert!(!store.sessions.contains_key("child"));
        store.discover(&dir).unwrap();
        assert_eq!(store.costs["child"].cost.as_ref().unwrap().output_tokens, 7);
        assert_eq!(store.source_offsets[&child].0, offset);
        assert!(store.sources_complete("child"));
        assert!(!store.ingest(&child).unwrap());
        assert_eq!(store.costs["child"].cost.as_ref().unwrap().output_tokens, 7);
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }

    // Every known file must reach EOF; absent sources and failed reads preserve a partial total.
    #[test]
    fn cost_completeness_covers_all_sources() {
        let dir = fixture("cost-sources");
        let first = write_cost_rollout(&dir, "first", "child", "parent", 100, 7);
        let second = write_cost_rollout(&dir, "second", "child", "parent", 200, 11);
        let mut store = Store::open(&dir.join("state.sqlite")).unwrap();
        store.discover(&dir).unwrap();
        store.ingest(&first).unwrap();
        assert!(!store.sources_complete("child"));
        store.ingest(&second).unwrap();
        assert!(store.sources_complete("child"));
        assert_eq!(
            store.costs["child"].cost.as_ref().unwrap().output_tokens,
            18
        );
        store.failed_sources.insert(second.clone());
        assert!(!store.sources_complete("child"));
        store.failed_sources.clear();
        fs::remove_file(&second).unwrap();
        store.discover(&dir).unwrap();
        assert!(!store.sources_complete("child"));
        assert!(
            store
                .cost_snapshot()
                .iter()
                .find(|member| member.id == "child")
                .unwrap()
                .partial
        );
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }

    // Repairing v8 ownership keeps fees, timestamps, cursor positions and unknown coverage intact.
    #[test]
    fn first_metadata_repairs_legacy_cache_without_replay() {
        let dir = fixture("cost-metadata-repair");
        let path = write_cost_rollout(&dir, "child", "child", "parent", 100, 7);
        let db = dir.join("state.sqlite");
        let mut store = Store::open(&db).unwrap();
        store.ingest(&path).unwrap();
        let offset = store.source_offsets[&path].0;
        store.db.execute("UPDATE sessions SET state=json_remove(json_set(state,
            '$.parent_id','','$.thread_kind','conversation'),'$.cost_coverage_incomplete') WHERE id='child'", []).unwrap();
        drop(store);
        let mut store = Store::open(&db).unwrap();
        assert!(store.costs["child"].partial);
        store.discover(&dir).unwrap();
        assert_eq!(store.costs["child"].parent_id, "parent");
        assert_eq!(store.sessions["child"].thread_kind, "subagent");
        assert_eq!(store.costs["child"].cost.as_ref().unwrap().output_tokens, 7);
        assert_eq!(store.source_offsets[&path].0, offset);
        assert_eq!(store.sessions["child"].last_ms, 101);
        drop(store);
        let store = Store::open(&db).unwrap();
        assert_eq!(store.costs["child"].parent_id, "parent");
        assert_eq!(store.source_offsets[&path].0, offset);
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }

    // Conflicting and sparse same-thread files cannot replace established ownership or agent names.
    #[test]
    fn multiple_sources_only_supplement_missing_metadata() {
        let dir = fixture("cost-metadata-conflict");
        let first = write_cost_rollout(&dir, "first", "child", "parent", 100, 7);
        let mut rows: Vec<Value> = fs::read_to_string(&first)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        rows[0]["payload"]["agent_nickname"] = serde_json::json!("Original");
        fs::write(
            &first,
            rows.into_iter()
                .map(|row| row.to_string() + "\n")
                .collect::<String>(),
        )
        .unwrap();
        let db = dir.join("state.sqlite");
        let mut store = Store::open(&db).unwrap();
        store.ingest(&first).unwrap();
        for (file, payload, stamp) in [
            (
                "conflicting",
                serde_json::json!({"id":"child","source":"vscode",
                "attached_thread":true,"forked_from_id":"foreign","agent_nickname":"Other"}),
                200,
            ),
            (
                "empty",
                serde_json::json!({"id":"child","source":"vscode"}),
                300,
            ),
        ] {
            let path = dir.join(format!("rollout-{file}.jsonl"));
            fs::write(
                &path,
                format!(
                    "{}\n",
                    serde_json::json!({
                "type":"session_meta","timestamp_ms":stamp,"payload":payload})
                ),
            )
            .unwrap();
            store.ingest(&path).unwrap();
            assert_eq!(store.costs["child"].parent_id, "parent");
            assert_eq!(store.costs["child"].thread_kind, "subagent");
            assert_eq!(store.costs["child"].agent_name, "Original");
            assert_eq!(store.sessions["child"].parent_id, "parent");
        }
        assert_eq!(store.costs["child"].cost.as_ref().unwrap().output_tokens, 7);
        drop(store);
        let mut store = Store::open(&db).unwrap();
        store.discover(&dir).unwrap();
        assert_eq!(store.costs["child"].parent_id, "parent");
        assert_eq!(store.costs["child"].thread_kind, "subagent");
        assert_eq!(store.costs["child"].agent_name, "Original");
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }

    // A partial JSONL tail waits for another file notification instead of creating a busy retry loop.
    #[test]
    fn history_queue_does_not_spin_on_partial_tails() {
        let dir = fixture("cost-tail");
        let path = write_cost_rollout(&dir, "child", "child", "parent", 100, 7);
        File::options()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"type\":")
            .unwrap();
        let mut store = Store::open(&dir.join("state.sqlite")).unwrap();
        store.discover(&dir).unwrap();
        assert!(!store.ingest(&path).unwrap());
        assert!(!store.sources_complete("child"));
        assert!(!store.pending_family_paths().contains(&path));
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }

    // Failed SQLite commits cannot publish uncommitted fees or advance the persisted source cursor.
    #[test]
    fn rejected_commit_keeps_cost_index_and_cursor_unchanged() {
        let dir = fixture("cost-transaction");
        let path = write_cost_rollout(&dir, "child", "child", "parent", 100, 7);
        let mut store = Store::open(&dir.join("state.sqlite")).unwrap();
        store.ingest(&path).unwrap();
        let offset = store.source_offsets[&path].0;
        let row = serde_json::json!({"type":"token_usage_record","timestamp_ms":200,
            "payload":{"thread_id":"child","response_id":"next","model":"gpt-6.1-sol",
                "usage":{"input_tokens":10,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":99}}});
        File::options()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all((row.to_string() + "\n").as_bytes())
            .unwrap();
        store.db.execute_batch("CREATE TRIGGER reject_cost BEFORE INSERT ON sessions BEGIN SELECT RAISE(ABORT,'test rejection'); END;").unwrap();
        assert!(store.ingest(&path).is_err());
        assert_eq!(store.costs["child"].cost.as_ref().unwrap().output_tokens, 7);
        assert_eq!(store.sessions["child"].cost.output_tokens, 7);
        assert_eq!(store.source_offsets[&path].0, offset);
        assert!(!store.sources_complete("child"));
        store.db.execute_batch("DROP TRIGGER reject_cost;").unwrap();
        store.ingest(&path).unwrap();
        assert_eq!(
            store.costs["child"].cost.as_ref().unwrap().output_tokens,
            106
        );
        drop(store);
        fs::remove_dir_all(dir).unwrap();
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

    /// Official chat names replace old directory fallbacks and survive later prompt observations.
    #[test]
    fn official_titles_override_prompt_and_directory() {
        let dir = fixture("titles");
        let file = dir.join("rollout-chat.jsonl");
        let index = dir.join("session_index.jsonl");
        fs::write(&file, concat!(
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"chat\",\"cwd\":\"D:/repo\"}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"item_completed\",\"item\":{\"type\":\"UserMessage\",\"content\":[{\"text\":\"A prompt\"}]}}}\n"
        )).unwrap();
        fs::write(
            &index,
            "{\"id\":\"chat\",\"thread_name\":\"Real conversation\"}\n",
        )
        .unwrap();
        let mut store = Store::open(&dir.join("state.sqlite")).unwrap();
        store.load_titles(&index).unwrap();
        store.ingest(&file).unwrap();
        assert_eq!(store.sessions["chat"].title, "Real conversation");
        fs::write(
            &index,
            "{\"id\":\"chat\",\"thread_name\":\"Renamed conversation\"}\n",
        )
        .unwrap();
        store.load_titles(&index).unwrap();
        assert_eq!(store.sessions["chat"].title, "Renamed conversation");
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }

    /// Details beyond the old 160-character summary are retained, without retaining result text.
    #[test]
    fn detailed_commands_and_source_metadata_are_preserved() {
        let script = format!("prefix-{}-suffix", "x".repeat(2000));
        let call = serde_json::json!({"type":"response_item","payload":{
            "type":"custom_tool_call","name":"exec","call_id":"call-a","input":script}});
        let call = thin_record(&serde_json::to_vec(&call).unwrap())
            .unwrap()
            .unwrap();
        assert!(call["payload"]["input"]
            .as_str()
            .unwrap()
            .ends_with("-suffix"));
        let item = serde_json::json!({"type":"event_msg","payload":{"type":"item_completed",
            "item":{"type":"CommandExecution","command":["powershell",script],"aggregated_output":"omit"}}});
        let item = thin_record(&serde_json::to_vec(&item).unwrap())
            .unwrap()
            .unwrap();
        assert!(item["payload"]["item"]["command"]
            .as_str()
            .unwrap()
            .ends_with("-suffix"));
        let meta = br#"{"type":"session_meta","payload":{"id":"child","source":{"subagent":{"parent_thread_id":"parent"}}}}"#;
        assert_eq!(
            thin_record(meta).unwrap().unwrap()["payload"]["source"]["subagent"]
                ["parent_thread_id"],
            "parent"
        );
    }

    /// Old reduced caches are invalidated atomically so newly supported records can be replayed.
    #[test]
    fn cache_version_replays_original_sources() {
        let dir = fixture("migration");
        let db = dir.join("state.sqlite");
        let file = dir.join("rollout-test.jsonl");
        fs::write(
            &file,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"chat\"}}\n",
        )
        .unwrap();
        let mut store = Store::open(&db).unwrap();
        store.ingest(&file).unwrap();
        store.db.pragma_update(None, "user_version", 1).unwrap();
        drop(store);
        let mut store = Store::open(&db).unwrap();
        assert!(store.sessions.is_empty());
        let count: i64 = store
            .db
            .query_row("SELECT count(*) FROM sources", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        store.ingest(&file).unwrap();
        assert!(store.sessions.contains_key("chat"));
        assert!(file.exists());
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }

    /// Copied parent metadata/events inside a child file cannot change its bound chat identity.
    #[test]
    fn child_transcript_keeps_first_metadata_identity() {
        let dir = fixture("child-identity");
        let root_file = dir.join("rollout-root.jsonl");
        let child_file = dir.join("rollout-child.jsonl");
        let rows = |values: Vec<Value>| {
            values
                .into_iter()
                .map(|row| row.to_string() + "\n")
                .collect::<String>()
        };
        let record = |time: i64, kind: &str, payload: Value| serde_json::json!({"type":kind,"timestamp_ms":time,"payload":payload});
        fs::write(&root_file, rows(vec![
            record(100,"session_meta",serde_json::json!({"id":"root","source":"vscode"})),
            record(200,"event_msg",serde_json::json!({"type":"task_started","turn_id":"root-turn","thread_id":"root"})),
            record(300,"token_usage_record",serde_json::json!({"thread_id":"root","turn_id":"root-turn","response_id":"root-response","usage":{"output_tokens":50},"turn_token_usage":{"output_tokens":50}})),
        ])).unwrap();
        fs::write(&child_file, rows(vec![
            record(400,"session_meta",serde_json::json!({"id":"child","source":{"subagent":{"parent_thread_id":"root"}}})),
            record(401,"session_meta",serde_json::json!({"id":"root","source":"vscode"})),
            record(402,"event_msg",serde_json::json!({"type":"task_started","turn_id":"copied-parent-turn","thread_id":"root"})),
            record(403,"token_usage_record",serde_json::json!({"thread_id":"root","turn_id":"copied-parent-turn","response_id":"copied-response","usage":{"output_tokens":999},"turn_token_usage":{"output_tokens":999}})),
            record(500,"event_msg",serde_json::json!({"type":"task_started","turn_id":"child-turn","thread_id":"child"})),
            record(600,"token_usage_record",serde_json::json!({"thread_id":"child","turn_id":"child-turn","response_id":"child-response","usage":{"output_tokens":10},"turn_token_usage":{"output_tokens":10}})),
        ])).unwrap();
        let mut store = Store::open(&dir.join("state.sqlite")).unwrap();
        store.ingest(&root_file).unwrap();
        store.ingest(&child_file).unwrap();
        assert_eq!(store.sessions["root"].turn_id, "root-turn");
        assert_eq!(store.sessions["root"].output_tokens, 50);
        assert_eq!(store.sessions["root"].requests.len(), 1);
        assert!(!store.sessions["root"].is_internal);
        assert_eq!(store.sessions["child"].turn_id, "child-turn");
        assert_eq!(store.sessions["child"].output_tokens, 10);
        assert_eq!(store.sessions["child"].requests.len(), 1);
        assert!(store.sessions["child"].is_internal);
        assert_eq!(store.sessions["child"].parent_id, "root");
        let bound: String = store
            .db
            .query_row(
                "SELECT thread_id FROM sources WHERE path=?1",
                [child_file.to_string_lossy().as_ref()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bound, "child");
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }

    /// Cost accounting retains every reported usage count while missing counts remain unknown.
    #[test]
    fn full_usage_counts_survive_thin_parser() {
        let counts = serde_json::json!({"input_tokens":1000,"cached_input_tokens":700,
            "cache_write_input_tokens":100,"output_tokens":50});
        let row = serde_json::json!({"type":"token_usage_record","payload":{
            "thread_id":"chat","turn_id":"turn","response_id":"response",
            "usage":counts,"turn_token_usage":counts,"thread_token_usage":counts}});
        let reduced = thin_record(&serde_json::to_vec(&row).unwrap())
            .unwrap()
            .unwrap();
        for field in ["usage", "turn_token_usage", "thread_token_usage"] {
            assert_eq!(reduced["payload"][field], counts);
        }
        let hook = serde_json::json!({"type":"monitor_hook","payload":{
            "thread_id":"chat","hook_event_name":"PostModelRequest","token_usage":counts}});
        let hook = thin_record(&serde_json::to_vec(&hook).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(hook["payload"]["token_usage"], counts);
        let partial = serde_json::json!({"type":"token_usage_record","payload":{
            "usage":{"output_tokens":50}}});
        let partial = thin_record(&serde_json::to_vec(&partial).unwrap())
            .unwrap()
            .unwrap();
        assert!(partial["payload"]["usage"]["input_tokens"].is_null());
        assert_eq!(partial["payload"]["usage"]["output_tokens"], 50);
    }

    /// Mid-turn speed changes retain their recorded tier rather than using current global settings.
    #[test]
    fn recorded_speed_settings_and_compaction_trigger_survive() {
        let row = serde_json::json!({"type":"event_msg","timestamp_ms":2000,
            "payload":{"type":"thread_settings_applied","turn_id":"turn-a",
                "service_tier":"fast","settings":{"model":"gpt-6.1-sol","service_tier":"fast",
                "instructions":"not copied"}}});
        let reduced = thin_record(&serde_json::to_vec(&row).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(reduced["payload"]["service_tier"], "fast");
        assert_eq!(reduced["payload"]["settings"]["service_tier"], "fast");
        assert_eq!(reduced["payload"]["settings"]["model"], "gpt-6.1-sol");
        assert!(reduced["payload"]["settings"].get("instructions").is_none());
        let hook = serde_json::json!({"type":"monitor_hook","timestamp_ms":2100,
            "payload":{"thread_id":"chat-a","turn_id":"turn-a","service_tier":"fast",
                "hook_event_name":"PreCompact","trigger":"auto"}});
        let hook = thin_record(&serde_json::to_vec(&hook).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(hook["payload"]["trigger"], "auto");
        assert_eq!(hook["payload"]["service_tier"], "fast");
    }

    /// Compaction markers remain observable while replacement history and summary bodies are skipped.
    #[test]
    fn compaction_marker_discards_summary_body() {
        let row = serde_json::json!({"type":"compacted","timestamp_ms":3000,
            "payload":{"turn_id":"turn-a","message":"x".repeat(1024*1024),
                "replacement_history":[{"content":"not copied"}],"trigger":"manual"}});
        let reduced = thin_record(&serde_json::to_vec(&row).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(reduced["type"], "compacted");
        assert_eq!(reduced["timestamp_ms"], 3000);
        assert_eq!(reduced["payload"]["trigger"], "manual");
        assert_eq!(reduced["payload"]["turn_id"], "turn-a");
        assert!(reduced["payload"].get("message").is_none());
        assert!(reduced["payload"].get("replacement_history").is_none());
        assert!(serde_json::to_vec(&reduced).unwrap().len() < 2048);
        let marker = thin_record(br#"{"type":"compacted","timestamp_ms":3100}"#)
            .unwrap()
            .unwrap();
        assert_eq!(marker["type"], "compacted");
    }

    /// Real desktop settings use thread_settings; missing tier differs from explicit null reset.
    #[test]
    fn actual_thread_settings_preserve_tier_presence() {
        let record = |payload: Value| {
            thin_record(
                &serde_json::to_vec(&serde_json::json!({"type":"event_msg",
                "timestamp_ms":2000,"payload":payload}))
                .unwrap(),
            )
            .unwrap()
            .unwrap()
        };
        let fast = record(
            serde_json::json!({"type":"thread_settings_applied","thread_id":"chat-a",
            "thread_settings":{"model":"gpt-6.1-sol","model_provider_id":"openai","service_tier":"priority"}}),
        );
        assert_eq!(
            fast["payload"]["thread_settings"]["service_tier"],
            "priority"
        );
        assert_eq!(fast["payload"]["thread_settings"]["model"], "gpt-6.1-sol");
        assert!(fast["payload"].get("service_tier").is_none());
        let off = record(serde_json::json!({"type":"thread_settings_applied",
            "thread_settings":{"service_tier":null}}));
        assert_eq!(
            off["payload"]["thread_settings"].get("service_tier"),
            Some(&Value::Null)
        );
        let missing = record(serde_json::json!({"type":"thread_settings_applied",
            "thread_settings":{"model":"gpt-6.1-sol"}}));
        assert!(missing["payload"]["thread_settings"]
            .get("service_tier")
            .is_none());
        let context = thin_record(br#"{"type":"turn_context","payload":{"model":"gpt-6.1-sol"}}"#)
            .unwrap()
            .unwrap();
        assert!(context["payload"].get("service_tier").is_none());
        let explicit = thin_record(br#"{"type":"monitor_hook","payload":{"service_tier":null}}"#)
            .unwrap()
            .unwrap();
        assert_eq!(explicit["payload"].get("service_tier"), Some(&Value::Null));
    }
    // First-run discovery retains useful chat relations instead of filling the list with guardians.
    #[test]
    fn discovery_retains_attached_parent_and_filters_system_tasks() {
        let dir = fixture("relationships");
        for (name, payload) in [
            (
                "parent",
                serde_json::json!({"id":"parent","source":"vscode"}),
            ),
            (
                "side",
                serde_json::json!({"id":"side","source":"vscode","attached_thread":true,"forked_from_id":"parent"}),
            ),
            (
                "guardian",
                serde_json::json!({"id":"guardian","source":{"subagent":{"other":"guardian"}}}),
            ),
        ] {
            fs::write(
                dir.join(format!("rollout-{name}.jsonl")),
                format!(
                    "{}\n",
                    serde_json::json!({"type":"session_meta","payload":payload})
                ),
            )
            .unwrap();
        }
        let files = recent_files(&dir).unwrap();
        assert!(files.contains(&dir.join("rollout-parent.jsonl")));
        assert!(files.contains(&dir.join("rollout-side.jsonl")));
        assert!(!files.contains(&dir.join("rollout-guardian.jsonl")));
        fs::remove_dir_all(dir).unwrap();
    }

    /// Parent timelines retain subagent/collaboration markers without copying agent prompts or outputs.
    #[test]
    fn subagent_activity_and_collaboration_keep_only_required_metadata() {
        let call = serde_json::json!({"type":"response_item","timestamp_ms":1000,
            "payload":{"type":"function_call","name":"wait","namespace":"collaboration",
                "call_id":"call-a","arguments":"{}"}});
        let call = thin_record(&serde_json::to_vec(&call).unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(call["payload"]["namespace"], "collaboration");
        let normalize = |event: &str, item: Value| {
            let row = serde_json::json!({"type":"event_msg","timestamp_ms":4000,
                "payload":{"type":event,"thread_id":"parent","turn_id":"turn-a",
                    "started_at_ms":1000,"completed_at_ms":4000,"item":item}});
            thin_record(&serde_json::to_vec(&row).unwrap())
                .unwrap()
                .unwrap()
        };
        for event in ["item_started", "item_completed"] {
            for kind in ["started", "interacted", "completed"] {
                let row = normalize(
                    event,
                    serde_json::json!({"type":"SubAgentActivity",
                    "id":"activity-a","kind":kind,"agent_thread_id":"child",
                    "agent_path":"/root/device_region","model":"gpt-6.1-sol",
                    "reasoning_effort":"high","prompt":"x".repeat(1024*1024),"output":"discard"}),
                );
                let item = &row["payload"]["item"];
                assert_eq!(row["payload"]["type"], event);
                assert_eq!(item["id"], "activity-a");
                assert_eq!(item["kind"], kind);
                assert_eq!(item["agent_thread_id"], "child");
                assert_eq!(item["agent_path"], "/root/device_region");
                assert_eq!(item["model"], "gpt-6.1-sol");
                assert_eq!(item["reasoning_effort"], "high");
                assert!(item.get("prompt").is_none());
                assert!(item.get("output").is_none());
                assert!(serde_json::to_vec(&row).unwrap().len() < 2048);
            }
        }
        let row = normalize(
            "item_completed",
            serde_json::json!({"type":"CollabAgentToolCall",
            "id":"collab-a","tool":"wait","status":"completed","sender_thread_id":"parent",
            "receiver_thread_ids":["child-a","child-b"],
            "receiver_agents":[{"prompt":"x".repeat(1024*1024)}],
            "agents_states":{"child-a":{"output":"large output"}},"output":"discard"}),
        );
        let item = &row["payload"]["item"];
        assert_eq!(item["id"], "collab-a");
        assert_eq!(item["tool"], "wait");
        assert_eq!(item["status"], "completed");
        assert_eq!(item["sender_thread_id"], "parent");
        assert_eq!(
            item["receiver_thread_ids"],
            serde_json::json!(["child-a", "child-b"])
        );
        assert!(item.get("receiver_agents").is_none());
        assert!(item.get("agents_states").is_none());
        assert!(item.get("output").is_none());
        assert!(serde_json::to_vec(&row).unwrap().len() < 2048);
    }
}
