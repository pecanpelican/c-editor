//! Debugger: breakpoints, the debug session (over DAP) and the Debug panel.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use egui::{Key, RichText, Ui};
use serde_json::{Value, json};

use super::{App, BottomTab, Modal};
use serde::{Deserialize, Serialize};

/// A breakpoint's condition and log message, saved with the session.
#[derive(Clone, Serialize, Deserialize, Default)]
pub(crate) struct BreakpointSetting {
    path: PathBuf,
    line: usize,
    #[serde(default)]
    condition: String,
    #[serde(default)]
    log: String,
}
use crate::dap;
use crate::make;

#[derive(Clone, Copy, PartialEq)]
enum Status {
    Starting,
    Running,
    Stopped,
    Exited,
}

#[derive(Clone)]
struct Frame {
    id: i64,
    name: String,
    path: Option<PathBuf>,
    line: usize, // 0-based
}

#[derive(Clone)]
struct Scope {
    name: String,
    reference: i64,
    expensive: bool,
}

#[derive(Clone)]
struct Var {
    name: String,
    value: String,
    ty: String,
    reference: i64,
    /// Expression that gives this variable (e.g. `p->pos.x`), for watches and memory.
    evaluate_name: String,
}

#[derive(Clone)]
struct WatchValue {
    value: Result<String, String>,
    reference: i64,
}

/// The Memory view: a hex dump of the bytes at an address or expression.
#[derive(Default)]
struct MemView {
    expr: String,
    /// The DAP memory reference the expression resolved to.
    reference: Option<String>,
    offset: i64,
    address: Option<u64>,
    data: Vec<u8>,
    unreadable: usize,
    /// Bytes from the previous stop, to highlight what changed.
    prev: Option<(u64, Vec<u8>)>,
    error: Option<String>,
}

const MEM_BYTES: usize = 256;

#[derive(Default, PartialEq, Clone, Copy)]
enum RightTab {
    #[default]
    Console,
    Memory,
}

/// Right-click actions on a variable.
enum VarAction {
    Watch(String),
    Array(String),
    Memory(String),
    Copy(String),
}

#[derive(Clone, PartialEq)]
enum ConsoleKind {
    Out,
    Err,
    Info,
    Input,
}

/// Actions from the breakpoint list in the Debug panel.
enum BpMenu {
    Edit(PathBuf, usize),
    Remove(PathBuf, usize),
    RemoveAll,
    Mute(bool),
}

enum Req {
    Initialize,
    Launch,
    SetBreakpoints(PathBuf),
    StackTrace,
    Scopes,
    Variables(i64),
    Evaluate(EvalKind),
    ReadMemory(u64),
    Other,
}

enum EvalKind {
    Hover(u64),
    Watch(String),
    /// `ptr, N` watch: first find the pointer's type, then show N elements.
    WatchType(String, String, usize),
    Repl,
    /// Memory view address; true once `&(expr)` was tried.
    Memory(u64, bool),
    /// Memory read with a debugger command (no `readMemory` support), from this address.
    MemoryDump(u64, u64),
}

struct Session {
    client: dap::Client,
    adapter_name: &'static str,
    launch: Value,
    pending: HashMap<i64, Req>,
    status: Status,
    stop_reason: String,
    thread: Option<i64>,
    frames: Vec<Frame>,
    frame: usize,
    scopes: Vec<Scope>,
    vars: HashMap<i64, Vec<Var>>,
    requested: HashSet<i64>,
    watch_values: HashMap<String, WatchValue>,
    /// Name path of each variables reference ("main/Locals/p"), and the values
    /// from the previous stop by path, to highlight what changed.
    paths: HashMap<i64, String>,
    prev: HashMap<String, String>,
    exit_code: Option<i64>,
    /// When we asked the adapter to stop; force-kill if it doesn't.
    disconnect_at: Option<f64>,
    /// Stop generation, so late answers from an earlier stop are ignored.
    stop_gen: u64,
    req_gen: HashMap<i64, u64>,
    /// Breakpoint requests to finish before sending configurationDone
    /// (gdb starts the program as soon as it gets configurationDone).
    config_wait: Option<HashSet<i64>>,
    /// gdb runs the program as soon as it gets `launch`, so it's sent after
    /// the breakpoints. lldb-dap wants `launch` first.
    launch_after_breakpoints: bool,
    /// The adapter supports `readMemory` (older lldb-dap doesn't, and crashes on it).
    read_memory: bool,
}

struct DebugHover {
    doc_id: u64,
    expr: String,
    pos: egui::Pos2,
    since: f64,
    sent: bool,
    id: u64,
    text: Option<String>,
}

#[derive(Default)]
pub(crate) struct DebugState {
    /// Breakpoint lines (0-based) per file.
    pub breakpoints: BTreeMap<PathBuf, Vec<usize>>,
    /// Whether the debugger accepted each breakpoint: (file, line) -> verified.
    verified: HashMap<(PathBuf, usize), bool>,
    session: Option<Session>,
    pub watches: Vec<String>,
    new_watch: String,
    console: Vec<(ConsoleKind, String)>,
    console_input: String,
    hover: Option<DebugHover>,
    hover_counter: u64,
    /// Line counts per open document, to move breakpoints when lines are added or removed.
    line_counts: HashMap<u64, (usize, usize)>,
    /// Where the program is paused: file, line, top frame?
    pub exec: Option<(PathBuf, usize, bool)>,
    /// Show numbers in hex.
    hex: bool,
    right_tab: RightTab,
    mem_input: String,
    mem: Option<MemView>,
    mem_gen: u64,
    /// Conditions and log messages: file -> line -> (condition, log message).
    bp_extra: BTreeMap<PathBuf, BTreeMap<usize, (String, String)>>,
    /// Breakpoints are kept but not sent to the debugger.
    pub muted: bool,
    /// Run to Cursor: a temporary breakpoint, removed at the next stop.
    run_to: Option<(PathBuf, usize)>,
}

impl DebugState {
    pub fn new(breakpoints: BTreeMap<PathBuf, Vec<usize>>, watches: Vec<String>) -> Self {
        Self { breakpoints, watches, ..Default::default() }
    }

    pub fn load_breakpoint_settings(&mut self, list: &[BreakpointSetting], muted: bool) {
        self.muted = muted;
        for b in list {
            if self.breakpoints.get(&b.path).is_some_and(|l| l.contains(&b.line)) {
                self.bp_extra.entry(b.path.clone()).or_default().insert(b.line, (b.condition.clone(), b.log.clone()));
            }
        }
    }

    pub fn breakpoint_settings(&self) -> Vec<BreakpointSetting> {
        let mut out = Vec::new();
        for (path, lines) in &self.bp_extra {
            for (line, (condition, log)) in lines {
                if !condition.is_empty() || !log.is_empty() {
                    out.push(BreakpointSetting { path: path.clone(), line: *line, condition: condition.clone(), log: log.clone() });
                }
            }
        }
        out
    }

    /// The DAP `breakpoints` list for one file.
    fn bp_json(&self, path: &Path) -> Vec<Value> {
        let mut out = Vec::new();
        if !self.muted {
            for &l in self.breakpoints.get(path).map(Vec::as_slice).unwrap_or_default() {
                let mut b = json!({"line": l + 1});
                if let Some((cond, log)) = self.bp_extra.get(path).and_then(|m| m.get(&l)) {
                    if !cond.trim().is_empty() {
                        b["condition"] = json!(cond.trim());
                    }
                    if !log.trim().is_empty() {
                        // Log points print `{expr}` values instead of stopping.
                        b["logMessage"] = json!(format!("{}\n", log.trim()));
                    }
                }
                out.push(b);
            }
        }
        if let Some((p, l)) = &self.run_to
            && p == path
            && !(out.iter().any(|b| b["line"] == json!(l + 1)))
        {
            out.push(json!({"line": l + 1}));
        }
        out
    }

    /// Lines in `bp_json(path)` order, to match the adapter's answer.
    fn bp_lines(&self, path: &Path) -> Vec<usize> {
        self.bp_json(path).iter().filter_map(|b| b["line"].as_u64()).map(|l| l as usize - 1).collect()
    }
}

impl App {
    // ----- breakpoints -----------------------------------------------------

    pub(super) fn debug_marks(&self, path: Option<&Path>) -> crate::editor::DebugMarks {
        let mut m = crate::editor::DebugMarks::default();
        let Some(path) = path else { return m };
        let active = self.dbg.session.is_some();
        if let Some(lines) = self.dbg.breakpoints.get(path) {
            m.breakpoints = lines
                .iter()
                .map(|&l| {
                    let ok = !active || self.dbg.verified.get(&(path.to_path_buf(), l)).copied().unwrap_or(false);
                    (l, ok)
                })
                .collect();
        }
        if let Some((p, line, top)) = &self.dbg.exec
            && p == path
        {
            m.exec_line = Some(*line);
            m.exec_top = *top;
        }
        m.inline = self.inline_values(path);
        m
    }

    pub(super) fn toggle_breakpoint(&mut self, path: &Path, line: usize) {
        let lines = self.dbg.breakpoints.entry(path.to_path_buf()).or_default();
        if let Some(i) = lines.iter().position(|&l| l == line) {
            lines.remove(i);
            if let Some(m) = self.dbg.bp_extra.get_mut(path) {
                m.remove(&line);
            }
        } else {
            lines.push(line);
            lines.sort_unstable();
        }
        if lines.is_empty() {
            self.dbg.breakpoints.remove(path);
        }
        self.send_breakpoints(path);
    }

    pub(super) fn cmd_toggle_breakpoint(&mut self) {
        let Some(doc) = self.docs.get(self.active) else { return };
        let Some(path) = doc.path.clone() else { return };
        let line = doc.cursor_line_col.0;
        self.toggle_breakpoint(&path, line);
    }

    fn send_breakpoints(&mut self, path: &Path) {
        let bps = self.dbg.bp_json(path);
        let Some(s) = &mut self.dbg.session else { return };
        if s.status == Status::Exited {
            return;
        }
        let seq = s.client.request("setBreakpoints", json!({"source": {"path": path}, "breakpoints": bps}));
        s.pending.insert(seq, Req::SetBreakpoints(path.to_path_buf()));
    }

    /// Keep breakpoints on their lines when lines are inserted or deleted above them.
    pub(super) fn track_breakpoint_lines(&mut self) {
        let Some(doc) = self.docs.get(self.active) else { return };
        let Some(path) = doc.path.clone() else { return };
        let count = doc.line_count();
        let cursor = doc.cursor_line_col.0;
        let prev = self.dbg.line_counts.insert(doc.id, (count, cursor));
        let Some((old_count, old_cursor)) = prev else { return };
        if old_count == count || !self.dbg.breakpoints.contains_key(&path) {
            return;
        }
        let delta = count as isize - old_count as isize;
        // The edit happened around the cursor: lines below it moved.
        let anchor = old_cursor.min(cursor);
        let moved = |l: usize| if l > anchor { (l as isize + delta).max(anchor as isize) as usize } else { l };
        if let Some(lines) = self.dbg.breakpoints.get_mut(&path) {
            for l in lines.iter_mut() {
                *l = moved(*l);
            }
            lines.sort_unstable();
            lines.dedup();
        }
        if let Some(m) = self.dbg.bp_extra.get_mut(&path) {
            *m = std::mem::take(m).into_iter().map(|(l, v)| (moved(l), v)).collect();
        }
        self.send_breakpoints(&path);
    }

