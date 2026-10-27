//! The `query_string` query and `q=` URI searches: a port of Lucene's classic QueryParser
//! grammar (QueryParser.jj) with Elasticsearch's QueryStringQueryParser field handling.

use crate::{ErrorKind, Query, QueryError};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    And,
    Or,
    Not,
    Plus,
    Minus,
    LParen,
    RParen,
    Colon,
    Caret,
    Star,
    /// The image with its escapes, as are the term kinds below.
    Term(String),
    Prefix(String),
    Wild(String),
    Quoted(String),
    /// `~` and what follows it.
    Fuzzy(String),
    Range {
        lo: String,
        hi: String,
        lo_inc: bool,
        hi_inc: bool,
    },
}

fn is_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{3000}')
}

fn is_term_char(c: char) -> bool {
    !is_ws(c) && !"!():^[]\"{}~*?\\/".contains(c)
}

/// Lucene's `discardEscapeChar`: drops each escaping backslash and reads `\uXXXX` as the UTF-16
/// unit it names.
fn unescape(s: &str) -> Result<String, String> {
    let mut out = vec![];
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        let c = match c {
            '\\' => match chars.next() {
                Some('u') => {
                    let mut unit = 0;
                    for _ in 0..4 {
                        let c = chars.next().ok_or("Truncated Unicode escape sequence.")?;
                        let d = c.to_digit(16).ok_or_else(|| {
                            format!("Non-hex character in Unicode escape sequence: {c}")
                        })?;
                        unit = unit * 16 + d as u16;
                    }
                    out.push(unit);
                    continue;
                }
                Some(c) => c,
                None => return Err("Term can not end with escape character.".into()),
            },
            c => c,
        };
        out.extend_from_slice(c.encode_utf16(&mut [0; 2]));
    }
    Ok(String::from_utf16_lossy(&out))
}

fn lex(q: &str) -> Result<Vec<Tok>, String> {
    let s: Vec<char> = q.chars().collect();
    let mut toks = vec![];
    let mut i = 0;
    // a backslash and the character it escapes, or one character `ok` accepts
    let take = |i: &mut usize, ok: &dyn Fn(char) -> bool| -> Result<bool, String> {
        match s.get(*i) {
            Some('\\') if *i + 1 < s.len() => *i += 2,
            Some('\\') => return Err("Lexical error: a backslash ends the query".into()),
            Some(&c) if ok(c) => *i += 1,
            _ => return Ok(false),
        }
        Ok(true)
    };
    let quoted = |i: &mut usize| -> Result<String, String> {
        let start = *i + 1;
        *i = start;
        while s.get(*i) != Some(&'"') {
            match s.get(*i) {
                None => return Err("Lexical error: unterminated quote".into()),
                Some('\\') => *i += 2,
                _ => *i += 1,
            }
        }
        *i += 1;
        Ok(s[start..*i - 1].iter().collect())
    };
    while i < s.len() {
        let c = s[i];
        let tok = match c {
            _ if is_ws(c) => {
                i += 1;
                continue;
            }
            // `+`, `-` and `!` before whitespace are terms of their own (BAREOPER)
            '+' | '-' | '!' if s.get(i + 1).is_some_and(|&n| is_ws(n)) => {
                i += 1;
                Tok::Term(c.into())
            }
            '+' | '-' | '!' | '(' | ')' | ':' | '^' => {
                i += 1;
                match c {
                    '+' => Tok::Plus,
                    '-' => Tok::Minus,
                    '!' => Tok::Not,
                    '(' => Tok::LParen,
                    ')' => Tok::RParen,
                    ':' => Tok::Colon,
                    _ => Tok::Caret,
                }
            }
            '"' => Tok::Quoted(quoted(&mut i)?),
            '~' => {
                let start = i + 1;
                i = start;
                while take(&mut i, &is_term_char)? {}
                Tok::Fuzzy(s[start..i].iter().collect())
            }
            '/' => return Err("regular expressions are not supported".into()),
            '[' | '{' => {
                i += 1;
                let bound = |i: &mut usize| -> Result<String, String> {
                    while s.get(*i).is_some_and(|&c| is_ws(c)) {
                        *i += 1;
                    }
                    if s.get(*i) == Some(&'"') {
                        return quoted(i);
                    }
                    let start = *i;
                    while s.get(*i).is_some_and(|&c| !matches!(c, ' ' | ']' | '}')) {
                        *i += 1;
                    }
                    Ok(s[start..*i].iter().collect())
                };
                let lo = bound(&mut i)?;
                let to = bound(&mut i)?;
                let hi = bound(&mut i)?;
                while s.get(i).is_some_and(|&c| is_ws(c)) {
                    i += 1;
                }
                let end = s.get(i).copied();
                if to != "TO" || hi.is_empty() || !matches!(end, Some(']' | '}')) {
                    return Err("Encountered a malformed range".into());
                }
                i += 1;
                Tok::Range {
                    lo,
                    hi,
                    lo_inc: c == '[',
                    hi_inc: end == Some(']'),
                }
            }
            _ => {
                let start = i;
                while take(&mut i, &|c| is_term_char(c) || "*?".contains(c))? {}
                if i == start {
                    return Err(format!("Lexical error: unexpected [{c}]"));
                }
                let image: String = s[start..i].iter().collect();
                // the image's unescaped wildcards, by position
                let mut wild = vec![];
                let mut chars = image.char_indices();
                while let Some((at, c)) = chars.next() {
                    match c {
                        '\\' => _ = chars.next(),
                        '*' | '?' => wild.push((at, c)),
                        _ => {}
                    }
                }
                match (image.as_str(), wild.as_slice()) {
                    ("AND" | "&&", _) => Tok::And,
                    ("OR" | "||", _) => Tok::Or,
                    ("NOT", _) => Tok::Not,
                    ("*", _) => Tok::Star,
                    (_, []) => Tok::Term(image),
                    (_, [(at, '*')]) if *at > 0 && *at == image.len() - 1 => {
                        Tok::Prefix(image[..*at].into())
                    }
                    _ => Tok::Wild(image),
                }
            }
        };
        toks.push(tok);
    }
    Ok(toks)
}

