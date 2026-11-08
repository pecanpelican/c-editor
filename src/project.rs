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
}

impl FileTree {
    pub fn new(root: PathBuf) -> Self {
        let mut expanded = HashSet::new();
        expanded.insert(root.clone());
        Self {
            root,
            expanded,
            children: HashMap::new(),
        }
    }

    pub fn refresh(&mut self) {
        self.children.clear();
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
        self.show_dir(ui, &root, 0, active, theme, &mut action);
        // Empty space below the tree also gets the root context menu.
        let rest = ui.allocate_response(
            egui::vec2(ui.available_width(), ui.available_height().max(40.0)),
            egui::Sense::click(),
        );
        rest.context_menu(|ui| dir_menu(ui, &root, &mut action, true));
        action
    }

    fn show_dir(
        &mut self,
        ui: &mut Ui,
        dir: &Path,
        depth: usize,
        active: Option<&Path>,
        theme: &Theme,
        action: &mut Option<TreeAction>,
    ) {
        let entries = self.entries(dir).to_vec();
        for e in entries {
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
                if open {
                    self.show_dir(ui, &e.path, depth + 1, active, theme, action);
                }
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

pub const MAX_LISTED_FILES: usize = 50_000;

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
    pub rx: mpsc::Receiver<SearchHit>,
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
            let mut sent = 0;
            'files: for rel in list_files(&root) {
                if c2.load(Ordering::Relaxed) {
                    break;
                }
                let path = root.join(&rel);
                let Ok(meta) = std::fs::metadata(&path) else { continue };
                if meta.len() > 4 * 1024 * 1024 {
                    continue;
                }
                let Ok(bytes) = std::fs::read(&path) else { continue };
                if bytes[..bytes.len().min(8192)].contains(&0) {
                    continue;
                }
                let text = String::from_utf8_lossy(&bytes);
                let ranges = crate::find::find_all(&text, &q2, case_sensitive, whole_word);
                if ranges.is_empty() {
                    continue;
                }
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
                    if tx
                        .send(SearchHit {
                            path: path.clone(),
                            line,
                            col,
                            line_text,
                        })
                        .is_err()
                    {
                        break 'files;
                    }
                    sent += 1;
                    if sent >= MAX_HITS {
                        break 'files;
                    }
                }
                ctx.request_repaint();
            }
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
            self.hits.push(h);
        }
    }

    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }
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