    // ----- starting and stopping ------------------------------------------

    pub(super) fn debug_active(&self) -> bool {
        self.dbg.session.as_ref().is_some_and(|s| s.status != Status::Exited)
    }

    fn debug_stopped(&self) -> bool {
        self.dbg.session.as_ref().is_some_and(|s| s.status == Status::Stopped)
    }

    /// F5: continue when paused, otherwise build and start debugging.
    pub(super) fn cmd_debug(&mut self, ctx: &egui::Context) {
        if self.debug_stopped() {
            self.debug_step("continue");
            return;
        }
        if self.debug_active() {
            return;
        }
        if self.project_root().is_none() {
            self.toast(ctx, "Open a project folder to debug", true);
            return;
        }
        if let Err(e) = dap::find_adapter(&self.settings.debugger_path) {
            self.toast(ctx, e, true);
            return;
        }
        if self.makefile.is_some() {
            let cfg = self.build_config();
            let target = if cfg.target == "run" { String::new() } else { cfg.target.clone() };
            self.cmd_build(ctx, Some(target));
            self.debug_after_build = true;
        } else {
            self.start_debugging(ctx);
        }
    }

    /// What to debug: the Run command from Build Settings, `make run`'s
    /// command, or the Makefile's default target if it's a program.
    pub(super) fn debug_target(&self) -> Option<(PathBuf, Vec<String>, PathBuf)> {
        let dir = self.make_dir()?;
        let cfg = self.build_config();
        let mut words = make::split_args(cfg.run_command.trim());
        if words.is_empty() && self.targets.iter().any(|t| t == "run") {
            words = dry_run_command(&make::make_program(&cfg), &dir);
        }
        if words.is_empty() {
            let target = if cfg.target.is_empty() { self.targets.first().cloned() } else { Some(cfg.target.clone()) };
            if let Some(t) = target
                && resolve_program(&t, &dir).is_some()
            {
                words = vec![format!("./{t}")];
            }
        }
        if words.is_empty() {
            return None;
        }
        let program = resolve_program(&words[0], &dir)?;
        Some((program, words[1..].to_vec(), dir))
    }

    pub(super) fn start_debugging(&mut self, ctx: &egui::Context) {
        let Some((program, args, cwd)) = self.debug_target() else {
            self.toast(ctx, "Set the program to debug as the Run command in Build Settings", true);
            self.modal = Some(Modal::BuildSettings(self.build_config()));
            return;
        };
        let adapter = match dap::find_adapter(&self.settings.debugger_path) {
            Ok(a) => a,
            Err(e) => {
                self.toast(ctx, e, true);
                return;
            }
        };
        let mut init = self.project_root().and_then(|r| adapter.init_file(&r));
        if init.is_some() && !self.folder_trusted() {
            let name = init.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            self.toast(ctx, format!("Skipped {name}: this folder isn't trusted (File > Trust This Folder…)"), false);
            init = None;
        }
        let adapter = adapter.with_init_file(init.as_deref());
        let mut client = match dap::Client::start(&adapter, &cwd, ctx) {
            Ok(c) => c,
            Err(e) => {
                self.toast(ctx, format!("Couldn't start {}: {e}", adapter.program.display()), true);
                return;
            }
        };
        let seq = client.request(
            "initialize",
            json!({
                "clientID": "cedit", "clientName": "cedit", "adapterID": adapter.name(),
                "linesStartAt1": true, "columnsStartAt1": true, "pathFormat": "path",
                "supportsRunInTerminalRequest": true, "supportsVariableType": true,
            }),
        );
        let mut pending = HashMap::new();
        pending.insert(seq, Req::Initialize);
        self.dbg.console.clear();
        self.dbg.verified.clear();
        let shown = program.strip_prefix(&cwd).unwrap_or(&program).display().to_string();
        self.dbg.console.push((ConsoleKind::Info, format!("Debugging {shown} with {}\n", adapter.program.display())));
        if let Some(f) = &init {
            self.dbg.console.push((ConsoleKind::Info, format!("Loading {}\n", f.file_name().unwrap_or_default().to_string_lossy())));
        }
        if adapter.kind == dap::AdapterKind::Gdb || !cfg!(target_os = "linux") {
            self.dbg.console.push((ConsoleKind::Info, "The program's output appears here (it can't read keyboard input while debugging)\n".into()));
        }
        self.dbg.session = Some(Session {
            client,
            adapter_name: adapter.name(),
            launch: adapter.launch_args(&program, &args, &cwd, init.as_deref()),
            pending,
            status: Status::Starting,
            stop_reason: String::new(),
            thread: None,
            frames: Vec::new(),
            frame: 0,
            scopes: Vec::new(),
            vars: HashMap::new(),
            requested: HashSet::new(),
            watch_values: HashMap::new(),
            paths: HashMap::new(),
            prev: HashMap::new(),
            exit_code: None,
            disconnect_at: None,
            stop_gen: 0,
            req_gen: HashMap::new(),
            config_wait: None,
            launch_after_breakpoints: adapter.kind == dap::AdapterKind::Gdb,
            read_memory: false,
        });
        self.dbg.exec = None;
        self.bottom_open = true;
        self.bottom_tab = BottomTab::Debug;
    }

    pub(super) fn cmd_stop_debugging(&mut self, ctx: &egui::Context) {
        self.debug_after_build = false;
        let now = ctx.input(|i| i.time);
        if let Some(s) = &mut self.dbg.session {
            if s.status == Status::Exited {
                self.dbg.session = None;
            } else if s.disconnect_at.is_none() {
                s.client.request("disconnect", json!({"terminateDebuggee": true}));
                s.disconnect_at = Some(now);
            }
        }
        self.dbg.exec = None;
    }

    pub(super) fn debug_menu(&mut self, ui: &mut Ui) {
        use crate::settings::Action as A;
        let ctx = ui.ctx().clone();
        let has_root = self.project_root().is_some();
        let active = self.debug_active();
        let stopped = self.debug_stopped();
        let running = self.dbg.session.as_ref().is_some_and(|s| s.status == Status::Running);
        let has_file = self.docs.get(self.active).is_some_and(|d| d.path.is_some() && d.viewer.is_none());
        ui.menu_button("Debug", |ui| {
            let start = if stopped { "Continue" } else { "Start Debugging" };
            if ui.add_enabled(has_root && (stopped || !active), egui::Button::new(start).shortcut_text(self.keymap.label(&ctx, A::Debug))).clicked() {
                self.cmd_debug(&ctx);
            }
            if ui.add_enabled(running, egui::Button::new("Pause")).clicked() {
                self.debug_step("pause");
            }
            for (a, cmd) in [(A::StepOver, "next"), (A::StepInto, "stepIn"), (A::StepOut, "stepOut")] {
                if ui.add_enabled(stopped, egui::Button::new(a.title()).shortcut_text(self.keymap.label(&ctx, a))).clicked() {
                    self.debug_step(cmd);
                }
            }
            if ui.add_enabled(stopped && has_file, egui::Button::new("Run to Cursor")).clicked() {
                self.cmd_run_to_cursor(&ctx);
            }
            if ui.add_enabled(active, egui::Button::new("Restart")).clicked() {
                self.cmd_restart_debugging(&ctx);
            }
            if ui.add_enabled(active, egui::Button::new("Stop Debugging").shortcut_text(self.keymap.label(&ctx, A::StopDebug))).clicked() {
                self.cmd_stop_debugging(&ctx);
            }
            ui.separator();
            if ui.add_enabled(has_file, egui::Button::new("Toggle Breakpoint").shortcut_text(self.keymap.label(&ctx, A::ToggleBreakpoint))).clicked() {
                self.cmd_toggle_breakpoint();
            }
            if ui.add_enabled(has_file, egui::Button::new("Edit Breakpoint…")).on_hover_text("Add a condition or log message to the breakpoint on this line").clicked() {
                self.cmd_edit_breakpoint();
            }
            let mut muted = self.dbg.muted;
            if ui.add_enabled(!self.dbg.breakpoints.is_empty(), egui::Checkbox::new(&mut muted, "Mute Breakpoints")).changed() {
                self.set_breakpoints_muted(muted);
            }
            if ui.add_enabled(!self.dbg.breakpoints.is_empty(), egui::Button::new("Remove All Breakpoints")).clicked() {
                self.cmd_clear_breakpoints();
            }
            ui.separator();
            if ui.button("Show Debugger").clicked() {
                self.bottom_open = true;
                self.bottom_tab = BottomTab::Debug;
            }
        });
    }

    pub(super) fn cmd_restart_debugging(&mut self, ctx: &egui::Context) {
        if self.debug_active() {
            self.debug_restart(ctx);
        } else {
            self.cmd_debug(ctx);
        }
    }

    /// Continue until the line with the cursor (a one-time breakpoint).
    pub(super) fn cmd_run_to_cursor(&mut self, ctx: &egui::Context) {
        let Some(doc) = self.docs.get(self.active) else { return };
        let Some(path) = doc.path.clone() else { return };
        let line = doc.cursor_line_col.0;
        if !self.debug_stopped() {
            self.toast(ctx, "Run to Cursor works while the program is paused", false);
            return;
        }
        self.dbg.run_to = Some((path.clone(), line));
        self.send_breakpoints(&path);
        self.debug_step("continue");
    }

    pub(super) fn cmd_clear_breakpoints(&mut self) {
        let paths: Vec<PathBuf> = self.dbg.breakpoints.keys().cloned().collect();
        self.dbg.breakpoints.clear();
        self.dbg.bp_extra.clear();
        for p in paths {
            self.send_breakpoints(&p);
        }
    }

    pub(super) fn set_breakpoints_muted(&mut self, muted: bool) {
        self.dbg.muted = muted;
        let paths: Vec<PathBuf> = self.dbg.breakpoints.keys().cloned().collect();
        for p in paths {
            self.send_breakpoints(&p);
        }
    }

    /// Edit the condition / log message of the breakpoint on the cursor line
    /// (adding the breakpoint if there isn't one).
    pub(super) fn cmd_edit_breakpoint(&mut self) {
        let Some(doc) = self.docs.get(self.active) else { return };
        let Some(path) = doc.path.clone() else { return };
        let line = doc.cursor_line_col.0;
        self.edit_breakpoint_at(path, line);
    }

