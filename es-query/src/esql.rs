//! ES|QL subset: `FROM` (with `METADATA _id, _index, _version`), `ROW` and `SHOW INFO`, with
//! `WHERE`, `EVAL`, `LIMIT`, `KEEP`, `DROP`, `RENAME`, `SORT`, and `STATS ... [BY ...]` over
//! `COUNT`, `COUNT_DISTINCT`, `MIN`, `MAX`, `SUM`, `AVG`, `MEDIAN`, `PERCENTILE` and `VALUES`,
//! grouped by expressions such as `BUCKET`.
//!
//! Expressions support comparisons (`== != < <= > >=`), `AND`/`OR`/`NOT`, parentheses,
//! `IS [NOT] NULL`, `[NOT] IN`, `[NOT] LIKE`/`RLIKE`, arithmetic and unary minus, time span
//! literals (`1 day`), and the functions in `CONVERSIONS` and `SCALAR_FNS` over field
//! names and literals. Any other syntax is a parse error; any other function fails verification.

use es_index::{
    FieldMapping, Index,
    aliases::{self, ResolveError},
};
use serde::Serialize;
use serde_json::Value;
use std::{
    borrow::{Borrow, Cow},
    cell::{OnceCell, RefCell},
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet},
    ops::{Range, RangeInclusive},
};

/// ES applies an implicit `LIMIT 1000`.
const DEFAULT_LIMIT: usize = 1000;
/// `esql.query.result_truncation_max_size`: the cap on an explicit `LIMIT`.
const MAX_LIMIT: usize = 10000;

#[derive(Debug, Clone, PartialEq)]
pub enum EsqlError {
    /// Rendered by ES as `parsing_exception`.
    Parse(String),
    /// Rendered by ES as `verification_exception`.
    Verification(String),
    /// A source names a closed index (`cluster_block_exception`).
    IndexClosed(String),
    /// A source names a missing index while others match (`index_not_found_exception`).
    IndexNotFound(String),
    /// An evaluation error that fails the query in ES (one its evaluators do not turn into a
    /// warning and a null): the HTTP status, the error type and its reason.
    Failed {
        status: u16,
        kind: &'static str,
        reason: String,
    },
}

thread_local! {
    /// The first [`EsqlError::Failed`] raised while evaluating the running query.
    static FAILURE: RefCell<Option<EsqlError>> = const { RefCell::new(None) };
}

