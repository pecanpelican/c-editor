//! Make integration: finding Makefiles, listing targets, running build
//! tasks and turning compiler output into clickable diagnostics.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub const MAKEFILE_NAMES: &[&str] = &["GNUmakefile", "makefile", "Makefile"];

/// Per-project build settings, remembered between runs.
#[derive(Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct BuildConfig {
    /// Target used by Build (empty = the Makefile's default target).
    pub target: String,
    /// Extra arguments passed to make, e.g. `DEBUG=1`.
    pub extra_args: String,
    /// Parallel jobs (`-j`); 0 = make's default.
    pub jobs: u32,
    /// Command used by Run when the Makefile has no `run` target.
    pub run_command: String,
    /// Override for the make program (empty = auto-detect).
    pub make_program: String,
}

/// Find the Makefile that applies to `start` (a file or directory): the
/// nearest one walking up, but not above `root`.
pub fn find_makefile(start: &Path, root: &Path) -> Option<PathBuf> {
    let mut dir = if start.is_dir() {
        Some(start)
    } else {
        start.parent()
    };
    while let Some(d) = dir {
        for name in MAKEFILE_NAMES {
            let p = d.join(name);
            if p.is_file() {
                return Some(p);
            }
        }
        if d == root || !d.starts_with(root) {
            break;
        }
        dir = d.parent();
    }
    None
}

/// Targets defined in a Makefile, in order. Pattern rules, special targets
/// (`.PHONY`) and computed names are skipped. Included makefiles are read
/// too (one level, relative paths only).
pub fn parse_targets(makefile: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(text) = std::fs::read_to_string(makefile) else {
        return out;
    };
    collect_targets(&text, &mut out);
    let dir = makefile.parent().unwrap_or(Path::new("."));
    for line in text.lines() {
        let l = line.trim_start();
        let rest = l
            .strip_prefix("include ")
            .or_else(|| l.strip_prefix("-include "))
            .or_else(|| l.strip_prefix("sinclude "));
        if let Some(rest) = rest {
            for f in rest.split_whitespace() {
                if f.contains('$') || f.contains('*') {
                    continue;
                }
                if let Ok(t) = std::fs::read_to_string(dir.join(f)) {
                    collect_targets(&t, &mut out);
                }
            }
        }
    }
    out
}

pub fn collect_targets(text: &str, out: &mut Vec<String>) {
    let mut continued = false;
    let mut in_define = false;
    for line in text.lines() {
        let was_continued = continued;
        continued = line.ends_with('\\');
        if was_continued || line.starts_with('\t') {
            continue;
        }
        let t = line.trim();
        if t.starts_with("define ") || t == "define" {
            in_define = true;
            continue;
        }
        if t.starts_with("endef") {
            in_define = false;
            continue;
        }
        if in_define || t.starts_with('#') {
            continue;
        }
        let Some(colon) = line.find(':') else { continue };
        let after = &line[colon + 1..];
        if after.starts_with('=') || line[..colon].contains('=') {
            continue; // := assignment, or a variable value with a colon
        }
        if after.starts_with(":=") {
            continue; // ::=
        }
        let head = &line[..colon];
        for name in head.split_whitespace() {
            let skip = name.starts_with('.')
                || name.contains(['%', '$', '('])
                || name.ends_with(".o")
                || name.ends_with(".d");
            if skip {
                continue;
            }
            if !out.iter().any(|t| t == name) {
                out.push(name.to_string());
            }
        }
    }
}

/// Locate the make program.
pub fn make_program(config: &BuildConfig) -> String {
    if !config.make_program.trim().is_empty() {
        return config.make_program.trim().to_string();
    }
    let candidates: &[&str] = if cfg!(windows) {
        &["make", "mingw32-make", "gmake"]
    } else if cfg!(any(target_os = "freebsd", target_os = "openbsd", target_os = "netbsd")) {
        &["gmake", "make"]
    } else {
        &["make"]
    };
    for c in candidates {
        if find_in_path(c).is_some() {
            return c.to_string();
        }
    }
    candidates[0].to_string()
}

pub fn find_in_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".into())
            .split(';')
            .map(|s| s.to_ascii_lowercase())
            .chain(std::iter::once(String::new()))
            .collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path) {
        for ext in &exts {
            let p = dir.join(format!("{program}{ext}"));
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// Split a command line into words, honouring single and double quotes.
pub fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut has = false;
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                has = true;
            }
            None if c.is_whitespace() => {
                if has || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            None => cur.push(c),
        }
    }
    if has || !cur.is_empty() {
        out.push(cur);
    }
    out
}

