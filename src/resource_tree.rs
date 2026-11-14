use crate::model::CostMember;
use crate::resources::{self, ProcessRow, Snapshot};
use crate::theme;
use eframe::egui::{self, RichText};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Clone, Default)]
pub(crate) struct Usage {
    pub cpu_percent: Option<f32>,
    pub memory_bytes: u64,
    pub process_count: usize,
    pub cpu_count: usize,
    pub memory_count: usize,
}

impl Usage {
    /// A missing process metric stays unknown instead of becoming a measured zero.
    fn add(&mut self, row: &ProcessRow) {
        self.process_count += 1;
        if let Some(cpu) = row.cpu_percent {
            self.cpu_percent = Some(self.cpu_percent.unwrap_or(0.0) + cpu);
            self.cpu_count += 1;
        }
        if let Some(bytes) = row.memory_bytes {
            self.memory_bytes = self.memory_bytes.saturating_add(bytes);
            self.memory_count += 1;
        }
    }

    fn label(&self) -> String {
        let cpu = self
            .cpu_percent
            .map_or_else(|| "—".into(), |n| format!("{n:.1}%"));
        let memory = if self.memory_count == 0 {
            "—".into()
        } else {
            format!("{:.1} MiB", self.memory_bytes as f64 / 1_048_576.0)
        };
        let partial =
            if self.cpu_count < self.process_count || self.memory_count < self.process_count {
                " · 部分读数"
            } else {
                ""
            };
        format!(
            "CPU {cpu} · {memory} · {} 进程{partial}",
            self.process_count
        )
    }
}

struct Node {
    title: String,
    kind: String,
    parent: Option<String>,
    own: Vec<usize>,
    children: Vec<String>,
}

pub(crate) struct SessionTree<'a> {
    rows: &'a [ProcessRow],
    nodes: BTreeMap<String, Node>,
    all: Vec<usize>,
    shared: Vec<usize>,
    unassigned: Vec<usize>,
}

impl<'a> SessionTree<'a> {
    /// Keep live owner sessions and their ancestors; idle history does not crowd the resource tree.
    pub fn new(rows: &'a [ProcessRow], metadata: &[CostMember], selected: Option<&str>) -> Self {
        let catalog: HashMap<_, _> = metadata.iter().map(|m| (m.id.as_str(), m)).collect();
        let mut tree = Self {
            rows,
            nodes: BTreeMap::new(),
            all: Vec::new(),
            shared: Vec::new(),
            unassigned: Vec::new(),
        };
        let mut seen = HashSet::new();
        for (index, row) in rows.iter().enumerate() {
            if !seen.insert((row.pid, row.start_time)) {
                continue;
            }
            tree.all.push(index);
            if row.shared {
                tree.shared.push(index);
            } else if let Some(id) = row.thread_id.as_deref() {
                tree.ensure_node(id, &catalog);
                tree.nodes.get_mut(id).unwrap().own.push(index);
            } else {
                tree.unassigned.push(index);
            }
        }
        if let Some(id) = selected {
            tree.ensure_node(id, &catalog);
        }
        let edges: Vec<_> = tree
            .nodes
            .iter()
            .filter_map(|(id, n)| n.parent.as_ref().map(|p| (p.clone(), id.clone())))
            .collect();
        for (parent, id) in edges {
            if let Some(node) = tree.nodes.get_mut(&parent) {
                node.children.push(id);
            }
        }
        tree
    }

    fn ensure_node(&mut self, id: &str, catalog: &HashMap<&str, &CostMember>) {
        let mut current = Some(id.to_owned());
        let mut seen = HashSet::new();
        while let Some(id) = current {
            if !seen.insert(id.clone()) || self.nodes.contains_key(&id) {
                break;
            }
            let member = catalog.get(id.as_str());
            let kind = member.map_or("unknown", |m| m.thread_kind.as_str());
            let parent = member
                .filter(|m| {
                    matches!(kind, "subagent" | "side_chat")
                        && !m.parent_id.is_empty()
                        && m.parent_id != id
                })
                .map(|m| m.parent_id.clone());
            let title = member
                .map(|m| {
                    if !m.title.trim().is_empty() {
                        m.title.clone()
                    } else if !m.agent_name.trim().is_empty() {
                        m.agent_name.clone()
                    } else {
                        format!("会话 {}", short(&id, 8))
                    }
                })
                .unwrap_or_else(|| format!("归属信息未加载 · {}", short(&id, 8)));
            self.nodes.insert(
                id,
                Node {
                    title,
                    kind: kind.to_owned(),
                    parent: parent.clone(),
                    own: Vec::new(),
                    children: Vec::new(),
                },
            );
            current = parent;
        }
    }

