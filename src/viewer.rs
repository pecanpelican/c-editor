//! Read-only viewer tabs for images and PDFs.
//!
//! Decoding and rendering happen on a background thread so a large image or
//! a slow PDF page never stalls the editor. Nothing starts until the tab is
//! first shown, so restoring a session with many viewer tabs stays cheap.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Condvar, Mutex};

use egui::{
    Align2, Color32, ColorImage, FontId, Rect, RichText, ScrollArea, Sense, TextureHandle,
    TextureOptions, Ui, Vec2, pos2, vec2,
};

use crate::theme::Theme;

/// Largest texture side we upload. Most GPUs allow more, but this keeps
/// memory use sane for huge scans.
const MAX_SIDE: u32 = 8192;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ViewerKind {
    Image,
    Pdf,
}

impl ViewerKind {
    pub fn for_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "ico" | "tif" | "tiff" => {
                Some(ViewerKind::Image)
            }
            "pdf" => Some(ViewerKind::Pdf),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ViewerKind::Image => "Image",
            ViewerKind::Pdf => "PDF",
        }
    }
}

pub enum Viewer {
    Image(ImageView),
    Pdf(PdfView),
}

impl Viewer {
    pub fn new(kind: ViewerKind, path: &Path) -> Self {
        match kind {
            ViewerKind::Image => Viewer::Image(ImageView::new(path)),
            ViewerKind::Pdf => Viewer::Pdf(PdfView::new(path)),
        }
    }

    pub fn kind(&self) -> ViewerKind {
        match self {
            Viewer::Image(_) => ViewerKind::Image,
            Viewer::Pdf(_) => ViewerKind::Pdf,
        }
    }

    pub fn show(&mut self, ui: &mut Ui, theme: &Theme) {
        match self {
            Viewer::Image(v) => v.show(ui, theme),
            Viewer::Pdf(v) => v.show(ui, theme),
        }
    }

    /// Short description for the status bar, e.g. "640 × 480" or "Page 2 of 9".
    pub fn status(&self) -> String {
        match self {
            Viewer::Image(v) => v.status(),
            Viewer::Pdf(v) => v.status(),
        }
    }
}

fn toolbar_frame(theme: &Theme) -> egui::Frame {
    egui::Frame::NONE
        .fill(theme.panel_bg)
        .inner_margin(egui::Margin::symmetric(8, 4))
}

fn centered_message(ui: &mut Ui, text: &str, color: Color32) {
    let rect = ui.available_rect_before_wrap();
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, text, FontId::proportional(14.0), color);
    ui.allocate_rect(rect, Sense::hover());
}

// ---------------------------------------------------------------- images

enum ImageState {
    NotStarted,
    Loading(Receiver<Result<(ColorImage, [u32; 2]), String>>),
    Ready { tex: TextureHandle, size: [usize; 2], orig: [u32; 2] },
    Failed(String),
}

pub struct ImageView {
    path: PathBuf,
    state: ImageState,
    /// None: fit to the window (never upscaling past 100%).
    zoom: Option<f32>,
    /// Offset of the image centre from the view centre, in points.
    pan: Vec2,
    last_zoom: f32,
}

fn decode_image(path: &Path) -> Result<(ColorImage, [u32; 2]), String> {
    let img = image::ImageReader::open(path)
        .map_err(|e| e.to_string())?
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .decode()
        .map_err(|e| e.to_string())?;
    let orig = [img.width(), img.height()];
    let img = if img.width() > MAX_SIDE || img.height() > MAX_SIDE {
        img.thumbnail(MAX_SIDE, MAX_SIDE)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok((ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()), orig))
}

