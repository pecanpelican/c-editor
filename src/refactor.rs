//! C refactorings that clangd doesn't offer for C: extract function,
//! inline variable, change signature, safe delete, and #include updates
//! when a header is renamed. Everything here works on plain text with a
//! small C tokenizer, so it is fast and testable; the app supplies
//! reference locations from clangd where a refactoring spans files.

use std::ops::Range;
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// Tokens

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TK {
    Ident,
    Number,
    Str,
    Char,
    Punct,
    /// A whole preprocessor line (with continuations).
    Preproc,
}

#[derive(Clone, Copy, Debug)]
pub struct Tok {
    pub kind: TK,
    pub start: usize,
    pub end: usize,
}

const PUNCT3: &[&str] = &["<<=", ">>=", "..."];
const PUNCT2: &[&str] = &[
    "->", "++", "--", "<<", ">>", "<=", ">=", "==", "!=", "&&", "||", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "##",
];

/// Tokenize C source, skipping whitespace and comments.
pub fn lex(text: &str) -> Vec<Tok> {
    let b = text.as_bytes();
    let n = b.len();
    let mut out = Vec::new();
    let mut i = 0;
    let mut line_start = true;
    while i < n {
        let c = b[i];
        if c == b'\n' {
            line_start = true;
            i += 1;
            continue;
        }
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'/' && i + 1 < n && b[i + 1] == b'/' {
            while i < n && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && i + 1 < n && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < n && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(n);
            continue;
        }
        let start = i;
        if c == b'#' && line_start {
            // Preprocessor line, honoring backslash continuations and comments.
            while i < n && b[i] != b'\n' {
                if b[i] == b'\\' && i + 1 < n && b[i + 1] == b'\n' {
                    i += 2;
                    continue;
                }
                if b[i] == b'/' && i + 1 < n && b[i + 1] == b'*' {
                    i += 2;
                    while i + 1 < n && !(b[i] == b'*' && b[i + 1] == b'/') {
                        i += 1;
                    }
                    i = (i + 2).min(n);
                    continue;
                }
                i += 1;
            }
            out.push(Tok { kind: TK::Preproc, start, end: i });
            continue;
        }
        line_start = false;
        if c == b'_' || c.is_ascii_alphabetic() || c >= 0x80 {
            while i < n && (b[i] == b'_' || b[i].is_ascii_alphanumeric() || b[i] >= 0x80) {
                i += 1;
            }
            // String/char prefixes: L"..", u8"..".
            if i < n && (b[i] == b'"' || b[i] == b'\'') && matches!(&text[start..i], "L" | "u" | "U" | "u8") {
                // fall through to the literal below, keeping `start`
            } else {
                out.push(Tok { kind: TK::Ident, start, end: i });
                continue;
            }
        }
        let c = b[i];
        if c == b'"' || c == b'\'' {
            let q = c;
            i += 1;
            while i < n && b[i] != q && b[i] != b'\n' {
                if b[i] == b'\\' {
                    i += 1;
                }
                i += 1;
            }
            i = (i + 1).min(n);
            out.push(Tok { kind: if q == b'"' { TK::Str } else { TK::Char }, start, end: i });
            continue;
        }
        if c.is_ascii_digit() || (c == b'.' && i + 1 < n && b[i + 1].is_ascii_digit()) {
            while i < n {
                let d = b[i];
                let exp_sign = (d == b'+' || d == b'-') && matches!(b[i - 1], b'e' | b'E' | b'p' | b'P');
                if d.is_ascii_alphanumeric() || d == b'.' || d == b'_' || exp_sign {
                    i += 1;
                } else {
                    break;
                }
            }
            out.push(Tok { kind: TK::Number, start, end: i });
            continue;
        }
        let rest = &text[i..];
        let len = PUNCT3
            .iter()
            .find(|p| rest.starts_with(**p))
            .map(|p| p.len())
            .or_else(|| PUNCT2.iter().find(|p| rest.starts_with(**p)).map(|p| p.len()))
            .unwrap_or_else(|| rest.chars().next().map(char::len_utf8).unwrap_or(1));
        i += len;
        out.push(Tok { kind: TK::Punct, start, end: i });
    }
    out
}

pub struct Src<'a> {
    pub text: &'a str,
    pub toks: Vec<Tok>,
    /// Brace depth before each token.
    pub depth: Vec<u32>,
    /// Index of the matching bracket for each bracket token.
    pub mate: Vec<Option<usize>>,
}

impl<'a> Src<'a> {
    pub fn new(text: &'a str) -> Self {
        let toks = lex(text);
        let mut depth = Vec::with_capacity(toks.len());
        let mut mate = vec![None; toks.len()];
        let mut d = 0u32;
        let mut stack: Vec<usize> = Vec::new();
        for (i, t) in toks.iter().enumerate() {
            let s = &text[t.start..t.end];
            if s == "}" {
                d = d.saturating_sub(1);
            }
            depth.push(d);
            if s == "{" {
                d += 1;
            }
            if t.kind == TK::Punct {
                match s {
                    "{" | "(" | "[" => stack.push(i),
                    "}" | ")" | "]" => {
                        let want = match s {
                            "}" => "{",
                            ")" => "(",
                            _ => "[",
                        };
                        // Pop to the matching opener (tolerates broken code).
                        if let Some(k) = stack.iter().rposition(|&j| &text[toks[j].start..toks[j].end] == want) {
                            let j = stack[k];
                            stack.truncate(k);
                            mate[i] = Some(j);
                            mate[j] = Some(i);
                        }
                    }
                    _ => {}
                }
            }
        }
        Src { text, toks, depth, mate }
    }

    pub fn s(&self, i: usize) -> &'a str {
        let t = self.toks[i];
        &self.text[t.start..t.end]
    }

    fn is(&self, i: usize, s: &str) -> bool {
        i < self.toks.len() && self.s(i) == s
    }

    /// Index of the token starting at or containing `byte`.
    pub fn tok_at(&self, byte: usize) -> Option<usize> {
        let i = self.toks.partition_point(|t| t.end <= byte);
        (i < self.toks.len() && self.toks[i].start <= byte).then_some(i)
    }

    /// First token index starting at or after `byte`.
    fn first_from(&self, byte: usize) -> usize {
        self.toks.partition_point(|t| t.start < byte)
    }

    fn ident(&self, i: usize) -> bool {
        i < self.toks.len() && self.toks[i].kind == TK::Ident
    }
}

pub const KEYWORDS: &[&str] = &[
    "auto", "break", "case", "char", "const", "continue", "default", "do", "double", "else", "enum", "extern", "float", "for", "goto", "if",
    "inline", "int", "long", "register", "restrict", "return", "short", "signed", "sizeof", "static", "struct", "switch", "typedef",
    "union", "unsigned", "void", "volatile", "while", "_Bool", "_Complex", "_Imaginary", "_Alignas", "_Alignof", "_Atomic", "_Generic",
    "_Noreturn", "_Static_assert", "_Thread_local", "bool", "true", "false", "alignas", "alignof", "constexpr", "nullptr",
    "static_assert", "thread_local", "typeof", "typeof_unqual",
];

/// Keywords that start a statement, never a declaration.
const STMT_WORDS: &[&str] = &["return", "goto", "case", "default", "if", "while", "for", "switch", "else", "do", "break", "continue", "sizeof"];

/// Storage-class and function specifiers, kept per declaration.
const SPECIFIERS: &[&str] = &["static", "extern", "inline", "_Noreturn", "__inline", "__inline__", "register", "auto", "_Thread_local", "thread_local"];

