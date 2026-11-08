//! Terminal tabs in the bottom panel, and running programs in them.

use std::path::PathBuf;

use egui::{Align, Layout, RichText, Ui};

use super::{App, BottomTab};
use crate::terminal::{self, Terminal};

#[derive(Default)]
pub(crate) struct TermState {
    list: Vec<Terminal>,
    active: usize,
    /// The terminal had keyboard focus last frame.
    pub focused: bool,
    focus_next: bool,
    shells_started: u32,
}

impl App {
    fn terminal_dir(&self) -> PathBuf {
        self.tree
            .as_ref()
            .map(|t| t.root.clone())
            .or_else(|| std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// Show the terminal panel and give it the keyboard, starting a shell if needed.
    pub(super) fn show_terminal(&mut self, ctx: &egui::Context) {
        self.bottom_open = true;
        self.bottom_tab = BottomTab::Terminal;
        if self.terms.list.is_empty() {
            self.new_terminal(ctx);
        }
        self.terms.focus_next = true;
    }

    pub(super) fn toggle_terminal(&mut self, ctx: &egui::Context) {
        if self.bottom_open && self.bottom_tab == BottomTab::Terminal && self.terms.focused {
            self.bottom_open = false;
            self.focus_active();
        } else {
            self.show_terminal(ctx);
        }
    }

    pub(super) fn new_terminal(&mut self, ctx: &egui::Context) {
        let dir = self.terminal_dir();
        self.terms.shells_started += 1;
        let title = format!("{} {}", terminal::shell_name(), self.terms.shells_started);
        match Terminal::spawn(title, None, &dir, &[], ctx) {
            Ok(t) => {
                self.terms.list.push(t);
                self.terms.active = self.terms.list.len() - 1;
                self.bottom_open = true;
                self.bottom_tab = BottomTab::Terminal;
                self.terms.focus_next = true;
            }
            Err(e) => self.toast(ctx, e, true),
        }
    }

    fn close_terminal(&mut self, i: usize) {
        if i < self.terms.list.len() {
            self.terms.list[i].stop();
            self.terms.list.remove(i);
            if self.terms.active >= self.terms.list.len() {
                self.terms.active = self.terms.list.len().saturating_sub(1);
            }
        }
    }

    /// Run `program args` in the "Run" terminal tab, replacing the last run.
    pub(super) fn run_in_terminal(&mut self, ctx: &egui::Context, program: String, args: Vec<String>, dir: PathBuf) {
        self.run_in_terminal_env(ctx, program, args, dir, &[]);
    }

    /// Like `run_in_terminal`, with extra environment variables. Returns the process id.
    pub(super) fn run_in_terminal_env(
        &mut self,
        ctx: &egui::Context,
        program: String,
        args: Vec<String>,
        dir: PathBuf,
        env: &[(String, String)],
    ) -> Option<u32> {
        let program = crate::make::find_in_path(&program)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or(program);
        let existing = self.terms.list.iter().position(|t| t.is_run);
        if let Some(i) = existing {
            self.terms.list[i].stop();
        }
        match Terminal::spawn("Run".into(), Some((program, args)), &dir, env, ctx) {
            Ok(t) => {
                let pid = t.pid();
                let i = match existing {
                    Some(i) => {
                        self.terms.list[i] = t;
                        i
                    }
                    None => {
                        self.terms.list.push(t);
                        self.terms.list.len() - 1
                    }
                };
                self.terms.active = i;
                self.bottom_open = true;
                self.bottom_tab = BottomTab::Terminal;
                self.terms.focus_next = true;
                pid
            }
            Err(e) => {
                self.toast(ctx, e, true);
                None
            }
        }
    }

    pub(super) fn run_terminal_running(&self) -> bool {
        self.terms.list.iter().any(|t| t.is_run && t.is_running())
    }

    pub(super) fn run_terminal_pid(&self) -> Option<u32> {
        self.terms.list.iter().find(|t| t.is_run).and_then(|t| t.pid())
    }

    pub(super) fn stop_run_terminal(&mut self) {
        for t in self.terms.list.iter_mut().filter(|t| t.is_run) {
            t.stop();
        }
    }

    /// Poll processes; close shell tabs whose shell has exited.
    pub(super) fn update_terminals(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        for t in &mut self.terms.list {
            t.poll(now, ctx);
        }
        let before = self.terms.list.len();
        let active_was_closed = self.terms.list.get(self.terms.active).is_some_and(|t| !t.is_run && t.finished());
        self.terms.list.retain(|t| t.is_run || !t.finished());
        if self.terms.list.len() != before {
            self.terms.active = self.terms.active.min(self.terms.list.len().saturating_sub(1));
            if active_was_closed && self.terms.list.is_empty() && self.bottom_tab == BottomTab::Terminal {
                self.bottom_open = false;
                self.focus_active();
            }
        }
    }

    /// Terminal tabs, shown in the bottom panel's header.
    pub(super) fn terminal_tabs(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        ui.separator();
        let mut close = None;
        for i in 0..self.terms.list.len() {
            let t = &self.terms.list[i];
            let mut label = RichText::new(&t.title);
            if !t.is_running() {
                label = label.weak();
            }
            if ui.selectable_label(i == self.terms.active, label).clicked() {
                self.terms.active = i;
                self.terms.focus_next = true;
            }
            if ui.small_button("×").on_hover_text("Close terminal").clicked() {
                close = Some(i);
            }
        }
        if let Some(i) = close {
            self.close_terminal(i);
        }
        let new = ui.small_button("+").on_hover_text(format!(
            "New terminal ({})",
            self.keymap.label(&ctx, crate::settings::Action::NewTerminal)
        ));
        if new.clicked() {
            self.new_terminal(&ctx);
        }
    }

    pub(super) fn terminal_panel(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        if self.terms.list.is_empty() {
            ui.with_layout(Layout::top_down(Align::Center), |ui| {
                ui.add_space(20.0);
                if ui.button("Start a terminal").clicked() {
                    self.new_terminal(&ctx);
                }
            });
            return;
        }
        let focus = std::mem::take(&mut self.terms.focus_next);
        let theme = self.theme.clone();
        let size = (self.font_size - 1.0).max(8.0);
        let active = self.terms.active.min(self.terms.list.len() - 1);
        let out = self.terms.list[active].ui(ui, &theme, size, focus);
        self.terms.focused = out.focused;
        if let Some((path, line, col)) = out.open {
            let p = PathBuf::from(&path);
            let p = if p.is_absolute() { p } else { self.terms.list[active].dir.join(p) };
            if p.is_file() {
                self.open_file(&p);
                if let Some(doc) = self.docs.get_mut(self.active)
                    && doc.path.as_deref() == Some(p.as_path())
                {
                    doc.goto_line_col(line - 1, col - 1);
                }
            }
        }
    }
}