impl ImageView {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            state: ImageState::NotStarted,
            zoom: None,
            pan: Vec2::ZERO,
            last_zoom: 1.0,
        }
    }

    fn status(&self) -> String {
        match &self.state {
            ImageState::Ready { orig, .. } => {
                format!("{} × {}  ·  {:.0}%", orig[0], orig[1], self.last_zoom * 100.0)
            }
            ImageState::Failed(_) => "Can't decode".into(),
            _ => "Loading…".into(),
        }
    }

    fn poll(&mut self, ctx: &egui::Context) {
        if matches!(self.state, ImageState::NotStarted) {
            let (tx, rx) = channel();
            let path = self.path.clone();
            let ctx2 = ctx.clone();
            std::thread::spawn(move || {
                let _ = tx.send(decode_image(&path));
                ctx2.request_repaint();
            });
            self.state = ImageState::Loading(rx);
        }
        if let ImageState::Loading(rx) = &self.state {
            match rx.try_recv() {
                Ok(Ok((img, orig))) => {
                    let size = img.size;
                    let tex = ctx.load_texture(
                        format!("cedit-image:{}", self.path.display()),
                        img,
                        TextureOptions::LINEAR,
                    );
                    self.state = ImageState::Ready { tex, size, orig };
                }
                Ok(Err(e)) => self.state = ImageState::Failed(e),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.state = ImageState::Failed("decoder stopped".into())
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
    }

    pub fn show(&mut self, ui: &mut Ui, theme: &Theme) {
        self.poll(ui.ctx());
        let (tex, size, orig) = match &self.state {
            ImageState::Ready { tex, size, orig } => (tex.clone(), *size, *orig),
            ImageState::Failed(e) => {
                centered_message(ui, &format!("Can't show this image: {e}"), theme.error);
                return;
            }
            _ => {
                centered_message(ui, "Loading…", theme.gutter_fg);
                return;
            }
        };
        // Texture pixels per original image pixel (below 1 for downscaled huge images).
        let tex_scale = size[0] as f32 / orig[0].max(1) as f32;
        let img_pts = vec2(orig[0] as f32, orig[1] as f32) / ui.ctx().pixels_per_point();

        toolbar_frame(theme).show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.selectable_label(self.zoom.is_none(), "Fit").clicked() {
                    self.zoom = None;
                    self.pan = Vec2::ZERO;
                }
                if ui.selectable_label(self.zoom == Some(1.0), "100%").clicked() {
                    self.zoom = Some(1.0);
                    self.pan = Vec2::ZERO;
                }
                if ui.button("−").on_hover_text("Zoom out").clicked() {
                    self.zoom = Some((self.last_zoom / 1.25).max(0.02));
                }
                if ui.button("+").on_hover_text("Zoom in").clicked() {
                    self.zoom = Some((self.last_zoom * 1.25).min(64.0));
                }
                ui.label(RichText::new(format!(
                    "{} × {} px  ·  {:.0}%",
                    orig[0],
                    orig[1],
                    self.last_zoom * 100.0
                ))
                .weak());
                if tex_scale < 1.0 {
                    ui.label(RichText::new("(shown downscaled)").weak());
                }
            });
        });

        let (rect, resp) = ui.allocate_exact_size(ui.available_size(), Sense::click_and_drag());
        let fit = (rect.width() / img_pts.x).min(rect.height() / img_pts.y).min(1.0);
        let mut zoom = self.zoom.unwrap_or(fit);

        // Scroll or pinch to zoom around the pointer; drag to pan.
        if let Some(hover) = resp.hover_pos() {
            let (scroll, pinch) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
            let factor = pinch * (scroll / 200.0).exp();
            if (factor - 1.0).abs() > 1e-4 {
                let new_zoom = (zoom * factor).clamp(0.02, 64.0);
                // Keep the image point under the pointer fixed.
                let centre = rect.center() + self.pan;
                let rel = (hover - centre) / zoom;
                self.pan = hover - rel * new_zoom - rect.center();
                zoom = new_zoom;
                self.zoom = Some(zoom);
            }
        }
        if resp.dragged() {
            self.pan += resp.drag_delta();
            if self.zoom.is_none() {
                self.zoom = Some(zoom);
            }
        }
        if resp.double_clicked() {
            self.zoom = if self.zoom.is_none() { Some(1.0) } else { None };
            self.pan = Vec2::ZERO;
        }
        if self.zoom.is_none() {
            self.pan = Vec2::ZERO;
        }
        self.last_zoom = zoom;

        let shown = img_pts * zoom;
        let img_rect = Rect::from_center_size(rect.center() + self.pan, shown);
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, theme.bg);
        // Checkerboard behind the image so transparency is visible.
        let vis = img_rect.intersect(rect);
        if vis.is_positive() {
            let light = Color32::from_gray(0xcc);
            let dark = Color32::from_gray(0x99);
            painter.rect_filled(vis, 0.0, light);
            let cell = 10.0;
            let x0 = ((vis.min.x - img_rect.min.x) / cell).floor() as i64;
            let y0 = ((vis.min.y - img_rect.min.y) / cell).floor() as i64;
            let x1 = ((vis.max.x - img_rect.min.x) / cell).ceil() as i64;
            let y1 = ((vis.max.y - img_rect.min.y) / cell).ceil() as i64;
            for y in y0..y1 {
                for x in x0..x1 {
                    if (x + y) % 2 == 0 {
                        continue;
                    }
                    let min = img_rect.min + vec2(x as f32 * cell, y as f32 * cell);
                    let r = Rect::from_min_size(min, Vec2::splat(cell)).intersect(vis);
                    painter.rect_filled(r, 0.0, dark);
                }
            }
        }
        painter.image(tex.id(), img_rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
        if resp.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        }
    }
}

