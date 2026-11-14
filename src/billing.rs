use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};

pub(crate) type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct ReceiptKey {
    pub thread_id: String,
    pub response_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    Apply,
    Seen,
    Unknown,
}

#[derive(Clone, Debug)]
pub(crate) struct SourceProblem {
    pub path: PathBuf,
    pub offset: u64,
    pub message: String,
}

pub(crate) fn ensure_schema(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS billing_receipts(
        thread_id TEXT NOT NULL,response_id TEXT NOT NULL,PRIMARY KEY(thread_id,response_id));
        CREATE TABLE IF NOT EXISTS billing_threads(
        thread_id TEXT PRIMARY KEY,ready INTEGER NOT NULL,legacy_watermark INTEGER);
        CREATE TABLE IF NOT EXISTS billing_source_prefixes(
        owner TEXT NOT NULL,path TEXT NOT NULL,cutoff INTEGER NOT NULL,PRIMARY KEY(owner,path));",
    )?;
    let columns = {
        let mut statement = db.prepare("PRAGMA table_info(billing_threads)")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
        rows.collect::<std::result::Result<BTreeSet<_>, _>>()?
    };
    for (name, sql) in [
        (
            "audited_cost_count",
            "ALTER TABLE billing_threads ADD COLUMN audited_cost_count INTEGER",
        ),
        (
            "audited_receipt_count",
            "ALTER TABLE billing_threads ADD COLUMN audited_receipt_count INTEGER",
        ),
        (
            "audited_missing_ids",
            "ALTER TABLE billing_threads ADD COLUMN audited_missing_ids TEXT",
        ),
    ] {
        if !columns.contains(name) {
            db.execute_batch(sql)?;
        }
    }
    Ok(())
}

/// Receipt ownership follows the source's first metadata identity, never copied foreign history.
pub(crate) fn record_key(owner: &str, row: &Value) -> Option<(ReceiptKey, i64)> {
    if row["type"] != "token_usage_record" || owner.is_empty() {
        return None;
    }
    let p = &row["payload"];
    if p["thread_id"].as_str().is_some_and(|id| id != owner) {
        return None;
    }
    let response = p["response_id"].as_str().filter(|id| !id.is_empty())?;
    let time = row["timestamp_ms"]
        .as_i64()
        .or_else(|| {
            row["timestamp"]
                .as_str()
                .and_then(|time| chrono::DateTime::parse_from_rfc3339(time).ok())
                .map(|time| time.timestamp_millis())
        })
        .unwrap_or(0);
    Some((
        ReceiptKey {
            thread_id: owner.into(),
            response_id: response.into(),
        },
        time,
    ))
}

/// Missing historical evidence blocks old replay, but cannot prevent proven post-watermark live work.
pub(crate) fn decision(db: &Connection, key: &ReceiptKey, timestamp_ms: i64) -> Result<Decision> {
    let seen = db
        .query_row(
            "SELECT 1 FROM billing_receipts WHERE thread_id=?1 AND response_id=?2",
            params![key.thread_id, key.response_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if seen {
        return Ok(Decision::Seen);
    }
    let state: Option<(bool, Option<i64>)> = db
        .query_row(
            "SELECT ready,legacy_watermark FROM billing_threads WHERE thread_id=?1",
            [&key.thread_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((ready, watermark)) = state {
        return Ok(
            if ready || watermark.is_some_and(|time| time > 0 && timestamp_ms > time) {
                Decision::Apply
            } else {
                Decision::Unknown
            },
        );
    }
    let has_sessions: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='sessions')",
        [],
        |row| row.get(0),
    )?;
    if !has_sessions {
        return Ok(Decision::Apply);
    }
    let prior: Option<(u64, u64)> = db.query_row(
        "SELECT
        COALESCE(json_extract(state,'$.cost.priced_requests'),0)+COALESCE(json_extract(state,'$.cost.unpriced_requests'),0),
        COALESCE(json_array_length(json_extract(state,'$.billing_seen')),0) FROM sessions WHERE id=?1",
        [&key.thread_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional()?;
    if prior.is_none() {
        let sources_exist: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='sources')",
            [],
            |row| row.get(0),
        )?;
        if sources_exist {
            let orphan: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM sources WHERE thread_id=?1)",
                [&key.thread_id],
                |row| row.get(0),
            )?;
            if orphan {
                return Ok(Decision::Unknown);
            }
        }
    }
    Ok(match prior {
        Some((0, 0)) | None => Decision::Apply,
        _ => Decision::Unknown,
    })
}