#[derive(Clone, Copy, PartialEq)]
enum Occur {
    Must,
    Should,
    MustNot,
}

struct Parser {
    toks: Vec<Tok>,
    at: usize,
    and: bool,
}

/// No documents.
fn match_none() -> Query {
    Query::Bool {
        must: vec![],
        should: vec![],
        must_not: vec![],
        filter: vec![],
        min_should: 1,
    }
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.at)
    }

    fn eat(&mut self, t: &Tok) -> bool {
        let found = self.peek() == Some(t);
        self.at += usize::from(found);
        found
    }

    /// Lucene's `Query` production: clauses joined by conjunctions, `None` if they analyze to
    /// nothing.
    fn query(&mut self, field: &str) -> Result<Option<Query>, String> {
        let mut clauses = vec![];
        let mut first = None;
        let mut started = false;
        loop {
            let conj = match started {
                false => None,
                true if self.eat(&Tok::And) => Some(Occur::Must),
                true if self.eat(&Tok::Or) => Some(Occur::Should),
                true => None,
            };
            if started && conj.is_none() && matches!(self.peek(), None | Some(Tok::RParen)) {
                break;
            }
            let modifier = match self.peek() {
                Some(Tok::Plus) => Some(Occur::Must),
                Some(Tok::Minus | Tok::Not) => Some(Occur::MustNot),
                _ => None,
            };
            self.at += usize::from(modifier.is_some());
            let q = self.clause(field)?;
            if !started && modifier.is_none() {
                first = q.clone();
            }
            self.add_clause(&mut clauses, conj, modifier, q);
            started = true;
        }
        if clauses.len() == 1 && first.is_some() {
            return Ok(first);
        }
        Ok(boolean(clauses))
    }

    /// Port of QueryParserBase.addClause.
    fn add_clause(
        &self,
        clauses: &mut Vec<(Occur, Query)>,
        conj: Option<Occur>,
        modifier: Option<Occur>,
        q: Option<Query>,
    ) {
        if let Some((occur, _)) = clauses.last_mut()
            && *occur != Occur::MustNot
        {
            match conj {
                Some(Occur::Must) => *occur = Occur::Must,
                Some(Occur::Should) if self.and => *occur = Occur::Should,
                _ => {}
            }
        }
        let Some(q) = q else { return };
        let prohibited = modifier == Some(Occur::MustNot);
        let required = match self.and {
            false => modifier == Some(Occur::Must) || (conj == Some(Occur::Must) && !prohibited),
            true => !prohibited && conj != Some(Occur::Should),
        };
        let occur = match (prohibited, required) {
            (true, _) => Occur::MustNot,
            (_, true) => Occur::Must,
            _ => Occur::Should,
        };
        clauses.push((occur, q));
    }

    fn clause(&mut self, field: &str) -> Result<Option<Query>, String> {
        let mut field = field.to_owned();
        if self.toks.get(self.at + 1) == Some(&Tok::Colon) {
            match self.peek() {
                Some(Tok::Term(f)) => field = unescape(f)?,
                Some(Tok::Star) => field = "*".into(),
                _ => {}
            }
            if matches!(self.peek(), Some(Tok::Term(_) | Tok::Star)) {
                self.at += 2;
            }
        }
        let q = if self.eat(&Tok::LParen) {
            let q = self.query(&field)?;
            if !self.eat(&Tok::RParen) {
                return Err(self.unexpected());
            }
            q
        } else {
            self.term(&field)?
        };
        if !self.eat(&Tok::Caret) {
            return Ok(q);
        }
        // Lucene's NUMBER token: digits, then optionally a dot and digits
        let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        let boost = match self.peek() {
            Some(Tok::Term(t))
                if t.split_once('.')
                    .map_or(digits(t), |(a, b)| digits(a) && digits(b)) =>
            {
                t.parse::<f32>().map_err(|e| e.to_string())?
            }
            _ => return Err(self.unexpected()),
        };
        self.at += 1;
        Ok(q.map(|q| q.boosted(boost.into())))
    }

    fn term(&mut self, field: &str) -> Result<Option<Query>, String> {
        let tok = self.peek().cloned();
        self.at += 1;
        let fuzzy = match self.peek() {
            Some(Tok::Fuzzy(f)) => Some(f.clone()),
            _ => None,
        };
        self.at += usize::from(fuzzy.is_some());
        if fuzzy.is_some() && !matches!(tok, Some(Tok::Quoted(_))) {
            return Err("fuzzy queries (`~`) are not supported".into());
        }
        let field = field.to_owned();
        let all = field == "*" && tok != Some(Tok::Star);
        let q = match tok {
            Some(Tok::Term(t)) => self.field_query(field, unescape(&t)?),
            Some(Tok::Star) if field == "*" => Query::MatchAll,
            Some(Tok::Star) => Query::Exists { field },
            Some(Tok::Prefix(p)) => Query::Normalized(Box::new(Query::Prefix {
                field,
                prefix: unescape(&p)?,
            })),
            Some(Tok::Wild(w)) => Query::Normalized(Box::new(Query::Wildcard {
                field,
                pattern: w,
                case_insensitive: false,
            })),
            Some(Tok::Range {
                lo,
                hi,
                lo_inc,
                hi_inc,
            }) => range(
                field,
                Some(unescape(&lo)?),
                Some(unescape(&hi)?),
                lo_inc,
                hi_inc,
            ),
            Some(Tok::Quoted(t)) if field == "_exists_" => Query::Exists {
                field: unescape(&t)?,
            },
            Some(Tok::Quoted(t)) => Query::MatchPhrase {
                field,
                query: unescape(&t)?,
                // Lucene reads the slop as a float and ignores one that does not parse
                slop: fuzzy
                    .and_then(|f| f.parse::<f32>().ok())
                    .map_or(0, |f| f as u32),
            },
            _ => {
                self.at -= 1;
                return Err(self.unexpected());
            }
        };
        // ES queries each term on every field, as a best_fields multi_match
        Ok(Some(match all {
            true => Query::Fields {
                fields: vec![("*".into(), 1.0)],
                query: Box::new(q),
                tie_breaker: 0.0,
            },
            false => q,
        }))
    }

    /// ES's `getFieldQuery` for an unquoted term.
    fn field_query(&self, field: String, text: String) -> Query {
        if field == "_exists_" {
            return Query::Exists { field: text };
        }
        if text.chars().count() > 1 {
            // `>=x`, `>x`, `<=x` and `<x` are ranges open on one side
            if let Some(r) = text.strip_prefix(">=") {
                return range(field, Some(r.into()), None, true, true);
            } else if let Some(r) = text.strip_prefix('>') {
                return range(field, Some(r.into()), None, false, true);
            } else if let Some(r) = text.strip_prefix("<=") {
                return range(field, None, Some(r.into()), true, true);
            } else if let Some(r) = text.strip_prefix('<') {
                return range(field, None, Some(r.into()), true, false);
            }
        }
        Query::Match {
            field,
            query: text,
            and: self.and,
        }
    }

    fn unexpected(&self) -> String {
        match self.peek() {
            None => "Encountered \"<EOF>\"".into(),
            Some(t) => format!("Encountered {t:?}"),
        }
    }
}

