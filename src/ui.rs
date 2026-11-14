use crate::collector;
use crate::model::{
    is_agent_tool, now_ms, AgentEvent, Compaction, CostMember, CostSummary, ModelRequest, Session,
    Snapshot, Tool, TurnTimeline,
};
use crate::stats::SessionStats;
use crate::{pricing, resources, stats, theme};
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, Vec2};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::Duration;

#[derive(Clone, Copy, Default, PartialEq)]
enum Filter {
    #[default]
    All,
    Tools,
    Models,
    Compactions,
}

#[derive(Default)]
struct SessionOrder {
    active: Vec<String>,
}

struct OrderActivity<'a> {
    session: &'a Session,
    active: bool,
    last_ms: i64,
}

// Activity updates never reorder members of the active group; idle chats use recency.
impl SessionOrder {
    fn arrange(&mut self, sessions: &[Session], now: i64) -> Vec<String> {
        let activity = ordering_activity(sessions, now);
        self.active
            .retain(|id| activity.iter().any(|s| &s.session.id == id && s.active));
        let mut newly_active: Vec<_> = activity
            .iter()
            .filter(|s| s.active && !self.active.contains(&s.session.id))
            .collect();
        newly_active.sort_by(|a, b| {
            b.last_ms
                .cmp(&a.last_ms)
                .then(a.session.id.cmp(&b.session.id))
        });
        self.active
            .extend(newly_active.into_iter().map(|s| s.session.id.clone()));
        let mut idle: Vec<_> = activity.iter().filter(|s| !s.active).collect();
        idle.sort_by(|a, b| {
            b.last_ms
                .cmp(&a.last_ms)
                .then(a.session.id.cmp(&b.session.id))
        });
        self.active
            .iter()
            .cloned()
            .chain(idle.into_iter().map(|s| s.session.id.clone()))
            .collect()
    }
}

/// A running child keeps its ancestors active without changing their measured metrics.
fn ordering_activity(sessions: &[Session], now: i64) -> Vec<OrderActivity<'_>> {
    let indices: HashMap<_, _> = sessions
        .iter()
        .enumerate()
        .map(|(i, s)| (s.id.as_str(), i))
        .collect();
    let mut activity: Vec<_> = sessions
        .iter()
        .map(|session| OrderActivity {
            session,
            active: is_active(session, now),
            last_ms: session.last_ms,
        })
        .collect();
    for session in sessions {
        let mut current = session;
        let mut visited = HashSet::new();
        while child_label(current).is_some()
            && visited.len() < 32
            && visited.insert(current.id.as_str())
        {
            let Some(&index) = indices.get(current.parent_id.as_str()) else {
                break;
            };
            activity[index].active |= is_active(session, now);
            activity[index].last_ms = activity[index].last_ms.max(session.last_ms);
            current = &sessions[index];
        }
    }
    activity
}

fn child_label(session: &Session) -> Option<&'static str> {
    match session.thread_kind.as_str() {
        "subagent" => Some("子智能体"),
        "side_chat" => Some("侧边聊天"),
        _ => None,
    }
}

struct SessionTree<'a> {
    sessions: HashMap<&'a str, &'a Session>,
    roots: Vec<&'a str>,
    children: HashMap<&'a str, Vec<&'a str>>,
    orphans: Vec<&'a str>,
}

impl<'a> SessionTree<'a> {
    fn new(sessions: &'a [Session], ordered: &'a [String], show_internal: bool) -> Self {
        let sessions: HashMap<_, _> = sessions
            .iter()
            .filter(|s| show_internal || s.thread_kind != "internal")
            .map(|s| (s.id.as_str(), s))
            .collect();
        let mut tree = Self {
            sessions,
            roots: Vec::new(),
            children: HashMap::new(),
            orphans: Vec::new(),
        };
        for id in ordered {
            let Some(&session) = tree.sessions.get(id.as_str()) else {
                continue;
            };
            if child_label(session).is_none() {
                tree.roots.push(id);
            } else {
                tree.children
                    .entry(&session.parent_id)
                    .or_default()
                    .push(id);
                if tree.root_for(session).is_none() {
                    tree.orphans.push(id);
                }
            }
        }
        tree
    }

    /// Missing ancestors and cycles remain explicit, never promoted to ordinary root chats.
    fn root_for(&self, mut session: &'a Session) -> Option<&'a str> {
        let mut visited = HashSet::new();
        while child_label(session).is_some() {
            if visited.len() >= 32 || !visited.insert(session.id.as_str()) {
                return None;
            }
            session = self.sessions.get(session.parent_id.as_str())?;
        }
        Some(session.id.as_str())
    }

    fn show(&self, ui: &mut egui::Ui, selected: &mut Option<String>) {
        let mut shown = HashSet::new();
        for id in &self.roots {
            self.show_branch(ui, id, selected, &mut shown, 0);
        }
        if !self.orphans.is_empty() {
            egui::CollapsingHeader::new(format!("父会话未加载 · {}", self.orphans.len()))
                .id_salt("orphan_sessions")
                .default_open(false)
                .show(ui, |ui| {
                    for id in &self.orphans {
                        self.show_branch(ui, id, selected, &mut shown, 0);
                    }
                });
        }
    }

    fn show_branch(
        &self,
        ui: &mut egui::Ui,
        id: &'a str,
        selected: &mut Option<String>,
        shown: &mut HashSet<&'a str>,
        depth: usize,
    ) {
        if depth >= 32 || !shown.insert(id) {
            return;
        }
        let Some(session) = self.sessions.get(id) else {
            return;
        };
        ui.push_id(("session_branch", id), |ui| {
            if session_card(ui, session, selected.as_deref() == Some(id)).clicked() {
                *selected = Some(id.to_owned());
            }
            if let Some(children) = self.children.get(id) {
                egui::CollapsingHeader::new(
                    RichText::new(format!("子会话 {}", children.len()))
                        .size(10.5)
                        .color(theme::MUTED),
                )
                .id_salt(("child_sessions", id))
                .default_open(false)
                .show(ui, |ui| {
                    for child in children {
                        self.show_branch(ui, child, selected, shown, depth + 1);
                    }
                });
            }
            ui.add_space(8.0);
        });
    }
}

#[derive(Clone, Copy, Default, PartialEq)]
enum Page {
    #[default]
    Sessions,
    Processes,
}

fn is_active(session: &Session, _now: i64) -> bool {
    session.started_ms.is_some() && session.ended_ms.is_none()
}

pub struct Monitor {
    receiver: Receiver<Snapshot>,
    snapshot: Snapshot,
    selected: Option<String>,
    source: PathBuf,
    order: SessionOrder,
    ordered_ids: Vec<String>,
    show_internal: bool,
    filter: Filter,
    timeline_search: String,
    page: Page,
    resources: resources::Monitor,
}

impl Monitor {
    /// Selection and layout share a single event-driven collector.
    pub fn new(context: &eframe::CreationContext<'_>, source: PathBuf, db: PathBuf) -> Self {
        theme::install(&context.egui_ctx);
        Self {
            receiver: collector::start(source.clone(), db, context.egui_ctx.clone()),
            snapshot: Snapshot::default(),
            selected: None,
            source,
            order: SessionOrder::default(),
            ordered_ids: Vec::new(),
            show_internal: false,
            filter: Filter::default(),
            timeline_search: String::new(),
            page: Page::default(),
            resources: resources::Monitor::new(context.egui_ctx.clone()),
        }
    }

    /// Update data without moving an existing selection or racing active sessions.
    fn receive(&mut self) {
        while let Ok(snapshot) = self.receiver.try_recv() {
            self.snapshot = snapshot;
        }
        self.ordered_ids = self.order.arrange(&self.snapshot.sessions, now_ms());
        let visible = |s: &&Session| self.show_internal || s.thread_kind != "internal";
        let exists = self
            .snapshot
            .sessions
            .iter()
            .filter(visible)
            .any(|s| Some(&s.id) == self.selected.as_ref());
        if !exists {
            self.selected = self
                .ordered_ids
                .iter()
                .find(|id| {
                    self.snapshot.sessions.iter().any(|s| {
                        &s.id == *id
                            && child_label(s).is_none()
                            && (self.show_internal || s.thread_kind != "internal")
                    })
                })
                .cloned();
        }
    }

