use std::path::{Path, PathBuf};

use egui::{Align, Key, KeyboardShortcut, Layout, Modifiers, RichText, Ui};
use serde::{Deserialize, Serialize};

use crate::editor::{self, Document};
use crate::find::FindState;
use crate::make::{BuildConfig, Diagnostic, Task};
use crate::project::{self, FileTree, SearchJob, TreeAction};
use crate::settings::{Keymap, Settings};
use crate::theme::Theme;

const DEFAULT_FONT_SIZE: f32 = 14.0;

#[derive(Serialize, Deserialize, Default)]
struct Session {
    root: Option<PathBuf>,
    open_files: Vec<PathBuf>,
    active: usize,
    font_size: Option<f32>,
    sidebar: Option<bool>,
    recent_folders: Vec<PathBuf>,
    #[serde(default)]
    build_configs: std::collections::HashMap<PathBuf, BuildConfig>,
    #[serde(default)]
    breakpoints: std::collections::BTreeMap<PathBuf, Vec<usize>>,
    #[serde(default)]
    watches: Vec<String>,
    /// Folders the user said may run their Makefile and debugger setup files.
    #[serde(default)]
    trusted_folders: Vec<PathBuf>,
}

#[derive(Clone, Copy)]
enum TabSet {
    All,
    Others,
    Right,
    Saved,
}

enum NameKind {
    NewFile,
    NewFolder,
    Rename,
}

enum Modal {
    /// Closing a tab with unsaved changes.
    UnsavedClose(u64),
    /// Closing several tabs, some with unsaved changes.
    UnsavedCloseMany(Vec<u64>),
    /// Quitting with unsaved changes.
    UnsavedQuit,
    Name {
        kind: NameKind,
        /// Directory for new entries, or the path being renamed.
        target: PathBuf,
        text: String,
        focus: bool,
    },
    ConfirmTrash(PathBuf),
    /// Ask before running anything from a folder (see `folder_trusted`).
    TrustFolder(PathBuf),
    Shortcuts,
    BuildSettings(BuildConfig),
    RenameSymbol { text: String, focus: bool },
}

struct QuickOpen {
    query: String,
    files: Vec<PathBuf>,
    selected: usize,
    focus: bool,
}

#[derive(Default)]
struct FindInFiles {
    query: String,
    case_sensitive: bool,
    whole_word: bool,
    focus: bool,
    job: Option<SearchJob>,
}

#[derive(PartialEq, Clone, Copy)]
pub(crate) enum BottomTab {
    Output,
    Problems,
    Search,
    References,
    Terminal,
    Debug,
    Profile,
}

struct Toast {
    text: String,
    until: f64,
    error: bool,
}

pub struct App {
    theme: Theme,
    font_size: f32,
    docs: Vec<Document>,
    active: usize,
    tree: Option<FileTree>,
    sidebar: bool,
    find: FindState,
    quick_open: Option<QuickOpen>,
    goto_line: Option<(String, bool)>,
    fif: FindInFiles,
    bottom_open: bool,
    bottom_tab: BottomTab,
    modal: Option<Modal>,
    toasts: Vec<Toast>,
    quit_confirmed: bool,
    last_disk_check: f64,
    was_focused: bool,
    title: String,
    recent_folders: Vec<PathBuf>,
    // Make integration (see build_ui.rs)
    build_configs: std::collections::HashMap<PathBuf, BuildConfig>,
    task: Option<Task>,
    diagnostics: Vec<Diagnostic>,
    diag_cursor: Option<usize>,
    makefile: Option<PathBuf>,
    makefile_mtime: Option<std::time::SystemTime>,
    targets: Vec<String>,
    makefile_key: Option<(PathBuf, Option<PathBuf>)>,
    run_after_build: bool,
    lsp: lsp_ui::LspState,
    last_cursor_rect: Option<egui::Rect>,
    settings: Settings,
    keymap: Keymap,
    settings_mtime: Option<std::time::SystemTime>,
    fonts_loaded: Option<String>,
    /// Files waiting for clang-format before saving: path -> time requested.
    save_after_format: std::collections::HashMap<PathBuf, f64>,
    terms: terminal_ui::TermState,
    dbg: debug_ui::DebugState,
    debug_after_build: bool,
    prof: profile_ui::ProfState,
    profile_after_build: bool,
    trusted_folders: Vec<PathBuf>,
    /// Folders the user answered "Not Now" for in this run.
    trust_declined: std::collections::HashSet<PathBuf>,
}

