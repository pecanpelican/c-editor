//! Sampling profiler support: run a program under `sample` (macOS) or
//! `perf` (Linux), and turn the result into a call tree and per-function stats.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// One stack frame as reported by the profiler.
#[derive(Clone, Debug, PartialEq)]
pub struct Frame {
    pub name: String,
    /// Source location as written in the debug info (often relative).
    pub file: Option<String>,
    pub line: Option<usize>, // 1-based
}

/// A node in the call tree (callers above callees).
#[derive(Clone, Debug, Default)]
pub struct Node {
    pub name: String,
    pub total: u64,
    pub self_count: u64,
    pub children: Vec<Node>,
    /// Most common source location seen for this node.
    pub file: Option<String>,
    pub line: Option<usize>,
    locs: HashMap<(String, usize), u64>,
}

impl Node {
    fn child(&mut self, name: &str) -> &mut Node {
        if let Some(i) = self.children.iter().position(|c| c.name == name) {
            return &mut self.children[i];
        }
        self.children.push(Node { name: name.to_string(), ..Default::default() });
        self.children.last_mut().unwrap()
    }

    fn finish(&mut self) {
        if let Some(((f, l), _)) = self.locs.iter().max_by_key(|(k, v)| (**v, std::cmp::Reverse((*k).clone()))) {
            self.file = Some(f.clone());
            self.line = Some(*l);
        }
        self.locs.clear();
        self.children.sort_by(|a, b| b.total.cmp(&a.total).then_with(|| a.name.cmp(&b.name)));
        for c in &mut self.children {
            c.finish();
        }
    }
}

#[derive(Clone, Debug)]
pub struct Function {
    pub name: String,
    pub self_count: u64,
    pub total: u64,
    /// The function's hottest line (by its own samples), or where it was seen.
    pub file: Option<String>,
    pub line: Option<usize>,
}

#[derive(Clone, Debug, Default)]
pub struct Profile {
    pub samples: u64,
    pub root: Node,
    pub functions: Vec<Function>,
    /// Samples whose innermost frame is at (file, line), for hot lines.
    pub lines: HashMap<(String, usize), u64>,
}

impl Profile {
    /// Build from stacks listed root first, with a sample count each.
    pub fn from_stacks(stacks: Vec<(Vec<Frame>, u64)>) -> Self {
        let mut root = Node { name: "all".into(), ..Default::default() };
        let mut funcs: HashMap<String, (u64, u64, HashMap<(String, usize), u64>, HashMap<(String, usize), u64>)> = HashMap::new();
        let mut lines: HashMap<(String, usize), u64> = HashMap::new();
        let mut samples = 0;
        for (stack, n) in stacks {
            if stack.is_empty() || n == 0 {
                continue;
            }
            samples += n;
            root.total += n;
            let mut node = &mut root;
            for f in &stack {
                node = node.child(&f.name);
                node.total += n;
                if let (Some(file), Some(line)) = (&f.file, f.line) {
                    *node.locs.entry((file.clone(), line)).or_default() += n;
                }
            }
            node.self_count += n;
            // Per-function totals count a function once per stack (recursion).
            let mut seen: Vec<&str> = Vec::new();
            for (i, f) in stack.iter().enumerate() {
                let e = funcs.entry(f.name.clone()).or_default();
                if !seen.contains(&f.name.as_str()) {
                    e.1 += n;
                    seen.push(&f.name);
                }
                let loc = f.file.clone().zip(f.line);
                if i == stack.len() - 1 {
                    e.0 += n;
                    if let Some(l) = &loc {
                        *e.2.entry(l.clone()).or_default() += n;
                        *lines.entry(l.clone()).or_default() += n;
                    }
                }
                if let Some(l) = loc {
                    *e.3.entry(l).or_default() += n;
                }
            }
        }
        root.finish();
        let mut functions: Vec<Function> = funcs
            .into_iter()
            .map(|(name, (self_count, total, self_locs, any_locs))| {
                let best = |m: &HashMap<(String, usize), u64>| m.iter().max_by_key(|(k, v)| (**v, std::cmp::Reverse((*k).clone()))).map(|(k, _)| k.clone());
                let loc = best(&self_locs).or_else(|| best(&any_locs));
                Function { name, self_count, total, file: loc.as_ref().map(|l| l.0.clone()), line: loc.map(|l| l.1) }
            })
            .collect();
        functions.sort_by(|a, b| b.self_count.cmp(&a.self_count).then(b.total.cmp(&a.total)).then_with(|| a.name.cmp(&b.name)));
        Profile { samples, root, functions, lines }
    }
}

// ----- perf (Linux) -----------------------------------------------------------