    fn sidebar(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("sessions")
            .default_width(245.0)
            .min_width(220.0)
            .max_width(320.0)
            .resizable(true)
            .frame(egui::Frame::new().fill(theme::BG).inner_margin(16))
            .show(ctx, |ui| {
                brand(ui);
                ui.add_space(24.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("会话").strong().size(14.0));
                    let count = self
                        .snapshot
                        .sessions
                        .iter()
                        .filter(|s| self.show_internal || s.thread_kind != "internal")
                        .count();
                    badge(ui, &count.to_string(), theme::SURFACE, theme::MUTED);
                });
                ui.add_space(8.0);
                ui.checkbox(&mut self.show_internal, "显示内部任务");
                ui.add_space(12.0);
                egui::ScrollArea::vertical()
                    .id_salt("sessions_scroll")
                    .show(ui, |ui| {
                        SessionTree::new(
                            &self.snapshot.sessions,
                            &self.ordered_ids,
                            self.show_internal,
                        )
                        .show(ui, &mut self.selected);
                    });
            });
    }
}

impl eframe::App for Monitor {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.receive();
        self.sidebar(ctx);
        egui::TopBottomPanel::bottom("footnote")
            .frame(
                egui::Frame::new()
                    .fill(theme::BG)
                    .inner_margin(egui::Margin::symmetric(22, 8)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    dot(ui, theme::PINK);
                    ui.label(RichText::new("本地采集").small().color(theme::MUTED))
                        .on_hover_text(self.source.display().to_string());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new("事件驱动 · 低频重绘")
                                .small()
                                .color(theme::MUTED),
                        );
                    });
                });
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::new().fill(theme::BG).inner_margin(22))
            .show(ctx, |ui| {
                page_tabs(ui, &mut self.page);
                ui.add_space(12.0);
                self.resources.set_visible(self.page == Page::Processes);
                if self.page == Page::Processes {
                    self.resources
                        .show(ui, ctx, &self.snapshot.costs, self.selected.as_deref());
                    return;
                }
                if let Some(error) = &self.snapshot.error {
                    theme::card(theme::YELLOW_SOFT).show(ui, |ui| {
                        ui.label(RichText::new(error).color(theme::INK).small());
                    });
                    ui.add_space(10.0);
                }
                if let Some(s) = self
                    .snapshot
                    .sessions
                    .iter()
                    .find(|s| Some(&s.id) == self.selected.as_ref())
                {
                    egui::ScrollArea::vertical()
                        .id_salt(("session_content", &s.id))
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            render_session(
                                ui,
                                s,
                                &self.snapshot.sessions,
                                &self.snapshot.costs,
                                &mut self.filter,
                                &mut self.timeline_search,
                            )
                        });
                    let minimized = ctx.input(|i| i.viewport().minimized == Some(true));
                    if is_active(s, now_ms()) && !minimized {
                        ctx.request_repaint_after(Duration::from_secs(1));
                    }
                } else {
                    empty_state(ui);
                }
            });
    }
}

/// Constrain navigation to one row so it cannot consume the content viewport height.
fn page_tabs(ui: &mut egui::Ui, page: &mut Page) {
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.selectable_value(page, Page::Processes, "进程资源");
            ui.selectable_value(page, Page::Sessions, "会话分析");
        });
    });
}

fn brand(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(39.0), Sense::hover());
        ui.painter().rect(
            rect,
            2,
            theme::YELLOW,
            Stroke::new(1.0_f32, theme::BORDER),
            egui::StrokeKind::Inside,
        );
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            "CP",
            FontId::monospace(14.0),
            theme::INK,
        );
        ui.add_space(5.0);
        ui.vertical(|ui| {
            ui.label(RichText::new("CodexProfiler").size(12.0).strong());
            ui.label(
                RichText::new("每一步，都看得见")
                    .size(11.0)
                    .color(theme::MUTED),
            );
        });
    });
}

/// Fixed-height cards stop titles and activity updates from shifting the session list.
fn session_card(ui: &mut egui::Ui, s: &Session, selected: bool) -> egui::Response {
    let child = child_label(s);
    let height = if child.is_some() { 104.0 } else { 82.0 };
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click());
    let fill = if selected {
        theme::PINK_SOFT
    } else if response.hovered() {
        theme::SURFACE
    } else {
        theme::BG
    };
    let stroke = if selected { theme::PINK } else { theme::BORDER };
    ui.painter().rect(
        rect,
        2,
        fill,
        Stroke::new(1.0_f32, stroke),
        egui::StrokeKind::Inside,
    );
    if selected {
        let rail = Rect::from_min_size(rect.min + Vec2::new(0.0, 21.0), Vec2::new(3.0, 40.0));
        ui.painter().rect_filled(rail, 2, theme::PINK);
    }
    if let Some(label) = child {
        let fill = if s.thread_kind == "subagent" {
            theme::PINK_SOFT
        } else {
            theme::YELLOW_SOFT
        };
        let tag = if s.agent_name.is_empty() {
            label.to_owned()
        } else {
            format!("{label} · {}", s.agent_name)
        };
        let tag_rect = Rect::from_min_size(
            rect.min + Vec2::new(13.0, 9.0),
            Vec2::new(rect.width() - 26.0, 18.0),
        );
        ui.painter().rect_filled(tag_rect, 2, fill);
        ui.painter().with_clip_rect(tag_rect).text(
            tag_rect.left_center() + Vec2::new(5.0, 0.0),
            Align2::LEFT_CENTER,
            tag,
            FontId::proportional(9.5),
            theme::INK,
        );
    }
    let title = display_title(s);
    let mut job = egui::text::LayoutJob::simple(
        title.clone(),
        FontId::proportional(12.0),
        theme::INK,
        rect.width() - 25.0,
    );
    job.wrap.max_rows = 2;
    let galley = ui.fonts(|fonts| fonts.layout_job(job));
    ui.painter().galley(
        rect.min + Vec2::new(13.0, if child.is_some() { 33.0 } else { 11.0 }),
        galley,
        theme::INK,
    );
    let active = is_active(s, now_ms());
    let color = if active { theme::PINK } else { theme::MUTED };
    ui.painter()
        .circle_filled(rect.min + Vec2::new(16.0, height - 18.0), 3.0, color);
    ui.painter().text(
        rect.min + Vec2::new(26.0, height - 18.0),
        Align2::LEFT_CENTER,
        short_phase(s),
        FontId::proportional(10.5),
        theme::MUTED,
    );
    response.on_hover_text(format!("{}\n{}\n{}", title, s.phase, s.cwd))
}

fn short_phase(s: &Session) -> &str {
    if s.phase.contains("失败") {
        "执行异常"
    } else if s.phase.contains("工具") {
        "工具执行中"
    } else if s.phase.contains("模型请求进行") {
        "模型响应中"
    } else if s.phase.contains("批准") {
        "等待批准"
    } else if s.ended_ms.is_some() {
        "已完成"
    } else {
        "进行中"
    }
}

fn display_title(s: &Session) -> String {
    if s.title.trim().is_empty() {
        format!("未命名会话 · {}", shortened(&s.id, 8))
    } else {
        s.title.clone()
    }
}

fn empty_state(ui: &mut egui::Ui) {
    ui.add_space(70.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new("准备好，观察下一步").size(22.0).strong());
        ui.add_space(10.0);
        ui.label(
            RichText::new("继续一个 Codex 会话，运行记录会自动出现在这里。").color(theme::MUTED),
        );
    });
}

fn render_session(
    ui: &mut egui::Ui,
    s: &Session,
    sessions: &[Session],
    costs: &[CostMember],
    filter: &mut Filter,
    search: &mut String,
) {
    ui.label(
        RichText::new("SESSION OVERVIEW")
            .size(10.0)
            .color(theme::MUTED),
    );
    ui.add_space(5.0);
    ui.add(egui::Label::new(RichText::new(display_title(s)).size(20.0).strong()).wrap());
    ui.add_space(8.0);
    ui.horizontal_wrapped(|ui| {
        badge(ui, &s.model, theme::SURFACE, theme::INK);
        badge(ui, tier_name(&s.service_tier), theme::PINK_SOFT, theme::INK);
        badge(ui, short_phase(s), theme::YELLOW_SOFT, theme::INK);
    });
    egui::CollapsingHeader::new(RichText::new("会话详情").size(11.0).color(theme::MUTED))
        .id_salt(("session_details", &s.id))
        .show(ui, |ui| {
            meta_line(ui, "会话", &s.id);
            meta_line(ui, "轮次", &s.turn_id);
            meta_line(ui, "状态", &s.phase);
            meta_line(ui, "档位来源", &s.service_tier_source);
            meta_line(ui, "目录", &s.cwd);
        });
    ui.add_space(18.0);
    let totals = stats::rollup(s, sessions, costs);
    metrics(ui, s, &totals);
    cost_details(ui, s, &totals);
    ui.add_space(20.0);
    render_timeline(ui, s, sessions, costs, filter, search);
}

