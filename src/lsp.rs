//! A small Language Server Protocol client, used for clangd (C) and
//! glsl_analyzer (GLSL).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::make::{Diagnostic, Severity};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ServerKind {
    Clangd,
    GlslAnalyzer,
}

impl ServerKind {
    pub fn name(self) -> &'static str {
        match self {
            ServerKind::Clangd => "clangd",
            ServerKind::GlslAnalyzer => "glsl_analyzer",
        }
    }

    /// Which server handles a file, if any.
    pub fn for_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "c" | "h" | "inc" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "m" => Some(ServerKind::Clangd),
            "glsl" | "vert" | "frag" | "geom" | "comp" | "tesc" | "tese" | "mesh" | "task" | "rgen"
            | "rint" | "rahit" | "rchit" | "rmiss" | "rcall" | "vs" | "fs" | "gs" => Some(ServerKind::GlslAnalyzer),
            _ => None,
        }
    }

    fn language_id(path: &Path) -> &'static str {
        match path.extension().and_then(|e| e.to_str()).unwrap_or_default() {
            "c" | "h" | "inc" => "c",
            "cc" | "cpp" | "cxx" | "hpp" | "hh" => "cpp",
            "m" => "objective-c",
            _ => "glsl",
        }
    }

    pub fn install_hint(self) -> &'static str {
        match self {
            ServerKind::Clangd => {
                if cfg!(target_os = "macos") {
                    "Install it with `xcode-select --install` or `brew install llvm`."
                } else if cfg!(windows) {
                    "Install LLVM (`winget install LLVM.LLVM`) and make sure its bin folder is on PATH."
                } else {
                    "Install the `clangd` package from your distribution."
                }
            }
            ServerKind::GlslAnalyzer => {
                "Download glsl_analyzer from github.com/nolanderc/glsl_analyzer/releases and put it on your PATH."
            }
        }
    }
}

/// Locate a server binary, checking common install locations that are
/// often missing from PATH.
pub fn find_server(kind: ServerKind) -> Option<PathBuf> {
    let name = kind.name();
    if let Some(p) = crate::make::find_in_path(name) {
        return Some(p);
    }
    let mut extra: Vec<PathBuf> = Vec::new();
    if kind == ServerKind::Clangd {
        if cfg!(target_os = "macos") {
            extra.push("/opt/homebrew/opt/llvm/bin/clangd".into());
            extra.push("/usr/local/opt/llvm/bin/clangd".into());
            extra.push("/Library/Developer/CommandLineTools/usr/bin/clangd".into());
            extra.push("/Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/bin/clangd".into());
        } else if cfg!(windows) {
            extra.push("C:\\Program Files\\LLVM\\bin\\clangd.exe".into());
        } else {
            for v in (14..=22).rev() {
                extra.push(format!("/usr/lib/llvm-{v}/bin/clangd").into());
                extra.push(format!("/usr/bin/clangd-{v}").into());
            }
        }
    }
    extra.into_iter().find(|p| p.is_file())
}

pub fn path_to_uri(path: &Path) -> String {
    let s = path.to_string_lossy().replace('\\', "/");
    let mut out = String::from("file://");
    if !s.starts_with('/') {
        out.push('/'); // Windows: file:///C:/...
    }
    for b in s.bytes() {
        let keep = b.is_ascii_alphanumeric() || b"/-_.~:".contains(&b);
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let mut bytes = Vec::with_capacity(rest.len());
    let b = rest.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&rest[i + 1..i + 3], 16) {
                bytes.push(v);
                i += 3;
                continue;
            }
        }
        bytes.push(b[i]);
        i += 1;
    }
    let s = String::from_utf8_lossy(&bytes).into_owned();
    // Windows: /C:/x -> C:/x
    let s = if s.len() > 2 && s.as_bytes()[0] == b'/' && s.as_bytes()[2] == b':' {
        s[1..].to_string()
    } else {
        s
    };
    Some(PathBuf::from(s))
}

/// Byte offset -> LSP position (line, character) in the given encoding.
pub fn byte_to_position(text: &str, byte: usize, utf8: bool) -> (u32, u32) {
    let byte = crate::textops::floor_char_boundary(text, byte);
    let ls = crate::textops::line_start(text, byte);
    let line = text[..ls].bytes().filter(|&b| b == b'\n').count();
    let seg = &text[ls..byte];
    let ch = if utf8 { seg.len() } else { seg.encode_utf16().count() };
    (line as u32, ch as u32)
}

/// LSP position -> byte offset, clamped to the line.
pub fn position_to_byte(text: &str, line: u32, character: u32, utf8: bool) -> usize {
    let ls = crate::textops::line_to_byte(text, line as usize);
    let le = crate::textops::line_end(text, ls);
    let line_text = &text[ls..le];
    if utf8 {
        return ls + crate::textops::floor_char_boundary(line_text, (character as usize).min(line_text.len()));
    }
    let mut units = 0u32;
    for (i, c) in line_text.char_indices() {
        if units >= character {
            return ls + i;
        }
        units += c.len_utf16() as u32;
    }
    le
}

