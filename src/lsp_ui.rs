//! Language-server features in the app: live diagnostics, completion,
//! hover, go to definition, references, rename, formatting and semantic
//! highlighting.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use egui::{Key, Modifiers, RichText};

use super::{App, BottomTab};
use crate::highlight::Tok;
use crate::lsp::{self, CodeAction, CompletionItem, Event, SemKind, Server, ServerKind, Signature, Symbol};
use crate::make::{Diagnostic, Severity};
use crate::textops;

pub(super) struct Completion {
    doc_id: u64,
    /// Byte offset where the word being completed starts.
    anchor: usize,
    items: Vec<CompletionItem>,
    filtered: Vec<usize>,
    selected: usize,
    req: u64,
    utf8: bool,
    /// Waiting for the server's answer.
    loading: bool,
}

pub(super) struct Hover {
    req: u64,
    doc_id: u64,
    word: std::ops::Range<usize>,
    pos: egui::Pos2,
    text: Option<String>,
    since: f64,
    sent: bool,
}

/// Parameter hints for the call being typed.
pub(super) struct SigState {
    req: u64,
    doc_id: u64,
    line: usize,
    sig: Option<Signature>,
}

/// The Alt+Enter menu of quick fixes and refactorings.
pub(super) struct ActionMenu {
    doc_id: u64,
    items: Vec<CodeAction>,
    selected: usize,
    utf8: bool,
}

/// Go to Symbol: search functions, types and macros across the project.
pub(super) struct SymbolPicker {
    query: String,
    sent: Option<String>,
    changed_at: f64,
    req: u64,
    items: Vec<Symbol>,
    utf8: bool,
    selected: usize,
    focus: bool,
}

pub(super) struct RefHit {
    pub path: PathBuf,
    pub line: usize,
    pub col: usize,
    pub text: String,
    /// Definition, call, write... when it could be worked out.
    pub kind: Option<crate::refactor::UseKind>,
}

#[derive(Default)]
pub(super) struct LspState {
    servers: HashMap<ServerKind, Server>,
    missing: HashSet<ServerKind>,
    failed: HashSet<ServerKind>,
    root: Option<PathBuf>,
    pub status: String,
    /// Live diagnostics per file (columns already converted to chars).
    pub diags: HashMap<PathBuf, Vec<Diagnostic>>,
    pub completion: Option<Completion>,
    pub hover: Option<Hover>,
    next_req: u64,
    /// path -> (version, time first seen) for debounced sync.
    seen: HashMap<PathBuf, (u64, f64)>,
    /// Navigation history for "Back".
    pub back: Vec<(PathBuf, usize)>,
    pub references: Vec<RefHit>,
    /// Generated compile_commands.json directory for clangd, once ready.
    compile_db: Option<PathBuf>,
    db_job: Option<mpsc::Receiver<Result<(PathBuf, usize), String>>>,
    db_checked: bool,
    sem_requested: HashMap<PathBuf, u64>,
    pub sig: Option<SigState>,
    pub actions: Option<ActionMenu>,
    pub symbols: Option<SymbolPicker>,
    /// Modification times of clangd's settings files and the Makefile, to
    /// restart clangd when they change.
    config_stamp: Option<(u64, u64)>,
    config_checked_at: f64,
    /// When to re-send every open file after a multi-file edit.
    nudge_at: Option<f64>,
}

/// Hash of the modification times of clangd's settings files that apply to
/// `files` (every folder above each file), and of the Makefile.
fn config_stamp(root: &Path, files: &[PathBuf], makefile: Option<&Path>) -> (u64, u64) {
    use std::hash::{Hash, Hasher};
    let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let mut dirs: HashSet<PathBuf> = HashSet::new();
    dirs.insert(root.to_path_buf());
    dirs.insert(root.join("build"));
    for f in files {
        let mut d = f.parent();
        while let Some(dir) = d {
            // clangd also reads .clangd files above the project folder.
            if !dirs.insert(dir.to_path_buf()) {
                break;
            }
            d = dir.parent();
        }
    }
    let mut dirs: Vec<PathBuf> = dirs.into_iter().collect();
    dirs.sort();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for d in &dirs {
        for name in [".clangd", "compile_flags.txt", "compile_commands.json"] {
            let p = d.join(name);
            if let Some(t) = mtime(&p) {
                p.hash(&mut h);
                t.hash(&mut h);
            }
        }
    }
    let mut m = std::collections::hash_map::DefaultHasher::new();
    if let Some(t) = makefile.and_then(mtime) {
        t.hash(&mut m);
    }
    (h.finish(), m.finish())
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn word_at(text: &str, byte: usize) -> std::ops::Range<usize> {
    let byte = textops::floor_char_boundary(text, byte);
    let start = text[..byte]
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word_char(*c))
        .last()
        .map(|(i, _)| i)
        .unwrap_or(byte);
    let end = text[byte..]
        .char_indices()
        .take_while(|(_, c)| is_word_char(*c))
        .last()
        .map(|(i, c)| byte + i + c.len_utf8())
        .unwrap_or(byte);
    start..end
}

impl Completion {
    pub fn anchor_pos(&self) -> usize {
        self.anchor
    }
}

impl LspState {
    pub fn server_for(&mut self, path: &Path) -> Option<&mut Server> {
        let kind = ServerKind::for_path(path)?;
        self.servers.get_mut(&kind).filter(|s| !s.has_exited())
    }

    pub fn shutdown_all(&mut self) {
        self.servers.clear(); // Drop shuts each one down.
        self.diags.clear();
        self.completion = None;
        self.hover = None;
        self.sig = None;
        self.actions = None;
        self.seen.clear();
        self.sem_requested.clear();
        self.compile_db = None;
        self.db_job = None;
        self.db_checked = false;
        self.failed.clear();
        self.config_stamp = None;
    }
}

