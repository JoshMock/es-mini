//! Text analysis: the `standard` and `keyword` analyzers.

#[rustfmt::skip]
mod standard_tables;

use standard_tables::{ACTION, ATTRIBUTE, CMAP_BLOCKS, CMAP_TOP, ROWMAP, TRANS};

/// Lucene's StandardAnalyzer.DEFAULT_MAX_TOKEN_LENGTH, in UTF-16 code units.
const MAX_TOKEN_LENGTH: usize = 255;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Analyzer {
    /// StandardTokenizer + LowerCaseFilter, no stopwords (the ES default).
    Standard,
    Keyword,
}

impl Analyzer {
    pub fn analyze(&self, text: &str) -> Vec<String> {
        match self {
            Self::Standard => standard_tokenize(text).into_iter().map(lowercase).collect(),
            Self::Keyword => vec![text.to_owned()],
        }
    }
}

/// Lucene LowerCaseFilter: per code point, simple case mapping.
fn lowercase(token: &str) -> String {
    // char::to_lowercase is the full mapping; only U+0130 expands, and its first char is Java's simple mapping.
    token
        .chars()
        .map(|c| c.to_lowercase().next().unwrap_or(c))
        .collect()
}

/// Lucene StandardTokenizer (Unicode 12.1 word breaks + emoji), ported from its
/// JFlex-generated DFA so tokens match byte for byte.
pub fn standard_tokenize(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < text.len() {
        let (len, keep) = scan(&text[pos..]);
        if keep {
            out.push(&text[pos..pos + len]);
        }
        pos += len;
    }
    out
}

/// Longest DFA match at the start of `s`: (byte length, whether it is a token).
/// Lookahead stops at Lucene's scanner buffer size, which is what splits long tokens.
fn scan(s: &str) -> (usize, bool) {
    let mut state = 0;
    let mut matched = None;
    let mut units = 0;
    for (i, c) in s.char_indices() {
        units += c.len_utf16();
        if units > MAX_TOKEN_LENGTH {
            break;
        }
        let next = TRANS[ROWMAP[state] as usize + char_class(c)];
        if next < 0 {
            break;
        }
        state = next as usize;
        if ATTRIBUTE[state] & 1 == 1 {
            matched = Some((i + c.len_utf8(), state));
            if ATTRIBUTE[state] & 8 == 8 {
                break;
            }
        }
    }
    match matched {
        // Action 1 is the grammar's catch-all "not a token" rule.
        Some((len, state)) => (len, ACTION[state] != 1),
        // ponytail: unreachable with Lucene's tables (catch-all rule); skip one char rather than loop forever
        None => (s.chars().next().map_or(s.len(), char::len_utf8), false),
    }
}

fn char_class(c: char) -> usize {
    let cp = c as usize;
    let class = match cp < 256 {
        true => CMAP_BLOCKS[cp],
        false => CMAP_BLOCKS[CMAP_TOP[cp >> 8] as usize | (cp & 255)],
    };
    class as usize
}