const ASSIGN_OPS: &[&str] = &["=", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "<<=", ">>="];

/// Is `s` a valid C identifier that isn't a keyword?
pub fn is_identifier(s: &str) -> bool {
    let mut c = s.chars();
    matches!(c.next(), Some(ch) if ch == '_' || ch.is_ascii_alphabetic())
        && c.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
        && !KEYWORDS.contains(&s)
}

// ---------------------------------------------------------------------------
// Functions

#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    /// The whole parameter as written, e.g. `const char *name`.
    pub text: String,
    /// Its name, if it has one.
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct FnDecl {
    /// `static`, `inline`... as written.
    pub specifiers: String,
    /// Return type, e.g. `const char *`.
    pub ret: String,
    pub name: String,
    pub params: Vec<Param>,
    pub variadic: bool,
    /// Byte where the declaration starts (first specifier or type token).
    pub start: usize,
    /// Byte range of the parentheses, inclusive.
    pub parens: Range<usize>,
    /// Byte range of the body braces, inclusive, for a definition.
    pub body: Option<Range<usize>>,
    /// Byte just after the `;` or `}` that ends it.
    pub end: usize,
}

impl FnDecl {
    /// `int add(int a, int b)` as it would be written.
    pub fn signature(&self) -> String {
        format_signature(&self.specifiers, &self.ret, &self.name, &self.params.iter().map(|p| p.text.clone()).collect::<Vec<_>>(), self.variadic)
    }
}

pub fn format_signature(spec: &str, ret: &str, name: &str, params: &[String], variadic: bool) -> String {
    let mut s = String::new();
    if !spec.trim().is_empty() {
        s.push_str(spec.trim());
        s.push(' ');
    }
    let ret = ret.trim();
    s.push_str(ret);
    if !ret.ends_with('*') {
        s.push(' ');
    }
    s.push_str(name);
    s.push('(');
    let mut ps: Vec<String> = params.iter().map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect();
    if variadic {
        ps.push("...".into());
    }
    if ps.is_empty() {
        s.push_str("void");
    } else {
        s.push_str(&ps.join(", "));
    }
    s.push(')');
    s
}

/// Split the tokens between parens `open`..`close` at top-level commas.
/// Returns byte ranges, trimmed.
fn split_commas(src: &Src, open: usize, close: usize) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut i = open + 1;
    let mut start = i;
    while i < close {
        let s = src.s(i);
        if matches!(s, "(" | "[" | "{")
            && let Some(m) = src.mate[i]
        {
            i = m + 1;
            continue;
        }
        if s == "," {
            if start < i {
                out.push(src.toks[start].start..src.toks[i - 1].end);
            } else {
                out.push(src.toks[i].start..src.toks[i].start);
            }
            start = i + 1;
        }
        i += 1;
    }
    if start < close {
        out.push(src.toks[start].start..src.toks[close - 1].end);
    } else if !out.is_empty() {
        out.push(src.toks[close].start..src.toks[close].start);
    }
    out
}

/// The name in a parameter declaration such as `int (*cb)(int)` or `char *s[]`.
pub fn param_name(text: &str) -> String {
    let src = Src::new(text);
    let n = src.toks.len();
    // Function pointer: name follows `(*`.
    for i in 0..n {
        if src.s(i) == "(" && src.is(i + 1, "*") {
            let mut j = i + 1;
            while src.is(j, "*") || (src.ident(j) && matches!(src.s(j), "const" | "volatile" | "restrict")) {
                j += 1;
            }
            if src.ident(j) {
                return src.s(j).to_string();
            }
        }
    }
    // Otherwise the last identifier before any `[`, that isn't the only word.
    let end = (0..n).find(|&i| src.s(i) == "[").unwrap_or(n);
    let idents: Vec<usize> = (0..end).filter(|&i| src.ident(i)).collect();
    if let Some(&last) = idents.last() {
        let w = src.s(last);
        let type_only = idents.len() == 1
            || KEYWORDS.contains(&w)
            || (last > 0 && matches!(src.s(last - 1), "struct" | "union" | "enum") && idents.len() == 2);
        if !type_only {
            return w.to_string();
        }
    }
    String::new()
}

/// Where a declaration or statement that contains token `i` starts at
/// file scope (just after the previous `;`, `}` or preprocessor line).
fn decl_start_tok(src: &Src, i: usize) -> usize {
    let mut j = i;
    while j > 0 {
        let p = j - 1;
        let t = src.toks[p];
        if t.kind == TK::Preproc || (src.depth[p] == src.depth[i] && matches!(src.s(p), ";" | "}" | "{")) {
            break;
        }
        // `)` of an attribute or a `}` closing a struct return type: keep going.
        j = p;
    }
    j
}

/// Parse the function declaration or definition whose name is the token at `name_tok`.
pub fn parse_fn_at(src: &Src, name_tok: usize) -> Option<FnDecl> {
    if !src.ident(name_tok) || !src.is(name_tok + 1, "(") || src.depth[name_tok] != 0 {
        return None;
    }
    let open = name_tok + 1;
    let close = src.mate[open]?;
    let start_tok = decl_start_tok(src, name_tok);
    // Must have a return type (or at least a specifier) before the name.
    if start_tok == name_tok {
        return None;
    }
    let mut specs = Vec::new();
    let mut k = start_tok;
    while k < name_tok && SPECIFIERS.contains(&src.s(k)) {
        specs.push(src.s(k));
        k += 1;
    }
    if src.s(start_tok) == "typedef" {
        return None;
    }
    let ret = if k < name_tok { src.text[src.toks[k].start..src.toks[name_tok].start].trim().to_string() } else { String::new() };
    let mut params = Vec::new();
    let mut variadic = false;
    for r in split_commas(src, open, close) {
        let t = src.text[r].trim().to_string();
        if t == "..." {
            variadic = true;
        } else if !(t.is_empty() || t == "void") {
            params.push(Param { name: param_name(&t), text: t });
        }
    }
    // After `)`: attributes, then `{`, `;` or `,`.
    let mut j = close + 1;
    while src.ident(j) && src.s(j).starts_with("__") {
        j += 1;
        if src.is(j, "(")
            && let Some(m) = src.mate[j]
        {
            j = m + 1;
        }
    }
    let (body, end) = if src.is(j, "{") {
        let m = src.mate[j]?;
        (Some(src.toks[j].start..src.toks[m].end), src.toks[m].end)
    } else if src.is(j, ";") || src.is(j, ",") {
        (None, src.toks[j].end)
    } else {
        return None;
    };
    Some(FnDecl {
        specifiers: specs.join(" "),
        ret,
        name: src.s(name_tok).to_string(),
        params,
        variadic,
        start: src.toks[start_tok].start,
        parens: src.toks[open].start..src.toks[close].end,
        body,
        end,
    })
}

/// The function definition whose body contains `byte`.
pub fn enclosing_fn(src: &Src, byte: usize) -> Option<FnDecl> {
    let i = src.tok_at(byte).unwrap_or_else(|| src.first_from(byte).saturating_sub(1));
    // Walk back to the depth-0 `{` before `i`.
    let mut j = i.min(src.toks.len().saturating_sub(1));
    loop {
        if src.depth[j] == 0 && src.s(j) == "{" {
            let m = src.mate[j]?;
            if src.toks[m].end < byte {
                return None;
            }
            // Find the name: the identifier before the parameter list's `(`.
            let mut k = j;
            while k > 0 {
                k -= 1;
                if src.s(k) == ")" {
                    let o = src.mate[k]?;
                    if o > 0 && src.ident(o - 1) && !src.s(o - 1).starts_with("__") {
                        return parse_fn_at(src, o - 1);
                    }
                    k = o;
                } else if !src.ident(k) {
                    return None;
                }
            }
            return None;
        }
        if j == 0 {
            return None;
        }
        j -= 1;
    }
}

