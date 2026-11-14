//! Pure text-editing operations used by the editor (auto-indent, block
//! indent, comment toggling...). All positions are byte offsets.

use std::ops::Range;

use std::sync::atomic::{AtomicUsize, Ordering};

/// Indent width in spaces (from settings).
static INDENT_WIDTH: AtomicUsize = AtomicUsize::new(4);

pub fn set_indent_width(n: usize) {
    INDENT_WIDTH.store(n.clamp(1, 16), Ordering::Relaxed);
}

static AUTO_CLOSE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub fn set_auto_close(on: bool) {
    AUTO_CLOSE.store(on, Ordering::Relaxed);
}

pub fn auto_close_enabled() -> bool {
    AUTO_CLOSE.load(Ordering::Relaxed)
}

pub fn indent_width() -> usize {
    INDENT_WIDTH.load(Ordering::Relaxed)
}

pub fn indent_unit() -> String {
    " ".repeat(indent_width())
}

pub fn char_to_byte(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(text.len())
}

#[allow(dead_code)]
pub fn byte_to_char(text: &str, byte: usize) -> usize {
    let byte = floor_char_boundary(text, byte);
    text[..byte].chars().count()
}

pub fn floor_char_boundary(text: &str, mut byte: usize) -> usize {
    if byte >= text.len() {
        return text.len();
    }
    while !text.is_char_boundary(byte) {
        byte -= 1;
    }
    byte
}

pub fn line_start(text: &str, byte: usize) -> usize {
    text[..byte].rfind('\n').map(|p| p + 1).unwrap_or(0)
}

pub fn line_end(text: &str, byte: usize) -> usize {
    text[byte..].find('\n').map(|p| p + byte).unwrap_or(text.len())
}

/// 0-based line and column (in chars) of a byte offset.
pub fn line_col(text: &str, byte: usize) -> (usize, usize) {
    let byte = floor_char_boundary(text, byte);
    let before = &text[..byte];
    let line = before.bytes().filter(|&b| b == b'\n').count();
    let ls = line_start(text, byte);
    (line, text[ls..byte].chars().count())
}

/// Byte offset of the start of 0-based `line` (clamped to the end).
pub fn line_to_byte(text: &str, line: usize) -> usize {
    if line == 0 {
        return 0;
    }
    let mut count = 0;
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            count += 1;
            if count == line {
                return i + 1;
            }
        }
    }
    text.len()
}

/// Byte offset for a 0-based line and 0-based column in chars, clamped.
pub fn line_col_to_byte(text: &str, line: usize, col: usize) -> usize {
    let ls = line_to_byte(text, line);
    let le = line_end(text, ls);
    text[ls..le]
        .char_indices()
        .nth(col)
        .map(|(b, _)| ls + b)
        .unwrap_or(le)
}

fn leading_ws(line: &str) -> &str {
    let n = line.len() - line.trim_start_matches([' ', '\t']).len();
    &line[..n]
}

/// Replace `sel` with a newline plus the current line's indentation.
/// After `{` (or `(`/`[`) the new line gets one extra level, and if the
/// cursor sits right before the matching closer, the closer is moved to
/// its own line. Returns the new cursor position.
pub fn newline_with_indent(text: &mut String, sel: Range<usize>, hard_tabs: bool) -> usize {
    let ls = line_start(text, sel.start);
    let indent = leading_ws(&text[ls..sel.start]).to_string();
    let before = text[ls..sel.start].trim_end();
    let after_first = text[sel.end..].chars().next();
    let unit_owned = if hard_tabs { "\t".to_string() } else { indent_unit() };
    let unit = unit_owned.as_str();
    let opener = before.chars().last();
    let opens = matches!(opener, Some('{' | '(' | '['));
    let closes_pair = matches!(
        (opener, after_first),
        (Some('{'), Some('}')) | (Some('('), Some(')')) | (Some('['), Some(']'))
    );
    let mut insert = String::from("\n");
    insert.push_str(&indent);
    if opens {
        insert.push_str(unit);
    }
    let cursor = sel.start + insert.len();
    if closes_pair {
        insert.push('\n');
        insert.push_str(&indent);
    }
    text.replace_range(sel, &insert);
    cursor
}