// ---------------------------------------------------------------------------
// Diagnostics

#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
    Note,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Diagnostic {
    pub path: PathBuf,
    /// 0-based
    pub line: usize,
    /// 0-based, in chars
    pub col: usize,
    pub severity: Severity,
    pub message: String,
}

fn severity_word(s: &str) -> Option<(Severity, usize)> {
    let lower = s.trim_start();
    let skipped = s.len() - lower.len();
    for (word, sev) in [
        ("fatal error", Severity::Error),
        ("error", Severity::Error),
        ("warning", Severity::Warning),
        ("note", Severity::Note),
    ] {
        if lower.len() >= word.len() && lower[..word.len()].eq_ignore_ascii_case(word) {
            let rest = &lower[word.len()..];
            // "error:" or "error C2065:" (MSVC)
            if let Some(p) = rest.find(':') {
                let between = &rest[..p];
                if between.trim().chars().all(|c| c.is_ascii_alphanumeric()) {
                    return Some((sev, skipped + word.len() + p + 1));
                }
            }
        }
    }
    None
}

fn resolve(path: &str, cwd: &Path) -> PathBuf {
    let path = path.trim();
    let b = path.as_bytes();
    let drive = b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/');
    let p = Path::new(path);
    if p.is_absolute() || drive { p.to_path_buf() } else { cwd.join(p) }
}

/// Parse one line of compiler output. Understands gcc/clang
/// (`file:line:col: error: msg`), MSVC (`file(line,col): error C1: msg`)
/// and glslang (`ERROR: file:line: msg`).
pub fn parse_diagnostic(line: &str, cwd: &Path) -> Option<Diagnostic> {
    let line = line.trim_end();
    // glslang / glslc style prefix.
    for (prefix, sev) in [("ERROR: ", Severity::Error), ("WARNING: ", Severity::Warning)] {
        if let Some(rest) = line.strip_prefix(prefix) {
            let (loc, msg) = split_location(rest)?;
            if loc.0.chars().all(|c| c.is_ascii_digit()) {
                return None; // "0:12" means string 0, no file name
            }
            return Some(Diagnostic {
                path: resolve(loc.0, cwd),
                line: loc.1.saturating_sub(1),
                col: loc.2.saturating_sub(1),
                severity: sev,
                message: msg.trim().trim_start_matches(':').trim().to_string(),
            });
        }
    }
    // MSVC: path(line[,col]): error C1234: message
    if let Some(open) = line.find("): ").and_then(|close| line[..close].rfind('(').map(|o| (o, close))) {
        let (o, c) = open;
        let nums = &line[o + 1..c];
        let mut it = nums.split(',');
        if let Some(l) = it.next().and_then(|s| s.trim().parse::<usize>().ok()) {
            let col = it.next().and_then(|s| s.trim().parse::<usize>().ok()).unwrap_or(1);
            if let Some((sev, used)) = severity_word(&line[c + 3..]) {
                return Some(Diagnostic {
                    path: resolve(&line[..o], cwd),
                    line: l.saturating_sub(1),
                    col: col.saturating_sub(1),
                    severity: sev,
                    message: line[c + 3 + used..].trim().to_string(),
                });
            }
        }
    }
    // gcc / clang
    let (loc, rest) = split_location(line)?;
    let (sev, used) = severity_word(rest.strip_prefix(':').unwrap_or(rest))?;
    let rest = rest.strip_prefix(':').unwrap_or(rest);
    Some(Diagnostic {
        path: resolve(loc.0, cwd),
        line: loc.1.saturating_sub(1),
        col: loc.2.saturating_sub(1),
        severity: sev,
        message: rest[used..].trim().to_string(),
    })
}

