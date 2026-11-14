use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, Metadata};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum IssueKind {
    InvalidRecord,
    UnverifiedBilling,
    TruncatedHistory,
}

/// A failed source remains partial after restart; repeated unchanged files do not create a retry loop.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct SourceIssue {
    pub kind: IssueKind,
    pub offset: u64,
    pub message: String,
    pub length: u64,
    pub modified_ns: Option<u64>,
    #[serde(default)]
    pub missing_source: bool,
    #[serde(default)]
    pub history_gap: bool,
    #[serde(default)]
    pub deferred_offset: Option<u64>,
    #[serde(default)]
    pub tail_offset: Option<u64>,
}

impl SourceIssue {
    /// Preserve held history and progress while the original log is temporarily unavailable.
    pub fn missing(previous: Option<&Self>) -> Self {
        Self {
            kind: IssueKind::UnverifiedBilling,
            offset: 0,
            message: "历史日志暂不可用，已保留已采集的数据。未恢复的记录仍标为待核对。".into(),
            length: previous.map_or(0, |issue| issue.length),
            modified_ns: previous.and_then(|issue| issue.modified_ns),
            missing_source: true,
            history_gap: previous.is_some_and(|issue| issue.history_gap),
            deferred_offset: previous.and_then(|issue| issue.deferred_offset),
            tail_offset: previous.and_then(|issue| issue.tail_offset),
        }
    }

    pub fn new(kind: IssueKind, offset: u64, message: String, metadata: &Metadata) -> Self {
        Self {
            history_gap: kind == IssueKind::TruncatedHistory,
            kind,
            offset,
            message,
            length: metadata.len(),
            modified_ns: modified_ns(metadata),
            missing_source: false,
            deferred_offset: None,
            tail_offset: None,
        }
    }

    /// Billing proof may become available without editing this file; other failures wait for a change.
    pub fn should_retry(&self, metadata: &Metadata) -> bool {
        self.kind == IssueKind::UnverifiedBilling
            || self.length != metadata.len()
            || self.modified_ns.is_none()
            || self.modified_ns != modified_ns(metadata)
    }

    /// Repaired or replaced content may have shifted earlier byte positions; receipts make replay safe.
    pub fn retry_offset(&self, committed: u64, metadata: &Metadata) -> u64 {
        if self.missing_source {
            return 0;
        }
        match self.kind {
            IssueKind::InvalidRecord if metadata.len() <= self.length => 0,
            IssueKind::UnverifiedBilling
                if metadata.len() >= self.tail_offset.unwrap_or(u64::MAX)
                    && (metadata.len() > self.length
                        || self.modified_ns == modified_ns(metadata)) =>
            {
                self.tail_offset.unwrap_or(committed)
            }
            IssueKind::InvalidRecord | IssueKind::UnverifiedBilling => committed.min(self.offset),
            IssueKind::TruncatedHistory => committed,
        }
    }

    pub fn repaired(&self) -> Option<Self> {
        self.history_gap.then(|| Self {
            kind: IssueKind::TruncatedHistory,
            missing_source: false,
            message: "来源历史曾被截短，已保留可确认的费用".into(),
            ..self.clone()
        })
    }
}

/// This small fingerprint detects common source replacement, not adversarial file tampering.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct SourceStamp {
    pub read_offset: u64,
    pub length: u64,
    pub modified_ns: Option<u64>,
    pub created_ns: Option<u64>,
    pub anchor_hash: u64,
}

fn modified_ns(metadata: &Metadata) -> Option<u64> {
    metadata.modified().ok().and_then(time_ns)
}

fn created_ns(metadata: &Metadata) -> Option<u64> {
    metadata.created().ok().and_then(time_ns)
}

fn time_ns(time: SystemTime) -> Option<u64> {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_nanos()).ok())
}

/// Hash at most 512 committed bytes, so an append check has constant I/O cost.
fn anchor_hash(file: &mut File, read_offset: u64) -> Result<u64> {
    let start = read_offset.saturating_sub(512);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = [0u8; 512];
    let count = (read_offset - start) as usize;
    file.read_exact(&mut bytes[..count])?;
    // Fixed FNV-1a stays stable across toolchain upgrades; this is not a security hash.
    let mut hash = 0xcbf29ce484222325u64;
    for byte in &bytes[..count] {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3);
    }
    Ok(hash)
}