/// Insert indentation at the cursor (spaces up to the next tab stop, or a
/// tab character when `hard_tabs`). Returns the new cursor.
pub fn insert_indent(text: &mut String, sel: Range<usize>, hard_tabs: bool) -> usize {
    let insert = if hard_tabs {
        "\t".to_string()
    } else {
        let ls = line_start(text, sel.start);
        let col = text[ls..sel.start].chars().count();
        " ".repeat(indent_width() - col % indent_width())
    };
    let cursor = sel.start + insert.len();
    text.replace_range(sel, &insert);
    cursor
}

/// Lines touched by a selection: (start of first line, end of last line).
/// A selection ending at column 0 does not include that last line.
fn selected_lines(text: &str, sel: &Range<usize>) -> (usize, usize) {
    let start = line_start(text, sel.start);
    let mut end_pos = sel.end;
    if sel.end > sel.start && sel.end > 0 && text.as_bytes()[sel.end - 1] == b'\n' {
        end_pos -= 1;
    }
    let end = line_end(text, end_pos.max(start));
    (start, end)
}

/// Apply `f` to each line in the selected block and return the new
/// selection covering the whole block.
fn map_lines(text: &mut String, sel: Range<usize>, f: impl Fn(&str) -> String) -> Range<usize> {
    let (start, end) = selected_lines(text, &sel);
    let block: Vec<String> = text[start..end].split('\n').map(&f).collect();
    let replaced = block.join("\n");
    let new_end = start + replaced.len();
    text.replace_range(start..end, &replaced);
    start..new_end
}

pub fn indent_lines(text: &mut String, sel: Range<usize>, hard_tabs: bool) -> Range<usize> {
    let unit_owned = if hard_tabs { "\t".to_string() } else { indent_unit() };
    let unit = unit_owned.as_str();
    map_lines(text, sel, |l| {
        if l.trim().is_empty() {
            l.to_string()
        } else {
            format!("{unit}{l}")
        }
    })
}

/// Remove one level of indentation. Returns the new selection; when the
/// selection was a plain cursor, the cursor stays on the same text.
pub fn dedent_lines(text: &mut String, sel: Range<usize>) -> Range<usize> {
    let single = sel.is_empty();
    let ls = line_start(text, sel.start);
    let removed_first = {
        let line = &text[ls..];
        if line.starts_with('\t') {
            1
        } else {
            line.bytes().take(indent_width()).take_while(|&b| b == b' ').count()
        }
    };
    let new = map_lines(text, sel.clone(), |l| {
        if let Some(rest) = l.strip_prefix('\t') {
            rest.to_string()
        } else {
            let n = l.bytes().take(indent_width()).take_while(|&b| b == b' ').count();
            l[n..].to_string()
        }
    });
    if single {
        let c = sel.start.saturating_sub(removed_first).max(ls);
        c..c
    } else {
        new
    }
}