/// Fails the running query with `kind` and `reason` (once `run` finishes); null meanwhile.
fn fail(status: u16, kind: &'static str, reason: String) -> Option<Value> {
    FAILURE.with_borrow_mut(|f| {
        f.get_or_insert(EsqlError::Failed {
            status,
            kind,
            reason,
        });
    });
    None
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Column {
    pub name: String,
    pub r#type: String,
    /// The types a field is mapped as when indices disagree (the column is then `unsupported`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_types: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_cast: Option<String>,
}

#[derive(Debug, PartialEq)]
pub struct Response {
    pub columns: Vec<Column>,
    pub values: Vec<Vec<Value>>,
    pub documents_found: usize,
    pub values_loaded: usize,
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    Str(String),
    Num(Value),
    Sym(&'static str),
}

/// A token's `(line, column)`, both 1-based, as Elasticsearch reports them.
type Pos = (usize, usize);

/// An identifier and where it starts.
type Name = (String, Pos);

/// A token, where it starts, and its byte range in the query.
type Token = (Tok, Pos, Range<usize>);

/// A parsing error at `at`, formatted like Elasticsearch's `ParsingException`.
fn parse_error((line, col): Pos, msg: &str) -> EsqlError {
    EsqlError::Parse(format!("line {line}:{col}: {msg}"))
}

/// Input text the way ANTLR shows it in messages (`getErrorDisplay`).
fn display(text: &str) -> String {
    text.replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

/// The tokens of `src` and the position of its end.
fn lex(src: &str) -> Result<(Vec<Token>, Pos), EsqlError> {
    let mut out: Vec<Token> = Vec::new();
    let mut it = src.char_indices().peekable();
    // Whether the command being lexed takes patterns, where `*` is part of a name; elsewhere it
    // multiplies (or is `COUNT(*)`).
    let mut patterns = false;
    let (mut line, mut line_start) = (1, 0);
    let digits = |from: usize| {
        from + src[from..]
            .find(|d: char| !d.is_ascii_digit())
            .unwrap_or(src.len() - from)
    };
    while let Some(&(i, c)) = it.peek() {
        let at = (line, src[line_start..i].chars().count() + 1);
        // ANTLR's lexer error for input from `i` to `end` that no token matches.
        let unrecognized = |end: usize| {
            parse_error(
                at,
                &format!("token recognition error at: '{}'", display(&src[i..end])),
            )
        };
        if c.is_whitespace() {
            it.next();
            if c == '\n' {
                (line, line_start) = (line + 1, i + 1);
            }
            continue;
        }
        let mut end = None;
        let tok = if c == '"' {
            it.next();
            let mut s = String::new();
            loop {
                match it.next() {
                    None => return Err(unrecognized(src.len())),
                    Some((_, '"')) => break,
                    Some((j, c @ ('\n' | '\r'))) => return Err(unrecognized(j + c.len_utf8())),
                    Some((_, '\\')) => match it.next() {
                        Some((_, 'n')) => s.push('\n'),
                        Some((_, 't')) => s.push('\t'),
                        Some((_, c)) => s.push(c),
                        None => return Err(unrecognized(src.len())),
                    },
                    Some((_, c)) => s.push(c),
                }
            }
            Tok::Str(s)
        } else if c == '`' {
            it.next();
            let mut s = String::new();
            loop {
                match it.next() {
                    None => return Err(unrecognized(src.len())),
                    Some((_, '`')) => break,
                    Some((_, c)) => s.push(c),
                }
            }
            Tok::Word(s)
        } else if c.is_ascii_digit()
            || c == '.' && src[i + 1..].starts_with(|d: char| d.is_ascii_digit())
        {
            let int_end = digits(i);
            let e = match src[int_end..].starts_with('.') {
                true => digits(int_end + 1),
                false => int_end,
            };
            end = Some(e);
            let s = &src[i..e];
            let n = match s.contains('.') {
                true => s.parse::<f64>().ok().map(Value::from),
                false => (s.parse::<i64>().ok().map(Value::from))
                    .or_else(|| s.parse::<u64>().ok().map(Value::from))
                    .or_else(|| s.parse::<f64>().ok().map(Value::from)),
            };
            Tok::Num(n.ok_or_else(|| unrecognized(e))?)
        } else if c.is_alphabetic() || "_@".contains(c) || patterns && c == '*' {
            let mut s = String::new();
            while let Some(&(_, c)) = it
                .peek()
                .filter(|(_, c)| c.is_alphanumeric() || "_@.".contains(*c) || patterns && *c == '*')
            {
                s.push(c);
                it.next();
            }
            Tok::Word(s)
        } else {
            let rest = &src[i..];
            let sym = [
                "==", "!=", "<=", ">=", "<", ">", "=", "+", "-", "*", "/", "%", "|", "(", ")", ",",
                "[", "]",
            ]
            .into_iter()
            .find(|s| rest.starts_with(s))
            .ok_or_else(|| unrecognized(i + c.len_utf8()))?;
            end = Some(i + sym.len());
            Tok::Sym(sym)
        };
        if let Some(end) = end {
            while it.next_if(|&(j, _)| j < end).is_some() {}
        }
        let end = it.peek().map_or(src.len(), |&(j, _)| j);
        if let (Tok::Word(w), None | Some((Tok::Sym("|"), ..))) = (&tok, out.last()) {
            patterns =
                ["from", "keep", "drop", "rename"].contains(&w.to_ascii_lowercase().as_str());
        }
        out.push((tok, at, i..end));
    }
    Ok((out, (line, src[line_start..].chars().count() + 1)))
}

// What `SHOW INFO` reports: the Elasticsearch 9.5.5 build this server matches.
const INFO_VERSION: &str = "9.5.5";
const INFO_DATE: &str = "2026-09-30T22:12:03.984586187Z";
const INFO_HASH: &str = "48acba663b09e70b115dd879329141506c377890";

// The token sets Elasticsearch 9.5.5's ANTLR parser reports as expected at each point.
const SOURCE_COMMANDS: &str = "{, 'row', 'from', 'ts', 'promql', 'set', 'show'}";
const PROCESSING_COMMANDS: &str = "{'change_point', 'enrich', 'completion', 'dissect', 'eval', \
    'grok', 'limit', 'rerank', 'sample', 'sort', STATS, 'where', 'uri_parts', 'metrics_info', \
    'registered_domain', 'ts_info', 'user_agent', 'ts_collapse', 'ip_location', 'fork', 'fuse', \
    'inline', 'inlinestats', 'lookup', 'mmr', 'mv_expand', 'drop', 'keep', 'rename'}";
const INDEX_PATTERN: &str = "{QUOTED_STRING, '(', UNQUOTED_SOURCE}";
const ID_PATTERN: &str = "{'?', '??', NAMED_OR_POSITIONAL_PARAM, \
    NAMED_OR_POSITIONAL_DOUBLE_PARAMS, '[', ID_PATTERN}";
const EXPRESSION: &str = "{QUOTED_STRING, INTEGER_LITERAL, DECIMAL_LITERAL, 'false', 'first', \
    'last', 'not', 'null', '?', 'true', '+', '-', '??', NAMED_OR_POSITIONAL_PARAM, \
    NAMED_OR_POSITIONAL_DOUBLE_PARAMS, '[', '(', UNQUOTED_IDENTIFIER, QUOTED_IDENTIFIER}";
const CONSTANT: &str = "{QUOTED_STRING, INTEGER_LITERAL, DECIMAL_LITERAL, 'false', 'null', '?', \
    'true', '+', '-', NAMED_OR_POSITIONAL_PARAM, '['}";
// What may follow each construct, reported for a stray token that is not the last one.
const AFTER_SOURCES: &str = "{<EOF>, '|', '::', ',', 'metadata'}";
const AFTER_ID_PATTERNS: &str = "{<EOF>, '|', ',', '.'}";
const AFTER_FIELD: &str = "{<EOF>, '|', 'and', '::', '.', 'or', '+', '-', '*', '/', '%'}";
const AFTER_LITERAL: &str = "{<EOF>, '|', 'and', '::', 'or', '+', '-', '*', '/', '%'}";
const AFTER_PARENS: &str = "{'and', '::', 'or', '+', '-', '*', '/', '%', ')'}";
const AFTER_PARENS_FIELD: &str = "{'and', '::', '.', 'or', '+', '-', '*', '/', '%', ')'}";
const AFTER_IS_NULL: &str = "{<EOF>, '|', 'and', 'or'}";
const AFTER_SORT_FIELD: &str = "{<EOF>, '|', 'and', 'asc', '::', ',', 'desc', '.', 'nulls', \
    'or', '+', '-', '*', '/', '%'}";
const AFTER_SORT_ORDER: &str = "{<EOF>, '|', ',', 'nulls'}";
const AFTER_SORT_NULLS: &str = "{<EOF>, '|', ','}";
const AFTER_LIMIT: &str = "{<EOF>, '|'}";
/// The words Elasticsearch's lexer reads as keywords in an expression; any other identifier
/// after an integer is a time span's unit.
const EXPRESSION_KEYWORDS: [&str; 19] = [
    "and", "asc", "by", "desc", "false", "first", "in", "is", "last", "like", "not", "null",
    "nulls", "on", "or", "rlike", "true", "with", "where",
];

/// What may follow the last item of a comma-separated list (`ROW`, `EVAL`, `BY`), or of `STATS`
/// aggregates, that ends where `follow` may.
fn after_list(follow: &'static str, stats: bool) -> &'static str {
    match (follow, stats) {
        (AFTER_LITERAL, false) => "{<EOF>, '|', 'and', '::', ',', 'or', '+', '-', '*', '/', '%'}",
        (AFTER_FIELD, false) => {
            "{<EOF>, '|', 'and', '::', ',', '.', 'or', '+', '-', '*', '/', '%'}"
        }
        (AFTER_IS_NULL, false) => "{<EOF>, '|', 'and', ',', 'or'}",
        (AFTER_LITERAL, true) => {
            "{<EOF>, 'where', '|', 'and', 'by', '::', ',', 'or', '+', '-', '*', '/', '%'}"
        }
        // ponytail: other STATS aggregate endings keep the plain set, unchecked against ES
        _ => follow,
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Expr {
    Field(String, Span),
    Lit(Value, Span),
    /// A time span literal such as `1 year`: its count and canonical unit (see `time_unit`).
    Interval(i64, &'static str, Span),
    Cmp(&'static str, Box<Expr>, Box<Expr>, Span),
    And(Box<Expr>, Box<Expr>, Span),
    Or(Box<Expr>, Box<Expr>, Span),
    Not(Box<Expr>, Span),
    IsNull(Box<Expr>, bool, Span),
    /// `x LIKE "pattern"`, or `x RLIKE "pattern"` when the flag is set.
    Like(Box<Expr>, String, bool, Span),
    /// `+ - * / %`, with the numeric type `resolve` gives it, which `eval` computes in.
    Arith(
        &'static str,
        Box<Expr>,
        Box<Expr>,
        OnceCell<&'static str>,
        Span,
    ),
    /// Unary `-`, with the numeric type `resolve` gives it.
    Neg(Box<Expr>, OnceCell<&'static str>, Span),
    /// A function call, with its canonical name and, once `resolve` checked it, its argument's
    /// type (a conversion) or its result type (a scalar function).
    Call(
        String,
        Vec<Expr>,
        OnceCell<(&'static str, &'static str)>,
        Span,
    ),
}

/// Where an expression starts, and its text's byte range in the query.
type Span = (Pos, Range<usize>);

#[derive(Debug, Clone, PartialEq)]
struct SortKey {
    field: String,
    at: Pos,
    desc: bool,
    nulls_first: bool,
}

/// Where the rows come from.
#[derive(Debug, Clone, PartialEq)]
enum Source {
    /// Index expressions, the `METADATA` fields to add as columns, and where `FROM` starts.
    From(Vec<String>, Vec<Name>, Pos),
    /// `ROW`, and `SHOW INFO` as a row of constants: named expressions without fields.
    Row(Vec<(String, Expr)>),
}

#[derive(Debug, Clone, PartialEq)]
enum Command {
    Where(Expr),
    /// Named expressions, each a column added (or moved last) in order.
    Eval(Vec<(String, Expr)>),
    Limit(usize),
    Keep(Vec<Name>),
    Drop(Vec<Name>),
    /// `old AS new` (or `new = old`) pairs, each with its text.
    Rename(Vec<(Name, String, String)>),
    Sort(Vec<SortKey>),
    /// Aggregates (see `aggregate`), then the named grouping expressions.
    Stats(Vec<(String, Expr)>, Vec<(String, Expr)>),
}

/// A recursive-descent parser that fails with the messages of Elasticsearch's ANTLR parser.
struct Parser<'a> {
    src: &'a str,
    toks: Vec<Token>,
    eof: Pos,
    pos: usize,
    /// The token where the current top-level operand of a boolean expression starts; ANTLR
    /// reports a failure past it as "no viable alternative" from there.
    operand: usize,
    parens: usize,
    /// The tokens that may follow what was parsed last.
    follow: &'static str,
    /// The query's start time in epoch millis, which every `NOW()` reads.
    now: i64,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|(t, ..)| t)
    }

    fn next(&mut self) -> Option<Token> {
        self.pos += 1;
        self.toks.get(self.pos - 1).cloned()
    }

    fn keyword(&mut self, kw: &str) -> bool {
        let hit = matches!(self.peek(), Some(Tok::Word(w)) if w.eq_ignore_ascii_case(kw));
        self.pos += hit as usize;
        hit
    }

    fn sym(&mut self, s: &'static str) -> bool {
        let hit = self.peek() == Some(&Tok::Sym(s));
        self.pos += hit as usize;
        hit
    }

    fn error<T>(&self, msg: &str) -> Result<T, EsqlError> {
        let at = self.toks.get(self.pos).map_or(self.eof, |(_, at, _)| *at);
        Err(parse_error(at, msg))
    }

    /// The current token as ANTLR shows it.
    fn text(&self) -> String {
        self.toks
            .get(self.pos)
            .map_or("<EOF>".into(), |(.., r)| display(&self.src[r.clone()]))
    }

    fn mismatched<T>(&self, expecting: &str) -> Result<T, EsqlError> {
        self.error(&format!(
            "mismatched input '{}' expecting {expecting}",
            self.text()
        ))
    }

    /// A required token is absent: ANTLR reports it as missing when the current token could
    /// follow it.
    fn missing<T>(&self, expecting: &str) -> Result<T, EsqlError> {
        match self.peek() {
            None | Some(Tok::Sym("|")) => {
                self.error(&format!("missing {expecting} at '{}'", self.text()))
            }
            _ => self.mismatched(expecting),
        }
    }

    /// An expression cannot start, or cannot continue, at the current token.
    fn bad_expression<T>(&self) -> Result<T, EsqlError> {
        if self.pos == self.operand {
            return self.mismatched(EXPRESSION);
        }
        self.no_viable_alternative(self.operand)
    }

    /// ANTLR found no rule that fits the tokens from `from` through the current one.
    fn no_viable_alternative<T>(&self, from: usize) -> Result<T, EsqlError> {
        let start = self.toks[from].2.start;
        let end = self
            .toks
            .get(self.pos)
            .map_or(self.src.len(), |(.., r)| r.end);
        self.error(&format!(
            "no viable alternative at input '{}'",
            display(&self.src[start..end])
        ))
    }

    /// A command Elasticsearch knows but this subset does not run.
    fn unsupported<T>(&self, commands: &str) -> Result<T, EsqlError> {
        match self.peek() {
            Some(Tok::Word(w)) if commands.contains(&format!("'{}'", w.to_ascii_lowercase())) => {
                self.error(&format!("unsupported command [{}]", w.to_ascii_lowercase()))
            }
            _ => self.mismatched(commands),
        }
    }

    fn ident(&mut self, expecting: &str) -> Result<Name, EsqlError> {
        match self.peek() {
            Some(Tok::Word(_)) => match self.next() {
                Some((Tok::Word(w), at, _)) => Ok((w, at)),
                _ => unreachable!("peeked a word"),
            },
            _ => self.mismatched(expecting),
        }
    }

    fn ident_list(&mut self, expecting: &str) -> Result<Vec<Name>, EsqlError> {
        let mut out = vec![self.ident(expecting)?];
        while self.sym(",") {
            out.push(self.ident(expecting)?);
        }
        Ok(out)
    }

    fn limit(&mut self) -> Result<Command, EsqlError> {
        let (_, at, ref keyword) = self.toks[self.pos - 1];
        let start = keyword.start;
        let negative = self.sym("-");
        let Some((Tok::Num(n), _, lit)) = self.toks.get(self.pos).cloned() else {
            return self.mismatched(CONSTANT);
        };
        let (_, lit_at, ref r) = self.toks[self.pos - negative as usize];
        let from = r.start;
        self.pos += 1;
        self.follow = AFTER_LIMIT;
        let unit = match n.as_i64() {
            Some(_) => self.time_unit_here(lit_at)?,
            None => None,
        };
        let (end, r#type) = match unit {
            Some((unit, word)) => (word.end, span_type(unit)),
            None => match n.as_u64().filter(|&n| !negative && n <= i32::MAX as u64) {
                Some(n) => return Ok(Command::Limit(n as usize)),
                None => (lit.end, number_type(&n, negative)),
            },
        };
        // ANTLR's text of a time span leaves out the space: `1day`.
        let value = self.src[from..end].replace(char::is_whitespace, "");
        Err(parse_error(
            at,
            &format!(
                "value of [{}] must be a non negative integer, found value [{value}] type [{type}]",
                &self.src[start..end]
            ),
        ))
    }

    /// The unit of a time span whose integer, starting at `at`, ends before the current token:
    /// an unquoted identifier that is not a keyword (and then the token is consumed).
    fn time_unit_here(
        &mut self,
        at: Pos,
    ) -> Result<Option<(&'static str, Range<usize>)>, EsqlError> {
        let Some((Tok::Word(w), _, r)) = self.toks.get(self.pos) else {
            return Ok(None);
        };
        if self.src[r.clone()].starts_with('`')
            || EXPRESSION_KEYWORDS.contains(&w.to_ascii_lowercase().as_str())
        {
            return Ok(None);
        }
        let Some(unit) = time_unit(w) else {
            return Err(parse_error(at, &format!("Unexpected temporal unit: '{w}'")));
        };
        let r = r.clone();
        self.pos += 1;
        Ok(Some((unit, r)))
    }

    fn source(&mut self) -> Result<Source, EsqlError> {
        if self.keyword("show") {
            if !self.keyword("info") {
                return self.mismatched("'info'");
            }
            let lit = |v: &str| Expr::Lit(v.into(), ((1, 1), 0..0));
            return Ok(Source::Row(vec![
                ("version".into(), lit(INFO_VERSION)),
                ("date".into(), lit(INFO_DATE)),
                ("hash".into(), lit(INFO_HASH)),
            ]));
        }
        if self.keyword("row") {
            return Ok(Source::Row(self.assignments(false)?));
        }
        let from = self.toks.get(self.pos).map_or(self.eof, |t| t.1);
        if !self.keyword("from") {
            return self.unsupported(SOURCE_COMMANDS);
        }
        let sources = self.ident_list(INDEX_PATTERN)?;
        self.follow = AFTER_SOURCES;
        let mut metadata = Vec::new();
        if self.keyword("metadata") {
            metadata = self.ident_list(ID_PATTERN)?;
            self.follow = AFTER_ID_PATTERNS;
        }
        let sources = sources.into_iter().map(|(s, _)| s).collect();
        Ok(Source::From(sources, metadata, from))
    }

    /// `name = expression, ...`, where an unnamed expression is named by its text.
    fn assignments(&mut self, stats: bool) -> Result<Vec<(String, Expr)>, EsqlError> {
        let mut fields = Vec::new();
        loop {
            let named = matches!(
                (self.peek(), self.toks.get(self.pos + 1)),
                (Some(Tok::Word(_)), Some((Tok::Sym("="), ..)))
            );
            let name = match named {
                true => {
                    self.pos += 2;
                    Some(self.toks[self.pos - 2].2.clone())
                }
                false => None,
            };
            let e = self.or()?;
            let name = name.unwrap_or_else(|| span(&e).1.clone());
            fields.push((self.src[name].to_owned(), e));
            if !self.sym(",") {
                self.follow = after_list(self.follow, stats);
                return Ok(fields);
            }
        }
    }

    fn query(&mut self) -> Result<(Source, Vec<Command>), EsqlError> {
        let sources = self.source()?;
        let mut cmds = Vec::new();
        while self.sym("|") {
            let command = match self.peek() {
                Some(Tok::Word(w)) => w.to_ascii_lowercase(),
                _ => String::new(),
            };
            if ![
                "where", "eval", "limit", "keep", "drop", "rename", "sort", "stats",
            ]
            .contains(&command.as_str())
            {
                return self.unsupported(PROCESSING_COMMANDS);
            }
            self.pos += 1;
            let cmd = match command.as_str() {
                "where" => Command::Where(self.or()?),
                "eval" => Command::Eval(self.assignments(false)?),
                "stats" => {
                    let aggs = self.assignments(true)?;
                    let by = match self.keyword("by") {
                        true => self.assignments(false)?,
                        false => Vec::new(),
                    };
                    Command::Stats(aggs, by)
                }
                "limit" => self.limit()?,
                "rename" => {
                    let mut pairs = Vec::new();
                    loop {
                        let start = self.pos;
                        let a = self.ident(ID_PATTERN)?;
                        let (old, new) = match () {
                            _ if self.keyword("as") => (a, self.ident(ID_PATTERN)?.0),
                            _ if self.sym("=") => (self.ident(ID_PATTERN)?, a.0),
                            _ => return self.no_viable_alternative(start),
                        };
                        let text = self.src[self.span(start).1].to_owned();
                        pairs.push((old, new, text));
                        if !self.sym(",") {
                            break;
                        }
                    }
                    self.follow = AFTER_ID_PATTERNS;
                    Command::Rename(pairs)
                }
                "keep" | "drop" => {
                    let names = self.ident_list(ID_PATTERN)?;
                    self.follow = AFTER_ID_PATTERNS;
                    match command.as_str() {
                        "keep" => Command::Keep(names),
                        _ => Command::Drop(names),
                    }
                }
                _ => {
                    let mut keys = Vec::new();
                    loop {
                        let (field, at) = self.ident(EXPRESSION)?;
                        self.follow = AFTER_SORT_FIELD;
                        let desc = self.keyword("desc");
                        if desc || self.keyword("asc") {
                            self.follow = AFTER_SORT_ORDER;
                        }
                        // Nulls are larger than any value by default.
                        let mut nulls_first = desc;
                        if self.keyword("nulls") {
                            nulls_first = match () {
                                _ if self.keyword("first") => true,
                                _ if self.keyword("last") => false,
                                _ => return self.missing("{'first', 'last'}"),
                            };
                            self.follow = AFTER_SORT_NULLS;
                        }
                        keys.push(SortKey {
                            field,
                            at,
                            desc,
                            nulls_first,
                        });
                        if !self.sym(",") {
                            break;
                        }
                    }
                    Command::Sort(keys)
                }
            };
            cmds.push(cmd);
        }
        match self.toks.len() - self.pos {
            0 => Ok((sources, cmds)),
            1 => self.error(&format!(
                "extraneous input '{}' expecting <EOF>",
                self.text()
            )),
            _ => self.mismatched(self.follow),
        }
    }

    /// The span from token `start` through the last token consumed.
    fn span(&self, start: usize) -> Span {
        let (_, at, ref r) = self.toks[start];
        (at, r.start..self.toks[self.pos - 1].2.end)
    }

    fn or(&mut self) -> Result<Expr, EsqlError> {
        let start = self.pos;
        let mut e = self.and()?;
        while self.keyword("or") {
            let right = self.and()?;
            e = Expr::Or(Box::new(e), Box::new(right), self.span(start));
        }
        Ok(e)
    }

    fn and(&mut self) -> Result<Expr, EsqlError> {
        let start = self.pos;
        let mut e = self.not()?;
        while self.keyword("and") {
            let right = self.not()?;
            e = Expr::And(Box::new(e), Box::new(right), self.span(start));
        }
        Ok(e)
    }

    fn not(&mut self) -> Result<Expr, EsqlError> {
        if self.parens == 0 {
            self.operand = self.pos;
        }
        let start = self.pos;
        match self.keyword("not") {
            true => {
                let e = self.not()?;
                Ok(Expr::Not(Box::new(e), self.span(start)))
            }
            false => self.cmp(),
        }
    }

    fn cmp(&mut self) -> Result<Expr, EsqlError> {
        let start = self.pos;
        let left = self.add()?;
        if self.keyword("is") {
            let negated = self.keyword("not");
            if !self.keyword("null") {
                return match negated {
                    true => self.missing("'null'"),
                    false => self.mismatched("{'not', 'null'}"),
                };
            }
            self.follow = AFTER_IS_NULL;
            return Ok(Expr::IsNull(Box::new(left), negated, self.span(start)));
        }
        let is = |i: usize, kw: &str| matches!(self.toks.get(self.pos + i), Some((Tok::Word(w), ..)) if w.eq_ignore_ascii_case(kw));
        let negated = is(0, "not");
        let rlike = is(negated as usize, "rlike");
        if rlike || is(negated as usize, "like") {
            self.pos += 1 + negated as usize;
            let Some(Tok::Str(p)) = self.peek().cloned() else {
                return self.mismatched("QUOTED_STRING");
            };
            if rlike && let Err(reason) = crate::regexp::compile(&p) {
                let msg = format!("Invalid regex pattern for RLIKE [{p}]: [{reason}]");
                return Err(parse_error(self.toks[start].1, &msg));
            }
            self.pos += 1;
            self.follow = AFTER_LITERAL;
            let e = Expr::Like(Box::new(left), p, rlike, self.span(start));
            return Ok(match negated {
                true => Expr::Not(Box::new(e), self.span(start)),
                false => e,
            });
        }
        // `x IN (a, b)` is `x == a OR x == b`, which has IN's three-valued result.
        if is(negated as usize, "in") {
            self.pos += 1 + negated as usize;
            if !self.sym("(") {
                return self.mismatched("'('");
            }
            self.parens += 1;
            let mut items = vec![self.add()?];
            while self.sym(",") {
                items.push(self.add()?);
            }
            if !self.sym(")") {
                return self.mismatched("{',', ')'}");
            }
            self.parens -= 1;
            self.follow = AFTER_LITERAL;
            let span = self.span(start);
            let eq = |x| Expr::Cmp("==", Box::new(left.clone()), Box::new(x), span.clone());
            let mut items = items.into_iter().map(eq);
            let first = items.next().expect("one item");
            let e = items.fold(first, |a, b| {
                Expr::Or(Box::new(a), Box::new(b), span.clone())
            });
            return Ok(match negated {
                true => Expr::Not(Box::new(e), span),
                false => e,
            });
        }
        match self.peek() {
            Some(Tok::Sym(op @ ("==" | "!=" | "<" | "<=" | ">" | ">="))) => {
                let op = *op;
                self.pos += 1;
                let right = self.add()?;
                Ok(Expr::Cmp(
                    op,
                    Box::new(left),
                    Box::new(right),
                    self.span(start),
                ))
            }
            _ => Ok(left),
        }
    }

    fn add(&mut self) -> Result<Expr, EsqlError> {
        self.arith(&["+", "-"], Self::mul)
    }

    fn mul(&mut self) -> Result<Expr, EsqlError> {
        self.arith(&["*", "/", "%"], Self::primary)
    }

    /// A left-associative chain of `operand`s joined by any of `ops`.
    fn arith(
        &mut self,
        ops: &[&'static str],
        operand: fn(&mut Self) -> Result<Expr, EsqlError>,
    ) -> Result<Expr, EsqlError> {
        let start = self.pos;
        let mut e = operand(self)?;
        loop {
            let op = match self.peek() {
                Some(Tok::Sym(s)) => *s,
                _ => return Ok(e),
            };
            let Some(&op) = ops.iter().find(|o| **o == op) else {
                return Ok(e);
            };
            self.pos += 1;
            let right = operand(self)?;
            e = Expr::Arith(
                op,
                Box::new(e),
                Box::new(right),
                OnceCell::new(),
                self.span(start),
            );
        }
    }

    fn primary(&mut self) -> Result<Expr, EsqlError> {
        if self.sym("(") {
            self.parens += 1;
            let e = self.or()?;
            if !self.sym(")") {
                return self.bad_expression();
            }
            self.parens -= 1;
            self.follow = match self.follow {
                AFTER_FIELD => AFTER_PARENS_FIELD,
                _ => AFTER_PARENS,
            };
            return Ok(e);
        }
        let start = self.pos;
        // Unary `+` is a no-op; `-` before a number is part of the literal.
        if self.sym("+") {
            return self.primary();
        }
        if self.peek() == Some(&Tok::Sym("-"))
            && !matches!(self.toks.get(self.pos + 1), Some((Tok::Num(_), ..)))
        {
            self.pos += 1;
            let x = self.primary()?;
            return Ok(Expr::Neg(Box::new(x), OnceCell::new(), self.span(start)));
        }
        if self.sym("[") {
            let mut items = Vec::new();
            loop {
                match self.primary()? {
                    Expr::Lit(v, _) => items.push(v),
                    _ => return self.mismatched(CONSTANT),
                }
                if !self.sym(",") {
                    break;
                }
            }
            if !self.sym("]") {
                return self.mismatched("{',', ']'}");
            }
            return Ok(Expr::Lit(Value::Array(items), self.span(start)));
        }
        let negative = matches!(
            (self.peek(), self.toks.get(self.pos + 1)),
            (Some(Tok::Sym("-")), Some((Tok::Num(_), ..)))
        );
        self.pos += negative as usize;
        let Some((tok, _, r)) = self.toks.get(self.pos).cloned() else {
            return self.bad_expression();
        };
        let span = (self.toks[start].1, self.toks[start].2.start..r.end);
        let (e, follow) = match tok {
            Tok::Num(n) => {
                let n = match (negative, n.as_i64()) {
                    (false, _) => n,
                    (true, Some(i)) => Value::from(-i),
                    (true, None) => Value::from(-n.as_f64().unwrap_or_default()),
                };
                // Look past the integer for a unit; the step past the literal below then
                // steps past the unit too.
                self.pos += 1;
                let unit = match n.as_i64() {
                    Some(i) => self.time_unit_here(span.0)?.map(|u| (i, u)),
                    None => None,
                };
                self.pos -= 1;
                match unit {
                    Some((i, (u, r))) => {
                        let span = (span.0, span.1.start..r.end);
                        (Expr::Interval(i, u, span), AFTER_LITERAL)
                    }
                    _ => (Expr::Lit(n, span), AFTER_LITERAL),
                }
            }
            Tok::Str(s) => (Expr::Lit(Value::String(s), span), AFTER_LITERAL),
            Tok::Word(w)
                if self
                    .toks
                    .get(self.pos + 1)
                    .is_some_and(|t| t.0 == Tok::Sym("(")) =>
            {
                self.pos += 2;
                self.parens += 1;
                let mut args = Vec::new();
                if let Some((Tok::Sym("*"), at, r)) = self.toks.get(self.pos).cloned() {
                    // `COUNT(*)`
                    self.pos += 1;
                    args.push(Expr::Field("*".into(), (at, r)));
                    if !self.sym(")") {
                        return self.bad_expression();
                    }
                } else if !self.sym(")") {
                    loop {
                        args.push(self.or()?);
                        if !self.sym(",") {
                            break;
                        }
                    }
                    if !self.sym(")") {
                        return self.bad_expression();
                    }
                }
                self.parens -= 1;
                if let Some(msg) = arity_error(&w, args.len()) {
                    let name = w.to_ascii_lowercase();
                    return Err(parse_error(
                        span.0,
                        &format!("error building [{name}]: {msg}"),
                    ));
                }
                // `NOW()` carries the query's start time, fixed once per query like ES.
                if w.eq_ignore_ascii_case("now") {
                    args.push(Expr::Lit(self.now.into(), span.clone()));
                }
                self.follow = AFTER_LITERAL;
                return Ok(Expr::Call(w, args, OnceCell::new(), self.span(start)));
            }
            Tok::Word(w) => {
                let lit = match w.to_ascii_lowercase().as_str() {
                    "null" => Some(Value::Null),
                    "true" => Some(true.into()),
                    "false" => Some(false.into()),
                    _ => None,
                };
                match lit {
                    Some(v) => (Expr::Lit(v, span), AFTER_LITERAL),
                    None => (Expr::Field(w, span), AFTER_FIELD),
                }
            }
            Tok::Sym(_) => return self.bad_expression(),
        };
        self.pos += 1;
        self.follow = follow;
        Ok(e)
    }
}

/// Columns of an index: mapped fields plus unmapped source fields, typed like ES dynamic mapping.
/// Values are `(mapping type, ES|QL type)`, e.g. `("float", "double")`.
fn columns<'a>(
    index: &Index,
    sources: impl Iterator<Item = &'a Value>,
) -> BTreeMap<String, (String, String)> {
    let mut dynamic: BTreeMap<String, String> = BTreeMap::new();
    for source in sources {
        flatten_types(&mut String::new(), source, &mut dynamic);
    }
    let mut cols: BTreeMap<_, _> = dynamic
        .into_iter()
        .map(|(c, t)| (c, (t.clone(), t)))
        .collect();
    for (name, m) in &index.mappings.properties {
        if *m == FieldMapping::Object {
            continue;
        }
        cols.insert(name.clone(), types_of(m));
        cols.remove(&format!("{name}.keyword"));
        for (sub, sm) in m.fields().into_iter().flatten() {
            cols.insert(format!("{name}.{sub}"), types_of(sm));
        }
    }
    cols
}

/// `(mapping type, ES|QL type)` of a mapped field.
fn types_of(m: &FieldMapping) -> (String, String) {
    (m.mapping_type(), m.type_name().to_owned())
}

/// ES's message for a field whose types disagree across indices (`TypeConflictedField`).
fn conflict_error(field: &str, types: &BTreeMap<String, Vec<String>>) -> String {
    let listed: Vec<String> = types
        .iter()
        .map(|(t, indices)| {
            let mut s = format!("[{t}] in [{}]", indices[..indices.len().min(3)].join(", "));
            if indices.len() > 3 {
                let noun = if indices.len() == 4 {
                    "index"
                } else {
                    "indices"
                };
                s += &format!(" and [{}] other {noun}", indices.len() - 3);
            }
            s
        })
        .collect();
    format!(
        "Cannot use field [{field}] due to ambiguities being mapped as [{}] incompatible types: {}",
        types.len(),
        listed.join(", ")
    )
}

/// Adds the dynamic types of `v`'s fields under `path`, a buffer it restores. It runs on every
/// document per query, so it allocates only for a column it has not seen.
fn flatten_types(path: &mut String, v: &Value, cols: &mut BTreeMap<String, String>) {
    let mut add = |path: &str, t: &str| {
        if !cols.contains_key(path) {
            cols.insert(path.to_owned(), t.to_owned());
        }
    };
    let len = path.len();
    let t = match v {
        Value::Object(o) => {
            for (k, v) in o {
                if len > 0 {
                    path.push('.');
                }
                path.push_str(k);
                flatten_types(path, v, cols);
                path.truncate(len);
            }
            return;
        }
        Value::Array(a) => return a.iter().for_each(|x| flatten_types(path, x, cols)),
        Value::Null => return,
        Value::String(_) => {
            path.push_str(".keyword");
            add(path, "keyword");
            path.truncate(len);
            "text"
        }
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_f64() => "double",
        Value::Number(_) => "long",
    };
    add(path, t);
}

/// The value of a column in one document; multi-values become arrays. A multi-field (or the
/// `.keyword` of an unmapped string) reads its parent's source value.
fn cell(source: &Value, column: &str, multi_field: bool) -> Value {
    let mut vals = Vec::new();
    collect(source, column, &mut vals);
    if vals.is_empty()
        && multi_field
        && let Some((base, _)) = column.rsplit_once('.')
    {
        collect(source, base, &mut vals);
    }
    match vals.len() {
        0 => Value::Null,
        1 => vals.remove(0),
        _ => Value::Array(vals),
    }
}

/// A source value as ES|QL reads it from doc values: dates as ISO strings, multi-values sorted,
/// keywords deduplicated. Text has no doc values and keeps source order.
fn doc_value(v: Value, ty: &str, m: Option<&FieldMapping>) -> Value {
    let mut vals = match v {
        Value::Array(a) => a,
        Value::Null => return v,
        v => vec![v],
    };
    if let Some(FieldMapping::Date { format, locale, .. }) = m {
        vals = vals
            .iter()
            .filter_map(|v| es_index::date_millis(format.as_deref(), locale.as_deref(), v))
            .map(|ms| es_index::iso_date(ms).into())
            .collect();
    }
    if ty != "text" {
        // ponytail: ISO dates sort as strings, right for years 0000-9999
        vals.sort_by(|a, b| compare(a, b).unwrap_or(Ordering::Equal));
    }
    if ty == "keyword" {
        vals.dedup();
    }
    match vals.len() {
        0 => Value::Null,
        1 => vals.remove(0),
        _ => Value::Array(vals),
    }
}

fn collect(v: &Value, path: &str, out: &mut Vec<Value>) {
    match v {
        Value::Array(a) => a.iter().for_each(|x| collect(x, path, out)),
        _ if path.is_empty() => {
            if !v.is_null() {
                out.push(v.clone());
            }
        }
        Value::Object(o) => {
            if let Some(x) = o.get(path) {
                collect(x, "", out);
            }
            for (i, _) in path.match_indices('.') {
                if let Some(x) = o.get(&path[..i]) {
                    collect(x, &path[i + 1..], out);
                }
            }
        }
        _ => {}
    }
}

fn compare(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64()?.partial_cmp(&y.as_f64()?),
        (Value::String(x), Value::String(y)) => Some(x.cmp(y)),
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

/// Three-valued logic: `None` is SQL NULL.
fn eval(e: &Expr, row: &Row) -> Option<Value> {
    let truth = |e: &Expr| eval(e, row).and_then(|v| v.as_bool());
    match e {
        Expr::Lit(v, _) => Some(v.clone()).filter(|v| !v.is_null()),
        // ponytail: only DATE_TRUNC and BUCKET read a time span, straight from the literal
        Expr::Interval(..) => None,
        Expr::Field(f, _) => row.get(f.as_str()).cloned().filter(|v| !v.is_null()),
        Expr::IsNull(x, negated, _) => Some(Value::Bool(eval(x, row).is_none() != *negated)),
        // A multi-value is null, like ES (which warns).
        Expr::Like(x, p, rlike, _) => match eval(x, row)? {
            // ponytail: parsed again per row; `resolve` checked that it parses
            Value::String(s) if *rlike => Some(crate::regexp::compile(p).ok()?.matches(&s).into()),
            Value::String(s) => {
                let (p, s): (Vec<char>, Vec<char>) = (p.chars().collect(), s.chars().collect());
                Some(crate::wildcard_match(&p, &s).into())
            }
            _ => None,
        },
        Expr::Not(x, _) => truth(x).map(|b| Value::Bool(!b)),
        Expr::And(a, b, _) => match (truth(a), truth(b)) {
            (Some(false), _) | (_, Some(false)) => Some(false.into()),
            (Some(true), Some(true)) => Some(true.into()),
            _ => None,
        },
        Expr::Or(a, b, _) => match (truth(a), truth(b)) {
            (Some(true), _) | (_, Some(true)) => Some(true.into()),
            (Some(false), Some(false)) => Some(false.into()),
            _ => None,
        },
        Expr::Arith(op, a, b, t, _) => arith(op, t.get().copied()?, &eval(a, row)?, &eval(b, row)?),
        // Like Java's `Math.negateExact`, null on overflow; null for a multi-value too.
        Expr::Neg(x, t, _) => {
            let v = eval(x, row)?;
            match *t.get()? {
                "double" => Some((-v.as_f64()?).into()),
                "integer" => i32::try_from(v.as_i64()?.checked_neg()?)
                    .ok()
                    .map(Value::from),
                _ => v.as_i64()?.checked_neg().map(Value::from),
            }
        }
        Expr::Call(_, args, ft, _) => {
            let &(f, t) = ft.get()?;
            if let ("DATE_TRUNC", [Expr::Interval(n, unit, _), x]) = (f, &args[..]) {
                return date_trunc(*n, unit, &eval(x, row)?);
            }
            if f == "BUCKET" {
                return bucket(args, t == "date", row);
            }
            if let "CASE" | "COALESCE" = f {
                let v = match f {
                    "COALESCE" => args.iter().find_map(|a| eval(a, row)),
                    // A multi-valued condition is false, like ES (which warns).
                    _ => args
                        .chunks(2)
                        .find_map(|c| match c {
                            [cond, v] => {
                                (eval(cond, row) == Some(true.into())).then(|| eval(v, row))
                            }
                            _ => Some(eval(&c[0], row)),
                        })
                        .flatten(),
                }?;
                // Numbers widened to double by `conditional_type`.
                let double = |v: &Value| v.as_f64().map_or(v.clone(), Value::from);
                return Some(match (t, v) {
                    ("double", Value::Array(a)) => Value::Array(a.iter().map(double).collect()),
                    ("double", v) => double(&v),
                    (_, v) => v,
                });
            }
            if SCALAR_FNS.iter().any(|(n, _)| *n == f) {
                let args: Vec<Value> = args.iter().map(|a| eval(a, row)).collect::<Option<_>>()?;
                return match f {
                    "SUBSTRING" | "TRIM" | "CONCAT" | "LENGTH" | "TO_LOWER" | "TO_UPPER"
                    | "STARTS_WITH" | "ENDS_WITH" => string_fn(f, &args),
                    "MV_COUNT" | "MV_MIN" | "MV_MAX" => mv_fn(f, &args[0]),
                    "DATE_EXTRACT" | "DATE_FORMAT" => date_fn(f, &args),
                    "NOW" => Some(es_index::iso_date(args[0].as_i64()?).into()),
                    _ => math_fn(f, t, &args),
                };
            }
            let v = eval(&args[0], row)?;
            let one = |v: &Value| convert(f, t, v);
            match v {
                // Converting each value; the ones that fail drop out, like ES (which warns).
                Value::Array(a) => {
                    let mut out: Vec<Value> = a.iter().filter_map(one).collect();
                    match out.len() {
                        0 => None,
                        1 => out.pop(),
                        _ => Some(Value::Array(out)),
                    }
                }
                v => one(&v),
            }
        }
        Expr::Cmp(op, a, b, _) => {
            // ponytail: multi-valued fields compare as null, like ES (which also warns)
            let ord = compare(&eval(a, row)?, &eval(b, row)?)?;
            let r = match *op {
                "==" => ord.is_eq(),
                "!=" => ord.is_ne(),
                "<" => ord.is_lt(),
                "<=" => ord.is_le(),
                ">" => ord.is_gt(),
                _ => ord.is_ge(),
            };
            Some(r.into())
        }
    }
}

/// `a op b` computed in type `t`, like Elasticsearch's evaluators: null on overflow, division by
/// zero, a non-finite double, or a multi-valued operand.
fn arith(op: &str, t: &str, a: &Value, b: &Value) -> Option<Value> {
    if t == "double" {
        let (a, b) = (a.as_f64()?, b.as_f64()?);
        let r = match op {
            "+" => a + b,
            "-" => a - b,
            "*" => a * b,
            _ if b == 0.0 => return None,
            "/" => a / b,
            _ => a % b,
        };
        return r.is_finite().then(|| r.into());
    }
    if t == "unsigned_long" {
        let (a, b) = (a.as_u64()?, b.as_u64()?);
        let r = match op {
            "+" => a.checked_add(b),
            "-" => a.checked_sub(b),
            "*" => a.checked_mul(b),
            "/" => a.checked_div(b),
            _ => a.checked_rem(b),
        };
        return r.map(Value::from);
    }
    let (a, b) = (a.as_i64()?, b.as_i64()?);
    // Java's `/` and `%` wrap on MIN / -1 rather than fail.
    let r = match op {
        "+" => a.checked_add(b),
        "-" => a.checked_sub(b),
        "*" => a.checked_mul(b),
        _ if b == 0 => None,
        "/" => Some(a.wrapping_div(b)),
        _ => Some(a.wrapping_rem(b)),
    };
    match (t, op) {
        ("integer", "/" | "%") => r.map(|r| Value::from(r as i32)),
        ("integer", _) => r.and_then(|r| i32::try_from(r).ok()).map(Value::from),
        _ => r.map(Value::from),
    }
}

/// The conversion functions and their result types.
const CONVERSIONS: [(&str, &str); 6] = [
    ("TO_STRING", "keyword"),
    ("TO_DATETIME", "date"),
    ("TO_INTEGER", "integer"),
    ("TO_LONG", "long"),
    ("TO_DOUBLE", "double"),
    ("TO_BOOLEAN", "boolean"),
];

/// The scalar functions and how many arguments they take.
const SCALAR_FNS: [(&str, RangeInclusive<usize>); 23] = [
    ("NOW", 0..=0),
    ("BUCKET", 2..=4),
    ("DATE_TRUNC", 2..=2),
    ("DATE_EXTRACT", 2..=2),
    ("DATE_FORMAT", 1..=2),
    ("CASE", 2..=usize::MAX),
    ("COALESCE", 1..=usize::MAX),
    ("CONCAT", 2..=usize::MAX),
    ("LENGTH", 1..=1),
    ("SUBSTRING", 2..=3),
    ("TO_LOWER", 1..=1),
    ("TO_UPPER", 1..=1),
    ("TRIM", 1..=1),
    ("STARTS_WITH", 2..=2),
    ("ENDS_WITH", 2..=2),
    ("ABS", 1..=1),
    ("CEIL", 1..=1),
    ("FLOOR", 1..=1),
    ("POW", 2..=2),
    ("ROUND", 1..=2),
    ("MV_COUNT", 1..=1),
    ("MV_MIN", 1..=1),
    ("MV_MAX", 1..=1),
];

/// Elasticsearch's message (`FunctionArgumentValidation`) for calling `f` with `n` arguments,
/// when `f` is a function it knows that does not take that many.
fn arity_error(f: &str, n: usize) -> Option<&'static str> {
    let f = f.to_ascii_uppercase();
    let arity = match SCALAR_FNS.iter().find(|(name, _)| *name == f) {
        Some((_, arity)) => arity.clone(),
        None if f == "TO_INTEGER" || f == "TO_LONG" => 1..=2,
        None if CONVERSIONS.iter().any(|(name, _)| *name == f) => 1..=1,
        None => return None,
    };
    (!arity.contains(&n)).then_some(match (*arity.start(), *arity.end()) {
        (0, 0) => "expects no arguments",
        (1, 1) => "expects exactly one argument",
        (2, 2) => "expects exactly two arguments",
        (1, 2) => "expects one or two arguments",
        (2, 3) => "expects two or three arguments",
        (2, 4) => "expects minimum two, maximum four arguments",
        (1, _) => "expects at least one argument",
        _ => "expects at least two arguments",
    })
}

/// What argument `i` of scalar function `f` must be, in Elasticsearch's words, and whether type
/// `t` is that (null always is).
fn accepts(f: &str, i: usize, t: &str) -> (&'static str, bool) {
    let (accepted, ok) = match (f, i) {
        ("SUBSTRING" | "TRIM", 0)
        | ("CONCAT" | "LENGTH" | "TO_LOWER" | "TO_UPPER" | "STARTS_WITH" | "ENDS_WITH", _) => {
            ("string", t == "keyword" || t == "text")
        }
        ("SUBSTRING", _) => ("integer", t == "integer"),
        ("DATE_EXTRACT" | "DATE_FORMAT", 0) => ("string", t == "keyword" || t == "text"),
        ("DATE_TRUNC", 0) => (
            "dateperiod or timeduration",
            t == "date_period" || t == "time_duration",
        ),
        ("DATE_TRUNC", _) => ("date_nanos or datetime", t == "date"),
        ("DATE_EXTRACT" | "DATE_FORMAT", _) => ("datetime or date_nanos", t == "date"),
        ("BUCKET", 0) => ("datetime or numeric", t == "date" || NUMERIC.contains(&t)),
        // Checked by `bucket_problem`, which knows the field's type.
        ("BUCKET", _) => ("any", true),
        // ponytail: no spatial types exist here (no geo fields or TO_GEOPOINT) for ES to refuse
        ("MV_COUNT" | "MV_MIN" | "MV_MAX", _) => ("any", true),
        ("ROUND", 1) => (
            "whole number except unsigned_long or counter types",
            t == "integer" || t == "long",
        ),
        _ => ("numeric", NUMERIC.contains(&t)),
    };
    (accepted, ok || t == "null")
}

/// How Elasticsearch names argument `i` in a type error (`ParamOrdinal.fromIndex`).
fn ordinal(i: usize) -> &'static str {
    ["first ", "second ", "third ", "fourth ", "fifth "]
        .get(i)
        .unwrap_or(&"")
}

/// The type of `CASE` or `COALESCE` over arguments of types `ts`, like Elasticsearch's type
/// resolution after its `ImplicitCasting` rule: numbers widen to the first numeric argument's
/// type when all of them can, then every value must have the first non-null value's type (text
/// counting as keyword) and every `CASE` condition must be boolean. Otherwise, the position of
/// the first argument that fails and the type it must be.
fn conditional_type(f: &str, ts: &[String]) -> Result<String, (usize, String)> {
    let numeric = |t: &str| NUMERIC.contains(&t);
    let target = ts.iter().map(String::as_str).find(|t| numeric(t));
    // ponytail: unsigned_long widens to double here; not checked against Elasticsearch
    let widens = |t: &str| {
        Some(t) == target || target == Some("double") || (t, target) == ("integer", Some("long"))
    };
    let cast = target.filter(|_| ts.iter().all(|t| !numeric(t) || widens(t)));
    let mut out = "null";
    for (i, t) in ts.iter().enumerate() {
        let t = match (t.as_str(), cast) {
            ("text", _) => "keyword",
            (t, Some(c)) if numeric(t) => c,
            (t, _) => t,
        };
        if f == "CASE" && i % 2 == 0 && i + 1 < ts.len() {
            if t != "boolean" && t != "null" {
                return Err((i, "boolean".into()));
            }
        } else if out == "null" {
            out = t;
        } else if t != out && t != "null" {
            return Err((i, out.into()));
        }
    }
    Ok(out.into())
}

/// Math function `f` of its arguments, the first of type `t`, like Elasticsearch's evaluators:
/// `None` on overflow, a non-finite result or a multi-valued argument (where ES warns). Whole
/// numbers pass through `FLOOR`, `CEIL` and `ROUND` without decimals untouched, multi-values too.
fn math_fn(f: &str, t: &str, args: &[Value]) -> Option<Value> {
    let x = &args[0];
    if f == "POW" {
        let r = x.as_f64()?.powf(args[1].as_f64()?);
        return r.is_finite().then(|| r.into());
    }
    if t == "double" {
        let x = x.as_f64()?;
        let r = match f {
            "ABS" => x.abs(),
            "CEIL" => x.ceil(),
            "FLOOR" => x.floor(),
            _ => round_double(x, args.get(1).map_or(Some(0), Value::as_i64)?),
        };
        return Some(r.into());
    }
    match f {
        "ABS" if t != "unsigned_long" => {
            let a = x.as_i64()?.checked_abs()?;
            if t == "integer" {
                i32::try_from(a).ok().map(Value::from)
            } else {
                Some(a.into())
            }
        }
        "ROUND" if args.len() == 2 => {
            let n = x.as_i64().map(i128::from).or(x.as_u64().map(i128::from))?;
            let r = round_whole(n, args[1].as_i64()?);
            // ponytail: an overflowing long fails the query in ES; null here
            match t {
                "integer" => i32::try_from(r).ok().map(Value::from),
                "long" => i64::try_from(r).ok().map(Value::from),
                _ => u64::try_from(r).ok().map(Value::from),
            }
        }
        _ => Some(x.clone()),
    }
}

/// Elasticsearch's `Maths.round(long, long)`: half away from zero at `-decimals` digits.
fn round_whole(n: i128, decimals: i64) -> i128 {
    if n == 0 || decimals >= 0 {
        return n;
    }
    let digits = ((n.abs() as f64).log10() + 1.0) as i64;
    if digits <= -decimals {
        return 0;
    }
    let unit = 10i128.pow((-decimals - 1) as u32);
    let scale = unit * 10;
    let (mut mid, rem) = (n / scale, n % scale);
    if rem >= 5 * unit {
        mid += 1;
    } else if rem <= -5 * unit {
        mid -= 1;
    }
    mid * scale
}

/// Elasticsearch's `Maths.round(Number, long)` for a double: half away from zero.
// ponytail: past 2^63 ES divides in BigDecimal; an f64 division can differ by an ulp there
fn round_double(x: f64, decimals: i64) -> f64 {
    let scale = 10f64.powf(decimals as f64);
    if scale == 0.0 || x == 0.0 {
        return 0.0;
    }
    let mid = x * scale;
    if mid.is_infinite() {
        return x;
    }
    let rounded = if mid.abs() < 2f64.powi(63) {
        java_round(mid.abs())
    } else {
        mid.abs()
    };
    rounded / scale * mid.signum()
}

/// Multi-value function `f` of `v`, ignoring nulls: its count, or its least or greatest value.
fn mv_fn(f: &str, v: &Value) -> Option<Value> {
    let vs = match v {
        Value::Array(a) => &a[..],
        v => std::slice::from_ref(v),
    };
    let vs = vs.iter().filter(|v| !v.is_null());
    if f == "MV_COUNT" {
        return Some(vs.count().into());
    }
    let pick = if f == "MV_MIN" {
        Ordering::Less
    } else {
        Ordering::Greater
    };
    vs.reduce(|a, b| if compare(b, a) == Some(pick) { b } else { a })
        .cloned()
}

/// `DATE_EXTRACT(part, date)` or `DATE_FORMAT([pattern,] date)` in UTC, like Elasticsearch's
/// evaluators: `None` for a multi-valued argument (where ES warns).
// ponytail: an unknown part or a pattern es_index::format_date lacks (week fields, zone
// names) is null, where ES fails the query or formats them
fn date_fn(f: &str, args: &[Value]) -> Option<Value> {
    const DAY: i64 = 86_400_000;
    let t = es_index::parse_date_millis(args.last()?)?;
    if f == "DATE_FORMAT" {
        return match args {
            [pattern, _] => {
                es_index::format_date(pattern.as_str()?, t, &es_index::tz::TimeZone::UTC)
            }
            _ => Some(es_index::iso_date(t)),
        }
        .map(Value::from);
    }
    let (days, ms) = (t.div_euclid(DAY), t.rem_euclid(DAY));
    let (y, m, d) = es_index::civil_from_days(days);
    // Java's `ChronoField` names, matched ignoring case.
    let v = match args[0].as_str()?.to_ascii_uppercase().as_str() {
        "YEAR" => y,
        "MONTH_OF_YEAR" => m,
        "DAY_OF_MONTH" => d,
        // ISO: Monday is 1, and 1970-01-01 was a Thursday.
        "DAY_OF_WEEK" => (days + 3).rem_euclid(7) + 1,
        "DAY_OF_YEAR" => days - es_index::days_from_civil(y, 1, 1) + 1,
        "EPOCH_DAY" => days,
        "HOUR_OF_DAY" => ms / 3_600_000,
        "MINUTE_OF_DAY" => ms / 60_000,
        "MINUTE_OF_HOUR" => ms / 60_000 % 60,
        "SECOND_OF_DAY" => ms / 1000,
        "SECOND_OF_MINUTE" => ms / 1000 % 60,
        "MILLI_OF_DAY" => ms,
        "MILLI_OF_SECOND" => ms % 1000,
        _ => return None,
    };
    Some(v.into())
}

/// The canonical unit of a time span literal's unit, like Elasticsearch's
/// `ExpressionBuilder.visitQualifiedIntegerLiteral` (ignoring case).
fn time_unit(w: &str) -> Option<&'static str> {
    Some(match w.to_ascii_lowercase().as_str() {
        "millisecond" | "milliseconds" | "ms" => "millisecond",
        "second" | "seconds" | "sec" | "s" => "second",
        "minute" | "minutes" | "min" => "minute",
        "hour" | "hours" | "h" => "hour",
        "day" | "days" | "d" => "day",
        "week" | "weeks" | "w" => "week",
        "month" | "months" | "mo" => "month",
        "quarter" | "quarters" | "q" => "quarter",
        "year" | "years" | "yr" | "y" => "year",
        _ => return None,
    })
}

/// The type of a time span in canonical `unit` (see `time_unit`).
fn span_type(unit: &str) -> &'static str {
    match unit {
        "millisecond" | "second" | "minute" | "hour" => "time_duration",
        _ => "date_period",
    }
}

/// `DATE_TRUNC(n unit, date)` in UTC, like Elasticsearch's `DateTrunc.createRounding`: a week
/// is 7 days and a quarter 3 months, as in ES's literals. One day, week, month, quarter (3
/// months) or year rounds to its calendar start; several months or years to intervals counted
/// from year 1 (`DateUtils.roundIntervalMonthOfYear`); anything else to a fixed interval since
/// the epoch.
// ponytail: a zero or negative span is null, where ES fails the query
fn date_trunc(n: i64, unit: &str, v: &Value) -> Option<Value> {
    const DAY: i64 = 86_400_000;
    let t = es_index::parse_date_millis(v)?;
    let (n, unit) = match unit {
        "week" => (n.checked_mul(7)?, "day"),
        "quarter" => (n.checked_mul(3)?, "month"),
        _ => (n, unit),
    };
    let calendar = match (unit, n) {
        ("day", 7) => Some("week"),
        ("month", 3) => Some("quarter"),
        ("month" | "year", 1) => Some(unit),
        _ => None,
    };
    if let Some(unit) = calendar {
        let start = crate::aggs::histogram::Unit::parse(unit)?.round(t, false);
        return Some(es_index::iso_date(start).into());
    }
    let months = match unit {
        "month" => Some(n),
        "year" => n.checked_mul(12),
        _ => None,
    };
    if let Some(months) = months.filter(|m| *m > 0) {
        let (y, m, _) = es_index::civil_from_days(t.div_euclid(DAY));
        let first = ((y - 1) * 12 + m - 1).div_euclid(months) * months;
        let days = es_index::days_from_civil(first.div_euclid(12) + 1, first.rem_euclid(12) + 1, 1);
        return Some(es_index::iso_date(days * DAY).into());
    }
    let ms = match unit {
        "millisecond" => 1,
        "second" => 1000,
        "minute" => 60_000,
        "hour" => 3_600_000,
        "day" => DAY,
        _ => return None,
    };
    let interval = n.checked_mul(ms).filter(|i| *i > 0)?;
    Some(es_index::iso_date(t.div_euclid(interval) * interval).into())
}

/// What is wrong with the arguments of `BUCKET`, of types `ts`, like Elasticsearch's
/// `Bucket.resolveType` and `postOptimizationVerification`.
fn bucket_problem(ts: &[String], e: &Expr, src: &str) -> Option<String> {
    let Expr::Call(_, args, ..) = e else {
        return None;
    };
    if ts[0] == "null" || ts[1] == "null" {
        return None;
    }
    let text = |e: &Expr| &src[span(e).1.clone()];
    let date = ts[0] == "date";
    let whole = ts[1] == "integer" || ts[1] == "long";
    let span = ts[1] == "date_period" || ts[1] == "time_duration";
    let range = match date {
        true => (
            "datetime, date_nanos or string",
            ["date", "keyword", "text"].as_slice(),
        ),
        false => ("numeric", &NUMERIC[..]),
    };
    let (accepted, ok) = match date {
        true => ("integral, date_period or time_duration", whole || span),
        false => ("numeric", NUMERIC.contains(&ts[1].as_str())),
    };
    let mut problem = (!ok).then_some((1, accepted));
    let needed = match date {
        true if !whole && args.len() == 2 => 2,
        true => 4,
        false if whole && args.len() != 2 => 4,
        false => 2,
    };
    if problem.is_none() && args.len() != needed {
        // ES names the `DataType` constants here.
        let upper = |t: &str| match t {
            "date" => "DATETIME".to_owned(),
            t => t.to_ascii_uppercase(),
        };
        return Some(format!(
            "function expects exactly {} arguments when the first one is of type [{}] and the \
             second of type [{}]",
            ["two", "four"][usize::from(needed == 4)],
            upper(&ts[0]),
            upper(&ts[1])
        ));
    }
    problem = problem.or_else(|| {
        (2..ts.len())
            .find(|&i| !range.1.contains(&ts[i].as_str()))
            .map(|i| (i, range.0))
    });
    if let Some((i, accepted)) = problem.filter(|(i, _)| ts[*i] != "null") {
        return Some(format!(
            "{}argument of [{}] must be [{accepted}], found value [{}] type [{}]",
            ordinal(i),
            text(e),
            text(&args[i]),
            ts[i]
        ));
    }
    let i = (1..args.len()).find(|&i| !names(&args[i]).is_empty())?;
    Some(format!(
        "{}argument of [{}] must be a constant, received [{}]",
        ordinal(i),
        text(e),
        text(&args[i])
    ))
}

/// `BUCKET(x, ...)` of one row: a date rounded down like `DATE_TRUNC`, or a number rounded down
/// to a multiple of the span as a double. With a target bucket count and a range, the span is
/// picked like Elasticsearch's `Bucket`.
fn bucket(args: &[Expr], date: bool, row: &Row) -> Option<Value> {
    let x = eval(&args[0], row)?;
    let fold = |e: &Expr| eval(e, &Row::new());
    if date {
        let (n, unit) = match args {
            [_, Expr::Interval(n, unit, _)] => (*n, *unit),
            [_, b, from, to] => {
                let millis = |e| es_index::parse_date_millis(&fold(e)?);
                date_rounding(fold(b)?.as_i64()?, millis(from)?, millis(to)?)
            }
            _ => return None,
        };
        return date_trunc(n, unit, &x);
    }
    let span = match args {
        [_, b] => fold(b)?.as_f64()?,
        [_, b, from, to] => {
            let b = fold(b)?.as_i64()? as f64;
            let precise = (fold(to)?.as_f64()? - fold(from)?.as_f64()?) / b;
            let next_power_of_ten = 10f64.powf(precise.log10().ceil());
            let half = next_power_of_ten / 2.0;
            if precise < half {
                half
            } else {
                next_power_of_ten
            }
        }
        _ => return None,
    };
    let r = (x.as_f64()? / span).floor() * span;
    r.is_finite().then(|| r.into())
}

/// The finest rounding, as `DATE_TRUNC`'s span, that cuts `from..to` into at most `buckets`
/// buckets, like Elasticsearch's `Bucket.DateRoundingPicker` in UTC.
// ponytail: a linear scan where ES binary-searches; the same pick, as coarser units fit sooner
fn date_rounding(buckets: i64, from: i64, to: i64) -> (i64, &'static str) {
    const PRIMARY: [(i64, &str); 16] = [
        (1, "day"),
        (12, "hour"),
        (3, "hour"),
        (1, "hour"),
        (30, "minute"),
        (10, "minute"),
        (5, "minute"),
        (1, "minute"),
        (30, "second"),
        (10, "second"),
        (5, "second"),
        (1, "second"),
        (100, "millisecond"),
        (50, "millisecond"),
        (10, "millisecond"),
        (1, "millisecond"),
    ];
    const SECONDARY: [(i64, &str); 3] = [(1, "week"), (1, "month"), (1, "year")];
    let fits = |&(n, unit): &(i64, &str)| {
        if buckets <= 0 {
            return false;
        }
        let width = match unit {
            "hour" => 3_600_000,
            "minute" => 60_000,
            "second" => 1000,
            "millisecond" => 1,
            // Calendar units vary in length: count them one at a time.
            _ => {
                let Some(unit) = crate::aggs::histogram::Unit::parse(unit) else {
                    return false;
                };
                let mut bucket = unit.round(from, false);
                return (0..buckets).any(|_| {
                    bucket = unit.round(bucket, true);
                    bucket >= to
                });
            }
        } * n;
        let start = from.div_euclid(width) * width;
        let range = (i128::from(to) - i128::from(start)).max(0) as u128;
        range.div_ceil(width as u128) <= buckets as u128
    };
    let primary = PRIMARY.iter().rev().find(|u| fits(u));
    *primary
        .or_else(|| SECONDARY.iter().find(|u| fits(u)))
        .unwrap_or(&SECONDARY[2])
}

/// Java's `Character.isWhitespace`.
fn java_whitespace(c: char) -> bool {
    matches!(c, '\u{1c}'..='\u{1f}')
        || c.is_whitespace() && !matches!(c, '\u{85}' | '\u{a0}' | '\u{2007}' | '\u{202f}')
}

/// String function `f` of its arguments, like Elasticsearch's evaluators: `None` for a
/// multi-valued argument (where ES warns).
fn string_fn(f: &str, args: &[Value]) -> Option<Value> {
    let s = args[0].as_str()?;
    match f {
        "TRIM" => return Some(s.trim_matches(java_whitespace).into()),
        // ponytail: no 1MB cap on the result, which fails the query in ES
        "CONCAT" => {
            return args
                .iter()
                .map(Value::as_str)
                .collect::<Option<String>>()
                .map(Value::from);
        }
        "LENGTH" => return Some(s.chars().count().into()),
        "TO_LOWER" => return Some(s.to_lowercase().into()),
        "TO_UPPER" => return Some(s.to_uppercase().into()),
        "STARTS_WITH" => return Some(s.starts_with(args[1].as_str()?).into()),
        "ENDS_WITH" => return Some(s.ends_with(args[1].as_str()?).into()),
        _ => {}
    }
    // SUBSTRING counts code points from 1; 0 counts as 1 and a negative start from the end.
    let n = s.chars().count() as i64;
    let start = args[1].as_i64()?;
    let len = match args.get(2) {
        Some(l) => l.as_i64()?,
        None => n,
    };
    if len < 0 {
        let reason = format!("Length parameter cannot be negative, found [{len}]");
        return fail(400, "illegal_argument_exception", reason);
    }
    let from = match start {
        0 => 0,
        s if s > 0 => s - 1,
        s => n + s,
    }
    .clamp(0, n);
    let to = n.min(from + len);
    let out: String = s
        .chars()
        .skip(from as usize)
        .take((to - from) as usize)
        .collect();
    Some(out.into())
}

/// Java's `Double.parseDouble`, refusing what Elasticsearch's `StringUtils.parseDouble` does.
fn parse_double(s: &str) -> Option<f64> {
    let s = s.trim_matches(|c| c <= ' ');
    let s = s.strip_suffix(['d', 'D', 'f', 'F']).unwrap_or(s);
    let (negative, unsigned) = match s.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let x = match unsigned.strip_prefix("0x").or(unsigned.strip_prefix("0X")) {
        Some(hex) => parse_hex_double(hex).map(|x| if negative { -x } else { x })?,
        None => s.parse().ok()?,
    };
    x.is_finite().then_some(x)
}

/// A Java hexadecimal float after its `0x`: hex digits with an optional point, then a binary
/// exponent (`1.8p1` is 3), rounded to the nearest double.
// ponytail: a subnormal result may round twice (scaling after rounding to 53 bits)
fn parse_hex_double(s: &str) -> Option<f64> {
    let (digits, exp) = s.split_once(['p', 'P'])?;
    let (int, frac) = digits.split_once('.').unwrap_or((digits, ""));
    if int.is_empty() && frac.is_empty() || exp.is_empty() {
        return None;
    }
    let exp = exp.parse::<i64>().ok()?.clamp(-5000, 5000) as i32;
    // The first 61+ bits, with any nonzero digit past them folded into the lowest bit so that
    // converting to f64 rounds like the exact value.
    let (mut mantissa, mut exp) = (0u64, exp - 4 * frac.len() as i32);
    for d in int.chars().chain(frac.chars()) {
        let d = u64::from(d.to_digit(16)?);
        if mantissa >> 60 == 0 {
            mantissa = mantissa << 4 | d;
        } else {
            mantissa |= (d != 0) as u64;
            exp += 4;
        }
    }
    let half = exp / 2;
    Some(mantissa as f64 * 2f64.powi(half) * 2f64.powi(exp - half))
}

/// Java's `Math.round`: halves round up.
fn java_round(x: f64) -> f64 {
    let r = x.floor();
    if x - r >= 0.5 { r + 1.0 } else { r }
}

/// Function `f` (upper-case, one of `CONVERSIONS`) of a single value of type `t`, like
/// Elasticsearch's convert evaluators: `None` where they fail (and warn).
fn convert(f: &str, t: &str, v: &Value) -> Option<Value> {
    let num = || match t {
        "double" => v.as_f64().map(Ok),
        _ => v
            .as_i64()
            .map(Err)
            .or_else(|| v.as_u64().map(|u| Ok(u as f64))),
    };
    match (f, v) {
        ("TO_DATETIME", _) => {
            es_index::parse_date_millis(v).map(|ms| es_index::iso_date(ms).into())
        }
        ("TO_STRING", Value::String(_)) | ("TO_BOOLEAN", Value::Bool(_)) => Some(v.clone()),
        ("TO_STRING", Value::Bool(b)) => Some(b.to_string().into()),
        ("TO_STRING", _) => match num()? {
            Ok(x) if t == "double" => Some(crate::aggs::java_double(x).into()),
            _ => Some(v.to_string().into()),
        },
        ("TO_BOOLEAN", Value::String(s)) => Some(s.eq_ignore_ascii_case("true").into()),
        ("TO_BOOLEAN", _) => Some(num()?.map_or_else(|i| i != 0, |x| x != 0.0).into()),
        ("TO_DOUBLE", Value::String(s)) => parse_double(s).map(Value::from),
        (_, Value::Bool(b)) if f != "TO_DOUBLE" => Some((*b as i64).into()),
        ("TO_DOUBLE", Value::Bool(b)) => Some((*b as u8 as f64).into()),
        ("TO_DOUBLE", _) => Some(num()?.unwrap_or_else(|i| i as f64).into()),
        _ => {
            // TO_INTEGER or TO_LONG: an integer, else a double rounded if in range.
            let n = match v {
                Value::String(s) => match s.parse::<i64>() {
                    Ok(i) => Err(i),
                    Err(_) => Ok(parse_double(s)?),
                },
                _ => num()?,
            };
            let i = match n {
                Err(i) => i,
                // `Math.round` saturates; 2^63 itself is out of range for a long.
                Ok(x) if (-(2f64.powi(63))..2f64.powi(63)).contains(&x) => java_round(x) as i64,
                Ok(_) => return None,
            };
            match f {
                "TO_INTEGER" => i32::try_from(i).ok().map(Value::from),
                _ => Some(i.into()),
            }
        }
    }
}

/// Calls `f` on `e` and on each expression in it, outermost first.
fn walk<'e>(e: &'e Expr, f: &mut impl FnMut(&'e Expr)) {
    f(e);
    match e {
        Expr::Call(_, args, ..) => args.iter().for_each(|a| walk(a, f)),
        Expr::Field(..) | Expr::Lit(..) | Expr::Interval(..) => {}
        Expr::Not(x, _) | Expr::IsNull(x, ..) | Expr::Neg(x, ..) | Expr::Like(x, ..) => walk(x, f),
        Expr::Cmp(_, a, b, _)
        | Expr::And(a, b, _)
        | Expr::Or(a, b, _)
        | Expr::Arith(_, a, b, ..) => {
            walk(a, f);
            walk(b, f);
        }
    }
}

fn fields(e: &Expr, out: &mut Vec<Name>) {
    walk(e, &mut |e| {
        if let Expr::Field(f, (at, _)) = e {
            out.push((f.clone(), *at));
        }
    });
}

/// A problem for each grouping function (`BUCKET`) in `e`, which only STATS may use.
fn grouping_outside_stats(e: &Expr, src: &str, bad: &mut Vec<(Pos, String)>) {
    walk(e, &mut |e| {
        if let Expr::Call(f, _, _, (at, r)) = e
            && f.eq_ignore_ascii_case("BUCKET")
        {
            let msg = format!(
                "cannot use grouping function [{}] outside of a STATS or LIMIT BY command",
                &src[r.clone()]
            );
            bad.push((*at, msg));
        }
    });
}

fn names(e: &Expr) -> Vec<String> {
    let mut out = Vec::new();
    fields(e, &mut out);
    out.into_iter().map(|(f, _)| f).collect()
}

fn span(e: &Expr) -> &Span {
    match e {
        Expr::Field(_, s) | Expr::Lit(_, s) | Expr::Not(_, s) | Expr::IsNull(.., s) => s,
        Expr::Interval(.., s) => s,
        Expr::Cmp(.., s) | Expr::And(.., s) | Expr::Or(.., s) | Expr::Arith(.., s) => s,
        Expr::Call(.., s) | Expr::Neg(.., s) | Expr::Like(.., s) => s,
    }
}

/// The ES|QL type of a numeric literal, negated when `negative`.
fn number_type(n: &Value, negative: bool) -> &'static str {
    let signed = n.as_i64().map(|v| if negative { -v } else { v });
    match () {
        _ if signed.is_some_and(|v| i32::try_from(v).is_ok()) => "integer",
        _ if signed.is_some() => "long",
        _ if n.is_u64() => "unsigned_long",
        _ => "double",
    }
}

const NUMERIC: [&str; 4] = ["integer", "long", "unsigned_long", "double"];

/// The ES|QL type of a supported `METADATA` field.
fn metadata_type(field: &str) -> Option<&'static str> {
    match field {
        "_id" | "_index" => Some("keyword"),
        "_version" => Some("long"),
        _ => None,
    }
}

/// The ES|QL type of a literal; a list has its first item's type.
fn literal_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => number_type(v, false),
        Value::Array(a) => a.first().map_or("null", literal_type),
        _ => "keyword",
    }
}