    fn edit_breakpoint_at(&mut self, path: PathBuf, line: usize) {
        let (condition, log) = self.dbg.bp_extra.get(&path).and_then(|m| m.get(&line)).cloned().unwrap_or_default();
        self.modal = Some(Modal::EditBreakpoint { path, line, condition, log, focus: true });
    }

    pub(super) fn edit_breakpoint_modal(
        &mut self,
        ctx: &egui::Context,
        path: PathBuf,
        line: usize,
        mut condition: String,
        mut log: String,
        mut focus: bool,
    ) {
        let mut done = None;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        egui::Modal::new(egui::Id::unique("edit-breakpoint")).show(ctx, |ui| {
            ui.set_width(440.0);
            ui.heading(format!("Breakpoint at {name}:{}", line + 1));
            ui.add_space(6.0);
            ui.label("Stop only when this is true (C expression):");
            let r = ui.add(egui::TextEdit::singleline(&mut condition).hint_text("e.g. i == 42 && p != NULL").font(egui::TextStyle::Monospace).desired_width(f32::INFINITY));
            if focus {
                r.request_focus();
                focus = false;
            }
            ui.add_space(4.0);
            ui.label("Log a message instead of stopping (put expressions in {braces}):");
            ui.add(egui::TextEdit::singleline(&mut log).hint_text("e.g. i = {i}, name = {name}").font(egui::TextStyle::Monospace).desired_width(f32::INFINITY));
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new("Remove Breakpoint").color(self.theme.error)).clicked() {
                    done = Some(Some(false));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(RichText::new("OK").strong()).clicked() || ui.input(|i| i.key_pressed(Key::Enter)) {
                        done = Some(Some(true));
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        done = Some(None);
                    }
                });
            });
        });
        match done {
            None => self.modal = Some(Modal::EditBreakpoint { path, line, condition, log, focus }),
            Some(None) => {}
            Some(Some(keep)) => {
                let lines = self.dbg.breakpoints.entry(path.clone()).or_default();
                let has = lines.contains(&line);
                if keep {
                    if !has {
                        lines.push(line);
                        lines.sort_unstable();
                    }
                    self.dbg.bp_extra.entry(path.clone()).or_default().insert(line, (condition, log));
                    self.send_breakpoints(&path);
                } else if has {
                    self.toggle_breakpoint(&path, line);
                } else if lines.is_empty() {
                    self.dbg.breakpoints.remove(&path);
                }
            }
        }
    }

    fn debug_restart(&mut self, ctx: &egui::Context) {
        self.dbg.session = None;
        self.dbg.exec = None;
        self.cmd_debug(ctx);
    }

    /// continue / next / stepIn / stepOut / pause
    pub(super) fn debug_step(&mut self, command: &str) {
        let Some(s) = &mut self.dbg.session else { return };
        let Some(thread) = s.thread else { return };
        let ok = match command {
            "pause" => s.status == Status::Running,
            _ => s.status == Status::Stopped,
        };
        if !ok {
            return;
        }
        let seq = s.client.request(command, json!({"threadId": thread}));
        s.pending.insert(seq, Req::Other);
        if command != "pause" {
            s.status = Status::Running;
            self.dbg.exec = None;
            self.dbg.hover = None;
        }
    }

    // ----- protocol --------------------------------------------------------

    pub(super) fn update_debug(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        let Some(s) = &mut self.dbg.session else { return };
        let msgs = s.client.poll();
        if let Some(t) = s.disconnect_at {
            if now - t > 2.0 {
                s.client.kill();
                self.dbg.session = None;
                self.dbg.console.push((ConsoleKind::Info, "Debugger stopped".into()));
                return;
            }
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
        for m in msgs {
            self.handle_dap(ctx, m);
        }
        self.update_debug_hover(ctx, now);
    }

    fn handle_dap(&mut self, ctx: &egui::Context, m: Value) {
        let ty = m["type"].as_str().unwrap_or_default();
        match ty {
            "response" => self.handle_response(ctx, m),
            "event" => self.handle_event(ctx, m),
            "request" => self.handle_reverse_request(ctx, m),
            "cedit_stderr" => {
                let text = m["text"].as_str().unwrap_or_default().to_string();
                // Some adapters print crash noise while shutting down; ignore it.
                let quitting = self.dbg.session.as_ref().is_none_or(|s| s.disconnect_at.is_some() || s.status == Status::Exited || s.exit_code.is_some());
                if !text.trim().is_empty() && !quitting {
                    self.dbg.console.push((ConsoleKind::Err, text));
                }
            }
            "cedit_exit" => {
                let was_running = self.dbg.session.as_ref().is_some_and(|s| s.status != Status::Exited && s.disconnect_at.is_none());
                if was_running {
                    self.dbg.console.push((ConsoleKind::Err, "The debugger quit unexpectedly".into()));
                }
                self.dbg.session = None;
                self.dbg.exec = None;
            }
            _ => {}
        }
    }

    fn handle_reverse_request(&mut self, ctx: &egui::Context, m: Value) {
        let seq = m["seq"].as_i64().unwrap_or(0);
        let command = m["command"].as_str().unwrap_or_default().to_string();
        if command == "runInTerminal" {
            let a = &m["arguments"];
            let args: Vec<String> = a["args"].as_array().map(|v| v.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
            let cwd = a["cwd"].as_str().map(PathBuf::from).or_else(|| self.make_dir()).unwrap_or_else(|| PathBuf::from("."));
            let env: Vec<(String, String)> = a["env"]
                .as_object()
                .map(|o| o.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect())
                .unwrap_or_default();
            let pid = if args.is_empty() {
                None
            } else {
                let pid = self.run_in_terminal_env(ctx, args[0].clone(), args[1..].to_vec(), cwd, &env);
                // Keep the Debug panel in front; the program's tab is one click away.
                self.bottom_tab = BottomTab::Debug;
                pid
            };
            if let Some(s) = &mut self.dbg.session {
                match pid {
                    Some(pid) => s.client.respond(seq, &command, true, json!({"processId": pid})),
                    None => s.client.respond(seq, &command, false, json!({})),
                }
            }
            self.dbg.console.push((ConsoleKind::Info, "The program runs in the Run terminal tab, where you can type input".into()));
        } else if let Some(s) = &mut self.dbg.session {
            s.client.respond(seq, &command, false, json!({}));
        }
    }

    fn handle_event(&mut self, ctx: &egui::Context, m: Value) {
        let event = m["event"].as_str().unwrap_or_default();
        let body = &m["body"];
        match event {
            "initialized" => {
                let all: Vec<(PathBuf, Vec<Value>)> =
                    self.dbg.breakpoints.keys().map(|p| (p.clone(), self.dbg.bp_json(p))).collect();
                let Some(s) = &mut self.dbg.session else { return };
                let mut wait = HashSet::new();
                for (p, bps) in all {
                    let seq = s.client.request("setBreakpoints", json!({"source": {"path": p}, "breakpoints": bps}));
                    s.pending.insert(seq, Req::SetBreakpoints(p));
                    wait.insert(seq);
                }
                let seq = s.client.request("setExceptionBreakpoints", json!({"filters": []}));
                s.pending.insert(seq, Req::Other);
                wait.insert(seq);
                s.config_wait = Some(wait);
                if s.status == Status::Starting {
                    s.status = Status::Running;
                }
            }
            "stopped" => {
                if let Some((p, _)) = self.dbg.run_to.take() {
                    self.send_breakpoints(&p);
                }
                let Some(s) = &mut self.dbg.session else { return };
                s.status = Status::Stopped;
                s.stop_gen += 1;
                if let Some(t) = body["threadId"].as_i64() {
                    s.thread = Some(t);
                }
                s.stop_reason = body["reason"].as_str().unwrap_or("paused").to_string();
                if let Some(text) = body["text"].as_str().or(body["description"].as_str())
                    && s.stop_reason == "exception"
                {
                    self.dbg.console.push((ConsoleKind::Err, text.to_string()));
                }
                // Remember this stop's values to highlight what the next step changes.
                let mut snap = HashMap::new();
                for (r, vars) in &s.vars {
                    if let Some(base) = s.paths.get(r) {
                        for v in vars {
                            snap.insert(format!("{base}/{}", v.name), summary(s, v, 0, false));
                        }
                    }
                }
                for (w, wv) in &s.watch_values {
                    if wv.value.is_ok() {
                        snap.insert(format!("watch:{w}"), summary(s, &watch_var(w, wv), 0, false));
                    }
                }
                s.prev = snap;
                s.paths.clear();
                s.frames.clear();
                s.scopes.clear();
                s.vars.clear();
                s.requested.clear();
                if let Some(m) = &mut self.dbg.mem
                    && let Some(a) = m.address
                {
                    m.prev = Some((a, m.data.clone()));
                }
                if s.thread.is_none() {
                    // Ask for threads first; stackTrace follows.
                    s.thread = Some(1);
                }
                let seq = s.client.request("stackTrace", json!({"threadId": s.thread, "startFrame": 0, "levels": 64}));
                s.pending.insert(seq, Req::StackTrace);
                s.req_gen.insert(seq, s.stop_gen);
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
            "continued" => {
                if let Some(s) = &mut self.dbg.session {
                    s.status = Status::Running;
                }
                self.dbg.exec = None;
            }
            "thread" => {
                if let Some(s) = &mut self.dbg.session
                    && s.thread.is_none()
                    && body["reason"] == "started"
                {
                    s.thread = body["threadId"].as_i64();
                }
            }
            "output" => {
                let cat = body["category"].as_str().unwrap_or("console");
                if cat == "telemetry" {
                    return;
                }
                let text = body["output"].as_str().unwrap_or_default();
                let kind = match cat {
                    "stdout" => ConsoleKind::Out,
                    "stderr" => ConsoleKind::Err,
                    _ => ConsoleKind::Info,
                };
                self.console_append(kind, text);
            }
            "breakpoint" => {
                let bp = &body["breakpoint"];
                if let (Some(path), Some(line)) = (bp["source"]["path"].as_str(), bp["line"].as_u64()) {
                    let verified = bp["verified"].as_bool().unwrap_or(false);
                    self.dbg.verified.insert((PathBuf::from(path), line as usize - 1), verified);
                }
            }
            "exited" => {
                let code = body["exitCode"].as_i64();
                if let Some(s) = &mut self.dbg.session {
                    s.exit_code = code;
                }
                self.dbg.console.push((ConsoleKind::Info, format!("Program exited with code {}", code.unwrap_or(0))));
            }
            "terminated" => {
                if let Some(s) = &mut self.dbg.session {
                    s.status = Status::Exited;
                    if s.disconnect_at.is_none() {
                        s.client.request("disconnect", json!({"terminateDebuggee": true}));
                        s.disconnect_at = Some(ctx.input(|i| i.time));
                    }
                }
                self.dbg.exec = None;
            }
            _ => {}
        }
    }

    fn console_append(&mut self, kind: ConsoleKind, text: &str) {
        // Output arrives in chunks; continue the last line if it had no newline.
        let mut parts = text.split('\n').peekable();
        if let Some((k, last)) = self.dbg.console.last_mut()
            && *k == kind
            && !last.ends_with('\n')
            && let Some(first) = parts.next()
        {
            last.push_str(first);
            if parts.peek().is_some() {
                last.push('\n');
            }
        }
        while let Some(p) = parts.next() {
            if p.is_empty() && parts.peek().is_none() {
                break;
            }
            let mut line = p.to_string();
            if parts.peek().is_some() {
                line.push('\n');
            }
            self.dbg.console.push((kind.clone(), line));
        }
        if self.dbg.console.len() > 5000 {
            self.dbg.console.drain(..1000);
        }
    }

    fn handle_response(&mut self, ctx: &egui::Context, m: Value) {
        let Some(s) = &mut self.dbg.session else { return };
        let seq = m["request_seq"].as_i64().unwrap_or(-1);
        let Some(req) = s.pending.remove(&seq) else { return };
        if let Some(wait) = &mut s.config_wait
            && wait.remove(&seq)
            && wait.is_empty()
        {
            s.config_wait = None;
            if s.launch_after_breakpoints {
                let args = s.launch.clone();
                let seq = s.client.request("launch", args);
                s.pending.insert(seq, Req::Launch);
            }
            let seq = s.client.request("configurationDone", json!({}));
            s.pending.insert(seq, Req::Other);
        }
        let stale = s.req_gen.remove(&seq).is_some_and(|g| g != s.stop_gen);
        let success = m["success"].as_bool().unwrap_or(false);
        let body = &m["body"];
        // lldb-dap puts the real reason in body.error.format; `message` may be absent or generic.
        let message = body["error"]["format"]
            .as_str()
            .or_else(|| m["message"].as_str())
            .unwrap_or("request failed")
            .to_string();
        if stale {
            return;
        }
        match req {
            Req::Initialize => {
                if !success {
                    self.dbg.console.push((ConsoleKind::Err, format!("The debugger refused to start: {message}")));
                    return;
                }
                s.read_memory = body["supportsReadMemoryRequest"].as_bool().unwrap_or(false);
                if !s.launch_after_breakpoints {
                    let args = s.launch.clone();
                    let seq = s.client.request("launch", args);
                    s.pending.insert(seq, Req::Launch);
                }
            }
            Req::Launch => {
                if !success {
                    let msg = format!("Couldn't launch the program: {message}");
                    self.dbg.console.push((ConsoleKind::Err, msg.clone()));
                    self.toast(ctx, msg, true);
                    self.cmd_stop_debugging(ctx);
                }
            }
            Req::SetBreakpoints(path) => {
                let lines = self.dbg.bp_lines(&path);
                let results = body["breakpoints"].as_array().cloned().unwrap_or_default();
                for (i, line) in lines.iter().enumerate() {
                    let verified = success && results.get(i).and_then(|b| b["verified"].as_bool()).unwrap_or(false);
                    self.dbg.verified.insert((path.clone(), *line), verified);
                }
            }
            Req::StackTrace => {
                let frames: Vec<Frame> = body["stackFrames"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|f| Frame {
                                id: f["id"].as_i64().unwrap_or(0),
                                name: f["name"].as_str().unwrap_or("?").to_string(),
                                path: f["source"]["path"].as_str().map(PathBuf::from),
                                line: (f["line"].as_u64().unwrap_or(1) as usize).saturating_sub(1),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                s.frames = frames;
                // Show the first frame that has source code (skip libc etc.).
                let pick = s.frames.iter().position(|f| f.path.as_ref().is_some_and(|p| p.is_file())).unwrap_or(0);
                self.select_frame(pick);
            }
            Req::Scopes => {
                s.scopes = body["scopes"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|x| Scope {
                                name: x["name"].as_str().unwrap_or("?").to_string(),
                                reference: x["variablesReference"].as_i64().unwrap_or(0),
                                expensive: x["expensive"].as_bool().unwrap_or(false) || x["name"].as_str().is_some_and(|n| n.contains("Register")),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let func = s.frames.get(s.frame).map(|f| f.name.clone()).unwrap_or_default();
                for sc in &s.scopes {
                    s.paths.insert(sc.reference, format!("{func}/{}", sc.name));
                }
                let wanted: Vec<i64> = s.scopes.iter().filter(|x| !x.expensive && x.reference > 0).map(|x| x.reference).collect();
                for r in wanted {
                    self.request_variables(r);
                }
            }
            Req::Variables(r) => {
                let vars: Vec<Var> = body["variables"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|v| Var {
                                name: v["name"].as_str().unwrap_or("?").to_string(),
                                value: v["value"].as_str().unwrap_or_default().to_string(),
                                ty: v["type"].as_str().unwrap_or_default().to_string(),
                                reference: v["variablesReference"].as_i64().unwrap_or(0),
                                evaluate_name: v["evaluateName"].as_str().unwrap_or_default().to_string(),
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if let Some(base) = s.paths.get(&r).cloned() {
                    for v in vars.iter().filter(|v| v.reference > 0) {
                        s.paths.insert(v.reference, format!("{base}/{}", v.name));
                    }
                }
                // Load the fields of local structs too, for the values shown in the code.
                let is_scope = s.scopes.iter().any(|sc| sc.reference == r && !sc.expensive);
                let structs: Vec<i64> = if is_scope { vars.iter().filter(|v| v.reference > 0 && is_aggregate(&v.value)).map(|v| v.reference).take(20).collect() } else { Vec::new() };
                s.vars.insert(r, vars);
                for r in structs {
                    self.request_variables(r);
                }
            }
            Req::Evaluate(kind) => {
                let result = body["result"].as_str().unwrap_or_default().to_string();
                match kind {
                    EvalKind::Hover(id) => {
                        if let Some(h) = &mut self.dbg.hover
                            && h.id == id
                        {
                            h.text = success.then(|| clean_value(&result));
                        }
                    }
                    EvalKind::Watch(expr) => {
                        let reference = if success { body["variablesReference"].as_i64().unwrap_or(0) } else { 0 };
                        if reference > 0 {
                            s.paths.insert(reference, format!("watch:{expr}"));
                        }
                        let load = reference > 0 && is_aggregate(&result);
                        let value = if success { Ok(result) } else { Err(message) };
                        s.watch_values.insert(expr, WatchValue { value, reference });
                        if load {
                            self.request_variables(reference);
                        }
                    }
                    EvalKind::WatchType(watch, expr, n) => {
                        let ty = body["type"].as_str().unwrap_or_default();
                        let array = if success { array_expression(&expr, ty, n) } else { None };
                        let array = array.or_else(|| (s.adapter_name == "gdb").then(|| format!("*({expr})@{n}")));
                        match array {
                            Some(a) => {
                                let frame = s.frames.get(s.frame).map(|f| f.id);
                                let hex = self.dbg.hex;
                                self.evaluate_in(frame, &a, EvalKind::Watch(watch), "watch", hex);
                            }
                            None => {
                                let err = if success { format!("{expr} isn't a pointer or array ({ty})") } else { message };
                                s.watch_values.insert(watch, WatchValue { value: Err(err), reference: 0 });
                            }
                        }
                    }
                    EvalKind::MemoryDump(mgen, addr) => {
                        if let Some(mv) = &mut self.dbg.mem
                            && mgen == self.dbg.mem_gen
                        {
                            let data = parse_memory_dump(&result);
                            if success && !data.is_empty() {
                                mv.address = Some(addr);
                                mv.data = data;
                                mv.unreadable = 0;
                                mv.error = None;
                            } else {
                                mv.data.clear();
                                mv.error = Some(if success { result.trim().to_string() } else { message });
                            }
                        }
                    }
                    EvalKind::Memory(mgen, retried) => {
                        if let Some(mv) = &mut self.dbg.mem
                            && mgen == self.dbg.mem_gen
                        {
                            let reference = memory_address(&result, body["memoryReference"].as_str());
                            match (success, reference) {
                                (true, Some(r)) => {
                                    mv.reference = Some(r);
                                    mv.error = None;
                                    self.read_memory();
                                }
                                // Not a pointer (an array, int, ...): look at where it's stored instead.
                                (true, None) if !retried => {
                                    let e = format!("&({})", mv.expr);
                                    let frame = self.current_frame_id();
                                    self.evaluate_in(frame, &e, EvalKind::Memory(mgen, true), "watch", false);
                                }
                                (true, None) => mv.error = Some(format!("{} isn't an address: {result}", mv.expr)),
                                (false, _) => mv.error = Some(message),
                            }
                        }
                    }
                    EvalKind::Repl => {
                        let kind = if success { ConsoleKind::Out } else { ConsoleKind::Err };
                        let text = if success { result } else { message };
                        self.dbg.console.push((kind, format!("{text}\n")));
                    }
                }
            }
            Req::ReadMemory(mgen) => {
                if let Some(mv) = &mut self.dbg.mem
                    && mgen == self.dbg.mem_gen
                {
                    if success {
                        mv.address = body["address"].as_str().and_then(parse_address);
                        mv.data = body["data"].as_str().map(base64_decode).unwrap_or_default();
                        mv.unreadable = body["unreadableBytes"].as_u64().unwrap_or(0) as usize;
                        mv.error = None;
                    } else {
                        mv.data.clear();
                        mv.error = Some(message);
                    }
                }
            }
            Req::Other => {
                if !success && !message.is_empty() && message != "notStopped" {
                    self.dbg.console.push((ConsoleKind::Err, message));
                }
            }
        }
    }

    fn request_variables(&mut self, reference: i64) {
        let Some(s) = &mut self.dbg.session else { return };
        if reference <= 0 || !s.requested.insert(reference) {
            return;
        }
        let mut args = json!({"variablesReference": reference});
        if self.dbg.hex {
            args["format"] = json!({"hex": true});
        }
        let seq = s.client.request("variables", args);
        s.pending.insert(seq, Req::Variables(reference));
        s.req_gen.insert(seq, s.stop_gen);
    }

    /// Show a frame of the call stack: open its source and load its variables.
    fn select_frame(&mut self, i: usize) {
        let Some(s) = &mut self.dbg.session else { return };
        let Some(f) = s.frames.get(i).cloned() else { return };
        s.frame = i;
        s.scopes.clear();
        s.vars.clear();
        s.requested.clear();
        let seq = s.client.request("scopes", json!({"frameId": f.id}));
        s.pending.insert(seq, Req::Scopes);
        s.req_gen.insert(seq, s.stop_gen);
        let top = s.frames.iter().position(|f| f.path.as_ref().is_some_and(|p| p.is_file())) == Some(i);
        let watches = self.dbg.watches.clone();
        for w in watches {
            self.evaluate_watch(Some(f.id), &w);
        }
        if self.dbg.mem.is_some() {
            self.refresh_memory();
        }
        if let Some(path) = f.path.filter(|p| p.is_file()) {
            self.dbg.exec = Some((path.clone(), f.line, top));
            self.open_file(&path);
            if let Some(doc) = self.docs.get_mut(self.active)
                && doc.path.as_deref() == Some(path.as_path())
            {
                doc.goto_line_col(f.line, 0);
            }
        } else {
            self.dbg.exec = None;
        }
    }

    fn current_frame_id(&self) -> Option<i64> {
        let s = self.dbg.session.as_ref()?;
        (s.status == Status::Stopped).then(|| s.frames.get(s.frame).map(|f| f.id)).flatten()
    }

    fn evaluate(&mut self, expr: &str, kind: EvalKind, context: &str) {
        let frame = self.current_frame_id();
        let hex = self.dbg.hex && context != "repl";
        self.evaluate_in(frame, expr, kind, context, hex);
    }

    fn evaluate_in(&mut self, frame: Option<i64>, expr: &str, kind: EvalKind, context: &str, hex: bool) {
        let Some(s) = &mut self.dbg.session else { return };
        let mut args = json!({"expression": expr, "context": context});
        if let Some(f) = frame {
            args["frameId"] = json!(f);
        }
        if hex {
            args["format"] = json!({"hex": true});
        }
        let seq = s.client.request("evaluate", args);
        s.pending.insert(seq, Req::Evaluate(kind));
        s.req_gen.insert(seq, s.stop_gen);
    }

    /// Evaluate a watch. `ptr, N` shows N elements of what `ptr` points to.
    fn evaluate_watch(&mut self, frame: Option<i64>, w: &str) {
        let hex = self.dbg.hex;
        match parse_array_watch(w) {
            Some((expr, n)) => self.evaluate_in(frame, &expr, EvalKind::WatchType(w.to_string(), expr.clone(), n), "watch", false),
            None => self.evaluate_in(frame, w, EvalKind::Watch(w.to_string()), "watch", hex),
        }
    }

    // ----- memory view -------------------------------------------------------

    fn open_memory(&mut self, expr: &str) {
        let expr = expr.trim().to_string();
        if expr.is_empty() {
            return;
        }
        self.dbg.mem_input = expr.clone();
        self.dbg.mem = Some(MemView { expr, ..Default::default() });
        self.dbg.right_tab = RightTab::Memory;
        self.refresh_memory();
    }

    /// Resolve the memory view's expression again (it may point elsewhere now), then read.
    fn refresh_memory(&mut self) {
        let Some(mv) = &mut self.dbg.mem else { return };
        if !self.dbg.session.as_ref().is_some_and(|s| s.status == Status::Stopped) {
            return;
        }
        self.dbg.mem_gen += 1;
        let mgen = self.dbg.mem_gen;
        let expr = mv.expr.clone();
        // A plain address needs no evaluation.
        if let Some(a) = parse_address(&expr) {
            mv.reference = Some(format!("0x{a:x}"));
            self.read_memory();
        } else {
            let frame = self.current_frame_id();
            self.evaluate_in(frame, &expr, EvalKind::Memory(mgen, false), "watch", false);
        }
    }

    fn read_memory(&mut self) {
        let mgen = self.dbg.mem_gen;
        let Some(mv) = &self.dbg.mem else { return };
        let Some(reference) = mv.reference.clone() else { return };
        let offset = mv.offset;
        let Some(s) = &mut self.dbg.session else { return };
        if s.read_memory {
            let seq = s.client.request("readMemory", json!({"memoryReference": reference, "offset": offset, "count": MEM_BYTES}));
            s.pending.insert(seq, Req::ReadMemory(mgen));
            return;
        }
        // Older adapters: ask the debugger itself to dump the bytes.
        let Some(addr) = parse_address(&reference).map(|a| a.wrapping_add_signed(offset)) else { return };
        let cmd = if s.adapter_name == "gdb" {
            format!("x/{MEM_BYTES}xb 0x{addr:x}")
        } else {
            format!("memory read --force -s1 -fx -c{MEM_BYTES} 0x{addr:x}")
        };
        self.evaluate(&cmd, EvalKind::MemoryDump(mgen, addr), "repl");
    }

    /// Values of the selected frame's variables to show at the end of the lines
    /// that use them, from the start of the function down to where it's paused.
    fn inline_values(&self, path: &Path) -> Vec<(usize, String)> {
        let Some(s) = self.dbg.session.as_ref().filter(|s| s.status == Status::Stopped) else { return Vec::new() };
        let Some((p, exec_line, _)) = &self.dbg.exec else { return Vec::new() };
        if p != path {
            return Vec::new();
        }
        let Some(doc) = self.docs.iter().find(|d| d.path.as_deref() == Some(path)) else { return Vec::new() };
        let mut values: HashMap<&str, String> = HashMap::new();
        for sc in s.scopes.iter().rev().filter(|sc| !sc.expensive) {
            for v in s.vars.get(&sc.reference).into_iter().flatten() {
                values.insert(v.name.as_str(), summary(s, v, 0, true));
            }
        }
        if values.is_empty() {
            return Vec::new();
        }
        let lines: Vec<&str> = doc.text.lines().take(exec_line + 1).collect();
        inline_value_lines(&lines, *exec_line, &values)
    }

    // ----- hover values ----------------------------------------------------

    /// Called with the editor's hover position; returns true if the debugger
    /// handles hovering (so the language server's hover stays hidden).
    pub(super) fn debug_on_hover(&mut self, now: f64, hover: Option<(usize, egui::Pos2)>) -> bool {
        if !self.debug_stopped() {
            self.dbg.hover = None;
            return false;
        }
        let Some((b, pos)) = hover else {
            self.dbg.hover = None;
            return true;
        };
        let Some(doc) = self.docs.get(self.active) else { return false };
        let expr = expression_at(&doc.text, b);
        if expr.is_empty() {
            self.dbg.hover = None;
            return true;
        }
        if let Some(h) = &mut self.dbg.hover
            && h.doc_id == doc.id
            && h.expr == expr
        {
            if h.text.is_none() {
                h.pos = pos;
            }
            return true;
        }
        self.dbg.hover_counter += 1;
        self.dbg.hover = Some(DebugHover { doc_id: doc.id, expr, pos, since: now, sent: false, id: self.dbg.hover_counter, text: None });
        true
    }

    fn update_debug_hover(&mut self, ctx: &egui::Context, now: f64) {
        let Some(h) = &mut self.dbg.hover else { return };
        if h.sent {
            return;
        }
        if now - h.since < 0.35 {
            ctx.request_repaint_after(std::time::Duration::from_millis(60));
            return;
        }
        h.sent = true;
        let (expr, id) = (h.expr.clone(), h.id);
        self.evaluate(&expr, EvalKind::Hover(id), "hover");
    }

    pub(super) fn debug_hover_active(&self) -> bool {
        self.dbg.hover.is_some()
    }

    pub(super) fn debug_hover_ui(&mut self, ctx: &egui::Context) {
        let Some(h) = &self.dbg.hover else { return };
        let Some(text) = &h.text else { return };
        let (expr, text, pos) = (h.expr.clone(), text.clone(), h.pos);
        egui::Area::new(egui::Id::unique("debug-hover"))
            .order(egui::Order::Tooltip)
            .fixed_pos(pos + egui::vec2(12.0, 18.0))
            .interactable(false)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_max_width(560.0);
                    let mut job = egui::text::LayoutJob::default();
                    let font = egui::FontId::monospace(12.5);
                    job.append(&format!("{expr} = "), 0.0, egui::TextFormat::simple(font.clone(), self.theme.variable));
                    let shown: String = text.lines().take(20).collect::<Vec<_>>().join("\n");
                    job.append(&shown, 0.0, egui::TextFormat::simple(font, self.theme.text));
                    ui.label(job);
                });
            });
    }

    // ----- panel -------------------------------------------------------------

    pub(super) fn debug_toolbar(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let keymap = self.keymap.clone();
        let label = |a: crate::settings::Action| keymap.label(&ctx, a);
        use crate::settings::Action as A;
        let status = self.dbg.session.as_ref().map(|s| s.status);
        let stopped = status == Some(Status::Stopped);
        let running = status == Some(Status::Running);
        let active = matches!(status, Some(Status::Starting | Status::Running | Status::Stopped));
        if !active {
            if ui.button("Start Debugging").on_hover_text(label(A::Debug)).clicked() {
                self.cmd_debug(&ctx);
            }
        } else if stopped {
            if ui.button("Continue").on_hover_text(label(A::Debug)).clicked() {
                self.debug_step("continue");
            }
        } else if ui.add_enabled(running, egui::Button::new("Pause")).clicked() {
            self.debug_step("pause");
        }
        if ui.add_enabled(stopped, egui::Button::new("Step Over")).on_hover_text(label(A::StepOver)).clicked() {
            self.debug_step("next");
        }
        if ui.add_enabled(stopped, egui::Button::new("Step Into")).on_hover_text(label(A::StepInto)).clicked() {
            self.debug_step("stepIn");
        }
        if ui.add_enabled(stopped, egui::Button::new("Step Out")).on_hover_text(label(A::StepOut)).clicked() {
            self.debug_step("stepOut");
        }
        if ui.add_enabled(active, egui::Button::new("Restart")).clicked() {
            self.debug_restart(&ctx);
        }
        if ui.add_enabled(active, egui::Button::new(RichText::new("Stop").color(if active { self.theme.error } else { self.theme.comment }))).on_hover_text(label(A::StopDebug)).clicked() {
            self.cmd_stop_debugging(&ctx);
        }
        let text = match &self.dbg.session {
            None => String::new(),
            Some(s) => match s.status {
                Status::Starting => format!("Starting {}…", s.adapter_name),
                Status::Running => "Running".into(),
                Status::Stopped => {
                    let place = s.frames.get(s.frame).map(|f| format!(" in {}", f.name)).unwrap_or_default();
                    format!("Paused ({}){place}", s.stop_reason)
                }
                Status::Exited => match s.exit_code {
                    Some(c) => format!("Exited with code {c}"),
                    None => "Finished".into(),
                },
            },
        };
        ui.label(RichText::new(text).weak());
    }

    pub(super) fn debug_panel(&mut self, ui: &mut Ui) {
        let theme = self.theme.clone();
        let font = egui::FontId::monospace(12.5);
        let mut select = None;
        let mut expand: Vec<i64> = Vec::new();
        let mut remove_watch = None;
        let mut add_watch = None;
        let mut repl = None;
        let mut clicked_bp: Option<(PathBuf, usize)> = None;
        let mut bp_menu: Option<BpMenu> = None;
        let mut actions: Vec<VarAction> = Vec::new();
        let mut hex_changed = false;
        let mut mem_go: Option<String> = None;
        let mut mem_page: i64 = 0;
        let root = self.project_root();
        ui.columns(3, |cols| {
            cols[0].vertical(|ui| {
                // Call stack + breakpoints.
                ui.label(RichText::new("CALL STACK").small().weak());
                egui::ScrollArea::vertical().id_salt("dbg-stack").max_height(ui.available_height() * 0.6).auto_shrink([false, true]).show(ui, |ui| {
                    match &self.dbg.session {
                        Some(s) if s.status == Status::Stopped => {
                            for (i, f) in s.frames.iter().enumerate() {
                                let loc = f.path.as_ref().map(|p| format!("  {}:{}", p.file_name().unwrap_or_default().to_string_lossy(), f.line + 1)).unwrap_or_default();
                                let mut job = egui::text::LayoutJob::default();
                                let color = if f.path.as_ref().is_some_and(|p| p.is_file()) { theme.function } else { theme.comment };
                                job.append(&f.name, 0.0, egui::TextFormat::simple(font.clone(), color));
                                job.append(&loc, 0.0, egui::TextFormat::simple(font.clone(), theme.comment));
                                if ui.add(egui::Button::selectable(i == s.frame, job).frame_when_inactive(false)).clicked() {
                                    select = Some(i);
                                }
                            }
                        }
                        Some(s) if s.status == Status::Running => {
                            ui.label(RichText::new("Running…").weak());
                        }
                        _ => {
                            ui.label(RichText::new("Not paused").weak());
                        }
                    }
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new("BREAKPOINTS").small().weak());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if !self.dbg.breakpoints.is_empty() {
                            if ui.small_button("Remove All").clicked() {
                                bp_menu = Some(BpMenu::RemoveAll);
                            }
                            let mut muted = self.dbg.muted;
                            if ui.checkbox(&mut muted, RichText::new("Mute").small()).on_hover_text("Keep the breakpoints but don't stop at them").changed() {
                                bp_menu = Some(BpMenu::Mute(muted));
                            }
                        }
                    });
                });
                egui::ScrollArea::vertical().id_salt("dbg-bps").auto_shrink([false, false]).show(ui, |ui| {
                    if self.dbg.breakpoints.is_empty() {
                        ui.label(RichText::new("Click left of a line number to add one").weak());
                    }
                    for (path, lines) in &self.dbg.breakpoints {
                        let rel = root.as_ref().and_then(|r| path.strip_prefix(r).ok()).unwrap_or(path);
                        for &l in lines {
                            let extra = self.dbg.bp_extra.get(path).and_then(|m| m.get(&l));
                            let mut job = egui::text::LayoutJob::default();
                            let color = if self.dbg.muted { theme.comment } else { theme.text };
                            job.append(&format!("{}:{}", rel.display(), l + 1), 0.0, egui::TextFormat::simple(font.clone(), color));
                            if let Some((cond, log)) = extra {
                                if !cond.trim().is_empty() {
                                    job.append(&format!("  if {}", cond.trim()), 0.0, egui::TextFormat::simple(font.clone(), theme.warning));
                                }
                                if !log.trim().is_empty() {
                                    job.append(&format!("  log \"{}\"", log.trim()), 0.0, egui::TextFormat::simple(font.clone(), theme.function));
                                }
                            }
                            let r = ui.add(egui::Button::selectable(false, job).frame_when_inactive(false)).on_hover_text("Click to go there, right-click for more");
                            if r.clicked() {
                                clicked_bp = Some((path.clone(), l));
                            }
                            r.context_menu(|ui| {
                                if ui.button("Edit Condition / Log Message…").clicked() {
                                    bp_menu = Some(BpMenu::Edit(path.clone(), l));
                                    ui.close();
                                }
                                if ui.button("Remove").clicked() {
                                    bp_menu = Some(BpMenu::Remove(path.clone(), l));
                                    ui.close();
                                }
                            });
                        }
                    }
                });

            });
            cols[1].vertical(|ui| {
                // Variables + watches.
                ui.horizontal(|ui| {
                    ui.label(RichText::new("VARIABLES").small().weak());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.checkbox(&mut self.dbg.hex, RichText::new("Hex").small()).on_hover_text("Show numbers in hexadecimal").changed() {
                            hex_changed = true;
                        }
                    });
                });
                egui::ScrollArea::both().id_salt("dbg-vars").auto_shrink([false, false]).show(ui, |ui| {
                    let session = self.dbg.session.as_ref().filter(|s| s.status == Status::Stopped);
                    if !self.dbg.watches.is_empty() {
                        ui.label(RichText::new("WATCHES").small().weak());
                    }
                    for (i, w) in self.dbg.watches.iter().enumerate() {
                        let value = session.and_then(|s| s.watch_values.get(w));
                        let shown = session.zip(value).filter(|(_, wv)| wv.value.is_ok()).map(|(s, wv)| summary(s, &watch_var(w, wv), 0, false));
                        let changed = session.is_some_and(|s| matches!((&shown, s.prev.get(&format!("watch:{w}"))), (Some(v), Some(old)) if v != old));
                        let mut job = egui::text::LayoutJob::default();
                        job.append(&format!("{w} = "), 0.0, egui::TextFormat::simple(font.clone(), theme.variable));
                        let (v, c) = match value.map(|v| &v.value) {
                            Some(Ok(_)) => (shown.clone().unwrap_or_default(), if changed { theme.warning } else { theme.text }),
                            Some(Err(e)) => (e.clone(), theme.error),
                            None => ("…".into(), theme.comment),
                        };
                        job.append(&v, 0.0, egui::TextFormat::simple(font.clone(), c));
                        let reference = value.map(|v| v.reference).unwrap_or(0);
                        let mut header = |ui: &mut Ui, job: egui::text::LayoutJob| {
                            if ui.small_button("×").on_hover_text("Remove watch").clicked() {
                                remove_watch = Some(i);
                            }
                            ui.add(egui::Label::new(job).wrap_mode(egui::TextWrapMode::Extend)).context_menu(|ui| {
                                if ui.button("View memory").clicked() {
                                    actions.push(VarAction::Memory(w.clone()));
                                    ui.close();
                                }
                                if ui.button("Copy value").clicked() {
                                    actions.push(VarAction::Copy(v.clone()));
                                    ui.close();
                                }
                            });
                        };
                        if let (Some(s), true) = (session, reference > 0) {
                            let id = ui.make_persistent_id(("watch", w));
                            egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false)
                                .show_header(ui, |ui| header(ui, job))
                                .body(|ui| show_vars(ui, s, reference, &theme, &font, &mut expand, &mut actions, 0));
                        } else {
                            ui.horizontal(|ui| {
                                ui.add_space(18.0);
                                header(ui, job);
                            });
                        }
                    }
                    ui.horizontal(|ui| {
                        let r = ui
                            .add(egui::TextEdit::singleline(&mut self.dbg.new_watch).hint_text("Add watch (ptr, N shows N elements)").desired_width(f32::INFINITY))
                            .on_hover_text("Any C expression. Add \", N\" after a pointer to see N elements, e.g. \"buf, 16\"");
                        if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && !self.dbg.new_watch.trim().is_empty() {
                            add_watch = Some(self.dbg.new_watch.trim().to_string());
                            self.dbg.new_watch.clear();
                            r.request_focus();
                        }
                    });
                    ui.add_space(6.0);
                    if let Some(s) = session {
                        for sc in &s.scopes {
                            let id = ui.make_persistent_id(("scope", &sc.name));
                            egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, !sc.expensive)
                                .show_header(ui, |ui| {
                                    ui.label(RichText::new(&sc.name).strong());
                                })
                                .body(|ui| {
                                    if sc.expensive {
                                        expand.push(sc.reference);
                                    }
                                    show_vars(ui, s, sc.reference, &theme, &font, &mut expand, &mut actions, 0);
                                });
                        }
                    }
                });

            });
            cols[2].vertical(|ui| {
                ui.horizontal(|ui| {
                    for (tab, name) in [(RightTab::Console, "CONSOLE"), (RightTab::Memory, "MEMORY")] {
                        let text = RichText::new(name).small();
                        let text = if self.dbg.right_tab == tab { text.strong() } else { text.weak() };
                        if ui.add(egui::Button::selectable(self.dbg.right_tab == tab, text).frame_when_inactive(false)).clicked() {
                            self.dbg.right_tab = tab;
                        }
                    }
                });
                if self.dbg.right_tab == RightTab::Memory {
                    memory_view(ui, &mut self.dbg, &theme, &font, &mut mem_go, &mut mem_page);
                    return;
                }
                let input_h = 28.0;
                let h = (ui.available_height() - input_h).max(40.0);
                egui::ScrollArea::both().id_salt("dbg-console").max_height(h).auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
                    for (kind, line) in &self.dbg.console {
                        let color = match kind {
                            ConsoleKind::Out => theme.text,
                            ConsoleKind::Err => theme.error,
                            ConsoleKind::Info => theme.comment,
                            ConsoleKind::Input => theme.function,
                        };
                        ui.label(RichText::new(line.trim_end_matches('\n')).font(font.clone()).color(color));
                    }
                });
                let enabled = self.debug_stopped();
                let r = ui.add_enabled(
                    enabled,
                    egui::TextEdit::singleline(&mut self.dbg.console_input)
                        .font(font.clone())
                        .hint_text(if enabled { "Evaluate an expression or debugger command" } else { "Pause the program to evaluate" })
                        .desired_width(f32::INFINITY),
                );
                if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) && !self.dbg.console_input.trim().is_empty() {
                    repl = Some(std::mem::take(&mut self.dbg.console_input));
                    r.request_focus();
                }
            });
        });
        if let Some(i) = select {
            self.select_frame(i);
        }
        for r in expand {
            self.request_variables(r);
        }
        if hex_changed && self.debug_stopped() {
            let frame = self.dbg.session.as_ref().map(|s| s.frame).unwrap_or(0);
            self.select_frame(frame);
        }
        if let Some(e) = mem_go {
            self.open_memory(&e);
        }
        if mem_page != 0
            && let Some(mv) = &mut self.dbg.mem
        {
            mv.offset += mem_page * MEM_BYTES as i64;
            mv.prev = None;
            self.read_memory();
        }
        for a in actions {
            match a {
                VarAction::Watch(e) => add_watch = Some(e),
                VarAction::Array(e) => add_watch = Some(format!("{e}, 10")),
                VarAction::Memory(e) => self.open_memory(&e),
                VarAction::Copy(v) => ui.ctx().copy_text(v),
            }
        }
        if let Some(i) = remove_watch {
            self.dbg.watches.remove(i);
        }
        if let Some(w) = add_watch
            && !self.dbg.watches.contains(&w)
        {
            self.dbg.watches.push(w.clone());
            if self.debug_stopped() {
                let frame = self.current_frame_id();
                self.evaluate_watch(frame, &w);
            }
        }
        if let Some(e) = repl {
            self.dbg.console.push((ConsoleKind::Input, format!("> {e}\n")));
            self.evaluate(e.trim(), EvalKind::Repl, "repl");
        }
        match bp_menu {
            Some(BpMenu::Edit(p, l)) => self.edit_breakpoint_at(p, l),
            Some(BpMenu::Remove(p, l)) => self.toggle_breakpoint(&p, l),
            Some(BpMenu::RemoveAll) => self.cmd_clear_breakpoints(),
            Some(BpMenu::Mute(m)) => self.set_breakpoints_muted(m),
            None => {}
        }
        if let Some((path, line)) = clicked_bp {
            self.open_file(&path);
            if let Some(doc) = self.docs.get_mut(self.active)
                && doc.path.as_deref() == Some(path.as_path())
            {
                doc.goto_line_col(line, 0);
            }
        }
    }

    pub(super) fn debug_status_text(&self) -> Option<String> {
        let s = self.dbg.session.as_ref()?;
        Some(match s.status {
            Status::Starting => "Debugger starting".into(),
            Status::Running => "Debugging".into(),
            Status::Stopped => "Paused".into(),
            Status::Exited => return None,
        })
    }
}