/// A range on `field`, its bounds normalized like the field's terms; a `*` bound is open.
fn range(
    field: String,
    lo: Option<String>,
    hi: Option<String>,
    lo_inc: bool,
    hi_inc: bool,
) -> Query {
    let open = |b: Option<String>| b.filter(|b| b != "*").map(|s| Box::new(Value::String(s)));
    let (lo, hi) = (open(lo), open(hi));
    let (gte, gt) = if lo_inc { (lo, None) } else { (None, lo) };
    let (lte, lt) = if hi_inc { (hi, None) } else { (None, hi) };
    Query::Normalized(Box::new(Query::Range {
        field,
        gt,
        gte,
        lt,
        lte,
        format: None,
        time_zone: None,
    }))
}

/// Lucene's `getBooleanQuery`, with ES's `fixNegativeQueryIfNeeded`: only prohibited clauses
/// match every other document.
fn boolean(clauses: Vec<(Occur, Query)>) -> Option<Query> {
    if clauses.is_empty() {
        return None;
    }
    let of = |o: Occur| {
        (clauses.iter())
            .filter(|(c, _)| *c == o)
            .map(|(_, q)| q.clone())
            .collect::<Vec<_>>()
    };
    let (must, should, must_not) = (of(Occur::Must), of(Occur::Should), of(Occur::MustNot));
    let negative = must.is_empty() && should.is_empty();
    Some(Query::Bool {
        min_should: usize::from(must.is_empty() && !should.is_empty()),
        must,
        should,
        must_not,
        filter: if negative {
            vec![Query::MatchAll]
        } else {
            vec![]
        },
    })
}

/// Parses Lucene query syntax: `[field:]term`, `"phrases"~slop`, `prefix*`, `wild?cards`,
/// `[a TO b}` and `>=x` ranges, `AND`/`OR`/`NOT`/`&&`/`||`/`!`/`+`/`-`, `( )` groups,
/// `_exists_:field` and `\` escapes. `df` is the default field; `and` makes AND the default
/// operator.
pub fn parse_query_string(q: &str, df: &str, and: bool) -> Result<Query, QueryError> {
    if q.trim().is_empty() {
        return Ok(match_none());
    }
    let err = |e: String| QueryError {
        kind: ErrorKind::Parse,
        msg: format!("Cannot parse '{q}': {e}"),
    };
    let mut p = Parser {
        toks: lex(q).map_err(err)?,
        at: 0,
        and,
    };
    let query = p.query(df).map_err(err)?;
    if p.peek().is_some() {
        return Err(err(p.unexpected()));
    }
    Ok(query.unwrap_or_else(match_none))
}
