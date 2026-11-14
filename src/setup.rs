use crate::hook;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
struct OwnedHandler {
    event: String,
    handler: Value,
}

#[derive(Default, Deserialize, PartialEq, Serialize)]
struct State {
    handlers: Vec<OwnedHandler>,
}

/// Keep existing installations attached to their cache; new installs use user data.
pub fn default_database(executable: &Path) -> std::result::Result<PathBuf, String> {
    if let Some(parent) = executable.parent() {
        let legacy = parent.join("data/monitor.sqlite");
        if legacy.is_file() {
            return Ok(legacy);
        }
    }
    let home = || std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
    let directory = if cfg!(target_os = "windows") {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .or_else(|| home().map(|p| PathBuf::from(p).join("AppData/Local")))
            .map(|p| p.join("CodexProfiler"))
    } else if cfg!(target_os = "macos") {
        home().map(|p| PathBuf::from(p).join("Library/Application Support/CodexProfiler"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(|| home().map(|p| PathBuf::from(p).join(".local/share")))
            .map(|p| p.join("codex-profiler"))
    };
    directory
        .map(|p| p.join("monitor.sqlite"))
        .ok_or_else(|| "Cannot locate user data directory; pass --db PATH".into())
}

fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Reject broken configuration before opening any write path.
fn parse_config(bytes: Option<&[u8]>) -> Result<Value> {
    let mut config = match bytes {
        Some(bytes) => {
            serde_json::from_slice(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes))?
        }
        None => json!({"hooks": {}}),
    };
    let root = config
        .as_object_mut()
        .ok_or("hooks.json must be an object")?;
    if !root.contains_key("hooks") {
        root.insert("hooks".into(), json!({}));
    }
    let events = root["hooks"].as_object().ok_or("hooks must be an object")?;
    for groups in events.values() {
        for group in groups
            .as_array()
            .ok_or("Hook event must contain an array")?
        {
            group.as_object().ok_or("Hook group must be an object")?;
            group["hooks"]
                .as_array()
                .ok_or("Hook group must contain a hooks array")?;
        }
    }
    Ok(config)
}

fn expected(executable: &Path, home: &Path) -> Vec<OwnedHandler> {
    hook::configuration(executable, home)["hooks"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(event, groups)| OwnedHandler {
            event: event.clone(),
            handler: groups[0]["hooks"][0].clone(),
        })
        .collect()
}

/// Adopt only the exact historical generator output, never substring matches.
fn legacy_handler(handler: &Value) -> bool {
    let Some(command) = handler["command"].as_str() else {
        return false;
    };
    let Some((executable, home)) = command
        .strip_prefix('"')
        .and_then(|s| s.split_once("\" --hook --codex-home \""))
        .and_then(|(exe, home)| home.strip_suffix('"').map(|home| (exe, home)))
    else {
        return false;
    };
    if executable.contains('"') || home.contains('"') {
        return false;
    }
    let basename = executable.rsplit(['/', '\\']).next().unwrap_or_default();
    if !matches!(
        basename,
        "codex-profiler" | "codex-profiler.exe" | "codex-monitor" | "codex-monitor.exe"
    ) {
        return false;
    }
    *handler
        == hook::configuration(Path::new(executable), Path::new(home))["hooks"]["Stop"][0]["hooks"]
            [0]
}

fn is_owned(event: &str, handler: &Value, owned: &[OwnedHandler]) -> bool {
    owned
        .iter()
        .any(|entry| entry.event == event && entry.handler == *handler)
}

fn legacy_owned(config: &Value) -> Vec<OwnedHandler> {
    let mut handlers = Vec::new();
    for (event, groups) in config["hooks"].as_object().unwrap() {
        for group in groups.as_array().unwrap() {
            for handler in group["hooks"].as_array().unwrap() {
                if legacy_handler(handler) {
                    handlers.push(OwnedHandler {
                        event: event.clone(),
                        handler: handler.clone(),
                    });
                }
            }
        }
    }
    handlers
}

/// Replace owned handlers in place so neighboring groups retain their identity/order.
fn merge(config: &mut Value, owned: &[OwnedHandler], desired: &[OwnedHandler]) {
    let events = config["hooks"].as_object_mut().unwrap();
    for (event, groups) in events.iter_mut() {
        let replacement = desired.iter().find(|entry| entry.event == *event);
        let groups = groups.as_array_mut().unwrap();
        let mut installed = groups.iter().any(|group| {
            group["hooks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|handler| replacement.is_some_and(|entry| entry.handler == *handler))
        });
        let mut emptied = Vec::new();
        for (index, group) in groups.iter_mut().enumerate() {
            let handlers = group["hooks"].as_array_mut().unwrap();
            let mut updated = Vec::new();
            let mut removed_owned = false;
            for handler in handlers.drain(..) {
                if replacement.is_some_and(|entry| entry.handler == handler) {
                    updated.push(handler);
                } else if is_owned(event, &handler, owned) {
                    removed_owned = true;
                    if !installed {
                        if let Some(entry) = replacement {
                            updated.push(entry.handler.clone());
                            installed = true;
                        }
                    }
                } else {
                    updated.push(handler);
                }
            }
            *handlers = updated;
            if removed_owned && handlers.is_empty() && group.as_object().unwrap().len() == 1 {
                emptied.push(index);
            }
        }
        // Keep user group metadata, including empty groups with matcher settings.
        for index in emptied.into_iter().rev() {
            groups.remove(index);
        }
    }
    for entry in desired {
        let groups = events
            .entry(&entry.event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .unwrap();
        let present = groups
            .iter()
            .any(|group| group["hooks"].as_array().unwrap().contains(&entry.handler));
        if !present {
            groups.push(json!({"hooks": [entry.handler.clone()]}));
        }
    }
}

fn nonce() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

/// Flush a same-directory temporary file before replacing the destination.
fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or("Path has no parent")?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".codex-profiler-{}-{}.tmp",
        std::process::id(),
        nonce()
    ));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        if let Ok(metadata) = fs::metadata(path) {
            file.set_permissions(metadata.permissions())?;
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        replace_file(&temp, path)?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}

#[cfg(not(windows))]
fn replace_file(source: &Path, target: &Path) -> std::io::Result<()> {
    fs::rename(source, target)
}

#[cfg(windows)]
fn replace_file(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }
    let source: Vec<_> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<_> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    // REPLACE_EXISTING | WRITE_THROUGH avoids a delete/rename gap on Windows.
    if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), 0x1 | 0x8) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn formatted(config: &Value, original: Option<&[u8]>) -> Result<Vec<u8>> {
    let original = original
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .unwrap_or("");
    let indent = original
        .lines()
        .find_map(|line| {
            let trimmed = line.trim_start();
            let count = line.len() - trimmed.len();
            (count > 0 && trimmed.starts_with('"')).then(|| line.as_bytes()[..count].to_vec())
        })
        .unwrap_or_else(|| b"  ".to_vec());
    let formatter = serde_json::ser::PrettyFormatter::with_indent(&indent);
    let mut bytes = Vec::new();
    config.serialize(&mut serde_json::Serializer::with_formatter(
        &mut bytes, formatter,
    ))?;
    bytes.push(b'\n');
    if original.contains("\r\n") {
        bytes = String::from_utf8(bytes)?.replace('\n', "\r\n").into_bytes();
    }
    Ok(bytes)
}

