//! Fast, dependency-free syntax highlighting for C, GLSL and Makefiles.
//!
//! The lexers are hand-written and run over the whole buffer. They are
//! designed to be cheap enough to rerun on every edit (a 100k line file
//! lexes in a few milliseconds).

use std::ops::Range;
use std::path::Path;

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontId};
use serde::{Deserialize, Serialize};

use crate::theme::Theme;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum Language {
    C,
    Glsl,
    Make,
    Plain,
}

impl Language {
    pub fn from_path(path: &Path) -> Self {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if matches!(name, "Makefile" | "makefile" | "GNUmakefile") {
            return Language::Make;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        match ext.as_str() {
            "c" | "h" | "inc" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "m" => Language::C,
            "glsl" | "vert" | "frag" | "geom" | "comp" | "tesc" | "tese" | "vs" | "fs" | "gs"
            | "mesh" | "task" | "rgen" | "rint" | "rahit" | "rchit" | "rmiss" | "rcall"
            | "hlsl" | "fx" | "fxh" | "metal" | "wgsl" | "shader" => Language::Glsl,
            "mk" | "make" | "mak" => Language::Make,
            _ => Language::Plain,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Language::C => "C",
            Language::Glsl => "GLSL",
            Language::Make => "Makefile",
            Language::Plain => "Plain Text",
        }
    }

    /// The prefix used by "toggle comment".
    pub fn line_comment(self) -> Option<&'static str> {
        match self {
            Language::C | Language::Glsl => Some("//"),
            Language::Make => Some("#"),
            Language::Plain => None,
        }
    }

