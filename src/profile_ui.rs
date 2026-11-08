//! Performance analyzer: profile the Run program, then show hot functions
//! and a flame graph in the bottom panel.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};

use egui::{Color32, Pos2, Rect, RichText, Sense, Ui, pos2, vec2};

use super::{App, BottomTab, Modal};
use crate::profile::{self, Node, Profile};

#[derive(Clone, Copy, PartialEq, Default)]
enum View {
    #[default]
    Functions,
    Flame,
}

enum Job {
    /// The program runs in the Run terminal tab (under perf, or watched by `sample`).
    Recording {
        out: PathBuf,
        terminal_pid: Option<u32>,
        sampler: Option<std::process::Child>,
        program_done_at: Option<f64>,
    },
    Analyzing(Receiver<Result<Profile, String>>),
}

#[derive(Default)]
pub(crate) struct ProfState {
    job: Option<Job>,
    profile: Option<Profile>,
    error: Option<String>,
    view: View,
    sort_by_total: bool,
    /// Path of child indices from the root to the zoomed-in node.
    zoom: Vec<usize>,
    /// Folder the program ran in, to resolve source paths.
    dir: PathBuf,
    files: Option<Vec<PathBuf>>,
    /// Share of samples per source line (0-based), per file, for the editor.
    heat: std::collections::HashMap<PathBuf, Vec<(usize, f32)>>,
    /// Functions whose source is in the project (shown in colour).
    user_funcs: std::collections::HashSet<String>,
}

impl App {
    pub(super) fn cmd_profile(&mut self, ctx: &egui::Context) {
        if cfg!(windows) {
            self.toast(ctx, "Profiling works on macOS (sample) and Linux (perf); Windows isn't supported yet", true);
            return;
        }
        if self.prof.job.is_some() {
            self.toast(ctx, "Already profiling. Stop the program to see the results", false);
            return;
        }
        if cfg!(target_os = "linux") && profile::find_perf().is_none() {
            self.toast(ctx, "Profiling needs perf: sudo apt install linux-tools-generic (or your distro's perf package)", true);
            return;
        }
        if self.project_root().is_none() {
            self.toast(ctx, "Open a project folder to profile", true);
            return;
        }
        if self.makefile.is_some() {
            let cfg = self.build_config();
            let target = if cfg.target == "run" { String::new() } else { cfg.target.clone() };
            self.cmd_build(ctx, Some(target));
            self.profile_after_build = true;
        } else {
            self.start_profiling(ctx);
        }
    }

    pub(super) fn start_profiling(&mut self, ctx: &egui::Context) {
        let Some((program, args, cwd)) = self.debug_target() else {
            self.toast(ctx, "Set the program to profile as the Run command in Build Settings", true);
            self.modal = Some(Modal::BuildSettings(self.build_config()));
            return;
        };
        let cache = self
            .project_root()
            .and_then(|r| crate::lsp::cache_dir_for(&r))
            .unwrap_or_else(std::env::temp_dir);
        let _ = std::fs::create_dir_all(&cache);
        let out = cache.join(if cfg!(target_os = "macos") { "profile.txt" } else { "perf.data" });
        let _ = std::fs::remove_file(&out);
        self.prof.error = None;
        self.prof.dir = cwd.clone();
        self.prof.files = None;
        let program_s = program.to_string_lossy().into_owned();
        if cfg!(target_os = "macos") {
            // Run the program, then attach `sample` to it.
            let Some(pid) = self.run_in_terminal_env(ctx, program_s, args, cwd, &[]) else { return };
            let sampler = std::process::Command::new("/usr/bin/sample")
                .args([pid.to_string().as_str(), "600", "1", "-mayDie", "-file"])
                .arg(&out)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            match sampler {
                Ok(child) => {
                    self.prof.job = Some(Job::Recording { out, terminal_pid: Some(pid), sampler: Some(child), program_done_at: None });
                }
                Err(e) => self.toast(ctx, format!("Couldn't start sample: {e}"), true),
            }
        } else {
            let Some(perf) = profile::find_perf() else { return };
            let mut perf_args: Vec<String> = ["record", "-F", "999", "-g", "-o"].iter().map(|s| s.to_string()).collect();
            perf_args.push(out.to_string_lossy().into_owned());
            perf_args.push("--".into());
            perf_args.push(program_s);
            perf_args.extend(args);
            let pid = self.run_in_terminal_env(ctx, perf.to_string_lossy().into_owned(), perf_args, cwd, &[]);
            if pid.is_some() {
                self.prof.job = Some(Job::Recording { out, terminal_pid: pid, sampler: None, program_done_at: None });
            }
        }
        if self.prof.job.is_some() {
            self.toast(ctx, "Profiling… the results appear when the program exits", false);
        }
    }