fn task_elapsed(s: &Session) -> Option<i64> {
    s.started_ms.map(|start| {
        s.ended_ms
            .unwrap_or_else(now_ms)
            .saturating_sub(start)
            .max(0)
    })
}

/// Cards keep primary metrics readable without placing large identity strings in the overview.
fn metrics(ui: &mut egui::Ui, s: &Session, totals: &SessionStats) {
    let elapsed = task_elapsed(s);
    let tps = s
        .request_tps
        .map(|n| format!("{n:.1}"))
        .unwrap_or_else(|| "—".into());
    let task_rate = elapsed
        .filter(|ms| *ms > 0)
        .map(|ms| {
            format!(
                "整体 {:.1} tok/s",
                s.output_tokens as f64 * 1000.0 / ms as f64
            )
        })
        .unwrap_or_else(|| "含工具执行和等待".into());
    let values = [
        (
            "模型请求速率",
            tps,
            "本会话 · 最近一次有效请求 tok/s".to_owned(),
            theme::PINK_SOFT,
        ),
        (
            "任务总耗时",
            elapsed.map(compact_duration).unwrap_or_else(|| "—".into()),
            "本会话当前轮次 · 本机观测".to_owned(),
            theme::YELLOW_SOFT,
        ),
        (
            "输出 tokens",
            number(s.output_tokens),
            task_rate,
            theme::SURFACE,
        ),
        (
            "会话 API 估算",
            group_cost(totals),
            if totals.missing_children > 0 {
                format!("部分汇总 · {} 个子会话待补读", totals.missing_children)
            } else if totals.incomplete_members > 0 {
                format!("部分估算 · {} 个会话用量待核对", totals.incomplete_members)
            } else if totals.cost.unpriced_requests > 0 {
                format!("部分估算 · {} 次未定价", totals.cost.unpriced_requests)
            } else if totals.members.len() > 1 {
                format!("全部轮次 · 含 {} 个子会话", totals.members.len() - 1)
            } else {
                "全部轮次 · 非订阅账单".into()
            },
            theme::SURFACE,
        ),
    ];
    let columns = if ui.available_width() >= 660.0 { 4 } else { 2 };
    for row in values.chunks(columns) {
        ui.columns(columns, |uis| {
            for (column, (title, value, note, fill)) in row.iter().enumerate() {
                metric(&mut uis[column], title, value, note, *fill);
            }
        });
        if columns == 2 {
            ui.add_space(10.0);
        }
    }
}

fn metric(ui: &mut egui::Ui, title: &str, value: &str, note: &str, fill: Color32) {
    theme::card(fill).show(ui, |ui| {
        ui.set_width((ui.available_width()).max(1.0));
        ui.set_min_height(84.0);
        ui.label(RichText::new(title).size(11.0).color(theme::MUTED));
        ui.add_space(8.0);
        ui.label(RichText::new(value).size(23.0).strong());
        ui.add_space(7.0);
        ui.add(egui::Label::new(RichText::new(note).size(9.0).color(theme::MUTED)).truncate());
    });
}

#[derive(Clone, Copy)]
enum TimelineRow<'a> {
    Tool(&'a Tool),
    Agent(&'a AgentEvent),
    Model(&'a ModelRequest),
    Compaction(&'a Compaction),
}

/// Borrow each row's original turn so details never search a different turn's records.
#[derive(Clone, Copy)]
struct TimelineScope<'a> {
    turn_id: &'a str,
    tools: &'a [Tool],
    agent_events: &'a [AgentEvent],
    requests: &'a [ModelRequest],
    compactions: &'a [Compaction],
    archived: bool,
}

impl<'a> TimelineScope<'a> {
    fn current(session: &'a Session) -> Self {
        Self {
            turn_id: &session.turn_id,
            tools: &session.tools,
            agent_events: &session.agent_events,
            requests: &session.requests,
            compactions: &session.compactions,
            archived: false,
        }
    }

    fn history(turn: &'a TurnTimeline) -> Self {
        Self {
            turn_id: &turn.turn_id,
            tools: &turn.tools,
            agent_events: &turn.agent_events,
            requests: &turn.requests,
            compactions: &turn.compactions,
            archived: true,
        }
    }
}

#[derive(Clone, Copy)]
struct TimelineEntry<'a> {
    row: TimelineRow<'a>,
    scope: TimelineScope<'a>,
}

impl TimelineRow<'_> {
    fn operation_title(self) -> String {
        let suffix = |compaction: bool| if compaction { " · 压缩" } else { "" };
        match self {
            Self::Tool(t) => format!("{}{}", t.name, suffix(t.compaction_id.is_some())),
            Self::Model(r) => format!(
                "{} · {}{}",
                r.model,
                tier_name(&r.service_tier),
                suffix(r.compaction_id.is_some())
            ),
            Self::Compaction(_) => "上下文压缩".into(),
            Self::Agent(event) => format!(
                "{} · {}",
                agent_event_title(event),
                if event.path.is_empty() {
                    &event.thread_id
                } else {
                    &event.path
                }
            ),
        }
    }

    /// Search the complete operation and command, including text clipped in the table.
    fn matches_search(self, needle: &str) -> bool {
        needle.is_empty()
            || self.operation_title().to_lowercase().contains(needle)
            || matches!(self, Self::Tool(t) if t.details.to_lowercase().contains(needle))
            || matches!(self, Self::Agent(e) if e.thread_id.to_lowercase().contains(needle))
    }

    fn time(&self) -> i64 {
        match self {
            Self::Tool(t) => t.started_ms,
            Self::Agent(e) => e.at_ms,
            Self::Model(r) => r.started_ms.or(r.ended_ms).unwrap_or(0),
            Self::Compaction(c) => c.started_ms.or(c.ended_ms).unwrap_or(0),
        }
    }
    fn key(&self) -> (&str, &str) {
        match self {
            Self::Tool(t) => ("tool", &t.id),
            Self::Agent(e) => ("agent", &e.id),
            Self::Model(r) => ("model", &r.id),
            Self::Compaction(c) => ("compaction", &c.id),
        }
    }

    fn unfinished(self) -> bool {
        match self {
            Self::Tool(t) => t.ended_ms.is_none(),
            Self::Model(r) => r.ended_ms.is_none(),
            Self::Compaction(c) => c.ended_ms.is_none(),
            Self::Agent(_) => false,
        }
    }
}

fn agent_event_title(event: &AgentEvent) -> &str {
    match event.kind.as_str() {
        "started" => "子智能体已启动",
        "interacted" => "子智能体交互",
        "completed" => "子智能体已完成",
        _ => "子智能体事件",
    }
}

#[derive(Clone, Copy)]
struct Columns {
    cells: [Rect; 6],
}

impl Columns {
    /// Only the operation column flexes; every text cell clips to its own track.
    fn new(rect: Rect) -> Self {
        let edges = [
            rect.left() + 21.0,
            rect.left() + 97.0,
            rect.left() + 153.0,
            rect.right() - 226.0,
            rect.right() - 140.0,
            rect.right() - 70.0,
            rect.right(),
        ];
        Self {
            cells: std::array::from_fn(|i| {
                Rect::from_min_max(
                    Pos2::new(edges[i], rect.top()),
                    Pos2::new(edges[i + 1], rect.bottom()),
                )
            }),
        }
    }

    /// Header and every row reuse the same horizontal bounds, including frame padding.
    fn at_row(self, rect: Rect) -> [Rect; 6] {
        self.cells.map(|cell| {
            Rect::from_min_max(
                Pos2::new(cell.left(), rect.top()),
                Pos2::new(cell.right(), rect.bottom()),
            )
        })
    }
}

fn timeline_frame() -> egui::Frame {
    egui::Frame::new()
        .fill(theme::SURFACE)
        .stroke(Stroke::new(1.0_f32, theme::BORDER))
        .corner_radius(2)
        .inner_margin(4)
}