/// The ES|QL type of an expression whose fields all exist, like Elasticsearch's type
/// resolution. `None` once a problem is recorded in `bad`: like Elasticsearch, an expression
/// is only checked when its operands resolved.
fn resolve(
    e: &Expr,
    src: &str,
    types: &BTreeMap<String, String>,
    bad: &mut Vec<(Pos, String)>,
) -> Option<String> {
    let text = |e: &Expr| &src[span(e).1.clone()];
    // "found value [x] type [t]" when an operand is not boolean (null is allowed).
    let not_boolean = |e: &Expr, t: &str| {
        (t != "boolean" && t != "null").then(|| format!("found value [{}] type [{t}]", text(e)))
    };
    let problem = match e {
        Expr::Field(f, _) => return Some(types[f].clone()),
        Expr::Lit(v, _) => return Some(literal_type(v).into()),
        Expr::Interval(_, unit, _) => return Some(span_type(unit).into()),
        Expr::Arith(op, a, b, t, _) => {
            let (ta, tb) = (resolve(a, src, types, bad), resolve(b, src, types, bad));
            let (ta, tb) = (ta?, tb?);
            let numeric = |t: &str| NUMERIC.contains(&t);
            let date = |t: &str| t == "datetime" || t == "date_nanos";
            // ponytail: no date math (it needs time span literals) or dense_vector operands
            let accepted = match *op {
                "+" | "-" => "date_nanos, datetime, numeric or dense_vector",
                "*" | "/" => "numeric or dense_vector",
                _ => "numeric",
            };
            let input = |x: &Expr, t: &str| {
                let ok = numeric(t) || t == "null" || accepted.contains(',') && date(t);
                (!ok).then(|| format!("[{accepted}], found value [{}] type [{t}]", text(x)))
            };
            let incompatible = match (ta.as_str(), tb.as_str()) {
                ("null", _) | (_, "null") => false,
                (x, y) if date(x) || date(y) => true,
                (x, y) => (x == "unsigned_long") != (y == "unsigned_long"),
            };
            let problem = match (input(a, &ta), input(b, &tb)) {
                (Some(found), _) => format!("first argument of [{}] must be {found}", text(e)),
                (_, Some(found)) => format!("second argument of [{}] must be {found}", text(e)),
                _ if incompatible => {
                    format!("[{op}] has arguments with incompatible types [{ta}] and [{tb}]")
                }
                _ => {
                    // Elasticsearch's `commonType` for the types that get this far.
                    let common = match (ta.as_str(), tb.as_str()) {
                        ("null", y) => y,
                        (x, y) if x == y || y == "null" => x,
                        ("double", _) | (_, "double") => "double",
                        ("long", _) | (_, "long") => "long",
                        (x, _) => x,
                    };
                    if let Some(&n) = NUMERIC.iter().find(|n| **n == common) {
                        let _ = t.set(n);
                    }
                    return Some(common.to_owned());
                }
            };
            bad.push((span(e).0, problem));
            return None;
        }
        Expr::Call(f, args, t, (at, _)) => {
            let upper = f.to_ascii_uppercase();
            // The parser checked the argument count.
            if let Some((name, _)) = SCALAR_FNS.iter().find(|(n, _)| *n == upper) {
                let ts: Vec<_> = args.iter().map(|a| resolve(a, src, types, bad)).collect();
                let ts: Vec<String> = ts.into_iter().collect::<Option<_>>()?;
                // The first argument of the wrong type, and what it must be.
                let mismatch = match *name {
                    "CASE" | "COALESCE" => match conditional_type(name, &ts) {
                        Ok(out) => {
                            let _ = t.set((*name, if out == "double" { "double" } else { "any" }));
                            return Some(out);
                        }
                        Err(found) => Some(found),
                    },
                    _ => ts.iter().enumerate().find_map(|(i, ta)| {
                        // `DATE_FORMAT(date)` leaves out the format.
                        let at = i + usize::from(*name == "DATE_FORMAT" && ts.len() == 1);
                        let (accepted, ok) = accepts(name, at, ta);
                        (!ok).then(|| (i, accepted.to_owned()))
                    }),
                };
                let mismatch = mismatch.map(|(i, accepted)| {
                    let nth = match *name {
                        "TRIM" | "ABS" | "CEIL" | "FLOOR" | "CONCAT" | "LENGTH" | "TO_LOWER"
                        | "TO_UPPER" => "",
                        _ => ordinal(i),
                    };
                    format!(
                        "{nth}argument of [{}] must be [{accepted}], found value [{}] type [{}]",
                        text(e),
                        text(&args[i]),
                        ts[i]
                    )
                });
                let problem = match *name {
                    "BUCKET" => mismatch.or_else(|| bucket_problem(&ts, e, src)),
                    _ => mismatch,
                };
                if let Some(problem) = problem {
                    bad.push((*at, problem));
                    return None;
                }
                // The result type: a string, an integer, a double, or the first argument's.
                let out = match *name {
                    "SUBSTRING" | "CONCAT" | "TO_LOWER" | "TO_UPPER" | "DATE_FORMAT" => "keyword",
                    "DATE_EXTRACT" => "long",
                    "LENGTH" => "integer",
                    "STARTS_WITH" | "ENDS_WITH" => "boolean",
                    "TRIM" if ts[0] != "null" => "keyword",
                    "MV_COUNT" => "integer",
                    "NOW" | "DATE_TRUNC" => "date",
                    "BUCKET" if ts[0] == "date" => "date",
                    "BUCKET" if ts[0] != "null" => "double",
                    "MV_MIN" | "MV_MAX" => "null",
                    "POW" => "double",
                    _ => NUMERIC.iter().find(|n| **n == ts[0]).unwrap_or(&"null"),
                };
                let _ = t.set((*name, out));
                return Some(match *name {
                    "MV_MIN" | "MV_MAX" => ts[0].clone(),
                    _ => out.into(),
                });
            }
            let Some(&(name, out)) = CONVERSIONS.iter().find(|(n, _)| *n == upper) else {
                bad.push((*at, format!("Unknown function [{f}]")));
                return None;
            };
            // ponytail: TO_INTEGER and TO_LONG with a base fail here; ES parses in that base
            let [arg] = &args[..] else {
                bad.push((*at, format!("[{f}] with a base is not supported")));
                return None;
            };
            let ta = resolve(arg, src, types, bad)?;
            // ponytail: dates, ips, versions and the like are not converted yet, nor booleans,
            // doubles and unsigned longs to dates
            let Some(&ta) = ["null", "boolean", "keyword", "text"]
                .iter()
                .chain(&NUMERIC)
                .find(|n| **n == ta)
                .filter(|t| {
                    name != "TO_DATETIME" || !matches!(**t, "boolean" | "double" | "unsigned_long")
                })
            else {
                bad.push((*at, format!("[{f}] of [{ta}] is not supported by es-mini")));
                return None;
            };
            let _ = t.set((name, ta));
            return Some(out.to_owned());
        }
        Expr::Neg(x, t, _) => {
            let tx = resolve(x, src, types, bad)?;
            match NUMERIC.iter().find(|n| **n == tx && **n != "unsigned_long") {
                Some(n) => {
                    let _ = t.set(n);
                    return Some(tx);
                }
                None if tx == "null" => return Some(tx),
                None => Some(format!(
                    "argument of [{}] must be [numeric, date_period or time_duration], found \
                     value [{}] type [{tx}]",
                    text(e),
                    text(x)
                )),
            }
        }
        Expr::IsNull(x, ..) => {
            resolve(x, src, types, bad)?;
            None
        }
        Expr::Like(x, ..) => {
            let t = resolve(x, src, types, bad)?;
            (!matches!(t.as_str(), "keyword" | "text" | "null")).then(|| {
                format!(
                    "argument of [{}] must be [string], found value [{}] type [{t}]",
                    text(e),
                    text(x)
                )
            })
        }
        Expr::Not(x, _) => {
            let t = resolve(x, src, types, bad)?;
            not_boolean(x, &t)
                .map(|found| format!("argument of [{}] must be [boolean], {found}", text(e)))
        }
        Expr::And(a, b, _) | Expr::Or(a, b, _) => {
            let (ta, tb) = (resolve(a, src, types, bad), resolve(b, src, types, bad));
            let (ta, tb) = (ta?, tb?);
            let (nth, found) = match not_boolean(a, &ta) {
                Some(found) => ("first", Some(found)),
                None => ("second", not_boolean(b, &tb)),
            };
            found.map(|found| format!("{nth} argument of [{}] must be [boolean], {found}", text(e)))
        }
        Expr::Cmp(_, a, b, _) => {
            let (ta, tb) = (resolve(a, src, types, bad), resolve(b, src, types, bad));
            let (ta, tb) = (ta?, tb?);
            // ponytail: only numeric, string and boolean operands are checked; dates, ips and
            // the like take string literals through implicit casts that this does not model.
            let class = |t: &str| match t {
                "integer" | "long" | "unsigned_long" | "double" => Some("numeric"),
                "keyword" | "text" => Some("string"),
                "boolean" => Some("boolean"),
                _ => None,
            };
            match (class(&ta), class(&tb)) {
                (Some(ca), Some(cb)) if ca != cb => {
                    let first = if ca == "numeric" { ca } else { &ta };
                    Some(format!(
                        "first argument of [{}] is [{first}] so second argument must also be \
                         [{first}] but was [{tb}]",
                        text(e)
                    ))
                }
                _ => None,
            }
        }
    };
    match problem {
        Some(p) => {
            bad.push((span(e).0, p));
            None
        }
        None => Some("boolean".into()),
    }
}

