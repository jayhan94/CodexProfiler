use crate::collector;
use crate::model::{now_ms, Session, Snapshot};
use eframe::egui;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::Duration;

pub struct Monitor {
    receiver: Receiver<Snapshot>,
    snapshot: Snapshot,
    selected: Option<String>,
    source: PathBuf,
}

impl Monitor {
    /// One collector serves every displayed session; selection never starts another worker.
    pub fn new(context: &eframe::CreationContext<'_>, source: PathBuf, db: PathBuf) -> Self {
        install_font(&context.egui_ctx);
        Self {
            receiver: collector::start(source.clone(), db, context.egui_ctx.clone()),
            snapshot: Snapshot {
                sessions: Vec::new(),
                error: None,
                updated_ms: 0,
            },
            selected: None,
            source,
        }
    }

    /// Drain coalesced snapshots before drawing, retaining only the newest one.
    fn receive(&mut self) {
        while let Ok(snapshot) = self.receiver.try_recv() {
            self.snapshot = snapshot;
        }
        let selection_exists = self
            .snapshot
            .sessions
            .iter()
            .any(|session| Some(&session.id) == self.selected.as_ref());
        if !selection_exists {
            self.selected = self
                .snapshot
                .sessions
                .first()
                .map(|session| session.id.clone());
        }
    }

    fn sidebar(&mut self, context: &egui::Context) {
        egui::SidePanel::left("sessions")
            .default_width(270.0)
            .resizable(true)
            .show(context, |ui| {
                ui.heading("Codex 会话");
                ui.small(format!("{} 个最近会话", self.snapshot.sessions.len()));
                ui.separator();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for session in &self.snapshot.sessions {
                        let selected = self.selected.as_ref() == Some(&session.id);
                        let title = if session.title.is_empty() {
                            &session.id
                        } else {
                            &session.title
                        };
                        let label = format!("{}\n{}", shortened(title, 28), session.phase);
                        if ui
                            .selectable_label(selected, label)
                            .on_hover_text(title)
                            .clicked()
                        {
                            self.selected = Some(session.id.clone());
                        }
                        ui.add_space(5.0);
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
                ui.label("本地日志 + 可选 hook");
            });
            ui.small(self.source.display().to_string());
            if let Some(error) = &self.snapshot.error {
                ui.colored_label(egui::Color32::LIGHT_RED, error);
            }
        });
        self.sidebar(context);
        egui::TopBottomPanel::bottom("footnote").show(context, |ui| {
            let age = now_ms().saturating_sub(self.snapshot.updated_ms).max(0) / 1000;
            ui.small(format!(
                "采集更新于 {age} 秒前；日志缺少事件时，状态与计时可能滞后。"
            ));
            ui.small("工具结果就绪不代表服务端已收到。精确模型请求速率缺少时间边界时显示未知。");
        });
        egui::CentralPanel::default().show(context, |ui| {
            if let Some(session) = self
                .snapshot
                .sessions
                .iter()
                .find(|session| Some(&session.id) == self.selected.as_ref())
            {
                render_session(ui, session);
                let minimized = context.input(|input| input.viewport().minimized == Some(true));
                if session.started_ms.is_some() && session.ended_ms.is_none() && !minimized {
                    context.request_repaint_after(Duration::from_secs(1));
                }
            } else {
                ui.heading("等待 Codex 会话");
                ui.label("开始或继续一个 Codex 聊天后，新增的日志会自动显示。");
            }
        });
    }
}

/// Task wall time includes tools and waits; it is deliberately distinct from model TPS.
fn render_session(ui: &mut egui::Ui, session: &Session) {
    let title = if session.title.is_empty() {
        "会话详情"
    } else {
        &session.title
    };
    ui.add(egui::Label::new(egui::RichText::new(title).heading()).wrap());
    ui.monospace(&session.id);
    ui.separator();
    render_summary(ui, session);
    ui.add_space(16.0);
    render_tools(ui, session);
}

fn render_summary(ui: &mut egui::Ui, session: &Session) {
    egui::Grid::new("summary")
        .num_columns(2)
        .spacing([24.0, 8.0])
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
            let elapsed_text = elapsed.map(duration).unwrap_or_else(|| "未知".into());
            summary_row(ui, "本轮任务总耗时", &elapsed_text);
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
            let model_tps = session
                .request_tps
                .map(|rate| format!("{rate:.1} tok/s"))
                .unwrap_or_else(|| "未知：缺少请求起止边界".into());
            summary_row(ui, "最近模型请求速率", &model_tps);
        });
}

fn render_tools(ui: &mut egui::Ui, session: &Session) {
    ui.heading("工具调用");
    ui.small("显示最近 100 项；执行中耗时为本机观测值。");
    ui.separator();
    egui::ScrollArea::vertical()
        .id_salt("tool_scroll")
        .show(ui, |ui| {
            egui::Grid::new("tools")
                .striped(true)
                .spacing([18.0, 8.0])
                .show(ui, |ui| {
                    for title in ["工具", "状态", "耗时"] {
                        ui.strong(title);
                    }
                    ui.end_row();
                    for tool in session.tools.iter().rev().take(100) {
                        ui.label(shortened(&tool.name, 48))
                            .on_hover_text(format!("{}\n{}", tool.name, tool.id));
                        let status = if tool.ended_ms.is_none() && session.ended_ms.is_some() {
                            "结束未知（任务已结束）"
                        } else {
                            &tool.status
                        };
                        ui.label(status);
                        let end = tool.ended_ms.or(session.ended_ms).unwrap_or_else(now_ms);
                        ui.monospace(duration(end.saturating_sub(tool.started_ms).max(0)));
                        ui.end_row();
                    }
                });
        });
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

/// CJK labels use a single system font when available, without a network download.
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