/// Find `perf`. Ubuntu's /usr/bin/perf refuses to run when the kernel and
/// tools versions differ, so also look in /usr/lib/linux-tools*.
pub fn find_perf() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    for dir in ["/usr/lib/linux-tools", "/usr/lib"] {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if dir == "/usr/lib" && !name.starts_with("linux-tools") {
                    continue;
                }
                let p = e.path().join("perf");
                if p.is_file() {
                    candidates.push(p);
                }
            }
        }
    }
    candidates.sort();
    let in_path = crate::make::find_in_path("perf");
    // Prefer the PATH one if it actually runs.
    if let Some(p) = &in_path
        && std::process::Command::new(p)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    {
        return in_path;
    }
    candidates.pop().or(in_path)
}

/// Parse `perf script -F comm,tid,ip,sym,srcline` output.
pub fn parse_perf_script(text: &str) -> Vec<(Vec<Frame>, u64)> {
    let mut stacks: HashMap<Vec<(String, Option<String>, Option<usize>)>, u64> = HashMap::new();
    let mut cur: Vec<Frame> = Vec::new();
    let mut in_record = false;
    let flush = |cur: &mut Vec<Frame>, stacks: &mut HashMap<_, u64>| {
        if !cur.is_empty() {
            cur.reverse(); // perf lists the innermost frame first
            let key: Vec<(String, Option<String>, Option<usize>)> = cur.drain(..).map(|f| (f.name, f.file, f.line)).collect();
            *stacks.entry(key).or_default() += 1;
        }
    };
    for line in text.lines() {
        if line.trim().is_empty() {
            flush(&mut cur, &mut stacks);
            in_record = false;
            continue;
        }
        if !line.starts_with([' ', '\t']) {
            // Record header: "prog  1234  ..." — a new sample.
            flush(&mut cur, &mut stacks);
            in_record = true;
            continue;
        }
        if !in_record {
            continue;
        }
        let t = line.trim();
        // Frame: "55d5c0a1b2c3 main" (address, symbol). Source: "main.c:12".
        if let Some((addr, sym)) = t.split_once(' ')
            && !addr.is_empty()
            && addr.chars().all(|c| c.is_ascii_hexdigit())
        {
            let sym = sym.trim();
            let sym = sym.split_once(" (").map(|(a, _)| a).unwrap_or(sym); // "(dso)" if present
            let name = if sym.is_empty() || sym == "[unknown]" { "[unknown]".to_string() } else { strip_suffix(sym) };
            cur.push(Frame { name, file: None, line: None });
        } else if let Some(f) = cur.last_mut()
            && f.file.is_none()
            && let Some((file, line)) = parse_file_line(t)
        {
            f.file = Some(file);
            f.line = Some(line);
        }
    }
    flush(&mut cur, &mut stacks);
    stacks
        .into_iter()
        .map(|(k, n)| (k.into_iter().map(|(name, file, line)| Frame { name, file, line }).collect(), n))
        .collect()
}

/// "foo@@GLIBC_2.34" / "foo@GLIBC_2.15" -> "foo" (keeps "foo@plt")
fn strip_suffix(sym: &str) -> String {
    sym.split("@@").next().unwrap_or(sym).split("@GLIBC").next().unwrap_or(sym).to_string()
}

/// "main.c:12" (also "/a/b/main.c:12 (discriminator 3)") -> ("main.c", 12).
fn parse_file_line(s: &str) -> Option<(String, usize)> {
    let s = s.split(" (").next()?.trim();
    let (file, line) = s.rsplit_once(':')?;
    let line: usize = line.parse().ok()?;
    if file.is_empty() || file.starts_with("??") || line == 0 {
        return None;
    }
    Some((file.to_string(), line))
}

// ----- sample (macOS) -----------------------------------------------------------