/// A STATS aggregate's function, upper case, its argument (`None` for `COUNT(*)`) and its
/// parameter (`PERCENTILE`'s percentile).
fn aggregate(e: &Expr) -> Option<(String, Option<&Expr>, Option<&Expr>)> {
    let Expr::Call(f, args, ..) = e else {
        return None;
    };
    let f = f.to_ascii_uppercase();
    match (f.as_str(), &args[..]) {
        ("COUNT", [Expr::Field(s, _)]) if s == "*" => Some((f, None, None)),
        (
            "COUNT" | "COUNT_DISTINCT" | "MIN" | "MAX" | "SUM" | "AVG" | "MEDIAN" | "VALUES",
            [arg],
        ) => Some((f, Some(arg), None)),
        ("PERCENTILE", [arg, p]) => Some((f, Some(arg), Some(p))),
        _ => None,
    }
}

fn unknown(c: &str) -> String {
    format!("Unknown column [{c}]")
}

/// ES's verification failure listing each distinct problem with where it occurs.
fn problems(found: Vec<(Pos, String)>) -> EsqlError {
    let mut seen = BTreeSet::new();
    let lines: Vec<String> = found
        .into_iter()
        .filter(|(_, m)| seen.insert(m.clone()))
        .map(|((line, col), m)| format!("\nline {line}:{col}: {m}"))
        .collect();
    let s = if lines.len() == 1 { "" } else { "s" };
    EsqlError::Verification(format!(
        "Found {} problem{s}{}",
        lines.len(),
        lines.concat()
    ))
}