    /// Makefiles need real tab characters for recipes.
    pub fn uses_hard_tabs(self) -> bool {
        self == Language::Make
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tok {
    Normal,
    Keyword,
    Type,
    Function,
    Number,
    Str,
    Comment,
    Preproc,
    Constant,
    Punct,
    Variable,
    Target,
}

pub type Spans = Vec<(Range<usize>, Tok)>;

pub fn tokenize(lang: Language, text: &str) -> Spans {
    match lang {
        Language::C => lex_c_family(text, false),
        Language::Glsl => lex_c_family(text, true),
        Language::Make => lex_make(text),
        Language::Plain => vec![(0..text.len(), Tok::Normal)],
    }
}

/// Build a layout job for `text`, with search matches given as byte ranges
/// (sorted, non-overlapping) painted with a background colour.
pub fn layout_job(
    lang: Language,
    text: &str,
    font: FontId,
    theme: &Theme,
    matches: &[Range<usize>],
    current_match: Option<usize>,
    semantic: &[(Range<usize>, Tok)],
) -> LayoutJob {
    let mut spans = tokenize(lang, text);
    apply_semantic(&mut spans, semantic);
    let mut job = LayoutJob::default();
    job.text.reserve(text.len());
    let mut mi = 0usize;
    for (range, tok) in spans {
        let color = theme.token_color(tok);
        let italic = tok == Tok::Comment && theme.italic_comments;
        let mut start = range.start;
        while start < range.end {
            // Skip matches that end before this point.
            while mi < matches.len() && matches[mi].end <= start {
                mi += 1;
            }
            let (end, bg) = if mi < matches.len() && matches[mi].start <= start {
                let bg = if Some(mi) == current_match {
                    theme.current_match_bg
                } else {
                    theme.match_bg
                };
                (matches[mi].end.min(range.end), bg)
            } else if mi < matches.len() && matches[mi].start < range.end {
                (matches[mi].start, Color32::TRANSPARENT)
            } else {
                (range.end, Color32::TRANSPARENT)
            };
            let format = TextFormat {
                font_id: font.clone(),
                color,
                italics: italic,
                background: bg,
                ..Default::default()
            };
            job.append(&text[start..end], 0.0, format);
            start = end;
        }
    }
    job
}

/// Recolour identifiers using semantic tokens from the language server.
/// Only tokens that exactly cover one identifier span are applied, so stale
/// tokens (from an older version of the text) can't corrupt highlighting.
fn apply_semantic(spans: &mut Spans, semantic: &[(Range<usize>, Tok)]) {
    if semantic.is_empty() {
        return;
    }
    let mut si = 0;
    for (range, tok) in spans.iter_mut() {
        if !matches!(*tok, Tok::Normal | Tok::Function | Tok::Constant | Tok::Type) {
            continue;
        }
        while si < semantic.len() && semantic[si].0.start < range.start {
            si += 1;
        }
        if si >= semantic.len() {
            break;
        }
        if semantic[si].0 == *range {
            *tok = semantic[si].1;
        }
    }
}

// ---------------------------------------------------------------------------
// C / GLSL

const C_KEYWORDS: &[&str] = &[
    "auto", "break", "case", "const", "continue", "default", "do", "else", "enum", "extern",
    "for", "goto", "if", "inline", "register", "restrict", "return", "sizeof", "static",
    "struct", "switch", "typedef", "union", "volatile", "while", "_Alignas", "_Alignof",
    "_Atomic", "_Generic", "_Noreturn", "_Static_assert", "_Thread_local", "alignas", "alignof",
    "constexpr", "static_assert", "thread_local", "typeof", "typeof_unqual", "__attribute__",
    "__declspec", "__inline", "__restrict", "__extension__", "asm", "__asm__",
];

const C_TYPES: &[&str] = &[
    "char", "double", "float", "int", "long", "short", "signed", "unsigned", "void", "bool",
    "_Bool", "_Complex", "_Imaginary", "FILE", "va_list", "jmp_buf",
];

const C_CONSTANTS: &[&str] = &["true", "false", "NULL", "nullptr", "stdin", "stdout", "stderr"];

const GLSL_KEYWORDS: &[&str] = &[
    "attribute", "const", "uniform", "varying", "buffer", "shared", "coherent", "volatile",
    "restrict", "readonly", "writeonly", "layout", "centroid", "flat", "smooth",
    "noperspective", "patch", "sample", "break", "continue", "do", "for", "while", "switch",
    "case", "default", "if", "else", "subroutine", "in", "out", "inout", "invariant", "precise",
    "discard", "return", "struct", "precision", "highp", "mediump", "lowp", "demote",
    "rayPayloadEXT", "rayPayloadInEXT", "hitAttributeEXT", "callableDataEXT",
    "callableDataInEXT", "shaderRecordEXT", "taskPayloadSharedEXT", "perprimitiveEXT",
];

const GLSL_TYPES: &[&str] = &[
    "void", "bool", "int", "uint", "float", "double", "atomic_uint",
    "accelerationStructureEXT", "rayQueryEXT",
];

const GLSL_TYPE_PREFIXES: &[&str] = &[
    "vec", "ivec", "uvec", "bvec", "dvec", "mat", "dmat", "sampler", "isampler", "usampler",
    "image", "iimage", "uimage", "texture", "itexture", "utexture", "subpassInput",
    "isubpassInput", "usubpassInput", "f16vec", "i8vec", "u8vec", "i16vec", "u16vec",
    "i64vec", "u64vec", "float16_t", "int8_t", "uint8_t", "int16_t", "uint16_t", "int64_t",
    "uint64_t",
];

const GLSL_CONSTANTS: &[&str] = &["true", "false"];

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Length in bytes of the UTF-8 character starting with byte `b`.
fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xff => 4,
        _ => 1, // continuation byte; should not happen at a char boundary
    }
}

struct SpanBuilder {
    spans: Spans,
    /// The last span is an identifier and must stay separate (semantic
    /// tokens are matched against whole identifier spans).
    last_is_word: bool,
}

impl SpanBuilder {
    fn new(cap: usize) -> Self {
        Self { spans: Vec::with_capacity(cap), last_is_word: false }
    }

    fn push_word(&mut self, range: Range<usize>, tok: Tok) {
        if !range.is_empty() {
            self.spans.push((range, tok));
            self.last_is_word = true;
        }
    }