fn save_state(path: &Path, state: &State) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(state)?;
    if read_optional(path)?.as_deref() != Some(bytes.as_slice()) {
        atomic_write(path, &bytes)?;
    }
    Ok(())
}

fn summary(changed: bool, backup: Option<PathBuf>, config: &Path, install: bool) -> Value {
    json!({"changed": changed, "backup": backup, "config": config,
        "review_required": install})
}

/// Setup is serialized separately from hook telemetry; Codex's own trust remains manual.
pub fn manage(home: &Path, executable: &Path, install: bool) -> Result<Value> {
    let config_path = home.join("hooks.json");
    // Invalid input must not create a lock, state or backup as a side effect.
    parse_config(read_optional(&config_path)?.as_deref())?;
    fs::create_dir_all(home.join("monitor"))?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(home.join("monitor/setup.lock"))?;
    lock.lock_exclusive()?;
    let original = read_optional(&config_path)?;
    let mut config = parse_config(original.as_deref())?;
    let before = config.clone();
    let state_path = home.join("monitor/install-state.json");
    let state_bytes = read_optional(&state_path)?;
    let has_state = state_bytes.is_some();
    let mut state: State = state_bytes
        .map(|bytes| serde_json::from_slice(&bytes))
        .transpose()?
        .unwrap_or_default();
    let current = expected(executable, home);
    // An old or SkipHooks installation must not uninstall another installation's hooks.
    if !install && has_state && !state.handlers.iter().any(|entry| current.contains(entry)) {
        return Ok(summary(false, None, &config_path, false));
    }
    if !has_state {
        state.handlers = if install {
            legacy_owned(&config)
        } else {
            current.clone()
        };
    }
    let desired = if install { current } else { Vec::new() };
    merge(&mut config, &state.handlers, &desired);
    let changed = config != before;
    if !install && !changed && !has_state {
        return Ok(summary(false, None, &config_path, false));
    }
    let mut backup = None;
    if changed {
        // Persist both generations first: interrupted upgrades remain safely removable.
        let mut recovery = state.handlers;
        for entry in &desired {
            if !recovery.contains(entry) {
                recovery.push(entry.clone());
            }
        }
        save_state(&state_path, &State { handlers: recovery })?;
        if let Some(bytes) = &original {
            let path = home.join(format!("hooks.json.codex-profiler-{}.bak", nonce()));
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            backup = Some(path);
        }
        let bytes = formatted(&config, original.as_deref())?;
        if read_optional(&config_path)? != original {
            return Err(
                "hooks.json changed during setup; rerun to merge the latest changes".into(),
            );
        }
        atomic_write(&config_path, &bytes)?;
    }
    save_state(&state_path, &State { handlers: desired })?;
    FileExt::unlock(&lock)?;
    Ok(summary(changed, backup, &config_path, install))
}