pub fn run(
    indices: &aliases::Map<impl aliases::IndexMeta + std::borrow::Borrow<Index>>,
    query: &str,
) -> Result<Response, EsqlError> {
    FAILURE.take();
    let r = run_at(indices, query, crate::dates::now_millis());
    FAILURE.take().map_or(r, Err)
}

/// `run` with `now` (epoch millis) as the query's start time.
fn run_at(
    indices: &aliases::Map<impl aliases::IndexMeta + std::borrow::Borrow<Index>>,
    query: &str,
    now: i64,
) -> Result<Response, EsqlError> {
    let (toks, eof) = lex(query)?;
    let (source, cmds) = Parser {
        src: query,
        toks,
        eof,
        pos: 0,
        operand: 0,
        parens: 0,
        follow: "<EOF>",
        now,
    }
    .query()?;
    let (source, cmds) = match source {
        // A row is an EVAL over one empty row.
        Source::Row(fields) => (None, [vec![Command::Eval(fields)], cmds].concat()),
        Source::From(sources, metadata, from) => (Some((sources, metadata, from)), cmds),
    };
    let Table {
        mut types,
        mut conflicts,
        groups,
        mut cols,
        empty,
        counted,
    } = match source {
        Some((sources, metadata, from)) => {
            let names: Vec<String> = metadata.iter().map(|(f, _)| f.clone()).collect();
            let table = table_from(indices, &sources, &names, &read(&cmds))?;
            // Like ES, unknown metadata fails verification once the indices resolve.
            let bad: Vec<&Name> = (metadata.iter())
                .filter(|(f, _)| metadata_type(f).is_none())
                .collect();
            if !bad.is_empty() {
                let list: Vec<String> = bad.iter().map(|(f, _)| format!("?{f}")).collect();
                let fields = (
                    from,
                    format!("unresolved metadata fields: [{}]", list.join(", ")),
                );
                let patterns =
                    (bad.iter()).map(|(f, at)| (*at, format!("Unresolved metadata pattern [{f}]")));
                return Err(problems(std::iter::once(fields).chain(patterns).collect()));
            }
            table
        }
        None => Table {
            types: BTreeMap::new(),
            conflicts: BTreeMap::new(),
            groups: vec![vec![Row::new()]],
            cols: Vec::new(),
            empty: false,
            counted: false,
        },
    };
    // A referenced column must exist and have one type.
    let check = |cols: &[String], conflicts: &BTreeMap<String, _>, f: &str| match conflicts.get(f) {
        _ if !cols.iter().any(|c| c == f) => Some(unknown(f)),
        Some(m) => Some(conflict_error(f, m)),
        None => None,
    };
    let mut rows: Vec<Cow<Row>> = groups.iter().flatten().map(Cow::Borrowed).collect();
    let mut explicit_limit = false;

    for cmd in &cmds {
        // Like ES, report every problem of the first command that has any.
        let bad: Vec<(Pos, String)> = match cmd {
            Command::Where(e) => {
                let mut refs = Vec::new();
                fields(e, &mut refs);
                let mut bad: Vec<_> = refs
                    .into_iter()
                    .filter_map(|(f, at)| Some((at, check(&cols, &conflicts, &f)?)))
                    .collect();
                if bad.is_empty()
                    && let Some(t) = resolve(e, query, &types, &mut bad)
                    && t != "boolean"
                    && t != "null"
                {
                    let found = t.to_ascii_uppercase();
                    let problem =
                        format!("Condition expression needs to be boolean, found [{found}]");
                    bad.push((span(e).0, problem));
                }
                if bad.is_empty() {
                    grouping_outside_stats(e, query, &mut bad);
                }
                bad
            }
            Command::Eval(named) => {
                // Each expression sees the columns the ones before it added.
                let mut bad = Vec::new();
                for (name, e) in named {
                    let mut refs = Vec::new();
                    fields(e, &mut refs);
                    bad.extend(
                        refs.into_iter()
                            .filter_map(|(f, at)| Some((at, check(&cols, &conflicts, &f)?))),
                    );
                    if !bad.is_empty() {
                        break;
                    }
                    let Some(t) = resolve(e, query, &types, &mut bad) else {
                        break;
                    };
                    cols.retain(|c| c != name);
                    cols.push(name.clone());
                    types.insert(name.clone(), t);
                    conflicts.remove(name);
                }
                if bad.is_empty() {
                    (named.iter()).for_each(|(_, e)| grouping_outside_stats(e, query, &mut bad));
                }
                bad
            }
            Command::Keep(patterns) | Command::Drop(patterns) => patterns
                .iter()
                .filter(|(p, _)| !cols.iter().any(|c| aliases::glob(p, c)))
                .map(|(p, at)| match p.contains('*') {
                    true => (*at, format!("No matches found for pattern [{p}]")),
                    false => (*at, unknown(p)),
                })
                .collect(),
            Command::Sort(keys) => keys
                .iter()
                .filter_map(|k| Some((k.at, check(&cols, &conflicts, &k.field)?)))
                .collect(),
            Command::Rename(pairs) => {
                // Like Elasticsearch's `projectionsForRename`: the new name replaces any column
                // of that name, and the old name is gone for the renamings after it.
                let mut bad = Vec::new();
                let mut renamed: BTreeMap<&str, &str> = BTreeMap::new();
                for ((old, at), new, text) in pairs.iter().filter(|(o, n, _)| o.0 != *n) {
                    cols.retain(|c| c != new);
                    let Some(i) = cols.iter().position(|c| c == old) else {
                        bad.push((
                            *at,
                            match renamed.get(old.as_str()) {
                                Some(to) => format!(
                                    "Column [{old}] renamed to [{to}] and is no longer \
                                     available [{text}]"
                                ),
                                None => unknown(old),
                            },
                        ));
                        continue;
                    };
                    cols[i] = new.clone();
                    // Old names keep their types: an earlier SORT may still use them.
                    types.insert(new.clone(), types[old].clone());
                    match conflicts.get(old).cloned() {
                        Some(c) => conflicts.insert(new.clone(), c),
                        None => conflicts.remove(new),
                    };
                    renamed.insert(old, new);
                }
                bad
            }
            Command::Limit(_) => Vec::new(),
            Command::Stats(aggs, by) => {
                // ponytail: `aggregate` lists the only supported aggregates
                let mut bad = Vec::new();
                let mut out = Vec::new();
                for (name, e) in aggs {
                    let Some((f, arg, param)) = aggregate(e) else {
                        let msg = format!(
                            "[{}] is not supported by es-mini in STATS",
                            &query[span(e).1.clone()]
                        );
                        bad.push((span(e).0, msg));
                        continue;
                    };
                    let Some(arg) = arg else {
                        out.push((name.clone(), "long".to_owned()));
                        continue;
                    };
                    let mut refs = Vec::new();
                    fields(arg, &mut refs);
                    param.into_iter().for_each(|p| fields(p, &mut refs));
                    bad.extend(
                        refs.into_iter()
                            .filter_map(|(f, at)| Some((at, check(&cols, &conflicts, &f)?))),
                    );
                    if !bad.is_empty() {
                        continue;
                    }
                    let Some(t) = resolve(arg, query, &types, &mut bad) else {
                        continue;
                    };
                    let numeric =
                        t == "null" || (NUMERIC.contains(&t.as_str()) && t != "unsigned_long");
                    let source = &query[span(e).1.clone()];
                    let expected = match f.as_str() {
                        "SUM" | "AVG" if !numeric => {
                            "argument of [{e}] must be [aggregate_metric_double, \
                                          exponential_histogram, tdigest or numeric except \
                                          unsigned_long or counter types]"
                        }
                        "MEDIAN" if !numeric => {
                            "argument of [{e}] must be [exponential_histogram, tdigest or \
                                     numeric except unsigned_long or counter types]"
                        }
                        "PERCENTILE" if !numeric => {
                            "first argument of [{e}] must be [exponential_histogram, \
                                         tdigest or numeric except unsigned_long]"
                        }
                        "COUNT_DISTINCT" if t == "unsigned_long" => {
                            "argument of [{e}] must be [any exact type except unsigned_long, \
                             _source, or counter types]"
                        }
                        _ => "",
                    };
                    if !expected.is_empty() {
                        let found = &query[span(arg).1.clone()];
                        let msg = expected.replace("{e}", source);
                        bad.push((
                            span(e).0,
                            format!("{msg}, found value [{found}] type [{t}]"),
                        ));
                        continue;
                    }
                    if let Some(p) = param {
                        let text = &query[span(p).1.clone()];
                        let Some(pt) = resolve(p, query, &types, &mut bad) else {
                            continue;
                        };
                        let msg = if !NUMERIC.contains(&pt.as_str()) || pt == "unsigned_long" {
                            format!(
                                "second argument of [{source}] must be [numeric except \
                                 unsigned_long], found value [{text}] type [{pt}]"
                            )
                        } else if !names(p).is_empty() {
                            format!(
                                "second argument of [{source}] must be a constant, received \
                                 [{text}]"
                            )
                        } else {
                            String::new()
                        };
                        if !msg.is_empty() {
                            bad.push((span(e).0, msg));
                            continue;
                        }
                    }
                    let t = match f.as_str() {
                        "COUNT" | "COUNT_DISTINCT" => "long".into(),
                        "AVG" | "MEDIAN" | "PERCENTILE" => "double".into(),
                        "SUM" if t == "integer" => "long".into(),
                        "VALUES" if t == "text" => "keyword".into(),
                        _ => t,
                    };
                    out.push((name.clone(), t));
                }
                for (name, e) in by {
                    let mut refs = Vec::new();
                    fields(e, &mut refs);
                    bad.extend(
                        refs.into_iter()
                            .filter_map(|(f, at)| Some((at, check(&cols, &conflicts, &f)?))),
                    );
                    if bad.is_empty()
                        && let Some(t) = resolve(e, query, &types, &mut bad)
                    {
                        out.push((name.clone(), t));
                    }
                }
                if bad.is_empty() {
                    cols.clear();
                    for (name, t) in out {
                        cols.retain(|c| *c != name);
                        cols.push(name.clone());
                        types.insert(name.clone(), t);
                        conflicts.remove(&name);
                    }
                }
                bad
            }
        };
        if !bad.is_empty() {
            return Err(problems(bad));
        }
        match cmd {
            Command::Where(e) => {
                rows.retain(|r| eval(e, r) == Some(Value::Bool(true)));
            }
            Command::Eval(named) => {
                for r in &mut rows {
                    let r = r.to_mut();
                    for (name, e) in named {
                        let v = eval(e, r).unwrap_or(Value::Null);
                        r.insert(name.clone(), v);
                    }
                }
            }
            &Command::Limit(n) => {
                rows.truncate(n.min(MAX_LIMIT));
                explicit_limit = true;
            }
            Command::Keep(patterns) => {
                let mut kept = Vec::new();
                for (p, _) in patterns {
                    let hits: Vec<_> = cols
                        .iter()
                        .filter(|c| aliases::glob(p, c) && !kept.contains(*c))
                        .cloned()
                        .collect();
                    kept.extend(hits);
                }
                cols = kept;
            }
            Command::Drop(patterns) => {
                cols.retain(|c| !patterns.iter().any(|(p, _)| aliases::glob(p, c)));
            }
            Command::Sort(keys) => {
                sort_rows(&mut rows, keys);
            }
            Command::Rename(pairs) => {
                for r in &mut rows {
                    let r = r.to_mut();
                    for ((old, _), new, _) in pairs.iter().filter(|(o, n, _)| o.0 != *n) {
                        let v = r.remove(old).unwrap_or(Value::Null);
                        r.insert(new.clone(), v);
                    }
                }
            }
            Command::Stats(aggs, by) => {
                // ponytail: linear group lookup, O(rows × groups); hash keys when that matters
                let mut buckets: Vec<(Vec<Value>, Vec<&Row>)> = Vec::new();
                for r in &rows {
                    // A row joins the group of every combination of its distinct key values.
                    let mut keys: Vec<Vec<Value>> = vec![Vec::new()];
                    for (_, e) in by {
                        let mut vs = Vec::new();
                        match eval(e, r) {
                            Some(Value::Array(a)) => a.into_iter().for_each(|v| {
                                if !vs.contains(&v) {
                                    vs.push(v);
                                }
                            }),
                            v => vs.push(v.unwrap_or(Value::Null)),
                        }
                        keys = keys
                            .into_iter()
                            .flat_map(|k| {
                                vs.iter()
                                    .map(move |v| [&k[..], std::slice::from_ref(v)].concat())
                            })
                            .collect();
                    }
                    for key in keys {
                        match buckets.iter_mut().find(|(k, _)| *k == key) {
                            Some(b) => b.1.push(r),
                            None => buckets.push((key, vec![r])),
                        }
                    }
                }
                if by.is_empty() && buckets.is_empty() {
                    buckets.push((Vec::new(), Vec::new()));
                }
                let agg = |name: &str, e: &Expr, group: &[&Row]| {
                    let (f, arg, param) = aggregate(e).expect("STATS was verified");
                    let Some(arg) = arg else {
                        return group.len().into();
                    };
                    // Every value in the group: multi-values count value by value.
                    let all = group.iter().filter_map(|r| eval(arg, r));
                    let all: Vec<Value> = all
                        .flat_map(|v| match v {
                            Value::Array(a) => a,
                            v => vec![v],
                        })
                        .filter(|v| !v.is_null())
                        .collect();
                    let doubles = all.iter().filter_map(Value::as_f64);
                    match f.as_str() {
                        "COUNT" => all.len().into(),
                        "COUNT_DISTINCT" => {
                            // Hashed like ES's HllStates, with its default precision threshold.
                            // ponytail: dates, IPs and versions hash their text where ES hashes
                            // their long or binary form; the same count up to the threshold
                            use crate::hll::{self, Hll};
                            let mut counter = Hll::new(hll::precision_from_threshold(3000));
                            for v in &all {
                                counter.collect(match v {
                                    Value::Number(n) if n.is_f64() => {
                                        hll::mix64(n.as_f64().unwrap_or_default().to_bits())
                                    }
                                    Value::Number(n) => {
                                        hll::mix64(n.as_i64().unwrap_or_default() as u64)
                                    }
                                    v => hll::murmur3(es_index::postings::text(v).as_bytes()),
                                });
                            }
                            counter.cardinality().into()
                        }
                        "MIN" | "MAX" => {
                            mv_fn(&format!("MV_{f}"), &Value::Array(all)).unwrap_or(Value::Null)
                        }
                        _ if all.is_empty() => Value::Null,
                        "VALUES" => {
                            // Distinct values in the order they come, like ES's BytesRefHash.
                            let mut distinct = Vec::new();
                            all.into_iter().for_each(|v| {
                                if !distinct.contains(&v) {
                                    distinct.push(v);
                                }
                            });
                            match distinct.len() {
                                1 => distinct.remove(0),
                                _ => Value::Array(distinct),
                            }
                        }
                        // An overflowing long sum is null, like ES (which also warns).
                        "SUM" if types[name] == "long" => all
                            .iter()
                            .try_fold(0i64, |s, v| s.checked_add(v.as_i64()?))
                            .map_or(Value::Null, Value::from),
                        // Doubles sum with Kahan compensation, like ES's CompensatedSum.
                        "SUM" => crate::aggs::compensated_sum(doubles).into(),
                        "MEDIAN" | "PERCENTILE" => {
                            let p = match param {
                                Some(p) => eval(p, &Row::new()).as_ref().and_then(Value::as_f64),
                                None => Some(50.0),
                            };
                            let Some(p) = p.filter(|p| (0.0..=100.0).contains(p)) else {
                                return Value::Null;
                            };
                            // Like ES's QuantileStates, a t-digest of compression 1000.
                            let mut digest = crate::tdigest::TDigest::new(1000.0);
                            doubles.for_each(|x| digest.add(x, 1));
                            digest.quantile(p / 100.0).into()
                        }
                        _ => (crate::aggs::compensated_sum(doubles) / all.len() as f64).into(),
                    }
                };
                rows = buckets
                    .into_iter()
                    .map(|(key, group)| {
                        let mut r: Row = aggs
                            .iter()
                            .map(|(a, e)| (a.clone(), agg(a, e, &group)))
                            .collect();
                        r.extend(by.iter().map(|(b, _)| b.clone()).zip(key));
                        Cow::Owned(r)
                    })
                    .collect();
            }
        }
    }
    if !explicit_limit {
        rows.truncate(DEFAULT_LIMIT);
    }
    let (documents_found, values_loaded) = match counted {
        true => load_counts(&groups, &cmds, &cols, &types),
        false => (0, 0),
    };
    if empty {
        // Wildcards that match nothing: Elasticsearch answers with a placeholder column.
        return Ok(Response {
            columns: vec![Column {
                name: "<no-fields>".into(),
                r#type: "null".into(),
                original_types: None,
                suggested_cast: None,
            }],
            values: Vec::new(),
            documents_found,
            values_loaded,
        });
    }
    Ok(Response {
        documents_found,
        values_loaded,
        columns: cols
            .iter()
            .map(|c| {
                let original = conflicts
                    .get(c)
                    .map(|m| m.keys().cloned().collect::<Vec<_>>());
                let cast = original
                    .as_ref()
                    .map(|o| match o[..] == ["date_nanos", "datetime"] {
                        true => "date_nanos".into(),
                        false => "keyword".into(),
                    });
                Column {
                    name: c.clone(),
                    r#type: types[c].clone(),
                    original_types: original,
                    suggested_cast: cast,
                }
            })
            .collect(),
        values: rows
            .into_iter()
            .map(|r| cols.iter().map(|c| r[c.as_str()].clone()).collect())
            .collect(),
    })
}

