//! User settings (settings.json) and the keymap.

use std::collections::BTreeMap;
use std::path::PathBuf;

use egui::{Key, KeyboardShortcut, Modifiers};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    /// "system", "dark", "light", "midnight", "high-contrast" or "sepia"
    pub theme: String,
    pub font_size: f32,
    /// Path to a .ttf/.otf monospace font to use in the editor (empty = built-in).
    pub font_path: String,
    pub tab_size: usize,
    /// Run clang-format (through clangd) before saving C files.
    pub format_on_save: bool,
    pub trim_trailing_whitespace: bool,
    pub ensure_final_newline: bool,
    /// Type the closing bracket or quote when typing an opening one.
    pub auto_close_brackets: bool,
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
            auto_close_brackets: true,
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

/// Change one top-level key in settings.json and keep everything else in
/// the file as it is, comments included. Creates the file if needed.
pub fn write_setting(key: &str, value: &serde_json::Value) -> Result<(), String> {
    let path = settings_path().ok_or("Couldn't find a place for settings.json")?;
    let text = std::fs::read_to_string(&path).unwrap_or_else(|_| default_file_text());
    let new = set_key_in_text(&text, key, value)?;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(&path, new).map_err(|e| format!("Couldn't save settings.json: {e}"))
}

/// The text of a settings file with `key` set to `value` (see `write_setting`).
fn set_key_in_text(text: &str, key: &str, value: &serde_json::Value) -> Result<String, String> {
    let b = text.as_bytes();
    let json = serde_json::to_string(value).map_err(|e| e.to_string())?;
    // Skip a string starting at i (a quote); returns the index after it.
    let skip_str = |mut i: usize| -> usize {
        i += 1;
        while i < b.len() && b[i] != b'"' {
            if b[i] == b'\\' {
                i += 1;
            }
            i += 1;
        }
        (i + 1).min(b.len())
    };
    let skip_ws = |mut i: usize| -> usize {
        loop {
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if b.get(i) == Some(&b'/') && b.get(i + 1) == Some(&b'/') {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            } else {
                return i;
            }
        }
    };
    let mut depth = 0i32;
    let mut first_brace = None;
    let mut i = 0;
    let mut found = None;
    while i < b.len() {
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'"' => {
                let end = skip_str(i);
                if depth == 1 && text.get(i + 1..end.saturating_sub(1)) == Some(key) {
                    let colon = skip_ws(end);
                    if b.get(colon) == Some(&b':') {
                        found = Some(skip_ws(colon + 1));
                        break;
                    }
                }
                i = end;
                continue;
            }
            b'{' | b'[' => {
                if depth == 0 && first_brace.is_none() {
                    first_brace = Some(i);
                }
                depth += 1;
            }
            b'}' | b']' => depth -= 1,
            _ => {}
        }
        i += 1;
    }
    let out = if let Some(start) = found {
        // Find where the existing value ends.
        let mut j = start;
        let mut d = 0i32;
        while j < b.len() {
            match b[j] {
                b'"' => {
                    j = skip_str(j);
                    if d == 0 {
                        break;
                    }
                    continue;
                }
                b'{' | b'[' => d += 1,
                b'}' | b']' => {
                    if d == 0 {
                        break;
                    }
                    d -= 1;
                    if d == 0 {
                        j += 1;
                        break;
                    }
                }
                b',' | b'\n' if d == 0 => break,
                b'/' if d == 0 && b.get(j + 1) == Some(&b'/') => break,
                _ => {}
            }
            j += 1;
        }
        let end = start + text[start..j].trim_end().len();
        format!("{}{}{}", &text[..start], json, &text[end..])
    } else {
        let brace = first_brace.ok_or("settings.json has no { … } object")?;
        let rest = &text[brace + 1..];
        let empty = rest.trim_start().starts_with('}');
        let comma = if empty { "" } else { "," };
        format!("{}\n  \"{key}\": {json}{comma}{}", &text[..=brace], rest)
    };
    serde_json::from_str::<Settings>(&strip_comments(&out))
        .map(|_| out)
        .map_err(|e| format!("Couldn't update settings.json: {e}"))
}