#[allow(clippy::too_many_arguments)]
fn show_vars(
    ui: &mut Ui,
    s: &Session,
    reference: i64,
    theme: &crate::theme::Theme,
    font: &egui::FontId,
    expand: &mut Vec<i64>,
    actions: &mut Vec<VarAction>,
    depth: usize,
) {
    let Some(vars) = s.vars.get(&reference) else {
        expand.push(reference);
        ui.label(RichText::new("…").weak());
        return;
    };
    if vars.is_empty() {
        ui.label(RichText::new("(none)").weak());
    }
    let base = s.paths.get(&reference);
    for v in vars {
        // Values that changed since the last stop are highlighted.
        let shown = summary(s, v, 0, false);
        let changed = base.and_then(|b| s.prev.get(&format!("{b}/{}", v.name))).is_some_and(|old| *old != shown);
        let mut job = egui::text::LayoutJob::default();
        job.append(&v.name, 0.0, egui::TextFormat::simple(font.clone(), theme.variable));
        job.append(" = ", 0.0, egui::TextFormat::simple(font.clone(), theme.punct));
        job.append(&shown, 0.0, egui::TextFormat::simple(font.clone(), if changed { theme.warning } else { theme.text }));
        let label = |ui: &mut Ui, job: egui::text::LayoutJob, actions: &mut Vec<VarAction>| {
            let mut r = ui.add(egui::Label::new(job).wrap_mode(egui::TextWrapMode::Extend));
            if !v.ty.is_empty() {
                r = r.on_hover_text(&v.ty);
            }
            r.context_menu(|ui| var_menu(ui, v, actions));
        };
        if v.reference > 0 && depth < 12 {
            let id = ui.make_persistent_id(("var", base.cloned().unwrap_or_default(), &v.name));
            egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false)
                .show_header(ui, |ui| label(ui, job, actions))
                .body(|ui| show_vars(ui, s, v.reference, theme, font, expand, actions, depth + 1));
        } else {
            ui.horizontal(|ui| {
                ui.add_space(18.0);
                label(ui, job, actions);
            });
        }
    }
}

