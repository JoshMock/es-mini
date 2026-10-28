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
    borrow::Cow,
    cell::{OnceCell, RefCell},
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, HashMap},
    hash::{BuildHasher, Hash, Hasher},
    ops::{Range, RangeInclusive},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering::Relaxed},
    },
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
// ponytail: fails only when a row is evaluated; ES also fails folding a constant expression,
// even over no rows (`ROW x = 9223372036854775807 | WHERE false | EVAL ROUND(x, -1)`)
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
            let mut e = match src[int_end..].starts_with('.') {
                true => digits(int_end + 1),
                false => int_end,
            };
            // An exponent: `e`, an optional sign, and digits.
            let exp = src[e..]
                .strip_prefix(['e', 'E'])
                .map(|x| x.strip_prefix(['+', '-']).unwrap_or(x));
            if exp.is_some_and(|x| x.starts_with(|d: char| d.is_ascii_digit())) {
                e = digits(src.len() - exp.unwrap_or_default().len());
            }
            end = Some(e);
            let s = &src[i..e];
            let n = match s.contains(['.', 'e', 'E']) {
                true => s.parse::<f64>().ok().map(Value::from),
                false => (s.parse::<i64>().ok().map(Value::from))
                    .or_else(|| s.parse::<u64>().ok().map(Value::from))
                    .or_else(|| s.parse::<f64>().ok().map(Value::from)),
            };
            match n.ok_or_else(|| unrecognized(e))? {
                // An infinite double.
                Value::Null => return Err(parse_error(at, &format!("Number [{s}] is too large"))),
                n => Tok::Num(n),
            }
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
    /// A comparison, with its operands' types once `resolve` finds both are `CASTABLE` (a
    /// string literal compared with one of those is cast to its type).
    Cmp(
        &'static str,
        Box<Expr>,
        Box<Expr>,
        OnceCell<[&'static str; 2]>,
        Span,
    ),
    And(Box<Expr>, Box<Expr>, Span),
    Or(Box<Expr>, Box<Expr>, Span),
    Not(Box<Expr>, Span),
    IsNull(Box<Expr>, bool, Span),
    /// `x LIKE "pattern"`, or `x RLIKE "pattern"` when the flag is set.
    Like(Box<Expr>, String, bool, Span),
    /// `+ - * / %`, with the numeric type `resolve` gives it, which `eval` computes in, or
    /// `date` for a date plus or minus a time span.
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
            let eq = |x| {
                Expr::Cmp(
                    "==",
                    Box::new(left.clone()),
                    Box::new(x),
                    OnceCell::new(),
                    span.clone(),
                )
            };
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
                    OnceCell::new(),
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

/// `eval` of an operand, read in place when it is a field or a literal.
fn operand<'a>(e: &'a Expr, row: &'a dyn Fields) -> Option<Cow<'a, Value>> {
    match e {
        Expr::Lit(v, _) => Some(Cow::Borrowed(v)),
        Expr::Field(f, _) => Some(Cow::Borrowed(row.field(f))),
        e => eval(e, row).map(Cow::Owned),
    }
    .filter(|v| !v.is_null())
}

/// `f` of two operands' values, null if either is null. Fields and literals are read in place
/// without building an `operand` per row.
fn operands<T>(
    a: &Expr,
    b: &Expr,
    row: &dyn Fields,
    f: impl FnOnce(&Value, &Value) -> Option<T>,
) -> Option<T> {
    fn in_place<'a>(e: &'a Expr, row: &'a dyn Fields) -> Option<&'a Value> {
        match e {
            Expr::Lit(v, _) => Some(v),
            Expr::Field(f, _) => Some(row.field(f)),
            _ => None,
        }
    }
    match (in_place(a, row), in_place(b, row)) {
        (Some(x), Some(y)) if x.is_null() || y.is_null() => None,
        (Some(x), Some(y)) => f(x, y),
        _ => f(&*operand(a, row)?, &*operand(b, row)?),
    }
}

/// Three-valued logic: `None` is SQL NULL.
fn eval(e: &Expr, row: &dyn Fields) -> Option<Value> {
    match e {
        Expr::Lit(v, _) => Some(v.clone()).filter(|v| !v.is_null()),
        // ponytail: only DATE_TRUNC and BUCKET read a time span, straight from the literal
        Expr::Interval(..) => None,
        Expr::Field(f, _) => Some(row.field(f).clone()).filter(|v| !v.is_null()),
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
        Expr::Not(..) | Expr::And(..) | Expr::Or(..) | Expr::Cmp(..) => {
            truth(e, row).map(Value::Bool)
        }
        Expr::Arith(op, a, b, t, _) if t.get() == Some(&"date") => match (&**a, &**b) {
            (d, Expr::Interval(n, unit, _)) => {
                date_plus(&eval(d, row)?, n.checked_mul(op_sign(op))?, unit)
            }
            (Expr::Interval(n, unit, _), d) => date_plus(&eval(d, row)?, *n, unit),
            // A null operand.
            _ => None,
        },
        Expr::Arith(op, a, b, t, _) => {
            let t = t.get().copied()?;
            operands(a, b, row, |a, b| arith(op, t, a, b))
        }
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
                // Numbers widened by `conditional_type`; a negative one is not an unsigned_long.
                let widen = |v: &Value| match t {
                    "double" => v.as_f64().map(Value::from),
                    "unsigned_long" => v.as_u64().map(Value::from),
                    _ => Some(v.clone()),
                };
                return match v {
                    Value::Array(a) => {
                        let mut out: Vec<Value> = a.iter().filter_map(widen).collect();
                        match out.len() {
                            0 => None,
                            1 => out.pop(),
                            _ => Some(Value::Array(out)),
                        }
                    }
                    v => widen(&v),
                };
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
    }
}

/// `eval` of a condition, without building a `Value` for comparisons and logic.
fn truth(e: &Expr, row: &dyn Fields) -> Option<bool> {
    match e {
        Expr::Cmp(op, a, b, t, _) => {
            // ponytail: multi-valued fields compare as null, like ES (which also warns)
            let truth = |v: &Value| {
                v.as_bool()
                    .or_else(|| Some(v.as_str()?.eq_ignore_ascii_case("true")))
            };
            let ord = operands(a, b, row, |a, b| match t.get() {
                Some(["ip", _]) => Some(es_index::ip_bytes(a)?.cmp(&es_index::ip_bytes(b)?)),
                Some(["boolean", _]) => Some(truth(a)?.cmp(&truth(b)?)),
                Some([ta, tb]) => Some(epoch_nanos(ta, a)?.cmp(&epoch_nanos(tb, b)?)),
                None => compare(a, b),
            })?;
            Some(match *op {
                "==" => ord.is_eq(),
                "!=" => ord.is_ne(),
                "<" => ord.is_lt(),
                "<=" => ord.is_le(),
                ">" => ord.is_gt(),
                _ => ord.is_ge(),
            })
        }
        Expr::Not(x, _) => truth(x, row).map(|b| !b),
        Expr::And(a, b, _) => match (truth(a, row), truth(b, row)) {
            (Some(false), _) | (_, Some(false)) => Some(false),
            (Some(true), Some(true)) => Some(true),
            _ => None,
        },
        Expr::Or(a, b, _) => match (truth(a, row), truth(b, row)) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (Some(false), Some(false)) => Some(false),
            _ => None,
        },
        e => eval(e, row)?.as_bool(),
    }
}

