//! `filter_path` response filtering: dotted patterns with `*`/`**` wildcards; `-` prefix excludes.
use serde_json::Value;

type Pats<'a> = Vec<Vec<&'a str>>;

fn glob(p: &str, s: &str) -> bool {
    match p.split_once('*') {
        None => p == s,
        Some((a, b)) => {
            s.starts_with(a) && (0..=s.len() - a.len()).any(|i| glob(b, &s[a.len() + i..]))
        }
    }
}

/// Advances patterns past `key`; the bool is true when a pattern matched completely.
fn step<'a>(pats: &Pats<'a>, key: &str) -> (Pats<'a>, bool) {
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
        } else if glob(p[0], key) {
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
            Some(x) => exc.push(x.split('.').collect()),
            None => inc.push(f.split('.').collect()),
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