fn var_menu(ui: &mut Ui, v: &Var, actions: &mut Vec<VarAction>) {
    let name = v.evaluate_name.clone();
    let has_name = !name.is_empty();
    let is_pointer = is_pointer_type(&v.ty);
    if ui.add_enabled(has_name, egui::Button::new("Add to watches")).clicked() {
        actions.push(VarAction::Watch(name.clone()));
        ui.close();
    }
    if is_pointer && ui.add_enabled(has_name, egui::Button::new("Show as array (10 elements)")).on_hover_text("Adds the watch \"ptr, 10\"; edit the number to see more").clicked() {
        actions.push(VarAction::Array(name.clone()));
        ui.close();
    }
    if ui.add_enabled(has_name, egui::Button::new(if is_pointer { "View memory it points to" } else { "View memory" })).clicked() {
        actions.push(VarAction::Memory(if is_pointer { name.clone() } else { format!("&({name})") }));
        ui.close();
    }
    if ui.button("Copy value").clicked() {
        actions.push(VarAction::Copy(v.value.clone()));
        ui.close();
    }
}

fn is_pointer_type(ty: &str) -> bool {
    let t = ty.trim().trim_end_matches("const").trim_end();
    t.ends_with('*') && !t.contains('(')
}

/// The Memory tab: an address box and a hex dump.
fn memory_view(ui: &mut Ui, dbg: &mut DebugState, theme: &crate::theme::Theme, font: &egui::FontId, go: &mut Option<String>, page: &mut i64) {
    let stopped = dbg.session.as_ref().is_some_and(|s| s.status == Status::Stopped);
    ui.horizontal(|ui| {
        let have = dbg.mem.as_ref().is_some_and(|m| m.reference.is_some());
        if ui.add_enabled(stopped && have, egui::Button::new("<").small()).on_hover_text("Previous 256 bytes").clicked() {
            *page = -1;
        }
        if ui.add_enabled(stopped && have, egui::Button::new(">").small()).on_hover_text("Next 256 bytes").clicked() {
            *page = 1;
        }
        let r = ui.add_enabled(
            stopped,
            egui::TextEdit::singleline(&mut dbg.mem_input)
                .font(font.clone())
                .hint_text(if stopped { "Address or expression: buf, &x, 0x7ff…" } else { "Pause the program to view memory" })
                .desired_width(f32::INFINITY),
        );
        if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
            *go = Some(dbg.mem_input.clone());
        }
    });
    let Some(mv) = &dbg.mem else {
        ui.label(RichText::new("Type an address or expression, or right-click a variable and choose View memory").weak());
        return;
    };
    if let Some(e) = &mv.error {
        ui.label(RichText::new(e).color(theme.error));
        return;
    }
    let Some(addr) = mv.address else {
        ui.label(RichText::new(if stopped { "Reading…" } else { "Not paused" }).weak());
        return;
    };
    let prev = mv.prev.as_ref();
    let old_byte = |a: u64| prev.and_then(|(pa, d)| a.checked_sub(*pa).and_then(|i| d.get(i as usize).copied()));
    // 16 bytes per row when there's room, else 8.
    let char_w = ui.fonts_mut(|f| f.glyph_width(font, '0'));
    let per_row: usize = if ui.available_width() >= char_w * 84.0 { 16 } else { 8 };
    egui::ScrollArea::both().id_salt("dbg-mem").auto_shrink([false, false]).show(ui, |ui| {
        let total = mv.data.len() + mv.unreadable;
        for row in 0..total.div_ceil(per_row) {
            let mut job = egui::text::LayoutJob::default();
            let row_addr = addr + (row * per_row) as u64;
            job.append(&format!("{row_addr:012x}  "), 0.0, egui::TextFormat::simple(font.clone(), theme.comment));
            let mut ascii = String::new();
            for col in 0..per_row {
                let i = row * per_row + col;
                if col == 8 {
                    job.append(" ", 0.0, egui::TextFormat::simple(font.clone(), theme.text));
                }
                match mv.data.get(i) {
                    Some(&b) => {
                        let changed = old_byte(row_addr + col as u64).is_some_and(|o| o != b);
                        let color = if changed {
                            theme.warning
                        } else if b == 0 {
                            theme.comment
                        } else {
                            theme.text
                        };
                        job.append(&format!("{b:02x} "), 0.0, egui::TextFormat::simple(font.clone(), color));
                        ascii.push(if (0x20..0x7f).contains(&b) { b as char } else { '.' });
                    }
                    None if i < total => {
                        job.append("?? ", 0.0, egui::TextFormat::simple(font.clone(), theme.comment));
                        ascii.push(' ');
                    }
                    None => {
                        job.append("   ", 0.0, egui::TextFormat::simple(font.clone(), theme.text));
                    }
                }
            }
            job.append(&format!(" {ascii}"), 0.0, egui::TextFormat::simple(font.clone(), theme.string));
            ui.add(egui::Label::new(job).wrap_mode(egui::TextWrapMode::Extend));
        }
    });
}

