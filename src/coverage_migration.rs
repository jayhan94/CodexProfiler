use crate::model::{self, Session};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::hash::Hasher;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const MARKER: &str = "cost-coverage-v2";

/// Upgrade only derived coverage evidence; existing costs and source cursors stay committed.
pub(crate) fn migrate(db: &mut Connection, hooks: &Path) -> Result<bool> {
    ensure_schema(db)?;
    // Persist proven usage gaps even when the history scan cannot finish. Keep its original worklist.
    let pending = pending_legacy_sessions(db, hooks)?;
    let repaired = repair_unpriced_sessions(db, hooks)?;
    let scope = format!("{MARKER}:{}", hooks.display());
    let completed = db
        .query_row("SELECT 1 FROM profiler_meta WHERE key=?1", [&scope], |_| {
            Ok(())
        })
        .optional()?
        .is_some();
    if completed && pending.is_empty() {
        return Ok(repaired);
    }
    let file = match File::open(hooks) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(repaired),
        Err(error) => return Err(error.into()),
    };
    let length = file.metadata()?.len();
    let (history, scanned, fingerprint) = read_history(file, length)?;
    verify_history_prefix(hooks, length, fingerprint)?;
    let tx = db.transaction()?;
    let cursor: Option<u64> = tx
        .query_row(
            "SELECT offset FROM sources WHERE path=?1",
            [hooks.to_string_lossy().as_ref()],
            |row| row.get(0),
        )
        .optional()?;
    if cursor.is_some_and(|offset| offset > scanned) {
        return Err("历史 hooks 不完整，无法验证已提交的采集进度".into());
    }
    let mut eligible = eligible_sessions(&tx, hooks)?;
    eligible.extend(history.keys().cloned());
    for (id, observed) in &history {
        tx.execute(
            "INSERT INTO coverage_backfill(id,state,source) VALUES(?1,?2,?3)
             ON CONFLICT(source,id) DO UPDATE SET state=excluded.state",
            params![id, compact_coverage(observed)?, hooks.to_string_lossy()],
        )?;
    }
    for id in eligible {
        migrate_session(
            &tx,
            &id,
            history.get(&id),
            hooks,
            pending.contains(&id),
            completed,
        )?;
    }
    tx.execute(
        "INSERT INTO profiler_meta(key,value) VALUES(?1,'done')
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [&scope],
    )?;
    tx.commit()?;
    Ok(true)
}

/// A downgrade may rewrite cached sessions without coverage while leaving the marker intact.
fn pending_legacy_sessions(db: &Connection, hooks: &Path) -> Result<HashSet<String>> {
    let eligible = eligible_sessions(db, hooks)?;
    let mut pending = HashSet::new();
    let mut statement = db.prepare("SELECT id,state FROM sessions")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (id, state) = row?;
        if !eligible.contains(&id) {
            continue;
        }
        let mut session: Session = serde_json::from_str(&state)?;
        if session.cost_coverage_incomplete.is_none() || mark_legacy_gap(&mut session)? {
            pending.insert(id);
        }
    }
    Ok(pending)
}

pub(crate) fn ensure_schema(db: &Connection) -> Result<()> {
    let tx = Transaction::new_unchecked(db, TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS coverage_backfill(
             source TEXT NOT NULL DEFAULT '', id TEXT NOT NULL, state TEXT NOT NULL,
             PRIMARY KEY(source,id));
         CREATE TABLE IF NOT EXISTS profiler_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )?;
    let columns = {
        let mut statement = tx.prepare("PRAGMA table_info(coverage_backfill)")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, u64>(5)?))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    if !columns
        .iter()
        .any(|(name, pk)| name == "source" && *pk == 1)
    {
        tx.execute_batch(
            "CREATE TABLE coverage_backfill_scoped(
                source TEXT NOT NULL DEFAULT '', id TEXT NOT NULL, state TEXT NOT NULL,
                PRIMARY KEY(source,id));",
        )?;
        let source = if columns.iter().any(|(name, _)| name == "source") {
            "source"
        } else {
            "''"
        };
        tx.execute_batch(&format!(
            "INSERT INTO coverage_backfill_scoped(source,id,state)
             SELECT {source},id,state FROM coverage_backfill;
             DROP TABLE coverage_backfill;
             ALTER TABLE coverage_backfill_scoped RENAME TO coverage_backfill;"
        ))?;
    }
    tx.commit()?;
    Ok(())
}