impl App {
    /// Called every frame: start servers, sync documents, handle replies.
    pub(super) fn update_lsp(&mut self, ctx: &egui::Context) {
        let root = self.tree.as_ref().map(|t| t.root.clone());
        if root != self.lsp.root {
            self.lsp.shutdown_all();
            self.lsp.root = root.clone();
        }
        let Some(root) = root else { return };
        let now = ctx.input(|i| i.time);

        // Restart clangd when its settings (.clangd, compile_commands.json, ...)
        // change, and re-read the Makefile's flags when the Makefile changes.
        if now - self.lsp.config_checked_at > 2.0 {
            self.lsp.config_checked_at = now;
            let files: Vec<PathBuf> = self
                .docs
                .iter()
                .filter_map(|d| d.path.clone())
                .filter(|p| ServerKind::for_path(p) == Some(ServerKind::Clangd))
                .collect();
            let stamp = config_stamp(&root, &files, self.makefile.as_deref());
            if let Some(old) = self.lsp.config_stamp
                && old != stamp
            {
                if old.1 != stamp.1 && self.lsp.compile_db.is_some() {
                    self.lsp.db_checked = false; // regenerate, then restart
                } else if old.0 != stamp.0 && self.lsp.servers.contains_key(&ServerKind::Clangd) {
                    self.lsp.servers.remove(&ServerKind::Clangd);
                    self.lsp.failed.remove(&ServerKind::Clangd);
                    self.lsp.seen.retain(|p, _| ServerKind::for_path(p) != Some(ServerKind::Clangd));
                    self.lsp.sem_requested.retain(|p, _| ServerKind::for_path(p) != Some(ServerKind::Clangd));
                    self.lsp.status = "clangd settings changed, restarting clangd".into();
                }
            }
            self.lsp.config_stamp = Some(stamp);
        }

        // compile_commands.json from the Makefile (clangd needs your flags).
        // Only in a trusted folder: `make -n` still runs $(shell ...) and `+` lines.
        if !self.lsp.db_checked && self.makefile.is_some() && !self.folder_trusted() && self.lsp.status.is_empty() {
            self.lsp.status = "clangd is using default flags (folder not trusted)".into();
        }
        if !self.lsp.db_checked && self.makefile.is_some() && self.folder_trusted() {
            self.lsp.db_checked = true;
            if !lsp::project_has_compile_db(&root)
                && let Some(out_dir) = lsp::cache_dir_for(&root)
            {
                let cfg = self.build_config();
                let program = crate::make::make_program(&cfg);
                let extra = crate::make::split_args(&cfg.extra_args);
                let dir = self.makefile.as_ref().and_then(|m| m.parent()).unwrap_or(&root).to_path_buf();
                let (tx, rx) = mpsc::channel();
                let ctx2 = ctx.clone();
                std::thread::spawn(move || {
                    let r = lsp::generate_compile_commands(&program, &dir, &extra, &out_dir).map(|n| (out_dir, n));
                    let _ = tx.send(r);
                    ctx2.request_repaint();
                });
                self.lsp.db_job = Some(rx);
                self.lsp.status = "Reading compiler flags from the Makefile…".into();
            }
        }
        if let Some(rx) = &self.lsp.db_job {
            match rx.try_recv() {
                Ok(Ok((dir, n))) => {
                    self.lsp.compile_db = Some(dir);
                    self.lsp.db_job = None;
                    self.lsp.status = format!("Found {n} compile commands in the Makefile");
                    // Restart clangd so it picks up the flags.
                    self.lsp.servers.remove(&ServerKind::Clangd);
                    self.lsp.seen.clear();
                }
                Ok(Err(e)) => {
                    self.lsp.db_job = None;
                    self.lsp.status = format!("clangd will use default flags ({e})");
                }
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => self.lsp.db_job = None,
            }
        }

        // Start servers needed by open documents.
        let needed: HashSet<ServerKind> = self
            .docs
            .iter()
            .filter_map(|d| d.path.as_deref().and_then(ServerKind::for_path))
            .collect();
        for kind in needed {
            let running = self.lsp.servers.get(&kind).is_some_and(|s| !s.has_exited());
            if running || self.lsp.missing.contains(&kind) || self.lsp.failed.contains(&kind) {
                continue;
            }
            if kind == ServerKind::Clangd && self.lsp.db_job.is_some() {
                continue; // wait for the compile database
            }
            let custom = match kind {
                ServerKind::Clangd => self.settings.clangd_path.trim().to_string(),
                ServerKind::GlslAnalyzer => self.settings.glsl_analyzer_path.trim().to_string(),
            };
            let found = if custom.is_empty() { lsp::find_server(kind) } else { Some(PathBuf::from(custom)) };
            let Some(bin) = found else {
                self.lsp.missing.insert(kind);
                continue;
            };
            let mut args: Vec<String> = Vec::new();
            if kind == ServerKind::Clangd {
                args.extend(["--background-index", "--header-insertion=never", "--log=error", "--pch-storage=memory"].map(String::from));
                if let Some(db) = &self.lsp.compile_db {
                    args.push(format!("--compile-commands-dir={}", db.display()));
                }
                args.extend(self.settings.clangd_args.iter().cloned());
            }
            match Server::start(kind, &bin, &root, &args, ctx.clone()) {
                Ok(s) => {
                    self.lsp.servers.insert(kind, s);
                    self.lsp.status = format!("Starting {}…", kind.name());
                }
                Err(e) => {
                    self.lsp.failed.insert(kind);
                    self.lsp.status = format!("Couldn't start {}: {e}", kind.name());
                }
            }
            // Re-send all documents to the new server.
            self.lsp.seen.retain(|p, _| ServerKind::for_path(p) != Some(kind));
        }

        if self.lsp.nudge_at.is_some_and(|t| now >= t) {
            self.lsp.nudge_at = None;
            for d in &mut self.docs {
                if d.path.as_deref().and_then(ServerKind::for_path).is_some() {
                    d.version += 1;
                }
            }
        }

        // Sync documents (debounced while typing).
        let open_paths: HashSet<PathBuf> = self.docs.iter().filter_map(|d| d.path.clone()).collect();
        let force_id = self.lsp.completion.as_ref().filter(|c| c.loading).map(|c| c.doc_id);
        for d in &self.docs {
            let Some(path) = &d.path else { continue };
            let Some(server) = self.lsp.servers.get_mut(&match ServerKind::for_path(path) {
                Some(k) => k,
                None => continue,
            }) else {
                continue;
            };
            let entry = self.lsp.seen.entry(path.clone()).or_insert((d.version, now));
            if entry.0 != d.version {
                *entry = (d.version, now);
            }
            let quiet = now - entry.1 > 0.3 || !server.is_open(path) || force_id == Some(d.id);
            if quiet && server.sent_version(path) != Some(d.version) {
                server.sync(path, &d.text, d.version);
            }
            // Semantic tokens once a version has been sent and settled.
            if server.supports_semantic
                && server.sent_version(path) == Some(d.version)
                && self.lsp.sem_requested.get(path) != Some(&d.version)
                && now - entry.1 > 0.5
            {
                server.semantic_tokens(path, d.version);
                self.lsp.sem_requested.insert(path.clone(), d.version);
            }
        }
        for server in self.lsp.servers.values_mut() {
            let stale: Vec<PathBuf> = self
                .lsp
                .seen
                .keys()
                .filter(|p| !open_paths.contains(*p) && server.is_open(p))
                .cloned()
                .collect();
            for p in stale {
                server.close(&p);
            }
        }
        self.lsp.seen.retain(|p, _| open_paths.contains(p));

        // Handle replies.
        let mut events = Vec::new();
        for s in self.lsp.servers.values_mut() {
            s.poll(&mut events);
        }
        for e in events {
            self.handle_lsp_event(ctx, e);
        }

        // Format-on-save that never got an answer: save anyway.
        let late: Vec<PathBuf> = self.save_after_format.iter().filter(|(_, t)| now - **t > 1.5).map(|(p, _)| p.clone()).collect();
        for p in late {
            self.save_after_format.remove(&p);
            if let Some(i) = self.docs.iter().position(|d| d.path.as_deref() == Some(p.as_path())) {
                self.save_doc_now(ctx, i);
            }
        }
        if !self.save_after_format.is_empty() {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }

        // Hover: ask after the pointer rests for a moment.
        if let Some(h) = &self.lsp.hover
            && !h.sent
        {
            ctx.request_repaint_after(std::time::Duration::from_millis(150));
        }
        if let Some(h) = &mut self.lsp.hover
            && !h.sent
            && now - h.since > 0.45
        {
            h.sent = true;
            let doc = self.docs.iter().find(|d| d.id == h.doc_id);
            if let Some(doc) = doc
                && let Some(path) = &doc.path
                && let Some(kind) = ServerKind::for_path(path)
                && let Some(server) = self.lsp.servers.get_mut(&kind)
                && server.sent_version(path) == Some(doc.version)
            {
                let pos = lsp::byte_to_position(&doc.text, h.word.start, server.utf8);
                server.hover(path, pos, h.req);
            }
        }
    }