/// `ptr, N` -> (`ptr`, N)
fn parse_array_watch(w: &str) -> Option<(String, usize)> {
    let (expr, n) = w.rsplit_once(',')?;
    let n: usize = n.trim().parse().ok()?;
    let expr = expr.trim();
    (n > 0 && n <= 100_000 && !expr.is_empty()).then(|| (expr.to_string(), n))
}

/// An expression showing `n` elements of what `expr` (of type `ty`) points to.
fn array_expression(expr: &str, ty: &str, n: usize) -> Option<String> {
    let t = ty.trim().trim_end_matches("const").trim_end();
    let elem = if let Some(base) = t.strip_suffix('*') {
        base.trim()
    } else if t.ends_with(']') {
        t[..t.rfind('[')?].trim()
    } else {
        return None;
    };
    if elem.is_empty() || elem.contains('(') {
        return None;
    }
    Some(format!("*({elem} (*)[{n}])({expr})"))
}

/// Bytes from `memory read -fx` (lldb) or `x/Nxb` (gdb) output:
/// "0x7ffd3c724c40: 0x68 0x65 ..." per line.
fn parse_memory_dump(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some((_, bytes)) = line.split_once(':') else { continue };
        for tok in bytes.split_whitespace() {
            match tok.strip_prefix("0x").and_then(|h| u8::from_str_radix(h, 16).ok()) {
                Some(b) if tok.len() == 4 => out.push(b),
                _ => break,
            }
        }
    }
    out
}