/// The settings.json spelling of a shortcut, e.g. "Cmd+Shift+K" (parsed back
/// by `parse_shortcut`). Returns None for keys that can't be bound.
pub fn shortcut_string(mods: Modifiers, key: Key) -> Option<String> {
    let mut s = String::new();
    if cfg!(target_os = "macos") {
        if mods.ctrl {
            s.push_str("Ctrl+");
        }
        if mods.mac_cmd || mods.command {
            s.push_str("Cmd+");
        }
    } else if mods.ctrl || mods.command {
        s.push_str("Ctrl+");
    }
    if mods.alt {
        s.push_str("Alt+");
    }
    if mods.shift {
        s.push_str("Shift+");
    }
    let k = match key {
        Key::ArrowLeft => "Left".to_string(),
        Key::ArrowRight => "Right".to_string(),
        Key::ArrowUp => "Up".to_string(),
        Key::ArrowDown => "Down".to_string(),
        Key::Slash => "/".into(),
        Key::Backslash => "\\".into(),
        Key::Period => ".".into(),
        Key::Comma => ",".into(),
        Key::Semicolon => ";".into(),
        Key::OpenBracket => "[".into(),
        Key::CloseBracket => "]".into(),
        Key::Equals => "=".into(),
        Key::Minus => "-".into(),
        Key::Plus => "+".into(),
        Key::Backtick => "`".into(),
        Key::Escape => return None,
        other => other.name().to_string(),
    };
    s.push_str(&k);
    let sc = parse_shortcut(&s)?;
    (sc.logical_key == key).then_some(s)
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
    s.push_str("\n\n// Themes: system, dark, light, midnight, high-contrast, sepia\n");
    s.push_str("// Actions you can bind in \"keybindings\":\n");
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
    CloseAllTabs,
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
    RefactorThis,
    ExtractVariable,
    ExtractFunction,
    InlineVariable,
    ChangeSignature,
    SafeDelete,
    CallHierarchy,
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
    CommandPalette,
    RecentFiles,
    MoveLineUp,
    MoveLineDown,
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
        Action::CloseAllTabs,
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
        Action::RefactorThis,
        Action::ExtractVariable,
        Action::ExtractFunction,
        Action::InlineVariable,
        Action::ChangeSignature,
        Action::SafeDelete,
        Action::CallHierarchy,
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
        Action::CommandPalette,
        Action::RecentFiles,
        Action::MoveLineUp,
        Action::MoveLineDown,
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
            Action::CloseAllTabs => "close_all_tabs",
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
            Action::RefactorThis => "refactor_this",
            Action::ExtractVariable => "extract_variable",
            Action::ExtractFunction => "extract_function",
            Action::InlineVariable => "inline_variable",
            Action::ChangeSignature => "change_signature",
            Action::SafeDelete => "safe_delete",
            Action::CallHierarchy => "call_hierarchy",
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
            Action::CommandPalette => "command_palette",
            Action::RecentFiles => "recent_files",
            Action::MoveLineUp => "move_line_up",
            Action::MoveLineDown => "move_line_down",
        }
    }

    /// Human-readable name, for menus, the command palette and the shortcut list.
    pub fn title(self) -> &'static str {
        match self {
            Action::Save => "Save",
            Action::SaveAs => "Save As…",
            Action::SaveAll => "Save All",
            Action::NewFile => "New File",
            Action::OpenFile => "Open File…",
            Action::OpenFolder => "Open Folder…",
            Action::CloseTab => "Close Tab",
            Action::CloseAllTabs => "Close All Tabs",
            Action::Find => "Find…",
            Action::Replace => "Replace…",
            Action::FindInFiles => "Find in Files…",
            Action::FindNext => "Find Next",
            Action::FindPrev => "Find Previous",
            Action::GotoLine => "Go to Line…",
            Action::GotoFile => "Go to File…",
            Action::ToggleSidebar => "Toggle Sidebar",
            Action::ToggleBottomPanel => "Toggle Bottom Panel",
            Action::ZoomIn => "Zoom In",
            Action::ZoomOut => "Zoom Out",
            Action::ZoomReset => "Reset Zoom",
            Action::NextTab => "Next Tab",
            Action::PrevTab => "Previous Tab",
            Action::Build => "Build",
            Action::Run => "Run",
            Action::Stop => "Stop Build / Program",
            Action::NextProblem => "Next Problem",
            Action::PrevProblem => "Previous Problem",
            Action::GotoDefinition => "Go to Definition",
            Action::GotoDeclaration => "Go to Declaration",
            Action::GotoSymbol => "Go to Symbol…",
            Action::QuickFix => "Quick Fix / Refactor…",
            Action::FindReferences => "Find References",
            Action::Rename => "Rename Symbol…",
            Action::RefactorThis => "Refactor This…",
            Action::ExtractVariable => "Extract Variable…",
            Action::ExtractFunction => "Extract Function…",
            Action::InlineVariable => "Inline Variable",
            Action::ChangeSignature => "Change Signature…",
            Action::SafeDelete => "Safe Delete…",
            Action::CallHierarchy => "Call Hierarchy",
            Action::Format => "Format File",
            Action::SwitchHeader => "Switch Header/Source",
            Action::Back => "Navigate Back",
            Action::Complete => "Autocomplete",
            Action::ToggleComment => "Toggle Comment",
            Action::DuplicateLine => "Duplicate Line",
            Action::DeleteLine => "Delete Line",
            Action::Settings => "Settings…",
            Action::ToggleTerminal => "Toggle Terminal",
            Action::NewTerminal => "New Terminal",
            Action::Debug => "Start Debugging / Continue",
            Action::StopDebug => "Stop Debugging",
            Action::ToggleBreakpoint => "Toggle Breakpoint",
            Action::StepOver => "Step Over",
            Action::StepInto => "Step Into",
            Action::StepOut => "Step Out",
            Action::Profile => "Profile",
            Action::CommandPalette => "Command Palette…",
            Action::RecentFiles => "Recent Files…",
            Action::MoveLineUp => "Move Line Up",
            Action::MoveLineDown => "Move Line Down",
        }
    }

    /// Group shown next to the action in the palette and the shortcut list.
    pub fn category(self) -> &'static str {
        use Action::*;
        match self {
            Save | SaveAs | SaveAll | NewFile | OpenFile | OpenFolder | CloseTab | CloseAllTabs | Settings => "File",
            Find | Replace | FindInFiles | FindNext | FindPrev | ToggleComment | DuplicateLine | DeleteLine
            | MoveLineUp | MoveLineDown => "Edit",
            GotoLine | GotoFile | NextTab | PrevTab | NextProblem | PrevProblem | CommandPalette | RecentFiles => "Go",
            GotoDefinition | GotoDeclaration | GotoSymbol | QuickFix | FindReferences | Rename | Format | SwitchHeader
            | Back | Complete => "Code",
            RefactorThis | ExtractVariable | ExtractFunction | InlineVariable | ChangeSignature | SafeDelete | CallHierarchy => "Refactor",
            Build | Run | Stop | Profile => "Build",
            Debug | StopDebug | ToggleBreakpoint | StepOver | StepInto | StepOut => "Debug",
            ToggleSidebar | ToggleBottomPanel | ZoomIn | ZoomOut | ZoomReset | ToggleTerminal | NewTerminal => "View",
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
            Action::CloseAllTabs => "Cmd+Alt+W",
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
            Action::RefactorThis => if mac { "Ctrl+T" } else { "Ctrl+Alt+Shift+T" },
            Action::ExtractVariable => "Cmd+Alt+V",
            Action::ExtractFunction => "Cmd+Alt+M",
            Action::InlineVariable => "Cmd+Alt+N",
            Action::ChangeSignature => "Cmd+F6",
            Action::SafeDelete => if mac { "Cmd+Delete" } else { "Alt+Delete" },
            Action::CallHierarchy => "Ctrl+Alt+H",
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
            Action::CommandPalette => "Cmd+Shift+P",
            Action::RecentFiles => "Cmd+E",
            Action::MoveLineUp => "Alt+Shift+Up",
            Action::MoveLineDown => "Alt+Shift+Down",
        }
    }

    /// Handled inside the editor widget (only when it has focus).
    pub fn is_editor_action(self) -> bool {
        matches!(
            self,
            Action::ToggleComment | Action::DuplicateLine | Action::DeleteLine | Action::MoveLineUp | Action::MoveLineDown
        )
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
    #[test]
    fn set_keys() {
        let text = "// c\n{\n  \"theme\": \"dark\", // keep me\n  \"font_size\": 14.0,\n  \"clangd_args\": [\n    \"--a\"\n  ],\n  \"keybindings\": {\"build\": \"Cmd+B\"}\n}\n";
        let t = set_key_in_text(text, "theme", &serde_json::json!("light")).unwrap();
        assert!(t.contains("\"theme\": \"light\", // keep me"));
        let t = set_key_in_text(&t, "font_size", &serde_json::json!(16.0)).unwrap();
        assert!(t.contains("\"font_size\": 16.0,"));
        let t = set_key_in_text(&t, "clangd_args", &serde_json::json!(["--x", "--y"])).unwrap();
        assert!(t.contains("\"clangd_args\": [\"--x\",\"--y\"],"));
        let t = set_key_in_text(&t, "keybindings", &serde_json::json!({"run": "F6"})).unwrap();
        assert!(t.contains("\"keybindings\": {\"run\":\"F6\"}\n}"));
        let t = set_key_in_text(&t, "tab_size", &serde_json::json!(2)).unwrap();
        let s: Settings = serde_json::from_str(&strip_comments(&t)).unwrap();
        assert_eq!((s.theme.as_str(), s.font_size, s.tab_size), ("light", 16.0, 2));
        assert_eq!(s.clangd_args, vec!["--x", "--y"]);
        assert_eq!(s.keybindings.get("run").map(String::as_str), Some("F6"));
        let t = set_key_in_text("{}", "tab_size", &serde_json::json!(8)).unwrap();
        let s: Settings = serde_json::from_str(&t).unwrap();
        assert_eq!(s.tab_size, 8);
        // The default file round-trips.
        let t = set_key_in_text(&default_file_text(), "theme", &serde_json::json!("sepia")).unwrap();
        let s: Settings = serde_json::from_str(&strip_comments(&t)).unwrap();
        assert_eq!(s.theme, "sepia");
    }

    #[test]
    fn shortcut_strings() {
        for a in Action::ALL {
            let sc = parse_shortcut(a.default_shortcut()).unwrap();
            let s = shortcut_string(sc.modifiers, sc.logical_key).unwrap();
            let back = parse_shortcut(&s).unwrap();
            assert_eq!(back.logical_key, sc.logical_key, "{s}");
        }
    }

}
