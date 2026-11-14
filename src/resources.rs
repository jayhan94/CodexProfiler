use crate::theme;
use eframe::egui::{self, RichText};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use sysinfo::{
    CpuRefreshKind, MemoryRefreshKind, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind,
};

const MAX_TRACKED: usize = 1_000;
type Identity = (u32, u64);

#[derive(Clone, Default, Serialize)]
struct Snapshot {
    rows: Vec<ProcessRow>,
    cpu_percent: Option<f32>,
    memory_bytes: u64,
    memory_percent: Option<f64>,
    process_count: usize,
    cpu_process_count: usize,
    memory_process_count: usize,
    logical_cpus: usize,
    total_memory: u64,
    warnings: Vec<String>,
    sample_millis: u128,
}

#[derive(Clone, Serialize)]
struct ProcessRow {
    pid: u32,
    parent: Option<u32>,
    start_time: u64,
    name: String,
    path: Option<String>,
    cpu_percent: Option<f32>,
    memory_bytes: Option<u64>,
    memory_percent: Option<f64>,
    scope: &'static str,
    depth: usize,
}

struct ProcessInfo {
    pid: u32,
    parent: Option<u32>,
    start: u64,
    name: String,
    path: Option<String>,
}

impl ProcessInfo {
    fn identity(&self) -> Identity {
        (self.pid, self.start)
    }
}

#[derive(Default)]
struct Control {
    visible: bool,
    stop: bool,
}

#[derive(Default)]
struct Shared {
    control: Mutex<Control>,
    wake: Condvar,
    latest: Mutex<Option<Snapshot>>,
}

pub struct Monitor {
    shared: Arc<Shared>,
    snapshot: Option<Snapshot>,
    selected: Option<Identity>,
}

impl Monitor {
    /// Native sampling happens entirely on a worker; the GUI only reads its latest snapshot.
    pub fn new(ctx: egui::Context) -> Self {
        let shared = Arc::new(Shared::default());
        let worker_shared = Arc::clone(&shared);
        if let Err(error) = thread::Builder::new()
            .name("codex-resource-sampler".into())
            .spawn(move || sample_loop(worker_shared, ctx))
        {
            *shared.latest.lock().unwrap() =
                Some(error_snapshot(format!("无法启动采样线程：{error}")));
        }
        Self {
            shared,
            snapshot: None,
            selected: None,
        }
    }

    /// Hidden pages retain discovery continuity at a lower polling rate.
    pub fn set_visible(&mut self, visible: bool) {
        let mut control = self.shared.control.lock().unwrap();
        if control.visible != visible {
            control.visible = visible;
            self.shared.wake.notify_one();
        }
    }

    /// Keep the summary fixed and give only the process table a finite scroll area.
    pub fn show(&mut self, ui: &mut egui::Ui, _ctx: &egui::Context) {
        if let Some(snapshot) = self.shared.latest.lock().unwrap().take() {
            self.snapshot = Some(snapshot);
        }
        ui.heading("进程资源");
        ui.label(
            RichText::new("所有 Codex 会话共用的本机进程树，不按聊天分摊；不含 Profiler 自身。")
                .small()
                .color(theme::MUTED),
        );
        let Some(snapshot) = &self.snapshot else {
            ui.label("正在发现 Codex 进程并采样…");
            return;
        };
        summary_cards(ui, snapshot);
        ui.label(
            RichText::new("工作集合计（共享页可能重复计入）")
                .small()
                .color(theme::MUTED),
        );
        ui.label(RichText::new(format!(
            "CPU 按整机 {} 个逻辑处理器归一化 · 内存比例以物理内存为分母 · 当前页每秒采样，其他页每 5 秒采样 · 本次 {:.0} ms",
            snapshot.logical_cpus, snapshot.sample_millis
        )).small().color(theme::MUTED));
        ui.label(
            RichText::new("仅能捕获采样时仍存在的进程；两次采样之间启动并结束的短任务可能遗漏。")
                .small()
                .color(theme::MUTED),
        );
        for warning in &snapshot.warnings {
            ui.colored_label(ui.visuals().warn_fg_color, warning);
        }
        if snapshot.process_count == 0 {
            ui.label("未发现可识别的 Codex 进程。启动 Codex 后会自动刷新。");
            return;
        }
        process_details(ui, snapshot, self.selected);
        ui.separator();
        process_table(ui, snapshot, &mut self.selected);
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.shared.control.lock().unwrap().stop = true;
        self.shared.wake.notify_one();
    }
}