/// Read only the tail; a long-running profiler must not load its whole event history.
fn latest_event(path: &Path) -> Result<Option<i64>> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(64 * 1024);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes
        .split(|byte| *byte == b'\n')
        .rev()
        .filter_map(|line| {
            let row: Value = serde_json::from_slice(line).ok()?;
            row["timestamp_ms"].as_i64()
        })
        .next())
}

pub fn doctor(home: &Path, database: &Path, executable: &Path) -> (Value, bool) {
    let mut issues = Vec::new();
    let hooks_configured = match read_optional(&home.join("hooks.json"))
        .and_then(|bytes| parse_config(bytes.as_deref()))
    {
        Ok(config) => expected(executable, home).iter().all(|entry| {
            config["hooks"][&entry.event]
                .as_array()
                .is_some_and(|groups| {
                    groups
                        .iter()
                        .any(|group| group["hooks"].as_array().unwrap().contains(&entry.handler))
                })
        }),
        Err(error) => {
            issues.push(format!("Cannot read hooks.json: {error}"));
            false
        }
    };
    if !hooks_configured {
        issues.push("Expected profiler hooks are missing; run --install-hooks".into());
    }
    if let Err(error) = fs::read_dir(home) {
        issues.push(format!("Cannot read CODEX_HOME: {error}"));
    }
    if database.exists() {
        if let Err(error) = File::open(database) {
            issues.push(format!("Cannot read database: {error}"));
        }
    }
    let log = home.join("monitor/hooks.jsonl");
    let log_exists = log.exists();
    let latest = if log_exists {
        match latest_event(&log) {
            Ok(timestamp) => timestamp,
            Err(error) => {
                issues.push(format!("Cannot read hook log: {error}"));
                None
            }
        }
    } else {
        None
    };
    let healthy = issues.is_empty();
    (
        json!({"version": env!("CARGO_PKG_VERSION"), "executable": executable,
        "codex_home": home, "database": database, "hooks_configured": hooks_configured,
        "hooks_log_exists": log_exists, "latest_event_ms": latest,
        "trust": "review-in-Codex", "issues": issues}),
        healthy,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let directory = std::env::temp_dir().join(format!(
                "profiler 空 格-{}-{}-{}",
                std::process::id(),
                nonce(),
                NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&directory).unwrap();
            Self(directory)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    // Real disk setup keeps user handlers, indentation, cache, and repeated install bytes.
    #[test]
    fn install_repeat_upgrade_and_uninstall_preserve_user_data() {
        let fixture = Fixture::new();
        let home = &fixture.0;
        let user = json!({"type":"command", "command":"echo user"});
        let original = json!({"custom":{"enabled":true},"hooks":{"Stop":[
            {"matcher":"custom", "hooks":[user.clone()]}]}});
        fs::write(
            home.join("hooks.json"),
            serde_json::to_string_pretty(&original).unwrap(),
        )
        .unwrap();
        fs::write(home.join("monitor.sqlite"), "keep cache").unwrap();
        let exe = home.join("version one/codex-profiler.exe");
        assert_eq!(manage(home, &exe, true).unwrap()["changed"], true);
        let first = fs::read(home.join("hooks.json")).unwrap();
        let state = fs::read(home.join("monitor/install-state.json")).unwrap();
        let entries = fs::read_dir(home).unwrap().count();
        assert_eq!(manage(home, &exe, true).unwrap()["changed"], false);
        assert_eq!(fs::read(home.join("hooks.json")).unwrap(), first);
        assert_eq!(
            fs::read(home.join("monitor/install-state.json")).unwrap(),
            state
        );
        assert_eq!(fs::read_dir(home).unwrap().count(), entries);
        let upgraded = home.join("version two/codex-profiler.exe");
        manage(home, &upgraded, true).unwrap();
        let config: Value =
            serde_json::from_slice(&fs::read(home.join("hooks.json")).unwrap()).unwrap();
        assert_eq!(config["hooks"]["Stop"][0]["hooks"][0], user);
        assert!(config["hooks"]["Stop"][1]["hooks"][0]["command"]
            .as_str()
            .unwrap()
            .contains("version two"));
        manage(home, &upgraded, false).unwrap();
        let config: Value =
            serde_json::from_slice(&fs::read(home.join("hooks.json")).unwrap()).unwrap();
        assert_eq!(config["custom"], original["custom"]);
        assert_eq!(config["hooks"]["Stop"], original["hooks"]["Stop"]);
        assert_eq!(
            fs::read_to_string(home.join("monitor.sqlite")).unwrap(),
            "keep cache"
        );
        assert_eq!(manage(home, &upgraded, false).unwrap()["changed"], false);
    }

    // User edits cease to be owned; upgrades and uninstall must retain those edits.
    #[test]
    fn modified_handlers_are_preserved() {
        let fixture = Fixture::new();
        let home = &fixture.0;
        let exe = home.join("codex-profiler.exe");
        manage(home, &exe, true).unwrap();
        let path = home.join("hooks.json");
        let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        config["hooks"]["Stop"][0]["hooks"][0]["timeout"] = json!(9);
        let edited = expected(&home.join("manual/codex-profiler.exe"), home)[0]
            .handler
            .clone();
        config["hooks"]["PreToolUse"][0]["hooks"][0] = edited.clone();
        fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
        let new = home.join("new/codex-profiler.exe");
        manage(home, &new, true).unwrap();
        manage(home, &new, false).unwrap();
        let config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(config["hooks"]["Stop"][0]["hooks"][0]["timeout"], 9);
        assert_eq!(config["hooks"]["PreToolUse"][0]["hooks"][0], edited);
    }

    // An upgrade replaces only the owned handler within a mixed, user-configured group.
    #[test]
    fn mixed_groups_keep_neighbors_and_indentation() {
        let fixture = Fixture::new();
        let home = &fixture.0;
        let old = home.join("old/codex-profiler");
        let new = home.join("new/codex-profiler");
        let owned = expected(&old, home)
            .into_iter()
            .find(|entry| entry.event == "Stop")
            .unwrap();
        let user = json!({"type":"command", "command":"echo keep"});
        let config = json!({"hooks":{"Stop":[{"hooks":[]},
            {"matcher":"tools", "custom":"keep", "hooks":[user.clone(), owned.handler, user.clone()]},
            {"hooks":[user.clone()]}]}});
        let formatter = serde_json::ser::PrettyFormatter::with_indent(b"    ");
        let mut bytes = Vec::new();
        config
            .serialize(&mut serde_json::Serializer::with_formatter(
                &mut bytes, formatter,
            ))
            .unwrap();
        fs::write(home.join("hooks.json"), bytes).unwrap();
        manage(home, &new, true).unwrap();
        let text = fs::read_to_string(home.join("hooks.json")).unwrap();
        assert!(text.contains("\n    \"hooks\""));
        let updated: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(updated["hooks"]["Stop"][0], config["hooks"]["Stop"][0]);
        assert_eq!(updated["hooks"]["Stop"][1]["hooks"][0], user);
        assert_eq!(updated["hooks"]["Stop"][1]["hooks"][2], user);
        assert_eq!(updated["hooks"]["Stop"][2], config["hooks"]["Stop"][2]);
        assert_eq!(updated["hooks"]["Stop"][1]["custom"], "keep");
        manage(home, &new, false).unwrap();
        let updated: Value =
            serde_json::from_slice(&fs::read(home.join("hooks.json")).unwrap()).unwrap();
        assert_eq!(
            updated["hooks"]["Stop"][1]["hooks"],
            json!([user.clone(), user])
        );
    }

    // Malformed configs abort unchanged, before state/backup creation.
    #[test]
    fn malformed_config_is_not_touched() {
        let fixture = Fixture::new();
        let home = &fixture.0;
        for text in ["{invalid", "[]", "{\"hooks\":{\"Stop\":[{}]}}"] {
            fs::write(home.join("hooks.json"), text).unwrap();
            assert!(manage(home, &home.join("codex-profiler"), true).is_err());
            assert_eq!(fs::read_to_string(home.join("hooks.json")).unwrap(), text);
            assert_eq!(fs::read_dir(home).unwrap().count(), 1);
        }
    }

    // Similar text cannot authorize deleting unrelated user commands.
    #[test]
    fn legacy_adoption_requires_exact_generated_shape() {
        let exe = Path::new("/legacy/codex-profiler");
        let home = Path::new("/home/user/.codex");
        let handler = expected(exe, home)[0].handler.clone();
        assert!(legacy_handler(&handler));
        let mut custom = handler.clone();
        custom["command"] = json!("echo codex-profiler --hook --codex-home");
        assert!(!legacy_handler(&custom));
        custom = handler;
        custom["timeout"] = json!(5);
        assert!(!legacy_handler(&custom));
    }

    // Legacy executable-relative data remains selected when updating an existing install.
    #[test]
    fn existing_legacy_cache_retains_continuity() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.0.join("data")).unwrap();
        let db = fixture.0.join("data/monitor.sqlite");
        fs::write(&db, "old cache").unwrap();
        assert_eq!(
            default_database(&fixture.0.join("codex-profiler")).unwrap(),
            db
        );
    }

    // Uninstalling a SkipHooks copy must leave a different manual installation untouched.
    #[test]
    fn uninstall_without_state_preserves_other_legacy_installations() {
        let fixture = Fixture::new();
        let home = &fixture.0;
        let old = home.join("manual/codex-profiler.exe");
        let config = serde_json::to_vec_pretty(&hook::configuration(&old, home)).unwrap();
        fs::write(home.join("hooks.json"), &config).unwrap();
        let result = manage(home, &home.join("skip-hooks/codex-profiler.exe"), false).unwrap();
        assert_eq!(result["changed"], false);
        assert_eq!(fs::read(home.join("hooks.json")).unwrap(), config);
        assert!(!home.join("monitor/install-state.json").exists());
        assert_eq!(fs::read_dir(home).unwrap().count(), 2);
    }

    // A previous binary cannot clear the active installation's shared ownership manifest.
    #[test]
    fn old_path_uninstall_preserves_new_owner_configuration_and_manifest() {
        let fixture = Fixture::new();
        let home = &fixture.0;
        let active = home.join("new/codex-profiler.exe");
        manage(home, &active, true).unwrap();
        let config = fs::read(home.join("hooks.json")).unwrap();
        let state = fs::read(home.join("monitor/install-state.json")).unwrap();
        let entries = fs::read_dir(home).unwrap().count();
        let result = manage(home, &home.join("old/codex-profiler.exe"), false).unwrap();
        assert_eq!(result["changed"], false);
        assert_eq!(fs::read(home.join("hooks.json")).unwrap(), config);
        assert_eq!(
            fs::read(home.join("monitor/install-state.json")).unwrap(),
            state
        );
        assert_eq!(fs::read_dir(home).unwrap().count(), entries);
    }

    // Without a manifest, only this executable's exact legacy handlers are removable.
    #[test]
    fn current_legacy_uninstall_removes_only_its_own_handlers() {
        let fixture = Fixture::new();
        let home = &fixture.0;
        let current = home.join("current/codex-profiler.exe");
        let other = home.join("other/codex-profiler.exe");
        let foreign = hook::configuration(&other, home)["hooks"]["Stop"][0].clone();
        let mut config = hook::configuration(&current, home);
        config["hooks"]["Stop"]
            .as_array_mut()
            .unwrap()
            .push(foreign.clone());
        fs::write(
            home.join("hooks.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        assert_eq!(manage(home, &current, false).unwrap()["changed"], true);
        let config: Value =
            serde_json::from_slice(&fs::read(home.join("hooks.json")).unwrap()).unwrap();
        assert_eq!(config["hooks"]["Stop"], json!([foreign]));
        assert_eq!(config["hooks"]["PreToolUse"], json!([]));
    }

    // An interrupted upgrade records both generations and remains resumable by the new binary.
    #[test]
    fn recovery_manifest_union_resumes_install_and_releases_old_owner() {
        let fixture = Fixture::new();
        let home = &fixture.0;
        let old = home.join("old/codex-profiler.exe");
        let new = home.join("new/codex-profiler.exe");
        fs::write(
            home.join("hooks.json"),
            serde_json::to_vec(&hook::configuration(&old, home)).unwrap(),
        )
        .unwrap();
        let mut handlers = expected(&old, home);
        handlers.extend(expected(&new, home));
        fs::create_dir_all(home.join("monitor")).unwrap();
        save_state(
            &home.join("monitor/install-state.json"),
            &State { handlers },
        )
        .unwrap();
        assert_eq!(manage(home, &new, true).unwrap()["changed"], true);
        let config = fs::read(home.join("hooks.json")).unwrap();
        let state = fs::read(home.join("monitor/install-state.json")).unwrap();
        assert_eq!(manage(home, &old, false).unwrap()["changed"], false);
        assert_eq!(fs::read(home.join("hooks.json")).unwrap(), config);
        assert_eq!(
            fs::read(home.join("monitor/install-state.json")).unwrap(),
            state
        );
        assert_eq!(manage(home, &new, false).unwrap()["changed"], true);
    }

    // Windows PowerShell 5.1 commonly writes UTF-8 JSON with a BOM.
    #[test]
    fn accepts_bom_configuration_without_losing_user_hooks() {
        let bytes = b"\xef\xbb\xbf{\"hooks\":{\"Stop\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"echo user\"}]}]}}";
        let config = parse_config(Some(bytes)).unwrap();
        assert_eq!(
            config["hooks"]["Stop"][0]["hooks"][0]["command"],
            "echo user"
        );
    }
}
