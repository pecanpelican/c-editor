//! The Settings window, theme switching and the keyboard shortcut editor.
//! Changes are written into settings.json key by key, so comments and
//! anything else in the file stay as they were.

use egui::{Key, RichText};
use serde_json::{Value, json};

use super::{App, Modal, ShortcutEditor};
use crate::settings::{Action, Settings};
use crate::theme::Theme;

impl App {
    /// Write changed keys to settings.json and apply the new settings now.
    pub(super) fn save_settings(&mut self, ctx: &egui::Context, new: Settings, changes: &[(&str, Value)]) {
        for (key, value) in changes {
            if let Err(e) = crate::settings::write_setting(key, value) {
                self.toast(ctx, e, true);
                return;
            }
        }
        // Don't show "Settings reloaded" for our own write.
        self.settings_mtime = Settings::mtime();
        self.apply_settings(ctx, new, None);
    }

    pub(super) fn set_theme(&mut self, ctx: &egui::Context, name: &str) {
        let mut s = self.settings.clone();
        s.theme = name.to_string();
        self.save_settings(ctx, s, &[("theme", json!(name))]);
    }

    pub(super) fn open_shortcuts(&mut self) {
        self.modal = Some(Modal::Shortcuts(ShortcutEditor::default()));
    }

    fn preview_theme(&mut self, ctx: &egui::Context, name: &str) {
        self.theme = Theme::by_name(name, ctx.system_theme().map(|t| t == egui::Theme::Dark));
        self.theme.apply(ctx);
        for d in &mut self.docs {
            d.invalidate_layout();
        }
    }