/// Toggle a line comment on every line of the selection. If every non-blank
/// line is already commented, comments are removed; otherwise they are
/// added at the block's minimum indentation.
pub fn toggle_comment(text: &mut String, sel: Range<usize>, prefix: &str) -> Range<usize> {
    let (start, end) = selected_lines(text, &sel);
    let lines: Vec<&str> = text[start..end].split('\n').collect();
    let non_blank: Vec<&&str> = lines.iter().filter(|l| !l.trim().is_empty()).collect();
    if non_blank.is_empty() {
        return sel;
    }
    let all_commented = non_blank.iter().all(|l| l.trim_start().starts_with(prefix));
    let min_indent = non_blank
        .iter()
        .map(|l| leading_ws(l).len())
        .min()
        .unwrap_or(0);
    let single_cursor = sel.is_empty();
    let cursor_col = sel.start - start;
    let mut delta: isize = 0;
    let new = map_lines(text, sel.clone(), |l| {
        if l.trim().is_empty() {
            return l.to_string();
        }
        if all_commented {
            let ws = leading_ws(l).len();
            let rest = &l[ws + prefix.len()..];
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            format!("{}{}", &l[..ws], rest)
        } else {
            format!("{}{} {}", &l[..min_indent], prefix, &l[min_indent..])
        }
    });
    if single_cursor {
        // Keep the cursor on the same character of a single line.
        let line_len_before = end - start;
        let line_len_after = new.end - new.start;
        delta += line_len_after as isize - line_len_before as isize;
        let c = (start as isize + cursor_col as isize + delta).max(start as isize) as usize;
        let c = c.min(new.end);
        c..c
    } else {
        new
    }
}

/// Duplicate the selection, or the current line if nothing is selected.
pub fn duplicate(text: &mut String, sel: Range<usize>) -> Range<usize> {
    if sel.is_empty() {
        let ls = line_start(text, sel.start);
        let le = line_end(text, sel.start);
        let line = text[ls..le].to_string();
        text.insert_str(le, &format!("\n{line}"));
        let c = sel.start + line.len() + 1;
        c..c
    } else {
        let s = text[sel.clone()].to_string();
        text.insert_str(sel.end, &s);
        sel.end..sel.end + s.len()
    }
}

/// Delete the lines touched by the selection.
pub fn delete_lines(text: &mut String, sel: Range<usize>) -> usize {
    let (start, end) = selected_lines(text, &sel);
    let (rs, re) = if end < text.len() {
        (start, end + 1)
    } else if start > 0 {
        (start - 1, end)
    } else {
        (start, end)
    };
    text.replace_range(rs..re, "");
    let c = rs.min(text.len());
    line_start(text, c)
}

/// If the cursor is on a line containing only whitespace, typing `}`
/// should remove one indentation level first. Returns the new cursor.
pub fn type_closing_brace(text: &mut String, sel: Range<usize>, ch: char) -> usize {
    let ls = line_start(text, sel.start);
    let before = &text[ls..sel.start];
    if !before.is_empty() && before.trim().is_empty() {
        let cut = if before.ends_with('\t') {
            1
        } else {
            before.bytes().rev().take(indent_width()).take_while(|&b| b == b' ').count()
        };
        let start = sel.start - cut;
        let mut s = String::new();
        s.push(ch);
        text.replace_range(start..sel.end, &s);
        start + ch.len_utf8()
    } else {
        let mut s = String::new();
        s.push(ch);
        text.replace_range(sel.clone(), &s);
        sel.start + ch.len_utf8()
    }
}

/// Move the lines touched by the selection one line up or down, keeping the
/// selection on the moved text. Returns `None` at the top or bottom.
pub fn move_lines(text: &mut String, sel: Range<usize>, up: bool) -> Option<Range<usize>> {
    let (start, end) = selected_lines(text, &sel);
    let block = text[start..end].to_string();
    if up {
        if start == 0 {
            return None;
        }
        let prev_start = line_start(text, start - 1);
        let prev = text[prev_start..start - 1].to_string();
        text.replace_range(prev_start..end, &format!("{block}\n{prev}"));
        let shift = start - prev_start;
        Some(sel.start - shift..sel.end - shift)
    } else {
        if end >= text.len() {
            return None;
        }
        let next_end = line_end(text, end + 1);
        let next = text[end + 1..next_end].to_string();
        text.replace_range(start..next_end, &format!("{next}\n{block}"));
        let shift = next.len() + 1;
        Some(sel.start + shift..sel.end + shift)
    }
}

/// The closing character that auto-close pairs with `open`, if any.
pub fn closing_pair(open: char) -> Option<char> {
    match open {
        '(' => Some(')'),
        '[' => Some(']'),
        '{' => Some('}'),
        '"' => Some('"'),
        '\'' => Some('\''),
        _ => None,
    }
}

