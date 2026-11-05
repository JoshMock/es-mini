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
pub(crate) fn match_none() -> Query {
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
                lenient: false,
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
        relation: Default::default(),
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
/// operator. Malformed syntax gives a [`Query::Invalid`] failing as Elasticsearch's
/// QueryStringQueryBuilder.doToQuery does on each shard.
pub fn parse_query_string(q: &str, df: &str, and: bool) -> Query {
    if q.trim().is_empty() {
        return match_none();
    }
    let parsed = lex(q).and_then(|toks| {
        let mut p = Parser { toks, at: 0, and };
        let query = p.query(df)?;
        match p.peek() {
            Some(_) => Err(p.unexpected()),
            None => Ok(query.unwrap_or_else(match_none)),
        }
    });
    parsed.unwrap_or_else(|e| {
        // Lucene's QueryParserBase.parse wraps the parser's ParseException or TokenMgrError
        let kind = match e.starts_with("Lexical error") {
            true => ErrorKind::TokenMgr,
            false => ErrorKind::Parse,
        };
        let lucene = QueryError::new(ErrorKind::Parse, format!("Cannot parse '{q}': {e}"));
        let lucene = lucene.caused_by(QueryError::new(kind, e));
        let msg = format!("Failed to parse query [{q}]");
        Query::Invalid(Box::new(
            QueryError::new(ErrorKind::QueryShard, msg).caused_by(lucene),
        ))
    })
}

/// Lucene's SimpleQueryParser with ES's SimpleQueryStringQueryParser leaves: `+` and `|`
/// join, `-` negates, `"phrase"~slop`, `prefix*`, `( )` groups and `\` escapes, each term a
/// most_fields multi_match over `fields`. `and` makes AND the default operator.
pub(crate) fn parse_simple_query_string(
    q: &str,
    fields: &[(String, f64)],
    and: bool,
    lenient: bool,
) -> Result<Query, String> {
    if q.trim() == "*" {
        return Ok(Query::MatchAll);
    }
    let p = Simple {
        data: q.chars().collect(),
        fields,
        and,
        lenient,
    };
    Ok(p.sub_query(0, p.data.len())?.unwrap_or_else(match_none))
}

struct Simple<'a> {
    data: Vec<char>,
    fields: &'a [(String, f64)],
    and: bool,
    lenient: bool,
}

/// SimpleQueryParser.State: the query so far, and the operators joining the next clause.
struct State {
    index: usize,
    end: usize,
    top: Option<Query>,
    /// `Some(true)` for MUST, `Some(false)` for SHOULD.
    current: Option<bool>,
    previous: Option<bool>,
    not: u32,
}

fn is_sqs_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

