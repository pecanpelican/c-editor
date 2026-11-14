//! An open document and the code-editor widget that shows it.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use egui::text::CCursor;
use egui::{Align, Align2, Event, FontId, Galley, Id, Key, Modifiers, Rect, ScrollArea, Sense, Shape, Ui, pos2, vec2};

use crate::buffer::{Record, TextIndex};
use crate::find::FindState;
use crate::highlight::{self, Language};
use crate::textops;
use crate::theme::Theme;
use crate::viewer::{Viewer, ViewerKind};

static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

pub struct Document {
    pub id: u64,
    pub path: Option<PathBuf>,
    pub text: String,
    saved: String,
    pub dirty: bool,
    /// Bumped on every text change (used for language-server sync).
    pub version: u64,
    /// Semantic highlighting from the language server: byte ranges.
    pub semantic: Vec<(Range<usize>, highlight::Tok)>,
    pub semantic_version: u64,
    pub crlf: bool,
    pub lang: Language,
    pub mtime: Option<SystemTime>,
    /// The file changed on disk while there were unsaved edits.
    pub disk_conflict: bool,
    /// Selection (byte range) to apply on the next frame; also focuses and
    /// scrolls to it.
    pub pending_select: Option<Range<usize>>,
    /// Whether applying `pending_select` should also focus the editor.
    pub pending_focus: bool,
    /// Keep requesting focus for a few frames: a modal that just closed
    /// still blocks focus for one frame.
    refocus_frames: u8,
    /// 0-based (line, col) of the cursor, for the status bar.
    pub cursor_line_col: (usize, usize),
    pub selection_chars: usize,
    /// Cursor byte offset as of the last frame.
    pub cursor_byte: usize,
    /// Selection: `anchor` is where it started, `head` is the caret (byte offsets).
    anchor: usize,
    head: usize,
    /// Selection when the text was last synced (the "before" of an undo step).
    synced_sel: (usize, usize),
    /// Preferred x for moving the caret up and down (pixels from line start).
    desired_x: Option<f32>,
    /// Line index, lexer states and undo history.
    pub index: TextIndex,
    /// IME composition in progress.
    preedit: String,
    /// When the caret last moved (it stays solid while moving).
    caret_time: f64,
    /// Scroll the caret into view on the next frame (centered if true).
    scroll_to_caret: Option<bool>,
    /// Mouse drag in progress: select by char (1), word (2) or line (3).
    drag_unit: u8,
    /// (version, head) -> matching bracket pair, cached.
    bracket_cache: Option<((u64, usize), Option<(usize, usize)>)>,
    last_head: Option<(usize, usize, u64)>,
    /// Images and PDFs open in a read-only viewer instead of the text editor.
    pub viewer: Option<Viewer>,
    /// Markdown files: show the rendered preview beside the editor.
    pub md_preview: bool,
    /// An editor command (toggle comment, move line...) chosen from a menu or
    /// the command palette, applied on the next frame.
    pub pending_action: Option<crate::settings::Action>,
}

pub enum OpenError {
    Io(std::io::Error),
    Binary,
    TooLarge,
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::Io(e) => write!(f, "{e}"),
            OpenError::Binary => write!(f, "looks like a binary file"),
            OpenError::TooLarge => write!(f, "file is larger than 20 MB"),
        }
    }
}

const MAX_FILE_SIZE: u64 = 20 * 1024 * 1024;

fn read_text_file(path: &Path) -> Result<(String, bool, Option<SystemTime>), OpenError> {
    let meta = std::fs::metadata(path).map_err(OpenError::Io)?;
    if meta.len() > MAX_FILE_SIZE {
        return Err(OpenError::TooLarge);
    }
    let bytes = std::fs::read(path).map_err(OpenError::Io)?;
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return Err(OpenError::Binary);
    }
    let text = match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    };
    let crlf = text.contains("\r\n");
    let text = if crlf { text.replace("\r\n", "\n") } else { text };
    Ok((text, crlf, meta.modified().ok()))
}

impl Document {
    pub fn new_untitled() -> Self {
        Self::from_parts(None, String::new(), false, None)
    }

    pub fn open(path: &Path) -> Result<Self, OpenError> {
        if let Some(kind) = ViewerKind::for_path(path) {
            let meta = std::fs::metadata(path).map_err(OpenError::Io)?;
            if meta.is_dir() {
                return Err(OpenError::Io(std::io::Error::other("is a directory")));
            }
            let mut doc = Self::from_parts(Some(path.to_path_buf()), String::new(), false, meta.modified().ok());
            doc.viewer = Some(Viewer::new(kind, path));
            return Ok(doc);
        }
        let (text, crlf, mtime) = read_text_file(path)?;
        Ok(Self::from_parts(Some(path.to_path_buf()), text, crlf, mtime))
    }

    fn from_parts(path: Option<PathBuf>, text: String, crlf: bool, mtime: Option<SystemTime>) -> Self {
        let lang = path
            .as_deref()
            .map(Language::from_path)
            .unwrap_or(Language::C);
        Self {
            id: NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            path,
            saved: text.clone(),
            text,
            dirty: false,
            version: 1,
            semantic: Vec::new(),
            semantic_version: 0,
            crlf,
            lang,
            mtime,
            disk_conflict: false,
            pending_select: None,
            pending_focus: true,
            refocus_frames: 0,
            cursor_line_col: (0, 0),
            selection_chars: 0,
            cursor_byte: 0,
            anchor: 0,
            head: 0,
            synced_sel: (0, 0),
            desired_x: None,
            index: TextIndex::default(),
            preedit: String::new(),
            caret_time: 0.0,
            scroll_to_caret: None,
            drag_unit: 0,
            bracket_cache: None,
            last_head: None,
            viewer: None,
            md_preview: true,
            pending_action: None,
        }
    }