#[derive(Clone, Debug)]
pub struct Location {
    pub path: PathBuf,
    pub line: u32,
    pub character: u32,
}

#[derive(Clone, Debug)]
pub struct CompletionItem {
    pub label: String,
    pub detail: String,
    pub kind: u32,
    pub insert: String,
    /// Replace range from the server (line, char, line, char), if given.
    pub range: Option<(u32, u32, u32, u32)>,
    pub filter: String,
    pub sort: String,
}

/// A quick fix or refactoring offered by the server (Alt+Enter).
#[derive(Clone, Debug)]
pub struct CodeAction {
    pub title: String,
    pub preferred: bool,
    pub is_fix: bool,
    pub edit: Option<HashMap<PathBuf, Vec<TextEdit>>>,
    /// A command to run on the server instead, e.g. clangd's tweaks.
    pub command: Option<Value>,
}

/// A symbol from `workspace/symbol`.
#[derive(Clone, Debug)]
pub struct Symbol {
    pub name: String,
    pub kind: u32,
    pub container: String,
    pub location: Location,
}

/// Parameter hints for the call being typed.
#[derive(Clone, Debug, Default)]
pub struct Signature {
    pub label: String,
    /// Byte range of the current parameter in `label`.
    pub active: Option<(usize, usize)>,
    pub doc: String,
}

#[derive(Clone, Debug)]
pub struct TextEdit {
    pub start: (u32, u32),
    pub end: (u32, u32),
    pub new_text: String,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SemKind {
    Type,
    Function,
    Macro,
    Constant,
    Parameter,
    Variable,
    Other,
}

#[derive(Clone, Debug)]
pub struct SemToken {
    pub line: u32,
    pub start: u32,
    pub len: u32,
    pub kind: SemKind,
}

/// Something the app should react to.
pub enum Event {
    Diagnostics { path: PathBuf, diags: Vec<Diagnostic> },
    Completion { req: u64, items: Vec<CompletionItem>, utf8: bool },
    Definition { locations: Vec<Location>, utf8: bool },
    Hover { req: u64, text: String },
    References { locations: Vec<Location>, utf8: bool },
    Edits { edits: HashMap<PathBuf, Vec<TextEdit>>, utf8: bool, what: &'static str },
    SemanticTokens { path: PathBuf, version: u64, tokens: Vec<SemToken>, utf8: bool },
    SwitchSourceHeader(Option<PathBuf>),
    SignatureHelp { req: u64, sig: Option<Signature> },
    CodeActions { actions: Vec<CodeAction>, utf8: bool },
    Symbols { req: u64, symbols: Vec<Symbol>, utf8: bool },
    Status(String),
    Exited(ServerKind, String),
}

#[derive(Clone)]
enum Pending {
    Initialize,
    Completion(u64),
    Definition,
    Hover(u64),
    References,
    Rename,
    Formatting(PathBuf),
    SemanticTokens(PathBuf, u64),
    SwitchSourceHeader,
    SignatureHelp(u64),
    CodeActions,
    Symbols(u64),
    Other,
    Shutdown,
}

pub struct Server {
    pub kind: ServerKind,
    child: Child,
    stdin: Arc<Mutex<ChildStdin>>,
    rx: mpsc::Receiver<Value>,
    next_id: u64,
    pending: HashMap<u64, Pending>,
    pub ready: bool,
    pub utf8: bool,
    /// Messages queued until `initialize` completes.
    queue: Vec<Value>,
    /// path -> version last sent
    open: HashMap<PathBuf, u64>,
    sem_legend: Vec<SemKind>,
    /// Diagnostics as the server sent them, for asking about quick fixes.
    raw_diags: HashMap<PathBuf, Vec<Value>>,
    pub supports_semantic: bool,
    pub supports_formatting: bool,
    pub supports_rename: bool,
    pub supports_references: bool,
    exited: bool,
}

pub(crate) fn write_message(w: &mut impl Write, v: &Value) -> std::io::Result<()> {
    let body = serde_json::to_vec(v)?;
    write!(w, "Content-Length: {}\r\n\r\n", body.len())?;
    w.write_all(&body)?;
    w.flush()
}

pub(crate) fn read_message(r: &mut impl BufRead) -> Option<Value> {
    let mut len = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            len = v.trim().parse::<usize>().ok();
        }
    }
    let mut buf = vec![0u8; len?];
    r.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

impl Server {
    pub fn start(kind: ServerKind, binary: &Path, root: &Path, extra_args: &[String], ctx: egui::Context) -> std::io::Result<Self> {
        let mut cmd = Command::new(binary);
        cmd.args(extra_args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }
        let mut child = cmd.spawn()?;
        let stdin = Arc::new(Mutex::new(child.stdin.take().expect("piped")));
        let stdout = child.stdout.take().expect("piped");
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut r = BufReader::new(stdout);
            while let Some(v) = read_message(&mut r) {
                if tx.send(v).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
            ctx.request_repaint();
        });
        let mut s = Self {
            kind,
            child,
            stdin,
            rx,
            next_id: 1,
            pending: HashMap::new(),
            ready: false,
            utf8: false,
            queue: Vec::new(),
            open: HashMap::new(),
            sem_legend: Vec::new(),
            raw_diags: HashMap::new(),
            supports_semantic: false,
            supports_formatting: false,
            supports_rename: false,
            supports_references: false,
            exited: false,
        };
        let params = json!({
            "processId": std::process::id(),
            "rootUri": path_to_uri(root),
            "workspaceFolders": [{ "uri": path_to_uri(root), "name": root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default() }],
            "capabilities": {
                "general": { "positionEncodings": ["utf-8", "utf-16"] },
                "textDocument": {
                    "synchronization": { "didSave": true, "dynamicRegistration": false },
                    "completion": {
                        "completionItem": { "snippetSupport": false, "documentationFormat": ["plaintext"] },
                        "contextSupport": true
                    },
                    "hover": { "contentFormat": ["plaintext", "markdown"] },
                    "definition": { "linkSupport": false },
                    "declaration": { "linkSupport": false },
                    "signatureHelp": {
                        "signatureInformation": { "documentationFormat": ["plaintext"], "parameterInformation": { "labelOffsetSupport": true } }
                    },
                    "codeAction": {
                        "codeActionLiteralSupport": {
                            "codeActionKind": { "valueSet": ["quickfix", "refactor", "refactor.extract", "refactor.inline", "refactor.rewrite", "source"] }
                        }
                    },
                    "references": {},
                    "rename": { "prepareSupport": false },
                    "formatting": {},
                    "publishDiagnostics": { "relatedInformation": false },
                    "semanticTokens": {
                        "requests": { "full": true },
                        "tokenTypes": ["type", "class", "enum", "struct", "typeParameter", "function", "method", "macro", "enumMember", "parameter", "variable", "property", "namespace"],
                        "tokenModifiers": [],
                        "formats": ["relative"]
                    }
                },
                "workspace": { "workspaceFolders": true, "applyEdit": true, "symbol": {} }
            },
            "clientInfo": { "name": "cedit" }
        });
        let id = s.alloc(Pending::Initialize);
        s.send_now(&json!({ "jsonrpc": "2.0", "id": id, "method": "initialize", "params": params }));
        Ok(s)
    }

