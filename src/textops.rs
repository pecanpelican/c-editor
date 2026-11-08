//! Pure text-editing operations used by the editor (auto-indent, block
//! indent, comment toggling...). All positions are byte offsets.

use std::ops::Range;

use std::sync::atomic::{AtomicUsize, Ordering};

/// Indent width in spaces (from settings).
static INDENT_WIDTH: AtomicUsize = AtomicUsize::new(4);

pub fn set_indent_width(n: usize) {
    INDENT_WIDTH.store(n.clamp(1, 16), Ordering::Relaxed);
}

fn indent_width() -> usize {
    INDENT_WIDTH.load(Ordering::Relaxed)
}

fn indent_unit() -> String {
    " ".repeat(indent_width())
}

pub fn char_to_byte(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(text.len())
}

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
}
