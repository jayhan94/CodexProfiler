use crate::billing::{Decision, ReceiptKey};
use crate::model::{self, CostMember, CostSummary, Session, Snapshot, SourceNotice};
use crate::source_state::{IssueKind, SourceIssue};
use crate::{billing, coverage_migration, effort_backfill, source_state};
use eframe::egui;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{value::RawValue, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::error::Error;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Error as IoError, ErrorKind, Read, Seek, SeekFrom};
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
                source_notices: Vec::new(),
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
        deserialize_with = "observed_string",
        skip_serializing_if = "Option::is_none"
    )]
    service_tier: Option<Option<String>>,
    #[serde(
        deserialize_with = "observed_string",
        skip_serializing_if = "Option::is_none"
    )]
    effort: Option<Option<String>>,
    #[serde(
        deserialize_with = "observed_string",
        skip_serializing_if = "Option::is_none"
    )]
    reasoning_effort: Option<Option<String>>,
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

// Preserve a recorded null separately from a field that was never observed.
fn observed_string<'de, D: serde::Deserializer<'de>>(
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
        deserialize_with = "observed_string",
        skip_serializing_if = "Option::is_none"
    )]
    reasoning_effort: Option<Option<String>>,
    #[serde(
        deserialize_with = "observed_string",
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
    source_issues: HashMap<PathBuf, SourceIssue>,
    billing_partial: HashSet<String>,
    hooks_path: Option<PathBuf>,
}

/// Reduced state and receipt claims share one commit with the source cursor.
#[derive(Default)]
struct IngestBatch {
    changed: HashSet<String>,
    receipts: HashSet<ReceiptKey>,
    issue: Option<SourceIssue>,
    expected: HashMap<String, Option<String>>,
    deferred: Option<u64>,
}

struct SourceProgress {
    committed: u64,
    scanned: u64,
}

impl IngestBatch {
    /// Keep a retryable financial prefix while recording how far reliable live work was scanned.
    fn progress(&mut self, next: u64, metadata: &fs::Metadata) -> SourceProgress {
        if let Some(deferred) = self.deferred {
            let old = self.issue.take();
            let mut issue = SourceIssue::new(
                old.as_ref()
                    .map_or(IssueKind::UnverifiedBilling, |issue| issue.kind.clone()),
                old.as_ref().map_or(deferred, |issue| issue.offset),
                old.as_ref().map_or_else(
                    || "历史费用账本尚未核对".into(),
                    |issue| issue.message.clone(),
                ),
                metadata,
            );
            issue.history_gap |= old.as_ref().is_some_and(|issue| issue.history_gap);
            issue.deferred_offset = Some(deferred);
            issue.tail_offset = Some(next);
            self.issue = Some(issue);
        } else if let Some(issue) = self.issue.as_mut() {
            issue.deferred_offset = None;
            issue.tail_offset = None;
        }
        SourceProgress {
            committed: self.deferred.map_or(next, |deferred| next.min(deferred)),
            scanned: next,
        }
    }
}

type SourceCheckpoint = Option<(u64, String, Option<String>)>;

struct SourceRead {
    offset: u64,
    thread_id: String,
    expected: SourceCheckpoint,
    issue: Option<SourceIssue>,
    deferred: Option<u64>,
    initial_stamp: source_state::SourceStamp,
    needs_stamp: bool,
    verify_owner: bool,
}

impl SourceRead {
    /// Unverified replacement content cannot become the original source's committed audit prefix.
    fn progress(
        &self,
        batch: &mut IngestBatch,
        next: u64,
        metadata: &fs::Metadata,
    ) -> SourceProgress {
        if self.verify_owner {
            return SourceProgress {
                committed: self.expected.as_ref().map_or(0, |checkpoint| checkpoint.0),
                scanned: self.offset,
            };
        }
        batch.progress(next, metadata)
    }
}