/// Why string `s` does not convert to type `t`, in ES's words (`EsqlDataTypeConverter`).
fn cast_error(s: &str, t: &str) -> Option<String> {
    let v = Value::from(s);
    let (format, ok) = match t {
        "ip" => {
            return es_index::ip_bytes(&v)
                .is_none()
                .then(|| format!("'{s}' is not an IP string literal."));
        }
        "boolean" => return None,
        _ if s.is_empty() => return Some("cannot parse empty datetime".into()),
        "date" => (
            "strict_date_optional_time",
            es_index::parse_date_millis(&v).is_some(),
        ),
        _ => {
            let format = "strict_date_optional_time_nanos";
            let nanos = es_index::disk::date_nanos(Some(format), None, &v);
            (format, nanos.flatten().is_some())
        }
    };
    (!ok).then(|| format!("failed to parse date field [{s}] with format [{format}]"))
}

/// Epoch nanos of a value of type `date` or `date_nanos`.
fn epoch_nanos(t: &str, v: &Value) -> Option<i128> {
    match t {
        "date" => Some(i128::from(es_index::parse_date_millis(v)?) * 1_000_000),
        _ => es_index::disk::date_nanos(None, None, v)
            .flatten()
            .map(i128::from),
    }
}

fn op_sign(op: &str) -> i64 {
    if op == "-" { -1 } else { 1 }
}

