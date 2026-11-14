use crate::model::now_ms;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;

/// Unknown fields are skipped during parsing, avoiding copies of large tool outputs.
#[derive(Deserialize, Serialize)]
struct HookInput {
    #[serde(skip_serializing)]
    transcript_path: String,
    hook_event_name: String,
    turn_id: Option<String>,
    agent_id: Option<String>,
    agent_type: Option<String>,
    model: Option<String>,
    tool_use_id: Option<String>,
    tool_name: Option<String>,
    token_usage: Option<Value>,
    status: Option<String>,
    response_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "present_value",
        skip_serializing_if = "Option::is_none"
    )]
    service_tier: Option<Value>,
    #[serde(
        default,
        deserialize_with = "present_value",
        skip_serializing_if = "Option::is_none"
    )]
    reasoning_effort: Option<Value>,
    #[serde(
        default,
        deserialize_with = "present_value",
        skip_serializing_if = "Option::is_none"
    )]
    effort: Option<Value>,
    trigger: Option<String>,
    #[serde(default, deserialize_with = "limited_input")]
    tool_input: Option<Value>,
}

fn present_value<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    Value::deserialize(deserializer).map(Some)
}

// Tool arguments are useful diagnostics, while outputs and prompts remain excluded.
fn limited_input<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    let input = Option::<Value>::deserialize(deserializer)?;
    Ok(input.map(|value| {
        let text = match value {
            Value::String(text) => text,
            other => serde_json::to_string_pretty(&other).unwrap_or_default(),
        };
        Value::String(text.chars().take(32 * 1024).collect())
    }))
}

/// Resolve the actual chat ID; parent session IDs do not uniquely identify forked chats.
fn transcript_thread(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let mut first = String::new();
    BufReader::new(File::open(path)?.take(1024 * 1024)).read_line(&mut first)?;
    let row: Value = serde_json::from_str(&first)?;
    if row["type"] != "session_meta" {
        return Err("Transcript does not start with session_meta".into());
    }
    row["payload"]["id"]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "Transcript session_meta is missing its chat id".into())
}

/// A lock serializes metadata-only appends from hooks running in different sessions.
pub fn emit(home: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let timestamp = now_ms();
    let input: HookInput = serde_json::from_reader(std::io::stdin().lock())?;
    let thread = transcript_thread(Path::new(&input.transcript_path))?;
    let mut payload = serde_json::to_value(input)?;
    payload["thread_id"] = json!(thread);
    let row = json!({"type": "monitor_hook", "timestamp_ms": timestamp, "payload": payload});
    let directory = home.join("monitor");
    std::fs::create_dir_all(&directory)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(directory.join("hooks.lock"))?;
    lock.lock_exclusive()?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join("hooks.jsonl"))?;
    writeln!(file, "{row}")?;
    FileExt::unlock(&lock)?;
    Ok(())
}

/// Generate a reviewable opt-in configuration without changing Codex configuration.
pub fn configuration(executable: &Path, home: &Path) -> Value {
    let command = format!(
        "\"{}\" --hook --codex-home \"{}\"",
        executable.display(),
        home.display()
    );
    // Codex may launch through either cmd or PowerShell on Windows.
    // Explicit cmd/call handles quoted executable paths in both shells.
    let windows_command = format!(
        "cmd.exe /d /c call \"{}\" --hook --codex-home \"{}\"",
        executable.display(),
        home.display()
    );
    let mut hooks = serde_json::Map::new();
    for event in [
        "UserPromptSubmit",
        "PreModelRequest",
        "PostModelRequest",
        "PreToolUse",
        "PostToolUse",
        "PermissionRequest",
        "Stop",
        "Interrupt",
        "PreCompact",
        "PostCompact",
    ] {
        hooks.insert(
            event.into(),
            json!([{"hooks": [{"type":"command", "command":command, "commandWindows":windows_command, "timeout":3}]}]),
        );
    }
    json!({"hooks": hooks})
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Configuration includes request and tool boundaries, using the same explicit source.
    #[test]
    fn configuration_has_correlated_boundaries() {
        let config = configuration(Path::new("monitor"), Path::new("source"));
        for event in [
            "PreModelRequest",
            "PostModelRequest",
            "PreToolUse",
            "PostToolUse",
        ] {
            assert_eq!(config["hooks"][event][0]["hooks"][0]["timeout"], 3);
            assert!(config["hooks"][event][0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .contains("--codex-home \"source\""));
        }
    }

    // Windows overrides explicitly invoke cmd rather than relying on PowerShell expression syntax.
    #[test]
    fn windows_command_supports_quoted_paths() {
        let config = configuration(
            Path::new("D:/Project With Spaces/monitor.exe"),
            Path::new("C:/User Data/.codex"),
        );
        let handler = &config["hooks"]["PreModelRequest"][0]["hooks"][0];
        assert_eq!(handler["commandWindows"], "cmd.exe /d /c call \"D:/Project With Spaces/monitor.exe\" --hook --codex-home \"C:/User Data/.codex\"");
        assert_eq!(handler["timeout"], 3);
    }

    /// Parent session IDs cannot silently stand in for the actual transcript identity.
    #[test]
    fn session_id_is_not_an_identity_fallback() {
        let input = json!({"hook_event_name":"Stop", "session_id":"parent"});
        assert!(serde_json::from_value::<HookInput>(input).is_err());
    }

    // Hook emitters preserve an explicit tier clear while keeping an absent tier absent.
    #[test]
    fn optional_tier_preserves_presence() {
        let base = json!({"transcript_path":"log","hook_event_name":"PreModelRequest"});
        let missing: HookInput = serde_json::from_value(base.clone()).unwrap();
        assert!(serde_json::to_value(missing)
            .unwrap()
            .get("service_tier")
            .is_none());
        let mut clear = base;
        clear["service_tier"] = Value::Null;
        let explicit: HookInput = serde_json::from_value(clear).unwrap();
        assert!(serde_json::to_value(explicit)
            .unwrap()
            .get("service_tier")
            .unwrap()
            .is_null());
    }

    // Hooks retain effort only when supplied, including an explicit unknown value.
    #[test]
    fn optional_effort_preserves_presence() {
        let base = json!({"transcript_path":"log","hook_event_name":"PreModelRequest"});
        let missing: HookInput = serde_json::from_value(base.clone()).unwrap();
        let missing = serde_json::to_value(missing).unwrap();
        assert!(missing.get("reasoning_effort").is_none());
        assert!(missing.get("effort").is_none());
        for value in [json!("high"), Value::Null] {
            let mut supplied = base.clone();
            supplied["reasoning_effort"] = value.clone();
            let input: HookInput = serde_json::from_value(supplied).unwrap();
            assert_eq!(
                serde_json::to_value(input).unwrap()["reasoning_effort"],
                value
            );
        }
    }
}