#[path = "build_ui.rs"]
mod build_ui;
#[path = "lsp_ui.rs"]
mod lsp_ui;
#[path = "terminal_ui.rs"]
mod terminal_ui;
#[path = "debug_ui.rs"]
mod debug_ui;
#[path = "profile_ui.rs"]
mod profile_ui;

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, args: Vec<PathBuf>) -> Self {
        let theme = Theme::dark();
        theme.apply(&cc.egui_ctx);
        cc.egui_ctx.global_style_mut(|s| {
            s.spacing.item_spacing = egui::vec2(6.0, 3.0);
            s.spacing.button_padding = egui::vec2(6.0, 2.0);
        });

        let (settings, settings_error) = Settings::load();
        let session: Session = cc
            .storage
            .and_then(|s| eframe::get_value(s, "session"))
            .unwrap_or_default();

        let mut app = Self {
            theme,
            font_size: settings.font_size,
            docs: Vec::new(),
            active: 0,
            tree: None,
            sidebar: session.sidebar.unwrap_or(true),
            find: FindState::default(),
            quick_open: None,
            goto_line: None,
            fif: FindInFiles::default(),
            bottom_open: false,
            bottom_tab: BottomTab::Output,
            modal: None,
            toasts: Vec::new(),
            quit_confirmed: false,
            last_disk_check: 0.0,
            was_focused: true,
            title: String::new(),
            recent_folders: session.recent_folders.clone(),
            build_configs: session.build_configs.clone(),
            task: None,
            diagnostics: Vec::new(),
            diag_cursor: None,
            makefile: None,
            makefile_mtime: None,
            targets: Vec::new(),
            makefile_key: None,
            run_after_build: false,
            lsp: Default::default(),
            last_cursor_rect: None,
            settings: settings.clone(),
            keymap: Keymap::new(&settings.keybindings).0,
            settings_mtime: Settings::mtime(),
            fonts_loaded: None,
            save_after_format: Default::default(),
            terms: Default::default(),
            dbg: debug_ui::DebugState::new(session.breakpoints.clone(), session.watches.clone()),
            debug_after_build: false,
            prof: Default::default(),
            profile_after_build: false,
            trusted_folders: session.trusted_folders.clone(),
            trust_declined: Default::default(),
        };
        app.apply_settings(&cc.egui_ctx, settings, settings_error);
        app.font_size = session.font_size.unwrap_or(app.settings.font_size);

        if args.is_empty() {
            if let Some(root) = session.root.filter(|r| r.is_dir()) {
                app.tree = Some(FileTree::new(root));
            }
            for f in &session.open_files {
                if f.is_file() {
                    app.open_file_quiet(f);
                }
            }
            app.active = session.active.min(app.docs.len().saturating_sub(1));
        } else {
            for a in args {
                let a = a.canonicalize().unwrap_or(a);
                if a.is_dir() {
                    app.open_folder(a);
                } else {
                    if app.tree.is_none()
                        && let Some(parent) = a.parent()
                    {
                        app.open_folder(parent.to_path_buf());
                    }
                    app.open_file(&a);
                }
            }
        }
        app
    }

    // ----- documents -------------------------------------------------------

    fn toast(&mut self, ctx: &egui::Context, text: impl Into<String>, error: bool) {
        let now = ctx.input(|i| i.time);
        self.toasts.push(Toast {
            text: text.into(),
            until: now + if error { 6.0 } else { 2.5 },
            error,
        });
    }

    fn active_doc(&mut self) -> Option<&mut Document> {
        self.docs.get_mut(self.active)
    }

    fn open_file_quiet(&mut self, path: &Path) -> bool {
        if let Some(i) = self.docs.iter().position(|d| d.path.as_deref() == Some(path)) {
            self.active = i;
            return true;
        }
        match Document::open(path) {
            Ok(doc) => {
                // Replace a pristine untitled tab instead of piling up tabs.
                if let Some(d) = self.docs.get(self.active)
                    && d.path.is_none()
                    && !d.dirty
                    && d.text.is_empty()
                {
                    self.docs[self.active] = doc;
                } else {
                    self.docs.push(doc);
                    self.active = self.docs.len() - 1;
                }
                true
            }
            Err(_) => false,
        }
    }

    fn open_file(&mut self, path: &Path) {
        if let Some(i) = self.docs.iter().position(|d| d.path.as_deref() == Some(path)) {
            self.active = i;
            return;
        }
        match Document::open(path) {
            Ok(_) => {
                self.open_file_quiet(path);
                if let Some(t) = &mut self.tree {
                    t.expand_to(path);
                }
            }
            Err(e) => {
                let msg = format!("Can't open {}: {e}", path.display());
                self.toasts.push(Toast {
                    text: msg,
                    until: f64::MAX,
                    error: true,
                });
            }
        }
    }

    /// May cedit run things from the open folder on its own: `make -n` for
    /// compiler flags, and the folder's .lldbinit/.gdbinit when debugging?
    /// A Makefile or debugger file from someone else can run any command, so
    /// this needs the user's OK once per folder (trusting a folder trusts
    /// everything inside it). Build, Run and Debug still work either way,
    /// because the user asked for those.
    pub(super) fn folder_trusted(&self) -> bool {
        self.project_root().is_some_and(|r| self.trusted_folders.iter().any(|t| r.starts_with(t)))
    }

    /// Show the trust prompt the first time a folder with something to run is opened.
    fn ask_trust_if_needed(&mut self) {
        if self.modal.is_some() || self.quick_open.is_some() || self.folder_trusted() {
            return;
        }
        let Some(root) = self.project_root() else { return };
        if self.trust_declined.contains(&root) {
            return;
        }
        let has_runnable =
            self.makefile.is_some() || root.join(".lldbinit").is_file() || root.join(".gdbinit").is_file();
        if has_runnable {
            self.modal = Some(Modal::TrustFolder(root));
        }
    }

    fn open_folder(&mut self, root: PathBuf) {
        self.recent_folders.retain(|r| r != &root);
        self.recent_folders.insert(0, root.clone());
        self.recent_folders.truncate(10);
        self.tree = Some(FileTree::new(root));
        self.sidebar = true;
    }

    fn save_doc(&mut self, ctx: &egui::Context, idx: usize) -> bool {
        let Some(doc) = self.docs.get(idx) else { return false };
        if doc.path.is_none() {
            return self.save_doc_as(ctx, idx);
        }
        // clang-format first, then save when the edits come back.
        let path = doc.path.clone().unwrap_or_default();
        if self.settings.format_on_save
            && !self.save_after_format.contains_key(&path)
            && crate::lsp::ServerKind::for_path(&path) == Some(crate::lsp::ServerKind::Clangd)
            && self.lsp.server_for(&path).is_some_and(|s| s.ready && s.supports_formatting)
        {
            let (text, version) = (doc.text.clone(), doc.version);
            if let Some(s) = self.lsp.server_for(&path) {
                s.sync(&path, &text, version);
                s.format(&path, self.settings.tab_size as u32, true);
            }
            let now = ctx.input(|i| i.time);
            self.save_after_format.insert(path, now);
            return true;
        }
        self.save_doc_now(ctx, idx)
    }

    fn save_doc_now(&mut self, ctx: &egui::Context, idx: usize) -> bool {
        let trim = self.settings.trim_trailing_whitespace;
        let final_nl = self.settings.ensure_final_newline;
        let Some(doc) = self.docs.get_mut(idx) else { return false };
        if trim || final_nl {
            let cleaned = crate::textops::clean_for_save(&doc.text, trim, final_nl);
            if cleaned != doc.text {
                let (l, c) = doc.cursor_line_col;
                doc.text = cleaned;
                doc.mark_changed();
                let b = crate::textops::line_col_to_byte(&doc.text, l, c);
                doc.pending_select = Some(b..b);
            }
        }
        match doc.save() {
            Ok(()) => {
                if let Some(p) = doc.path.clone()
                    && let Some(s) = self.lsp.server_for(&p)
                {
                    s.did_save(&p);
                }
                true
            }
            Err(e) => {
                let msg = format!("Save failed: {e}");
                self.toast(ctx, msg, true);
                false
            }
        }
    }

    fn save_doc_as(&mut self, ctx: &egui::Context, idx: usize) -> bool {
        let Some(doc) = self.docs.get(idx) else { return false };
        let mut dialog = rfd::FileDialog::new().set_file_name(doc.title());
        if let Some(dir) = doc
            .path
            .as_ref()
            .and_then(|p| p.parent())
            .map(Path::to_path_buf)
            .or_else(|| self.tree.as_ref().map(|t| t.root.clone()))
        {
            dialog = dialog.set_directory(dir);
        }
        let Some(path) = dialog.save_file() else { return false };
        let doc = &mut self.docs[idx];
        match doc.save_as(path) {
            Ok(()) => {
                if let Some(t) = &mut self.tree {
                    t.refresh();
                }
                true
            }
            Err(e) => {
                self.toast(ctx, format!("Save failed: {e}"), true);
                false
            }
        }
    }

    /// Save every dirty file right away (no format-on-save), e.g. before a build.
    fn save_all_now(&mut self, ctx: &egui::Context) {
        for i in 0..self.docs.len() {
            if self.docs[i].dirty {
                if self.docs[i].path.is_some() {
                    self.save_doc_now(ctx, i);
                }
            }
        }
    }

    fn save_all(&mut self, ctx: &egui::Context) {
        for i in 0..self.docs.len() {
            if self.docs[i].dirty {
                self.save_doc(ctx, i);
            }
        }
    }

    fn request_close(&mut self, idx: usize) {
        if let Some(doc) = self.docs.get(idx) {
            if doc.dirty {
                self.modal = Some(Modal::UnsavedClose(doc.id));
            } else {
                self.close_now(idx);
            }
        }
    }

    /// Close several tabs at once, asking once about any unsaved ones.
    fn request_close_many(&mut self, ids: Vec<u64>) {
        if ids.is_empty() {
            return;
        }
        let any_dirty = ids.iter().filter_map(|id| self.doc_index(*id)).any(|i| self.docs[i].dirty);
        if any_dirty {
            self.modal = Some(Modal::UnsavedCloseMany(ids));
        } else {
            self.close_ids(&ids);
        }
    }

    fn close_ids(&mut self, ids: &[u64]) {
        let keep_active = self.docs.get(self.active).map(|d| d.id).filter(|id| !ids.contains(id));
        self.docs.retain(|d| !ids.contains(&d.id));
        self.active = keep_active.and_then(|id| self.doc_index(id)).unwrap_or(0).min(self.docs.len().saturating_sub(1));
        self.focus_active();
    }

    /// Ids for "Close All", "Close Others" (keep `keep`), "Close to the Right" and "Close Saved".
    fn tab_ids(&self, which: TabSet, at: usize) -> Vec<u64> {
        self.docs
            .iter()
            .enumerate()
            .filter(|(i, d)| match which {
                TabSet::All => true,
                TabSet::Others => *i != at,
                TabSet::Right => *i > at,
                TabSet::Saved => !d.dirty,
            })
            .map(|(_, d)| d.id)
            .collect()
    }

    fn close_now(&mut self, idx: usize) {
        if idx < self.docs.len() {
            self.docs.remove(idx);
            if self.active >= self.docs.len() {
                self.active = self.docs.len().saturating_sub(1);
            } else if idx < self.active {
                self.active -= 1;
            }
        }
    }

    fn doc_index(&self, id: u64) -> Option<usize> {
        self.docs.iter().position(|d| d.id == id)
    }

    // ----- commands --------------------------------------------------------

    fn cmd_open_file(&mut self) {
        let mut d = rfd::FileDialog::new();
        if let Some(t) = &self.tree {
            d = d.set_directory(&t.root);
        }
        if let Some(paths) = d.pick_files() {
            for p in paths {
                self.open_file(&p);
            }
        }
    }

    fn cmd_open_folder(&mut self) {
        if let Some(p) = rfd::FileDialog::new().pick_folder() {
            self.open_folder(p);
        }
    }

    fn cmd_new_file(&mut self) {
        self.docs.push(Document::new_untitled());
        self.active = self.docs.len() - 1;
        let d = &mut self.docs[self.active];
        d.pending_select = Some(0..0);
    }

    fn cmd_find(&mut self, ctx: &egui::Context, replace: bool) {
        self.find.open = true;
        self.find.replace_open = replace;
        self.find.focus_query = true;
        if let Some(doc) = self.docs.get(self.active) {
            let sel = doc.selected_text(ctx);
            if !sel.is_empty() && !sel.contains('\n') {
                self.find.query = sel;
            }
        }
    }

    fn cmd_find_in_files(&mut self, ctx: &egui::Context) {
        self.bottom_open = true;
        self.bottom_tab = BottomTab::Search;
        self.fif.focus = true;
        if let Some(doc) = self.docs.get(self.active) {
            let sel = doc.selected_text(ctx);
            if !sel.is_empty() && !sel.contains('\n') {
                self.fif.query = sel;
            }
        }
    }

    fn cmd_quick_open(&mut self) {
        let Some(tree) = &self.tree else { return };
        self.quick_open = Some(QuickOpen {
            query: String::new(),
            files: project::list_files(&tree.root),
            selected: 0,
            focus: true,
        });
    }

    fn find_step(&mut self, ctx: &egui::Context, forward: bool) {
        self.find_step_ex(ctx, forward, true);
    }

    fn find_step_ex(&mut self, ctx: &egui::Context, forward: bool, focus_editor: bool) {
        let Some(doc) = self.docs.get_mut(self.active) else { return };
        self.find.update(&doc.text);
        let sel = doc.selection(ctx);
        let idx = if forward {
            self.find.next_from(if sel.is_empty() { sel.start } else { sel.start + 1 })
        } else {
            self.find.prev_from(sel.start)
        };
        if let Some(i) = idx {
            self.find.current = Some(i);
            doc.pending_select = Some(self.find.matches[i].clone());
            doc.pending_focus = focus_editor;
        }
    }

    fn replace_one(&mut self, ctx: &egui::Context) {
        let Some(doc) = self.docs.get_mut(self.active) else { return };
        self.find.update(&doc.text);
        let sel = doc.selection(ctx);
        // If the selection is exactly a match, replace it; then move on.
        if let Some(i) = self.find.matches.iter().position(|m| *m == sel) {
            let m = self.find.matches[i].clone();
            let rep = self.find.replacement.clone();
            doc.replace_range(m, &rep);
            self.find.update(&doc.text);
            let after = sel.start + rep.len();
            if let Some(n) = self.find.next_from(after) {
                self.find.current = Some(n);
                doc.pending_select = Some(self.find.matches[n].clone());
            }
            doc.pending_focus = false;
        } else {
            self.find_step_ex(ctx, true, false);
        }
    }

    fn replace_all(&mut self, ctx: &egui::Context) {
        let Some(doc) = self.docs.get_mut(self.active) else { return };
        self.find.update(&doc.text);
        let n = self.find.matches.len();
        if n == 0 {
            return;
        }
        let mut out = String::with_capacity(doc.text.len());
        let mut last = 0;
        for m in &self.find.matches {
            out.push_str(&doc.text[last..m.start]);
            out.push_str(&self.find.replacement);
            last = m.end;
        }
        out.push_str(&doc.text[last..]);
        doc.text = out;
        doc.mark_changed();
        self.toast(ctx, format!("Replaced {n} occurrence{}", if n == 1 { "" } else { "s" }), false);
    }

    fn zoom(&mut self, delta: f32) {
        self.font_size = (self.font_size + delta).clamp(8.0, 40.0);
    }

    // ----- input -----------------------------------------------------------

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        use crate::settings::Action as A;
        for action in self.keymap.pressed(ctx, false, self.terms.focused) {
            match action {
                A::Save => {
                    self.save_doc(ctx, self.active);
                }
                A::SaveAs => {
                    self.save_doc_as(ctx, self.active);
                }
                A::SaveAll => self.save_all(ctx),
                A::NewFile => self.cmd_new_file(),
                A::OpenFile => self.cmd_open_file(),
                A::OpenFolder => self.cmd_open_folder(),
                A::CloseTab => self.request_close(self.active),
                A::CloseAllTabs => self.request_close_many(self.tab_ids(TabSet::All, 0)),
                A::Find => self.cmd_find(ctx, false),
                A::Replace => self.cmd_find(ctx, true),
                A::FindInFiles => self.cmd_find_in_files(ctx),
                A::FindNext => {
                    if self.find.query.is_empty() {
                        self.cmd_find(ctx, false);
                    } else {
                        self.find_step(ctx, true);
                    }
                }
                A::FindPrev => self.find_step(ctx, false),
                A::GotoLine => {
                    if !self.docs.is_empty() {
                        self.goto_line = Some((String::new(), true));
                    }
                }
                A::GotoFile => self.cmd_quick_open(),
                A::ToggleSidebar => self.sidebar = !self.sidebar,
                A::ToggleBottomPanel => self.bottom_open = !self.bottom_open,
                A::ZoomIn => self.zoom(1.0),
                A::ZoomOut => self.zoom(-1.0),
                A::ZoomReset => self.font_size = self.settings.font_size,
                A::NextTab | A::PrevTab => {
                    if !self.docs.is_empty() {
                        let n = self.docs.len();
                        self.active = if action == A::NextTab { (self.active + 1) % n } else { (self.active + n - 1) % n };
                        self.focus_active();
                    }
                }
                A::Build => self.cmd_build(ctx, None),
                A::Run => self.cmd_run(ctx),
                A::Stop => self.cmd_stop(),
                A::NextProblem => self.goto_diagnostic(true),
                A::PrevProblem => self.goto_diagnostic(false),
                A::GotoDefinition => self.cmd_definition_at(ctx, None),
                A::GotoDeclaration => self.cmd_declaration(ctx),
                A::GotoSymbol => self.cmd_goto_symbol(ctx),
                A::QuickFix => self.cmd_quick_fix(ctx),
                A::FindReferences => self.cmd_references(ctx),
                A::Rename => self.cmd_rename(ctx),
                A::Format => self.cmd_format(ctx),
                A::SwitchHeader => self.cmd_switch_header(ctx),
                A::Back => self.nav_back(),
                A::Complete => {
                    if let Some(d) = self.docs.get(self.active) {
                        let c = d.cursor_byte;
                        self.on_typed(ctx, "", c, true);
                    }
                }
                A::Settings => self.cmd_open_settings(ctx),
                A::ToggleTerminal => self.toggle_terminal(ctx),
                A::NewTerminal => self.new_terminal(ctx),
                A::Debug => self.cmd_debug(ctx),
                A::StopDebug => self.cmd_stop_debugging(ctx),
                A::ToggleBreakpoint => self.cmd_toggle_breakpoint(),
                A::StepOver => self.debug_step("next"),
                A::StepInto => self.debug_step("stepIn"),
                A::StepOut => self.debug_step("stepOut"),
                A::Profile => self.cmd_profile(ctx),
                A::ToggleComment | A::DuplicateLine | A::DeleteLine => {}
            }
        }
        // Extra fixed aliases.
        let cmd = Modifiers::COMMAND;
        if ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(cmd | Modifiers::SHIFT, Key::G))) {
            self.find_step(ctx, false);
        }
        if ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(cmd, Key::G))) && !self.find.query.is_empty() {
            self.find_step(ctx, true);
        }
        if ctx.input(|i| i.key_pressed(Key::Escape)) && self.find.open && self.modal.is_none() && !self.terms.focused {
            self.find.open = false;
            self.focus_active();
        }
    }

    fn cmd_open_settings(&mut self, ctx: &egui::Context) {
        let _ = Settings::load(); // creates the file with defaults if missing
        match crate::settings::settings_path() {
            Some(p) if p.is_file() => {
                self.open_file(&p);
                self.toast(ctx, "Save settings.json to apply changes", false);
            }
            _ => self.toast(ctx, "Couldn't find a place for settings.json", true),
        }
    }

    /// Apply loaded settings (theme, font, tab size, keymap).
    fn apply_settings(&mut self, ctx: &egui::Context, s: Settings, error: Option<String>) {
        let font_changed = s.font_path != self.settings.font_path || self.fonts_loaded.is_none();
        self.theme = if s.theme.eq_ignore_ascii_case("light") { Theme::light() } else { Theme::dark() };
        self.theme.apply(ctx);
        if (s.font_size - self.settings.font_size).abs() > 0.01 {
            self.font_size = s.font_size;
        }
        crate::textops::set_indent_width(s.tab_size);
        let (keymap, warnings) = crate::settings::Keymap::new(&s.keybindings);
        self.keymap = keymap;
        for d in &mut self.docs {
            d.invalidate_layout();
        }
        if font_changed {
            self.fonts_loaded = Some(s.font_path.clone());
            let mut fonts = egui::FontDefinitions::default();
            if !s.font_path.trim().is_empty() {
                match std::fs::read(s.font_path.trim()) {
                    Ok(bytes) => {
                        fonts.font_data.insert("user-mono".into(), std::sync::Arc::new(egui::FontData::from_owned(bytes)));
                        if let Some(list) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
                            list.insert(0, "user-mono".into());
                        }
                    }
                    Err(e) => self.toast(ctx, format!("Couldn't load font {}: {e}", s.font_path), true),
                }
            }
            ctx.set_fonts(fonts);
        }
        self.settings = s;
        if let Some(e) = error {
            self.toast(ctx, e, true);
        }
        for w in warnings {
            self.toast(ctx, w, true);
        }
    }

    fn focus_active(&mut self) {
        if let Some(d) = self.docs.get_mut(self.active) {
            d.focus();
        }
    }
}

