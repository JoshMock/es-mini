//! ES|QL subset: `FROM` with `WHERE`, `LIMIT`, `KEEP`, `DROP`, and `SORT`.
//!
//! WHERE supports comparisons (`== != < <= > >=`), `AND`/`OR`/`NOT`, parentheses, and
//! `IS [NOT] NULL` over field names and literals. Anything else is a parse error.

use es_index::{
    FieldMapping, Index,
    aliases::{self, ResolveError},
};
use serde::Serialize;
use serde_json::Value;
use std::{cmp::Ordering, collections::BTreeMap};

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
    /// A source names a closed index (`index_closed_exception`).
    IndexClosed(String),
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Column {
    pub name: String,
    pub r#type: String,
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

fn lex(src: &str) -> Result<Vec<Tok>, EsqlError> {
    let err = |m: String| EsqlError::Parse(m);
    let mut out = Vec::new();
    let mut it = src.char_indices().peekable();
    while let Some(&(i, c)) = it.peek() {
        if c.is_whitespace() {
            it.next();
        } else if c == '"' {
            it.next();
            let mut s = String::new();
            loop {
                match it.next() {
                    None => return Err(err(format!("unterminated string at {i}"))),
                    Some((_, '"')) => break,
                    Some((_, '\\')) => match it.next() {
                        Some((_, 'n')) => s.push('\n'),
                        Some((_, 't')) => s.push('\t'),
                        Some((_, c)) => s.push(c),
                        None => return Err(err(format!("unterminated string at {i}"))),
                    },
                    Some((_, c)) => s.push(c),
                }
            }
            out.push(Tok::Str(s));
        } else if c == '`' {
            it.next();
            let mut s = String::new();
            loop {
                match it.next() {
                    None => return Err(err(format!("unterminated quoted identifier at {i}"))),
                    Some((_, '`')) => break,
                    Some((_, c)) => s.push(c),
                }
            }
            out.push(Tok::Word(s));
        } else if c.is_ascii_digit() {
            let mut s = String::new();
            while let Some(&(_, c)) = it.peek().filter(|(_, c)| c.is_ascii_digit() || *c == '.') {
                s.push(c);
                it.next();
            }
            let n = match s.contains('.') {
                true => s.parse::<f64>().ok().map(Value::from),
                false => s.parse::<i64>().ok().map(Value::from),
            };
            out.push(Tok::Num(
                n.ok_or_else(|| err(format!("invalid number [{s}]")))?,
            ));
        } else if c.is_alphabetic() || "_@*".contains(c) {
            let mut s = String::new();
            while let Some(&(_, c)) = it
                .peek()
                .filter(|(_, c)| c.is_alphanumeric() || "_@*.".contains(*c))
            {
                s.push(c);
                it.next();
            }
            out.push(Tok::Word(s));
        } else {
            let rest = &src[i..];
            let sym = ["==", "!=", "<=", ">=", "<", ">", "|", "(", ")", ","]
                .into_iter()
                .find(|s| rest.starts_with(s))
                .ok_or_else(|| err(format!("unexpected character [{c}] at {i}")))?;
            sym.chars().for_each(|_| {
                it.next();
            });
            out.push(Tok::Sym(sym));
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, PartialEq)]
enum Expr {
    Field(String),
    Lit(Value),
    Cmp(&'static str, Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    IsNull(Box<Expr>, bool),
}

#[derive(Debug, Clone, PartialEq)]
struct SortKey {
    field: String,
    desc: bool,
    nulls_first: bool,
}

#[derive(Debug, Clone, PartialEq)]
enum Command {
    Where(Expr),
    Limit(usize),
    Keep(Vec<String>),
    Drop(Vec<String>),
    Sort(Vec<SortKey>),
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn next(&mut self) -> Option<Tok> {
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

    fn fail<T>(&self, what: &str) -> Result<T, EsqlError> {
        let found = self
            .peek()
            .map_or("end of input".to_owned(), |t| format!("{t:?}"));
        Err(EsqlError::Parse(format!("expected {what}, found {found}")))
    }

    fn ident(&mut self) -> Result<String, EsqlError> {
        match self.peek() {
            Some(Tok::Word(w)) => {
                let w = w.clone();
                self.pos += 1;
                Ok(w)
            }
            _ => self.fail("an identifier"),
        }
    }

    fn ident_list(&mut self) -> Result<Vec<String>, EsqlError> {
        let mut out = vec![self.ident()?];
        while self.sym(",") {
            out.push(self.ident()?);
        }
        Ok(out)
    }

    fn query(&mut self) -> Result<(Vec<String>, Vec<Command>), EsqlError> {
        if !self.keyword("from") {
            return self.fail("FROM");
        }
        let sources = self.ident_list()?;
        let mut cmds = Vec::new();
        while self.sym("|") {
            let cmd = match self.ident()?.to_ascii_lowercase().as_str() {
                "where" => Command::Where(self.or()?),
                "limit" => match self.next() {
                    Some(Tok::Num(n)) if n.is_u64() => {
                        Command::Limit(n.as_u64().unwrap_or_default() as usize)
                    }
                    _ => return self.fail("an integer"),
                },
                "keep" => Command::Keep(self.ident_list()?),
                "drop" => Command::Drop(self.ident_list()?),
                "sort" => {
                    let mut keys = Vec::new();
                    loop {
                        let field = self.ident()?;
                        let desc = self.keyword("desc");
                        if !desc {
                            self.keyword("asc");
                        }
                        // Nulls are larger than any value by default.
                        let mut nulls_first = desc;
                        if self.keyword("nulls") {
                            nulls_first = match () {
                                _ if self.keyword("first") => true,
                                _ if self.keyword("last") => false,
                                _ => return self.fail("FIRST or LAST"),
                            };
                        }
                        keys.push(SortKey {
                            field,
                            desc,
                            nulls_first,
                        });
                        if !self.sym(",") {
                            break;
                        }
                    }
                    Command::Sort(keys)
                }
                other => return Err(EsqlError::Parse(format!("unsupported command [{other}]"))),
            };
            cmds.push(cmd);
        }
        match self.peek() {
            None => Ok((sources, cmds)),
            Some(_) => self.fail("| or end of input"),
        }
    }

    fn or(&mut self) -> Result<Expr, EsqlError> {
        let mut e = self.and()?;
        while self.keyword("or") {
            e = Expr::Or(Box::new(e), Box::new(self.and()?));
        }
        Ok(e)
    }

    fn and(&mut self) -> Result<Expr, EsqlError> {
        let mut e = self.not()?;
        while self.keyword("and") {
            e = Expr::And(Box::new(e), Box::new(self.not()?));
        }
        Ok(e)
    }

    fn not(&mut self) -> Result<Expr, EsqlError> {
        match self.keyword("not") {
            true => Ok(Expr::Not(Box::new(self.not()?))),
            false => self.cmp(),
        }
    }

    fn cmp(&mut self) -> Result<Expr, EsqlError> {
        let left = self.primary()?;
        if self.keyword("is") {
            let negated = self.keyword("not");
            if !self.keyword("null") {
                return self.fail("NULL");
            }
            return Ok(Expr::IsNull(Box::new(left), negated));
        }
        match self.peek() {
            Some(Tok::Sym(op @ ("==" | "!=" | "<" | "<=" | ">" | ">="))) => {
                let op = *op;
                self.pos += 1;
                Ok(Expr::Cmp(op, Box::new(left), Box::new(self.primary()?)))
            }
            _ => Ok(left),
        }
    }

    fn primary(&mut self) -> Result<Expr, EsqlError> {
        if self.sym("(") {
            let e = self.or()?;
            return match self.sym(")") {
                true => Ok(e),
                false => self.fail(")"),
            };
        }
        match self.next() {
            Some(Tok::Num(n)) => Ok(Expr::Lit(n)),
            Some(Tok::Str(s)) => Ok(Expr::Lit(Value::String(s))),
            Some(Tok::Word(w)) if w.eq_ignore_ascii_case("null") => Ok(Expr::Lit(Value::Null)),
            Some(Tok::Word(w)) if w.eq_ignore_ascii_case("true") => Ok(Expr::Lit(true.into())),
            Some(Tok::Word(w)) if w.eq_ignore_ascii_case("false") => Ok(Expr::Lit(false.into())),
            Some(Tok::Word(w)) => Ok(Expr::Field(w)),
            _ => {
                self.pos -= 1;
                self.fail("an expression")
            }
        }
    }
}

/// Columns of an index: mapped fields plus unmapped source fields, typed like ES dynamic mapping.
fn columns(index: &Index) -> BTreeMap<String, String> {
    let mut cols: BTreeMap<String, String> = BTreeMap::new();
    for (_, d) in index.docs() {
        flatten_types("", &d.source, &mut cols);
    }
    for (name, m) in &index.mappings.properties {
        let t = match m {
            FieldMapping::Text | FieldMapping::TextKeyword { .. } => "text",
            FieldMapping::Keyword => "keyword",
            FieldMapping::Long => "long",
            FieldMapping::Integer | FieldMapping::Short | FieldMapping::Byte => "integer",
            FieldMapping::Double | FieldMapping::Float => "double",
            FieldMapping::Boolean => "boolean",
            FieldMapping::Date { .. } => "date",
        };
        cols.insert(name.clone(), t.to_owned());
        cols.remove(&format!("{name}.keyword"));
        // ponytail: dynamic strings only ever get a keyword multi-field
        if let FieldMapping::TextKeyword { fields } = m {
            for sub in fields.keys() {
                cols.insert(format!("{name}.{sub}"), "keyword".to_owned());
            }
        }
    }
    cols
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

/// The value of a (possibly `.keyword`) column in one document; multi-values become arrays.
fn cell(source: &Value, column: &str) -> Value {
    let mut vals = Vec::new();
    collect(source, column, &mut vals);
    if vals.is_empty()
        && let Some(base) = column.strip_suffix(".keyword")
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
fn eval(e: &Expr, row: &BTreeMap<&str, Value>) -> Option<Value> {
    let truth = |e: &Expr| eval(e, row).and_then(|v| v.as_bool());
    match e {
        Expr::Lit(v) => Some(v.clone()).filter(|v| !v.is_null()),
        Expr::Field(f) => row.get(f.as_str()).cloned().filter(|v| !v.is_null()),
        Expr::IsNull(x, negated) => Some(Value::Bool(eval(x, row).is_none() != *negated)),
        Expr::Not(x) => truth(x).map(|b| Value::Bool(!b)),
        Expr::And(a, b) => match (truth(a), truth(b)) {
            (Some(false), _) | (_, Some(false)) => Some(false.into()),
            (Some(true), Some(true)) => Some(true.into()),
            _ => None,
        },
        Expr::Or(a, b) => match (truth(a), truth(b)) {
            (Some(true), _) | (_, Some(true)) => Some(true.into()),
            (Some(false), Some(false)) => Some(false.into()),
            _ => None,
        },
        Expr::Cmp(op, a, b) => {
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

fn fields(e: &Expr, out: &mut Vec<String>) {
    match e {
        Expr::Field(f) => out.push(f.clone()),
        Expr::Lit(_) => {}
        Expr::Not(x) | Expr::IsNull(x, _) => fields(x, out),
        Expr::Cmp(_, a, b) | Expr::And(a, b) | Expr::Or(a, b) => {
            fields(a, out);
            fields(b, out);
        }
    }
}

fn glob(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((pre, rest)) => {
            name.starts_with(pre)
                && (0..=name.len() - pre.len()).any(|i| {
                    name.is_char_boundary(pre.len() + i) && glob(rest, &name[pre.len() + i..])
                })
        }
    }
}

fn unknown(c: &str) -> EsqlError {
    EsqlError::Verification(format!("Unknown column [{c}]"))
}

pub fn run(indices: &BTreeMap<String, Index>, query: &str) -> Result<Response, EsqlError> {
    let (sources, cmds) = Parser {
        toks: lex(query)?,
        pos: 0,
    }
    .query()?;
    let unknown_index = |e| match e {
        ResolveError::IndexClosed(i) => EsqlError::IndexClosed(i),
        ResolveError::IndexNotFound(i)
        | ResolveError::NoWriteIndex(i)
        | ResolveError::InvalidIndexName(i) => {
            EsqlError::Verification(format!("Unknown index [{i}]"))
        }
    };
    let targets = aliases::resolve_data(indices, &sources.join(",")).map_err(unknown_index)?;
    if targets.is_empty() {
        return Err(EsqlError::Verification(format!(
            "Unknown index [{}]",
            sources.join(",")
        )));
    }
    // ponytail: conflicting types across indices keep the first index's type; ES reports `unsupported`
    let mut types = BTreeMap::new();
    let mut docs = Vec::new();
    for t in &targets {
        let index = &indices[&t.index];
        for (c, ty) in columns(index) {
            types.entry(c).or_insert(ty);
        }
        let filter = t
            .filter
            .as_ref()
            .map(crate::parse_query)
            .transpose()
            .map_err(EsqlError::Verification)?;
        let searcher = filter.as_ref().map(|f| (crate::Searcher::new(index, f), f));
        let mut mine: Vec<_> = index
            .docs()
            .filter(|(id, d)| {
                searcher
                    .as_ref()
                    .is_none_or(|(s, f)| s.score(f, id, d).is_some())
            })
            .map(|(_, d)| d)
            .collect();
        mine.sort_by_key(|d| d.seq_no);
        docs.extend(mine);
    }
    let mut cols: Vec<String> = types.keys().cloned().collect();
    let mut rows: Vec<BTreeMap<&str, Value>> = docs
        .iter()
        .map(|d| {
            types
                .keys()
                .map(|c| (c.as_str(), cell(&d.source, c)))
                .collect()
        })
        .collect();
    let mut explicit_limit = false;

    for cmd in cmds {
        match cmd {
            Command::Where(e) => {
                let mut refs = Vec::new();
                fields(&e, &mut refs);
                if let Some(f) = refs.iter().find(|f| !cols.contains(f)) {
                    return Err(unknown(f));
                }
                rows.retain(|r| eval(&e, r) == Some(Value::Bool(true)));
            }
            Command::Limit(n) => {
                rows.truncate(n.min(MAX_LIMIT));
                explicit_limit = true;
            }
            Command::Keep(patterns) => {
                let mut kept = Vec::new();
                for p in &patterns {
                    let hits: Vec<_> = cols
                        .iter()
                        .filter(|c| glob(p, c) && !kept.contains(*c))
                        .cloned()
                        .collect();
                    if hits.is_empty() && !p.contains('*') {
                        return Err(unknown(p));
                    }
                    kept.extend(hits);
                }
                cols = kept;
            }
            Command::Drop(patterns) => {
                for p in &patterns {
                    if !p.contains('*') && !cols.contains(p) {
                        return Err(unknown(p));
                    }
                }
                cols.retain(|c| !patterns.iter().any(|p| glob(p, c)));
            }
            Command::Sort(keys) => {
                if let Some(k) = keys.iter().find(|k| !cols.contains(&k.field)) {
                    return Err(unknown(&k.field));
                }
                rows.sort_by(|a, b| {
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
        }
    }
    if !explicit_limit {
        rows.truncate(DEFAULT_LIMIT);
    }
    // ponytail: assumes top-N pushdown, so only returned rows are loaded; ES's real counts vary by plan
    let (documents_found, values_loaded) = (rows.len(), rows.len() * cols.len());
    Ok(Response {
        documents_found,
        values_loaded,
        columns: cols
            .iter()
            .map(|c| Column {
                name: c.clone(),
                r#type: types[c].clone(),
            })
            .collect(),
        values: rows
            .into_iter()
            .map(|r| cols.iter().map(|c| r[c.as_str()].clone()).collect())
            .collect(),
    })
}
