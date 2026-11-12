//! An open document and the code-editor widget that shows it.

use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use egui::text::{CCursor, CCursorRange};
use egui::{
    Align, Align2, Event, FontId, Galley, Id, Key, Modifiers, Rect, ScrollArea, Sense, Shape,
    TextBuffer, TextEdit, Ui, Vec2, pos2, vec2,
};

use crate::find::{FindState, hash_text};
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
    galley_cache: Option<(u64, Arc<Galley>)>,
    last_cursor: Option<(usize, usize, usize)>,
    /// Images and PDFs open in a read-only viewer instead of the text editor.
    pub viewer: Option<Viewer>,
    /// Markdown files: show the rendered preview beside the editor.
    pub md_preview: bool,
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
            galley_cache: None,
            last_cursor: None,
            viewer: None,
            md_preview: true,
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

    pub fn invalidate_layout(&mut self) {
        self.galley_cache = None;
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
        self.galley_cache = None;
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
        self.text.bytes().filter(|&b| b == b'\n').count() + 1
    }

    /// Current selection as a sorted byte range, read from the widget state.
    pub fn selection(&self, ctx: &egui::Context) -> Range<usize> {
        let state = TextEdit::load_state(ctx, self.text_edit_id());
        let Some(range) = state.and_then(|s| s.cursor.char_range()) else {
            return 0..0;
        };
        let r = range.as_sorted_char_range();
        textops::char_to_byte(&self.text, r.start.0)..textops::char_to_byte(&self.text, r.end.0)
    }

    pub fn selected_text(&self, ctx: &egui::Context) -> String {
        let s = self.selection(ctx);
        self.text[s].to_string()
    }

    fn set_selection_now(&self, ctx: &egui::Context, sel: Range<usize>) {
        let id = self.text_edit_id();
        let mut state = TextEdit::load_state(ctx, id).unwrap_or_default();
        let a = textops::byte_to_char(&self.text, sel.start);
        let b = textops::byte_to_char(&self.text, sel.end);
        state
            .cursor
            .set_char_range(Some(CCursorRange::two(CCursor::new(a), CCursor::new(b))));
        TextEdit::store_state(ctx, id, state);
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
    }
    let action = ctx.input_mut(|i| {
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
        // Typing a closing brace on a blank line dedents it.
        let mut closing = None;
        i.events.retain(|e| {
            if closing.is_none()
                && let Event::Text(t) = e
                && (t == "}" || t == ")" || t == "]")
            {
                closing = t.chars().next();
                return false;
            }
            true
        });
        closing.map(Action::Closing)
    });
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
            let c = textops::type_closing_brace(&mut doc.text, sel, ch);
            c..c
        }
    };
    doc.mark_changed();
    doc.set_selection_now(&ctx, new_sel);
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
    }

    let mut scroll_to_cursor = false;
    if let Some(sel) = doc.pending_select.take() {
        let sel = textops::floor_char_boundary(&doc.text, sel.start)
            ..textops::floor_char_boundary(&doc.text, sel.end);
        doc.set_selection_now(ui.ctx(), sel);
        if doc.pending_focus {
            doc.refocus_frames = 3;
        }
        doc.pending_focus = true;
        scroll_to_cursor = true;
    }

    if doc.refocus_frames > 0 {
        doc.refocus_frames -= 1;
        if !ui.memory(|m| m.has_focus(id)) {
            ui.memory_mut(|m| m.request_focus(id));
        }
        ui.ctx().request_repaint();
    }

    let (row_height, char_w) = ui.fonts_mut(|f| (f.row_height(&font), f.glyph_width(&font, '0')));
    let line_count = doc.line_count();
    let digits = line_count.to_string().len().max(3);
    let gutter_w = char_w * (digits as f32 + 3.0);
    let text_margin_left = gutter_w + char_w;
    let avail = ui.available_size();

    let lang = doc.lang;
    let ppp = ui.ctx().pixels_per_point();
    let sem_version = doc.semantic_version;
    let semantic = &doc.semantic;
    let cache = &mut doc.galley_cache;
    let mut layouter = |ui: &Ui, buf: &dyn TextBuffer, _wrap: f32| -> Arc<Galley> {
        let text = buf.as_str();
        let th = hash_text(text);
        let matches_valid = find.open && find.text_hash == th;
        let mut h = DefaultHasher::new();
        (th, matches_valid, font_size.to_bits(), ppp.to_bits(), lang, sem_version).hash(&mut h);
        if matches_valid {
            (&find.matches, find.current).hash(&mut h);
        }
        let key = h.finish();
        if let Some((k, g)) = cache.as_ref()
            && *k == key
        {
            return g.clone();
        }
        let (m, cur): (&[Range<usize>], _) = if matches_valid {
            (&find.matches, find.current)
        } else {
            (&[], None)
        };
        let mut job = highlight::layout_job(lang, text, font.clone(), theme, m, cur, semantic);
        job.wrap.max_width = f32::INFINITY;
        let g = ui.fonts_mut(|f| f.layout_job(job));
        *cache = Some((key, g.clone()));
        g
    };

    let mut focused = false;
    let bg = theme.bg;
    egui::Frame::NONE.fill(bg).show(ui, |ui| {
        ScrollArea::both()
            .id_salt(("cedit-scroll", doc.id))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = Vec2::ZERO;
                let line_bg_idx = ui.painter().add(Shape::Noop);
                let exec_bg_idx = ui.painter().add(Shape::Noop);
                let heat_bg_idx = ui.painter().add(Shape::Noop);
                let output = ui.horizontal_top(|ui| {
                ui.add_space(text_margin_left);
                TextEdit::multiline(&mut doc.text)
                    .id(id)
                    .font(font.clone())
                    .frame(egui::Frame::NONE)
                    .margin(egui::Margin {
                        left: 0,
                        right: 24,
                        top: 4,
                        bottom: 4,
                    })
                    .desired_width(f32::INFINITY)
                    .min_size(vec2((avail.x - text_margin_left).max(10.0), avail.y))
                    .lock_focus(true)
                    .layouter(&mut layouter)
                    .show(ui)
                }).inner;

                focused = output.response.response.has_focus();
                if output.response.response.changed() {
                    doc.dirty = doc.text != doc.saved;
                    doc.version += 1;
                }

                let galley = &output.galley;
                let origin = output.galley_pos - vec2(galley.rect.left(), 0.0);

                // Pointer: hover and Cmd+click targets.
                let resp = &output.response.response;
                if let Some(p) = resp.hover_pos() {
                    let c = galley.cursor_from_pos(p - origin);
                    let b = textops::char_to_byte(&doc.text, c.index.0);
                    out.hover = Some((b, p));
                    if resp.clicked() && ui.input(|i| i.modifiers.command) {
                        out.cmd_click = Some(b);
                    }
                }

                // Cursor bookkeeping (status bar + current line).
                let mut cursor_row_y = None;
                if let Some(range) = output.cursor_range {
                    let rect = galley.pos_from_cursor(range.primary);
                    out.cursor_rect = Some(Rect::from_min_size(origin + rect.min.to_vec2(), vec2(2.0, row_height)));
                    let cb = textops::char_to_byte(&doc.text, range.primary.index.0);
                    out.cursor = Some(cb);
                    doc.cursor_byte = cb;
                    cursor_row_y = Some(origin.y + rect.min.y);
                    let key = (range.primary.index.0, range.secondary.index.0, doc.text.len());
                    if doc.last_cursor != Some(key) {
                        doc.last_cursor = Some(key);
                        let b = textops::char_to_byte(&doc.text, range.primary.index.0);
                        doc.cursor_line_col = textops::line_col(&doc.text, b);
                        let r = range.as_sorted_char_range();
                        doc.selection_chars = r.end.0 - r.start.0;
                    }
                    if scroll_to_cursor {
                        let r = Rect::from_min_size(
                            origin + rect.min.to_vec2(),
                            vec2(2.0, row_height),
                        );
                        ui.scroll_to_rect(r.expand2(vec2(80.0, 0.0)), Some(Align::Center));
                    }
                }

                let clip = ui.clip_rect();
                if let Some(y) = cursor_row_y
                    && focused
                {
                    ui.painter().set(
                        line_bg_idx,
                        Shape::rect_filled(
                            Rect::from_min_max(pos2(clip.left(), y), pos2(clip.right(), y + row_height)),
                            0.0,
                            theme.current_line_bg,
                        ),
                    );
                }

                // Line-number gutter, pinned to the left edge of the viewport.
                let gutter = Rect::from_min_max(
                    pos2(clip.left(), clip.top()),
                    pos2(clip.left() + gutter_w, clip.bottom()),
                );
                let painter = ui.painter().with_clip_rect(gutter);
                painter.rect_filled(gutter, 0.0, bg);
                let rows = &galley.rows;
                let pitch = if rows.len() > 1 {
                    (rows[rows.len() - 1].pos.y - rows[0].pos.y) / (rows.len() - 1) as f32
                } else {
                    row_height
                };
                let first = (((clip.top() - origin.y) / pitch).floor().max(0.0)) as usize;
                let last = ((((clip.bottom() - origin.y) / pitch).ceil()) as usize + 1)
                    .min(line_count)
                    .min(rows.len());
                let cur_line = doc.cursor_line_col.0;
                for line in first..last {
                    let y = origin.y + rows[line].pos.y;
                    let color = if line == cur_line && focused {
                        theme.gutter_current_fg
                    } else {
                        theme.gutter_fg
                    };
                    painter.text(
                        pos2(gutter.right() - char_w, y),
                        Align2::RIGHT_TOP,
                        (line + 1).to_string(),
                        font.clone(),
                        color,
                    );
                }
                // Breakpoints: click the gutter to toggle.
                let gresp = ui.interact(gutter, id.with("gutter"), Sense::click());
                let line_at = |y: f32| (((y - origin.y) / pitch).floor().max(0.0) as usize).min(line_count.saturating_sub(1));
                let dot_center = |y: f32| pos2(gutter.left() + char_w * 0.5 + 4.0, y + pitch * 0.5);
                let dot_r = (pitch * 0.28).clamp(3.0, 6.0);
                let bp_color = theme.error;
                for &(line, verified) in &debug.breakpoints {
                    let Some(row) = rows.get(line) else { continue };
                    let c = dot_center(origin.y + row.pos.y);
                    if verified {
                        painter.circle_filled(c, dot_r, bp_color);
                    } else {
                        painter.circle_stroke(c, dot_r - 0.5, egui::Stroke::new(1.5, bp_color));
                    }
                }
                if let Some(p) = gresp.hover_pos() {
                    let line = line_at(p.y);
                    if !debug.breakpoints.iter().any(|b| b.0 == line)
                        && let Some(row) = rows.get(line)
                    {
                        painter.circle_filled(dot_center(origin.y + row.pos.y), dot_r, bp_color.gamma_multiply(0.35));
                    }
                    if gresp.clicked() {
                        out.gutter_click = Some(line);
                    }
                }
                // Profiler heat: shade hot lines and show their share of samples.
                let mut heat_shapes = Vec::new();
                for &(line, frac) in &debug.heat {
                    let Some(row) = rows.get(line) else { continue };
                    let y = origin.y + row.pos.y;
                    if y + pitch < clip.top() || y > clip.bottom() || frac < 0.002 {
                        continue;
                    }
                    let alpha = (0.08 + frac * 0.8).min(0.4);
                    heat_shapes.push(Shape::rect_filled(
                        Rect::from_min_max(pos2(gutter.right(), y), pos2(clip.right(), y + pitch)),
                        0.0,
                        theme.error.gamma_multiply(alpha),
                    ));
                    ui.painter().text(
                        pos2(clip.right() - 8.0, y + pitch * 0.5),
                        Align2::RIGHT_CENTER,
                        format!("{:.1}%", frac * 100.0),
                        FontId::proportional((font_size - 2.0).max(9.0)),
                        theme.warning,
                    );
                }
                ui.painter().set(heat_bg_idx, Shape::Vec(heat_shapes));
                // Debugger: variable values after the end of the lines that use them.
                let text_clip = Rect::from_min_max(pos2(gutter.right(), clip.top()), clip.max);
                let inline_painter = ui.painter().with_clip_rect(text_clip);
                let inline_font = FontId::monospace((font_size - 1.0).max(9.0));
                for (line, text) in &debug.inline {
                    let Some(row) = rows.get(*line) else { continue };
                    let y = origin.y + row.pos.y;
                    if y + pitch < clip.top() || y > clip.bottom() {
                        continue;
                    }
                    let x = origin.x + row.rect().right() + char_w * 3.0;
                    let galley = inline_painter.layout_no_wrap(text.clone(), inline_font.clone(), theme.function);
                    let r = Rect::from_min_size(pos2(x, y + (pitch - galley.size().y) * 0.5), galley.size()).expand2(vec2(5.0, 0.0));
                    inline_painter.rect_filled(r, 3.0, theme.function.gamma_multiply(0.12));
                    inline_painter.galley(pos2(x, r.top()), galley, theme.function);
                }
                if let Some(line) = debug.exec_line
                    && let Some(row) = rows.get(line)
                {
                    let y = origin.y + row.pos.y;
                    let color = if debug.exec_top { theme.warning } else { theme.function };
                    ui.painter().set(
                        exec_bg_idx,
                        Shape::rect_filled(
                            Rect::from_min_max(pos2(clip.left(), y), pos2(clip.right(), y + pitch)),
                            0.0,
                            color.gamma_multiply(0.18),
                        ),
                    );
                    // An arrow pointing at the line.
                    let c = dot_center(y);
                    let h = dot_r * 1.1;
                    painter.add(Shape::convex_polygon(
                        vec![pos2(c.x - h, c.y - h), pos2(c.x + h, c.y), pos2(c.x - h, c.y + h)],
                        color,
                        egui::Stroke::NONE,
                    ));
                }

                // Compiler diagnostics: a marker in the gutter and an underline.
                // Worst severity last so it wins the gutter marker; one tooltip per line.
                let mut sorted: Vec<&&crate::make::Diagnostic> = diags.iter().collect();
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
                    let Some(row) = rows.get(d.line) else { continue };
                    let y = origin.y + row.pos.y;
                    if y + pitch < clip.top() || y > clip.bottom() {
                        continue;
                    }
                    let color = match d.severity {
                        crate::make::Severity::Error => theme.error,
                        crate::make::Severity::Warning => theme.warning,
                        crate::make::Severity::Note => continue,
                    };
                    let marker = Rect::from_min_size(pos2(gutter.left() + 2.0, y + 2.0), vec2(3.0, pitch - 4.0));
                    painter.rect_filled(marker, 1.0, color);
                    // Underline the word at the reported column.
                    let ls = textops::line_to_byte(&doc.text, d.line);
                    let le = textops::line_end(&doc.text, ls);
                    let line_text = &doc.text[ls..le];
                    let col = d.col.min(line_text.chars().count());
                    let word_len = line_text
                        .chars()
                        .skip(col)
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .count()
                        .max(1);
                    let x0 = origin.x + row.pos.x + row.row.x_offset(egui::text::CharIndex(col));
                    let x1 = origin.x + row.pos.x + row.row.x_offset(egui::text::CharIndex(col + word_len));
                    let x1 = if x1 <= x0 { x0 + char_w } else { x1 };
                    let uy = y + pitch - 2.0;
                    let mut pts = Vec::new();
                    let mut x = x0;
                    let mut up = true;
                    while x < x1 {
                        pts.push(pos2(x, if up { uy - 1.5 } else { uy + 0.5 }));
                        x += 2.5;
                        up = !up;
                    }
                    pts.push(pos2(x1, uy));
                    ui.painter().add(Shape::line(pts, egui::Stroke::new(1.2, color)));
                    // Hover anywhere on the line's marker or underline for the message.
                    let hover_rect = Rect::from_min_max(pos2(gutter.left(), y), pos2(gutter.right(), y + pitch))
                        .union(Rect::from_min_max(pos2(x0, y), pos2(x1, y + pitch)));
                    let msg = line_msgs.get(&d.line).cloned().unwrap_or_default();
                    ui.interact(hover_rect, id.with(("diag", n)), Sense::hover())
                        .on_hover_text(msg);
                }
            });
    });
    out.focused = focused;
    out
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