impl eframe::App for App {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let session = Session {
            root: self.tree.as_ref().map(|t| t.root.clone()),
            open_files: self.docs.iter().filter_map(|d| d.path.clone()).collect(),
            active: self.active,
            font_size: Some(self.font_size),
            sidebar: Some(self.sidebar),
            recent_folders: self.recent_folders.clone(),
            build_configs: self.build_configs.clone(),
            breakpoints: self.dbg.breakpoints.clone(),
            watches: self.dbg.watches.clone(),
            trusted_folders: self.trusted_folders.clone(),
        };
        eframe::set_value(storage, "session", &session);
    }

    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();

        // Unsaved changes on quit.
        if ctx.input(|i| i.viewport().close_requested())
            && !self.quit_confirmed
            && self.docs.iter().any(|d| d.dirty)
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.modal = Some(Modal::UnsavedQuit);
        }

        // Dropped files and folders.
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
        for p in dropped {
            if p.is_dir() {
                self.open_folder(p);
            } else {
                self.open_file(&p);
            }
        }

        // Watch for external changes: on focus gain and every 2 seconds.
        let (now, focused) = ctx.input(|i| (i.time, i.focused));
        if (focused && !self.was_focused) || now - self.last_disk_check > 2.0 {
            self.last_disk_check = now;
            if focused && !self.was_focused
                && let Some(t) = &mut self.tree
            {
                t.refresh();
            }
            for d in &mut self.docs {
                d.check_disk();
            }
            let m = Settings::mtime();
            if m != self.settings_mtime {
                self.settings_mtime = m;
                let (s, err) = Settings::load();
                self.apply_settings(&ctx, s, err);
                self.toast(&ctx, "Settings reloaded", false);
            }
        }
        self.was_focused = focused;
        ctx.request_repaint_after(std::time::Duration::from_secs(2));

        self.update_build_state(&ctx);
        self.ask_trust_if_needed();
        self.update_lsp(&ctx);
        self.update_terminals(&ctx);
        self.update_debug(&ctx);
        self.update_profile(&ctx);
        if self.modal.is_none() && self.quick_open.is_none() {
            self.handle_shortcuts(&ctx);
        }

        self.update_title(&ctx);
        self.terms.focused = false; // set again below if the terminal is drawn with focus

        egui::Panel::top("menu").show(ui, |ui| self.menu_bar(ui));
        egui::Panel::bottom("status")
            .exact_size(22.0)
            .show(ui, |ui| self.status_bar(ui));
        if self.bottom_open {
            egui::Panel::bottom("bottom")
                .resizable(true)
                .default_size(220.0)
                .min_size(80.0)
                .show(ui, |ui| self.bottom_panel(ui));
        }
        if self.sidebar && self.tree.is_some() {
            egui::Panel::left("tree")
                .resizable(true)
                .default_size(240.0)
                .min_size(120.0)
                .show(ui, |ui| self.sidebar_ui(ui));
        }
        egui::CentralPanel::default()
            .frame(egui::Frame::NONE.fill(self.theme.bg))
            .show(ui, |ui| self.central(ui));

        self.completion_ui(&ctx, self.last_cursor_rect);
        self.signature_ui(&ctx, self.last_cursor_rect);
        self.code_actions_ui(&ctx, self.last_cursor_rect);
        self.symbols_ui(&ctx);
        if !self.debug_hover_active() {
            self.hover_ui(&ctx);
        }
        self.debug_hover_ui(&ctx);
        self.quick_open_ui(&ctx);
        self.goto_line_ui(&ctx);
        self.modal_ui(&ctx);
        self.toasts_ui(&ctx);
    }
}

