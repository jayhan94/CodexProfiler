use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    pub sessions: Vec<Session>,
    pub error: Option<String>,
    pub updated_ms: i64,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub model: String,
    pub phase: String,
    pub turn_id: String,
    pub started_ms: Option<i64>,
    pub ended_ms: Option<i64>,
    pub last_ms: i64,
    pub output_tokens: u64,
    pub request_tps: Option<f64>,
    pub tools: Vec<Tool>,
    pub request_started_ms: Option<i64>,
    pub usage_total: u64,
    pub turn_base: u64,
    pub records: bool,
    pub model_boundary_ms: i64,
    pub seen_responses: Vec<String>,
    pub record_tokens: u64,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Tool {
    pub id: String,
    pub name: String,
    pub started_ms: i64,
    pub ended_ms: Option<i64>,
    pub status: String,
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn text(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap_or_default().to_owned()
}

fn timestamp(row: &Value) -> i64 {
    row["timestamp_ms"]
        .as_i64()
        .or_else(|| {
            row["timestamp"].as_str().and_then(|s| {
                chrono::DateTime::parse_from_rfc3339(s)
                    .ok()
                    .map(|t| t.timestamp_millis())
            })
        })
        .unwrap_or(0)
}

// Log observations never imply that the server received input or is reasoning.
pub fn apply(s: &mut Session, row: &Value) {
    let p = &row["payload"];
    let ts = timestamp(row);
    if ts > 0 && ts < s.last_ms {
        // Multiple sources can deliver boundaries late; retain timing without rewinding status.
        let same_turn = p["turn_id"].as_str() == Some(s.turn_id.as_str());
        if same_turn && (row["type"] == "monitor_hook" || row["type"] == "hook_event") {
            let phase = s.phase.clone();
            hook(s, p, ts);
            s.phase = phase;
            trim_tools(s);
        } else if same_turn && row["type"] == "event_msg" && p["type"] == "item_completed" {
            completed_item(s, p, ts);
        } else if row["type"] == "event_msg" && p["type"] == "token_count" {
            event(s, p, ts);
        } else if same_turn && row["type"] == "token_usage_record" {
            usage(s, p);
        }
        return;
    }
    s.last_ms = s.last_ms.max(ts);
    match row["type"].as_str().unwrap_or_default() {
        "session_meta" => {
            s.id = text(p, "id");
            if s.title.is_empty() {
                s.title = text(p, "cwd");
            }
        }
        "turn_context" => s.model = text(p, "model"),
        "event_msg" => event(s, p, ts),
        "response_item" => response(s, p, ts),
        "token_usage_record" => usage(s, p),
        "hook_event" | "monitor_hook" => hook(s, p, ts),
        _ => {}
    }
    trim_tools(s);
}

fn trim_tools(s: &mut Session) {
    if s.tools.len() > 100 {
        s.tools.drain(..s.tools.len() - 100);
    }
}

// Each new user turn owns its elapsed time and output counters.
fn begin(s: &mut Session, id: String, ts: i64) {
    if id == s.turn_id && s.started_ms.is_some() {
        return;
    }
    s.turn_id = id;
    s.started_ms = Some(ts);
    s.ended_ms = None;
    s.output_tokens = 0;
    s.request_tps = None;
    s.request_started_ms = None;
    s.turn_base = s.usage_total;
    s.records = false;
    s.model_boundary_ms = 0;
    s.seen_responses.clear();
    s.record_tokens = 0;
    s.tools.clear();
    s.phase = "等待后续事件（模型/网络状态未知）".into();
}

fn event(s: &mut Session, p: &Value, ts: i64) {
    if p["type"] != "task_started"
        && p["turn_id"]
            .as_str()
            .is_some_and(|id| !s.turn_id.is_empty() && id != s.turn_id)
    {
        return;
    }
    match p["type"].as_str().unwrap_or_default() {
        "task_started" => begin(s, text(p, "turn_id"), ts),
        "task_complete" | "task_completed" | "turn_aborted" => {
            s.ended_ms = Some(ts);
            s.phase = if p["type"] == "turn_aborted" {
                "已中断"
            } else {
                "已完成"
            }
            .into();
            for t in &mut s.tools {
                if t.ended_ms.is_none() {
                    t.status = "终止时未观测到结果".into();
                }
            }
        }
        "token_count" => {
            if let Some(total) = p["info"]["total_token_usage"]["output_tokens"].as_u64() {
                s.usage_total = s.usage_total.max(total);
                if s.started_ms.is_some_and(|start| ts >= start) {
                    s.output_tokens = s.output_tokens.max(total.saturating_sub(s.turn_base));
                }
            }
        }
        "item_completed" => completed_item(s, p, ts),
        _ => {}
    }
}

fn response(s: &mut Session, p: &Value, ts: i64) {
    match p["type"].as_str().unwrap_or_default() {
        "function_call" | "custom_tool_call" => {
            upsert_tool(
                s,
                Tool {
                    id: text(p, "call_id"),
                    name: text(p, "name"),
                    started_ms: ts,
                    status: "调用已发出".into(),
                    ..Tool::default()
                },
            );
            s.phase = "工具调用中（以日志为准）".into();
        }
        "function_call_output" | "custom_tool_call_output" => {
            finish_tool(s, &text(p, "call_id"), ts, "结果已就绪");
            if s.ended_ms.is_none() {
                s.phase = if s.tools.iter().any(|tool| tool.ended_ms.is_none()) {
                    "工具调用中（以日志为准）"
                } else {
                    "结果已就绪 / 等待后续事件"
                }
                .into();
            }
        }
        _ => {}
    }
}

fn upsert_tool(s: &mut Session, tool: Tool) {
    if let Some(t) = s.tools.iter_mut().find(|t| t.id == tool.id) {
        if t.ended_ms.is_some() && tool.ended_ms.is_none() {
            return;
        }
        *t = tool;
    } else {
        s.tools.push(tool);
    }
}

fn finish_tool(s: &mut Session, id: &str, ts: i64, status: &str) {
    if let Some(t) = s.tools.iter_mut().find(|t| t.id == id) {
        t.ended_ms = Some(ts);
        t.status = status.into();
    }
}

// Commands can be represented as argv arrays in desktop transcripts.
fn command_summary(value: &Value) -> String {
    let command = value.as_str().map(str::to_owned).unwrap_or_else(|| {
        value
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default()
    });
    command.chars().take(100).collect()
}

// Completed nested items expose actual tool names even inside code-mode exec.
fn completed_item(s: &mut Session, p: &Value, ts: i64) {
    let i = &p["item"];
    let kind = i["type"].as_str().unwrap_or_default();
    if kind == "UserMessage" {
        let content = &i["content"];
        if let Some(a) = content.as_array() {
            if let Some(t) = a.iter().find_map(|v| v["text"].as_str()) {
                s.title = t.chars().take(60).collect();
            }
        }
    }
    let name = match kind {
        "CommandExecution" => format!("Bash: {}", command_summary(&i["command"])),
        "McpToolCall" => format!("{} / {}", text(i, "server"), text(i, "tool")),
        "Extension" => text(i, "kind"),
        "DynamicToolCall" => text(i, "tool"),
        _ => return,
    };
    let start = p["started_at_ms"].as_i64();
    if let Some(start) = start {
        upsert_tool(
            s,
            Tool {
                id: text(i, "id"),
                name,
                started_ms: start,
                ended_ms: Some(p["completed_at_ms"].as_i64().unwrap_or(ts)),
                status: if i["exit_code"].as_i64().is_some_and(|n| n != 0) {
                    "失败"
                } else {
                    "结果已就绪"
                }
                .into(),
            },
        );
    }
}

// Cumulative per-turn usage is authoritative; response IDs deduplicate older formats.
fn usage(s: &mut Session, p: &Value) {
    if p["turn_id"].as_str().is_some_and(|id| id != s.turn_id) {
        return;
    }
    if let Some(total) = p["thread_token_usage"]["output_tokens"].as_u64() {
        s.usage_total = s.usage_total.max(total);
    }
    if let Some(total) = p["turn_token_usage"]["output_tokens"].as_u64() {
        s.output_tokens = s.output_tokens.max(total);
        s.record_tokens = s.record_tokens.max(total);
        remember_response(s, p);
        s.records = true;
        return;
    }
    let Some(id) = p["response_id"].as_str() else {
        return;
    };
    if s.seen_responses.iter().any(|seen| seen == id) {
        return;
    }
    if let Some(tokens) = p["usage"]["output_tokens"].as_u64() {
        remember_response(s, p);
        s.record_tokens = s.record_tokens.saturating_add(tokens);
        s.output_tokens = s.output_tokens.max(s.record_tokens);
        s.records = true;
    }
}

fn remember_response(s: &mut Session, p: &Value) {
    if let Some(id) = p["response_id"].as_str() {
        if !s.seen_responses.iter().any(|seen| seen == id) {
            s.seen_responses.push(id.to_owned());
            if s.seen_responses.len() > 128 {
                s.seen_responses.remove(0);
            }
        }
    }
}

// Optional hook records provide request timing; missing usage is never zero throughput.
fn hook(s: &mut Session, p: &Value, ts: i64) {
    let name = p["hook_event_name"].as_str().unwrap_or_default();
    if name != "UserPromptSubmit"
        && p["turn_id"]
            .as_str()
            .is_some_and(|id| !s.turn_id.is_empty() && id != s.turn_id)
    {
        return;
    }
    if let Some(model) = p["model"].as_str() {
        s.model = model.into();
    }
    match name {
        "UserPromptSubmit" => begin(s, text(p, "turn_id"), ts),
        "PreModelRequest" | "PostModelRequest" => model_hook(s, p, ts, name),
        "PreToolUse" => {
            s.phase = "工具执行中".into();
            upsert_tool(
                s,
                Tool {
                    id: text(p, "tool_use_id"),
                    name: text(p, "tool_name"),
                    started_ms: ts,
                    status: "工具执行中".into(),
                    ..Tool::default()
                },
            );
        }
        "PostToolUse" => {
            finish_tool(s, &text(p, "tool_use_id"), ts, "结果已就绪");
            if s.ended_ms.is_none() && s.tools.iter().all(|tool| tool.ended_ms.is_some()) {
                s.phase = "结果已就绪 / 等待后续事件".into();
            }
        }
        "PermissionRequest" => s.phase = "等待批准".into(),
        "Stop" | "Interrupt" => {
            s.ended_ms = Some(ts);
            s.phase = "已结束".into();
        }
        _ => {}
    }
}

// Hook chronology is independent of rollout delivery: a late completion cannot consume a newer start.
fn model_hook(s: &mut Session, p: &Value, ts: i64, name: &str) {
    if ts <= s.model_boundary_ms {
        return;
    }
    s.model_boundary_ms = ts;
    if name == "PreModelRequest" {
        s.request_started_ms = Some(ts);
        s.phase = "模型请求进行中（含网络和排队）".into();
        return;
    }
    let completed = p["status"]
        .as_str()
        .is_none_or(|status| status == "completed");
    s.request_tps = s
        .request_started_ms
        .take()
        .filter(|_| completed)
        .and_then(|start| {
            p["token_usage"]["output_tokens"]
                .as_u64()
                .filter(|_| ts > start)
                .map(|n| n as f64 * 1000.0 / (ts - start) as f64)
        });
    s.phase = if completed {
        "模型请求结束"
    } else {
        "模型请求失败或取消"
    }
    .into();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Actual per-turn usage excludes inputs and does not add reasoning twice.
    #[test]
    fn usage_and_new_turn_are_isolated() {
        let mut s = Session::default();
        apply(
            &mut s,
            &json!({"type":"event_msg","timestamp_ms":1000,
            "payload":{"type":"task_started","turn_id":"one"}}),
        );
        apply(
            &mut s,
            &json!({"type":"token_usage_record","timestamp_ms":1100,
            "payload":{"turn_id":"one","turn_token_usage":{"output_tokens":100},
                "usage":{"input_tokens":99999,"output_tokens":100,"reasoning_output_tokens":50}}}),
        );
        assert_eq!(s.output_tokens, 100);
        apply(
            &mut s,
            &json!({"type":"event_msg","timestamp_ms":2000,
            "payload":{"type":"task_started","turn_id":"two"}}),
        );
        assert_eq!(s.output_tokens, 0);
        apply(
            &mut s,
            &json!({"type":"event_msg","timestamp_ms":1000,
            "payload":{"type":"task_started","turn_id":"one"}}),
        );
        assert_eq!(s.turn_id, "two");
    }

    // A response boundary gives effective request TPS; tool wait belongs elsewhere.
    #[test]
    fn request_rate_requires_paired_boundaries() {
        let mut s = Session::default();
        for (name, ts, usage) in [
            ("PreModelRequest", 1000, Value::Null),
            ("PostModelRequest", 3000, json!({"output_tokens":100})),
        ] {
            apply(
                &mut s,
                &json!({"type":"monitor_hook","timestamp_ms":ts,
                "payload":{"hook_event_name":name,"token_usage":usage}}),
            );
        }
        assert_eq!(s.request_tps, Some(50.0));
        apply(
            &mut s,
            &json!({"type":"monitor_hook","timestamp_ms":4000,
            "payload":{"hook_event_name":"PostModelRequest","token_usage":{"output_tokens":100}}}),
        );
        assert_eq!(s.request_tps, None);
    }

    // Overlapping calls finish independently by ID rather than stack order.
    #[test]
    fn overlapping_tools_pair_by_id() {
        let mut s = Session::default();
        for (id, ts) in [("a", 1000), ("b", 1100)] {
            apply(
                &mut s,
                &json!({"type":"response_item","timestamp_ms":ts,
                "payload":{"type":"function_call","name":"Bash","call_id":id}}),
            );
        }
        apply(
            &mut s,
            &json!({"type":"response_item","timestamp_ms":1500,
            "payload":{"type":"function_call_output","call_id":"a"}}),
        );
        assert_eq!(s.tools[0].ended_ms, Some(1500));
        assert_eq!(s.tools[1].ended_ms, None);
    }

    // A delayed completion from an older attempt must not close the current request.
    #[test]
    fn late_model_hook_preserves_newer_request() {
        let mut s = Session::default();
        for (name, time) in [
            ("PreModelRequest", 1000),
            ("PostModelRequest", 2000),
            ("PreModelRequest", 3000),
            ("PostModelRequest", 2000),
        ] {
            apply(
                &mut s,
                &json!({"type":"monitor_hook","timestamp_ms":time,
                "payload":{"hook_event_name":name,"token_usage":{"output_tokens":100}}}),
            );
        }
        assert_eq!(s.request_started_ms, Some(3000));
        apply(
            &mut s,
            &json!({"type":"monitor_hook","timestamp_ms":5000,
            "payload":{"hook_event_name":"PostModelRequest","status":"completed",
                       "token_usage":{"output_tokens":200}}}),
        );
        assert_eq!(s.request_tps, Some(100.0));
    }

    // A late start from the hook stream cannot reopen a call already finished in the rollout.
    #[test]
    fn late_tool_start_cannot_reopen_finished_call() {
        let mut s = Session::default();
        for (name, time) in [
            ("PreToolUse", 1000),
            ("PostToolUse", 2000),
            ("PreToolUse", 1000),
        ] {
            apply(
                &mut s,
                &json!({"type":"monitor_hook","timestamp_ms":time,
                "payload":{"turn_id":"","hook_event_name":name,"tool_use_id":"call-a",
                           "tool_name":"Bash"}}),
            );
        }
        assert_eq!(s.tools[0].ended_ms, Some(2000));
        assert_eq!(s.phase, "结果已就绪 / 等待后续事件");
    }

    // The same response's legacy counter and structured usage must not be added together.
    #[test]
    fn overlapping_usage_formats_do_not_double_count() {
        let mut s = Session::default();
        apply(
            &mut s,
            &json!({"type":"event_msg","timestamp_ms":1000,
            "payload":{"type":"task_started","turn_id":"turn-a"}}),
        );
        apply(
            &mut s,
            &json!({"type":"event_msg","timestamp_ms":2000,
            "payload":{"type":"token_count","info":{"total_token_usage":{"output_tokens":100}}}}),
        );
        let usage = json!({"type":"token_usage_record","timestamp_ms":2100,
            "payload":{"turn_id":"turn-a","response_id":"response-a",
                       "usage":{"output_tokens":100,"reasoning_output_tokens":40}}});
        apply(&mut s, &usage);
        apply(&mut s, &usage);
        assert_eq!(s.output_tokens, 100);
    }

    // A stop event tied to a previous turn must not end the current task.
    #[test]
    fn mismatched_turn_stop_is_ignored() {
        let mut s = Session::default();
        apply(
            &mut s,
            &json!({"type":"event_msg","timestamp_ms":1000,
            "payload":{"type":"task_started","turn_id":"new"}}),
        );
        apply(
            &mut s,
            &json!({"type":"monitor_hook","timestamp_ms":2000,
            "payload":{"hook_event_name":"Stop","turn_id":"old"}}),
        );
        assert_eq!(s.ended_ms, None);
    }
}