type Row = BTreeMap<String, Value>;

/// The rows a source command yields, before any processing command.
struct Table {
    /// Each column's ES|QL type (`unsupported` for a type conflict).
    types: BTreeMap<String, String>,
    /// For each conflicting column, the indices per mapping type.
    conflicts: BTreeMap<String, BTreeMap<String, Vec<String>>>,
    /// One group of rows per index.
    groups: Vec<Vec<Row>>,
    /// The columns, in output order.
    cols: Vec<String>,
    /// No index matched: the response is a placeholder column.
    empty: bool,
    /// Whether the rows came from documents, for `documents_found` and `values_loaded`.
    counted: bool,
}

/// The table of `FROM sources`: every document of the indices they resolve to.
fn table_from(
    indices: &aliases::Map<impl aliases::IndexMeta + std::borrow::Borrow<Index>>,
    sources: &[String],
    metadata: &[String],
    read: &dyn Fn(&str) -> bool,
) -> Result<Table, EsqlError> {
    // Like Elasticsearch: a missing index is only "unknown" when no source matches anything.
    let none_match = || {
        sources
            .iter()
            .all(|s| !aliases::resolve_data(indices, s).is_ok_and(|t| !t.is_empty()))
    };
    let unknown_index = |e| match e {
        ResolveError::IndexClosed(i) => EsqlError::IndexClosed(i),
        ResolveError::IndexNotFound(i) if !none_match() => EsqlError::IndexNotFound(i),
        ResolveError::IndexNotFound(_) => {
            EsqlError::Verification(format!("Unknown index [{}]", sources.join(",")))
        }
        ResolveError::NoWriteIndex(i)
        | ResolveError::InvalidIndexName(i)
        | ResolveError::AliasNotAllowed(i) => {
            EsqlError::Verification(format!("Unknown index [{i}]"))
        }
    };
    let targets = aliases::resolve_data(indices, &sources.join(",")).map_err(unknown_index)?;
    // Per column: its ES|QL type, and the indices for each mapping type (named like ES|QL does).
    let mut mapped: BTreeMap<String, (String, BTreeMap<String, Vec<String>>)> = BTreeMap::new();
    let mut docs = Vec::new();
    for t in &targets {
        let index: &Index = indices[&t.index].borrow();
        // Read once: on-disk documents are decoded on every read.
        let mut mine: Vec<_> = index.docs().collect();
        for (c, (es_type, ty)) in columns(index, mine.iter().map(|(_, d)| &d.source)) {
            let es_type = if es_type == "date" {
                "datetime".into()
            } else {
                es_type
            };
            let e = mapped.entry(c).or_insert_with(|| (ty, BTreeMap::new()));
            e.1.entry(es_type).or_default().push(t.index.clone());
        }
        let filter = t
            .filter
            .as_ref()
            .map(crate::parse_query)
            .transpose()
            .map_err(|e| EsqlError::Verification(e.msg))?;
        let searcher = filter.as_ref().map(|f| (crate::Searcher::new(index, f), f));
        mine.retain(|(id, d)| {
            searcher
                .as_ref()
                .is_none_or(|(s, f)| s.score(f, id, d).is_some())
        });
        mine.sort_by_key(|(_, d)| d.seq_no);
        docs.push(mine);
    }
    let conflicts: BTreeMap<String, BTreeMap<String, Vec<String>>> = mapped
        .iter_mut()
        .filter(|(_, (_, m))| m.len() > 1)
        .map(|(c, (_, m))| {
            m.values_mut().for_each(|v| v.sort());
            (c.clone(), m.clone())
        })
        .collect();
    let mut types: BTreeMap<String, String> = mapped
        .into_iter()
        .map(|(c, (t, _))| {
            let t = if conflicts.contains_key(&c) {
                "unsupported".into()
            } else {
                t
            };
            (c, t)
        })
        .collect();
    // One group of rows per index, for counting what Elasticsearch's per-shard plan loads.
    let groups = docs
        .iter()
        .zip(&targets)
        .map(|(g, t)| {
            let mappings = &indices[&t.index].borrow().mappings;
            // Per column read, what loading it needs: looked up once, not per document.
            let read: Vec<_> = (types.iter())
                .filter(|(c, _)| read(c))
                .map(|(c, ty)| {
                    let multi_field = c.ends_with(".keyword")
                        || !mappings.properties.contains_key(c) && mappings.field(c).is_some();
                    (
                        c,
                        ty,
                        conflicts.contains_key(c),
                        mappings.field(c),
                        multi_field,
                    )
                })
                .collect();
            g.iter()
                .map(|(id, d)| {
                    let mut row: Row = (read.iter())
                        .map(|&(c, ty, conflict, m, multi_field)| {
                            let v = match conflict {
                                true => Value::Null,
                                false => doc_value(cell(&d.source, c, multi_field), ty, m),
                            };
                            (c.clone(), v)
                        })
                        .collect();
                    for f in metadata {
                        let v = match f.as_str() {
                            "_id" => Value::from(id.to_string()),
                            "_index" => Value::from(t.index.as_str()),
                            _ => Value::from(d.version),
                        };
                        row.insert(f.clone(), v);
                    }
                    row
                })
                .collect()
        })
        .collect();
    // Metadata columns follow the fields, in the order listed.
    let mut cols: Vec<String> = types.keys().cloned().collect();
    for f in metadata {
        cols.retain(|c| c != f);
        cols.push(f.clone());
        types.insert(f.clone(), metadata_type(f).unwrap_or_default().into());
    }
    Ok(Table {
        cols,
        types,
        conflicts,
        groups,
        empty: targets.is_empty(),
        counted: true,
    })
}