    pub fn title(&self) -> String {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "untitled".to_string())
    }

    pub fn is_markdown(&self) -> bool {
        self.viewer.is_none()
            && self.path.as_ref().and_then(|p| p.extension()).and_then(|e| e.to_str()).is_some_and(|e| {
                matches!(e.to_ascii_lowercase().as_str(), "md" | "markdown" | "mdown" | "mkd")
            })
    }

    pub fn text_edit_id(&self) -> Id {
        Id::unique(("cedit-editor", self.id))
    }

    /// Layout is computed per frame for the visible lines only; kept for callers
    /// that used to drop a cached whole-file layout.
    pub fn invalidate_layout(&mut self) {}

    /// Bring the line index, lexer states and undo history up to date with
    /// `text` (cheap when nothing changed).
    pub fn sync(&mut self) {
        self.sync_as(Record::Edit);
    }

    fn sync_as(&mut self, record: Record) {
        let before = self.synced_sel;
        self.index.sync(&self.text, self.version, before, record);
        self.clamp_selection();
        self.synced_sel = (self.anchor, self.head);
    }

    fn clamp_selection(&mut self) {
        let len = self.text.len();
        self.anchor = textops::floor_char_boundary(&self.text, self.anchor.min(len));
        self.head = textops::floor_char_boundary(&self.text, self.head.min(len));
    }

    /// Set the selection (byte offsets; `head` is where the caret goes).
    pub fn set_selection(&mut self, sel: Range<usize>) {
        self.anchor = sel.start;
        self.head = sel.end;
        self.clamp_selection();
        self.desired_x = None;
        self.index.seal();
    }

    /// Replace `range` with `with`, put the caret after it and record undo.
    pub fn edit(&mut self, range: Range<usize>, with: &str) {
        self.sync();
        self.text.replace_range(range.clone(), with);
        self.version += 1;
        let c = range.start + with.len();
        self.anchor = c;
        self.head = c;
        self.desired_x = None;
        self.sync();
        self.dirty = self.text.len() != self.saved.len() || self.text != self.saved;
        self.scroll_to_caret.get_or_insert(false);
    }

    pub fn undo(&mut self) {
        self.sync();
        let Some(r) = self.index.pop_undo() else { return };
        let end = (r.at + r.inserted.len()).min(self.text.len());
        if !self.text.is_char_boundary(r.at) || !self.text.is_char_boundary(end) {
            return;
        }
        self.text.replace_range(r.at..end, &r.removed);
        self.version += 1;
        self.anchor = r.sel_before.0;
        self.head = r.sel_before.1;
        if self.anchor == self.head && self.head != r.at + r.removed.len() && self.head != r.at {
            self.anchor = r.at + r.removed.len();
            self.head = self.anchor;
        }
        self.sync_as(Record::Undo);
        self.dirty = self.text != self.saved;
        self.scroll_to_caret = Some(false);
    }

    pub fn redo(&mut self) {
        self.sync();
        let Some(r) = self.index.pop_redo() else { return };
        let end = (r.at + r.inserted.len()).min(self.text.len());
        if !self.text.is_char_boundary(r.at) || !self.text.is_char_boundary(end) {
            return;
        }
        self.text.replace_range(r.at..end, &r.removed);
        self.version += 1;
        let c = r.at + r.removed.len();
        self.anchor = c;
        self.head = c;
        self.sync_as(Record::Redo);
        self.dirty = self.text != self.saved;
        self.scroll_to_caret = Some(false);
    }

    pub fn mark_changed(&mut self) {
        self.dirty = self.text != self.saved;
        self.version += 1;
    }

    pub fn save(&mut self) -> std::io::Result<()> {
        if self.viewer.is_some() {
            // Viewer tabs are read-only; never write the empty buffer over the file.
            return Ok(());
        }
        let Some(path) = self.path.clone() else {
            return Err(std::io::Error::other("no path"));
        };
        let data = if self.crlf {
            self.text.replace('\n', "\r\n")
        } else {
            self.text.clone()
        };
        // Write to a temp file in the same directory, then rename, so a
        // crash mid-write never leaves a truncated file.
        let tmp = path.with_file_name(format!(
            ".{}.cedit-tmp",
            path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()
        ));
        write_new_file(&tmp, data.as_bytes())?;
        if let Ok(meta) = std::fs::metadata(&path) {
            let _ = std::fs::set_permissions(&tmp, meta.permissions());
        }
        if let Err(e) = std::fs::rename(&tmp, &path) {
            // Some filesystems refuse rename-over; fall back to a direct write.
            let _ = std::fs::remove_file(&tmp);
            std::fs::write(&path, data.as_bytes()).map_err(|_| e)?;
        }
        self.saved = self.text.clone();
        self.dirty = false;
        self.disk_conflict = false;
        self.mtime = std::fs::metadata(&path).ok().and_then(|m| m.modified().ok());
        Ok(())
    }

    pub fn save_as(&mut self, path: PathBuf) -> std::io::Result<()> {
        if self.viewer.is_some() {
            return Err(std::io::Error::other("this file is open in a read-only viewer"));
        }
        self.lang = Language::from_path(&path);
        self.path = Some(path);
        self.semantic.clear();
        self.save()
    }

    /// Check whether the file changed on disk. Reloads silently when there
    /// are no unsaved edits; otherwise flags a conflict. Returns true if
    /// the document was reloaded.
    pub fn check_disk(&mut self) -> bool {
        let Some(path) = &self.path else { return false };
        let Ok(meta) = std::fs::metadata(path) else {
            return false;
        };
        let mtime = meta.modified().ok();
        if mtime == self.mtime || mtime.is_none() {
            return false;
        }
        if self.dirty {
            self.disk_conflict = true;
            self.mtime = mtime;
            return false;
        }
        self.reload()
    }

    pub fn reload(&mut self) -> bool {
        let Some(path) = self.path.clone() else { return false };
        if let Some(v) = &self.viewer {
            self.viewer = Some(Viewer::new(v.kind(), &path));
            self.mtime = std::fs::metadata(&path).ok().and_then(|m| m.modified().ok());
            return true;
        }
        match read_text_file(&path) {
            Ok((text, crlf, mtime)) => {
                self.saved = text.clone();
                self.text = text;
                self.crlf = crlf;
                self.mtime = mtime;
                self.dirty = false;
                self.disk_conflict = false;
                self.version += 1;
                true
            }
            Err(_) => false,
        }
    }

    /// Select a 0-based line (and column) and scroll to it.
    /// Focus the editor (over the next few frames).
    pub fn focus(&mut self) {
        self.refocus_frames = 3;
    }

    pub fn goto_line_col(&mut self, line: usize, col: usize) {
        let b = textops::line_col_to_byte(&self.text, line, col);
        self.pending_select = Some(b..b);
    }

    pub fn line_count(&self) -> usize {
        if self.index.is_fresh(&self.text, self.version) {
            return self.index.line_count();
        }
        self.text.bytes().filter(|&b| b == b'\n').count() + 1
    }

    /// Current selection as a sorted byte range.
    pub fn selection(&self, _ctx: &egui::Context) -> Range<usize> {
        let len = self.text.len();
        let (a, h) = (self.anchor.min(len), self.head.min(len));
        a.min(h)..a.max(h)
    }

    pub fn selected_text(&self, ctx: &egui::Context) -> String {
        let s = self.selection(ctx);
        self.text[s].to_string()
    }

    fn set_selection_now(&mut self, _ctx: &egui::Context, sel: Range<usize>) {
        self.set_selection(sel);
        self.scroll_to_caret.get_or_insert(false);
    }

    /// Replace the given byte range and leave the cursor after it.
    pub fn replace_range(&mut self, range: Range<usize>, with: &str) {
        self.text.replace_range(range.clone(), with);
        let c = range.start + with.len();
        self.pending_select = Some(c..c);
        self.mark_changed();
    }
}

#[derive(Default)]
pub struct EditorOutput {
    pub focused: bool,
    /// Cursor position (byte offset) and its rectangle on screen.
    pub cursor: Option<usize>,
    pub cursor_rect: Option<Rect>,
    /// Text typed this frame (before auto-indent handling).
    pub typed: String,
    /// Byte offset under the mouse pointer, and the pointer position.
    pub hover: Option<(usize, egui::Pos2)>,
    /// Cmd/Ctrl+click on a byte offset.
    pub cmd_click: Option<usize>,
    /// Click in the gutter on this line (0-based): toggle a breakpoint.
    pub gutter_click: Option<usize>,
}

/// Debugger state drawn in the gutter.
#[derive(Default)]
pub struct DebugMarks {
    /// (line, verified) for each breakpoint, 0-based lines.
    pub breakpoints: Vec<(usize, bool)>,
    /// Where the program is paused, if in this file.
    pub exec_line: Option<usize>,
    /// True when `exec_line` is the top frame (not a caller picked in the call stack).
    pub exec_top: bool,
    /// Profiler: share of samples per line (0-based).
    pub heat: Vec<(usize, f32)>,
    /// Debugger: variable values to show after the end of a line (0-based).
    pub inline: Vec<(usize, String)>,
}

