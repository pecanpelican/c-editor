//! Project-wide helpers: the file tree, listing files, and find-in-files.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

use egui::{RichText, Ui};

use crate::theme::Theme;

const HIDDEN_NAMES: &[&str] = &[".git", ".DS_Store", ".cache", ".hg", ".svn", "Thumbs.db"];
const SKIP_DIRS_IN_SEARCH: &[&str] = &["node_modules", "target", "__pycache__"];

fn is_hidden_entry(name: &str) -> bool {
    HIDDEN_NAMES.contains(&name) || name.ends_with(".cedit-tmp")
}

#[derive(Clone)]
struct Entry {
    path: PathBuf,
    name: String,
    is_dir: bool,
}

pub enum TreeAction {
    Open(PathBuf),
    NewFile(PathBuf),
    NewFolder(PathBuf),
    Rename(PathBuf),
    Delete(PathBuf),
    CopyPath(PathBuf),
    Reveal(PathBuf),
}

pub struct FileTree {
    pub root: PathBuf,
    expanded: HashSet<PathBuf>,
    children: HashMap<PathBuf, Vec<Entry>>,
    /// Height of one row as measured last frame (rows are drawn only when visible).
    row_pitch: f32,
    /// All files in the project, listed in the background (for quick open).
    files: FileIndex,
}

/// A list of every file under the project root, built on a background
/// thread so opening "Go to file" in a huge tree never blocks the UI.
#[derive(Default)]
struct FileIndex {
    files: Arc<Vec<PathBuf>>,
    /// Bumped each time a new listing arrives.
    generation: u64,
    rx: Option<mpsc::Receiver<Vec<PathBuf>>>,
    stale: bool,
    scanned_at: Option<std::time::Instant>,
}

impl FileTree {
    pub fn new(root: PathBuf) -> Self {
        let mut expanded = HashSet::new();
        expanded.insert(root.clone());
        Self {
            root,
            expanded,
            children: HashMap::new(),
            row_pitch: 0.0,
            files: FileIndex::default(),
        }
    }

    pub fn refresh(&mut self) {
        self.children.clear();
        self.files.stale = true;
    }