/// Split `path:line[:col]<rest>` into ((path, line, col), rest). Handles
/// Windows drive letters. Returns col 1 when absent. `rest` starts at the
/// character after the last number (usually ':').
fn split_location(s: &str) -> Option<((&str, usize, usize), &str)> {
    let b = s.as_bytes();
    // Skip a drive letter like C:\ or C:/
    let start = if b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/') {
        2
    } else {
        0
    };
    let mut i = start;
    while i < b.len() {
        if b[i] == b':' && i + 1 < b.len() && b[i + 1].is_ascii_digit() && i > start {
            let path = &s[..i];
            if path.contains(' ') && !path.contains('/') && !path.contains('\\') && !path.contains('.') {
                return None; // "In function 'x':", "make: *** ..." etc.
            }
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            let line: usize = s[i + 1..j].parse().ok()?;
            let mut col = 1;
            if j + 1 < b.len() && b[j] == b':' && b[j + 1].is_ascii_digit() {
                let mut k = j + 1;
                while k < b.len() && b[k].is_ascii_digit() {
                    k += 1;
                }
                col = s[j + 1..k].parse().ok()?;
                j = k;
            }
            if j < b.len() && b[j] != b':' {
                return None;
            }
            return Some(((path, line, col), &s[j..]));
        }
        i += 1;
    }
    None
}

/// `make: Entering directory '/x/y'` -> Some(Enter("/x/y"))
pub enum DirChange {
    Enter(PathBuf),
    Leave,
}

pub fn parse_dir_change(line: &str) -> Option<DirChange> {
    let idx = line.find("Entering directory ");
    if let Some(i) = idx {
        let rest = line[i + "Entering directory ".len()..].trim();
        let rest = rest.trim_matches(|c| c == '\'' || c == '`' || c == '"' || c == '\u{2018}' || c == '\u{2019}');
        return Some(DirChange::Enter(PathBuf::from(rest)));
    }
    if line.contains("Leaving directory ") {
        return Some(DirChange::Leave);
    }
    None
}