// ---------------------------------------------------------------------------
// Occurrences

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UseKind {
    Definition,
    Declaration,
    Call,
    Write,
    Read,
    /// Inside a #define or other preprocessor line.
    Macro,
}

impl UseKind {
    pub fn label(self) -> &'static str {
        match self {
            UseKind::Definition => "definition",
            UseKind::Declaration => "declaration",
            UseKind::Call => "call",
            UseKind::Write => "write",
            UseKind::Read => "read",
            UseKind::Macro => "macro",
        }
    }
}

/// Does the identifier at token `i` get assigned or have its address taken?
fn is_write(src: &Src, i: usize) -> bool {
    // Prefix ++/-- or unary &.
    if i > 0 {
        let p = src.s(i - 1);
        if p == "++" || p == "--" {
            return true;
        }
        if p == "&" {
            let unary = i < 2 || {
                let pp = src.toks[i - 2];
                pp.kind == TK::Punct && !matches!(src.s(i - 2), ")" | "]")
                    || (pp.kind == TK::Ident && STMT_WORDS.contains(&src.s(i - 2)))
            };
            if unary {
                return true;
            }
        }
    }
    // Walk `.member` and `[index]` (but not `->`, which writes the pointee).
    let mut j = i + 1;
    loop {
        if src.is(j, ".") && src.ident(j + 1) {
            j += 2;
        } else if src.is(j, "[")
            && let Some(m) = src.mate[j]
        {
            j = m + 1;
        } else {
            break;
        }
    }
    j < src.toks.len() && (ASSIGN_OPS.contains(&src.s(j)) || matches!(src.s(j), "++" | "--"))
}

/// How the identifier at `byte` is used.
pub fn classify(src: &Src, byte: usize) -> Option<UseKind> {
    let i = src.tok_at(byte)?;
    if src.toks[i].kind == TK::Preproc {
        return Some(UseKind::Macro);
    }
    if src.toks[i].kind != TK::Ident {
        return None;
    }
    let call = src.is(i + 1, "(");
    if src.depth[i] == 0 {
        if call && let Some(f) = parse_fn_at(src, i) {
            return Some(if f.body.is_some() { UseKind::Definition } else { UseKind::Declaration });
        }
        // Inside a struct (depth 0 for braces of the struct is >0, so here
        // it's a file-scope declaration or initializer).
        let st = decl_start_tok(src, i);
        if st < i && !src.text[src.toks[st].start..src.toks[i].start].contains('=') {
            return Some(UseKind::Declaration);
        }
        return Some(if is_write(src, i) { UseKind::Write } else { UseKind::Read });
    }
    if call {
        return Some(UseKind::Call);
    }
    if i > 0 && src.ident(i - 1) && !STMT_WORDS.contains(&src.s(i - 1)) && !src.is(i - 2, ".") && !src.is(i - 2, "->") {
        return Some(UseKind::Declaration);
    }
    Some(if is_write(src, i) { UseKind::Write } else { UseKind::Read })
}

// ---------------------------------------------------------------------------
// Local variables

#[derive(Clone, Debug)]
pub struct LocalVar {
    pub name: String,
    /// Base type without storage class, e.g. `const struct node`.
    pub base: String,
    /// Declarator pointer stars, e.g. `*` or `**`.
    pub stars: String,
    /// Array suffix, e.g. `[16]`.
    pub array: String,
    /// Token index of the name.
    pub tok: usize,
    /// Bytes where the name is visible.
    pub scope: Range<usize>,
    /// Initializer byte range, if any.
    pub init: Option<Range<usize>>,
    /// Token range of the whole declaration statement (through `;`).
    pub stmt: Range<usize>,
    /// How many variables the statement declares.
    pub siblings: usize,
    pub is_param: bool,
}

impl LocalVar {
    /// The type for a variable or return value, e.g. `char *`.
    pub fn value_type(&self) -> String {
        if !self.array.is_empty() {
            return format!("{} *", self.base);
        }
        if self.stars.is_empty() { self.base.clone() } else { format!("{} {}", self.base, self.stars) }
    }

    /// A parameter declaring a variable like this one, named `name`.
    pub fn as_param(&self, name: &str, by_pointer: bool) -> String {
        let ptr = if by_pointer { "*" } else { "" };
        format!("{} {}{ptr}{name}{}", self.base, self.stars, self.array)
    }
}

/// The end byte of the scope a declaration at token `i` lives in.
fn scope_end(src: &Src, i: usize) -> usize {
    // Innermost enclosing `{`.
    let mut j = i;
    let d = src.depth[i];
    while j > 0 {
        j -= 1;
        if src.s(j) == "{" && src.depth[j] < d {
            return src.mate[j].map(|m| src.toks[m].start).unwrap_or(src.text.len());
        }
    }
    src.text.len()
}

/// Parse a declaration statement starting at token `s` inside a function.
fn parse_decl_stmt(src: &Src, s: usize, limit: usize, out: &mut Vec<LocalVar>) -> Option<usize> {
    if !src.ident(s) || STMT_WORDS.contains(&src.s(s)) || src.s(s) == "typedef" {
        return None;
    }
    // Run of identifiers and `*`, allowing `struct X`.
    let mut j = s;
    let mut idents = 0;
    while j < limit && (src.ident(j) || src.is(j, "*")) {
        if src.ident(j) {
            idents += 1;
        }
        j += 1;
    }
    if idents < 2 || j >= limit || !matches!(src.s(j), "=" | ";" | "," | "[") {
        return None;
    }
    // The first declarator's name is the last identifier of the run.
    let name0 = (s..j).rev().find(|&k| src.ident(k))?;
    let mut base_end = name0;
    while base_end > s && src.is(base_end - 1, "*") {
        base_end -= 1;
    }
    if base_end == s {
        return None;
    }
    let base_words: Vec<&str> = (s..base_end).map(|k| src.s(k)).filter(|w| !SPECIFIERS.contains(w)).collect();
    let base = base_words.join(" ").replace(" *", "*");
    // Find the statement's end `;` at this level.
    let mut e = j;
    while e < limit && src.s(e) != ";" {
        if matches!(src.s(e), "(" | "[" | "{")
            && let Some(m) = src.mate[e]
        {
            e = m;
        }
        e += 1;
    }
    if e >= limit {
        return None;
    }
    let scope = src.toks[s].start..scope_end(src, s);
    let first = out.len();
    // Each declarator: `*`* name `[..]`* (= init)?
    let mut k = base_end;
    loop {
        let mut stars = String::new();
        while src.is(k, "*") {
            stars.push('*');
            k += 1;
        }
        while src.ident(k) && matches!(src.s(k), "const" | "volatile" | "restrict") {
            k += 1;
        }
        if !src.ident(k) {
            break;
        }
        let name_tok = k;
        k += 1;
        let arr_start = k;
        while src.is(k, "[")
            && let Some(m) = src.mate[k]
        {
            k = m + 1;
        }
        let array = if k > arr_start { src.text[src.toks[arr_start].start..src.toks[k - 1].end].to_string() } else { String::new() };
        let mut init = None;
        if src.is(k, "=") {
            let a = k + 1;
            let mut b = a;
            while b < e && src.s(b) != "," {
                if matches!(src.s(b), "(" | "[" | "{")
                    && let Some(m) = src.mate[b]
                {
                    b = m;
                }
                b += 1;
            }
            if b > a {
                init = Some(src.toks[a].start..src.toks[b - 1].end);
            }
            k = b;
        }
        out.push(LocalVar {
            name: src.s(name_tok).to_string(),
            base: base.clone(),
            stars,
            array,
            tok: name_tok,
            scope: src.toks[name_tok].end..scope.end,
            init,
            stmt: s..e + 1,
            siblings: 0,
            is_param: false,
        });
        if src.is(k, ",") {
            k += 1;
        } else {
            break;
        }
    }
    let n = out.len() - first;
    for v in &mut out[first..] {
        v.siblings = n;
    }
    Some(e)
}

