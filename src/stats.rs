use crate::model::{CostMember, CostSummary, Session};
use std::collections::{HashMap, HashSet};

/// A read-only view of a chat and its owned agent/side-chat descendants.
pub struct SessionStats {
    pub members: Vec<CostMember>,
    pub cost: CostSummary,
    pub missing_children: usize,
    pub incomplete_members: usize,
}

/// Aggregate the persisted ownership index, including sessions outside the visible timeline cache.
pub fn rollup(root: &Session, sessions: &[Session], costs: &[CostMember]) -> SessionStats {
    // Tests and early snapshots may not have the index yet; keep known sessions available.
    let fallback: Vec<_> = if costs.is_empty() {
        sessions.iter().map(CostMember::from_session).collect()
    } else {
        Vec::new()
    };
    let source = if costs.is_empty() { &fallback } else { costs };
    let mut children: HashMap<&str, Vec<&CostMember>> = HashMap::new();
    let mut known: HashSet<&str> = source.iter().map(|member| member.id.as_str()).collect();
    known.insert(&root.id);
    for member in source {
        if matches!(member.thread_kind.as_str(), "subagent" | "side_chat") {
            children.entry(&member.parent_id).or_default().push(member);
        }
    }
    let mut root_member = CostMember::from_session(root);
    if let Some(indexed) = source.iter().find(|member| member.id == root.id) {
        root_member.partial |= indexed.partial || indexed.cost.is_none();
    }
    let mut members = vec![root_member];
    let mut seen = HashSet::from([root.id.clone()]);
    let mut cursor = 0;
    while cursor < members.len() {
        if let Some(descendants) = children.get(members[cursor].id.as_str()) {
            for child in descendants {
                if seen.insert(child.id.clone()) {
                    members.push((*child).clone());
                }
            }
        }
        cursor += 1;
    }
    members[1..].sort_by(|left, right| left.id.cmp(&right.id));
    let mut cost = CostSummary::default();
    let mut missing = HashSet::new();
    let mut incomplete_members = 0;
    for member in &members {
        if let Some(own) = &member.cost {
            add_cost(&mut cost, own);
            incomplete_members += usize::from(member.partial);
        } else if member.id != root.id {
            missing.insert(member.id.as_str());
        }
    }
    for session in sessions.iter().filter(|session| seen.contains(&session.id)) {
        for event in &session.agent_events {
            if !event.thread_id.is_empty() && !known.contains(event.thread_id.as_str()) {
                missing.insert(event.thread_id.as_str());
            }
        }
    }
    let missing_children = missing.len();
    drop(missing);
    SessionStats {
        members,
        cost,
        missing_children,
        incomplete_members,
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
        let stats = rollup(&root, &sessions, &[]);
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
        let stats = rollup(&root, &sessions, &[]);
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
        let stats = rollup(&root, &sessions, &[]);
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
        let stats = rollup(&root, &sessions, &[]);
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
        let stats = rollup(&root, &sessions, &[]);
        assert_eq!(stats.missing_children, 2);
        assert_eq!(stats.members.len(), 2);
    }

    // Rollup returns derived totals and leaves the original cost, current-turn tokens, and TPS untouched.
    #[test]
    fn aggregation_never_mutates_source_sessions() {
        let root = session("root", "", "conversation", 1);
        let sessions = vec![session("child", "root", "subagent", 2)];
        let before = serde_json::to_value((&root, &sessions)).unwrap();
        let stats = rollup(&root, &sessions, &[]);
        assert_eq!(stats.cost.output_tokens, 6);
        assert_eq!(root.output_tokens, 999);
        assert_eq!(root.request_tps, Some(77.0));
        assert_eq!(serde_json::to_value((&root, &sessions)).unwrap(), before);
    }

    // A new parent turn can clear timeline notices while the persisted child cost stays in the total.
    #[test]
    fn persisted_child_outside_recent_sessions_is_still_included() {
        let mut root = session("root", "", "conversation", 1);
        let child = session("child", "root", "subagent", 2);
        root.agent_events = vec![event("child")];
        let index = vec![
            CostMember::from_session(&root),
            CostMember::from_session(&child),
        ];
        crate::model::apply(
            &mut root,
            &serde_json::json!({"type":"event_msg","timestamp_ms":2000,
            "payload":{"type":"task_started","turn_id":"next"}}),
        );
        assert!(root.agent_events.is_empty());
        let stats = rollup(&root, &[], &index);
        assert_eq!(stats.members.len(), 2);
        assert_eq!(stats.cost.uncached_usd, 3.0);
        assert_eq!(stats.missing_children, 0);
    }

    // A known but unread child remains partial, while a cached grandchild still contributes once.
    #[test]
    fn unread_intermediate_child_is_partial_without_hiding_known_descendants() {
        let root = session("root", "", "conversation", 1);
        let child = CostMember {
            id: "child".into(),
            parent_id: "root".into(),
            thread_kind: "subagent".into(),
            cost: None,
            partial: true,
            ..CostMember::default()
        };
        let grandchild = session("grandchild", "child", "subagent", 3);
        let index = vec![child, CostMember::from_session(&grandchild)];
        let stats = rollup(&root, &[], &index);
        assert_eq!(stats.members.len(), 3);
        assert_eq!(stats.cost.uncached_usd, 4.0);
        assert_eq!(stats.missing_children, 1);
    }

    // An observed child with uncertain cost keeps the known amount but marks the group partial.
    #[test]
    fn incomplete_child_does_not_turn_unknown_cost_into_zero() {
        let root = session("root", "", "conversation", 1);
        let mut child = CostMember::from_session(&session("child", "root", "subagent", 2));
        child.partial = true;
        let stats = rollup(&root, &[], &[child]);
        assert_eq!(stats.cost.uncached_usd, 3.0);
        assert!(stats.incomplete_members >= 1);
    }
}