/// The caller captures before committing and saves with its cursor/session transaction.
pub(crate) fn capture_stamp(path: &Path, read_offset: u64) -> Result<SourceStamp> {
    let mut file = File::open(path)?;
    capture_file_stamp(&mut file, read_offset)
}

/// Keep the opened file's identity when its pathname is replaced during a read.
pub(crate) fn capture_file_stamp(file: &mut File, read_offset: u64) -> Result<SourceStamp> {
    let metadata = file.metadata()?;
    if read_offset > metadata.len() {
        return Err("来源在提交游标前被截短，请重新读取".into());
    }
    Ok(SourceStamp {
        read_offset,
        length: metadata.len(),
        modified_ns: modified_ns(&metadata),
        created_ns: created_ns(&metadata),
        anchor_hash: anchor_hash(file, read_offset)?,
    })
}

/// Unchanged files need no read; growing files verify only the previous cursor's small anchor.
pub(crate) fn changed_prefix(
    path: &Path,
    previous: &SourceStamp,
    metadata: &Metadata,
) -> Result<bool> {
    let created = created_ns(metadata);
    if previous
        .created_ns
        .zip(created)
        .is_some_and(|(old, new)| old != new)
        || metadata.len() < previous.length
    {
        return Ok(true);
    }
    if metadata.len() == previous.length {
        return Ok(previous.modified_ns != modified_ns(metadata));
    }
    Ok(anchor_hash(&mut File::open(path)?, previous.read_offset)? != previous.anchor_hash)
}

/// Old source rows have no stamp until first initialized; their existing cursors remain valid.
pub(crate) fn load_stamp(db: &Connection, path: &Path) -> Result<Option<SourceStamp>> {
    let value: Option<Option<String>> = db
        .query_row(
            "SELECT stamp FROM sources WHERE path=?1",
            [path.to_string_lossy().as_ref()],
            |row| row.get(0),
        )
        .optional()?;
    value
        .flatten()
        .map(|text| serde_json::from_str(&text).map_err(Into::into))
        .transpose()
}

/// The caller supplies the same transaction used for the source cursor and financial receipts.
pub(crate) fn save_stamp(db: &Connection, path: &Path, stamp: Option<&SourceStamp>) -> Result<()> {
    let value = stamp.map(serde_json::to_string).transpose()?;
    db.execute(
        "UPDATE sources SET stamp=?2 WHERE path=?1",
        params![path.to_string_lossy(), value],
    )?;
    Ok(())
}

/// Add integrity state in place, retaining all existing source cursors.
pub(crate) fn ensure_schema(db: &mut Connection) -> Result<()> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let names = {
        let mut columns = tx.prepare("PRAGMA table_info(sources)")?;
        let rows = columns.query_map([], |row| row.get::<_, String>(1))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    if !names.iter().any(|name| name == "issue") {
        tx.execute_batch("ALTER TABLE sources ADD COLUMN issue TEXT")?;
    }
    if !names.iter().any(|name| name == "stamp") {
        tx.execute_batch("ALTER TABLE sources ADD COLUMN stamp TEXT")?;
    }
    tx.commit()?;
    Ok(())
}

pub(crate) fn load_issues(db: &Connection) -> Result<HashMap<PathBuf, SourceIssue>> {
    let mut statement = db.prepare("SELECT path,issue FROM sources WHERE issue IS NOT NULL")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut issues = HashMap::new();
    for row in rows {
        let (path, value) = row?;
        issues.insert(PathBuf::from(path), serde_json::from_str(&value)?);
    }
    Ok(issues)
}