/// Detection and repair share one home scope, including hook-only chats already backfilled here.
fn eligible_sessions(db: &Connection, hooks: &Path) -> Result<HashSet<String>> {
    let mut eligible = source_sessions(db, hooks)?;
    let mut known = db.prepare("SELECT id FROM coverage_backfill WHERE source=?1")?;
    let rows = known.query_map([hooks.to_string_lossy().as_ref()], |row| {
        row.get::<_, String>(0)
    })?;
    for row in rows {
        eligible.insert(row?);
    }
    Ok(eligible)
}

/// Missing history cannot prove completeness, but existing token evidence can still prove a gap.
fn repair_unpriced_sessions(db: &mut Connection, hooks: &Path) -> Result<bool> {
    let tx = db.transaction()?;
    let mut changed = false;
    for id in eligible_sessions(&tx, hooks)? {
        let Some(mut session) = load_session(&tx, &id)? else {
            continue;
        };
        if mark_legacy_gap(&mut session)? {
            model::refresh_cost_coverage(&mut session);
            persist_session(&tx, &session)?;
            changed = true;
        }
    }
    tx.commit()?;
    Ok(changed)
}

/// An unresolved gap is already migrated; only newly recorded evidence or a changed flag needs work.
fn mark_legacy_gap(session: &mut Session) -> Result<bool> {
    if !model::has_unpriced_legacy_usage(session) {
        return Ok(false);
    }
    let flag = session.cost_coverage_incomplete;
    let gaps = serde_json::to_string(&session.legacy_usage_gaps)?;
    model::mark_unpriced_legacy_usage(session);
    Ok(flag != session.cost_coverage_incomplete
        || gaps != serde_json::to_string(&session.legacy_usage_gaps)?)
}

/// Read a stable file prefix; an unfinished final line remains for the normal source reader.
fn read_history(file: File, length: u64) -> Result<(HashMap<String, Session>, u64, u64)> {
    let mut reader = BufReader::new(file.take(length));
    let mut history: HashMap<String, Session> = HashMap::new();
    let mut line = Vec::new();
    let mut scanned = 0;
    let mut read = 0;
    let mut fingerprint = DefaultHasher::new();
    loop {
        line.clear();
        let count = reader.read_until(b'\n', &mut line)?;
        if count == 0 {
            break;
        }
        read += count as u64;
        fingerprint.write(&line);
        if !line.ends_with(b"\n") {
            break;
        }
        scanned += count as u64;
        let row: Value = serde_json::from_slice(&line)?;
        let payload = &row["payload"];
        if !matches!(row["type"].as_str(), Some("monitor_hook" | "hook_event"))
            || !matches!(
                payload["hook_event_name"].as_str(),
                Some("PreModelRequest" | "PostModelRequest")
            )
        {
            continue;
        }
        let Some(id) = payload["thread_id"].as_str().filter(|id| !id.is_empty()) else {
            return Err("历史模型 hook 缺少会话 ID，无法核对费用".into());
        };
        let session = history.entry(id.to_owned()).or_insert_with(|| {
            let mut session = Session {
                id: id.to_owned(),
                ..Session::default()
            };
            model::reset_cost_coverage(&mut session);
            session
        });
        model::observe_cost_coverage(session, &row);
    }
    if read != length {
        return Err("历史 hooks 在扫描时被截断，请重新核对".into());
    }
    Ok((history, scanned, fingerprint.finish()))
}

/// Check the original prefix again; ordinary appends may continue while the migration runs.
fn verify_history_prefix(path: &Path, length: u64, expected: u64) -> Result<()> {
    let mut reader = BufReader::new(File::open(path)?.take(length));
    let mut fingerprint = DefaultHasher::new();
    let mut line = Vec::new();
    let mut read = 0;
    loop {
        line.clear();
        let count = reader.read_until(b'\n', &mut line)?;
        if count == 0 {
            break;
        }
        read += count as u64;
        fingerprint.write(&line);
    }
    if read != length || fingerprint.finish() != expected {
        return Err("历史 hooks 在扫描时发生更改，请重新核对".into());
    }
    Ok(())
}

