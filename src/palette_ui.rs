//! The command palette (every command, searchable) and the Recent Files
//! switcher. Both are the same fuzzy-filtered list in a popup.

use std::path::PathBuf;

use egui::{Key, Modifiers, RichText};

use super::{App, BottomTab, Modal, TabSet};
use crate::settings::Action;

#[derive(Clone, Copy, PartialEq)]
pub(super) enum PaletteMode {
    Commands,
    RecentFiles,
}

pub(super) struct Palette {
    pub mode: PaletteMode,
    pub query: String,
    pub selected: usize,
    pub focus: bool,
}

#[derive(Clone)]
enum Cmd {
    Action(Action),
    Theme(&'static str),
    RestartServers,
    MarkdownPreview,
    CloseOthers,
    CloseSaved,
    CloseFolder,
    TrustFolder,
    Shortcuts,
    BuildSettings,
    Clean,
    OpenMakefile,
    OpenSettingsJson,
    ToggleLineEndings,
    CopyPath,
    CopyRelativePath,
    Reveal,
    ToggleSetting(&'static str),
    ShowPanel(BottomTab),
    Folder(PathBuf),
    Target(String),
    File(PathBuf),
    RunToCursor,
    PauseDebug,
    RestartDebug,
    EditBreakpoint,
    ClearBreakpoints,
}

struct Item {
    label: String,
    group: String,
    shortcut: String,
    cmd: Cmd,
}

impl App {
    pub(super) fn open_palette(&mut self) {
        self.palette = Some(Palette { mode: PaletteMode::Commands, query: String::new(), selected: 0, focus: true });
    }

    pub(super) fn open_recent_files(&mut self) {
        // Start on the previous file so Cmd+E, Enter flips between two files.
        let selected = usize::from(self.recent_files_list().len() > 1);
        self.palette = Some(Palette { mode: PaletteMode::RecentFiles, query: String::new(), selected, focus: true });
    }

    /// Remember a file for the Recent Files list (most recent first).
    pub(super) fn note_recent_file(&mut self, path: &std::path::Path) {
        self.recent_files.retain(|p| p != path);
        self.recent_files.insert(0, path.to_path_buf());
        self.recent_files.truncate(60);
    }

    fn recent_files_list(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        if let Some(p) = self.docs.get(self.active).and_then(|d| d.path.clone()) {
            out.push(p);
        }
        for p in &self.recent_files {
            if !out.contains(p) && p.is_file() {
                out.push(p.clone());
            }
        }
        out
    }

    fn palette_items(&self, ctx: &egui::Context) -> Vec<Item> {
        let mut items = Vec::new();
        let has_doc = !self.docs.is_empty();
        let has_root = self.project_root().is_some();
        let debugging = self.debug_active();
        for &a in Action::ALL {
            let available = match a {
                Action::Build => self.makefile.is_some(),
                Action::Run | Action::Debug | Action::Profile => has_root,
                Action::StopDebug | Action::StepOver | Action::StepInto | Action::StepOut => debugging,
                Action::GotoFile | Action::GotoSymbol | Action::FindInFiles => self.tree.is_some(),
                Action::CommandPalette => false,
                Action::Save
                | Action::SaveAs
                | Action::CloseTab
                | Action::Find
                | Action::Replace
                | Action::GotoLine
                | Action::GotoDefinition
                | Action::GotoDeclaration
                | Action::FindReferences
                | Action::Rename
                | Action::Format
                | Action::SwitchHeader
                | Action::QuickFix
                | Action::ToggleComment
                | Action::DuplicateLine
                | Action::DeleteLine
                | Action::MoveLineUp
                | Action::MoveLineDown
                | Action::ToggleBreakpoint => has_doc,
                _ => true,
            };
            if available {
                items.push(Item {
                    label: a.title().trim_end_matches('…').to_string(),
                    group: a.category().to_string(),
                    shortcut: self.keymap.label(ctx, a),
                    cmd: Cmd::Action(a),
                });
            }
        }
        let mut add = |group: &str, label: &str, cmd: Cmd| {
            items.push(Item { label: label.to_string(), group: group.to_string(), shortcut: String::new(), cmd });
        };
        for (id, name) in crate::theme::Theme::NAMES {
            let current = if self.settings.theme.eq_ignore_ascii_case(id) { "  (current)" } else { "" };
            add("Theme", &format!("{name}{current}"), Cmd::Theme(id));
        }
        add("View", "Keyboard Shortcuts", Cmd::Shortcuts);
        add("View", "Show Problems", Cmd::ShowPanel(BottomTab::Problems));
        add("View", "Show Build Output", Cmd::ShowPanel(BottomTab::Output));
        add("View", "Show Debugger", Cmd::ShowPanel(BottomTab::Debug));
        add("File", "Open settings.json", Cmd::OpenSettingsJson);
        add("Settings", if self.settings.auto_close_brackets { "Turn Off Auto-Close Brackets" } else { "Turn On Auto-Close Brackets" }, Cmd::ToggleSetting("auto_close_brackets"));
        add("Settings", if self.settings.format_on_save { "Turn Off Format on Save" } else { "Turn On Format on Save" }, Cmd::ToggleSetting("format_on_save"));
        add("Settings", if self.settings.trim_trailing_whitespace { "Stop Trimming Trailing Whitespace" } else { "Trim Trailing Whitespace on Save" }, Cmd::ToggleSetting("trim_trailing_whitespace"));
        add("Code", "Restart Language Servers", Cmd::RestartServers);
        if self.docs.len() > 1 {
            add("File", "Close Other Tabs", Cmd::CloseOthers);
        }
        if self.docs.iter().any(|d| !d.dirty) {
            add("File", "Close Saved Tabs", Cmd::CloseSaved);
        }
        if self.tree.is_some() {
            add("File", "Close Folder", Cmd::CloseFolder);
        }
        if has_root && !self.folder_trusted() {
            add("File", "Trust This Folder", Cmd::TrustFolder);
        }
        if let Some(d) = self.docs.get(self.active) {
            if d.is_markdown() {
                add("View", "Toggle Markdown Preview", Cmd::MarkdownPreview);
            }
            if d.viewer.is_none() {
                add("File", if d.crlf { "Use LF Line Endings" } else { "Use CRLF Line Endings" }, Cmd::ToggleLineEndings);
            }
            if d.path.is_some() {
                add("File", "Copy Path of Current File", Cmd::CopyPath);
                add("File", "Copy Relative Path of Current File", Cmd::CopyRelativePath);
                add("File", if cfg!(target_os = "macos") { "Reveal in Finder" } else { "Reveal in File Manager" }, Cmd::Reveal);
                add("Debug", "Edit Breakpoint at Cursor (Condition, Log Message)", Cmd::EditBreakpoint);
            }
        }
        if has_root {
            add("Build", "Build Settings", Cmd::BuildSettings);
        }
        if self.makefile.is_some() {
            add("Build", "Clean", Cmd::Clean);
            add("Build", "Open Makefile", Cmd::OpenMakefile);
            for t in &self.targets {
                add("Build Target", t, Cmd::Target(t.clone()));
            }
        }
        if debugging {
            add("Debug", "Run to Cursor", Cmd::RunToCursor);
            add("Debug", "Pause", Cmd::PauseDebug);
            add("Debug", "Restart Debugging", Cmd::RestartDebug);
        }
        if !self.dbg.breakpoints.is_empty() {
            add("Debug", "Remove All Breakpoints", Cmd::ClearBreakpoints);
        }
        let current = self.tree.as_ref().map(|t| t.root.clone());
        for f in &self.recent_folders {
            if Some(f) != current.as_ref() {
                add("Open Recent", &f.display().to_string(), Cmd::Folder(f.clone()));
            }
        }
        items
    }

    fn recent_file_items(&self) -> Vec<Item> {
        let root = self.tree.as_ref().map(|t| t.root.clone());
        self.recent_files_list()
            .into_iter()
            .map(|p| {
                let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let rel = root
                    .as_ref()
                    .and_then(|r| p.parent()?.strip_prefix(r).ok().map(|d| d.display().to_string()))
                    .unwrap_or_else(|| p.parent().map(|d| d.display().to_string()).unwrap_or_default());
                let open = self.docs.iter().any(|d| d.path.as_deref() == Some(p.as_path()));
                let dirty = self.docs.iter().any(|d| d.dirty && d.path.as_deref() == Some(p.as_path()));
                Item {
                    label: if dirty { format!("{name} •") } else { name },
                    group: rel,
                    shortcut: if open { "open".into() } else { String::new() },
                    cmd: Cmd::File(p),
                }
            })
            .collect()
    }

    pub(super) fn palette_ui(&mut self, ctx: &egui::Context) {
        let Some(mode) = self.palette.as_ref().map(|p| p.mode) else { return };
        let items = match mode {
            PaletteMode::Commands => self.palette_items(ctx),
            PaletteMode::RecentFiles => self.recent_file_items(),
        };
        let history = self.recent_commands.clone();
        let Some(pal) = &mut self.palette else { return };

        // Rank: fuzzy score, then recently used commands, then list order.
        let mut ranked: Vec<(i64, usize)> = items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| {
                let hay = match mode {
                    PaletteMode::Commands => format!("{} {}", it.label, it.group),
                    PaletteMode::RecentFiles => format!("{}/{}", it.group, it.label),
                };
                let mut score = crate::project::fuzzy_score(&hay, &pal.query)?;
                // Prefer matches in the label itself.
                if let Some(s) = crate::project::fuzzy_score(&it.label, &pal.query) {
                    score += s / 2 + 5;
                }
                if mode == PaletteMode::Commands
                    && let Some(h) = history.iter().position(|h| *h == it.label)
                {
                    score += if pal.query.is_empty() { 10_000 - h as i64 } else { 20 - h.min(19) as i64 };
                }
                Some((score, i))
            })
            .collect();
        if mode == PaletteMode::Commands || !pal.query.is_empty() {
            ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        }
        if !ranked.is_empty() {
            pal.selected = pal.selected.min(ranked.len() - 1);
        }

        let mut chosen = None;
        let mut close = false;
        let cmd_e = mode == PaletteMode::RecentFiles;
        ctx.input_mut(|i| {
            if i.consume_key(Modifiers::NONE, Key::ArrowDown) || (cmd_e && i.consume_key(Modifiers::COMMAND, Key::E)) {
                pal.selected = if ranked.is_empty() { 0 } else { (pal.selected + 1) % ranked.len() };
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                pal.selected = if ranked.is_empty() { 0 } else { (pal.selected + ranked.len() - 1) % ranked.len() };
            }
            if i.consume_key(Modifiers::NONE, Key::PageDown) {
                pal.selected = (pal.selected + 10).min(ranked.len().saturating_sub(1));
            }
            if i.consume_key(Modifiers::NONE, Key::PageUp) {
                pal.selected = pal.selected.saturating_sub(10);
            }
            if i.consume_key(Modifiers::NONE, Key::Enter) {
                chosen = ranked.get(pal.selected).map(|&(_, idx)| idx);
            }
            if i.consume_key(Modifiers::NONE, Key::Escape) {
                close = true;
            }
        });

        let theme = self.theme.clone();
        let hint = match mode {
            PaletteMode::Commands => "Type a command",
            PaletteMode::RecentFiles => "Recent files",
        };
        let area = egui::Area::new(egui::Id::unique("palette"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 60.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).inner_margin(8.0).show(ui, |ui| {
                    let w = (ctx.content_rect().width() - 80.0).clamp(320.0, 620.0);
                    ui.set_width(w);
                    let r = ui.add(egui::TextEdit::singleline(&mut pal.query).hint_text(hint).desired_width(f32::INFINITY));
                    if pal.focus {
                        r.request_focus();
                        pal.focus = false;
                    }
                    if r.changed() {
                        pal.selected = 0;
                    }
                    ui.add_space(4.0);
                    egui::ScrollArea::vertical().max_height(380.0).auto_shrink([false, true]).show(ui, |ui| {
                        for (row, &(_, idx)) in ranked.iter().enumerate() {
                            let it = &items[idx];
                            let mut job = egui::text::LayoutJob::default();
                            job.append(&it.label, 0.0, egui::TextFormat::simple(egui::FontId::proportional(14.0), theme.text));
                            if !it.group.is_empty() {
                                job.append(
                                    &format!("   {}", it.group),
                                    0.0,
                                    egui::TextFormat::simple(egui::FontId::proportional(12.0), theme.gutter_fg),
                                );
                            }
                            let selected = row == pal.selected;
                            let mut btn = egui::Button::new(job).frame(false).min_size(egui::vec2(ui.available_width(), 24.0));
                            if !it.shortcut.is_empty() {
                                btn = btn.shortcut_text(RichText::new(&it.shortcut).monospace().color(theme.gutter_fg));
                            }
                            let bg = ui.painter().add(egui::Shape::Noop);
                            let resp = ui.with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| ui.add(btn)).inner;
                            if selected {
                                ui.painter().set(bg, egui::Shape::rect_filled(resp.rect, 3.0, theme.selection));
                            }
                            if selected {
                                resp.scroll_to_me(None);
                            }
                            if resp.clicked() {
                                chosen = Some(idx);
                            }
                        }
                        if ranked.is_empty() {
                            ui.label(RichText::new(if mode == PaletteMode::Commands { "No matching commands" } else { "No recent files" }).weak());
                        }
                    });
                });
            });
        // Click outside closes it.
        if ctx.input(|i| i.pointer.any_pressed())
            && let Some(p) = ctx.input(|i| i.pointer.interact_pos())
            && !area.response.rect.contains(p)
        {
            close = true;
        }

