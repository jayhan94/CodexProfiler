use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Snapshot {
    pub sessions: Vec<Session>,
    pub costs: Vec<CostMember>,
    pub error: Option<String>,
    pub updated_ms: i64,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub is_internal: bool,
    pub parent_id: String,
    pub thread_kind: String,
    pub agent_name: String,
    pub model: String,
    #[serde(default = "standard_tier")]
    pub service_tier: String,
    #[serde(default = "assumed_tier_source")]
    pub service_tier_source: String,
    pub tier_history: Vec<(i64, String, String)>,
    pub service_tier_updated_ms: i64,
    pub compactions: Vec<Compaction>,
    pub active_compaction: Option<String>,
    pub phase: String,
    pub turn_id: String,
    pub started_ms: Option<i64>,
    pub ended_ms: Option<i64>,
    pub last_ms: i64,
    pub output_tokens: u64,
    pub request_tps: Option<f64>,
    pub tools: Vec<Tool>,
    pub agent_events: Vec<AgentEvent>,
    pub requests: Vec<ModelRequest>,
    pub cost: CostSummary,
    pub cost_coverage_incomplete: Option<bool>,
    pub cost_coverage: Vec<CostCoverageRequest>,
    pub cost_coverage_known: Vec<CostCoverageUsage>,
    #[serde(skip)]
    pub cost_coverage_runtime: CostCoverageRuntime,
    pub billing_seen: Vec<String>,
    pub billing_quotes: Vec<BillingQuote>,
    pub billing_models: Vec<(String, String)>,
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
    pub details: String,
    pub compaction_id: Option<String>,
    pub started_ms: i64,
    pub ended_ms: Option<i64>,
    pub status: String,
}

/// Agent lifecycle notifications are points in time, not measured tool executions.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentEvent {
    pub id: String,
    pub kind: String,
    pub thread_id: String,
    pub path: String,
    pub at_ms: i64,
}

/// Recognize both namespaced desktop calls and standalone collaboration tool names.
pub fn is_agent_tool(tool: &Tool) -> bool {
    let name = tool.name.strip_prefix("collaboration");
    name.is_some()
        || matches!(
            tool.name.as_str(),
            "spawn_agent"
                | "send_message"
                | "followup_task"
                | "list_agents"
                | "wait_agent"
                | "interrupt_agent"
                | "resume_agent"
                | "close_agent"
        )
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelRequest {
    pub id: String,
    pub model: String,
    #[serde(default = "standard_tier")]
    pub service_tier: String,
    #[serde(default = "assumed_tier_source")]
    pub service_tier_source: String,
    pub compaction_id: Option<String>,
    pub started_ms: Option<i64>,
    pub ended_ms: Option<i64>,
    pub output_tokens: Option<u64>,
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub cache_write_input_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
    pub status: String,
    pub source: String,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Compaction {
    pub id: String,
    pub started_ms: Option<i64>,
    pub ended_ms: Option<i64>,
    pub trigger: String,
    pub status: String,
    pub hook_pending: bool,
}

fn standard_tier() -> String {
    "standard".into()
}
fn assumed_tier_source() -> String {
    "assumed".into()
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BillingQuote {
    response_id: String,
    turn_id: String,
    timestamp_ms: i64,
    request_started_ms: Option<i64>,
    model: String,
    tier: String,
    tier_source: String,
    input: Option<u64>,
    cached: Option<u64>,
    written: Option<u64>,
    output: Option<u64>,
    price: Option<crate::pricing::CostBreakdown>,
}

/// Canonical observed request costs across every turn; input includes cache subsets.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CostSummary {
    pub uncached_usd: f64,
    pub cached_usd: f64,
    pub write_usd: f64,
    pub output_usd: f64,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    pub output_tokens: u64,
    pub priced_requests: u64,
    pub unpriced_requests: u64,
}

/// Small persistent hook evidence, independent of the bounded current-turn timeline.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CostCoverageRequest {
    key: String,
    turn_id: String,
    agent_id: String,
    started_ms: Option<i64>,
    ended_ms: Option<i64>,
    response_id: Option<String>,
    requires_usage: bool,
    matched_response_id: Option<String>,
}

/// A canonical record's identity and scope survive eviction from the bounded price ledger.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CostCoverageUsage {
    response_id: String,
    turn_id: String,
    timestamp_ms: i64,
}

/// Rebuilt once after deserialization; indexes never increase the persisted cache size.
#[derive(Clone, Default)]
pub struct CostCoverageRuntime {
    requests: std::collections::HashMap<String, Vec<usize>>,
    known: std::collections::HashMap<String, Vec<usize>>,
    known_ids: std::collections::HashMap<String, usize>,
    ended_ids: std::collections::HashMap<String, usize>,
    indexed_requests: usize,
    indexed_known: usize,
    initialized: bool,
    pending: usize,
}

/// Persisted cost and ownership without loading a session's timeline into the UI.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CostMember {
    pub id: String,
    pub parent_id: String,
    pub thread_kind: String,
    pub title: String,
    pub agent_name: String,
    pub cost: Option<CostSummary>,
    pub partial: bool,
}

impl CostMember {
    pub fn from_session(s: &Session) -> Self {
        Self {
            id: s.id.clone(),
            parent_id: s.parent_id.clone(),
            thread_kind: s.thread_kind.clone(),
            title: s.title.clone(),
            agent_name: s.agent_name.clone(),
            cost: Some(s.cost.clone()),
            partial: s.cost_coverage_incomplete != Some(false),
        }
    }
}

/// Only a new source or a complete historical hook scan can establish verified empty coverage.
pub(crate) fn reset_cost_coverage(s: &mut Session) {
    s.cost_coverage.clear();
    s.cost_coverage_known.clear();
    s.cost_coverage_incomplete = Some(false);
    s.cost_coverage_runtime = CostCoverageRuntime {
        initialized: true,
        ..CostCoverageRuntime::default()
    };
}

/// Move verified hook observations into the real cached session without touching its prices or metrics.
pub(crate) fn copy_cost_coverage(target: &mut Session, observed: &Session) {
    target.cost_coverage = observed.cost_coverage.clone();
    target.cost_coverage_known = observed.cost_coverage_known.clone();
    target.cost_coverage_incomplete = observed.cost_coverage_incomplete;
    target.cost_coverage_runtime = CostCoverageRuntime::default();
}

fn index_cost_coverage(s: &mut Session) {
    let runtime = &mut s.cost_coverage_runtime;
    if runtime.indexed_requests > s.cost_coverage.len()
        || runtime.indexed_known > s.cost_coverage_known.len()
    {
        *runtime = CostCoverageRuntime::default();
    }
    if !runtime.initialized {
        runtime.pending = s
            .cost_coverage
            .iter()
            .filter(|r| r.requires_usage && r.matched_response_id.is_none())
            .count();
        runtime.initialized = true;
    }
    for (index, request) in s
        .cost_coverage
        .iter()
        .enumerate()
        .skip(runtime.indexed_requests)
    {
        runtime
            .requests
            .entry(request.turn_id.clone())
            .or_default()
            .push(index);
        if request.ended_ms.is_some() {
            if let Some(id) = &request.response_id {
                runtime.ended_ids.insert(id.clone(), index);
            }
        }
    }
    runtime.indexed_requests = s.cost_coverage.len();
    for (index, usage) in s
        .cost_coverage_known
        .iter()
        .enumerate()
        .skip(runtime.indexed_known)
    {
        runtime
            .known
            .entry(usage.turn_id.clone())
            .or_default()
            .push(index);
        runtime.known_ids.insert(usage.response_id.clone(), index);
    }
    runtime.indexed_known = s.cost_coverage_known.len();
}

fn update_coverage_flag(s: &mut Session) {
    if s.cost_coverage_incomplete.is_some() {
        s.cost_coverage_incomplete = Some(s.cost_coverage_runtime.pending > 0);
    }
}

/// Record full hook rows before live-turn filtering so late prior-turn usage can settle them.
pub(crate) fn observe_cost_coverage(s: &mut Session, row: &Value) {
    record_cost_coverage(s, row);
}

fn record_cost_coverage(s: &mut Session, row: &Value) -> bool {
    if !matches!(row["type"].as_str(), Some("monitor_hook" | "hook_event")) {
        return false;
    }
    let p = &row["payload"];
    if p["thread_id"]
        .as_str()
        .is_some_and(|id| !s.id.is_empty() && id != s.id)
    {
        return false;
    }
    let name = p["hook_event_name"].as_str().unwrap_or_default();
    if !matches!(name, "PreModelRequest" | "PostModelRequest") {
        return false;
    }
    index_cost_coverage(s);
    let turn = text(p, "turn_id");
    let agent = text(p, "agent_id");
    let at = timestamp(row);
    let response = p["response_id"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_owned);
    if name == "PreModelRequest" {
        let key = format!("pre:{turn}:{agent}:{at}");
        if !s
            .cost_coverage_runtime
            .requests
            .get(&turn)
            .into_iter()
            .flatten()
            .any(|index| s.cost_coverage[*index].key == key)
        {
            s.cost_coverage.push(CostCoverageRequest {
                key,
                turn_id: turn,
                agent_id: agent,
                started_ms: Some(at),
                response_id: response,
                ..CostCoverageRequest::default()
            });
            return true;
        }
        return false;
    }
    if response
        .as_ref()
        .is_some_and(|id| s.cost_coverage_runtime.ended_ids.contains_key(id))
        || s.cost_coverage_runtime
            .requests
            .get(&turn)
            .into_iter()
            .flatten()
            .any(|index| {
                let r = &s.cost_coverage[*index];
                r.agent_id == agent && r.ended_ms == Some(at) && r.response_id == response
            })
    {
        return false;
    }
    finish_cost_coverage(s, p, turn, agent, at, response);
    true
}

fn finish_cost_coverage(
    s: &mut Session,
    p: &Value,
    turn: String,
    agent: String,
    at: i64,
    response: Option<String>,
) {
    let candidates: Vec<_> = s
        .cost_coverage_runtime
        .requests
        .get(&turn)
        .into_iter()
        .flatten()
        .copied()
        .filter(|index| {
            let r = &s.cost_coverage[*index];
            r.ended_ms.is_none()
                && r.agent_id == agent
                && r.started_ms.is_some_and(|start| start <= at)
                && (response.is_none() || r.response_id.is_none() || r.response_id == response)
        })
        .collect();
    let explicit = response.as_ref().and_then(|id| {
        candidates
            .iter()
            .copied()
            .find(|index| s.cost_coverage[*index].response_id.as_ref() == Some(id))
    });
    let index = explicit.or_else(|| {
        if candidates.len() == 1 {
            Some(candidates[0])
        } else {
            None
        }
    });
    let index = index.unwrap_or_else(|| {
        s.cost_coverage.push(CostCoverageRequest {
            key: format!("post:{turn}:{agent}:{at}"),
            turn_id: turn,
            agent_id: agent,
            ..CostCoverageRequest::default()
        });
        s.cost_coverage.len() - 1
    });
    let request = &mut s.cost_coverage[index];
    let was_pending = request.requires_usage && request.matched_response_id.is_none();
    request.ended_ms = Some(at);
    request.response_id = response.or(request.response_id.take());
    let completed = p["status"]
        .as_str()
        .is_none_or(|status| status == "completed");
    let has_usage = p["token_usage"].as_object().is_some_and(|usage| {
        usage
            .values()
            .any(|count| count.as_u64().is_some_and(|n| n > 0))
    });
    request.requires_usage = completed || has_usage;
    let pending = request.requires_usage && request.matched_response_id.is_none();
    if let Some(id) = &request.response_id {
        s.cost_coverage_runtime.ended_ids.insert(id.clone(), index);
    }
    s.cost_coverage_runtime.pending = s
        .cost_coverage_runtime
        .pending
        .saturating_sub(usize::from(was_pending))
        + usize::from(pending);
    update_coverage_flag(s);
}

/// A full refresh is reserved for historical backfill, not every tool notification.
pub(crate) fn refresh_cost_coverage(s: &mut Session) {
    refresh_coverage_scope(s, None);
}

