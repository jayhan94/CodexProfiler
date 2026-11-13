use crate::collector;
use crate::model::{now_ms, Compaction, ModelRequest, Session, Snapshot, Tool};
use crate::{pricing, theme};
use eframe::egui::{self, Align2, Color32, FontId, Pos2, Rect, RichText, Sense, Stroke, Vec2};
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

// Activity updates never reorder members of the active group; idle chats use recency.
impl SessionOrder {
    fn arrange(&mut self, sessions: &[Session], now: i64) -> Vec<String> {
        self.active
            .retain(|id| sessions.iter().any(|s| &s.id == id && is_active(s, now)));
        let mut newly_active: Vec<_> = sessions
            .iter()
            .filter(|s| is_active(s, now) && !self.active.contains(&s.id))
            .collect();
        newly_active.sort_by(|a, b| b.last_ms.cmp(&a.last_ms).then(a.id.cmp(&b.id)));
        self.active
            .extend(newly_active.into_iter().map(|s| s.id.clone()));
        let mut idle: Vec<_> = sessions.iter().filter(|s| !is_active(s, now)).collect();
        idle.sort_by(|a, b| b.last_ms.cmp(&a.last_ms).then(a.id.cmp(&b.id)));
        self.active
            .iter()
            .cloned()
            .chain(idle.into_iter().map(|s| s.id.clone()))
            .collect()
    }
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
        }
    }

    /// Update data without moving an existing selection or racing active sessions.
    fn receive(&mut self) {
        while let Ok(snapshot) = self.receiver.try_recv() {
            self.snapshot = snapshot;
        }
        self.ordered_ids = self.order.arrange(&self.snapshot.sessions, now_ms());
        let visible = |s: &&Session| self.show_internal || !s.is_internal;
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
                    self.snapshot
                        .sessions
                        .iter()
                        .any(|s| &s.id == *id && (self.show_internal || !s.is_internal))
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
                        .filter(|s| self.show_internal || !s.is_internal)
                        .count();
                    badge(ui, &count.to_string(), theme::SURFACE, theme::MUTED);
                });
                ui.add_space(8.0);
                ui.checkbox(&mut self.show_internal, "显示内部任务");
                ui.add_space(12.0);
                egui::ScrollArea::vertical()
                    .id_salt("sessions_scroll")
                    .show(ui, |ui| {
                        for id in &self.ordered_ids {
                            let Some(s) = self.snapshot.sessions.iter().find(|s| &s.id == id)
                            else {
                                continue;
                            };
                            if !self.show_internal && s.is_internal {
                                continue;
                            }
                            if session_card(ui, s, self.selected.as_ref() == Some(id)).clicked() {
                                self.selected = Some(id.clone());
                            }
                            ui.add_space(8.0);
                        }
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
                        .show(ui, |ui| render_session(ui, s, &mut self.filter));
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
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 82.0), Sense::click());
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
    let title = display_title(s);
    let mut job = egui::text::LayoutJob::simple(
        title.clone(),
        FontId::proportional(12.0),
        theme::INK,
        rect.width() - 25.0,
    );
    job.wrap.max_rows = 2;
    let galley = ui.fonts(|fonts| fonts.layout_job(job));
    ui.painter()
        .galley(rect.min + Vec2::new(13.0, 11.0), galley, theme::INK);
    let active = is_active(s, now_ms());
    let color = if active { theme::PINK } else { theme::MUTED };
    ui.painter()
        .circle_filled(rect.min + Vec2::new(16.0, 64.0), 3.0, color);
    ui.painter().text(
        rect.min + Vec2::new(26.0, 64.0),
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

fn render_session(ui: &mut egui::Ui, s: &Session, filter: &mut Filter) {
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
    metrics(ui, s);
    cost_details(ui, s);
    ui.add_space(20.0);
    render_timeline(ui, s, filter);
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
fn metrics(ui: &mut egui::Ui, s: &Session) {
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
            "tok/s · 最近一次有效请求".to_owned(),
            theme::PINK_SOFT,
        ),
        (
            "任务总耗时",
            elapsed.map(compact_duration).unwrap_or_else(|| "—".into()),
            "当前轮次 · 本机观测".to_owned(),
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
            session_cost(s),
            if s.cost.unpriced_requests > 0 {
                format!("部分估算 · {} 次未定价", s.cost.unpriced_requests)
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
    Model(&'a ModelRequest),
    Compaction(&'a Compaction),
}

impl TimelineRow<'_> {
    fn time(&self) -> i64 {
        match self {
            Self::Tool(t) => t.started_ms,
            Self::Model(r) => r.started_ms.or(r.ended_ms).unwrap_or(0),
            Self::Compaction(c) => c.started_ms.or(c.ended_ms).unwrap_or(0),
        }
    }
    fn key(&self) -> (&str, &str) {
        match self {
            Self::Tool(t) => ("tool", &t.id),
            Self::Model(r) => ("model", &r.id),
            Self::Compaction(c) => ("compaction", &c.id),
        }
    }
}

struct Columns {
    time: f32,
    kind: f32,
    name: f32,
    status: f32,
    elapsed: f32,
    output: f32,
}

impl Columns {
    fn new(rect: Rect) -> Self {
        let x = rect.left();
        Self {
            time: x + 21.0,
            kind: x + 97.0,
            name: x + 153.0,
            status: rect.right() - 210.0,
            elapsed: rect.right() - 124.0,
            output: rect.right() - 65.0,
        }
    }
}

/// The timeline keeps column geometry identical for models and tools, regardless of title length.
fn render_timeline(ui: &mut egui::Ui, s: &Session, filter: &mut Filter) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("运行时间线").size(15.0).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            filter_buttons(ui, filter);
        });
    });
    ui.add_space(7.0);
    timeline_heading(ui);
    ui.add_space(5.0);
    let mut rows = Vec::new();
    if matches!(*filter, Filter::All | Filter::Tools) {
        rows.extend(s.tools.iter().map(TimelineRow::Tool));
    }
    if matches!(*filter, Filter::All | Filter::Models) {
        rows.extend(s.requests.iter().map(TimelineRow::Model));
    }
    if matches!(*filter, Filter::All | Filter::Compactions) {
        rows.extend(s.compactions.iter().map(TimelineRow::Compaction));
    }
    rows.sort_by(|a, b| b.time().cmp(&a.time()).then(a.key().cmp(&b.key())));
    if rows.is_empty() {
        ui.add_space(30.0);
        ui.label(RichText::new("当前筛选暂无记录").color(theme::MUTED));
    }
    for row in rows.into_iter().take(300) {
        ui.push_id((&s.id, row.key()), |ui| timeline_card(ui, row, s));
        ui.add_space(5.0);
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

fn timeline_heading(ui: &mut egui::Ui) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 24.0), Sense::hover());
    let cols = Columns::new(rect);
    for (x, title) in [
        (cols.time, "时间"),
        (cols.kind, "类型"),
        (cols.name, "操作"),
        (cols.status, "状态"),
        (cols.elapsed, "耗时"),
        (cols.output, "输出"),
    ] {
        ui.painter().text(
            Pos2::new(x, rect.center().y),
            Align2::LEFT_CENTER,
            title,
            FontId::proportional(10.0),
            theme::MUTED,
        );
    }
}