/// Caller uses its Session/source transaction; the savepoint also makes standalone batches atomic.
pub(crate) fn commit_new(db: &Connection, receipts: &[ReceiptKey]) -> Result<()> {
    db.execute_batch("SAVEPOINT billing_receipt_batch")?;
    let result = (|| -> Result<()> {
        for key in receipts {
            db.execute(
                "INSERT INTO billing_receipts(thread_id,response_id) VALUES(?1,?2)",
                params![key.thread_id, key.response_id],
            )?;
            db.execute(
                "INSERT INTO billing_threads(thread_id,ready,legacy_watermark) VALUES(?1,1,NULL)
                ON CONFLICT(thread_id) DO NOTHING",
                [&key.thread_id],
            )?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        db.execute_batch("ROLLBACK TO billing_receipt_batch; RELEASE billing_receipt_batch")?;
        return Err(error);
    }
    db.execute_batch("RELEASE billing_receipt_batch")?;
    Ok(())
}

pub(crate) fn partial_threads(db: &Connection) -> Result<Vec<String>> {
    let mut statement =
        db.prepare("SELECT thread_id FROM billing_threads WHERE ready=0 ORDER BY thread_id")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

/// Seed durable receipts from committed prefixes only; fees and source cursors are never rewritten.
pub(crate) fn migrate(db: &mut Connection) -> Result<Vec<SourceProblem>> {
    ensure_schema(db)?;
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let chats = {
        let mut statement = tx.prepare("SELECT id,
            COALESCE(json_extract(state,'$.cost.priced_requests'),0)+COALESCE(json_extract(state,'$.cost.unpriced_requests'),0),
            json_extract(state,'$.last_ms') FROM sessions")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, Option<i64>>(2)?,
            ))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    let mut problems = Vec::new();
    for (owner, expected, watermark) in chats {
        let sources = committed_source_prefixes(&tx, &owner)?;
        if prepare_thread_audit(&tx, &owner, expected, watermark)? {
            continue;
        }
        let billing_cutoff: Option<i64> = tx.query_row(
            "SELECT legacy_watermark FROM billing_threads WHERE thread_id=?1",
            [&owner],
            |row| row.get(0),
        )?;
        let mut complete = !sources.is_empty() || expected == 0;
        for (path, offset) in sources {
            tx.execute_batch("SAVEPOINT billing_seed_source")?;
            match seed_source(&tx, &path, offset, &owner, billing_cutoff) {
                Ok(()) => tx.execute_batch("RELEASE billing_seed_source")?,
                Err(problem) => {
                    tx.execute_batch(
                        "ROLLBACK TO billing_seed_source; RELEASE billing_seed_source",
                    )?;
                    problems.push(problem);
                    complete = false;
                }
            }
        }
        let actual: u64 = tx.query_row(
            "SELECT COUNT(*) FROM billing_receipts WHERE thread_id=?1",
            [&owner],
            |row| row.get(0),
        )?;
        let missing_recent = recent_receipts_missing(&tx, &owner)?;
        if missing_recent {
            complete = false;
            problems.push(SourceProblem {
                path: PathBuf::new(),
                offset: 0,
                message: format!("会话 {owner} 的已知近期响应未出现在已验证来源中"),
            });
        }
        if complete && actual != expected {
            complete = false;
            problems.push(SourceProblem {
                path: PathBuf::new(),
                offset: 0,
                message: format!(
                    "会话 {owner} 已计费次数 {expected} 与唯一响应 {actual} 不一致，保留原费用并标记未验证"
                ),
            });
        }
        tx.execute(
            "UPDATE billing_threads SET ready=?2 WHERE thread_id=?1",
            params![owner, complete],
        )?;
        remember_thread_audit(&tx, &owner, expected)?;
    }
    tx.execute(
        "INSERT OR IGNORE INTO billing_threads(thread_id,ready,legacy_watermark)
        SELECT DISTINCT thread_id,0,NULL FROM sources WHERE thread_id<>''",
        [],
    )?;
    tx.commit()?;
    Ok(problems)
}

/// Financial holds may move the live cursor backwards; previously committed proof never shrinks.
fn committed_source_prefixes(db: &Connection, owner: &str) -> Result<Vec<(PathBuf, u64)>> {
    db.execute(
        "INSERT INTO billing_source_prefixes(owner,path,cutoff)
        SELECT ?1,path,offset FROM sources WHERE thread_id=?1
        ON CONFLICT(owner,path) DO UPDATE SET cutoff=MAX(billing_source_prefixes.cutoff,excluded.cutoff)",
        [owner],
    )?;
    let mut statement =
        db.prepare("SELECT path,cutoff FROM billing_source_prefixes WHERE owner=?1 ORDER BY path")?;
    let rows = statement.query_map([owner], |row| {
        Ok((
            PathBuf::from(row.get::<_, String>(0)?),
            row.get::<_, u64>(1)?,
        ))
    })?;
    Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
}

fn recent_receipts_missing(db: &Connection, owner: &str) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(
        SELECT 1 FROM sessions s,json_each(s.state,'$.billing_seen') recent
        WHERE s.id=?1 AND NOT EXISTS(SELECT 1 FROM billing_receipts r
            WHERE r.thread_id=s.id AND r.response_id=recent.value))",
        [owner],
        |row| row.get(0),
    )?)
}

fn missing_receipt_ids(db: &Connection, owner: &str) -> Result<BTreeSet<String>> {
    let mut statement = db.prepare(
        "SELECT recent.value FROM sessions s,json_each(s.state,'$.billing_seen') recent
        WHERE s.id=?1 AND NOT EXISTS(SELECT 1 FROM billing_receipts r
            WHERE r.thread_id=s.id AND r.response_id=recent.value)",
    )?;
    let rows = statement.query_map([owner], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<std::result::Result<BTreeSet<_>, _>>()?)
}

fn receipt_count(db: &Connection, owner: &str) -> Result<u64> {
    Ok(db.query_row(
        "SELECT COUNT(*) FROM billing_receipts WHERE thread_id=?1",
        [owner],
        |row| row.get(0),
    )?)
}

