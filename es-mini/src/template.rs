//! Search templates: mustache rendering of inline `source` templates.
//!
//! ponytail: plain mustache only (variables, `{{{ }}}`/`{{& }}`, sections, inverted sections,
//! comments); ES's `toJson`/`join`/`url` lambdas and custom delimiters are not supported.

use crate::{error::AppError, indices::parse_body};
use axum::body::Bytes;
use serde_json::Value;

enum Node<'a> {
    Text(&'a str),
    Var(&'a str, bool),
    Section(&'a str, bool, Vec<Node<'a>>),
}

/// Renders `source` with `params` the way Elasticsearch's mustache engine does for JSON:
/// `{{x}}` is JSON-string escaped, `{{{x}}}` is raw.
pub fn render(source: &str, params: &Value) -> Result<String, String> {
    let mut rest = source;
    let nodes = parse(&mut rest, None)?;
    let mut out = String::new();
    emit(&nodes, &mut vec![params], &mut out);
    Ok(out)
}

fn parse<'a>(rest: &mut &'a str, open: Option<&str>) -> Result<Vec<Node<'a>>, String> {
    let mut nodes = Vec::new();
    while let Some(i) = rest.find("{{") {
        nodes.push(Node::Text(&rest[..i]));
        let (close, tag) = if rest[i..].starts_with("{{{") {
            ("}}}", &rest[i + 3..])
        } else {
            ("}}", &rest[i + 2..])
        };
        let end = tag
            .find(close)
            .ok_or_else(|| format!("unclosed tag in [{rest}]"))?;
        *rest = &tag[end + close.len()..];
        let tag = &tag[..end];
        let name = |t: &'a str| t[1..].trim();
        match tag.chars().next() {
            _ if close == "}}}" => nodes.push(Node::Var(tag.trim(), false)),
            Some('!') => {}
            Some('&') => nodes.push(Node::Var(name(tag), false)),
            Some(c @ ('#' | '^')) => {
                let children = parse(rest, Some(name(tag)))?;
                nodes.push(Node::Section(name(tag), c == '^', children));
            }
            Some('/') if open == Some(name(tag)) => return Ok(nodes),
            Some('/') => return Err(format!("mismatched close tag [{}]", name(tag))),
            _ => nodes.push(Node::Var(tag.trim(), true)),
        }
    }
    if let Some(open) = open {
        return Err(format!("section [{open}] is not closed"));
    }
    nodes.push(Node::Text(rest));
    Ok(nodes)
}

/// Looks `name` up through the context stack, innermost first; `.` is the current context.
fn lookup<'v>(stack: &[&'v Value], name: &str) -> Option<&'v Value> {
    let top = *stack.last()?;
    if name == "." {
        return Some(top);
    }
    let mut parts = name.split('.');
    let first = parts.next()?;
    let mut v = stack.iter().rev().find_map(|c| get(c, first))?;
    for p in parts {
        v = get(v, p)?;
    }
    Some(v)
}

fn get<'v>(v: &'v Value, key: &str) -> Option<&'v Value> {
    match v {
        Value::Object(o) => o.get(key),
        Value::Array(a) => a.get(key.parse::<usize>().ok()?),
        _ => None,
    }
}

fn emit<'v>(nodes: &[Node], stack: &mut Vec<&'v Value>, out: &mut String) {
    for node in nodes {
        match node {
            Node::Text(t) => out.push_str(t),
            Node::Var(name, escape) => {
                let s = match lookup(stack, name) {
                    None | Some(Value::Null) => continue,
                    // ES's mustache wraps a list in a map from index to item
                    Some(Value::Array(a)) => format!(
                        "{{{}}}",
                        (a.iter().enumerate())
                            .map(|(i, v)| format!("{i}={}", java_string(v)))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    Some(v) => java_string(v),
                };
                match escape {
                    true => {
                        let quoted = Value::from(s).to_string();
                        out.push_str(&quoted[1..quoted.len() - 1]);
                    }
                    false => out.push_str(&s),
                }
            }
            Node::Section(name, inverted, children) => {
                let v = lookup(stack, name);
                let falsy = match v {
                    None | Some(Value::Null | Value::Bool(false)) => true,
                    Some(Value::Array(a)) => a.is_empty(),
                    Some(Value::String(s)) => s.is_empty(),
                    _ => false,
                };
                if *inverted {
                    if falsy {
                        emit(children, stack, out);
                    }
                    continue;
                }
                let items: Vec<&'v Value> = match v {
                    _ if falsy => vec![],
                    Some(Value::Array(a)) => a.iter().collect(),
                    Some(v) => vec![v],
                    None => vec![],
                };
                for item in items {
                    stack.push(item);
                    emit(children, stack, out);
                    stack.pop();
                }
            }
        }
    }
}

/// A params value as Java's `toString` prints it, params being parsed into `HashMap`s.
// ponytail: integers past u64 print as doubles; Java's are BigIntegers
fn java_string(v: &Value) -> String {
    let join = |parts: Vec<String>| parts.join(", ");
    match v {
        Value::Array(a) => format!("[{}]", join(a.iter().map(java_string).collect())),
        Value::Object(o) => {
            let mut entries: Vec<_> = o.iter().collect();
            es_query::java_hash_order(&mut entries);
            let entries = entries
                .iter()
                .map(|(k, v)| format!("{k}={}", java_string(v)));
            format!("{{{}}}", join(entries.collect()))
        }
        Value::Number(n) if n.is_f64() => es_query::java_double(n.as_f64().unwrap_or(f64::NAN)),
        v => es_index::setting_string(v),
    }
}

/// Renders a search template request body (`source` + `params`) into a `_search` body.
pub fn search_body(body: &Bytes) -> Result<Bytes, AppError> {
    let body = parse_body(body)?;
    if body.get("id").is_some() {
        return Err(AppError::IllegalArgument(
            "stored search templates ([id]) are not supported; use an inline [source]".into(),
        ));
    }
    let source = match &body["source"] {
        Value::String(s) => s.clone(),
        Value::Null => {
            return Err(AppError::ActionRequestValidation(
                "template's source must be specified".into(),
            ));
        }
        other => other.to_string(),
    };
    let rendered = render(&source, &body["params"]).map_err(AppError::IllegalArgument)?;
    Ok(Bytes::from(rendered))
}