/// All parameters and local variables of a function, in order.
pub fn locals(src: &Src, f: &FnDecl) -> Vec<LocalVar> {
    let mut out = Vec::new();
    let Some(body) = f.body.clone() else { return out };
    let bstart = src.first_from(body.start);
    let bend = src.tok_at(body.end - 1).unwrap_or(src.toks.len());
    // Parameters.
    let open = src.first_from(f.parens.start);
    if let Some(close) = src.mate[open] {
        for r in split_commas(src, open, close) {
            let text = src.text[r.clone()].trim();
            let name = param_name(text);
            if name.is_empty() {
                continue;
            }
            let ps = src.first_from(r.start);
            let pe = src.first_from(r.end);
            if let Some(nt) = (ps..pe).rev().find(|&k| src.s(k) == name) {
                let mut sub = Vec::new();
                // Reuse the declarator parser by pretending the parameter is a statement.
                let fake = format!("{text};");
                let fsrc = Src::new(&fake);
                parse_decl_stmt(&fsrc, 0, fsrc.toks.len(), &mut sub);
                let (base, stars, array) = match sub.into_iter().next() {
                    Some(v) => (v.base, v.stars, v.array),
                    None => (text[..text.rfind(&name).unwrap_or(0)].trim().to_string(), String::new(), String::new()),
                };
                out.push(LocalVar {
                    name,
                    base,
                    stars,
                    array,
                    tok: nt,
                    scope: body.clone(),
                    init: None,
                    stmt: ps..pe,
                    siblings: 1,
                    is_param: true,
                });
            }
        }
    }
    // Body: try a declaration at each statement start.
    let mut i = bstart + 1;
    let mut at_start = true;
    while i < bend {
        let s = src.s(i);
        if at_start
            && let Some(e) = parse_decl_stmt(src, i, bend, &mut out)
        {
            i = e + 1;
            at_start = true;
            continue;
        }
        // `for (int i = 0; ...)`
        if s == "for"
            && src.is(i + 1, "(")
            && let Some(close) = src.mate[i + 1]
        {
            let n = out.len();
            if let Some(e) = parse_decl_stmt(src, i + 2, close, &mut out) {
                // Visible through the loop body.
                let body_end = if src.is(close + 1, "{") {
                    src.mate[close + 1].map(|m| src.toks[m].start).unwrap_or(body.end)
                } else {
                    let mut k = close + 1;
                    while k < bend && src.s(k) != ";" {
                        k += 1;
                    }
                    src.toks.get(k).map(|t| t.end).unwrap_or(body.end)
                };
                for v in &mut out[n..] {
                    v.scope.end = body_end;
                }
                i = e + 1;
                at_start = false;
                continue;
            }
            i += 2;
            at_start = false;
            continue;
        }
        at_start = matches!(s, ";" | "{" | "}" | ")" | ":" | "else" | "do") && src.toks[i].kind == TK::Punct
            || matches!(s, "else" | "do");
        // `)` only starts a statement after if/while/for headers; close enough
        // since a declaration can't follow an expression's `)` anyway.
        i += 1;
    }
    out
}

/// The variable that the identifier token `i` refers to, if it's a local.
fn resolve<'v>(src: &Src, vars: &'v [LocalVar], i: usize) -> Option<&'v LocalVar> {
    if !src.ident(i) || (i > 0 && (src.is(i - 1, ".") || src.is(i - 1, "->"))) {
        return None;
    }
    let name = src.s(i);
    let at = src.toks[i].start;
    vars.iter()
        .filter(|v| v.name == name && (v.tok == i || (v.scope.start <= at && at < v.scope.end)))
        .max_by_key(|v| v.tok)
}

// ---------------------------------------------------------------------------
// Edits

/// A byte-range replacement in one file.
#[derive(Clone, Debug, PartialEq)]
pub struct Edit {
    pub range: Range<usize>,
    pub text: String,
}

pub fn apply(text: &str, edits: &[Edit]) -> String {
    let mut es: Vec<&Edit> = edits.iter().collect();
    es.sort_by_key(|e| std::cmp::Reverse(e.range.start));
    let mut s = text.to_string();
    for e in es {
        s.replace_range(e.range.clone(), &e.text);
    }
    s
}

fn line_start(text: &str, b: usize) -> usize {
    text[..b].rfind('\n').map(|i| i + 1).unwrap_or(0)
}

fn line_end_incl(text: &str, b: usize) -> usize {
    text[b..].find('\n').map(|i| b + i + 1).unwrap_or(text.len())
}

fn indent_of(line: &str) -> &str {
    &line[..line.len() - line.trim_start().len()]
}

// ---------------------------------------------------------------------------
// Extract function

#[derive(Clone, Debug)]
pub struct ExtractPlan {
    /// Parameters as written in the new function.
    pub params: Vec<String>,
    pub ret: String,
    /// Body lines (already re-indented by one level) without braces.
    pub body: String,
    /// Selection to replace, and how the call is written there.
    pub replace: Range<usize>,
    /// Call text, with `{name}` where the function name goes.
    pub call: String,
    /// Where the new function goes.
    pub insert_at: usize,
    pub brace_on_new_line: bool,
    pub expression: bool,
    pub notes: Vec<String>,
}

impl ExtractPlan {
    pub fn function_text(&self, name: &str, is_static: bool, ret: &str, indent: &str) -> String {
        let sig = format_signature(if is_static { "static" } else { "" }, ret, name, &self.params, false);
        let mut s = sig;
        s.push_str(if self.brace_on_new_line { "\n{\n" } else { " {\n" });
        for l in self.body.lines() {
            if l.trim().is_empty() {
                s.push('\n');
            } else {
                s.push_str(indent);
                s.push_str(l);
                s.push('\n');
            }
        }
        s.push_str("}\n\n");
        s
    }

    pub fn edits(&self, name: &str, is_static: bool, ret: &str, indent: &str) -> Vec<Edit> {
        vec![
            Edit { range: self.insert_at..self.insert_at, text: self.function_text(name, is_static, ret, indent) },
            Edit { range: self.replace.clone(), text: self.call.replace("{name}", name) },
        ]
    }
}