fn summary_cards(ui: &mut egui::Ui, snapshot: &Snapshot) {
    ui.columns(3, |columns| {
        let values = [
            ("CPU · 整机占比", percent32(snapshot.cpu_percent)),
            (
                "内存 · 工作集合计",
                if snapshot.process_count > 0 && snapshot.memory_process_count == 0 {
                    "未知".into()
                } else {
                    format!(
                        "{:.1} MiB · {}",
                        mib(snapshot.memory_bytes),
                        percent(snapshot.memory_percent)
                    )
                },
            ),
            ("Codex 进程", snapshot.process_count.to_string()),
        ];
        for (column, (title, value)) in columns.iter_mut().zip(values) {
            theme::card(theme::SURFACE).show(column, |ui| {
                ui.set_min_width((ui.available_width() - 2.0).max(0.0));
                ui.label(RichText::new(title).small().color(theme::MUTED));
                ui.label(RichText::new(value).size(21.0).strong());
            });
        }
    });
}

fn process_details(ui: &mut egui::Ui, snapshot: &Snapshot, selected: Option<Identity>) {
    if let Some(row) = snapshot
        .rows
        .iter()
        .find(|row| Some((row.pid, row.start_time)) == selected)
    {
        ui.label(
            RichText::new(format!(
                "{} · PID {} · 父 PID {} · {}",
                row.name,
                row.pid,
                row.parent
                    .map_or_else(|| "未知".into(), |pid| pid.to_string()),
                row.scope
            ))
            .small(),
        );
        ui.label(
            RichText::new(
                row.path
                    .as_deref()
                    .unwrap_or("路径不可读取（权限限制或进程已结束）"),
            )
            .small()
            .color(theme::MUTED),
        );
    } else {
        ui.label(
            RichText::new("点击进程查看路径和父进程；CPU“采样中”表示尚无第二次有效读数。")
                .small()
                .color(theme::MUTED),
        );
    }
}

fn process_table(ui: &mut egui::Ui, snapshot: &Snapshot, selected: &mut Option<Identity>) {
    egui::ScrollArea::both()
        .id_salt("resource-process-table")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Grid::new("resource-process-grid")
                .striped(true)
                .num_columns(5)
                .min_col_width(78.0)
                .show(ui, |ui| {
                    for title in ["进程", "PID", "CPU %", "内存", "内存 %"] {
                        ui.strong(title);
                    }
                    ui.end_row();
                    for row in &snapshot.rows {
                        let identity = (row.pid, row.start_time);
                        ui.horizontal(|ui| {
                            ui.add_space(row.depth.min(12) as f32 * 12.0);
                            if ui
                                .selectable_label(*selected == Some(identity), &row.name)
                                .clicked()
                            {
                                *selected = Some(identity);
                            }
                            if row.scope == "共享 Codex 沙箱" {
                                ui.label(RichText::new("共享沙箱").small().color(theme::PINK));
                            }
                        });
                        ui.monospace(row.pid.to_string());
                        ui.monospace(percent32(row.cpu_percent));
                        ui.monospace(row.memory_bytes.map_or_else(
                            || "未知".into(),
                            |bytes| format!("{:.1} MiB", mib(bytes)),
                        ));
                        ui.monospace(percent(row.memory_percent));
                        ui.end_row();
                    }
                });
        });
}

fn percent32(value: Option<f32>) -> String {
    value.map_or_else(|| "采样中 / 未知".into(), |value| format!("{value:.1}%"))
}

fn percent(value: Option<f64>) -> String {
    value.map_or_else(|| "未知".into(), |value| format!("{value:.1}%"))
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / 1_048_576.0
}

fn error_snapshot(message: String) -> Snapshot {
    Snapshot {
        warnings: vec![message],
        ..Snapshot::default()
    }
}

/// A single replaceable slot bounds pending snapshots even while the page is hidden.
fn sample_loop(shared: Arc<Shared>, ctx: egui::Context) {
    let mut sampler = match Sampler::new() {
        Ok(sampler) => sampler,
        Err(error) => {
            *shared.latest.lock().unwrap() = Some(error_snapshot(error));
            ctx.request_repaint();
            return;
        }
    };
    loop {
        if shared.control.lock().unwrap().stop {
            return;
        }
        let snapshot = sampler.collect();
        *shared.latest.lock().unwrap() = Some(snapshot);
        let control = shared.control.lock().unwrap();
        if control.stop {
            return;
        }
        if control.visible {
            ctx.request_repaint();
        }
        let delay = Duration::from_secs(if control.visible { 1 } else { 5 });
        let (control, _) = shared.wake.wait_timeout(control, delay).unwrap();
        if control.stop {
            return;
        }
    }
}

