//! Lucene's `RegExp` syntax, matched against a whole string by backtracking.
// ponytail: backtracking can take exponential time on nested repeats like `(a*)*b`; build
// Lucene's automaton when patterns come from untrusted input at scale

/// A parsed regular expression.
#[derive(Debug)]
pub(crate) enum Node {
    /// One character in (or, negated, outside) the inclusive ranges; `.` is an empty negation.
    Class(Vec<(char, char)>, bool),
    Concat(Vec<Node>),
    /// No alternatives matches nothing (`#`).
    Union(Vec<Node>),
    Repeat(Box<Node>, usize, Option<usize>),
}

/// Parses `pattern` like Lucene's `RegExp` with `RegExp.ALL`; `None` if it is invalid.
// ponytail: `~` complement, `&` intersection, `<n-m>` intervals, `<name>` automata and the
// `\d`-style shorthands are refused rather than supported
pub(crate) fn parse(pattern: &str) -> Option<Node> {
    let mut p = Parser {
        s: pattern.chars().collect(),
        i: 0,
    };
    let n = p.union()?;
    (p.i == p.s.len()).then_some(n)
}

impl Node {
    /// Whether the whole of `s` matches.
    pub(crate) fn matches(&self, s: &str) -> bool {
        let s: Vec<char> = s.chars().collect();
        self.at(&s, 0, &|i| i == s.len())
    }

    /// Whether this matches `s` from `i` up to some `j` for which `k(j)` holds.
    fn at(&self, s: &[char], i: usize, k: &dyn Fn(usize) -> bool) -> bool {
        match self {
            Node::Class(ranges, negated) => s.get(i).is_some_and(|c| {
                ranges.iter().any(|(a, b)| (a..=b).contains(&c)) != *negated && k(i + 1)
            }),
            Node::Concat(ns) => concat(ns, s, i, k),
            Node::Union(ns) => ns.iter().any(|n| n.at(s, i, k)),
            Node::Repeat(n, min, max) => repeat(n, (*min, *max), 0, s, i, k),
        }
    }
}

fn concat(ns: &[Node], s: &[char], i: usize, k: &dyn Fn(usize) -> bool) -> bool {
    match ns.split_first() {
        None => k(i),
        Some((n, rest)) => n.at(s, i, &|j| concat(rest, s, j, k)),
    }
}

/// Greedy: one more repetition first; past `min`, only ones that consume something.
fn repeat(
    n: &Node,
    (min, max): (usize, Option<usize>),
    count: usize,
    s: &[char],
    i: usize,
    k: &dyn Fn(usize) -> bool,
) -> bool {
    if max == Some(count) {
        return k(i);
    }
    let more = |j| (j > i || count < min) && repeat(n, (min, max), count + 1, s, j, k);
    n.at(s, i, &more) || (count >= min && k(i))
}

struct Parser {
    s: Vec<char>,
    i: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.i += 1;
        Some(c)
    }

    fn eat(&mut self, c: char) -> bool {
        let found = self.peek() == Some(c);
        self.i += usize::from(found);
        found
    }

    fn union(&mut self) -> Option<Node> {
        let mut alts = vec![self.concat()?];
        while self.eat('|') {
            alts.push(self.concat()?);
        }
        Some(match alts.len() {
            1 => alts.remove(0),
            _ => Node::Union(alts),
        })
    }

    fn concat(&mut self) -> Option<Node> {
        let mut seq = vec![self.repeat()?];
        while self.peek().is_some_and(|c| !matches!(c, ')' | '|' | '&')) {
            seq.push(self.repeat()?);
        }
        Some(Node::Concat(seq))
    }

    fn repeat(&mut self) -> Option<Node> {
        let mut n = self.simple()?;
        loop {
            let bounds = match self.peek() {
                Some('?') => (0, Some(1)),
                Some('*') => (0, None),
                Some('+') => (1, None),
                Some('{') => {
                    self.i += 1;
                    let min = self.number()?;
                    let max = match (self.eat(','), self.peek()) {
                        (false, _) => Some(min),
                        (true, Some('}')) => None,
                        (true, _) => Some(self.number()?),
                    };
                    if self.peek() != Some('}') {
                        return None;
                    }
                    (min, max)
                }
                _ => return Some(n),
            };
            self.i += 1;
            n = Node::Repeat(Box::new(n), bounds.0, bounds.1);
        }
    }

    fn number(&mut self) -> Option<usize> {
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        self.s[start..self.i]
            .iter()
            .collect::<String>()
            .parse()
            .ok()
    }

    fn simple(&mut self) -> Option<Node> {
        let one = |c| Node::Class(vec![(c, c)], false);
        Some(match self.next()? {
            '.' => Node::Class(vec![], true),
            '#' => Node::Union(vec![]),
            '@' => Node::Repeat(Box::new(Node::Class(vec![], true)), 0, None),
            '(' if self.eat(')') => Node::Concat(vec![]),
            '(' => {
                let n = self.union()?;
                self.eat(')').then_some(n)?
            }
            '"' => {
                let end = self.i + self.s[self.i..].iter().position(|&c| c == '"')?;
                let n = Node::Concat(self.s[self.i..end].iter().map(|&c| one(c)).collect());
                self.i = end + 1;
                n
            }
            '[' => {
                let negated = self.eat('^');
                let mut ranges = vec![];
                loop {
                    let a = self.char_exp()?;
                    let b = if self.eat('-') { self.char_exp()? } else { a };
                    ranges.push((a, b));
                    if self.eat(']') {
                        break Node::Class(ranges, negated);
                    }
                }
            }
            '~' | '<' => return None,
            '\\' => one(self.escaped()?),
            c => one(c),
        })
    }

    fn char_exp(&mut self) -> Option<char> {
        match self.next()? {
            '\\' => self.escaped(),
            c => Some(c),
        }
    }

    fn escaped(&mut self) -> Option<char> {
        self.next().filter(|c| !"dswDSW".contains(*c))
    }
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn matches_whole_strings_like_lucene() {
        let m = |p: &str, s: &str| parse(p).unwrap().matches(s);
        assert!(m("a(b|c)*d", "abcbd") && !m("a(b|c)*d", "abcbde"));
        assert!(m("[^a-c]{2,3}", "xyz") && !m("[^a-c]{2,3}", "xa"));
        assert!(m("(a?){3}b", "ab") && m("\"a.b\"@", "a.bc") && !m("\"a.b\"", "axb"));
        assert!(m("\\.\\*", ".*") && !m("#", "") && m("()", ""));
        for bad in ["(a", "a{2", "[ab", "a&b", "~a", "<1-5>", "\\d", "a|"] {
            assert!(parse(bad).is_none(), "{bad}");
        }
    }
}