/// Editor key handling that egui's TextEdit doesn't do: auto-indent,
/// block indent, comment toggling, line duplication.
fn handle_keys(ui: &mut Ui, doc: &mut Document, keymap: &crate::settings::Keymap) {
    let ctx = ui.ctx().clone();
    let sel = doc.selection(&ctx);
    let hard_tabs = doc.lang.uses_hard_tabs();
    let comment = doc.lang.line_comment();

    enum Action {
        Newline,
        Indent,
        Dedent,
        Comment,
        Duplicate,
        DeleteLine,
        Closing(char),
        MoveUp,
        MoveDown,
        Open(char),
        Backspace,
    }
    // A command picked from a menu or the command palette.
    let pending = doc.pending_action.take().and_then(|a| {
        use crate::settings::Action as A;
        match a {
            A::ToggleComment => Some(Action::Comment),
            A::DuplicateLine => Some(Action::Duplicate),
            A::DeleteLine => Some(Action::DeleteLine),
            A::MoveLineUp => Some(Action::MoveUp),
            A::MoveLineDown => Some(Action::MoveDown),
            _ => None,
        }
    });
    let auto_close = textops::auto_close_enabled();
    let action = pending.or_else(|| ctx.input_mut(|i| {
        if i.consume_key(Modifiers::SHIFT, Key::Tab) {
            return Some(Action::Dedent);
        }
        if i.consume_key(Modifiers::NONE, Key::Tab) {
            return Some(Action::Indent);
        }
        if i.consume_key(Modifiers::NONE, Key::Enter) {
            return Some(Action::Newline);
        }
        use crate::settings::Action as A;
        let mut hit = |a: A| keymap.shortcut(a).is_some_and(|sc| i.consume_shortcut(&sc));
        if hit(A::DeleteLine) {
            return Some(Action::DeleteLine);
        }
        if hit(A::ToggleComment) {
            return Some(Action::Comment);
        }
        if hit(A::DuplicateLine) {
            return Some(Action::Duplicate);
        }
        if hit(A::MoveLineUp) {
            return Some(Action::MoveUp);
        }
        if hit(A::MoveLineDown) {
            return Some(Action::MoveDown);
        }
        if auto_close && i.consume_key(Modifiers::NONE, Key::Backspace) {
            return Some(Action::Backspace);
        }
        // Typing a closing brace on a blank line dedents it; with auto-close,
        // openers insert their pair and closers step over an existing one.
        let mut picked = None;
        i.events.retain(|e| {
            if picked.is_none()
                && let Event::Text(t) = e
                && t.chars().count() == 1
                && let Some(ch) = t.chars().next()
            {
                if matches!(ch, '}' | ')' | ']') || (auto_close && matches!(ch, '"' | '\'')) {
                    picked = Some(Action::Closing(ch));
                    return false;
                }
                if auto_close && matches!(ch, '(' | '[' | '{') {
                    picked = Some(Action::Open(ch));
                    return false;
                }
            }
            true
        });
        picked
    }));
    let Some(action) = action else { return };

    let new_sel = match action {
        Action::Newline => {
            let c = textops::newline_with_indent(&mut doc.text, sel, hard_tabs);
            c..c
        }
        Action::Indent => {
            let multi_line = doc.text[sel.clone()].contains('\n');
            if multi_line {
                textops::indent_lines(&mut doc.text, sel, hard_tabs)
            } else {
                let c = textops::insert_indent(&mut doc.text, sel, hard_tabs);
                c..c
            }
        }
        Action::Dedent => textops::dedent_lines(&mut doc.text, sel),
        Action::Comment => match comment {
            Some(p) => textops::toggle_comment(&mut doc.text, sel, p),
            None => return,
        },
        Action::Duplicate => textops::duplicate(&mut doc.text, sel),
        Action::DeleteLine => {
            let c = textops::delete_lines(&mut doc.text, sel);
            c..c
        }
        Action::Closing(ch) => {
            if auto_close && let Some(c) = textops::skip_closer(&doc.text, &sel, ch) {
                doc.set_selection_now(&ctx, c..c);
                return;
            }
            if matches!(ch, '"' | '\'') {
                match textops::auto_close(&mut doc.text, sel.clone(), ch) {
                    Some(r) => r,
                    None => {
                        doc.text.replace_range(sel.clone(), &ch.to_string());
                        let c = sel.start + 1;
                        c..c
                    }
                }
            } else {
                let c = textops::type_closing_brace(&mut doc.text, sel, ch);
                c..c
            }
        }
        Action::Open(ch) => match textops::auto_close(&mut doc.text, sel.clone(), ch) {
            Some(r) => r,
            None => {
                doc.text.replace_range(sel.clone(), &ch.to_string());
                let c = sel.start + 1;
                c..c
            }
        },
        Action::Backspace => match textops::delete_pair(&mut doc.text, &sel) {
            Some(c) => c..c,
            None => {
                // Not a pair: put the key back for the text editor.
                ctx.input_mut(|i| {
                    i.events.push(Event::Key {
                        key: Key::Backspace,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: Modifiers::NONE,
                    })
                });
                return;
            }
        },
        Action::MoveUp | Action::MoveDown => {
            match textops::move_lines(&mut doc.text, sel, matches!(action, Action::MoveUp)) {
                Some(r) => r,
                None => return,
            }
        }
    };
    doc.mark_changed();
    doc.set_selection_now(&ctx, new_sel);
}

/// Lines longer than this are drawn only around the visible part (assuming a
/// monospace font), so a megabyte-long line isn't laid out every frame.
const LONG_LINE: usize = 4000;

/// Columns `s` takes in a monospace font (egui draws a tab 4 spaces wide).
fn mono_cols(s: &str) -> usize {
    s.chars().map(|c| if c == '\t' { 4 } else { 1 }).sum()
}

/// Byte offset in `s` of monospace column `col` (clamped to the end).
fn col_to_byte(s: &str, col: usize) -> usize {
    let mut cols = 0;
    for (i, c) in s.char_indices() {
        let w = if c == '\t' { 4 } else { 1 };
        if cols + w > col {
            return if col - cols > w / 2 { i + c.len_utf8() } else { i };
        }
        cols += w;
    }
    s.len()
}

/// A laid-out line (or the visible part of a long one).
struct LineLayout {
    /// Byte range of the line, without its newline.
    range: Range<usize>,
    /// The part of the line that `galley` shows.
    shown: Range<usize>,
    /// x of the start of `shown`, relative to the start of the line.
    x0: f32,
    galley: Arc<Galley>,
}

impl LineLayout {
    /// x of byte `b` (clamped to the line), relative to the start of the line.
    fn x_of(&self, text: &str, b: usize, char_w: f32) -> f32 {
        let b = textops::floor_char_boundary(text, b.clamp(self.range.start, self.range.end));
        if b < self.shown.start || b > self.shown.end {
            return mono_cols(&text[self.range.start..b]) as f32 * char_w;
        }
        let ci = text[self.shown.start..b].chars().count();
        self.x0 + self.galley.pos_from_cursor(CCursor::new(ci)).min.x
    }