    fn push(&mut self, range: Range<usize>, tok: Tok) {
        if range.is_empty() {
            return;
        }
        if std::mem::take(&mut self.last_is_word) {
            self.spans.push((range, tok));
            return;
        }
        if let Some((last, last_tok)) = self.spans.last_mut()
            && *last_tok == tok
            && last.end == range.start
        {
            last.end = range.end;
            return;
        }
        self.spans.push((range, tok));
    }
}

fn is_glsl_composite_type(word: &str) -> bool {
    GLSL_TYPE_PREFIXES.iter().any(|p| {
        let Some(rest) = word.strip_prefix(p) else {
            return false;
        };
        if rest.is_empty() {
            return p.ends_with("_t") || p.ends_with("Input");
        }
        rest.as_bytes()[0].is_ascii_digit()
            || ["Cube", "Buffer", "Rect", "MS", "Shadow"]
                .iter()
                .any(|s| rest.starts_with(s))
    })
}

fn classify_c_ident(word: &str, glsl: bool) -> Option<Tok> {
    if glsl {
        if GLSL_KEYWORDS.contains(&word) {
            return Some(Tok::Keyword);
        }
        if GLSL_TYPES.contains(&word) || is_glsl_composite_type(word) {
            return Some(Tok::Type);
        }
        if GLSL_CONSTANTS.contains(&word) || word.starts_with("gl_") {
            return Some(Tok::Constant);
        }
    } else {
        if C_KEYWORDS.contains(&word) {
            return Some(Tok::Keyword);
        }
        if C_TYPES.contains(&word) || (word.ends_with("_t") && word.len() > 2) {
            return Some(Tok::Type);
        }
        if C_CONSTANTS.contains(&word) {
            return Some(Tok::Constant);
        }
    }
    None
}