/// The timeline keeps column geometry identical for models and tools, regardless of title length.
fn render_timeline(
    ui: &mut egui::Ui,
    s: &Session,
    sessions: &[Session],
    costs: &[CostMember],
    filter: &mut Filter,
    search: &mut String,
) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("运行时间线").size(15.0).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            filter_buttons(ui, filter);
        });
    });
    ui.label(
        RichText::new("展示会话近期记录，跨轮次保留；子智能体事件也可在“工具”中查看。")
            .small()
            .color(theme::MUTED),
    );
    ui.horizontal(|ui| {
        let width = (ui.available_width() - 65.0).max(80.0);
        ui.add_sized(
            [width, 28.0],
            egui::TextEdit::singleline(search)
                .id_salt("timeline_operation_search")
                .hint_text("搜索操作：工具、命令、模型…"),
        );
        if ui
            .add_enabled(!search.is_empty(), egui::Button::new("清除"))
            .clicked()
        {
            search.clear();
        }
    });
    ui.add_space(7.0);
    let columns = timeline_heading(ui);
    ui.add_space(5.0);
    let rows = timeline_rows(s, *filter, search);
    if rows.is_empty() {
        ui.add_space(30.0);
        ui.label(RichText::new("当前筛选暂无匹配的操作").color(theme::MUTED));
    }
    for entry in rows.into_iter().take(300) {
        ui.push_id((&s.id, entry.scope.turn_id, entry.row.key()), |ui| {
            timeline_card(ui, entry, s, sessions, costs, columns)
        });
        ui.add_space(5.0);
    }
}

/// Apply type and keyword filters together before limiting the visible history.
fn timeline_rows<'a>(s: &'a Session, filter: Filter, search: &str) -> Vec<TimelineEntry<'a>> {
    let mut rows = Vec::new();
    let scopes = std::iter::once(TimelineScope::current(s))
        .chain(s.timeline_history.iter().map(TimelineScope::history));
    for scope in scopes {
        append_timeline_rows(&mut rows, scope, filter);
    }
    let needle = search.trim().to_lowercase();
    rows.retain(|entry| entry.row.matches_search(&needle));
    rows.sort_by(|a, b| {
        b.row
            .time()
            .cmp(&a.row.time())
            .then(a.row.key().cmp(&b.row.key()))
            .then(a.scope.turn_id.cmp(b.scope.turn_id))
    });
    rows
}

fn append_timeline_rows<'a>(
    rows: &mut Vec<TimelineEntry<'a>>,
    scope: TimelineScope<'a>,
    filter: Filter,
) {
    let entry = |row| TimelineEntry { row, scope };
    if matches!(filter, Filter::All | Filter::Tools) {
        rows.extend(scope.tools.iter().map(TimelineRow::Tool).map(entry));
        rows.extend(scope.agent_events.iter().map(TimelineRow::Agent).map(entry));
    }
    if matches!(filter, Filter::All | Filter::Models) {
        rows.extend(scope.requests.iter().map(TimelineRow::Model).map(entry));
    }
    if matches!(filter, Filter::All | Filter::Compactions) {
        rows.extend(
            scope
                .compactions
                .iter()
                .map(TimelineRow::Compaction)
                .map(entry),
        );
    }
}

fn filter_buttons(ui: &mut egui::Ui, filter: &mut Filter) {
    for (value, title) in [
        (Filter::Compactions, "压缩"),
        (Filter::Models, "模型"),
        (Filter::Tools, "工具"),
        (Filter::All, "全部"),
    ] {
        let selected = *filter == value;
        let fill = if selected {
            theme::PINK
        } else {
            theme::SURFACE
        };
        let ink = if selected {
            theme::SURFACE
        } else {
            theme::MUTED
        };
        let button = egui::Button::new(RichText::new(title).size(11.0).color(ink))
            .fill(fill)
            .stroke(Stroke::new(
                1.0_f32,
                if selected { theme::PINK } else { theme::BORDER },
            ))
            .corner_radius(2)
            .min_size(Vec2::new(48.0, 27.0));
        if ui.add(button).clicked() {
            *filter = value;
        }
    }
}

fn timeline_heading(ui: &mut egui::Ui) -> Columns {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 24.0), Sense::hover());
    let cols = Columns::new(rect - timeline_frame().total_margin());
    for (i, title) in ["时间", "类型", "操作", "状态", "耗时", "输出"]
        .into_iter()
        .enumerate()
    {
        timeline_text(
            ui.painter(),
            cols.cells[i],
            if i == 1 {
                Align2::CENTER_CENTER
            } else {
                Align2::LEFT_CENTER
            },
            title,
            FontId::proportional(10.0),
            theme::MUTED,
        );
    }
    cols
}

fn timeline_card(
    ui: &mut egui::Ui,
    entry: TimelineEntry<'_>,
    session: &Session,
    sessions: &[Session],
    costs: &[CostMember],
    columns: Columns,
) {
    timeline_frame().show(ui, |ui| {
        let row = entry.row;
        let id = ui.make_persistent_id(row.key());
        let mut open =
            egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false);
        let (rect, response) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 42.0), Sense::click());
        paint_timeline_row(ui, rect, columns, entry, session.ended_ms, open.is_open());
        if response.clicked() {
            open.toggle(ui);
        }
        response.on_hover_text("点击展开详情");
        open.show_body_unindented(ui, |ui| {
            ui.add_space(6.0);
            meta_line(ui, "轮次", entry.scope.turn_id);
            if entry.scope.archived && row.unfinished() {
                ui.small("结束未观测；历史记录的耗时未知。");
            }
            match row {
                TimelineRow::Tool(t) => {
                    tool_details(ui, t);
                    if is_agent_tool(t) {
                        if let Some(event) = entry.scope.agent_events.iter().find(|e| e.id == t.id)
                        {
                            agent_stats(ui, event, sessions, costs);
                        }
                    }
                }
                TimelineRow::Agent(e) => {
                    agent_details(ui, e);
                    agent_stats(ui, e, sessions, costs);
                }
                TimelineRow::Model(r) => request_details(ui, r),
                TimelineRow::Compaction(c) => compaction_details(ui, c, entry.scope),
            }
        });
    });
}

fn paint_timeline_row(
    ui: &egui::Ui,
    rect: Rect,
    columns: Columns,
    entry: TimelineEntry<'_>,
    task_end: Option<i64>,
    open: bool,
) {
    let row = entry.row;
    let cells = columns.at_row(rect);
    let painter = ui.painter();
    let arrow = if open { "−" } else { "+" };
    painter.text(
        Pos2::new(rect.left() + 7.0, rect.center().y),
        Align2::LEFT_CENTER,
        arrow,
        FontId::monospace(12.0),
        theme::MUTED,
    );
    let (kind, title, status, elapsed, output, fill) =
        row_summary(row, task_end, entry.scope.archived);
    timeline_text(
        painter,
        cells[0],
        Align2::LEFT_CENTER,
        &clock_short(Some(row.time())),
        FontId::monospace(10.0),
        theme::MUTED,
    );
    let badge_rect = Rect::from_center_size(cells[1].center(), Vec2::new(40.0, 21.0));
    painter.rect_filled(badge_rect, 2, fill);
    timeline_text(
        painter,
        cells[1],
        Align2::CENTER_CENTER,
        kind,
        FontId::monospace(10.0),
        theme::INK,
    );
    for (i, text, font, color) in [
        (2, title, FontId::proportional(11.5), theme::INK),
        (3, status, FontId::proportional(10.5), theme::MUTED),
        (4, elapsed, FontId::monospace(10.5), theme::INK),
        (5, output, FontId::monospace(10.5), theme::INK),
    ] {
        timeline_text(painter, cells[i], Align2::LEFT_CENTER, &text, font, color);
    }
}

/// Each cell keeps long values from painting over the next column in narrow windows.
fn timeline_text(
    painter: &egui::Painter,
    cell: Rect,
    align: Align2,
    text: &str,
    font: FontId,
    color: Color32,
) {
    let anchor = if align == Align2::CENTER_CENTER {
        cell.center()
    } else {
        cell.left_center()
    };
    painter
        .with_clip_rect(painter.clip_rect().intersect(cell))
        .text(anchor, align, text, font, color);
}