    /// Byte closest to `x` (relative to the start of the line).
    fn byte_at_x(&self, text: &str, x: f32, char_w: f32) -> usize {
        if self.shown != self.range {
            let col = (x / char_w).round().max(0.0) as usize;
            return self.range.start + col_to_byte(&text[self.range.clone()], col);
        }
        let c = self.galley.cursor_from_pos(vec2(x - self.x0, 1.0));
        self.shown.start + textops::char_to_byte(&text[self.shown.clone()], c.index.0)
    }

    fn width(&self, text: &str, char_w: f32) -> f32 {
        self.x_of(text, self.range.end, char_w)
    }
}

/// The part of `range` to lay out for horizontal view `vis_x` (pixels from
/// the line start), and the x where it starts.
fn shown_part(text: &str, range: Range<usize>, vis_x: &Range<f32>, char_w: f32) -> (Range<usize>, f32) {
    if range.len() <= LONG_LINE {
        return (range, 0.0);
    }
    let line = &text[range.clone()];
    let col0 = ((vis_x.start / char_w) as usize).saturating_sub(64);
    let cols = ((vis_x.end - vis_x.start).max(0.0) / char_w) as usize + 128;
    let b0 = col_to_byte(line, col0);
    let x0 = mono_cols(&line[..b0]) as f32 * char_w;
    let b1 = b0 + col_to_byte(&line[b0..], cols);
    (range.start + b0..range.start + b1, x0)
}

/// Lay out a line without highlighting (for positions on lines not drawn).
fn plain_layout(ui: &Ui, doc: &Document, line: usize, font: &FontId) -> LineLayout {
    let range = doc.index.line_range(line.min(doc.index.line_count() - 1));
    let (shown, x0) = if range.len() > LONG_LINE { (range.start..range.start, 0.0) } else { (range.clone(), 0.0) };
    let job = egui::text::LayoutJob::simple_singleline(doc.text[shown.clone()].to_string(), font.clone(), egui::Color32::WHITE);
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    LineLayout { range, shown, x0, galley }
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

#[derive(PartialEq)]
enum CharClass {
    Word,
    Space,
    Punct,
    Newline,
}

fn class_of(c: char) -> CharClass {
    if c == '\n' {
        CharClass::Newline
    } else if c.is_whitespace() {
        CharClass::Space
    } else if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Punct
    }
}

fn prev_char(text: &str, b: usize) -> usize {
    text[..b].chars().next_back().map_or(0, |c| b - c.len_utf8())
}

fn next_char(text: &str, b: usize) -> usize {
    text[b..].chars().next().map_or(text.len(), |c| b + c.len_utf8())
}

/// Start of the word before `b` (skipping spaces first).
fn word_left(text: &str, mut b: usize) -> usize {
    let mut it = text[..b].chars().rev().peekable();
    if it.peek() == Some(&'\n') {
        return b - 1;
    }
    while let Some(&c) = it.peek() {
        if class_of(c) != CharClass::Space {
            break;
        }
        b -= c.len_utf8();
        it.next();
    }
    let Some(&first) = it.peek() else { return b };
    let class = class_of(first);
    if class == CharClass::Newline {
        return b;
    }
    while let Some(&c) = it.peek() {
        if class_of(c) != class {
            break;
        }
        b -= c.len_utf8();
        it.next();
    }
    b
}

/// End of the word after `b` (skipping spaces first).
fn word_right(text: &str, mut b: usize) -> usize {
    let mut it = text[b..].chars().peekable();
    if it.peek() == Some(&'\n') {
        return b + 1;
    }
    while let Some(&c) = it.peek() {
        if class_of(c) != CharClass::Space {
            break;
        }
        b += c.len_utf8();
        it.next();
    }
    let Some(&first) = it.peek() else { return b };
    let class = class_of(first);
    if class == CharClass::Newline {
        return b;
    }
    while let Some(&c) = it.peek() {
        if class_of(c) != class {
            break;
        }
        b += c.len_utf8();
        it.next();
    }
    b
}

/// The identifier touching byte `b`, if any.
fn ident_at(text: &str, b: usize) -> Option<Range<usize>> {
    let bytes = text.as_bytes();
    let mut s = b.min(bytes.len());
    while s > 0 && is_ident_byte(bytes[s - 1]) {
        s -= 1;
    }
    let mut e = b.min(bytes.len());
    while e < bytes.len() && is_ident_byte(bytes[e]) {
        e += 1;
    }
    (e > s && !bytes[s].is_ascii_digit()).then_some(s..e)
}

/// Find the bracket matching the one at or just before the caret, skipping
/// brackets in strings and comments. Returns (bracket at caret, its match).
fn bracket_pair(doc: &mut Document) -> Option<(usize, usize)> {
    let key = (doc.version, doc.head);
    if let Some((k, v)) = doc.bracket_cache
        && k == key
    {
        return v;
    }
    let v = find_bracket_pair(doc);
    doc.bracket_cache = Some((key, v));
    v
}

fn find_bracket_pair(doc: &mut Document) -> Option<(usize, usize)> {
    const MAX_LINES: usize = 5000;
    let lang = doc.lang;
    if lang == Language::Plain {
        return None;
    }
    let bytes = doc.text.as_bytes();
    let h = doc.head.min(bytes.len());
    let is_br = |c: u8| matches!(c, b'(' | b')' | b'[' | b']' | b'{' | b'}');
    let p = if h < bytes.len() && is_br(bytes[h]) {
        h
    } else if h > 0 && is_br(bytes[h - 1]) {
        h - 1
    } else {
        return None;
    };
    let c = bytes[p];
    let (open, close, forward) = match c {
        b'(' => (b'(', b')', true),
        b'[' => (b'[', b']', true),
        b'{' => (b'{', b'}', true),
        b')' => (b'(', b')', false),
        b']' => (b'[', b']', false),
        _ => (b'{', b'}', false),
    };
    let code = |t: highlight::Tok| !matches!(t, highlight::Tok::Str | highlight::Tok::Comment);
    let mut spans = Vec::new();
    let start_line = doc.index.line_of(p);
    let lc = doc.index.line_count();
    let mut depth = 0i32;
    let mut first = true;
    let mut lines: Box<dyn Iterator<Item = usize>> = if forward {
        Box::new(start_line..(start_line + MAX_LINES).min(lc))
    } else {
        Box::new((start_line.saturating_sub(MAX_LINES)..=start_line).rev())
    };
    for line in &mut lines {
        let range = doc.index.line_range(line);
        let st = doc.index.state_at(&doc.text, lang, line);
        spans.clear();
        highlight::lex_line(lang, &doc.text, range, st, &mut spans);
        let bytes = doc.text.as_bytes();
        if first {
            first = false;
            // The bracket at the caret must itself be code.
            if !spans.iter().any(|(r, t)| r.contains(&p) && code(*t)) {
                return None;
            }
        }
        let mut visit = |i: usize| -> Option<usize> {
            if bytes[i] == open {
                depth += if forward { 1 } else { -1 };
            } else if bytes[i] == close {
                depth += if forward { -1 } else { 1 };
            } else {
                return None;
            }
            (depth == 0).then_some(i)
        };
        if forward {
            for (r, t) in &spans {
                if !code(*t) || r.end <= p {
                    continue;
                }
                for i in r.start.max(p)..r.end {
                    if let Some(m) = visit(i) {
                        return Some((p, m));
                    }
                }
            }
        } else {
            for (r, t) in spans.iter().rev() {
                if !code(*t) || r.start > p {
                    continue;
                }
                for i in (r.start..r.end.min(p + 1)).rev() {
                    if let Some(m) = visit(i) {
                        return Some((p, m));
                    }
                }
            }
        }
    }
    None
}