    /// Stop profiling: end the program; results follow.
    pub(super) fn stop_profiling(&mut self) {
        self.stop_run_terminal();
    }

    pub(super) fn update_profile(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        let (run_pid, run_running) = (self.run_terminal_pid(), self.run_terminal_running());
        let Some(job) = &mut self.prof.job else { return };
        match job {
            Job::Recording { out, terminal_pid, sampler, program_done_at } => {
                let program_running = run_pid == *terminal_pid && run_running;
                if !program_running && program_done_at.is_none() {
                    *program_done_at = Some(now);
                }
                let sampler_done = match sampler {
                    Some(c) => match c.try_wait() {
                        Ok(Some(_)) | Err(_) => true,
                        Ok(None) => {
                            // `sample` should stop when the program exits; don't wait forever.
                            if program_done_at.is_some_and(|t| now - t > 15.0) {
                                let _ = c.kill();
                                true
                            } else {
                                false
                            }
                        }
                    },
                    None => !program_running,
                };
                if !sampler_done {
                    ctx.request_repaint_after(std::time::Duration::from_millis(250));
                    return;
                }
                let out = out.clone();
                let dir = self.prof.dir.clone();
                let (tx, rx) = mpsc::channel();
                let ctx2 = ctx.clone();
                std::thread::spawn(move || {
                    let _ = tx.send(analyze(&out, &dir));
                    ctx2.request_repaint();
                });
                self.prof.job = Some(Job::Analyzing(rx));
            }
            Job::Analyzing(rx) => {
                if let Ok(result) = rx.try_recv() {
                    self.prof.job = None;
                    match result {
                        Ok(p) if p.samples > 0 => {
                            self.prof.heat = self.heat_map(&p);
                            self.prof.user_funcs = self.user_functions(&p);
                            self.prof.profile = Some(p);
                            self.prof.zoom.clear();
                            self.bottom_open = true;
                            self.bottom_tab = BottomTab::Profile;
                        }
                        Ok(_) => {
                            self.prof.error = Some("No samples were recorded. The program may have finished too quickly; profile something that runs for at least a second.".into());
                            self.bottom_open = true;
                            self.bottom_tab = BottomTab::Profile;
                        }
                        Err(e) => {
                            self.prof.error = Some(e);
                            self.bottom_open = true;
                            self.bottom_tab = BottomTab::Profile;
                        }
                    }
                }
            }
        }
    }

    fn heat_map(&mut self, p: &Profile) -> std::collections::HashMap<PathBuf, Vec<(usize, f32)>> {
        if self.prof.files.is_none() {
            self.prof.files = Some(self.project_root().map(|r| crate::project::list_files(&r)).unwrap_or_default());
        }
        let files = self.prof.files.as_deref().unwrap_or_default();
        let mut resolved: std::collections::HashMap<&str, Option<PathBuf>> = std::collections::HashMap::new();
        let mut out: std::collections::HashMap<PathBuf, Vec<(usize, f32)>> = std::collections::HashMap::new();
        let total = p.samples.max(1) as f32;
        for ((file, line), n) in &p.lines {
            let path = resolved.entry(file.as_str()).or_insert_with(|| profile::resolve_source(file, &self.prof.dir, files));
            if let Some(path) = path {
                out.entry(path.clone()).or_default().push((line.saturating_sub(1), *n as f32 / total));
            }
        }
        out
    }

    fn user_functions(&mut self, p: &Profile) -> std::collections::HashSet<String> {
        let files = self.prof.files.as_deref().unwrap_or_default();
        p.functions
            .iter()
            .filter(|f| f.file.as_ref().is_some_and(|file| profile::resolve_source(file, &self.prof.dir, files).is_some()))
            .map(|f| f.name.clone())
            .collect()
    }

    /// Hot lines to shade in the editor while the Profile tab is showing.
    pub(super) fn profile_heat(&self, path: Option<&std::path::Path>) -> Vec<(usize, f32)> {
        if !(self.bottom_open && self.bottom_tab == BottomTab::Profile) {
            return Vec::new();
        }
        path.and_then(|p| self.prof.heat.get(p)).cloned().unwrap_or_default()
    }

