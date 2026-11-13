#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod collector;
mod hook;
mod model;
mod ui;

use eframe::egui;
use std::path::PathBuf;
use std::time::{Duration, Instant};

struct Options {
    codex_home: PathBuf,
    db: PathBuf,
    headless_seconds: Option<u64>,
}

/// Keep command-line overrides small so fixture-based checks use the same collector.
fn options() -> Result<Options, String> {
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .map(|home| PathBuf::from(home).join(".codex"))
        })
        .ok_or("Set CODEX_HOME or pass --codex-home PATH")?;
    let data = std::env::current_exe()
        .map_err(|error| error.to_string())?
        .parent()
        .ok_or("Executable has no parent directory")?
        .join("data");
    let mut result = Options {
        codex_home: home,
        db: data.join("monitor.sqlite"),
        headless_seconds: None,
    };
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        if matches!(argument.as_str(), "--hook" | "--print-hooks") {
            continue;
        }
        let value = args
            .next()
            .ok_or_else(|| format!("Missing value for {argument}"))?;
        match argument.as_str() {
            "--codex-home" => result.codex_home = value.into(),
            "--db" => result.db = value.into(),
            "--headless-seconds" => {
                result.headless_seconds = Some(value.parse().map_err(|_| "Invalid seconds")?);
            }
            _ => return Err(format!("Unknown option: {argument}")),
        }
    }
    Ok(result)
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
    let args: Vec<_> = std::env::args().collect();
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        print_help();
        return Ok(());
    }
    let options = options().unwrap_or_else(|error| {
        eprintln!("{error}");
        std::process::exit(2);
    });
    if args.iter().any(|arg| arg == "--hook") {
        if let Err(error) = hook::emit(&options.codex_home) {
            eprintln!("Codex monitor hook: {error}");
            std::process::exit(1);
        }
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--print-hooks") {
        let exe = std::env::current_exe().expect("Executable path");
        println!("{}", hook::configuration(&exe, &options.codex_home));
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
        "Codex Monitor",
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

fn print_help() {
    println!(
        "Codex Monitor\n\
Usage: codex-monitor [--codex-home PATH] [--db PATH] [--headless-seconds N]\n\
       codex-monitor --print-hooks [--codex-home PATH]\n\
       codex-monitor --hook [--codex-home PATH]\n\n\
The default source is CODEX_HOME or USERPROFILE/HOME/.codex.\n\
The default SQLite cache is data/monitor.sqlite beside this executable.\n\
--print-hooks prints optional command hook configuration; it does not install it.\n\
Merge the printed hooks into your Codex hooks configuration and review them in\n\
Codex Settings > Hooks (CLI: /hooks). Hooks write metadata to monitor/hooks.jsonl\n\
inside the selected CODEX_HOME. No prompts or tool result contents are saved."
    );
}
