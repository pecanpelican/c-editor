//! Bookkeeping for a document's text: a line index, the lexer state at the
//! start of each line, and the undo history.
//!
//! Code all over cedit edits `Document::text` directly (textops, language
//! server edits, replace all, reload from disk), so instead of hooking every
//! edit, [`TextIndex::sync`] diffs the text against a shadow copy of what it
//! last saw. The diff is one replaced range (common prefix and suffix are
//! found with block compares, about a millisecond for a 10 MB file), which is
//! all that is needed to patch the line index, invalidate lexer states below
//! the edit and record an undo step.

use std::ops::Range;
use std::time::Instant;

use crate::highlight::{self, Language, LineState};

/// One undoable change: `removed` was replaced by `inserted` at byte `at`.
pub struct UndoRec {
    pub at: usize,
    pub removed: String,
    pub inserted: String,
    /// Selection (anchor, head) before the change.
    pub sel_before: (usize, usize),
    time: Instant,
}

/// Where a change found by `sync` goes.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Record {
    /// A normal edit: push on the undo stack and clear redo.
    Edit,
    /// The change was an undo: push on the redo stack.
    Undo,
    /// The change was a redo: push on the undo stack, keep redo.
    Redo,
}

/// Undo history is trimmed (oldest first) above this many bytes.
const UNDO_BYTES_MAX: usize = 128 * 1024 * 1024;

#[derive(Default)]
pub struct TextIndex {
    /// Byte offset of the start of each line; `starts[0] == 0`.
    starts: Vec<usize>,
    shadow: String,
    version: u64,
    built: bool,
    /// `states[i]` is the lexer state at the start of line `i`, valid for
    /// `i < states_valid`.
    states: Vec<LineState>,
    states_valid: usize,
    lang: Option<Language>,
    /// Length in bytes of the longest line seen (only grows between rebuilds).
    pub max_line_len: usize,
    undo: Vec<UndoRec>,
    redo: Vec<UndoRec>,
    undo_bytes: usize,
    /// The next edit starts a new undo step even if it could merge.
    sealed: bool,
}

/// Length of the common prefix of `a` and `b`.
pub fn common_prefix(a: &[u8], b: &[u8]) -> usize {
    const BLOCK: usize = 1024;
    let n = a.len().min(b.len());
    let mut i = 0;
    while i + BLOCK <= n && a[i..i + BLOCK] == b[i..i + BLOCK] {
        i += BLOCK;
    }
    while i < n && a[i] == b[i] {
        i += 1;
    }
    i
}

/// Length of the common suffix of `a` and `b`, at most `max`.
pub fn common_suffix(a: &[u8], b: &[u8], max: usize) -> usize {
    const BLOCK: usize = 1024;
    let (la, lb) = (a.len(), b.len());
    let mut s = 0;
    while s + BLOCK <= max && a[la - s - BLOCK..la - s] == b[lb - s - BLOCK..lb - s] {
        s += BLOCK;
    }
    while s < max && a[la - s - 1] == b[lb - s - 1] {
        s += 1;
    }
    s
}

/// The single replaced range between `old` and `new`: returns
/// `(start, old_end, new_end)` on char boundaries, so `old[start..old_end]`
/// was replaced by `new[start..new_end]`.
pub fn diff(old: &str, new: &str) -> (usize, usize, usize) {
    let (a, b) = (old.as_bytes(), new.as_bytes());
    let mut p = common_prefix(a, b);
    while p > 0 && !(old.is_char_boundary(p) && new.is_char_boundary(p)) {
        p -= 1;
    }
    let max = (a.len() - p).min(b.len() - p);
    let mut s = common_suffix(a, b, max);
    while s > 0 && !(old.is_char_boundary(a.len() - s) && new.is_char_boundary(b.len() - s)) {
        s -= 1;
    }
    (p, a.len() - s, b.len() - s)
}

fn newline_positions(text: &str, base: usize) -> impl Iterator<Item = usize> + '_ {
    text.bytes().enumerate().filter(|&(_, c)| c == b'\n').map(move |(i, _)| base + i + 1)
}

impl TextIndex {
    fn rebuild(&mut self, text: &str, version: u64) {
        self.starts.clear();
        self.starts.push(0);
        self.starts.extend(newline_positions(text, 0));
        self.max_line_len = self
            .starts
            .windows(2)
            .map(|w| w[1] - w[0])
            .chain(std::iter::once(text.len() - self.starts[self.starts.len() - 1]))
            .max()
            .unwrap_or(0);
        self.shadow.clear();
        self.shadow.push_str(text);
        self.version = version;
        self.built = true;
        self.states_valid = 0;
    }