fn coverage_known_in_scope(
    s: &Session,
    turn: Option<&str>,
) -> std::collections::HashMap<String, CostCoverageUsage> {
    use std::collections::{HashMap, HashSet};
    let indices: Vec<_> = match turn {
        None => (0..s.cost_coverage_known.len()).collect(),
        Some("") => s
            .cost_coverage_runtime
            .requests
            .get("")
            .into_iter()
            .flatten()
            .filter_map(|index| s.cost_coverage[*index].response_id.as_ref())
            .filter_map(|id| s.cost_coverage_runtime.known_ids.get(id).copied())
            .collect(),
        Some(turn) => s
            .cost_coverage_runtime
            .known
            .get(turn)
            .cloned()
            .unwrap_or_default(),
    };
    let requested: HashSet<_> = s
        .cost_coverage_runtime
        .requests
        .get("")
        .into_iter()
        .flatten()
        .filter_map(|index| s.cost_coverage[*index].response_id.as_deref())
        .collect();
    let mut known: HashMap<_, _> = indices
        .into_iter()
        .map(|index| {
            let usage = &s.cost_coverage_known[index];
            (usage.response_id.clone(), usage.clone())
        })
        .collect();
    for quote in &s.billing_quotes {
        let include = match turn {
            None => true,
            Some("") => requested.contains(quote.response_id.as_str()),
            Some(turn) => quote.turn_id == turn,
        };
        if include {
            known
                .entry(quote.response_id.clone())
                .or_insert_with(|| CostCoverageUsage {
                    response_id: quote.response_id.clone(),
                    turn_id: quote.turn_id.clone(),
                    timestamp_ms: quote.timestamp_ms,
                });
        }
    }
    known
}

fn coverage_request_scope(
    s: &Session,
    turn: Option<&str>,
    known: &std::collections::HashMap<String, CostCoverageUsage>,
) -> Vec<usize> {
    let Some(turn) = turn else {
        return (0..s.cost_coverage.len()).collect();
    };
    let mut indices = s
        .cost_coverage_runtime
        .requests
        .get(turn)
        .cloned()
        .unwrap_or_default();
    if !turn.is_empty() {
        indices.extend(
            s.cost_coverage_runtime
                .requests
                .get("")
                .into_iter()
                .flatten()
                .filter(|index| {
                    s.cost_coverage[**index]
                        .response_id
                        .as_ref()
                        .is_some_and(|id| known.contains_key(id))
                })
                .copied(),
        );
    }
    indices
}

/// Re-match only the affected turn; other turns retain their proven matches and pending count.
fn refresh_coverage_scope(s: &mut Session, turn: Option<&str>) {
    use std::collections::HashSet;
    index_cost_coverage(s);
    if turn.is_some_and(|turn| {
        !s.cost_coverage_runtime.requests.contains_key(turn)
            && !s.cost_coverage_runtime.requests.contains_key("")
    }) {
        return;
    }
    let known = coverage_known_in_scope(s, turn);
    let indices = coverage_request_scope(s, turn, &known);
    let (matches, claims) = {
        let requests: Vec<_> = indices
            .iter()
            .map(|index| &s.cost_coverage[*index])
            .collect();
        anonymous_coverage_matches(&requests, &known)
    };
    let mut new_proofs = HashSet::new();
    let mut old_pending = 0;
    let mut new_pending = 0;
    for (slot, index) in indices.into_iter().enumerate() {
        let request = &mut s.cost_coverage[index];
        old_pending += usize::from(request.requires_usage && request.matched_response_id.is_none());
        request.matched_response_id = None;
        if !request.requires_usage {
            continue;
        }
        let proof = if let Some(id) = &request.response_id {
            known
                .get(id)
                .filter(|usage| request.turn_id.is_empty() || request.turn_id == usage.turn_id)
        } else {
            matches[slot]
                .as_ref()
                .filter(|id| claims.get(*id) == Some(&1))
                .and_then(|id| known.get(id))
        };
        if let Some(proof) = proof {
            request.matched_response_id = Some(proof.response_id.clone());
            if !s
                .cost_coverage_runtime
                .known_ids
                .contains_key(&proof.response_id)
                && new_proofs.insert(proof.response_id.clone())
            {
                s.cost_coverage_known.push(proof.clone());
            }
        } else {
            new_pending += 1;
        }
    }
    s.cost_coverage_runtime.pending =
        s.cost_coverage_runtime.pending.saturating_sub(old_pending) + new_pending;
    update_coverage_flag(s);
}

/// Anonymous intervals require unique proof in both directions; concurrent ambiguity stays partial.
fn anonymous_coverage_matches(
    requests: &[&CostCoverageRequest],
    known: &std::collections::HashMap<String, CostCoverageUsage>,
) -> (
    Vec<Option<String>>,
    std::collections::HashMap<String, usize>,
) {
    use std::collections::{HashMap, HashSet};
    let reserved: HashSet<_> = requests
        .iter()
        .filter_map(|r| r.response_id.as_ref())
        .collect();
    let mut by_turn: HashMap<&str, Vec<&CostCoverageUsage>> = HashMap::new();
    for usage in known.values() {
        if !reserved.contains(&usage.response_id) {
            by_turn.entry(&usage.turn_id).or_default().push(usage);
        }
    }
    let mut matches = vec![None; requests.len()];
    let mut claims = HashMap::new();
    for (index, request) in requests.iter().enumerate() {
        if !request.requires_usage || request.response_id.is_some() || request.turn_id.is_empty() {
            continue;
        }
        let Some((start, end)) = request
            .started_ms
            .zip(request.ended_ms)
            .filter(|(a, b)| *a > 0 && b >= a)
        else {
            continue;
        };
        let mut count = 0;
        if let Some(usages) = by_turn.get(request.turn_id.as_str()) {
            for usage in usages
                .iter()
                .filter(|u| u.timestamp_ms >= start && u.timestamp_ms <= end)
            {
                *claims.entry(usage.response_id.clone()).or_insert(0) += 1;
                count += 1;
                matches[index] = if count == 1 {
                    Some(usage.response_id.clone())
                } else {
                    None
                };
            }
        }
    }
    (matches, claims)
}

impl CostSummary {
    pub fn total(&self) -> f64 {
        self.uncached_usd + self.cached_usd + self.write_usd + self.output_usd
    }
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
    // A hook may arrive before the transcript's first metadata record.
    if s.id.is_empty() && matches!(row["type"].as_str(), Some("monitor_hook" | "hook_event")) {
        if let Some(id) = p["thread_id"].as_str().filter(|id| !id.is_empty()) {
            s.id = id.into();
        }
    }
    if row["type"] == "session_meta" && ts > 0 && ts < s.last_ms {
        supplement_metadata(s, row);
        return;
    }
    if record_cost_coverage(s, row) {
        refresh_coverage_scope(s, Some(p["turn_id"].as_str().unwrap_or_default()));
    }
    // Billing follows canonical response ownership, independently of the displayed latest turn.
    remember_billing_model(s, row);
    observe_service_tier(s, row, ts);
    if row["type"] == "token_usage_record" && account_usage(s, p, ts) {
        refresh_coverage_scope(s, Some(p["turn_id"].as_str().unwrap_or_default()));
    }
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
            usage(s, p, ts);
        } else if row["type"] == "compacted" && s.started_ms.is_some_and(|start| ts >= start) {
            compaction_end(s, p, ts, false);
        }
        trim_tools(s);
        return;
    }
    s.last_ms = s.last_ms.max(ts);
    match row["type"].as_str().unwrap_or_default() {
        "session_meta" => metadata(s, p),
        "turn_context" => {
            s.model = text(p, "model");
            if s.model == "codex-autoreview" {
                s.is_internal = true;
            }
        }
        "event_msg" => event(s, p, ts),
        "response_item" => response(s, p, ts),
        "token_usage_record" => usage(s, p, ts),
        "hook_event" | "monitor_hook" => hook(s, p, ts),
        "compacted" => compaction_end(s, p, ts, false),
        _ => {}
    }
    trim_tools(s);
}

fn trim_tools(s: &mut Session) {
    if s.agent_events.len() > 100 {
        s.agent_events.drain(..s.agent_events.len() - 100);
    }
    if s.compactions.len() > 50 {
        s.compactions.drain(..s.compactions.len() - 50);
    }
    if s.requests.len() > 100 {
        s.requests.drain(..s.requests.len() - 100);
    }
    if s.tools.len() > 100 {
        s.tools.drain(..s.tools.len() - 100);
    }
}

/// Observe only explicit tier fields. Missing tier metadata never turns Fast off.
fn observe_service_tier(s: &mut Session, row: &Value, ts: i64) {
    if s.service_tier.is_empty() {
        s.service_tier = standard_tier();
        s.service_tier_source = assumed_tier_source();
    }
    let p = &row["payload"];
    let mut reported = None;
    if p["type"] == "thread_settings_applied" {
        if let Some(settings) = p["thread_settings"].as_object() {
            if let Some(value) = settings.get("service_tier") {
                reported = Some(value);
            }
        }
    }
    if row["type"] == "turn_context" {
        if let Some(value) = p.get("service_tier") {
            reported = Some(value);
        }
    }
    let Some(value) = reported else {
        return;
    };
    let tier = value.as_str().unwrap_or("standard").to_lowercase();
    let mut changed = false;
    if !s
        .tier_history
        .iter()
        .any(|(time, old, _)| *time == ts && old == &tier)
    {
        changed = true;
        s.tier_history.push((ts, tier.clone(), "settings".into()));
        s.tier_history.sort_by_key(|(time, _, _)| *time);
        if s.tier_history.len() > 512 {
            s.tier_history.remove(0);
        }
    }
    if ts >= s.service_tier_updated_ms {
        s.service_tier = tier;
        s.service_tier_source = "reported".into();
        s.service_tier_updated_ms = ts;
    }
    if changed {
        reconcile_service_history(s);
    }
}

fn tier_at(s: &Session, ts: i64) -> (String, String) {
    s.tier_history
        .iter()
        .rev()
        .find(|(time, _, _)| *time <= ts)
        .map(|(_, tier, source)| (tier.clone(), source.clone()))
        .unwrap_or_else(|| (standard_tier(), assumed_tier_source()))
}

fn request_tier(s: &Session, p: &Value, ts: i64) -> (String, String) {
    if let Some(value) = p.get("service_tier") {
        return (
            value.as_str().unwrap_or("standard").to_lowercase(),
            "request".into(),
        );
    }
    tier_at(s, ts)
}

/// Chunked sources may deliver settings late; re-evaluate inferred tiers at each observed start.
fn reconcile_service_history(s: &mut Session) {
    for index in 0..s.requests.len() {
        let request = &s.requests[index];
        if matches!(request.service_tier_source.as_str(), "request" | "response") {
            continue;
        }
        let Some(start) = request.started_ms else {
            continue;
        };
        let (tier, source) = tier_at(s, start);
        s.requests[index].service_tier = tier;
        s.requests[index].service_tier_source = source;
        fill_request_cost(&mut s.requests[index], &Value::Null);
        reconcile_request_bill(s, index);
    }
    // The ledger outlives the visible request buffer, so older observed starts remain correctable.
    for index in 0..s.billing_quotes.len() {
        let mut quote = s.billing_quotes[index].clone();
        if matches!(quote.tier_source.as_str(), "request" | "response") {
            continue;
        }
        let (tier, source) = tier_at(s, quote.request_started_ms.unwrap_or(quote.timestamp_ms));
        if quote.tier == tier && quote.tier_source == source {
            continue;
        }
        let old = quote.price;
        quote.tier = tier;
        quote.tier_source = source;
        quote.price = quote_price(&quote);
        adjust_quote_total(&mut s.cost, old, quote.price);
        s.billing_quotes[index] = quote;
    }
}

/// A request's start snapshot takes precedence over a later session setting.
fn usage_tier(s: &Session, p: &Value, ts: i64) -> (String, String) {
    if let Some(tier) = p["service_tier"].as_str() {
        return (tier.to_lowercase(), "response".into());
    }
    let id = p["response_id"].as_str();
    if let Some(r) = s.requests.iter().find(|r| Some(r.id.as_str()) == id) {
        return (r.service_tier.clone(), r.service_tier_source.clone());
    }
    let candidates: Vec<_> = s
        .requests
        .iter()
        .filter(|r| {
            r.source != "usage"
                && r.started_ms.is_some_and(|start| ts >= start)
                && r.ended_ms.is_none_or(|end| ts <= end)
        })
        .collect();
    if candidates.len() == 1 {
        return (
            candidates[0].service_tier.clone(),
            candidates[0].service_tier_source.clone(),
        );
    }
    tier_at(s, ts)
}