    pub(super) fn handle_lsp_event(&mut self, ctx: &egui::Context, e: Event) {
        match e {
            Event::Diagnostics { path, mut diags } => {
                let utf8 = self.lsp.server_for(&path).map(|s| s.utf8).unwrap_or(false);
                // Convert LSP columns to char columns using the document text.
                let fix_hint = format!("(fix available: {})", self.keymap.label(ctx, crate::settings::Action::QuickFix));
                for d in &mut diags {
                    d.message = d.message.replace("(fix available)", &fix_hint);
                }
                if let Some(doc) = self.docs.iter_mut().find(|d| d.path.as_deref() == Some(path.as_path())) {
                    doc.sync();
                    for d in &mut diags {
                        if d.line >= doc.index.line_count() {
                            continue;
                        }
                        let r = doc.index.line_range(d.line);
                        let line = &doc.text[r];
                        let b = lsp::position_to_byte(line, 0, d.col as u32, utf8);
                        d.col = line[..b].chars().count();
                    }
                }
                if diags.is_empty() {
                    self.lsp.diags.remove(&path);
                } else {
                    self.lsp.diags.insert(path, diags);
                }
            }
            Event::Completion { req, items, utf8 } => {
                if let Some(c) = &mut self.lsp.completion
                    && c.req == req
                {
                    c.items = items;
                    c.utf8 = utf8;
                    c.loading = false;
                    c.items.sort_by(|a, b| a.sort.cmp(&b.sort));
                    self.refilter_completion();
                }
            }
            Event::Definition { locations, utf8 } => {
                if let Some(loc) = locations.first() {
                    self.jump_to_location(&loc.path, loc.line, loc.character, utf8, true);
                } else {
                    self.toast(ctx, "No definition found", false);
                }
            }
            Event::References { locations, utf8 } => {
                let mut hits = Vec::new();
                // Group by file so each file is read and tokenized once.
                let mut by_file: Vec<(PathBuf, Vec<lsp::Location>)> = Vec::new();
                for l in locations {
                    match by_file.iter_mut().find(|(p, _)| *p == l.path) {
                        Some((_, v)) => v.push(l),
                        None => by_file.push((l.path.clone(), vec![l])),
                    }
                }
                for (path, locs) in by_file {
                    let text = self
                        .docs
                        .iter()
                        .find(|d| d.path.as_deref() == Some(path.as_path()))
                        .map(|d| d.text.clone())
                        .or_else(|| std::fs::read_to_string(&path).ok())
                        .unwrap_or_default();
                    let c_like = ServerKind::for_path(&path) == Some(ServerKind::Clangd);
                    let src = c_like.then(|| crate::refactor::Src::new(&text));
                    let mut file_hits = Vec::new();
                    for l in locs {
                        let b = lsp::position_to_byte(&text, l.line, l.character, utf8);
                        let (line, col) = textops::line_col(&text, b);
                        let ls = textops::line_start(&text, b);
                        let le = textops::line_end(&text, b);
                        let kind = src.as_ref().and_then(|s| crate::refactor::classify(s, b));
                        file_hits.push(RefHit { path: l.path, line, col, text: text[ls..le].trim().to_string(), kind });
                    }
                    file_hits.sort_by_key(|h| (h.line, h.col));
                    hits.extend(file_hits);
                }
                let n = hits.len();
                self.lsp.references = hits;
                self.bottom_open = true;
                self.bottom_tab = BottomTab::References;
                self.toast(ctx, format!("{n} reference{}", if n == 1 { "" } else { "s" }), false);
            }
            Event::SignatureHelp { req, sig } => {
                if let Some(st) = &mut self.lsp.sig
                    && st.req == req
                {
                    if sig.is_some() {
                        st.sig = sig;
                    } else {
                        self.lsp.sig = None;
                    }
                }
            }
            Event::CodeActions { mut actions, utf8 } => {
                if self.take_extract_variable(ctx, &actions, utf8) {
                    return;
                }
                let Some(doc) = self.docs.get(self.active) else { return };
                if actions.is_empty() {
                    self.toast(ctx, "No quick fixes or refactorings here", false);
                    return;
                }
                // Preferred fixes first, then other fixes, then refactorings.
                actions.sort_by_key(|a| (!a.preferred, !a.is_fix));
                self.lsp.actions = Some(ActionMenu { doc_id: doc.id, items: actions, selected: 0, utf8 });
            }
            Event::Symbols { req, symbols, utf8 } => {
                if let Some(p) = &mut self.lsp.symbols
                    && p.req == req
                {
                    // `typedef struct Vec2 {...} Vec2;` comes back twice: keep one.
                    let mut seen = std::collections::HashSet::new();
                    p.items = symbols
                        .into_iter()
                        .filter(|s| seen.insert((s.name.clone(), s.location.path.clone(), s.location.line)))
                        .collect();
                    p.utf8 = utf8;
                    p.selected = 0;
                }
            }
            Event::Hover { req, text } => {
                if let Some(h) = &mut self.lsp.hover
                    && h.req == req
                {
                    h.text = Some(text);
                }
            }
            Event::Edits { edits, utf8, what } => {
                if self.should_preview(what, &edits) {
                    self.preview_edits(what, edits, utf8);
                    return;
                }
                let mut files = 0;
                for (path, list) in edits {
                    let then_save = what == "Format" && self.save_after_format.remove(&path).is_some();
                    if list.is_empty() {
                        if then_save && let Some(i) = self.docs.iter().position(|d| d.path.as_deref() == Some(path.as_path())) {
                            self.save_doc_now(ctx, i);
                        }
                        continue;
                    }
                    if !self.docs.iter().any(|d| d.path.as_deref() == Some(path.as_path())) {
                        let keep = self.active;
                        self.open_file(&path);
                        self.active = keep.min(self.docs.len().saturating_sub(1));
                    }
                    if let Some(doc) = self.docs.iter_mut().find(|d| d.path.as_deref() == Some(path.as_path())) {
                        let sel = doc.selection(ctx);
                        let b = if what == "Format" {
                            // A formatter rewrites whole lines: keep the line and column.
                            let (l, c) = textops::line_col(&doc.text, sel.start);
                            lsp::apply_edits(&mut doc.text, &list, utf8);
                            textops::line_col_to_byte(&doc.text, l, c)
                        } else {
                            lsp::apply_edits_keeping(&mut doc.text, &list, utf8, sel.start)
                        };
                        doc.mark_changed();
                        doc.pending_select = Some(b..b);
                        files += 1;
                    }
                    if then_save && let Some(i) = self.docs.iter().position(|d| d.path.as_deref() == Some(path.as_path())) {
                        self.save_doc_now(ctx, i);
                    }
                }
                if files > 1 {
                    // clangd doesn't re-check open files when a header they
                    // include changes in memory; nudge them once the edited
                    // files have been sent.
                    self.lsp.nudge_at = Some(ctx.input(|i| i.time) + 0.8);
                    ctx.request_repaint_after(std::time::Duration::from_millis(900));
                }
                let s = if files == 1 { "" } else { "s" };
                match what {
                    "Rename" => self.toast(ctx, format!("Renamed in {files} file{s}"), false),
                    "Format" | "Fix" => {}
                    _ => self.toast(ctx, format!("{what}: changed {files} file{s}"), false),
                }
                self.after_edits(ctx);
            }
            Event::SemanticTokens { path, version, tokens, utf8 } => {
                if let Some(doc) = self.docs.iter_mut().find(|d| d.path.as_deref() == Some(path.as_path()))
                    && doc.version == version
                {
                    doc.sync();
                    let mut out = Vec::with_capacity(tokens.len());
                    for t in tokens {
                        let tok = match t.kind {
                            SemKind::Type => Tok::Type,
                            SemKind::Function => Tok::Function,
                            SemKind::Macro | SemKind::Constant => Tok::Constant,
                            _ => continue,
                        };
                        if t.line as usize >= doc.index.line_count() {
                            continue;
                        }
                        let ls = doc.index.line_start(t.line as usize);
                        let le = doc.index.line_end(t.line as usize);
                        let line = &doc.text[ls..le];
                        let (s, e) = if utf8 {
                            (t.start as usize, (t.start + t.len) as usize)
                        } else {
                            let s = lsp::position_to_byte(line, 0, t.start, false);
                            let e = lsp::position_to_byte(line, 0, t.start + t.len, false);
                            (s, e)
                        };
                        if e <= line.len() && line.is_char_boundary(s) && line.is_char_boundary(e) {
                            out.push((ls + s..ls + e, tok));
                        }
                    }
                    out.sort_by_key(|(r, _)| r.start);
                    doc.semantic = out;
                    doc.semantic_version += 1;
                }
            }
            Event::SwitchSourceHeader(p) => match p.filter(|p| p.is_file()) {
                Some(p) => self.open_file(&p),
                None => self.switch_header_fallback(ctx),
            },
            Event::PrepareRename { req, result } => self.on_prepare_rename(ctx, req, result),
            Event::RefsFor { req, locations, utf8 } => self.on_refs_for(ctx, req, locations, utf8),
            Event::Calls { req, items } => self.on_calls(ctx, req, items),
            Event::Status(s) => self.lsp.status = s,
            Event::Exited(kind, s) => {
                self.lsp.status = s;
                self.lsp.failed.insert(kind);
                self.lsp.servers.remove(&kind);
            }
        }
    }