fn lex_c_family(text: &str, glsl: bool) -> Spans {
    let b = text.as_bytes();
    let n = b.len();
    let mut out = SpanBuilder::new(n / 4);
    let mut i = 0;
    let mut line_start = true; // only whitespace seen since the last newline
    let mut in_include = false; // inside `#include` line: treat <...> as string

    while i < n {
        let c = b[i];
        // Newline
        if c == b'\n' {
            out.push(i..i + 1, Tok::Normal);
            i += 1;
            line_start = true;
            in_include = false;
            continue;
        }
        if c == b' ' || c == b'\t' || c == b'\r' {
            out.push(i..i + 1, Tok::Normal);
            i += 1;
            continue;
        }
        // Comments
        if c == b'/' && i + 1 < n && b[i + 1] == b'/' {
            let mut j = i + 2;
            while j < n && b[j] != b'\n' {
                // Backslash line continuation continues the comment.
                if b[j] == b'\\' && j + 1 < n && b[j + 1] == b'\n' {
                    j += 2;
                    continue;
                }
                j += 1;
            }
            out.push(i..j, Tok::Comment);
            i = j;
            line_start = false;
            continue;
        }
        if c == b'/' && i + 1 < n && b[i + 1] == b'*' {
            let mut j = i + 2;
            while j < n && !(b[j] == b'*' && j + 1 < n && b[j + 1] == b'/') {
                j += 1;
            }
            j = (j + 2).min(n);
            out.push(i..j, Tok::Comment);
            i = j;
            line_start = false;
            continue;
        }
        // Preprocessor directive
        if c == b'#' && line_start {
            let mut j = i + 1;
            while j < n && (b[j] == b' ' || b[j] == b'\t') {
                j += 1;
            }
            let word_start = j;
            while j < n && is_ident(b[j]) {
                j += 1;
            }
            let word = &text[word_start..j];
            in_include = matches!(word, "include" | "include_next" | "import" | "embed");
            out.push(i..j, Tok::Preproc);
            i = j;
            line_start = false;
            continue;
        }
        line_start = false;
        if in_include && c == b'<' {
            let mut j = i + 1;
            while j < n && b[j] != b'>' && b[j] != b'\n' {
                j += 1;
            }
            if j < n && b[j] == b'>' {
                j += 1;
            }
            out.push(i..j, Tok::Str);
            i = j;
            continue;
        }
        // Strings and chars
        if c == b'"' || c == b'\'' {
            let quote = c;
            let mut j = i + 1;
            while j < n && b[j] != quote && b[j] != b'\n' {
                if b[j] == b'\\' && j + 1 < n {
                    j += 1;
                }
                j += utf8_len(b[j]);
            }
            if j < n && b[j] == quote {
                j += 1;
            }
            let j = j.min(n);
            out.push(i..j, Tok::Str);
            i = j;
            continue;
        }
        // Numbers
        if c.is_ascii_digit() || (c == b'.' && i + 1 < n && b[i + 1].is_ascii_digit()) {
            let mut j = i;
            while j < n {
                let d = b[j];
                if is_ident(d) || d == b'.' || d == b'\'' {
                    j += 1;
                } else if (d == b'+' || d == b'-') && {
                    let prev = b[j - 1] | 0x20;
                    let hex = j - i > 1 && b[i] == b'0' && (b[i + 1] | 0x20) == b'x';
                    (!hex && prev == b'e') || (hex && prev == b'p')
                } {
                    j += 1;
                } else {
                    break;
                }
            }
            out.push(i..j, Tok::Number);
            i = j;
            continue;
        }
        // Identifiers
        if is_ident_start(c) {
            let mut j = i;
            while j < n && is_ident(b[j]) {
                j += 1;
            }
            let word = &text[i..j];
            let tok = if let Some(tok) = classify_c_ident(word, glsl) {
                tok
            } else {
                // Look ahead for '(' -> function call / definition.
                let mut k = j;
                while k < n && (b[k] == b' ' || b[k] == b'\t') {
                    k += 1;
                }
                if k < n && b[k] == b'(' {
                    Tok::Function
                } else if word.len() > 1
                    && word.bytes().any(|x| x.is_ascii_uppercase())
                    && !word.bytes().any(|x| x.is_ascii_lowercase())
                {
                    Tok::Constant
                } else {
                    Tok::Normal
                }
            };
            out.push_word(i..j, tok);
            i = j;
            continue;
        }
        if c >= 0x80 {
            let l = utf8_len(c).min(n - i);
            out.push(i..i + l, Tok::Normal);
            i += l;
            continue;
        }
        // Punctuation
        let tok = match c {
            b'{' | b'}' | b'(' | b')' | b'[' | b']' | b';' | b',' => Tok::Normal,
            _ => Tok::Punct,
        };
        out.push(i..i + 1, tok);
        i += 1;
    }
    out.spans
}

// ---------------------------------------------------------------------------
// Makefiles

const MAKE_DIRECTIVES: &[&str] = &[
    "include", "-include", "sinclude", "ifeq", "ifneq", "ifdef", "ifndef", "else", "endif",
    "define", "endef", "export", "unexport", "override", "private", "vpath", "undefine",
];

/// Lex a `$(...)`, `${...}` or `$x` reference starting at `i` (which is `$`).
fn make_var_end(b: &[u8], i: usize) -> usize {
    let n = b.len();
    if i + 1 >= n {
        return n;
    }
    let open = b[i + 1];
    let close = match open {
        b'(' => b')',
        b'{' => b'}',
        b'\n' => return i + 1,
        _ => return i + 2,
    };
    let mut depth = 0;
    let mut j = i + 1;
    while j < n && b[j] != b'\n' {
        if b[j] == open {
            depth += 1;
        } else if b[j] == close {
            depth -= 1;
            if depth == 0 {
                return j + 1;
            }
        }
        j += 1;
    }
    j
}