/// Whether a query reads (or outputs) a source column: every column unless a KEEP or STATS
/// narrows the output to the columns its commands name. Skipping the rest saves loading them.
fn read(cmds: &[Command]) -> impl Fn(&str) -> bool + '_ {
    let narrowed = (cmds.iter()).any(|c| matches!(c, Command::Keep(_) | Command::Stats(..)));
    let mut named = BTreeSet::new();
    for c in cmds {
        match c {
            Command::Where(e) => named.extend(names(e)),
            Command::Eval(es) => named.extend(es.iter().flat_map(|(_, e)| names(e))),
            Command::Sort(keys) => named.extend(keys.iter().map(|k| k.field.clone())),
            Command::Rename(pairs) => named.extend(pairs.iter().map(|((old, _), ..)| old.clone())),
            Command::Stats(aggs, by) => {
                named.extend(aggs.iter().chain(by).flat_map(|(_, e)| names(e)))
            }
            Command::Keep(_) | Command::Drop(_) | Command::Limit(_) => {}
        }
    }
    move |c| {
        !narrowed
            || named.contains(c)
            || cmds.iter().any(|cmd| match cmd {
                Command::Keep(ps) => ps.iter().any(|(p, _)| aliases::glob(p, c)),
                _ => false,
            })
    }
}