/// The caller supplies its transaction so issue state and source progress commit together.
pub(crate) fn save_issue(db: &Connection, path: &Path, issue: Option<&SourceIssue>) -> Result<()> {
    let value = issue.map(serde_json::to_string).transpose()?;
    db.execute(
        "UPDATE sources SET issue=?2 WHERE path=?1",
        params![path.to_string_lossy(), value],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Disappearance preserves deferred billing work; recovery starts at metadata rather than a held live tail.
    #[test]
    fn missing_source_preserves_held_history_and_replays_from_zero() {
        let path = source_file("missing-held", b"restored\n");
        let metadata = path.metadata().unwrap();
        let mut previous =
            SourceIssue::new(IssueKind::UnverifiedBilling, 2, "pending".into(), &metadata);
        previous.history_gap = true;
        previous.deferred_offset = Some(2);
        previous.tail_offset = Some(9);
        let missing = SourceIssue::missing(Some(&previous));
        assert!(missing.missing_source);
        assert!(missing.history_gap);
        assert_eq!(missing.deferred_offset, Some(2));
        assert_eq!(missing.tail_offset, Some(9));
        assert_eq!(missing.retry_offset(2, &metadata), 0);
        let repaired = missing.repaired().unwrap();
        assert!(!repaired.missing_source);
        assert_eq!(repaired.kind, IssueKind::TruncatedHistory);
        std::fs::remove_file(path).unwrap();
    }

    // Adding the issue column and recording a failure must preserve an existing committed cursor.
    #[test]
    fn issue_schema_preserves_source_cursor_and_round_trips() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE sources(path TEXT PRIMARY KEY, offset INTEGER, thread_id TEXT);
            INSERT INTO sources VALUES('test',42,'chat');",
        )
        .unwrap();
        ensure_schema(&mut db).unwrap();
        ensure_schema(&mut db).unwrap();
        let issue = SourceIssue {
            kind: IssueKind::InvalidRecord,
            offset: 42,
            message: "invalid record".into(),
            length: 100,
            modified_ns: Some(1),
            missing_source: false,
            history_gap: false,
            deferred_offset: None,
            tail_offset: None,
        };
        save_issue(&db, Path::new("test"), Some(&issue)).unwrap();
        assert_eq!(load_issues(&db).unwrap()[Path::new("test")], issue);
        assert_eq!(
            db.query_row("SELECT offset FROM sources", [], |row| row.get::<_, u64>(0))
                .unwrap(),
            42
        );
        save_issue(&db, Path::new("test"), None).unwrap();
        assert!(load_issues(&db).unwrap().is_empty());
    }

    fn source_file(name: &str, content: &[u8]) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "profiler-stamp-{name}-{}-{}.jsonl",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap()
        ));
        std::fs::write(&path, content).unwrap();
        path
    }

    // A same-length rewrite must invalidate the consumed prefix even though the cursor still equals EOF.
    #[test]
    fn equal_length_rewrite_is_detected() {
        let path = source_file("equal", b"original\n");
        let stamp = capture_stamp(&path, 9).unwrap();
        std::fs::write(&path, b"replaced\n").unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(
                SystemTime::UNIX_EPOCH
                    + std::time::Duration::from_nanos(stamp.modified_ns.unwrap())
                    + std::time::Duration::from_secs(1),
            )
            .unwrap();
        assert!(changed_prefix(&path, &stamp, &path.metadata().unwrap()).unwrap());
        std::fs::remove_file(path).unwrap();
    }

    // Truncate-and-regrow past the old length is detected by the old cursor's anchor.
    #[test]
    fn longer_replacement_is_detected() {
        let path = source_file("longer", b"original\n");
        let stamp = capture_stamp(&path, 9).unwrap();
        std::fs::write(&path, b"replaced with a longer history\n").unwrap();
        assert!(changed_prefix(&path, &stamp, &path.metadata().unwrap()).unwrap());
        std::fs::remove_file(path).unwrap();
    }

    // Ordinary append preserves the committed anchor; a captured stamp and cursor roll back together.
    #[test]
    fn append_preserves_prefix_and_stamp_is_transactional() {
        use std::io::Write;
        let path = source_file("append", b"original\n");
        let stamp = capture_stamp(&path, 9).unwrap();
        assert!(!changed_prefix(&path, &stamp, &path.metadata().unwrap()).unwrap());
        File::options()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"new record\n")
            .unwrap();
        assert!(!changed_prefix(&path, &stamp, &path.metadata().unwrap()).unwrap());
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE sources(path TEXT PRIMARY KEY,offset INTEGER,thread_id TEXT)",
        )
        .unwrap();
        db.execute(
            "INSERT INTO sources VALUES(?1,9,'chat')",
            [path.to_string_lossy().as_ref()],
        )
        .unwrap();
        ensure_schema(&mut db).unwrap();
        assert!(load_stamp(&db, &path).unwrap().is_none());
        let tx = db.transaction().unwrap();
        save_stamp(&tx, &path, Some(&stamp)).unwrap();
        tx.rollback().unwrap();
        assert!(load_stamp(&db, &path).unwrap().is_none());
        save_stamp(&db, &path, Some(&stamp)).unwrap();
        assert_eq!(load_stamp(&db, &path).unwrap(), Some(stamp));
        assert_eq!(
            db.query_row("SELECT offset FROM sources", [], |row| row.get::<_, u64>(0))
                .unwrap(),
            9
        );
        drop(db);
        std::fs::remove_file(path).unwrap();
    }
}