    /// Each process identity contributes once, even when a parent includes nested sessions.
    fn indices(&self, id: &str) -> Vec<usize> {
        let mut pending = vec![id];
        let mut visited = HashSet::new();
        let mut rows = HashSet::new();
        while let Some(id) = pending.pop() {
            if !visited.insert(id) {
                continue;
            }
            if let Some(node) = self.nodes.get(id) {
                rows.extend(node.own.iter().copied());
                pending.extend(node.children.iter().map(String::as_str));
            }
        }
        let mut rows: Vec<_> = rows.into_iter().collect();
        rows.sort_unstable();
        rows
    }

    fn usage_for(&self, indices: &[usize]) -> Usage {
        let mut usage = Usage::default();
        for index in indices {
            usage.add(&self.rows[*index]);
        }
        usage
    }

    pub fn usage(&self, focused: Option<&str>) -> Usage {
        match focused {
            Some(id) => self.usage_for(&self.indices(id)),
            None => self.usage_for(&self.all),
        }
    }

    pub fn show(
        &self,
        ui: &mut egui::Ui,
        snapshot: &Snapshot,
        focused: Option<&str>,
        selected_session: Option<&str>,
        selected_process: &mut Option<(u32, u64)>,
    ) {
        egui::ScrollArea::both()
            .id_salt(("resource-session-tree", focused))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let mut shown = HashSet::new();
                if let Some(id) = focused {
                    self.branch(
                        ui,
                        snapshot,
                        id,
                        selected_session,
                        selected_process,
                        &mut shown,
                    );
                } else {
                    for (id, node) in &self.nodes {
                        if node
                            .parent
                            .as_ref()
                            .is_none_or(|parent| !self.nodes.contains_key(parent))
                        {
                            self.branch(
                                ui,
                                snapshot,
                                id,
                                selected_session,
                                selected_process,
                                &mut shown,
                            );
                        }
                    }
                    for id in self.nodes.keys() {
                        if !shown.contains(id) {
                            self.branch(
                                ui,
                                snapshot,
                                id,
                                selected_session,
                                selected_process,
                                &mut shown,
                            );
                        }
                    }
                }
                self.unowned(
                    ui,
                    snapshot,
                    "共享 Codex 后台进程",
                    "以下是全局共享资源，未分摊到任何会话。",
                    &self.shared,
                    selected_process,
                );
                self.unowned(
                    ui,
                    snapshot,
                    "未归属工具进程",
                    "以下进程缺少可读取的会话标识，暂不计入单个会话。",
                    &self.unassigned,
                    selected_process,
                );
            });
    }

    fn branch(
        &self,
        ui: &mut egui::Ui,
        snapshot: &Snapshot,
        id: &str,
        selected_session: Option<&str>,
        selected_process: &mut Option<(u32, u64)>,
        shown: &mut HashSet<String>,
    ) {
        if !shown.insert(id.to_owned()) {
            return;
        }
        let Some(node) = self.nodes.get(id) else {
            return;
        };
        let tag = match node.kind.as_str() {
            "subagent" => "子智能体",
            "side_chat" => "侧边聊天",
            _ => "会话",
        };
        let usage = self.usage_for(&self.indices(id));
        let heading = format!("{tag} · {}  |  {}", short(&node.title, 28), usage.label());
        egui::CollapsingHeader::new(RichText::new(heading).color(theme::INK))
            .id_salt(("resource-session", id))
            .default_open(self.is_selected_ancestor(id, selected_session))
            .show(ui, |ui| {
                ui.small(format!("会话 {id} · 含子会话；下方进程为本会话自身用量。"));
                if node.own.is_empty() {
                    ui.small("当前没有已归属的独占进程，共享部分另列。");
                } else {
                    resources::process_rows(ui, snapshot, &node.own, selected_process, id);
                }
                for child in &node.children {
                    self.branch(
                        ui,
                        snapshot,
                        child,
                        selected_session,
                        selected_process,
                        shown,
                    );
                }
            })
            .header_response
            .on_hover_text(&node.title);
    }

    fn is_selected_ancestor(&self, id: &str, selected: Option<&str>) -> bool {
        let mut current = selected;
        let mut seen = HashSet::new();
        while let Some(candidate) = current {
            if !seen.insert(candidate) {
                break;
            }
            if candidate == id {
                return true;
            }
            current = self
                .nodes
                .get(candidate)
                .and_then(|node| node.parent.as_deref());
        }
        false
    }

    fn unowned(
        &self,
        ui: &mut egui::Ui,
        snapshot: &Snapshot,
        title: &str,
        note: &str,
        indices: &[usize],
        selected: &mut Option<(u32, u64)>,
    ) {
        if indices.is_empty() {
            return;
        }
        egui::CollapsingHeader::new(format!(
            "{title}（全局） | {}",
            self.usage_for(indices).label()
        ))
        .id_salt(("unowned-processes", title))
        .default_open(false)
        .show(ui, |ui| {
            ui.small(note);
            resources::process_rows(ui, snapshot, indices, selected, title);
        });
    }
}

