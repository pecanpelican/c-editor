//! User settings (settings.json) and the keymap.

use std::collections::BTreeMap;
use std::path::PathBuf;

use egui::{Key, KeyboardShortcut, Modifiers};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// "dark" or "light"
    pub theme: String,
    pub font_size: f32,
    /// Path to a .ttf/.otf monospace font to use in the editor (empty = built-in).
    pub font_path: String,
    pub tab_size: usize,
    /// Run clang-format (through clangd) before saving C files.
    pub format_on_save: bool,
    pub trim_trailing_whitespace: bool,
    pub ensure_final_newline: bool,
    /// Override the clangd binary (empty = search PATH and common locations).
    pub clangd_path: String,
    /// Extra arguments for clangd, e.g. ["--clang-tidy"].
    pub clangd_args: Vec<String>,
    pub glsl_analyzer_path: String,
    /// Debugger to use: a path to lldb-dap or gdb (empty = find one).
    pub debugger_path: String,
    /// Action name -> shortcut, e.g. "build": "Cmd+B". "Cmd" means Ctrl on
    /// Windows and Linux. Only list the ones you want to change.
    pub keybindings: BTreeMap<String, String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: "dark".into(),
            font_size: 14.0,
            font_path: String::new(),
            tab_size: 4,
            format_on_save: false,
            trim_trailing_whitespace: true,
            ensure_final_newline: true,
            clangd_path: String::new(),
            clangd_args: Vec::new(),
            glsl_analyzer_path: String::new(),
            debugger_path: String::new(),
            keybindings: BTreeMap::new(),
        }
    }
}

pub fn config_dir() -> Option<PathBuf> {
    let base = if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join("Library/Application Support"))
    } else if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    }?;
    Some(base.join("cedit"))
}

pub fn settings_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("settings.json"))
}

impl Settings {
    /// Load settings, creating the file with defaults if it doesn't exist.
    /// Returns the settings and an error message if the file couldn't be parsed.
    pub fn load() -> (Self, Option<String>) {
        let Some(path) = settings_path() else {
            return (Self::default(), None);
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Settings>(&strip_comments(&text)) {
                Ok(s) => (s.sanitized(), None),
                Err(e) => (Self::default(), Some(format!("settings.json: {e}"))),
            },
            Err(_) => {
                let s = Self::default();
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = std::fs::write(&path, default_file_text());
                (s, None)
            }
        }
    }

    fn sanitized(mut self) -> Self {
        self.font_size = self.font_size.clamp(8.0, 40.0);
        self.tab_size = self.tab_size.clamp(1, 16);
        self
    }

    pub fn mtime() -> Option<std::time::SystemTime> {
        settings_path()
            .and_then(|p| std::fs::metadata(p).ok())
            .and_then(|m| m.modified().ok())
    }
}

/// Allow `//` line comments in settings.json (outside strings).
fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        let mut in_str = false;
        let mut escaped = false;
        let mut cut = line.len();
        let b = line.as_bytes();
        for i in 0..b.len() {
            let c = b[i];
            if in_str {
                if escaped {
                    escaped = false;
                } else if c == b'\\' {
                    escaped = true;
                } else if c == b'"' {
                    in_str = false;
                }
            } else if c == b'"' {
                in_str = true;
            } else if c == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
                cut = i;
                break;
            }
        }
        out.push_str(&line[..cut]);
        out.push('\n');
    }
    out
}

fn default_file_text() -> String {
    let mut s = String::from(
        "// cedit settings. Save this file to apply changes.\n\
         // Lines starting with // are comments.\n",
    );
    let mut d = Settings::default();
    d.keybindings.insert("build".into(), Action::Build.default_shortcut().into());
    let json = serde_json::to_string_pretty(&d).unwrap_or_default();
    s.push_str(&json);
    s.push_str("\n\n// Actions you can bind in \"keybindings\":\n");
    for a in Action::ALL {
        s.push_str(&format!("//   \"{}\": \"{}\"\n", a.id(), a.default_shortcut()));
    }
    s
}

// ---------------------------------------------------------------------------
// Keymap

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Action {
    Save,
    SaveAs,
    SaveAll,
    NewFile,
    OpenFile,
    OpenFolder,
    CloseTab,
    Find,
    Replace,
    FindInFiles,
    FindNext,
    FindPrev,
    GotoLine,
    GotoFile,
    ToggleSidebar,
    ToggleBottomPanel,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    NextTab,
    PrevTab,
    Build,
    Run,
    Stop,
    NextProblem,
    PrevProblem,
    GotoDefinition,
    GotoDeclaration,
    GotoSymbol,
    QuickFix,
    FindReferences,
    Rename,
    Format,
    SwitchHeader,
    Back,
    Complete,
    ToggleComment,
    DuplicateLine,
    DeleteLine,
    Settings,
    ToggleTerminal,
    NewTerminal,
    Debug,
    StopDebug,
    ToggleBreakpoint,
    StepOver,
    StepInto,
    StepOut,
    Profile,
}