/// Record the post-seeding baseline, so equal growth in fees/receipts never slides a partial watermark.
fn remember_thread_audit(db: &Connection, owner: &str, expected: u64) -> Result<()> {
    let receipts = receipt_count(db, owner)?;
    let missing_ids = serde_json::to_string(&missing_receipt_ids(db, owner)?)?;
    db.execute(
        "UPDATE billing_threads SET audited_cost_count=?2,audited_receipt_count=?3,audited_missing_ids=?4
        WHERE thread_id=?1",
        params![owner, expected, receipts, missing_ids],
    )?;
    Ok(())
}

struct ThreadAudit {
    ready: bool,
    cost: Option<u64>,
    receipts: Option<u64>,
    missing_ids: Option<String>,
}

/// A missing receipt baseline cannot distinguish old writes; later unexplained deltas reopen the audit.
fn prepare_thread_audit(
    db: &Connection,
    owner: &str,
    expected: u64,
    watermark: Option<i64>,
) -> Result<bool> {
    let audit = db.query_row(
        "SELECT ready,audited_cost_count,audited_receipt_count,audited_missing_ids FROM billing_threads WHERE thread_id=?1",
        [owner],
        |row| Ok(ThreadAudit {
            ready: row.get(0)?,
            cost: row.get(1)?,
            receipts: row.get(2)?,
            missing_ids: row.get(3)?,
        }),
    ).optional()?;
    let receipts = receipt_count(db, owner)?;
    let missing = missing_receipt_ids(db, owner)?;
    if let Some(audit) = audit {
        let ready = audit.ready;
        let old_cost = audit.cost;
        let old_receipts = audit.receipts;
        let old_missing = audit.missing_ids;
        if ready && receipts == expected && missing.is_empty() {
            remember_thread_audit(db, owner, expected)?;
            return Ok(true);
        }
        let old_ids =
            old_missing.and_then(|text| serde_json::from_str::<BTreeSet<String>>(&text).ok());
        let changed_delta = old_cost
            .zip(old_receipts)
            .is_none_or(|(cost, receipts_before)| {
                expected as i128 - receipts as i128 != cost as i128 - receipts_before as i128
            });
        let new_missing = old_ids
            .as_ref()
            .is_none_or(|before| !missing.is_subset(before));
        if ready || changed_delta || new_missing {
            db.execute(
                "UPDATE billing_threads SET ready=0,legacy_watermark=CASE
                WHEN ?2 IS NULL THEN NULL ELSE MAX(COALESCE(legacy_watermark,?2),?2) END
                WHERE thread_id=?1",
                params![owner, watermark.filter(|time| *time > 0)],
            )?;
        }
    } else {
        db.execute(
            "INSERT INTO billing_threads(thread_id,ready,legacy_watermark) VALUES(?1,0,?2)",
            params![owner, watermark.filter(|time| *time > 0)],
        )?;
    }
    Ok(false)
}

fn seed_source(
    db: &Connection,
    path: &Path,
    cutoff: u64,
    owner: &str,
    billing_cutoff: Option<i64>,
) -> std::result::Result<(), SourceProblem> {
    let mut position = 0;
    let result = (|| -> Result<()> {
        let file = File::open(path)?;
        if file.metadata()?.len() < cutoff {
            return Err("已提交来源前缀被截短".into());
        }
        if cutoff == 0 {
            return Ok(());
        }
        let initial_hash = prefix_hash(File::open(path)?, cutoff)?;
        let mut reader = BufReader::new(file.take(cutoff));
        let mut first = true;
        while let Some(line) = bounded_line(&mut reader)? {
            if line.iter().all(u8::is_ascii_whitespace) {
                position += line.len() as u64;
                continue;
            }
            let offset = position;
            let row: Value = serde_json::from_slice(&line)?;
            if first {
                if row["type"] != "session_meta" || row["payload"]["id"].as_str() != Some(owner) {
                    return Err("已提交来源的首条会话归属不一致".into());
                }
                first = false;
            }
            if let Some((key, timestamp_ms)) = record_key(owner, &row) {
                seed_committed_receipt(db, &key, timestamp_ms, billing_cutoff)?;
            }
            position += line.len() as u64;
            if position <= offset {
                return Err("来源游标没有前进".into());
            }
        }
        if first {
            return Err("已提交来源没有有效会话元数据".into());
        }
        if position != cutoff {
            return Err("无法完整读取已提交来源前缀".into());
        }
        if initial_hash != prefix_hash(File::open(path)?, cutoff)? {
            return Err("迁移期间已提交来源前缀发生修改".into());
        }
        Ok(())
    })();
    result.map_err(|error| SourceProblem {
        path: path.to_path_buf(),
        offset: position,
        message: error.to_string(),
    })
}

/// An old byte cutoff cannot turn a later live response in replacement content into an already-paid ID.
fn seed_committed_receipt(
    db: &Connection,
    key: &ReceiptKey,
    timestamp_ms: i64,
    billing_cutoff: Option<i64>,
) -> Result<()> {
    let seen = db
        .query_row(
            "SELECT 1 FROM billing_receipts WHERE thread_id=?1 AND response_id=?2",
            params![key.thread_id, key.response_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if seen {
        return Ok(());
    }
    if billing_cutoff.is_some_and(|cutoff| cutoff > 0 && timestamp_ms > cutoff) {
        return Err("来源旧前缀包含晚于旧计费水位的新响应，不能补记为已收费".into());
    }
    db.execute(
        "INSERT INTO billing_receipts(thread_id,response_id) VALUES(?1,?2)",
        params![key.thread_id, key.response_id],
    )?;
    Ok(())
}

fn bounded_line(reader: &mut impl BufRead) -> Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            if line.is_empty() {
                return Ok(None);
            }
            return Err("已提交来源游标位于未完成的 JSONL 行内".into());
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let count = newline.map_or(available.len(), |at| at + 1);
        if line.len() + count > 8 * 1024 * 1024 {
            return Err("来源含超过8MiB的未验证记录".into());
        }
        line.extend_from_slice(&available[..count]);
        reader.consume(count);
        if newline.is_some() {
            return Ok(Some(line));
        }
    }
}