struct Sampler {
    system: System,
    tracked: HashSet<Identity>,
    previous: HashSet<Identity>,
    last_sample: Option<Instant>,
}

impl Sampler {
    fn new() -> Result<Self, String> {
        let mut system = System::new().map_err(|error| format!("无法读取系统进程：{error}"))?;
        system.refresh_cpu_list(CpuRefreshKind::nothing());
        Ok(Self {
            system,
            tracked: HashSet::new(),
            previous: HashSet::new(),
            last_sample: None,
        })
    }

    /// Refresh only CPU, RSS and executable identity; never collect commands or environment secrets.
    fn collect(&mut self) -> Snapshot {
        let began = Instant::now();
        self.system
            .refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .without_tasks()
                .with_cpu()
                .with_memory()
                .with_exe(UpdateKind::OnlyIfNotSet),
        );
        let processes: BTreeMap<_, _> = self
            .system
            .processes()
            .iter()
            .map(|(pid, process)| {
                let pid = pid.as_u32();
                (
                    pid,
                    ProcessInfo {
                        pid,
                        parent: process.parent().map(|pid| pid.as_u32()),
                        start: process.start_time(),
                        name: process.name().to_string_lossy().into_owned(),
                        path: process
                            .exe()
                            .map(|path| path.to_string_lossy().into_owned()),
                    },
                )
            })
            .collect();
        let selected = select_processes(&processes, &self.tracked, std::process::id());
        let enough_time = self
            .last_sample
            .is_some_and(|time| time.elapsed() >= sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        let mut snapshot = self.snapshot(&processes, &selected, enough_time);
        // Keep only live identities, so reparenting survives but PID reuse cannot inherit membership.
        self.tracked = selected
            .iter()
            .filter_map(|pid| processes.get(pid))
            .filter(|process| process.start > 0)
            .take(MAX_TRACKED)
            .map(ProcessInfo::identity)
            .collect();
        self.previous = self.tracked.clone();
        self.last_sample = Some(Instant::now());
        snapshot.sample_millis = began.elapsed().as_millis();
        snapshot
    }

    fn snapshot(
        &self,
        processes: &BTreeMap<u32, ProcessInfo>,
        selected: &BTreeSet<u32>,
        ready: bool,
    ) -> Snapshot {
        let logical_cpus = self.system.cpus().len();
        let total_memory = self.system.total_memory();
        let mut snapshot = Snapshot {
            logical_cpus,
            total_memory,
            process_count: selected.len(),
            ..Snapshot::default()
        };
        let mut inaccessible = 0;
        let mut missing_memory = 0;
        for (pid, depth) in tree_order(processes, selected) {
            let info = &processes[&pid];
            let Some(process) = self.system.process(sysinfo::Pid::from_u32(pid)) else {
                continue;
            };
            let cpu = (ready
                && logical_cpus > 0
                && self.previous.contains(&info.identity())
                && info.start > 0)
                .then(|| process.cpu_usage() / logical_cpus as f32);
            let memory = (process.memory() > 0).then(|| process.memory());
            if info.path.is_none() {
                inaccessible += 1;
            }
            if memory.is_none() {
                missing_memory += 1;
            }
            if let Some(cpu) = cpu {
                snapshot.cpu_percent = Some(snapshot.cpu_percent.unwrap_or(0.0) + cpu);
                snapshot.cpu_process_count += 1;
            }
            if memory.is_some() {
                snapshot.memory_process_count += 1;
            }
            snapshot.memory_bytes = snapshot.memory_bytes.saturating_add(memory.unwrap_or(0));
            if snapshot.rows.len() < MAX_TRACKED {
                snapshot.rows.push(ProcessRow {
                    pid,
                    parent: info.parent,
                    start_time: info.start,
                    name: info.name.clone(),
                    path: info.path.clone(),
                    cpu_percent: cpu,
                    memory_bytes: memory,
                    memory_percent: ratio(memory, total_memory),
                    scope: root_kind(info).unwrap_or("Codex 工具 / 子进程"),
                    depth,
                });
            }
        }
        snapshot.memory_percent = ratio(Some(snapshot.memory_bytes), total_memory);
        if ready && snapshot.cpu_process_count < snapshot.process_count {
            snapshot.warnings.push(format!(
                "{} 个进程的 CPU 尚在采样或不可读取；CPU 合计仅包含有效读数。",
                snapshot.process_count - snapshot.cpu_process_count
            ));
        }
        if inaccessible > 0 {
            snapshot.warnings.push(format!(
                "{inaccessible} 个进程的路径不可读取；权限限制可能使身份或指标不完整。"
            ));
        }
        if missing_memory > 0 {
            snapshot.warnings.push(format!(
                "{missing_memory} 个进程的内存不可读取；内存合计仅包含可用读数。"
            ));
        }
        if selected.len() > MAX_TRACKED {
            snapshot.warnings.push(format!("进程超过 {MAX_TRACKED} 个；合计包含全部已发现进程，明细和连续身份跟踪仅保留前 {MAX_TRACKED} 个。"));
        }
        if logical_cpus == 0 || total_memory == 0 {
            snapshot
                .warnings
                .push("系统 CPU 或物理内存信息不可用，相关比例显示未知。".into());
        }
        snapshot
    }
}