    /// All files under the root (relative paths) and a generation number that
    /// changes when the list does. The first call starts a background scan
    /// (the list is empty until it finishes); later calls rescan when the
    /// list is older than a few seconds or the tree was refreshed.
    pub fn all_files(&mut self, ctx: &egui::Context) -> (Arc<Vec<PathBuf>>, u64, bool) {
        let ix = &mut self.files;
        if let Some(rx) = &ix.rx
            && let Ok(files) = rx.try_recv()
        {
            ix.files = Arc::new(files);
            ix.generation += 1;
            ix.rx = None;
        }
        let old = ix.scanned_at.is_none_or(|t| t.elapsed().as_secs() >= 5);
        if ix.rx.is_none() && (ix.stale || old) {
            ix.stale = false;
            ix.scanned_at = Some(std::time::Instant::now());
            let (tx, rx) = mpsc::channel();
            let root = self.root.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(list_files(&root));
                ctx.request_repaint();
            });
            ix.rx = Some(rx);
        }
        (ix.files.clone(), ix.generation, ix.rx.is_some())
    }

    pub fn expand_to(&mut self, path: &Path) {
        let mut p = path.parent();
        while let Some(dir) = p {
            if !dir.starts_with(&self.root) {
                break;
            }
            self.expanded.insert(dir.to_path_buf());
            p = dir.parent();
        }
    }

    fn entries(&mut self, dir: &Path) -> &[Entry] {
        self.children.entry(dir.to_path_buf()).or_insert_with(|| {
            let mut v: Vec<Entry> = std::fs::read_dir(dir)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .filter_map(|e| {
                            let name = e.file_name().to_string_lossy().into_owned();
                            if is_hidden_entry(&name) {
                                return None;
                            }
                            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false)
                                || (e.file_type().map(|t| t.is_symlink()).unwrap_or(false)
                                    && e.path().is_dir());
                            Some(Entry {
                                path: e.path(),
                                name,
                                is_dir,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            v.sort_by(|a, b| {
                b.is_dir
                    .cmp(&a.is_dir)
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            });
            v
        })
    }

    pub fn show(&mut self, ui: &mut Ui, active: Option<&Path>, theme: &Theme) -> Option<TreeAction> {
        let mut action = None;
        let root = self.root.clone();
        let root_name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root.display().to_string());
        let resp = ui.label(RichText::new(root_name).strong());
        resp.context_menu(|ui| dir_menu(ui, &root, &mut action, true));

        // Flatten the expanded part of the tree, then draw only the rows in
        // view: a folder with thousands of files costs nothing when scrolled away.
        let mut rows: Vec<(usize, Entry)> = Vec::new();
        self.collect_rows(&root, 0, &mut rows);
        let pitch = if self.row_pitch > 0.0 { self.row_pitch } else { ui.spacing().interact_size.y + ui.spacing().item_spacing.y };
        let top = ui.cursor().top();
        let clip = ui.clip_rect();
        let first = (((clip.top() - top) / pitch).floor().max(0.0) as usize).min(rows.len());
        let last = ((((clip.bottom() - top) / pitch).ceil().max(0.0)) as usize + 1).clamp(first, rows.len());
        if first > 0 {
            ui.add_space(first as f32 * pitch);
        }
        let start_y = ui.cursor().top();
        for (depth, e) in &rows[first..last] {
            self.show_row(ui, e, *depth, active, theme, &mut action);
        }
        if last > first + 1 {
            let measured = (ui.cursor().top() - start_y) / (last - first) as f32;
            if measured > 1.0 && (measured - self.row_pitch).abs() > 0.01 {
                self.row_pitch = measured;
                ui.ctx().request_repaint();
            }
        }
        if rows.len() > last {
            ui.add_space((rows.len() - last) as f32 * pitch);
        }
        // Empty space below the tree also gets the root context menu.
        let rest = ui.allocate_response(
            egui::vec2(ui.available_width(), ui.available_height().max(40.0)),
            egui::Sense::click(),
        );
        rest.context_menu(|ui| dir_menu(ui, &root, &mut action, true));
        action
    }

    fn collect_rows(&mut self, dir: &Path, depth: usize, rows: &mut Vec<(usize, Entry)>) {
        let entries = self.entries(dir).to_vec();
        for e in entries {
            let open = e.is_dir && self.expanded.contains(&e.path);
            let path = e.path.clone();
            rows.push((depth, e));
            if open {
                self.collect_rows(&path, depth + 1, rows);
            }
        }
    }

    fn show_row(&mut self, ui: &mut Ui, e: &Entry, depth: usize, active: Option<&Path>, theme: &Theme, action: &mut Option<TreeAction>) {
        let indent = 14.0 * depth as f32;
        if e.is_dir {
            let open = self.expanded.contains(&e.path);
            let resp = ui
                .horizontal(|ui| {
                    ui.add_space(indent);
                    ui.add(
                        egui::Button::selectable(false, format!("    {}", e.name))
                            .frame_when_inactive(false),
                    )
                })
                .inner;
            paint_disclosure(ui, resp.rect, open, theme.punct);
            if resp.clicked() {
                if open {
                    self.expanded.remove(&e.path);
                } else {
                    self.expanded.insert(e.path.clone());
                }
            }
            resp.context_menu(|ui| dir_menu(ui, &e.path, action, false));
        } else {
            let selected = active == Some(e.path.as_path());
            let color = file_color(&e.name, theme);
            let text = RichText::new(format!("    {}", e.name)).color(color);
            let resp = ui
                .horizontal(|ui| {
                    ui.add_space(indent);
                    ui.add(egui::Button::selectable(selected, text).frame_when_inactive(false))
                })
                .inner;
            if resp.clicked() {
                *action = Some(TreeAction::Open(e.path.clone()));
            }
            resp.context_menu(|ui| {
                if ui.button("Open").clicked() {
                    *action = Some(TreeAction::Open(e.path.clone()));
                }
                ui.separator();
                if ui.button("Rename…").clicked() {
                    *action = Some(TreeAction::Rename(e.path.clone()));
                }
                if ui.button("Move to Trash").clicked() {
                    *action = Some(TreeAction::Delete(e.path.clone()));
                }
                ui.separator();
                if ui.button("Copy Path").clicked() {
                    *action = Some(TreeAction::CopyPath(e.path.clone()));
                }
                if ui.button(reveal_label()).clicked() {
                    *action = Some(TreeAction::Reveal(e.path.clone()));
                }
            });
        }
    }
}

/// Draw a small open/closed triangle at the left of a tree row.
fn paint_disclosure(ui: &Ui, rect: egui::Rect, open: bool, color: egui::Color32) {
    let c = egui::pos2(rect.left() + 10.0, rect.center().y);
    let s = 3.5;
    let pts = if open {
        vec![egui::pos2(c.x - s, c.y - s * 0.6), egui::pos2(c.x + s, c.y - s * 0.6), egui::pos2(c.x, c.y + s * 0.8)]
    } else {
        vec![egui::pos2(c.x - s * 0.6, c.y - s), egui::pos2(c.x + s * 0.8, c.y), egui::pos2(c.x - s * 0.6, c.y + s)]
    };
    ui.painter().add(egui::Shape::convex_polygon(pts, color, egui::Stroke::NONE));
}

fn reveal_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "Reveal in Finder"
    } else if cfg!(windows) {
        "Show in Explorer"
    } else {
        "Open Containing Folder"
    }
}