    fn open_profile_location(&mut self, file: &str, line: usize) {
        if self.prof.files.is_none() {
            self.prof.files = Some(self.project_root().map(|r| crate::project::list_files(&r)).unwrap_or_default());
        }
        let files = self.prof.files.as_deref().unwrap_or_default();
        let Some(path) = profile::resolve_source(file, &self.prof.dir, files) else {
            return;
        };
        self.open_file(&path);
        if let Some(doc) = self.docs.get_mut(self.active)
            && doc.path.as_deref() == Some(path.as_path())
        {
            doc.goto_line_col(line.saturating_sub(1), 0);
        }
    }

    // ----- panel -------------------------------------------------------------

    pub(super) fn profile_toolbar(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        match &self.prof.job {
            Some(Job::Recording { .. }) => {
                if ui.button(RichText::new("Stop").color(self.theme.error)).on_hover_text("End the program and show the profile").clicked() {
                    self.stop_profiling();
                }
                ui.spinner();
                ui.label(RichText::new("Recording…").weak());
            }
            Some(Job::Analyzing(_)) => {
                ui.spinner();
                ui.label(RichText::new("Reading profile…").weak());
            }
            None => {
                let sc = self.keymap.label(&ctx, crate::settings::Action::Profile);
                if ui.button("Start Profiling").on_hover_text(format!("Build, run and profile the program ({sc})")).clicked() {
                    self.cmd_profile(&ctx);
                }
            }
        }
        if let Some(p) = &self.prof.profile {
            ui.separator();
            ui.selectable_value(&mut self.prof.view, View::Functions, "Hot Functions");
            ui.selectable_value(&mut self.prof.view, View::Flame, "Flame Graph");
            ui.label(RichText::new(format!("{} samples", p.samples)).weak());
        }
    }

    pub(super) fn profile_panel(&mut self, ui: &mut Ui) {
        if let Some(e) = &self.prof.error {
            ui.label(RichText::new(e).color(self.theme.error));
        }
        if self.prof.profile.is_none() {
            if self.prof.error.is_none() {
                let how = if cfg!(target_os = "macos") {
                    "Profile builds your program, runs it and samples it with macOS's `sample` tool. When it exits you get its hottest functions and a flame graph."
                } else {
                    "Profile builds your program, runs it under `perf` and shows its hottest functions and a flame graph when it exits."
                };
                ui.label(RichText::new(how).weak());
                ui.label(RichText::new("Build with -g so functions map to source lines. The program runs in the Run terminal tab.").weak());
            }
            return;
        }
        match self.prof.view {
            View::Functions => self.functions_view(ui),
            View::Flame => self.flame_view(ui),
        }
    }