fn ratio(bytes: Option<u64>, total: u64) -> Option<f64> {
    bytes
        .filter(|_| total > 0)
        .map(|bytes| bytes as f64 * 100.0 / total as f64)
}

/// Exact Codex identities add roots; generic Node and shell names never do.
fn root_kind(process: &ProcessInfo) -> Option<&'static str> {
    let name = process.name.to_ascii_lowercase();
    if matches!(
        name.as_str(),
        "codex-windows-sandbox-service.exe" | "codex-windows-sandbox-service"
    ) {
        return Some("共享 Codex 沙箱");
    }
    if matches!(name.as_str(), "codex" | "codex.exe") {
        return Some("Codex 核心 / CLI");
    }
    let path = process
        .path
        .as_deref()
        .unwrap_or("")
        .replace('\\', "/")
        .to_ascii_lowercase();
    let windows_app = name == "chatgpt.exe" && path.contains("/windowsapps/openai.codex_");
    let mac_app =
        path.contains("/codex.app/contents/") && matches!(name.as_str(), "codex" | "chatgpt");
    (windows_app || mac_app).then_some("Codex 桌面应用")
}

/// Follow live parent edges and previously seen identities, including tools outliving their parent.
fn select_processes(
    processes: &BTreeMap<u32, ProcessInfo>,
    tracked: &HashSet<Identity>,
    observer: u32,
) -> BTreeSet<u32> {
    let children = children_by_parent(processes);
    let excluded = descendants(&children, [observer]);
    let roots = processes
        .values()
        .filter(|process| {
            root_kind(process).is_some()
                || (process.start > 0 && tracked.contains(&process.identity()))
        })
        .map(|process| process.pid);
    descendants(&children, roots)
        .difference(&excluded)
        .copied()
        .collect()
}

fn children_by_parent(processes: &BTreeMap<u32, ProcessInfo>) -> HashMap<u32, Vec<u32>> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for process in processes.values() {
        if let Some(parent) = process.parent.and_then(|pid| processes.get(&pid)) {
            // A reused parent PID that started later cannot own this older child.
            if parent.start == 0 || process.start == 0 || parent.start <= process.start {
                children.entry(parent.pid).or_default().push(process.pid);
            }
        }
    }
    children
}

fn descendants(
    children: &HashMap<u32, Vec<u32>>,
    roots: impl IntoIterator<Item = u32>,
) -> BTreeSet<u32> {
    let mut queue: VecDeque<_> = roots.into_iter().collect();
    let mut selected = BTreeSet::new();
    while let Some(pid) = queue.pop_front() {
        if selected.insert(pid) {
            queue.extend(children.get(&pid).into_iter().flatten().copied());
        }
    }
    selected
}

