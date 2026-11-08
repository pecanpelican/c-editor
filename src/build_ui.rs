//! Make integration UI: toolbar, Build menu, output and problems panels.

use std::path::{Path, PathBuf};

use egui::{Align, Key, KeyboardShortcut, Layout, Modifiers, RichText, Ui};

use super::{App, BottomTab, Modal};
use crate::make::{self, BuildConfig, LineKind, Severity, Task, TaskKind};

impl App {
    pub(super) fn project_root(&self) -> Option<PathBuf> {
        self.tree.as_ref().map(|t| t.root.clone())
    }

    pub(super) fn build_config(&self) -> BuildConfig {
        self.project_root()
            .and_then(|r| self.build_configs.get(&r).cloned())
            .unwrap_or_default()
    }

    /// Re-detect the Makefile when the project or active file changes, and
    /// re-read targets when the Makefile changes. Also polls the running task.
    pub(super) fn update_build_state(&mut self, ctx: &egui::Context) {
        let root = self.project_root();
        let active = self.docs.get(self.active).and_then(|d| d.path.clone());
        let key = root.clone().map(|r| (r, active.as_ref().and_then(|a| a.parent().map(Path::to_path_buf))));
        let now = ctx.input(|i| i.time);
        let recheck = (now - self.last_disk_check) < 0.001; // disk check just ran
        if key != self.makefile_key || recheck {
            if key != self.makefile_key {
                self.makefile_key = key;
                self.makefile_mtime = None;
            }
            let found = root.as_ref().and_then(|r| {
                active
                    .as_ref()
                    .filter(|a| a.starts_with(r))
                    .and_then(|a| make::find_makefile(a, r))
                    .or_else(|| make::find_makefile(r, r))
            });
            let mtime = found
                .as_ref()
                .and_then(|p| std::fs::metadata(p).ok())
                .and_then(|m| m.modified().ok());
            if found != self.makefile || mtime != self.makefile_mtime {
                self.targets = found.as_ref().map(|p| make::parse_targets(p)).unwrap_or_default();
                self.makefile = found;
                self.makefile_mtime = mtime;
            }
        }

        let mut finished = None;
        if let Some(task) = &mut self.task {
            let was_running = task.is_running();
            task.poll();
            if was_running && !task.is_running() {
                finished = Some((task.kind, task.succeeded()));
            }
        }
        if let Some((kind, ok)) = finished {
            self.on_task_finished(ctx, kind, ok);
        }
    }

    fn on_task_finished(&mut self, ctx: &egui::Context, kind: TaskKind, ok: bool) {
        let Some(task) = &self.task else { return };
        if kind != TaskKind::Run {
            self.diagnostics = task.diagnostics.clone();
            self.diag_cursor = None;
        }
        if let Some(t) = &mut self.tree {
            t.refresh();
        }
        let errors = task.count(Severity::Error);
        let warnings = task.count(Severity::Warning);
        match kind {
            TaskKind::Build => {
                if ok {
                    let w = if warnings > 0 {
                        format!(" with {warnings} warning{}", plural(warnings))
                    } else {
                        String::new()
                    };
                    if self.run_after_build {
                        self.run_after_build = false;
                        self.start_run_command(ctx);
                    } else if self.debug_after_build {
                        self.debug_after_build = false;
                        self.start_debugging(ctx);
                    } else if self.profile_after_build {
                        self.profile_after_build = false;
                        self.start_profiling(ctx);
                    } else {
                        self.toast(ctx, format!("Build succeeded{w}"), false);
                    }
                } else {
                    self.run_after_build = false;
                    self.debug_after_build = false;
                    self.profile_after_build = false;
                    let msg = if errors > 0 {
                        format!("Build failed: {errors} error{}", plural(errors))
                    } else {
                        "Build failed".to_string()
                    };
                    self.toast(ctx, msg, true);
                    if errors > 0 {
                        self.bottom_tab = BottomTab::Problems;
                    }
                }
            }
            TaskKind::Clean => self.toast(ctx, if ok { "Clean finished" } else { "Clean failed" }, !ok),
            TaskKind::Run => {}
        }
    }

    /// Error/warning counts from the language server across open files.
    pub(super) fn live_counts(&self) -> (usize, usize) {
        let live = self.live_diagnostics();
        let e = live.iter().filter(|d| d.severity == Severity::Error).count();
        let w = live.iter().filter(|d| d.severity == Severity::Warning).count();
        (e, w)
    }