    fn functions_view(&mut self, ui: &mut Ui) {
        let Some(p) = &self.prof.profile else { return };
        let theme = self.theme.clone();
        let font = egui::FontId::monospace(12.5);
        let total = p.samples.max(1) as f64;
        let mut rows: Vec<&profile::Function> = p.functions.iter().collect();
        if self.prof.sort_by_total {
            rows.sort_by(|a, b| b.total.cmp(&a.total).then(b.self_count.cmp(&a.self_count)));
        }
        let mut open = None;
        let mut sort = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new("prof-funcs").striped(true).num_columns(4).spacing(vec2(18.0, 3.0)).show(ui, |ui| {
                let head = |ui: &mut Ui, text: &str, active: bool| {
                    let t = RichText::new(text).small().strong();
                    ui.add(egui::Button::selectable(active, t).frame_when_inactive(false)).clicked()
                };
                if head(ui, "SELF", !self.prof.sort_by_total) {
                    sort = Some(false);
                }
                if head(ui, "TOTAL", self.prof.sort_by_total) {
                    sort = Some(true);
                }
                ui.label(RichText::new("FUNCTION").small().strong());
                ui.label(RichText::new("WHERE").small().strong());
                ui.end_row();
                for f in rows.iter().take(400) {
                    let selfp = f.self_count as f64 / total * 100.0;
                    let totp = f.total as f64 / total * 100.0;
                    pct_cell(ui, selfp, &font, theme.error);
                    pct_cell(ui, totp, &font, theme.warning);
                    let user_code = self.prof.user_funcs.contains(&f.name);
                    let color = if user_code { theme.function } else { theme.comment };
                    let name = RichText::new(&f.name).font(font.clone()).color(color);
                    let r = ui.add(egui::Button::selectable(false, name).frame_when_inactive(false));
                    let loc = match (&f.file, f.line) {
                        (Some(file), Some(line)) => format!("{}:{line}", short_path(file)),
                        _ => String::new(),
                    };
                    let r2 = ui.add(egui::Button::selectable(false, RichText::new(&loc).font(font.clone()).color(theme.comment)).frame_when_inactive(false));
                    if (r.clicked() || r2.clicked())
                        && let (Some(file), Some(line)) = (&f.file, f.line)
                    {
                        open = Some((file.clone(), line));
                    }
                    ui.end_row();
                }
            });
        });
        if let Some(s) = sort {
            self.prof.sort_by_total = s;
        }
        if let Some((file, line)) = open {
            self.open_profile_location(&file, line);
        }
    }

    fn flame_view(&mut self, ui: &mut Ui) {
        let Some(p) = &self.prof.profile else { return };
        // Find the zoomed node; drop the zoom if the path is stale.
        let mut node = &p.root;
        let mut crumbs: Vec<(String, usize)> = vec![("all".into(), 0)];
        for (depth, &i) in self.prof.zoom.iter().enumerate() {
            match node.children.get(i) {
                Some(c) => {
                    node = c;
                    crumbs.push((c.name.clone(), depth + 1));
                }
                None => break,
            }
        }
        let mut set_zoom: Option<Vec<usize>> = None;
        ui.horizontal(|ui| {
            for (i, (name, depth)) in crumbs.iter().enumerate() {
                if i > 0 {
                    ui.label(RichText::new("›").weak());
                }
                if ui.add(egui::Button::selectable(i == crumbs.len() - 1, name.as_str()).frame_when_inactive(false)).clicked() {
                    set_zoom = Some(self.prof.zoom[..*depth].to_vec());
                }
            }
            ui.label(RichText::new("Click to zoom, double-click to open the source").weak().small());
        });
        let row_h = 18.0;
        let depth = max_depth(node, 0) + 1;
        let total_samples = p.samples.max(1);
        let mut clicked: Option<Vec<usize>> = None;
        let mut open: Option<(String, usize)> = None;
        let theme = self.theme.clone();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            let width = ui.available_width();
            let (rect, resp) = ui.allocate_exact_size(vec2(width, depth as f32 * row_h), Sense::click());
            let painter = ui.painter_at(rect);
            let hover = resp.hover_pos();
            let mut hovered: Option<(String, u64, Option<String>, Option<usize>)> = None;
            let base = self.prof.zoom.clone();
            let user_funcs = &self.prof.user_funcs;
            let mut draw = Draw { painter: &painter, row_h, rect, hover, hovered: &mut hovered, clicked: &mut clicked, open: &mut open, resp: &resp, theme: &theme, user_funcs };
            draw.node(node, rect.left(), width, 0, &base);
            if let Some((name, n, file, line)) = hovered {
                let pct = n as f64 / total_samples as f64 * 100.0;
                let loc = match (file, line) {
                    (Some(f), Some(l)) => format!("\n{}:{l}", short_path(&f)),
                    _ => String::new(),
                };
                let text = format!("{name}\n{n} samples ({pct:.1}%){loc}");
                egui::Tooltip::for_enabled(&resp).at_pointer().show(|ui| ui.label(text));
            }
        });
        if let Some(z) = set_zoom.or(clicked) {
            self.prof.zoom = z;
        }
        if let Some((file, line)) = open {
            self.open_profile_location(&file, line);
        }
    }
}

struct Draw<'a> {
    painter: &'a egui::Painter,
    row_h: f32,
    rect: Rect,
    hover: Option<Pos2>,
    hovered: &'a mut Option<(String, u64, Option<String>, Option<usize>)>,
    clicked: &'a mut Option<Vec<usize>>,
    open: &'a mut Option<(String, usize)>,
    resp: &'a egui::Response,
    theme: &'a crate::theme::Theme,
    user_funcs: &'a std::collections::HashSet<String>,
}