/// Typing an opening bracket or quote. With a selection, wraps it; otherwise
/// inserts the pair when the next character is whitespace, a closer or the
/// end of the line. Returns `None` when the character should be typed
/// normally (for example `'` right after a letter, as in `don't`).
pub fn auto_close(text: &mut String, sel: Range<usize>, open: char) -> Option<Range<usize>> {
    let close = closing_pair(open)?;
    let n = open.len_utf8();
    if !sel.is_empty() {
        let inner = text[sel.clone()].to_string();
        if inner.contains('\n') && open != '{' && open != '(' && open != '[' {
            return None;
        }
        text.replace_range(sel.clone(), &format!("{open}{inner}{close}"));
        return Some(sel.start + n..sel.end + n);
    }
    let next = text[sel.start..].chars().next();
    let prev = text[..sel.start].chars().next_back();
    let next_ok = next.is_none_or(|c| c.is_whitespace() || matches!(c, ')' | ']' | '}' | ';' | ','));
    if !next_ok {
        return None;
    }
    if (open == '"' || open == '\'') && prev.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '\\') {
        return None;
    }
    if (open == '"' || open == '\'') && in_string_or_comment(text, sel.start) {
        return None;
    }
    text.insert_str(sel.start, &format!("{open}{close}"));
    Some(sel.start + n..sel.start + n)
}

/// Typing a closer right before the same character just steps over it.
/// Returns the new cursor when it did.
pub fn skip_closer(text: &str, sel: &Range<usize>, ch: char) -> Option<usize> {
    if !sel.is_empty() || !matches!(ch, ')' | ']' | '}' | '"' | '\'') {
        return None;
    }
    text[sel.start..].starts_with(ch).then(|| sel.start + ch.len_utf8())
}

/// Backspace between an empty pair such as `(|)` removes both characters.
pub fn delete_pair(text: &mut String, sel: &Range<usize>) -> Option<usize> {
    if !sel.is_empty() || sel.start == 0 {
        return None;
    }
    let prev = text[..sel.start].chars().next_back()?;
    let close = closing_pair(prev)?;
    if !text[sel.start..].starts_with(close) {
        return None;
    }
    let start = sel.start - prev.len_utf8();
    text.replace_range(start..sel.start + close.len_utf8(), "");
    Some(start)
}

/// Rough check for whether `pos` is inside a string, char literal or
/// comment on its line (enough to avoid pairing quotes inside them).
fn in_string_or_comment(text: &str, pos: usize) -> bool {
    let ls = line_start(text, pos);
    let mut quote: Option<char> = None;
    let mut chars = text[ls..pos].chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == '\\' {
                    chars.next();
                } else if c == q {
                    quote = None;
                }
            }
            None => {
                if c == '"' || c == '\'' {
                    quote = Some(c);
                } else if c == '/' && chars.peek() == Some(&'/') {
                    return true;
                }
            }
        }
    }
    quote.is_some()
}

/// The bracket matching the one at or just before `pos`, skipping brackets
/// inside strings and comments. Returns (bracket at cursor, its match).
#[allow(dead_code)] // for the editor's bracket highlight
pub fn matching_bracket(text: &str, pos: usize) -> Option<(usize, usize)> {
    let b = text.as_bytes();
    let at = |i: usize| -> Option<u8> { b.get(i).copied().filter(|c| b"()[]{}".contains(c)) };
    let here = if pos < b.len() && at(pos).is_some() {
        pos
    } else if pos > 0 && at(pos - 1).is_some() {
        pos - 1
    } else {
        return None;
    };
    let code = code_mask(text);
    if !code[here] {
        return None;
    }
    let c = b[here];
    let (open, close, forward) = match c {
        b'(' => (b'(', b')', true),
        b'[' => (b'[', b']', true),
        b'{' => (b'{', b'}', true),
        b')' => (b'(', b')', false),
        b']' => (b'[', b']', false),
        _ => (b'{', b'}', false),
    };
    let mut depth = 0isize;
    let mut check = |i: usize| -> Option<usize> {
        if !code[i] {
            return None;
        }
        if b[i] == open {
            if forward { depth += 1 } else { depth -= 1 }
        } else if b[i] == close {
            if forward { depth -= 1 } else { depth += 1 }
        }
        (depth == 0).then_some(i)
    };
    if forward {
        for i in here..b.len() {
            if let Some(m) = check(i) {
                return Some((here, m));
            }
        }
    } else {
        for i in (0..=here).rev() {
            if let Some(m) = check(i) {
                return Some((here, m));
            }
        }
    }
    None
}