/// Plan extracting the selected statements (or expression) into a new function.
pub fn plan_extract(text: &str, sel: Range<usize>) -> Result<ExtractPlan, String> {
    let src = Src::new(text);
    // Trim the selection to tokens.
    let a = src.first_from(sel.start);
    let b = src.toks.partition_point(|t| t.end <= sel.end); // tokens fully inside
    if a >= b {
        return Err("Select the code to extract first".into());
    }
    let f = enclosing_fn(&src, src.toks[a].start).ok_or("The selection isn't inside a function body")?;
    let body = f.body.clone().unwrap();
    if src.toks[a].start <= body.start || src.toks[b - 1].end >= body.end {
        return Err("Select statements inside the function body".into());
    }
    // Balanced brackets, and no jumps out.
    for i in a..b {
        if let Some(m) = src.mate[i]
            && (m < a || m >= b)
        {
            return Err("The selection must contain whole blocks and expressions".into());
        }
    }
    let last = src.s(b - 1);
    let expression = !(last == ";" || last == "}") && !(a..b).any(|i| src.s(i) == ";");
    if !expression {
        let prev = src.s(a - 1);
        if !matches!(prev, ";" | "{" | "}" | ":") && src.toks[a - 1].kind != TK::Preproc {
            return Err("Start the selection at the beginning of a statement".into());
        }
    }
    let mut loops = 0i32;
    for i in a..b {
        match src.s(i) {
            "return" => return Err("The selection contains `return`; extract code without it".into()),
            "goto" => return Err("The selection contains `goto`".into()),
            "for" | "while" | "do" | "switch" => loops += 1,
            "break" | "continue" if loops == 0 => {
                return Err(format!("The selection contains `{}` for a loop outside it", src.s(i)));
            }
            _ => {}
        }
    }
    let vars = locals(&src, &f);
    let sel_start = src.toks[a].start;
    let sel_end = src.toks[b - 1].end;

    // Variables used in the selection, in order of first use.
    struct Use {
        var: usize,
        written: bool,
        first_is_plain_write: bool,
    }
    let mut uses: Vec<Use> = Vec::new();
    let mut inner_decl: Vec<usize> = Vec::new();
    for i in a..b {
        let Some(v) = resolve(&src, &vars, i) else { continue };
        let vi = vars.iter().position(|x| std::ptr::eq(x, v)).unwrap();
        if v.tok >= a && v.tok < b {
            if !inner_decl.contains(&vi) {
                inner_decl.push(vi);
            }
            continue;
        }
        let w = is_write(&src, i);
        let plain = src.is(i + 1, "=");
        match uses.iter_mut().find(|u| u.var == vi) {
            Some(u) => u.written |= w,
            None => uses.push(Use { var: vi, written: w, first_is_plain_write: plain }),
        }
    }
    // Used after the selection?
    let used_after = |vi: usize| -> bool {
        let v = &vars[vi];
        let from = src.first_from(sel_end);
        (from..src.toks.len()).take_while(|&i| src.toks[i].start < v.scope.end).any(|i| {
            src.s(i) == v.name && resolve(&src, &vars, i).is_some_and(|r| std::ptr::eq(r, v))
        })
    };
    for &vi in &inner_decl {
        if used_after(vi) {
            return Err(format!("`{}` is declared in the selection but used after it", vars[vi].name));
        }
    }
    let outs: Vec<usize> = uses.iter().filter(|u| u.written && vars[u.var].array.is_empty() && used_after(u.var)).map(|u| u.var).collect();
    if expression && !outs.is_empty() {
        return Err(format!("The expression changes `{}`; extract whole statements instead", vars[outs[0]].name));
    }
    let return_var = if outs.len() == 1 { Some(outs[0]) } else { None };
    let pointer_vars: Vec<usize> = if outs.len() > 1 { outs.clone() } else { Vec::new() };

    let mut params = Vec::new();
    let mut args = Vec::new();
    let mut prelude = String::new();
    for u in &uses {
        let v = &vars[u.var];
        let local_ok = Some(u.var) == return_var || !pointer_vars.contains(&u.var) && !used_after(u.var);
        if u.first_is_plain_write && local_ok && v.array.is_empty() {
            // Assigned before it's read: a local in the new function.
            prelude.push_str(&format!("{};\n", v.as_param(&v.name, false).trim()));
            continue;
        }
        let ptr = pointer_vars.contains(&u.var);
        params.push(v.as_param(&v.name, ptr).trim().to_string());
        args.push(if ptr { format!("&{}", v.name) } else { v.name.clone() });
    }

    // The body: the selected text, with pointer variables dereferenced.
    let (rs, re) = if expression { (sel_start, sel_end) } else { (line_start(text, sel_start), line_end_incl(text, sel_end)) };
    if !expression && text[rs..sel_start].trim() != "" {
        return Err("Start the selection at the beginning of a line".into());
    }
    let mut derefs: Vec<Edit> = Vec::new();
    for i in a..b {
        if let Some(v) = resolve(&src, &vars, i) {
            let vi = vars.iter().position(|x| std::ptr::eq(x, v)).unwrap();
            if pointer_vars.contains(&vi) {
                derefs.push(Edit { range: src.toks[i].start - rs..src.toks[i].end - rs, text: format!("(*{})", v.name) });
            }
        }
    }
    let raw = apply(&text[rs..re], &derefs);
    let ret;
    let mut body = String::new();
    let call;
    if expression {
        ret = guess_expr_type(&src, a, &vars);
        body = format!("return {};\n", raw.trim());
        call = format!("{{name}}({})", args.join(", "));
    } else {
        // Re-indent: drop the common indentation.
        let common = raw
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| indent_of(l).len())
            .min()
            .unwrap_or(0);
        body.push_str(&prelude);
        for l in raw.lines() {
            if l.trim().is_empty() {
                body.push('\n');
            } else {
                body.push_str(&l[common.min(indent_of(l).len())..]);
                body.push('\n');
            }
        }
        let ind = indent_of(&text[rs..]).to_string();
        match return_var {
            Some(vi) => {
                let v = &vars[vi];
                ret = v.value_type();
                body.push_str(&format!("return {};\n", v.name));
                call = format!("{ind}{} = {{name}}({});\n", v.name, args.join(", "));
            }
            None => {
                ret = "void".to_string();
                call = format!("{ind}{{name}}({});\n", args.join(", "));
            }
        }
    }
    // Insert above the enclosing function and any comment right above it.
    let mut insert_at = line_start(text, f.start);
    loop {
        if insert_at == 0 {
            break;
        }
        let prev_start = line_start(text, insert_at - 1);
        let l = text[prev_start..insert_at].trim();
        if l.starts_with("//") || l.starts_with("/*") || l.starts_with('*') || l.ends_with("*/") {
            insert_at = prev_start;
        } else {
            break;
        }
    }
    let between = &text[f.parens.end..f.body.as_ref().unwrap().start];
    let brace_on_new_line = between.contains('\n');
    let mut notes = Vec::new();
    if let Some(vi) = return_var {
        notes.push(format!("Returns the new value of `{}`", vars[vi].name));
    }
    for &vi in &pointer_vars {
        notes.push(format!("`{}` is passed by pointer because the code changes it", vars[vi].name));
    }
    Ok(ExtractPlan {
        params,
        ret,
        body,
        replace: rs..re,
        call,
        insert_at,
        brace_on_new_line,
        expression,
        notes,
    })
}

/// A best guess at an expression's type, from `T name = <expr>` or the operators in it.
fn guess_expr_type(src: &Src, a: usize, vars: &[LocalVar]) -> String {
    if a >= 2 && src.is(a - 1, "=")
        && let Some(v) = resolve(src, vars, a - 2)
    {
        return v.value_type();
    }
    "int".to_string()
}