impl Simple<'_> {
    fn sub_query(&self, start: usize, end: usize) -> Result<Option<Query>, String> {
        let mut s = State {
            index: start,
            end,
            top: None,
            current: None,
            previous: None,
            not: 0,
        };
        while s.index < s.end {
            match self.data[s.index] {
                '(' => self.consume_sub_query(&mut s)?,
                ')' => s.index += 1,
                '"' => self.consume_phrase(&mut s)?,
                c @ ('+' | '|') => {
                    if s.current.is_none() && s.top.is_some() {
                        s.current = Some(c == '+');
                    }
                    s.index += 1;
                }
                '-' => {
                    s.not += 1;
                    s.index += 1;
                    continue;
                }
                c if is_sqs_ws(c) => s.index += 1,
                _ => self.consume_token(&mut s)?,
            }
            s.not = 0;
        }
        Ok(s.top)
    }

    fn consume_sub_query(&self, s: &mut State) -> Result<(), String> {
        s.index += 1;
        let start = s.index;
        let mut depth = 1;
        let mut escaped = false;
        while s.index < s.end {
            if !escaped {
                match self.data[s.index] {
                    '\\' => {
                        escaped = true;
                        s.index += 1;
                        continue;
                    }
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            escaped = false;
            s.index += 1;
        }
        if s.index == s.end {
            s.index = start;
        } else if s.index == start {
            s.current = None;
            s.index += 1;
        } else {
            let sub = self.sub_query(start, s.index)?;
            self.build(s, sub);
            s.index += 1;
        }
        Ok(())
    }

    fn consume_phrase(&self, s: &mut State) -> Result<(), String> {
        s.index += 1;
        let start = s.index;
        let mut phrase = String::new();
        let mut escaped = false;
        let mut has_slop = false;
        while s.index < s.end {
            if !escaped {
                match self.data[s.index] {
                    '\\' => {
                        escaped = true;
                        s.index += 1;
                        continue;
                    }
                    '"' => {
                        if self.data.get(s.index + 1) == Some(&'~') && s.index + 1 < s.end {
                            s.index += 1;
                            has_slop = s.end > s.index + 1;
                        }
                        break;
                    }
                    _ => {}
                }
            }
            escaped = false;
            phrase.push(self.data[s.index]);
            s.index += 1;
        }
        if s.index == s.end {
            s.index = start;
        } else if s.index == start {
            s.current = None;
            s.index += 1;
        } else {
            let slop = if has_slop { self.fuzziness(s) } else { 0 };
            let query = Query::MatchPhrase {
                field: String::new(),
                query: phrase,
                slop,
            };
            // MultiMatchQueryParser's phrase type: dis_max, tie_breaker 0
            self.build(s, Some(self.on_fields(query, 0.0)));
            s.index += 1;
        }
        Ok(())
    }

    fn consume_token(&self, s: &mut State) -> Result<(), String> {
        let mut token = String::new();
        let (mut escaped, mut prefix, mut fuzzy) = (false, false, false);
        while s.index < s.end {
            let c = self.data[s.index];
            if !escaped {
                if c == '\\' {
                    escaped = true;
                    prefix = false;
                    s.index += 1;
                    continue;
                } else if self.token_finished(s) {
                    break;
                } else if !token.is_empty() && c == '~' {
                    fuzzy = true;
                    break;
                }
                prefix = !token.is_empty() && c == '*';
            }
            escaped = false;
            token.push(c);
            s.index += 1;
        }
        if token.is_empty() {
            return Ok(());
        }
        if fuzzy && self.fuzziness(s) > 0 {
            // ponytail: no fuzzy `term~N`; es-mini has no fuzzy query to build
            return Err("[simple_query_string] fuzzy queries (`~`) are not supported".into());
        }
        let branch = if prefix {
            token.pop();
            let prefix = Query::Prefix {
                field: String::new(),
                prefix: token,
            };
            self.on_fields(Query::Normalized(Box::new(prefix)), 1.0)
        } else {
            let query = Query::Match {
                field: String::new(),
                query: token,
                and: self.and,
            };
            // the multi_match type SimpleQueryStringBuilder defaults to, most_fields
            self.on_fields(query, 1.0)
        };
        self.build(s, Some(branch));
        Ok(())
    }

    fn on_fields(&self, query: Query, tie_breaker: f64) -> Query {
        Query::Fields {
            fields: self.fields.to_vec(),
            query: Box::new(query),
            tie_breaker,
            lenient: self.lenient,
        }
    }

    /// SimpleQueryParser.buildQueryTree: a clause joined to the query so far, by `+` or `|`
    /// or else the default operator, nesting the query so far when the operator changes.
    fn build(&self, s: &mut State, branch: Option<Query>) {
        let Some(mut branch) = branch else { return };
        if s.not % 2 == 1 {
            branch = Query::Bool {
                must: vec![],
                should: vec![Query::MatchAll],
                must_not: vec![branch],
                filter: vec![],
                min_should: 1,
            };
        }
        s.top = Some(match s.top.take() {
            None => branch,
            Some(mut top) => {
                let must = *s.current.get_or_insert(self.and);
                if s.previous != Some(must) {
                    top = boolean_of(must, vec![top]);
                }
                if let Query::Bool {
                    must: m, should, ..
                } = &mut top
                {
                    if must { m } else { should }.push(branch);
                }
                s.previous = Some(must);
                top
            }
        });
        s.current = None;
    }

    /// SimpleQueryParser.parseFuzziness: the number after the `~` at `s.index`, 2 without one,
    /// leaving `s.index` where the token ends.
    fn fuzziness(&self, s: &mut State) -> u32 {
        let mut text = String::new();
        if self.data[s.index] != '~' {
            return 0;
        }
        while s.index < s.end {
            s.index += 1;
            if s.index < s.end {
                if self.token_finished(s) {
                    break;
                }
                text.push(self.data[s.index]);
            }
        }
        match text.is_empty() {
            true => 2,
            false => text.parse::<i32>().unwrap_or(0).max(0) as u32,
        }
    }

    fn token_finished(&self, s: &State) -> bool {
        matches!(self.data[s.index], '"' | '|' | '+' | '(' | ')') || is_sqs_ws(self.data[s.index])
    }
}

/// A boolean of `clauses`, all required (`must`) or all optional.
fn boolean_of(must: bool, clauses: Vec<Query>) -> Query {
    let (must, should) = match must {
        true => (clauses, vec![]),
        false => (vec![], clauses),
    };
    Query::Bool {
        min_should: usize::from(must.is_empty()),
        must,
        should,
        must_not: vec![],
        filter: vec![],
    }
}