        if let Some(idx) = chosen {
            let item = &items[idx];
            let cmd = item.cmd.clone();
            if mode == PaletteMode::Commands {
                self.recent_commands.retain(|h| *h != item.label);
                self.recent_commands.insert(0, item.label.clone());
                self.recent_commands.truncate(12);
            }
            self.palette = None;
            self.focus_active();
            self.run_palette_cmd(ctx, cmd);
        } else if close {
            self.palette = None;
            self.focus_active();
        }
    }

    fn run_palette_cmd(&mut self, ctx: &egui::Context, cmd: Cmd) {
        match cmd {
            Cmd::Action(a) => self.run_action(ctx, a),
            Cmd::Theme(name) => self.set_theme(ctx, name),
            Cmd::RestartServers => self.cmd_restart_servers(),
            Cmd::MarkdownPreview => {
                if let Some(d) = self.docs.get_mut(self.active) {
                    d.md_preview = !d.md_preview;
                }
            }
            Cmd::CloseOthers => self.request_close_many(self.tab_ids(TabSet::Others, self.active)),
            Cmd::CloseSaved => self.request_close_many(self.tab_ids(TabSet::Saved, 0)),
            Cmd::CloseFolder => self.tree = None,
            Cmd::TrustFolder => {
                if let Some(r) = self.project_root() {
                    self.modal = Some(Modal::TrustFolder(r));
                }
            }
            Cmd::Shortcuts => self.open_shortcuts(),
            Cmd::BuildSettings => self.modal = Some(Modal::BuildSettings(self.build_config())),
            Cmd::Clean => self.cmd_clean(ctx),
            Cmd::OpenMakefile => {
                if let Some(m) = self.makefile.clone() {
                    self.open_file(&m);
                }
            }
            Cmd::OpenSettingsJson => self.cmd_open_settings_json(ctx),
            Cmd::ToggleLineEndings => {
                if let Some(d) = self.docs.get_mut(self.active) {
                    d.crlf = !d.crlf;
                    d.dirty = true;
                }
            }
            Cmd::CopyPath | Cmd::CopyRelativePath => {
                if let Some(p) = self.docs.get(self.active).and_then(|d| d.path.clone()) {
                    let p = match (&cmd, &self.tree) {
                        (Cmd::CopyRelativePath, Some(t)) => p.strip_prefix(&t.root).map(|r| r.to_path_buf()).unwrap_or(p),
                        _ => p,
                    };
                    ctx.copy_text(p.display().to_string());
                    self.toast(ctx, "Path copied", false);
                }
            }
            Cmd::Reveal => {
                if let Some(p) = self.docs.get(self.active).and_then(|d| d.path.clone()) {
                    crate::project::reveal_in_file_manager(&p);
                }
            }
            Cmd::ToggleSetting(key) => {
                let mut s = self.settings.clone();
                let v = match key {
                    "auto_close_brackets" => {
                        s.auto_close_brackets = !s.auto_close_brackets;
                        s.auto_close_brackets
                    }
                    "format_on_save" => {
                        s.format_on_save = !s.format_on_save;
                        s.format_on_save
                    }
                    _ => {
                        s.trim_trailing_whitespace = !s.trim_trailing_whitespace;
                        s.trim_trailing_whitespace
                    }
                };
                self.save_settings(ctx, s, &[(key, serde_json::json!(v))]);
            }
            Cmd::ShowPanel(tab) => {
                self.bottom_open = true;
                self.bottom_tab = tab;
            }
            Cmd::Folder(f) => {
                if f.is_dir() {
                    self.open_folder(f);
                } else {
                    self.recent_folders.retain(|x| x != &f);
                    self.toast(ctx, format!("{} no longer exists", f.display()), true);
                }
            }
            Cmd::Target(t) => self.cmd_build(ctx, Some(t)),
            Cmd::File(p) => {
                self.open_file(&p);
                self.focus_active();
            }
            Cmd::RunToCursor => self.cmd_run_to_cursor(ctx),
            Cmd::PauseDebug => self.debug_step("pause"),
            Cmd::RestartDebug => self.cmd_restart_debugging(ctx),
            Cmd::EditBreakpoint => self.cmd_edit_breakpoint(),
            Cmd::ClearBreakpoints => self.cmd_clear_breakpoints(),
        }
    }
}