/// A variable name for an extracted expression: the called function or
/// accessed member, else `value`.
pub fn suggest_name(expr: &str) -> String {
    let src = Src::new(expr.trim());
    let n = src.toks.len();
    let mut name = String::new();
    if n >= 3 && src.ident(0) && src.is(1, "(") && src.mate[1] == Some(n - 1) {
        let f = src.s(0);
        // get_width -> width, is_valid -> valid
        // get_width -> width; otherwise scale -> scale_result, since a local
        // named like the function would hide it in its own initializer.
        name = ["get_", "make_", "compute_", "calc_", "is_", "has_", "read_", "load_"]
            .iter()
            .find_map(|p| f.strip_prefix(p))
            .map(str::to_string)
            .unwrap_or_else(|| format!("{f}_result"));
    } else if n >= 3 && src.ident(n - 1) && matches!(src.s(n - 2), "." | "->") {
        name = src.s(n - 1).to_string();
    } else if src.toks.first().is_some_and(|t| t.kind == TK::Str) {
        name = "text".into();
    }
    if !is_identifier(&name) {
        name = "value".into();
    }
    name
}

// ---------------------------------------------------------------------------
// Inline variable

pub struct InlinePlan {
    pub name: String,
    pub edits: Vec<Edit>,
    pub uses: usize,
}

/// Plan replacing a local variable's uses with its initializer.
pub fn plan_inline(text: &str, byte: usize) -> Result<InlinePlan, String> {
    let src = Src::new(text);
    let i = src.tok_at(byte).filter(|&i| src.ident(i)).ok_or("Put the cursor on a local variable")?;
    let f = enclosing_fn(&src, byte).ok_or("Put the cursor on a local variable inside a function")?;
    let vars = locals(&src, &f);
    let v = resolve(&src, &vars, i).ok_or("Only local variables can be inlined")?;
    if v.is_param {
        return Err(format!("`{}` is a parameter", v.name));
    }
    let init = v.init.clone().ok_or_else(|| format!("`{}` has no initializer", v.name))?;
    if !v.array.is_empty() || text[init.clone()].trim_start().starts_with('{') {
        return Err("Arrays and braced initializers can't be inlined".into());
    }
    let mut uses = Vec::new();
    let from = v.tok + 1;
    for k in from..src.toks.len() {
        if src.toks[k].start >= v.scope.end {
            break;
        }
        if src.s(k) == v.name && resolve(&src, &vars, k).is_some_and(|r| std::ptr::eq(r, v)) {
            if is_write(&src, k) {
                return Err(format!("`{}` is changed after it's declared", v.name));
            }
            uses.push(k);
        }
    }
    let init_text = text[init.clone()].trim();
    let simple = {
        let is = Src::new(init_text);
        is.toks.len() == 1
            || (is.toks.len() >= 3 && is.mate[0] == Some(is.toks.len() - 1))
            || (is.ident(0) && is.is(1, "(") && is.mate[1] == Some(is.toks.len() - 1))
    };
    let replacement = if simple { init_text.to_string() } else { format!("({init_text})") };
    let mut edits: Vec<Edit> = uses.iter().map(|&k| Edit { range: src.toks[k].start..src.toks[k].end, text: replacement.clone() }).collect();
    // Remove the declaration.
    let stmt_a = src.toks[v.stmt.start].start;
    let stmt_b = src.toks[v.stmt.end - 1].end;
    if v.siblings == 1 {
        let ls = line_start(text, stmt_a);
        let le = line_end_incl(text, stmt_b);
        if text[ls..stmt_a].trim().is_empty() && text[stmt_b..le].trim().is_empty() {
            edits.push(Edit { range: ls..le, text: String::new() });
        } else {
            edits.push(Edit { range: stmt_a..stmt_b, text: String::new() });
        }
    } else {
        // Remove just this declarator and its comma.
        let name_start = src.toks[v.tok].start;
        let mut s = name_start;
        let mut k = v.tok;
        while k > 0 && src.is(k - 1, "*") {
            k -= 1;
            s = src.toks[k].start;
        }
        let end = src.toks[src.tok_at(init.end - 1).unwrap()].end;
        let next = src.first_from(end);
        if src.is(next, ",") {
            let after = src.toks[next + 1].start;
            edits.push(Edit { range: s..after, text: String::new() });
        } else if k > 0 && src.is(k - 1, ",") {
            edits.push(Edit { range: src.toks[k - 1].start..end, text: String::new() });
        }
    }
    Ok(InlinePlan { name: v.name.clone(), uses: uses.len(), edits })
}

// ---------------------------------------------------------------------------
// Change signature

#[derive(Clone, Debug, PartialEq)]
pub struct NewParam {
    /// The parameter as it will be written, e.g. `int count`.
    pub text: String,
    /// Which old parameter it was (keeps call arguments), or None if new.
    pub from: Option<usize>,
    /// Argument for calls when the parameter is new.
    pub default: String,
}

#[derive(Clone, Debug)]
pub struct NewSig {
    pub ret: String,
    pub name: String,
    pub params: Vec<NewParam>,
}