impl Action {
    pub const ALL: &'static [Action] = &[
        Action::Save,
        Action::SaveAs,
        Action::SaveAll,
        Action::NewFile,
        Action::OpenFile,
        Action::OpenFolder,
        Action::CloseTab,
        Action::Find,
        Action::Replace,
        Action::FindInFiles,
        Action::FindNext,
        Action::FindPrev,
        Action::GotoLine,
        Action::GotoFile,
        Action::ToggleSidebar,
        Action::ToggleBottomPanel,
        Action::ZoomIn,
        Action::ZoomOut,
        Action::ZoomReset,
        Action::NextTab,
        Action::PrevTab,
        Action::Build,
        Action::Run,
        Action::Stop,
        Action::NextProblem,
        Action::PrevProblem,
        Action::GotoDefinition,
        Action::GotoDeclaration,
        Action::GotoSymbol,
        Action::QuickFix,
        Action::FindReferences,
        Action::Rename,
        Action::Format,
        Action::SwitchHeader,
        Action::Back,
        Action::Complete,
        Action::ToggleComment,
        Action::DuplicateLine,
        Action::DeleteLine,
        Action::Settings,
        Action::ToggleTerminal,
        Action::NewTerminal,
        Action::Debug,
        Action::StopDebug,
        Action::ToggleBreakpoint,
        Action::StepOver,
        Action::StepInto,
        Action::StepOut,
        Action::Profile,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Action::Save => "save",
            Action::SaveAs => "save_as",
            Action::SaveAll => "save_all",
            Action::NewFile => "new_file",
            Action::OpenFile => "open_file",
            Action::OpenFolder => "open_folder",
            Action::CloseTab => "close_tab",
            Action::Find => "find",
            Action::Replace => "replace",
            Action::FindInFiles => "find_in_files",
            Action::FindNext => "find_next",
            Action::FindPrev => "find_previous",
            Action::GotoLine => "go_to_line",
            Action::GotoFile => "go_to_file",
            Action::ToggleSidebar => "toggle_sidebar",
            Action::ToggleBottomPanel => "toggle_bottom_panel",
            Action::ZoomIn => "zoom_in",
            Action::ZoomOut => "zoom_out",
            Action::ZoomReset => "zoom_reset",
            Action::NextTab => "next_tab",
            Action::PrevTab => "previous_tab",
            Action::Build => "build",
            Action::Run => "run",
            Action::Stop => "stop",
            Action::NextProblem => "next_problem",
            Action::PrevProblem => "previous_problem",
            Action::GotoDefinition => "go_to_definition",
            Action::GotoDeclaration => "go_to_declaration",
            Action::GotoSymbol => "go_to_symbol",
            Action::QuickFix => "quick_fix",
            Action::FindReferences => "find_references",
            Action::Rename => "rename_symbol",
            Action::Format => "format_file",
            Action::SwitchHeader => "switch_header_source",
            Action::Back => "back",
            Action::Complete => "autocomplete",
            Action::ToggleComment => "toggle_comment",
            Action::DuplicateLine => "duplicate_line",
            Action::DeleteLine => "delete_line",
            Action::Settings => "settings",
            Action::ToggleTerminal => "toggle_terminal",
            Action::NewTerminal => "new_terminal",
            Action::Debug => "debug",
            Action::StopDebug => "stop_debugging",
            Action::ToggleBreakpoint => "toggle_breakpoint",
            Action::StepOver => "step_over",
            Action::StepInto => "step_into",
            Action::StepOut => "step_out",
            Action::Profile => "profile",
        }
    }

    pub fn default_shortcut(self) -> &'static str {
        let mac = cfg!(target_os = "macos");
        match self {
            Action::Save => "Cmd+S",
            Action::SaveAs => "Cmd+Shift+S",
            Action::SaveAll => "Cmd+Alt+S",
            Action::NewFile => "Cmd+N",
            Action::OpenFile => "Cmd+O",
            Action::OpenFolder => "Cmd+Shift+O",
            Action::CloseTab => "Cmd+W",
            Action::Find => "Cmd+F",
            Action::Replace => if mac { "Cmd+Alt+F" } else { "Ctrl+H" },
            Action::FindInFiles => "Cmd+Shift+F",
            Action::FindNext => "F3",
            Action::FindPrev => "Shift+F3",
            Action::GotoLine => if mac { "Cmd+L" } else { "Ctrl+G" },
            Action::GotoFile => "Cmd+P",
            Action::ToggleSidebar => "Cmd+1",
            Action::ToggleBottomPanel => "Cmd+J",
            Action::ZoomIn => "Cmd+=",
            Action::ZoomOut => "Cmd+-",
            Action::ZoomReset => "Cmd+0",
            Action::NextTab => "Ctrl+Tab",
            Action::PrevTab => "Ctrl+Shift+Tab",
            Action::Build => "Cmd+B",
            Action::Run => "Cmd+R",
            Action::Stop => "Cmd+.",
            Action::NextProblem => "F8",
            Action::PrevProblem => "Shift+F8",
            Action::GotoDefinition => "F12",
            Action::GotoDeclaration => "Ctrl+F12",
            Action::GotoSymbol => "Cmd+T",
            Action::QuickFix => "Alt+Enter",
            Action::FindReferences => "Shift+F12",
            Action::Rename => "F2",
            Action::Format => "Cmd+Alt+L",
            Action::SwitchHeader => "Alt+O",
            Action::Back => if mac { "Cmd+[" } else { "Alt+Left" },
            Action::Complete => "Ctrl+Space",
            Action::ToggleComment => "Cmd+/",
            Action::DuplicateLine => "Cmd+D",
            Action::DeleteLine => "Cmd+Shift+K",
            Action::Settings => "Cmd+,",
            Action::ToggleTerminal => "Ctrl+`",
            Action::NewTerminal => "Ctrl+Shift+`",
            Action::Debug => "F5",
            Action::StopDebug => "Shift+F5",
            Action::ToggleBreakpoint => "F9",
            Action::StepOver => "F10",
            Action::StepInto => "F11",
            Action::StepOut => "Shift+F11",
            Action::Profile => "Cmd+Shift+R",
        }
    }

    /// Handled inside the editor widget (only when it has focus).
    pub fn is_editor_action(self) -> bool {
        matches!(self, Action::ToggleComment | Action::DuplicateLine | Action::DeleteLine)
    }
}