/// Where the memory view should look for an evaluated expression. A pointer
/// (its value prints as an address) shows what it points to; anything else
/// shows where it's stored. Apple's lldb-dap gives `memoryReference` as the
/// variable's own address even for pointers, so the printed value wins.
fn memory_address(result: &str, memory_reference: Option<&str>) -> Option<String> {
    let r = result.trim();
    if r.starts_with("0x") {
        return first_hex(r);
    }
    memory_reference.map(String::from).or_else(|| r.contains(" @ 0x").then(|| first_hex(r)).flatten())
}

fn first_hex(s: &str) -> Option<String> {
    let i = s.find("0x")?;
    let digits: String = s[i + 2..].chars().take_while(|c| c.is_ascii_hexdigit()).collect();
    (!digits.is_empty()).then(|| format!("0x{digits}"))
}

fn parse_address(s: &str) -> Option<u64> {
    let s = s.trim();
    match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => u64::from_str_radix(h, 16).ok(),
        None => s.parse().ok().filter(|_| s.len() > 4),
    }
}

fn base64_decode(s: &str) -> Vec<u8> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    };
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0);
    for v in s.bytes().filter_map(val) {
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    out
}

/// Debuggers show a struct or array as a placeholder ("Point @ 0x7ff…",
/// "{...}") with its fields as children.
fn is_aggregate(value: &str) -> bool {
    let v = value.trim();
    v.is_empty() || v.contains(" @ 0x") || v.starts_with('{')
}