/// Only migrate sessions belonging to this Codex home or explicitly named by its hook records.
fn source_sessions(db: &Connection, hooks: &Path) -> Result<HashSet<String>> {
    let Some(home) = hooks.parent().and_then(Path::parent) else {
        return Err("无法确定 Codex 主目录".into());
    };
    let root = home.join("sessions");
    let mut statement = db.prepare("SELECT path,thread_id FROM sources")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut ids = HashSet::new();
    for row in rows {
        let (path, id) = row?;
        if !id.is_empty() && Path::new(&path).starts_with(&root) {
            ids.insert(id);
        }
    }
    Ok(ids)
}

fn compact_coverage(session: &Session) -> Result<String> {
    let mut value = serde_json::to_value(session)?;
    if let Some(fields) = value.as_object_mut() {
        fields.retain(|name, _| {
            matches!(
                name.as_str(),
                "id" | "cost_coverage" | "cost_coverage_known" | "cost_coverage_incomplete"
            )
        });
    }
    Ok(serde_json::to_string(&value)?)
}

fn migrate_session(
    db: &Connection,
    id: &str,
    observed: Option<&Session>,
    hooks: &Path,
    pending: bool,
    completed: bool,
) -> Result<()> {
    let Some(mut session) = load_session(db, id)? else {
        return Ok(());
    };
    // v1 could mark old token-only usage complete; repair that mistake even when its flag exists.
    let was_legacy = session.cost_coverage_incomplete.is_none();
    let marked = mark_legacy_gap(&mut session)?;
    // A failed first scan can leave only its separately committed gap, with no hook evidence yet.
    let missing_history = !completed
        && session.cost_coverage.is_empty()
        && model::has_unpriced_legacy_usage(&session);
    if !was_legacy && !marked && !pending && !missing_history {
        return Ok(());
    }
    let previous = if observed.is_none() {
        scoped_backfill(db, id, hooks)?
    } else {
        None
    };
    if let Some(observed) = observed.or(previous.as_ref()) {
        model::copy_cost_coverage(&mut session, observed);
    } else {
        model::reset_cost_coverage(&mut session);
    }
    restore_current_requests(&mut session);
    model::refresh_cost_coverage(&mut session);
    persist_session(db, &session)
}

fn load_session(db: &Connection, id: &str) -> Result<Option<Session>> {
    let state: Option<String> = db
        .query_row("SELECT state FROM sessions WHERE id=?1", [id], |row| {
            row.get(0)
        })
        .optional()?;
    state
        .map(|state| serde_json::from_str(&state).map_err(Into::into))
        .transpose()
}

fn persist_session(db: &Connection, session: &Session) -> Result<()> {
    db.execute(
        "UPDATE sessions SET state=?2 WHERE id=?1",
        params![session.id, serde_json::to_string(session)?],
    )?;
    Ok(())
}