fn dir_menu(ui: &mut Ui, dir: &Path, action: &mut Option<TreeAction>, is_root: bool) {
    if ui.button("New File…").clicked() {
        *action = Some(TreeAction::NewFile(dir.to_path_buf()));
    }
    if ui.button("New Folder…").clicked() {
        *action = Some(TreeAction::NewFolder(dir.to_path_buf()));
    }
    if !is_root {
        ui.separator();
        if ui.button("Rename…").clicked() {
            *action = Some(TreeAction::Rename(dir.to_path_buf()));
        }
        if ui.button("Move to Trash").clicked() {
            *action = Some(TreeAction::Delete(dir.to_path_buf()));
        }
    }
    ui.separator();
    if ui.button("Copy Path").clicked() {
        *action = Some(TreeAction::CopyPath(dir.to_path_buf()));
    }
    if ui.button(reveal_label()).clicked() {
        *action = Some(TreeAction::Reveal(dir.to_path_buf()));
    }
}

fn file_color(name: &str, theme: &Theme) -> egui::Color32 {
    let lang = crate::highlight::Language::from_path(Path::new(name));
    match lang {
        crate::highlight::Language::C => {
            if name.ends_with(".h") {
                theme.ty
            } else {
                theme.text
            }
        }
        crate::highlight::Language::ObjC => theme.text,
        crate::highlight::Language::Glsl => theme.string,
        crate::highlight::Language::Make => theme.keyword,
        crate::highlight::Language::Plain => theme.punct,
    }
}

/// Open the OS file manager at `path`.
pub fn reveal_in_file_manager(path: &Path) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg("-R").arg(path).spawn();
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer")
        .arg(format!("/select,{}", path.display()))
        .spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let dir = if path.is_dir() {
            path
        } else {
            path.parent().unwrap_or(path)
        };
        let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
    }
}

// ---------------------------------------------------------------------------
// Listing files

pub const MAX_LISTED_FILES: usize = 300_000;

/// All files under `root` (relative paths), skipping hidden and vendor dirs.
pub fn list_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if is_hidden_entry(&name) {
                continue;
            }
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if name.starts_with('.') || SKIP_DIRS_IN_SEARCH.contains(&name.as_ref()) {
                    continue;
                }
                stack.push(e.path());
            } else if ft.is_file()
                && let Ok(rel) = e.path().strip_prefix(root)
            {
                out.push(rel.to_path_buf());
                if out.len() >= MAX_LISTED_FILES {
                    return out;
                }
            }
        }
    }
    out.sort();
    out
}

/// Fuzzy subsequence match score (higher is better), or None.
pub fn fuzzy_score(candidate: &str, query: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    if candidate.is_ascii() && query.is_ascii() {
        return fuzzy_score_ascii(candidate.as_bytes(), query.as_bytes());
    }
    let cand: Vec<char> = candidate.chars().collect();
    let cand_lower: Vec<char> = candidate.to_lowercase().chars().collect();
    if cand.len() != cand_lower.len() {
        // Unusual Unicode case mapping; fall back to substring.
        return candidate
            .to_lowercase()
            .contains(&query.to_lowercase())
            .then_some(1);
    }
    let file_start = candidate.rfind(['/', '\\']).map(|p| candidate[..p + 1].chars().count()).unwrap_or(0);
    let mut score = 0i64;
    let mut ci = 0usize;
    let mut last_match: Option<usize> = None;
    for qc in query.to_lowercase().chars() {
        if qc == ' ' {
            continue;
        }
        let mut found = None;
        while ci < cand_lower.len() {
            if cand_lower[ci] == qc {
                found = Some(ci);
                break;
            }
            ci += 1;
        }
        let i = found?;
        score += 10;
        if last_match == Some(i.wrapping_sub(1)) {
            score += 15; // consecutive
        }
        if i == 0 || matches!(cand[i - 1], '/' | '\\' | '_' | '-' | '.' | ' ') {
            score += 20; // word start
        }
        if i >= file_start {
            score += 10; // in file name rather than directories
        }
        last_match = Some(i);
        ci = i + 1;
    }
    score -= cand.len() as i64 / 4;
    Some(score)
}