/// Whether a focused terminal should get this key combination instead of the app.
pub fn terminal_takes(sc: &KeyboardShortcut) -> bool {
    let (m, k) = (sc.modifiers, sc.logical_key);
    let name = k.name();
    let fkey = name.len() > 1 && name.starts_with('F') && name[1..].chars().all(|c| c.is_ascii_digit());
    if fkey || matches!(k, Key::Backtick | Key::Tab) {
        return false;
    }
    if cfg!(target_os = "macos") {
        // Cmd shortcuts stay with the app; Ctrl and Option go to the shell.
        return !(m.command || m.mac_cmd);
    }
    if m.ctrl || m.command {
        // Ctrl+letter is the shell's (Ctrl+C, Ctrl+R...); Ctrl+Shift+X and
        // Ctrl+digit/punctuation stay with the app.
        let shell_key = (name.len() == 1 && name.as_bytes()[0].is_ascii_alphabetic())
            || matches!(k, Key::Space | Key::OpenBracket | Key::CloseBracket | Key::Backslash);
        return !m.shift && shell_key;
    }
    true
}

/// Parse "Cmd+Shift+B", "Ctrl+Space", "F12", "Alt+Left", "Cmd+/".
pub fn parse_shortcut(s: &str) -> Option<KeyboardShortcut> {
    let mut mods = Modifiers::NONE;
    let parts: Vec<&str> = s.split('+').map(str::trim).collect();
    // "Cmd++" -> last part empty means the key is '+'
    let (key_part, mod_parts) = if s.ends_with("++") {
        ("+", &parts[..parts.len().saturating_sub(2)])
    } else {
        (*parts.last()?, &parts[..parts.len() - 1])
    };
    for m in mod_parts {
        match m.to_ascii_lowercase().as_str() {
            "cmd" | "command" | "super" | "meta" => mods = mods | Modifiers::COMMAND,
            "ctrl" | "control" => mods = mods | Modifiers::CTRL,
            "shift" => mods = mods | Modifiers::SHIFT,
            "alt" | "option" | "opt" => mods = mods | Modifiers::ALT,
            _ => return None,
        }
    }
    let key = match key_part.to_ascii_lowercase().as_str() {
        "space" => Key::Space,
        "tab" => Key::Tab,
        "enter" | "return" => Key::Enter,
        "esc" | "escape" => Key::Escape,
        "left" => Key::ArrowLeft,
        "right" => Key::ArrowRight,
        "up" => Key::ArrowUp,
        "down" => Key::ArrowDown,
        "backspace" => Key::Backspace,
        "delete" => Key::Delete,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "/" => Key::Slash,
        "\\" => Key::Backslash,
        "." => Key::Period,
        "," => Key::Comma,
        ";" => Key::Semicolon,
        "[" => Key::OpenBracket,
        "]" => Key::CloseBracket,
        "=" => Key::Equals,
        "-" => Key::Minus,
        "+" => Key::Plus,
        "`" => Key::Backtick,
        other => Key::from_name(&other.to_uppercase()).or_else(|| Key::from_name(other))?,
    };
    Some(KeyboardShortcut::new(mods, key))
}