fn usage_start(s: &Session, p: &Value, ts: i64) -> Option<i64> {
    let id = p["response_id"].as_str();
    if let Some(request) = s.requests.iter().find(|r| Some(r.id.as_str()) == id) {
        return request.started_ms;
    }
    let candidates: Vec<_> = s
        .requests
        .iter()
        .filter(|r| {
            r.source != "usage"
                && r.started_ms.is_some_and(|start| ts >= start)
                && r.ended_ms.is_none_or(|end| ts <= end)
        })
        .collect();
    if candidates.len() == 1 {
        candidates[0].started_ms
    } else {
        None
    }
}

/// Membership uses observed starts and intervals; end-only compaction does not invent children.
fn compaction_at(s: &Session, ts: i64) -> Option<String> {
    s.compactions
        .iter()
        .rev()
        .find(|c| {
            c.started_ms.is_some_and(|start| ts >= start) && c.ended_ms.is_none_or(|end| ts <= end)
        })
        .map(|c| c.id.clone())
}

fn compaction_start(s: &mut Session, p: &Value, ts: i64) {
    let id = format!("compact:{ts}");
    if s.compactions.iter().any(|c| c.id == id) {
        return;
    }
    s.compactions.push(Compaction {
        id: id.clone(),
        started_ms: Some(ts),
        trigger: p["trigger"].as_str().unwrap_or("unknown").into(),
        status: "进行中".into(),
        hook_pending: true,
        ..Compaction::default()
    });
    s.active_compaction = Some(id);
    s.phase = "正在压缩上下文".into();
}

fn compaction_end(s: &mut Session, p: &Value, ts: i64, from_hook: bool) {
    let index = s
        .active_compaction
        .as_ref()
        .and_then(|id| s.compactions.iter().position(|c| &c.id == id))
        .or_else(|| {
            if from_hook {
                s.compactions.iter().rposition(|c| c.hook_pending)
            } else {
                s.compactions.iter().position(|c| {
                    c.started_ms.is_some_and(|start| ts >= start)
                        && c.ended_ms.is_some_and(|end| ts <= end)
                })
            }
        });
    if let Some(index) = index {
        let c = &mut s.compactions[index];
        if from_hook || c.ended_ms.is_none() {
            c.ended_ms = Some(ts);
        }
        c.status = "已完成".into();
        if from_hook {
            c.hook_pending = false;
        }
        if let Some(trigger) = p["trigger"].as_str() {
            c.trigger = trigger.into();
        }
        let id = c.id.clone();
        reconcile_compaction(s, &id);
    } else {
        let id = format!("compact:end:{ts}");
        if !s.compactions.iter().any(|c| c.id == id) {
            s.compactions.push(Compaction {
                id,
                ended_ms: Some(ts),
                trigger: p["trigger"].as_str().unwrap_or("unknown").into(),
                status: "已完成（开始时间未知）".into(),
                ..Compaction::default()
            });
        }
    }
    s.active_compaction = None;
    s.phase = "上下文压缩完成 / 等待后续事件".into();
}

/// A complete hook interval can reconcile a prior log-only completion and its child observations.
fn reconcile_compaction(s: &mut Session, id: &str) {
    let Some(c) = s.compactions.iter().find(|c| c.id == id) else {
        return;
    };
    let Some((start, end)) = c.started_ms.zip(c.ended_ms) else {
        return;
    };
    let candidates: Vec<_> = s
        .compactions
        .iter()
        .filter(|c| {
            c.started_ms.is_none() && c.ended_ms.is_some_and(|time| time >= start && time <= end)
        })
        .map(|c| c.id.clone())
        .collect();
    if candidates.len() == 1 {
        s.compactions.retain(|c| c.id != candidates[0]);
    }
    for tool in &mut s.tools {
        if tool.compaction_id.is_none() && tool.started_ms >= start && tool.started_ms <= end {
            tool.compaction_id = Some(id.into());
        }
    }
    for request in &mut s.requests {
        let observed = request.started_ms.or(request.ended_ms);
        if request.compaction_id.is_none()
            && observed.is_some_and(|time| time >= start && time <= end)
        {
            request.compaction_id = Some(id.into());
        }
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
    s.agent_events.clear();
    s.requests.clear();
    s.compactions.clear();
    s.active_compaction = None;
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
                    name: if p["namespace"] == "collaboration" {
                        format!("collaboration.{}", text(p, "name"))
                    } else {
                        text(p, "name")
                    },
                    details: call_details(p),
                    compaction_id: compaction_at(s, ts),
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
        let previous = t.details.clone();
        let compaction = t.compaction_id.clone();
        *t = tool;
        if t.compaction_id.is_none() {
            t.compaction_id = compaction;
        }
        if t.details.is_empty() {
            t.details = previous;
        }
    } else {
        s.tools.push(tool);
    }
}

fn finish_tool(s: &mut Session, id: &str, ts: i64, status: &str) {
    if let Some(t) = s.tools.iter_mut().find(|t| t.id == id) {
        // Result delivery can follow a recorded execution end; keep that precise boundary.
        if t.ended_ms.is_none() {
            t.ended_ms = Some(ts);
            t.status = status.into();
        }
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
    if kind == "SubAgentActivity" {
        agent_activity(s, i, ts);
        return;
    }
    if kind == "ContextCompaction" {
        compaction_end(s, p, ts, false);
        return;
    }
    if kind == "UserMessage" && s.title.is_empty() {
        if let Some(a) = i["content"].as_array() {
            if let Some(t) = a
                .iter()
                .filter_map(|v| v["text"].as_str())
                .find(|t| genuine_prompt(t))
            {
                s.title = t.trim().chars().take(80).collect();
            }
        }
    }
    let name = match kind {
        "CommandExecution" => format!("Bash: {}", command_summary(&i["command"])),
        "McpToolCall" => format!("{} / {}", text(i, "server"), text(i, "tool")),
        "Extension" => text(i, "kind"),
        "DynamicToolCall" => text(i, "tool"),
        "CollabAgentToolCall" => format!("collaboration.{}", text(i, "tool")),
        _ => return,
    };
    let start = p["started_at_ms"].as_i64();
    if let Some(start) = start {
        upsert_tool(
            s,
            Tool {
                id: text(i, "id"),
                name,
                compaction_id: compaction_at(s, start),
                details: if kind == "CommandExecution" {
                    detail(&i["command"])
                } else if kind == "CollabAgentToolCall" {
                    let targets = detail(&i["receiver_thread_ids"]);
                    let previous = s
                        .tools
                        .iter()
                        .find(|t| t.id == i["id"])
                        .map(|t| t.details.as_str())
                        .unwrap_or_default();
                    let target_line = format!("子会话：{targets}");
                    if previous.ends_with(&target_line) {
                        previous.to_owned()
                    } else {
                        format!("{previous}\n{target_line}").trim().to_owned()
                    }
                } else {
                    call_details(i)
                },
                started_ms: start,
                ended_ms: Some(p["completed_at_ms"].as_i64().unwrap_or(ts)),
                status: if i["status"] == "failed"
                    || i["exit_code"].as_i64().is_some_and(|n| n != 0)
                {
                    "失败"
                } else {
                    "结果已就绪"
                }
                .into(),
            },
        );
    }
}

/// Keep lifecycle notifications separate; the matching call remains the sole tool record.
fn agent_activity(s: &mut Session, item: &Value, ts: i64) {
    let id = text(item, "id");
    if id.is_empty()
        || !matches!(
            item["kind"].as_str(),
            Some("started" | "interacted" | "completed")
        )
    {
        return;
    }
    let event = AgentEvent {
        id: id.clone(),
        kind: text(item, "kind"),
        thread_id: text(item, "agent_thread_id"),
        path: text(item, "agent_path"),
        at_ms: ts,
    };
    if let Some(previous) = s.agent_events.iter_mut().find(|e| e.id == id) {
        *previous = event;
    } else {
        s.agent_events.push(event);
    }
}

/// Keep a bounded model lookup so late usage cannot inherit a newer turn's model.
fn remember_billing_model(s: &mut Session, row: &Value) {
    if row["type"] != "turn_context" {
        return;
    }
    let p = &row["payload"];
    let (Some(turn), Some(model)) = (p["turn_id"].as_str(), p["model"].as_str()) else {
        return;
    };
    if let Some(entry) = s.billing_models.iter_mut().find(|(id, _)| id == turn) {
        entry.1 = model.into();
    } else {
        s.billing_models.push((turn.into(), model.into()));
        if s.billing_models.len() > 512 {
            s.billing_models.remove(0);
        }
    }
}

fn usage_model<'a>(s: &'a Session, p: &'a Value) -> &'a str {
    if let Some(model) = p["model"].as_str() {
        return model;
    }
    let turn = p["turn_id"].as_str().unwrap_or_default();
    s.billing_models
        .iter()
        .rev()
        .find(|(id, _)| id == turn)
        .map(|(_, model)| model.as_str())
        .unwrap_or_else(|| if turn == s.turn_id { &s.model } else { "" })
}

/// Source cursors guarantee replay continuity; this recent-ID guard also rejects duplicate records.
fn account_usage(s: &mut Session, p: &Value, ts: i64) -> bool {
    if p["thread_id"]
        .as_str()
        .is_some_and(|owner| !s.id.is_empty() && owner != s.id)
    {
        return false;
    }
    let Some(id) = p["response_id"].as_str().filter(|id| !id.is_empty()) else {
        return false;
    };
    if s.billing_seen.iter().any(|seen| seen == id) {
        return false;
    }
    s.billing_seen.push(id.into());
    if s.billing_seen.len() > 2048 {
        s.billing_seen.remove(0);
    }
    let usage = &p["usage"];
    let input = usage["input_tokens"].as_u64();
    let output = usage["output_tokens"].as_u64();
    let cached = usage["cached_input_tokens"].as_u64();
    let written = usage["cache_write_input_tokens"].as_u64();
    s.cost.input_tokens = s.cost.input_tokens.saturating_add(input.unwrap_or(0));
    s.cost.cached_input_tokens = s
        .cost
        .cached_input_tokens
        .saturating_add(cached.unwrap_or(0));
    s.cost.cache_write_input_tokens = s
        .cost
        .cache_write_input_tokens
        .saturating_add(written.unwrap_or(0));
    s.cost.output_tokens = s.cost.output_tokens.saturating_add(output.unwrap_or(0));
    let (tier, tier_source) = usage_tier(s, p, ts);
    let mut quote = BillingQuote {
        response_id: id.into(),
        turn_id: text(p, "turn_id"),
        timestamp_ms: ts,
        request_started_ms: usage_start(s, p, ts),
        model: usage_model(s, p).into(),
        tier,
        tier_source,
        input,
        cached,
        written,
        output,
        price: None,
    };
    let priced = quote_price(&quote);
    quote.price = priced;
    s.billing_quotes.push(quote);
    if s.billing_quotes.len() > 2048 {
        s.billing_quotes.remove(0);
    }
    if let Some(price) = priced {
        s.cost.uncached_usd += price.uncached_usd;
        s.cost.cached_usd += price.cached_usd;
        s.cost.write_usd += price.write_usd;
        s.cost.output_usd += price.output_usd;
        s.cost.priced_requests += 1;
    } else {
        s.cost.unpriced_requests += 1;
    }
    true
}

fn quote_price(quote: &BillingQuote) -> Option<crate::pricing::CostBreakdown> {
    let (input, output) = quote.input.zip(quote.output)?;
    let (cached, written) = quote.cached.zip(quote.written)?;
    crate::pricing::request_for_tier(&quote.model, input, cached, written, output, &quote.tier)
}

/// Correct an earlier assumption once the same response acquires an observed request start.
fn reconcile_request_bill(s: &mut Session, index: usize) {
    let request = &s.requests[index];
    let Some(start) = request.started_ms else {
        return;
    };
    let Some(end) = request.ended_ms else {
        return;
    };
    let direct = s
        .billing_quotes
        .iter()
        .position(|q| q.response_id == request.id);
    let matches: Vec<_> = s
        .billing_quotes
        .iter()
        .enumerate()
        .filter(|(_, q)| q.turn_id == s.turn_id && q.timestamp_ms >= start && q.timestamp_ms <= end)
        .map(|(i, _)| i)
        .collect();
    let Some(billing) = direct.or_else(|| {
        if matches.len() == 1 {
            Some(matches[0])
        } else {
            None
        }
    }) else {
        return;
    };
    let mut quote = s.billing_quotes[billing].clone();
    quote.request_started_ms = Some(start);
    let r = &mut s.requests[index];
    r.id = quote.response_id.clone();
    r.input_tokens = quote.input.or(r.input_tokens);
    r.cached_input_tokens = quote.cached.or(r.cached_input_tokens);
    r.cache_write_input_tokens = quote.written.or(r.cache_write_input_tokens);
    r.output_tokens = quote.output.or(r.output_tokens);
    fill_request_cost(r, &Value::Null);
    if r.service_tier_source == "assumed"
        || matches!(quote.tier_source.as_str(), "request" | "response")
    {
        s.billing_quotes[billing] = quote;
        return;
    }
    let old = quote.price;
    quote.tier = r.service_tier.clone();
    quote.tier_source = r.service_tier_source.clone();
    if !r.model.is_empty() {
        quote.model = r.model.clone();
    }
    quote.price = quote_price(&quote);
    adjust_quote_total(&mut s.cost, old, quote.price);
    s.billing_quotes[billing] = quote;
}

fn adjust_quote_total(
    cost: &mut CostSummary,
    old: Option<crate::pricing::CostBreakdown>,
    new: Option<crate::pricing::CostBreakdown>,
) {
    if let Some(price) = old {
        cost.uncached_usd -= price.uncached_usd;
        cost.cached_usd -= price.cached_usd;
        cost.write_usd -= price.write_usd;
        cost.output_usd -= price.output_usd;
        cost.priced_requests = cost.priced_requests.saturating_sub(1);
    } else {
        cost.unpriced_requests = cost.unpriced_requests.saturating_sub(1);
    }
    if let Some(price) = new {
        cost.uncached_usd += price.uncached_usd;
        cost.cached_usd += price.cached_usd;
        cost.write_usd += price.write_usd;
        cost.output_usd += price.output_usd;
        cost.priced_requests += 1;
    } else {
        cost.unpriced_requests += 1;
    }
}

/// Request details may be enriched by hooks; session cost only uses canonical usage records.
fn fill_request_cost(request: &mut ModelRequest, usage: &Value) {
    request.input_tokens = usage["input_tokens"].as_u64().or(request.input_tokens);
    request.cached_input_tokens = usage["cached_input_tokens"]
        .as_u64()
        .or(request.cached_input_tokens);
    request.cache_write_input_tokens = usage["cache_write_input_tokens"]
        .as_u64()
        .or(request.cache_write_input_tokens);
    request.output_tokens = usage["output_tokens"].as_u64().or(request.output_tokens);
    request.cost_usd = request
        .input_tokens
        .zip(request.output_tokens)
        .zip(
            request
                .cached_input_tokens
                .zip(request.cache_write_input_tokens),
        )
        .and_then(|((input, output), (cached, written))| {
            crate::pricing::request_for_tier(
                &request.model,
                input,
                cached,
                written,
                output,
                if request.service_tier.is_empty() {
                    "standard"
                } else {
                    &request.service_tier
                },
            )
            .map(|cost| cost.total())
        });
}

// Cumulative per-turn usage is authoritative; response IDs deduplicate older formats.
fn usage(s: &mut Session, p: &Value, ts: i64) {
    if p["turn_id"].as_str().is_some_and(|id| id != s.turn_id) {
        return;
    }
    request_usage(s, p, ts);
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
                    details: detail(&p["tool_input"]),
                    compaction_id: compaction_at(s, ts),
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
        "PreCompact" => compaction_start(s, p, ts),
        "PostCompact" => compaction_end(s, p, ts, true),
        "Stop" | "Interrupt" => {
            s.ended_ms = Some(ts);
            s.phase = "已结束".into();
        }
        _ => {}
    }
}