/// [`fuzzy_score`] for ASCII strings, without allocating (quick open scores
/// every file in the project on each keystroke).
fn fuzzy_score_ascii(cand: &[u8], query: &[u8]) -> Option<i64> {
    let file_start = cand.iter().rposition(|&c| c == b'/' || c == b'\\').map_or(0, |p| p + 1);
    let mut score = 0i64;
    let mut ci = 0usize;
    let mut last_match: Option<usize> = None;
    for &q in query {
        if q == b' ' {
            continue;
        }
        let qc = q.to_ascii_lowercase();
        let i = ci + cand[ci..].iter().position(|c| c.to_ascii_lowercase() == qc)?;
        score += 10;
        if last_match == Some(i.wrapping_sub(1)) {
            score += 15;
        }
        if i == 0 || matches!(cand[i - 1], b'/' | b'\\' | b'_' | b'-' | b'.' | b' ') {
            score += 20;
        }
        if i >= file_start {
            score += 10;
        }
        last_match = Some(i);
        ci = i + 1;
    }
    score -= cand.len() as i64 / 4;
    Some(score)
}

// ---------------------------------------------------------------------------
// Find in files

#[derive(Clone)]
pub struct SearchHit {
    pub path: PathBuf,
    pub line: usize,
    /// Column in chars of the match start.
    pub col: usize,
    pub line_text: String,
}

#[allow(dead_code)]
pub struct SearchJob {
    pub query: String,
    pub rx: mpsc::Receiver<Vec<SearchHit>>,
    pub cancel: Arc<AtomicBool>,
    pub hits: Vec<SearchHit>,
    pub done: Arc<AtomicBool>,
}

pub const MAX_HITS: usize = 5_000;

impl SearchJob {
    pub fn start(root: PathBuf, query: String, case_sensitive: bool, whole_word: bool, ctx: egui::Context) -> Self {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let (c2, d2, q2) = (cancel.clone(), done.clone(), query.clone());
        std::thread::spawn(move || {
            // List files, then search them on all cores. Each worker sends a
            // file's hits together, so results stay grouped by file.
            let files = list_files(&root);
            let next = std::sync::atomic::AtomicUsize::new(0);
            let sent = std::sync::atomic::AtomicUsize::new(0);
            let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, 16);
            std::thread::scope(|scope| {
                for _ in 0..threads {
                    scope.spawn(|| {
                        loop {
                            if c2.load(Ordering::Relaxed) || sent.load(Ordering::Relaxed) >= MAX_HITS {
                                break;
                            }
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            let Some(rel) = files.get(i) else { break };
                            let path = root.join(rel);
                            let hits = search_file(&path, &q2, case_sensitive, whole_word);
                            if hits.is_empty() {
                                continue;
                            }
                            let n = hits.len();
                            let before = sent.fetch_add(n, Ordering::Relaxed);
                            if before >= MAX_HITS {
                                break;
                            }
                            let mut hits = hits;
                            hits.truncate(n.min(MAX_HITS - before));
                            if tx.send(hits).is_err() {
                                c2.store(true, Ordering::Relaxed);
                                break;
                            }
                            ctx.request_repaint();
                        }
                    });
                }
            });
            d2.store(true, Ordering::Relaxed);
            ctx.request_repaint();
        });
        Self {
            query,
            rx,
            cancel,
            hits: Vec::new(),
            done,
        }
    }

    pub fn poll(&mut self) {
        while let Ok(h) = self.rx.try_recv() {
            self.hits.extend(h);
        }
    }

    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }
}

/// All matches of `query` in one file (skipping big and binary files).
fn search_file(path: &Path, query: &str, case_sensitive: bool, whole_word: bool) -> Vec<SearchHit> {
    let mut out = Vec::new();
    let Ok(meta) = std::fs::metadata(path) else { return out };
    if meta.len() > 4 * 1024 * 1024 {
        return out;
    }
    let Ok(bytes) = std::fs::read(path) else { return out };
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return out;
    }
    let text = String::from_utf8_lossy(&bytes);
    let ranges = crate::find::find_all(&text, query, case_sensitive, whole_word);
    let mut line = 0;
    let mut line_start = 0;
    let mut scanned = 0;
    for r in ranges {
        for (i, b) in text.as_bytes()[scanned..r.start].iter().enumerate() {
            if *b == b'\n' {
                line += 1;
                line_start = scanned + i + 1;
            }
        }
        scanned = r.start;
        let line_end = text[r.start..].find('\n').map(|p| p + r.start).unwrap_or(text.len());
        let line_text: String = text[line_start..line_end].trim_end_matches('\r').chars().take(300).collect();
        let col = text[line_start..r.start].chars().count();
        out.push(SearchHit { path: path.to_path_buf(), line, col, line_text });
        if out.len() >= MAX_HITS {
            break;
        }
    }
    out
}

impl Drop for SearchJob {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy() {
        assert!(fuzzy_score("src/main.c", "main").is_some());
        assert!(fuzzy_score("src/main.c", "smc").is_some());
        assert!(fuzzy_score("src/main.c", "xyz").is_none());
        let a = fuzzy_score("src/render/shader.c", "shader").unwrap();
        let b = fuzzy_score("shaders/old/really_long_path_here.c", "shader").unwrap();
        assert!(a > b);
    }
}
