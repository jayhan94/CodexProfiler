#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod collector;
mod coverage_migration;
mod hook;
mod model;
mod pricing;
mod process_ownership;
mod resource_tree;
mod resources;
mod setup;
mod stats;
mod theme;
mod ui;

use eframe::egui;
use std::path::PathBuf;
use std::time::{Duration, Instant};

struct Options {
    codex_home: PathBuf,
    db: PathBuf,
    headless_seconds: Option<u64>,
    mode: Mode,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Gui,
    Hook,
    PrintHooks,
    InstallHooks,
    UninstallHooks,
    Doctor,
    Resources,
    Version,
    Help,
}

/// Keep command-line overrides small so fixture-based checks use the same collector.
fn options() -> Result<Options, String> {
    let mut home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .map(|home| PathBuf::from(home).join(".codex"))
        });
    let mut db = None;
    let mut seconds = None;
    let mut mode = Mode::Gui;
    let mut args = std::env::args_os().skip(1);
    while let Some(argument) = args.next() {
        let flag = argument.to_str().ok_or("Options must be valid text")?;
        let selected = match flag {
            "--hook" => Some(Mode::Hook),
            "--print-hooks" => Some(Mode::PrintHooks),
            "--install-hooks" => Some(Mode::InstallHooks),
            "--uninstall-hooks" => Some(Mode::UninstallHooks),
            "--doctor" => Some(Mode::Doctor),
            "--resources-json" => Some(Mode::Resources),
            "--version" | "-V" => Some(Mode::Version),
            "--help" | "-h" => Some(Mode::Help),
            _ => None,
        };
        if let Some(selected) = selected {
            if mode != Mode::Gui {
                return Err("Choose only one command mode".into());
            }
            mode = selected;
            continue;
        }
        if !matches!(flag, "--codex-home" | "--db" | "--headless-seconds") {
            return Err(format!("Unknown option: {flag}. Use --help for usage."));
        }
        let value = args
            .next()
            .filter(|value| !value.is_empty() && !value.to_string_lossy().starts_with("--"))
            .ok_or_else(|| format!("Missing value for {flag}"))?;
        match flag {
            "--codex-home" => home = Some(value.into()),
            "--db" => db = Some(PathBuf::from(value)),
            "--headless-seconds" => {
                seconds = Some(
                    value
                        .to_str()
                        .ok_or("Invalid seconds")?
                        .parse::<u64>()
                        .map_err(|_| "Invalid seconds")?,
                );
            }
            _ => unreachable!(),
        }
    }
    if seconds.is_some() && mode != Mode::Gui {
        return Err("--headless-seconds cannot be combined with another command mode".into());
    }
    if matches!(mode, Mode::Help | Mode::Version) {
        return Ok(Options {
            codex_home: PathBuf::new(),
            db: PathBuf::new(),
            headless_seconds: None,
            mode,
        });
    }
    let exe = std::env::current_exe().map_err(|error| error.to_string())?;
    Ok(Options {
        codex_home: home.ok_or("Set CODEX_HOME or pass --codex-home PATH")?,
        db: match db {
            Some(db) => db,
            None => setup::default_database(&exe)?,
        },
        headless_seconds: seconds,
        mode,
    })
}