// ---------------------------------------------------------------- PDFs

enum PdfMsg {
    Opened(Vec<(f32, f32)>),
    Failed(String),
    Page { index: usize, scale: f32, image: ColorImage },
}

#[derive(Default)]
struct PdfQueue {
    /// Pages (and render scales) the view wants, most important first.
    wanted: Vec<(usize, f32)>,
    /// Page the worker is rendering right now.
    busy: Option<usize>,
    quit: bool,
}

struct PdfShared {
    queue: Mutex<PdfQueue>,
    cv: Condvar,
}

struct PdfWorker {
    shared: Arc<PdfShared>,
    rx: Receiver<PdfMsg>,
}

impl Drop for PdfWorker {
    fn drop(&mut self) {
        if let Ok(mut q) = self.shared.queue.lock() {
            q.quit = true;
        }
        self.shared.cv.notify_all();
    }
}

fn pdf_thread(path: PathBuf, shared: Arc<PdfShared>, tx: Sender<PdfMsg>, ctx: egui::Context) {
    use hayro::hayro_interpret::InterpreterSettings;
    use hayro::hayro_syntax::Pdf;
    use hayro::vello_cpu::color::palette::css::WHITE;
    use hayro::{PixmapSettings, RenderCache, RenderSettings, render};

    let send = |m: PdfMsg| {
        let _ = tx.send(m);
        ctx.request_repaint();
    };
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) => return send(PdfMsg::Failed(e.to_string())),
    };
    let pdf = match Pdf::new(data) {
        Ok(p) => p,
        Err(hayro::hayro_syntax::LoadPdfError::Decryption(_)) => {
            return send(PdfMsg::Failed("the PDF is encrypted".into()));
        }
        Err(_) => return send(PdfMsg::Failed("not a valid PDF".into())),
    };
    let pages = pdf.pages();
    send(PdfMsg::Opened(pages.iter().map(|p| p.render_dimensions()).collect()));
    let cache = RenderCache::new();
    let interp = InterpreterSettings::default();
    let settings = RenderSettings::default();
    loop {
        let (index, scale) = {
            let mut q = shared.queue.lock().unwrap();
            q.busy = None;
            while q.wanted.is_empty() && !q.quit {
                q = shared.cv.wait(q).unwrap();
            }
            if q.quit {
                return;
            }
            let job = q.wanted.remove(0);
            q.busy = Some(job.0);
            job
        };
        let Some(page) = pages.get(index) else { continue };
        let px = PixmapSettings { x_scale: scale, y_scale: scale, bg_color: WHITE };
        // A malformed page must not take the whole viewer down.
        let pixmap = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            render(page, &cache, &interp, &settings, &px)
        }));
        let image = match pixmap {
            Ok(p) => ColorImage::from_rgba_premultiplied(
                [p.width() as usize, p.height() as usize],
                p.data_as_u8_slice(),
            ),
            // Show a blank page rather than retrying forever.
            Err(_) => ColorImage::new([1, 1], vec![Color32::WHITE]),
        };
        send(PdfMsg::Page { index, scale, image });
    }
}

