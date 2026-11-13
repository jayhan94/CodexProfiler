use crate::collector;
use crate::model::{now_ms, ModelRequest, Session, Snapshot, Tool};
use eframe::egui;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::Duration;

#[derive(Clone, Copy, Default, PartialEq)]
enum Filter {
    #[default]
    All,
    Tools,
    Models,
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
    /// The same collector and ordering state serve every session selection.
    pub fn new(context: &eframe::CreationContext<'_>, source: PathBuf, db: PathBuf) -> Self {
        install_font(&context.egui_ctx);
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

    /// Retain only the newest snapshot, preserving selection by chat identity.
    fn receive(&mut self) {
        while let Ok(snapshot) = self.receiver.try_recv() {
            self.snapshot = snapshot;
        }
        self.ordered_ids = self.order.arrange(&self.snapshot.sessions, now_ms());
        let visible = |s: &&Session| self.show_internal || !s.is_internal;
        let selection_exists = self
            .snapshot
            .sessions
            .iter()
            .filter(visible)
            .any(|s| Some(&s.id) == self.selected.as_ref());
        if !selection_exists {
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

    fn sidebar(&mut self, context: &egui::Context) {
        egui::SidePanel::left("sessions")
            .default_width(280.0)
            .resizable(true)
            .show(context, |ui| {
                ui.heading("Codex 会话");
                ui.checkbox(&mut self.show_internal, "显示内部任务 / 子 agent");
                let visible = self
                    .snapshot
                    .sessions
                    .iter()
                    .filter(|s| self.show_internal || !s.is_internal)
                    .count();
                ui.small(format!("{visible} 个会话 · 活跃会话保持相对顺序"));
                ui.separator();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for id in &self.ordered_ids {
                        let Some(session) = self.snapshot.sessions.iter().find(|s| &s.id == id)
                        else {
                            continue;
                        };
                        if !self.show_internal && session.is_internal {
                            continue;
                        }
                        ui.push_id(&session.id, |ui| {
                            let title = display_title(session);
                            let label = format!("{}\n{}", shortened(&title, 30), session.phase);
                            let hover =
                                format!("{}\n会话：{}\n目录：{}", title, session.id, session.cwd);
                            if ui
                                .selectable_label(self.selected.as_ref() == Some(id), label)
                                .on_hover_text(hover)
                                .clicked()
                            {
                                self.selected = Some(id.clone());
                            }
                            ui.add_space(5.0);
                        });
                    }
                });
            });
    }
}

impl eframe::App for Monitor {
    fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.receive();
        egui::TopBottomPanel::top("heading").show(context, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Codex Monitor");
                ui.label("会话运行时间线");
            });
            ui.small(self.source.display().to_string());
            if let Some(error) = &self.snapshot.error {
                ui.colored_label(egui::Color32::LIGHT_RED, error);
            }
        });
        self.sidebar(context);
        egui::TopBottomPanel::bottom("footnote").show(context, |ui| {
            ui.small("工具结果就绪不代表服务端已收到；历史记录缺少请求边界时保留未知。");
        });
        egui::CentralPanel::default().show(context, |ui| {
            if let Some(session) = self
                .snapshot
                .sessions
                .iter()
                .find(|s| Some(&s.id) == self.selected.as_ref())
            {
                render_session(ui, session, &mut self.filter);
                let minimized = context.input(|i| i.viewport().minimized == Some(true));
                if is_active(session, now_ms()) && !minimized {
                    context.request_repaint_after(Duration::from_secs(1));
                }
            } else {
                ui.heading("等待 Codex 会话");
                ui.label("继续一个 Codex 聊天后，新增事件会自动显示。");
            }
        });
    }
}

fn display_title(session: &Session) -> String {
    if session.title.trim().is_empty() {
        format!("未命名会话 · {}", shortened(&session.id, 12))
    } else {
        session.title.clone()
    }
}

/// Task wall time includes tools and waits; request TPS uses its own boundaries.
fn render_session(ui: &mut egui::Ui, session: &Session, filter: &mut Filter) {
    ui.add(egui::Label::new(egui::RichText::new(display_title(session)).heading()).wrap());
    ui.monospace(&session.id);
    ui.separator();
    render_summary(ui, session);
    ui.add_space(12.0);
    render_timeline(ui, session, filter);
}