/// Remove ANSI escape sequences (colour codes) from a line.
pub fn strip_ansi(s: &str) -> String {
    if !s.contains('\x1b') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c2 in chars.by_ref() {
                    if c2.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                chars.next();
            }
        } else {
            out.push(c);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Running tasks

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TaskKind {
    Build,
    Run,
    Clean,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineKind {
    Command,
    Stdout,
    Stderr,
    Info,
}

pub struct OutputLine {
    pub text: String,
    pub kind: LineKind,
    /// Index into `Task::diagnostics` if this line is a diagnostic.
    pub diag: Option<usize>,
}

enum Msg {
    Line(String, bool),
    Exit(Option<i32>),
    SpawnError(String),
}

pub struct Task {
    pub kind: TaskKind,
    pub lines: Vec<OutputLine>,
    pub diagnostics: Vec<Diagnostic>,
    pub exit: Option<Option<i32>>,
    pub started: Instant,
    pub finished: Option<Duration>,
    pid: Option<u32>,
    rx: mpsc::Receiver<Msg>,
    dir_stack: Vec<PathBuf>,
    killed: bool,
}

pub const MAX_OUTPUT_LINES: usize = 50_000;

impl Task {
    /// Spawn `program args...` in `dir`. Output is collected on background
    /// threads; call `poll` every frame.
    pub fn spawn(kind: TaskKind, program: &str, args: &[String], dir: &Path, ctx: egui::Context) -> Self {
        let (tx, rx) = mpsc::channel();
        let shown = std::iter::once(program.to_string())
            .chain(args.iter().map(|a| if a.contains(' ') { format!("\"{a}\"") } else { a.clone() }))
            .collect::<Vec<_>>()
            .join(" ");
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(dir)
            // Compilers record $PWD (when it names the same folder) as the debug-info
            // directory. Keep it in the same form cedit uses, so debugger breakpoint
            // paths match even when the project is reached through a symlink.
            .env("PWD", dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Own process group, so Stop can kill make and everything it started.
            cmd.process_group(0);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut pid = None;
        match cmd.spawn() {
            Ok(mut child) => {
                pid = Some(child.id());
                let out = child.stdout.take();
                let err = child.stderr.take();
                let t1 = out.map(|o| spawn_reader(o, false, tx.clone(), ctx.clone()));
                let t2 = err.map(|e| spawn_reader(e, true, tx.clone(), ctx.clone()));
                let tx2 = tx.clone();
                let ctx2 = ctx.clone();
                std::thread::spawn(move || {
                    let status = child.wait().ok().and_then(|s| s.code());
                    if let Some(t) = t1 {
                        let _ = t.join();
                    }
                    if let Some(t) = t2 {
                        let _ = t.join();
                    }
                    let _ = tx2.send(Msg::Exit(status));
                    ctx2.request_repaint();
                });
            }
            Err(e) => {
                let msg = if e.kind() == std::io::ErrorKind::NotFound {
                    format!("Couldn't find `{program}`. Is it installed and on your PATH?")
                } else {
                    format!("Couldn't start `{program}`: {e}")
                };
                let _ = tx.send(Msg::SpawnError(msg));
            }
        }
        let mut task = Self {
            kind,
            lines: Vec::new(),
            diagnostics: Vec::new(),
            exit: None,
            started: Instant::now(),
            finished: None,
            pid,
            rx,
            dir_stack: vec![dir.to_path_buf()],
            killed: false,
        };
        task.lines.push(OutputLine {
            text: format!("$ {shown}"),
            kind: LineKind::Command,
            diag: None,
        });
        task
    }

    pub fn is_running(&self) -> bool {
        self.exit.is_none()
    }

    pub fn succeeded(&self) -> bool {
        matches!(self.exit, Some(Some(0)))
    }

    /// Pull new output. Returns true if anything changed.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.rx.try_recv() {
            changed = true;
            match msg {
                Msg::Line(text, is_err) => self.push_line(text, is_err),
                Msg::Exit(code) => {
                    self.exit = Some(code);
                    self.finished = Some(self.started.elapsed());
                    let secs = self.started.elapsed().as_secs_f32();
                    let text = if self.killed {
                        "Stopped.".to_string()
                    } else {
                        match code {
                            Some(0) => format!("Finished in {secs:.1}s."),
                            Some(c) => format!("Exited with code {c} after {secs:.1}s."),
                            None => format!("Terminated by a signal after {secs:.1}s."),
                        }
                    };
                    self.lines.push(OutputLine { text, kind: LineKind::Info, diag: None });
                }
                Msg::SpawnError(e) => {
                    self.lines.push(OutputLine { text: e, kind: LineKind::Stderr, diag: None });
                    self.exit = Some(None);
                    self.finished = Some(self.started.elapsed());
                }
            }
        }
        changed
    }

    fn push_line(&mut self, text: String, is_err: bool) {
        if self.lines.len() >= MAX_OUTPUT_LINES {
            if self.lines.len() == MAX_OUTPUT_LINES {
                self.lines.push(OutputLine {
                    text: "(output truncated)".into(),
                    kind: LineKind::Info,
                    diag: None,
                });
            }
            return;
        }
        let text = strip_ansi(&text);
        match parse_dir_change(&text) {
            Some(DirChange::Enter(d)) => self.dir_stack.push(d),
            Some(DirChange::Leave) => {
                if self.dir_stack.len() > 1 {
                    self.dir_stack.pop();
                }
            }
            None => {}
        }
        let cwd = self.dir_stack.last().cloned().unwrap_or_default();
        let diag = if self.kind != TaskKind::Run {
            parse_diagnostic(&text, &cwd).map(|d| {
                self.diagnostics.push(d);
                self.diagnostics.len() - 1
            })
        } else {
            None
        };
        self.lines.push(OutputLine {
            text,
            kind: if is_err { LineKind::Stderr } else { LineKind::Stdout },
            diag,
        });
    }

    pub fn stop(&mut self) {
        let Some(pid) = self.pid else { return };
        if !self.is_running() {
            return;
        }
        self.killed = true;
        kill_tree(pid);
    }

    pub fn count(&self, sev: Severity) -> usize {
        self.diagnostics.iter().filter(|d| d.severity == sev).count()
    }
}

impl Drop for Task {
    fn drop(&mut self) {
        self.stop();
    }
}

fn spawn_reader(
    stream: impl Read + Send + 'static,
    is_err: bool,
    tx: mpsc::Sender<Msg>,
    ctx: egui::Context,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut buf = Vec::new();
        let mut last_repaint = Instant::now();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let mut s = String::from_utf8_lossy(&buf).into_owned();
                    while s.ends_with('\n') || s.ends_with('\r') {
                        s.pop();
                    }
                    if tx.send(Msg::Line(s, is_err)).is_err() {
                        break;
                    }
                    if last_repaint.elapsed() > Duration::from_millis(30) {
                        ctx.request_repaint();
                        last_repaint = Instant::now();
                    }
                }
            }
        }
        ctx.request_repaint();
    })
}