impl App {
    fn code_menu(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let has = !self.docs.is_empty();
        ui.menu_button("Code", |ui| {
            if ui.add_enabled(has, egui::Button::new("Go to Definition").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::GotoDefinition))).clicked() {
                self.cmd_definition_at(&ctx, None);
            }
            if ui.add_enabled(has, egui::Button::new("Go to Declaration").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::GotoDeclaration))).clicked() {
                self.cmd_declaration(&ctx);
            }
            if ui.add_enabled(self.tree.is_some(), egui::Button::new("Go to Symbol…").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::GotoSymbol))).clicked() {
                self.cmd_goto_symbol(&ctx);
            }
            if ui.add_enabled(has, egui::Button::new("Find References").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::FindReferences))).clicked() {
                self.cmd_references(&ctx);
            }
            if ui.add_enabled(!self.lsp.back.is_empty(), egui::Button::new("Back").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::Back))).clicked() {
                self.nav_back();
            }
            if ui.add_enabled(has, egui::Button::new("Switch Header/Source").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::SwitchHeader))).clicked() {
                self.cmd_switch_header(&ctx);
            }
            ui.separator();
            if ui.add_enabled(has, egui::Button::new("Quick Fix / Refactor…").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::QuickFix))).clicked() {
                self.cmd_quick_fix(&ctx);
            }
            if ui.add_enabled(has, egui::Button::new("Rename Symbol…").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::Rename))).clicked() {
                self.cmd_rename(&ctx);
            }
            if ui.add_enabled(has, egui::Button::new("Format File").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::Format))).clicked() {
                self.cmd_format(&ctx);
            }
            if ui.add_enabled(has, egui::Button::new("Autocomplete").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::Complete))).clicked() {
                if let Some(d) = self.docs.get(self.active) {
                    let c = d.cursor_byte;
                    self.on_typed(&ctx, "", c, true);
                }
            }
            ui.separator();
            if ui.button("Restart Language Servers").clicked() {
                self.cmd_restart_servers();
            }
        });
    }

    fn update_title(&mut self, ctx: &egui::Context) {
        let mut title = String::new();
        if let Some(d) = self.docs.get(self.active) {
            if d.dirty {
                title.push_str("● ");
            }
            title.push_str(&d.title());
            title.push_str(" — ");
        }
        if let Some(t) = &self.tree
            && let Some(n) = t.root.file_name()
        {
            title.push_str(&n.to_string_lossy());
            title.push_str(" — ");
        }
        title.push_str("cedit");
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }

    fn menu_bar(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.add(egui::Button::new("New File").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::NewFile))).clicked() {
                    self.cmd_new_file();
                }
                if ui.add(egui::Button::new("Open File…").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::OpenFile))).clicked() {
                    self.cmd_open_file();
                }
                if ui.add(egui::Button::new("Open Folder…").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::OpenFolder))).clicked() {
                    self.cmd_open_folder();
                }
                let recent: Vec<PathBuf> = self.recent_folders.clone();
                ui.add_enabled_ui(!recent.is_empty(), |ui| {
                    ui.menu_button("Open Recent", |ui| {
                        for r in recent {
                            if ui.button(r.display().to_string()).clicked() {
                                if r.is_dir() {
                                    self.open_folder(r);
                                } else {
                                    self.recent_folders.retain(|x| x != &r);
                                }
                            }
                        }
                    });
                });
                ui.separator();
                let has_doc = !self.docs.is_empty();
                if ui.add_enabled(has_doc, egui::Button::new("Save").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::Save))).clicked() {
                    self.save_doc(&ctx, self.active);
                }
                if ui.add_enabled(has_doc, egui::Button::new("Save As…").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::SaveAs))).clicked() {
                    self.save_doc_as(&ctx, self.active);
                }
                if ui.add_enabled(has_doc, egui::Button::new("Save All").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::SaveAll))).clicked() {
                    self.save_all(&ctx);
                }
                ui.separator();
                if ui.add_enabled(has_doc, egui::Button::new("Close Tab").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::CloseTab))).clicked() {
                    self.request_close(self.active);
                }
                if ui.add_enabled(has_doc, egui::Button::new("Close All Tabs").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::CloseAllTabs))).clicked() {
                    self.request_close_many(self.tab_ids(TabSet::All, 0));
                }
                if ui.add_enabled(self.docs.len() > 1, egui::Button::new("Close Other Tabs")).clicked() {
                    self.request_close_many(self.tab_ids(TabSet::Others, self.active));
                }
                if ui.add_enabled(self.docs.iter().any(|d| !d.dirty), egui::Button::new("Close Saved Tabs")).clicked() {
                    self.request_close_many(self.tab_ids(TabSet::Saved, 0));
                }
                if ui.add_enabled(self.tree.is_some(), egui::Button::new("Close Folder")).clicked() {
                    self.tree = None;
                }
                if let Some(root) = self.project_root().filter(|_| !self.folder_trusted())
                    && ui.button("Trust This Folder…").clicked()
                {
                    self.modal = Some(Modal::TrustFolder(root));
                }
                ui.separator();
                if ui.add(egui::Button::new("Settings…").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::Settings))).clicked() {
                    self.cmd_open_settings(&ctx);
                }
                ui.separator();
                if ui.button("Quit").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("Edit", |ui| {
                if ui.add(egui::Button::new("Find…").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::Find))).clicked() {
                    self.cmd_find(&ctx, false);
                }
                if ui.add(egui::Button::new("Replace…").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::Replace))).clicked() {
                    self.cmd_find(&ctx, true);
                }
                if ui.add(egui::Button::new("Find in Files…").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::FindInFiles))).clicked() {
                    self.cmd_find_in_files(&ctx);
                }
                ui.separator();
                ui.label(RichText::new("In the editor:").weak());
                ui.add(egui::Button::new("Toggle Comment").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::ToggleComment)));
                ui.add(egui::Button::new("Duplicate Line").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::DuplicateLine)));
                ui.add(egui::Button::new("Delete Line").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::DeleteLine)));
                ui.add(egui::Button::new("Indent / Unindent").shortcut_text("Tab / Shift+Tab"));
            });
            ui.menu_button("Go", |ui| {
                if ui.add_enabled(self.tree.is_some(), egui::Button::new("Go to File…").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::GotoFile))).clicked() {
                    self.cmd_quick_open();
                }
                if ui.add_enabled(!self.docs.is_empty(), egui::Button::new("Go to Line…").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::GotoLine))).clicked() {
                    self.goto_line = Some((String::new(), true));
                }
                ui.separator();
                if ui.add(egui::Button::new("Next Tab").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::NextTab))).clicked() && !self.docs.is_empty() {
                    self.active = (self.active + 1) % self.docs.len();
                }
            });
            self.code_menu(ui);
            self.build_menu(ui);
            ui.menu_button("View", |ui| {
                if ui.add(egui::Button::new("Toggle Sidebar").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::ToggleSidebar))).clicked() {
                    self.sidebar = !self.sidebar;
                }
                if ui.add(egui::Button::new("Toggle Bottom Panel").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::ToggleBottomPanel))).clicked() {
                    self.bottom_open = !self.bottom_open;
                }
                if ui.add(egui::Button::new("Terminal").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::ToggleTerminal))).clicked() {
                    self.show_terminal(&ctx);
                }
                if ui.add(egui::Button::new("New Terminal").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::NewTerminal))).clicked() {
                    self.new_terminal(&ctx);
                }
                ui.separator();
                if ui.add(egui::Button::new("Zoom In").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::ZoomIn))).clicked() {
                    self.zoom(1.0);
                }
                if ui.add(egui::Button::new("Zoom Out").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::ZoomOut))).clicked() {
                    self.zoom(-1.0);
                }
                if ui.add(egui::Button::new("Reset Zoom").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::ZoomReset))).clicked() {
                    self.font_size = DEFAULT_FONT_SIZE;
                }
            });
            ui.menu_button("Help", |ui| {
                if ui.button("Keyboard Shortcuts").clicked() {
                    self.modal = Some(Modal::Shortcuts);
                }
            });
            self.build_toolbar(ui);
        });
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        let Some(d) = self.docs.get(self.active) else { return };
        let path = d
            .path
            .as_ref()
            .map(|p| {
                self.tree
                    .as_ref()
                    .and_then(|t| p.strip_prefix(&t.root).ok())
                    .unwrap_or(p)
                    .display()
                    .to_string()
            })
            .unwrap_or_else(|| "untitled".into());
        let lang = d.lang.name();
        let crlf = d.crlf;
        let (l, c) = d.cursor_line_col;
        let sel = d.selection_chars;
        ui.horizontal_centered(|ui| {
            ui.label(RichText::new(path).weak());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(lang);
                ui.separator();
                self.lsp_status_ui(ui);
                ui.separator();
                if let Some(t) = self.debug_status_text() {
                    ui.label(RichText::new(t).color(self.theme.warning));
                    ui.separator();
                }
                ui.label(if crlf { "CRLF" } else { "LF" });
                ui.separator();
                if sel > 0 {
                    ui.label(format!("{sel} chars"));
                }
                ui.label(format!("Ln {}, Col {}", l + 1, c + 1));
            });
        });
    }

    fn sidebar_ui(&mut self, ui: &mut Ui) {
        let active = self.docs.get(self.active).and_then(|d| d.path.clone());
        let theme = self.theme.clone();
        let mut action = None;
        ui.horizontal(|ui| {
            ui.label(RichText::new("PROJECT").small().weak());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("Refresh").on_hover_text("Reload the file tree").clicked()
                    && let Some(t) = &mut self.tree
                {
                    t.refresh();
                }
            });
        });
        egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
            if let Some(tree) = &mut self.tree {
                action = tree.show(ui, active.as_deref(), &theme);
            }
        });
        if let Some(a) = action {
            self.tree_action(ui.ctx(), a);
        }
    }

    fn tree_action(&mut self, ctx: &egui::Context, a: TreeAction) {
        match a {
            TreeAction::Open(p) => self.open_file(&p),
            TreeAction::NewFile(dir) => {
                self.modal = Some(Modal::Name { kind: NameKind::NewFile, target: dir, text: String::new(), focus: true })
            }
            TreeAction::NewFolder(dir) => {
                self.modal = Some(Modal::Name { kind: NameKind::NewFolder, target: dir, text: String::new(), focus: true })
            }
            TreeAction::Rename(p) => {
                let text = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                self.modal = Some(Modal::Name { kind: NameKind::Rename, target: p, text, focus: true })
            }
            TreeAction::Delete(p) => self.modal = Some(Modal::ConfirmTrash(p)),
            TreeAction::CopyPath(p) => {
                ctx.copy_text(p.display().to_string());
                self.toast(ctx, "Path copied", false);
            }
            TreeAction::Reveal(p) => project::reveal_in_file_manager(&p),
        }
    }

    fn central(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        if self.docs.is_empty() {
            self.welcome(ui);
            return;
        }
        self.active = self.active.min(self.docs.len() - 1);

        // Tabs
        egui::Frame::NONE
            .fill(self.theme.panel_bg)
            .inner_margin(egui::Margin::symmetric(4, 3))
            .show(ui, |ui| {
                egui::ScrollArea::horizontal()
                    .id_salt("tabs")
                    .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let mut close = None;
                            let mut close_set: Option<(TabSet, usize)> = None;
                            let mut copy_path = None;
                            let mut activate = None;
                            let n = self.docs.len();
                            for (i, d) in self.docs.iter().enumerate() {
                                let selected = i == self.active;
                                let label = if d.dirty { format!("{} •", d.title()) } else { d.title() };
                                let text = if selected { RichText::new(label).strong() } else { RichText::new(label) };
                                let r = ui.add(egui::Button::selectable(selected, text));
                                if r.clicked() {
                                    activate = Some(i);
                                }
                                if r.middle_clicked() {
                                    close = Some(i);
                                }
                                if let Some(p) = &d.path {
                                    r.clone().on_hover_text(p.display().to_string());
                                }
                                r.context_menu(|ui| {
                                    if ui.button("Close").clicked() {
                                        close = Some(i);
                                        ui.close();
                                    }
                                    if ui.add_enabled(n > 1, egui::Button::new("Close Others")).clicked() {
                                        close_set = Some((TabSet::Others, i));
                                        ui.close();
                                    }
                                    if ui.add_enabled(i + 1 < n, egui::Button::new("Close to the Right")).clicked() {
                                        close_set = Some((TabSet::Right, i));
                                        ui.close();
                                    }
                                    if ui.button("Close Saved").clicked() {
                                        close_set = Some((TabSet::Saved, i));
                                        ui.close();
                                    }
                                    if ui.button("Close All").clicked() {
                                        close_set = Some((TabSet::All, i));
                                        ui.close();
                                    }
                                    if let Some(p) = &d.path {
                                        ui.separator();
                                        if ui.button("Copy Path").clicked() {
                                            copy_path = Some(p.display().to_string());
                                            ui.close();
                                        }
                                    }
                                });
                                if ui.add(egui::Button::new(RichText::new("×").weak()).frame(false)).on_hover_text("Close").clicked() {
                                    close = Some(i);
                                }
                                ui.add_space(6.0);
                            }
                            if let Some(i) = activate {
                                self.active = i;
                                self.focus_active();
                            }
                            if let Some(i) = close {
                                self.request_close(i);
                            }
                            if let Some((set, at)) = close_set {
                                self.request_close_many(self.tab_ids(set, at));
                            }
                            if let Some(p) = copy_path {
                                ui.ctx().copy_text(p);
                            }
                        });
                    });
            });
        if self.docs.is_empty() {
            return;
        }

        // Disk conflict banner
        let theme = self.theme.clone();
        if self.docs[self.active].disk_conflict {
            egui::Frame::NONE
                .fill(theme.panel_bg)
                .inner_margin(egui::Margin::symmetric(8, 4))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.colored_label(theme.warning, "This file changed on disk, and you have unsaved edits.");
                        if ui.button("Reload from disk").clicked() {
                            self.docs[self.active].reload();
                        }
                        if ui.button("Keep my version").clicked() {
                            self.docs[self.active].disk_conflict = false;
                        }
                    });
                });
        }

        if self.find.open {
            self.find_bar(ui);
        }

        let doc = &mut self.docs[self.active];
        if self.find.open {
            self.find.update(&doc.text);
        }
        let doc_path = doc.path.clone();
        let _ = doc;
        self.completion_keys(&ctx);
        let mut diags: Vec<&Diagnostic> = match &doc_path {
            Some(p) => self.diagnostics.iter().filter(|d| &d.path == p).collect(),
            None => Vec::new(),
        };
        if let Some(live) = doc_path.as_ref().and_then(|p| self.lsp.diags.get(p)) {
            for d in live {
                if !diags.iter().any(|x| x.line == d.line && x.message == d.message) {
                    diags.push(d);
                }
            }
        }
        let mut marks = self.debug_marks(doc_path.as_deref());
        marks.heat = self.profile_heat(doc_path.as_deref());
        let doc = &mut self.docs[self.active];
        let out = editor::show(ui, doc, &self.find, &theme, self.font_size, &diags, &self.keymap, &marks);
        self.track_breakpoint_lines();
        if let (Some(line), Some(p)) = (out.gutter_click, doc_path.as_ref()) {
            self.toggle_breakpoint(p, line);
        }
        self.last_cursor_rect = out.cursor_rect;
        let now = ctx.input(|i| i.time);
        if !out.typed.is_empty()
            && let Some(c) = out.cursor
        {
            self.on_typed(&ctx, &out.typed, c, false);
        } else if let (Some(c), Some(comp)) = (out.cursor, &self.lsp.completion) {
            // Cursor moved away from the word being completed.
            let doc = &self.docs[self.active];
            let still = c >= comp.anchor_pos() && doc.text.get(comp.anchor_pos()..c).is_some_and(|w| w.chars().all(|ch| ch.is_alphanumeric() || ch == '_'));
            if !still || !out.focused {
                self.lsp.completion = None;
            }
        }
        let hover = if self.modal.is_none() { out.hover } else { None };
        if self.debug_on_hover(now, hover) {
            self.on_hover(now, None);
        } else {
            self.on_hover(now, hover);
        }
        if let Some(b) = out.cmd_click {
            self.cmd_definition_at(&ctx, Some(b));
        }
        let _ = ctx;
    }

    fn welcome(&mut self, ui: &mut Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.25);
            ui.heading(RichText::new("cedit").size(32.0));
            ui.label(RichText::new("A code editor for C, GLSL and Makefiles").weak());
            ui.add_space(20.0);
            if ui.button("Open Folder…").clicked() {
                self.cmd_open_folder();
            }
            if ui.button("Open File…").clicked() {
                self.cmd_open_file();
            }
            if ui.button("New File").clicked() {
                self.cmd_new_file();
            }
            if !self.recent_folders.is_empty() {
                ui.add_space(16.0);
                ui.label(RichText::new("Recent").weak());
                for r in self.recent_folders.clone() {
                    if ui.link(r.display().to_string()).clicked() && r.is_dir() {
                        self.open_folder(r);
                    }
                }
            }
            if self.tree.is_some() {
                ui.add_space(16.0);
                let sc = ui.ctx().format_shortcut(&KeyboardShortcut::new(Modifiers::COMMAND, Key::P));
                ui.label(RichText::new(format!("Press {sc} to jump to any file")).weak());
            }
        });
    }

    fn find_bar(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let theme = self.theme.clone();
        let mut step: Option<bool> = None;
        let mut do_replace = false;
        let mut do_replace_all = false;
        egui::Frame::NONE
            .fill(theme.panel_bg)
            .inner_margin(egui::Margin::symmetric(8, 4))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut self.find.query)
                            .hint_text("Find")
                            .desired_width(260.0)
                            .id_salt("find-query"),
                    );
                    if self.find.focus_query {
                        r.request_focus();
                        self.find.focus_query = false;
                    }
                    if r.lost_focus() && ctx.input(|i| i.key_pressed(Key::Enter)) {
                        step = Some(!ctx.input(|i| i.modifiers.shift));
                        ctx.input_mut(|i| {
                            i.consume_key(Modifiers::SHIFT, Key::Enter);
                            i.consume_key(Modifiers::NONE, Key::Enter);
                        });
                        r.request_focus();
                    }
                    ui.toggle_value(&mut self.find.case_sensitive, "Aa").on_hover_text("Match case");
                    ui.toggle_value(&mut self.find.whole_word, "W").on_hover_text("Whole word");
                    let n = self.find.matches.len();
                    let status = if self.find.query.is_empty() {
                        String::new()
                    } else if n == 0 {
                        "No results".into()
                    } else if let Some(c) = self.find.current {
                        format!("{} of {n}", c + 1)
                    } else {
                        format!("{n} results")
                    };
                    ui.label(RichText::new(status).weak());
                    if ui.small_button("Prev").on_hover_text("Previous match (Shift+Enter)").clicked() {
                        step = Some(false);
                    }
                    if ui.small_button("Next").on_hover_text("Next match (Enter)").clicked() {
                        step = Some(true);
                    }
                    ui.toggle_value(&mut self.find.replace_open, "Replace").on_hover_text("Show replace");
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("×").on_hover_text("Close (Esc)").clicked() {
                            self.find.open = false;
                        }
                    });
                });
                if self.find.replace_open {
                    ui.horizontal(|ui| {
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut self.find.replacement)
                                .hint_text("Replace")
                                .desired_width(260.0)
                                .id_salt("find-replace"),
                        );
                        if r.lost_focus() && ctx.input(|i| i.key_pressed(Key::Enter)) {
                            do_replace = true;
                            ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter));
                            r.request_focus();
                        }
                        if ui.button("Replace").clicked() {
                            do_replace = true;
                        }
                        if ui.button("Replace All").clicked() {
                            do_replace_all = true;
                        }
                    });
                }
            });
        if let Some(fwd) = step {
            self.find_step_ex(&ctx, fwd, false);
        }
        if do_replace {
            self.replace_one(&ctx);
        }
        if do_replace_all {
            self.replace_all(&ctx);
        }
        if !self.find.open {
            self.focus_active();
        }
    }

    fn bottom_panel(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.bottom_tab, BottomTab::Output, "Build Output");
            let (e, w) = self.problem_counts();
            let label = if e + w > 0 { format!("Problems ({})", e + w) } else { "Problems".to_string() };
            ui.selectable_value(&mut self.bottom_tab, BottomTab::Problems, label);
            ui.selectable_value(&mut self.bottom_tab, BottomTab::Search, "Find in Files");
            ui.selectable_value(&mut self.bottom_tab, BottomTab::References, "References");
            if ui.selectable_value(&mut self.bottom_tab, BottomTab::Terminal, "Terminal").clicked() {
                self.show_terminal(ui.ctx());
            }
            ui.selectable_value(&mut self.bottom_tab, BottomTab::Debug, "Debug");
            ui.selectable_value(&mut self.bottom_tab, BottomTab::Profile, "Profile");
            if self.bottom_tab == BottomTab::Terminal {
                self.terminal_tabs(ui);
            }
            if self.bottom_tab == BottomTab::Debug {
                ui.separator();
                self.debug_toolbar(ui);
            }
            if self.bottom_tab == BottomTab::Profile {
                ui.separator();
                self.profile_toolbar(ui);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("×").clicked() {
                    self.bottom_open = false;
                }
            });
        });
        ui.separator();
        match self.bottom_tab {
            BottomTab::Output => self.output_panel(ui),
            BottomTab::Problems => self.problems_panel(ui),
            BottomTab::Search => self.search_panel(ui),
            BottomTab::References => self.references_panel(ui),
            BottomTab::Terminal => self.terminal_panel(ui),
            BottomTab::Debug => self.debug_panel(ui),
            BottomTab::Profile => self.profile_panel(ui),
        }
    }

    fn search_panel(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let Some(root) = self.tree.as_ref().map(|t| t.root.clone()) else {
            ui.label("Open a folder to search across files.");
            return;
        };
        let mut start = false;
        ui.horizontal(|ui| {
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.fif.query)
                    .hint_text("Search in project")
                    .desired_width(320.0)
                    .id_salt("fif-query"),
            );
            if self.fif.focus {
                r.request_focus();
                self.fif.focus = false;
            }
            if r.lost_focus() && ctx.input(|i| i.key_pressed(Key::Enter)) {
                start = true;
            }
            ui.toggle_value(&mut self.fif.case_sensitive, "Aa").on_hover_text("Match case");
            ui.toggle_value(&mut self.fif.whole_word, "W").on_hover_text("Whole word");
            if ui.button("Search").clicked() {
                start = true;
            }
            if let Some(job) = &mut self.fif.job {
                job.poll();
                let n = job.hits.len();
                let more = if n >= project::MAX_HITS { "+" } else { "" };
                let state = if job.is_done() { "" } else { " (searching…)" };
                ui.label(RichText::new(format!("{n}{more} results{state}")).weak());
            }
        });
        if start && !self.fif.query.is_empty() {
            self.fif.job = Some(SearchJob::start(
                root.clone(),
                self.fif.query.clone(),
                self.fif.case_sensitive,
                self.fif.whole_word,
                ctx.clone(),
            ));
        }
        let mut open: Option<(PathBuf, usize, usize)> = None;
        if let Some(job) = &self.fif.job {
            let theme = &self.theme;
            let row_h = ui.text_style_height(&egui::TextStyle::Monospace) + 4.0;
            egui::ScrollArea::both()
                .auto_shrink([false, false])
                .show_rows(ui, row_h, job.hits.len(), |ui, range| {
                    for h in &job.hits[range] {
                        let rel = h.path.strip_prefix(&root).unwrap_or(&h.path);
                        let mut job_text = egui::text::LayoutJob::default();
                        let mono = egui::FontId::monospace(12.5);
                        job_text.append(
                            &format!("{}:{}  ", rel.display(), h.line + 1),
                            0.0,
                            egui::TextFormat::simple(mono.clone(), theme.function),
                        );
                        job_text.append(h.line_text.trim(), 0.0, egui::TextFormat::simple(mono, theme.text));
                        let r = ui.add(egui::Button::selectable(false, job_text).frame_when_inactive(false));
                        if r.clicked() {
                            open = Some((h.path.clone(), h.line, h.col));
                        }
                    }
                });
        }
        if let Some((p, l, c)) = open {
            self.open_file(&p);
            let qlen = self.fif.query.len();
            if let Some(d) = self.active_doc()
                && d.path.as_deref() == Some(p.as_path())
            {
                let start = crate::textops::line_col_to_byte(&d.text, l, c);
                let end = start + qlen.min(d.text.len() - start);
                let end = crate::textops::floor_char_boundary(&d.text, end);
                d.pending_select = Some(start..end);
            }
        }
    }

    fn quick_open_ui(&mut self, ctx: &egui::Context) {
        let Some(qo) = &mut self.quick_open else { return };
        let mut chosen: Option<PathBuf> = None;
        let mut close = false;
        let mut ranked: Vec<(i64, &PathBuf)> = qo
            .files
            .iter()
            .filter_map(|f| {
                let s = f.to_string_lossy();
                project::fuzzy_score(&s, &qo.query).map(|sc| (sc, f))
            })
            .collect();
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        ranked.truncate(200);
        if !ranked.is_empty() {
            qo.selected = qo.selected.min(ranked.len() - 1);
        }
        ctx.input_mut(|i| {
            if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                qo.selected = (qo.selected + 1).min(ranked.len().saturating_sub(1));
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                qo.selected = qo.selected.saturating_sub(1);
            }
            if i.consume_key(Modifiers::NONE, Key::Enter) {
                chosen = ranked.get(qo.selected).map(|(_, p)| (*p).clone());
            }
            if i.consume_key(Modifiers::NONE, Key::Escape) {
                close = true;
            }
        });
        let modal = egui::Modal::new(egui::Id::unique("quick-open")).show(ctx, |ui| {
            ui.set_width(560.0);
            let r = ui.add(
                egui::TextEdit::singleline(&mut qo.query)
                    .hint_text("Go to file")
                    .desired_width(f32::INFINITY),
            );
            if qo.focus {
                r.request_focus();
                qo.focus = false;
            }
            if r.changed() {
                qo.selected = 0;
            }
            ui.separator();
            egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                for (i, (_, p)) in ranked.iter().enumerate() {
                    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let dir = p.parent().map(|d| d.display().to_string()).unwrap_or_default();
                    let mut job = egui::text::LayoutJob::default();
                    job.append(&name, 0.0, egui::TextFormat::simple(egui::FontId::proportional(14.0), self.theme.text));
                    job.append(&format!("   {dir}"), 0.0, egui::TextFormat::simple(egui::FontId::proportional(12.0), self.theme.gutter_fg));
                    let r = ui.add(egui::Button::selectable(i == qo.selected, job).frame_when_inactive(false));
                    if i == qo.selected {
                        r.scroll_to_me(None);
                    }
                    if r.clicked() {
                        chosen = Some((*p).clone());
                    }
                }
                if ranked.is_empty() {
                    ui.label(RichText::new("No matching files").weak());
                }
            });
        });
        if modal.should_close() {
            close = true;
        }
        if let Some(p) = chosen {
            let root = self.tree.as_ref().map(|t| t.root.clone()).unwrap_or_default();
            self.quick_open = None;
            self.open_file(&root.join(p));
            self.focus_active();
        } else if close {
            self.quick_open = None;
            self.focus_active();
        }
    }

    fn goto_line_ui(&mut self, ctx: &egui::Context) {
        let Some((text, focus)) = &mut self.goto_line else { return };
        let mut go = None;
        let mut close = false;
        let total = self.docs.get(self.active).map(|d| d.line_count()).unwrap_or(1);
        let modal = egui::Modal::new(egui::Id::unique("goto-line")).show(ctx, |ui| {
            ui.label(format!("Go to line (1 – {total}), optionally line:column"));
            let r = ui.add(egui::TextEdit::singleline(text).desired_width(240.0));
            if *focus {
                r.request_focus();
                *focus = false;
            }
            if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                let mut parts = text.split(':');
                let line = parts.next().and_then(|s| s.trim().parse::<usize>().ok());
                let col = parts.next().and_then(|s| s.trim().parse::<usize>().ok()).unwrap_or(1);
                if let Some(l) = line {
                    go = Some((l.saturating_sub(1), col.saturating_sub(1)));
                }
                close = true;
            }
        });
        if modal.should_close() || ctx.input(|i| i.key_pressed(Key::Escape)) {
            close = true;
        }
        if let Some((l, c)) = go
            && let Some(d) = self.active_doc()
        {
            d.goto_line_col(l, c);
        }
        if close {
            self.goto_line = None;
            if go.is_none() {
                self.focus_active();
            }
        }
    }

    fn modal_ui(&mut self, ctx: &egui::Context) {
        let Some(modal) = self.modal.take() else { return };
        let mut keep = true;
        match modal {
            Modal::UnsavedClose(id) => {
                let Some(idx) = self.doc_index(id) else { return };
                let title = self.docs[idx].title();
                let mut choice = None;
                egui::Modal::new(egui::Id::unique("unsaved-close")).show(ctx, |ui| {
                    ui.label(RichText::new(format!("Save changes to {title}?")).strong());
                    ui.label("Your changes will be lost if you don't save them.");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Save").clicked() {
                            choice = Some(0);
                        }
                        if ui.button("Don't Save").clicked() {
                            choice = Some(1);
                        }
                        if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                            choice = Some(2);
                        }
                    });
                });
                match choice {
                    Some(0) => {
                        let ok = if self.docs[idx].path.is_some() { self.save_doc_now(ctx, idx) } else { self.save_doc_as(ctx, idx) };
                        if ok {
                            self.close_now(idx);
                        }
                        keep = false;
                    }
                    Some(1) => {
                        self.close_now(idx);
                        keep = false;
                    }
                    Some(_) => keep = false,
                    None => {}
                }
                if keep {
                    self.modal = Some(Modal::UnsavedClose(id));
                }
            }
            Modal::UnsavedCloseMany(ids) => {
                let dirty: Vec<String> = ids.iter().filter_map(|id| self.doc_index(*id)).filter(|i| self.docs[*i].dirty).map(|i| self.docs[i].title()).collect();
                let mut choice = None;
                egui::Modal::new(egui::Id::unique("unsaved-close-many")).show(ctx, |ui| {
                    ui.label(RichText::new(if dirty.len() == 1 { "Save changes to this file?".to_string() } else { format!("Save changes to {} files?", dirty.len()) }).strong());
                    for d in &dirty {
                        ui.label(format!("• {d}"));
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Save and Close").clicked() {
                            choice = Some(0);
                        }
                        if ui.button("Don't Save").clicked() {
                            choice = Some(1);
                        }
                        if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                            choice = Some(2);
                        }
                    });
                });
                match choice {
                    Some(0) => {
                        for id in &ids {
                            if let Some(i) = self.doc_index(*id)
                                && self.docs[i].dirty
                            {
                                if self.docs[i].path.is_some() {
                                    self.save_doc_now(ctx, i);
                                } else {
                                    self.save_doc_as(ctx, i);
                                }
                            }
                        }
                        // Anything that still couldn't be saved stays open.
                        let done: Vec<u64> = ids.iter().copied().filter(|id| self.doc_index(*id).is_some_and(|i| !self.docs[i].dirty)).collect();
                        self.close_ids(&done);
                    }
                    Some(1) => self.close_ids(&ids),
                    Some(_) => {}
                    None => self.modal = Some(Modal::UnsavedCloseMany(ids)),
                }
            }
            Modal::UnsavedQuit => {
                let dirty: Vec<String> = self.docs.iter().filter(|d| d.dirty).map(|d| d.title()).collect();
                let mut choice = None;
                egui::Modal::new(egui::Id::unique("unsaved-quit")).show(ctx, |ui| {
                    ui.label(RichText::new("You have unsaved changes").strong());
                    for d in &dirty {
                        ui.label(format!("• {d}"));
                    }
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Save All and Quit").clicked() {
                            choice = Some(0);
                        }
                        if ui.button("Quit Without Saving").clicked() {
                            choice = Some(1);
                        }
                        if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                            choice = Some(2);
                        }
                    });
                });
                match choice {
                    Some(0) => {
                        self.save_all_now(ctx);
                        for i in 0..self.docs.len() {
                            if self.docs[i].dirty && self.docs[i].path.is_none() {
                                self.save_doc_as(ctx, i);
                            }
                        }
                        if !self.docs.iter().any(|d| d.dirty) {
                            self.quit_confirmed = true;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                    }
                    Some(1) => {
                        self.quit_confirmed = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                    Some(_) => {}
                    None => self.modal = Some(Modal::UnsavedQuit),
                }
            }
            Modal::Name { kind, target, mut text, mut focus } => {
                let title = match kind {
                    NameKind::NewFile => "New file name",
                    NameKind::NewFolder => "New folder name",
                    NameKind::Rename => "Rename to",
                };
                let mut submit = false;
                let mut cancel = false;
                egui::Modal::new(egui::Id::unique("name-input")).show(ctx, |ui| {
                    ui.label(title);
                    let r = ui.add(egui::TextEdit::singleline(&mut text).desired_width(300.0));
                    if focus {
                        r.request_focus();
                        focus = false;
                    }
                    if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        submit = true;
                    }
                    ui.horizontal(|ui| {
                        if ui.button("OK").clicked() {
                            submit = true;
                        }
                        if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                            cancel = true;
                        }
                    });
                });
                let name = text.trim().to_string();
                if submit && !name.is_empty() && !name.contains(['/', '\\']) && name != "." && name != ".." {
                    let result = match kind {
                        NameKind::NewFile => {
                            let p = target.join(&name);
                            if p.exists() {
                                Err(std::io::Error::other("already exists"))
                            } else {
                                std::fs::write(&p, "").map(|_| Some(p))
                            }
                        }
                        NameKind::NewFolder => {
                            let p = target.join(&name);
                            std::fs::create_dir(&p).map(|_| None)
                        }
                        NameKind::Rename => {
                            let p = target.with_file_name(&name);
                            if p.exists() {
                                Err(std::io::Error::other("already exists"))
                            } else {
                                std::fs::rename(&target, &p).map(|_| {
                                    // Update open documents under the old path.
                                    for d in &mut self.docs {
                                        if let Some(dp) = &d.path
                                            && let Ok(rest) = dp.strip_prefix(&target)
                                        {
                                            let np = if rest.as_os_str().is_empty() { p.clone() } else { p.join(rest) };
                                            d.lang = crate::highlight::Language::from_path(&np);
                                            d.path = Some(np);
                                        }
                                    }
                                    None
                                })
                            }
                        }
                    };
                    match result {
                        Ok(open) => {
                            if let Some(t) = &mut self.tree {
                                t.refresh();
                            }
                            if let Some(p) = open {
                                self.open_file(&p);
                                self.focus_active();
                            }
                        }
                        Err(e) => self.toast(ctx, format!("Failed: {e}"), true),
                    }
                } else if !cancel {
                    self.modal = Some(Modal::Name { kind, target, text, focus });
                }
            }
            Modal::ConfirmTrash(p) => {
                let mut choice = None;
                let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                egui::Modal::new(egui::Id::unique("confirm-trash")).show(ctx, |ui| {
                    ui.label(format!("Move \"{name}\" to the trash?"));
                    ui.horizontal(|ui| {
                        if ui.button("Move to Trash").clicked() {
                            choice = Some(true);
                        }
                        if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                            choice = Some(false);
                        }
                    });
                });
                match choice {
                    Some(true) => match trash::delete(&p) {
                        Ok(()) => {
                            if let Some(t) = &mut self.tree {
                                t.refresh();
                            }
                            // Close clean tabs for deleted files.
                            let mut i = 0;
                            while i < self.docs.len() {
                                let under = self.docs[i].path.as_ref().is_some_and(|dp| dp.starts_with(&p));
                                if under && !self.docs[i].dirty {
                                    self.close_now(i);
                                } else {
                                    i += 1;
                                }
                            }
                        }
                        Err(e) => self.toast(ctx, format!("Couldn't move to trash: {e}"), true),
                    },
                    Some(false) => {}
                    None => self.modal = Some(Modal::ConfirmTrash(p)),
                }
            }
            Modal::TrustFolder(root) => {
                let mut choice = None;
                egui::Modal::new(egui::Id::unique("trust-folder")).show(ctx, |ui| {
                    ui.set_max_width(460.0);
                    ui.heading("Trust this folder?");
                    ui.label(egui::RichText::new(root.display().to_string()).monospace());
                    ui.add_space(6.0);
                    ui.label(
                        "For code completion, cedit asks make for a dry run of the Makefile to learn your \
                         compiler flags, and the debugger loads the folder's .lldbinit or .gdbinit. \
                         These files can run any command on your computer.",
                    );
                    ui.add_space(4.0);
                    ui.label("Only trust folders you made yourself or got from someone you trust.");
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Trust Folder").clicked() {
                            choice = Some(true);
                        }
                        if ui.button("Not Now").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                            choice = Some(false);
                        }
                    });
                });
                match choice {
                    Some(true) => {
                        self.trust_declined.remove(&root);
                        self.trusted_folders.retain(|t| !t.starts_with(&root));
                        self.trusted_folders.push(root);
                    }
                    Some(false) => {
                        self.trust_declined.insert(root);
                        self.toast(
                            ctx,
                            "cedit won't run anything from this folder on its own. Trust it later from the File menu.",
                            false,
                        );
                    }
                    None => self.modal = Some(Modal::TrustFolder(root)),
                }
            }
            Modal::BuildSettings(cfg) => self.build_settings_modal(ctx, cfg),
            Modal::RenameSymbol { mut text, mut focus } => {
                let mut submit = false;
                let mut cancel = false;
                egui::Modal::new(egui::Id::unique("rename-symbol")).show(ctx, |ui| {
                    ui.label("Rename symbol to");
                    let r = ui.add(egui::TextEdit::singleline(&mut text).desired_width(300.0));
                    if focus {
                        r.request_focus();
                        focus = false;
                    }
                    if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                        submit = true;
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Rename").clicked() {
                            submit = true;
                        }
                        if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                            cancel = true;
                        }
                    });
                });
                let name = text.trim().to_string();
                if submit && !name.is_empty() {
                    self.do_rename(ctx, name);
                    self.focus_active();
                } else if cancel {
                    self.focus_active();
                } else {
                    self.modal = Some(Modal::RenameSymbol { text, focus });
                }
            }
            Modal::Shortcuts => {
                let mut close = false;
                let mut rows: Vec<(String, String)> = crate::settings::Action::ALL
                    .iter()
                    .map(|a| (self.keymap.label(ctx, *a), a.id().replace('_', " ")))
                    .filter(|(k, _)| !k.is_empty())
                    .collect();
                rows.push(("Tab / Shift+Tab".into(), "indent / unindent".into()));
                rows.push(("Cmd/Ctrl+click".into(), "go to definition".into()));
                egui::Modal::new(egui::Id::unique("shortcuts")).show(ctx, |ui| {
                    ui.heading("Keyboard shortcuts");
                    ui.label(RichText::new("Change these under \"keybindings\" in File → Settings.").weak());
                    egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                        egui::Grid::new("sc-grid").num_columns(2).spacing([24.0, 4.0]).show(ui, |ui| {
                            for (k, d) in &rows {
                                ui.monospace(k);
                                ui.label(d);
                                ui.end_row();
                            }
                        });
                    });
                    if ui.button("Close").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        close = true;
                    }
                });
                if !close {
                    self.modal = Some(Modal::Shortcuts);
                }
            }
        }
    }

    fn toasts_ui(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        self.toasts.retain(|t| t.until > now);
        if self.toasts.is_empty() {
            return;
        }
        let mut dismiss = None;
        egui::Area::new(egui::Id::unique("toasts"))
            .anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-12.0, -30.0))
            .show(ctx, |ui| {
                for (i, t) in self.toasts.iter().enumerate() {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let color = if t.error { self.theme.error } else { self.theme.text };
                            ui.colored_label(color, &t.text);
                            if ui.small_button("×").clicked() {
                                dismiss = Some(i);
                            }
                        });
                    });
                }
            });
        if let Some(i) = dismiss {
            self.toasts.remove(i);
        }
        ctx.request_repaint_after(std::time::Duration::from_millis(500));
    }
}