pub struct PdfView {
    path: PathBuf,
    worker: Option<PdfWorker>,
    pages: Vec<(f32, f32)>,
    error: Option<String>,
    /// None: fit page width to the window.
    zoom: Option<f32>,
    last_zoom: f32,
    textures: HashMap<usize, (f32, TextureHandle)>,
    current: usize,
    goto: Option<usize>,
    page_input: String,
}

const PAGE_GAP: f32 = 12.0;
const PAGE_MARGIN: f32 = 16.0;

impl PdfView {
    fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
            worker: None,
            pages: Vec::new(),
            error: None,
            zoom: None,
            last_zoom: 1.0,
            textures: HashMap::new(),
            current: 0,
            goto: None,
            page_input: String::new(),
        }
    }

    fn status(&self) -> String {
        if self.error.is_some() {
            return "Can't open".into();
        }
        if self.pages.is_empty() {
            return "Loading…".into();
        }
        format!("Page {} of {}  ·  {:.0}%", self.current + 1, self.pages.len(), self.last_zoom * 100.0)
    }

    fn poll(&mut self, ctx: &egui::Context) {
        if self.worker.is_none() && self.error.is_none() {
            let shared = Arc::new(PdfShared { queue: Mutex::new(PdfQueue::default()), cv: Condvar::new() });
            let (tx, rx) = channel();
            let (path, s2, ctx2) = (self.path.clone(), shared.clone(), ctx.clone());
            std::thread::spawn(move || pdf_thread(path, s2, tx, ctx2));
            self.worker = Some(PdfWorker { shared, rx });
        }
        let Some(w) = &self.worker else { return };
        let mut stopped = false;
        loop {
            match w.rx.try_recv() {
                Ok(PdfMsg::Opened(p)) => self.pages = p,
                Ok(PdfMsg::Failed(e)) => self.error = Some(e),
                Ok(PdfMsg::Page { index, scale, image }) => {
                    let tex = ctx.load_texture(
                        format!("cedit-pdf:{}:{index}", self.path.display()),
                        image,
                        TextureOptions::LINEAR,
                    );
                    self.textures.insert(index, (scale, tex));
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    stopped = true;
                    break;
                }
            }
        }
        if stopped && self.error.is_none() && self.pages.is_empty() {
            self.error = Some("the PDF reader stopped".into());
        }
    }

    pub fn show(&mut self, ui: &mut Ui, theme: &Theme) {
        self.poll(ui.ctx());
        if let Some(e) = &self.error {
            centered_message(ui, &format!("Can't show this PDF: {e}"), theme.error);
            return;
        }
        if self.pages.is_empty() {
            centered_message(ui, "Loading…", theme.gutter_fg);
            return;
        }
        let n = self.pages.len();

        toolbar_frame(theme).show(ui, |ui| {
            ui.horizontal(|ui| {
                if ui.add_enabled(self.current > 0, egui::Button::new("◀")).on_hover_text("Previous page").clicked() {
                    self.goto = Some(self.current - 1);
                }
                let r = ui.add(egui::TextEdit::singleline(&mut self.page_input).desired_width(36.0).align(egui::Align2::CENTER_CENTER));
                if !r.has_focus() {
                    self.page_input = (self.current + 1).to_string();
                }
                if r.lost_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter))
                    && let Ok(p) = self.page_input.trim().parse::<usize>()
                {
                    self.goto = Some(p.clamp(1, n) - 1);
                }
                ui.label(format!("of {n}"));
                if ui.add_enabled(self.current + 1 < n, egui::Button::new("▶")).on_hover_text("Next page").clicked() {
                    self.goto = Some(self.current + 1);
                }
                ui.separator();
                if ui.selectable_label(self.zoom.is_none(), "Fit Width").clicked() {
                    self.zoom = None;
                }
                if ui.button("−").on_hover_text("Zoom out").clicked() {
                    self.zoom = Some((self.last_zoom / 1.2).max(0.1));
                }
                if ui.button("+").on_hover_text("Zoom in").clicked() {
                    self.zoom = Some((self.last_zoom * 1.2).min(8.0));
                }
                ui.label(RichText::new(format!("{:.0}%", self.last_zoom * 100.0)).weak());
            });
        });

        // Ctrl/Cmd + scroll (or pinch) zooms.
        let pinch = ui.input(|i| i.zoom_delta());
        if (pinch - 1.0).abs() > 1e-4 && ui.rect_contains_pointer(ui.available_rect_before_wrap()) {
            self.zoom = Some((self.last_zoom * pinch).clamp(0.1, 8.0));
        }

        let avail_w = ui.available_width();
        let max_w = self.pages.iter().map(|p| p.0).fold(1.0f32, f32::max);
        let zoom = self.zoom.unwrap_or(((avail_w - 2.0 * PAGE_MARGIN) / max_w).max(0.1));
        self.last_zoom = zoom;
        let ppp = ui.ctx().pixels_per_point();

        // Page tops in points.
        let mut tops = Vec::with_capacity(n);
        let mut y = PAGE_MARGIN;
        for p in &self.pages {
            tops.push(y);
            y += p.1 * zoom + PAGE_GAP;
        }
        let total = vec2(max_w * zoom + 2.0 * PAGE_MARGIN, y - PAGE_GAP + PAGE_MARGIN);

        let mut area = ScrollArea::both().id_salt(("pdf", &self.path)).auto_shrink([false, false]);
        if let Some(g) = self.goto.take() {
            area = area.vertical_scroll_offset(tops[g] - PAGE_MARGIN);
        }
        let bg = theme.bg;
        let dim = theme.gutter_fg;
        let mut visible: Vec<usize> = Vec::new();
        let mut current = self.current;
        area.show_viewport(ui, |ui, viewport| {
            ui.painter().rect_filled(ui.max_rect(), 0.0, bg);
            let content_w = total.x.max(viewport.width());
            ui.set_min_size(vec2(content_w, total.y));
            let origin = ui.min_rect().min;
            let mid = viewport.center().y;
            for (i, p) in self.pages.iter().enumerate() {
                let size = vec2(p.0, p.1) * zoom;
                let x = (content_w - size.x) / 2.0;
                let r = Rect::from_min_size(pos2(x, tops[i]), size);
                if r.min.y <= mid {
                    current = i;
                }
                if !r.intersects(viewport) {
                    continue;
                }
                visible.push(i);
                let screen = r.translate(origin.to_vec2());
                let painter = ui.painter();
                painter.rect_filled(screen.expand(1.0), 0.0, Color32::from_black_alpha(60));
                painter.rect_filled(screen, 0.0, Color32::WHITE);
                if let Some((_, tex)) = self.textures.get(&i) {
                    painter.image(tex.id(), screen, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                } else {
                    painter.text(screen.center(), Align2::CENTER_CENTER, "Rendering…", FontId::proportional(13.0), dim);
                }
            }
        });
        self.current = current;

        // Ask the worker for pages that are missing or rendered at the wrong size.
        let Some(w) = &self.worker else { return };
        let target = |i: usize| -> f32 {
            let (pw, ph) = self.pages[i];
            let mut s = zoom * ppp;
            let cap = (MAX_SIDE as f32 / pw.max(1.0)).min(MAX_SIDE as f32 / ph.max(1.0));
            s = s.min(cap);
            s
        };
        let mut wanted: Vec<(usize, f32)> = Vec::new();
        let first = visible.first().copied().unwrap_or(self.current);
        let last = visible.last().copied().unwrap_or(self.current);
        // Visible pages first, then one page either side so paging feels instant.
        let around = [last + 1, first.wrapping_sub(1)];
        for i in visible.iter().copied().chain(around.into_iter().filter(|&i| i < n)) {
            let s = target(i);
            let ok = self.textures.get(&i).is_some_and(|(have, _)| (have / s - 1.0).abs() < 0.15);
            if !ok {
                wanted.push((i, s));
            }
        }
        if let Ok(mut q) = w.shared.queue.lock() {
            let busy = q.busy;
            wanted.retain(|(i, _)| Some(*i) != busy);
            if q.wanted != wanted {
                q.wanted = wanted;
                w.shared.cv.notify_all();
            }
        }
        // Free textures far from the view.
        let lo = first.saturating_sub(6);
        let hi = last + 6;
        self.textures.retain(|i, _| *i >= lo && *i <= hi);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Run frames of the viewer headlessly until `done` or a timeout.
    fn run_until(v: &mut Viewer, done: impl Fn(&Viewer) -> bool) {
        let ctx = egui::Context::default();
        let theme = Theme::dark();
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(20) {
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(900.0, 700.0))),
                ..Default::default()
            };
            let mut out = ctx.run_ui(input, |ui| {
                egui::CentralPanel::default().show(ui, |ui| v.show(ui, &theme));
            });
            out.textures_delta.clear();
            if done(v) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("viewer never finished: {}", v.status());
    }

    #[test]
    fn kinds_by_extension() {
        assert_eq!(ViewerKind::for_path(Path::new("a/b.PNG")), Some(ViewerKind::Image));
        assert_eq!(ViewerKind::for_path(Path::new("x.jpeg")), Some(ViewerKind::Image));
        assert_eq!(ViewerKind::for_path(Path::new("doc.pdf")), Some(ViewerKind::Pdf));
        assert_eq!(ViewerKind::for_path(Path::new("main.c")), None);
        assert_eq!(ViewerKind::for_path(Path::new("README.md")), None);
    }

    #[test]
    fn image_loads_and_reports_size() {
        let dir = std::env::temp_dir().join(format!("cedit-img-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.png");
        image::RgbaImage::from_pixel(37, 21, image::Rgba([10, 200, 30, 128])).save(&path).unwrap();
        let mut v = Viewer::new(ViewerKind::Image, &path);
        run_until(&mut v, |v| matches!(v, Viewer::Image(i) if matches!(i.state, ImageState::Ready { .. })));
        assert!(v.status().starts_with("37 × 21"), "{}", v.status());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn broken_image_shows_error() {
        let path = std::env::temp_dir().join(format!("cedit-bad-{}.png", std::process::id()));
        std::fs::write(&path, b"not a png").unwrap();
        let mut v = Viewer::new(ViewerKind::Image, &path);
        run_until(&mut v, |v| matches!(v, Viewer::Image(i) if matches!(i.state, ImageState::Failed(_))));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn pdf_opens_and_renders_first_page() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/sample.pdf");
        let mut v = Viewer::new(ViewerKind::Pdf, &path);
        run_until(&mut v, |v| matches!(v, Viewer::Pdf(p) if p.textures.contains_key(&0)));
        let Viewer::Pdf(p) = &v else { unreachable!() };
        assert_eq!(p.pages.len(), 2);
        assert!(v.status().starts_with("Page 1 of 2"), "{}", v.status());
    }

    #[test]
    fn non_pdf_shows_error() {
        let path = std::env::temp_dir().join(format!("cedit-bad-{}.pdf", std::process::id()));
        std::fs::write(&path, b"hello").unwrap();
        let mut v = Viewer::new(ViewerKind::Pdf, &path);
        run_until(&mut v, |v| matches!(v, Viewer::Pdf(p) if p.error.is_some()));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn markdown_preview_renders() {
        let ctx = egui::Context::default();
        egui_extras::install_image_loaders(&ctx);
        let mut cache = egui_commonmark::CommonMarkCache::default();
        let md = "# Title\n\n**bold** `code`\n\n- [x] task\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n```c\nint x;\n```\n";
        for _ in 0..3 {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                egui_commonmark::CommonMarkViewer::new()
                    .default_implicit_uri_scheme("file:///tmp/")
                    .show(ui, &mut cache, md);
            });
            out.textures_delta.clear();
        }
    }
}