fn lex_make(text: &str) -> Spans {
    let b = text.as_bytes();
    let n = b.len();
    let mut out = SpanBuilder::new(n / 8);
    let mut i = 0;
    let mut continued = false; // previous line ended with a backslash
    while i < n {
        let line_end = memchr_newline(b, i);
        let line = &text[i..line_end];
        let recipe = b[i] == b'\t';
        let was_continued = continued;
        continued = line.ends_with('\\');

        // Find the first non-space word to detect directives and assignments.
        let trimmed_off = line.len() - line.trim_start().len();
        let first_word_end = line[trimmed_off..]
            .find(|ch: char| ch.is_whitespace())
            .map(|p| p + trimmed_off)
            .unwrap_or(line.len());
        let first_word = &line[trimmed_off..first_word_end];

        let mut pos = i;
        if !recipe && !was_continued && MAKE_DIRECTIVES.contains(&first_word) {
            out.push(i..i + first_word_end, Tok::Keyword);
            pos = i + first_word_end;
        } else if !recipe && !was_continued && !line.trim_start().starts_with('#') {
            // Assignment or rule?
            if let Some(op) = find_make_operator(line) {
                let (op_pos, op_len, is_rule) = op;
                let head = i..i + op_pos;
                if is_rule {
                    let name = line[..op_pos].trim();
                    let tok = if name.starts_with('.') && name.chars().all(|c| c.is_ascii_uppercase() || c == '.' || c == '_') {
                        Tok::Keyword
                    } else {
                        Tok::Target
                    };
                    lex_make_segment(text, b, head, tok, &mut out);
                } else {
                    lex_make_segment(text, b, head, Tok::Variable, &mut out);
                }
                out.push(i + op_pos..i + op_pos + op_len, Tok::Punct);
                pos = i + op_pos + op_len;
            }
        }
        lex_make_segment(text, b, pos..line_end, Tok::Normal, &mut out);
        if line_end < n {
            out.push(line_end..line_end + 1, Tok::Normal);
        }
        i = line_end + 1;
    }
    out.spans
}

fn memchr_newline(b: &[u8], from: usize) -> usize {
    b[from..]
        .iter()
        .position(|&c| c == b'\n')
        .map(|p| p + from)
        .unwrap_or(b.len())
}