fn timeline_card(ui: &mut egui::Ui, row: TimelineRow<'_>, session: &Session) {
    egui::Frame::new()
        .fill(theme::SURFACE)
        .stroke(Stroke::new(1.0_f32, theme::BORDER))
        .corner_radius(2)
        .inner_margin(4)
        .show(ui, |ui| {
            let id = ui.make_persistent_id(row.key());
            let mut open = egui::collapsing_header::CollapsingState::load_with_default_open(
                ui.ctx(),
                id,
                false,
            );
            let (rect, response) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 42.0), Sense::click());
            paint_timeline_row(ui, rect, row, session.ended_ms, open.is_open());
            if response.clicked() {
                open.toggle(ui);
            }
            response.on_hover_text("点击展开详情");
            open.show_body_unindented(ui, |ui| {
                ui.add_space(6.0);
                match row {
                    TimelineRow::Tool(t) => tool_details(ui, t),
                    TimelineRow::Model(r) => request_details(ui, r),
                    TimelineRow::Compaction(c) => compaction_details(ui, c, session),
                }
            });
        });
}

fn paint_timeline_row(
    ui: &egui::Ui,
    rect: Rect,
    row: TimelineRow<'_>,
    task_end: Option<i64>,
    open: bool,
) {
    let c = Columns::new(rect);
    let painter = ui.painter();
    let center = rect.center().y;
    let arrow = if open { "−" } else { "+" };
    painter.text(
        Pos2::new(rect.left() + 7.0, center),
        Align2::LEFT_CENTER,
        arrow,
        FontId::monospace(12.0),
        theme::MUTED,
    );
    let (kind, title, status, elapsed, output, fill) = row_summary(row, task_end);
    painter.text(
        Pos2::new(c.time, center),
        Align2::LEFT_CENTER,
        clock_short(Some(row.time())),
        FontId::monospace(10.0),
        theme::MUTED,
    );
    let badge_rect =
        Rect::from_center_size(Pos2::new(c.kind + 20.0, center), Vec2::new(40.0, 21.0));
    painter.rect_filled(badge_rect, 2, fill);
    painter.text(
        badge_rect.center(),
        Align2::CENTER_CENTER,
        kind,
        FontId::monospace(10.0),
        theme::INK,
    );
    let name_rect = Rect::from_min_max(
        Pos2::new(c.name, rect.top()),
        Pos2::new(c.status - 10.0, rect.bottom()),
    );
    painter.with_clip_rect(name_rect).text(
        Pos2::new(c.name, center),
        Align2::LEFT_CENTER,
        title,
        FontId::proportional(11.5),
        theme::INK,
    );
    let status_rect = Rect::from_min_max(
        Pos2::new(c.status, rect.top()),
        Pos2::new(c.elapsed - 6.0, rect.bottom()),
    );
    painter.with_clip_rect(status_rect).text(
        Pos2::new(c.status, center),
        Align2::LEFT_CENTER,
        status,
        FontId::proportional(10.5),
        theme::MUTED,
    );
    painter.text(
        Pos2::new(c.elapsed, center),
        Align2::LEFT_CENTER,
        elapsed,
        FontId::monospace(10.5),
        theme::INK,
    );
    painter.text(
        Pos2::new(c.output, center),
        Align2::LEFT_CENTER,
        output,
        FontId::monospace(10.5),
        theme::INK,
    );
}

