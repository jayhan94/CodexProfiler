use crate::model::{self, ModelRequest, Session};
use crate::source_state::{self, SourceStamp};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::Deserialize;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
type Effort = (i64, Option<String>);

#[derive(Clone)]
struct Source {
    path: PathBuf,
    offset: u64,
    stamp: Option<String>,
}

struct Scanned {
    source: Source,
    stamp: SourceStamp,
    history: Vec<Effort>,
    starts: Vec<i64>,
}

/// Read only durable source prefixes for visible requests whose start was observed before upgrade.
pub(crate) fn restore(db: &mut Connection, home: &Path, visible_ids: &[String]) -> Result<bool> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS effort_backfill(
        scope TEXT NOT NULL, path TEXT NOT NULL, thread_id TEXT NOT NULL,
        offset INTEGER NOT NULL, stamp TEXT NOT NULL,
        PRIMARY KEY(scope,path,thread_id));",
    )?;
    let root = home.join("sessions");
    let Ok(root) = root.canonicalize() else {
        return Ok(false);
    };
    let scope = root.to_string_lossy().into_owned();
    let mut changed = false;
    for id in visible_ids.iter().take(96) {
        let Some(session) = load_session(db, id)? else {
            continue;
        };
        let starts = unknown_starts(&session);
        if starts.is_empty() {
            continue;
        }
        let mut scanned = Vec::new();
        let mut complete = true;
        for source in sources(db, id)? {
            let in_scope = source
                .path
                .canonicalize()
                .map(|path| path.starts_with(&root))
                .unwrap_or_else(|_| source.path.starts_with(home.join("sessions")));
            if !in_scope {
                continue;
            }
            match scan(db, &scope, id, source, &starts) {
                Ok(Some(result)) => scanned.push(result),
                Ok(None) => (),
                // Missing or damaged history remains unknown and gets no success marker.
                Err(_) => {
                    complete = false;
                    break;
                }
            }
        }
        if complete && !scanned.is_empty() {
            changed |= save(db, &scope, id, &scanned)?;
        }
    }
    Ok(changed)
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

fn needs_effort(request: &ModelRequest) -> bool {
    request.started_ms.is_some()
        && request.reasoning_effort.is_none()
        && request.reasoning_effort_source != "request"
}

fn unknown_starts(session: &Session) -> Vec<i64> {
    let mut starts: Vec<_> = requests(session)
        .filter(|(_, request)| needs_effort(request))
        .filter_map(|(_, request)| request.started_ms)
        .collect();
    starts.sort_unstable();
    starts.dedup();
    starts
}

fn requests(session: &Session) -> impl Iterator<Item = (String, &ModelRequest)> {
    session
        .requests
        .iter()
        .enumerate()
        .map(|(index, request)| (format!("$.requests[{index}]"), request))
        .chain(
            session
                .timeline_history
                .iter()
                .enumerate()
                .flat_map(|(turn, timeline)| {
                    timeline
                        .requests
                        .iter()
                        .enumerate()
                        .map(move |(index, request)| {
                            (
                                format!("$.timeline_history[{turn}].requests[{index}]"),
                                request,
                            )
                        })
                }),
        )
}

