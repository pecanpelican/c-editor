//! Integrated terminal: a shell (or your program) on a pseudo-terminal,
//! parsed with `vt100` and drawn with egui.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use egui::{Color32, FontId, Key, Modifiers, Pos2, Rect, Sense, Stroke, Ui, Vec2, pos2, vec2};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};

use crate::theme::Theme;

const SCROLLBACK: usize = 10_000;
const PAD: Vec2 = vec2(6.0, 3.0);

type Cell = (u16, u16); // (row, col) on the visible screen

pub struct Terminal {
    pub title: String,
    /// The program started by Run (as opposed to an interactive shell).
    pub is_run: bool,
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    pid: Option<u32>,
    reader_done: Arc<AtomicBool>,
    pub dir: std::path::PathBuf,
    /// Set once the process has exited: (time seen, message).
    exited: Option<(f64, String)>,
    exit_reported: bool,
    stopped: bool,
    size: (u16, u16),
    selection: Option<(Cell, Cell)>,
    scroll_acc: f32,
}

/// What the app needs to know after drawing a terminal.
pub struct TermOutput {
    pub focused: bool,
    /// Cmd/Ctrl+click on `path:line[:col]`: (path as written, line, col).
    pub open: Option<(String, usize, usize)>,
}

impl Terminal {
    /// Start `program args` (or the user's shell when `program` is None) in `dir`.
    pub fn spawn(
        title: String,
        program: Option<(String, Vec<String>)>,
        dir: &Path,
        env: &[(String, String)],
        ctx: &egui::Context,
    ) -> Result<Self, String> {
        let size = (24u16, 80u16);
        let pty = portable_pty::native_pty_system();
        let pair = pty
            .openpty(PtySize { rows: size.0, cols: size.1, pixel_width: 0, pixel_height: 0 })
            .map_err(|e| format!("Couldn't open a terminal: {e}"))?;
        let is_run = program.is_some();
        let mut cmd = match program {
            Some((p, args)) => {
                let mut c = CommandBuilder::new(p);
                c.args(args);
                c
            }
            None => default_shell(),
        };
        cmd.cwd(dir);
        if cfg!(unix) {
            // Same form as cedit's paths (see make.rs), not the symlink-resolved one.
            cmd.env("PWD", dir);
            cmd.env("TERM", "xterm-256color");
            cmd.env("COLORTERM", "truecolor");
        }
        cmd.env("TERM_PROGRAM", "cedit");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = pair.slave.spawn_command(cmd).map_err(|e| format!("Couldn't start the shell: {e}"))?;
        drop(pair.slave);
        let pid = child.process_id();
        let mut reader = pair.master.try_clone_reader().map_err(|e| e.to_string())?;
        let writer: Arc<Mutex<Box<dyn Write + Send>>> =
            Arc::new(Mutex::new(pair.master.take_writer().map_err(|e| e.to_string())?));
        let rows = std::num::NonZeroU16::new(size.0).unwrap();
        let cols = std::num::NonZeroU16::new(size.1).unwrap();
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, SCROLLBACK)));
        let reader_done = Arc::new(AtomicBool::new(false));
        {
            let parser = parser.clone();
            let writer = writer.clone();
            let done = reader_done.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 16 * 1024];
                loop {
                    let n = match reader.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    let data = &buf[..n];
                    let mut reply = Vec::new();
                    if let Ok(mut p) = parser.lock() {
                        p.process(data);
                        // Answer the queries shells and Windows' ConPTY send.
                        if contains(data, b"\x1b[6n") {
                            let (r, c) = p.screen().cursor_position();
                            reply.extend_from_slice(format!("\x1b[{};{}R", r + 1, c + 1).as_bytes());
                        }
                        if contains(data, b"\x1b[c") || contains(data, b"\x1b[0c") {
                            reply.extend_from_slice(b"\x1b[?1;2c");
                        }
                    }
                    if !reply.is_empty()
                        && let Ok(mut w) = writer.lock()
                    {
                        let _ = w.write_all(&reply);
                        let _ = w.flush();
                    }
                    ctx.request_repaint();
                }
                done.store(true, Ordering::Relaxed);
                ctx.request_repaint();
            });
        }
        Ok(Self {
            title,
            is_run,
            parser,
            writer,
            master: pair.master,
            child,
            pid,
            reader_done,
            dir: dir.to_path_buf(),
            exited: None,
            exit_reported: false,
            stopped: false,
            size,
            selection: None,
            scroll_acc: 0.0,
        })
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub fn is_running(&self) -> bool {
        self.exited.is_none()
    }

    /// True once the process has exited and its output has been drained.
    pub fn finished(&self) -> bool {
        self.exit_reported
    }

    pub fn write(&self, bytes: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }

    /// Stop the program and everything it started.
    pub fn stop(&mut self) {
        if !self.is_running() {
            return;
        }
        self.stopped = true;
        match self.pid {
            Some(pid) => crate::make::kill_tree(pid),
            None => {
                let _ = self.child.kill();
            }
        }
    }

    /// Check whether the process exited. Call once a frame.
    pub fn poll(&mut self, now: f64, ctx: &egui::Context) {
        if self.exited.is_none()
            && let Ok(Some(status)) = self.child.try_wait()
        {
            let msg = if self.stopped {
                "Process stopped".to_string()
            } else if let Some(sig) = status.signal() {
                format!("Process terminated ({sig})")
            } else {
                format!("Process exited with code {}", status.exit_code())
            };
            self.exited = Some((now, msg));
        }
        if let Some((t, msg)) = &self.exited
            && !self.exit_reported
        {
            // Let the last of the output arrive before the exit line.
            if self.reader_done.load(Ordering::Relaxed) || now - t > 0.3 {
                self.exit_reported = true;
                if self.is_run
                    && let Ok(mut p) = self.parser.lock()
                {
                    p.process(format!("\r\n\x1b[0m\x1b[90m[{msg}]\x1b[0m\r\n").as_bytes());
                }
            } else {
                ctx.request_repaint_after(std::time::Duration::from_millis(100));
            }
        }
        if self.exited.is_none() && cfg!(windows) {
            // ConPTY doesn't close the pipe when the child exits; keep checking.
            ctx.request_repaint_after(std::time::Duration::from_millis(500));
        }
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        if (rows, cols) == self.size || rows == 0 || cols == 0 {
            return;
        }
        self.size = (rows, cols);
        if let Ok(mut p) = self.parser.lock() {
            p.screen_mut().set_size(std::num::NonZeroU16::new(rows).unwrap(), std::num::NonZeroU16::new(cols).unwrap());
        }
        let _ = self.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
        self.selection = None;
    }

    fn selected_text(&self) -> Option<String> {
        let ((r0, c0), (r1, c1)) = normalize(self.selection?);
        let p = self.parser.lock().ok()?;
        let text = p.screen().contents_between(r0, c0, r1, c1 + 1);
        (!text.is_empty()).then_some(text)
    }

    /// Draw the terminal into the remaining space and handle its input.
    pub fn ui(&mut self, ui: &mut Ui, theme: &Theme, font_size: f32, request_focus: bool) -> TermOutput {
        let font = FontId::monospace(font_size);
        let (cw, rh) = ui.fonts_mut(|f| (f.glyph_width(&font, 'M'), f.row_height(&font)));
        let (rect, resp) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        let inner = rect.shrink2(PAD);
        let cols = ((inner.width() / cw).floor() as i32).clamp(2, 1000) as u16;
        let rows = ((inner.height() / rh).floor() as i32).clamp(1, 500) as u16;
        self.resize(rows, cols);

        if request_focus || resp.clicked() || resp.drag_started() {
            resp.request_focus();
        }
        let focused = resp.has_focus();
        if focused {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    resp.id,
                    egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true },
                )
            });
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
        }

        let cell_at = |p: Pos2| -> Cell {
            let c = ((p.x - inner.left()) / cw).floor().clamp(0.0, cols as f32 - 1.0) as u16;
            let r = ((p.y - inner.top()) / rh).floor().clamp(0.0, rows as f32 - 1.0) as u16;
            (r, c)
        };

        // Mouse: selection and scrolling.
        if resp.drag_started()
            && let Some(p) = resp.interact_pointer_pos()
        {
            let c = cell_at(p);
            self.selection = Some((c, c));
        } else if resp.dragged()
            && let (Some(p), Some((a, _))) = (resp.interact_pointer_pos(), self.selection)
        {
            self.selection = Some((a, cell_at(p)));
        } else if resp.clicked() {
            self.selection = None;
        }
        let mut open = None;
        let link_mod = ui.input(|i| i.modifiers.command);
        if link_mod
            && resp.hovered()
            && let Some(p) = resp.hover_pos()
        {
            let (r, c) = cell_at(p);
            if let Some(link) = self.link_at(r, c) {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                if resp.clicked() {
                    open = Some(link);
                }
            }
        }
        if resp.hovered() {
            let dy = ui.input(|i| i.smooth_scroll_delta.y);
            if dy != 0.0 {
                self.scroll_acc += dy / rh;
                let lines = self.scroll_acc.trunc() as i32;
                self.scroll_acc -= lines as f32;
                if lines != 0 {
                    self.scroll_lines(lines);
                }
            }
        }

        if focused {
            self.handle_input(ui);
        }

        self.paint(ui, rect, inner, &font, cw, rh, theme, focused);
        TermOutput { focused, open }
    }

    /// A `path:line[:col]` under the given cell, as printed by compilers.
    fn link_at(&self, r: u16, c: u16) -> Option<(String, usize, usize)> {
        let p = self.parser.lock().ok()?;
        let row: Vec<char> = p.screen().rows(0, self.size.1).nth(r as usize)?.chars().collect();
        drop(p);
        let c = c as usize;
        let is_link_char = |ch: char| ch.is_alphanumeric() || "_./\\-~:+".contains(ch);
        if c >= row.len() || !is_link_char(row[c]) {
            return None;
        }
        let mut a = c;
        while a > 0 && is_link_char(row[a - 1]) {
            a -= 1;
        }
        let mut b = c;
        while b < row.len() && is_link_char(row[b]) {
            b += 1;
        }
        parse_link(&row[a..b].iter().collect::<String>())
    }

    /// Positive = up (back into history).
    fn scroll_lines(&mut self, lines: i32) {
        let Ok(mut p) = self.parser.lock() else { return };
        if p.screen().alternate_screen() {
            // Full-screen programs (less, vim) get arrow keys instead.
            let seq: &[u8] = match (lines > 0, p.screen().application_cursor()) {
                (true, true) => b"\x1bOA",
                (true, false) => b"\x1b[A",
                (false, true) => b"\x1bOB",
                (false, false) => b"\x1b[B",
            };
            drop(p);
            for _ in 0..lines.unsigned_abs().min(50) {
                self.write(seq);
            }
            return;
        }
        let cur = p.screen().scrollback() as i64;
        let next = (cur + lines as i64).max(0) as usize;
        p.screen_mut().set_scrollback(next);
        drop(p);
        self.selection = None;
    }

    fn reset_scroll(&mut self) {
        if let Ok(mut p) = self.parser.lock()
            && p.screen().scrollback() != 0
        {
            p.screen_mut().set_scrollback(0);
        }
    }

    fn handle_input(&mut self, ui: &mut Ui) {
        let events = ui.input(|i| i.events.clone());
        let (app_cursor, bracketed) = self
            .parser
            .lock()
            .map(|p| (p.screen().application_cursor(), p.screen().bracketed_paste()))
            .unwrap_or((false, false));
        let mac = cfg!(target_os = "macos");
        for ev in events {
            let mut out: Vec<u8> = Vec::new();
            match ev {
                egui::Event::Text(t) => out.extend_from_slice(t.as_bytes()),
                egui::Event::Ime(egui::ImeEvent::Commit(t)) => out.extend_from_slice(t.as_bytes()),
                egui::Event::Paste(t) => {
                    let t = t.replace("\r\n", "\r").replace('\n', "\r");
                    if bracketed {
                        out.extend_from_slice(b"\x1b[200~");
                        out.extend_from_slice(t.as_bytes());
                        out.extend_from_slice(b"\x1b[201~");
                    } else {
                        out.extend_from_slice(t.as_bytes());
                    }
                }
                egui::Event::Copy => {
                    // Cmd+C on a Mac; Ctrl+C elsewhere copies a selection or interrupts.
                    if let Some(text) = self.selected_text() {
                        ui.ctx().copy_text(text);
                        self.selection = None;
                        continue;
                    } else if !mac {
                        out.push(0x03);
                    }
                }
                egui::Event::Cut if !mac => out.push(0x18),
                egui::Event::Key { key, pressed: true, modifiers, .. } => {
                    if mac && modifiers.mac_cmd {
                        continue; // app shortcuts
                    }
                    if !mac && modifiers.ctrl && modifiers.shift && key == Key::C {
                        if let Some(text) = self.selected_text() {
                            ui.ctx().copy_text(text);
                        }
                        continue;
                    }
                    if modifiers.shift && matches!(key, Key::PageUp | Key::PageDown) {
                        let page = self.size.0 as i32 - 1;
                        self.scroll_lines(if key == Key::PageUp { page } else { -page });
                        continue;
                    }
                    key_bytes(key, modifiers, app_cursor, mac, &mut out);
                }
                _ => {}
            }
            if !out.is_empty() {
                self.reset_scroll();
                self.selection = None;
                self.write(&out);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn paint(&self, ui: &Ui, rect: Rect, inner: Rect, font: &FontId, cw: f32, rh: f32, theme: &Theme, focused: bool) {
        let painter = ui.painter_at(rect);
        let default_bg = theme.bg;
        let default_fg = theme.text;
        painter.rect_filled(rect, 0.0, default_bg);
        let Ok(p) = self.parser.lock() else { return };
        let screen = p.screen();
        let (rows, cols) = (self.size.0, self.size.1);
        let sel = self.selection.map(normalize);
        let sel_bg = ui.visuals().selection.bg_fill;
        let in_sel = |r: u16, c: u16| match sel {
            Some(((r0, c0), (r1, c1))) => (r, c) >= (r0, c0) && (r, c) <= (r1, c1),
            None => false,
        };
        let x_of = |c: u16| inner.left() + c as f32 * cw;
        for r in 0..rows {
            let y = inner.top() + r as f32 * rh;
            let mut c = 0;
            while c < cols {
                let Some(cell) = screen.cell(r, c) else { break };
                let style = CellStyle::of(cell, theme, in_sel(r, c), sel_bg);
                let start = c;
                let mut text = String::new();
                if cell.is_wide() {
                    // Draw wide characters on their own so the grid stays aligned.
                    text.push_str(cell.contents());
                    c += 2;
                } else {
                    while c < cols {
                        let Some(cell) = screen.cell(r, c) else { break };
                        if cell.is_wide() || CellStyle::of(cell, theme, in_sel(r, c), sel_bg) != style {
                            break;
                        }
                        if !cell.is_wide_continuation() {
                            let s = cell.contents();
                            text.push_str(if s.is_empty() { " " } else { s });
                        }
                        c += 1;
                    }
                }
                let x0 = x_of(start);
                if style.bg != default_bg {
                    painter.rect_filled(Rect::from_min_max(pos2(x0, y), pos2(x_of(c.min(cols)), y + rh)), 0.0, style.bg);
                }
                if !text.trim_end().is_empty() {
                    let mut job = egui::text::LayoutJob::default();
                    job.append(
                        text.trim_end(),
                        0.0,
                        egui::TextFormat {
                            font_id: font.clone(),
                            color: style.fg,
                            italics: style.italic,
                            underline: if style.underline { Stroke::new(1.0, style.fg) } else { Stroke::NONE },
                            ..Default::default()
                        },
                    );
                    painter.galley(pos2(x0, y), painter.layout_job(job), style.fg);
                }
            }
        }
        // Cursor.
        if screen.scrollback() == 0 && !screen.hide_cursor() && self.is_running() {
            let (r, c) = screen.cursor_position();
            if r < rows && c < cols {
                let cr = Rect::from_min_size(pos2(x_of(c), inner.top() + r as f32 * rh), vec2(cw, rh));
                if focused {
                    painter.rect_filled(cr, 0.0, default_fg);
                    if let Some(cell) = screen.cell(r, c)
                        && !cell.contents().is_empty()
                    {
                        painter.text(cr.min, egui::Align2::LEFT_TOP, cell.contents(), font.clone(), default_bg);
                    }
                } else {
                    painter.rect_stroke(cr.shrink(0.5), 0.0, Stroke::new(1.0, default_fg.gamma_multiply(0.6)), egui::StrokeKind::Inside);
                }
            }
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if self.is_running() {
            let _ = self.child.kill();
        }
    }
}

#[derive(PartialEq, Clone, Copy)]
struct CellStyle {
    fg: Color32,
    bg: Color32,
    italic: bool,
    underline: bool,
}

impl CellStyle {
    fn of(cell: &vt100::Cell, theme: &Theme, selected: bool, sel_bg: Color32) -> Self {
        let mut fg = color(cell.fgcolor(), theme, true, cell.bold());
        let mut bg = color(cell.bgcolor(), theme, false, false);
        if cell.inverse() {
            std::mem::swap(&mut fg, &mut bg);
        }
        if cell.dim() {
            fg = fg.gamma_multiply(0.6);
        }
        if selected {
            bg = sel_bg;
        }
        Self { fg, bg, italic: cell.italic(), underline: cell.underline() }
    }
}

fn color(c: vt100::Color, theme: &Theme, fg: bool, bold: bool) -> Color32 {
    match c {
        vt100::Color::Default => {
            if fg {
                theme.text
            } else {
                theme.bg
            }
        }
        vt100::Color::Idx(i) => {
            // Bold text in one of the 8 base colours shows as the bright version.
            let i = if bold && i < 8 { i + 8 } else { i };
            indexed(i, theme.dark)
        }
        vt100::Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
    }
}

fn indexed(i: u8, dark: bool) -> Color32 {
    const DARK: [u32; 16] = [
        0x000000, 0xcd3131, 0x0dbc79, 0xe5e510, 0x2472c8, 0xbc3fbc, 0x11a8cd, 0xe5e5e5, //
        0x666666, 0xf14c4c, 0x23d18b, 0xf5f543, 0x3b8eea, 0xd670d6, 0x29b8db, 0xffffff,
    ];
    const LIGHT: [u32; 16] = [
        0x000000, 0xcd3131, 0x00a000, 0x949800, 0x0451a5, 0xbc05bc, 0x0598bc, 0x555555, //
        0x666666, 0xcd3131, 0x14ce14, 0xb5ba00, 0x0451a5, 0xbc05bc, 0x0598bc, 0xa5a5a5,
    ];
    let rgb = |v: u32| Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8);
    match i {
        0..=15 => rgb(if dark { DARK } else { LIGHT }[i as usize]),
        16..=231 => {
            let i = i - 16;
            let level = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            Color32::from_rgb(level(i / 36), level((i / 6) % 6), level(i % 6))
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            Color32::from_rgb(v, v, v)
        }
    }
}

/// Bytes a key press sends to the program, xterm style.
fn key_bytes(key: Key, m: Modifiers, app_cursor: bool, mac: bool, out: &mut Vec<u8>) {
    // xterm modifier parameter: 1 + shift + 2*alt + 4*ctrl
    let mod_param = 1 + m.shift as u8 + 2 * m.alt as u8 + 4 * m.ctrl as u8;
    let csi_letter = |out: &mut Vec<u8>, letter: u8, ss3_ok: bool| {
        if mod_param > 1 {
            out.extend_from_slice(format!("\x1b[1;{mod_param}").as_bytes());
            out.push(letter);
        } else if app_cursor && ss3_ok {
            out.extend_from_slice(&[0x1b, b'O', letter]);
        } else {
            out.extend_from_slice(&[0x1b, b'[', letter]);
        }
    };
    let tilde = |out: &mut Vec<u8>, n: u8| {
        if mod_param > 1 {
            out.extend_from_slice(format!("\x1b[{n};{mod_param}~").as_bytes());
        } else {
            out.extend_from_slice(format!("\x1b[{n}~").as_bytes());
        }
    };
    match key {
        Key::Enter => out.push(b'\r'),
        Key::Backspace => {
            if m.alt {
                out.push(0x1b);
            }
            out.push(if m.ctrl { 0x08 } else { 0x7f });
        }
        Key::Tab => out.extend_from_slice(if m.shift { b"\x1b[Z" } else { b"\t" }),
        Key::Escape => out.push(0x1b),
        // Option+Left/Right on a Mac move by word, like Terminal.app.
        Key::ArrowLeft if mac && m.alt => out.extend_from_slice(b"\x1bb"),
        Key::ArrowRight if mac && m.alt => out.extend_from_slice(b"\x1bf"),
        Key::ArrowUp => csi_letter(out, b'A', true),
        Key::ArrowDown => csi_letter(out, b'B', true),
        Key::ArrowRight => csi_letter(out, b'C', true),
        Key::ArrowLeft => csi_letter(out, b'D', true),
        Key::Home => csi_letter(out, b'H', true),
        Key::End => csi_letter(out, b'F', true),
        Key::Insert => tilde(out, 2),
        Key::Delete => tilde(out, 3),
        Key::PageUp => tilde(out, 5),
        Key::PageDown => tilde(out, 6),
        Key::F1 => csi_letter(out, b'P', true),
        Key::F2 => csi_letter(out, b'Q', true),
        Key::F3 => csi_letter(out, b'R', true),
        Key::F4 => csi_letter(out, b'S', true),
        Key::F5 => tilde(out, 15),
        Key::F6 => tilde(out, 17),
        Key::F7 => tilde(out, 18),
        Key::F8 => tilde(out, 19),
        Key::F9 => tilde(out, 20),
        Key::F10 => tilde(out, 21),
        Key::F11 => tilde(out, 23),
        Key::F12 => tilde(out, 24),
        _ if m.ctrl => {
            if let Some(b) = ctrl_byte(key) {
                if m.alt {
                    out.push(0x1b);
                }
                out.push(b);
            }
        }
        // Alt+key elsewhere: the character arrives as text; prefix it with ESC
        // (Meta). On a Mac, Option types special characters instead.
        _ if m.alt && !mac && key_is_char(key) => out.push(0x1b),
        _ => {}
    }
}

fn ctrl_byte(key: Key) -> Option<u8> {
    let name = key.name();
    if name.len() == 1 {
        let ch = name.as_bytes()[0].to_ascii_uppercase();
        if ch.is_ascii_uppercase() {
            return Some(ch - b'A' + 1);
        }
    }
    Some(match key {
        Key::Space | Key::Num2 => 0x00,
        Key::OpenBracket => 0x1b,
        Key::Backslash => 0x1c,
        Key::CloseBracket => 0x1d,
        Key::Num6 => 0x1e,
        Key::Minus | Key::Slash => 0x1f,
        _ => return None,
    })
}

fn key_is_char(key: Key) -> bool {
    key.name().chars().count() == 1 || key == Key::Space
}

/// "src/main.c:12:5:" -> ("src/main.c", 12, 5). Windows drive letters are kept.
fn parse_link(word: &str) -> Option<(String, usize, usize)> {
    let word = word.trim_end_matches([':', '.']);
    let (drive, rest) = match word.as_bytes() {
        [d, b':', b'\\' | b'/', ..] if d.is_ascii_alphabetic() => word.split_at(2),
        _ => ("", word),
    };
    let mut parts = rest.split(':');
    let path = format!("{drive}{}", parts.next()?);
    let line: usize = parts.next()?.parse().ok()?;
    let col: usize = parts.next().and_then(|c| c.parse().ok()).unwrap_or(1);
    (!path.is_empty() && line > 0).then_some((path, line, col.max(1)))
}

fn normalize((a, b): (Cell, Cell)) -> (Cell, Cell) {
    if a <= b { (a, b) } else { (b, a) }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// The user's shell: $SHELL as a login shell on macOS/Linux, PowerShell on Windows.
fn default_shell() -> CommandBuilder {
    if cfg!(windows) {
        for p in ["pwsh.exe", "powershell.exe"] {
            if let Some(path) = crate::make::find_in_path(p) {
                let mut c = CommandBuilder::new(path);
                c.arg("-NoLogo");
                return c;
            }
        }
    }
    CommandBuilder::new_default_prog()
}

/// Name for a new shell tab: "zsh", "bash", "pwsh"...
pub fn shell_name() -> String {
    let s = if cfg!(windows) {
        if crate::make::find_in_path("pwsh.exe").is_some() { "pwsh".to_string() } else { "powershell".to_string() }
    } else {
        std::env::var("SHELL").ok().and_then(|s| s.rsplit('/').next().map(str::to_string)).unwrap_or_else(|| "shell".into())
    };
    if s.is_empty() { "shell".into() } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(key: Key, m: Modifiers, app: bool) -> Vec<u8> {
        let mut v = Vec::new();
        key_bytes(key, m, app, false, &mut v);
        v
    }

    #[test]
    fn keys() {
        assert_eq!(bytes(Key::C, Modifiers::CTRL, false), vec![3]);
        assert_eq!(bytes(Key::D, Modifiers::CTRL, false), vec![4]);
        assert_eq!(bytes(Key::ArrowUp, Modifiers::NONE, false), b"\x1b[A");
        assert_eq!(bytes(Key::ArrowUp, Modifiers::NONE, true), b"\x1bOA");
        assert_eq!(bytes(Key::ArrowLeft, Modifiers::CTRL, true), b"\x1b[1;5D");
        assert_eq!(bytes(Key::Delete, Modifiers::NONE, false), b"\x1b[3~");
        assert_eq!(bytes(Key::Enter, Modifiers::NONE, false), b"\r");
        assert_eq!(bytes(Key::Backspace, Modifiers::NONE, false), vec![0x7f]);
        assert_eq!(bytes(Key::Tab, Modifiers::SHIFT, false), b"\x1b[Z");
        assert_eq!(bytes(Key::B, Modifiers::ALT, false), vec![0x1b]);
        assert!(bytes(Key::B, Modifiers::NONE, false).is_empty());
    }

    #[test]
    fn links() {
        assert_eq!(parse_link("src/main.c:12:5:"), Some(("src/main.c".into(), 12, 5)));
        assert_eq!(parse_link("main.c:7"), Some(("main.c".into(), 7, 1)));
        assert_eq!(parse_link("C:\\x\\a.c:3:2"), Some(("C:\\x\\a.c".into(), 3, 2)));
        assert_eq!(parse_link("hello"), None);
        assert_eq!(parse_link("12:30"), Some(("12".into(), 30, 1)));
    }

    #[test]
    fn palette() {
        assert_eq!(indexed(16, true), Color32::from_rgb(0, 0, 0));
        assert_eq!(indexed(231, true), Color32::from_rgb(255, 255, 255));
        assert_eq!(indexed(232, true), Color32::from_rgb(8, 8, 8));
    }
}