    pub(super) fn jump_to_location(&mut self, path: &Path, line: u32, character: u32, utf8: bool, record: bool) {
        if record && let Some(doc) = self.docs.get(self.active) && let Some(p) = &doc.path {
            let ctx_sel = doc.cursor_line_col;
            let b = textops::line_col_to_byte(&doc.text, ctx_sel.0, ctx_sel.1);
            self.lsp.back.push((p.clone(), b));
            if self.lsp.back.len() > 100 {
                self.lsp.back.remove(0);
            }
        }
        self.open_file(path);
        if let Some(doc) = self.docs.get_mut(self.active)
            && doc.path.as_deref() == Some(path)
        {
            let b = lsp::position_to_byte(&doc.text, line, character, utf8);
            doc.pending_select = Some(b..b);
        }
    }

    pub(super) fn nav_back(&mut self) {
        if let Some((p, b)) = self.lsp.back.pop() {
            self.open_file(&p);
            if let Some(doc) = self.docs.get_mut(self.active)
                && doc.path.as_deref() == Some(p.as_path())
            {
                let b = textops::floor_char_boundary(&doc.text, b);
                doc.pending_select = Some(b..b);
            }
        }
    }

    /// Run `f` with the active document's server and LSP position of the cursor.
    pub(super) fn with_server_at_cursor(&mut self, ctx: &egui::Context, f: impl FnOnce(&mut Server, &Path, (u32, u32))) -> bool {
        let Some(doc) = self.docs.get(self.active) else { return false };
        let Some(path) = doc.path.clone() else { return false };
        let sel = doc.selection(ctx);
        let (text, version) = (doc.text.clone(), doc.version);
        let Some(server) = self.lsp.server_for(&path) else {
            let msg = match ServerKind::for_path(&path) {
                Some(k) if self.lsp.missing.contains(&k) => format!("{} not found. {}", k.name(), k.install_hint()),
                Some(k) => format!("{} is not running yet", k.name()),
                None => "No language server for this file type".to_string(),
            };
            self.toast(ctx, msg, true);
            return false;
        };
        server.sync(&path, &text, version);
        let pos = lsp::byte_to_position(&text, sel.start, server.utf8);
        f(server, &path, pos);
        true
    }

    pub(super) fn cmd_definition_at(&mut self, ctx: &egui::Context, byte: Option<usize>) {
        if let Some(b) = byte
            && let Some(doc) = self.docs.get_mut(self.active)
        {
            // Put the cursor where the user clicked first.
            doc.pending_select = Some(b..b);
            doc.set_selection(b..b);
        }
        self.with_server_at_cursor(ctx, |s, p, pos| s.definition(p, pos));
    }

    pub(super) fn cmd_declaration(&mut self, ctx: &egui::Context) {
        self.with_server_at_cursor(ctx, |s, p, pos| s.declaration(p, pos));
    }

    /// Alt+Enter: ask for quick fixes and refactorings at the cursor.
    pub(super) fn cmd_quick_fix(&mut self, ctx: &egui::Context) {
        let Some(doc) = self.docs.get(self.active) else { return };
        let sel = doc.selection(ctx);
        let (s, e) = (sel.start, sel.end);
        let text = doc.text.clone();
        self.lsp.completion = None;
        self.with_server_at_cursor(ctx, |srv, p, _| {
            let a = lsp::byte_to_position(&text, s, srv.utf8);
            let b = lsp::byte_to_position(&text, e, srv.utf8);
            srv.code_actions(p, a, b);
        });
    }

    pub(super) fn apply_code_action(&mut self, ctx: &egui::Context, a: CodeAction, utf8: bool) {
        if let Some(edits) = a.edit {
            self.handle_lsp_event(ctx, Event::Edits { edits, utf8, what: "Fix" });
        }
        if let Some(cmd) = a.command
            && let Some(path) = self.docs.get(self.active).and_then(|d| d.path.clone())
            && let Some(server) = self.lsp.server_for(&path)
        {
            server.execute_command(&cmd);
        }
    }

    pub(super) fn cmd_goto_symbol(&mut self, ctx: &egui::Context) {
        if !self.lsp.servers.values().any(|s| s.ready) {
            self.toast(ctx, "The language server isn't running yet", true);
            return;
        }
        // Start with the word under the cursor.
        let query = self
            .docs
            .get(self.active)
            .map(|d| {
                let sel = d.selected_text(ctx);
                if !sel.is_empty() && !sel.contains('\n') { sel } else { d.text[word_at(&d.text, d.cursor_byte.min(d.text.len()))].to_string() }
            })
            .unwrap_or_default();
        self.lsp.symbols = Some(SymbolPicker { query, sent: None, changed_at: 0.0, req: 0, items: Vec::new(), utf8: false, selected: 0, focus: true });
    }