#[derive(Clone)]
pub struct Keymap {
    bindings: Vec<(Action, KeyboardShortcut)>,
}

impl Keymap {
    /// Build the keymap; returns unknown actions/shortcuts as warnings.
    pub fn new(overrides: &BTreeMap<String, String>) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let mut bindings = Vec::new();
        for &a in Action::ALL {
            let s = overrides.get(a.id()).map(String::as_str).unwrap_or(a.default_shortcut());
            if s.is_empty() || s.eq_ignore_ascii_case("none") {
                continue;
            }
            match parse_shortcut(s) {
                Some(sc) => bindings.push((a, sc)),
                None => {
                    warnings.push(format!("Unknown shortcut \"{s}\" for {}", a.id()));
                    if let Some(sc) = parse_shortcut(a.default_shortcut()) {
                        bindings.push((a, sc));
                    }
                }
            }
        }
        for k in overrides.keys() {
            if !Action::ALL.iter().any(|a| a.id() == k) {
                warnings.push(format!("Unknown action \"{k}\" in keybindings"));
            }
        }
        // Most specific first, so Cmd+Shift+S isn't taken by Cmd+S.
        bindings.sort_by_key(|(_, sc)| {
            let m = sc.modifiers;
            std::cmp::Reverse(m.shift as u8 + m.alt as u8 + m.ctrl as u8 + m.command as u8 + m.mac_cmd as u8)
        });
        (Self { bindings }, warnings)
    }

    pub fn shortcut(&self, a: Action) -> Option<KeyboardShortcut> {
        self.bindings.iter().find(|(x, _)| *x == a).map(|(_, s)| *s)
    }

    pub fn label(&self, ctx: &egui::Context, a: Action) -> String {
        self.shortcut(a).map(|s| ctx.format_shortcut(&s)).unwrap_or_default()
    }

    /// Consume the first matching shortcut this frame (app-level actions).
    /// While the terminal has focus, keys it needs (Ctrl+letter, Alt+key...)
    /// are left for it.
    pub fn pressed(&self, ctx: &egui::Context, editor_actions: bool, terminal_focused: bool) -> Vec<Action> {
        let mut out = Vec::new();
        ctx.input_mut(|i| {
            for (a, sc) in &self.bindings {
                if a.is_editor_action() != editor_actions || (terminal_focused && terminal_takes(sc)) {
                    continue;
                }
                while i.consume_shortcut(sc) {
                    out.push(*a);
                }
            }
        });
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse() {
        assert_eq!(parse_shortcut("Cmd+Shift+B"), Some(KeyboardShortcut::new(Modifiers::COMMAND | Modifiers::SHIFT, Key::B)));
        assert_eq!(parse_shortcut("F12"), Some(KeyboardShortcut::new(Modifiers::NONE, Key::F12)));
        assert_eq!(parse_shortcut("Ctrl+Space"), Some(KeyboardShortcut::new(Modifiers::CTRL, Key::Space)));
        assert_eq!(parse_shortcut("Cmd+/"), Some(KeyboardShortcut::new(Modifiers::COMMAND, Key::Slash)));
        assert_eq!(parse_shortcut("Cmd+="), Some(KeyboardShortcut::new(Modifiers::COMMAND, Key::Equals)));
        assert_eq!(parse_shortcut("Alt+Left"), Some(KeyboardShortcut::new(Modifiers::ALT, Key::ArrowLeft)));
        assert_eq!(parse_shortcut("Cmd+1"), Some(KeyboardShortcut::new(Modifiers::COMMAND, Key::Num1)));
        assert!(parse_shortcut("Hyper+X").is_none());
        for a in Action::ALL {
            assert!(parse_shortcut(a.default_shortcut()).is_some(), "{:?}", a);
        }
    }

    #[test]
    fn defaults_file_parses() {
        let text = default_file_text();
        let s: Settings = serde_json::from_str(&strip_comments(&text)).unwrap();
        assert_eq!(s.tab_size, 4);
        let s: Settings = serde_json::from_str(&strip_comments("{ \"theme\": \"light\", // c\n \"x\": 1 }")).unwrap();
        assert_eq!(s.theme, "light");
        assert_eq!(s.font_size, 14.0);
    }
}
