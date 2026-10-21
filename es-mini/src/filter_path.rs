//! `filter_path` response filtering: dotted patterns with `*`/`**` wildcards; `-` prefix excludes.
use serde_json::Value;

type Pats = Vec<Vec<String>>;

/// Splits on unescaped dots; `\.` is a literal dot, as in Java's `FilterPath.parse`.
fn split(f: &str) -> Vec<String> {
    let (mut out, mut cur, mut chars) = (Vec::new(), String::new(), f.chars().peekable());
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'.') => cur.push(chars.next().unwrap_or('.')),
            '.' => out.push(std::mem::take(&mut cur)),
            c => cur.push(c),
        }
    }
    out.push(cur);
    out
}

fn glob(p: &str, s: &str) -> bool {
    match p.split_once('*') {
        None => p == s,
        Some((a, b)) => {
            s.starts_with(a) && (0..=s.len() - a.len()).any(|i| glob(b, &s[a.len() + i..]))
        }
    }
}

/// Advances patterns past `key`; the bool is true when a pattern matched completely.
fn step(pats: &Pats, key: &str) -> (Pats, bool) {
    let (mut next, mut full) = (Vec::new(), false);
    for p in pats {
        if p[0] == "**" {
            next.push(p.clone());
            match p.get(1) {
                None => full = true,
                Some(n) if glob(n, key) => match p.len() {
                    2 => full = true,
                    _ => next.push(p[2..].to_vec()),
                },
                _ => {}
            }
        } else if glob(&p[0], key) {
            match p.len() {
                1 => full = true,
                _ => next.push(p[1..].to_vec()),
            }
        }
    }
    (next, full)
}

fn walk(v: &Value, inc: Option<&Pats>, exc: &Pats) -> Option<Value> {
    match v {
        Value::Object(m) => {
            let mut out = serde_json::Map::new();
            for (k, child) in m {
                let (e, dropped) = step(exc, k);
                if dropped {
                    continue;
                }
                let kept = match inc {
                    None => walk(child, None, &e),
                    Some(i) => match step(i, k) {
                        (_, true) => walk(child, None, &e),
                        (n, false) if !n.is_empty() => walk(child, Some(&n), &e),
                        _ => None,
                    },
                };
                if let Some(c) = kept {
                    out.insert(k.clone(), c);
                }
            }
            (inc.is_none() || !out.is_empty()).then_some(Value::Object(out))
        }
        Value::Array(a) => {
            let items: Vec<Value> = a.iter().filter_map(|x| walk(x, inc, exc)).collect();
            (inc.is_none() || !items.is_empty()).then_some(Value::Array(items))
        }
        _ => inc.is_none().then(|| v.clone()),
    }
}

pub fn apply(v: &Value, filter: &str) -> Value {
    let (mut inc, mut exc): (Pats, Pats) = (Vec::new(), Vec::new());
    for f in filter.split(',').filter(|f| !f.is_empty()) {
        match f.strip_prefix('-') {
            Some(x) => exc.push(split(x)),
            None => inc.push(split(f)),
        }
    }
    let inc = (!inc.is_empty()).then_some(inc);
    walk(v, inc.as_ref(), &exc).unwrap_or_else(|| Value::Object(Default::default()))
}

#[cfg(test)]
mod tests {
    use super::apply;
    use serde_json::json;

    #[test]
    fn escaped_dot_is_literal() {
        let v = json!({"a.b": 1, "a": {"b": 2}});
        assert_eq!(apply(&v, "a\\.b"), json!({"a.b": 1}));
        assert_eq!(apply(&v, "a.b"), json!({"a": {"b": 2}}));
    }

    #[test]
    fn includes_and_excludes() {
        let v = json!({"took": 1, "hits": {"hits": [{"_source": {"n": 1, "p": {"a": 1}}}]}});
        assert_eq!(apply(&v, "took"), json!({"took": 1}));
        assert_eq!(
            apply(&v, "-**._source.p,**._source"),
            json!({"hits": {"hits": [{"_source": {"n": 1}}]}})
        );
        assert_eq!(
            apply(&v, "hits.**._so*"),
            json!({"hits": {"hits": [{"_source": {"n": 1, "p": {"a": 1}}}]}})
        );
    }
}