// Late transcript metadata can fill identity gaps without rewinding a live hook-derived session.
pub(crate) fn supplement_metadata(s: &mut Session, row: &Value) {
    let p = &row["payload"];
    let Some(id) = p["id"].as_str().filter(|id| !id.is_empty()) else {
        return;
    };
    if !s.id.is_empty() && s.id != id {
        return;
    }
    if [row["thread_id"].as_str(), p["thread_id"].as_str()]
        .into_iter()
        .flatten()
        .any(|routed| routed != id)
    {
        return;
    }
    let mut candidate = Session::default();
    metadata(&mut candidate, p);
    if s.id.is_empty() {
        s.id = candidate.id;
    }
    if s.cwd.is_empty() {
        s.cwd = candidate.cwd;
    }
    if s.parent_id.is_empty() {
        s.parent_id = candidate.parent_id;
    }
    if s.agent_name.is_empty() {
        s.agent_name = candidate.agent_name;
    }
    if s.thread_kind.is_empty() {
        s.thread_kind = candidate.thread_kind;
        s.is_internal |= candidate.is_internal;
    }
}

// Metadata describes the workspace; it is never a conversation display title.
fn metadata(s: &mut Session, p: &Value) {
    s.id = text(p, "id");
    s.cwd = text(p, "cwd");
    let source = &p["source"];
    let origin = source.to_string().to_lowercase();
    let role = text(p, "agent_role").to_lowercase();
    let parent = p
        .get("parent_thread_id")
        .and_then(Value::as_str)
        .or_else(|| {
            source
                .pointer("/subagent/parent_thread_id")
                .and_then(Value::as_str)
        })
        .or_else(|| {
            source
                .pointer("/subagent/thread_spawn/parent_thread_id")
                .and_then(Value::as_str)
        });
    if let Some(parent) = parent {
        s.parent_id = parent.into();
    }
    // Attached chats have an explicit flag. A regular fork alone is not a sidebar chat.
    let attached = p["attached_thread"].as_bool() == Some(true);
    if attached {
        if let Some(parent) = p["forked_from_id"].as_str() {
            s.parent_id = parent.into();
        }
    }
    s.thread_kind = if origin.contains("guardian") || role.contains("review") {
        "internal"
    } else if source.pointer("/subagent/thread_spawn").is_some()
        || (origin.contains("subagent") && !s.parent_id.is_empty())
    {
        "subagent"
    } else if attached {
        "side_chat"
    } else {
        "conversation"
    }
    .into();
    s.is_internal = matches!(s.thread_kind.as_str(), "internal" | "subagent");
    let nickname = p["agent_nickname"].as_str().or_else(|| {
        source
            .pointer("/subagent/thread_spawn/agent_nickname")
            .and_then(Value::as_str)
    });
    let path = p["agent_path"].as_str().or_else(|| {
        source
            .pointer("/subagent/thread_spawn/agent_path")
            .and_then(Value::as_str)
    });
    s.agent_name = match (nickname, path.and_then(|v| v.rsplit('/').next())) {
        (Some(name), Some(task)) => format!("{name} · {task}"),
        (Some(name), None) => name.into(),
        (None, Some(task)) => task.into(),
        _ => String::new(),
    };
}

fn genuine_prompt(value: &str) -> bool {
    let t = value.trim();
    !t.is_empty()
        && ![
            "# AGENTS.md",
            "<environment_context>",
            "The following is the Codex",
            "# Selected text:",
            "<send_user_message_question",
            "<INSTRUCTIONS>",
            "<turn_aborted>",
        ]
        .iter()
        .any(|prefix| t.starts_with(prefix))
}

// Details remain bounded and preserve the actual script instead of an interpreter-only prefix.
fn detail(value: &Value) -> String {
    let text = match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(a) if a.iter().all(Value::is_string) => a
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n"),
        _ => serde_json::to_string_pretty(value).unwrap_or_default(),
    };
    let mut chars = text.chars();
    let mut result: String = chars.by_ref().take(32 * 1024).collect();
    if chars.next().is_some() {
        result.push_str("\n[详情超过 32 KiB，已截断]");
    }
    result
}

fn call_details(p: &Value) -> String {
    for key in ["input", "arguments", "tool_input", "command"] {
        let value = detail(&p[key]);
        if !value.is_empty() {
            return value;
        }
    }
    String::new()
}

// Usage-only history is visible but has no invented request start or throughput.
fn request_usage(s: &mut Session, p: &Value, ts: i64) {
    let Some(id) = p["response_id"].as_str().filter(|id| !id.is_empty()) else {
        return;
    };
    let tokens = p["usage"]["output_tokens"].as_u64();
    if let Some(index) = s.requests.iter().position(|r| r.id == id) {
        fill_request_cost(&mut s.requests[index], &p["usage"]);
        reconcile_request_bill(s, index);
        refresh_latest_tps(s, index);
        return;
    }
    let matches: Vec<_> = s
        .requests
        .iter()
        .enumerate()
        .filter(|(_, r)| {
            r.source == "hook"
                && r.started_ms.is_some_and(|start| ts >= start)
                && r.ended_ms.is_some_and(|end| ts <= end)
        })
        .map(|(index, _)| index)
        .collect();
    if matches.len() == 1 {
        let r = &mut s.requests[matches[0]];
        r.id = id.into();
        r.source = "hook+usage".into();
        fill_request_cost(r, &p["usage"]);
        reconcile_request_bill(s, matches[0]);
        refresh_latest_tps(s, matches[0]);
    } else {
        let (tier, tier_source) = usage_tier(s, p, ts);
        s.requests.push(ModelRequest {
            id: id.into(),
            model: usage_model(s, p).to_owned(),
            service_tier: tier,
            service_tier_source: tier_source,
            compaction_id: compaction_at(s, ts),
            ended_ms: Some(ts),
            output_tokens: tokens,
            status: "已完成（开始时间未知）".into(),
            source: "usage".into(),
            ..ModelRequest::default()
        });
        let last = s.requests.len() - 1;
        fill_request_cost(&mut s.requests[last], &p["usage"]);
    }
}

// Link only one usage observation strictly within this request's observed boundaries.
fn merge_request_usage(s: &mut Session, index: usize) -> usize {
    let Some(start) = s.requests[index].started_ms else {
        return index;
    };
    let Some(end) = s.requests[index].ended_ms else {
        return index;
    };
    let explicit: Vec<_> = s
        .requests
        .iter()
        .enumerate()
        .filter(|(_, r)| r.source == "usage" && r.id == s.requests[index].id)
        .map(|(i, _)| i)
        .collect();
    let candidates: Vec<_> = if explicit.is_empty() {
        s.requests
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                r.source == "usage" && r.ended_ms.is_some_and(|time| time >= start && time <= end)
            })
            .map(|(i, _)| i)
            .collect()
    } else {
        explicit
    };
    if candidates.len() == 1 {
        let source = s.requests.remove(candidates[0]);
        let target = if candidates[0] < index {
            index - 1
        } else {
            index
        };
        s.requests[target].id = source.id;
        s.requests[target].source = "hook+usage".into();
        s.requests[target].output_tokens =
            source.output_tokens.or(s.requests[target].output_tokens);
        s.requests[target].input_tokens = source.input_tokens.or(s.requests[target].input_tokens);
        s.requests[target].cached_input_tokens = source
            .cached_input_tokens
            .or(s.requests[target].cached_input_tokens);
        s.requests[target].cache_write_input_tokens = source
            .cache_write_input_tokens
            .or(s.requests[target].cache_write_input_tokens);
        fill_request_cost(&mut s.requests[target], &Value::Null);
        return target;
    }
    index
}

// Late usage can fill the last completed request, but cannot replace a newer boundary.
fn refresh_latest_tps(s: &mut Session, index: usize) {
    let request = &s.requests[index];
    if s.request_started_ms.is_some() || request.ended_ms != Some(s.model_boundary_ms) {
        return;
    }
    s.request_tps = if request.status == "已完成" {
        request
            .started_ms
            .zip(request.ended_ms)
            .filter(|(start, end)| end > start)
            .and_then(|(start, end)| {
                request
                    .output_tokens
                    .map(|tokens| tokens as f64 * 1000.0 / (end - start) as f64)
            })
    } else {
        None
    };
}