    /// Whether the index matches `text` at `version` without a sync.
    pub fn is_fresh(&self, text: &str, version: u64) -> bool {
        self.built && self.version == version && self.shadow.len() == text.len()
    }

    /// Bring the index up to date with `text`. Returns the replaced range
    /// `(start, old_end, new_end)` if the text changed.
    pub fn sync(&mut self, text: &str, version: u64, sel_before: (usize, usize), record: Record) -> Option<(usize, usize, usize)> {
        if !self.built {
            self.rebuild(text, version);
            return None;
        }
        if self.is_fresh(text, version) {
            return None;
        }
        self.version = version;
        let (p, old_end, new_end) = diff(&self.shadow, text);
        if p == old_end && p == new_end {
            return None;
        }

        // Line starts: those at or before `p` are unchanged; those after the
        // old edit end shift; new ones come from the inserted text.
        let first = self.starts.partition_point(|&q| q <= p);
        let last = self.starts.partition_point(|&q| q <= old_end);
        let delta = new_end as isize - old_end as isize;
        let inserted = &text[p..new_end];
        let new_starts: Vec<usize> = newline_positions(inserted, p).collect();
        let added = new_starts.len();
        self.starts.splice(first..last, new_starts);
        if delta != 0 {
            for q in &mut self.starts[first + added..] {
                *q = (*q as isize + delta) as usize;
            }
        }
        // Longest line: check the lines the edit touched.
        let l0 = first.saturating_sub(1);
        let l1 = (first + added + 1).min(self.starts.len());
        for l in l0..l1 {
            let s = self.starts[l];
            let e = self.starts.get(l + 1).copied().unwrap_or(text.len());
            self.max_line_len = self.max_line_len.max(e - s);
        }
        self.states_valid = self.states_valid.min(l0 + 1);

        let rec = UndoRec {
            at: p,
            removed: self.shadow[p..old_end].to_string(),
            inserted: inserted.to_string(),
            sel_before,
            time: Instant::now(),
        };
        self.shadow.replace_range(p..old_end, inserted);
        debug_assert_eq!(self.shadow, text);
        self.push_record(rec, record);
        Some((p, old_end, new_end))
    }

    fn push_record(&mut self, rec: UndoRec, record: Record) {
        match record {
            Record::Undo => {
                self.redo.push(rec);
                return;
            }
            Record::Edit => {
                self.redo.clear();
            }
            Record::Redo => {}
        }
        let sealed = std::mem::take(&mut self.sealed);
        if record == Record::Edit
            && !sealed
            && let Some(last) = self.undo.last_mut()
            && rec.time.duration_since(last.time).as_secs_f32() < 1.0
        {
            // Typing: extend the previous insertion (which may have replaced a selection).
            if !last.inserted.is_empty()
                && rec.removed.is_empty()
                && rec.at == last.at + last.inserted.len()
                && !rec.inserted.contains('\n')
                && rec.inserted.len() <= 4
            {
                self.undo_bytes += rec.inserted.len();
                last.inserted.push_str(&rec.inserted);
                last.time = rec.time;
                return;
            }
            // Backspacing: extend the previous deletion.
            if last.inserted.is_empty()
                && rec.inserted.is_empty()
                && rec.at + rec.removed.len() == last.at
                && !rec.removed.contains('\n')
                && rec.removed.len() <= 4
            {
                self.undo_bytes += rec.removed.len();
                last.removed.insert_str(0, &rec.removed);
                last.at = rec.at;
                last.time = rec.time;
                return;
            }
        }
        self.undo_bytes += rec.removed.len() + rec.inserted.len();
        self.undo.push(rec);
        while self.undo_bytes > UNDO_BYTES_MAX && self.undo.len() > 1 {
            let r = self.undo.remove(0);
            self.undo_bytes -= r.removed.len() + r.inserted.len();
        }
    }

    /// Start a new undo step with the next edit (after the caret moves).
    pub fn seal(&mut self) {
        self.sealed = true;
    }

    pub fn pop_undo(&mut self) -> Option<UndoRec> {
        let r = self.undo.pop()?;
        self.undo_bytes = self.undo_bytes.saturating_sub(r.removed.len() + r.inserted.len());
        self.sealed = true;
        Some(r)
    }

