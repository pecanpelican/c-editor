//! Debug Adapter Protocol client, and finding a debugger to talk to:
//! lldb-dap (macOS, Linux, Windows) or gdb 14+ (`gdb -i dap`).

use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};

use serde_json::{Value, json};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum AdapterKind {
    Lldb,
    Gdb,
}

#[derive(Clone, Debug)]
pub struct Adapter {
    pub kind: AdapterKind,
    pub program: PathBuf,
    pub args: Vec<String>,
}

impl Adapter {
    pub fn name(&self) -> &'static str {
        match self.kind {
            AdapterKind::Lldb => "lldb",
            AdapterKind::Gdb => "gdb",
        }
    }

    fn new(program: PathBuf) -> Self {
        let file = program.file_name().map(|f| f.to_string_lossy().to_lowercase()).unwrap_or_default();
        if file.contains("gdb") {
            Adapter { kind: AdapterKind::Gdb, program, args: vec!["-q".into(), "-i".into(), "dap".into()] }
        } else {
            Adapter { kind: AdapterKind::Lldb, program, args: Vec::new() }
        }
    }

    /// The project's own debugger setup file, for custom data formatters
    /// (lldb `type summary`/`synthetic` or gdb pretty-printers): `.lldbinit` or
    /// `.gdbinit` in the project folder.
    pub fn init_file(&self, root: &Path) -> Option<PathBuf> {
        let name = match self.kind {
            AdapterKind::Lldb => ".lldbinit",
            AdapterKind::Gdb => ".gdbinit",
        };
        Some(root.join(name)).filter(|p| p.is_file())
    }

    /// Load `init` (see `init_file`) when the debugger starts. gdb takes it as a
    /// command-line file; lldb gets it in the launch request.
    pub fn with_init_file(mut self, init: Option<&Path>) -> Self {
        if let (AdapterKind::Gdb, Some(f)) = (self.kind, init) {
            // We load it ourselves; stop gdb warning that it won't auto-load it.
            self.args.splice(0..0, ["-iex".to_string(), "set auto-load local-gdbinit off".to_string()]);
            self.args.push("-x".into());
            self.args.push(f.to_string_lossy().into_owned());
        }
        self
    }

    /// Arguments for the `launch` request.
    pub fn launch_args(&self, program: &Path, args: &[String], cwd: &Path, init: Option<&Path>) -> Value {
        let init_commands: Vec<String> = match (self.kind, init) {
            (AdapterKind::Lldb, Some(f)) => vec![format!("command source -s 1 '{}'", f.display())],
            _ => Vec::new(),
        };
        match self.kind {
            AdapterKind::Lldb => json!({
                "initCommands": init_commands,
                "program": program,
                "args": args,
                "cwd": cwd,
                "stopOnEntry": false,
                // Run the program in a terminal tab so it can read input. Linux only:
                // lldb-dap doesn't support it on Windows, and on macOS the launcher is
                // Apple's own lldb-dap binary, which System Integrity Protection won't let
                // the debugger attach to ("Failed to attach to the target process").
                "runInTerminal": cfg!(target_os = "linux"),
            }),
            AdapterKind::Gdb => json!({
                "program": program,
                "args": args,
                "cwd": cwd,
            }),
        }
    }
}

/// Find a debugger. `custom` is the `debugger_path` setting (empty = search).
pub fn find_adapter(custom: &str) -> Result<Adapter, String> {
    let custom = custom.trim();
    if !custom.is_empty() {
        let p = PathBuf::from(custom);
        let p = if p.is_file() { Some(p) } else { crate::make::find_in_path(custom) };
        return p.map(Adapter::new).ok_or_else(|| format!("debugger_path \"{custom}\" wasn't found"));
    }
    if cfg!(target_os = "macos")
        && let Some(p) = xcrun_find("lldb-dap")
    {
        return Ok(Adapter::new(p));
    }
    for name in ["lldb-dap", "lldb-vscode"] {
        if let Some(p) = crate::make::find_in_path(name) {
            return Ok(Adapter::new(p));
        }
    }
    let mut fixed: Vec<PathBuf> = Vec::new();
    if cfg!(target_os = "macos") {
        fixed.push("/Library/Developer/CommandLineTools/usr/bin/lldb-dap".into());
        fixed.push("/opt/homebrew/opt/llvm/bin/lldb-dap".into());
        fixed.push("/usr/local/opt/llvm/bin/lldb-dap".into());
    }
    if cfg!(windows) {
        fixed.push(r"C:\Program Files\LLVM\bin\lldb-dap.exe".into());
    }
    if let Some(p) = fixed.into_iter().find(|p| p.is_file()) {
        return Ok(Adapter::new(p));
    }
    // Versioned names from distro packages, e.g. lldb-dap-18.
    if let Some(p) = find_versioned("lldb-dap-") {
        return Ok(Adapter::new(p));
    }
    if let Some(p) = crate::make::find_in_path("gdb")
        && gdb_has_dap(&p)
    {
        return Ok(Adapter::new(p));
    }
    Err(install_hint().to_string())
}