    pub(super) fn problem_counts(&self) -> (usize, usize) {
        let e = self.diagnostics.iter().filter(|d| d.severity == Severity::Error).count();
        let w = self.diagnostics.iter().filter(|d| d.severity == Severity::Warning).count();
        (e, w)
    }

    pub(super) fn make_dir(&self) -> Option<PathBuf> {
        self.makefile
            .as_ref()
            .and_then(|m| m.parent().map(Path::to_path_buf))
            .or_else(|| self.project_root())
    }

    fn make_args(&self, cfg: &BuildConfig, target: Option<&str>) -> Vec<String> {
        let mut args = Vec::new();
        if cfg.jobs > 0 {
            args.push(format!("-j{}", cfg.jobs));
        }
        args.extend(make::split_args(&cfg.extra_args));
        if let Some(t) = target.filter(|t| !t.is_empty()) {
            args.push(t.to_string());
        }
        args
    }

    fn start_make(&mut self, ctx: &egui::Context, kind: TaskKind, target: Option<&str>) {
        let Some(dir) = self.make_dir() else {
            self.toast(ctx, "Open a folder with a Makefile first", true);
            return;
        };
        if self.makefile.is_none() {
            self.toast(ctx, "No Makefile found in this project", true);
            return;
        }
        if let Some(t) = &mut self.task {
            t.stop();
        }
        self.save_all_now(ctx);
        let cfg = self.build_config();
        let program = make::make_program(&cfg);
        let args = self.make_args(&cfg, target);
        self.task = Some(Task::spawn(kind, &program, &args, &dir, ctx.clone()));
        self.bottom_open = true;
        self.bottom_tab = BottomTab::Output;
    }

    pub(super) fn cmd_build(&mut self, ctx: &egui::Context, target: Option<String>) {
        let cfg = self.build_config();
        let t = target.unwrap_or(cfg.target);
        self.run_after_build = false;
        self.start_make(ctx, TaskKind::Build, Some(&t));
    }

    pub(super) fn cmd_clean(&mut self, ctx: &egui::Context) {
        if !self.targets.iter().any(|t| t == "clean") {
            self.toast(ctx, "This Makefile has no `clean` target", true);
            return;
        }
        self.start_make(ctx, TaskKind::Clean, Some("clean"));
    }

    /// Run: build, then start the configured command (or `make run`) in a
    /// terminal tab, where the program can read keyboard input.
    pub(super) fn cmd_run(&mut self, ctx: &egui::Context) {
        let cfg = self.build_config();
        let has_cmd = !cfg.run_command.trim().is_empty();
        let has_run_target = self.targets.iter().any(|t| t == "run");
        if !has_cmd && !has_run_target {
            if self.project_root().is_some() {
                self.toast(ctx, "Set the command to run in Build Settings", false);
                self.modal = Some(Modal::BuildSettings(cfg));
            }
            return;
        }
        if self.makefile.is_some() {
            let target = if cfg.target == "run" { String::new() } else { cfg.target.clone() };
            self.cmd_build(ctx, Some(target));
            self.run_after_build = true;
        } else {
            self.start_run_command(ctx);
        }
    }

    fn start_run_command(&mut self, ctx: &egui::Context) {
        let cfg = self.build_config();
        let Some(dir) = self.make_dir() else { return };
        let (program, args) = if !cfg.run_command.trim().is_empty() {
            make::shell_command(cfg.run_command.trim())
        } else {
            let mut args = make::split_args(&cfg.extra_args);
            args.push("--no-print-directory".into());
            args.push("run".into());
            (make::make_program(&cfg), args)
        };
        self.run_in_terminal(ctx, program, args, dir);
    }

    pub(super) fn cmd_stop(&mut self) {
        self.run_after_build = false;
        self.debug_after_build = false;
        self.profile_after_build = false;
        if let Some(t) = &mut self.task {
            t.stop();
        }
        self.stop_run_terminal();
    }

    /// Jump to the next/previous error or warning.
    pub(super) fn goto_diagnostic(&mut self, forward: bool) {
        let idx: Vec<usize> = self
            .diagnostics
            .iter()
            .enumerate()
            .filter(|(_, d)| d.severity != Severity::Note)
            .map(|(i, _)| i)
            .collect();
        if idx.is_empty() {
            return;
        }
        let pos = self.diag_cursor.and_then(|c| idx.iter().position(|&i| i == c));
        let next = match (pos, forward) {
            (None, true) => 0,
            (None, false) => idx.len() - 1,
            (Some(p), true) => (p + 1) % idx.len(),
            (Some(p), false) => (p + idx.len() - 1) % idx.len(),
        };
        self.open_diagnostic(idx[next]);
    }