/// Returns (byte offset, length, is_rule) for the first top-level `=`, `:=`,
/// `::=`, `?=`, `+=`, `!=` or rule `:` in a Makefile line.
fn find_make_operator(line: &str) -> Option<(usize, usize, bool)> {
    let b = line.as_bytes();
    let mut depth = 0i32;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'#' if depth == 0 => return None,
            b'$' if i + 1 < b.len() && (b[i + 1] == b'(' || b[i + 1] == b'{') => {
                depth += 1;
                i += 2;
                continue;
            }
            b')' | b'}' if depth > 0 => depth -= 1,
            b'=' if depth == 0 => {
                let start = if i > 0 && matches!(b[i - 1], b'?' | b'+' | b'!') {
                    i - 1
                } else {
                    i
                };
                return Some((start, i + 1 - start, false));
            }
            b':' if depth == 0 => {
                if line[i..].starts_with("::=") {
                    return Some((i, 3, false));
                }
                if line[i..].starts_with(":=") {
                    return Some((i, 2, false));
                }
                // Windows drive letter like C:\ is unlikely in Makefiles; treat as rule.
                let len = if line[i..].starts_with("::") { 2 } else { 1 };
                return Some((i, len, true));
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Lex a part of a Makefile line: comments, variable references and
/// the rest as `base`.
fn lex_make_segment(text: &str, b: &[u8], range: Range<usize>, base: Tok, out: &mut SpanBuilder) {
    let mut i = range.start;
    let end = range.end;
    let mut plain_start = i;
    while i < end {
        let c = b[i];
        if c == b'#' && (i == 0 || b[i - 1] != b'\\') && base != Tok::Str {
            out.push(plain_start..i, base);
            out.push(i..end, Tok::Comment);
            return;
        }
        if c == b'$' {
            out.push(plain_start..i, base);
            let j = make_var_end(b, i).min(end);
            out.push(i..j, Tok::Constant);
            i = j;
            plain_start = i;
            continue;
        }
        if c == b'"' || c == b'\'' {
            out.push(plain_start..i, base);
            let mut j = i + 1;
            while j < end && b[j] != c {
                j += utf8_len(b[j]);
            }
            let j = (j + 1).min(end);
            out.push(i..j, Tok::Str);
            i = j;
            plain_start = i;
            continue;
        }
        i += utf8_len(c);
    }
    let _ = text;
    out.push(plain_start..end.min(b.len()), base);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn covers(text: &str, spans: &Spans) {
        let mut pos = 0;
        for (r, _) in spans {
            assert_eq!(r.start, pos, "gap in spans for {text:?}: {spans:?}");
            assert!(text.is_char_boundary(r.start) && text.is_char_boundary(r.end));
            pos = r.end;
        }
        assert_eq!(pos, text.len());
    }

    #[test]
    fn c_spans_cover_text() {
        let src = "#include <stdio.h>\n/* héllo */ int main(void) { printf(\"hi ✓ %d\\n\", 0x1Fu + 1e-3f); // c\n return MAX_SIZE; }\n'é' \"unterminated\n";
        let spans = tokenize(Language::C, src);
        covers(src, &spans);
        let find = |s: &str| {
            let at = src.find(s).unwrap();
            spans.iter().find(|(r, _)| r.start <= at && at < r.end).unwrap().1
        };
        assert_eq!(find("#include"), Tok::Preproc);
        assert_eq!(find("<stdio.h>"), Tok::Str);
        assert_eq!(find("int"), Tok::Type);
        assert_eq!(find("main"), Tok::Function);
        assert_eq!(find("return"), Tok::Keyword);
        assert_eq!(find("MAX_SIZE"), Tok::Constant);
        assert_eq!(find("0x1Fu"), Tok::Number);
        assert_eq!(find("// c"), Tok::Comment);
    }

    #[test]
    fn glsl_types() {
        let src = "layout(location = 0) in vec3 pos;\nuniform sampler2D tex;\nvoid main() { gl_Position = vec4(pos, 1.0); }";
        let spans = tokenize(Language::Glsl, src);
        covers(src, &spans);
        let find = |s: &str| {
            let at = src.find(s).unwrap();
            spans.iter().find(|(r, _)| r.start <= at && at < r.end).unwrap().1
        };
        assert_eq!(find("layout"), Tok::Keyword);
        assert_eq!(find("vec3"), Tok::Type);
        assert_eq!(find("sampler2D"), Tok::Type);
        assert_eq!(find("gl_Position"), Tok::Constant);
    }

    #[test]
    fn make_spans() {
        let src = "CC ?= cc\nCFLAGS := -O2 # opt\n.PHONY: all\nall: main.o $(OBJS)\n\t$(CC) -o $@ $^ 'x'\nifeq ($(OS),Windows_NT)\nendif\nλ: x\n";
        let spans = tokenize(Language::Make, src);
        covers(src, &spans);
        let find = |s: &str| {
            let at = src.find(s).unwrap();
            spans.iter().find(|(r, _)| r.start <= at && at < r.end).unwrap().1
        };
        assert_eq!(find("CC ?="), Tok::Variable);
        assert_eq!(find("# opt"), Tok::Comment);
        assert_eq!(find(".PHONY"), Tok::Keyword);
        assert_eq!(find("all:"), Tok::Target);
        assert_eq!(find("$(OBJS)"), Tok::Constant);
        assert_eq!(find("ifeq"), Tok::Keyword);
    }

    #[test]
    fn match_overlay() {
        let theme = Theme::dark();
        let src = "int abc = abc;";
        let job = layout_job(Language::C, src, FontId::monospace(12.0), &theme, &[4..7, 10..13], Some(1), &[]);
        assert_eq!(job.text, src);
        let bgs: Vec<_> = job
            .sections
            .iter()
            .filter(|s| s.format.background != Color32::TRANSPARENT)
            .map(|s| &src[s.byte_range.start.0..s.byte_range.end.0])
            .collect();
        assert_eq!(bgs, vec!["abc", "abc"]);
    }
}