/// Parse the "Call graph:" section of macOS `sample` output.
///
/// ```text
/// Call graph:
///     2219 Thread_4815046   DispatchQueue_1: com.apple.main-thread  (serial)
///       2219 start  (in dyld) + 2840  [0x18a8d60e0]
///         2219 main  (in demo) + 64  [0x1027f3e9c]  main.c:30
///           1500 update  (in demo) + 20  [0x1027f3e30]  main.c:12
///           + 1500 slow  (in demo) + 40  [0x1027f3df0]  main.c:6
///           719 fib  (in demo) + 28  [0x1027f3d8c]  main.c:9
/// ```
pub fn parse_sample(text: &str) -> Vec<(Vec<Frame>, u64)> {
    let mut out = Vec::new();
    let mut in_graph = false;
    // Stack of (depth, frame, count, children_count_sum)
    let mut stack: Vec<(usize, Frame, u64, u64)> = Vec::new();
    let mut base: Option<usize> = None;
    let pop_to = |stack: &mut Vec<(usize, Frame, u64, u64)>, depth: usize, out: &mut Vec<(Vec<Frame>, u64)>| {
        while stack.last().is_some_and(|(d, ..)| *d >= depth) {
            let (_, _, count, kids) = stack.last().cloned().unwrap();
            let own = count.saturating_sub(kids);
            if own > 0 {
                // Skip the thread line (depth 0) in the stack itself.
                let frames: Vec<Frame> = stack.iter().skip(1).map(|(_, f, ..)| f.clone()).collect();
                if !frames.is_empty() {
                    out.push((frames, own));
                }
            }
            stack.pop();
        }
    };
    for line in text.lines() {
        if line.starts_with("Call graph:") {
            in_graph = true;
            continue;
        }
        if !in_graph {
            continue;
        }
        if line.trim().is_empty() || line.starts_with("Total number in stack") || line.starts_with("Sort by top of stack") {
            if !stack.is_empty() || base.is_some() {
                break;
            }
            continue;
        }
        let Some(pos) = line.find(|c: char| c.is_ascii_digit()) else { continue };
        if !line[..pos].chars().all(|c| " +!:|".contains(c)) {
            continue;
        }
        let base_col = *base.get_or_insert(pos);
        let depth = pos.saturating_sub(base_col) / 2;
        let rest = &line[pos..];
        let Some((count, frame_text)) = rest.split_once(' ') else { continue };
        let Ok(count) = count.parse::<u64>() else { continue };
        pop_to(&mut stack, depth, &mut out);
        if let Some(parent) = stack.last_mut() {
            parent.3 += count;
        }
        stack.push((depth, parse_sample_frame(frame_text), count, 0));
    }
    pop_to(&mut stack, 0, &mut out);
    out
}

/// "main  (in demo) + 64  [0x1027f3e9c]  main.c:30" -> Frame
fn parse_sample_frame(t: &str) -> Frame {
    let t = t.trim();
    let name = t.split("  (in ").next().unwrap_or(t).trim().to_string();
    let (file, line) = t
        .rsplit_once("]  ")
        .and_then(|(_, loc)| parse_file_line(loc.trim()))
        .map(|(f, l)| (Some(f), Some(l)))
        .unwrap_or((None, None));
    Frame { name, file, line }
}