    pub fn pop_redo(&mut self) -> Option<UndoRec> {
        self.sealed = true;
        self.redo.pop()
    }

    pub fn line_count(&self) -> usize {
        self.starts.len().max(1)
    }

    /// 0-based line containing `byte`.
    pub fn line_of(&self, byte: usize) -> usize {
        self.starts.partition_point(|&q| q <= byte).saturating_sub(1)
    }

    pub fn line_start(&self, line: usize) -> usize {
        self.starts.get(line).copied().unwrap_or(self.shadow.len())
    }

    /// End of `line`, not including its newline.
    pub fn line_end(&self, line: usize) -> usize {
        match self.starts.get(line + 1) {
            Some(&next) => next - 1,
            None => self.shadow.len(),
        }
    }

    pub fn line_range(&self, line: usize) -> Range<usize> {
        self.line_start(line)..self.line_end(line)
    }

    /// The lexer state at the start of `line`, lexing the lines above it
    /// that have not been lexed since they last changed.
    pub fn state_at(&mut self, text: &str, lang: Language, line: usize) -> LineState {
        if self.lang != Some(lang) {
            self.lang = Some(lang);
            self.states_valid = 0;
        }
        let line = line.min(self.line_count() - 1);
        if self.states.len() < self.line_count() {
            self.states.resize(self.line_count(), LineState::Normal);
        }
        if self.states_valid == 0 {
            self.states[0] = LineState::Normal;
            self.states_valid = 1;
        }
        let mut scratch = Vec::new();
        while self.states_valid <= line {
            let l = self.states_valid - 1;
            scratch.clear();
            let next = highlight::lex_line(lang, text, self.line_range(l), self.states[l], &mut scratch);
            self.states[l + 1] = next;
            self.states_valid += 1;
        }
        self.states[line]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(ix: &TextIndex, text: &str) {
        let mut expect = vec![0];
        expect.extend(newline_positions(text, 0));
        assert_eq!(ix.starts, expect, "line starts for {text:?}");
    }

    #[test]
    fn index_follows_edits() {
        let mut ix = TextIndex::default();
        let mut text = String::from("one\ntwo\nthree\n");
        let mut v = 1;
        ix.sync(&text, v, (0, 0), Record::Edit);
        check(&ix, &text);
        let edits: &[(Range<usize>, &str)] = &[
            (4..4, "inserted\nlines\n"),
            (0..3, "ONE"),
            (2..10, ""),
            (0..0, "\n\n"),
            (5..5, "é"),
        ];
        for (r, s) in edits {
            text.replace_range(r.clone(), s);
            v += 1;
            ix.sync(&text, v, (0, 0), Record::Edit);
            check(&ix, &text);
        }
        let end = text.len();
        text.replace_range(0..end, "");
        v += 1;
        ix.sync(&text, v, (0, 0), Record::Edit);
        check(&ix, &text);
        assert_eq!(ix.line_count(), 1);
    }

    #[test]
    fn diff_on_char_boundaries() {
        assert_eq!(diff("aéb", "aèb"), (1, 3, 3));
        assert_eq!(diff("abc", "abc"), (3, 3, 3));
        assert_eq!(diff("aaa", "aaaa"), (3, 3, 4));
        assert_eq!(diff("", "x"), (0, 0, 1));
    }

    #[test]
    fn typing_merges_into_one_undo_step() {
        let mut ix = TextIndex::default();
        let mut text = String::from("int x;\n");
        ix.sync(&text, 1, (0, 0), Record::Edit);
        for (i, c) in "abc".chars().enumerate() {
            text.insert(6 + i, c);
            ix.sync(&text, 2 + i as u64, (0, 0), Record::Edit);
        }
        let r = ix.pop_undo().unwrap();
        assert_eq!((r.at, r.removed.as_str(), r.inserted.as_str()), (6, "", "abc"));
        assert!(ix.pop_undo().is_none());
    }

    #[test]
    fn states_follow_block_comments() {
        let mut ix = TextIndex::default();
        let mut text = String::from("a\n/* x\ny */\nb\n");
        ix.sync(&text, 1, (0, 0), Record::Edit);
        assert_eq!(ix.state_at(&text, Language::C, 2), LineState::BlockComment);
        assert_eq!(ix.state_at(&text, Language::C, 3), LineState::Normal);
        text.replace_range(9..11, ""); // remove "*/"
        ix.sync(&text, 2, (0, 0), Record::Edit);
        assert_eq!(ix.state_at(&text, Language::C, 3), LineState::BlockComment);
    }
}