    pub(super) fn preferences_modal(&mut self, ctx: &egui::Context, mut draft: Settings) {
        let mut save = false;
        let mut cancel = false;
        let mut open_json = false;
        let mut open_keys = false;
        let theme_before = draft.theme.clone();
        let mut clangd_args = draft.clangd_args.join(" ");
        egui::Modal::new(egui::Id::unique("preferences")).show(ctx, |ui| {
            ui.set_width(520.0);
            ui.heading("Settings");
            ui.add_space(6.0);
            let section = |ui: &mut egui::Ui, title: &str| {
                ui.add_space(6.0);
                ui.label(RichText::new(title).strong());
                ui.separator();
            };
            section(ui, "Appearance");
            egui::Grid::new("prefs-look").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
                ui.label("Theme");
                let current = Theme::NAMES
                    .iter()
                    .find(|(id, _)| id.eq_ignore_ascii_case(&draft.theme))
                    .map(|(_, n)| *n)
                    .unwrap_or("Dark");
                egui::ComboBox::from_id_salt("prefs-theme").selected_text(current).width(200.0).show_ui(ui, |ui| {
                    for (id, name) in Theme::NAMES {
                        ui.selectable_value(&mut draft.theme, id.to_string(), *name);
                    }
                });
                ui.end_row();
                ui.label("Editor font size");
                ui.add(egui::Slider::new(&mut draft.font_size, 8.0..=40.0).step_by(1.0));
                ui.end_row();
                ui.label("Editor font file");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut draft.font_path).hint_text("built-in").desired_width(260.0));
                    if ui.button("Browse…").clicked()
                        && let Some(p) = rfd::FileDialog::new().add_filter("Fonts", &["ttf", "otf", "ttc"]).pick_file()
                    {
                        draft.font_path = p.display().to_string();
                    }
                });
                ui.end_row();
            });
            section(ui, "Editing");
            egui::Grid::new("prefs-edit").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
                ui.label("Indent width");
                ui.add(egui::DragValue::new(&mut draft.tab_size).range(1..=16).suffix(" spaces"));
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut draft.auto_close_brackets, "Close brackets and quotes automatically");
                ui.end_row();
                ui.label("On save");
                ui.checkbox(&mut draft.trim_trailing_whitespace, "Trim trailing whitespace");
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut draft.ensure_final_newline, "End the file with a newline");
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut draft.format_on_save, "Format C files with clang-format");
                ui.end_row();
            });
            section(ui, "Tools (leave empty to find them automatically)");
            egui::Grid::new("prefs-tools").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
                let path_row = |ui: &mut egui::Ui, label: &str, value: &mut String, hint: &str| {
                    ui.label(label);
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(value).hint_text(hint).desired_width(260.0));
                        if ui.button("Browse…").clicked()
                            && let Some(p) = rfd::FileDialog::new().pick_file()
                        {
                            *value = p.display().to_string();
                        }
                    });
                    ui.end_row();
                };
                path_row(ui, "clangd", &mut draft.clangd_path, "clangd");
                ui.label("clangd arguments");
                ui.add(egui::TextEdit::singleline(&mut clangd_args).hint_text("--clang-tidy").desired_width(260.0));
                ui.end_row();
                path_row(ui, "glsl_analyzer", &mut draft.glsl_analyzer_path, "glsl_analyzer");
                path_row(ui, "Debugger", &mut draft.debugger_path, "lldb-dap or gdb");
            });
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Keyboard Shortcuts…").clicked() {
                    open_keys = true;
                }
                if ui.button("Edit settings.json").on_hover_text("All settings, as a file").clicked() {
                    open_json = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(RichText::new("Save").strong()).clicked() {
                        save = true;
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        cancel = true;
                    }
                });
            });
        });
        draft.clangd_args = clangd_args.split_whitespace().map(str::to_string).collect();
        if draft.theme != theme_before {
            self.preview_theme(ctx, &draft.theme);
        }
        if save || open_json || open_keys {
            let old = self.settings.clone();
            let mut changes: Vec<(&str, Value)> = Vec::new();
            macro_rules! diff {
                ($($f:ident),*) => {$(
                    if draft.$f != old.$f {
                        changes.push((stringify!($f), json!(draft.$f)));
                    }
                )*};
            }
            diff!(theme, font_size, font_path, tab_size, auto_close_brackets, trim_trailing_whitespace,
                ensure_final_newline, format_on_save, clangd_path, clangd_args, glsl_analyzer_path, debugger_path);
            if !changes.is_empty() {
                self.save_settings(ctx, draft, &changes);
            }
            if open_json {
                self.cmd_open_settings_json(ctx);
            } else if open_keys {
                self.open_shortcuts();
            }
        } else if cancel {
            let t = self.settings.theme.clone();
            self.preview_theme(ctx, &t);
        } else {
            self.modal = Some(Modal::Preferences(Box::new(draft)));
        }
    }

    /// The shortcut list. Click a shortcut, press keys to rebind it.
    pub(super) fn shortcuts_modal(&mut self, ctx: &egui::Context, mut ed: ShortcutEditor) {
        let mut close = false;
        let mut set: Option<(Action, String)> = None;
        let mut reset_all = false;

        // Capture the next key press for the action being rebound.
        if let Some(a) = ed.capturing
            && ed.conflict.is_none()
        {
            let pressed = ctx.input_mut(|i| {
                let mut hit = None;
                i.events.retain(|e| {
                    if hit.is_none()
                        && let egui::Event::Key { key, pressed: true, modifiers, .. } = e
                        && !is_modifier_key(*key)
                    {
                        hit = Some((*key, *modifiers));
                        return false;
                    }
                    true
                });
                hit
            });
            match pressed {
                Some((Key::Escape, m)) if m.is_none() => ed.capturing = None,
                Some((Key::Backspace | Key::Delete, m)) if m.is_none() => {
                    set = Some((a, "none".into()));
                    ed.capturing = None;
                }
                Some((key, mods)) => {
                    if let Some(s) = crate::settings::shortcut_string(mods, key) {
                        // Compare as shown, so Cmd+K and Ctrl+K count as the same on Windows and Linux.
                        let shown = crate::settings::parse_shortcut(&s).map(|sc| ctx.format_shortcut(&sc));
                        let other = Action::ALL
                            .iter()
                            .copied()
                            .find(|o| *o != a && shown.is_some() && self.keymap.shortcut(*o).map(|x| ctx.format_shortcut(&x)) == shown);
                        match other {
                            Some(o) => ed.conflict = Some((a, s, o)),
                            None => {
                                set = Some((a, s));
                                ed.capturing = None;
                            }
                        }
                    }
                }
                None => {}
            }
        }

        egui::Modal::new(egui::Id::unique("shortcuts")).show(ctx, |ui| {
            ui.set_width(560.0);
            ui.heading("Keyboard shortcuts");
            ui.label(RichText::new("Click a shortcut, then press the new keys. Backspace removes it, Esc cancels.").weak());
            ui.add_space(4.0);
            let r = ui.add(egui::TextEdit::singleline(&mut ed.filter).hint_text("Search commands or keys").desired_width(f32::INFINITY));
            if ed.capturing.is_none() && !r.has_focus() && ed.filter.is_empty() {
                r.request_focus();
            }
            if let Some((a, s, o)) = ed.conflict.clone() {
                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(format!("{s} is already used by \"{}\".", o.title())).color(self.theme.warning));
                    if ui.button("Use it for both").on_hover_text("The first one in the list wins").clicked() {
                        set = Some((a, s.clone()));
                        ed.conflict = None;
                        ed.capturing = None;
                    }
                    if ui.button(format!("Remove it from \"{}\"", o.title())).clicked() {
                        set = Some((a, s.clone()));
                        ed.unbind = Some(o);
                        ed.conflict = None;
                        ed.capturing = None;
                    }
                    if ui.button("Cancel").clicked() {
                        ed.conflict = None;
                        ed.capturing = None;
                    }
                });
            }
            ui.add_space(4.0);
            let q = ed.filter.to_lowercase();
            egui::ScrollArea::vertical().max_height(420.0).auto_shrink([false, false]).show(ui, |ui| {
                egui::Grid::new("sc-grid").num_columns(3).striped(true).spacing([16.0, 4.0]).show(ui, |ui| {
                    let mut last_group = "";
                    let mut actions: Vec<Action> = Action::ALL.to_vec();
                    actions.sort_by_key(|a| a.category());
                    for a in actions {
                        let label = self.keymap.label(ctx, a);
                        if !q.is_empty()
                            && !a.title().to_lowercase().contains(&q)
                            && !label.to_lowercase().contains(&q)
                            && !a.id().contains(&q)
                        {
                            continue;
                        }
                        if a.category() != last_group {
                            last_group = a.category();
                            ui.label(RichText::new(last_group).small().weak());
                            ui.end_row();
                        }
                        ui.label(a.title());
                        let capturing = ed.capturing == Some(a);
                        let text = if capturing {
                            RichText::new("press keys…").italics().color(self.theme.warning)
                        } else if label.is_empty() {
                            RichText::new("none").weak()
                        } else {
                            RichText::new(&label).monospace()
                        };
                        if ui.add(egui::Button::new(text).min_size(egui::vec2(140.0, 0.0))).on_hover_text("Click to change").clicked() {
                            ed.capturing = Some(a);
                            ed.conflict = None;
                        }
                        let custom = self.settings.keybindings.contains_key(a.id());
                        if custom {
                            if ui.small_button("Reset").on_hover_text(format!("Back to {}", a.default_shortcut())).clicked() {
                                set = Some((a, String::new()));
                            }
                        } else {
                            ui.label("");
                        }
                        ui.end_row();
                    }
                    let extra = [("Indent / unindent", "Tab / Shift+Tab"), ("Go to definition", "Cmd/Ctrl+click")];
                    for (d, k) in extra {
                        if q.is_empty() || d.to_lowercase().contains(&q) {
                            ui.label(d);
                            ui.label(RichText::new(k).monospace().weak());
                            ui.label("");
                            ui.end_row();
                        }
                    }
                });
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.add_enabled(!self.settings.keybindings.is_empty(), egui::Button::new("Reset All")).clicked() {
                    reset_all = true;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Close").clicked() || (ed.capturing.is_none() && ui.input(|i| i.key_pressed(Key::Escape))) {
                        close = true;
                    }
                });
            });
        });

        if set.is_some() || reset_all {
            let mut s = self.settings.clone();
            if reset_all {
                s.keybindings.clear();
            }
            if let Some((a, sc)) = set {
                if sc.is_empty() {
                    s.keybindings.remove(a.id());
                } else {
                    s.keybindings.insert(a.id().to_string(), sc);
                }
            }
            if let Some(o) = ed.unbind.take() {
                s.keybindings.insert(o.id().to_string(), "none".into());
            }
            let value = json!(s.keybindings);
            self.save_settings(ctx, s, &[("keybindings", value)]);
        }
        if !close {
            self.modal = Some(Modal::Shortcuts(ed));
        }
    }
}

/// Shift, Ctrl, Alt and Cmd on their own (egui reports them as key presses too).
fn is_modifier_key(key: Key) -> bool {
    let n = key.name();
    ["Shift", "Alt", "Ctrl", "Control", "Meta", "Super", "Cmd", "Command", "Option"].iter().any(|m| n.contains(m))
}