    fn open_diagnostic(&mut self, i: usize) {
        let Some(d) = self.diagnostics.get(i).cloned() else { return };
        self.diag_cursor = Some(i);
        self.open_file(&d.path);
        if let Some(doc) = self.docs.get_mut(self.active)
            && doc.path.as_deref() == Some(d.path.as_path())
        {
            doc.goto_line_col(d.line, d.col);
        }
    }

    pub(super) fn build_menu(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let has_make = self.makefile.is_some();
        let running = self.task.as_ref().is_some_and(|t| t.is_running()) || self.run_terminal_running();
        ui.menu_button("Build", |ui| {
            if ui.add_enabled(has_make, egui::Button::new("Build").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::Build))).clicked() {
                self.cmd_build(&ctx, None);
            }
            if ui.add_enabled(self.project_root().is_some(), egui::Button::new("Build and Run").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::Run))).clicked() {
                self.cmd_run(&ctx);
            }
            if ui.add_enabled(self.project_root().is_some(), egui::Button::new("Build and Debug").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::Debug))).clicked() {
                self.cmd_debug(&ctx);
            }
            if ui.add_enabled(self.project_root().is_some() && !cfg!(windows), egui::Button::new("Build and Profile").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::Profile))).clicked() {
                self.cmd_profile(&ctx);
            }
            if ui.add_enabled(has_make, egui::Button::new("Clean")).clicked() {
                self.cmd_clean(&ctx);
            }
            if ui.add_enabled(running, egui::Button::new("Stop").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::Stop))).clicked() {
                self.cmd_stop();
            }
            ui.separator();
            let targets = self.targets.clone();
            ui.add_enabled_ui(!targets.is_empty(), |ui| {
                ui.menu_button("Build Target", |ui| {
                    for t in targets {
                        if ui.button(&t).clicked() {
                            self.cmd_build(&ctx, Some(t));
                        }
                    }
                });
            });
            ui.separator();
            if ui.add(egui::Button::new("Next Problem").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::NextProblem))).clicked() {
                self.goto_diagnostic(true);
            }
            if ui.add(egui::Button::new("Previous Problem").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::PrevProblem))).clicked() {
                self.goto_diagnostic(false);
            }
            ui.separator();
            if ui.add_enabled(self.project_root().is_some(), egui::Button::new("Build Settings…")).clicked() {
                self.modal = Some(Modal::BuildSettings(self.build_config()));
            }
            if let Some(m) = &self.makefile
                && ui.button("Open Makefile").clicked()
            {
                let m = m.clone();
                self.open_file(&m);
            }
        });
    }

    /// Target picker and Build/Run/Stop buttons, right-aligned in the menu bar.
    pub(super) fn build_toolbar(&mut self, ui: &mut Ui) {
        if self.project_root().is_none() {
            return;
        }
        let ctx = ui.ctx().clone();
        let debugging = self.debug_active();
        let running = self.task.as_ref().is_some_and(|t| t.is_running()) || (self.run_terminal_running() && !debugging);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add_space(4.0);
            if debugging && !running {
                if ui.button(RichText::new("Stop").color(self.theme.error)).on_hover_text("Stop debugging").clicked() {
                    self.cmd_stop_debugging(&ctx);
                }
                if ui.button("Continue").clicked() {
                    self.cmd_debug(&ctx);
                }
                ui.label(RichText::new("debugging").color(self.theme.warning));
            } else if running {
                if ui.button(RichText::new("Stop").color(self.theme.error)).clicked() {
                    self.cmd_stop();
                }
                if self.task.as_ref().is_some_and(|t| t.is_running()) {
                    ui.spinner();
                } else {
                    ui.label(RichText::new("running").weak());
                }
            } else {
                if ui.button("Debug").on_hover_text(format!("Build and debug ({})", self.keymap.label(&ctx, crate::settings::Action::Debug))).clicked() {
                    self.cmd_debug(&ctx);
                }
                if ui.button("Run").on_hover_text("Build and run").clicked() {
                    self.cmd_run(&ctx);
                }
                if ui
                    .add_enabled(self.makefile.is_some(), egui::Button::new(RichText::new("Build").strong()))
                    .clicked()
                {
                    self.cmd_build(&ctx, None);
                }
            }
            if self.makefile.is_some() {
                let mut cfg = self.build_config();
                let before = cfg.target.clone();
                let label = if cfg.target.is_empty() {
                    match self.targets.first() {
                        Some(t) => format!("{t} (default)"),
                        None => "default".to_string(),
                    }
                } else {
                    cfg.target.clone()
                };
                egui::ComboBox::from_id_salt("target")
                    .selected_text(label)
                    .width(110.0)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut cfg.target, String::new(), "(default target)");
                        for t in &self.targets {
                            ui.selectable_value(&mut cfg.target, t.clone(), t);
                        }
                    });
                if cfg.target != before
                    && let Some(r) = self.project_root()
                {
                    self.build_configs.insert(r, cfg);
                }
            } else {
                ui.label(RichText::new("no Makefile").weak());
            }
            let (e, w) = self.problem_counts();
            let (le, lw) = self.live_counts();
            let (e, w) = (e.max(le), w.max(lw));
            if e > 0 || w > 0 {
                let text = RichText::new(format!("{e} error{}, {w} warning{}", plural(e), plural(w))).color(if e > 0 {
                    self.theme.error
                } else {
                    self.theme.warning
                });
                if ui.add(egui::Button::new(text).frame(false)).on_hover_text("Show problems").clicked() {
                    self.bottom_open = true;
                    self.bottom_tab = BottomTab::Problems;
                }
            }
        });
    }

    pub(super) fn output_panel(&mut self, ui: &mut Ui) {
        let Some(task) = &self.task else {
            let hint = if self.makefile.is_some() {
                let sc = ui.ctx().format_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::B));
                format!("Press {sc} to build.")
            } else {
                "No Makefile found. Open a folder that has one to build it.".to_string()
            };
            ui.label(RichText::new(hint).weak());
            return;
        };
        let theme = &self.theme;
        let font = egui::FontId::monospace(12.5);
        let row_h = ui.fonts_mut(|f| f.row_height(&font)) + 2.0;
        let mut open = None;
        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show_rows(ui, row_h, task.lines.len(), |ui, range| {
                for line in &task.lines[range] {
                    let (color, clickable) = match line.diag.and_then(|i| task.diagnostics.get(i)) {
                        Some(d) => (
                            match d.severity {
                                Severity::Error => theme.error,
                                Severity::Warning => theme.warning,
                                Severity::Note => theme.function,
                            },
                            true,
                        ),
                        None => match line.kind {
                            LineKind::Command => (theme.comment, false),
                            LineKind::Info => (theme.comment, false),
                            LineKind::Stderr => (theme.text, false),
                            LineKind::Stdout => (theme.text, false),
                        },
                    };
                    let text = RichText::new(&line.text).font(font.clone()).color(color);
                    if clickable {
                        let r = ui.add(egui::Label::new(text.underline()).sense(egui::Sense::click()).extend());
                        let r = r.on_hover_cursor(egui::CursorIcon::PointingHand);
                        if r.clicked() {
                            open = line.diag;
                        }
                    } else {
                        ui.add(egui::Label::new(text).extend());
                    }
                }
            });
        if let Some(i) = open {
            // Diagnostics of the visible task equal self.diagnostics after a
            // build; look the line up by value to be safe.
            let d = task.diagnostics[i].clone();
            if let Some(j) = self.diagnostics.iter().position(|x| *x == d) {
                self.open_diagnostic(j);
            } else {
                self.diagnostics = task.diagnostics.clone();
                self.open_diagnostic(i);
            }
        }
    }

    pub(super) fn problems_panel(&mut self, ui: &mut Ui) {
        // Build output first, then live problems from the language server.
        let mut list: Vec<(Option<usize>, crate::make::Diagnostic)> =
            self.diagnostics.iter().cloned().enumerate().map(|(i, d)| (Some(i), d)).collect();
        for d in self.live_diagnostics() {
            if !self.diagnostics.iter().any(|b| b.path == d.path && b.line == d.line && b.message == d.message) {
                list.push((None, d));
            }
        }
        if list.is_empty() {
            ui.label(RichText::new("No problems. Build errors and live errors from clangd show up here.").weak());
            return;
        }
        let root = self.project_root();
        let theme = self.theme.clone();
        let mut open: Option<(Option<usize>, crate::make::Diagnostic)> = None;
        let font = egui::FontId::monospace(12.5);
        egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
            for (bi, d) in &list {
                let rel = root
                    .as_ref()
                    .and_then(|r| d.path.strip_prefix(r).ok())
                    .unwrap_or(&d.path);
                let (word, color) = match d.severity {
                    Severity::Error => ("error", theme.error),
                    Severity::Warning => ("warning", theme.warning),
                    Severity::Note => ("note", theme.comment),
                };
                let indent = if d.severity == Severity::Note { "    " } else { "" };
                let mut job = egui::text::LayoutJob::default();
                job.append(indent, 0.0, egui::TextFormat::simple(font.clone(), theme.text));
                job.append(&format!("{word:<8}"), 0.0, egui::TextFormat::simple(font.clone(), color));
                job.append(
                    &format!("{}:{}:{}  ", rel.display(), d.line + 1, d.col + 1),
                    0.0,
                    egui::TextFormat::simple(font.clone(), theme.function),
                );
                job.append(&d.message, 0.0, egui::TextFormat::simple(font.clone(), theme.text));
                if bi.is_none() {
                    job.append("  (live)", 0.0, egui::TextFormat::simple(font.clone(), theme.comment));
                }
                let selected = bi.is_some() && self.diag_cursor == *bi;
                if ui.add(egui::Button::selectable(selected, job).frame_when_inactive(false)).clicked() {
                    open = Some((*bi, d.clone()));
                }
            }
        });
        match open {
            Some((Some(i), _)) => self.open_diagnostic(i),
            Some((None, d)) => {
                self.open_file(&d.path);
                if let Some(doc) = self.docs.get_mut(self.active)
                    && doc.path.as_deref() == Some(d.path.as_path())
                {
                    doc.goto_line_col(d.line, d.col);
                }
            }
            None => {}
        }
    }

    pub(super) fn build_settings_modal(&mut self, ctx: &egui::Context, mut cfg: BuildConfig) {
        let mut done = None;
        let targets = self.targets.clone();
        let detected = make::make_program(&BuildConfig::default());
        egui::Modal::new(egui::Id::unique("build-settings")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.heading("Build settings");
            match &self.makefile {
                Some(m) => ui.label(RichText::new(format!("Makefile: {}", m.display())).weak()),
                None => ui.label(RichText::new("No Makefile found in this project.").weak()),
            };
            ui.add_space(6.0);
            egui::Grid::new("build-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label("Build target");
                egui::ComboBox::from_id_salt("cfg-target")
                    .selected_text(if cfg.target.is_empty() { "(default)".to_string() } else { cfg.target.clone() })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut cfg.target, String::new(), "(default)");
                        for t in &targets {
                            ui.selectable_value(&mut cfg.target, t.clone(), t);
                        }
                    });
                ui.end_row();

                ui.label("Extra make arguments");
                ui.add(egui::TextEdit::singleline(&mut cfg.extra_args).hint_text("e.g. DEBUG=1 CC=clang").desired_width(280.0));
                ui.end_row();

                ui.label("Parallel jobs");
                ui.horizontal(|ui| {
                    ui.add(egui::DragValue::new(&mut cfg.jobs).range(0..=64));
                    ui.label(RichText::new("0 = make's default").weak());
                });
                ui.end_row();

                ui.label("Run command");
                ui.add(
                    egui::TextEdit::singleline(&mut cfg.run_command)
                        .hint_text(if cfg!(windows) { "e.g. demo.exe" } else { "e.g. ./demo" })
                        .desired_width(280.0),
                );
                ui.end_row();

                ui.label("Make program");
                ui.add(egui::TextEdit::singleline(&mut cfg.make_program).hint_text(format!("auto: {detected}")).desired_width(280.0));
                ui.end_row();
            });
            ui.add_space(4.0);
            ui.label(
                RichText::new(
                    "Run builds first, then runs the command in the Makefile's folder. \
                     If the run command is empty and the Makefile has a `run` target, Run uses `make run`. \
                     Debug starts the same program under the debugger; compile with -g to see your code.",
                )
                .weak()
                .small(),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Save").clicked() {
                    done = Some(true);
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                    done = Some(false);
                }
            });
        });
        match done {
            Some(true) => {
                if let Some(r) = self.project_root() {
                    self.build_configs.insert(r, cfg);
                }
            }
            Some(false) => {}
            None => self.modal = Some(Modal::BuildSettings(cfg)),
        }
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}
