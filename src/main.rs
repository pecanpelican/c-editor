// Hide the console window on Windows release builds.
#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

mod app;
mod buffer;
mod editor;
mod find;
mod perf;
mod highlight;
mod lsp;
mod dap;
mod make;
mod terminal;
mod profile;
mod project;
mod recovery;
mod refactor;
mod settings;
mod textops;
mod theme;
mod viewer;

use std::path::PathBuf;

/// Apps started from the Finder/Dock get a minimal PATH (no Homebrew, no
/// ~/.local/bin). Ask the user's login shell for the real one so `make`,
/// `clang` and `clangd` are found the same way as in a terminal.
#[cfg(target_os = "macos")]
fn fix_path_from_login_shell() {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let Ok(mut child) = Command::new(shell)
        .args(["-l", "-c", "printf '%s' \"$PATH\""])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < Duration::from_secs(3) => {
                std::thread::sleep(Duration::from_millis(20))
            }
            _ => {
                let _ = child.kill();
                return;
            }
        }
    }
    let mut out = String::new();
    if let Some(mut so) = child.stdout.take() {
        let _ = so.read_to_string(&mut out);
    }
    let out = out.trim();
    if !out.is_empty() {
        // SAFETY: called at startup before any other threads exist.
        unsafe { std::env::set_var("PATH", out) };
    }
}

fn main() -> eframe::Result {
    #[cfg(target_os = "macos")]
    fix_path_from_login_shell();
    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("cedit")
            .with_app_id("cedit")
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([640.0, 400.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "cedit",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, args)))),
    )
}
