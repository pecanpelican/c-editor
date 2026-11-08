use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::Range;

pub fn hash_text(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

#[derive(Default)]
pub struct FindState {
    pub open: bool,
    pub replace_open: bool,
    pub query: String,
    pub replacement: String,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub focus_query: bool,
    /// Matches (byte ranges) in the text with hash `text_hash`.
    pub matches: Vec<Range<usize>>,
    pub current: Option<usize>,
    pub text_hash: u64,
    key: u64,
}

impl FindState {
    fn options_key(&self, text_hash: u64) -> u64 {
        let mut h = DefaultHasher::new();
        (
            &self.query,
            self.case_sensitive,
            self.whole_word,
            self.open,
            text_hash,
        )
            .hash(&mut h);
        h.finish()
    }

    /// Recompute matches if the query, options or text changed.
    pub fn update(&mut self, text: &str) {
        let th = hash_text(text);
        let key = self.options_key(th);
        if key == self.key {
            return;
        }
        self.key = key;
        self.text_hash = th;
        self.matches = if self.open {
            find_all(text, &self.query, self.case_sensitive, self.whole_word)
        } else {
            Vec::new()
        };
        if let Some(c) = self.current
            && c >= self.matches.len()
        {
            self.current = if self.matches.is_empty() { None } else { Some(0) };
        }
    }

    /// Index of the first match at or after `byte` (wrapping).
    pub fn next_from(&self, byte: usize) -> Option<usize> {
        if self.matches.is_empty() {
            return None;
        }
        Some(
            self.matches
                .iter()
                .position(|m| m.start >= byte)
                .unwrap_or(0),
        )
    }

    /// Index of the last match strictly before `byte` (wrapping).
    pub fn prev_from(&self, byte: usize) -> Option<usize> {
        if self.matches.is_empty() {
            return None;
        }
        Some(
            self.matches
                .iter()
                .rposition(|m| m.start < byte)
                .unwrap_or(self.matches.len() - 1),
        )
    }
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

pub fn find_all(text: &str, query: &str, case_sensitive: bool, whole_word: bool) -> Vec<Range<usize>> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    // ASCII-only case folding keeps byte offsets identical between the
    // folded and original strings.
    let (hay, needle);
    let (h, n): (&str, &str) = if case_sensitive {
        (text, query)
    } else {
        hay = text.to_ascii_lowercase();
        needle = query.to_ascii_lowercase();
        (&hay, &needle)
    };
    let tb = text.as_bytes();
    let mut start = 0;
    while let Some(p) = h[start..].find(n) {
        let s = start + p;
        let e = s + n.len();
        let ok = !whole_word
            || ((s == 0 || !is_word_byte(tb[s - 1])) && (e == tb.len() || !is_word_byte(tb[e])));
        if ok {
            out.push(s..e);
        }
        start = if e > s { e } else { s + 1 };
        if out.len() > 100_000 {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds() {
        assert_eq!(find_all("Foo foo food", "foo", false, false), vec![0..3, 4..7, 8..11]);
        assert_eq!(find_all("Foo foo food", "foo", true, true), vec![4..7]);
        assert_eq!(find_all("ü foo", "FOO", false, false), vec![3..6]);
    }
}