fn short(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let mut value: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        value.push('…');
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: u32, owner: Option<&str>, cpu: f32, memory: u64) -> ProcessRow {
        ProcessRow {
            pid,
            start_time: 1,
            thread_id: owner.map(str::to_owned),
            cpu_percent: Some(cpu),
            memory_bytes: Some(memory),
            ..ProcessRow::default()
        }
    }

    // A parent includes owned descendants once; shared and unassigned processes remain global.
    #[test]
    fn session_totals_are_unique_and_do_not_divide_shared_resources() {
        let metadata = vec![
            CostMember {
                id: "root".into(),
                ..CostMember::default()
            },
            CostMember {
                id: "child".into(),
                parent_id: "root".into(),
                thread_kind: "subagent".into(),
                ..CostMember::default()
            },
        ];
        let rows = vec![
            process(1, Some("root"), 1.0, 100),
            process(2, Some("child"), 2.0, 200),
            process(2, Some("child"), 2.0, 200),
            ProcessRow {
                shared: true,
                ..process(3, None, 4.0, 400)
            },
            process(4, None, 8.0, 800),
        ];
        let tree = SessionTree::new(&rows, &metadata, Some("root"));
        let parent = tree.usage(Some("root"));
        assert_eq!(parent.process_count, 2);
        assert_eq!(parent.cpu_percent, Some(3.0));
        assert_eq!(parent.memory_bytes, 300);
        assert_eq!(tree.usage(Some("child")).memory_bytes, 200);
        assert_eq!(tree.usage(None).process_count, 4);
        assert_eq!(tree.shared.len(), 1);
        assert_eq!(tree.unassigned.len(), 1);
    }

    // A tagged process stays visible without transcript metadata, while missing metrics stay unknown.
    #[test]
    fn unknown_session_and_unreadable_metrics_are_not_measured_zero() {
        let rows = vec![ProcessRow {
            pid: 1,
            start_time: 1,
            thread_id: Some("unknown".into()),
            ..ProcessRow::default()
        }];
        let tree = SessionTree::new(&rows, &[], Some("unknown"));
        assert!(tree.nodes["unknown"].title.contains("未加载"));
        let usage = tree.usage(Some("unknown"));
        assert_eq!(usage.process_count, 1);
        assert_eq!(usage.cpu_percent, None);
        assert_eq!(usage.memory_count, 0);
        assert_eq!(tree.usage(Some("other")).process_count, 0);
    }
}