    /// Parameter hints: ask after `(` or `,`, close after `)` or leaving the line.
    fn update_signature(&mut self, typed: &str, cursor: usize) {
        let Some(doc) = self.docs.get(self.active) else { return };
        let Some(path) = doc.path.clone() else { return };
        let line = doc.text[..cursor.min(doc.text.len())].matches('\n').count();
        let last = typed.chars().last();
        let open = self.lsp.sig.as_ref().is_some_and(|s| s.doc_id == doc.id && s.line == line);
        if !open {
            self.lsp.sig = None;
        }
        let ask = matches!(last, Some('(' | ',')) || (open && !typed.is_empty());
        if last == Some(')') && open {
            // Still inside an outer call? Ask again; the server closes it if not.
        } else if !ask {
            return;
        }
        let (doc_id, version, text) = (doc.id, doc.version, doc.text.clone());
        self.lsp.next_req += 1;
        let req = self.lsp.next_req;
        let Some(server) = self.lsp.server_for(&path) else { return };
        server.sync(&path, &text, version);
        let pos = lsp::byte_to_position(&text, cursor, server.utf8);
        server.signature_help(&path, pos, req);
        let sig = self.lsp.sig.take().filter(|s| s.doc_id == doc_id).and_then(|s| s.sig);
        self.lsp.sig = Some(SigState { req, doc_id, line, sig });
    }

    pub(super) fn cmd_references(&mut self, ctx: &egui::Context) {
        self.with_server_at_cursor(ctx, |s, p, pos| s.references(p, pos));
    }

    pub(super) fn cmd_rename(&mut self, ctx: &egui::Context) {
        let Some(doc) = self.docs.get(self.active) else { return };
        let sel = doc.selection(ctx);
        let w = word_at(&doc.text, sel.start);
        if w.is_empty() {
            return;
        }
        let name = doc.text[w].to_string();
        self.start_rename(ctx, name);
    }

    /// Returns false if the name was refused (the rename box stays open).
    pub(super) fn do_rename(&mut self, ctx: &egui::Context, new_name: String) -> bool {
        if !self.check_new_name(ctx, &new_name) {
            return false;
        }
        self.with_server_at_cursor(ctx, |s, p, pos| s.rename(p, pos, &new_name));
        true
    }

    pub(super) fn cmd_format(&mut self, ctx: &egui::Context) {
        let hard = self.docs.get(self.active).is_some_and(|d| d.lang.uses_hard_tabs());
        self.with_server_at_cursor(ctx, |s, p, _| s.format(p, 4, !hard));
    }

    pub(super) fn cmd_switch_header(&mut self, ctx: &egui::Context) {
        let ok = self.docs.get(self.active).and_then(|d| d.path.clone()).is_some_and(|p| {
            ServerKind::for_path(&p) == Some(ServerKind::Clangd) && self.lsp.server_for(&p).is_some()
        });
        if ok {
            self.with_server_at_cursor(ctx, |s, p, _| s.switch_source_header(p));
        } else {
            self.switch_header_fallback(ctx);
        }
    }

    /// Swap .c <-> .h next to the current file.
    fn switch_header_fallback(&mut self, ctx: &egui::Context) {
        let Some(path) = self.docs.get(self.active).and_then(|d| d.path.clone()) else { return };
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default();
        let others: &[&str] = match ext {
            "h" => &["c", "m", "cc", "cpp", "mm"],
            "c" | "cc" | "cpp" | "m" | "mm" => &["h", "hpp"],
            _ => &[],
        };
        for o in others {
            let p = path.with_extension(o);
            if p.is_file() {
                self.open_file(&p);
                return;
            }
        }
        self.toast(ctx, "No matching header or source file", false);
    }

    pub(super) fn cmd_restart_servers(&mut self) {
        self.lsp.shutdown_all();
        self.lsp.missing.clear();
        self.lsp.root = None;
        for d in &mut self.docs {
            d.semantic.clear();
        }
    }

    // ----- completion ------------------------------------------------------

    /// Start or update completion after the user typed `typed`.
    pub(super) fn on_typed(&mut self, ctx: &egui::Context, typed: &str, cursor: usize, manual: bool) {
        if !manual {
            self.lsp.actions = None;
            self.update_signature(typed, cursor);
        }
        let Some(doc) = self.docs.get(self.active) else { return };
        let Some(path) = doc.path.clone() else { return };
        let text = &doc.text;
        let cursor = textops::floor_char_boundary(text, cursor);
        let w = word_at(text, cursor);
        let word_start = if w.start < cursor { w.start } else { cursor };
        let last = typed.chars().last();
        let prev2 = &text[..cursor];
        let line_before = prev2[textops::line_start(text, cursor)..].trim_start();
        let in_include = line_before.strip_prefix('#').is_some_and(|r| r.trim_start().starts_with("include"));
        let trigger = match last {
            Some('.') if !in_include => Some('.'),
            Some('>') if prev2.ends_with("->") => Some('>'),
            // Header names after #include < or ", and folders after /.
            Some(c @ ('<' | '"' | '/')) if in_include => Some(c),
            _ => None,
        };
        // Keep an active completion if we're still in the same word.
        if let Some(c) = &self.lsp.completion
            && c.doc_id == doc.id
            && !manual
            && trigger.is_none()
        {
            if cursor >= c.anchor && text[c.anchor..cursor].chars().all(is_word_char) {
                self.refilter_completion();
                return;
            }
            self.lsp.completion = None;
        }
        let ident_typed = last.is_some_and(is_word_char) && cursor - word_start >= 2;
        if !(manual || trigger.is_some() || ident_typed) {
            return;
        }
        let (doc_id, version, text_owned) = (doc.id, doc.version, doc.text.clone());
        self.lsp.next_req += 1;
        let req = self.lsp.next_req;
        let Some(server) = self.lsp.server_for(&path) else { return };
        server.sync(&path, &text_owned, version);
        let pos = lsp::byte_to_position(&text_owned, cursor, server.utf8);
        server.completion(&path, pos, trigger, req);
        self.lsp.completion = Some(Completion {
            doc_id,
            anchor: if trigger.is_some() { cursor } else { word_start },
            items: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
            req,
            utf8: server.utf8,
            loading: true,
        });
        let _ = ctx;
    }