fn row_summary(
    row: TimelineRow<'_>,
    task_end: Option<i64>,
) -> (&str, String, String, String, String, Color32) {
    match row {
        TimelineRow::Tool(t) => {
            let end = t.ended_ms.or(task_end).unwrap_or_else(now_ms);
            (
                "TOOL",
                format!(
                    "{}{}",
                    t.name,
                    if t.compaction_id.is_some() {
                        " · 压缩"
                    } else {
                        ""
                    }
                ),
                t.status.clone(),
                compact_duration(end.saturating_sub(t.started_ms).max(0)),
                "—".into(),
                theme::YELLOW_SOFT,
            )
        }
        TimelineRow::Model(r) => {
            let elapsed = r
                .started_ms
                .map(|start| {
                    compact_duration(
                        r.ended_ms
                            .unwrap_or_else(now_ms)
                            .saturating_sub(start)
                            .max(0),
                    )
                })
                .unwrap_or_else(|| "—".into());
            (
                "LLM",
                format!(
                    "{} · {}{}",
                    r.model,
                    tier_name(&r.service_tier),
                    if r.compaction_id.is_some() {
                        " · 压缩"
                    } else {
                        ""
                    }
                ),
                r.status.clone(),
                elapsed,
                r.output_tokens.map(number).unwrap_or_else(|| "—".into()),
                theme::PINK_SOFT,
            )
        }
        TimelineRow::Compaction(c) => (
            "CMP",
            "上下文压缩".into(),
            c.status.clone(),
            observed_duration(c.started_ms, c.ended_ms),
            "—".into(),
            theme::YELLOW_SOFT,
        ),
    }
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

fn session_cost(s: &Session) -> String {
    if s.cost.priced_requests == 0 {
        "—".into()
    } else if s.cost.unpriced_requests > 0 {
        format!("~{}", money(s.cost.total()))
    } else {
        money(s.cost.total())
    }
}

/// Costs aggregate observed requests across the whole chat, never reset at turn boundaries.
fn cost_details(ui: &mut egui::Ui, s: &Session) {
    let title = RichText::new("成本明细 · API / USD")
        .size(10.0)
        .color(theme::MUTED);
    egui::CollapsingHeader::new(title).id_salt(("cost", &s.id)).show(ui, |ui| {
        cost_grid(ui, s);
        let cost = &s.cost;
        let hit = if cost.input_tokens == 0 {
            "未知".into()
        } else {
            format!("{:.1}%", cost.cached_input_tokens as f64 * 100.0 / cost.input_tokens as f64)
        };
        ui.small(format!("缓存命中率 {hit} · 已定价 {} 次 · 未定价 {} 次", cost.priced_requests, cost.unpriced_requests));
        if cost.unpriced_requests > 0 {
            ui.small("当前总额是部分估算，未定价请求未被计为 $0。");
        }
        ui.small(format!("本轮已观测：{} 次工具调用 · {} 次模型请求", s.tools.len(), s.requests.len()));
        let note = "当前线程累计，子 agent 在各自线程单独计费。API 等价估算，非订阅账单；30m 缓存写入。基础 Standard 价格按每次请求档位调整，含已知长上下文规则；不含工具费用及区域附加费。";
        ui.label(RichText::new(note).small().color(theme::MUTED));
        ui.horizontal_wrapped(|ui| {
            ui.small(format!("价格快照 {}", pricing::AS_OF));
            ui.hyperlink_to("官方价格来源", pricing::SOURCE);
        });
    });
}

fn cost_grid(ui: &mut egui::Ui, s: &Session) {
    let c = &s.cost;
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

fn observed_duration(start: Option<i64>, end: Option<i64>) -> String {
    start
        .map(|start| compact_duration(end.unwrap_or_else(now_ms).saturating_sub(start).max(0)))
        .unwrap_or_else(|| "—".into())
}

/// Compression rows summarize children without adding their token costs a second time.
fn compaction_details(ui: &mut egui::Ui, c: &Compaction, session: &Session) {
    meta_line(ui, "压缩 ID", &c.id);
    meta_line(ui, "触发方式", &c.trigger);
    ui.small(format!(
        "开始 {}    结束 {}    总耗时 {}",
        clock(c.started_ms),
        clock(c.ended_ms),
        observed_duration(c.started_ms, c.ended_ms)
    ));
    let requests: Vec<_> = session
        .requests
        .iter()
        .filter(|r| r.compaction_id.as_deref() == Some(c.id.as_str()))
        .collect();
    let tools: Vec<_> = session
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

    fn session(id: &str, time: i64, active: bool) -> Session {
        Session {
            id: id.into(),
            last_ms: time,
            started_ms: Some(1),
            ended_ms: if active { None } else { Some(time) },
            ..Session::default()
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