#[derive(Default, Debug)]
pub struct SigEdits {
    pub edits: Vec<Edit>,
    pub decls: usize,
    pub calls: usize,
    /// Occurrences left as they are, with why.
    pub skipped: Vec<(usize, &'static str)>,
}

/// Rewrite one file's occurrences (`offsets`: byte starts of the function's
/// name) to the new signature.
pub fn change_signature_in(text: &str, offsets: &[usize], old_name: &str, new: &NewSig, old_param_count: usize) -> SigEdits {
    let src = Src::new(text);
    let mut out = SigEdits::default();
    let new_params: Vec<String> = new.params.iter().map(|p| p.text.clone()).collect();
    for &off in offsets {
        let Some(i) = src.tok_at(off) else { continue };
        if src.toks[i].kind == TK::Preproc {
            out.skipped.push((off, "inside a macro"));
            continue;
        }
        if src.s(i) != old_name {
            out.skipped.push((off, "not the function's name here"));
            continue;
        }
        if let Some(f) = parse_fn_at(&src, i) {
            let sig = format_signature(&f.specifiers, &new.ret, &new.name, &new_params, f.variadic);
            out.edits.push(Edit { range: f.start..f.parens.end, text: sig });
            out.decls += 1;
            continue;
        }
        if src.is(i + 1, "(")
            && src.depth[i] > 0
            && let Some(close) = src.mate[i + 1]
        {
            let old_args: Vec<String> = split_commas(&src, i + 1, close).into_iter().map(|r| text[r].trim().to_string()).collect();
            let old_args: Vec<String> = if old_args.len() == 1 && old_args[0].is_empty() { Vec::new() } else { old_args };
            let mut args: Vec<String> = new
                .params
                .iter()
                .map(|p| match p.from {
                    Some(k) => old_args.get(k).cloned().unwrap_or_else(|| p.default.clone()),
                    None => p.default.clone(),
                })
                .collect();
            // Extra arguments to a variadic function stay at the end.
            args.extend(old_args.iter().skip(old_param_count).cloned());
            out.edits.push(Edit { range: src.toks[i].start..src.toks[close].end, text: format!("{}({})", new.name, args.join(", ")) });
            out.calls += 1;
            continue;
        }
        if new.name != old_name {
            out.edits.push(Edit { range: src.toks[i].start..src.toks[i].end, text: new.name.clone() });
        }
        out.skipped.push((off, "used without a call (function pointer)"));
    }
    out
}

// ---------------------------------------------------------------------------
// Safe delete

/// Byte range to delete for the declaration whose name is at `off`:
/// a whole function definition or prototype, or a file-scope variable,
/// with its leading comment and one trailing blank line.
pub fn deletion_range(text: &str, off: usize) -> Option<Range<usize>> {
    let src = Src::new(text);
    let i = src.tok_at(off)?;
    let (start, end) = if let Some(f) = parse_fn_at(&src, i) {
        if src.text[f.end - 1..f.end] == *"," {
            return None; // `int a(void), b(void);`
        }
        (f.start, f.end)
    } else if src.depth[i] == 0 {
        let st = decl_start_tok(&src, i);
        let mut e = i;
        while e < src.toks.len() && src.s(e) != ";" {
            if src.s(e) == "," && src.depth[e] == 0 {
                return None; // several variables in one statement
            }
            if matches!(src.s(e), "(" | "[" | "{")
                && let Some(m) = src.mate[e]
            {
                e = m;
            }
            e += 1;
        }
        (src.toks[st].start, src.toks.get(e)?.end)
    } else {
        return None;
    };
    let mut a = line_start(text, start);
    if !text[a..start].trim().is_empty() {
        a = start;
    } else {
        // Leading comment lines.
        while a > 0 {
            let p = line_start(text, a - 1);
            let l = text[p..a].trim();
            if l.starts_with("//") || l.starts_with("/*") || l.starts_with('*') || l.ends_with("*/") {
                a = p;
            } else {
                break;
            }
        }
    }
    let mut b = line_end_incl(text, end);
    if !text[end..b].trim().is_empty() {
        b = end;
    } else if a == 0 || text[line_start(text, a - 1)..a].trim().is_empty() {
        // And one blank line after it, if there's one before it too.
        let nb = line_end_incl(text, b);
        if b < text.len() && text[b..nb].trim().is_empty() {
            b = nb;
        }
    }
    Some(a..b)
}

// ---------------------------------------------------------------------------
// #include updates

/// Edits to `#include "..."` lines in `includer` that point at `old`,
/// so they point at `new` instead.
pub fn include_edits(text: &str, includer: &Path, old: &Path, new: &Path, include_dirs: &[PathBuf]) -> Vec<Edit> {
    let mut out = Vec::new();
    let dir = includer.parent().unwrap_or(Path::new(""));
    let mut pos = 0;
    for line in text.split_inclusive('\n') {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix('#') {
            let rest = rest.trim_start();
            if let Some(after) = rest.strip_prefix("include").or_else(|| rest.strip_prefix("import")) {
                let after_trim = after.trim_start();
                if let Some(q) = after_trim.strip_prefix('"')
                    && let Some(close) = q.find('"')
                {
                    let inc = &q[..close];
                    // Byte offset of the include text within `line`.
                    let q_start = pos + (line.len() - q.len());
                    let candidates = std::iter::once(dir.to_path_buf()).chain(include_dirs.iter().cloned());
                    for base in candidates {
                        let full = base.join(inc);
                        if same_path(&full, old) {
                            let new_inc = relative_include(inc, &base, new);
                            out.push(Edit { range: q_start..q_start + inc.len(), text: new_inc });
                            break;
                        }
                    }
                }
            }
        }
        pos += line.len();
    }
    out
}

fn same_path(a: &Path, b: &Path) -> bool {
    normalize(a) == normalize(b)
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

/// How to write `new` in an include that was `old_inc` relative to `base`.
fn relative_include(old_inc: &str, base: &Path, new: &Path) -> String {
    let base = normalize(base);
    let new = normalize(new);
    if let Ok(rel) = new.strip_prefix(&base) {
        return rel.to_string_lossy().replace('\\', "/");
    }
    // Climb with ../ as needed.
    let bc: Vec<_> = base.components().collect();
    let nc: Vec<_> = new.components().collect();
    let common = bc.iter().zip(&nc).take_while(|(a, b)| a == b).count();
    if common == 0 {
        return old_inc.to_string();
    }
    let mut s = String::new();
    for _ in common..bc.len() {
        s.push_str("../");
    }
    let rest: PathBuf = nc[common..].iter().collect();
    s.push_str(&rest.to_string_lossy().replace('\\', "/"));
    s
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const CODE: &str = r#"#include <stdio.h>
/* Adds. */
static int add(int a, int b)
{
    return a + b;
}

int main(void)
{
    int x = 3, y;
    int total = 0;
    y = x * 2;
    total += add(x, y);
    total += y;
    printf("%d\n", total);
    return 0;
}
"#;

    #[test]
    fn lexes() {
        let t = lex("a->b /* c */ \"s\\\"\" 'x' 1.5e-3 <<= #x\n# define Y 1\nz");
        let s: Vec<&str> = t.iter().map(|t| &"a->b /* c */ \"s\\\"\" 'x' 1.5e-3 <<= #x\n# define Y 1\nz"[t.start..t.end]).collect();
        assert_eq!(s, vec!["a", "->", "b", "\"s\\\"\"", "'x'", "1.5e-3", "<<=", "#", "x", "# define Y 1", "z"]);
    }

    #[test]
    fn parses_functions() {
        let src = Src::new(CODE);
        let i = src.tok_at(CODE.find("add(int").unwrap()).unwrap();
        let f = parse_fn_at(&src, i).unwrap();
        assert_eq!(f.specifiers, "static");
        assert_eq!(f.ret, "int");
        assert_eq!(f.params, vec![Param { text: "int a".into(), name: "a".into() }, Param { text: "int b".into(), name: "b".into() }]);
        assert!(f.body.is_some());
        assert_eq!(f.signature(), "static int add(int a, int b)");
        let m = enclosing_fn(&src, CODE.find("total += y").unwrap()).unwrap();
        assert_eq!(m.name, "main");
        assert!(m.params.is_empty());
        assert_eq!(param_name("int (*cb)(void *, int)"), "cb");
        assert_eq!(param_name("const char *s[]"), "s");
        assert_eq!(param_name("struct node"), "");
        assert_eq!(param_name("unsigned"), "");
    }

    #[test]
    fn classifies() {
        let src = Src::new(CODE);
        let at = |s: &str| classify(&src, CODE.find(s).unwrap()).unwrap();
        assert_eq!(at("add(int"), UseKind::Definition);
        assert_eq!(at("add(x"), UseKind::Call);
        assert_eq!(at("y = x"), UseKind::Write);
        assert_eq!(at("total += y"), UseKind::Write);
        assert_eq!(at("y;\n    printf"), UseKind::Read);
        assert_eq!(at("total = 0"), UseKind::Declaration);
    }

    #[test]
    fn finds_locals() {
        let src = Src::new(CODE);
        let m = enclosing_fn(&src, CODE.find("total += y").unwrap()).unwrap();
        let v = locals(&src, &m);
        let names: Vec<&str> = v.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, vec!["x", "y", "total"]);
        assert_eq!(v[0].siblings, 2);
        assert_eq!(v[0].init.clone().map(|r| &CODE[r]), Some("3"));
        let add = enclosing_fn(&src, CODE.find("a + b").unwrap()).unwrap();
        let p = locals(&src, &add);
        assert_eq!(p.len(), 2);
        assert!(p[0].is_param);
        assert_eq!(p[0].base, "int");
        let code = "void f(const char **argv) {\n  for (int i = 0; i < 3; i++) { struct node *n = 0, arr[4]; }\n}\n";
        let src = Src::new(code);
        let f = enclosing_fn(&src, code.find("arr").unwrap()).unwrap();
        let v = locals(&src, &f);
        let d: Vec<String> = v.iter().map(|v| format!("{}|{}|{}|{}", v.name, v.base, v.stars, v.array)).collect();
        assert_eq!(d, vec!["argv|const char|**|", "i|int||", "n|struct node|*|", "arr|struct node||[4]"]);
    }

    #[test]
    fn extracts_statements_with_return_value() {
        let s = CODE.find("    y = x * 2;").unwrap();
        let e = CODE.find("    total += y;").unwrap() + "    total += y;".len();
        let p = plan_extract(CODE, s..e).map_err(|e| panic!("{e}")).unwrap();
        // total is changed and printed after: it is returned.
        assert_eq!(p.ret, "int");
        assert_eq!(p.params, vec!["int x", "int total"]);
        let out = apply(CODE, &p.edits("compute", true, &p.ret, "    "));
        assert!(out.contains("static int compute(int x, int total)\n{\n    int y;\n    y = x * 2;\n    total += add(x, y);\n    total += y;\n    return total;\n}\n\nint main"), "{out}");
        assert!(out.contains("    total = compute(x, total);\n    printf"), "{out}");
    }

    #[test]
    fn extract_uses_pointers_for_several_outputs() {
        let code = "void f(void)\n{\n    int a = 1, b = 2;\n    a++;\n    b += a;\n    g(a, b);\n}\n";
        let s = code.find("    a++").unwrap();
        let e = code.find("    g(").unwrap();
        let p = plan_extract(code, s..e).unwrap();
        assert_eq!(p.ret, "void");
        assert_eq!(p.params, vec!["int *a", "int *b"]);
        let out = apply(code, &p.edits("step", true, &p.ret, "    "));
        assert!(out.contains("    (*a)++;\n    (*b) += (*a);\n"), "{out}");
        assert!(out.contains("    step(&a, &b);\n"), "{out}");
    }

    #[test]
    fn extracts_expression() {
        let code = "int f(int w, int h) {\n    int area = w * h + 1;\n    return area;\n}\n";
        let s = code.find("w * h").unwrap();
        let p = plan_extract(code, s..s + "w * h + 1".len()).unwrap();
        assert!(p.expression);
        assert_eq!(p.ret, "int");
        let out = apply(code, &p.edits("calc", true, &p.ret, "    "));
        assert!(out.starts_with("static int calc(int w, int h) {\n    return w * h + 1;\n}\n\nint f"), "{out}");
        assert!(out.contains("int area = calc(w, h);"), "{out}");
    }

    #[test]
    fn extract_refuses_bad_selections() {
        let code = "int f(int n) {\n    int r = 0;\n    while (n) {\n        if (n == 3) break;\n        r = n;\n    }\n    return r;\n}\n";
        let s = code.find("        if").unwrap();
        let e = code.find("        r = n;").unwrap();
        assert!(plan_extract(code, s..e).unwrap_err().contains("break"));
        let s = code.find("    int r").unwrap();
        let e = code.find("    while").unwrap();
        assert!(plan_extract(code, s..e).unwrap_err().contains("used after"));
        let s = code.find("    return r").unwrap();
        assert!(plan_extract(code, s..code.len() - 2).is_err());
    }

    #[test]
    fn inlines_variable() {
        let code = "int f(int a) {\n    int t = a + 1;\n    return t * t;\n}\n";
        let p = plan_inline(code, code.find("t * t").unwrap()).unwrap();
        assert_eq!(p.uses, 2);
        assert_eq!(apply(code, &p.edits), "int f(int a) {\n    return (a + 1) * (a + 1);\n}\n");
        let code = "void f(void) {\n    int a = g(), b = 2;\n    h(a, b);\n}\n";
        let p = plan_inline(code, code.find("a, b").unwrap()).unwrap();
        assert_eq!(apply(code, &p.edits), "void f(void) {\n    int b = 2;\n    h(g(), b);\n}\n");
        let code = "void f(void) {\n    int a = 1;\n    a++;\n}\n";
        assert!(plan_inline(code, code.find("a++").unwrap()).is_err());
    }

    #[test]
    fn changes_signature() {
        let src_code = "int add(int a, int b);\nint add(int a, int b)\n{\n    return a + b;\n}\nvoid g(void) { int (*p)(int, int) = add; add(1, f(2, 3)); }\n";
        let offs: Vec<usize> = src_code.match_indices("add").map(|(i, _)| i).collect();
        let new = NewSig {
            ret: "long".into(),
            name: "sum".into(),
            params: vec![
                NewParam { text: "int b".into(), from: Some(1), default: String::new() },
                NewParam { text: "int a".into(), from: Some(0), default: String::new() },
                NewParam { text: "int scale".into(), from: None, default: "1".into() },
            ],
        };
        let r = change_signature_in(src_code, &offs, "add", &new, 2);
        assert_eq!((r.decls, r.calls, r.skipped.len()), (2, 1, 1));
        let out = apply(src_code, &r.edits);
        assert_eq!(out, "long sum(int b, int a, int scale);\nlong sum(int b, int a, int scale)\n{\n    return a + b;\n}\nvoid g(void) { int (*p)(int, int) = sum; sum(f(2, 3), 1, 1); }\n");
    }

    #[test]
    fn safe_delete_ranges() {
        let code = "int x;\n\n/* doc */\nstatic int add(int a, int b)\n{\n    return a + b;\n}\n\nint y = 2;\nint z;\n\nint w;\n";
        let r = deletion_range(code, code.find("add").unwrap()).unwrap();
        assert_eq!(&code[r.clone()], "/* doc */\nstatic int add(int a, int b)\n{\n    return a + b;\n}\n\n");
        let r = deletion_range(code, code.find("y =").unwrap()).unwrap();
        assert_eq!(&code[r], "int y = 2;\n");
        // No blank line before `z`, so the one after it stays.
        let r = deletion_range(code, code.find("z;").unwrap()).unwrap();
        assert_eq!(&code[r], "int z;\n");
        assert!(deletion_range("int a, b;\n", 4).is_none());
    }

    #[test]
    fn include_updates() {
        let text = "#include \"util/old.h\"\n#include <stdio.h>\n  #  include \"old.h\"\n";
        let e = include_edits(text, Path::new("/p/src/main.c"), Path::new("/p/src/util/old.h"), Path::new("/p/src/util/new_name.h"), &[]);
        assert_eq!(apply(text, &e), "#include \"util/new_name.h\"\n#include <stdio.h>\n  #  include \"old.h\"\n");
        let e = include_edits(text, Path::new("/p/src/util/x.c"), Path::new("/p/src/util/old.h"), Path::new("/p/inc/old.h"), &[]);
        assert_eq!(apply(text, &e), "#include \"util/old.h\"\n#include <stdio.h>\n  #  include \"../../inc/old.h\"\n");
    }

    #[test]
    fn names() {
        assert_eq!(suggest_name("get_width(win)"), "width");
        assert_eq!(suggest_name("p->height"), "height");
        assert_eq!(suggest_name("a * b"), "value");
        assert_eq!(suggest_name("scale(a, 2)"), "scale_result");
    }

    #[test]
    fn identifiers() {
        assert!(is_identifier("foo_1"));
        assert!(!is_identifier("1foo"));
        assert!(!is_identifier("int"));
        assert!(!is_identifier("a-b"));
    }
}