fn render_summary(ui: &mut egui::Ui, session: &Session) {
    egui::Grid::new("summary")
        .num_columns(2)
        .spacing([24.0, 6.0])
        .show(ui, |ui| {
            summary_row(ui, "观测状态", &session.phase);
            summary_row(ui, "模型", &session.model);
            summary_row(ui, "当前轮次", &session.turn_id);
            let elapsed = session.started_ms.map(|start| {
                session
                    .ended_ms
                    .unwrap_or_else(now_ms)
                    .saturating_sub(start)
                    .max(0)
            });
            summary_row(
                ui,
                "本轮任务总耗时",
                &elapsed.map(duration).unwrap_or_else(|| "未知".into()),
            );
            summary_row(ui, "本轮输出 tokens", &session.output_tokens.to_string());
            let throughput = elapsed
                .filter(|ms| *ms > 0)
                .map(|ms| {
                    format!(
                        "{:.1} tok/s（含工具与等待）",
                        session.output_tokens as f64 * 1000.0 / ms as f64
                    )
                })
                .unwrap_or_else(|| "未知".into());
            summary_row(ui, "任务输出速率", &throughput);
            let rate = session
                .request_tps
                .map(|r| format!("{r:.1} tok/s（请求有效吞吐量）"))
                .unwrap_or_else(|| "未知：等待可用的模型请求边界".into());
            summary_row(ui, "最近模型请求速率", &rate);
        });
}

#[derive(Clone, Copy)]
enum TimelineRow<'a> {
    Tool(&'a Tool),
    Model(&'a ModelRequest),
}

impl TimelineRow<'_> {
    fn time(&self) -> i64 {
        match self {
            Self::Tool(t) => t.started_ms,
            Self::Model(r) => r.started_ms.or(r.ended_ms).unwrap_or(0),
        }
    }
    fn key(&self) -> (&str, &str) {
        match self {
            Self::Tool(t) => ("tool", &t.id),
            Self::Model(r) => ("model", &r.id),
        }
    }
}

/// Source types share one timeline; filtering changes visibility, never recorded state.
fn render_timeline(ui: &mut egui::Ui, session: &Session, filter: &mut Filter) {
    ui.horizontal(|ui| {
        ui.heading("运行时间线");
        for (value, title) in [
            (Filter::All, "全部"),
            (Filter::Tools, "工具调用"),
            (Filter::Models, "模型调用"),
        ] {
            ui.selectable_value(filter, value, title);
        }
    });
    ui.small("按事件开始时间倒序；点击展开命令 / 参数 / 模型请求详情。最多显示最近 300 项。");
    ui.separator();
    let mut rows = Vec::new();
    if *filter != Filter::Models {
        rows.extend(session.tools.iter().map(TimelineRow::Tool));
    }
    if *filter != Filter::Tools {
        rows.extend(session.requests.iter().map(TimelineRow::Model));
    }
    rows.sort_by(|a, b| b.time().cmp(&a.time()).then(a.key().cmp(&b.key())));
    egui::ScrollArea::vertical()
        .id_salt(("timeline", &session.id))
        .show(ui, |ui| {
            if rows.is_empty() {
                ui.label("当前筛选条件下暂无记录。");
            }
            for row in rows.into_iter().take(300) {
                ui.push_id((&session.id, row.key()), |ui| match row {
                    TimelineRow::Tool(tool) => render_tool(ui, tool, session.ended_ms),
                    TimelineRow::Model(request) => render_request(ui, request),
                });
            }
        });
}