/// Old unscoped rows need a rollout in this home; another home's scoped evidence never applies.
fn scoped_backfill(db: &Connection, id: &str, hooks: &Path) -> Result<Option<Session>> {
    let row: Option<(String, String)> = db
        .query_row(
            "SELECT state,source FROM coverage_backfill WHERE id=?1 AND source IN (?2,'')
             ORDER BY CASE WHEN source=?2 THEN 0 ELSE 1 END LIMIT 1",
            params![id, hooks.to_string_lossy()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((state, source)) = row else {
        return Ok(None);
    };
    if source != hooks.to_string_lossy()
        && !(source.is_empty() && source_sessions(db, hooks)?.contains(id))
    {
        return Ok(None);
    }
    Ok(Some(serde_json::from_str(&state)?))
}

/// Older caches retain only the latest request list; preserve unmatched evidence not in hook history.
fn restore_current_requests(session: &mut Session) {
    let recorded = serde_json::to_value(&session.cost_coverage).unwrap_or(Value::Null);
    let missing: Vec<_> = session
        .requests
        .iter()
        .filter(|request| {
            request.source == "hook"
                && request.status == "已完成"
                && request.ended_ms.is_some()
                && !recorded.as_array().is_some_and(|items| {
                    items.iter().any(|item| {
                        item["turn_id"].as_str() == Some(session.turn_id.as_str())
                            && item["started_ms"].as_i64() == request.started_ms
                            && item["ended_ms"].as_i64() == request.ended_ms
                    })
                })
        })
        .cloned()
        .collect();
    let thread_id = session.id.clone();
    let turn_id = session.turn_id.clone();
    for request in missing {
        let response = (!request.id.starts_with("hook:") && !request.id.is_empty())
            .then_some(request.id.as_str());
        let payload = |name| {
            json!({"type":"monitor_hook", "payload":{
            "thread_id":thread_id, "turn_id":turn_id,
            "hook_event_name":name, "status":"completed", "response_id":response }})
        };
        if let Some(start) = request.started_ms {
            let mut row = payload("PreModelRequest");
            row["timestamp_ms"] = json!(start);
            model::observe_cost_coverage(session, &row);
        }
        let mut row = payload("PostModelRequest");
        row["timestamp_ms"] = json!(request.ended_ms);
        model::observe_cost_coverage(session, &row);
    }
}

/// Restore observations for a historical chat first loaded after the migration completed.
pub(crate) fn restore(
    db: &Connection,
    session: &mut Session,
    is_new: bool,
    hooks: &Path,
) -> Result<()> {
    let was_legacy = session.cost_coverage_incomplete.is_none();
    let marked = mark_legacy_gap(session)?;
    if !is_new && !was_legacy {
        if marked {
            model::refresh_cost_coverage(session);
        }
        return Ok(());
    }
    if let Some(observed) = scoped_backfill(db, &session.id, hooks)? {
        model::copy_cost_coverage(session, &observed);
        restore_current_requests(session);
        model::refresh_cost_coverage(session);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::CostSummary;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn fixture() -> (std::path::PathBuf, Connection) {
        let dir = std::env::temp_dir().join(format!(
            "codex-coverage-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(dir.join("monitor")).unwrap();
        fs::create_dir_all(dir.join("sessions")).unwrap();
        let db = Connection::open(dir.join("monitor.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE sources(path TEXT PRIMARY KEY, offset INTEGER NOT NULL, thread_id TEXT NOT NULL);
            CREATE TABLE sessions(id TEXT PRIMARY KEY, last_ms INTEGER NOT NULL, state TEXT NOT NULL);").unwrap();
        (dir, db)
    }

    fn hook(id: &str, turn: &str, kind: &str, time: i64) -> String {
        format!(
            "{}\n",
            json!({"type":"monitor_hook","timestamp_ms":time,"payload":{
            "thread_id":id,"turn_id":turn,"hook_event_name":kind,"status":"completed"}})
        )
    }

    fn legacy_state(id: &str) -> String {
        let session = Session {
            id: id.into(),
            parent_id: "parent".into(),
            thread_kind: "subagent".into(),
            cost: CostSummary {
                output_usd: 7.0,
                priced_requests: 1,
                ..CostSummary::default()
            },
            ..Session::default()
        };
        let mut value = serde_json::to_value(session).unwrap();
        for key in [
            "cost_coverage",
            "cost_coverage_known",
            "cost_coverage_incomplete",
        ] {
            value.as_object_mut().unwrap().remove(key);
        }
        value.to_string()
    }

    fn cached_session(db: &Connection, id: &str) -> Session {
        let state: String = db
            .query_row("SELECT state FROM sessions WHERE id=?1", [id], |row| {
                row.get(0)
            })
            .unwrap();
        serde_json::from_str(&state).unwrap()
    }

    fn source_row(db: &Connection, home: &Path, id: &str, offset: u64) {
        let path = home.join("sessions").join(format!("rollout-{id}.jsonl"));
        db.execute(
            "INSERT INTO sources VALUES(?1,?2,?3)",
            params![path.to_string_lossy(), offset, id],
        )
        .unwrap();
    }

    // Old token-only usage and v1's false-complete flag stay partial after refresh and restart.
    #[test]
    fn token_only_legacy_usage_survives_empty_hook_migration() {
        let (dir, mut db) = fixture();
        let path = dir.join("monitor/hooks.jsonl");
        fs::write(&path, "").unwrap();
        ensure_schema(&db).unwrap();
        db.execute(
            "INSERT INTO profiler_meta VALUES(?1,'done')",
            [format!("cost-coverage-v1:{}", path.display())],
        )
        .unwrap();
        for (id, flag) in [("legacy", None), ("v1", Some(false))] {
            let session = Session {
                id: id.into(),
                parent_id: "parent".into(),
                thread_kind: "subagent".into(),
                turn_id: "old-turn".into(),
                usage_total: 120,
                output_tokens: 99,
                cost_coverage_incomplete: flag,
                ..Session::default()
            };
            db.execute(
                "INSERT INTO sessions VALUES(?1,1,?2)",
                params![id, serde_json::to_string(&session).unwrap()],
            )
            .unwrap();
            source_row(&db, &dir, id, 123);
        }
        db.execute(
            "INSERT INTO sessions VALUES('unused',1,?1)",
            [serde_json::to_string(&Session {
                id: "unused".into(),
                ..Session::default()
            })
            .unwrap()],
        )
        .unwrap();
        source_row(&db, &dir, "unused", 0);
        assert!(migrate(&mut db, &path).unwrap());
        for id in ["legacy", "v1"] {
            let mut session = cached_session(&db, id);
            assert_eq!(session.cost_coverage_incomplete, Some(true));
            assert_eq!(session.cost.priced_requests, 0);
            assert_eq!(session.usage_total, 120);
            model::refresh_cost_coverage(&mut session);
            assert!(model::CostMember::from_session(&session).partial);
            persist_session(&db, &session).unwrap();
        }
        assert_eq!(
            cached_session(&db, "unused").cost_coverage_incomplete,
            Some(false)
        );
        drop(db);
        let mut db = Connection::open(dir.join("monitor.sqlite")).unwrap();
        assert!(!migrate(&mut db, &path).unwrap());
        for id in ["legacy", "v1"] {
            let mut session = cached_session(&db, id);
            model::refresh_cost_coverage(&mut session);
            assert_eq!(session.cost_coverage_incomplete, Some(true));
        }
        let offsets: Vec<u64> = db
            .prepare("SELECT offset FROM sources WHERE thread_id!='unused'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert_eq!(offsets, [123, 123]);
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    // With no hook file, repair v1's false-complete row once without inventing a completed scan.
    #[test]
    fn missing_history_repairs_legacy_gap_without_a_completion_marker() {
        let (dir, mut db) = fixture();
        let path = dir.join("monitor/hooks.jsonl");
        let session = Session {
            id: "child".into(),
            turn_id: "old-turn".into(),
            usage_total: 120,
            cost_coverage_incomplete: Some(false),
            ..Session::default()
        };
        db.execute(
            "INSERT INTO sessions VALUES('child',1,?1)",
            [serde_json::to_string(&session).unwrap()],
        )
        .unwrap();
        source_row(&db, &dir, "child", 321);
        assert!(migrate(&mut db, &path).unwrap());
        assert_eq!(
            cached_session(&db, "child").cost_coverage_incomplete,
            Some(true)
        );
        drop(db);
        let mut db = Connection::open(dir.join("monitor.sqlite")).unwrap();
        assert!(!migrate(&mut db, &path).unwrap());
        let markers: u64 = db
            .query_row("SELECT count(*) FROM profiler_meta", [], |row| row.get(0))
            .unwrap();
        assert_eq!(markers, 0);
        let offset: u64 = db
            .query_row("SELECT offset FROM sources", [], |row| row.get(0))
            .unwrap();
        assert_eq!(offset, 321);
        assert_eq!(cached_session(&db, "child").cost.total(), 0.0);
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    // A retained false flag must be repaired even with v2's marker and no missing-field row.
    #[test]
    fn completed_marker_does_not_hide_false_complete_legacy_usage() {
        for with_history in [true, false] {
            let (dir, mut db) = fixture();
            let path = dir.join("monitor/hooks.jsonl");
            if with_history {
                fs::write(&path, "").unwrap();
            }
            ensure_schema(&db).unwrap();
            db.execute(
                "INSERT INTO profiler_meta VALUES(?1,'done')",
                [format!("{MARKER}:{}", path.display())],
            )
            .unwrap();
            let session = Session {
                id: "child".into(),
                turn_id: "old-turn".into(),
                output_tokens: 99,
                usage_total: 120,
                cost_coverage_incomplete: Some(false),
                ..Session::default()
            };
            db.execute(
                "INSERT INTO sessions VALUES('child',1,?1)",
                [serde_json::to_string(&session).unwrap()],
            )
            .unwrap();
            source_row(&db, &dir, "child", 123);
            let mut cold = session;
            restore(&db, &mut cold, false, &path).unwrap();
            assert_eq!(cold.cost_coverage_incomplete, Some(true));
            assert!(!cold.legacy_usage_gaps.is_empty());
            assert!(migrate(&mut db, &path).unwrap());
            drop(db);
            let mut db = Connection::open(dir.join("monitor.sqlite")).unwrap();
            assert!(!migrate(&mut db, &path).unwrap());
            assert_eq!(
                cached_session(&db, "child").cost_coverage_incomplete,
                Some(true)
            );
            drop(db);
            fs::remove_dir_all(dir).unwrap();
        }
    }

    // Upgrade unscoped backfills without losing rows; another home's downgrade cannot retrigger A.
    #[test]
    fn scoped_migration_is_idempotent_with_another_legacy_home() {
        let (dir, mut db) = fixture();
        let other = dir.join("other-home");
        fs::create_dir_all(other.join("monitor")).unwrap();
        fs::create_dir_all(other.join("sessions")).unwrap();
        let first = dir.join("monitor/hooks.jsonl");
        let second = other.join("monitor/hooks.jsonl");
        fs::write(&first, hook("a", "turn", "PostModelRequest", 1000)).unwrap();
        fs::write(&second, hook("b", "turn", "PostModelRequest", 2000)).unwrap();
        db.execute_batch(
            "CREATE TABLE coverage_backfill(id TEXT PRIMARY KEY,state TEXT NOT NULL);
             CREATE TABLE profiler_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);",
        )
        .unwrap();
        for (id, home, path) in [("a", &dir, &first), ("b", &other, &second)] {
            db.execute(
                "INSERT INTO sessions VALUES(?1,1,?2)",
                params![id, legacy_state(id)],
            )
            .unwrap();
            db.execute(
                "INSERT INTO coverage_backfill VALUES(?1,?2)",
                params![id, legacy_state(id)],
            )
            .unwrap();
            db.execute(
                "INSERT INTO profiler_meta VALUES(?1,'done')",
                [format!("cost-coverage-v1:{}", path.display())],
            )
            .unwrap();
            source_row(&db, home, id, 456);
        }
        assert!(migrate(&mut db, &first).unwrap());
        let unscoped: String = db
            .query_row(
                "SELECT source FROM coverage_backfill WHERE id='b'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(unscoped.is_empty());
        assert_eq!(cached_session(&db, "b").cost.total(), 7.0);
        assert!(scoped_backfill(&db, "b", &second).unwrap().is_some());
        assert!(scoped_backfill(&db, "b", &first).unwrap().is_none());
        assert!(migrate(&mut db, &second).unwrap());
        db.execute(
            "UPDATE sessions SET state=?1 WHERE id='b'",
            [legacy_state("b")],
        )
        .unwrap();
        drop(db);
        let mut db = Connection::open(dir.join("monitor.sqlite")).unwrap();
        assert!(!migrate(&mut db, &first).unwrap());
        assert!(!migrate(&mut db, &first).unwrap());
        assert_eq!(cached_session(&db, "b").cost_coverage_incomplete, None);
        assert!(migrate(&mut db, &second).unwrap());
        assert!(!migrate(&mut db, &second).unwrap());
        assert_eq!(cached_session(&db, "b").cost.total(), 7.0);
        let owner: String = db
            .query_row(
                "SELECT source FROM coverage_backfill WHERE id='b' AND source!=''",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(owner, second.to_string_lossy());
        assert!(scoped_backfill(&db, "b", &first).unwrap().is_none());
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    // Upgrading the old id-only key must retain each home's evidence for a copied thread ID.
    #[test]
    fn same_thread_id_keeps_independent_scoped_backfills() {
        let (dir, mut db) = fixture();
        let other = dir.join("other-home");
        fs::create_dir_all(other.join("monitor")).unwrap();
        let first = dir.join("monitor/hooks.jsonl");
        let second = other.join("monitor/hooks.jsonl");
        let a = hook("shared", "turn-a", "PostModelRequest", 1000);
        let b = hook("shared", "turn-b", "PostModelRequest", 2000)
            + &hook("shared", "turn-b", "PostModelRequest", 3000);
        fs::write(&first, &a).unwrap();
        fs::write(&second, &b).unwrap();
        let (history, _, _) = read_history(File::open(&first).unwrap(), a.len() as u64).unwrap();
        db.execute_batch(
            "CREATE TABLE coverage_backfill(id TEXT PRIMARY KEY,state TEXT NOT NULL,
                source TEXT NOT NULL DEFAULT '');
             CREATE TABLE profiler_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);",
        )
        .unwrap();
        db.execute(
            "INSERT INTO coverage_backfill VALUES('shared',?1,?2)",
            params![
                compact_coverage(&history["shared"]).unwrap(),
                first.to_string_lossy()
            ],
        )
        .unwrap();
        db.execute(
            "INSERT INTO profiler_meta VALUES(?1,'done')",
            [format!("{MARKER}:{}", first.display())],
        )
        .unwrap();
        assert!(migrate(&mut db, &second).unwrap());
        assert!(!migrate(&mut db, &first).unwrap());
        let count: u64 = db
            .query_row(
                "SELECT count(*) FROM coverage_backfill WHERE id='shared'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
        for (path, turn, expected) in [(&first, "turn-a", 1), (&second, "turn-b", 2)] {
            let mut session = Session {
                id: "shared".into(),
                ..Session::default()
            };
            model::reset_cost_coverage(&mut session);
            restore(&db, &mut session, true, path).unwrap();
            assert_eq!(session.cost_coverage.len(), expected);
            let coverage = serde_json::to_value(&session.cost_coverage).unwrap();
            assert!(coverage
                .as_array()
                .unwrap()
                .iter()
                .all(|request| request["turn_id"] == turn));
        }
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    // Upgrade preserves committed cost/cursor, flags unmatched history, and remains idempotent.
    #[test]
    fn migration_preserves_cached_cost_and_restores_unloaded_children() {
        let (dir, mut db) = fixture();
        let path = dir.join("monitor/hooks.jsonl");
        let rows = hook("child", "turn", "PreModelRequest", 1000)
            + &hook("child", "turn", "PostModelRequest", 2000)
            + &hook("future", "turn", "PreModelRequest", 3000)
            + &hook("future", "turn", "PostModelRequest", 4000);
        fs::write(&path, &rows).unwrap();
        db.execute(
            "INSERT INTO sessions VALUES('child',1,?1)",
            [legacy_state("child")],
        )
        .unwrap();
        db.execute(
            "INSERT INTO sources VALUES(?1,?2,'')",
            params![path.to_string_lossy(), rows.len()],
        )
        .unwrap();
        assert!(migrate(&mut db, &path).unwrap());
        let state: String = db
            .query_row("SELECT state FROM sessions WHERE id='child'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let child: Session = serde_json::from_str(&state).unwrap();
        assert_eq!(child.cost.total(), 7.0);
        assert_eq!(child.parent_id, "parent");
        assert_eq!(child.cost_coverage_incomplete, Some(true));
        let offset: usize = db
            .query_row(
                "SELECT offset FROM sources WHERE path=?1",
                [path.to_string_lossy().as_ref()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(offset, rows.len());
        assert!(!migrate(&mut db, &path).unwrap());
        let mut future = Session {
            id: "future".into(),
            ..Session::default()
        };
        model::reset_cost_coverage(&mut future);
        restore(&db, &mut future, true, &path).unwrap();
        assert_eq!(future.cost_coverage_incomplete, Some(true));
        // A downgrade can remove the new fields while leaving the migration marker intact.
        db.execute(
            "UPDATE sessions SET state=?1 WHERE id='child'",
            [legacy_state("child")],
        )
        .unwrap();
        assert!(migrate(&mut db, &path).unwrap());
        let state: String = db
            .query_row("SELECT state FROM sessions WHERE id='child'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Session>(&state)
                .unwrap()
                .cost_coverage_incomplete,
            Some(true)
        );
        assert!(!migrate(&mut db, &path).unwrap());
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    // A damaged or shorter hook source cannot commit a misleading complete migration.
    #[test]
    fn migration_failure_leaves_legacy_state_and_cursor_untouched() {
        let (dir, mut db) = fixture();
        let path = dir.join("monitor/hooks.jsonl");
        let rows = hook("child", "turn", "PreModelRequest", 1000);
        fs::write(&path, &rows).unwrap();
        let original = legacy_state("child");
        db.execute("INSERT INTO sessions VALUES('child',1,?1)", [&original])
            .unwrap();
        db.execute(
            "INSERT INTO sources VALUES(?1,?2,'')",
            params![path.to_string_lossy(), rows.len() + 1],
        )
        .unwrap();
        assert!(migrate(&mut db, &path).is_err());
        let state: String = db
            .query_row("SELECT state FROM sessions WHERE id='child'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(state, original);
        let marker: i64 = db
            .query_row("SELECT count(*) FROM profiler_meta", [], |r| r.get(0))
            .unwrap();
        assert_eq!(marker, 0);
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    // Damaged history cannot hide a proven gap; repairing it later still restores hook evidence.
    #[test]
    fn damaged_history_keeps_repaired_gap_and_retries_backfill_after_restart() {
        let (dir, mut db) = fixture();
        let path = dir.join("monitor/hooks.jsonl");
        fs::write(&path, "broken-json\n").unwrap();
        ensure_schema(&db).unwrap();
        db.execute(
            "INSERT INTO profiler_meta VALUES(?1,'done')",
            [format!("cost-coverage-v1:{}", path.display())],
        )
        .unwrap();
        let session = Session {
            id: "child".into(),
            turn_id: "old-turn".into(),
            output_tokens: 99,
            usage_total: 120,
            cost_coverage_incomplete: Some(false),
            ..Session::default()
        };
        db.execute(
            "INSERT INTO sessions VALUES('child',1,?1)",
            [serde_json::to_string(&session).unwrap()],
        )
        .unwrap();
        source_row(&db, &dir, "child", 123);
        db.execute(
            "INSERT INTO sources VALUES(?1,0,'')",
            [path.to_string_lossy().as_ref()],
        )
        .unwrap();
        assert!(migrate(&mut db, &path).is_err());
        let repaired = cached_session(&db, "child");
        assert_eq!(repaired.cost_coverage_incomplete, Some(true));
        assert!(!repaired.legacy_usage_gaps.is_empty());
        assert_eq!(repaired.cost.total(), session.cost.total());
        assert_eq!(repaired.cost.priced_requests, 0);
        assert_eq!(repaired.usage_total, 120);
        let done: u64 = db
            .query_row(
                "SELECT count(*) FROM profiler_meta WHERE key=?1",
                [format!("{MARKER}:{}", path.display())],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(done, 0);
        let backfills: u64 = db
            .query_row("SELECT count(*) FROM coverage_backfill", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(backfills, 0);
        fs::write(&path, hook("child", "old-turn", "PostModelRequest", 1000)).unwrap();
        drop(db);
        let mut db = Connection::open(dir.join("monitor.sqlite")).unwrap();
        assert!(migrate(&mut db, &path).unwrap());
        let repaired = cached_session(&db, "child");
        assert_eq!(repaired.cost_coverage.len(), 1);
        assert_eq!(repaired.cost_coverage_incomplete, Some(true));
        assert!(!migrate(&mut db, &path).unwrap());
        let offsets: Vec<u64> = db
            .prepare("SELECT offset FROM sources ORDER BY offset")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert_eq!(offsets, [0, 123]);
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    // Truncation and same-sized replacement must not validate as a complete legacy scan.
    #[test]
    fn migration_rejects_a_replaced_or_truncated_hook_source() {
        let (dir, db) = fixture();
        let path = dir.join("monitor/hooks.jsonl");
        let rows = hook("child", "turn", "PreModelRequest", 1000);
        fs::write(&path, &rows).unwrap();
        let file = File::open(&path).unwrap();
        let length = rows.len() as u64;
        fs::write(&path, b"{}").unwrap();
        assert!(read_history(file, length).is_err());
        fs::write(&path, &rows).unwrap();
        let (_, _, hash) = read_history(File::open(&path).unwrap(), length).unwrap();
        fs::write(&path, rows.replace("child", "other")).unwrap();
        assert!(verify_history_prefix(&path, length, hash).is_err());
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }
}
