use crate::model::{CostSummary, Session};
use std::collections::{HashMap, HashSet};

/// A read-only view of a chat and its owned agent/side-chat descendants.
pub struct SessionStats<'a> {
    pub members: Vec<&'a Session>,
    pub cost: CostSummary,
    pub missing_children: usize,
}

/// Aggregate raw chat costs once per identity; ordinary conversation forks stay independent.
pub fn rollup<'a>(root: &'a Session, sessions: &'a [Session]) -> SessionStats<'a> {
    let mut children: HashMap<&str, Vec<&Session>> = HashMap::new();
    let mut known: HashSet<&str> = sessions.iter().map(|session| session.id.as_str()).collect();
    known.insert(&root.id);
    for session in sessions {
        if matches!(session.thread_kind.as_str(), "subagent" | "side_chat") {
            children
                .entry(&session.parent_id)
                .or_default()
                .push(session);
        }
    }
    let mut members = vec![root];
    let mut seen: HashSet<&str> = HashSet::from([root.id.as_str()]);
    let mut cursor = 0;
    while cursor < members.len() {
        if let Some(descendants) = children.get(members[cursor].id.as_str()) {
            for child in descendants {
                if seen.insert(&child.id) {
                    members.push(child);
                }
            }
        }
        cursor += 1;
    }
    members[1..].sort_by(|left, right| left.id.cmp(&right.id));
    let mut cost = CostSummary::default();
    let mut missing = HashSet::new();
    for session in &members {
        add_cost(&mut cost, &session.cost);
        for event in &session.agent_events {
            if !event.thread_id.is_empty() && !known.contains(event.thread_id.as_str()) {
                missing.insert(event.thread_id.as_str());
            }
        }
    }
    SessionStats {
        members,
        cost,
        missing_children: missing.len(),
    }
}

