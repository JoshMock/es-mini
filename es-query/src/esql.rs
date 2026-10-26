//! ES|QL subset: `FROM` (with `METADATA _id, _index, _version`), `ROW` and `SHOW INFO`, with
//! `WHERE`, `EVAL`, `LIMIT`, `KEEP`, `DROP`, `SORT`, and `STATS COUNT(*) [BY ...]`.
//!
//! Expressions support comparisons (`== != < <= > >=`), `AND`/`OR`/`NOT`, parentheses,
//! `IS [NOT] NULL`, arithmetic, and the functions in `CONVERSIONS` and `SCALAR_FNS` over field
//! names and literals. Any other syntax is a parse error; any other function fails verification.

use es_index::{
    FieldMapping, Index, Mappings,
    aliases::{self, ResolveError},
};
use serde::Serialize;
use serde_json::Value;
use std::{
    borrow::{Borrow, Cow},
    cell::OnceCell,
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
    let mut out = Vec::new();
    let mut it = src.char_indices().peekable();
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
        } else if c.is_alphabetic() || "_@*".contains(c) {
            let mut s = String::new();
            while let Some(&(_, c)) = it
                .peek()
                .filter(|(_, c)| c.is_alphanumeric() || "_@*.".contains(*c))
            {
                s.push(c);
                it.next();
            }
            Tok::Word(s)
        } else {
            let rest = &src[i..];
            let sym = [
                "==", "!=", "<=", ">=", "<", ">", "=", "+", "-", "/", "%", "|", "(", ")", ",", "[",
                "]",
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
const AFTER_IS_NULL: &str = "{<EOF>, '|', 'and', 'or'}";
const AFTER_SORT_FIELD: &str = "{<EOF>, '|', 'and', 'asc', '::', ',', 'desc', '.', 'nulls', \
    'or', '+', '-', '*', '/', '%'}";
const AFTER_SORT_ORDER: &str = "{<EOF>, '|', ',', 'nulls'}";
const AFTER_SORT_NULLS: &str = "{<EOF>, '|', ','}";
// ponytail: not checked against Elasticsearch (`LIMIT 1 x` lexes as a time span there).
const AFTER_LIMIT: &str = "{<EOF>, '|'}";

#[derive(Debug, Clone, PartialEq)]
enum Expr {
    Field(String, Span),
    Lit(Value, Span),
    Cmp(&'static str, Box<Expr>, Box<Expr>, Span),
    And(Box<Expr>, Box<Expr>, Span),
    Or(Box<Expr>, Box<Expr>, Span),
    Not(Box<Expr>, Span),
    IsNull(Box<Expr>, bool, Span),
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
    /// Index expressions, and the `METADATA` fields to add as columns.
    From(Vec<String>, Vec<String>),
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
    Sort(Vec<SortKey>),
    /// Aggregates (only `COUNT(*)`), then the named grouping expressions.
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
        let start = self.toks[self.operand].2.start;
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
        self.pos += 1;
        self.follow = AFTER_LIMIT;
        if let Some(n) = n.as_u64().filter(|&n| !negative && n <= i32::MAX as u64) {
            return Ok(Command::Limit(n as usize));
        }
        let r#type = number_type(&n, negative);
        let value = &self.src[(if negative {
            self.toks[self.pos - 2].2.start
        } else {
            lit.start
        })..lit.end];
        Err(parse_error(
            at,
            &format!(
                "value of [{}] must be a non negative integer, found value [{value}] type [{type}]",
                &self.src[start..lit.end]
            ),
        ))
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
            return Ok(Source::Row(self.assignments()?));
        }
        if !self.keyword("from") {
            return self.unsupported(SOURCE_COMMANDS);
        }
        let sources = self.ident_list(INDEX_PATTERN)?;
        self.follow = AFTER_SOURCES;
        let mut metadata = Vec::new();
        if self.keyword("metadata") {
            for (f, at) in self.ident_list(ID_PATTERN)? {
                if metadata_type(&f).is_none() {
                    // ponytail: message not checked against Elasticsearch
                    let msg = format!("unsupported metadata field [{f}]");
                    return Err(parse_error(at, &msg));
                }
                metadata.push(f);
            }
            self.follow = AFTER_ID_PATTERNS;
        }
        let sources = sources.into_iter().map(|(s, _)| s).collect();
        Ok(Source::From(sources, metadata))
    }

    /// `name = expression, ...`, where an unnamed expression is named by its text.
    fn assignments(&mut self) -> Result<Vec<(String, Expr)>, EsqlError> {
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
            if !["where", "eval", "limit", "keep", "drop", "sort", "stats"]
                .contains(&command.as_str())
            {
                return self.unsupported(PROCESSING_COMMANDS);
            }
            self.pos += 1;
            let cmd = match command.as_str() {
                "where" => Command::Where(self.or()?),
                "eval" => Command::Eval(self.assignments()?),
                "stats" => {
                    let aggs = self.assignments()?;
                    let by = match self.keyword("by") {
                        true => self.assignments()?,
                        false => Vec::new(),
                    };
                    Command::Stats(aggs, by)
                }
                "limit" => self.limit()?,
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
            // ponytail: `*` lexes as a pattern word, so `a*2` without spaces is not a product
            let op = match self.peek() {
                Some(Tok::Sym(s)) => *s,
                Some(Tok::Word(w)) if w == "*" => "*",
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
            self.follow = AFTER_PARENS;
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
            Tok::Num(n) if negative => {
                let n = match n.as_i64() {
                    Some(i) => Value::from(-i),
                    None => Value::from(-n.as_f64().unwrap_or_default()),
                };
                (Expr::Lit(n, span), AFTER_LITERAL)
            }
            Tok::Num(n) => (Expr::Lit(n, span), AFTER_LITERAL),
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
                if !self.sym(")") {
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
                // ponytail: not checked against Elasticsearch's follow set after a call
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
fn columns(index: &Index) -> BTreeMap<String, (String, String)> {
    let mut dynamic: BTreeMap<String, String> = BTreeMap::new();
    for (_, d) in index.docs() {
        flatten_types("", &d.source, &mut dynamic);
    }
    let mut cols: BTreeMap<_, _> = dynamic
        .into_iter()
        .map(|(c, t)| (c, (t.clone(), t)))
        .collect();
    for (name, m) in &index.mappings.properties {
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

fn flatten_types(prefix: &str, v: &Value, cols: &mut BTreeMap<String, String>) {
    let t = match v {
        Value::Object(o) => {
            for (k, v) in o {
                let key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten_types(&key, v, cols);
            }
            return;
        }
        Value::Array(a) => return a.iter().for_each(|x| flatten_types(prefix, x, cols)),
        Value::Null => return,
        Value::String(_) => {
            cols.entry(format!("{prefix}.keyword"))
                .or_insert_with(|| "keyword".into());
            "text"
        }
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_f64() => "double",
        Value::Number(_) => "long",
    };
    cols.entry(prefix.to_owned())
        .or_insert_with(|| t.to_owned());
}

/// The value of a column in one document; multi-values become arrays. A multi-field (or the
/// `.keyword` of an unmapped string) reads its parent's source value.
fn cell(source: &Value, column: &str, mappings: &Mappings) -> Value {
    let mut vals = Vec::new();
    collect(source, column, &mut vals);
    let multi_field = column.ends_with(".keyword")
        || !mappings.properties.contains_key(column) && mappings.field(column).is_some();
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
        Expr::Field(f, _) => row.get(f.as_str()).cloned().filter(|v| !v.is_null()),
        Expr::IsNull(x, negated, _) => Some(Value::Bool(eval(x, row).is_none() != *negated)),
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
            if SCALAR_FNS.iter().any(|(n, _)| *n == f) {
                let args: Vec<Value> = args.iter().map(|a| eval(a, row)).collect::<Option<_>>()?;
                return match f {
                    "SUBSTRING" | "TRIM" => string_fn(f, &args),
                    "MV_COUNT" | "MV_MIN" | "MV_MAX" => mv_fn(f, &args[0]),
                    // ponytail: read per row; ES fixes `now` once per query
                    "NOW" => Some(es_index::iso_date(crate::dates::now_millis()).into()),
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
const SCALAR_FNS: [(&str, RangeInclusive<usize>); 11] = [
    ("NOW", 0..=0),
    ("SUBSTRING", 2..=3),
    ("TRIM", 1..=1),
    ("ABS", 1..=1),
    ("CEIL", 1..=1),
    ("FLOOR", 1..=1),
    ("POW", 2..=2),
    ("ROUND", 1..=2),
    ("MV_COUNT", 1..=1),
    ("MV_MIN", 1..=1),
    ("MV_MAX", 1..=1),
];

/// What argument `i` of scalar function `f` must be, in Elasticsearch's words, and whether type
/// `t` is that (null always is).
fn accepts(f: &str, i: usize, t: &str) -> (&'static str, bool) {
    let (accepted, ok) = match (f, i) {
        ("SUBSTRING" | "TRIM", 0) => ("string", t == "keyword" || t == "text"),
        ("SUBSTRING", _) => ("integer", t == "integer"),
        // ponytail: spatial types are not refused
        ("MV_COUNT" | "MV_MIN" | "MV_MAX", _) => ("any", true),
        ("ROUND", 1) => (
            "whole number except unsigned_long or counter types",
            t == "integer" || t == "long",
        ),
        _ => ("numeric", NUMERIC.contains(&t)),
    };
    (accepted, ok || t == "null")
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

/// Java's `Character.isWhitespace`.
fn java_whitespace(c: char) -> bool {
    matches!(c, '\u{1c}'..='\u{1f}')
        || c.is_whitespace() && !matches!(c, '\u{85}' | '\u{a0}' | '\u{2007}' | '\u{202f}')
}

/// String function `f` (`SUBSTRING` or `TRIM`) of its arguments, like Elasticsearch's
/// evaluators: `None` for a multi-valued argument (where ES warns).
fn string_fn(f: &str, args: &[Value]) -> Option<Value> {
    let s = args[0].as_str()?;
    if f == "TRIM" {
        return Some(s.trim_matches(java_whitespace).into());
    }
    // SUBSTRING counts code points from 1; 0 counts as 1 and a negative start from the end.
    let n = s.chars().count() as i64;
    let start = args[1].as_i64()?;
    let len = match args.get(2) {
        Some(l) => l.as_i64()?,
        None => n,
    };
    // ponytail: a negative length fails the query in ES; null here
    if len < 0 {
        return None;
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
// ponytail: no hexadecimal floats
fn parse_double(s: &str) -> Option<f64> {
    let s = s.trim_matches(|c| c <= ' ');
    let s = s.strip_suffix(['d', 'D', 'f', 'F']).unwrap_or(s);
    let x: f64 = s.parse().ok()?;
    x.is_finite().then_some(x)
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

fn fields(e: &Expr, out: &mut Vec<Name>) {
    match e {
        Expr::Call(_, args, ..) => args.iter().for_each(|a| fields(a, out)),
        Expr::Field(f, (at, _)) => out.push((f.clone(), *at)),
        Expr::Lit(..) => {}
        Expr::Not(x, _) | Expr::IsNull(x, ..) | Expr::Neg(x, ..) => fields(x, out),
        Expr::Cmp(_, a, b, _)
        | Expr::And(a, b, _)
        | Expr::Or(a, b, _)
        | Expr::Arith(_, a, b, ..) => {
            fields(a, out);
            fields(b, out);
        }
    }
}

fn names(e: &Expr) -> Vec<String> {
    let mut out = Vec::new();
    fields(e, &mut out);
    out.into_iter().map(|(f, _)| f).collect()
}

fn span(e: &Expr) -> &Span {
    match e {
        Expr::Field(_, s) | Expr::Lit(_, s) | Expr::Not(_, s) | Expr::IsNull(.., s) => s,
        Expr::Cmp(.., s) | Expr::And(.., s) | Expr::Or(.., s) | Expr::Arith(.., s) => s,
        Expr::Call(.., s) | Expr::Neg(.., s) => s,
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
            if let Some((name, arity)) = SCALAR_FNS.iter().find(|(n, _)| *n == upper) {
                // ponytail: arity errors are not ES's messages
                if !arity.contains(&args.len()) {
                    bad.push((*at, format!("[{f}] takes {arity:?} arguments")));
                    return None;
                }
                let ts: Vec<_> = args.iter().map(|a| resolve(a, src, types, bad)).collect();
                let ts: Vec<String> = ts.into_iter().collect::<Option<_>>()?;
                for (i, (a, ta)) in args.iter().zip(&ts).enumerate() {
                    let (accepted, ok) = accepts(name, i, ta);
                    if !ok {
                        let nth = match (*name, i) {
                            ("TRIM" | "ABS" | "CEIL" | "FLOOR", _) => "",
                            (_, 0) => "first ",
                            (_, 1) => "second ",
                            _ => "third ",
                        };
                        bad.push((
                            *at,
                            format!(
                                "{nth}argument of [{}] must be [{accepted}], found value [{}] \
                                 type [{ta}]",
                                text(e),
                                text(a)
                            ),
                        ));
                        return None;
                    }
                }
                // The result type: a string, an integer, a double, or the first argument's.
                let out = match *name {
                    "SUBSTRING" => "keyword",
                    "TRIM" if ts[0] != "null" => "keyword",
                    "MV_COUNT" => "integer",
                    "NOW" => "date",
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
            // ponytail: single-argument conversions only; arity errors are not ES's messages
            let [arg] = &args[..] else {
                bad.push((*at, format!("[{f}] takes exactly one argument")));
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

/// Whether `e` is `COUNT(*)`.
fn count_star(e: &Expr) -> bool {
    matches!(e, Expr::Call(f, args, ..)
        if f.eq_ignore_ascii_case("count") && matches!(&args[..], [Expr::Field(s, _)] if s == "*"))
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
    let (toks, eof) = lex(query)?;
    let (source, cmds) = Parser {
        src: query,
        toks,
        eof,
        pos: 0,
        operand: 0,
        parens: 0,
        follow: "<EOF>",
    }
    .query()?;
    let (source, cmds) = match source {
        // A row is an EVAL over one empty row.
        Source::Row(fields) => (None, [vec![Command::Eval(fields)], cmds].concat()),
        Source::From(sources, metadata) => (Some((sources, metadata)), cmds),
    };
    let Table {
        mut types,
        mut conflicts,
        groups,
        mut cols,
        empty,
        counted,
    } = match source {
        Some((sources, metadata)) => table_from(indices, &sources, &metadata)?,
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
            Command::Limit(_) => Vec::new(),
            Command::Stats(aggs, by) => {
                // ponytail: COUNT(*) is the only aggregate; the esql-stats goal adds the rest
                let mut bad: Vec<_> = aggs
                    .iter()
                    .filter(|(_, e)| !count_star(e))
                    .map(|(_, e)| {
                        let msg = format!(
                            "[{}] is not supported by es-mini in STATS",
                            &query[span(e).1.clone()]
                        );
                        (span(e).0, msg)
                    })
                    .collect();
                let mut grouped = Vec::new();
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
                        grouped.push((name.clone(), t));
                    }
                }
                if bad.is_empty() {
                    let counts = aggs.iter().map(|(n, _)| (n.clone(), "long".to_owned()));
                    cols.clear();
                    for (name, t) in counts.chain(grouped) {
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
            Command::Stats(aggs, by) => {
                // ponytail: linear group lookup, O(rows × groups), and a multi-valued key is one
                // group rather than one per value; hash and expand keys when that matters
                let mut buckets: Vec<(Vec<Value>, u64)> = Vec::new();
                for r in &rows {
                    let key: Vec<Value> = by
                        .iter()
                        .map(|(_, e)| eval(e, r).unwrap_or(Value::Null))
                        .collect();
                    match buckets.iter_mut().find(|(k, _)| *k == key) {
                        Some(b) => b.1 += 1,
                        None => buckets.push((key, 1)),
                    }
                }
                if by.is_empty() && buckets.is_empty() {
                    buckets.push((Vec::new(), 0));
                }
                rows = buckets
                    .into_iter()
                    .map(|(key, n)| {
                        let mut r: Row = aggs.iter().map(|(a, _)| (a.clone(), n.into())).collect();
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
        for (c, (es_type, ty)) in columns(index) {
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
        let mut mine: Vec<_> = index
            .docs()
            .filter(|(id, d)| {
                searcher
                    .as_ref()
                    .is_none_or(|(s, f)| s.score(f, id, d).is_some())
            })
            .collect();
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
            g.iter()
                .map(|(id, d)| {
                    let mut row: Row = types
                        .keys()
                        .map(|c| {
                            let v = match conflicts.contains_key(c) {
                                true => Value::Null,
                                false => cell(&d.source, c, mappings),
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
    // ponytail: the plan is cut at an EVAL or STATS as if at a LIMIT, though ES pushes a LIMIT
    // through an EVAL and a STATS reads every match rather than the first 1000
    let (head, tail) = cmds.split_at(
        cmds.iter()
            .position(|c| matches!(c, Command::Limit(_) | Command::Eval(_) | Command::Stats(..)))
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
    let mut later: Vec<String> = out.to_vec();
    for c in tail {
        match c {
            Command::Where(e) => later.extend(names(e)),
            Command::Eval(named) => later.extend(named.iter().flat_map(|(_, e)| names(e))),
            Command::Sort(keys) => later.extend(keys.iter().map(|k| k.field.clone())),
            Command::Stats(_, by) => later.extend(by.iter().flat_map(|(_, e)| names(e))),
            _ => {}
        }
    }
    let keys: BTreeSet<String> = sort.iter().map(|k| k.field.clone()).collect();
    let later: BTreeSet<String> = later.into_iter().collect();
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
        Expr::Not(x, _) | Expr::IsNull(x, ..) => pushable(x),
        Expr::And(a, b, _) | Expr::Or(a, b, _) => pushable(a) && pushable(b),
        Expr::Field(..) | Expr::Lit(..) => true,
    }
}