fn sort_rows<R: Borrow<Row>>(rows: &mut [R], keys: &[SortKey]) {
    rows.sort_by(|a, b| {
        let (a, b) = (a.borrow(), b.borrow());
        keys.iter().fold(Ordering::Equal, |acc, k| {
            acc.then_with(|| {
                let (x, y) = (&a[k.field.as_str()], &b[k.field.as_str()]);
                match (x.is_null(), y.is_null()) {
                    (true, true) => Ordering::Equal,
                    (true, false) => {
                        if k.nulls_first {
                            Ordering::Less
                        } else {
                            Ordering::Greater
                        }
                    }
                    (false, true) => {
                        if k.nulls_first {
                            Ordering::Greater
                        } else {
                            Ordering::Less
                        }
                    }
                    _ => {
                        let o = compare(x, y).unwrap_or(Ordering::Equal);
                        if k.desc { o.reverse() } else { o }
                    }
                }
            })
        })
    });
}

/// `documents_found` and `values_loaded` as Elasticsearch 9.5.5's plan counts them. Up to the
/// first LIMIT (implicitly 1000) the plan runs per shard: Lucene runs the filters on a field and a
/// literal and, with no other filter, the LIMIT or a non-`text` SORT (a top N per index); each
/// field is loaded only for the rows still alive when the plan first needs it. Everything after the
/// first LIMIT runs on its output, so the fields it uses load for those rows.
fn load_counts(
    groups: &[Vec<Row>],
    cmds: &[Command],
    out: &[String],
    types: &BTreeMap<String, String>,
) -> (usize, usize) {
    // ponytail: the plan is cut at an EVAL, RENAME or STATS as if at a LIMIT, though ES pushes a
    // LIMIT through an EVAL or RENAME and a STATS reads every match rather than the first 1000
    let (head, tail) = cmds.split_at(
        cmds.iter()
            .position(|c| {
                matches!(
                    c,
                    Command::Limit(_) | Command::Eval(_) | Command::Rename(_) | Command::Stats(..)
                )
            })
            .unwrap_or(cmds.len()),
    );
    let limit = match tail.first() {
        Some(&Command::Limit(n)) => n.min(MAX_LIMIT),
        _ => DEFAULT_LIMIT,
    };
    let mut filters = Vec::new();
    let mut sort: &[SortKey] = &[];
    for c in head {
        match c {
            Command::Where(e) => conjuncts(e, &mut filters),
            // ES drops a SORT that a later SORT overrides.
            Command::Sort(keys) => sort = keys,
            _ => {}
        }
    }
    let (pushed, residual): (Vec<&Expr>, Vec<&Expr>) =
        filters.into_iter().partition(|e| pushable(e));
    // The fields the tail uses, named as before it: walked backwards through renames.
    let mut later: BTreeSet<String> = out.iter().cloned().collect();
    for c in tail.iter().rev() {
        match c {
            Command::Rename(pairs) => {
                for ((old, _), new, _) in pairs.iter().rev() {
                    if later.remove(new) {
                        later.insert(old.clone());
                    }
                }
            }
            Command::Where(e) => later.extend(names(e)),
            Command::Eval(named) => later.extend(named.iter().flat_map(|(_, e)| names(e))),
            Command::Sort(keys) => later.extend(keys.iter().map(|k| k.field.clone())),
            Command::Stats(_, by) => later.extend(by.iter().flat_map(|(_, e)| names(e))),
            _ => {}
        }
    }
    let keys: BTreeSet<String> = sort.iter().map(|k| k.field.clone()).collect();
    let passes = |r: &Row, es: &[&Expr]| es.iter().all(|e| eval(e, r) == Some(true.into()));
    let values = |rows: &[&Row], fields: &BTreeSet<String>| -> usize {
        let count = |v: &Value| match v {
            Value::Null => 0,
            Value::Array(a) => a.len(),
            _ => 1,
        };
        rows.iter()
            .flat_map(|r| fields.iter().filter_map(|f| r.get(f.as_str()).map(count)))
            .sum()
    };
    let found = groups
        .iter()
        .map(|g| g.iter().filter(|r| passes(r, &pushed)).collect::<Vec<_>>());
    if residual.is_empty() && keys.iter().all(|k| types[k] != "text") {
        let emitted: Vec<&Row> = match sort.is_empty() {
            true => found.flatten().take(limit).collect(),
            false => found
                .flat_map(|mut g| {
                    sort_rows(&mut g, sort);
                    g.truncate(limit);
                    g
                })
                .collect(),
        };
        return (emitted.len(), values(&emitted, &(&keys | &later)));
    }
    // ponytail: reads every match; ES's source can stop early once a plain LIMIT fills
    let mut rows: Vec<&Row> = found.flatten().collect();
    let documents = rows.len();
    let mut loaded: BTreeSet<String> = residual.iter().flat_map(|e| names(e)).collect();
    let mut n = values(&rows, &loaded);
    rows.retain(|r| passes(r, &residual));
    n += values(&rows, &(&keys - &loaded));
    loaded.extend(keys);
    sort_rows(&mut rows, sort);
    rows.truncate(limit);
    n += values(&rows, &(&later - &loaded));
    (documents, n)
}

fn conjuncts<'a>(e: &'a Expr, out: &mut Vec<&'a Expr>) {
    match e {
        Expr::And(a, b, _) => {
            conjuncts(a, out);
            conjuncts(b, out);
        }
        _ => out.push(e),
    }
}

/// Whether Lucene can run the filter: anything but a comparison of two fields.
fn pushable(e: &Expr) -> bool {
    match e {
        Expr::Cmp(_, a, b, _) => {
            !matches!((&**a, &**b), (Expr::Field(..), Expr::Field(..)))
                && pushable(a)
                && pushable(b)
        }
        // ponytail: ES may rewrite `age + 1 > 30` into a pushable `age > 29`; this does not
        Expr::Arith(..) | Expr::Call(..) | Expr::Neg(..) => names(e).is_empty(),
        Expr::Not(x, _) | Expr::IsNull(x, ..) | Expr::Like(x, ..) => pushable(x),
        Expr::And(a, b, _) | Expr::Or(a, b, _) => pushable(a) && pushable(b),
        Expr::Field(..) | Expr::Lit(..) | Expr::Interval(..) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Like Elasticsearch, NOW() is the query's start time on every row.
    #[test]
    fn esql_es_semantics_now_is_fixed_once_per_query() {
        let mut i = Index::default();
        for id in ["1", "2", "3"] {
            i.put(id, serde_json::json!({"n": 1}));
        }
        let indices = BTreeMap::from([("i".to_owned(), i)]);
        let r = run_at(&indices, "FROM i | EVAL t = NOW() | KEEP t", 1000).unwrap();
        let t = Value::from("1970-01-01T00:00:01.000Z");
        assert_eq!(r.values, vec![vec![t]; 3]);
    }
}