pub fn install_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "No debugger found. Install Xcode (it includes lldb-dap) or run: brew install llvm"
    } else if cfg!(windows) {
        "No debugger found. Install LLVM (winget install LLVM.LLVM), which includes lldb-dap"
    } else {
        "No debugger found. Install gdb 14 or newer, or lldb (it includes lldb-dap)"
    }
}

fn xcrun_find(tool: &str) -> Option<PathBuf> {
    let out = Command::new("xcrun").args(["-f", tool]).stderr(Stdio::null()).output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !s.is_empty() && Path::new(&s).is_file()).then(|| PathBuf::from(s))
}

fn find_versioned(prefix: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let mut best: Option<(u32, PathBuf)> = None;
    for dir in std::env::split_paths(&path) {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(v) = name.strip_prefix(prefix).and_then(|v| v.parse::<u32>().ok())
                && best.as_ref().is_none_or(|(b, _)| v > *b)
            {
                best = Some((v, e.path()));
            }
        }
    }
    best.map(|(_, p)| p)
}

/// gdb speaks DAP from version 14.
fn gdb_has_dap(gdb: &Path) -> bool {
    let Ok(out) = Command::new(gdb).arg("--version").stderr(Stdio::null()).output() else { return false };
    let first = String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or_default().to_string();
    gdb_version(&first).is_some_and(|v| v >= 14)
}

fn gdb_version(line: &str) -> Option<u32> {
    // "GNU gdb (Ubuntu 15.1-1ubuntu1~24.04.1) 15.1" -> 15
    let last = line.split_whitespace().last()?;
    last.split('.').next()?.parse().ok()
}

/// A running debug adapter.
pub struct Client {
    child: Child,
    stdin: ChildStdin,
    rx: Receiver<Value>,
    seq: i64,
}

impl Client {
    pub fn start(adapter: &Adapter, cwd: &Path, ctx: &egui::Context) -> std::io::Result<Self> {
        let mut cmd = Command::new(&adapter.program);
        cmd.args(&adapter.args)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let (tx, rx) = mpsc::channel();
        {
            let tx = tx.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                let mut r = BufReader::new(stdout);
                let log = std::env::var_os("CEDIT_DAP_LOG").is_some();
                while let Some(msg) = crate::lsp::read_message(&mut r) {
                    if log {
                        eprintln!("dap << {msg}");
                    }
                    if tx.send(msg).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
                let _ = tx.send(json!({"type": "cedit_exit"}));
                ctx.request_repaint();
            });
        }
        {
            let ctx = ctx.clone();
            std::thread::spawn(move || {
                use std::io::BufRead;
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let _ = tx.send(json!({"type": "cedit_stderr", "text": line}));
                    ctx.request_repaint();
                }
            });
        }
        Ok(Self { child, stdin, rx, seq: 0 })
    }

    /// Send a request; returns its sequence number.
    pub fn request(&mut self, command: &str, arguments: Value) -> i64 {
        self.seq += 1;
        let msg = json!({"seq": self.seq, "type": "request", "command": command, "arguments": arguments});
        if std::env::var_os("CEDIT_DAP_LOG").is_some() {
            eprintln!("dap >> {msg}");
        }
        let _ = crate::lsp::write_message(&mut self.stdin, &msg);
        let _ = self.stdin.flush();
        self.seq
    }

    /// Answer a request from the adapter (e.g. runInTerminal).
    pub fn respond(&mut self, request_seq: i64, command: &str, success: bool, body: Value) {
        self.seq += 1;
        let msg = json!({
            "seq": self.seq, "type": "response", "request_seq": request_seq,
            "command": command, "success": success, "body": body,
        });
        let _ = crate::lsp::write_message(&mut self.stdin, &msg);
    }

    pub fn poll(&mut self) -> Vec<Value> {
        self.rx.try_iter().collect()
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(gdb_version("GNU gdb (Ubuntu 15.1-1ubuntu1~24.04.1) 15.1"), Some(15));
        assert_eq!(gdb_version("GNU gdb (GDB) 14.2"), Some(14));
        assert_eq!(gdb_version("GNU gdb (GDB) Fedora 12.1-2.fc36"), Some(12));
        assert_eq!(Adapter::new("/usr/bin/gdb".into()).kind, AdapterKind::Gdb);
        assert_eq!(Adapter::new("/usr/bin/lldb-dap-18".into()).kind, AdapterKind::Lldb);
    }
}