fn prefix_hash(file: File, limit: u64) -> Result<u64> {
    use std::hash::Hasher;
    let mut reader = file.take(limit);
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    let mut buffer = [0u8; 32 * 1024];
    let mut read = 0;
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.write(&buffer[..count]);
        read += count as u64;
    }
    if read != limit {
        return Err("来源前缀读取不足".into());
    }
    Ok(hash.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn db() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE sessions(id TEXT PRIMARY KEY,last_ms INTEGER,state TEXT);
            CREATE TABLE sources(path TEXT PRIMARY KEY,offset INTEGER,thread_id TEXT);",
        )
        .unwrap();
        ensure_schema(&db).unwrap();
        db
    }

    fn key(id: &str) -> ReceiptKey {
        ReceiptKey {
            thread_id: "chat".into(),
            response_id: id.into(),
        }
    }

    // Durable identity is independent of the model's 2048-entry recent-ID window and connection lifetime.
    #[test]
    fn durable_receipts_exceed_recent_memory_and_survive_reopen() {
        let path = std::env::temp_dir().join(format!(
            "profiler-receipt-{}-{}.sqlite",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let db = Connection::open(&path).unwrap();
        ensure_schema(&db).unwrap();
        let keys: Vec<_> = (0..2050).map(|index| key(&format!("r{index}"))).collect();
        commit_new(&db, &keys).unwrap();
        drop(db);
        let db = Connection::open(&path).unwrap();
        assert_eq!(decision(&db, &key("r0"), 1).unwrap(), Decision::Seen);
        assert_eq!(decision(&db, &key("r2049"), 1).unwrap(), Decision::Seen);
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM billing_receipts", [], |r| r
                .get::<_, u64>(0))
                .unwrap(),
            2050
        );
        drop(db);
        std::fs::remove_file(path).unwrap();
    }

    // A duplicate batch claim and an outer source-transaction rollback leave no partial receipt writes.
    #[test]
    fn failed_batch_or_outer_commit_keeps_receipts_atomic() {
        let mut db = db();
        commit_new(&db, &[key("existing")]).unwrap();
        assert!(commit_new(&db, &[key("new"), key("existing")]).is_err());
        assert_eq!(decision(&db, &key("new"), 1).unwrap(), Decision::Apply);
        let tx = db.transaction().unwrap();
        commit_new(&tx, &[key("rolled-back")]).unwrap();
        tx.rollback().unwrap();
        assert_eq!(
            decision(&db, &key("rolled-back"), 1).unwrap(),
            Decision::Apply
        );
    }

    // Seeding old canonical prefixes changes neither accumulated dollars nor the committed source cursor.
    #[test]
    fn migration_preserves_cost_and_cursor() {
        let mut db = db();
        let path = std::env::temp_dir().join(format!(
            "profiler-source-{}-{}.jsonl",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let source = format!(
            "{}\n{}\n",
            json!({"type":"session_meta","payload":{"id":"chat"}}),
            json!({"type":"token_usage_record","payload":{"thread_id":"chat","response_id":"old"}})
        );
        std::fs::write(&path, &source).unwrap();
        let state=json!({"last_ms":1000,"cost":{"priced_requests":1,"unpriced_requests":0,"output_usd":12.5}}).to_string();
        db.execute("INSERT INTO sessions VALUES('chat',1000,?1)", [&state])
            .unwrap();
        db.execute(
            "INSERT INTO sources VALUES(?1,?2,'chat')",
            params![path.to_string_lossy(), source.len()],
        )
        .unwrap();
        assert!(migrate(&mut db).unwrap().is_empty());
        assert_eq!(decision(&db, &key("old"), 1).unwrap(), Decision::Seen);
        assert_eq!(decision(&db, &key("new"), 1).unwrap(), Decision::Apply);
        assert_eq!(
            db.query_row("SELECT state FROM sessions", [], |r| r.get::<_, String>(0))
                .unwrap(),
            state
        );
        assert_eq!(
            db.query_row("SELECT offset FROM sources", [], |r| r.get::<_, u64>(0))
                .unwrap(),
            source.len() as u64
        );
        std::fs::remove_file(path).unwrap();
    }

    // Missing old sources keep a fixed uncertainty watermark while later live requests can still progress.
    #[test]
    fn missing_source_blocks_old_replay_but_not_new_live_usage() {
        let mut db = db();
        let path = std::env::temp_dir().join("profiler-deliberately-missing-billing-source.jsonl");
        db.execute(
            "INSERT INTO sessions VALUES('chat',1000,?1)",
            [json!({"last_ms":1000,"cost":{"priced_requests":1}}).to_string()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO sources VALUES(?1,10,'chat')",
            [path.to_string_lossy().as_ref()],
        )
        .unwrap();
        let issues = migrate(&mut db).unwrap();
        assert!(!issues.is_empty());
        assert_eq!(decision(&db, &key("old"), 900).unwrap(), Decision::Unknown);
        assert_eq!(decision(&db, &key("new"), 1100).unwrap(), Decision::Apply);
        commit_new(&db, &[key("new")]).unwrap();
        db.execute(
            "UPDATE sessions SET state=?1",
            [json!({"last_ms":5000,"cost":{"priced_requests":2}}).to_string()],
        )
        .unwrap();
        migrate(&mut db).unwrap();
        assert_eq!(
            decision(&db, &key("after-cutover"), 1200).unwrap(),
            Decision::Apply
        );
        assert_eq!(partial_threads(&db).unwrap(), vec!["chat"]);
    }

    // Metadata-only batches are a genuinely empty baseline, not unverified prior model charges.
    #[test]
    fn metadata_first_can_charge_first_canonical_later() {
        let db = db();
        db.execute(
            "INSERT INTO sessions VALUES('chat',1000,?1)",
            [
                json!({"cost":{"priced_requests":0,"unpriced_requests":0},"billing_seen":[]})
                    .to_string(),
            ],
        )
        .unwrap();
        assert_eq!(decision(&db, &key("first"), 1001).unwrap(), Decision::Apply);
        commit_new(&db, &[key("first")]).unwrap();
        assert_eq!(decision(&db, &key("first"), 1001).unwrap(), Decision::Seen);
    }

    // Financial source identity must exclude copied parent rows and preserve canonical event timestamps.
    #[test]
    fn record_identity_and_timestamp_are_owner_scoped() {
        let row = json!({"type":"token_usage_record","timestamp":"2026-11-14T00:00:00Z",
            "payload":{"thread_id":"chat","response_id":"r1"}});
        let (id, time) = record_key("chat", &row).unwrap();
        assert_eq!(id, key("r1"));
        assert!(time > 0);
        assert!(record_key("child", &row).is_none());
    }

    // Receipt backfill shares the source reader's whitespace rule and still validates the first actual record.
    #[test]
    fn migration_skips_whitespace_without_losing_committed_position() {
        let mut db = db();
        let path = std::env::temp_dir().join(format!(
            "profiler-spaced-source-{}-{}.jsonl",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let source = format!(
            "\n \t\r\n{}\n\n   \n{}\n\t\n",
            json!({"type":"session_meta","payload":{"id":"chat"}}),
            json!({"type":"token_usage_record","payload":{"thread_id":"chat","response_id":"old"}})
        );
        std::fs::write(&path, &source).unwrap();
        db.execute(
            "INSERT INTO sessions VALUES('chat',1000,?1)",
            [json!({"last_ms":1000,"cost":{"priced_requests":1}}).to_string()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO sources VALUES(?1,?2,'chat')",
            params![path.to_string_lossy(), source.len()],
        )
        .unwrap();
        assert!(migrate(&mut db).unwrap().is_empty());
        assert_eq!(decision(&db, &key("old"), 1).unwrap(), Decision::Seen);
        assert_eq!(
            db.query_row("SELECT offset FROM sources", [], |r| r.get::<_, u64>(0))
                .unwrap(),
            source.len() as u64
        );
        std::fs::remove_file(path).unwrap();
    }

    fn legacy_source_fixture(db: &Connection) -> (PathBuf, String) {
        let path = std::env::temp_dir().join(format!(
            "profiler-downgrade-{}-{}.jsonl",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let source = format!(
            "{}\n{}\n",
            json!({"type":"session_meta","payload":{"id":"chat"}}),
            json!({"type":"token_usage_record","payload":{"thread_id":"chat","response_id":"old"}})
        );
        std::fs::write(&path, &source).unwrap();
        db.execute(
            "INSERT INTO sessions VALUES('chat',1000,?1)",
            [json!({"last_ms":1000,"cost":{"priced_requests":1,"output_usd":2.0}}).to_string()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO sources VALUES(?1,?2,'chat')",
            params![path.to_string_lossy(), source.len()],
        )
        .unwrap();
        (path, source)
    }

    // Returning from v0.2.5 must seed newly charged IDs despite an earlier successful receipt migration.
    #[test]
    fn downgrade_new_charges_reopen_receipt_audit_without_recharging() {
        let mut db = db();
        let (path, mut source) = legacy_source_fixture(&db);
        assert!(migrate(&mut db).unwrap().is_empty());
        source.push_str(&format!("{}\n",json!({"type":"token_usage_record","payload":{"thread_id":"chat","response_id":"legacy-added"}})));
        std::fs::write(&path, &source).unwrap();
        let state =
            json!({"last_ms":2000,"cost":{"priced_requests":2,"output_usd":5.0}}).to_string();
        db.execute("UPDATE sessions SET last_ms=2000,state=?1", [&state])
            .unwrap();
        db.execute("UPDATE sources SET offset=?1", [source.len() as u64])
            .unwrap();
        assert!(migrate(&mut db).unwrap().is_empty());
        assert_eq!(
            decision(&db, &key("legacy-added"), 1500).unwrap(),
            Decision::Seen
        );
        assert_eq!(
            db.query_row("SELECT state FROM sessions", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            state
        );
        assert_eq!(
            db.query_row("SELECT offset FROM sources", [], |row| row.get::<_, u64>(0))
                .unwrap(),
            source.len() as u64
        );
        assert!(partial_threads(&db).unwrap().is_empty());
        std::fs::remove_file(path).unwrap();
    }

    // Missing source after a downgrade establishes a new fixed unknown-history boundary, not blind Apply.
    #[test]
    fn downgrade_missing_source_renews_cutover_once() {
        let mut db = db();
        let (path, _) = legacy_source_fixture(&db);
        assert!(migrate(&mut db).unwrap().is_empty());
        std::fs::remove_file(&path).unwrap();
        db.execute(
            "UPDATE sessions SET last_ms=2000,state=?1",
            [json!({"last_ms":2000,"cost":{"priced_requests":2}}).to_string()],
        )
        .unwrap();
        db.execute("UPDATE sources SET offset=offset+100", [])
            .unwrap();
        assert!(!migrate(&mut db).unwrap().is_empty());
        assert_eq!(decision(&db, &key("old"), 1).unwrap(), Decision::Seen);
        assert_eq!(
            decision(&db, &key("legacy-added"), 1500).unwrap(),
            Decision::Unknown
        );
        assert_eq!(
            decision(&db, &key("new-live"), 2100).unwrap(),
            Decision::Apply
        );
        commit_new(&db, &[key("new-live")]).unwrap();
        db.execute(
            "UPDATE sessions SET last_ms=5000,state=?1",
            [json!({"last_ms":5000,"cost":{"priced_requests":3}}).to_string()],
        )
        .unwrap();
        migrate(&mut db).unwrap();
        assert_eq!(
            decision(&db, &key("newer-live"), 2200).unwrap(),
            Decision::Apply
        );
        assert_eq!(partial_threads(&db).unwrap(), vec!["chat"]);
        assert_eq!(
            db.query_row(
                "SELECT legacy_watermark FROM billing_threads WHERE thread_id='chat'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            2000
        );
    }

    // v0.2.5 truncation may reset its count to the same value while changing the actual charged IDs.
    #[test]
    fn equal_count_with_changed_recent_ids_is_not_a_verified_fast_path() {
        let mut db = db();
        let (path, _) = legacy_source_fixture(&db);
        assert!(migrate(&mut db).unwrap().is_empty());
        let source = format!(
            "{}\n{}\n",
            json!({"type":"session_meta","payload":{"id":"chat"}}),
            json!({"type":"token_usage_record","payload":{"thread_id":"chat","response_id":"replacement"}})
        );
        std::fs::write(&path, &source).unwrap();
        db.execute(
            "UPDATE sessions SET last_ms=2000,state=?1",
            [
                json!({"last_ms":2000,"cost":{"priced_requests":1},"billing_seen":["replacement"]})
                    .to_string(),
            ],
        )
        .unwrap();
        db.execute("UPDATE sources SET offset=?1", [source.len() as u64])
            .unwrap();
        assert!(!migrate(&mut db).unwrap().is_empty());
        assert_eq!(
            decision(&db, &key("replacement"), 1500).unwrap(),
            Decision::Seen
        );
        assert_eq!(partial_threads(&db).unwrap(), vec!["chat"]);
        std::fs::remove_file(path).unwrap();
    }

    fn partial_legacy_chat(db: &mut Connection) {
        db.execute(
            "INSERT INTO sessions VALUES('chat',1000,?1)",
            [
                json!({"last_ms":1000,"cost":{"priced_requests":1},"billing_seen":["missing-old"]})
                    .to_string(),
            ],
        )
        .unwrap();
        db.execute(
            "INSERT INTO sources VALUES('deliberately-missing-partial-audit.jsonl',100,'chat')",
            [],
        )
        .unwrap();
        assert!(!migrate(db).unwrap().is_empty());
    }

    // A second old-binary write is unsafe even if the first audit was already partial and old IDs were evicted.
    #[test]
    fn downgrade_while_already_partial_renews_legacy_boundary() {
        let mut db = db();
        partial_legacy_chat(&mut db);
        let recent: Vec<_> = (3..2051).map(|index| format!("legacy-{index}")).collect();
        let state = json!({"last_ms":4000,"cost":{"priced_requests":2051},"billing_seen":recent})
            .to_string();
        db.execute("UPDATE sessions SET last_ms=4000,state=?1", [&state])
            .unwrap();
        db.execute("UPDATE sources SET offset=offset+1000", [])
            .unwrap();
        migrate(&mut db).unwrap();
        assert_eq!(
            decision(&db, &key("legacy-1"), 1500).unwrap(),
            Decision::Unknown
        );
        assert_eq!(decision(&db, &key("fresh"), 4100).unwrap(), Decision::Apply);
        assert_eq!(
            db.query_row(
                "SELECT legacy_watermark FROM billing_threads WHERE thread_id='chat'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            4000
        );
        assert_eq!(
            db.query_row("SELECT state FROM sessions WHERE id='chat'", [], |r| r
                .get::<_, String>(
                0
            ))
            .unwrap(),
            state
        );
    }

    // Normal atomic cost/receipt growth under a partial historical audit keeps the original fixed boundary.
    #[test]
    fn normal_live_growth_does_not_slide_partial_boundary_on_restart() {
        let mut db = db();
        partial_legacy_chat(&mut db);
        for (index, last_ms) in [(1, 3000), (2, 6000)] {
            let tx = db.transaction().unwrap();
            commit_new(&tx, &[key(&format!("live-{index}"))]).unwrap();
            let recent = vec!["missing-old".to_owned(), format!("live-{index}")];
            tx.execute("UPDATE sessions SET last_ms=?1,state=?2",params![last_ms,
                json!({"last_ms":last_ms,"cost":{"priced_requests":1+index},"billing_seen":recent}).to_string()]).unwrap();
            tx.commit().unwrap();
            migrate(&mut db).unwrap();
            assert_eq!(
                db.query_row(
                    "SELECT legacy_watermark FROM billing_threads WHERE thread_id='chat'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
                1000
            );
            assert_eq!(
                decision(&db, &key("delayed-live"), 1500).unwrap(),
                Decision::Apply
            );
        }
    }

    // Equal request counts still reveal a legacy replacement when a new unpaid recent ID appears.
    #[test]
    fn changed_recent_ids_reaudit_an_already_partial_chat() {
        let mut db = db();
        partial_legacy_chat(&mut db);
        db.execute("UPDATE sessions SET last_ms=4000,state=?1",
            [json!({"last_ms":4000,"cost":{"priced_requests":1},"billing_seen":["replacement-paid"]}).to_string()]).unwrap();
        migrate(&mut db).unwrap();
        assert_eq!(
            decision(&db, &key("replacement-paid"), 1500).unwrap(),
            Decision::Unknown
        );
        assert_eq!(
            db.query_row(
                "SELECT legacy_watermark FROM billing_threads WHERE thread_id='chat'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            4000
        );
    }

    // Old receipt/thread rows survive the additive schema upgrade; a missing audit baseline is conservative once.
    #[test]
    fn old_partial_schema_is_upgraded_without_dropping_receipts() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE sessions(id TEXT PRIMARY KEY,last_ms INTEGER,state TEXT);
            CREATE TABLE sources(path TEXT PRIMARY KEY,offset INTEGER,thread_id TEXT);
            CREATE TABLE billing_receipts(thread_id TEXT,response_id TEXT,PRIMARY KEY(thread_id,response_id));
            CREATE TABLE billing_threads(thread_id TEXT PRIMARY KEY,ready INTEGER NOT NULL,legacy_watermark INTEGER);
            INSERT INTO billing_receipts VALUES('chat','known');
            INSERT INTO billing_threads VALUES('chat',0,1000);").unwrap();
        db.execute("INSERT INTO sessions VALUES('chat',4000,?1)",
            [json!({"last_ms":4000,"cost":{"priced_requests":2},"billing_seen":["known","unknown-old"]}).to_string()]).unwrap();
        migrate(&mut db).unwrap();
        assert_eq!(decision(&db, &key("known"), 500).unwrap(), Decision::Seen);
        assert_eq!(
            decision(&db, &key("unknown-old"), 1500).unwrap(),
            Decision::Unknown
        );
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM billing_receipts", [], |r| r
                .get::<_, u64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            db.query_row("SELECT legacy_watermark FROM billing_threads", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            4000
        );
    }

    // A temporary financial hold must not erase the old committed prefix needed to restore receipts.
    #[test]
    fn restored_source_reuses_original_committed_cutoff_after_hold() {
        let mut db = db();
        let path = std::env::temp_dir().join(format!(
            "profiler-prefix-restore-{}-{}.jsonl",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let meta = format!(
            "{}\n",
            json!({"type":"session_meta","payload":{"id":"chat"}})
        );
        let original = format!(
            "{meta}{}\n",
            json!({"type":"token_usage_record","payload":{"thread_id":"chat","response_id":"already-paid"}})
        );
        let state=json!({"last_ms":1000,"cost":{"priced_requests":1,"output_usd":7.0},"billing_seen":["already-paid"]}).to_string();
        db.execute("INSERT INTO sessions VALUES('chat',1000,?1)", [&state])
            .unwrap();
        db.execute(
            "INSERT INTO sources VALUES(?1,?2,'chat')",
            params![path.to_string_lossy(), original.len()],
        )
        .unwrap();
        assert!(!migrate(&mut db).unwrap().is_empty());
        db.execute("UPDATE sources SET offset=?1", [meta.len() as u64])
            .unwrap();
        // The live tail is intentionally malformed: the old audit must stop at its original L.
        std::fs::write(&path, format!("{original}uncommitted live tail\n")).unwrap();
        assert!(migrate(&mut db).unwrap().is_empty());
        assert_eq!(
            decision(&db, &key("already-paid"), 500).unwrap(),
            Decision::Seen
        );
        assert!(partial_threads(&db).unwrap().is_empty());
        assert_eq!(
            db.query_row("SELECT cutoff FROM billing_source_prefixes", [], |row| row
                .get::<_, u64>(
                0
            ))
            .unwrap(),
            original.len() as u64
        );
        assert_eq!(
            db.query_row("SELECT offset FROM sources", [], |row| row.get::<_, u64>(0))
                .unwrap(),
            meta.len() as u64
        );
        assert_eq!(
            db.query_row("SELECT state FROM sessions", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            state
        );
        std::fs::remove_file(path).unwrap();
    }

    // Restoring only a shortened prefix cannot make the retained longer committed proof disappear.
    #[test]
    fn truncated_restore_keeps_original_prefix_unverified() {
        let mut db = db();
        let path = std::env::temp_dir().join(format!(
            "profiler-prefix-short-{}-{}.jsonl",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let meta = format!(
            "{}\n",
            json!({"type":"session_meta","payload":{"id":"chat"}})
        );
        let cutoff = meta.len() as u64 + 100;
        db.execute(
            "INSERT INTO sessions VALUES('chat',1000,?1)",
            [json!({"last_ms":1000,"cost":{"priced_requests":1}}).to_string()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO sources VALUES(?1,?2,'chat')",
            params![path.to_string_lossy(), cutoff],
        )
        .unwrap();
        migrate(&mut db).unwrap();
        db.execute("UPDATE sources SET offset=?1", [meta.len() as u64])
            .unwrap();
        std::fs::write(&path, &meta).unwrap();
        assert!(!migrate(&mut db).unwrap().is_empty());
        assert_eq!(partial_threads(&db).unwrap(), vec!["chat"]);
        assert_eq!(
            db.query_row("SELECT cutoff FROM billing_source_prefixes", [], |row| row
                .get::<_, u64>(
                0
            ))
            .unwrap(),
            cutoff
        );
        assert_eq!(
            decision(&db, &key("unknown-old"), 500).unwrap(),
            Decision::Unknown
        );
        std::fs::remove_file(path).unwrap();
    }

    // Restoring same-owner/same-size live content must not seed a post-watermark response as paid history.
    #[test]
    fn replacement_live_response_after_watermark_is_not_seeded_seen() {
        let mut db = db();
        let path = std::env::temp_dir().join(format!(
            "profiler-replacement-live-{}-{}.jsonl",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let meta = format!(
            "{}\n",
            json!({"type":"session_meta","timestamp_ms":1,"payload":{"id":"chat"}})
        );
        let canonical = |id, time| {
            format!(
                "{}\n",
                json!({"type":"token_usage_record","timestamp_ms":time,
            "payload":{"thread_id":"chat","response_id":id}})
            )
        };
        let old = format!("{meta}{}", canonical("paid", 1000));
        let replacement = format!("{meta}{}", canonical("live", 2000));
        assert_eq!(old.len(), replacement.len());
        let state = json!({"last_ms":1000,"cost":{"priced_requests":1},"billing_seen":["paid"]})
            .to_string();
        db.execute("INSERT INTO sessions VALUES('chat',1000,?1)", [&state])
            .unwrap();
        db.execute(
            "INSERT INTO sources VALUES(?1,?2,'chat')",
            params![path.to_string_lossy(), old.len()],
        )
        .unwrap();
        migrate(&mut db).unwrap();
        std::fs::write(&path, &replacement).unwrap();
        let problems = migrate(&mut db).unwrap();
        assert!(problems
            .iter()
            .any(|problem| problem.message.contains("计费水位")));
        assert_eq!(decision(&db, &key("live"), 2000).unwrap(), Decision::Apply);
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM billing_receipts", [], |row| row
                .get::<_, u64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            db.query_row("SELECT state FROM sessions", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            state
        );
        commit_new(&db, &[key("live")]).unwrap();
        assert_eq!(decision(&db, &key("live"), 2000).unwrap(), Decision::Seen);
        std::fs::remove_file(path).unwrap();
    }

    // Future live receipts already committed with financial state remain valid across historical backfill.
    #[test]
    fn already_paid_live_receipt_above_watermark_is_not_rejected() {
        let mut db = db();
        let path = std::env::temp_dir().join(format!(
            "profiler-known-live-{}-{}.jsonl",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        let meta = format!(
            "{}\n",
            json!({"type":"session_meta","payload":{"id":"chat"}})
        );
        let old = format!(
            "{meta}{}\n",
            json!({"type":"token_usage_record","timestamp_ms":1000,
            "payload":{"thread_id":"chat","response_id":"paid"}})
        );
        db.execute(
            "INSERT INTO sessions VALUES('chat',1000,?1)",
            [
                json!({"last_ms":1000,"cost":{"priced_requests":1},"billing_seen":["paid"]})
                    .to_string(),
            ],
        )
        .unwrap();
        db.execute(
            "INSERT INTO sources VALUES(?1,?2,'chat')",
            params![path.to_string_lossy(), old.len()],
        )
        .unwrap();
        migrate(&mut db).unwrap();
        commit_new(&db, &[key("live")]).unwrap();
        let combined = format!(
            "{old}{}\n",
            json!({"type":"token_usage_record","timestamp_ms":2000,
            "payload":{"thread_id":"chat","response_id":"live"}})
        );
        std::fs::write(&path, &combined).unwrap();
        db.execute("UPDATE sources SET offset=?1", [combined.len() as u64])
            .unwrap();
        db.execute(
            "UPDATE sessions SET last_ms=3000,state=?1",
            [
                json!({"last_ms":3000,"cost":{"priced_requests":2},"billing_seen":["paid","live"]})
                    .to_string(),
            ],
        )
        .unwrap();
        assert!(migrate(&mut db).unwrap().is_empty());
        assert_eq!(decision(&db, &key("paid"), 1000).unwrap(), Decision::Seen);
        assert_eq!(decision(&db, &key("live"), 2000).unwrap(), Decision::Seen);
        assert!(partial_threads(&db).unwrap().is_empty());
        assert_eq!(
            db.query_row(
                "SELECT legacy_watermark FROM billing_threads WHERE thread_id='chat'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            1000
        );
        std::fs::remove_file(path).unwrap();
    }
}