/// For each byte: true if it is code (not inside a string, char literal or
/// comment). C-style lexing, good enough for C, GLSL and Objective-C.
#[allow(dead_code)]
fn code_mask(text: &str) -> Vec<bool> {
    let b = text.as_bytes();
    let mut mask = vec![true; b.len()];
    let mut i = 0;
    while i < b.len() {
        let start = i;
        match b[i] {
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i = (i + 2).min(b.len());
            }
            q @ (b'"' | b'\'') => {
                i += 1;
                while i < b.len() && b[i] != q && b[i] != b'\n' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                i = (i + 1).min(b.len());
            }
            _ => {
                i += 1;
                continue;
            }
        }
        for m in &mut mask[start..i.min(b.len())] {
            *m = false;
        }
    }
    mask
}

/// Trim trailing whitespace on each line and/or make sure the text ends
/// with exactly one newline.
pub fn clean_for_save(text: &str, trim: bool, final_newline: bool) -> String {
    let mut out = if trim {
        let mut s: String = text
            .split('\n')
            .map(|l| l.trim_end_matches([' ', '\t']))
            .collect::<Vec<_>>()
            .join("\n");
        if !final_newline && text.ends_with('\n') && !s.ends_with('\n') {
            s.push('\n');
        }
        s
    } else {
        text.to_string()
    };
    if final_newline && !out.is_empty() {
        while out.ends_with("\n\n") {
            out.pop();
        }
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newline_indent() {
        let mut t = String::from("    if (x) {}");
        let at = t.find('}').unwrap();
        let c = newline_with_indent(&mut t, at..at, false);
        assert_eq!(t, "    if (x) {\n        \n    }");
        assert_eq!(&t[c..], "\n    }");

        let mut t = String::from("\tfoo");
        let c = newline_with_indent(&mut t, 4..4, true);
        assert_eq!(t, "\tfoo\n\t");
        assert_eq!(c, t.len());
    }

    #[test]
    fn indent_and_dedent() {
        let mut t = String::from("a\nb\n\nc");
        let n = t.len();
        let s = indent_lines(&mut t, 0..n, false);
        assert_eq!(t, "    a\n    b\n\n    c");
        assert_eq!(s, 0..t.len());
        let s = dedent_lines(&mut t, s);
        assert_eq!(t, "a\nb\n\nc");
        assert_eq!(s, 0..t.len());

        let mut t = String::from("  x");
        let s = dedent_lines(&mut t, 3..3);
        assert_eq!(t, "x");
        assert_eq!(s, 1..1);
    }

    #[test]
    fn selection_ending_at_line_start() {
        let mut t = String::from("a\nb\nc");
        indent_lines(&mut t, 0..2, false);
        assert_eq!(t, "    a\nb\nc");
    }

    #[test]
    fn comments() {
        let mut t = String::from("  int a;\n    int b;");
        let n = t.len();
        toggle_comment(&mut t, 0..n, "//");
        assert_eq!(t, "  // int a;\n  //   int b;");
        let n = t.len();
        toggle_comment(&mut t, 0..n, "//");
        assert_eq!(t, "  int a;\n    int b;");

        let mut t = String::from("x = 1");
        let s = toggle_comment(&mut t, 2..2, "#");
        assert_eq!(t, "# x = 1");
        assert_eq!(s, 4..4);
    }

    #[test]
    fn dup_and_delete() {
        let mut t = String::from("one\ntwo");
        let s = duplicate(&mut t, 1..1);
        assert_eq!(t, "one\none\ntwo");
        assert_eq!(s, 5..5);
        let c = delete_lines(&mut t, 5..5);
        assert_eq!(t, "one\ntwo");
        assert_eq!(c, 4);
        let mut t = String::from("only");
        delete_lines(&mut t, 0..0);
        assert_eq!(t, "");
    }

    #[test]
    fn closing_brace() {
        let mut t = String::from("{\n        ");
        let n = t.len();
        let c = type_closing_brace(&mut t, n..n, '}');
        assert_eq!(t, "{\n    }");
        assert_eq!(c, t.len());
    }

    #[test]
    fn cleaning() {
        assert_eq!(clean_for_save("a  \n\tb\t\n\n\n", true, true), "a\n\tb\n");
        assert_eq!(clean_for_save("x", false, true), "x\n");
        assert_eq!(clean_for_save("x  ", true, false), "x");
        assert_eq!(clean_for_save("", true, true), "");
    }

    #[test]
    fn positions() {
        let t = "héllo\nwörld";
        assert_eq!(line_col(t, t.find('w').unwrap()), (1, 0));
        assert_eq!(line_col_to_byte(t, 1, 2), t.find('r').unwrap());
        assert_eq!(byte_to_char(t, t.len()), 11);
        assert_eq!(char_to_byte(t, 2), 3);
    }
    #[test]
    fn moving_lines() {
        let mut t = String::from("a\nbb\nc");
        let s = move_lines(&mut t, 3..3, true).unwrap();
        assert_eq!(t, "bb\na\nc");
        assert_eq!(s, 1..1);
        let s = move_lines(&mut t, s, false).unwrap();
        assert_eq!(t, "a\nbb\nc");
        assert_eq!(s, 3..3);
        assert!(move_lines(&mut t, 0..0, true).is_none());
        let n = t.len();
        assert!(move_lines(&mut t, n..n, false).is_none());
        // A two-line block.
        let mut t = String::from("1\n2\n3\n4");
        let s = move_lines(&mut t, 2..5, false).unwrap();
        assert_eq!(t, "1\n4\n2\n3");
        assert_eq!(&t[s], "2\n3");
    }

    #[test]
    fn pairs() {
        let mut t = String::from("f");
        let s = auto_close(&mut t, 1..1, '(').unwrap();
        assert_eq!(t, "f()");
        assert_eq!(s, 2..2);
        assert_eq!(skip_closer(&t, &s, ')'), Some(3));
        assert_eq!(delete_pair(&mut t, &s), Some(1));
        assert_eq!(t, "f");
        // Not before a word, not after a letter for quotes.
        let mut t = String::from("x");
        assert!(auto_close(&mut t, 0..0, '(').is_none());
        let mut t = String::from("don");
        assert!(auto_close(&mut t, 3..3, '\'').is_none());
        // Wrap a selection.
        let mut t = String::from("a + b");
        let s = auto_close(&mut t, 0..5, '(').unwrap();
        assert_eq!(t, "(a + b)");
        assert_eq!(&t[s], "a + b");
        // Inside a string.
        let mut t = String::from("\"ab \"");
        assert!(auto_close(&mut t, 4..4, '"').is_none());
    }

    #[test]
    fn brackets() {
        let t = "f(a[1], \")\") { /* } */ }";
        assert_eq!(matching_bracket(t, 1), Some((1, 11)));
        assert_eq!(matching_bracket(t, 12), Some((11, 1)));
        let open = t.find('{').unwrap();
        assert_eq!(matching_bracket(t, open), Some((open, t.len() - 1)));
        assert_eq!(matching_bracket(t, t.len()), Some((t.len() - 1, open)));
        assert_eq!(matching_bracket("abc", 1), None);
    }

}