    fn alloc(&mut self, p: Pending) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.pending.insert(id, p);
        id
    }

    fn send_now(&mut self, v: &Value) {
        if let Ok(mut w) = self.stdin.lock()
            && write_message(&mut *w, v).is_err()
        {
            self.exited = true;
        }
    }

    fn send(&mut self, v: Value) {
        if self.ready {
            self.send_now(&v);
        } else {
            self.queue.push(v);
        }
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    fn request(&mut self, method: &str, params: Value, p: Pending) -> u64 {
        let id = self.alloc(p);
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        id
    }

    pub fn is_open(&self, path: &Path) -> bool {
        self.open.contains_key(path)
    }

    pub fn sent_version(&self, path: &Path) -> Option<u64> {
        self.open.get(path).copied()
    }

    /// Open or update a document (full text sync).
    pub fn sync(&mut self, path: &Path, text: &str, version: u64) {
        let uri = path_to_uri(path);
        match self.open.get(path) {
            None => {
                self.notify(
                    "textDocument/didOpen",
                    json!({ "textDocument": { "uri": uri, "languageId": ServerKind::language_id(path), "version": version, "text": text } }),
                );
            }
            Some(v) if *v == version => return,
            Some(_) => {
                self.notify(
                    "textDocument/didChange",
                    json!({ "textDocument": { "uri": uri, "version": version }, "contentChanges": [{ "text": text }] }),
                );
            }
        }
        self.open.insert(path.to_path_buf(), version);
    }

    pub fn did_save(&mut self, path: &Path) {
        if self.open.contains_key(path) {
            self.notify("textDocument/didSave", json!({ "textDocument": { "uri": path_to_uri(path) } }));
        }
    }

    pub fn close(&mut self, path: &Path) {
        if self.open.remove(path).is_some() {
            self.notify("textDocument/didClose", json!({ "textDocument": { "uri": path_to_uri(path) } }));
        }
    }

    fn pos(path: &Path, pos: (u32, u32)) -> Value {
        json!({ "textDocument": { "uri": path_to_uri(path) }, "position": { "line": pos.0, "character": pos.1 } })
    }

    pub fn completion(&mut self, path: &Path, pos: (u32, u32), trigger: Option<char>, req: u64) {
        let mut p = Self::pos(path, pos);
        p["context"] = match trigger {
            Some(c) => json!({ "triggerKind": 2, "triggerCharacter": c.to_string() }),
            None => json!({ "triggerKind": 1 }),
        };
        self.request("textDocument/completion", p, Pending::Completion(req));
    }

    pub fn definition(&mut self, path: &Path, pos: (u32, u32)) {
        self.request("textDocument/definition", Self::pos(path, pos), Pending::Definition);
    }

    pub fn declaration(&mut self, path: &Path, pos: (u32, u32)) {
        self.request("textDocument/declaration", Self::pos(path, pos), Pending::Definition);
    }

    pub fn signature_help(&mut self, path: &Path, pos: (u32, u32), req: u64) {
        self.request("textDocument/signatureHelp", Self::pos(path, pos), Pending::SignatureHelp(req));
    }

    /// Quick fixes and refactorings for the cursor, with the diagnostics on its line.
    pub fn code_actions(&mut self, path: &Path, start: (u32, u32), end: (u32, u32)) {
        let line = start.0 as u64;
        let diags: Vec<Value> = self
            .raw_diags
            .get(path)
            .map(|v| {
                v.iter()
                    .filter(|d| {
                        let a = d["range"]["start"]["line"].as_u64().unwrap_or(u64::MAX);
                        let b = d["range"]["end"]["line"].as_u64().unwrap_or(a);
                        a <= line && line <= b
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        self.request(
            "textDocument/codeAction",
            json!({
                "textDocument": { "uri": path_to_uri(path) },
                "range": { "start": { "line": start.0, "character": start.1 }, "end": { "line": end.0, "character": end.1 } },
                "context": { "diagnostics": diags }
            }),
            Pending::CodeActions,
        );
    }

    pub fn execute_command(&mut self, command: &Value) {
        let params = json!({ "command": command["command"], "arguments": command.get("arguments").cloned().unwrap_or(json!([])) });
        self.request("workspace/executeCommand", params, Pending::Other);
    }

    pub fn workspace_symbols(&mut self, query: &str, req: u64) {
        self.request("workspace/symbol", json!({ "query": query }), Pending::Symbols(req));
    }

    pub fn hover(&mut self, path: &Path, pos: (u32, u32), req: u64) {
        self.request("textDocument/hover", Self::pos(path, pos), Pending::Hover(req));
    }

    pub fn references(&mut self, path: &Path, pos: (u32, u32)) {
        let mut p = Self::pos(path, pos);
        p["context"] = json!({ "includeDeclaration": true });
        self.request("textDocument/references", p, Pending::References);
    }

    pub fn rename(&mut self, path: &Path, pos: (u32, u32), new_name: &str) {
        let mut p = Self::pos(path, pos);
        p["newName"] = json!(new_name);
        self.request("textDocument/rename", p, Pending::Rename);
    }

    pub fn format(&mut self, path: &Path, tab_size: u32, spaces: bool) {
        self.request(
            "textDocument/formatting",
            json!({ "textDocument": { "uri": path_to_uri(path) }, "options": { "tabSize": tab_size, "insertSpaces": spaces } }),
            Pending::Formatting(path.to_path_buf()),
        );
    }

    pub fn semantic_tokens(&mut self, path: &Path, version: u64) {
        if !self.supports_semantic {
            return;
        }
        self.request(
            "textDocument/semanticTokens/full",
            json!({ "textDocument": { "uri": path_to_uri(path) } }),
            Pending::SemanticTokens(path.to_path_buf(), version),
        );
    }

    pub fn switch_source_header(&mut self, path: &Path) {
        if self.kind == ServerKind::Clangd {
            self.request(
                "textDocument/switchSourceHeader",
                json!({ "uri": path_to_uri(path) }),
                Pending::SwitchSourceHeader,
            );
        }
    }

    pub fn shutdown(&mut self) {
        if self.ready && !self.exited {
            let id = self.alloc(Pending::Shutdown);
            self.send_now(&json!({ "jsonrpc": "2.0", "id": id, "method": "shutdown" }));
            self.send_now(&json!({ "jsonrpc": "2.0", "method": "exit" }));
        }
        let _ = self.child.kill();
    }

    /// Handle incoming messages, producing events.
    pub fn poll(&mut self, out: &mut Vec<Event>) {
        while let Ok(msg) = self.rx.try_recv() {
            self.handle(msg, out);
        }
        if !self.exited
            && let Ok(Some(status)) = self.child.try_wait()
        {
            self.exited = true;
            out.push(Event::Exited(self.kind, format!("{} stopped ({status})", self.kind.name())));
        }
    }

    pub fn has_exited(&self) -> bool {
        self.exited
    }

    fn handle(&mut self, msg: Value, out: &mut Vec<Event>) {
        // Server -> client request: answer with null so the server doesn't wait.
        if let (Some(id), Some(method)) = (msg.get("id"), msg.get("method").and_then(Value::as_str)) {
            let result = match method {
                "workspace/configuration" => {
                    let n = msg["params"]["items"].as_array().map(|a| a.len()).unwrap_or(0);
                    Value::Array(vec![Value::Null; n])
                }
                // The server applying a fix or refactoring (e.g. after executeCommand).
                "workspace/applyEdit" => {
                    out.push(Event::Edits { edits: parse_workspace_edit(&msg["params"]["edit"]), utf8: self.utf8, what: "Fix" });
                    json!({ "applied": true })
                }
                _ => Value::Null,
            };
            let reply = json!({ "jsonrpc": "2.0", "id": id.clone(), "result": result });
            self.send_now(&reply);
            return;
        }
        if let Some(method) = msg.get("method").and_then(Value::as_str) {
            match method {
                "textDocument/publishDiagnostics" => {
                    let params = &msg["params"];
                    if let Some(path) = params["uri"].as_str().and_then(uri_to_path) {
                        self.raw_diags.insert(path.clone(), params["diagnostics"].as_array().cloned().unwrap_or_default());
                        let diags = params["diagnostics"]
                            .as_array()
                            .map(|a| a.iter().filter_map(|d| self.to_diag(&path, d)).collect())
                            .unwrap_or_default();
                        out.push(Event::Diagnostics { path, diags });
                    }
                }
                "window/showMessage" => {
                    if let Some(m) = msg["params"]["message"].as_str() {
                        out.push(Event::Status(m.to_string()));
                    }
                }
                _ => {}
            }
            return;
        }
        let Some(id) = msg.get("id").and_then(Value::as_u64) else { return };
        let Some(p) = self.pending.remove(&id) else { return };
        let result = &msg["result"];
        if msg.get("error").is_some() {
            if let Pending::Rename = p {
                let m = msg["error"]["message"].as_str().unwrap_or("rename failed").to_string();
                out.push(Event::Status(m));
            }
            return;
        }
        match p {
            Pending::Initialize => {
                let caps = &result["capabilities"];
                self.utf8 = result["capabilities"]["positionEncoding"].as_str() == Some("utf-8")
                    || result["offsetEncoding"].as_str() == Some("utf-8");
                self.supports_formatting = !caps["documentFormattingProvider"].is_null() && caps["documentFormattingProvider"] != false;
                self.supports_rename = !caps["renameProvider"].is_null() && caps["renameProvider"] != false;
                self.supports_references = !caps["referencesProvider"].is_null() && caps["referencesProvider"] != false;
                if let Some(legend) = caps["semanticTokensProvider"]["legend"]["tokenTypes"].as_array() {
                    self.supports_semantic = !caps["semanticTokensProvider"]["full"].is_null();
                    self.sem_legend = legend
                        .iter()
                        .map(|t| match t.as_str().unwrap_or_default() {
                            "type" | "class" | "enum" | "struct" | "typeParameter" | "interface" => SemKind::Type,
                            "function" | "method" => SemKind::Function,
                            "macro" => SemKind::Macro,
                            "enumMember" => SemKind::Constant,
                            "parameter" => SemKind::Parameter,
                            "variable" | "property" => SemKind::Variable,
                            _ => SemKind::Other,
                        })
                        .collect();
                }
                self.send_now(&json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }));
                self.ready = true;
                for m in std::mem::take(&mut self.queue) {
                    self.send_now(&m);
                }
                out.push(Event::Status(format!("{} ready", self.kind.name())));
            }
            Pending::Completion(req) => {
                let items = result
                    .get("items")
                    .and_then(Value::as_array)
                    .or_else(|| result.as_array())
                    .map(|a| a.iter().filter_map(parse_completion).collect())
                    .unwrap_or_default();
                out.push(Event::Completion { req, items, utf8: self.utf8 });
            }
            Pending::Definition => out.push(Event::Definition { locations: parse_locations(result), utf8: self.utf8 }),
            Pending::References => out.push(Event::References { locations: parse_locations(result), utf8: self.utf8 }),
            Pending::Hover(req) => {
                let text = hover_text(&result["contents"]);
                out.push(Event::Hover { req, text });
            }
            Pending::Rename => out.push(Event::Edits { edits: parse_workspace_edit(result), utf8: self.utf8, what: "Rename" }),
            Pending::Formatting(path) => {
                let edits = result.as_array().map(|a| a.iter().filter_map(parse_text_edit).collect()).unwrap_or_default();
                let mut m = HashMap::new();
                m.insert(path, edits);
                out.push(Event::Edits { edits: m, utf8: self.utf8, what: "Format" });
            }
            Pending::SemanticTokens(path, version) => {
                let data: Vec<u32> = result["data"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|v| v.as_u64().map(|x| x as u32)).collect())
                    .unwrap_or_default();
                let mut tokens = Vec::with_capacity(data.len() / 5);
                let (mut line, mut start) = (0u32, 0u32);
                for c in data.chunks_exact(5) {
                    if c[0] > 0 {
                        line += c[0];
                        start = c[1];
                    } else {
                        start += c[1];
                    }
                    let kind = self.sem_legend.get(c[3] as usize).copied().unwrap_or(SemKind::Other);
                    tokens.push(SemToken { line, start, len: c[2], kind });
                }
                out.push(Event::SemanticTokens { path, version, tokens, utf8: self.utf8 });
            }
            Pending::SwitchSourceHeader => {
                out.push(Event::SwitchSourceHeader(result.as_str().and_then(uri_to_path)));
            }
            Pending::SignatureHelp(req) => out.push(Event::SignatureHelp { req, sig: parse_signature(result) }),
            Pending::CodeActions => {
                let actions = result.as_array().map(|a| a.iter().filter_map(parse_code_action).collect()).unwrap_or_default();
                out.push(Event::CodeActions { actions, utf8: self.utf8 });
            }
            Pending::Symbols(req) => {
                let symbols = result
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| {
                                let loc = parse_locations(&s["location"]).into_iter().next()?;
                                Some(Symbol {
                                    name: s["name"].as_str()?.to_string(),
                                    kind: s["kind"].as_u64().unwrap_or(0) as u32,
                                    container: s["containerName"].as_str().unwrap_or_default().to_string(),
                                    location: loc,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                out.push(Event::Symbols { req, symbols, utf8: self.utf8 });
            }
            Pending::Other => {}
            Pending::Shutdown => {}
        }
    }

    fn to_diag(&self, path: &Path, d: &Value) -> Option<Diagnostic> {
        let start = &d["range"]["start"];
        let severity = match d["severity"].as_u64().unwrap_or(1) {
            1 => Severity::Error,
            2 => Severity::Warning,
            _ => Severity::Note,
        };
        Some(Diagnostic {
            path: path.to_path_buf(),
            line: start["line"].as_u64()? as usize,
            // Converted to chars by the app, which has the text.
            col: start["character"].as_u64()? as usize,
            severity,
            message: d["message"].as_str().unwrap_or_default().to_string(),
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn parse_range(v: &Value) -> Option<(u32, u32, u32, u32)> {
    Some((
        v["start"]["line"].as_u64()? as u32,
        v["start"]["character"].as_u64()? as u32,
        v["end"]["line"].as_u64()? as u32,
        v["end"]["character"].as_u64()? as u32,
    ))
}

fn parse_completion(v: &Value) -> Option<CompletionItem> {
    let label = v["label"].as_str()?.trim().to_string();
    let (insert, range) = if let Some(te) = v.get("textEdit").filter(|t| t.is_object()) {
        let r = te.get("range").or_else(|| te.get("replace"));
        (te["newText"].as_str().unwrap_or(&label).to_string(), r.and_then(parse_range))
    } else {
        (v["insertText"].as_str().unwrap_or(&label).to_string(), None)
    };
    Some(CompletionItem {
        filter: v["filterText"].as_str().unwrap_or(&insert).trim().to_string(),
        sort: v["sortText"].as_str().unwrap_or(&label).to_string(),
        detail: v["detail"].as_str().unwrap_or_default().to_string(),
        kind: v["kind"].as_u64().unwrap_or(0) as u32,
        label,
        insert,
        range,
    })
}

fn parse_locations(v: &Value) -> Vec<Location> {
    let one = |l: &Value| -> Option<Location> {
        let uri = l["uri"].as_str().or_else(|| l["targetUri"].as_str())?;
        let range = if l.get("range").is_some() { &l["range"] } else { &l["targetSelectionRange"] };
        Some(Location {
            path: uri_to_path(uri)?,
            line: range["start"]["line"].as_u64()? as u32,
            character: range["start"]["character"].as_u64()? as u32,
        })
    };
    match v {
        Value::Array(a) => a.iter().filter_map(one).collect(),
        Value::Object(_) => one(v).into_iter().collect(),
        _ => Vec::new(),
    }
}

fn parse_text_edit(v: &Value) -> Option<TextEdit> {
    let r = parse_range(&v["range"])?;
    Some(TextEdit { start: (r.0, r.1), end: (r.2, r.3), new_text: v["newText"].as_str()?.to_string() })
}

fn parse_workspace_edit(v: &Value) -> HashMap<PathBuf, Vec<TextEdit>> {
    let mut out: HashMap<PathBuf, Vec<TextEdit>> = HashMap::new();
    if let Some(changes) = v["changes"].as_object() {
        for (uri, edits) in changes {
            if let Some(p) = uri_to_path(uri) {
                let e = edits.as_array().map(|a| a.iter().filter_map(parse_text_edit).collect()).unwrap_or_default();
                out.insert(p, e);
            }
        }
    }
    if let Some(dc) = v["documentChanges"].as_array() {
        for d in dc {
            if let Some(p) = d["textDocument"]["uri"].as_str().and_then(uri_to_path) {
                let e: Vec<TextEdit> = d["edits"].as_array().map(|a| a.iter().filter_map(parse_text_edit).collect()).unwrap_or_default();
                out.entry(p).or_default().extend(e);
            }
        }
    }
    out
}

fn parse_signature(v: &Value) -> Option<Signature> {
    let sigs = v["signatures"].as_array()?;
    let i = v["activeSignature"].as_u64().unwrap_or(0) as usize;
    let sig = sigs.get(i).or(sigs.first())?;
    let label = sig["label"].as_str()?.to_string();
    let p = sig["activeParameter"].as_u64().or_else(|| v["activeParameter"].as_u64()).unwrap_or(0) as usize;
    // Parameter labels are either [start, end] offsets (UTF-16) or substrings.
    let active = sig["parameters"].as_array().and_then(|ps| ps.get(p)).and_then(|param| match &param["label"] {
        Value::Array(a) if a.len() == 2 => {
            let to_byte = |u: u64| position_to_byte(&label, 0, u as u32, false);
            Some((to_byte(a[0].as_u64()?), to_byte(a[1].as_u64()?)))
        }
        Value::String(s) => label.find(s.as_str()).map(|b| (b, b + s.len())),
        _ => None,
    });
    let doc = match &sig["documentation"] {
        Value::String(s) => s.clone(),
        Value::Object(o) => o.get("value").and_then(Value::as_str).unwrap_or_default().to_string(),
        _ => String::new(),
    };
    Some(Signature { label, active, doc })
}

fn parse_code_action(v: &Value) -> Option<CodeAction> {
    let title = v["title"].as_str()?.to_string();
    // A bare Command has a string `command`; a CodeAction may carry an edit and/or a command object.
    let command = match &v["command"] {
        Value::String(_) => Some(v.clone()),
        c @ Value::Object(_) => Some(c.clone()),
        _ => None,
    };
    let edit = v.get("edit").filter(|e| e.is_object()).map(parse_workspace_edit);
    if edit.is_none() && command.is_none() {
        return None;
    }
    Some(CodeAction {
        title,
        preferred: v["isPreferred"].as_bool().unwrap_or(false),
        is_fix: v["kind"].as_str().is_some_and(|k| k.starts_with("quickfix")),
        edit,
        command,
    })
}

fn hover_text(contents: &Value) -> String {
    let s = match contents {
        Value::String(s) => s.clone(),
        Value::Object(o) => o.get("value").and_then(Value::as_str).unwrap_or_default().to_string(),
        Value::Array(a) => a.iter().map(hover_text).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    };
    // Drop markdown fences and horizontal rules for plain display.
    s.lines()
        .filter(|l| !l.trim_start().starts_with("```") && l.trim() != "---")
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// Apply LSP text edits to `text`. Edits are applied from the end so
/// earlier offsets stay valid.
pub fn apply_edits(text: &mut String, edits: &[TextEdit], utf8: bool) {
    apply_edits_keeping(text, edits, utf8, 0);
}

/// Applies the edits and returns where byte `keep` ends up, so the cursor
/// stays on the same code when lines are added above it.
pub fn apply_edits_keeping(text: &mut String, edits: &[TextEdit], utf8: bool, keep: usize) -> usize {
    let mut keep = keep.min(text.len());
    let mut ranges: Vec<(usize, usize, &str)> = edits
        .iter()
        .map(|e| {
            let s = position_to_byte(text, e.start.0, e.start.1, utf8);
            let t = position_to_byte(text, e.end.0, e.end.1, utf8);
            (s.min(t), t.max(s), e.new_text.as_str())
        })
        .collect();
    ranges.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
    for (s, e, t) in ranges {
        if e <= keep {
            keep = keep - (e - s) + t.len();
        } else if s < keep {
            keep = s + t.len();
        }
        text.replace_range(s..e, t);
    }
    keep
}

// ---------------------------------------------------------------------------
// compile_commands.json from a Makefile

const COMPILERS: &[&str] = &["cc", "gcc", "clang", "c99", "c11", "tcc", "icc", "icx", "clang-cl", "cl", "g++", "c++", "clang++"];

fn is_compiler(word: &str) -> bool {
    let base = Path::new(word)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(word)
        .trim_end_matches(".exe");
    if COMPILERS.contains(&base) {
        return true;
    }
    // Cross compilers and versioned names: arm-none-eabi-gcc, gcc-13, clang-18
    let stripped = base.trim_end_matches(|c: char| c.is_ascii_digit() || c == '.' || c == '-');
    COMPILERS.iter().any(|c| stripped.ends_with(&format!("-{c}")) || stripped == *c)
}

fn is_source(arg: &str) -> bool {
    let lower = arg.to_ascii_lowercase();
    [".c", ".cc", ".cpp", ".cxx", ".m", ".s"].iter().any(|e| lower.ends_with(e)) && !arg.starts_with('-')
}

/// Turn `make -n` output into compile_commands.json entries.
pub fn compile_commands_from_dry_run(output: &str, start_dir: &Path) -> Vec<Value> {
    let mut dirs = vec![start_dir.to_path_buf()];
    let mut out = Vec::new();
    let mut joined = String::new();
    for raw in output.lines() {
        // Join backslash-continued lines.
        if let Some(stripped) = raw.strip_suffix('\\') {
            joined.push_str(stripped);
            joined.push(' ');
            continue;
        }
        joined.push_str(raw);
        let line = std::mem::take(&mut joined);
        match crate::make::parse_dir_change(&line) {
            Some(crate::make::DirChange::Enter(d)) => {
                dirs.push(d);
                continue;
            }
            Some(crate::make::DirChange::Leave) => {
                if dirs.len() > 1 {
                    dirs.pop();
                }
                continue;
            }
            None => {}
        }
        let dir = dirs.last().cloned().unwrap_or_default();
        for part in line.split("&&").flat_map(|p| p.split(';')) {
            let args = crate::make::split_args(part.trim().trim_start_matches('@'));
            let Some(first) = args.first() else { continue };
            if !is_compiler(first) {
                continue;
            }
            for a in &args {
                if is_source(a) {
                    let file = if Path::new(a).is_absolute() { PathBuf::from(a) } else { dir.join(a) };
                    out.push(json!({
                        "directory": dir.to_string_lossy(),
                        "arguments": args,
                        "file": file.to_string_lossy(),
                    }));
                }
            }
        }
    }
    out
}

/// Where generated compile databases go (outside the user's project).
pub fn cache_dir_for(root: &Path) -> Option<PathBuf> {
    let base = if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Caches/cedit"))
    } else if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(|h| PathBuf::from(h).join("cedit\\cache"))
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
            .map(|p| p.join("cedit"))
    }?;
    let hash = crate::find::hash_text(&root.to_string_lossy());
    let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    Some(base.join(format!("{name}-{hash:016x}")))
}

/// Does the project already provide compile flags for clangd?
pub fn project_has_compile_db(root: &Path) -> bool {
    ["compile_commands.json", "compile_flags.txt", "build/compile_commands.json", ".clangd"]
        .iter()
        .any(|f| root.join(f).is_file())
}

/// Run `make -n -B -k -w` in `dir` and write compile_commands.json into
/// `out_dir`. Returns the number of entries.
pub fn generate_compile_commands(make: &str, dir: &Path, extra: &[String], out_dir: &Path) -> Result<usize, String> {
    let mut cmd = Command::new(make);
    cmd.args(["-n", "-B", "-k", "-w"]).args(extra).current_dir(dir).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let output = cmd.output().map_err(|e| format!("couldn't run {make}: {e}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    let entries = compile_commands_from_dry_run(&text, dir);
    if entries.is_empty() {
        return Err("no compiler commands found in `make -n` output".into());
    }
    std::fs::create_dir_all(out_dir).map_err(|e| e.to_string())?;
    let json = serde_json::to_string_pretty(&Value::Array(entries.clone())).map_err(|e| e.to_string())?;
    std::fs::write(out_dir.join("compile_commands.json"), json).map_err(|e| e.to_string())?;
    Ok(entries.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_keep_cursor() {
        let mut t = String::from("#include \"a.h\"\nint main() { printf(); }\n");
        let cur = t.find("printf").unwrap();
        let ed = vec![TextEdit { start: (1, 0), end: (1, 0), new_text: "#include <stdio.h>\n".into() }];
        let k = apply_edits_keeping(&mut t, &ed, true, cur);
        assert!(t[k..].starts_with("printf"));
        // An edit after the cursor leaves it alone.
        let ed = vec![TextEdit { start: (2, 0), end: (2, 3), new_text: "long".into() }];
        let k2 = apply_edits_keeping(&mut t, &ed, true, 3);
        assert_eq!(k2, 3);
        assert!(t.contains("long main"));
    }

    #[test]
    fn positions() {
        let t = "a\nhé😀llo\n";
        // 'l' after the emoji: bytes h(1)+é(2)+😀(4) = 7; utf16 units 1+1+2 = 4
        assert_eq!(byte_to_position(t, 2 + 7, true), (1, 7));
        assert_eq!(byte_to_position(t, 2 + 7, false), (1, 4));
        assert_eq!(position_to_byte(t, 1, 4, false), 9);
        assert_eq!(position_to_byte(t, 1, 7, true), 9);
        assert_eq!(position_to_byte(t, 1, 99, false), t.len() - 1);
    }

    #[test]
    fn uris() {
        let p = Path::new("/home/me/my project/a.c");
        let u = path_to_uri(p);
        assert_eq!(u, "file:///home/me/my%20project/a.c");
        assert_eq!(uri_to_path(&u).unwrap(), p);
        assert_eq!(uri_to_path("file:///C:/x/y.c").unwrap(), PathBuf::from("C:/x/y.c"));
    }

    #[test]
    fn edits() {
        let mut t = String::from("int  x=1;\nint y;");
        apply_edits(
            &mut t,
            &[
                TextEdit { start: (0, 3), end: (0, 5), new_text: " ".into() },
                TextEdit { start: (0, 6), end: (0, 7), new_text: " = ".into() },
            ],
            true,
        );
        assert_eq!(t, "int x = 1;\nint y;");
    }

    #[test]
    fn dry_run() {
        let out = "make: Entering directory '/p'\ncc -Wall -Isrc -c -o src/main.o src/main.c\necho done\narm-none-eabi-gcc -O2 \\\n  -c lib/x.c -o lib/x.o\ncc -o app src/main.o -lm\nmake[1]: Entering directory '/p/sub'\n@gcc-13 -DX=1 -c y.c && echo ok\n";
        let e = compile_commands_from_dry_run(out, Path::new("/start"));
        // Windows joins with a backslash.
        let files: Vec<String> = e.iter().map(|v| v["file"].as_str().unwrap().replace('\\', "/")).collect();
        assert_eq!(files, vec!["/p/src/main.c", "/p/lib/x.c", "/p/sub/y.c"]);
        assert_eq!(e[2]["directory"], "/p/sub");
        assert!(!is_compiler("echo"));
        assert!(is_compiler("/usr/bin/clang-18"));
    }
}