/// Whole-word (for identifiers) or plain occurrences of `needle` in `hay`,
/// offset by `base`.
fn occurrences(hay: &str, base: usize, needle: &str, whole_word: bool, out: &mut Vec<Range<usize>>) {
    if needle.is_empty() {
        return;
    }
    let hb = hay.as_bytes();
    let mut from = 0;
    while let Some(i) = hay[from..].find(needle) {
        let s = from + i;
        let e = s + needle.len();
        if !whole_word || ((s == 0 || !is_ident_byte(hb[s - 1])) && (e == hb.len() || !is_ident_byte(hb[e]))) {
            out.push(base + s..base + e);
        }
        from = e;
    }
}

pub fn show(
    ui: &mut Ui,
    doc: &mut Document,
    find: &FindState,
    theme: &Theme,
    font_size: f32,
    diags: &[&crate::make::Diagnostic],
    keymap: &crate::settings::Keymap,
    debug: &DebugMarks,
) -> EditorOutput {
    let id = doc.text_edit_id();
    let ctx = ui.ctx().clone();
    doc.sync();
    let font = FontId::monospace(font_size);
    let has_focus = ui.memory(|m| m.has_focus(id));
    let mut out = EditorOutput::default();
    if has_focus {
        out.typed = ui.input(|i| {
            i.events
                .iter()
                .filter_map(|e| if let Event::Text(t) = e { Some(t.as_str()) } else { None })
                .collect()
        });
        handle_keys(ui, doc, keymap);
        doc.sync();
    }

    if let Some(sel) = doc.pending_select.take() {
        doc.set_selection(sel);
        if doc.pending_focus {
            doc.refocus_frames = 3;
        }
        doc.pending_focus = true;
        doc.scroll_to_caret = Some(true);
    }

    if doc.refocus_frames > 0 {
        doc.refocus_frames -= 1;
        if !ui.memory(|m| m.has_focus(id)) {
            ui.memory_mut(|m| m.request_focus(id));
        }
        ui.ctx().request_repaint();
    }

    let (row_h, char_w) = ui.fonts_mut(|f| (f.row_height(&font), f.glyph_width(&font, '0')));
    let line_count = doc.index.line_count();
    let digits = line_count.to_string().len().max(3);
    let gutter_w = char_w * (digits as f32 + 3.0);
    let text_margin_left = gutter_w + char_w;
    let avail = ui.available_size();
    let pad_top = 4.0;
    let content = vec2(
        (text_margin_left + doc.index.max_line_len as f32 * char_w * 1.2 + 64.0).max(avail.x),
        (pad_top + line_count as f32 * row_h + avail.y * 0.4).max(avail.y),
    );
    let lang = doc.lang;
    let mac = ctx.os() == egui::os::OperatingSystem::Mac;
    let now = ui.input(|i| i.time);

    let mut focused = has_focus;
    let bg = theme.bg;
    egui::Frame::NONE.fill(bg).show(ui, |ui| {
        ScrollArea::both()
            .id_salt(("cedit-scroll", doc.id))
            .auto_shrink([false, false])
            .show_viewport(ui, |ui, viewport| {
                let (rect, _) = ui.allocate_exact_size(content, Sense::hover());
                let resp = ui.interact(rect, id, Sense::click_and_drag());
                let clip = ui.clip_rect();
                let text_x = rect.min.x + text_margin_left;
                let text_y = rect.min.y + pad_top;
                let line_y = |l: usize| text_y + l as f32 * row_h;
                let line_at = |y: f32| (((y - text_y) / row_h).floor().max(0.0) as usize).min(line_count - 1);
                let gutter = Rect::from_min_max(pos2(clip.left(), clip.top()), pos2(clip.left() + gutter_w, clip.bottom()));
                let pos_to_byte = |ui: &Ui, doc: &Document, p: egui::Pos2| -> usize {
                    let l = line_at(p.y);
                    plain_layout(ui, doc, l, &font).byte_at_x(&doc.text, p.x - text_x, char_w)
                };

                // ---- Mouse
                let pointer = ui.input(|i| i.pointer.interact_pos());
                let modifiers = ui.input(|i| i.modifiers);
                let in_text = |p: egui::Pos2| p.x >= gutter.right() && clip.contains(p);
                if let Some(p) = pointer
                    && resp.contains_pointer()
                    && in_text(p)
                {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
                    let b = pos_to_byte(ui, doc, p);
                    out.hover = Some((b, p));
                    if ui.input(|i| i.pointer.primary_pressed()) {
                        ui.memory_mut(|m| m.request_focus(id));
                        focused = true;
                        if modifiers.shift {
                            doc.head = b;
                        } else {
                            doc.anchor = b;
                            doc.head = b;
                        }
                        doc.desired_x = None;
                        doc.index.seal();
                        doc.drag_unit = 1;
                    }
                    if resp.clicked() && modifiers.command {
                        out.cmd_click = Some(b);
                    }
                    if resp.triple_clicked() {
                        let l = doc.index.line_of(b);
                        let end = (doc.index.line_end(l) + 1).min(doc.text.len());
                        doc.anchor = doc.index.line_start(l);
                        doc.head = end;
                        doc.drag_unit = 0;
                    } else if resp.double_clicked() {
                        let r = ident_at(&doc.text, b).unwrap_or_else(|| b..next_char(&doc.text, b).min(doc.index.line_end(doc.index.line_of(b))));
                        doc.anchor = r.start;
                        doc.head = r.end;
                        doc.drag_unit = 0;
                    }
                }
                if doc.drag_unit > 0 {
                    let (down, released, origin) = ui.input(|i| (i.pointer.primary_down(), i.pointer.primary_released(), i.pointer.press_origin()));
                    let moved = pointer.zip(origin).is_some_and(|(p, o)| (p - o).length() > 2.0);
                    if down || released {
                        if let Some(p) = pointer
                            && (resp.dragged() || moved)
                        {
                            let b = pos_to_byte(ui, doc, pos2(p.x.max(gutter.right()), p.y));
                            if b != doc.head {
                                doc.head = b;
                            }
                            if down && !clip.shrink2(vec2(0.0, row_h * 0.5)).contains(p) {
                                doc.scroll_to_caret = Some(false);
                                ui.ctx().request_repaint();
                            }
                        }
                    }
                    if !down {
                        doc.drag_unit = 0;
                    }
                }

                // ---- Keyboard
                if focused {
                    ui.memory_mut(|m| {
                        m.set_focus_lock_filter(
                            id,
                            egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: false },
                        )
                    });
                    let events = ui.input(|i| i.events.clone());
                    let page = ((clip.height() / row_h) as usize).saturating_sub(1).max(1);
                    for ev in events {
                        handle_event(ui, doc, &ev, mac, page, &font, char_w);
                    }
                }

                // ---- Caret bookkeeping
                let sel = doc.selection(&ctx);
                let head = doc.head;
                let key = (doc.anchor, head, doc.version);
                if doc.last_head != Some(key) {
                    doc.last_head = Some(key);
                    doc.caret_time = now;
                    let l = doc.index.line_of(head);
                    let ls = doc.index.line_start(l);
                    doc.cursor_line_col = (l, doc.text[ls..head].chars().count());
                    doc.selection_chars = if sel.len() > 1 << 20 { sel.len() } else { doc.text[sel.clone()].chars().count() };
                }
                doc.cursor_byte = head;
                out.cursor = Some(head);

                // ---- Visible lines
                let first = (((viewport.min.y - pad_top) / row_h).floor().max(0.0) as usize).min(line_count - 1);
                let last = ((((viewport.max.y - pad_top) / row_h).ceil().max(0.0)) as usize + 1).clamp(first + 1, line_count);
                doc.index.state_at(&doc.text, lang, last - 1);
                let vis_x = (viewport.min.x - text_margin_left).max(0.0)..(viewport.max.x - text_margin_left).max(0.0);

                // Overlays: find matches, other occurrences, matching brackets.
                let find_valid = find.open && find.doc_key == (doc.id, doc.version);
                let vis_range = doc.index.line_start(first)..doc.index.line_end(last - 1);
                let mut occ: Vec<Range<usize>> = Vec::new();
                let needle = if sel.is_empty() {
                    ident_at(&doc.text, head).map(|r| (doc.text[r].to_string(), true))
                } else if sel.len() <= 256 && !doc.text[sel.clone()].contains('\n') && !doc.text[sel.clone()].trim().is_empty() {
                    Some((doc.text[sel.clone()].to_string(), false))
                } else {
                    None
                };
                if let Some((n, whole)) = &needle {
                    occurrences(&doc.text[vis_range.clone()], vis_range.start, n, *whole, &mut occ);
                    if occ.len() < 2 && *whole {
                        occ.clear();
                    }
                }
                let brackets = if sel.is_empty() { bracket_pair(doc) } else { None };
                let occ_bg = theme.selection.gamma_multiply(0.45);
                let bracket_bg = theme.text.gamma_multiply(0.22);

                let mut layouts: Vec<LineLayout> = Vec::with_capacity(last - first);
                let mut spans = Vec::new();
                let mut overlays: Vec<(Range<usize>, egui::Color32)> = Vec::new();
                let mi0 = if find_valid { find.matches.partition_point(|m| m.end < vis_range.start) } else { 0 };
                let oi0 = 0usize;
                let _ = oi0;
                for line in first..last {
                    let range = doc.index.line_range(line);
                    let st = doc.index.state_at(&doc.text, lang, line);
                    spans.clear();
                    highlight::lex_line(lang, &doc.text, range.clone(), st, &mut spans);
                    let (shown, x0) = shown_part(&doc.text, range.clone(), &vis_x, char_w);
                    overlays.clear();
                    for r in &occ {
                        if r.start < range.end.max(range.start + 1) && r.end > range.start {
                            overlays.push((r.clone(), occ_bg));
                        }
                    }
                    if find_valid {
                        let mut mi = mi0;
                        while mi < find.matches.len() && find.matches[mi].start <= range.end {
                            let m = &find.matches[mi];
                            if m.end > range.start || (m.start == range.start && m.is_empty()) {
                                let c = if find.current == Some(mi) { theme.current_match_bg } else { theme.match_bg };
                                overlays.push((m.clone(), c));
                            }
                            mi += 1;
                        }
                    }
                    if let Some((a, b)) = brackets {
                        for p in [a, b] {
                            if range.contains(&p) {
                                overlays.push((p..p + 1, bracket_bg));
                            }
                        }
                    }
                    let job = highlight::line_job(&doc.text, shown.clone(), &spans, &doc.semantic, &overlays, &font, theme);
                    let galley = ui.fonts_mut(|f| f.layout_job(job));
                    layouts.push(LineLayout { range, shown, x0, galley });
                }
                let layout_of = |ui: &Ui, doc: &Document, l: usize| -> LineLayout {
                    if l >= first && l < last {
                        let ll = &layouts[l - first];
                        LineLayout { range: ll.range.clone(), shown: ll.shown.clone(), x0: ll.x0, galley: ll.galley.clone() }
                    } else {
                        plain_layout(ui, doc, l, &font)
                    }
                };

                // ---- Backgrounds
                let painter = ui.painter().clone();
                let head_line = doc.index.line_of(head);
                let caret_ll = layout_of(ui, doc, head_line);
                let caret_x = text_x + caret_ll.x_of(&doc.text, head, char_w);
                let caret_rect = Rect::from_min_size(pos2(caret_x, line_y(head_line)), vec2(2.0, row_h));
                out.cursor_rect = Some(caret_rect);
                if focused {
                    painter.rect_filled(
                        Rect::from_min_max(pos2(clip.left(), line_y(head_line)), pos2(clip.right(), line_y(head_line) + row_h)),
                        0.0,
                        theme.current_line_bg,
                    );
                }
                if let Some(line) = debug.exec_line
                    && line < line_count
                {
                    let y = line_y(line);
                    let color = if debug.exec_top { theme.warning } else { theme.function };
                    painter.rect_filled(Rect::from_min_max(pos2(clip.left(), y), pos2(clip.right(), y + row_h)), 0.0, color.gamma_multiply(0.18));
                }
                for &(line, frac) in &debug.heat {
                    if line >= line_count || frac < 0.002 {
                        continue;
                    }
                    let y = line_y(line);
                    if y + row_h < clip.top() || y > clip.bottom() {
                        continue;
                    }
                    let alpha = (0.08 + frac * 0.8).min(0.4);
                    painter.rect_filled(Rect::from_min_max(pos2(gutter.right(), y), pos2(clip.right(), y + row_h)), 0.0, theme.error.gamma_multiply(alpha));
                    painter.text(
                        pos2(clip.right() - 8.0, y + row_h * 0.5),
                        Align2::RIGHT_CENTER,
                        format!("{:.1}%", frac * 100.0),
                        FontId::proportional((font_size - 2.0).max(9.0)),
                        theme.warning,
                    );
                }
                if !sel.is_empty() {
                    let sel_bg = if focused { theme.selection } else { theme.selection.gamma_multiply(0.6) };
                    for ll in &layouts {
                        if ll.range.start > sel.end || sel.start > ll.range.end || (sel.end == ll.range.start && ll.range.start > 0) {
                            continue;
                        }
                        let xa = if sel.start > ll.range.start { ll.x_of(&doc.text, sel.start, char_w) } else { 0.0 };
                        let xb = if sel.end <= ll.range.end { ll.x_of(&doc.text, sel.end, char_w) } else { ll.width(&doc.text, char_w) + char_w * 0.6 };
                        let y = line_y(doc.index.line_of(ll.range.start));
                        painter.rect_filled(Rect::from_min_max(pos2(text_x + xa, y), pos2(text_x + xb, y + row_h)), 0.0, sel_bg);
                    }
                }

                // ---- Text
                let text_clip = Rect::from_min_max(pos2(gutter.right(), clip.top()), clip.max);
                let text_painter = painter.with_clip_rect(text_clip);
                for (i, ll) in layouts.iter().enumerate() {
                    text_painter.galley(pos2(text_x + ll.x0, line_y(first + i)), ll.galley.clone(), theme.text);
                }

                // ---- Caret and IME composition
                if focused {
                    if !doc.preedit.is_empty() {
                        let g = text_painter.layout_no_wrap(doc.preedit.clone(), font.clone(), theme.text);
                        let r = Rect::from_min_size(caret_rect.min, g.size());
                        text_painter.rect_filled(r, 0.0, bg);
                        text_painter.galley(r.min, g, theme.text);
                        text_painter.hline(r.x_range(), r.bottom() - 1.0, egui::Stroke::new(1.0, theme.text));
                    }
                    let phase = (now - doc.caret_time).rem_euclid(1.0);
                    if phase < 0.6 {
                        text_painter.rect_filled(caret_rect, 0.0, ui.visuals().text_cursor.stroke.color);
                    }
                    let next = if phase < 0.6 { 0.6 - phase } else { 1.0 - phase };
                    ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(next + 0.01));
                    ui.ctx().output_mut(|o| {
                        o.ime = Some(egui::output::IMEOutput {
                            purpose: Default::default(),
                            rect: clip,
                            cursor_rect: caret_rect,
                            should_interrupt_composition: false,
                        })
                    });
                }

                // ---- Debugger values after the end of lines
                let inline_font = FontId::monospace((font_size - 1.0).max(9.0));
                for (line, text) in &debug.inline {
                    if *line < first || *line >= last {
                        continue;
                    }
                    let ll = &layouts[line - first];
                    let y = line_y(*line);
                    let x = text_x + ll.width(&doc.text, char_w) + char_w * 3.0;
                    let galley = text_painter.layout_no_wrap(text.clone(), inline_font.clone(), theme.function);
                    let r = Rect::from_min_size(pos2(x, y + (row_h - galley.size().y) * 0.5), galley.size()).expand2(vec2(5.0, 0.0));
                    text_painter.rect_filled(r, 3.0, theme.function.gamma_multiply(0.12));
                    text_painter.galley(pos2(x, r.top()), galley, theme.function);
                }

                // ---- Gutter, pinned to the left edge of the viewport
                let gp = painter.with_clip_rect(gutter);
                gp.rect_filled(gutter, 0.0, bg);
                for line in first..last {
                    let color = if line == head_line && focused { theme.gutter_current_fg } else { theme.gutter_fg };
                    gp.text(pos2(gutter.right() - char_w, line_y(line)), Align2::RIGHT_TOP, (line + 1).to_string(), font.clone(), color);
                }
                let gresp = ui.interact(gutter, id.with("gutter"), Sense::click());
                let dot_center = |y: f32| pos2(gutter.left() + char_w * 0.5 + 4.0, y + row_h * 0.5);
                let dot_r = (row_h * 0.28).clamp(3.0, 6.0);
                let bp_color = theme.error;
                for &(line, verified) in &debug.breakpoints {
                    if line < first || line >= last {
                        continue;
                    }
                    let c = dot_center(line_y(line));
                    if verified {
                        gp.circle_filled(c, dot_r, bp_color);
                    } else {
                        gp.circle_stroke(c, dot_r - 0.5, egui::Stroke::new(1.5, bp_color));
                    }
                }
                if let Some(p) = gresp.hover_pos() {
                    let line = line_at(p.y);
                    if !debug.breakpoints.iter().any(|b| b.0 == line) {
                        gp.circle_filled(dot_center(line_y(line)), dot_r, bp_color.gamma_multiply(0.35));
                    }
                    if gresp.clicked() {
                        out.gutter_click = Some(line);
                    }
                }
                if let Some(line) = debug.exec_line
                    && line < line_count
                {
                    let color = if debug.exec_top { theme.warning } else { theme.function };
                    let c = dot_center(line_y(line));
                    let h = dot_r * 1.1;
                    gp.add(Shape::convex_polygon(vec![pos2(c.x - h, c.y - h), pos2(c.x + h, c.y), pos2(c.x - h, c.y + h)], color, egui::Stroke::NONE));
                }

                // ---- Compiler diagnostics: a gutter marker and a squiggle.
                // Worst severity last so it wins the gutter marker; one tooltip per line.
                let mut sorted: Vec<&&crate::make::Diagnostic> = diags.iter().filter(|d| d.line >= first && d.line < last).collect();
                sorted.sort_by(|a, b| b.severity.cmp(&a.severity));
                let mut line_msgs: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
                for d in &sorted {
                    if d.severity != crate::make::Severity::Note {
                        let e = line_msgs.entry(d.line).or_default();
                        if !e.is_empty() {
                            e.insert(0, '\n');
                        }
                        let word = if d.severity == crate::make::Severity::Error { "error" } else { "warning" };
                        e.insert_str(0, &format!("{word}: {}", d.message));
                    }
                }
                for (n, d) in sorted.iter().enumerate() {
                    let color = match d.severity {
                        crate::make::Severity::Error => theme.error,
                        crate::make::Severity::Warning => theme.warning,
                        crate::make::Severity::Note => continue,
                    };
                    let y = line_y(d.line);
                    let ll = &layouts[d.line - first];
                    gp.rect_filled(Rect::from_min_size(pos2(gutter.left() + 2.0, y + 2.0), vec2(3.0, row_h - 4.0)), 1.0, color);
                    let line_text = &doc.text[ll.range.clone()];
                    let col = d.col.min(line_text.chars().count());
                    let b0 = ll.range.start + textops::char_to_byte(line_text, col);
                    let word_len = line_text[b0 - ll.range.start..].bytes().take_while(|&c| is_ident_byte(c) || c >= 0x80).count().max(1);
                    let b1 = textops::floor_char_boundary(&doc.text, (b0 + word_len).min(ll.range.end));
                    let x0 = text_x + ll.x_of(&doc.text, b0, char_w);
                    let x1 = text_x + ll.x_of(&doc.text, b1, char_w);
                    let x1 = if x1 <= x0 { x0 + char_w } else { x1 };
                    let uy = y + row_h - 2.0;
                    let mut pts = Vec::new();
                    let mut x = x0;
                    let mut up = true;
                    while x < x1 {
                        pts.push(pos2(x, if up { uy - 1.5 } else { uy + 0.5 }));
                        x += 2.5;
                        up = !up;
                    }
                    pts.push(pos2(x1, uy));
                    text_painter.add(Shape::line(pts, egui::Stroke::new(1.2, color)));
                    let hover_rect = Rect::from_min_max(pos2(gutter.left(), y), pos2(gutter.right(), y + row_h))
                        .union(Rect::from_min_max(pos2(x0, y), pos2(x1, y + row_h)));
                    let msg = line_msgs.get(&d.line).cloned().unwrap_or_default();
                    ui.interact(hover_rect, id.with(("diag", n)), Sense::hover()).on_hover_text(msg);
                }

                // ---- Keep the caret in view
                if let Some(center) = doc.scroll_to_caret.take() {
                    let r = caret_rect.expand2(vec2(char_w * 4.0, 0.0));
                    let r = Rect::from_min_max(pos2(r.left().max(rect.left() + text_margin_left - char_w) - gutter_w, r.top()), r.max);
                    ui.scroll_to_rect(r, if center { Some(Align::Center) } else { None });
                }
            });
    });
    if !focused {
        doc.preedit.clear();
    }
    out.focused = focused;
    out
}