/// Reuse the durable cursor, owner and issue as the source transaction's comparison token.
fn source_checkpoint(db: &Connection, path: &Path) -> Result<SourceCheckpoint> {
    Ok(db
        .query_row(
            "SELECT offset,thread_id,issue FROM sources WHERE path=?1",
            [path.to_string_lossy().as_ref()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?)
}

/// Missing history is recoverable only when its durable owner is already known.
fn save_missing_source(db: &Connection, path: &Path) -> Result<Option<SourceIssue>> {
    if !is_rollout(path) {
        return Ok(None);
    }
    let Some((_, owner, stored)) = source_checkpoint(db, path)? else {
        return Ok(None);
    };
    if owner.is_empty() {
        return Ok(None);
    }
    let previous: Option<SourceIssue> = stored
        .map(|value| serde_json::from_str(&value))
        .transpose()?;
    let issue = SourceIssue::missing(previous.as_ref());
    source_state::save_issue(db, path, Some(&issue))?;
    Ok(Some(issue))
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
    output_tokens: u64,
    usage_total: u64,
}

impl CachedCost {
    fn member(self) -> CostMember {
        let legacy_unknown = (self.output_tokens > 0 || self.usage_total > 0)
            && self
                .cost
                .as_ref()
                .is_none_or(|cost| cost.priced_requests + cost.unpriced_requests == 0);
        CostMember {
            id: self.id,
            parent_id: self.parent_id,
            thread_kind: self.thread_kind,
            title: self.title,
            agent_name: self.agent_name,
            partial: self.cost.is_none()
                || self.cost_coverage_incomplete != Some(false)
                || legacy_unknown,
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
        let mut db = Connection::open(path)?;
        db.busy_timeout(Duration::from_secs(2))?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA cache_size=-4096;
             CREATE TABLE IF NOT EXISTS sources(
                 path TEXT PRIMARY KEY, offset INTEGER NOT NULL, thread_id TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS sessions(
                 id TEXT PRIMARY KEY, last_ms INTEGER NOT NULL, state TEXT NOT NULL);",
        )?;
        coverage_migration::ensure_schema(&db)?;
        source_state::ensure_schema(&mut db)?;
        billing::ensure_schema(&db)?;
        let version = db.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))?;
        if version > CACHE_VERSION {
            return Err("统计缓存来自更新版本，请使用相应版本打开".into());
        }
        if version < CACHE_VERSION {
            // Billing state is durable even when older timeline fields need supplementing.
            db.pragma_update(None, "user_version", CACHE_VERSION)?;
        }
        let mut diagnostics = Vec::new();
        for problem in billing::migrate(&mut db)? {
            if problem.missing_source && save_missing_source(&db, &problem.path)?.is_some() {
                continue;
            }
            diagnostics.push(if problem.path.as_os_str().is_empty() {
                problem.message.clone()
            } else {
                format!("{}: {}", problem.path.display(), problem.message)
            });
            if problem.path.as_os_str().is_empty() {
                continue;
            }
            let issue = match problem.path.metadata() {
                // Audit failure means old billing proof is missing, not that today's JSON is invalid.
                // Let the normal reader recover live usage and classify any actual malformed record.
                Ok(metadata) => SourceIssue::new(
                    IssueKind::UnverifiedBilling,
                    problem.offset,
                    problem.message,
                    &metadata,
                ),
                Err(_) => SourceIssue {
                    kind: IssueKind::UnverifiedBilling,
                    offset: problem.offset,
                    message: problem.message,
                    length: 0,
                    modified_ns: None,
                    missing_source: false,
                    history_gap: false,
                    deferred_offset: None,
                    tail_offset: None,
                },
            };
            source_state::save_issue(&db, &problem.path, Some(&issue))?;
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
        let source_issues = source_state::load_issues(&db)?;
        let billing_partial = billing::partial_threads(&db)?.into_iter().collect();
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
            error: (!diagnostics.is_empty()).then(|| diagnostics.join("\n")),
            titles: HashMap::new(),
            costs,
            catalog: HashMap::new(),
            source_offsets,
            member_sources,
            failed_sources: HashSet::new(),
            attempted_sources: HashSet::new(),
            source_issues,
            billing_partial,
            hooks_path: None,
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
                    "UPDATE sessions SET state=json_set(state,'$.title',?2) WHERE id=?1",
                    params![id, session.title],
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
            source_notices: self.source_notices(),
            error: self.error.clone(),
            updated_ms: model::now_ms(),
        }
    }

    /// Publish localized missing-history context without assigning it to unrelated chats.
    fn source_notices(&self) -> Vec<SourceNotice> {
        let mut notices: Vec<_> = self
            .source_issues
            .iter()
            .filter_map(|(path, issue)| {
                let (_, owner) = self.source_offsets.get(path)?;
                if !issue.missing_source || owner.is_empty() {
                    return None;
                }
                Some(SourceNotice {
                    thread_id: owner.clone(),
                    path: path.to_string_lossy().into_owned(),
                    message: issue.message.clone(),
                })
            })
            .collect();
        notices.sort_by(|a, b| a.thread_id.cmp(&b.thread_id).then(a.path.cmp(&b.path)));
        notices
    }

    /// Only known, missing rollout files become session notices; other failures remain actionable errors.
    fn record_read_failure(&mut self, path: &Path, error: &(dyn Error + Send + Sync + 'static)) {
        let missing = error
            .downcast_ref::<IoError>()
            .is_some_and(|error| error.kind() == ErrorKind::NotFound);
        let result = if missing {
            save_missing_source(&self.db, path)
        } else {
            Ok(None)
        };
        match result {
            Ok(Some(issue)) => {
                self.source_issues.insert(path.to_owned(), issue);
                self.clear_source_error(path);
            }
            Ok(None) => self.error = Some(format!("{}: {error}", path.display())),
            Err(save_error) => {
                self.error = Some(format!(
                    "{}: {error}; 记录采集提示失败：{save_error}",
                    path.display()
                ))
            }
        }
        self.failed_sources.insert(path.to_owned());
    }

    /// Recovering one source must not erase independent diagnostics collected at startup.
    fn clear_source_error(&mut self, path: &Path) {
        let Some(message) = self.error.take() else {
            return;
        };
        let prefix = format!("{}:", path.display());
        let remaining = message
            .lines()
            .filter(|line| !line.starts_with(&prefix))
            .collect::<Vec<_>>()
            .join("\n");
        self.error = (!remaining.is_empty()).then_some(remaining);
    }

    fn cost_snapshot(&self) -> Vec<CostMember> {
        let hooks_complete = self.hooks_complete();
        let mut costs: Vec<_> = self
            .costs
            .values()
            .cloned()
            .map(|mut member| {
                member.partial |= !hooks_complete
                    || !self.sources_complete(&member.id)
                    || self.billing_partial.contains(&member.id);
                member
            })
            .collect();
        costs.sort_by(|left, right| left.id.cmp(&right.id));
        costs
    }

    /// An unfinished global hook row may contain usage for any chat, so no family can be called complete.
    fn hooks_complete(&self) -> bool {
        let Some(path) = &self.hooks_path else {
            return true;
        };
        match path.metadata() {
            Ok(metadata) => {
                !self.failed_sources.contains(path)
                    && !self.source_issues.contains_key(path)
                    && self
                        .source_offsets
                        .get(path)
                        .is_some_and(|(offset, _)| *offset == metadata.len())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                !self.failed_sources.contains(path)
                    && !self.source_issues.contains_key(path)
                    && self
                        .source_offsets
                        .get(path)
                        .is_none_or(|(offset, _)| *offset == 0)
            }
            Err(_) => false,
        }
    }

    /// A cached row is not proof that every source reached EOF, including copied rollout files.
    fn sources_complete(&self, id: &str) -> bool {
        self.member_sources.get(id).is_some_and(|paths| {
            !paths.is_empty()
                && paths.iter().all(|path| {
                    !self.failed_sources.contains(path)
                        && !self.source_issues.contains_key(path)
                        && self.catalog.get(path).is_some_and(|entry| {
                            entry.member.id == id
                                && self
                                    .source_offsets
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
                        Err(error) => self.record_read_failure(entry.path(), error.as_ref()),
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
                    let unfinished = self.source_issues.contains_key(path)
                        || self.catalog.get(path).is_some_and(|entry| {
                            !self
                                .source_offsets
                                .get(path)
                                .is_some_and(|(offset, owner)| {
                                    owner == &id && *offset == entry.length
                                })
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
            let session = self.decode_session(id, state.as_deref())?;
            self.sessions.insert(id.to_owned(), session);
        }
        Ok(())
    }

    /// Reload only after effort JSON patches were committed, preserving fresh concurrent billing state.
    fn restore_effort(&mut self, home: &Path) -> Result<()> {
        let visible_ids: Vec<_> = self.sessions.keys().cloned().collect();
        match effort_backfill::restore(&mut self.db, home, &visible_ids) {
            Ok(true) => {
                for id in visible_ids {
                    self.sessions.remove(&id);
                    self.ensure_session(&id)?;
                }
            }
            Ok(false) => (),
            Err(error) => self.error = Some(format!("历史 Think 设置回填未完成：{error}")),
        }
        Ok(())
    }

    fn decode_session(&self, id: &str, state: Option<&str>) -> Result<Session> {
        let is_new = state.is_none();
        let mut session = match state {
            Some(state) => serde_json::from_str(state)?,
            None => Session {
                id: id.to_owned(),
                ..Session::default()
            },
        };
        if is_new {
            model::reset_cost_coverage(&mut session);
        }
        if let Some(hooks) = &self.hooks_path {
            coverage_migration::restore(&self.db, &mut session, is_new, hooks)?;
        }
        if let Some(member) = self.costs.get(id) {
            session.parent_id = member.parent_id.clone();
            session.thread_kind = member.thread_kind.clone();
            session.agent_name = member.agent_name.clone();
            session.is_internal |= matches!(session.thread_kind.as_str(), "internal" | "subagent");
        }
        Ok(session)
    }

    /// Every batch starts from committed state, including changes made by another Profiler window.
    fn prepare_session(&mut self, id: &str, batch: &mut IngestBatch) -> Result<()> {
        if batch.expected.contains_key(id) {
            return Ok(());
        }
        let state: Option<String> = self
            .db
            .query_row("SELECT state FROM sessions WHERE id=?1", [id], |row| {
                row.get(0)
            })
            .optional()?;
        let session = self.decode_session(id, state.as_deref())?;
        batch.expected.insert(id.to_owned(), state);
        self.sessions.insert(id.to_owned(), session);
        Ok(())
    }

    /// Keep financial history when a source is shortened; receipts make replay idempotent.
    fn ingest(&mut self, path: &Path) -> Result<bool> {
        if path.file_name().is_some_and(|name| name == "hooks.jsonl") {
            self.hooks_path = Some(path.to_owned());
        }
        self.observe_source(path)?;
        self.attempted_sources.insert(path.to_owned());
        let mut file = File::open(path)?;
        let metadata = file.metadata()?;
        let Some(mut source) = self.prepare_source(path, &metadata)? else {
            return Ok(false);
        };
        source.initial_stamp = source_state::capture_file_stamp(&mut file, source.offset)?;
        let mut batch = IngestBatch {
            issue: source.issue.clone(),
            deferred: source.deferred,
            ..IngestBatch::default()
        };
        if source.offset == metadata.len() {
            if batch.issue.is_some() || source.needs_stamp {
                let progress = source.progress(&mut batch, source.offset, &metadata);
                self.commit(path, progress, &source.thread_id, &source.expected, &batch)?;
            }
            return Ok(false);
        }
        file.seek(SeekFrom::Start(source.offset))?;
        let mut bytes = Vec::new();
        (&mut file).take(CHUNK_BYTES).read_to_end(&mut bytes)?;
        let Some(end) = bytes.iter().rposition(|byte| *byte == b'\n') else {
            return self.handle_incomplete(path, &metadata, &source, bytes.len());
        };
        let reduced = self.reduce_chunk(path, &bytes[..=end], &metadata, &mut source, &mut batch);
        let next = match reduced {
            Ok(next) => next,
            Err(error) => {
                self.rollback_batch(&batch)?;
                return Err(error);
            }
        };
        if source_state::changed_prefix(path, &source.initial_stamp, &path.metadata()?)? {
            self.rollback_batch(&batch)?;
            return Err("来源在读取期间被替换，请重试".into());
        }
        let progress = source.progress(&mut batch, next, &metadata);
        if let Err(error) = self.commit(path, progress, &source.thread_id, &source.expected, &batch)
        {
            self.rollback_batch(&batch)?;
            return Err(error);
        }
        self.trim_cache();
        let blocked = batch
            .issue
            .as_ref()
            .is_some_and(|issue| issue.kind == IssueKind::InvalidRecord);
        Ok(!blocked
            && !source.verify_owner
            && next < metadata.len()
            && bytes.len() as u64 == CHUNK_BYTES)
    }

    fn prepare_source(
        &mut self,
        path: &Path,
        metadata: &fs::Metadata,
    ) -> Result<Option<SourceRead>> {
        let expected = source_checkpoint(&self.db, path)?;
        let (committed, thread_id, stored) = expected.clone().unwrap_or_default();
        let mut offset = committed;
        let mut issue: Option<SourceIssue> = stored
            .map(|value| serde_json::from_str(&value))
            .transpose()?;
        let verify_owner = issue.as_ref().is_some_and(|issue| issue.missing_source);
        let previous = source_state::load_stamp(&self.db, path)?;
        let replaced = match &previous {
            Some(stamp) => source_state::changed_prefix(path, stamp, metadata)?,
            None => false,
        };
        let shrunk = committed > metadata.len()
            || previous
                .as_ref()
                .is_some_and(|stamp| metadata.len() < stamp.length);
        let mut deferred = None;
        if let Some(existing) = issue.clone() {
            self.source_issues.insert(path.to_owned(), existing.clone());
            let settled =
                existing.deferred_offset.is_some() && !self.billing_partial.contains(&thread_id);
            if !replaced
                && !settled
                && existing.kind != IssueKind::TruncatedHistory
                && !existing.should_retry(metadata)
            {
                self.error = Some(format!("{}: {}", path.display(), existing.message));
                return Ok(None);
            }
            if settled {
                issue = if existing.kind == IssueKind::UnverifiedBilling && !existing.missing_source
                {
                    existing.repaired()
                } else {
                    Some(existing.clone())
                };
            } else {
                offset = existing.retry_offset(committed, metadata);
                if !replaced
                    && existing.kind != IssueKind::InvalidRecord
                    && !existing.missing_source
                {
                    if let Some(tail) = existing.tail_offset.filter(|tail| *tail <= metadata.len())
                    {
                        offset = tail;
                    }
                }
                if !replaced && offset > committed {
                    deferred = existing.deferred_offset;
                } else if existing.kind == IssueKind::UnverifiedBilling && !existing.missing_source
                {
                    issue = existing.repaired();
                }
            }
        }
        if replaced || verify_owner {
            offset = 0;
            deferred = None;
        }
        if shrunk {
            if verify_owner {
                if let Some(issue) = &mut issue {
                    issue.history_gap = true;
                }
            } else {
                issue = Some(SourceIssue::new(
                    IssueKind::TruncatedHistory,
                    committed,
                    "来源已截短，保留已有费用并重新读取现有记录".into(),
                    metadata,
                ));
            }
            offset = 0;
            deferred = None;
        }
        let initial_stamp = source_state::capture_stamp(path, offset)?;
        Ok(Some(SourceRead {
            offset,
            thread_id,
            expected,
            issue,
            deferred,
            initial_stamp,
            needs_stamp: previous.is_none(),
            verify_owner,
        }))
    }

    /// Commit only the valid prefix and leave the cursor at the first unreadable record.
    fn reduce_chunk(
        &mut self,
        path: &Path,
        bytes: &[u8],
        metadata: &fs::Metadata,
        source: &mut SourceRead,
        batch: &mut IngestBatch,
    ) -> Result<u64> {
        let mut consumed = 0;
        for line in bytes.split_inclusive(|byte| *byte == b'\n') {
            let position = source.offset + consumed;
            let record = line.strip_suffix(b"\n").unwrap_or(line);
            if record.iter().all(u8::is_ascii_whitespace) {
                consumed += line.len() as u64;
                continue;
            }
            let problem = match thin_record(record) {
                Ok(Some(row)) => {
                    if source.verify_owner
                        && (row["type"] != "session_meta"
                            || row["payload"]["id"].as_str() != Some(source.thread_id.as_str()))
                    {
                        Some((
                            IssueKind::InvalidRecord,
                            "恢复的日志缺少匹配本会话的首条元数据".into(),
                        ))
                    } else {
                        let first = position == 0 || source.verify_owner;
                        source.verify_owner = false;
                        self.apply_record(&row, &mut source.thread_id, batch, first)?
                    }
                }
                Ok(None) => None,
                Err(error) => Some((
                    IssueKind::InvalidRecord,
                    format!("invalid JSON record: {error}"),
                )),
            };
            if let Some((kind, message)) = problem {
                let mut issue = SourceIssue::new(kind, position, message, metadata);
                issue.history_gap |= batch.issue.as_ref().is_some_and(|old| old.history_gap);
                self.error = Some(format!("{}: {}", path.display(), issue.message));
                if issue.kind == IssueKind::UnverifiedBilling {
                    batch.deferred = Some(batch.deferred.map_or(position, |old| old.min(position)));
                    issue.offset = batch.deferred.unwrap();
                    batch.issue = Some(issue);
                    consumed += line.len() as u64;
                    continue;
                }
                batch.issue = Some(issue);
                break;
            }
            consumed += line.len() as u64;
            if let Some(old) = &source.issue {
                if matches!(
                    old.kind,
                    IssueKind::InvalidRecord | IssueKind::UnverifiedBilling
                ) && source.offset + consumed > old.offset
                    && batch.deferred.is_none()
                    && !source.verify_owner
                {
                    batch.issue = old.repaired();
                }
            }
        }
        Ok(source.offset + consumed)
    }

    fn apply_record(
        &mut self,
        row: &Value,
        thread_id: &mut String,
        batch: &mut IngestBatch,
        first: bool,
    ) -> Result<Option<(IssueKind, String)>> {
        let payload = &row["payload"];
        let mixed_hooks = matches!(row["type"].as_str(), Some("monitor_hook" | "hook_event"));
        if row["type"] == "session_meta" {
            if let Some(id) = payload["id"].as_str() {
                if thread_id.is_empty() {
                    *thread_id = id.to_owned();
                } else if id != thread_id {
                    if first {
                        return Ok(Some((
                            IssueKind::InvalidRecord,
                            "来源文件的会话归属已改变".into(),
                        )));
                    }
                    return Ok(None);
                }
            }
        }
        let explicit = payload["thread_id"]
            .as_str()
            .or_else(|| row["thread_id"].as_str());
        let routed = if mixed_hooks {
            explicit.unwrap_or_default().to_owned()
        } else if explicit.is_some_and(|id| id != thread_id) {
            return Ok(None);
        } else {
            thread_id.clone()
        };
        if routed.is_empty() {
            return Ok(Some((
                IssueKind::InvalidRecord,
                "record has no chat identity".into(),
            )));
        }
        let mut allow_charge = true;
        if let Some((key, time)) = billing::record_key(&routed, row) {
            if batch.receipts.contains(&key) {
                allow_charge = false;
            } else {
                match billing::decision(&self.db, &key, time)? {
                    Decision::Seen => allow_charge = false,
                    Decision::Apply => {
                        batch.receipts.insert(key);
                    }
                    Decision::Unknown => {
                        self.billing_partial.insert(routed.clone());
                        return Ok(Some((
                            IssueKind::UnverifiedBilling,
                            "历史费用账本尚未核对，已保留可确认的金额".into(),
                        )));
                    }
                }
            }
        }
        self.prepare_session(&routed, batch)?;
        if let Some(session) = self.sessions.get_mut(&routed) {
            model::apply_with_billing(session, row, allow_charge);
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
        batch.changed.insert(routed);
        Ok(None)
    }

    fn handle_incomplete(
        &mut self,
        path: &Path,
        metadata: &fs::Metadata,
        source: &SourceRead,
        read: usize,
    ) -> Result<bool> {
        if read as u64 != CHUNK_BYTES || source.offset + read as u64 >= metadata.len() {
            return Ok(false);
        }
        let mut issue = SourceIssue::new(
            IssueKind::InvalidRecord,
            source.offset,
            "record larger than 8 MiB; source remains partial".into(),
            metadata,
        );
        issue.history_gap |= source.issue.as_ref().is_some_and(|old| old.history_gap);
        self.error = Some(format!("{}: {}", path.display(), issue.message));
        let mut batch = IngestBatch {
            issue: Some(issue),
            deferred: source.deferred,
            ..IngestBatch::default()
        };
        let progress = batch.progress(source.offset, metadata);
        self.commit(path, progress, &source.thread_id, &source.expected, &batch)?;
        Ok(false)
    }

    fn rollback_batch(&mut self, batch: &IngestBatch) -> Result<()> {
        for id in batch.expected.keys() {
            self.sessions.remove(id);
            self.ensure_session(id)?;
            if let Some(session) = self.sessions.get(id) {
                self.costs
                    .insert(id.clone(), CostMember::from_session(session));
            }
        }
        Ok(())
    }

    /// The source version, financial claims and reduced sessions are one optimistic unit of work.
    fn commit(
        &mut self,
        path: &Path,
        progress: SourceProgress,
        thread_id: &str,
        expected_source: &SourceCheckpoint,
        batch: &IngestBatch,
    ) -> Result<()> {
        let stamp = source_state::capture_stamp(path, progress.scanned)?;
        let transaction = self.db.transaction()?;
        let current = source_checkpoint(&transaction, path)?;
        if &current != expected_source {
            return Err("source advanced in another Profiler window; retry required".into());
        }
        for (id, expected) in &batch.expected {
            let current: Option<String> = transaction
                .query_row("SELECT state FROM sessions WHERE id=?1", [id], |row| {
                    row.get(0)
                })
                .optional()?;
            if &current != expected {
                return Err("session changed in another Profiler window; retry required".into());
            }
        }
        billing::commit_new(
            &transaction,
            &batch.receipts.iter().cloned().collect::<Vec<_>>(),
        )?;
        for id in &batch.changed {
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
            params![path.to_string_lossy(), progress.committed, thread_id],
        )?;
        source_state::save_issue(&transaction, path, batch.issue.as_ref())?;
        source_state::save_stamp(&transaction, path, Some(&stamp))?;
        transaction.commit()?;
        self.source_offsets
            .insert(path.to_owned(), (progress.committed, thread_id.to_owned()));
        if let Some(entry) = self.catalog.get_mut(path) {
            entry.length = stamp.length;
        }
        if let Some(issue) = &batch.issue {
            self.source_issues.insert(path.to_owned(), issue.clone());
            if issue.kind == IssueKind::TruncatedHistory {
                self.clear_source_error(path);
            }
        } else {
            self.source_issues.remove(path);
            self.clear_source_error(path);
        }
        if batch
            .issue
            .as_ref()
            .is_some_and(|issue| issue.kind == IssueKind::InvalidRecord)
        {
            self.failed_sources.insert(path.to_owned());
        } else {
            self.failed_sources.remove(path);
        }
        for id in &batch.changed {
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
    store.restore_effort(&home)?;
    store.hooks_path = Some(hooks.clone());
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
            store.record_read_failure(&path, error.as_ref());
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

    // A deleted old log becomes a notice for its owner after restart, preserving every cached cost and cursor.
    #[test]
    fn missing_history_is_scoped_after_restart_without_losing_cache() {
        let dir = fixture("missing-history-notice");
        let missing = dir.join("rollout-missing.jsonl");
        fs::write(
            &missing,
            serde_json::json!({"type":"session_meta","payload":{
            "id":"missing","source":"vscode"}})
            .to_string()
                + "\n",
        )
        .unwrap();
        let healthy = write_cost_rollout(&dir, "healthy", "healthy", "", 100, 7);
        let db = dir.join("state.sqlite");
        let mut store = Store::open(&db).unwrap();
        store.ingest(&missing).unwrap();
        store.ingest(&healthy).unwrap();
        let offset = store.source_offsets[&missing].0;
        let stamp = source_state::load_stamp(&store.db, &missing).unwrap();
        drop(store);
        fs::remove_file(&missing).unwrap();
        for _ in 0..2 {
            let store = Store::open(&db).unwrap();
            let snapshot = store.snapshot();
            assert!(snapshot.error.is_none());
            assert_eq!(snapshot.source_notices.len(), 1);
            assert_eq!(snapshot.source_notices[0].thread_id, "missing");
            assert_eq!(snapshot.source_notices[0].path, missing.to_string_lossy());
            assert_eq!(store.source_offsets[&missing].0, offset);
            assert_eq!(
                source_state::load_stamp(&store.db, &missing).unwrap(),
                stamp
            );
            assert_eq!(
                store.costs["healthy"].cost.as_ref().unwrap().output_tokens,
                7
            );
            assert!(
                snapshot
                    .costs
                    .iter()
                    .find(|m| m.id == "missing")
                    .unwrap()
                    .partial
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }

    // A deletion during queued ingestion stays local; restoring the original file clears it without recharging.
    #[test]
    fn missing_runtime_source_recovers_without_duplicate_fees_or_retry_loop() {
        let dir = fixture("missing-live-source");
        let file = write_cost_rollout(&dir, "first", "chat", "", 100, 7);
        let original = fs::read(&file).unwrap();
        let mut store = Store::open(&dir.join("state.sqlite")).unwrap();
        store.ingest(&file).unwrap();
        let offset = store.source_offsets[&file].0;
        let stamp = source_state::load_stamp(&store.db, &file).unwrap();
        fs::remove_file(&file).unwrap();
        let mut queue = VecDeque::new();
        let mut queued = HashSet::new();
        let index = dir.join("session_index.jsonl");
        process_queued(&mut store, file.clone(), &index, &mut queue, &mut queued);
        assert!(store.error.is_none());
        assert!(queue.is_empty());
        assert_eq!(store.source_notices().len(), 1);
        assert_eq!(store.source_offsets[&file].0, offset);
        assert_eq!(source_state::load_stamp(&store.db, &file).unwrap(), stamp);
        fs::write(&file, original).unwrap();
        process_queued(&mut store, file.clone(), &index, &mut queue, &mut queued);
        assert!(store.source_notices().is_empty());
        assert!(!store.failed_sources.contains(&file));
        assert_eq!(store.sessions["chat"].cost.output_tokens, 7);
        assert_eq!(store.sessions["chat"].cost.priced_requests, 1);
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }

    // Empty recovery cannot prove availability; a foreign header cannot attach new fees to the original owner.
    #[test]
    fn missing_source_recovery_requires_matching_metadata() {
        let dir = fixture("missing-source-owner");
        let file = write_cost_rollout(&dir, "first", "chat", "", 100, 7);
        let mut store = Store::open(&dir.join("state.sqlite")).unwrap();
        store.ingest(&file).unwrap();
        fs::remove_file(&file).unwrap();
        store.record_read_failure(&file, &IoError::from(ErrorKind::NotFound));
        fs::write(&file, b"\n").unwrap();
        store.ingest(&file).unwrap();
        assert!(store.source_issues[&file].missing_source);
        assert!(store.source_issues[&file].history_gap);
        fs::write(
            &file,
            serde_json::json!({"type":"session_meta","payload":{"id":"other"}}).to_string() + "\n",
        )
        .unwrap();
        store.ingest(&file).unwrap();
        assert_eq!(store.source_issues[&file].kind, IssueKind::InvalidRecord);
        assert!(store
            .error
            .as_ref()
            .unwrap()
            .contains(file.to_string_lossy().as_ref()));
        assert_eq!(store.sessions["chat"].cost.output_tokens, 7);
        assert_eq!(store.sessions["chat"].cost.priced_requests, 1);
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }

    // Permission failures and unowned missing files remain global diagnostics with concrete paths.
    #[test]
    fn nonhistorical_read_errors_are_not_hidden() {
        let dir = fixture("unscoped-source-failure");
        let file = write_cost_rollout(&dir, "first", "chat", "", 100, 7);
        let mut store = Store::open(&dir.join("state.sqlite")).unwrap();
        store.ingest(&file).unwrap();
        let unknown = dir.join("rollout-unowned.jsonl");
        for (path, kind) in [
            (&file, ErrorKind::PermissionDenied),
            (&unknown, ErrorKind::NotFound),
        ] {
            store.record_read_failure(path, &IoError::from(kind));
            assert!(store.source_notices().is_empty());
            assert!(store
                .error
                .as_ref()
                .unwrap()
                .contains(path.to_string_lossy().as_ref()));
        }
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }

    // Reclassifying and then restoring A leaves B's independent global error visible throughout.
    #[test]
    fn missing_source_does_not_clear_other_global_diagnostics() {
        let dir = fixture("independent-source-diagnostics");
        let file = write_cost_rollout(&dir, "first", "chat", "", 100, 7);
        let mut store = Store::open(&dir.join("state.sqlite")).unwrap();
        store.ingest(&file).unwrap();
        let other = format!(
            "{}: permission denied",
            dir.join("rollout-other.jsonl").display()
        );
        store.error = Some(format!("{}: invalid JSON\n{other}", file.display()));
        store.record_read_failure(&file, &IoError::from(ErrorKind::NotFound));
        assert_eq!(store.error.as_deref(), Some(other.as_str()));
        store.ingest(&file).unwrap();
        assert_eq!(store.error.as_deref(), Some(other.as_str()));
        assert!(store.source_notices().is_empty());
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }

    // A restored non-transcript stops after one chunk and cannot move the original audit cursor.
    #[test]
    fn missing_source_without_metadata_does_not_spin_or_commit_foreign_progress() {
        let dir = fixture("missing-source-no-metadata");
        let file = write_cost_rollout(&dir, "first", "chat", "", 100, 7);
        let mut store = Store::open(&dir.join("state.sqlite")).unwrap();
        store.ingest(&file).unwrap();
        let offset = store.source_offsets[&file].0;
        fs::remove_file(&file).unwrap();
        store.record_read_failure(&file, &IoError::from(ErrorKind::NotFound));
        let row = serde_json::json!({"type":"response_item","payload":{
            "type":"message","role":"assistant","content":"x".repeat(64*1024)}})
        .to_string()
            + "\n";
        fs::write(&file, row.repeat(CHUNK_BYTES as usize / row.len() + 2)).unwrap();
        assert!(!store.ingest(&file).unwrap());
        assert_eq!(store.source_offsets[&file].0, offset);
        assert_eq!(store.source_notices().len(), 1);
        assert_eq!(store.sessions["chat"].cost.output_tokens, 7);
        assert!(store.error.is_none());
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }

    // Unknown old history does not block safe live fees; once proof returns the held prefix is replayed.
    #[test]
    fn unverified_history_keeps_a_live_tail_and_recovers_after_restart() {
        let dir = fixture("live-tail-cost");
        let old = dir.join("rollout-missing.jsonl");
        let meta = serde_json::json!({"type":"session_meta","timestamp_ms":1,
            "payload":{"id":"chat","source":"vscode"}})
        .to_string()
            + "\n";
        let old_rows = meta.clone() + &canonical("billed", "chat", 100, 7);
        let db = dir.join("state.sqlite");
        let store = Store::open(&db).unwrap();
        let mut initial = Session {
            id: "chat".into(),
            last_ms: 500,
            cost: CostSummary {
                output_tokens: 7,
                output_usd: 1.0,
                priced_requests: 1,
                ..CostSummary::default()
            },
            billing_seen: vec!["billed".into()],
            ..Session::default()
        };
        model::reset_cost_coverage(&mut initial);
        store
            .db
            .execute(
                "INSERT INTO sessions VALUES('chat',500,?1)",
                [serde_json::to_string(&initial).unwrap()],
            )
            .unwrap();
        store
            .db
            .execute(
                "INSERT INTO sources(path,offset,thread_id) VALUES(?1,?2,'chat')",
                params![old.to_string_lossy(), old_rows.len()],
            )
            .unwrap();
        drop(store);
        let live = dir.join("rollout-live.jsonl");
        fs::write(
            &live,
            meta + &canonical("uncertain", "chat", 200, 5) + &canonical("new", "chat", 1000, 11),
        )
        .unwrap();
        let mut store = Store::open(&db).unwrap();
        store.ingest(&live).unwrap();
        assert_eq!(store.sessions["chat"].cost.output_tokens, 18);
        assert!(store.source_offsets[&live].0 < live.metadata().unwrap().len());
        assert_eq!(
            store.source_issues[&live].tail_offset,
            Some(live.metadata().unwrap().len())
        );
        drop(store);
        let mut store = Store::open(&db).unwrap();
        File::options()
            .append(true)
            .open(&live)
            .unwrap()
            .write_all(canonical("next", "chat", 1100, 13).as_bytes())
            .unwrap();
        store.ingest(&live).unwrap();
        assert_eq!(store.sessions["chat"].cost.output_tokens, 31);
        fs::write(&old, old_rows).unwrap();
        store.ingest(&old).unwrap();
        assert!(store.source_offsets[&old].0 < old.metadata().unwrap().len());
        assert_eq!(store.sessions["chat"].cost.output_tokens, 31);
        drop(store);
        let mut store = Store::open(&db).unwrap();
        store.discover(&dir).unwrap();
        store.ingest(&old).unwrap();
        store.ingest(&live).unwrap();
        assert_eq!(store.sessions["chat"].cost.output_tokens, 36);
        assert!(!store.source_issues.contains_key(&live));
        assert_eq!(
            store.source_offsets[&live].0,
            live.metadata().unwrap().len()
        );
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }

    // Upgrading a stamp-less v0.2.5 cache must read valid new work despite a shorter old audit source.
    #[test]
    fn legacy_shortened_source_still_collects_safe_live_fees() {
        let dir = fixture("legacy-short-live-fees");
        let file = dir.join("rollout-chat.jsonl");
        let meta = serde_json::json!({"type":"session_meta","timestamp_ms":1,
            "payload":{"id":"chat","source":"vscode"}})
        .to_string()
            + "\n";
        fs::write(&file, meta + &canonical("new-live", "chat", 1000, 11)).unwrap();
        let original_cutoff = file.metadata().unwrap().len() + 123;
        let db_path = dir.join("state.sqlite");
        let db = Connection::open(&db_path).unwrap();
        db.execute_batch("PRAGMA user_version=8;
            CREATE TABLE sources(path TEXT PRIMARY KEY,offset INTEGER NOT NULL,thread_id TEXT NOT NULL);
            CREATE TABLE sessions(id TEXT PRIMARY KEY,last_ms INTEGER NOT NULL,state TEXT NOT NULL);")
            .unwrap();
        let legacy = serde_json::json!({"id":"chat","last_ms":500,
            "cost":{"output_tokens":7,"output_usd":1.0,"priced_requests":1},
            "billing_seen":["already-billed"]});
        db.execute(
            "INSERT INTO sessions VALUES('chat',500,?1)",
            [legacy.to_string()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO sources VALUES(?1,?2,'chat')",
            params![file.to_string_lossy(), original_cutoff],
        )
        .unwrap();
        drop(db);
        let mut store = Store::open(&db_path).unwrap();
        assert!(source_state::load_stamp(&store.db, &file)
            .unwrap()
            .is_none());
        assert_eq!(
            store.source_issues[&file].kind,
            IssueKind::UnverifiedBilling
        );
        store.ingest(&file).unwrap();
        assert_eq!(store.sessions["chat"].cost.output_tokens, 18);
        assert_eq!(store.sessions["chat"].cost.priced_requests, 2);
        assert!(store.sessions["chat"].cost.total() > 1.0);
        assert!(store.billing_partial.contains("chat"));
        assert!(
            store
                .cost_snapshot()
                .iter()
                .find(|member| member.id == "chat")
                .unwrap()
                .partial
        );
        drop(store);
        let mut store = Store::open(&db_path).unwrap();
        store.ingest(&file).unwrap();
        assert_eq!(store.sessions["chat"].cost.output_tokens, 18);
        assert_eq!(store.sessions["chat"].cost.priced_requests, 2);
        assert!(
            store
                .cost_snapshot()
                .iter()
                .find(|member| member.id == "chat")
                .unwrap()
                .partial
        );
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }

    // A damaged first hook line cannot turn complete merely because the zero-cursor file disappears.
    #[test]
    fn missing_damaged_hook_source_remains_partial_at_zero_cursor() {
        let dir = fixture("missing-damaged-hook");
        let file = write_cost_rollout(&dir, "chat", "chat", "", 100, 7);
        let hooks = dir.join("hooks.jsonl");
        fs::write(&hooks, b"{broken}\n").unwrap();
        let db = dir.join("state.sqlite");
        let mut store = Store::open(&db).unwrap();
        store.ingest(&file).unwrap();
        store.ingest(&hooks).unwrap();
        assert_eq!(store.source_offsets[&hooks].0, 0);
        fs::remove_file(&hooks).unwrap();
        assert!(!store.hooks_complete());
        drop(store);
        let mut store = Store::open(&db).unwrap();
        store.hooks_path = Some(hooks);
        store.discover(&dir).unwrap();
        assert!(
            store
                .cost_snapshot()
                .iter()
                .find(|m| m.id == "chat")
                .unwrap()
                .partial
        );
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }

    // Equal-sized and growing rewrites are replayed from zero, preserving and deduplicating old fees.
    #[test]
    fn source_rewrites_cannot_hide_new_canonical_records_before_old_cursor() {
        let dir = fixture("rewritten-source");
        let file = write_cost_rollout(&dir, "first", "chat", "", 100, 7);
        let db = dir.join("state.sqlite");
        let mut store = Store::open(&db).unwrap();
        store.ingest(&file).unwrap();
        let original = fs::read_to_string(&file).unwrap();
        let same = original.replace("\"first\"", "\"other\"");
        assert_eq!(same.len(), original.len());
        fs::write(&file, same).unwrap();
        store.ingest(&file).unwrap();
        assert_eq!(store.sessions["chat"].cost.output_tokens, 14);
        fs::write(
            &file,
            original.replace("\"first\"", "\"third\"") + &canonical("last", "chat", 200, 3),
        )
        .unwrap();
        store.ingest(&file).unwrap();
        assert_eq!(store.sessions["chat"].cost.output_tokens, 24);
        assert!(!store.ingest(&file).unwrap());
        assert_eq!(store.sessions["chat"].cost.output_tokens, 24);
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }

    // Upgraded EOF sources get a durable stamp before a later rewrite can reuse their old cursor.
    #[test]
    fn unstamped_legacy_eof_source_tracks_later_replacement() {
        let dir = fixture("legacy-source-stamp");
        let file = write_cost_rollout(&dir, "first", "chat", "", 100, 7);
        let db = dir.join("state.sqlite");
        let mut store = Store::open(&db).unwrap();
        store.ingest(&file).unwrap();
        store
            .db
            .execute("UPDATE sources SET stamp=NULL", [])
            .unwrap();
        drop(store);
        let mut store = Store::open(&db).unwrap();
        assert!(!store.ingest(&file).unwrap());
        assert!(source_state::load_stamp(&store.db, &file)
            .unwrap()
            .is_some());
        let replacement = fs::read_to_string(&file)
            .unwrap()
            .replace("\"first\"", "\"other\"");
        fs::write(&file, replacement).unwrap();
        store.ingest(&file).unwrap();
        assert_eq!(store.sessions["chat"].cost.output_tokens, 14);
        assert!(!store.ingest(&file).unwrap());
        assert_eq!(store.sessions["chat"].cost.output_tokens, 14);
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }

    fn canonical(id: &str, owner: &str, time: i64, output: u64) -> String {
        serde_json::json!({"type":"token_usage_record","timestamp_ms":time,
            "payload":{"thread_id":owner,"response_id":id,"model":"gpt-6.1-sol","turn_id":"turn",
                "usage":{"input_tokens":10,"cached_input_tokens":0,"cache_write_input_tokens":0,
                    "output_tokens":output}}})
        .to_string()
            + "\n"
    }

    // Old copied responses remain deduplicated after the model cache evicts them and after restart.
    #[test]
    fn durable_receipts_protect_historical_copy_beyond_recent_id_window() {
        let dir = fixture("durable-receipts");
        let file = dir.join("rollout-first.jsonl");
        let meta = serde_json::json!({"type":"session_meta","timestamp_ms":1,
            "payload":{"id":"chat","source":"vscode"}})
        .to_string()
            + "\n";
        let mut rows = meta.clone();
        for index in 0..2050 {
            rows.push_str(&canonical(&format!("r{index}"), "chat", index + 10, 1));
        }
        fs::write(&file, rows).unwrap();
        let db = dir.join("state.sqlite");
        let mut store = Store::open(&db).unwrap();
        store.ingest(&file).unwrap();
        drop(store);
        let mut store = Store::open(&db).unwrap();
        let copy = dir.join("rollout-copy.jsonl");
        fs::write(&copy, meta + &canonical("r0", "chat", 10, 1)).unwrap();
        store.ingest(&copy).unwrap();
        assert_eq!(store.sessions["chat"].cost.priced_requests, 2050);
        assert_eq!(store.sessions["chat"].cost.output_tokens, 2050);
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }

    // Shortening one source preserves the other source's prices and marks the missing history.
    #[test]
    fn source_truncation_preserves_all_committed_session_fees() {
        let dir = fixture("truncated-cost");
        let first = write_cost_rollout(&dir, "first", "chat", "", 100, 7);
        let second = write_cost_rollout(&dir, "second", "chat", "", 200, 11);
        let db = dir.join("state.sqlite");
        let mut store = Store::open(&db).unwrap();
        store.ingest(&first).unwrap();
        store.ingest(&second).unwrap();
        let meta = fs::read_to_string(&first)
            .unwrap()
            .lines()
            .next()
            .unwrap()
            .to_owned()
            + "\n";
        fs::write(&first, meta).unwrap();
        store.ingest(&first).unwrap();
        assert_eq!(store.sessions["chat"].cost.output_tokens, 18);
        assert!(store.source_issues[&first].history_gap);
        drop(store);
        let mut store = Store::open(&db).unwrap();
        store.discover(&dir).unwrap();
        assert_eq!(store.costs["chat"].cost.as_ref().unwrap().output_tokens, 18);
        assert!(
            store
                .cost_snapshot()
                .iter()
                .find(|m| m.id == "chat")
                .unwrap()
                .partial
        );
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }

    // A damaged row stops progress durably; fixing it resumes without charging the valid prefix twice.
    #[test]
    fn malformed_source_stays_partial_until_repaired_after_restart() {
        let dir = fixture("damaged-cost");
        let file = write_cost_rollout(&dir, "chat", "chat", "", 100, 7);
        let prefix = fs::read_to_string(&file).unwrap();
        let db = dir.join("state.sqlite");
        let mut store = Store::open(&db).unwrap();
        store.ingest(&file).unwrap();
        let tail = canonical("later", "chat", 300, 11);
        fs::write(&file, prefix.clone() + "{invalid JSON}\n" + &tail).unwrap();
        store.ingest(&file).unwrap();
        assert_eq!(store.source_offsets[&file].0, prefix.len() as u64);
        assert_eq!(store.sessions["chat"].cost.output_tokens, 7);
        drop(store);
        let mut store = Store::open(&db).unwrap();
        store.discover(&dir).unwrap();
        store.ingest(&file).unwrap();
        assert!(store.source_issues.contains_key(&file));
        assert!(!store.pending_family_paths().contains(&file));
        fs::write(
            &file,
            prefix + &canonical("restored", "chat", 200, 5) + &tail,
        )
        .unwrap();
        store.ingest(&file).unwrap();
        assert_eq!(store.sessions["chat"].cost.output_tokens, 23);
        assert!(!store.source_issues.contains_key(&file));
        assert!(store.sources_complete("chat"));
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }

    // Two open windows read the latest committed cost before reducing their own source.
    #[test]
    fn separate_collectors_do_not_overwrite_each_others_committed_fees() {
        let dir = fixture("multiple-windows");
        let first = write_cost_rollout(&dir, "first", "chat", "", 100, 7);
        let second = write_cost_rollout(&dir, "second", "chat", "", 200, 11);
        let db = dir.join("state.sqlite");
        let mut a = Store::open(&db).unwrap();
        let mut b = Store::open(&db).unwrap();
        a.ingest(&first).unwrap();
        b.ingest(&second).unwrap();
        let titles = dir.join("session_index.jsonl");
        fs::write(
            &titles,
            serde_json::json!({"id":"chat","thread_name":"renamed"}).to_string() + "\n",
        )
        .unwrap();
        a.load_titles(&titles).unwrap();
        File::options()
            .append(true)
            .open(&first)
            .unwrap()
            .write_all(canonical("next", "chat", 300, 5).as_bytes())
            .unwrap();
        a.ingest(&first).unwrap();
        assert_eq!(a.sessions["chat"].cost.output_tokens, 23);
        drop(a);
        drop(b);
        fs::remove_dir_all(dir).unwrap();
    }

    // A competing commit invalidates the whole batch, including receipt claims and cached totals.
    #[test]
    fn competing_session_update_rejects_uncommitted_receipts() {
        let dir = fixture("optimistic-cost");
        let first = write_cost_rollout(&dir, "first", "chat", "", 100, 7);
        let second = write_cost_rollout(&dir, "second", "chat", "", 200, 11);
        let db = dir.join("state.sqlite");
        let mut a = Store::open(&db).unwrap();
        a.ingest(&first).unwrap();
        let mut b = Store::open(&db).unwrap();
        let source = a
            .prepare_source(&first, &first.metadata().unwrap())
            .unwrap()
            .unwrap();
        let mut batch = IngestBatch::default();
        let mut owner = "chat".to_owned();
        let row: Value = serde_json::from_str(&canonical("pending", "chat", 300, 5)).unwrap();
        assert!(a
            .apply_record(&row, &mut owner, &mut batch, false)
            .unwrap()
            .is_none());
        b.ingest(&second).unwrap();
        assert!(a
            .commit(
                &first,
                SourceProgress {
                    committed: source.offset,
                    scanned: source.offset
                },
                &owner,
                &source.expected,
                &batch
            )
            .is_err());
        a.rollback_batch(&batch).unwrap();
        assert_eq!(a.sessions["chat"].cost.output_tokens, 18);
        let key = ReceiptKey {
            thread_id: "chat".into(),
            response_id: "pending".into(),
        };
        assert_eq!(
            billing::decision(&a.db, &key, 300).unwrap(),
            Decision::Apply
        );
        drop(a);
        drop(b);
        fs::remove_dir_all(dir).unwrap();
    }

    // An incomplete global model-hook line keeps the family partial across restart until completed.
    #[test]
    fn unread_hook_tail_prevents_complete_family_cost() {
        let dir = fixture("hook-tail-cost");
        let file = write_cost_rollout(&dir, "chat", "chat", "", 1500, 7);
        let hooks = dir.join("hooks.jsonl");
        let pre = serde_json::json!({"type":"monitor_hook","timestamp_ms":1000,
            "payload":{"thread_id":"chat","turn_id":"turn","hook_event_name":"PreModelRequest"}})
        .to_string()
            + "\n";
        let post = serde_json::json!({"type":"monitor_hook","timestamp_ms":2000,
            "payload":{"thread_id":"chat","turn_id":"turn","hook_event_name":"PostModelRequest","status":"completed"}}).to_string();
        fs::write(&hooks, pre + &post).unwrap();
        let db = dir.join("state.sqlite");
        let mut store = Store::open(&db).unwrap();
        store.ingest(&file).unwrap();
        store.ingest(&hooks).unwrap();
        assert!(!store.hooks_complete());
        drop(store);
        let mut store = Store::open(&db).unwrap();
        store.hooks_path = Some(hooks.clone());
        store.discover(&dir).unwrap();
        assert!(
            store
                .cost_snapshot()
                .iter()
                .find(|m| m.id == "chat")
                .unwrap()
                .partial
        );
        File::options()
            .append(true)
            .open(&hooks)
            .unwrap()
            .write_all(b"\n")
            .unwrap();
        store.ingest(&hooks).unwrap();
        assert!(store.hooks_complete());
        assert!(
            !store
                .cost_snapshot()
                .iter()
                .find(|m| m.id == "chat")
                .unwrap()
                .partial
        );
        drop(store);
        fs::remove_dir_all(dir).unwrap();
    }

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
    fn cache_version_upgrade_preserves_financial_state_and_cursors() {
        let dir = fixture("migration");
        let db = dir.join("state.sqlite");
        let file = dir.join("rollout-test.jsonl");
        fs::write(
            &file,
            serde_json::json!({"type":"session_meta","payload":{"id":"chat"}}).to_string()
                + "\n"
                + &canonical("first", "chat", 100, 7),
        )
        .unwrap();
        let mut store = Store::open(&db).unwrap();
        store.ingest(&file).unwrap();
        let offset = store.source_offsets[&file].0;
        store.db.pragma_update(None, "user_version", 1).unwrap();
        drop(store);
        let mut store = Store::open(&db).unwrap();
        assert_eq!(store.sessions["chat"].cost.output_tokens, 7);
        let count: i64 = store
            .db
            .query_row("SELECT count(*) FROM sources", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(store.source_offsets[&file].0, offset);
        assert!(!store.ingest(&file).unwrap());
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

    // Actual effort fields survive reduction without retaining prompts; missing and null differ.
    #[test]
    fn reasoning_effort_fields_preserve_observed_presence() {
        let context = thin_record(
            br#"{"type":"turn_context","payload":{"effort":"max","prompt":"discard"}}"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(context["payload"]["effort"], "max");
        assert!(context["payload"].get("prompt").is_none());
        for effort in [serde_json::json!("high"), Value::Null] {
            let row = serde_json::json!({"type":"event_msg","payload":{
                "type":"thread_settings_applied","thread_settings":{
                    "reasoning_effort":effort,"instructions":"discard"}}});
            let reduced = thin_record(&serde_json::to_vec(&row).unwrap())
                .unwrap()
                .unwrap();
            assert_eq!(
                reduced["payload"]["thread_settings"]["reasoning_effort"],
                effort
            );
            assert!(reduced["payload"]["thread_settings"]
                .get("instructions")
                .is_none());
        }
        let missing = thin_record(br#"{"type":"turn_context","payload":{"model":"gpt-6.1-sol"}}"#)
            .unwrap()
            .unwrap();
        assert!(missing["payload"].get("effort").is_none());
        assert!(missing["payload"].get("reasoning_effort").is_none());
        let hook = thin_record(br#"{"type":"monitor_hook","payload":{"reasoning_effort":"low"}}"#)
            .unwrap()
            .unwrap();
        assert_eq!(hook["payload"]["reasoning_effort"], "low");
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
