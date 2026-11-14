use crate::model::{self, Session};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::hash::Hasher;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const MARKER: &str = "cost-coverage-v1";

/// Upgrade only derived coverage evidence; existing costs and source cursors stay committed.
pub(crate) fn migrate(db: &mut Connection, hooks: &Path) -> Result<bool> {
    ensure_schema(db)?;
    let scope = format!("{MARKER}:{}", hooks.display());
    let completed = db
        .query_row("SELECT 1 FROM profiler_meta WHERE key=?1", [&scope], |_| {
            Ok(())
        })
        .optional()?
        .is_some();
    if completed && !has_legacy_sessions(db, hooks)? {
        return Ok(false);
    }
    let file = match File::open(hooks) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
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
    let mut eligible = source_sessions(&tx, hooks)?;
    eligible.extend(history.keys().cloned());
    for (id, observed) in &history {
        tx.execute(
            "INSERT INTO coverage_backfill(id,state) VALUES(?1,?2)
             ON CONFLICT(id) DO UPDATE SET state=excluded.state",
            params![id, compact_coverage(observed)?],
        )?;
    }
    for id in eligible {
        migrate_session(&tx, &id, history.get(&id))?;
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
fn has_legacy_sessions(db: &Connection, hooks: &Path) -> Result<bool> {
    let mut eligible = source_sessions(db, hooks)?;
    let mut known = db.prepare("SELECT id FROM coverage_backfill")?;
    for row in known.query_map([], |row| row.get::<_, String>(0))? {
        eligible.insert(row?);
    }
    let mut statement = db.prepare(
        "SELECT id FROM sessions WHERE json_extract(state,'$.cost_coverage_incomplete') IS NULL",
    )?;
    for row in statement.query_map([], |row| row.get::<_, String>(0))? {
        if eligible.contains(&row?) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn ensure_schema(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS coverage_backfill(id TEXT PRIMARY KEY, state TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS profiler_meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )?;
    Ok(())
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

fn migrate_session(db: &Connection, id: &str, observed: Option<&Session>) -> Result<()> {
    let state: Option<String> = db
        .query_row("SELECT state FROM sessions WHERE id=?1", [id], |row| {
            row.get(0)
        })
        .optional()?;
    let Some(state) = state else {
        return Ok(());
    };
    let mut session: Session = serde_json::from_str(&state)?;
    if session.cost_coverage_incomplete.is_some() {
        return Ok(());
    }
    if let Some(observed) = observed {
        model::copy_cost_coverage(&mut session, observed);
    } else {
        model::reset_cost_coverage(&mut session);
    }
    restore_current_requests(&mut session);
    model::refresh_cost_coverage(&mut session);
    db.execute(
        "UPDATE sessions SET state=?2 WHERE id=?1",
        params![id, serde_json::to_string(&session)?],
    )?;
    Ok(())
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
pub(crate) fn restore(db: &Connection, session: &mut Session, is_new: bool) -> Result<()> {
    if !is_new && session.cost_coverage_incomplete.is_some() {
        return Ok(());
    }
    let state: Option<String> = db
        .query_row(
            "SELECT state FROM coverage_backfill WHERE id=?1",
            [&session.id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(state) = state {
        let observed: Session = serde_json::from_str(&state)?;
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
        restore(&db, &mut future, true).unwrap();
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