thread_local! {
    /// The last line copied or cut without a selection.
    static LINE_COPY: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// One input event for the focused editor: caret movement, editing,
/// clipboard, undo, IME.
fn handle_event(ui: &Ui, doc: &mut Document, ev: &Event, mac: bool, page: usize, font: &FontId, char_w: f32) {
    let sel = doc.selection(ui.ctx());
    match ev {
        Event::Text(t) if !t.is_empty() && doc.preedit.is_empty() => {
            doc.edit(sel, t);
        }
        Event::Paste(t) => {
            let t = t.replace("\r\n", "\n");
            // A line copied without a selection pastes as a whole line above
            // the caret's line (like CLion).
            let whole_line = sel.is_empty() && LINE_COPY.with(|c| c.borrow().as_deref() == Some(t.as_str()));
            if whole_line {
                let ls = doc.index.line_start(doc.index.line_of(doc.head));
                let col = doc.head - ls;
                doc.edit(ls..ls, &t);
                let c = (ls + t.len() + col).min(doc.text.len());
                doc.anchor = c;
                doc.head = c;
            } else {
                doc.edit(sel, &t);
            }
        }
        Event::Copy | Event::Cut => {
            let no_sel = sel.is_empty();
            let r = if no_sel {
                // No selection: the whole line (like CLion).
                let l = doc.index.line_of(doc.head);
                doc.index.line_start(l)..(doc.index.line_end(l) + 1).min(doc.text.len())
            } else {
                sel
            };
            let copied = doc.text[r.clone()].to_string();
            LINE_COPY.with(|c| *c.borrow_mut() = no_sel.then(|| copied.clone()));
            ui.ctx().copy_text(copied);
            if matches!(ev, Event::Cut) {
                doc.edit(r, "");
            }
        }
        Event::Ime(ime) => match ime {
            egui::ImeEvent::Preedit { text, .. } => doc.preedit = text.clone(),
            egui::ImeEvent::Commit(t) => {
                doc.preedit.clear();
                if !t.is_empty() {
                    doc.edit(sel, t);
                }
            }
            _ => {}
        },
        Event::Key { key, pressed: true, modifiers: m, .. } => handle_key(ui, doc, *key, *m, mac, page, font, char_w),
        _ => {}
    }
}

fn handle_key(ui: &Ui, doc: &mut Document, key: Key, m: Modifiers, mac: bool, page: usize, font: &FontId, char_w: f32) {
    let sel = doc.selection(ui.ctx());
    let word = if mac { m.alt } else { m.ctrl };
    let line_mod = mac && m.mac_cmd;
    let head = doc.head.min(doc.text.len());
    let line = doc.index.line_of(head);
    let ls = doc.index.line_start(line);
    let le = doc.index.line_end(line);
    // Home goes to the first non-blank character, then to column 0.
    let smart_home = {
        let indent = doc.text[ls..le].len() - doc.text[ls..le].trim_start_matches([' ', '\t']).len();
        if head == ls + indent { ls } else { ls + indent }
    };
    let move_to = |doc: &mut Document, b: usize, extend: bool| {
        doc.head = b;
        if !extend {
            doc.anchor = b;
        }
        doc.desired_x = None;
        doc.index.seal();
        doc.scroll_to_caret.get_or_insert(false);
    };
    let vertical = |ui: &Ui, doc: &mut Document, down: bool, n: usize, extend: bool| {
        let x = doc.desired_x.unwrap_or_else(|| plain_layout(ui, doc, line, font).x_of(&doc.text, head, char_w));
        let target = if down { (line + n) as isize } else { line as isize - n as isize };
        let b = if target < 0 {
            0
        } else if target as usize >= doc.index.line_count() {
            doc.text.len()
        } else {
            plain_layout(ui, doc, target as usize, font).byte_at_x(&doc.text, x, char_w)
        };
        move_to(doc, b, extend);
        doc.desired_x = Some(x);
    };
    match key {
        Key::ArrowLeft | Key::ArrowRight => {
            let fwd = key == Key::ArrowRight;
            if !m.shift && !sel.is_empty() && !word && !line_mod {
                move_to(doc, if fwd { sel.end } else { sel.start }, false);
                return;
            }
            let b = if line_mod {
                if fwd { le } else { smart_home }
            } else if word {
                if fwd { word_right(&doc.text, head) } else { word_left(&doc.text, head) }
            } else if fwd {
                next_char(&doc.text, head)
            } else {
                prev_char(&doc.text, head)
            };
            move_to(doc, b, m.shift);
        }
        Key::ArrowUp | Key::ArrowDown => {
            let down = key == Key::ArrowDown;
            if line_mod {
                move_to(doc, if down { doc.text.len() } else { 0 }, m.shift);
            } else if m.alt || m.ctrl {
                // Left for other bindings (e.g. move line).
            } else {
                vertical(ui, doc, down, 1, m.shift);
            }
        }
        Key::PageUp | Key::PageDown => vertical(ui, doc, key == Key::PageDown, page, m.shift),
        Key::Home | Key::End => {
            let end = key == Key::End;
            let b = if m.command { if end { doc.text.len() } else { 0 } } else if end { le } else { smart_home };
            move_to(doc, b, m.shift);
        }
        Key::Backspace => {
            if !sel.is_empty() {
                doc.edit(sel, "");
                return;
            }
            if head == 0 {
                return;
            }
            let start = if line_mod {
                if head == ls { head - 1 } else { ls }
            } else if word {
                word_left(&doc.text, head)
            } else {
                let before = &doc.text[ls..head];
                let w = textops::indent_width();
                if !doc.lang.uses_hard_tabs() && !before.is_empty() && w > 0 && before.bytes().all(|c| c == b' ') {
                    // Soft tabs: back to the previous indent stop.
                    ls + ((before.len() - 1) / w) * w
                } else {
                    prev_char(&doc.text, head)
                }
            };
            doc.edit(start..head, "");
        }
        Key::Delete => {
            if !sel.is_empty() {
                doc.edit(sel, "");
                return;
            }
            let end = if line_mod {
                if head == le { (le + 1).min(doc.text.len()) } else { le }
            } else if word {
                word_right(&doc.text, head)
            } else {
                next_char(&doc.text, head)
            };
            if end > head {
                doc.edit(head..end, "");
            }
        }
        Key::A if m.command && !m.shift => {
            doc.anchor = 0;
            doc.head = doc.text.len();
            doc.index.seal();
        }
        Key::Z if m.command => {
            if m.shift { doc.redo() } else { doc.undo() }
        }
        Key::Y if m.ctrl && !mac => doc.redo(),
        _ => {}
    }
}

/// Write `data` to a brand-new file at `path`. Whatever is already at `path` is
/// removed first (a symlink is removed, not followed), and the file is created
/// exclusively, so a planted symlink can't redirect the write to another file.
fn write_new_file(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
    f.write_all(data)?;
    f.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A project can ship `.name.cedit-tmp` as a symlink to some other file;
    /// saving must not write through it.
    #[cfg(unix)]
    #[test]
    fn save_ignores_planted_temp_symlink() {
        let dir = std::env::temp_dir().join(format!("cedit-save-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let victim = dir.join("victim.txt");
        std::fs::write(&victim, "precious\n").unwrap();
        let file = dir.join("main.c");
        std::fs::write(&file, "int x;\n").unwrap();
        std::os::unix::fs::symlink(&victim, dir.join(".main.c.cedit-tmp")).unwrap();

        let mut doc = Document::open(&file).ok().expect("open");
        doc.text = "int y;\n".into();
        doc.save().unwrap();

        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious\n");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "int y;\n");
        assert!(!std::fs::symlink_metadata(&file).unwrap().file_type().is_symlink());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