fn row_summary(
    row: TimelineRow<'_>,
    task_end: Option<i64>,
    archived: bool,
) -> (&str, String, String, String, String, Color32) {
    let status = |end: Option<i64>, reported: &str| {
        if archived && end.is_none() {
            "结束未观测".to_owned()
        } else {
            reported.to_owned()
        }
    };
    match row {
        TimelineRow::Tool(t) => {
            let end = if archived {
                t.ended_ms
            } else {
                t.ended_ms.or(task_end)
            };
            (
                if is_agent_tool(t) { "AGENT" } else { "TOOL" },
                row.operation_title(),
                status(t.ended_ms, &t.status),
                observed_duration(Some(t.started_ms), end, archived),
                "—".into(),
                if is_agent_tool(t) {
                    theme::BLUE
                } else {
                    theme::YELLOW_SOFT
                },
            )
        }
        TimelineRow::Model(r) => {
            let elapsed = observed_duration(r.started_ms, r.ended_ms, archived);
            (
                "LLM",
                row.operation_title(),
                status(r.ended_ms, &r.status),
                elapsed,
                r.output_tokens.map(number).unwrap_or_else(|| "—".into()),
                theme::PINK_SOFT,
            )
        }
        TimelineRow::Compaction(c) => (
            "CMP",
            row.operation_title(),
            status(c.ended_ms, &c.status),
            observed_duration(c.started_ms, c.ended_ms, archived),
            "—".into(),
            theme::YELLOW_SOFT,
        ),
        TimelineRow::Agent(_) => (
            "AGENT",
            row.operation_title(),
            "事件".into(),
            "—".into(),
            "—".into(),
            theme::BLUE,
        ),
    }
}

fn agent_details(ui: &mut egui::Ui, event: &AgentEvent) {
    meta_line(ui, "事件", agent_event_title(event));
    meta_line(ui, "子会话", &event.thread_id);
    meta_line(ui, "子智能体", &event.path);
    meta_line(ui, "时间", &clock(Some(event.at_ms)));
    ui.small("这是状态通知，通知本身不产生模型费用。");
}

/// Reuse the child overview; repeated timeline references never alter stored costs.
fn agent_stats(ui: &mut egui::Ui, event: &AgentEvent, sessions: &[Session], costs: &[CostMember]) {
    ui.separator();
    let Some(child) = sessions
        .iter()
        .find(|s| !event.thread_id.is_empty() && s.id == event.thread_id)
    else {
        ui.small("子会话当前轮次尚未加载，暂时无法显示速率和耗时。");
        if let Some(member) = costs
            .iter()
            .find(|m| !event.thread_id.is_empty() && m.id == event.thread_id)
        {
            if let Some(cost) = &member.cost {
                ui.label(format!(
                    "子会话自身已记录费用 {}",
                    member_cost(member, cost)
                ));
            }
        }
        return;
    };
    ui.label(RichText::new("子会话当前统计").strong());
    ui.small("速率、耗时及输出对应子会话当前轮次；费用为全部轮次累计，已按归属汇总到父会话。");
    let totals = stats::rollup(child, sessions, costs);
    metrics(ui, child, &totals);
    cost_details(ui, child, &totals);
}

fn tool_details(ui: &mut egui::Ui, tool: &Tool) {
    if let Some(id) = &tool.compaction_id {
        meta_line(ui, "所属上下文压缩", id);
    }
    meta_line(ui, "调用 ID", &tool.id);
    ui.label(
        RichText::new(format!(
            "开始 {}    结束 {}",
            clock(Some(tool.started_ms)),
            clock(tool.ended_ms)
        ))
        .small()
        .color(theme::MUTED),
    );
    if tool.details.is_empty() {
        ui.label(
            RichText::new("历史记录没有命令明细")
                .small()
                .color(theme::MUTED),
        );
    } else {
        code_details(ui, &tool.details);
    }
}

fn request_details(ui: &mut egui::Ui, r: &ModelRequest) {
    badge(ui, tier_name(&r.service_tier), theme::PINK_SOFT, theme::INK);
    meta_line(ui, "档位来源", &r.service_tier_source);
    if let Some(id) = &r.compaction_id {
        meta_line(ui, "所属上下文压缩", id);
    }
    ui.label(
        RichText::new(format!(
            "API 估算 {}",
            r.cost_usd.map(money).unwrap_or_else(|| "未知".into())
        ))
        .small()
        .color(theme::MUTED),
    );
    meta_line(ui, "请求 ID", &r.id);
    ui.label(
        RichText::new(format!(
            "开始 {}    结束 {}",
            clock(r.started_ms),
            clock(r.ended_ms)
        ))
        .small()
        .color(theme::MUTED),
    );
    let rate = r
        .started_ms
        .zip(r.ended_ms)
        .zip(r.output_tokens)
        .filter(|((start, end), _)| end > start)
        .map(|((start, end), n)| format!("{:.1} tok/s", n as f64 * 1000.0 / (end - start) as f64))
        .unwrap_or_else(|| "未知 · 缺少完整请求边界".into());
    ui.label(
        RichText::new(format!("有效吞吐量 {rate}  ·  来源 {}", r.source))
            .small()
            .color(theme::MUTED),
    );
}

fn tier_name(tier: &str) -> &str {
    match tier {
        "default" | "standard" | "auto" | "" => "Standard",
        "fast" | "priority" => "Fast",
        "ultrafast" => "Ultrafast",
        "flex" => "Flex",
        "batch" => "Batch",
        other => other,
    }
}

fn money(value: f64) -> String {
    if value.abs() < 0.01 {
        format!("${value:.4}")
    } else {
        format!("${value:.2}")
    }
}

fn session_cost(cost: &CostSummary) -> String {
    if cost.priced_requests == 0 {
        "—".into()
    } else if cost.unpriced_requests > 0 {
        format!("~{}", money(cost.total()))
    } else {
        money(cost.total())
    }
}

fn member_cost(member: &CostMember, cost: &CostSummary) -> String {
    if member.partial && cost.priced_requests > 0 {
        format!("~{}", money(cost.total()))
    } else {
        session_cost(cost)
    }
}

fn group_cost(totals: &SessionStats) -> String {
    if (totals.missing_children > 0 || totals.incomplete_members > 0)
        && totals.cost.priced_requests > 0
    {
        format!("~{}", money(totals.cost.total()))
    } else {
        session_cost(&totals.cost)
    }
}

/// Show whole-chat owned costs, including descendants, without changing source usage records.
fn cost_details(ui: &mut egui::Ui, s: &Session, totals: &SessionStats) {
    let title = RichText::new("成本明细 · API / USD")
        .size(10.0)
        .color(theme::MUTED);
    egui::CollapsingHeader::new(title).id_salt(("cost", &s.id)).show(ui, |ui| {
        cost_grid(ui, s, &totals.cost);
        let cost = &totals.cost;
        let hit = if cost.input_tokens == 0 {
            "未知".into()
        } else {
            format!("{:.1}%", cost.cached_input_tokens as f64 * 100.0 / cost.input_tokens as f64)
        };
        ui.small(format!("缓存命中率 {hit} · 已定价 {} 次 · 未定价 {} 次", cost.priced_requests, cost.unpriced_requests));
        if cost.unpriced_requests > 0 {
            ui.small("当前总额是部分估算，未定价请求未被计为 $0。");
        }
        if totals.incomplete_members > 0 {
            ui.small(format!("{} 个会话的费用记录尚未完整核对，当前为部分估算。", totals.incomplete_members));
        }
        ui.small(format!("本会话本轮已观测：{} 次工具调用 · {} 次模型请求", s.tools.len(), s.requests.len()));
        if totals.members.len() > 1 || totals.missing_children > 0 {
            cost_ownership(ui, totals);
        }
        let note = "全部轮次累计，含已记录的子智能体和侧边聊天，每个会话只计一次。API 等价估算，非订阅账单；30m 缓存写入。价格按每次请求档位调整，含已知长上下文规则；不含工具费用及区域附加费。";
        ui.label(RichText::new(note).small().color(theme::MUTED));
        ui.horizontal_wrapped(|ui| {
            ui.small(format!("价格快照 {}", pricing::AS_OF));
            ui.hyperlink_to("官方价格来源", pricing::SOURCE);
        });
    });
}

/// Itemize each member's own cost so nested children remain easy to reconcile without double counting.
fn cost_ownership(ui: &mut egui::Ui, totals: &SessionStats) {
    ui.separator();
    ui.strong("费用归属 · 各会话自身用量");
    egui::Grid::new("cost_ownership")
        .num_columns(2)
        .show(ui, |ui| {
            for (index, member) in totals.members.iter().enumerate() {
                ui.label(if index == 0 {
                    "本会话自身".into()
                } else {
                    member_title(member)
                });
                ui.monospace(
                    member
                        .cost
                        .as_ref()
                        .map_or_else(|| "待补读".into(), |cost| member_cost(member, cost)),
                );
                ui.end_row();
            }
        });
    if totals.missing_children > 0 {
        ui.small(format!(
            "{} 个关联子会话尚待补读，当前为部分汇总。",
            totals.missing_children
        ));
    }
}