    fn refilter_completion(&mut self) {
        let Some(c) = &mut self.lsp.completion else { return };
        let Some(doc) = self.docs.iter().find(|d| d.id == c.doc_id) else { return };
        let cursor = doc.cursor_byte.min(doc.text.len());
        if cursor < c.anchor {
            self.lsp.completion = None;
            return;
        }
        let prefix = doc.text[c.anchor..cursor].to_lowercase();
        let mut scored: Vec<(i64, usize)> = c
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| {
                let f = it.filter.to_lowercase();
                if prefix.is_empty() {
                    return Some((0, i));
                }
                if f.starts_with(&prefix) {
                    Some((1000 - i as i64 / 10, i))
                } else {
                    crate::project::fuzzy_score(&f, &prefix).map(|s| (s, i))
                }
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        c.filtered = scored.into_iter().map(|(_, i)| i).take(100).collect();
        c.selected = 0;
        if !c.loading && c.filtered.is_empty() {
            self.lsp.completion = None;
        }
    }

    /// Keys for the completion popup; must run before the editor sees them.
    pub(super) fn completion_keys(&mut self, ctx: &egui::Context) {
        if self.refactor_menu_keys(ctx) {
            return;
        }
        if let Some(m) = &mut self.lsp.actions {
            let n = m.items.len();
            let (mut pick, mut close) = (None, false);
            ctx.input_mut(|i| {
                if i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                    m.selected = (m.selected + 1) % n;
                }
                if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                    m.selected = (m.selected + n - 1) % n;
                }
                if i.consume_key(Modifiers::NONE, Key::Enter) || i.consume_key(Modifiers::NONE, Key::Tab) {
                    pick = Some(m.selected);
                }
                if i.consume_key(Modifiers::NONE, Key::Escape) {
                    close = true;
                }
            });
            if let Some(i) = pick {
                let m = self.lsp.actions.take().expect("menu open");
                let utf8 = m.utf8;
                if let Some(a) = m.items.into_iter().nth(i) {
                    self.apply_code_action(ctx, a, utf8);
                }
            }
            if close {
                self.lsp.actions = None;
                if let Some(d) = self.docs.get_mut(self.active) {
                    d.focus();
                }
            }
            return;
        }
        if self.lsp.sig.is_some() && self.lsp.completion.is_none() && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape)) {
            self.lsp.sig = None;
            if let Some(d) = self.docs.get_mut(self.active) {
                d.focus();
            }
            return;
        }
        let Some(c) = &mut self.lsp.completion else { return };
        let Some(doc) = self.docs.get(self.active) else { return };
        if doc.id != c.doc_id {
            return;
        }
        if !ctx.memory(|m| m.has_focus(doc.text_edit_id())) {
            // egui drops focus on Escape before we see the key; treat that
            // as "close the popup" and give focus back to the editor.
            if ctx.input(|i| i.key_pressed(Key::Escape)) {
                self.lsp.completion = None;
                if let Some(d) = self.docs.get_mut(self.active) {
                    d.focus();
                }
            }
            return;
        }
        let n = c.filtered.len();
        let mut accept = false;
        let mut close = false;
        ctx.input_mut(|i| {
            if n > 0 && i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                c.selected = (c.selected + 1) % n;
            }
            if n > 0 && i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                c.selected = (c.selected + n - 1) % n;
            }
            if n > 0 && (i.consume_key(Modifiers::NONE, Key::Enter) || i.consume_key(Modifiers::NONE, Key::Tab)) {
                accept = true;
            }
            if i.consume_key(Modifiers::NONE, Key::Escape) {
                close = true;
            }
        });
        if accept {
            self.accept_completion(ctx);
        } else if close {
            self.lsp.completion = None;
            // egui drops focus on Escape; keep typing in the editor.
            if let Some(d) = self.docs.get_mut(self.active) {
                d.focus();
            }
        }
    }

    fn accept_completion(&mut self, ctx: &egui::Context) {
        let Some(c) = self.lsp.completion.take() else { return };
        let Some(&idx) = c.filtered.get(c.selected) else { return };
        let item = c.items[idx].clone();
        let Some(doc) = self.docs.iter_mut().find(|d| d.id == c.doc_id) else { return };
        let cursor = doc.selection(ctx).end.min(doc.text.len());
        let start = match item.range {
            Some((l, ch, _, _)) => lsp::position_to_byte(&doc.text, l, ch, c.utf8).min(cursor),
            None => c.anchor.min(cursor),
        };
        // Also replace the rest of the identifier after the cursor.
        let end = cursor
            + doc.text[cursor..]
                .char_indices()
                .take_while(|(_, ch)| is_word_char(*ch))
                .last()
                .map(|(i, ch)| i + ch.len_utf8())
                .unwrap_or(0);
        let insert = item.insert.trim().to_string();
        doc.text.replace_range(start..end, &insert);
        doc.mark_changed();
        let b = start + insert.len();
        doc.pending_select = Some(b..b);
    }

    pub(super) fn completion_ui(&mut self, ctx: &egui::Context, cursor_rect: Option<egui::Rect>) {
        let Some(c) = &mut self.lsp.completion else { return };
        let Some(rect) = cursor_rect else { return };
        if self.docs.get(self.active).map(|d| d.id) != Some(c.doc_id) {
            return;
        }
        if c.filtered.is_empty() && !c.loading {
            return;
        }
        let theme = &self.theme;
        let mut clicked = None;
        egui::Area::new(egui::Id::unique("completion"))
            .order(egui::Order::Foreground)
            .fixed_pos(rect.left_bottom() + egui::vec2(0.0, 2.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).inner_margin(egui::Margin::same(2)).show(ui, |ui| {
                    ui.set_min_width(320.0);
                    if c.loading && c.filtered.is_empty() {
                        ui.label(RichText::new("…").weak());
                        return;
                    }
                    egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
                        for (row, &i) in c.filtered.iter().enumerate() {
                            let it = &c.items[i];
                            let (tag, color) = kind_tag(it.kind, theme);
                            let mut job = egui::text::LayoutJob::default();
                            let mono = egui::FontId::monospace(12.5);
                            job.append(&format!("{tag} "), 0.0, egui::TextFormat::simple(mono.clone(), color));
                            job.append(&it.label, 0.0, egui::TextFormat::simple(mono.clone(), theme.text));
                            if !it.detail.is_empty() {
                                job.append(&format!("  {}", it.detail), 0.0, egui::TextFormat::simple(egui::FontId::monospace(11.5), theme.comment));
                            }
                            let r = ui.add(egui::Button::selectable(row == c.selected, job).frame_when_inactive(false));
                            if row == c.selected {
                                r.scroll_to_me(None);
                            }
                            if r.clicked() {
                                clicked = Some(row);
                            }
                        }
                    });
                });
            });
        if let Some(row) = clicked {
            if let Some(c) = &mut self.lsp.completion {
                c.selected = row;
            }
            self.accept_completion(ctx);
        }
    }

    /// Parameter hints above the cursor.
    pub(super) fn signature_ui(&mut self, ctx: &egui::Context, cursor_rect: Option<egui::Rect>) {
        let Some(st) = &self.lsp.sig else { return };
        let Some(sig) = &st.sig else { return };
        let Some(rect) = cursor_rect else { return };
        let Some(doc) = self.docs.get(self.active) else { return };
        if doc.id != st.doc_id || doc.cursor_line_col.0 != st.line {
            return;
        }
        let theme = &self.theme;
        egui::Area::new(egui::Id::unique("signature-help"))
            .order(egui::Order::Tooltip)
            .pivot(egui::Align2::LEFT_BOTTOM)
            .fixed_pos(rect.left_top() - egui::vec2(0.0, 4.0))
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_max_width(640.0);
                    let mono = egui::FontId::monospace(12.5);
                    let mut job = egui::text::LayoutJob::default();
                    match sig.active.filter(|(a, b)| a <= b && *b <= sig.label.len()) {
                        Some((a, b)) => {
                            job.append(&sig.label[..a], 0.0, egui::TextFormat::simple(mono.clone(), theme.text));
                            let mut f = egui::TextFormat::simple(mono.clone(), theme.warning);
                            f.underline = egui::Stroke::new(1.0, theme.warning);
                            job.append(&sig.label[a..b], 0.0, f);
                            job.append(&sig.label[b..], 0.0, egui::TextFormat::simple(mono.clone(), theme.text));
                        }
                        None => job.append(&sig.label, 0.0, egui::TextFormat::simple(mono.clone(), theme.text)),
                    }
                    ui.label(job);
                    let doc_text: String = sig.doc.lines().take(6).collect::<Vec<_>>().join("\n");
                    if !doc_text.trim().is_empty() {
                        ui.label(RichText::new(doc_text.trim()).small().color(theme.comment));
                    }
                });
            });
    }

    /// The Alt+Enter menu, at the cursor.
    pub(super) fn code_actions_ui(&mut self, ctx: &egui::Context, cursor_rect: Option<egui::Rect>) {
        let Some(m) = &self.lsp.actions else { return };
        if self.docs.get(self.active).map(|d| d.id) != Some(m.doc_id) {
            self.lsp.actions = None;
            return;
        }
        let Some(rect) = cursor_rect else { return };
        let theme = &self.theme;
        let mut pick = None;
        let resp = egui::Area::new(egui::Id::unique("code-actions"))
            .order(egui::Order::Foreground)
            .fixed_pos(rect.left_bottom() + egui::vec2(0.0, 2.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).inner_margin(egui::Margin::same(2)).show(ui, |ui| {
                    ui.set_min_width(260.0);
                    for (i, a) in m.items.iter().enumerate() {
                        let color = if a.is_fix { theme.warning } else { theme.text };
                        let tag = if a.is_fix { "fix  " } else { "     " };
                        let mut job = egui::text::LayoutJob::default();
                        job.append(tag, 0.0, egui::TextFormat::simple(egui::FontId::monospace(11.5), theme.comment));
                        job.append(&a.title, 0.0, egui::TextFormat::simple(egui::FontId::proportional(13.5), color));
                        if ui.add(egui::Button::selectable(i == m.selected, job).frame_when_inactive(i == m.selected)).clicked() {
                            pick = Some(i);
                        }
                    }
                });
            });
        let clicked_outside = ctx.input(|i| i.pointer.any_pressed()) && !resp.response.contains_pointer();
        if let Some(i) = pick {
            let m = self.lsp.actions.take().expect("menu open");
            let utf8 = m.utf8;
            if let Some(a) = m.items.into_iter().nth(i) {
                self.apply_code_action(ctx, a, utf8);
            }
        } else if clicked_outside {
            self.lsp.actions = None;
        }
    }

    /// Go to Symbol: a search box over the project's functions, types and macros.
    pub(super) fn symbols_ui(&mut self, ctx: &egui::Context) {
        let Some(p) = &mut self.lsp.symbols else { return };
        let now = ctx.input(|i| i.time);
        // Ask the server once typing pauses.
        if p.sent.as_deref() != Some(p.query.as_str()) && now - p.changed_at > 0.15 {
            let q = p.query.trim().to_string();
            self.lsp.next_req += 1;
            p.req = self.lsp.next_req;
            p.sent = Some(p.query.clone());
            let kind = if self.lsp.servers.get(&ServerKind::Clangd).is_some_and(|s| s.ready) {
                Some(ServerKind::Clangd)
            } else {
                self.lsp.servers.iter().find(|(_, s)| s.ready).map(|(k, _)| *k)
            };
            if let Some(s) = kind.and_then(|k| self.lsp.servers.get_mut(&k)) {
                if q.is_empty() {
                    p.items.clear();
                } else {
                    s.workspace_symbols(&q, p.req);
                }
            }
        } else if p.sent.as_deref() != Some(p.query.as_str()) {
            ctx.request_repaint_after(std::time::Duration::from_millis(60));
        }
        let mut chosen: Option<usize> = None;
        let mut close = false;
        let n = p.items.len();
        ctx.input_mut(|i| {
            if n > 0 && i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                p.selected = (p.selected + 1).min(n - 1);
            }
            if i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                p.selected = p.selected.saturating_sub(1);
            }
            if i.consume_key(Modifiers::NONE, Key::Enter) && n > 0 {
                chosen = Some(p.selected);
            }
            if i.consume_key(Modifiers::NONE, Key::Escape) {
                close = true;
            }
        });
        let root = self.tree.as_ref().map(|t| t.root.clone());
        let theme = &self.theme;
        let modal = egui::Modal::new(egui::Id::unique("goto-symbol")).show(ctx, |ui| {
            ui.set_width(620.0);
            let r = ui.add(egui::TextEdit::singleline(&mut p.query).hint_text("Go to symbol: function, struct, macro, variable…").desired_width(f32::INFINITY));
            if p.focus {
                // Select the prefilled word so typing replaces it.
                r.request_focus();
                let mut state = egui::TextEdit::load_state(ui.ctx(), r.id).unwrap_or_default();
                let n = p.query.chars().count();
                state.cursor.set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(n))));
                egui::TextEdit::store_state(ui.ctx(), r.id, state);
                p.focus = false;
            }
            if r.changed() {
                p.changed_at = now;
            }
            ui.separator();
            egui::ScrollArea::vertical().max_height(380.0).show(ui, |ui| {
                for (i, s) in p.items.iter().enumerate() {
                    let rel = root.as_ref().and_then(|r| s.location.path.strip_prefix(r).ok()).unwrap_or(&s.location.path);
                    let (tag, color) = symbol_tag(s.kind, theme);
                    let mut job = egui::text::LayoutJob::default();
                    job.append(&format!("{tag} "), 0.0, egui::TextFormat::simple(egui::FontId::monospace(12.5), color));
                    job.append(&s.name, 0.0, egui::TextFormat::simple(egui::FontId::monospace(13.5), theme.text));
                    let place = if s.container.is_empty() { String::new() } else { format!("  in {}", s.container) };
                    job.append(&format!("{place}   {}:{}", rel.display(), s.location.line + 1), 0.0, egui::TextFormat::simple(egui::FontId::proportional(12.0), theme.gutter_fg));
                    let r = ui.add(egui::Button::selectable(i == p.selected, job).frame_when_inactive(false));
                    if i == p.selected {
                        r.scroll_to_me(None);
                    }
                    if r.clicked() {
                        chosen = Some(i);
                    }
                }
                if p.items.is_empty() {
                    let msg = if p.query.trim().is_empty() { "Type part of a name" } else { "No matching symbols (the project index may still be building)" };
                    ui.label(RichText::new(msg).weak());
                }
            });
        });
        if modal.should_close() {
            close = true;
        }
        if let Some(i) = chosen {
            let p = self.lsp.symbols.take().expect("picker open");
            if let Some(s) = p.items.get(i) {
                let l = s.location.clone();
                self.jump_to_location(&l.path, l.line, l.character, p.utf8, true);
            }
        } else if close {
            self.lsp.symbols = None;
            self.focus_active();
        }
    }

    // ----- hover -----------------------------------------------------------

    pub(super) fn on_hover(&mut self, now: f64, hover: Option<(usize, egui::Pos2)>) {
        let Some(doc) = self.docs.get(self.active) else {
            self.lsp.hover = None;
            return;
        };
        let Some((b, pos)) = hover else {
            self.lsp.hover = None;
            return;
        };
        if doc.path.as_deref().and_then(ServerKind::for_path).is_none() {
            return;
        }
        let w = word_at(&doc.text, b);
        if w.is_empty() {
            self.lsp.hover = None;
            return;
        }
        if let Some(h) = &mut self.lsp.hover
            && h.doc_id == doc.id
            && h.word == w
        {
            if h.text.is_none() {
                h.pos = pos;
            }
            return;
        }
        self.lsp.next_req += 1;
        self.lsp.hover = Some(Hover { req: self.lsp.next_req, doc_id: doc.id, word: w, pos, text: None, since: now, sent: false });
    }

    pub(super) fn hover_ui(&mut self, ctx: &egui::Context) {
        let Some(h) = &self.lsp.hover else { return };
        let Some(text) = h.text.as_ref().filter(|t| !t.is_empty()) else { return };
        if self.lsp.completion.is_some() || self.refactor_busy() {
            return;
        }
        let text = text.clone();
        egui::Area::new(egui::Id::unique("hover"))
            .order(egui::Order::Tooltip)
            .fixed_pos(h.pos + egui::vec2(12.0, 18.0))
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_max_width(560.0);
                    let shown: String = text.lines().take(30).collect::<Vec<_>>().join("\n");
                    ui.label(RichText::new(shown).monospace().size(12.5));
                });
            });
    }

    // ----- panels & status -------------------------------------------------

    pub(super) fn references_panel(&mut self, ui: &mut egui::Ui) {
        if self.lsp.references.is_empty() {
            ui.label(RichText::new("Use Find References (Shift+F12) on a symbol.").weak());
            return;
        }
        use crate::refactor::UseKind;
        let root = self.tree.as_ref().map(|t| t.root.clone());
        let theme = self.theme.clone();
        let mut open = None;
        let font = egui::FontId::monospace(12.5);
        let refs = &self.lsp.references;
        // Summary: how many of each kind, across how many files.
        let mut files: Vec<&Path> = refs.iter().map(|r| r.path.as_path()).collect();
        files.dedup();
        let count = |k: UseKind| refs.iter().filter(|r| r.kind == Some(k)).count();
        let mut parts = vec![format!("{} usage{} in {} file{}", refs.len(), if refs.len() == 1 { "" } else { "s" }, files.len(), if files.len() == 1 { "" } else { "s" })];
        for k in [UseKind::Call, UseKind::Write, UseKind::Read, UseKind::Declaration, UseKind::Definition] {
            let n = count(k);
            if n > 0 {
                parts.push(format!("{n} {}{}", k.label(), if n == 1 { "" } else { "s" }));
            }
        }
        ui.label(RichText::new(parts.join(" · ")).color(theme.comment));
        let tag_color = |k: Option<UseKind>| match k {
            Some(UseKind::Write) => theme.warning,
            Some(UseKind::Definition | UseKind::Declaration) => theme.keyword,
            Some(UseKind::Call) => theme.function,
            _ => theme.comment,
        };
        egui::ScrollArea::both().auto_shrink([false, false]).show(ui, |ui| {
            let mut last: Option<&Path> = None;
            for (i, r) in refs.iter().enumerate() {
                if last != Some(r.path.as_path()) {
                    last = Some(r.path.as_path());
                    let rel = root.as_ref().and_then(|x| r.path.strip_prefix(x).ok()).unwrap_or(&r.path);
                    let n = refs.iter().filter(|x| x.path == r.path).count();
                    ui.label(RichText::new(format!("{}  ({n})", rel.display())).strong().color(theme.function));
                }
                let mut job = egui::text::LayoutJob::default();
                job.append(&format!("{:>6}  ", r.line + 1), 0.0, egui::TextFormat::simple(font.clone(), theme.comment));
                let tag = r.kind.map(|k| k.label()).unwrap_or("");
                job.append(&format!("{tag:<11}  "), 0.0, egui::TextFormat::simple(egui::FontId::monospace(11.0), tag_color(r.kind)));
                job.append(&r.text, 0.0, egui::TextFormat::simple(font.clone(), theme.text));
                if ui.add(egui::Button::selectable(false, job).frame_when_inactive(false)).clicked() {
                    open = Some(i);
                }
            }
        });
        if let Some(i) = open {
            let r = &self.lsp.references[i];
            let (p, l, c) = (r.path.clone(), r.line, r.col);
            self.open_file(&p);
            if let Some(d) = self.docs.get_mut(self.active)
                && d.path.as_deref() == Some(p.as_path())
            {
                d.goto_line_col(l, c);
            }
        }
    }

    /// Language-server status for the status bar, with a tooltip.
    pub(super) fn lsp_status_ui(&mut self, ui: &mut egui::Ui) {
        let Some(path) = self.docs.get(self.active).and_then(|d| d.path.clone()) else { return };
        let Some(kind) = ServerKind::for_path(&path) else { return };
        let (text, color, tip) = if self.lsp.missing.contains(&kind) {
            (format!("{} not found", kind.name()), self.theme.warning, kind.install_hint().to_string())
        } else if self.lsp.failed.contains(&kind) {
            (format!("{} stopped", kind.name()), self.theme.error, format!("{}\nUse Code → Restart Language Servers.", self.lsp.status))
        } else if self.lsp.servers.get(&kind).is_some_and(|s| s.ready) {
            (kind.name().to_string(), self.theme.comment, self.lsp.status.clone())
        } else {
            (format!("{}…", kind.name()), self.theme.comment, self.lsp.status.clone())
        };
        ui.label(RichText::new(text).color(color)).on_hover_text(tip);
    }

    /// Live diagnostics for the Problems panel (files other than build output).
    pub(super) fn live_diagnostics(&self) -> Vec<Diagnostic> {
        let mut v: Vec<Diagnostic> = self.lsp.diags.values().flatten().filter(|d| d.severity != Severity::Note).cloned().collect();
        v.sort_by(|a, b| a.path.cmp(&b.path).then(a.line.cmp(&b.line)));
        v
    }
}