/// Resolve a debug-info path ("main.c", "src/x.c", "/abs/x.c") against the
/// program's folder, then the project's files by name.
pub fn resolve_source(file: &str, dir: &Path, project_files: &[PathBuf]) -> Option<PathBuf> {
    let p = Path::new(file);
    if p.is_absolute() {
        return p.is_file().then(|| p.to_path_buf());
    }
    let joined = dir.join(p);
    if joined.is_file() {
        return Some(joined);
    }
    let name = p.file_name()?;
    project_files.iter().find(|f| f.file_name() == Some(name) && f.ends_with(p)).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PERF: &str = "hot   14221  2451.096935: \n\
\t    563733403200 slow\n  hot.c:5\n\
\t    563733403300 main\n  hot.c:16\n\
\t    7fcecd22a1ca __libc_start_call_main\n  libc-start.c:74\n\
\t    7fcecd22a28b __libc_start_main@@GLIBC_2.34\n  libc-start.c:128\n\
\t    5637334030a5 _start\n  ??:0\n\
\n\
hot   14221  2451.097932: \n\
\t    563733403200 fib\n  hot.c:8\n\
\t    563733403200 fib\n  hot.c:8\n\
\t    563733403300 main\n  hot.c:17\n\
\t    5637334030a5 _start\n  ??:0\n\
\n\
hot   14221  2451.097990: \n\
\t    563733403200 slow\n  hot.c:5\n\
\t    563733403300 main\n  hot.c:16\n\
\t    7fcecd22a1ca __libc_start_call_main\n  libc-start.c:74\n\
\t    7fcecd22a28b __libc_start_main@@GLIBC_2.34\n  libc-start.c:128\n\
\t    5637334030a5 _start\n  ??:0\n";

    #[test]
    fn perf_script() {
        let stacks = parse_perf_script(PERF);
        let p = Profile::from_stacks(stacks);
        assert_eq!(p.samples, 3);
        assert_eq!(p.functions[0].name, "slow");
        assert_eq!(p.functions[0].self_count, 2);
        assert_eq!(p.functions[0].file.as_deref(), Some("hot.c"));
        assert_eq!(p.functions[0].line, Some(5));
        let fib = p.functions.iter().find(|f| f.name == "fib").unwrap();
        assert_eq!((fib.self_count, fib.total), (1, 1)); // recursion counted once
        let main = p.functions.iter().find(|f| f.name == "main").unwrap();
        assert_eq!((main.self_count, main.total), (0, 3));
        assert_eq!(p.root.children[0].name, "_start");
        assert_eq!(p.root.children[0].total, 3);
        assert!(p.functions.iter().any(|f| f.name == "__libc_start_main"));
        assert_eq!(p.lines.get(&("hot.c".to_string(), 5)), Some(&2));
    }

    const SAMPLE: &str = "Analysis of sampling demo (pid 123) every 1 millisecond
Process:         demo [123]

Call graph:
    100 Thread_4815046   DispatchQueue_1: com.apple.main-thread  (serial)
      100 start  (in dyld) + 2840  [0x18a8d60e0]
        100 main  (in demo) + 64  [0x1027f3e9c]  main.c:30
          60 update  (in demo) + 20  [0x1027f3e30]  main.c:12
          + 55 slow  (in demo) + 40  [0x1027f3df0]  main.c:6
          + ! 50 sqrt  (in libsystem_m.dylib) + 8  [0x18a000000]
          35 fib  (in demo) + 28  [0x1027f3d8c]  main.c:9
          + 20 fib  (in demo) + 40  [0x1027f3d90]  main.c:9

Total number in stack (recursive counted multiple times):
        55       slow  (in demo)
";

    #[test]
    fn sample_output() {
        let p = Profile::from_stacks(parse_sample(SAMPLE));
        assert_eq!(p.samples, 100);
        let start = &p.root.children[0];
        assert_eq!(start.name, "start");
        let main = &start.children[0];
        assert_eq!((main.name.as_str(), main.total, main.self_count), ("main", 100, 5));
        let update = &main.children[0];
        assert_eq!((update.name.as_str(), update.total, update.self_count), ("update", 60, 5));
        assert_eq!(update.children[0].children[0].name, "sqrt");
        let sqrt = p.functions.iter().find(|f| f.name == "sqrt").unwrap();
        assert_eq!(sqrt.self_count, 50);
        let fib = p.functions.iter().find(|f| f.name == "fib").unwrap();
        assert_eq!((fib.self_count, fib.total), (35, 35));
        assert_eq!(fib.line, Some(9));
    }

    /// Trimmed from a real `sample` report (macOS 26.6, Apple Silicon): several
    /// offsets on one frame, `! : |` guides, and deep lines indented with spaces only.
    const SAMPLE_REAL: &str = r#"Analysis of sampling hot (pid 95459) every 1 millisecond
Process:         hot [95459]
Code Type:       ARM64
----

Call graph:
    100 Thread_28317039   DispatchQueue_1: com.apple.main-thread  (serial)
      100 start  (in dyld) + 6992  [0x18139c4e4]
        60 main  (in hot) + 36  [0x102f5c550]  hot.c:16
        + 60 slow_sqrt_sum  (in hot) + 24,20,...  [0x102f5c478,0x102f5c474,...]  hot.c:6
        40 main  (in hot) + 72  [0x102f5c574]  hot.c:18
          30 fib  (in hot) + 56  [0x102f5c4f8]  hot.c:12
          ! 20 fib  (in hot) + 56  [0x102f5c4f8]  hot.c:12
          ! : 12 fib  (in hot) + 72  [0x102f5c508]  hot.c:12
          ! :   5 fib  (in hot) + 56  [0x102f5c4f8]  hot.c:12
          ! :     5 fib  (in hot) + 20  [0x102f5c4d4]  hot.c:11
          ! 10 fib  (in hot) + 16  [0x102f5c4d0]  hot.c:11
          10 fib  (in hot) + 0  [0x102f5c4c0]  hot.c:11

Total number in stack (recursive counted multiple, when >=5):
        10706       fib  (in hot) + 56  [0x102f5c4f8]  hot.c:12

Sort by top of stack, same collapsed (when >= 5):
        slow_sqrt_sum  (in hot)        60
        fib  (in hot)        40
"#;

    #[test]
    fn real_sample_output() {
        let stacks = parse_sample(SAMPLE_REAL);
        assert_eq!(stacks.iter().map(|(f, _)| f.len()).max(), Some(7));
        let p = Profile::from_stacks(stacks);
        assert_eq!(p.samples, 100);
        let slow = p.functions.iter().find(|f| f.name == "slow_sqrt_sum").unwrap();
        assert_eq!((slow.self_count, slow.file.as_deref(), slow.line), (60, Some("hot.c"), Some(6)));
        let fib = p.functions.iter().find(|f| f.name == "fib").unwrap();
        assert_eq!((fib.self_count, fib.total), (40, 40));
        let main = p.functions.iter().find(|f| f.name == "main").unwrap();
        assert_eq!(main.total, 100);
    }

    #[test]
    fn file_lines() {
        assert_eq!(parse_file_line("main.c:12"), Some(("main.c".into(), 12)));
        assert_eq!(parse_file_line("/a/b.c:3 (discriminator 2)"), Some(("/a/b.c".into(), 3)));
        assert_eq!(parse_file_line("??:0"), None);
    }
}