fn member_title(member: &CostMember) -> String {
    if !member.title.trim().is_empty() {
        member.title.clone()
    } else if !member.agent_name.trim().is_empty() {
        member.agent_name.clone()
    } else {
        format!("未命名会话 · {}", shortened(&member.id, 8))
    }
}

fn cost_grid(ui: &mut egui::Ui, s: &Session, c: &CostSummary) {
    let fresh = c.input_tokens.saturating_sub(
        c.cached_input_tokens
            .saturating_add(c.cache_write_input_tokens),
    );
    egui::Grid::new(("cost_grid", &s.id))
        .num_columns(3)
        .spacing([22.0, 6.0])
        .show(ui, |ui| {
            for text in ["类型", "tokens", "API 估算"] {
                ui.strong(text);
            }
            ui.end_row();
            for (title, tokens, dollars) in [
                ("输入 · 未命中缓存", fresh, c.uncached_usd),
                ("输入 · 缓存读取", c.cached_input_tokens, c.cached_usd),
                ("输入 · 缓存写入", c.cache_write_input_tokens, c.write_usd),
                ("输出", c.output_tokens, c.output_usd),
            ] {
                ui.label(title);
                ui.monospace(number(tokens));
                ui.monospace(if c.priced_requests == 0 {
                    "—".into()
                } else {
                    money(dollars)
                });
                ui.end_row();
            }
        });
}

fn observed_duration(start: Option<i64>, end: Option<i64>, archived: bool) -> String {
    if archived && end.is_none() {
        return "未知".into();
    }
    start
        .map(|start| compact_duration(end.unwrap_or_else(now_ms).saturating_sub(start).max(0)))
        .unwrap_or_else(|| "—".into())
}

/// Compression rows summarize children without adding their token costs a second time.
fn compaction_details(ui: &mut egui::Ui, c: &Compaction, scope: TimelineScope<'_>) {
    meta_line(ui, "压缩 ID", &c.id);
    meta_line(ui, "触发方式", &c.trigger);
    ui.small(format!(
        "开始 {}    结束 {}    总耗时 {}",
        clock(c.started_ms),
        clock(c.ended_ms),
        observed_duration(c.started_ms, c.ended_ms, scope.archived)
    ));
    let requests: Vec<_> = scope
        .requests
        .iter()
        .filter(|r| r.compaction_id.as_deref() == Some(c.id.as_str()))
        .collect();
    let tools: Vec<_> = scope
        .tools
        .iter()
        .filter(|t| t.compaction_id.as_deref() == Some(c.id.as_str()))
        .collect();
    ui.label(format!(
        "已观测子模型请求 {} 次 · 子工具调用 {} 次",
        requests.len(),
        tools.len()
    ));
    egui::Grid::new(("compression_totals", &c.id))
        .num_columns(2)
        .show(ui, |ui| {
            for (label, count) in [
                (
                    "输入 tokens",
                    request_token_total(&requests, |r| r.input_tokens),
                ),
                (
                    "缓存读取 tokens",
                    request_token_total(&requests, |r| r.cached_input_tokens),
                ),
                (
                    "缓存写入 tokens",
                    request_token_total(&requests, |r| r.cache_write_input_tokens),
                ),
                (
                    "输出 tokens",
                    request_token_total(&requests, |r| r.output_tokens),
                ),
            ] {
                ui.label(label);
                ui.monospace(count);
                ui.end_row();
            }
            ui.label("子请求 API 估算");
            ui.monospace(request_cost_total(&requests));
            ui.end_row();
        });
    ui.small("以下子请求费用已计入会话总额；压缩汇总不会再次计费。");
    for r in requests {
        egui::CollapsingHeader::new(format!(
            "LLM · {} · {}",
            r.model,
            tier_name(&r.service_tier)
        ))
        .id_salt(("compression_child_request", &c.id, &r.id))
        .show(ui, |ui| request_details(ui, r));
    }
    for t in tools {
        egui::CollapsingHeader::new(format!("TOOL · {}", shortened(&t.name, 60)))
            .id_salt(("compression_child_tool", &c.id, &t.id))
            .show(ui, |ui| tool_details(ui, t));
    }
}

fn request_token_total(
    requests: &[&ModelRequest],
    field: fn(&ModelRequest) -> Option<u64>,
) -> String {
    let values: Vec<_> = requests.iter().filter_map(|r| field(r)).collect();
    if values.is_empty() {
        return "未知".into();
    }
    let total = values.iter().fold(0_u64, |sum, n| sum.saturating_add(*n));
    if values.len() == requests.len() {
        number(total)
    } else {
        format!(
            "{}（已知部分，{} 项缺失）",
            number(total),
            requests.len() - values.len()
        )
    }
}

fn request_cost_total(requests: &[&ModelRequest]) -> String {
    let values: Vec<_> = requests.iter().filter_map(|r| r.cost_usd).collect();
    if values.is_empty() {
        return "未知".into();
    }
    let total: f64 = values.iter().sum();
    if values.len() == requests.len() {
        money(total)
    } else {
        format!(
            "部分估算 {} · {} 次未定价",
            money(total),
            requests.len() - values.len()
        )
    }
}

fn badge(ui: &mut egui::Ui, title: &str, fill: Color32, ink: Color32) {
    egui::Frame::new()
        .fill(fill)
        .corner_radius(2)
        .inner_margin(egui::Margin::symmetric(8, 4))
        .show(ui, |ui| {
            ui.label(
                RichText::new(if title.is_empty() { "—" } else { title })
                    .size(10.5)
                    .color(ink),
            );
        });
}

fn dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(7.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 3.0, color);
}

fn meta_line(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(label).size(10.0).color(theme::MUTED));
        ui.label(
            RichText::new(if value.is_empty() { "—" } else { value })
                .size(10.0)
                .monospace(),
        );
    });
}

// An immutable buffer permits selection and copying while leaving captured commands untouched.
fn code_details(ui: &mut egui::Ui, value: &str) {
    let mut text = value;
    egui::Frame::new()
        .fill(theme::BG)
        .corner_radius(2)
        .inner_margin(10)
        .show(ui, |ui| {
            egui::ScrollArea::both().max_height(190.0).show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut text)
                        .code_editor()
                        .font(egui::TextStyle::Monospace)
                        .desired_width(f32::INFINITY)
                        .frame(false),
                );
            });
        });
}

fn clock(milliseconds: Option<i64>) -> String {
    milliseconds
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%H:%M:%S%.3f")
                .to_string()
        })
        .unwrap_or_else(|| "未知".into())
}

fn clock_short(milliseconds: Option<i64>) -> String {
    milliseconds
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|t| {
            t.with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "—".into())
}

fn compact_duration(milliseconds: i64) -> String {
    let seconds = milliseconds as f64 / 1000.0;
    if seconds < 60.0 {
        format!("{seconds:.1}s")
    } else {
        format!(
            "{}m {:02}s",
            milliseconds / 60_000,
            milliseconds / 1000 % 60
        )
    }
}

fn number(n: u64) -> String {
    let text = n.to_string();
    let mut result = String::new();
    for (i, c) in text.chars().enumerate() {
        if i > 0 && (text.len() - i).is_multiple_of(3) {
            result.push(',');
        }
        result.push(c);
    }
    result
}