/// LSP SymbolKind -> short tag.
fn symbol_tag(kind: u32, theme: &crate::theme::Theme) -> (&'static str, egui::Color32) {
    match kind {
        12 | 6 | 9 => ("f", theme.function),   // function, method, constructor
        23 | 5 | 11 | 26 => ("T", theme.ty), // struct, class, interface, type parameter
        10 => ("E", theme.ty),                 // enum
        22 | 14 => ("c", theme.constant),      // enum member, constant
        13 => ("v", theme.text),               // variable
        8 | 7 => ("m", theme.text),            // field, property
        _ => ("·", theme.comment),
    }
}

fn kind_tag(kind: u32, theme: &crate::theme::Theme) -> (&'static str, egui::Color32) {
    match kind {
        2..=4 => ("f", theme.function),     // method, function, constructor
        5 | 10 => ("m", theme.text),            // field, property
        6 => ("v", theme.text),                 // variable
        7 | 22 | 25 => ("T", theme.ty),         // class, struct, type param
        13 => ("E", theme.ty),                  // enum
        20 | 21 => ("c", theme.constant),       // enum member, constant
        14 => ("k", theme.keyword),             // keyword
        15 => ("s", theme.string),              // snippet
        17 => ("#", theme.preproc),             // file
        _ => ("·", theme.comment),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words() {
        let t = "int foo_bar = 1;";
        assert_eq!(word_at(t, 6), 4..11);
        assert_eq!(word_at(t, 4), 4..11);
        assert_eq!(word_at(t, 11), 4..11);
        assert_eq!(word_at(t, 12), 12..12);
    }

    #[test]
    fn clangd_config_changes_are_noticed() {
        let d = std::env::temp_dir().join(format!("cedit-cfg-{}", std::process::id()));
        std::fs::create_dir_all(d.join("src")).unwrap();
        let f = d.join("src/a.m");
        std::fs::write(&f, "").unwrap();
        let a = config_stamp(&d, std::slice::from_ref(&f), None);
        std::fs::write(d.join(".clangd"), "CompileFlags:\n  Add: [-xc]\n").unwrap();
        let b = config_stamp(&d, std::slice::from_ref(&f), None);
        assert_ne!(a.0, b.0);
        assert_eq!(a.1, b.1);
        let _ = std::fs::remove_dir_all(&d);
    }
}