/// A one-line value: structs as `{x = 1, y = 2}` and arrays as `{0, 1, 4}` (from
/// their loaded children). `clean` cuts C strings at the terminating NUL.
fn summary(s: &Session, v: &Var, depth: usize, clean: bool) -> String {
    let value = v.value.trim();
    if v.reference > 0 && is_aggregate(value) {
        let Some(fields) = s.vars.get(&v.reference) else { return if value.is_empty() { "{…}".into() } else { value.to_string() } };
        if depth > 0 {
            return "{…}".into();
        }
        // lldb names elements "[0]", gdb "0".
        let array = fields.first().is_some_and(|f| f.name.starts_with('[') || f.name.bytes().all(|b| b.is_ascii_digit()));
        let max = if array { 10 } else { 4 };
        let mut parts: Vec<String> = fields
            .iter()
            .take(max)
            .map(|f| {
                let fv = summary(s, f, depth + 1, clean);
                if array { fv } else { format!("{} = {fv}", f.name) }
            })
            .collect();
        if fields.len() > max {
            parts.push("…".into());
        }
        return format!("{{{}}}", parts.join(", "));
    }
    // lldb shows `char *` as `0x5555… "text"`; inside a struct or in the code, the text is enough.
    let value = if (clean || depth > 0) && value.starts_with("0x") {
        value.find(" \"").map(|i| &value[i + 1..]).unwrap_or(value)
    } else {
        value
    };
    if clean { clean_c_string(value) } else { value.to_string() }
}

fn watch_var(w: &str, wv: &WatchValue) -> Var {
    Var {
        name: w.to_string(),
        value: wv.value.clone().unwrap_or_default(),
        ty: String::new(),
        reference: wv.reference,
        evaluate_name: String::new(),
    }
}

/// `"hi\0\0garbage"` (lldb) or `"hi", '\000' <repeats 13 times>, ...` (gdb) -> `"hi"`.
fn clean_c_string(value: &str) -> String {
    if let Some(body) = value.strip_prefix('"')
        && let Some(i) = body.find("\", '\\000'").or_else(|| body.find("\\0"))
    {
        return format!("\"{}\"", &body[..i]);
    }
    value.to_string()
}

/// For each line from the start of the function to `exec_line`, the variables
/// it uses with their values: "i = 2   total = 5".
fn inline_value_lines(lines: &[&str], exec_line: usize, values: &HashMap<&str, String>) -> Vec<(usize, String)> {
    let mut start = exec_line.saturating_sub(300);
    for l in (start..=exec_line.min(lines.len().saturating_sub(1))).rev() {
        let t = lines[l];
        if t.starts_with('}') {
            start = l + 1;
            break;
        }
        let first = t.chars().next().unwrap_or(' ');
        if !first.is_whitespace() && first != '#' && first != '/' && first != '{' && t.contains('(') && !t.trim_end().ends_with(';') {
            start = l;
            break;
        }
    }
    let mut out = Vec::new();
    for (l, line) in lines.iter().enumerate().take(exec_line + 1).skip(start) {
        let code = line.split("//").next().unwrap_or("");
        let mut shown: Vec<&str> = Vec::new();
        let bytes = code.as_bytes();
        let (mut i, mut in_str) = (0, false);
        while i < bytes.len() {
            let c = bytes[i];
            if c == b'"' || c == b'\'' {
                in_str = !in_str;
                i += 1;
            } else if !in_str && (c.is_ascii_alphabetic() || c == b'_') && (i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_' || bytes[i - 1] == b'.' || bytes[i - 1] == b'>')) {
                let s0 = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                let w = &code[s0..i];
                if values.contains_key(w) && !shown.contains(&w) && !is_c_keyword(w) {
                    shown.push(w);
                }
            } else {
                i += 1;
            }
        }
        if shown.is_empty() {
            continue;
        }
        let parts: Vec<String> = shown
            .iter()
            .take(5)
            .map(|w| {
                let v: String = values[*w].split_whitespace().collect::<Vec<_>>().join(" ");
                let v = if v.chars().count() > 40 { format!("{}…", v.chars().take(39).collect::<String>()) } else { v };
                format!("{w} = {v}")
            })
            .collect();
        out.push((l, parts.join("   ")));
    }
    out
}

/// The C expression under the cursor: an identifier with any `a.b` / `p->x`
/// before it, e.g. hovering `x` in `p->pos.x` gives `p->pos.x`.
fn expression_at(text: &str, b: usize) -> String {
    let bytes = text.as_bytes();
    let is_id = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    if b >= bytes.len() || !is_id(bytes[b]) {
        return String::new();
    }
    let mut end = b;
    while end < bytes.len() && is_id(bytes[end]) {
        end += 1;
    }
    let mut start = b;
    loop {
        while start > 0 && is_id(bytes[start - 1]) {
            start -= 1;
        }
        if start >= 1 && bytes[start - 1] == b'.' && start >= 2 && is_id(bytes[start - 2]) {
            start -= 1;
        } else if start >= 2 && &bytes[start - 2..start] == b"->" && start >= 3 && is_id(bytes[start - 3]) {
            start -= 2;
        } else {
            break;
        }
    }
    let expr = &text[start..end];
    if expr.as_bytes()[0].is_ascii_digit() || is_c_keyword(expr) {
        return String::new();
    }
    expr.to_string()
}

fn is_c_keyword(w: &str) -> bool {
    matches!(
        w,
        "int" | "char" | "float" | "double" | "void" | "long" | "short" | "unsigned" | "signed" | "struct" | "union" | "enum"
            | "const" | "static" | "return" | "if" | "else" | "for" | "while" | "do" | "switch" | "case" | "break"
            | "continue" | "sizeof" | "typedef" | "extern" | "goto" | "default" | "volatile" | "inline" | "bool"
    )
}

/// lldb answers hover requests with "(int) v = 3" or "(int) x = 3" for `p.x`;
/// show "3  (int)" instead.
fn clean_value(result: &str) -> String {
    if let Some(rest) = result.strip_prefix('(')
        && let Some(close) = rest.find(") ")
    {
        let after = &rest[close + 2..];
        if let Some((name, value)) = after.split_once(" = ")
            && !name.contains(' ')
        {
            return format!("{value}  ({})", &rest[..close]);
        }
    }
    result.to_string()
}

/// `make -n run` and take the command it would run, e.g. `./demo --fast`.
fn dry_run_command(make_program: &str, dir: &Path) -> Vec<String> {
    let Ok(out) = std::process::Command::new(make_program)
        .args(["-n", "--no-print-directory", "run"])
        .current_dir(dir)
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let Some(line) = text.lines().map(str::trim).rfind(|l| !l.is_empty()) else { return Vec::new() };
    if line.contains("&&") || line.contains(';') || line.contains('|') {
        return Vec::new();
    }
    make::split_args(line.trim_start_matches('@'))
}

/// Resolve `./demo`, `bin/demo` or `demo` to an existing executable.
fn resolve_program(word: &str, dir: &Path) -> Option<PathBuf> {
    let p = Path::new(word);
    let candidates: Vec<PathBuf> = if p.is_absolute() {
        vec![p.to_path_buf()]
    } else {
        vec![dir.join(p)]
    };
    for c in candidates {
        if c.is_file() {
            return Some(c);
        }
        if cfg!(windows) {
            let exe = c.with_extension("exe");
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    if !word.contains('/') && !word.contains('\\') {
        return make::find_in_path(word);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expressions() {
        let t = "x = p->pos.x + arr[i].len;";
        assert_eq!(expression_at(t, 0), "x");
        assert_eq!(expression_at(t, 11), "p->pos.x");
        assert_eq!(expression_at(t, 7), "p->pos");
        assert_eq!(expression_at(t, 4), "p");
        assert_eq!(expression_at(t, 22), "len");
        assert_eq!(expression_at("return 0;", 1), "");
    }

    #[test]
    fn debug_helpers() {
        assert_eq!(clean_c_string(r#""hi\0\0\xf0""#), r#""hi""#);
        assert_eq!(clean_c_string(r#""hi", '\000' <repeats 13 times>, "\377""#), r#""hi""#);
        assert_eq!(clean_c_string("42"), "42");
        assert_eq!(parse_array_watch("buf, 16"), Some(("buf".into(), 16)));
        assert_eq!(parse_array_watch("f(a, b)"), None);
        assert_eq!(array_expression("p", "int *", 4).as_deref(), Some("*(int (*)[4])(p)"));
        assert_eq!(array_expression("s", "const char *const", 2).as_deref(), Some("*(const char (*)[2])(s)"));
        assert_eq!(array_expression("a", "float[8]", 3).as_deref(), Some("*(float (*)[3])(a)"));
        assert_eq!(array_expression("x", "int", 3), None);
        assert_eq!(base64_decode("aGVsbG8="), b"hello");
        assert_eq!(parse_memory_dump("(lldb) memory read\n0x10: 0x68 0x69 0x00\n0x13: 0xff\n"), vec![0x68, 0x69, 0, 0xff]);
        assert_eq!(parse_memory_dump("0x7ffe <buf>:\t0x41\t0x42"), vec![0x41, 0x42]);
        assert_eq!(first_hex("(char (*)[32]) 0x7ffd1234"), Some("0x7ffd1234".into()));
        assert_eq!(parse_address("0x10"), Some(16));
        assert_eq!(memory_address("0x0000000b62c009e0", Some("0x16FDFE5B8")).as_deref(), Some("0x0000000b62c009e0"));
        assert_eq!(memory_address("\"hello\\0\"", Some("0x16FDFE5C8")).as_deref(), Some("0x16FDFE5C8"));
        assert_eq!(memory_address("Point @ 0x16fdfe5b0", None).as_deref(), Some("0x16fdfe5b0"));
        assert_eq!(memory_address("140", None), None);
        let lines = ["int f(int n) {", "    int total = n;", "    // n here", "    total += n;"];
        let values: HashMap<&str, String> = [("n", "3".to_string()), ("total", "3".to_string())].into();
        let out = inline_value_lines(&lines, 3, &values);
        assert_eq!(out, vec![(0, "n = 3".to_string()), (1, "total = 3   n = 3".to_string()), (3, "total = 3   n = 3".to_string())]);
    }

    #[test]
    fn values() {
        assert_eq!(clean_value("(int) v = 3"), "3  (int)");
        assert_eq!(clean_value("(int) x = 3"), "3  (int)");
        assert_eq!(clean_value("3"), "3");
    }
}