/// Date `v` plus `n` of time span `unit` in UTC, like Java's `ZonedDateTime.plus`: months clamp
/// the day to the month's last. Null where the result is out of range.
fn date_plus(v: &Value, n: i64, unit: &str) -> Option<Value> {
    let t = es_index::parse_date_millis(v)?;
    // Past 290 million years the millis overflow, as `asMillis` would.
    let months = |m: i64| {
        let n = n.checked_mul(m).filter(|n| n.abs() <= 12 * 290_000_000)?;
        Some(es_index::date_math::add_months(t, n))
    };
    let t = match unit {
        "month" => months(1),
        "quarter" => months(3),
        "year" => months(12),
        _ => {
            let ms: i64 = match unit {
                "millisecond" => 1,
                "second" => 1000,
                "minute" => 60_000,
                "hour" => 3_600_000,
                "day" => 86_400_000,
                _ => 7 * 86_400_000,
            };
            n.checked_mul(ms).and_then(|d| t.checked_add(d))
        }
    }?;
    Some(es_index::iso_date(t).into())
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
const CONVERSIONS: [(&str, &str); 7] = [
    ("TO_STRING", "keyword"),
    ("TO_DATETIME", "date"),
    ("TO_INTEGER", "integer"),
    ("TO_LONG", "long"),
    ("TO_DOUBLE", "double"),
    ("TO_BOOLEAN", "boolean"),
    ("TO_IP", "ip"),
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
    // Elasticsearch's `EsqlDataTypeConverter.commonType(t, target) == target`.
    let widens = |t: &str| match target {
        Some("double") => true,
        Some("unsigned_long") => t != "double",
        _ => Some(t) == target || (t, target) == ("integer", Some("long")),
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
            match t {
                "integer" => i32::try_from(r).ok().map(Value::from),
                // ES's `RoundLongEvaluator` does not turn the overflow into a warning.
                "long" => i64::try_from(r)
                    .ok()
                    .map(Value::from)
                    .or_else(|| fail(500, "arithmetic_exception", "long overflow".into())),
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

/// Elasticsearch's `Maths.round(Number, long)` for a double: half away from zero. Past 2^63 ES
/// divides in 34-digit `BigDecimal`s, which rounds to the same double as an f64 division: a
/// quotient of two doubles is never within 2^-107 (relative) of a halfway point between doubles,
/// and the two 34-digit roundings move it by less than 10^-33.
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
/// evaluators: `None` for a multi-valued argument or an unknown part (where ES warns).
// ponytail: DATE_FORMAT prints null for a pattern es_index::format_date lacks (week fields, zone
// names) or rejects, where ES formats it or fails the query
fn date_fn(f: &str, args: &[Value]) -> Option<Value> {
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
    chrono_field(args[0].as_str()?, t).map(Value::from)
}

/// Java's `ChronoField` named `part` (ignoring case) of epoch millis `t` in UTC.
fn chrono_field(part: &str, t: i64) -> Option<i64> {
    const DAY: i64 = 86_400_000;
    let (days, ms) = (t.div_euclid(DAY), t.rem_euclid(DAY));
    let (y, m, d) = es_index::civil_from_days(days);
    let doy = days - es_index::days_from_civil(y, 1, 1) + 1;
    let h = ms / 3_600_000;
    Some(match part.to_ascii_uppercase().as_str() {
        "NANO_OF_SECOND" => ms % 1000 * 1_000_000,
        "NANO_OF_DAY" => ms * 1_000_000,
        "MICRO_OF_SECOND" => ms % 1000 * 1000,
        "MICRO_OF_DAY" => ms * 1000,
        "MILLI_OF_SECOND" => ms % 1000,
        "MILLI_OF_DAY" => ms,
        "SECOND_OF_MINUTE" => ms / 1000 % 60,
        "SECOND_OF_DAY" => ms / 1000,
        "MINUTE_OF_HOUR" => ms / 60_000 % 60,
        "MINUTE_OF_DAY" => ms / 60_000,
        "HOUR_OF_AMPM" => h % 12,
        "CLOCK_HOUR_OF_AMPM" => (h + 11) % 12 + 1,
        "HOUR_OF_DAY" => h,
        "CLOCK_HOUR_OF_DAY" => (h + 23) % 24 + 1,
        "AMPM_OF_DAY" => h / 12,
        // ISO: Monday is 1, and 1970-01-01 was a Thursday.
        "DAY_OF_WEEK" => (days + 3).rem_euclid(7) + 1,
        "ALIGNED_DAY_OF_WEEK_IN_MONTH" => (d - 1) % 7 + 1,
        "ALIGNED_DAY_OF_WEEK_IN_YEAR" => (doy - 1) % 7 + 1,
        "DAY_OF_MONTH" => d,
        "DAY_OF_YEAR" => doy,
        "EPOCH_DAY" => days,
        "ALIGNED_WEEK_OF_MONTH" => (d - 1) / 7 + 1,
        "ALIGNED_WEEK_OF_YEAR" => (doy - 1) / 7 + 1,
        "MONTH_OF_YEAR" => m,
        "PROLEPTIC_MONTH" => y * 12 + m - 1,
        "YEAR_OF_ERA" if y < 1 => 1 - y,
        "YEAR_OF_ERA" | "YEAR" => y,
        "ERA" => i64::from(y >= 1),
        "INSTANT_SECONDS" => t.div_euclid(1000),
        "OFFSET_SECONDS" => 0,
        _ => return None,
    })
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
fn bucket(args: &[Expr], date: bool, row: &dyn Fields) -> Option<Value> {
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
        "CONCAT" => {
            let out = args.iter().map(Value::as_str).collect::<Option<String>>()?;
            if out.len() > 1 << 20 {
                let reason = "concatenating more than [1048576] bytes is not supported";
                return fail(400, "esql_client_exception", reason.into());
            }
            return Some(out.into());
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

fn iso_date_value(millis: i64) -> Value {
    es_index::iso_date(millis).into()
}

/// Epoch nanos `n` as ES's `strict_date_optional_time_nanos`: 3 to 9 fraction digits.
fn iso_nanos(n: i64) -> String {
    let millis = es_index::iso_date(n.div_euclid(1_000_000));
    let fraction = format!("{:09}", n.rem_euclid(1_000_000_000));
    let fraction = fraction.trim_end_matches('0');
    format!("{}.{fraction:0<3}Z", &millis[..millis.len() - 5])
}

/// Function `f` (upper-case, one of `CONVERSIONS`) of a single value of type `t`, like
/// Elasticsearch's convert evaluators: `None` where they fail (and warn).
fn convert(f: &str, t: &str, v: &Value) -> Option<Value> {
    let nanos = || es_index::disk::date_nanos(None, None, v).flatten();
    // Dates and date_nanos convert as their epoch long, but for their strings.
    let long: Value = match (f, t) {
        ("TO_STRING", "date") => return es_index::parse_date_millis(v).map(iso_date_value),
        ("TO_STRING", "date_nanos") => return nanos().map(|n| iso_nanos(n).into()),
        // The canonical text, like `DocValueFormat.IP`.
        ("TO_STRING", "ip") | ("TO_IP", _) => {
            return es_index::ip_bytes(v).map(|b| es_index::ip_string(b).into());
        }
        // Truncated to millis, like `DateUtils.toMilliSeconds`.
        ("TO_DATETIME", "date_nanos") => return nanos().map(|n| iso_date_value(n / 1_000_000)),
        (_, "date") => es_index::parse_date_millis(v)?.into(),
        (_, "date_nanos") => nanos()?.into(),
        _ => v.clone(),
    };
    let (t, v) = if t.starts_with("date") {
        ("long", &long)
    } else {
        (t, v)
    };
    let num = || match t {
        "double" => v.as_f64().map(Ok),
        _ => v
            .as_i64()
            .map(Err)
            .or_else(|| v.as_u64().map(|u| Ok(u as f64))),
    };
    match (f, v) {
        ("TO_DATETIME", Value::String(_)) => es_index::parse_date_millis(v).map(iso_date_value),
        // Numbers are epoch millis, read like TO_LONG reads them.
        ("TO_DATETIME", _) => convert("TO_LONG", t, v)?.as_i64().map(iso_date_value),
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
        Expr::Cmp(_, a, b, ..)
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

/// The types a string literal compared with them is cast to, like ES's ImplicitCasting.
const CASTABLE: [&str; 4] = ["date", "date_nanos", "ip", "boolean"];

const NUMERIC: [&str; 4] = ["integer", "long", "unsigned_long", "double"];

/// The ES|QL type of a supported `METADATA` field.
fn metadata_type(field: &str) -> Option<&'static str> {
    match field {
        "_id" | "_index" => Some("keyword"),
        "_version" => Some("long"),
        _ => None,
    }
}

/// Elasticsearch's name for ES|QL type `t` in messages: dates are `datetime`.
fn es_name(t: &str) -> &str {
    if t == "date" { "datetime" } else { t }
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
            let date = |t: &str| t == "date" || t == "date_nanos";
            let temporal = |t: &str| t == "date_period" || t == "time_duration";
            // ponytail: no dense_vector operands; es-mini maps no dense_vector fields
            let accepted = match *op {
                "+" | "-" => "date_nanos, datetime, numeric or dense_vector",
                "*" | "/" => "numeric or dense_vector",
                _ => "numeric",
            };
            let input = |x: &Expr, t: &str| {
                let ok =
                    numeric(t) || t == "null" || accepted.contains(',') && (date(t) || temporal(t));
                (!ok).then(|| {
                    format!(
                        "[{accepted}], found value [{}] type [{}]",
                        text(x),
                        es_name(t)
                    )
                })
            };
            // Elasticsearch's `DateTimeArithmeticOperation.checkCompatibility`.
            let incompatible = match (ta.as_str(), tb.as_str()) {
                ("null", _) | (_, "null") => false,
                (x, y) if date(x) && temporal(y) || temporal(x) && date(y) => false,
                (x, y) if temporal(x) && x == y => false,
                (x, y) if date(x) || date(y) || temporal(x) || temporal(y) => true,
                (x, y) => (x == "unsigned_long") != (y == "unsigned_long"),
            };
            // The type of a date or time span operation, `commonType`'s pick.
            let dated = [&ta, &tb]
                .into_iter()
                .find(|t| date(t))
                .or_else(|| [&ta, &tb].into_iter().find(|t| temporal(t)));
            let problem = match (input(a, &ta), input(b, &tb)) {
                (Some(found), _) => format!("first argument of [{}] must be {found}", text(e)),
                (_, Some(found)) => format!("second argument of [{}] must be {found}", text(e)),
                _ if incompatible => format!(
                    "[{op}] has arguments with incompatible types [{}] and [{}]",
                    es_name(&ta),
                    es_name(&tb)
                ),
                // Elasticsearch's `Sub.resolveType`.
                _ if *op == "-" && tb == "date" => format!(
                    "[-] arguments are in unsupported order: cannot subtract a [DATETIME] value \
                     [{}] from a [{}] amount [{}]",
                    text(b),
                    ta.to_ascii_uppercase(),
                    text(a)
                ),
                // ponytail: date_nanos and folding two time spans are not computed
                _ if dated.is_some_and(|t| t != "date") => {
                    format!("[{}] is not supported by es-mini", text(e))
                }
                _ if dated.is_some() => {
                    let _ = t.set("date");
                    return Some("date".into());
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
                            let widened =
                                ["double", "unsigned_long"].into_iter().find(|w| out == *w);
                            let _ = t.set((*name, widened.unwrap_or("any")));
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
                // Like ES building its evaluator, an unknown literal part fails the query.
                if let ("DATE_EXTRACT", Expr::Lit(Value::String(part), _)) = (*name, &args[0])
                    && chrono_field(part, 0).is_none()
                {
                    let reason = format!("invalid date field for [{}]: {part}", text(e));
                    fail(400, "invalid_argument_exception", reason);
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
            // The types each conversion takes (of those es-mini has) and how ES lists all it
            // takes.
            // ponytail: no TO_VERSION, as es-mini has no version type; no TO_IP leading_zeros
            // option, so it always rejects leading zeros like ES's default
            let (takes, accepted): (&[&str], _) = match name {
                "TO_STRING" => (&["boolean", "date", "date_nanos", "ip"], None),
                "TO_DATETIME" => (
                    &["date", "date_nanos"],
                    Some("date_nanos or datetime or numeric or string"),
                ),
                "TO_INTEGER" => (
                    &["boolean", "date"],
                    Some("boolean or counter_integer or datetime or numeric or string"),
                ),
                "TO_LONG" => (
                    &["boolean", "date", "date_nanos"],
                    Some(
                        "boolean or counter_integer or counter_long or date_nanos or datetime \
                         or geohash or geohex or geotile or numeric or string",
                    ),
                ),
                "TO_DOUBLE" => (
                    &["boolean", "date"],
                    Some(
                        "boolean or counter_double or counter_integer or counter_long or \
                         datetime or numeric or string",
                    ),
                ),
                "TO_IP" => (&["ip"], Some("ip or string")),
                _ => (&["boolean"], Some("boolean or numeric or string")),
            };
            let numeric: &[&str] = if name == "TO_IP" { &[] } else { &NUMERIC };
            let Some(&ta) = ["null", "keyword", "text"]
                .iter()
                .chain(numeric)
                .chain(takes)
                .find(|n| **n == ta)
            else {
                let reason = match accepted {
                    Some(a) => format!(
                        "argument of [{}] must be [{a}], found value [{}] type [{}]",
                        text(e),
                        text(arg),
                        es_name(&ta)
                    ),
                    // ponytail: ES's list of what TO_STRING takes is not spelled out here
                    None => format!("[{f}] of [{ta}] is not supported by es-mini"),
                };
                bad.push((*at, reason));
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
        Expr::Cmp(_, a, b, typed, _) => {
            let (ta, tb) = (resolve(a, src, types, bad), resolve(b, src, types, bad));
            let (mut ta, mut tb) = (ta?, tb?);
            let literal = |x: &Expr, other: &String| match x {
                Expr::Lit(Value::String(s), (at, _)) if CASTABLE.contains(&other.as_str()) => {
                    Some((s.clone(), *at, other.clone()))
                }
                _ => None,
            };
            if let Some((s, at, to)) = literal(b, &ta).or_else(|| literal(a, &tb)) {
                if let Some(error) = cast_error(&s, &to) {
                    let upper = if to == "date" { "DATETIME" } else { &to };
                    let upper = upper.to_ascii_uppercase();
                    let msg = format!("Cannot convert string [{s}] to [{upper}], error [{error}]");
                    bad.push((at, msg));
                    return None;
                }
                (ta, tb) = (to.clone(), to);
            }
            let class = |t: &str| match t {
                "integer" | "long" | "unsigned_long" | "double" => Some("numeric"),
                "keyword" | "text" => Some("string"),
                "date" | "date_nanos" => Some("date"),
                "boolean" => Some("boolean"),
                "ip" => Some("ip"),
                _ => None,
            };
            match (class(&ta), class(&tb)) {
                (Some(ca), Some(cb)) if ca != cb => {
                    let first = if ca == "numeric" { ca } else { es_name(&ta) };
                    Some(format!(
                        "first argument of [{}] is [{first}] so second argument must also be \
                         [{first}] but was [{}]",
                        text(e),
                        es_name(&tb)
                    ))
                }
                _ => {
                    let name = |t: &str| CASTABLE.into_iter().find(|n| *n == t);
                    if let Some(both) = name(&ta).zip(name(&tb)) {
                        let _ = typed.set([both.0, both.1]);
                    }
                    None
                }
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
    // Verification fails a query before anything can fail evaluating it.
    match (r, FAILURE.take()) {
        (Ok(_), Some(e)) => Err(e),
        (r, _) => r,
    }
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
    let reads = read(&cmds);
    let table = match source {
        Some((sources, metadata, from)) => {
            let names: Vec<String> = metadata.iter().map(|(f, _)| f.clone()).collect();
            let table = table_from(indices, &sources, &names, &reads)?;
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
        None => Arc::new(Table {
            types: BTreeMap::new(),
            conflicts: BTreeMap::new(),
            groups: vec![Group::new(1, BTreeMap::new())],
            cols: Vec::new(),
            empty: false,
            counted: false,
        }),
    };
    let (mut types, mut conflicts, mut cols) = (
        table.types.clone(),
        table.conflicts.clone(),
        table.cols.clone(),
    );
    let (groups, empty, counted) = (&table.groups, table.empty, table.counted);
    // The columns the query reads, which a row it changes keeps: decided once, not per row.
    let kept: Vec<&String> = table.cols.iter().filter(|c| reads(c)).collect();
    // A referenced column must exist and have one type.
    let check = |cols: &[String], conflicts: &BTreeMap<String, _>, f: &str| match conflicts.get(f) {
        _ if !cols.iter().any(|c| c == f) => Some(unknown(f)),
        Some(m) => Some(conflict_error(f, m)),
        None => None,
    };
    // Per EVAL, the values it added to documents, which their rows then point into.
    let evals: Vec<(OnceCell<_>, OnceCell<Vec<Added>>)> =
        cmds.iter().map(|_| Default::default()).collect();
    let mut rows: Vec<Line> = groups.iter().flat_map(Group::lines).collect();
    let mut explicit_limit = false;
    // The rows at the cut of `load_counts`' plan, which it may reuse.
    let cut = plan_cut(&cmds);
    let mut at_cut = Vec::new();

    for (i, cmd) in cmds.iter().enumerate() {
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
                    // What COUNT_DISTINCT hashes: the value's long or binary form.
                    if let Expr::Call(_, _, hashed, _) = e
                        && let Some(&ty) = ["date", "date_nanos", "ip"].iter().find(|ty| **ty == t)
                    {
                        let _ = hashed.set(("COUNT_DISTINCT", ty));
                    }
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
                rows.retain(|r| truth(e, r) == Some(true));
            }
            Command::Eval(named) => {
                // A document's new values go in one block per EVAL, not an allocation per row.
                let mut block = Vec::new();
                let mut vals = Vec::with_capacity(named.len());
                for r in &mut rows {
                    vals.clear();
                    for (_, e) in named {
                        let names = &named[..vals.len()];
                        let row = Pending(r, names, &vals);
                        vals.push(eval(e, &row).unwrap_or(Value::Null));
                    }
                    match r {
                        Line::Doc(..) => block.append(&mut vals),
                        Line::Own(r) => {
                            r.extend(named.iter().map(|(n, _)| n.clone()).zip(vals.drain(..)))
                        }
                    }
                }
                let (block_cell, added_cell) = &evals[i];
                let block = block_cell.get_or_init(|| block);
                let mut at = 0;
                let added = rows.iter().filter_map(|r| match r {
                    Line::Doc(_, _, prev) => {
                        at += named.len();
                        let vals = &block[at - named.len()..at];
                        Some(Added(named, vals, *prev))
                    }
                    Line::Own(_) => None,
                });
                let mut added = added_cell.get_or_init(|| added.collect()).iter();
                for r in &mut rows {
                    if let Line::Doc(_, _, a) = r {
                        *a = added.next();
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
            // Only the rows a LIMIT keeps need sorting; KEEP, DROP and RENAME keep every row.
            Command::Sort(keys) => match cmds[i + 1..]
                .iter()
                .find(|c| !matches!(c, Command::Keep(_) | Command::Drop(_) | Command::Rename(_)))
            {
                Some(&Command::Limit(n)) => top_rows(&mut rows, keys, n.min(MAX_LIMIT)),
                _ => sort_rows(&mut rows, keys),
            },
            Command::Rename(pairs) => {
                for r in &mut rows {
                    let r = owned(r, &kept);
                    for ((old, _), new, _) in pairs.iter().filter(|(o, n, _)| o.0 != *n) {
                        let v = r.remove(old).unwrap_or(Value::Null);
                        r.insert(new.clone(), v);
                    }
                }
            }
            Command::Stats(aggs, by) => {
                // Each group's key and its rows' positions.
                let mut buckets: Vec<(Vec<Value>, Vec<usize>)> = Vec::new();
                let mut index = GroupIndex::default();
                // Each row's key values, read in place where a key is a column.
                let mut vals: Vec<Cow<Value>> = Vec::with_capacity(by.len());
                for (i, r) in rows.iter().enumerate() {
                    vals.clear();
                    vals.extend(by.iter().map(|(_, e)| match e {
                        Expr::Field(f, _) => Cow::Borrowed(r.field(f)),
                        e => Cow::Owned(eval(e, r).unwrap_or(Value::Null)),
                    }));
                    if !vals.iter().any(|v| v.is_array()) {
                        join_group(&mut buckets, &mut index, vals.iter().map(|v| &**v), i);
                        continue;
                    }
                    // A row joins the group of every combination of its distinct key values.
                    let mut keys: Vec<Vec<Value>> = vec![Vec::new()];
                    for v in &vals {
                        let mut vs = Vec::new();
                        match &**v {
                            Value::Array(a) => a.iter().for_each(|v| {
                                if !vs.contains(v) {
                                    vs.push(v.clone());
                                }
                            }),
                            v => vs.push(v.clone()),
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
                        join_group(&mut buckets, &mut index, key.iter(), i);
                    }
                }
                if by.is_empty() && buckets.is_empty() {
                    buckets.push((Vec::new(), Vec::new()));
                }
                // Each aggregate's argument per row, read in row order and in place where it is
                // a field: a group then gathers its values from these instead of chasing its
                // rows through memory.
                let arg = |e| aggregate(e).expect("STATS was verified").1;
                let computed: Vec<Vec<Value>> = (aggs.iter())
                    .map(|(_, e)| match arg(e) {
                        Some(Expr::Field(..)) | None => Vec::new(),
                        Some(a) => (rows.iter())
                            .map(|r| eval(a, r).unwrap_or(Value::Null))
                            .collect(),
                    })
                    .collect();
                let args: Vec<Vec<&Value>> = (aggs.iter().zip(&computed))
                    .map(|((_, e), c)| match arg(e) {
                        Some(Expr::Field(f, _)) => rows.iter().map(|r| r.field(f)).collect(),
                        _ => c.iter().collect(),
                    })
                    .collect();
                let agg = |name: &str, e: &Expr, group: &[usize], arg: &[&Value]| {
                    let (f, arg_expr, param) = aggregate(e).expect("STATS was verified");
                    if arg_expr.is_none() {
                        return group.len().into();
                    }
                    // Every value in the group: multi-values count value by value.
                    let mut all: Vec<&Value> = Vec::with_capacity(group.len());
                    for v in group.iter().map(|&i| arg[i]) {
                        match v {
                            Value::Array(a) => all.extend(a.iter().filter(|v| !v.is_null())),
                            Value::Null => {}
                            v => all.push(v),
                        }
                    }
                    let doubles = all.iter().filter_map(|v| v.as_f64());
                    match f.as_str() {
                        "COUNT" => all.len().into(),
                        "COUNT_DISTINCT" => {
                            // Hashed like ES's HllStates, with its default precision threshold:
                            // dates as epoch millis or nanos, IPs as their 16 bytes.
                            // ponytail: date_nanos read in the default format; es-mini has no
                            // version type to hash
                            use crate::hll::{self, Hll};
                            let mut counter = Hll::new(hll::precision_from_threshold(3000));
                            let ty = match e {
                                Expr::Call(_, _, hashed, _) => hashed.get().map(|h| h.1),
                                _ => None,
                            };
                            for v in &all {
                                let long = match ty {
                                    Some("date") => es_index::parse_date_millis(v),
                                    Some("date_nanos") => {
                                        es_index::disk::date_nanos(None, None, v).flatten()
                                    }
                                    _ => None,
                                };
                                let typed = match ty {
                                    Some("ip") => es_index::ip_bytes(v).map(|b| hll::murmur3(&b)),
                                    _ => long.map(|l| hll::mix64(l as u64)),
                                };
                                counter.collect(typed.unwrap_or_else(|| match v {
                                    Value::Number(n) if n.is_f64() => {
                                        hll::mix64(n.as_f64().unwrap_or_default().to_bits())
                                    }
                                    Value::Number(n) => {
                                        hll::mix64(n.as_i64().unwrap_or_default() as u64)
                                    }
                                    v => hll::murmur3(es_index::postings::text(v).as_bytes()),
                                }));
                            }
                            counter.cardinality().into()
                        }
                        "MIN" | "MAX" => {
                            let all = Value::Array(all.into_iter().cloned().collect());
                            mv_fn(&format!("MV_{f}"), &all).unwrap_or(Value::Null)
                        }
                        _ if all.is_empty() => Value::Null,
                        "VALUES" => {
                            // Distinct values in the order they come, like ES's BytesRefHash.
                            let mut distinct = Vec::new();
                            all.into_iter().for_each(|v| {
                                if !distinct.contains(v) {
                                    distinct.push(v.clone());
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
                        let mut r: Row = (aggs.iter().zip(&args))
                            .map(|((a, e), arg)| (a.clone(), agg(a, e, &group, arg)))
                            .collect();
                        r.extend(by.iter().map(|(b, _)| b.clone()).zip(key));
                        Line::Own(r)
                    })
                    .collect();
            }
        }
        if i + 1 == cut {
            at_cut = rows.iter().take(MAX_LIMIT).cloned().collect();
        }
    }
    if !explicit_limit {
        rows.truncate(DEFAULT_LIMIT);
    }
    let (documents_found, values_loaded) = match counted {
        true => load_counts(groups, &cmds, &cols, &types, &at_cut),
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
            .map(|r| cols.iter().map(|c| r.field(c).clone()).collect())
            .collect(),
    })
}

type Row = BTreeMap<String, Value>;

/// Adds `r` to the STATS group of `key`, a new group if none has it. `index` lists the groups
/// by their key's `key_hash`.
fn join_group<'v, T>(
    groups: &mut Vec<(Vec<Value>, Vec<T>)>,
    index: &mut GroupIndex,
    key: impl Iterator<Item = &'v Value> + Clone,
    r: T,
) {
    let slots = index.entry(key_hash(key.clone())).or_default();
    match slots.iter().find(|&&g| groups[g].0.iter().eq(key.clone())) {
        Some(&g) => groups[g].1.push(r),
        None => {
            slots.push(groups.len());
            groups.push((key.cloned().collect(), vec![r]));
        }
    }
}

/// Groups by their key's `key_hash`, which is already a hash: used as is.
type GroupIndex = HashMap<u64, Vec<usize>, std::hash::BuildHasherDefault<Prehashed>>;

#[derive(Default)]
struct Prehashed(u64);

impl Hasher for Prehashed {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, _: &[u8]) {
        unreachable!("only u64 keys")
    }
    fn write_u64(&mut self, n: u64) {
        self.0 = n;
    }
}

/// A hash of values that agrees with their equality: numbers hash their `f64` value (either
/// zero as `0.0`), objects only their size (their equality ignores key order).
fn key_hash<'v>(key: impl Iterator<Item = &'v Value>) -> u64 {
    fn value(v: &Value, h: &mut impl Hasher) {
        std::mem::discriminant(v).hash(h);
        match v {
            Value::Null => {}
            Value::Bool(b) => b.hash(h),
            Value::Number(n) => (n.as_f64().unwrap_or_default() + 0.0).to_bits().hash(h),
            Value::String(s) => s.hash(h),
            Value::Array(a) => a.iter().for_each(|v| value(v, h)),
            Value::Object(o) => o.len().hash(h),
        }
    }
    // Keyed at random once per process, so data cannot be made to collide on purpose: the
    // group index uses this hash as is.
    static KEYS: std::sync::LazyLock<std::hash::RandomState> =
        std::sync::LazyLock::new(Default::default);
    let mut h = KEYS.build_hasher();
    key.for_each(|v| value(v, &mut h));
    h.finish()
}

/// A row's values by column name; a column it lacks reads as null.
trait Fields {
    fn field(&self, c: &str) -> &Value;
}

static NULL: Value = Value::Null;

impl Fields for Row {
    fn field(&self, c: &str) -> &Value {
        self.get(c).unwrap_or(&NULL)
    }
}

/// One index's documents, column by column: rows read them in place instead of chasing a map
/// per row.
struct Group {
    len: usize,
    /// By name.
    cols: Vec<(String, Vec<Value>)>,
    /// Where the last column looked up is: rows read one column many times in a row. Shared by
    /// concurrent queries; a stale hint only costs the search.
    hint: AtomicUsize,
}

impl Group {
    fn new(len: usize, cols: BTreeMap<String, Vec<Value>>) -> Group {
        let cols = cols.into_iter().collect();
        let hint = AtomicUsize::new(0);
        Group { len, cols, hint }
    }

    fn col(&self, c: &str) -> Option<&[Value]> {
        let at = self.hint.load(Relaxed);
        if let Some((name, v)) = self.cols.get(at)
            && name == c
        {
            return Some(v);
        }
        let at = (self.cols.binary_search_by(|(name, _)| name.as_str().cmp(c))).ok()?;
        self.hint.store(at, Relaxed);
        Some(&self.cols[at].1)
    }

    fn lines(&self) -> impl Iterator<Item = Line<'_>> {
        (0..self.len).map(move |i| Line::Doc(self, i, None))
    }
}

/// A row in a query: a document still in its group with the columns EVAL added (the last of a
/// name wins), or one a command built or changed.
#[derive(Clone)]
enum Line<'a> {
    Doc(&'a Group, usize, Option<&'a Added<'a>>),
    Own(Row),
}

/// The columns one EVAL added to a document, their values, and what the EVALs before it added.
struct Added<'a>(&'a [(String, Expr)], &'a [Value], Option<&'a Added<'a>>);

impl Added<'_> {
    fn get(&self, c: &str) -> Option<&Value> {
        match self.0.iter().rposition(|(n, _)| n == c) {
            Some(k) => Some(&self.1[k]),
            None => self.2?.get(c),
        }
    }
}

impl Fields for Line<'_> {
    fn field(&self, c: &str) -> &Value {
        match self {
            Line::Doc(g, i, added) => match added.and_then(|a| a.get(c)) {
                Some(v) => v,
                None => g.col(c).map_or(&NULL, |v| &v[*i]),
            },
            Line::Own(r) => r.field(c),
        }
    }
}

/// A row while an EVAL runs: with the values it computed so far for the columns it names.
struct Pending<'r>(&'r dyn Fields, &'r [(String, Expr)], &'r [Value]);

impl Fields for Pending<'_> {
    fn field(&self, c: &str) -> &Value {
        match self.1.iter().rposition(|(n, _)| n == c) {
            Some(k) => &self.2[k],
            None => self.0.field(c),
        }
    }
}

/// The rows a source command yields, before any processing command.
struct Table {
    /// Each column's ES|QL type (`unsupported` for a type conflict).
    types: BTreeMap<String, String>,
    /// For each conflicting column, the indices per mapping type.
    conflicts: BTreeMap<String, BTreeMap<String, Vec<String>>>,
    /// One group of rows per index.
    groups: Vec<Group>,
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
) -> Result<Arc<Table>, EsqlError> {
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
    let filters = (targets.iter())
        .map(|t| t.filter.as_ref().map(crate::parse_query).transpose())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| EsqlError::Verification(e.msg))?;
    // A whole index's table, every column read, is kept until the index changes.
    // ponytail: holds every value of the index in memory until its next write; cache per
    // column, or bound it, if memory matters more than repeated queries
    if let [t] = &targets[..]
        && t.filter.is_none()
        && metadata.is_empty()
    {
        let index: &Index = indices[&t.index].borrow();
        return Ok(index.memo(|| table_of(indices, &targets, &filters, &[], &|_| true)));
    }
    Ok(Arc::new(table_of(
        indices, &targets, &filters, metadata, read,
    )))
}

/// The table of the documents of `targets` that their `filters` match.
fn table_of(
    indices: &aliases::Map<impl aliases::IndexMeta + std::borrow::Borrow<Index>>,
    targets: &[aliases::Target],
    filters: &[Option<crate::Query>],
    metadata: &[String],
    read: &dyn Fn(&str) -> bool,
) -> Table {
    // Per column: its ES|QL type, and the indices for each mapping type (named like ES|QL does).
    let mut mapped: BTreeMap<String, (String, BTreeMap<String, Vec<String>>)> = BTreeMap::new();
    let mut docs = Vec::new();
    for (t, filter) in targets.iter().zip(filters) {
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
        .zip(targets)
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
            let mut cols: BTreeMap<String, Vec<Value>> = (read.iter())
                .map(|&(c, ty, conflict, m, multi_field)| {
                    let v = g.iter().map(|(_, d)| match conflict {
                        true => Value::Null,
                        false => doc_value(cell(&d.source, c, multi_field), ty, m),
                    });
                    (c.clone(), v.collect())
                })
                .collect();
            for f in metadata {
                let v = g.iter().map(|(id, d)| match f.as_str() {
                    "_id" => Value::from(id.to_string()),
                    "_index" => Value::from(t.index.as_str()),
                    _ => Value::from(d.version),
                });
                cols.insert(f.clone(), v.collect());
            }
            Group::new(g.len(), cols)
        })
        .collect();
    // Metadata columns follow the fields, in the order listed.
    let mut cols: Vec<String> = types.keys().cloned().collect();
    for f in metadata {
        cols.retain(|c| c != f);
        cols.push(f.clone());
        types.insert(f.clone(), metadata_type(f).unwrap_or_default().into());
    }
    Table {
        cols,
        types,
        conflicts,
        groups,
        empty: targets.is_empty(),
        counted: true,
    }
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

/// A row to change: a document is copied with only the `kept` columns and the ones EVAL added.
fn owned<'r>(r: &'r mut Line, kept: &[&String]) -> &'r mut Row {
    if let Line::Doc(_, _, added) = *r {
        let mut own: Row = (kept.iter())
            .map(|&c| (c.clone(), r.field(c).clone()))
            .collect();
        // The oldest EVAL first, so the last value of a name wins.
        let mut evals = Vec::new();
        let mut a = added;
        while let Some(x) = a {
            evals.push(x);
            a = x.2;
        }
        for Added(named, vals, _) in evals.into_iter().rev() {
            let names = named.iter().map(|(n, _)| n.clone());
            own.extend(names.zip(vals.iter().cloned()));
        }
        *r = Line::Own(own);
    }
    let Line::Own(r) = r else { unreachable!() };
    r
}

fn sort_rows<R: Fields>(rows: &mut [R], keys: &[SortKey]) {
    rows.sort_by(|a, b| cmp_rows(a, b, keys));
}

/// `sort_rows` then `truncate(n)`, without sorting the rows past the first `n`.
fn top_rows<R: Fields>(rows: &mut Vec<R>, keys: &[SortKey], n: usize) {
    if n >= rows.len() {
        return sort_rows(rows, keys);
    }
    // Each row's sort values, looked up once rather than per comparison.
    let vals: Vec<&Value> = (rows.iter())
        .flat_map(|r| keys.iter().map(|k| r.field(&k.field)))
        .collect();
    let at = |i: usize| &vals[i * keys.len()..][..keys.len()];
    // Ties broken by position, as the stable sort does.
    let cmp = |&i: &usize, &j: &usize| {
        (keys.iter().zip(at(i).iter().zip(at(j))))
            .fold(Ordering::Equal, |o, (k, (x, y))| {
                o.then_with(|| cmp_values(x, y, k))
            })
            .then(i.cmp(&j))
    };
    // The best n so far, worst on top of a max-heap: most rows lose to it in one comparison.
    let mut heap = std::collections::BinaryHeap::with_capacity(n);
    for i in 0..rows.len() {
        if heap.len() < n {
            heap.push(Ranked(i, &cmp));
        } else if let Some(mut worst) = heap.peek_mut()
            && cmp(&i, &worst.0).is_lt()
        {
            worst.0 = i;
        }
    }
    let top: Vec<usize> = heap.into_sorted_vec().into_iter().map(|r| r.0).collect();
    let mut slots: Vec<Option<R>> = rows.drain(..).map(Some).collect();
    rows.extend(top.into_iter().filter_map(|i| slots[i].take()));
}

/// A row position ordered by a comparator, for a `BinaryHeap`.
struct Ranked<'c, C>(usize, &'c C);

impl<C: Fn(&usize, &usize) -> Ordering> Ord for Ranked<'_, C> {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.1)(&self.0, &other.0)
    }
}

impl<C: Fn(&usize, &usize) -> Ordering> PartialOrd for Ranked<'_, C> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<C: Fn(&usize, &usize) -> Ordering> PartialEq for Ranked<'_, C> {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl<C: Fn(&usize, &usize) -> Ordering> Eq for Ranked<'_, C> {}

fn cmp_rows(a: &dyn Fields, b: &dyn Fields, keys: &[SortKey]) -> Ordering {
    keys.iter().fold(Ordering::Equal, |acc, k| {
        acc.then_with(|| cmp_values(a.field(&k.field), b.field(&k.field), k))
    })
}

fn cmp_values(x: &Value, y: &Value, k: &SortKey) -> Ordering {
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
}

/// `documents_found` and `values_loaded` as Elasticsearch 9.5.5's plan counts them. Up to the
/// first LIMIT (implicitly 1000) the plan runs per shard: Lucene runs the filters on a field and a
/// literal and, with no other filter, the LIMIT or a non-`text` SORT (a top N per index); each
/// field is loaded only for the rows still alive when the plan first needs it. Everything after the
/// first LIMIT runs on its output, so the fields it uses load for those rows.
fn load_counts(
    groups: &[Group],
    cmds: &[Command],
    out: &[String],
    types: &BTreeMap<String, String>,
    at_cut: &[Line],
) -> (usize, usize) {
    // ponytail: the plan is cut at an EVAL, RENAME or STATS as if at a LIMIT, though ES pushes a
    // LIMIT through an EVAL or RENAME and a STATS reads every match rather than the first 1000
    let (head, tail) = cmds.split_at(plan_cut(cmds));
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
    let passes = |r: &Line, es: &[&Expr]| es.iter().all(|e| truth(e, r) == Some(true));
    let values = |rows: &[Line], fields: &BTreeSet<String>| -> usize {
        let count = |v: &Value| match v {
            Value::Null => 0,
            Value::Array(a) => a.len(),
            _ => 1,
        };
        rows.iter()
            .flat_map(|r| fields.iter().map(|f| count(r.field(f))))
            .sum()
    };
    // The query's own rows at the cut are this top N when it has one index, one SORT before the
    // cut (a second one would have broken the ties) and the cut is a LIMIT or the end.
    let reuse = groups.len() == 1
        && matches!(tail.first(), None | Some(Command::Limit(_)))
        && head
            .iter()
            .filter(|c| matches!(c, Command::Sort(_)))
            .count()
            == 1;
    let found = (groups.iter()).map(|g| g.lines().filter(|r| passes(r, &pushed)));
    if residual.is_empty() && keys.iter().all(|k| types[k] != "text") {
        let emitted: Vec<Line> = match sort.is_empty() {
            true => found.flatten().take(limit).collect(),
            false if reuse => at_cut.iter().take(limit).cloned().collect(),
            false => found
                .flat_map(|g| {
                    let mut g: Vec<Line> = g.collect();
                    top_rows(&mut g, sort, limit);
                    g
                })
                .collect(),
        };
        return (emitted.len(), values(&emitted, &(&keys | &later)));
    }
    // ponytail: reads every match; ES's source can stop early once a plain LIMIT fills
    let mut rows: Vec<Line> = found.flatten().collect();
    let documents = rows.len();
    let mut loaded: BTreeSet<String> = residual.iter().flat_map(|e| names(e)).collect();
    let mut n = values(&rows, &loaded);
    rows.retain(|r| passes(r, &residual));
    n += values(&rows, &(&keys - &loaded));
    loaded.extend(keys);
    top_rows(&mut rows, sort, limit);
    n += values(&rows, &(&later - &loaded));
    (documents, n)
}

/// Where `load_counts` cuts the plan: at the first LIMIT, EVAL, RENAME or STATS.
fn plan_cut(cmds: &[Command]) -> usize {
    (cmds.iter())
        .position(|c| {
            matches!(
                c,
                Command::Limit(_) | Command::Eval(_) | Command::Rename(_) | Command::Stats(..)
            )
        })
        .unwrap_or(cmds.len())
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
        Expr::Cmp(_, a, b, ..) => {
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

    /// An index's table is reused between queries, but never past a write or mapping change.
    #[test]
    fn esql_sees_every_write_and_mapping_change() {
        let values = |indices: &BTreeMap<String, Index>, q: &str| run(indices, q).unwrap().values;
        let mut indices = BTreeMap::from([("i".to_owned(), Index::default())]);
        let i = indices.get_mut("i").unwrap();
        i.map_dynamic("1", &serde_json::json!({"n": 1})).unwrap();
        i.put("1", serde_json::json!({"n": 1}));
        let q = "FROM i | STATS c = COUNT(*), s = SUM(n)";
        assert_eq!(values(&indices, q), [[Value::from(1), 1.into()]]);
        indices
            .get_mut("i")
            .unwrap()
            .put("2", serde_json::json!({"n": 2}));
        assert_eq!(values(&indices, q), [[Value::from(2), 3.into()]]);
        indices.get_mut("i").unwrap().delete("1");
        assert_eq!(values(&indices, q), [[Value::from(1), 2.into()]]);
        let i = indices.get_mut("i").unwrap();
        i.map_dynamic("3", &serde_json::json!({"m": "x"})).unwrap();
        assert_eq!(values(&indices, "FROM i | KEEP m"), [[Value::Null]]);
    }

    /// Equal STATS keys hash alike, so they share a group.
    #[test]
    fn after_bench_equal_group_keys_hash_alike() {
        let pairs = [
            (serde_json::json!(0.0), serde_json::json!(-0.0)),
            (
                serde_json::json!({"a": 1, "b": 2}),
                serde_json::json!({"b": 2, "a": 1}),
            ),
        ];
        for (a, b) in pairs {
            assert_eq!(a, b);
            assert_eq!(key_hash([&a].into_iter()), key_hash([&b].into_iter()));
        }
    }

    /// A column lookup finds the column whatever the hint left by the lookup before it.
    #[test]
    fn group_col_finds_each_column_after_any_other() {
        let names = ["a", "b", "c"];
        let cols = names.map(|n| (n.to_owned(), vec![Value::from(n)]));
        let g = Group::new(1, BTreeMap::from(cols));
        for n in ["c", "c", "a", "x", "b", "a", "c"] {
            let found = names.contains(&n).then(|| vec![Value::from(n)]);
            assert_eq!(g.col(n).map(<[Value]>::to_vec), found);
        }
    }

    /// A top N keeps the rows a stable sort and a truncation keep, ties in input order.
    #[test]
    fn top_rows_is_a_stable_sort_truncated() {
        let rows: Vec<Row> = [3, 1, 2, 1, 3, 2, 1, 3]
            .iter()
            .enumerate()
            .map(|(i, k)| {
                let m = if i % 3 == 0 {
                    Value::Null
                } else {
                    (i % 2).into()
                };
                Row::from([
                    ("k".into(), (*k).into()),
                    ("m".into(), m),
                    ("i".into(), i.into()),
                ])
            })
            .collect();
        let key = |field: &str, desc| SortKey {
            field: field.into(),
            at: (1, 1),
            desc,
            nulls_first: false,
        };
        for keys in [vec![key("k", false)], vec![key("k", false), key("m", true)]] {
            let mut sorted = rows.clone();
            sort_rows(&mut sorted, &keys);
            for n in 0..=rows.len() + 1 {
                let mut top = rows.clone();
                top_rows(&mut top, &keys, n);
                assert_eq!(top, sorted[..n.min(rows.len())]);
            }
        }
    }
}