impl Draw<'_> {
    /// Icicle layout: callers on top, callees below, width = share of samples.
    fn node(&mut self, n: &Node, x: f32, w: f32, depth: usize, path: &[usize]) {
        if w < 1.0 {
            return;
        }
        let y = self.rect.top() + depth as f32 * self.row_h;
        let r = Rect::from_min_size(pos2(x, y), vec2(w, self.row_h - 1.0));
        let user = self.user_funcs.contains(&n.name);
        let fill = frame_color(&n.name, user, self.theme.dark);
        self.painter.rect_filled(r.shrink2(vec2(0.5, 0.0)), 2.0, fill);
        if w > 30.0 {
            let font = egui::FontId::proportional(12.0);
            let text = fit_text(self.painter, &n.name, &font, w - 6.0);
            self.painter.text(pos2(x + 3.0, y + self.row_h * 0.5 - 0.5), egui::Align2::LEFT_CENTER, text, font, Color32::from_rgb(20, 20, 20));
        }
        if let Some(h) = self.hover
            && r.contains(h)
        {
            *self.hovered = Some((n.name.clone(), n.total, n.file.clone(), n.line));
            self.painter.rect_stroke(r, 2.0, egui::Stroke::new(1.5, Color32::WHITE), egui::StrokeKind::Inside);
            if self.resp.double_clicked() {
                if let (Some(f), Some(l)) = (&n.file, n.line) {
                    *self.open = Some((f.clone(), l));
                }
            } else if self.resp.clicked() {
                *self.clicked = Some(path.to_vec());
            }
        }
        let mut cx = x;
        for (i, c) in n.children.iter().enumerate() {
            let cw = w * c.total as f32 / n.total.max(1) as f32;
            let mut p = path.to_vec();
            p.push(i);
            self.node(c, cx, cw, depth + 1, &p);
            cx += cw;
        }
    }
}

fn max_depth(n: &Node, d: usize) -> usize {
    n.children.iter().map(|c| max_depth(c, d + 1)).max().unwrap_or(d).min(200)
}

/// Warm colours for your code, cool greys for system libraries.
fn frame_color(name: &str, user: bool, dark: bool) -> Color32 {
    let h = crate::find::hash_text(name);
    let v = (h % 1000) as f32 / 1000.0;
    if user {
        // orange .. yellow
        let r = 230.0 + 20.0 * v;
        let g = 120.0 + 90.0 * v;
        let b = 50.0 + 30.0 * (1.0 - v);
        Color32::from_rgb(r as u8, g as u8, b as u8)
    } else if dark {
        let c = 150.0 + 40.0 * v;
        Color32::from_rgb((c * 0.85) as u8, (c * 0.9) as u8, c as u8)
    } else {
        let c = 185.0 + 40.0 * v;
        Color32::from_rgb((c * 0.85) as u8, (c * 0.9) as u8, c as u8)
    }
}

fn fit_text(painter: &egui::Painter, text: &str, font: &egui::FontId, max_w: f32) -> String {
    let width = |s: &str| painter.layout_no_wrap(s.to_string(), font.clone(), Color32::WHITE).size().x;
    if width(text) <= max_w {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut lo, mut hi) = (0usize, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let s: String = chars[..mid].iter().collect::<String>() + "…";
        if width(&s) <= max_w {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    if lo == 0 { String::new() } else { chars[..lo].iter().collect::<String>() + "…" }
}

fn pct_cell(ui: &mut Ui, pct: f64, font: &egui::FontId, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(vec2(90.0, 16.0), Sense::hover());
    let bar = Rect::from_min_size(rect.min + vec2(0.0, 3.0), vec2((rect.width() - 44.0) * (pct as f32 / 100.0).clamp(0.0, 1.0), 10.0));
    ui.painter().rect_filled(bar, 2.0, color.gamma_multiply(0.7));
    ui.painter().text(pos2(rect.right(), rect.center().y), egui::Align2::RIGHT_CENTER, format!("{pct:.1}%"), font.clone(), ui.visuals().text_color());
}

fn short_path(file: &str) -> String {
    let parts: Vec<&str> = file.split(['/', '\\']).collect();
    if parts.len() <= 2 { file.to_string() } else { parts[parts.len() - 2..].join("/") }
}

/// Turn the recorded file into a profile (runs on a worker thread).
fn analyze(out: &std::path::Path, dir: &std::path::Path) -> Result<Profile, String> {
    if !out.is_file() {
        return Err(if cfg!(target_os = "macos") {
            "sample didn't produce a report. The program may have exited before sampling started.".into()
        } else {
            "perf didn't record anything. If it reported a permission error, run: sudo sysctl kernel.perf_event_paranoid=1".into()
        });
    }
    let stacks = if cfg!(target_os = "macos") {
        let text = std::fs::read_to_string(out).map_err(|e| e.to_string())?;
        profile::parse_sample(&text)
    } else {
        let perf = profile::find_perf().ok_or("perf not found")?;
        let output = std::process::Command::new(perf)
            .args(["script", "-F", "comm,tid,ip,sym,srcline", "-i"])
            .arg(out)
            .current_dir(dir)
            .stderr(std::process::Stdio::null())
            .output()
            .map_err(|e| e.to_string())?;
        profile::parse_perf_script(&String::from_utf8_lossy(&output.stdout))
    };
    Ok(Profile::from_stacks(stacks))
}