// Hook chronology is independent of rollout delivery: a late completion cannot consume a newer start.
fn model_hook(s: &mut Session, p: &Value, ts: i64, name: &str) {
    if ts <= s.model_boundary_ms {
        return;
    }
    s.model_boundary_ms = ts;
    if name == "PreModelRequest" {
        s.request_started_ms = Some(ts);
        s.requests.push(ModelRequest {
            id: format!("hook:{ts}"),
            model: s.model.clone(),
            service_tier: request_tier(s, p, ts).0,
            service_tier_source: request_tier(s, p, ts).1,
            compaction_id: compaction_at(s, ts),
            started_ms: Some(ts),
            status: "进行中".into(),
            source: "hook".into(),
            ..ModelRequest::default()
        });
        s.phase = "模型请求进行中（含网络和排队）".into();
        return;
    }
    let completed = p["status"]
        .as_str()
        .is_none_or(|status| status == "completed");
    let start = s.request_started_ms.take();
    let tokens = p["token_usage"]["output_tokens"].as_u64();
    let snapshot = s
        .requests
        .iter()
        .find(|r| r.source == "hook" && r.started_ms == start && r.ended_ms.is_none())
        .cloned();
    let tier = snapshot
        .as_ref()
        .map(|r| (r.service_tier.clone(), r.service_tier_source.clone()))
        .unwrap_or_else(|| tier_at(s, start.unwrap_or(ts)));
    let mut request = ModelRequest {
        id: format!("hook:{ts}"),
        model: snapshot
            .as_ref()
            .map(|r| r.model.clone())
            .unwrap_or_else(|| s.model.clone()),
        service_tier: tier.0,
        service_tier_source: tier.1,
        compaction_id: snapshot
            .as_ref()
            .and_then(|r| r.compaction_id.clone())
            .or_else(|| start.and_then(|time| compaction_at(s, time))),
        started_ms: start,
        ended_ms: Some(ts),
        output_tokens: tokens,
        status: if completed {
            "已完成"
        } else {
            p["status"].as_str().unwrap_or("失败")
        }
        .into(),
        source: "hook".into(),
        ..ModelRequest::default()
    };
    fill_request_cost(&mut request, &p["token_usage"]);
    if let Some(id) = p["response_id"].as_str() {
        request.id = id.into();
    }
    let index = s
        .requests
        .iter()
        .position(|r| r.source == "hook" && r.started_ms == start && r.ended_ms.is_none());
    let index = if let Some(index) = index {
        s.requests[index] = request;
        index
    } else {
        s.requests.push(request);
        s.requests.len() - 1
    };
    let index = merge_request_usage(s, index);
    reconcile_request_bill(s, index);
    refresh_latest_tps(s, index);
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

    // Lifecycle notifications must not duplicate the spawn call or invent child usage/duration.
    #[test]
    fn agent_lifecycle_is_separate_from_call_timing_and_cost() {
        let mut s = Session {
            turn_id: "turn".into(),
            ..Session::default()
        };
        apply(
            &mut s,
            &json!({"type":"response_item","timestamp_ms":100,
            "payload":{"type":"function_call","namespace":"collaboration",
                "name":"spawn_agent","call_id":"spawn"}}),
        );
        let event = json!({"type":"event_msg","timestamp_ms":200,
            "payload":{"type":"item_completed","turn_id":"turn",
                "started_at_ms":200,"completed_at_ms":200,"item":{
                    "type":"SubAgentActivity","kind":"started","id":"spawn",
                    "agent_thread_id":"child","agent_path":"/root/test"}}});
        apply(&mut s, &event);
        apply(
            &mut s,
            &json!({"type":"response_item","timestamp_ms":300,
            "payload":{"type":"function_call_output","call_id":"spawn"}}),
        );
        apply(
            &mut s,
            &json!({"type":"event_msg","timestamp_ms":4000,
            "payload":{"type":"item_completed","turn_id":"turn","item":{
                "type":"SubAgentActivity","kind":"completed","id":"complete-child",
                "agent_thread_id":"child","agent_path":"/root/test"}}}),
        );
        apply(&mut s, &event);
        assert_eq!(s.tools.len(), 1);
        assert!(is_agent_tool(&s.tools[0]));
        assert_eq!(s.tools[0].started_ms, 100);
        assert_eq!(s.tools[0].ended_ms, Some(300));
        assert_eq!(s.agent_events.len(), 2);
        assert_eq!(s.agent_events[1].at_ms, 4000);
        assert_eq!(s.output_tokens, 0);
        assert_eq!(s.cost.total(), 0.0);
        assert!(s.requests.is_empty());
    }

    // Recorded collab tool items must provide real wait timing even without response-item logs.
    #[test]
    fn collaboration_items_recover_call_and_targets() {
        let mut s = Session::default();
        apply(
            &mut s,
            &json!({"type":"response_item","timestamp_ms":100,
            "payload":{"type":"function_call","namespace":"collaboration",
                "name":"wait","call_id":"wait"}}),
        );
        let event = json!({"type":"event_msg","timestamp_ms":450,
            "payload":{"type":"item_completed","started_at_ms":100,"completed_at_ms":450,
                "item":{"type":"CollabAgentToolCall","id":"wait", "tool":"wait",
                    "status":"completed","receiver_thread_ids":["child"]}}});
        apply(&mut s, &event);
        let details = s.tools[0].details.clone();
        apply(&mut s, &event);
        assert_eq!(s.tools[0].details, details);
        apply(
            &mut s,
            &json!({"type":"response_item","timestamp_ms":471,
            "payload":{"type":"function_call_output","call_id":"wait"}}),
        );
        assert_eq!(s.tools.len(), 1);
        assert_eq!(s.tools[0].name, "collaboration.wait");
        assert_eq!(s.tools[0].started_ms, 100);
        assert_eq!(s.tools[0].ended_ms, Some(450));
        assert!(s.tools[0].details.contains("child"));
        assert!(is_agent_tool(&s.tools[0]));
        assert!(!is_agent_tool(&Tool {
            name: "wait".into(),
            ..Tool::default()
        }));
    }

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

    // Usage inside one hook interval merges into one complete request, preserving exact bounds.
    #[test]
    fn usage_and_hook_form_one_request_in_both_delivery_orders() {
        for delayed in [false, true] {
            let mut s = Session {
                turn_id: "a".into(),
                ..Session::default()
            };
            let pre = json!({"type":"monitor_hook","timestamp_ms":1000,
                "payload":{"turn_id":"a","hook_event_name":"PreModelRequest"}});
            let post = json!({"type":"monitor_hook","timestamp_ms":3000,
                "payload":{"turn_id":"a","hook_event_name":"PostModelRequest",
                    "status":"completed","token_usage":{"output_tokens":100}}});
            let usage = json!({"type":"token_usage_record","timestamp_ms":2500,
                "payload":{"turn_id":"a","response_id":"resp-a","usage":{"output_tokens":100}}});
            apply(&mut s, &pre);
            if delayed {
                apply(&mut s, &post);
                apply(&mut s, &usage);
            } else {
                apply(&mut s, &usage);
                apply(&mut s, &post);
            }
            assert_eq!(s.requests.len(), 1);
            assert_eq!(s.requests[0].id, "resp-a");
            assert_eq!(s.requests[0].started_ms, Some(1000));
            assert_eq!(s.requests[0].ended_ms, Some(3000));
            assert_eq!(s.request_tps, Some(50.0));
        }
    }

    // Two usage responses within the interval are ambiguous and must never be guessed into one.
    #[test]
    fn ambiguous_usage_keeps_separate_observations() {
        let mut s = Session {
            turn_id: "a".into(),
            ..Session::default()
        };
        apply(
            &mut s,
            &json!({"type":"monitor_hook","timestamp_ms":1000,
            "payload":{"turn_id":"a","hook_event_name":"PreModelRequest"}}),
        );
        for (id, ts) in [("one", 1500), ("two", 2000)] {
            apply(
                &mut s,
                &json!({"type":"token_usage_record","timestamp_ms":ts,
                "payload":{"turn_id":"a","response_id":id,"usage":{"output_tokens":40}}}),
            );
        }
        apply(
            &mut s,
            &json!({"type":"monitor_hook","timestamp_ms":3000,
            "payload":{"turn_id":"a","hook_event_name":"PostModelRequest",
                "status":"completed","token_usage":{"output_tokens":80}}}),
        );
        assert_eq!(s.requests.len(), 3);
        assert_eq!(s.requests.iter().filter(|r| r.source == "usage").count(), 2);
    }

    // Completed history without request boundaries has a visible row and no invented TPS.
    #[test]
    fn usage_history_has_unknown_start_and_failed_hook_has_no_rate() {
        let mut s = Session {
            turn_id: "a".into(),
            ..Session::default()
        };
        apply(
            &mut s,
            &json!({"type":"token_usage_record","timestamp_ms":1000,
            "payload":{"turn_id":"a","response_id":"historic","usage":{"output_tokens":42}}}),
        );
        assert_eq!(s.requests[0].started_ms, None);
        assert_eq!(s.requests[0].output_tokens, Some(42));
        assert_eq!(s.request_tps, None);
        for (event, ts) in [("PreModelRequest", 2000), ("PostModelRequest", 3000)] {
            apply(
                &mut s,
                &json!({"type":"monitor_hook","timestamp_ms":ts,
                "payload":{"turn_id":"a","hook_event_name":event,"status":"failed"}}),
            );
        }
        assert_eq!(s.requests.last().unwrap().status, "failed");
        assert_eq!(s.request_tps, None);
    }

    // Code-mode scripts and argv contents must remain inspectable beyond the summary's 160 chars.
    #[test]
    fn command_details_preserve_script_body() {
        let mut s = Session::default();
        let script = format!("{}\nprint('actual command')", "#".repeat(1000));
        apply(
            &mut s,
            &json!({"type":"response_item","timestamp_ms":1000,
            "payload":{"type":"custom_tool_call","name":"exec","call_id":"tool-a","input":script}}),
        );
        assert!(s.tools[0].details.ends_with("print('actual command')"));
        assert!(s.tools[0].details.len() > 1000);
        apply(
            &mut s,
            &json!({"type":"event_msg","timestamp_ms":3000,
            "payload":{"type":"item_completed","started_at_ms":1000,"completed_at_ms":2000,
                "item":{"type":"CommandExecution","id":"nested","command":["runtime.exe","exec",script]}}}),
        );
        assert!(s.tools.last().unwrap().details.contains("actual command"));
    }

    // Workspace paths and injected instructions are not user-facing conversation names.
    #[test]
    fn metadata_and_fallback_title_are_separate() {
        let mut s = Session::default();
        apply(
            &mut s,
            &json!({"type":"session_meta","timestamp_ms":1000,
            "payload":{"id":"chat","cwd":"D:/project","source":"vscode"}}),
        );
        assert!(s.title.is_empty());
        assert_eq!(s.cwd, "D:/project");
        for (text, ts) in [
            ("# AGENTS.md instructions", 2000),
            ("请修复登录问题", 3000),
            ("继续下一步", 4000),
        ] {
            apply(
                &mut s,
                &json!({"type":"event_msg","timestamp_ms":ts,
                "payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"text":text}]}}}),
            );
        }
        assert_eq!(s.title, "请修复登录问题");
        apply(
            &mut s,
            &json!({"type":"session_meta","timestamp_ms":5000,
            "payload":{"id":"child","source":{"subagent":{"thread_spawn":{"parent_thread_id":"root"}}}}}),
        );
        assert!(s.is_internal);
        assert_eq!(s.parent_id, "root");
    }

    // A nullable hook usage field is filled by its unique usage record in either delivery order.
    #[test]
    fn nullable_hook_usage_updates_summary_in_both_orders() {
        for delayed in [false, true] {
            let mut s = Session {
                turn_id: "a".into(),
                ..Session::default()
            };
            let pre = json!({"type":"monitor_hook","timestamp_ms":1000,
                "payload":{"turn_id":"a","hook_event_name":"PreModelRequest"}});
            let post = json!({"type":"monitor_hook","timestamp_ms":3000,
                "payload":{"turn_id":"a","hook_event_name":"PostModelRequest",
                    "status":"completed","token_usage":null}});
            let usage = json!({"type":"token_usage_record","timestamp_ms":2500,
                "payload":{"turn_id":"a","response_id":"resp-a","usage":{"output_tokens":100}}});
            apply(&mut s, &pre);
            if delayed {
                apply(&mut s, &post);
                apply(&mut s, &usage);
            } else {
                apply(&mut s, &usage);
                apply(&mut s, &post);
            }
            assert_eq!(s.requests.len(), 1);
            assert_eq!(s.requests[0].output_tokens, Some(100));
            assert_eq!(s.request_tps, Some(50.0));
        }
    }

    // Usage for an older request must not replace the summary after a newer request has begun.
    #[test]
    fn late_usage_cannot_replace_newer_inflight_summary() {
        let mut s = Session {
            turn_id: "a".into(),
            ..Session::default()
        };
        for (name, ts) in [
            ("PreModelRequest", 1000),
            ("PostModelRequest", 3000),
            ("PreModelRequest", 4000),
        ] {
            apply(
                &mut s,
                &json!({"type":"monitor_hook","timestamp_ms":ts,
                "payload":{"turn_id":"a","hook_event_name":name,
                    "status":"completed","token_usage":null}}),
            );
        }
        apply(
            &mut s,
            &json!({"type":"token_usage_record","timestamp_ms":2500,
            "payload":{"turn_id":"a","response_id":"old","usage":{"output_tokens":100}}}),
        );
        assert_eq!(s.request_started_ms, Some(4000));
        assert_eq!(s.request_tps, None);
        assert_eq!(s.requests[0].output_tokens, Some(100));
    }

    fn priced_record(
        id: &str,
        turn: &str,
        time: i64,
        input: u64,
        cached: u64,
        written: u64,
        output: u64,
    ) -> Value {
        json!({"type":"token_usage_record","timestamp_ms":time,"payload":{
            "response_id":id,"turn_id":turn,"thread_id":"chat","usage":{
                "input_tokens":input,"cached_input_tokens":cached,
                "cache_write_input_tokens":written,"output_tokens":output}}})
    }

    fn billing_turn(s: &mut Session, id: &str, model: &str, time: i64) {
        apply(
            s,
            &json!({"type":"event_msg","timestamp_ms":time,
            "payload":{"type":"task_started","turn_id":id}}),
        );
        apply(
            s,
            &json!({"type":"turn_context","timestamp_ms":time+1,
            "payload":{"turn_id":id,"model":model}}),
        );
    }

    // Canonical usage bills cache subsets separately and ignores the same response in hook form.
    #[test]
    fn cost_prices_cached_input_once() {
        let mut s = Session {
            id: "chat".into(),
            ..Session::default()
        };
        billing_turn(&mut s, "one", "gpt-6.1-sol", 1000);
        let usage = priced_record("r1", "one", 2000, 1000, 800, 0, 100);
        apply(&mut s, &usage);
        apply(&mut s, &usage);
        apply(
            &mut s,
            &json!({"type":"monitor_hook","timestamp_ms":2100,
            "payload":{"turn_id":"one","hook_event_name":"PostModelRequest","response_id":"r1",
                "status":"completed","token_usage":{"input_tokens":1000,"cached_input_tokens":800,"output_tokens":100}}}),
        );
        assert_eq!(s.cost.priced_requests, 1);
        assert_eq!(s.cost.unpriced_requests, 0);
        assert_eq!(s.cost.input_tokens, 1000);
        assert_eq!(s.cost.cached_input_tokens, 800);
        assert!((s.cost.total() - 0.00148).abs() < 1e-10);
        assert!((s.requests[0].cost_usd.unwrap() - 0.00148).abs() < 1e-10);
    }

    // Costs retain previous turns and mixed models after serialized restart, without replay charges.
    #[test]
    fn cost_survives_new_turn_and_restart() {
        let mut s = Session {
            id: "chat".into(),
            ..Session::default()
        };
        billing_turn(&mut s, "one", "gpt-6.1-sol", 1000);
        let first = priced_record("r1", "one", 2000, 1000, 0, 0, 100);
        apply(&mut s, &first);
        billing_turn(&mut s, "two", "gpt-6-luna", 3000);
        apply(&mut s, &priced_record("r2", "two", 4000, 1000, 0, 0, 100));
        assert!((s.cost.total() - 0.00315).abs() < 1e-10);
        assert_eq!(s.cost.priced_requests, 2);
        let json = serde_json::to_string(&s).unwrap();
        let mut restored: Session = serde_json::from_str(&json).unwrap();
        apply(&mut restored, &first);
        assert_eq!(restored.cost.priced_requests, 2);
        assert_eq!(restored.cost.total(), s.cost.total());
    }

    // Late old-turn usage uses that turn's model, not the newer visible turn's model.
    #[test]
    fn late_usage_uses_saved_billing_model() {
        let mut s = Session {
            id: "chat".into(),
            ..Session::default()
        };
        billing_turn(&mut s, "one", "gpt-6.1-sol", 1000);
        billing_turn(&mut s, "two", "gpt-6-luna", 3000);
        apply(&mut s, &priced_record("r1", "one", 2000, 1000, 0, 0, 100));
        assert_eq!(s.turn_id, "two");
        assert_eq!(s.cost.priced_requests, 1);
        assert!((s.cost.total() - 0.003).abs() < 1e-10);
    }

    // Unknown models, incomplete input counters and impossible cache subsets remain unpriced.
    #[test]
    fn invalid_and_missing_usage_is_partial_cost() {
        let mut s = Session {
            id: "chat".into(),
            ..Session::default()
        };
        billing_turn(&mut s, "one", "unknown-model", 1000);
        apply(&mut s, &priced_record("r1", "one", 2000, 1000, 0, 0, 100));
        billing_turn(&mut s, "two", "gpt-6.1-sol", 3000);
        apply(
            &mut s,
            &priced_record("r2", "two", 4000, 1000, 900, 200, 100),
        );
        apply(
            &mut s,
            &json!({"type":"token_usage_record","timestamp_ms":5000,
            "payload":{"turn_id":"two","thread_id":"chat","response_id":"r3","usage":{"output_tokens":100}}}),
        );
        assert_eq!(s.cost.priced_requests, 0);
        assert_eq!(s.cost.unpriced_requests, 3);
        assert_eq!(s.cost.total(), 0.0);
    }

    // Input cache writes are their own subset; output reasoning is already included in output tokens.
    #[test]
    fn cost_separates_write_tokens_and_response_ownership() {
        let mut s = Session {
            id: "chat".into(),
            ..Session::default()
        };
        billing_turn(&mut s, "one", "gpt-6.1-sol", 1000);
        apply(
            &mut s,
            &priced_record("r1", "one", 2000, 1000, 400, 100, 100),
        );
        let mut copied = priced_record("foreign", "one", 2500, 1000, 0, 0, 100);
        copied["payload"]["thread_id"] = json!("parent-chat");
        apply(&mut s, &copied);
        assert_eq!(s.cost.priced_requests, 1);
        assert_eq!(s.cost.cache_write_input_tokens, 100);
        assert!((s.cost.uncached_usd - 0.001).abs() < 1e-10);
        assert!((s.cost.cached_usd - 0.00004).abs() < 1e-10);
        assert!((s.cost.write_usd - 0.00025).abs() < 1e-10);
        assert!((s.cost.output_usd - 0.001).abs() < 1e-10);
        assert!((s.cost.total() - 0.00229).abs() < 1e-10);
    }

    // Missing cache telemetry means unknown pricing, not a fabricated all-cache-miss request.
    #[test]
    fn missing_cache_counters_are_not_assumed_zero() {
        let mut s = Session {
            id: "chat".into(),
            ..Session::default()
        };
        billing_turn(&mut s, "one", "gpt-6.1-sol", 1000);
        let mut missing_read = priced_record("r1", "one", 2000, 1000, 0, 0, 100);
        missing_read["payload"]["usage"]
            .as_object_mut()
            .unwrap()
            .remove("cached_input_tokens");
        apply(&mut s, &missing_read);
        assert_eq!(s.cost.priced_requests, 0);
        assert_eq!(s.cost.unpriced_requests, 1);
        assert_eq!(s.requests[0].cached_input_tokens, None);
        assert_eq!(s.requests[0].cost_usd, None);
        let mut missing_write = priced_record("r2", "one", 3000, 1000, 0, 0, 100);
        missing_write["payload"]["usage"]
            .as_object_mut()
            .unwrap()
            .remove("cache_write_input_tokens");
        apply(&mut s, &missing_write);
        assert_eq!(s.cost.unpriced_requests, 2);
        assert_eq!(s.requests[1].cache_write_input_tokens, None);
        assert_eq!(s.requests[1].cost_usd, None);
        assert_eq!(s.cost.total(), 0.0);
    }

    fn service_setting(s: &mut Session, ts: i64, tier: Value) {
        apply(
            s,
            &json!({"type":"event_msg","timestamp_ms":ts,
            "payload":{"type":"thread_settings_applied","thread_settings":{"service_tier":tier}}}),
        );
    }

    fn request_event(s: &mut Session, ts: i64, name: &str) {
        apply(
            s,
            &json!({"type":"monitor_hook","timestamp_ms":ts,
            "payload":{"turn_id":"one","hook_event_name":name,"status":"completed"}}),
        );
    }

    // Each request keeps its Standard/Fast snapshot even when the same turn changes settings again.
    #[test]
    fn same_turn_tier_toggles_snapshot_each_request() {
        let mut s = Session {
            id: "chat".into(),
            ..Session::default()
        };
        billing_turn(&mut s, "one", "gpt-6.1-sol", 1000);
        for (index, tier) in [json!("default"), json!("priority"), Value::Null]
            .into_iter()
            .enumerate()
        {
            let ts = 2000 + index as i64 * 3000;
            service_setting(&mut s, ts, tier);
            request_event(&mut s, ts + 100, "PreModelRequest");
            apply(
                &mut s,
                &priced_record(&format!("r{index}"), "one", ts + 500, 1000, 0, 0, 100),
            );
            request_event(&mut s, ts + 1000, "PostModelRequest");
        }
        assert_eq!(s.requests.len(), 3);
        assert_eq!(s.requests[0].service_tier, "default");
        assert_eq!(s.requests[1].service_tier, "priority");
        assert_eq!(s.requests[2].service_tier, "standard");
        assert!((s.requests[0].cost_usd.unwrap() - 0.003).abs() < 1e-10);
        assert!((s.requests[1].cost_usd.unwrap() - 0.006).abs() < 1e-10);
        assert!((s.cost.total() - 0.012).abs() < 1e-10);
    }

    // A user change while inference is running affects the next request, not the in-flight one.
    #[test]
    fn inflight_request_keeps_its_tier_and_price() {
        let mut s = Session {
            id: "chat".into(),
            ..Session::default()
        };
        billing_turn(&mut s, "one", "gpt-6.1-sol", 1000);
        request_event(&mut s, 1100, "PreModelRequest");
        service_setting(&mut s, 1200, json!("priority"));
        apply(&mut s, &priced_record("r1", "one", 1500, 1000, 0, 0, 100));
        request_event(&mut s, 2000, "PostModelRequest");
        request_event(&mut s, 2100, "PreModelRequest");
        service_setting(&mut s, 2200, Value::Null);
        apply(&mut s, &priced_record("r2", "one", 2500, 1000, 0, 0, 100));
        request_event(&mut s, 3000, "PostModelRequest");
        assert_eq!(s.requests[0].service_tier, "standard");
        assert_eq!(s.requests[1].service_tier, "priority");
        assert!((s.cost.total() - 0.009).abs() < 1e-10);
    }

    // Replaying late hook observations corrects an assumed quote once, without duplicating usage.
    #[test]
    fn late_priority_snapshot_corrects_assumed_cost() {
        let mut s = Session {
            id: "chat".into(),
            ..Session::default()
        };
        billing_turn(&mut s, "one", "gpt-6.1-sol", 1000);
        apply(&mut s, &priced_record("r1", "one", 1500, 1000, 0, 0, 100));
        assert!((s.cost.total() - 0.003).abs() < 1e-10);
        apply(
            &mut s,
            &json!({"type":"monitor_hook","timestamp_ms":1100,
            "payload":{"turn_id":"one","hook_event_name":"PreModelRequest","service_tier":"priority"}}),
        );
        request_event(&mut s, 2000, "PostModelRequest");
        assert_eq!(s.cost.priced_requests, 1);
        assert_eq!(s.cost.input_tokens, 1000);
        assert!((s.cost.total() - 0.006).abs() < 1e-10);
        assert!((s.requests[0].cost_usd.unwrap() - 0.006).abs() < 1e-10);
    }

    // Missing turn metadata keeps Fast enabled; an explicitly reported null turns it off.
    #[test]
    fn absent_service_tier_does_not_reset_reported_priority() {
        let mut s = Session::default();
        service_setting(&mut s, 1000, json!("priority"));
        apply(
            &mut s,
            &json!({"type":"turn_context","timestamp_ms":2000,
            "payload":{"model":"gpt-6.1-sol"}}),
        );
        assert_eq!(s.service_tier, "priority");
        apply(
            &mut s,
            &json!({"type":"turn_context","timestamp_ms":3000,
            "payload":{"model":"gpt-6.1-sol","service_tier":null}}),
        );
        assert_eq!(s.service_tier, "standard");
    }

    // Child membership is fixed when a call starts and survives a later compaction-end observation.
    #[test]
    fn compaction_groups_children_and_never_bills_parent_twice() {
        let mut s = Session {
            id: "chat".into(),
            ..Session::default()
        };
        billing_turn(&mut s, "one", "gpt-6.1-sol", 1000);
        request_event(&mut s, 1100, "PreCompact");
        request_event(&mut s, 1200, "PreModelRequest");
        apply(
            &mut s,
            &json!({"type":"monitor_hook","timestamp_ms":1300,
            "payload":{"turn_id":"one","hook_event_name":"PreToolUse","tool_use_id":"t1","tool_name":"Bash"}}),
        );
        let parent = s.active_compaction.clone();
        request_event(&mut s, 2000, "PostCompact");
        apply(&mut s, &priced_record("r1", "one", 2500, 1000, 0, 0, 100));
        request_event(&mut s, 2600, "PostModelRequest");
        assert_eq!(s.tools[0].compaction_id, parent);
        assert_eq!(s.requests[0].compaction_id, parent);
        assert!(s.active_compaction.is_none());
        assert_eq!(s.compactions[0].ended_ms, Some(2000));
        assert_eq!(s.cost.priced_requests, 1);
        assert!((s.cost.total() - 0.003).abs() < 1e-10);
        request_event(&mut s, 3000, "PreModelRequest");
        assert_eq!(s.requests.last().unwrap().compaction_id, None);
    }

    // End-only logs have no duration; a later complete hook interval reconciles the same completion.
    #[test]
    fn compaction_completion_without_start_remains_unknown() {
        let mut s = Session {
            turn_id: "one".into(),
            ..Session::default()
        };
        apply(
            &mut s,
            &json!({"type":"compacted","timestamp_ms":1800,"payload":{}}),
        );
        assert_eq!(s.compactions.len(), 1);
        assert_eq!(s.compactions[0].started_ms, None);
        assert!(s.active_compaction.is_none());
        request_event(&mut s, 1000, "PreCompact");
        request_event(&mut s, 2000, "PostCompact");
        assert_eq!(s.compactions.len(), 1);
        assert_eq!(s.compactions[0].started_ms, Some(1000));
        assert_eq!(s.compactions[0].ended_ms, Some(2000));
    }

    // Late earlier settings correct a stale inferred snapshot and its canonical bill exactly once.
    #[test]
    fn late_settings_before_request_reprice_snapshot_and_ledger() {
        let mut s = Session {
            id: "chat".into(),
            ..Session::default()
        };
        service_setting(&mut s, 100, json!("default"));
        billing_turn(&mut s, "one", "gpt-6.1-sol", 800);
        request_event(&mut s, 1000, "PreModelRequest");
        apply(&mut s, &priced_record("r1", "one", 1100, 1000, 0, 0, 100));
        request_event(&mut s, 1200, "PostModelRequest");
        assert_eq!(s.requests[0].service_tier_source, "settings");
        assert!((s.cost.total() - 0.003).abs() < 1e-10);
        service_setting(&mut s, 500, json!("priority"));
        assert_eq!(s.requests[0].service_tier, "priority");
        assert!((s.requests[0].cost_usd.unwrap() - 0.006).abs() < 1e-10);
        assert!((s.cost.total() - 0.006).abs() < 1e-10);
        assert_eq!(s.cost.priced_requests, 1);
        service_setting(&mut s, 500, json!("priority"));
        assert!((s.cost.total() - 0.006).abs() < 1e-10);
    }

    // A later toggle cannot rewrite a request that had already begun, regardless of delivery order.
    #[test]
    fn late_settings_after_request_start_do_not_reprice_it() {
        let mut s = Session {
            id: "chat".into(),
            ..Session::default()
        };
        service_setting(&mut s, 100, json!("default"));
        billing_turn(&mut s, "one", "gpt-6.1-sol", 800);
        request_event(&mut s, 1000, "PreModelRequest");
        apply(&mut s, &priced_record("r1", "one", 1150, 1000, 0, 0, 100));
        request_event(&mut s, 1200, "PostModelRequest");
        service_setting(&mut s, 1100, json!("priority"));
        assert_eq!(s.requests[0].service_tier, "default");
        assert!((s.cost.total() - 0.003).abs() < 1e-10);
    }

    // Explicit request telemetry outranks inferred settings, including settings delivered late.
    #[test]
    fn explicit_request_tier_is_not_replaced_by_late_settings() {
        let mut s = Session {
            id: "chat".into(),
            ..Session::default()
        };
        billing_turn(&mut s, "one", "gpt-6.1-sol", 800);
        apply(
            &mut s,
            &json!({"type":"monitor_hook","timestamp_ms":1000,
            "payload":{"turn_id":"one","hook_event_name":"PreModelRequest","service_tier":"standard"}}),
        );
        apply(&mut s, &priced_record("r1", "one", 1100, 1000, 0, 0, 100));
        request_event(&mut s, 1200, "PostModelRequest");
        service_setting(&mut s, 900, json!("priority"));
        assert_eq!(s.requests[0].service_tier, "standard");
        assert_eq!(s.requests[0].service_tier_source, "request");
        assert!((s.cost.total() - 0.003).abs() < 1e-10);
    }

    // A bounded timeline eviction does not discard the start needed to correct its persisted bill.
    #[test]
    fn ledger_retains_request_start_after_timeline_eviction() {
        let mut s = Session {
            id: "chat".into(),
            ..Session::default()
        };
        service_setting(&mut s, 100, json!("default"));
        billing_turn(&mut s, "one", "gpt-6.1-sol", 800);
        request_event(&mut s, 1000, "PreModelRequest");
        apply(&mut s, &priced_record("r1", "one", 1100, 1000, 0, 0, 100));
        request_event(&mut s, 1200, "PostModelRequest");
        s.requests.clear();
        service_setting(&mut s, 500, json!("priority"));
        assert!((s.cost.total() - 0.006).abs() < 1e-10);
        assert_eq!(s.cost.priced_requests, 1);
    }

    // A delayed future toggle cannot temporarily reprice a billed request awaiting its Post hook.
    #[test]
    fn billed_inflight_request_retains_its_start_before_post_hook() {
        let mut s = Session {
            id: "chat".into(),
            ..Session::default()
        };
        service_setting(&mut s, 100, json!("default"));
        billing_turn(&mut s, "one", "gpt-6.1-sol", 800);
        request_event(&mut s, 1000, "PreModelRequest");
        apply(&mut s, &priced_record("r1", "one", 1500, 1000, 0, 0, 100));
        service_setting(&mut s, 1200, json!("priority"));
        assert!((s.cost.total() - 0.003).abs() < 1e-10);
        request_event(&mut s, 2000, "PostModelRequest");
        assert!((s.cost.total() - 0.003).abs() < 1e-10);
        assert_eq!(s.requests[0].service_tier, "default");
    }
    // Only explicit attachment creates a sidebar relation; a normal fork stays independent.
    #[test]
    fn attached_chat_and_agent_have_distinct_relationships() {
        let mut side = Session::default();
        apply(
            &mut side,
            &json!({"type":"session_meta","payload":{
            "id":"side","source":"vscode","attached_thread":true,"forked_from_id":"parent"}}),
        );
        assert_eq!(side.thread_kind, "side_chat");
        assert_eq!(side.parent_id, "parent");
        let mut fork = Session::default();
        apply(
            &mut fork,
            &json!({"type":"session_meta","payload":{
            "id":"fork","source":"vscode","attached_thread":false,"forked_from_id":"parent"}}),
        );
        assert_eq!(fork.thread_kind, "conversation");
        assert!(fork.parent_id.is_empty());
        let mut agent = Session::default();
        apply(
            &mut agent,
            &json!({"type":"session_meta","payload":{
            "id":"agent","parent_thread_id":"parent","agent_nickname":"Helper",
            "source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent"}}}}}),
        );
        assert_eq!(agent.thread_kind, "subagent");
        assert_eq!(agent.parent_id, "parent");
        assert_eq!(agent.agent_name, "Helper");
    }

    // A newer hook binds the chat while late transcript metadata supplies its missing relationship.
    #[test]
    fn late_metadata_fills_child_identity_without_rewinding_live_state() {
        let mut s = Session::default();
        apply(
            &mut s,
            &json!({"type":"monitor_hook","timestamp_ms":3000,
            "payload":{"thread_id":"child","turn_id":"live-turn",
                "hook_event_name":"UserPromptSubmit","model":"live-model"}}),
        );
        let phase = s.phase.clone();
        apply(
            &mut s,
            &json!({"type":"session_meta","timestamp_ms":1000,
            "thread_id":"child","payload":{"id":"child","cwd":"D:/project",
                "source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent",
                    "agent_nickname":"Alice","agent_path":"/root/review"}}}}}),
        );
        assert_eq!(s.id, "child");
        assert_eq!(s.cwd, "D:/project");
        assert_eq!(s.parent_id, "parent");
        assert_eq!(s.thread_kind, "subagent");
        assert_eq!(s.agent_name, "Alice · review");
        assert!(s.is_internal);
        assert_eq!(s.phase, phase);
        assert_eq!(s.turn_id, "live-turn");
        assert_eq!(s.model, "live-model");
        assert_eq!(s.last_ms, 3000);
        assert_eq!(s.started_ms, Some(3000));
    }

    // Late metadata is supplementary: known relationships and workspace/name choices remain authoritative.
    #[test]
    fn late_metadata_preserves_known_identity_fields() {
        let mut s = Session {
            id: "child".into(),
            cwd: "D:/current".into(),
            parent_id: "known-parent".into(),
            thread_kind: "side_chat".into(),
            agent_name: "Known name".into(),
            model: "current-model".into(),
            phase: "current phase".into(),
            turn_id: "current-turn".into(),
            last_ms: 3000,
            ..Session::default()
        };
        apply(
            &mut s,
            &json!({"type":"session_meta","timestamp_ms":1000,
            "payload":{"id":"child","cwd":"D:/old","agent_nickname":"Old name",
                "source":{"subagent":{"parent_thread_id":"old-parent"}}}}),
        );
        assert_eq!(s.parent_id, "known-parent");
        assert_eq!(s.thread_kind, "side_chat");
        assert_eq!(s.agent_name, "Known name");
        assert_eq!(s.cwd, "D:/current");
        assert!(!s.is_internal);
        assert_eq!(s.model, "current-model");
        assert_eq!(s.phase, "current phase");
        assert_eq!(s.turn_id, "current-turn");
        assert_eq!(s.last_ms, 3000);
    }

    // A copied parent record or mismatched source route must not supply a child's missing fields.
    #[test]
    fn late_metadata_rejects_other_chat_and_route() {
        for (id, route, payload_route) in [
            ("parent", "parent", "parent"),
            ("child", "parent", "child"),
            ("child", "child", "parent"),
        ] {
            let mut s = Session {
                id: "child".into(),
                last_ms: 3000,
                ..Session::default()
            };
            apply(
                &mut s,
                &json!({"type":"session_meta","timestamp_ms":1000,
                "thread_id":route,"payload":{"id":id,"thread_id":payload_route,
                    "cwd":"D:/wrong","source":{"subagent":{"parent_thread_id":"wrong-parent"}}}}),
            );
            assert_eq!(s.id, "child");
            assert!(s.cwd.is_empty());
            assert!(s.parent_id.is_empty());
            assert!(s.thread_kind.is_empty());
            assert_eq!(s.last_ms, 3000);
        }
    }

    // Metadata delivered normally continues through the existing classification path.
    #[test]
    fn normal_metadata_still_sets_side_chat_relationship() {
        let mut s = Session::default();
        apply(
            &mut s,
            &json!({"type":"session_meta","timestamp_ms":1000,
            "payload":{"id":"side","cwd":"D:/project","source":"vscode",
                "attached_thread":true,"forked_from_id":"parent","agent_nickname":"Side"}}),
        );
        assert_eq!(s.id, "side");
        assert_eq!(s.parent_id, "parent");
        assert_eq!(s.thread_kind, "side_chat");
        assert_eq!(s.agent_name, "Side");
        assert!(!s.is_internal);
        assert_eq!(s.last_ms, 1000);
    }

    fn coverage_session() -> Session {
        let mut s = Session {
            id: "chat".into(),
            ..Session::default()
        };
        reset_cost_coverage(&mut s);
        billing_turn(&mut s, "one", "gpt-6.1-sol", 800);
        s
    }

    // A loaded hook-only child cannot contribute a seemingly complete zero-dollar cost.
    #[test]
    fn hook_only_child_has_unknown_cost_coverage() {
        let mut s = coverage_session();
        s.parent_id = "parent".into();
        s.thread_kind = "subagent".into();
        request_event(&mut s, 1000, "PreModelRequest");
        assert!(!CostMember::from_session(&s).partial);
        request_event(&mut s, 1200, "PostModelRequest");
        assert_eq!(s.cost_coverage_incomplete, Some(true));
        assert!(CostMember::from_session(&s).partial);
        assert_eq!(s.cost.priced_requests, 0);
        assert_eq!(s.cost.unpriced_requests, 0);
    }

    // Canonical usage and terminal hooks settle identically in either source-delivery order.
    #[test]
    fn coverage_settles_usage_before_or_after_post_hook() {
        for usage_first in [false, true] {
            let mut s = coverage_session();
            request_event(&mut s, 1000, "PreModelRequest");
            let canonical = priced_record("r1", "one", 1100, 1000, 0, 0, 100);
            if usage_first {
                apply(&mut s, &canonical);
            }
            request_event(&mut s, 1200, "PostModelRequest");
            if !usage_first {
                assert!(CostMember::from_session(&s).partial);
                apply(&mut s, &canonical);
            }
            assert_eq!(s.cost_coverage_incomplete, Some(false));
            assert!(!CostMember::from_session(&s).partial);
            assert_eq!(s.cost.priced_requests, 1);
        }
    }

    // New turns and serialized restart retain missing coverage until late old-turn canonical usage arrives.
    #[test]
    fn coverage_survives_turn_reset_and_restart() {
        let mut s = coverage_session();
        request_event(&mut s, 1000, "PreModelRequest");
        request_event(&mut s, 1200, "PostModelRequest");
        billing_turn(&mut s, "two", "gpt-6.1-sol", 2000);
        let mut restored: Session =
            serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert!(CostMember::from_session(&restored).partial);
        apply(
            &mut restored,
            &priced_record("r1", "one", 1100, 1000, 0, 0, 100),
        );
        assert_eq!(restored.turn_id, "two");
        assert_eq!(restored.cost_coverage_incomplete, Some(false));
        assert_eq!(restored.cost.priced_requests, 1);
    }

    // Failed/cancelled requests without any reported usage do not invent missing billable work.
    #[test]
    fn failed_without_usage_has_no_coverage_gap() {
        for status in ["failed", "cancelled"] {
            let mut s = coverage_session();
            request_event(&mut s, 1000, "PreModelRequest");
            apply(
                &mut s,
                &json!({"type":"monitor_hook","timestamp_ms":1200,
                "payload":{"turn_id":"one","hook_event_name":"PostModelRequest","status":status,"token_usage":null}}),
            );
            assert_eq!(s.cost_coverage_incomplete, Some(false));
            assert!(!CostMember::from_session(&s).partial);
        }
    }

    // Repeated hooks and canonical rows remain one evidence record and one financial charge.
    #[test]
    fn coverage_duplicate_hooks_do_not_double_count() {
        let mut s = coverage_session();
        for _ in 0..2 {
            request_event(&mut s, 1000, "PreModelRequest");
        }
        for _ in 0..2 {
            request_event(&mut s, 1200, "PostModelRequest");
        }
        let usage = priced_record("r1", "one", 1100, 1000, 0, 0, 100);
        apply(&mut s, &usage);
        apply(&mut s, &usage);
        assert_eq!(s.cost_coverage.len(), 1);
        assert_eq!(s.cost_coverage_known.len(), 1);
        assert_eq!(s.cost_coverage_incomplete, Some(false));
        assert_eq!(s.cost.priced_requests, 1);
    }

    // Equal concurrent counts cannot establish response ownership when anonymous intervals overlap.
    #[test]
    fn concurrent_anonymous_coverage_stays_partial_when_ambiguous() {
        let mut s = coverage_session();
        for (agent, name, ts) in [
            ("a", "PreModelRequest", 1000),
            ("b", "PreModelRequest", 1100),
            ("a", "PostModelRequest", 1300),
            ("b", "PostModelRequest", 1400),
        ] {
            apply(
                &mut s,
                &json!({"type":"monitor_hook","timestamp_ms":ts,
                "payload":{"turn_id":"one","agent_id":agent,"hook_event_name":name,"status":"completed"}}),
            );
        }
        for (id, ts) in [("r1", 1200), ("r2", 1250)] {
            apply(&mut s, &priced_record(id, "one", ts, 1000, 0, 0, 100));
        }
        assert_eq!(s.cost.priced_requests, 2);
        assert_eq!(s.cost_coverage_incomplete, Some(true));
        assert!(s
            .cost_coverage
            .iter()
            .all(|request| request.matched_response_id.is_none()));
    }

    // A terminal hook without Pre needs an explicit response ID; matching only turn counts is unsafe.
    #[test]
    fn partial_hook_stream_requires_explicit_identity() {
        for response in [Some("r1"), None] {
            let mut s = coverage_session();
            apply(&mut s, &priced_record("r1", "one", 1100, 1000, 0, 0, 100));
            let mut hook = json!({"type":"monitor_hook","timestamp_ms":1200,
                "payload":{"turn_id":"one","hook_event_name":"PostModelRequest","status":"completed"}});
            if let Some(id) = response {
                hook["payload"]["response_id"] = json!(id);
            }
            apply(&mut s, &hook);
            assert_eq!(s.cost_coverage_incomplete, Some(response.is_none()));
        }
    }

    // Previously verified canonical evidence remains valid after the bounded pricing ledger evicts it.
    #[test]
    fn verified_coverage_survives_price_ledger_eviction() {
        let mut s = coverage_session();
        request_event(&mut s, 1000, "PreModelRequest");
        apply(&mut s, &priced_record("r1", "one", 1100, 1000, 0, 0, 100));
        request_event(&mut s, 1200, "PostModelRequest");
        s.billing_quotes.clear();
        refresh_cost_coverage(&mut s);
        assert_eq!(s.cost_coverage_incomplete, Some(false));
        assert_eq!(
            s.cost_coverage[0].matched_response_id.as_deref(),
            Some("r1")
        );
    }

    // Legacy snapshots remain explicitly uncertain until a verified historical scan establishes coverage.
    #[test]
    fn legacy_coverage_is_not_silently_complete() {
        let mut s = Session::default();
        assert_eq!(s.cost_coverage_incomplete, None);
        assert!(CostMember::from_session(&s).partial);
        refresh_cost_coverage(&mut s);
        assert!(CostMember::from_session(&s).partial);
        reset_cost_coverage(&mut s);
        refresh_cost_coverage(&mut s);
        assert!(!CostMember::from_session(&s).partial);
    }

    // Historical hook evidence can be transferred independently of current turn state and prices.
    #[test]
    fn copying_coverage_does_not_touch_session_cost_or_identity() {
        let mut observed = coverage_session();
        request_event(&mut observed, 1000, "PreModelRequest");
        request_event(&mut observed, 1200, "PostModelRequest");
        let mut target = coverage_session();
        apply(
            &mut target,
            &priced_record("r1", "one", 1100, 1000, 0, 0, 100),
        );
        let before = serde_json::to_value(&target.cost).unwrap();
        target.parent_id = "owned-parent".into();
        copy_cost_coverage(&mut target, &observed);
        refresh_cost_coverage(&mut target);
        assert_eq!(serde_json::to_value(&target.cost).unwrap(), before);
        assert_eq!(target.parent_id, "owned-parent");
        assert_eq!(target.cost_coverage_incomplete, Some(false));
    }

    // Updating a new turn leaves earlier verified proof intact even after its price quote was evicted.
    #[test]
    fn new_turn_refresh_preserves_old_turn_coverage_proof() {
        let mut s = coverage_session();
        request_event(&mut s, 1000, "PreModelRequest");
        apply(&mut s, &priced_record("r1", "one", 1100, 1000, 0, 0, 100));
        request_event(&mut s, 1200, "PostModelRequest");
        let old = serde_json::to_value(&s.cost_coverage[0]).unwrap();
        s.billing_quotes.clear();
        billing_turn(&mut s, "two", "gpt-6.1-sol", 2000);
        for (name, ts) in [("PreModelRequest", 2100), ("PostModelRequest", 2400)] {
            apply(
                &mut s,
                &json!({"type":"monitor_hook","timestamp_ms":ts,
                "payload":{"turn_id":"two","hook_event_name":name,"status":"completed"}}),
            );
        }
        assert_eq!(serde_json::to_value(&s.cost_coverage[0]).unwrap(), old);
        assert_eq!(s.cost_coverage_incomplete, Some(true));
        apply(&mut s, &priced_record("r2", "two", 2300, 1000, 0, 0, 100));
        assert_eq!(s.cost_coverage_incomplete, Some(false));
        assert_eq!(serde_json::to_value(&s.cost_coverage[0]).unwrap(), old);
    }

    // Tool hooks and duplicate canonical rows do not rebuild the runtime coverage index or proofs.
    #[test]
    fn unrelated_hooks_and_duplicate_usage_skip_coverage_work() {
        let mut s = coverage_session();
        request_event(&mut s, 1000, "PreModelRequest");
        let usage = priced_record("r1", "one", 1100, 1000, 0, 0, 100);
        apply(&mut s, &usage);
        request_event(&mut s, 1200, "PostModelRequest");
        let proof = serde_json::to_value(&s.cost_coverage).unwrap();
        let indexed = s.cost_coverage_runtime.indexed_known;
        apply(
            &mut s,
            &json!({"type":"monitor_hook","timestamp_ms":1300,
            "payload":{"turn_id":"one","hook_event_name":"PreToolUse","tool_use_id":"tool"}}),
        );
        apply(&mut s, &usage);
        assert_eq!(s.cost_coverage_runtime.indexed_known, indexed);
        assert_eq!(serde_json::to_value(&s.cost_coverage).unwrap(), proof);
        assert_eq!(s.cost_coverage_incomplete, Some(false));
    }

    // Foreign usage must not settle a local pending request during the scoped refresh path.
    #[test]
    fn foreign_thread_usage_does_not_clear_pending_coverage() {
        let mut s = coverage_session();
        request_event(&mut s, 1000, "PreModelRequest");
        request_event(&mut s, 1200, "PostModelRequest");
        let mut usage = priced_record("r1", "one", 1100, 1000, 0, 0, 100);
        usage["payload"]["thread_id"] = json!("foreign");
        apply(&mut s, &usage);
        assert_eq!(s.cost_coverage_incomplete, Some(true));
        assert_eq!(s.cost.priced_requests, 0);
    }
}