fn sources(db: &Connection, id: &str) -> Result<Vec<Source>> {
    let mut statement =
        db.prepare("SELECT path,offset,stamp FROM sources WHERE thread_id=?1 ORDER BY path")?;
    let rows = statement.query_map([id], |row| {
        Ok(Source {
            path: PathBuf::from(row.get::<_, String>(0)?),
            offset: row.get(1)?,
            stamp: row.get(2)?,
        })
    })?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

/// A completed prefix is reused on restart; only a newly committed tail needs another read.
fn scan(
    db: &Connection,
    scope: &str,
    id: &str,
    source: Source,
    starts: &[i64],
) -> Result<Option<Scanned>> {
    let marker: Option<(u64, String)> = db
        .query_row(
            "SELECT offset,stamp FROM effort_backfill WHERE scope=?1 AND path=?2 AND thread_id=?3",
            params![scope, source.path.to_string_lossy(), id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let metadata = source.path.metadata()?;
    if let Some(stamp) = source.stamp.as_deref() {
        let stamp: SourceStamp = serde_json::from_str(stamp)?;
        if source_state::changed_prefix(&source.path, &stamp, &metadata)? {
            return Err("Historical source no longer matches its committed prefix".into());
        }
    }
    let from = if let Some((offset, stamp)) = marker {
        let stamp: SourceStamp = serde_json::from_str(&stamp)?;
        if offset <= source.offset
            && !source_state::changed_prefix(&source.path, &stamp, &metadata)?
        {
            if offset == source.offset {
                return Ok(None);
            }
            offset
        } else {
            0
        }
    } else {
        0
    };
    let stamp = source_state::capture_stamp(&source.path, source.offset)?;
    let mut file = File::open(&source.path)?;
    file.seek(SeekFrom::Start(from))?;
    let history = read_history(file.take(source.offset - from), id, from == 0, starts)?;
    if source_state::changed_prefix(&source.path, &stamp, &source.path.metadata()?)? {
        return Err("Historical source changed during effort backfill".into());
    }
    Ok(Some(Scanned {
        source,
        stamp,
        history,
        starts: starts.to_vec(),
    }))
}

/// Streaming deserialization skips prompts and tool payloads without retaining complete JSON lines.
fn read_history(
    reader: impl Read,
    owner: &str,
    verify_owner: bool,
    starts: &[i64],
) -> Result<Vec<Effort>> {
    let mut records =
        serde_json::Deserializer::from_reader(BufReader::with_capacity(64 * 1024, reader))
            .into_iter::<Record>();
    if verify_owner {
        let first = records.next().ok_or("Historical source is empty")??;
        if first.kind != "session_meta" || first.payload.id.as_deref() != Some(owner) {
            return Err("Historical source belongs to another chat".into());
        }
    }
    // Keep one effective setting per visible start plus the newest setting, regardless of log size.
    let mut chosen: Vec<Option<Effort>> = vec![None; starts.len() + 1];
    for record in records {
        let record = record?;
        if record
            .payload
            .thread_id
            .as_deref()
            .or(record.thread_id.as_deref())
            .is_some_and(|id| id != owner)
        {
            continue;
        }
        let Some(effort) = record.effort() else {
            continue;
        };
        for (index, start) in starts.iter().enumerate() {
            if effort.0 <= *start && chosen[index].as_ref().is_none_or(|old| old.0 <= effort.0) {
                chosen[index] = Some(effort.clone());
            }
        }
        let latest = chosen
            .last_mut()
            .expect("One slot always retains the latest setting");
        if latest.as_ref().is_none_or(|old| old.0 <= effort.0) {
            *latest = Some(effort);
        }
    }
    Ok(chosen.into_iter().flatten().collect())
}

/// Re-read state under the writer lock; JSON updates touch only effort, never stale financial state.
fn save(db: &mut Connection, scope: &str, id: &str, scanned: &[Scanned]) -> Result<bool> {
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current_sources = sources(&tx, id)?;
    if scanned.iter().any(|scan| {
        !current_sources.iter().any(|source| {
            source.path == scan.source.path
                && source.offset == scan.source.offset
                && source.stamp == scan.source.stamp
        })
    }) {
        return Ok(false);
    }
    let Some(mut session) = load_session(&tx, id)? else {
        return Ok(false);
    };
    // Compacted history is valid only for the starts it was scanned for; retry after concurrent changes.
    let current_starts = unknown_starts(&session);
    if scanned.iter().any(|scan| scan.starts != current_starts) {
        return Ok(false);
    }
    let pending: Vec<_> = requests(&session)
        .filter(|(_, request)| needs_effort(request))
        .map(|(path, _)| path)
        .collect();
    let previous = session.effort_history.clone();
    let history: Vec<_> = scanned
        .iter()
        .flat_map(|scan| scan.history.iter().cloned())
        .collect();
    model::supplement_reasoning_history(&mut session, &history);
    let mut changed = false;
    if previous != session.effort_history {
        tx.execute(
            "UPDATE sessions SET state=json_set(state,'$.effort_history',json(?2)) WHERE id=?1",
            params![id, serde_json::to_string(&session.effort_history)?],
        )?;
        changed = true;
    }
    for (path, request) in requests(&session) {
        if pending.contains(&path) && request.reasoning_effort.is_some() {
            tx.execute(
                "UPDATE sessions SET state=json_set(state,?2,?3,?4,'settings') WHERE id=?1",
                params![
                    id,
                    format!("{path}.reasoning_effort"),
                    request.reasoning_effort,
                    format!("{path}.reasoning_effort_source")
                ],
            )?;
            changed = true;
        }
    }
    for scan in scanned {
        tx.execute("INSERT INTO effort_backfill(scope,path,thread_id,offset,stamp) VALUES(?1,?2,?3,?4,?5)
            ON CONFLICT(scope,path,thread_id) DO UPDATE SET offset=excluded.offset,stamp=excluded.stamp",
            params![scope, scan.source.path.to_string_lossy(), id, scan.source.offset, serde_json::to_string(&scan.stamp)?])?;
    }
    tx.commit()?;
    Ok(changed)
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Settings {
    #[serde(deserialize_with = "present")]
    reasoning_effort: Option<Option<String>>,
    collaboration_mode: Option<Mode>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Mode {
    settings: ModeSettings,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct ModeSettings {
    #[serde(deserialize_with = "present")]
    reasoning_effort: Option<Option<String>>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Payload {
    #[serde(rename = "type")]
    kind: String,
    id: Option<String>,
    thread_id: Option<String>,
    #[serde(deserialize_with = "present")]
    effort: Option<Option<String>>,
    #[serde(deserialize_with = "present")]
    reasoning_effort: Option<Option<String>>,
    thread_settings: Option<Settings>,
    settings: Option<Settings>,
    collaboration_mode: Option<Mode>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Record {
    #[serde(rename = "type")]
    kind: String,
    timestamp_ms: Option<i64>,
    timestamp: Option<String>,
    thread_id: Option<String>,
    payload: Payload,
}

fn present<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(deserializer).map(Some)
}

impl Record {
    fn effort(self) -> Option<Effort> {
        let timestamp = self.timestamp_ms.or_else(|| {
            self.timestamp
                .as_deref()
                .and_then(|text| chrono::DateTime::parse_from_rfc3339(text).ok())
                .map(|time| time.timestamp_millis())
        })?;
        let observed = if self.kind == "turn_context" {
            self.payload
                .effort
                .or(self.payload.reasoning_effort)
                .or_else(|| {
                    self.payload
                        .collaboration_mode
                        .and_then(|mode| mode.settings.reasoning_effort)
                })
        } else if self.kind == "event_msg" && self.payload.kind == "thread_settings_applied" {
            self.payload
                .thread_settings
                .or(self.payload.settings)
                .and_then(|settings| {
                    settings.reasoning_effort.or_else(|| {
                        settings
                            .collaboration_mode
                            .and_then(|mode| mode.settings.reasoning_effort)
                    })
                })
        } else {
            None
        }?;
        Some((
            timestamp,
            observed
                .map(|value| value.trim().to_lowercase())
                .filter(|value| !value.is_empty()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn fixture() -> (Connection, PathBuf, PathBuf) {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let home =
            std::env::temp_dir().join(format!("profiler-effort-{}-{unique}", std::process::id()));
        fs::create_dir_all(home.join("sessions")).unwrap();
        let path = home.join("sessions").join("rollout-chat.jsonl");
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE sessions(id TEXT PRIMARY KEY,last_ms INTEGER,state TEXT);
            CREATE TABLE sources(path TEXT PRIMARY KEY,offset INTEGER,thread_id TEXT,stamp TEXT);",
        )
        .unwrap();
        (db, home, path)
    }

    fn cache(db: &Connection, path: &Path, state: &Value, prefix: u64) {
        db.execute(
            "INSERT INTO sessions VALUES('chat',3000,?1)",
            [state.to_string()],
        )
        .unwrap();
        let stamp = source_state::capture_stamp(path, prefix).unwrap();
        db.execute(
            "INSERT INTO sources VALUES(?1,?2,'chat',?3)",
            params![
                path.to_string_lossy(),
                prefix,
                serde_json::to_string(&stamp).unwrap()
            ],
        )
        .unwrap();
    }

    fn state(db: &Connection) -> Value {
        let state: String = db
            .query_row("SELECT state FROM sessions WHERE id='chat'", [], |row| {
                row.get(0)
            })
            .unwrap();
        serde_json::from_str(&state).unwrap()
    }

    fn without_effort(mut value: Value) -> Value {
        value.as_object_mut().unwrap().remove("effort_history");
        let clear = |request: &mut Value| {
            request.as_object_mut().unwrap().remove("reasoning_effort");
            request
                .as_object_mut()
                .unwrap()
                .remove("reasoning_effort_source");
        };
        for request in value["requests"].as_array_mut().unwrap() {
            clear(request);
        }
        if let Some(turns) = value["timeline_history"].as_array_mut() {
            for turn in turns {
                for request in turn["requests"].as_array_mut().unwrap() {
                    clear(request);
                }
            }
        }
        value
    }

    // Real cache-shaped records recover High/Max only from the committed prefix; every other field survives.
    #[test]
    fn cached_requests_recover_effort_without_replaying_financial_or_source_state() {
        let (mut db, home, path) = fixture();
        let prefix = concat!(
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"chat\"}}\n",
            "{\"type\":\"turn_context\",\"timestamp_ms\":300,\"payload\":{\"effort\":\"high\"}}\n",
            "{\"type\":\"event_msg\",\"timestamp_ms\":1500,\"payload\":{\"type\":\"thread_settings_applied\",\"thread_settings\":{\"reasoning_effort\":\"max\"}}}\n"
        );
        let tail =
            "{\"type\":\"turn_context\",\"timestamp_ms\":1700,\"payload\":{\"effort\":\"low\"}}\n";
        fs::write(&path, format!("{prefix}{tail}")).unwrap();
        let before = json!({"id":"chat","phase":"unchanged","cost":{"priced_requests":17,"output_usd":12.5},
            "future_financial":{"keep":123},"requests":[
                {"id":"high","started_ms":1000},{"id":"max","started_ms":2000},
                {"id":"end-only","ended_ms":2500},{"id":"known","started_ms":3000,"reasoning_effort":"low"}],
            "timeline_history":[{"turn_id":"old","requests":[{"id":"archived","started_ms":500}]}]});
        cache(&db, &path, &before, prefix.len() as u64);
        let source: (u64, String) = db
            .query_row("SELECT offset,stamp FROM sources", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert!(restore(&mut db, &home, &["chat".into()]).unwrap());
        let after = state(&db);
        assert_eq!(after["requests"][0]["reasoning_effort"], "high");
        assert_eq!(after["requests"][1]["reasoning_effort"], "max");
        assert!(after["requests"][2]["reasoning_effort"].is_null());
        assert_eq!(after["requests"][3]["reasoning_effort"], "low");
        assert_eq!(
            after["timeline_history"][0]["requests"][0]["reasoning_effort"],
            "high"
        );
        assert_eq!(without_effort(after), without_effort(before));
        let after_source = db
            .query_row("SELECT offset,stamp FROM sources", [], |row| {
                Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap();
        assert_eq!(after_source, source);
        fs::remove_dir_all(home).unwrap();
    }

    // A valid source without effort is marked once; missing/damaged prefixes stay retryable until restored.
    #[test]
    fn completed_sources_skip_repeated_reads_while_missing_and_bad_sources_retry() {
        let (mut db, home, path) = fixture();
        let meta = "{\"type\":\"session_meta\",\"payload\":{\"id\":\"chat\"}}\n";
        fs::write(&path, meta).unwrap();
        cache(
            &db,
            &path,
            &json!({"id":"chat","requests":[{"id":"old","started_ms":2000}]}),
            meta.len() as u64,
        );
        assert!(!restore(&mut db, &home, &["chat".into()]).unwrap());
        let scope = home
            .join("sessions")
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(scan(
            &db,
            &scope,
            "chat",
            sources(&db, "chat").unwrap().remove(0),
            &[2000]
        )
        .unwrap()
        .is_none());
        db.execute("DELETE FROM effort_backfill", []).unwrap();
        fs::remove_file(&path).unwrap();
        assert!(!restore(&mut db, &home, &["chat".into()]).unwrap());
        assert_eq!(
            db.query_row("SELECT count(*) FROM effort_backfill", [], |row| row
                .get::<_, u64>(0))
                .unwrap(),
            0
        );
        let good = format!("{meta}{{\"type\":\"turn_context\",\"timestamp_ms\":1000,\"payload\":{{\"effort\":\"max\"}}}}\n");
        fs::write(&path, format!("{meta}{{broken\n")).unwrap();
        db.execute(
            "UPDATE sources SET offset=?1,stamp=NULL",
            [path.metadata().unwrap().len()],
        )
        .unwrap();
        assert!(!restore(&mut db, &home, &["chat".into()]).unwrap());
        assert_eq!(
            db.query_row("SELECT count(*) FROM effort_backfill", [], |row| row
                .get::<_, u64>(0))
                .unwrap(),
            0
        );
        fs::write(&path, &good).unwrap();
        db.execute("UPDATE sources SET offset=?1", [good.len() as u64])
            .unwrap();
        assert!(restore(&mut db, &home, &["chat".into()]).unwrap());
        assert_eq!(state(&db)["requests"][0]["reasoning_effort"], "max");
        fs::remove_dir_all(home).unwrap();
    }

    // Only the selected home and observed starts are eligible; large prompt bodies are ignored.
    #[test]
    fn home_scope_and_streaming_history_keep_unrelated_records_out() {
        let (mut db, home, path) = fixture();
        let other_home = home.join("other");
        fs::create_dir_all(other_home.join("sessions")).unwrap();
        fs::write(
            &path,
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"chat\"}}\n",
        )
        .unwrap();
        cache(
            &db,
            &path,
            &json!({"id":"chat","requests":[{"id":"old","started_ms":2000}]}),
            path.metadata().unwrap().len(),
        );
        assert!(!restore(&mut db, &other_home, &["chat".into()]).unwrap());
        assert_eq!(
            db.query_row("SELECT count(*) FROM effort_backfill", [], |row| row
                .get::<_, u64>(0))
                .unwrap(),
            0
        );
        let rows = format!("{{\"type\":\"response_item\",\"payload\":{{\"content\":\"{}\"}}}}\n{{\"type\":\"turn_context\",\"timestamp_ms\":1000,\"payload\":{{\"effort\":\"high\"}}}}\n", "x".repeat(2 * 1024 * 1024));
        let history = read_history(rows.as_bytes(), "chat", false, &[2000]).unwrap();
        assert!(history
            .iter()
            .any(|(time, effort)| *time == 1000 && effort.as_deref() == Some("high")));
        db.execute(
            "UPDATE sessions SET state=?1",
            [json!({"id":"chat","requests":[{"id":"end-only","ended_ms":2000}]}).to_string()],
        )
        .unwrap();
        assert!(!restore(&mut db, &home, &["chat".into()]).unwrap());
        assert_eq!(
            db.query_row("SELECT count(*) FROM effort_backfill", [], |row| row
                .get::<_, u64>(0))
                .unwrap(),
            0
        );
        fs::remove_dir_all(home).unwrap();
    }

    // Copied parent contexts are excluded; a payload thread ID takes precedence over the envelope ID.
    #[test]
    fn copied_parent_contexts_cannot_supply_child_effort() {
        for (envelope, payload, accepted) in [
            ("parent", None, false),
            ("parent", Some("child"), true),
            ("child", Some("parent"), false),
        ] {
            let mut context = json!({"type":"turn_context","thread_id":envelope,
                "timestamp_ms":1500,"payload":{"effort":"max"}});
            if let Some(thread) = payload {
                context["payload"]["thread_id"] = json!(thread);
            }
            let rows = format!(
                "{}\n{}\n",
                json!({"type":"turn_context","thread_id":"child",
                "timestamp_ms":500,"payload":{"effort":"high"}}),
                context
            );
            let history = read_history(rows.as_bytes(), "child", false, &[2000]).unwrap();
            assert_eq!(
                history
                    .iter()
                    .any(|(_, effort)| effort.as_deref() == Some("max")),
                accepted
            );
            if !accepted {
                assert!(history
                    .iter()
                    .all(|(_, effort)| effort.as_deref() == Some("high")));
            }
        }
    }

    // A new unknown start invalidates a compacted scan; retry recovers its omitted intermediate setting.
    #[test]
    fn concurrent_unknown_start_rejects_history_without_marking_source_done() {
        let (mut db, home, path) = fixture();
        let rows = [
            json!({"type":"session_meta","payload":{"id":"chat"}}),
            json!({"type":"turn_context","timestamp_ms":500,"payload":{"effort":"high"}}),
            json!({"type":"turn_context","timestamp_ms":1500,"payload":{"effort":"low"}}),
            json!({"type":"turn_context","timestamp_ms":2500,"payload":{"effort":"max"}}),
        ]
        .map(|row| row.to_string())
        .join("\n")
            + "\n";
        fs::write(&path, &rows).unwrap();
        cache(
            &db,
            &path,
            &json!({"id":"chat","cost":{"output_usd":1.0},
            "requests":[{"id":"old","started_ms":1000}]}),
            rows.len() as u64,
        );
        assert!(!restore(&mut db, &home, &[]).unwrap());
        let scope = home
            .join("sessions")
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let scanned = scan(
            &db,
            &scope,
            "chat",
            sources(&db, "chat").unwrap().remove(0),
            &[1000],
        )
        .unwrap()
        .unwrap();
        assert!(!scanned.history.iter().any(|(time, _)| *time == 1500));
        let mut concurrent = state(&db);
        concurrent["requests"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id":"new","started_ms":2000}));
        concurrent["cost"]["output_usd"] = json!(99.0);
        db.execute("UPDATE sessions SET state=?1", [concurrent.to_string()])
            .unwrap();
        assert!(!save(&mut db, &scope, "chat", &[scanned]).unwrap());
        assert_eq!(state(&db), concurrent);
        assert_eq!(
            db.query_row("SELECT count(*) FROM effort_backfill", [], |row| row
                .get::<_, u64>(0))
                .unwrap(),
            0
        );
        assert!(restore(&mut db, &home, &["chat".into()]).unwrap());
        let after = state(&db);
        assert_eq!(after["requests"][0]["reasoning_effort"], "high");
        assert_eq!(after["requests"][1]["reasoning_effort"], "low");
        assert_eq!(after["cost"]["output_usd"], 99.0);
        fs::remove_dir_all(home).unwrap();
    }
}