fn shortened(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let mut text: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        text.push('…');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model;

    // An unpriced child keeps the parent's known amount partial; late usage clears it after restart.
    #[test]
    fn parent_cost_marks_unmatched_child_and_recovers_when_usage_arrives() {
        let mut parent = Session {
            id: "parent".into(),
            cost: CostSummary {
                output_usd: 1.0,
                priced_requests: 1,
                ..CostSummary::default()
            },
            ..Session::default()
        };
        model::reset_cost_coverage(&mut parent);
        let mut child = Session {
            id: "child".into(),
            parent_id: "parent".into(),
            thread_kind: "subagent".into(),
            turn_id: "first".into(),
            ..Session::default()
        };
        model::reset_cost_coverage(&mut child);
        for (name, timestamp) in [("PreModelRequest", 1000), ("PostModelRequest", 2000)] {
            model::apply(
                &mut child,
                &serde_json::json!({"type":"monitor_hook","timestamp_ms":timestamp,
                "payload":{"thread_id":"child","turn_id":"first","model":"gpt-6.1-sol",
                    "hook_event_name":name,"status":"completed"}}),
            );
        }
        let costs = [
            CostMember::from_session(&parent),
            CostMember::from_session(&child),
        ];
        let totals = stats::rollup(&parent, &[], &costs);
        assert_eq!(totals.incomplete_members, 1);
        assert_eq!(group_cost(&totals), "~$1.00");
        model::apply(
            &mut child,
            &serde_json::json!({"type":"event_msg","timestamp_ms":3000,
            "payload":{"type":"task_started","turn_id":"next"}}),
        );
        let mut restored: Session =
            serde_json::from_str(&serde_json::to_string(&child).unwrap()).unwrap();
        model::apply(
            &mut restored,
            &serde_json::json!({"type":"token_usage_record","timestamp_ms":1500,
            "payload":{"thread_id":"child","turn_id":"first","response_id":"r1","model":"gpt-6.1-sol",
                "usage":{"input_tokens":1000,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":100}}}),
        );
        let costs = [
            CostMember::from_session(&parent),
            CostMember::from_session(&restored),
        ];
        let totals = stats::rollup(&parent, &[], &costs);
        assert_eq!(totals.incomplete_members, 0);
        assert!(!group_cost(&totals).starts_with('~'));
        assert_eq!(totals.cost.priced_requests, 2);
    }

    // Agent details resolve the exact child ID, show its existing TPS/cost, and keep missing data unknown.
    #[test]
    fn agent_overview_uses_child_identity_and_current_statistics() {
        let sessions = vec![
            Session {
                id: "child".into(),
                agent_name: "reviewer".into(),
                request_tps: Some(12.5),
                output_tokens: 321,
                cost: CostSummary {
                    output_usd: 2.0,
                    priced_requests: 1,
                    ..CostSummary::default()
                },
                ..Session::default()
            },
            Session {
                id: "other-child".into(),
                agent_name: "reviewer".into(),
                request_tps: Some(99.9),
                cost: CostSummary {
                    output_usd: 99.0,
                    priced_requests: 1,
                    ..CostSummary::default()
                },
                ..Session::default()
            },
        ];
        for (child_id, expected, absent) in [
            (
                "child",
                vec!["12.5", "321", "$2.00"],
                vec!["99.9", "$99.00"],
            ),
            (
                "missing",
                vec!["子会话当前轮次尚未加载"],
                vec!["$0.00", "$2.00"],
            ),
        ] {
            let context = egui::Context::default();
            let event = AgentEvent {
                thread_id: child_id.into(),
                ..AgentEvent::default()
            };
            let output = context.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default()
                    .show(ctx, |ui| agent_stats(ui, &event, &sessions, &[]));
            });
            let text = output
                .shapes
                .iter()
                .filter_map(|shape| match &shape.shape {
                    egui::Shape::Text(text) => Some(text.galley.job.text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            for value in expected {
                assert!(text.contains(value), "missing {value}: {text}");
            }
            for value in absent {
                assert!(!text.contains(value), "unexpected {value}: {text}");
            }
        }
    }

    // Search full commands regardless of case, combine with type, and restore history on clear.
    #[test]
    fn timeline_search_matches_operations_and_respects_type() {
        let session = Session {
            tools: vec![
                Tool {
                    id: "command".into(),
                    name: "exec".into(),
                    details: "cargo test --package Profiler".into(),
                    started_ms: 10,
                    ..Tool::default()
                },
                Tool {
                    id: "web".into(),
                    name: "web.search".into(),
                    started_ms: 30,
                    ..Tool::default()
                },
            ],
            requests: vec![ModelRequest {
                id: "model".into(),
                model: "gpt-6.1-sol".into(),
                service_tier: "fast".into(),
                started_ms: Some(20),
                ..ModelRequest::default()
            }],
            compactions: vec![Compaction {
                id: "compression".into(),
                started_ms: Some(40),
                ..Compaction::default()
            }],
            agent_events: vec![AgentEvent {
                id: "agent-notice".into(),
                kind: "completed".into(),
                thread_id: "child-123".into(),
                path: "/root/schema_tests".into(),
                at_ms: 50,
            }],
            ..Session::default()
        };
        for (filter, query, expected) in [
            (Filter::All, " CARGO TEST ", vec!["command"]),
            (Filter::Tools, "PROFILER", vec!["command"]),
            (Filter::All, "web", vec!["web"]),
            (Filter::Models, "GPT-6.1", vec!["model"]),
            (Filter::All, "FAST", vec!["model"]),
            (Filter::Tools, "gpt-6.1", vec![]),
            (Filter::All, "压缩", vec!["compression"]),
            (Filter::All, "missing", vec![]),
            (Filter::Tools, "schema_tests", vec!["agent-notice"]),
            (Filter::All, "CHILD-123", vec!["agent-notice"]),
            (Filter::Models, "schema_tests", vec![]),
            (
                Filter::All,
                "  ",
                vec!["agent-notice", "compression", "web", "model", "command"],
            ),
        ] {
            let actual: Vec<_> = timeline_rows(&session, filter, query)
                .iter()
                .map(|entry| entry.row.key().1.to_owned())
                .collect();
            assert_eq!(actual, expected, "query: {query}");
        }
        let event_row = row_summary(TimelineRow::Agent(&session.agent_events[0]), None, false);
        assert_eq!(event_row.0, "AGENT");
        assert_eq!(event_row.3, "—");
    }

    // Switching turns retains searchable old commands and model snapshots in chronological order.
    #[test]
    fn timeline_search_includes_prior_turn_commands_and_models() {
        let session = Session {
            turn_id: "current".into(),
            requests: vec![ModelRequest {
                id: "new-request".into(),
                model: "gpt-6-luna".into(),
                started_ms: Some(40),
                ..ModelRequest::default()
            }],
            timeline_history: vec![TurnTimeline {
                turn_id: "previous".into(),
                tools: vec![Tool {
                    id: "old-tool".into(),
                    name: "exec".into(),
                    details: "cargo profile --old-turn".into(),
                    started_ms: 20,
                    ..Tool::default()
                }],
                requests: vec![ModelRequest {
                    id: "old-request".into(),
                    model: "gpt-6.1-sol".into(),
                    started_ms: Some(10),
                    ..ModelRequest::default()
                }],
                ..TurnTimeline::default()
            }],
            ..Session::default()
        };
        let all: Vec<_> = timeline_rows(&session, Filter::All, "")
            .iter()
            .map(|entry| entry.row.key().1.to_owned())
            .collect();
        assert_eq!(all, ["new-request", "old-tool", "old-request"]);
        for (filter, query, expected) in [
            (Filter::Tools, "PROFILE", "old-tool"),
            (Filter::Models, "gpt-6.1", "old-request"),
        ] {
            let rows = timeline_rows(&session, filter, query);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].row.key().1, expected);
            assert_eq!(rows[0].scope.turn_id, "previous");
            assert!(rows[0].scope.archived);
        }
    }

    // An unfinished archived operation never inherits a later turn's end or keeps counting upwards.
    #[test]
    fn historical_open_rows_keep_unknown_status_and_duration() {
        let session = Session {
            timeline_history: vec![TurnTimeline {
                turn_id: "old".into(),
                tools: vec![Tool {
                    started_ms: 10,
                    ..Tool::default()
                }],
                requests: vec![ModelRequest {
                    started_ms: Some(20),
                    ..ModelRequest::default()
                }],
                compactions: vec![Compaction {
                    started_ms: Some(30),
                    ..Compaction::default()
                }],
                ..TurnTimeline::default()
            }],
            ..Session::default()
        };
        let rows = timeline_rows(&session, Filter::All, "");
        assert_eq!(rows.len(), 3);
        for entry in rows {
            for later_end in [Some(1000), Some(999_999), None] {
                let summary = row_summary(entry.row, later_end, entry.scope.archived);
                assert_eq!(summary.2, "结束未观测");
                assert_eq!(summary.3, "未知");
            }
        }
    }

    // Old compactions read only their own turn's children, even when another turn reuses the same ID.
    #[test]
    fn historical_compaction_details_keep_original_children_and_unknown_end() {
        let session = Session {
            requests: vec![ModelRequest {
                id: "current-request".into(),
                model: "current-model".into(),
                compaction_id: Some("compact".into()),
                output_tokens: Some(9999),
                cost_usd: Some(99.0),
                ..ModelRequest::default()
            }],
            timeline_history: vec![TurnTimeline {
                turn_id: "old".into(),
                compactions: vec![Compaction {
                    id: "compact".into(),
                    started_ms: Some(10),
                    ..Compaction::default()
                }],
                tools: vec![Tool {
                    name: "original-tool".into(),
                    compaction_id: Some("compact".into()),
                    ..Tool::default()
                }],
                requests: vec![ModelRequest {
                    id: "old-request".into(),
                    model: "original-model".into(),
                    compaction_id: Some("compact".into()),
                    output_tokens: Some(10),
                    cost_usd: Some(0.03),
                    ..ModelRequest::default()
                }],
                ..TurnTimeline::default()
            }],
            ..Session::default()
        };
        let context = egui::Context::default();
        let output = context.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let scope = TimelineScope::history(&session.timeline_history[0]);
                compaction_details(ui, &scope.compactions[0], scope);
            });
        });
        let text = output
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some(text.galley.job.text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        for expected in [
            "总耗时 未知",
            "已观测子模型请求 1 次 · 子工具调用 1 次",
            "$0.03",
            "original-model",
            "original-tool",
        ] {
            assert!(text.contains(expected), "missing {expected}: {text}");
        }
        for absent in ["current-model", "$99.00", "9,999"] {
            assert!(!text.contains(absent), "unexpected {absent}: {text}");
        }
    }

    // Navigation must leave space for overview and resources at normal and compact heights.
    #[test]
    fn page_navigation_leaves_content_near_top() {
        for height in [480.0, 900.0] {
            let context = egui::Context::default();
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, height))),
                ..Default::default()
            };
            let _ = context.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    page_tabs(ui, &mut Page::Sessions);
                    let content = ui.label("overview").rect;
                    assert!(content.top() < 60.0, "content displaced: {content:?}");
                    assert!(ui.available_height() > height - 100.0);
                });
            });
        }
    }

    // Narrow and wide layouts must align the heading with the frame's actual row content.
    #[test]
    fn timeline_columns_match_framed_rows_at_two_widths() {
        for width in [420.0, 920.0] {
            let context = egui::Context::default();
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(width, 300.0))),
                ..Default::default()
            };
            let _ = context.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let heading = timeline_heading(ui);
                    timeline_frame().show(ui, |ui| {
                        let (row, _) = ui.allocate_exact_size(
                            Vec2::new(ui.available_width(), 42.0),
                            Sense::hover(),
                        );
                        let actual = Columns::new(row);
                        for (header, cell) in heading.cells.into_iter().zip(actual.cells) {
                            assert!((header.left() - cell.left()).abs() < 0.01);
                            assert!((header.right() - cell.right()).abs() < 0.01);
                            assert!(cell.width() > 0.0);
                        }
                        let cells = heading.at_row(row);
                        assert_eq!(cells[1].center().x, actual.cells[1].center().x);
                        for pair in cells.windows(2) {
                            assert!(pair[0].right() <= pair[1].left());
                        }
                    });
                });
            });
        }
    }

    fn session(id: &str, time: i64, active: bool) -> Session {
        Session {
            id: id.into(),
            last_ms: time,
            started_ms: Some(1),
            ended_ms: if active { None } else { Some(time) },
            ..Session::default()
        }
    }

    fn child(id: &str, parent: &str, time: i64, active: bool, kind: &str) -> Session {
        Session {
            parent_id: parent.into(),
            thread_kind: kind.into(),
            ..session(id, time, active)
        }
    }

    // Active descendants promote parents, while updates preserve root and child ordering.
    #[test]
    fn child_activity_preserves_root_and_sibling_order() {
        let mut sessions = vec![
            session("a", 1, false),
            session("b", 2, false),
            child("a-child-1", "a", 100, true, "subagent"),
            child("a-child-2", "a", 90, true, "side_chat"),
            child("b-child", "b", 80, true, "subagent"),
        ];
        let mut order = SessionOrder::default();
        let ids = order.arrange(&sessions, 100);
        let tree = SessionTree::new(&sessions, &ids, false);
        assert_eq!(tree.roots, ["a", "b"]);
        assert_eq!(tree.children["a"], ["a-child-1", "a-child-2"]);
        sessions[3].last_ms = 200;
        sessions[4].last_ms = 300;
        let ids = order.arrange(&sessions, 300);
        let tree = SessionTree::new(&sessions, &ids, false);
        assert_eq!(tree.roots, ["a", "b"]);
        assert_eq!(tree.children["a"], ["a-child-1", "a-child-2"]);
        assert!(sessions[0].ended_ms.is_some());
    }

    // Both child types are nested and labeled; an ordinary fork remains a root chat.
    #[test]
    fn tree_distinguishes_children_forks_and_missing_parents() {
        let sessions = vec![
            session("parent", 1, false),
            child("agent", "parent", 2, true, "subagent"),
            child("side", "parent", 3, true, "side_chat"),
            child("fork", "parent", 4, false, "conversation"),
            child("orphan", "missing", 5, true, "subagent"),
        ];
        let ids: Vec<_> = sessions.iter().map(|s| s.id.clone()).collect();
        let tree = SessionTree::new(&sessions, &ids, false);
        assert_eq!(tree.roots, ["parent", "fork"]);
        assert_eq!(tree.children["parent"], ["agent", "side"]);
        assert_eq!(tree.orphans, ["orphan"]);
        assert_eq!(child_label(&sessions[1]), Some("子智能体"));
        assert_eq!(child_label(&sessions[2]), Some("侧边聊天"));
    }

    // Default-collapsed groups hide children without replacing parent or child selection.
    #[test]
    fn collapsed_tree_preserves_selection() {
        let sessions = vec![
            session("parent", 1, false),
            child("child", "parent", 2, true, "subagent"),
        ];
        let ids: Vec<_> = sessions.iter().map(|s| s.id.clone()).collect();
        let tree = SessionTree::new(&sessions, &ids, false);
        for selected_id in ["parent", "child"] {
            let context = egui::Context::default();
            let mut selected = Some(selected_id.to_owned());
            let _ = context.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let mut shown = HashSet::new();
                    tree.show_branch(ui, "parent", &mut selected, &mut shown, 0);
                    assert!(shown.contains("parent"));
                    assert!(!shown.contains("child"));
                });
            });
            assert_eq!(selected.as_deref(), Some(selected_id));
        }
    }

    // Concurrent activity never lets chats repeatedly leapfrog each other.
    #[test]
    fn active_order_survives_activity_updates() {
        let mut order = SessionOrder::default();
        assert_eq!(
            order.arrange(&[session("a", 100, true), session("b", 90, true)], 100),
            ["a", "b"]
        );
        assert_eq!(
            order.arrange(&[session("b", 110, true), session("a", 100, true)], 110),
            ["a", "b"]
        );
    }

    // Reactivation enters the top group; completed sessions then follow last activity.
    #[test]
    fn reactivated_chat_joins_stable_active_group() {
        let mut order = SessionOrder::default();
        order.arrange(&[session("a", 100, true), session("b", 90, false)], 100);
        assert_eq!(
            order.arrange(&[session("b", 120, true), session("a", 100, true)], 120),
            ["a", "b"]
        );
        assert_eq!(
            order.arrange(&[session("a", 100, false), session("b", 120, false)], 120),
            ["b", "a"]
        );
    }

    // Equal timestamps have deterministic order, independent of HashMap/source iteration.
    #[test]
    fn tied_times_use_chat_identity() {
        let mut order = SessionOrder::default();
        assert_eq!(
            order.arrange(&[session("b", 100, false), session("a", 100, false)], 100),
            ["a", "b"]
        );
    }
    // A long tool may legitimately run without new output for several minutes.
    #[test]
    fn long_silent_task_stays_in_active_group() {
        let mut order = SessionOrder::default();
        assert_eq!(
            order.arrange(
                &[session("a", 100, true), session("b", 500_000, false)],
                500_000
            ),
            ["a", "b"]
        );
    }
    // Missing compression child usage must not be displayed as a measured zero total.
    #[test]
    fn compression_totals_distinguish_unknown_and_zero() {
        let unknown = ModelRequest::default();
        assert_eq!(request_token_total(&[&unknown], |r| r.input_tokens), "未知");
        assert_eq!(request_cost_total(&[&unknown]), "未知");
        let zero = ModelRequest {
            input_tokens: Some(0),
            cost_usd: Some(0.0),
            ..ModelRequest::default()
        };
        assert_eq!(request_token_total(&[&zero], |r| r.input_tokens), "0");
        assert!(request_cost_total(&[&zero, &unknown]).contains("部分估算"));
    }
}
