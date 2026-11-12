# cedit

A fast, native code editor for **C** (and Objective-C), **GLSL** and **Makefiles**, written in Rust with
[egui](https://github.com/emilk/egui). One binary that runs on macOS, Windows and Linux. Note was made to 
test what I could do with Claude not to demonstrate my technical skills (and also because I wanted an editor just for C)

## Download

Ready-made builds for macOS (Apple Silicon and Intel), Windows and Linux are on the
[Releases](../../releases/latest) page.

**macOS:** download `cedit-macos-universal.zip`, unzip it and drag `cedit.app` to
Applications. The app isn't notarized by Apple, so the first launch is blocked. Allow it once
with:

```sh
xattr -dr com.apple.quarantine /Applications/cedit.app
```

(or open it, then go to *System Settings → Privacy & Security* and click *Open Anyway*).

**Windows:** download `cedit-windows-x86_64.zip`, unzip it anywhere and run `cedit.exe`. If
SmartScreen says "Windows protected your PC", click *More info → Run anyway*.

**Linux:** download `cedit-linux-x86_64.tar.gz`, then:

```sh
tar xzf cedit-linux-x86_64.tar.gz
cd cedit && ./install-linux.sh     # puts cedit in ~/.local/bin and adds it to your app menu
```

Or just run `./cedit` from that folder. Any desktop with X11 or Wayland works.

## Tools cedit uses

cedit is the editor; your compiler, `make`, clangd and the debugger do the heavy lifting.
Without them you still get editing and highlighting.

| OS | Install |
| --- | --- |
| macOS | `xcode-select --install` (clang, make, clangd and lldb-dap in one go) |
| Windows | `winget install LLVM.LLVM` (clang, clangd, lldb-dap) and a `make`, e.g. `winget install ezwinports.make` or MSYS2. clang links with the Visual Studio Build Tools |
| Debian, Ubuntu | `sudo apt install build-essential clangd lldb` (or `gdb` 14+ instead of lldb) |
| Fedora | `sudo dnf install gcc make clang-tools-extra lldb` |
| Arch | `sudo pacman -S base-devel clang lldb` |

For GLSL, put [glsl_analyzer](https://github.com/nolanderc/glsl_analyzer/releases) on your PATH.
For profiling on Linux, install `perf` (see *Performance analyzer* below).

## Build from source

1. Install Rust 1.95 or newer from <https://rustup.rs> (`rustup update` upgrades an older one).
2. Install the system pieces Rust needs:

   | OS | Install |
   | --- | --- |
   | macOS | `xcode-select --install` |
   | Windows | Visual Studio Build Tools with *Desktop development with C++* (the rustup installer offers to set this up) |
   | Debian, Ubuntu | `sudo apt install build-essential libxkbcommon-dev libgl1-mesa-dev libwayland-dev` |
   | Fedora | `sudo dnf install gcc libxkbcommon-devel mesa-libGL-devel wayland-devel` |
   | Arch | `sudo pacman -S base-devel libxkbcommon mesa wayland` |

3. Build and run:

   ```sh
   git clone <this repository's URL> cedit
   cd cedit
   cargo run --release                      # reopen your last session
   cargo run --release -- path/to/project   # open a folder
   cargo run --release -- file.c            # open a file (its folder becomes the project)
   ```

   The binary is `target/release/cedit` (`target\release\cedit.exe` on Windows). Copy it anywhere.

4. Optional, to install it like an app:

   | OS | Command | Result |
   | --- | --- | --- |
   | macOS | `scripts/package-macos.sh` (add `universal` for Intel + Apple Silicon) | `dist/cedit.app`; then `cp -R dist/cedit.app /Applications/` |
   | Linux | `scripts/package-linux.sh` | `dist/cedit-linux.tar.gz` with `install-linux.sh` inside |
   | Windows | none needed | copy `cedit.exe` wherever you like and pin it to Start |

## Features

**Editing**
- File tree (new file/folder, rename, move to trash, copy path, reveal in Finder/Explorer)
- Tabs, unsaved markers, prompts before closing or quitting with unsaved work
- Highlighting for C, Objective-C (.m, .mm), GLSL/shader files and Makefiles, plus type/function/macro colouring
  from clangd
- Auto-indent, smart `}` dedent, Tab/Shift+Tab block indent, real tabs in Makefiles
- Toggle comment, duplicate line, delete line
- Find/replace (match case, whole word), find in files, go to file (fuzzy), go to line
- Reloads files changed on disk and warns about conflicting unsaved edits
- Keeps CRLF line endings; saves atomically; trims trailing whitespace on save (configurable)
- Remembers your folder, tabs and zoom; drag and drop files or folders

**Viewing images, PDFs and Markdown**
- Images (PNG, JPEG, GIF, BMP, WebP, ICO, TIFF) open in a viewer tab: fit to window or 100%,
  scroll or pinch to zoom around the pointer, drag to pan, double-click to toggle
- PDFs open in a viewer tab with page navigation, fit-width and zoom. Rendering is pure Rust
  (no Pdfium or Poppler needed); encrypted PDFs aren't supported yet
- Markdown files open in the editor with a live rendered preview beside it (tables, task lists,
  code blocks, local images). Toggle it with *View → Markdown Preview* or the status bar

**Building with Make**
- Finds the Makefile nearest the current file and lists its targets in the toolbar
- **Build** (Cmd+B) saves everything and runs `make` with the selected target
- **Run** (Cmd+R) builds and then runs your program in a *Run* terminal tab, where it can read
  keyboard input. Set the command in *Build → Build Settings…* (for example `./demo`), or add a
  `run` target to your Makefile
- **Stop** (Cmd+.) kills make, or the running program, and everything it started
- Errors from gcc, clang, MSVC and glslang appear in *Problems*, in the gutter and as
  squiggles. Hover for the message, click to jump, F8 / Shift+F8 to step through them
- Build Settings: extra make arguments (`DEBUG=1 CC=clang`), parallel jobs, make program

**Terminal**
- A real shell in the bottom panel (Ctrl+\`): your login shell on macOS and Linux, PowerShell on
  Windows. It starts in the project folder
- Several terminals as tabs (Ctrl+Shift+\` or the + button)
- Colours, full-screen programs (vim, less, top), scrollback with the mouse wheel or
  Shift+PageUp/PageDown
- Drag to select; Cmd+C copies on a Mac. On Windows and Linux, Ctrl+C copies when something is
  selected and interrupts otherwise; Ctrl+Shift+C always copies
- Cmd+click (Ctrl+click) a `file.c:12:5` location, for example in compiler output, to open it
- On a Mac, Cmd shortcuts still reach the editor while the terminal has focus. On Windows and
  Linux, Ctrl+letter goes to the shell (so Ctrl+R searches shell history) and Ctrl+Shift
  shortcuts and function keys go to the editor

**Debugger**
- Click left of a line number (or press F9) to set a breakpoint
- **Debug** (F5) builds, then starts your Run program under the debugger. It stops at
  breakpoints and shows where it paused
- Step over (F10), step into (F11), step out (Shift+F11), continue (F5), stop (Shift+F5)
- The *Debug* panel shows the call stack (click a frame to look at it), local variables
  (expand structs and pointers), watch expressions and a console for evaluating expressions or
  typing debugger commands
- Hover over a variable while paused to see its value
- While paused, variable values appear at the end of the lines that use them, from the start of
  the function down to the current line. Structs show as `{x = 1, y = 2}`
- Values that changed since the last step are highlighted in the Variables and Watches lists
- Watches expand like variables. Write `ptr, N` to see N elements of what a pointer points to
  (e.g. `data, 16`), or right-click a pointer and choose *Show as array*
- The *Hex* checkbox shows numbers in hexadecimal
- *Memory* tab (next to Console): a hex and ASCII dump of 256 bytes at an address or expression
  (`buf`, `&x`, `p->data`, `0x7ffe…`), or right-click a variable and choose *View memory*. It
  follows the expression as you step and highlights bytes that changed; `<` and `>` page
  through memory
- Custom data views: put lldb formatters (`type summary add …`, `type synthetic add …`) in a
  `.lldbinit` in the project folder, or gdb commands and Python pretty-printers in a `.gdbinit`.
  cedit loads it when debugging starts, once you've trusted the folder (see below). This is also
  how library formatters (e.g. Qt's) are loaded
- On Linux with lldb, your program runs in the *Run* terminal tab so it can read keyboard
  input. On macOS and Windows, and with gdb, its output appears in the debug console instead
  (keyboard input isn't available while debugging there)
- Compile with `-g` (and ideally `-O0`) so the debugger can map the program back to your code

The debugger is lldb-dap or gdb 14+, whichever is found first:

| OS | How |
| --- | --- |
| macOS | Comes with Xcode 16 or newer; otherwise `brew install llvm`. macOS may ask for your password the first time you debug |
| Windows | `winget install LLVM.LLVM` (includes lldb-dap) |
| Linux | `sudo apt install lldb` (lldb-dap) or gdb 14 or newer |

Set `"debugger_path"` in settings to pick one, e.g. `"gdb"` or `"/opt/homebrew/opt/llvm/bin/lldb-dap"`.

**Performance analyzer**
- **Profile** (Cmd+Shift+R, or *Build → Build and Profile*) builds, then runs your Run program
  in the *Run* terminal while sampling it about 1000 times a second. When the program exits (or
  you press Stop) the *Profile* panel shows the results
- *Hot Functions*: each function's own time (SELF) and time including what it calls (TOTAL).
  Click a column header to sort, click a row to jump to the function
- *Flame Graph*: the call tree, widest bars take the most time. Click a bar to zoom in, use the
  breadcrumbs to zoom out, double-click to open the code, hover for exact numbers
- While the Profile panel is open, hot lines in the editor are shaded with their share of the
  samples
- Compile with `-g` so samples map back to your functions and lines

| OS | How |
| --- | --- |
| macOS | Uses the built-in `sample` tool, nothing to install |
| Linux | Needs perf: `sudo apt install linux-tools-generic` (or your distro's `perf` package). If perf reports a permission error, run `sudo sysctl kernel.perf_event_paranoid=1` |
| Windows | Not supported yet |

**Code intelligence (clangd)**
- Live errors and warnings as you type: syntax errors, implicit function declarations, type
  mismatches, missing includes. Squiggles, gutter marks and the error count update without
  building
- Quick fixes (Alt+Enter): when clangd knows a fix, the tooltip says so and Alt+Enter offers it,
  for example *Include <stdio.h> for symbol printf*. The same menu has clangd's refactorings
- Autocomplete (as you type, after `.` and `->`, or Ctrl+Space), including struct members and
  header names after `#include <` or `#include "`
- Parameter hints while you type a call: the signature appears above the cursor after `(` with
  the current argument underlined
- Hover for types, signatures, docs and what a macro expands to
- Go to definition (F12 or Cmd+click), go to declaration, usually the header (Ctrl+F12), back
  (Alt+Left / Cmd+[)
- Go to symbol in the project (Cmd+T): functions, structs, typedefs, macros and globals across
  your files and the headers they include
- Find references (Shift+F12), rename symbol across files (F2; changes are left unsaved for
  you to review)
- Format with clang-format (Cmd+Alt+L, or on save), switch header/source (Alt+O)
- clangd needs your compiler flags. If your project has no `compile_commands.json`, cedit runs
  `make -n` once to read them and keeps the result in its cache folder, not in your project
- **Trusted folders.** A Makefile's dry run and a `.lldbinit`/`.gdbinit` can run any command, so
  the first time you open a folder that has one, cedit asks whether you trust it. Until you say
  yes, it runs nothing from that folder on its own: clangd uses default flags and the debugger
  skips the setup file. Build, Run and Debug still work, because you asked for those. Trust a
  folder later with *File → Trust This Folder…*; trusting a folder also trusts everything in it

## Settings

*File → Settings…* (Cmd+,) opens `settings.json`. Save it to apply changes. It lives in
`~/Library/Application Support/cedit/` on macOS, `%APPDATA%\cedit\` on Windows and
`~/.config/cedit/` on Linux.

```jsonc
{
  "theme": "dark",                 // or "light"
  "font_size": 14.0,
  "font_path": "",                 // e.g. "/Library/Fonts/JetBrainsMono-Regular.ttf"
  "tab_size": 4,
  "format_on_save": false,         // clang-format C files when saving
  "trim_trailing_whitespace": true,
  "ensure_final_newline": true,
  "clangd_path": "",               // empty = find it automatically
  "clangd_args": [],               // e.g. ["--clang-tidy"]
  "glsl_analyzer_path": "",
  "debugger_path": "",             // lldb-dap or gdb; empty = find one
  "keybindings": { "build": "Cmd+B", "go_to_definition": "Cmd+Alt+B" }
}
```

In shortcuts, `Cmd` means Ctrl on Windows and Linux. The bottom of the file lists every action
you can rebind. *Help → Keyboard Shortcuts* shows the current bindings.

## Default shortcuts

| Shortcut | Action |
| --- | --- |
| Cmd+P | Go to file |
| Cmd+F, F3 / Shift+F3 | Find, next, previous |
| Cmd+Alt+F (mac), Ctrl+H | Replace |
| Cmd+Shift+F | Find in files |
| Cmd+L (mac), Ctrl+G | Go to line (`line` or `line:col`) |
| Cmd+B / Cmd+R / Cmd+. | Build / build and run / stop |
| F8 / Shift+F8 | Next / previous problem |
| F5 / Shift+F5 | Start debugging or continue / stop debugging |
| F9 | Toggle breakpoint |
| F10 / F11 / Shift+F11 | Step over / into / out |
| Cmd+Shift+R | Build and profile |
| F12, Cmd+click | Go to definition |
| Ctrl+F12 | Go to declaration |
| Cmd+T | Go to symbol in project |
| Alt+Enter | Quick fix / refactor |
| Shift+F12 | Find references |
| F2 | Rename symbol |
| Cmd+Alt+L | Format file |
| Alt+O | Switch header/source |
| Alt+Left, Cmd+[ (mac) | Back |
| Ctrl+Space | Autocomplete |
| Cmd+/ | Toggle comment |
| Cmd+D | Duplicate line or selection |
| Cmd+Shift+K | Delete line |
| Tab / Shift+Tab | Indent / unindent |
| Cmd+S, Cmd+Shift+S, Cmd+Alt+S | Save, save as, save all |
| Cmd+N, Cmd+O, Cmd+Shift+O | New file, open file, open folder |
| Cmd+W / Cmd+Alt+W | Close tab / close all tabs (right-click a tab for Close Others, Close to the Right, Close Saved) |
| Ctrl+Tab | Next tab |
| Cmd+1 / Cmd+J | Toggle sidebar / bottom panel |
| Ctrl+\` / Ctrl+Shift+\` | Show or hide the terminal / new terminal |
| Cmd+= / Cmd+- / Cmd+0 | Zoom |
| Cmd+, | Settings |

## Making a release

`.github/workflows/build.yml` runs the tests on macOS, Windows and Linux on every push to
GitHub. To publish downloads:

1. Set the new version in `Cargo.toml` (e.g. `version = "0.2.0"`) and commit it.
2. Tag and push:

   ```sh
   git tag v0.2.0
   git push origin main v0.2.0
   ```

The workflow then builds a universal macOS app, a Windows zip and a Linux tarball and attaches
them to a GitHub release named after the tag, with notes generated from the commits. It refuses
to build if the tag and the `Cargo.toml` version don't match. Each push's builds can also be
downloaded from the run's page under *Actions*.

## License

MIT, see [LICENSE](LICENSE).

## Code layout

| File | What's in it |
| --- | --- |
| `src/app.rs` | Window layout, menus, shortcuts, dialogs |
| `src/editor.rs` | Open documents and the code editor widget |
| `src/viewer.rs` | Image and PDF viewer tabs |
| `src/textops.rs` | Editing operations (indent, comment, …) |
| `src/highlight.rs` | Lexers for C, GLSL and Make |
| `src/find.rs` | In-file search |
| `src/project.rs` | File tree, file listing, fuzzy matching, find in files |
| `src/make.rs` | Makefile detection, targets, running make, parsing compiler errors |
| `src/build_ui.rs` | Build toolbar, menu, output and problems panels |
| `src/lsp.rs` | Language Server Protocol client, compile_commands.json generation |
| `src/terminal.rs` | Terminal emulator widget (pseudo-terminal, key mapping, drawing) |
| `src/terminal_ui.rs` | Terminal tabs and running programs in them |
| `src/lsp_ui.rs` | Completion, hover, navigation, rename, formatting, live errors |
| `src/dap.rs` | Debug Adapter Protocol client, finding lldb-dap or gdb |
| `src/debug_ui.rs` | Breakpoints, debug session and the Debug panel |
| `src/profile.rs` | Reading perf and `sample` output into a call tree |
| `src/profile_ui.rs` | Profiling runs, hot functions list, flame graph and line shading |
| `src/settings.rs` | settings.json and the keymap |
| `src/theme.rs` | Dark and light colours |

Run the tests with `cargo test`.
