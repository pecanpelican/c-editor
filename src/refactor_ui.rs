//! Refactoring in the app: the Refactor menu and "Refactor This" popup,
//! extract function/variable, inline variable, change signature, safe
//! delete, rename previews, #include updates after renaming a file, and
//! the call hierarchy panel.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use egui::{Key, RichText};

use super::App;
use crate::lsp::{self, CallItem, Event, Location, TextEdit};
use crate::refactor::{self, Edit, ExtractPlan, FnDecl, NewParam, NewSig, UseKind};
use crate::textops;

#[derive(Default)]
pub(super) struct RefactorState {
    dialog: Option<Dialog>,
    menu: Option<RefactorMenu>,
    /// Reference requests made for a refactoring, by request id.
    refs: HashMap<u64, RefsPurpose>,
    /// Waiting for `prepareRename`.
    prepare: Option<u64>,
    /// Waiting for code actions to pick "Extract to variable" from (doc id, suggested name).
    want_extract_var: Option<(u64, String)>,
    /// After clangd extracts a variable, ask for its name (doc id, suggested name).
    name_after_extract: Option<(u64, String)>,
    /// Set while applying a previewed edit, so it isn't previewed again.
    pub bypass_preview: bool,
    pub calls: CallTree,
    /// The user unticked "Save changed files" in a preview.
    dont_save: bool,
    next_req: u64,
    name_for_prepare: String,
}

enum RefsPurpose {
    ChangeSignature { name: String },
    SafeDelete { name: String },
}

enum Dialog {
    Extract(ExtractDialog),
    ChangeSig(SigDialog),
    Preview(Preview),
}

struct ExtractDialog {
    doc_id: u64,
    version: u64,
    plan: ExtractPlan,
    name: String,
    ret: String,
    is_static: bool,
    focus: bool,
    error: Option<String>,
}

struct ParamRow {
    text: String,
    from: Option<usize>,
    default: String,
}

struct SigDialog {
    old: FnDecl,
    locations: Vec<Location>,
    utf8: bool,
    name: String,
    ret: String,
    params: Vec<ParamRow>,
    focus: bool,
    error: Option<String>,
}

struct PreviewRow {
    line: usize,
    before: String,
    after: String,
}

struct PreviewFile {
    path: PathBuf,
    edits: Vec<TextEdit>,
    rows: Vec<PreviewRow>,
    on: bool,
}

pub(super) struct Preview {
    title: String,
    what: &'static str,
    files: Vec<PreviewFile>,
    utf8: bool,
    notes: Vec<String>,
    /// A warning shown in red, e.g. remaining usages for Safe Delete.
    danger: Option<String>,
    apply_label: &'static str,
}

#[derive(Clone, Copy, PartialEq)]
enum MenuItem {
    Rename,
    ChangeSignature,
    ExtractVariable,
    ExtractFunction,
    InlineVariable,
    SafeDelete,
    CallHierarchy,
    RenameFile,
}

impl MenuItem {
    const ALL: [MenuItem; 8] = [
        MenuItem::Rename,
        MenuItem::ChangeSignature,
        MenuItem::ExtractVariable,
        MenuItem::ExtractFunction,
        MenuItem::InlineVariable,
        MenuItem::SafeDelete,
        MenuItem::CallHierarchy,
        MenuItem::RenameFile,
    ];

    fn label(self) -> &'static str {
        match self {
            MenuItem::Rename => "Rename…",
            MenuItem::ChangeSignature => "Change Signature…",
            MenuItem::ExtractVariable => "Extract Variable…",
            MenuItem::ExtractFunction => "Extract Function…",
            MenuItem::InlineVariable => "Inline Variable",
            MenuItem::SafeDelete => "Safe Delete…",
            MenuItem::CallHierarchy => "Call Hierarchy",
            MenuItem::RenameFile => "Rename File…",
        }
    }

    fn action(self) -> Option<crate::settings::Action> {
        use crate::settings::Action as A;
        Some(match self {
            MenuItem::Rename => A::Rename,
            MenuItem::ChangeSignature => A::ChangeSignature,
            MenuItem::ExtractVariable => A::ExtractVariable,
            MenuItem::ExtractFunction => A::ExtractFunction,
            MenuItem::InlineVariable => A::InlineVariable,
            MenuItem::SafeDelete => A::SafeDelete,
            MenuItem::CallHierarchy => A::CallHierarchy,
            MenuItem::RenameFile => return None,
        })
    }
}

struct RefactorMenu {
    doc_id: u64,
    selected: usize,
    opened_at: f64,
    /// Picked or closed from the keyboard this frame.
    pick: Option<usize>,
    close: bool,
}

#[derive(Default)]
pub(super) struct CallTree {
    incoming: bool,
    roots: Vec<CallNode>,
    /// request id -> path of the node whose children it fills (empty = roots).
    pending: HashMap<u64, Vec<usize>>,
    asked: bool,
}

struct CallNode {
    item: CallItem,
    children: Option<Vec<CallNode>>,
    open: bool,
}

impl CallTree {
    fn node_mut(&mut self, path: &[usize]) -> Option<&mut CallNode> {
        let (first, rest) = path.split_first()?;
        let mut n = self.roots.get_mut(*first)?;
        for &i in rest {
            n = n.children.as_mut()?.get_mut(i)?;
        }
        Some(n)
    }
}

/// LSP edits for byte edits in `text`, with UTF-8 positions.
fn to_text_edits(text: &str, edits: &[Edit]) -> Vec<TextEdit> {
    edits
        .iter()
        .map(|e| TextEdit {
            start: lsp::byte_to_position(text, e.range.start, true),
            end: lsp::byte_to_position(text, e.range.end, true),
            new_text: e.text.clone(),
        })
        .collect()
}

fn shorten(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() > 140 { format!("{}…", s.chars().take(140).collect::<String>()) } else { s.to_string() }
}

const SOURCE_EXTS: &[&str] = &[
    "c", "h", "m", "mm", "cc", "cpp", "cxx", "hh", "hpp", "hxx", "inc", "glsl", "vert", "frag", "comp", "geom", "tesc", "tese", "metal", "hlsl",
];

impl App {
    fn refactor_req(&mut self) -> u64 {
        self.refactor.next_req += 1;
        // Keep clear of other request counters by using a high range.
        (1 << 40) + self.refactor.next_req
    }