/// The headless path exercises real file ingestion and SQLite without a graphics device.
fn headless(options: Options, seconds: u64) {
    let receiver = collector::start(options.codex_home, options.db, egui::Context::default());
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut latest = None;
    while Instant::now() < deadline {
        if let Ok(snapshot) = receiver.recv_timeout(Duration::from_millis(200)) {
            latest = Some(snapshot);
        }
    }
    if let Some(snapshot) = latest {
        let failed = snapshot.error.is_some();
        let sessions: Vec<_> = snapshot
            .sessions
            .iter()
            .map(|session| {
                serde_json::json!({
                    "id": session.id,
                    "phase": session.phase,
                    "title": session.title,
                    "is_internal": session.is_internal,
                    "requests": session.requests.len(),
                    "cost_usd": session.cost.total(),
                    "priced_requests": session.cost.priced_requests,
                    "unpriced_requests": session.cost.unpriced_requests,
                    "service_tier": session.service_tier,
                    "commands_with_details": session.tools.iter().filter(|t| !t.details.is_empty()).count(),
                    "turn_id": session.turn_id,
                    "output_tokens": session.output_tokens,
                    "tools": session.tools.len(),
                    "request_tps": session.request_tps
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({"sessions": sessions, "error": snapshot.error})
        );
        if failed {
            std::process::exit(1);
        }
    } else {
        eprintln!("No collector snapshot received within {seconds} seconds");
        std::process::exit(1);
    }
}

fn main() -> eframe::Result<()> {
    attach_cli_console();
    let options = options().unwrap_or_else(|error| {
        eprintln!("{error}");
        std::process::exit(2);
    });
    if options.mode == Mode::Help {
        print_help();
        return Ok(());
    }
    if options.mode == Mode::Version {
        println!("CodexProfiler {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if options.mode == Mode::Resources {
        return print_resources();
    }
    if options.mode == Mode::Hook {
        if let Err(error) = hook::emit(&options.codex_home) {
            eprintln!("CodexProfiler hook: {error}");
            std::process::exit(1);
        }
        return Ok(());
    }
    let exe = std::env::current_exe().expect("Executable path");
    if options.mode == Mode::PrintHooks {
        println!("{}", hook::configuration(&exe, &options.codex_home));
        return Ok(());
    }
    if matches!(options.mode, Mode::InstallHooks | Mode::UninstallHooks) {
        let result = setup::manage(
            &options.codex_home,
            &exe,
            options.mode == Mode::InstallHooks,
        )
        .unwrap_or_else(|error| {
            eprintln!("CodexProfiler setup: {error}");
            std::process::exit(1);
        });
        println!("{result}");
        return Ok(());
    }
    if options.mode == Mode::Doctor {
        let (report, healthy) = setup::doctor(&options.codex_home, &options.db, &exe);
        println!("{report}");
        if !healthy {
            std::process::exit(1);
        }
        return Ok(());
    }
    if let Some(seconds) = options.headless_seconds {
        headless(options, seconds);
        return Ok(());
    }
    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1120.0, 720.0])
            .with_min_inner_size([800.0, 500.0]),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "CodexProfiler",
        native,
        Box::new(move |context| {
            Ok(Box::new(ui::Monitor::new(
                context,
                options.codex_home,
                options.db,
            )))
        }),
    )
}

/// Read-only resource probe uses the same sampler and normalization as the resources page.
fn print_resources() -> eframe::Result<()> {
    match resources::sample_json() {
        Ok(snapshot) => println!("{snapshot}"),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
    Ok(())
}

fn print_help() {
    println!(
        "CodexProfiler\n\
Usage: codex-profiler [--codex-home PATH] [--db PATH] [--headless-seconds N]\n\
       codex-profiler --install-hooks|--uninstall-hooks|--doctor [--codex-home PATH]\n\
       codex-profiler --print-hooks [--codex-home PATH]\n\
       codex-profiler --version|--resources-json\n\
       codex-profiler --hook [--codex-home PATH]\n\n\
The default source is CODEX_HOME or USERPROFILE/HOME/.codex.\n\
The SQLite cache uses your platform user data directory. Existing executable-relative\n\
data/monitor.sqlite caches remain in use. Use --db PATH to choose another cache.\n\
--print-hooks prints optional command hook configuration; it does not install it.\n\
--install-hooks merges hooks into CODEX_HOME/hooks.json and backs up changed config.\n\
--uninstall-hooks removes managed hooks and preserves collected data. Review hooks in\n\
Codex Settings > Hooks (CLI: /hooks). Hooks write metadata to monitor/hooks.jsonl\n\
inside the selected CODEX_HOME. Full prompts and tool result bodies are excluded; titles and tool arguments stay local."
    );
}

#[cfg(not(windows))]
fn attach_cli_console() {}

/// GUI releases attach only for explicit CLI commands, preserving redirected handles.
#[cfg(windows)]
fn attach_cli_console() {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn AttachConsole(process: u32) -> i32;
        fn GetStdHandle(kind: u32) -> *mut c_void;
        fn SetStdHandle(kind: u32, handle: *mut c_void) -> i32;
    }
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.is_empty() || args.iter().any(|argument| argument == "--hook") {
        return;
    }
    unsafe {
        let handles: Vec<_> = [(-10_i32) as u32, (-11_i32) as u32, (-12_i32) as u32]
            .into_iter()
            .map(|kind| (kind, GetStdHandle(kind)))
            .collect();
        AttachConsole(u32::MAX);
        for (kind, handle) in handles {
            if !handle.is_null() && handle as isize != -1 {
                SetStdHandle(kind, handle);
            }
        }
    }
}