/// Wrap a user command for the platform shell.
/// Kill a process and everything it started (its process group on Unix).
pub fn kill_tree(pid: u32) {
    #[cfg(unix)]
    {
        // Negative pid = the whole process group.
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(format!("-{pid}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .creation_flags(0x0800_0000)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

pub fn shell_command(cmd: &str) -> (String, Vec<String>) {
    if cfg!(windows) {
        ("cmd".into(), vec!["/C".into(), cmd.into()])
    } else {
        ("sh".into(), vec!["-c".into(), cmd.into()])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        let mk = "CC := cc\nVAR = a:b\n.PHONY: all clean\nall: app\napp: main.o util.o\n\t$(CC) -o $@ $^\n%.o: %.c\n\t$(CC) -c $<\nclean:\n\trm -f *.o\ninstall test: all\n$(OUT): x\ndefine X\nfake: y\nendef\n";
        let mut out = Vec::new();
        collect_targets(mk, &mut out);
        assert_eq!(out, vec!["all", "app", "clean", "install", "test"]);
    }

    #[test]
    fn gcc_diagnostics() {
        let cwd = Path::new("/proj");
        let d = parse_diagnostic("src/main.c:12:5: error: 'x' undeclared (first use in this function)", cwd).unwrap();
        assert_eq!(d.path, PathBuf::from("/proj/src/main.c"));
        assert_eq!((d.line, d.col), (11, 4));
        assert_eq!(d.severity, Severity::Error);
        assert_eq!(d.message, "'x' undeclared (first use in this function)");

        let d = parse_diagnostic("main.c:3: warning: implicit declaration", cwd).unwrap();
        assert_eq!((d.line, d.col, d.severity), (2, 0, Severity::Warning));

        let d = parse_diagnostic("/abs/x.h:1:2: fatal error: y.h: No such file or directory", cwd).unwrap();
        assert_eq!(d.path, PathBuf::from("/abs/x.h"));
        assert_eq!(d.message, "y.h: No such file or directory");

        let d = parse_diagnostic("src/a.c:7:10: note: declared here", cwd).unwrap();
        assert_eq!(d.severity, Severity::Note);

        assert!(parse_diagnostic("src/main.c: In function 'main':", cwd).is_none());
        assert!(parse_diagnostic("make: *** [Makefile:10: app] Error 1", cwd).is_none());
        assert!(parse_diagnostic("cc -Wall -c main.c -o main.o", cwd).is_none());
        assert!(parse_diagnostic("   12 |     int y = x;", cwd).is_none());
    }

    #[test]
    fn windows_and_glslang() {
        let cwd = Path::new("C:\\proj");
        let d = parse_diagnostic("C:\\proj\\main.c:4:1: error: expected ';'", cwd).unwrap();
        assert_eq!(d.path, PathBuf::from("C:\\proj\\main.c"));
        assert_eq!(d.line, 3);

        let d = parse_diagnostic("main.c(10,5): error C2065: 'x': undeclared identifier", cwd).unwrap();
        assert_eq!((d.line, d.col), (9, 4));
        assert_eq!(d.message, "'x': undeclared identifier");

        let cwd = Path::new("/p");
        let d = parse_diagnostic("ERROR: shaders/basic.frag:12: 'foo' : undeclared identifier", cwd).unwrap();
        assert_eq!(d.path, PathBuf::from("/p/shaders/basic.frag"));
        assert_eq!(d.line, 11);
        assert!(parse_diagnostic("ERROR: 0:12: 'foo' : undeclared identifier", cwd).is_none());
    }

    #[test]
    fn dirs_and_ansi() {
        assert!(matches!(parse_dir_change("make[1]: Entering directory '/a/b'"), Some(DirChange::Enter(p)) if p == Path::new("/a/b")));
        assert_eq!(strip_ansi("\x1b[01;31merror\x1b[0m: x"), "error: x");
        assert_eq!(split_args("CFLAGS='-O2 -g' DEBUG=1  \"a b\""), vec!["CFLAGS=-O2 -g", "DEBUG=1", "a b"]);
    }

    #[test]
    fn runs_a_command() {
        if cfg!(windows) {
            return;
        }
        let ctx = egui::Context::default();
        let (p, a) = shell_command("echo hello; echo 'x.c:1:2: error: boom' 1>&2; exit 3");
        let mut t = Task::spawn(TaskKind::Build, &p, &a, Path::new("/tmp"), ctx);
        let start = Instant::now();
        while t.is_running() && start.elapsed() < Duration::from_secs(5) {
            t.poll();
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(t.exit, Some(Some(3)));
        assert!(t.lines.iter().any(|l| l.text == "hello"));
        assert_eq!(t.count(Severity::Error), 1);
        assert_eq!(t.diagnostics[0].path, PathBuf::from("/tmp/x.c"));
    }
}