    /// The current text of `path`: the open document's, else the file on disk.
    fn text_of(&self, path: &Path) -> Option<String> {
        self.docs
            .iter()
            .find(|d| d.path.as_deref() == Some(path))
            .map(|d| d.text.clone())
            .or_else(|| std::fs::read_to_string(path).ok())
    }

    fn active_is_c(&self) -> bool {
        self.docs.get(self.active).and_then(|d| d.path.as_ref()).is_some_and(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()).unwrap_or_default(),
                "c" | "h" | "m" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "mm" | "inc"
            )
        })
    }

    // ----- menu ------------------------------------------------------------

    pub(super) fn refactor_menu(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let has = !self.docs.is_empty();
        ui.menu_button("Refactor", |ui| {
            if ui.add_enabled(has, egui::Button::new("Refactor This…").shortcut_text(self.keymap.label(&ctx, crate::settings::Action::RefactorThis))).clicked() {
                self.cmd_refactor_this(&ctx);
            }
            ui.separator();
            for item in MenuItem::ALL {
                let mut b = egui::Button::new(item.label());
                if let Some(a) = item.action() {
                    b = b.shortcut_text(self.keymap.label(&ctx, a));
                }
                if ui.add_enabled(has, b).clicked() {
                    self.run_menu_item(&ctx, item);
                }
            }
        });
    }

    pub(super) fn cmd_refactor_this(&mut self, ctx: &egui::Context) {
        let Some(doc) = self.docs.get(self.active) else { return };
        self.lsp.completion = None;
        self.refactor.menu = Some(RefactorMenu { doc_id: doc.id, selected: 0, opened_at: ctx.input(|i| i.time), pick: None, close: false });
    }

    fn run_menu_item(&mut self, ctx: &egui::Context, item: MenuItem) {
        match item {
            MenuItem::Rename => self.cmd_rename(ctx),
            MenuItem::ChangeSignature => self.cmd_change_signature(ctx),
            MenuItem::ExtractVariable => self.cmd_extract_variable(ctx),
            MenuItem::ExtractFunction => self.cmd_extract_function(ctx),
            MenuItem::InlineVariable => self.cmd_inline_variable(ctx),
            MenuItem::SafeDelete => self.cmd_safe_delete(ctx),
            MenuItem::CallHierarchy => self.cmd_call_hierarchy(ctx),
            MenuItem::RenameFile => {
                if let Some(p) = self.docs.get(self.active).and_then(|d| d.path.clone()) {
                    let text = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    self.modal = Some(super::Modal::Name { kind: super::NameKind::Rename, target: p, text, focus: true });
                } else {
                    self.toast(ctx, "Save the file first", false);
                }
            }
        }
    }

    fn refactor_this_ui(&mut self, ctx: &egui::Context) {
        let Some(m) = &mut self.refactor.menu else { return };
        if self.docs.get(self.active).map(|d| d.id) != Some(m.doc_id) {
            self.refactor.menu = None;
            return;
        }
        let (key_pick, esc) = (m.pick.take(), std::mem::take(&mut m.close));
        let pos = self.last_cursor_rect.map(|r| r.left_bottom() + egui::vec2(0.0, 2.0)).unwrap_or(egui::pos2(200.0, 120.0));
        let theme = self.theme.clone();
        let mut pick = key_pick;
        let selected = m.selected;
        let opened_at = m.opened_at;
        let resp = egui::Area::new(egui::Id::unique("refactor-this"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).inner_margin(egui::Margin::same(4)).show(ui, |ui| {
                    ui.set_min_width(280.0);
                    ui.label(RichText::new("Refactor This").strong());
                    ui.separator();
                    for (i, item) in MenuItem::ALL.iter().enumerate() {
                        let mut job = egui::text::LayoutJob::default();
                        job.append(&format!("{}  ", i + 1), 0.0, egui::TextFormat::simple(egui::FontId::monospace(11.5), theme.comment));
                        job.append(item.label(), 0.0, egui::TextFormat::simple(egui::FontId::proportional(13.5), theme.text));
                        if let Some(a) = item.action() {
                            let k = self.keymap.label(ctx, a);
                            if !k.is_empty() {
                                job.append(&format!("   {k}"), 0.0, egui::TextFormat::simple(egui::FontId::proportional(11.5), theme.comment));
                            }
                        }
                        if ui.add(egui::Button::selectable(i == selected, job).frame_when_inactive(i == selected)).clicked() {
                            pick = Some(i);
                        }
                    }
                });
            });
        let now = ctx.input(|i| i.time);
        let clicked_outside = now - opened_at > 0.2 && ctx.input(|i| i.pointer.any_pressed()) && !resp.response.contains_pointer();
        if let Some(i) = pick {
            self.refactor.menu = None;
            self.run_menu_item(ctx, MenuItem::ALL[i]);
        } else if esc || clicked_outside {
            self.refactor.menu = None;
            self.focus_active();
        }
    }

    /// Keyboard for the Refactor This popup; runs before the editor sees keys.
    /// Returns true while the popup is open.
    pub(super) fn refactor_menu_keys(&mut self, ctx: &egui::Context) -> bool {
        let Some(m) = &mut self.refactor.menu else { return false };
        let n = MenuItem::ALL.len();
        ctx.input_mut(|i| {
            if i.consume_key(egui::Modifiers::NONE, Key::ArrowUp) {
                m.selected = (m.selected + n - 1) % n;
            }
            if i.consume_key(egui::Modifiers::NONE, Key::ArrowDown) {
                m.selected = (m.selected + 1) % n;
            }
            if i.consume_key(egui::Modifiers::NONE, Key::Enter) {
                m.pick = Some(m.selected);
            }
            if i.consume_key(egui::Modifiers::NONE, Key::Escape) {
                m.close = true;
            }
            // Number keys pick directly.
            for d in 1..=n {
                if let Some(k) = Key::from_name(&d.to_string())
                    && i.consume_key(egui::Modifiers::NONE, k)
                {
                    m.pick = Some(d - 1);
                }
            }
            // Typed digits would otherwise land in the editor as text.
            i.events.retain(|e| !matches!(e, egui::Event::Text(_)));
        });
        true
    }

    /// Hide hover popups while a refactoring popup or dialog is up.
    pub(super) fn refactor_busy(&self) -> bool {
        self.refactor.menu.is_some() || self.refactor.dialog.is_some()
    }

    // ----- extract / inline ------------------------------------------------

    pub(super) fn cmd_extract_function(&mut self, ctx: &egui::Context) {
        let Some(doc) = self.docs.get(self.active) else { return };
        let sel = doc.selection(ctx);
        match refactor::plan_extract(&doc.text, sel) {
            Ok(plan) => {
                let ret = plan.ret.clone();
                self.refactor.dialog = Some(Dialog::Extract(ExtractDialog {
                    doc_id: doc.id,
                    version: doc.version,
                    plan,
                    name: String::new(),
                    ret,
                    is_static: true,
                    focus: true,
                    error: None,
                }));
            }
            Err(e) => self.toast(ctx, format!("Extract Function: {e}"), true),
        }
    }

    pub(super) fn cmd_extract_variable(&mut self, ctx: &egui::Context) {
        let Some(doc) = self.docs.get(self.active) else { return };
        let sel = doc.selection(ctx);
        if sel.is_empty() {
            self.toast(ctx, "Select an expression to extract", false);
            return;
        }
        let suggested = refactor::suggest_name(&doc.text[sel.clone()]);
        self.refactor.want_extract_var = Some((doc.id, suggested));
        self.cmd_quick_fix(ctx);
    }

    /// Code actions arrived while Extract Variable was waiting for them.
    /// Returns true if they were used.
    pub(super) fn take_extract_variable(&mut self, ctx: &egui::Context, actions: &[lsp::CodeAction], utf8: bool) -> bool {
        let Some((doc_id, name)) = self.refactor.want_extract_var.take() else { return false };
        match actions.iter().find(|a| a.title.to_ascii_lowercase().contains("extract") && a.title.to_ascii_lowercase().contains("variable")) {
            Some(a) => {
                self.refactor.name_after_extract = Some((doc_id, name));
                self.apply_code_action(ctx, a.clone(), utf8);
            }
            None => self.toast(ctx, "Select a complete expression to extract into a variable", false),
        }
        true
    }

    /// After edits land: if clangd just extracted a variable, select its
    /// placeholder name and ask for a real one.
    pub(super) fn after_edits(&mut self, ctx: &egui::Context) {
        let Some((doc_id, name)) = self.refactor.name_after_extract.take() else { return };
        let Some(doc) = self.docs.iter_mut().find(|d| d.id == doc_id) else { return };
        let Some(pos) = find_word(&doc.text, "placeholder", doc.cursor_byte) else { return };
        doc.pending_select = Some(pos..pos + "placeholder".len());
        // Don't pull focus back to the editor: the name box needs it.
        doc.pending_focus = false;
        // Point the editor's cursor there right away so Rename uses it.
        let id = doc.text_edit_id();
        let mut state = egui::TextEdit::load_state(ctx, id).unwrap_or_default();
        let a = textops::byte_to_char(&doc.text, pos);
        state.cursor.set_char_range(Some(egui::text::CCursorRange::one(egui::text::CCursor::new(a))));
        egui::TextEdit::store_state(ctx, id, state);
        self.modal = Some(super::Modal::RenameSymbol { text: name, focus: true });
    }

    pub(super) fn cmd_inline_variable(&mut self, ctx: &egui::Context) {
        let Some(doc) = self.docs.get_mut(self.active) else { return };
        let sel = doc.selection(ctx);
        match refactor::plan_inline(&doc.text, sel.start) {
            Ok(plan) => {
                // Keep the cursor on the same code.
                let mut keep = sel.start;
                for e in &plan.edits {
                    if e.range.end <= keep {
                        keep = keep + e.text.len() - e.range.len();
                    }
                }
                doc.text = refactor::apply(&doc.text, &plan.edits);
                let keep = textops::floor_char_boundary(&doc.text, keep.min(doc.text.len()));
                doc.pending_select = Some(keep..keep);
                doc.mark_changed();
                let n = plan.uses;
                self.toast(ctx, format!("Inlined `{}` ({n} use{})", plan.name, if n == 1 { "" } else { "s" }), false);
            }
            Err(e) => self.toast(ctx, format!("Inline Variable: {e}"), true),
        }
    }

    // ----- change signature / safe delete ------------------------------------

    pub(super) fn cmd_change_signature(&mut self, ctx: &egui::Context) {
        self.refs_for(ctx, |name| RefsPurpose::ChangeSignature { name });
    }

    pub(super) fn cmd_safe_delete(&mut self, ctx: &egui::Context) {
        self.refs_for(ctx, |name| RefsPurpose::SafeDelete { name });
    }

    fn refs_for(&mut self, ctx: &egui::Context, purpose: impl FnOnce(String) -> RefsPurpose) {
        let Some(doc) = self.docs.get(self.active) else { return };
        let sel = doc.selection(ctx);
        let w = word_range(&doc.text, sel.start);
        if w.is_empty() {
            self.toast(ctx, "Put the cursor on a function or variable name", false);
            return;
        }
        let name = doc.text[w].to_string();
        let req = self.refactor_req();
        if self.with_server_at_cursor(ctx, |s, p, pos| s.references_for(p, pos, req)) {
            self.refactor.refs.insert(req, purpose(name));
        }
    }

    pub(super) fn on_refs_for(&mut self, ctx: &egui::Context, req: u64, locations: Vec<Location>, utf8: bool) {
        let Some(purpose) = self.refactor.refs.remove(&req) else { return };
        if locations.is_empty() {
            self.toast(ctx, "No references found (is clangd still indexing?)", true);
            return;
        }
        // Classify every occurrence.
        let mut texts: HashMap<PathBuf, String> = HashMap::new();
        let mut occ: Vec<(Location, usize, Option<UseKind>)> = Vec::new();
        for l in locations {
            if !texts.contains_key(&l.path) {
                let Some(t) = self.text_of(&l.path) else { continue };
                texts.insert(l.path.clone(), t);
            }
            let text = &texts[&l.path];
            let b = lsp::position_to_byte(text, l.line, l.character, utf8);
            let kind = refactor::classify(&refactor::Src::new(text), b);
            occ.push((l, b, kind));
        }
        match purpose {
            RefsPurpose::ChangeSignature { name } => {
                let decl = occ
                    .iter()
                    .filter(|(_, _, k)| *k == Some(UseKind::Definition))
                    .chain(occ.iter().filter(|(_, _, k)| *k == Some(UseKind::Declaration)))
                    .find_map(|(l, b, _)| {
                        let src = refactor::Src::new(&texts[&l.path]);
                        refactor::parse_fn_at(&src, src.tok_at(*b)?)
                    });
                let Some(old) = decl else {
                    self.toast(ctx, format!("`{name}` doesn't look like a function"), true);
                    return;
                };
                let params = old.params.iter().enumerate().map(|(i, p)| ParamRow { text: p.text.clone(), from: Some(i), default: String::new() }).collect();
                self.refactor.dialog = Some(Dialog::ChangeSig(SigDialog {
                    name: old.name.clone(),
                    ret: old.ret.clone(),
                    params,
                    old,
                    locations: occ.into_iter().map(|(l, _, _)| l).collect(),
                    utf8,
                    focus: true,
                    error: None,
                }));
            }
            RefsPurpose::SafeDelete { name } => {
                let mut edits: HashMap<PathBuf, Vec<TextEdit>> = HashMap::new();
                let mut usages = Vec::new();
                let mut seen = std::collections::HashSet::new();
                for (l, b, k) in &occ {
                    let text = &texts[&l.path];
                    let is_decl = matches!(k, Some(UseKind::Definition | UseKind::Declaration)) && {
                        let src = refactor::Src::new(text);
                        src.tok_at(*b).is_some_and(|i| src.depth[i] == 0)
                    };
                    if is_decl {
                        match refactor::deletion_range(text, *b) {
                            Some(r) => {
                                if seen.insert((l.path.clone(), r.start)) {
                                    edits.entry(l.path.clone()).or_default().extend(to_text_edits(text, &[Edit { range: r, text: String::new() }]));
                                }
                            }
                            None => {
                                self.toast(ctx, format!("Safe Delete can't remove this declaration of `{name}` (several names declared together?)"), true);
                                return;
                            }
                        }
                    } else {
                        usages.push(l.clone());
                    }
                }
                if edits.is_empty() {
                    self.toast(ctx, "Safe Delete works on functions and file-scope variables", false);
                    return;
                }
                let mut p = self.make_preview(format!("Safe Delete `{name}`"), "Safe Delete", edits, true);
                if usages.is_empty() {
                    p.notes.push(format!("`{name}` isn't used anywhere else, so it's safe to delete."));
                    p.apply_label = "Delete";
                } else {
                    let n = usages.len();
                    p.danger = Some(format!("`{name}` is still used in {n} place{} (listed in References). Deleting it will break the build.", if n == 1 { "" } else { "s" }));
                    p.apply_label = "Delete Anyway";
                    self.handle_lsp_event(ctx, Event::References { locations: usages, utf8 });
                }
                self.refactor.dialog = Some(Dialog::Preview(p));
            }
        }
    }

    fn apply_change_signature(&mut self, ctx: &egui::Context, d: &SigDialog) {
        let new = NewSig {
            ret: d.ret.trim().to_string(),
            name: d.name.trim().to_string(),
            params: d.params.iter().map(|p| NewParam { text: p.text.trim().to_string(), from: p.from, default: p.default.trim().to_string() }).collect(),
        };
        let mut by_file: HashMap<PathBuf, Vec<&Location>> = HashMap::new();
        for l in &d.locations {
            by_file.entry(l.path.clone()).or_default().push(l);
        }
        let mut edits = HashMap::new();
        let mut notes = Vec::new();
        let (mut decls, mut calls) = (0, 0);
        for (path, locs) in by_file {
            let Some(text) = self.text_of(&path) else { continue };
            let offs: Vec<usize> = locs.iter().map(|l| lsp::position_to_byte(&text, l.line, l.character, d.utf8)).collect();
            let r = refactor::change_signature_in(&text, &offs, &d.old.name, &new, d.old.params.len());
            decls += r.decls;
            calls += r.calls;
            for (off, why) in &r.skipped {
                let (line, _) = textops::line_col(&text, *off);
                notes.push(format!("{}:{} left as is ({why})", path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default(), line + 1));
            }
            if !r.edits.is_empty() {
                edits.insert(path, to_text_edits(&text, &r.edits));
            }
        }
        let mut p = self.make_preview(format!("Change Signature of `{}`", d.old.name), "Change Signature", edits, true);
        p.notes.insert(0, format!("{decls} declaration{} and {calls} call{} updated.", if decls == 1 { "" } else { "s" }, if calls == 1 { "" } else { "s" }));
        p.notes.extend(notes);
        self.refactor.dialog = Some(Dialog::Preview(p));
        let _ = ctx;
    }

    // ----- rename ------------------------------------------------------------

    /// Ask the server whether the symbol can be renamed, then show the rename box.
    pub(super) fn start_rename(&mut self, ctx: &egui::Context, name: String) {
        let supports = self
            .docs
            .get(self.active)
            .and_then(|d| d.path.clone())
            .and_then(|p| self.lsp.server_for(&p).map(|s| s.supports_prepare_rename))
            .unwrap_or(false);
        if !supports {
            self.modal = Some(super::Modal::RenameSymbol { text: name, focus: true });
            return;
        }
        let req = self.refactor_req();
        if self.with_server_at_cursor(ctx, |s, p, pos| s.prepare_rename(p, pos, req)) {
            self.refactor.prepare = Some(req);
            self.refactor.name_for_prepare = name;
        }
    }

    pub(super) fn on_prepare_rename(&mut self, ctx: &egui::Context, req: u64, result: Result<Option<(u32, u32, u32, u32)>, String>) {
        if self.refactor.prepare != Some(req) {
            return;
        }
        self.refactor.prepare = None;
        match result {
            Ok(_) => {
                let name = std::mem::take(&mut self.refactor.name_for_prepare);
                self.modal = Some(super::Modal::RenameSymbol { text: name, focus: true });
            }
            Err(e) => self.toast(ctx, format!("Can't rename: {e}"), true),
        }
    }

    /// Check a new name before asking the server to rename. Returns false
    /// (with a message) if it isn't a valid identifier.
    pub(super) fn check_new_name(&mut self, ctx: &egui::Context, name: &str) -> bool {
        if self.active_is_c() && !refactor::is_identifier(name) {
            let why = if refactor::KEYWORDS.contains(&name) { "is a C keyword" } else { "isn't a valid C identifier" };
            self.toast(ctx, format!("`{name}` {why}"), true);
            return false;
        }
        true
    }

    /// Whether these edits should be previewed instead of applied right away.
    pub(super) fn should_preview(&self, what: &str, edits: &HashMap<PathBuf, Vec<TextEdit>>) -> bool {
        if self.refactor.bypass_preview || what != "Rename" {
            return false;
        }
        let all_open = edits.keys().all(|p| self.docs.iter().any(|d| d.path.as_deref() == Some(p.as_path())));
        edits.values().filter(|v| !v.is_empty()).count() > 1 || !all_open
    }

    pub(super) fn preview_edits(&mut self, what: &'static str, edits: HashMap<PathBuf, Vec<TextEdit>>, utf8: bool) {
        let n: usize = edits.values().map(Vec::len).sum();
        let mut p = self.make_preview(format!("{what}: {n} change{} in {} files", if n == 1 { "" } else { "s" }, edits.len()), what, edits, utf8);
        p.apply_label = "Rename";
        self.refactor.dialog = Some(Dialog::Preview(p));
    }

    // ----- rename file -------------------------------------------------------

    /// After a file is renamed or moved, offer to fix `#include "..."` lines that point at it.
    pub(super) fn after_file_renamed(&mut self, old: &Path, new: &Path) {
        if !new.is_file() {
            return;
        }
        let Some(root) = self.project_root() else { return };
        let dirs = vec![root.clone(), root.join("include"), root.join("inc"), root.join("src")];
        let mut edits = HashMap::new();
        for rel in crate::project::list_files(&root) {
            let path = root.join(&rel);
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
            if !SOURCE_EXTS.contains(&ext.as_str()) {
                continue;
            }
            let Some(text) = self.text_of(&path) else { continue };
            if !text.contains("include") && !text.contains("import") {
                continue;
            }
            // The includer may itself be the renamed file.
            let includer = if path == new { old.to_path_buf() } else { path.clone() };
            let e = refactor::include_edits(&text, &includer, old, new, &dirs);
            if !e.is_empty() {
                edits.insert(path, to_text_edits(&text, &e));
            }
        }
        if edits.is_empty() {
            return;
        }
        let name = new.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut p = self.make_preview(format!("Update #include lines for {name}"), "Update Includes", edits, true);
        p.apply_label = "Update";
        self.refactor.dialog = Some(Dialog::Preview(p));
    }

    // ----- preview -----------------------------------------------------------

    fn make_preview(&self, title: String, what: &'static str, edits: HashMap<PathBuf, Vec<TextEdit>>, utf8: bool) -> Preview {
        let mut files = Vec::new();
        for (path, list) in edits {
            if list.is_empty() {
                continue;
            }
            let text = self.text_of(&path).unwrap_or_default();
            let mut rows = Vec::new();
            for e in &list {
                let s = lsp::position_to_byte(&text, e.start.0, e.start.1, utf8);
                let t = lsp::position_to_byte(&text, e.end.0, e.end.1, utf8);
                let ls = textops::line_start(&text, s);
                let le = textops::line_end(&text, t);
                let before = &text[ls..le];
                let after = format!("{}{}{}", &text[ls..s], e.new_text, &text[t..le]);
                let first = |x: &str| shorten(x.lines().find(|l| !l.trim().is_empty()).unwrap_or(""));
                let n_before = before.lines().count().max(1);
                let mut b = first(before);
                if n_before > 1 {
                    b = format!("{b}  (+{} lines)", n_before - 1);
                }
                let whole_lines = e.new_text.is_empty() && text[ls..s].trim().is_empty() && (t == text.len() || text[..t].ends_with('\n'));
                let a = if whole_lines || after.trim().is_empty() { "(deleted)".to_string() } else { first(&after) };
                rows.push(PreviewRow { line: textops::line_col(&text, s).0, before: b, after: a });
            }
            rows.sort_by_key(|r| r.line);
            files.push(PreviewFile { path, edits: list, rows, on: true });
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Preview { title, what, files, utf8, notes: Vec::new(), danger: None, apply_label: "Apply" }
    }

    fn apply_preview(&mut self, ctx: &egui::Context, p: Preview) {
        let edits: HashMap<PathBuf, Vec<TextEdit>> = p.files.into_iter().filter(|f| f.on).map(|f| (f.path, f.edits)).collect();
        if edits.is_empty() {
            return;
        }
        let paths: Vec<PathBuf> = edits.keys().cloned().collect();
        self.refactor.bypass_preview = true;
        self.handle_lsp_event(ctx, Event::Edits { edits, utf8: p.utf8, what: p.what });
        self.refactor.bypass_preview = false;
        // Save so clangd (which reads included headers from disk) and the
        // build see the change everywhere at once, like CLion does.
        if !self.refactor.dont_save {
            for path in paths {
                if let Some(i) = self.docs.iter().position(|d| d.path.as_deref() == Some(path.as_path())) {
                    self.save_doc_now(ctx, i);
                }
            }
        }
    }

    // ----- dialogs -----------------------------------------------------------

    /// Draw the refactoring popup and dialogs. Called once per frame.
    pub(super) fn refactor_ui(&mut self, ctx: &egui::Context) {
        self.refactor_this_ui(ctx);
        let Some(dialog) = self.refactor.dialog.take() else { return };
        let keep = match dialog {
            Dialog::Extract(d) => self.extract_dialog(ctx, d),
            Dialog::ChangeSig(d) => self.sig_dialog(ctx, d),
            Dialog::Preview(p) => self.preview_dialog(ctx, p),
        };
        if let Some(d) = keep {
            // Don't overwrite a dialog opened while handling this one.
            if self.refactor.dialog.is_none() {
                self.refactor.dialog = Some(d);
            }
        }
    }

    fn extract_dialog(&mut self, ctx: &egui::Context, mut d: ExtractDialog) -> Option<Dialog> {
        let hard = self.docs.iter().find(|x| x.id == d.doc_id).is_some_and(|x| x.lang.uses_hard_tabs());
        let indent = if hard { "\t".to_string() } else { textops::indent_unit() };
        let (mut ok, mut cancel) = (false, false);
        let theme = self.theme.clone();
        egui::Modal::new(egui::Id::unique("extract-function")).show(ctx, |ui| {
            ui.set_width(560.0);
            ui.heading(if d.plan.expression { "Extract Function (from expression)" } else { "Extract Function" });
            egui::Grid::new("xf-grid").num_columns(2).show(ui, |ui| {
                ui.label("Name");
                let r = ui.add(egui::TextEdit::singleline(&mut d.name).desired_width(320.0).hint_text("new_function"));
                if d.focus {
                    r.request_focus();
                    d.focus = false;
                }
                if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    ok = true;
                }
                ui.end_row();
                ui.label("Returns");
                ui.add(egui::TextEdit::singleline(&mut d.ret).desired_width(320.0));
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut d.is_static, "static (only visible in this file)");
                ui.end_row();
            });
            for n in &d.plan.notes {
                ui.label(RichText::new(n).color(theme.comment));
            }
            ui.add_space(4.0);
            ui.label(RichText::new("Preview").strong());
            let name = if d.name.trim().is_empty() { "new_function" } else { d.name.trim() };
            let preview = d.plan.function_text(name, d.is_static, d.ret.trim(), &indent);
            egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(preview.trim_end()).monospace().color(theme.text)).wrap_mode(egui::TextWrapMode::Extend));
            });
            ui.label(RichText::new(format!("Call: {}", d.plan.call.trim().replace("{name}", name))).monospace().color(theme.comment));
            if let Some(e) = &d.error {
                ui.colored_label(theme.error, e);
            }
            ui.horizontal(|ui| {
                if ui.button("Extract").clicked() {
                    ok = true;
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                    cancel = true;
                }
            });
        });
        if cancel {
            self.focus_active();
            return None;
        }
        if !ok {
            return Some(Dialog::Extract(d));
        }
        let name = d.name.trim().to_string();
        if !refactor::is_identifier(&name) {
            d.error = Some(if name.is_empty() { "Type a name for the new function".into() } else { format!("`{name}` isn't a valid C identifier") });
            return Some(Dialog::Extract(d));
        }
        let doc = self.docs.iter_mut().find(|x| x.id == d.doc_id)?;
        if doc.version != d.version {
            self.toast(ctx, "The file changed; run Extract Function again", true);
            return None;
        }
        if refactor::lex(&doc.text).iter().any(|t| doc.text[t.start..t.end] == name) {
            d.error = Some(format!("`{name}` is already used in this file"));
            return Some(Dialog::Extract(d));
        }
        let edits = d.plan.edits(&name, d.is_static, d.ret.trim(), &indent);
        let func_len = edits[0].text.len();
        doc.text = refactor::apply(&doc.text, &edits);
        let c = d.plan.replace.start + func_len;
        let c = textops::floor_char_boundary(&doc.text, c.min(doc.text.len()));
        doc.pending_select = Some(c..c);
        doc.mark_changed();
        self.toast(ctx, format!("Extracted `{name}`"), false);
        None
    }

    fn sig_dialog(&mut self, ctx: &egui::Context, mut d: SigDialog) -> Option<Dialog> {
        let (mut ok, mut cancel) = (false, false);
        let theme = self.theme.clone();
        let mut action: Option<(usize, i32)> = None; // (row, -1 up / +1 down / 0 remove)
        egui::Modal::new(egui::Id::unique("change-signature")).show(ctx, |ui| {
            ui.set_width(620.0);
            ui.heading("Change Signature");
            egui::Grid::new("cs-grid").num_columns(2).show(ui, |ui| {
                ui.label("Name");
                let r = ui.add(egui::TextEdit::singleline(&mut d.name).desired_width(360.0));
                if d.focus {
                    r.request_focus();
                    d.focus = false;
                }
                ui.end_row();
                ui.label("Returns");
                ui.add(egui::TextEdit::singleline(&mut d.ret).desired_width(360.0));
                ui.end_row();
            });
            ui.add_space(4.0);
            ui.label(RichText::new("Parameters").strong());
            egui::Grid::new("cs-params").num_columns(3).striped(true).show(ui, |ui| {
                ui.label(RichText::new("Declaration").color(theme.comment));
                ui.label(RichText::new("Value at existing calls").color(theme.comment));
                ui.label("");
                ui.end_row();
                let n = d.params.len();
                for (i, p) in d.params.iter_mut().enumerate() {
                    ui.add_sized([260.0, 20.0], egui::TextEdit::singleline(&mut p.text).hint_text("int count").font(egui::TextStyle::Monospace));
                    if p.from.is_none() {
                        ui.add_sized([170.0, 20.0], egui::TextEdit::singleline(&mut p.default).hint_text("0").font(egui::TextStyle::Monospace));
                    } else {
                        ui.label(RichText::new("(kept)").color(theme.comment));
                    }
                    ui.horizontal(|ui| {
                        if ui.add_enabled(i > 0, egui::Button::new("Up").small()).clicked() {
                            action = Some((i, -1));
                        }
                        if ui.add_enabled(i + 1 < n, egui::Button::new("Down").small()).clicked() {
                            action = Some((i, 1));
                        }
                        if ui.add(egui::Button::new("Remove").small()).clicked() {
                            action = Some((i, 0));
                        }
                    });
                    ui.end_row();
                }
            });
            if ui.button("Add Parameter").clicked() {
                d.params.push(ParamRow { text: String::new(), from: None, default: String::new() });
            }
            ui.add_space(4.0);
            let sig = refactor::format_signature(&d.old.specifiers, &d.ret, &d.name, &d.params.iter().map(|p| p.text.clone()).collect::<Vec<_>>(), d.old.variadic);
            ui.label(RichText::new("Before").color(theme.comment));
            ui.label(RichText::new(d.old.signature()).monospace());
            ui.label(RichText::new("After").color(theme.comment));
            ui.label(RichText::new(sig).monospace().color(theme.function));
            ui.label(RichText::new(format!("{} references found by clangd. Calls are rewritten; you'll see every change before it's applied.", d.locations.len())).color(theme.comment));
            if let Some(e) = &d.error {
                ui.colored_label(theme.error, e);
            }
            ui.horizontal(|ui| {
                if ui.button("Preview Changes").clicked() {
                    ok = true;
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                    cancel = true;
                }
            });
        });
        if let Some((i, dir)) = action {
            match dir {
                0 => {
                    d.params.remove(i);
                }
                -1 => d.params.swap(i, i - 1),
                _ => d.params.swap(i, i + 1),
            }
        }
        if cancel {
            self.focus_active();
            return None;
        }
        if !ok {
            return Some(Dialog::ChangeSig(d));
        }
        let name = d.name.trim();
        d.error = if !refactor::is_identifier(name) {
            Some(format!("`{name}` isn't a valid C identifier"))
        } else if d.ret.trim().is_empty() {
            Some("Type a return type (void for none)".into())
        } else if d.params.iter().any(|p| p.text.trim().is_empty()) {
            Some("Every parameter needs a declaration, like `int count`".into())
        } else if d.params.iter().any(|p| p.from.is_none() && p.default.trim().is_empty()) {
            Some("New parameters need a value to pass at existing calls".into())
        } else {
            None
        };
        if d.error.is_some() {
            return Some(Dialog::ChangeSig(d));
        }
        self.apply_change_signature(ctx, &d);
        None
    }

    fn preview_dialog(&mut self, ctx: &egui::Context, mut p: Preview) -> Option<Dialog> {
        let (mut ok, mut cancel) = (false, false);
        let theme = self.theme.clone();
        let root = self.project_root();
        let mut jump: Option<(PathBuf, usize)> = None;
        egui::Modal::new(egui::Id::unique("refactor-preview")).show(ctx, |ui| {
            ui.set_width(760.0);
            ui.heading(&p.title);
            if let Some(dg) = &p.danger {
                ui.colored_label(theme.error, dg);
            }
            for n in &p.notes {
                ui.label(RichText::new(n).color(theme.comment));
            }
            ui.separator();
            egui::ScrollArea::vertical().max_height(380.0).auto_shrink([false, true]).show(ui, |ui| {
                for f in &mut p.files {
                    let rel = root.as_ref().and_then(|r| f.path.strip_prefix(r).ok()).unwrap_or(&f.path).display().to_string();
                    let open = self.docs.iter().any(|d| d.path.as_deref() == Some(f.path.as_path()));
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut f.on, RichText::new(&rel).strong().color(theme.function));
                        ui.label(RichText::new(format!("{} change{}{}", f.rows.len(), if f.rows.len() == 1 { "" } else { "s" }, if open { "" } else { " (opens the file)" })).color(theme.comment));
                    });
                    for r in &f.rows {
                        let mut job = egui::text::LayoutJob::default();
                        let mono = egui::FontId::monospace(12.0);
                        job.append(&format!("{:>5}  ", r.line + 1), 0.0, egui::TextFormat::simple(mono.clone(), theme.comment));
                        job.append(&r.before, 0.0, egui::TextFormat { strikethrough: egui::Stroke::new(1.0, theme.error.gamma_multiply(0.7)), ..egui::TextFormat::simple(mono.clone(), theme.comment) });
                        job.append("\n       ", 0.0, egui::TextFormat::simple(mono.clone(), theme.comment));
                        job.append(&r.after, 0.0, egui::TextFormat::simple(mono.clone(), theme.text));
                        if ui.add(egui::Button::selectable(false, job).frame_when_inactive(false)).on_hover_text("Show in editor").clicked() {
                            jump = Some((f.path.clone(), r.line));
                        }
                    }
                    ui.add_space(4.0);
                }
            });
            ui.separator();
            ui.horizontal(|ui| {
                let any = p.files.iter().any(|f| f.on);
                let b = egui::Button::new(RichText::new(p.apply_label).color(if p.danger.is_some() { theme.error } else { theme.text }));
                if ui.add_enabled(any, b).clicked() || (p.danger.is_none() && any && ui.input(|i| i.key_pressed(Key::Enter))) {
                    ok = true;
                }
                if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                    cancel = true;
                }
                let mut save = !self.refactor.dont_save;
                if ui.checkbox(&mut save, "Save changed files").on_hover_text("Undo still works afterwards in each file").changed() {
                    self.refactor.dont_save = !save;
                }
            });
        });
        if let Some((path, line)) = jump {
            self.open_file(&path);
            if let Some(doc) = self.docs.get_mut(self.active) {
                doc.goto_line_col(line, 0);
            }
        }
        if cancel {
            self.focus_active();
            return None;
        }
        if ok {
            self.apply_preview(ctx, p);
            self.focus_active();
            return None;
        }
        Some(Dialog::Preview(p))
    }

    // ----- call hierarchy ----------------------------------------------------

    pub(super) fn cmd_call_hierarchy(&mut self, ctx: &egui::Context) {
        let req = self.refactor_req();
        if self.with_server_at_cursor(ctx, |s, p, pos| s.prepare_call_hierarchy(p, pos, req)) {
            let t = &mut self.refactor.calls;
            t.pending.clear();
            t.pending.insert(req, Vec::new());
            t.roots.clear();
            t.asked = true;
            if !t.incoming && t.roots.is_empty() {
                t.incoming = true;
            }
            self.bottom_open = true;
            self.bottom_tab = super::BottomTab::Calls;
        }
    }

    fn request_calls(&mut self, path: Vec<usize>) {
        let incoming = self.refactor.calls.incoming;
        let Some(node) = self.refactor.calls.node_mut(&path) else { return };
        let item = node.item.clone();
        let req = self.refactor_req();
        if let Some(server) = self.lsp.server_for(&item.location.path) {
            server.calls(&item.raw, incoming, req);
            self.refactor.calls.pending.insert(req, path);
        }
    }

    pub(super) fn on_calls(&mut self, ctx: &egui::Context, req: u64, items: Vec<CallItem>) {
        let Some(path) = self.refactor.calls.pending.remove(&req) else { return };
        let nodes: Vec<CallNode> = items.into_iter().map(|item| CallNode { item, children: None, open: false }).collect();
        if path.is_empty() {
            if nodes.is_empty() {
                self.toast(ctx, "No function here for a call hierarchy", false);
            }
            self.refactor.calls.roots = nodes;
            if !self.refactor.calls.roots.is_empty() {
                self.refactor.calls.roots[0].open = true;
                self.request_calls(vec![0]);
            }
        } else if let Some(n) = self.refactor.calls.node_mut(&path) {
            n.children = Some(nodes);
        }
    }

    pub(super) fn calls_panel(&mut self, ui: &mut egui::Ui) {
        let theme = self.theme.clone();
        let root_dir = self.project_root();
        if !self.refactor.calls.asked {
            let k = self.keymap.label(ui.ctx(), crate::settings::Action::CallHierarchy);
            ui.label(RichText::new(format!("Use Call Hierarchy ({k}) on a function to see who calls it and what it calls.")).weak());
            return;
        }
        let mut switch = None;
        ui.horizontal(|ui| {
            let t = &self.refactor.calls;
            if ui.selectable_label(t.incoming, "Callers").clicked() && !t.incoming {
                switch = Some(true);
            }
            if ui.selectable_label(!t.incoming, "Callees").clicked() && t.incoming {
                switch = Some(false);
            }
            if let Some(r) = t.roots.first() {
                ui.label(RichText::new(format!("of {}", r.item.name)).color(theme.function).strong());
            }
        });
        if let Some(inc) = switch {
            let t = &mut self.refactor.calls;
            t.incoming = inc;
            t.pending.clear();
            fn reset(n: &mut CallNode) {
                n.children = None;
            }
            for r in &mut t.roots {
                reset(r);
                r.open = true;
            }
            if !t.roots.is_empty() {
                self.request_calls(vec![0]);
            }
        }
        ui.separator();
        let mut toggle: Option<Vec<usize>> = None;
        let mut open_loc: Option<(PathBuf, u32, u32)> = None;
        let incoming = self.refactor.calls.incoming;
        #[allow(clippy::too_many_arguments)]
        fn draw(
            ui: &mut egui::Ui,
            nodes: &[CallNode],
            path: &mut Vec<usize>,
            depth: usize,
            theme: &crate::theme::Theme,
            root_dir: &Option<PathBuf>,
            incoming: bool,
            toggle: &mut Option<Vec<usize>>,
            open_loc: &mut Option<(PathBuf, u32, u32)>,
        ) {
            for (i, n) in nodes.iter().enumerate() {
                path.push(i);
                ui.horizontal(|ui| {
                    ui.add_space(depth as f32 * 16.0);
                    let sign = if n.open { "-" } else { "+" };
                    if ui.add(egui::Button::new(RichText::new(sign).monospace()).small().frame(false)).clicked() {
                        *toggle = Some(path.clone());
                    }
                    let rel = root_dir.as_ref().and_then(|r| n.item.location.path.strip_prefix(r).ok()).unwrap_or(&n.item.location.path);
                    let mut job = egui::text::LayoutJob::default();
                    let mono = egui::FontId::monospace(12.5);
                    job.append(&n.item.name, 0.0, egui::TextFormat::simple(mono.clone(), theme.function));
                    let sites = if depth > 0 && n.item.sites.len() > 1 { format!("  ×{}", n.item.sites.len()) } else { String::new() };
                    job.append(&format!("  {}:{}{sites}", rel.display(), n.item.location.line + 1), 0.0, egui::TextFormat::simple(mono, theme.comment));
                    let mut b = ui.add(egui::Button::selectable(false, job).frame_when_inactive(false));
                    if !n.item.detail.is_empty() {
                        b = b.on_hover_text(&n.item.detail);
                    }
                    if b.clicked() {
                        // For callers, jump to the call; otherwise to the function.
                        match n.item.sites.first() {
                            Some(&(l, c)) if incoming && depth > 0 => *open_loc = Some((n.item.location.path.clone(), l, c)),
                            _ => *open_loc = Some((n.item.location.path.clone(), n.item.location.line, n.item.location.character)),
                        }
                    }
                });
                if n.open {
                    match &n.children {
                        None => {
                            ui.horizontal(|ui| {
                                ui.add_space((depth + 1) as f32 * 16.0 + 20.0);
                                ui.label(RichText::new("loading…").weak());
                            });
                        }
                        Some(c) if c.is_empty() => {
                            ui.horizontal(|ui| {
                                ui.add_space((depth + 1) as f32 * 16.0 + 20.0);
                                ui.label(RichText::new(if incoming { "no callers" } else { "no calls found (listing callees needs clangd 20 or newer)" }).weak());
                            });
                        }
                        Some(c) => draw(ui, c, path, depth + 1, theme, root_dir, incoming, toggle, open_loc),
                    }
                }
                path.pop();
            }
        }
        egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
            let mut path = Vec::new();
            draw(ui, &self.refactor.calls.roots, &mut path, 0, &theme, &root_dir, incoming, &mut toggle, &mut open_loc);
        });
        if let Some(p) = toggle
            && let Some(n) = self.refactor.calls.node_mut(&p)
        {
            n.open = !n.open;
            if n.open && n.children.is_none() {
                self.request_calls(p);
            }
        }
        if let Some((path, l, c)) = open_loc {
            let utf8 = self.lsp.server_for(&path).map(|s| s.utf8).unwrap_or(false);
            self.jump_to_location(&path, l, c, utf8, true);
        }
    }
}

fn is_word_byte(b: u8) -> bool {
    b == b'_' || b.is_ascii_alphanumeric()
}

/// Byte range of the identifier at `byte`.
fn word_range(text: &str, byte: usize) -> std::ops::Range<usize> {
    let b = text.as_bytes();
    let byte = byte.min(b.len());
    let mut s = byte;
    while s > 0 && is_word_byte(b[s - 1]) {
        s -= 1;
    }
    let mut e = byte;
    while e < b.len() && is_word_byte(b[e]) {
        e += 1;
    }
    s..e
}

/// The whole-word occurrence of `word` nearest to `near`.
fn find_word(text: &str, word: &str, near: usize) -> Option<usize> {
    let b = text.as_bytes();
    text.match_indices(word)
        .map(|(i, _)| i)
        .filter(|&i| (i == 0 || !is_word_byte(b[i - 1])) && (i + word.len() >= b.len() || !is_word_byte(b[i + word.len()])))
        .min_by_key(|&i| i.abs_diff(near))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words() {
        assert_eq!(find_word("int placeholder2; int placeholder = 1; f(placeholder);", "placeholder", 0), Some(22));
        assert_eq!(word_range("foo(bar_1)", 6), 4..9);
    }
}