fn render_tool(ui: &mut egui::Ui, tool: &Tool, task_end: Option<i64>) {
    let end = tool.ended_ms.or(task_end).unwrap_or_else(now_ms);
    let status = if tool.ended_ms.is_none() && task_end.is_some() {
        "结束未知"
    } else {
        &tool.status
    };
    let heading = format!(
        "{}  工具  {}  · {} · {}",
        clock(Some(tool.started_ms)),
        shortened(&tool.name, 50),
        status,
        duration(end.saturating_sub(tool.started_ms).max(0))
    );
    egui::CollapsingHeader::new(heading)
        .id_salt(("tool", &tool.id))
        .show(ui, |ui| {
            summary_row_grid(ui, &tool.id, "调用 ID");
            ui.label(format!(
                "开始：{}    结束：{}",
                clock(Some(tool.started_ms)),
                clock(tool.ended_ms)
            ));
            if tool.details.is_empty() {
                ui.small("这条历史记录未保留命令明细。");
            } else {
                code_details(ui, &tool.details);
            }
        });
}

fn render_request(ui: &mut egui::Ui, request: &ModelRequest) {
    let elapsed = request
        .started_ms
        .zip(request.ended_ms)
        .map(|(start, end)| duration(end.saturating_sub(start).max(0)))
        .unwrap_or_else(|| {
            if let Some(start) = request.started_ms {
                duration(now_ms().saturating_sub(start).max(0))
            } else {
                "耗时未知".into()
            }
        });
    let tokens = request
        .output_tokens
        .map(|n| format!("{n} tokens"))
        .unwrap_or_else(|| "tokens 未知".into());
    let heading = format!(
        "{}  LLM  {}  · {} · {} · {}",
        clock(request.started_ms.or(request.ended_ms)),
        request.model,
        request.status,
        elapsed,
        tokens
    );
    egui::CollapsingHeader::new(heading)
        .id_salt(("model", &request.id))
        .show(ui, |ui| {
            summary_row_grid(ui, &request.id, "请求 ID");
            ui.label(format!(
                "开始：{}    结束：{}",
                clock(request.started_ms),
                clock(request.ended_ms)
            ));
            let rate = request
                .started_ms
                .zip(request.ended_ms)
                .zip(request.output_tokens)
                .filter(|((start, end), _)| end > start)
                .map(|((start, end), n)| {
                    format!("{:.1} tok/s", n as f64 * 1000.0 / (end - start) as f64)
                })
                .unwrap_or_else(|| "未知：历史记录没有完整请求边界".into());
            ui.label(format!("请求有效吞吐量：{rate}"));
            ui.small(format!("事件来源：{}", request.source));
        });
}

fn summary_row_grid(ui: &mut egui::Ui, value: &str, label: &str) {
    ui.horizontal_wrapped(|ui| {
        ui.label(label);
        ui.monospace(value);
    });
}

// An immutable text buffer supports selecting/copying without editing captured details.
fn code_details(ui: &mut egui::Ui, value: &str) {
    let mut text = value;
    egui::ScrollArea::horizontal()
        .max_height(240.0)
        .show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut text)
                    .code_editor()
                    .desired_width(f32::INFINITY),
            );
        });
}

fn clock(milliseconds: Option<i64>) -> String {
    milliseconds
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%H:%M:%S%.3f")
                .to_string()
        })
        .unwrap_or_else(|| "未知".into())
}

fn summary_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.label(label);
    ui.label(if value.is_empty() { "未知" } else { value });
    ui.end_row();
}

fn duration(milliseconds: i64) -> String {
    let seconds = milliseconds as f64 / 1000.0;
    if seconds < 60.0 {
        format!("{seconds:.1} 秒")
    } else {
        format!("{} 分 {:.1} 秒", milliseconds / 60_000, seconds % 60.0)
    }
}

fn shortened(value: &str, limit: usize) -> String {
    let mut chars = value.chars();
    let mut text: String = chars.by_ref().take(limit).collect();
    if chars.next().is_some() {
        text.push('…');
    }
    text
}

/// One local font supplies Chinese labels and command details without network access.
fn install_font(context: &egui::Context) {
    for path in ["C:/Windows/Fonts/simhei.ttf", "C:/Windows/Fonts/msyh.ttc"] {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts
                .font_data
                .insert("cjk".into(), egui::FontData::from_owned(bytes).into());
            fonts
                .families
                .entry(egui::FontFamily::Proportional)
                .or_default()
                .insert(0, "cjk".into());
            fonts
                .families
                .entry(egui::FontFamily::Monospace)
                .or_default()
                .push("cjk".into());
            context.set_fonts(fonts);
            break;
        }
    }
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
}