fn tree_order(
    processes: &BTreeMap<u32, ProcessInfo>,
    selected: &BTreeSet<u32>,
) -> Vec<(u32, usize)> {
    let children = children_by_parent(processes);
    let mut stack: Vec<_> = selected
        .iter()
        .rev()
        .filter(|pid| {
            processes[pid].parent.is_none_or(|parent| {
                !selected.contains(&parent)
                    || !children
                        .get(&parent)
                        .is_some_and(|children| children.contains(pid))
            })
        })
        .map(|pid| (*pid, 0))
        .collect();
    let mut visited = HashSet::new();
    let mut ordered = Vec::with_capacity(selected.len());
    while let Some((pid, depth)) = stack.pop() {
        if visited.insert(pid) {
            ordered.push((pid, depth));
            if let Some(children) = children.get(&pid) {
                stack.extend(
                    children
                        .iter()
                        .rev()
                        .filter(|pid| selected.contains(pid))
                        .map(|pid| (*pid, depth + 1)),
                );
            }
        }
    }
    ordered.extend(
        selected
            .iter()
            .filter(|pid| !visited.contains(pid))
            .map(|pid| (*pid, 0)),
    );
    ordered
}

/// Optional headless diagnostic uses the same native sampler and warms up CPU deltas.
pub fn sample_json() -> Result<String, String> {
    let mut sampler = Sampler::new()?;
    sampler.collect();
    thread::sleep(Duration::from_secs(1));
    serde_json::to_string_pretty(&sampler.collect()).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(pid: u32, parent: Option<u32>, start: u64, name: &str) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent,
            start,
            name: name.into(),
            path: None,
        }
    }

    fn map(rows: Vec<ProcessInfo>) -> BTreeMap<u32, ProcessInfo> {
        rows.into_iter().map(|row| (row.pid, row)).collect()
    }

    // Nested CLI roots must not double count children; unrelated editor Node processes stay out.
    #[test]
    fn deduplicates_roots_and_excludes_unrelated_tools() {
        let rows = map(vec![
            info(1, None, 1, "codex"),
            info(2, Some(1), 2, "codex.exe"),
            info(3, Some(2), 3, "node.exe"),
            info(4, None, 4, "node.exe"),
        ]);
        assert_eq!(
            select_processes(&rows, &HashSet::new(), 99),
            BTreeSet::from([1, 2, 3])
        );
    }

    // A sampled shell continues to count after its Codex parent exits and the shell is reparented.
    #[test]
    fn retains_reparented_children_and_their_new_children() {
        let rows = map(vec![
            info(2, None, 2, "powershell.exe"),
            info(3, Some(2), 3, "node.exe"),
        ]);
        assert_eq!(
            select_processes(&rows, &HashSet::from([(2, 2)]), 99),
            BTreeSet::from([2, 3])
        );
    }

    // A new unrelated process reusing a tracked PID must not inherit Codex membership.
    #[test]
    fn excludes_reused_pid() {
        let rows = map(vec![
            info(2, None, 20, "node.exe"),
            info(3, Some(2), 21, "pwsh.exe"),
        ]);
        assert!(select_processes(&rows, &HashSet::from([(2, 2)]), 99).is_empty());
    }

    // Shared sandbox is a root even without a readable path; its service parent is not included.
    #[test]
    fn sandbox_root_includes_shell_but_not_service_parent() {
        let rows = map(vec![
            info(1, None, 1, "services.exe"),
            info(2, Some(1), 2, "codex-windows-sandbox-service.exe"),
            info(3, Some(2), 3, "powershell.exe"),
        ]);
        assert_eq!(
            select_processes(&rows, &HashSet::new(), 99),
            BTreeSet::from([2, 3])
        );
    }

    // Exclude only the monitor instance; the same executable used as a Codex hook remains eligible.
    #[test]
    fn excludes_observer_tree_but_includes_hook_helpers() {
        let rows = map(vec![
            info(1, None, 1, "codex"),
            info(2, Some(1), 2, "codex-profiler.exe"),
            info(3, Some(1), 3, "codex-profiler.exe"),
            info(4, Some(2), 4, "node.exe"),
        ]);
        assert_eq!(
            select_processes(&rows, &HashSet::new(), 2),
            BTreeSet::from([1, 3])
        );
    }

    // Electron's historical ChatGPT filename identifies Codex only under the actual Codex package.
    #[test]
    fn recognizes_codex_package_not_generic_chatgpt() {
        let mut app = info(1, None, 1, "ChatGPT.exe");
        app.path = Some("C:\\Program Files\\WindowsApps\\OpenAI.Codex_1\\app\\ChatGPT.exe".into());
        assert_eq!(root_kind(&app), Some("Codex 桌面应用"));
        app.path = Some("C:\\Other\\ChatGPT.exe".into());
        assert_eq!(root_kind(&app), None);
    }
}