/// Sum only cost-related cumulative counters; live timing and TPS have different scopes.
fn add_cost(total: &mut CostSummary, own: &CostSummary) {
    total.uncached_usd += own.uncached_usd;
    total.cached_usd += own.cached_usd;
    total.write_usd += own.write_usd;
    total.output_usd += own.output_usd;
    total.input_tokens = total.input_tokens.saturating_add(own.input_tokens);
    total.cached_input_tokens = total
        .cached_input_tokens
        .saturating_add(own.cached_input_tokens);
    total.cache_write_input_tokens = total
        .cache_write_input_tokens
        .saturating_add(own.cache_write_input_tokens);
    total.output_tokens = total.output_tokens.saturating_add(own.output_tokens);
    total.priced_requests = total.priced_requests.saturating_add(own.priced_requests);
    total.unpriced_requests = total
        .unpriced_requests
        .saturating_add(own.unpriced_requests);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::AgentEvent;

    fn session(id: &str, parent: &str, kind: &str, value: u64) -> Session {
        Session {
            id: id.into(),
            parent_id: parent.into(),
            thread_kind: kind.into(),
            cost: CostSummary {
                uncached_usd: value as f64,
                cached_usd: value as f64 / 2.0,
                write_usd: value as f64 / 4.0,
                output_usd: value as f64 / 8.0,
                input_tokens: value * 100,
                cached_input_tokens: value * 10,
                cache_write_input_tokens: value * 5,
                output_tokens: value * 2,
                priced_requests: value,
                unpriced_requests: value - 1,
            },
            output_tokens: 999,
            request_tps: Some(77.0),
            ..Session::default()
        }
    }

    fn event(thread_id: &str) -> AgentEvent {
        AgentEvent {
            thread_id: thread_id.into(),
            ..AgentEvent::default()
        }
    }

    // Child and grandchild raw costs contribute exactly once, with root first and stable IDs after.
    #[test]
    fn owned_descendants_roll_up_all_cost_components() {
        let root = session("root", "", "conversation", 1);
        let sessions = vec![
            session("z-child", "root", "subagent", 2),
            session("a-grandchild", "z-child", "side_chat", 3),
            session("root", "", "conversation", 1),
        ];
        let stats = rollup(&root, &sessions);
        assert_eq!(
            stats
                .members
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>(),
            vec!["root", "a-grandchild", "z-child"]
        );
        assert_eq!(stats.cost.uncached_usd, 6.0);
        assert_eq!(stats.cost.cached_usd, 3.0);
        assert_eq!(stats.cost.write_usd, 1.5);
        assert_eq!(stats.cost.output_usd, 0.75);
        assert_eq!(stats.cost.input_tokens, 600);
        assert_eq!(stats.cost.cached_input_tokens, 60);
        assert_eq!(stats.cost.cache_write_input_tokens, 30);
        assert_eq!(stats.cost.output_tokens, 12);
        assert_eq!(stats.cost.priced_requests, 6);
        assert_eq!(stats.cost.unpriced_requests, 3);
        assert_eq!(stats.missing_children, 0);
    }

    // Ordinary forks and their descendants are separate chats even when they name this parent.
    #[test]
    fn ordinary_forks_are_not_agent_descendants() {
        let root = session("root", "", "conversation", 1);
        let sessions = vec![
            session("fork", "root", "conversation", 2),
            session("fork-agent", "fork", "subagent", 3),
            session("plain-fork", "root", "fork", 4),
        ];
        let stats = rollup(&root, &sessions);
        assert_eq!(stats.members.len(), 1);
        assert_eq!(stats.cost.total(), root.cost.total());
    }

    // Repeated lifecycle notices and duplicated snapshot rows cannot duplicate a child's costs.
    #[test]
    fn repeated_child_notifications_count_once() {
        let mut root = session("root", "", "conversation", 1);
        root.agent_events = vec![event("child"), event("child"), event("child")];
        let child = session("child", "root", "subagent", 2);
        let sessions = vec![child.clone(), child];
        let stats = rollup(&root, &sessions);
        assert_eq!(stats.members.len(), 2);
        assert_eq!(stats.cost.uncached_usd, 3.0);
        assert_eq!(stats.missing_children, 0);
    }

    // References do not override ownership: another parent's existing child is neither owned nor missing.
    #[test]
    fn foreign_parent_references_are_not_included() {
        let mut root = session("root", "", "conversation", 1);
        root.agent_events = vec![event("foreign"), event("existing-conversation")];
        let sessions = vec![
            session("foreign", "elsewhere", "subagent", 9),
            session("existing-conversation", "root", "conversation", 8),
        ];
        let stats = rollup(&root, &sessions);
        assert_eq!(stats.members.len(), 1);
        assert_eq!(stats.missing_children, 0);
        assert_eq!(stats.cost.input_tokens, 100);
    }

    // Missing descendants are marked partial once per nonempty referenced identity across the tree.
    #[test]
    fn unavailable_referenced_children_mark_partial() {
        let mut root = session("root", "", "conversation", 1);
        root.agent_events = vec![event("missing"), event(""), event("missing")];
        let mut child = session("child", "root", "side_chat", 2);
        child.agent_events = vec![event("missing"), event("missing-other"), event("root")];
        let sessions = vec![child];
        let stats = rollup(&root, &sessions);
        assert_eq!(stats.missing_children, 2);
        assert_eq!(stats.members.len(), 2);
    }

    // Rollup returns derived totals and leaves the original cost, current-turn tokens, and TPS untouched.
    #[test]
    fn aggregation_never_mutates_source_sessions() {
        let root = session("root", "", "conversation", 1);
        let sessions = vec![session("child", "root", "subagent", 2)];
        let before = serde_json::to_value((&root, &sessions)).unwrap();
        let stats = rollup(&root, &sessions);
        assert_eq!(stats.cost.output_tokens, 6);
        assert_eq!(root.output_tokens, 999);
        assert_eq!(root.request_tps, Some(77.0));
        assert_eq!(serde_json::to_value((&root, &sessions)).unwrap(), before);
    }
}
