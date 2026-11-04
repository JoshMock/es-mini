//! Lucene's `RegExp` syntax with `RegExp.ALL | RegExp.DEPRECATED_COMPLEMENT`, as Elasticsearch
//! parses terms include/exclude and ES|QL RLIKE patterns, compiled to an automaton like
//! `RegExp.toAutomaton` and determinized, so matching takes time linear in the string's length.

use std::collections::HashMap;

const MAX: u32 = char::MAX as u32;
/// Lucene's `Operations.DEFAULT_DETERMINIZE_WORK_LIMIT`.
const WORK_LIMIT: usize = 10_000;

/// A parsed regular expression.
#[derive(Debug)]
pub(crate) enum Node {
    /// One code point in (or, negated, outside) the inclusive ranges; `.` is an empty negation.
    Class(Vec<(u32, u32)>, bool),
    Concat(Vec<Node>),
    /// No alternatives matches nothing (`#`).
    Union(Vec<Node>),
    Intersection(Vec<Node>),
    Repeat(Box<Node>, usize, Option<usize>),
    Complement(Box<Node>),
    /// `<min-max>`: decimal numbers `digits` wide, or with any leading zeros when 0.
    Interval(u32, u32, usize),
    /// `<name>`: a named automaton, of which Elasticsearch provides none.
    Named(String),
}

/// Parses `pattern` like Lucene's `RegExp` constructor, with its error messages.
pub(crate) fn parse(pattern: &str) -> Result<Node, String> {
    let mut p = Parser {
        s: pattern.chars().collect(),
        i: 0,
    };
    if p.s.is_empty() {
        return Ok(Node::Concat(vec![]));
    }
    let n = p.union()?;
    match p.more() {
        true => Err(format!("end-of-string expected at position {}", p.i)),
        false => Ok(n),
    }
}

/// `pattern`'s deterministic automaton.
pub(crate) fn compile(pattern: &str) -> Result<Automaton, String> {
    parse(pattern)?.automaton()
}

impl Node {
    /// The automaton `RegExp.toAutomaton` builds, determinized as Elasticsearch's callers do.
    pub(crate) fn automaton(&self) -> Result<Automaton, String> {
        self.nfa()?.determinize()
    }

    fn nfa(&self) -> Result<Automaton, String> {
        let all = |ns: &[Node]| ns.iter().map(Node::nfa).collect::<Result<Vec<_>, _>>();
        Ok(match self {
            Node::Class(ranges, false) => Automaton::class(ranges),
            Node::Class(ranges, true) => Automaton::class(&gaps(ranges)),
            Node::Concat(ns) => (all(ns)?.iter()).fold(Automaton::new(true), |a, b| a.concat(b)),
            Node::Union(ns) => Automaton::union(&all(ns)?),
            Node::Intersection(ns) => {
                let ns = all(ns)?;
                ns[1..].iter().fold(ns[0].clone(), |a, b| a.intersect(b))
            }
            Node::Repeat(n, min, max) => n.nfa()?.repeat(*min, *max),
            Node::Complement(n) => n.nfa()?.complement()?,
            &Node::Interval(min, max, digits) => Automaton::interval(min, max, digits),
            Node::Named(name) => return Err(format!("'{name}' not found")),
        })
    }
}

/// The code points outside `ranges`.
fn gaps(ranges: &[(u32, u32)]) -> Vec<(u32, u32)> {
    let mut sorted = ranges.to_vec();
    sorted.sort_unstable();
    let (mut out, mut next) = (vec![], 0);
    for (lo, hi) in sorted {
        if lo > next {
            out.push((next, lo - 1));
        }
        next = next.max(hi + 1);
    }
    if next <= MAX {
        out.push((next, MAX));
    }
    out
}

/// An automaton over code points without epsilon moves, starting at state 0.
#[derive(Debug, Clone)]
pub(crate) struct Automaton {
    accept: Vec<bool>,
    /// Each state's transitions: an inclusive code point range and the target state.
    edges: Vec<Vec<(u32, u32, usize)>>,
}

impl Automaton {
    /// One state, accepting the empty string or nothing.
    fn new(accept: bool) -> Automaton {
        Automaton {
            accept: vec![accept],
            edges: vec![vec![]],
        }
    }

    fn class(ranges: &[(u32, u32)]) -> Automaton {
        let mut a = Automaton::new(false);
        a.accept.push(true);
        a.edges.push(vec![]);
        a.edges[0] = ranges.iter().map(|&(lo, hi)| (lo, hi, 1)).collect();
        a
    }

    /// Copies `o`'s states in after these; returns where they start.
    fn append(&mut self, o: &Automaton) -> usize {
        let off = self.accept.len();
        self.accept.extend(&o.accept);
        let shift = |es: &Vec<_>| es.iter().map(|&(lo, hi, t)| (lo, hi, t + off)).collect();
        self.edges.extend(o.edges.iter().map(shift));
        off
    }

    /// Adds `edges` to state `s`, without duplicates (which nested repeats would multiply).
    fn link(&mut self, s: usize, edges: &[(u32, u32, usize)]) {
        let es = &mut self.edges[s];
        es.extend_from_slice(edges);
        es.sort_unstable();
        es.dedup();
    }

    fn union(alts: &[Automaton]) -> Automaton {
        let mut a = Automaton::new(false);
        for o in alts {
            let off = a.append(o);
            a.accept[0] |= o.accept[0];
            let start = a.edges[off].clone();
            a.link(0, &start);
        }
        a
    }

    fn concat(mut self, o: &Automaton) -> Automaton {
        let ends: Vec<usize> = (0..self.accept.len()).filter(|&s| self.accept[s]).collect();
        let off = self.append(o);
        let start = self.edges[off].clone();
        for s in ends {
            self.accept[s] = o.accept[0];
            self.link(s, &start);
        }
        self
    }

    fn star(&self) -> Automaton {
        let mut a = Automaton::new(true);
        let off = a.append(self);
        let start = a.edges[off].clone();
        for s in std::iter::once(0).chain(off..a.accept.len()) {
            if a.accept[s] {
                a.link(s, &start);
            }
        }
        a
    }

    /// `min` to `max` (or unboundedly many) copies in a row, each past `min` optional.
    fn repeat(&self, min: usize, max: Option<usize>) -> Automaton {
        let (mut a, mut ends) = (Automaton::new(true), vec![0]);
        for i in 0..max.unwrap_or(min) {
            let off = a.append(self);
            let start = a.edges[off].clone();
            for &s in &ends {
                a.link(s, &start);
                if i < min {
                    a.accept[s] = self.accept[0];
                }
            }
            ends = (off..a.accept.len()).filter(|&s| a.accept[s]).collect();
        }
        match max {
            None => a.concat(&self.star()),
            Some(_) => a,
        }
    }

    /// The product automaton, accepting what both accept.
    fn intersect(&self, o: &Automaton) -> Automaton {
        let mut pairs = vec![(0, 0)];
        let mut ids = HashMap::from([((0, 0), 0)]);
        let mut a = Automaton {
            accept: vec![],
            edges: vec![],
        };
        while let Some(&(p, q)) = pairs.get(a.accept.len()) {
            let mut out = vec![];
            for &(l1, h1, t1) in &self.edges[p] {
                for &(l2, h2, t2) in &o.edges[q] {
                    let (lo, hi) = (l1.max(l2), h1.min(h2));
                    if lo <= hi {
                        let t = *ids.entry((t1, t2)).or_insert_with(|| {
                            pairs.push((t1, t2));
                            pairs.len() - 1
                        });
                        out.push((lo, hi, t));
                    }
                }
            }
            a.accept.push(self.accept[p] && o.accept[q]);
            a.edges.push(out);
        }
        a
    }

    /// Lucene's `Operations.complement`: determinized, made total, accepting states flipped.
    fn complement(&self) -> Result<Automaton, String> {
        let mut d = self.determinize()?;
        let dead = d.accept.len();
        d.accept.push(false);
        d.edges.push(vec![]);
        for s in 0..=dead {
            let ranges: Vec<_> = d.edges[s].iter().map(|e| (e.0, e.1)).collect();
            (d.edges[s]).extend(gaps(&ranges).into_iter().map(|(lo, hi)| (lo, hi, dead)));
            d.accept[s] = !d.accept[s];
        }
        Ok(d)
    }

    /// Lucene's `Operations.determinize`: the subset construction, failing past the same
    /// effort (the subset sizes summed).
    // ponytail: effort counts this module's NFA states, not Lucene's, so a pattern near the
    // limit may fail in one and not the other, and the error names different counts
    fn determinize(&self) -> Result<Automaton, String> {
        let mut sets = vec![vec![0]];
        let mut ids = HashMap::from([(vec![0], 0)]);
        let mut d = Automaton {
            accept: vec![],
            edges: vec![],
        };
        let mut effort = 0;
        while let Some(set) = sets.get(d.accept.len()).cloned() {
            effort += set.len();
            if effort >= WORK_LIMIT * 10 {
                return Err(format!(
                    "Determinizing automaton with {} states and {} transitions would require more than {WORK_LIMIT} effort.",
                    self.accept.len(),
                    self.edges.iter().map(Vec::len).sum::<usize>()
                ));
            }
            let edges: Vec<_> = set.iter().flat_map(|&s| &self.edges[s]).collect();
            let mut points: Vec<u32> = edges.iter().flat_map(|e| [e.0, e.1 + 1]).collect();
            points.sort_unstable();
            points.dedup();
            let mut out: Vec<(u32, u32, usize)> = vec![];
            for w in points.windows(2) {
                let (lo, hi) = (w[0], w[1] - 1);
                let mut to: Vec<usize> = (edges.iter())
                    .filter(|e| e.0 <= lo && hi <= e.1)
                    .map(|e| e.2)
                    .collect();
                if to.is_empty() {
                    continue;
                }
                to.sort_unstable();
                to.dedup();
                let id = match ids.get(&to) {
                    Some(&id) => id,
                    None => {
                        ids.insert(to.clone(), sets.len());
                        sets.push(to);
                        sets.len() - 1
                    }
                };
                match out.last_mut() {
                    Some(last) if last.2 == id && last.1 + 1 == lo => last.1 = hi,
                    _ => out.push((lo, hi, id)),
                }
            }
            d.accept.push(set.iter().any(|&s| self.accept[s]));
            d.edges.push(out);
        }
        Ok(d)
    }

    /// Lucene's `Automata.makeDecimalInterval`.
    fn interval(min: u32, max: u32, digits: usize) -> Automaton {
        let d = if digits > 0 {
            digits
        } else {
            max.to_string().len()
        };
        let (x, y) = (
            format!("{min:0d$}").into_bytes(),
            format!("{max:0d$}").into_bytes(),
        );
        // With any leading zeros, state 0 skips them into the fixed-width part, at state 1.
        let zeros = usize::from(digits == 0);
        let mut a = Automaton {
            accept: vec![false; zeros],
            edges: vec![vec![]; zeros],
        };
        // The fixed-width part's states: digits read, and whether they are x's and y's prefix.
        let mut keys = vec![(0, true, true)];
        let mut ids = HashMap::from([(keys[0], zeros)]);
        while let Some(&(p, lo, hi)) = keys.get(a.accept.len() - zeros) {
            let mut out = vec![];
            for c in b'0'..=b'9' {
                if p == d || lo && c < x[p] || hi && c > y[p] {
                    continue;
                }
                let k = (p + 1, lo && c == x[p], hi && c == y[p]);
                let t = *ids.entry(k).or_insert_with(|| {
                    keys.push(k);
                    zeros + keys.len() - 1
                });
                out.push((u32::from(c), u32::from(c), t));
            }
            a.accept.push(p == d);
            a.edges.push(out);
        }
        if zeros == 1 {
            let zero = u32::from(b'0');
            a.edges[0].push((zero, zero, 0));
            let mut q = Some(1);
            for _ in 0..d {
                let Some(s) = q else { break };
                let es = a.edges[s].clone();
                q = es.iter().find(|e| e.0 == zero).map(|e| e.2);
                a.link(0, &es);
            }
        }
        a
    }

    /// Whether some string matches both.
    pub(crate) fn intersects(&self, o: &Automaton) -> bool {
        self.intersect(o).accept.contains(&true)
    }

    /// Whether the whole of `s` matches; `self` is deterministic.
    pub(crate) fn matches(&self, s: &str) -> bool {
        let mut q = 0;
        for c in s.chars().map(u32::from) {
            match self.edges[q].iter().find(|e| e.0 <= c && c <= e.1) {
                Some(e) => q = e.2,
                None => return false,
            }
        }
        self.accept[q]
    }
}

fn char_node(c: u32) -> Node {
    Node::Class(vec![(c, c)], false)
}

/// One node, or `f` of several.
fn one_or(mut ns: Vec<Node>, f: fn(Vec<Node>) -> Node) -> Node {
    match ns.len() {
        1 => ns.remove(0),
        _ => f(ns),
    }
}

/// What `<s>` holds: a decimal interval `n-m`, else a named automaton; `None` if malformed.
fn angle(s: &str) -> Option<Node> {
    let Some(i) = s.find('-') else {
        return Some(Node::Named(s.into()));
    };
    if i == 0 || i == s.len() - 1 || s.rfind('-') != Some(i) {
        return None;
    }
    let (lo, hi) = (&s[..i], &s[i + 1..]);
    let (a, b) = (lo.parse::<i32>().ok()?, hi.parse::<i32>().ok()?);
    let digits = if lo.len() == hi.len() { lo.len() } else { 0 };
    Some(Node::Interval(a.min(b) as u32, a.max(b) as u32, digits))
}

/// Lucene's `RegExp` parser: `|` binds loosest, then `&`, concatenation, repeats, and `~`.
struct Parser {
    s: Vec<char>,
    i: usize,
}

impl Parser {
    fn more(&self) -> bool {
        self.i < self.s.len()
    }

    fn peek(&self, set: &str) -> bool {
        self.s.get(self.i).is_some_and(|&c| set.contains(c))
    }

    /// Whether a backslash here starts a shorthand (or an invalid letter escape).
    fn at_shorthand(&self) -> bool {
        (self.s.get(self.i)).is_some_and(|&c| c == '\\' || c.is_ascii_alphabetic())
    }

    fn eat(&mut self, c: char) -> bool {
        let found = self.s.get(self.i) == Some(&c);
        self.i += usize::from(found);
        found
    }

    fn expect(&mut self, c: char) -> Result<(), String> {
        match self.eat(c) {
            true => Ok(()),
            false => Err(format!("expected '{c}' at position {}", self.i)),
        }
    }

    fn next(&mut self) -> Result<char, String> {
        let c = *self.s.get(self.i).ok_or("unexpected end-of-string")?;
        self.i += 1;
        Ok(c)
    }

    fn union(&mut self) -> Result<Node, String> {
        let mut alts = vec![self.intersection()?];
        while self.eat('|') {
            alts.push(self.intersection()?);
        }
        Ok(one_or(alts, Node::Union))
    }

    fn intersection(&mut self) -> Result<Node, String> {
        let mut parts = vec![self.concat()?];
        while self.eat('&') {
            parts.push(self.concat()?);
        }
        Ok(one_or(parts, Node::Intersection))
    }

    fn concat(&mut self) -> Result<Node, String> {
        let mut seq = vec![self.repeat()?];
        while self.more() && !self.peek(")|&") {
            seq.push(self.repeat()?);
        }
        Ok(one_or(seq, Node::Concat))
    }

    fn repeat(&mut self) -> Result<Node, String> {
        let mut n = self.complement()?;
        while self.peek("?*+{") {
            let (min, max) = match self.next()? {
                '?' => (0, Some(1)),
                '*' => (0, None),
                '+' => (1, None),
                _ => {
                    let at = self.i;
                    let min = self.number()?;
                    let min = min.ok_or_else(|| format!("integer expected at position {at}"))?;
                    let max = match self.eat(',') {
                        true => self.number()?,
                        false => Some(min),
                    };
                    self.expect('}')?;
                    if let Some(max) = max.filter(|&max| min > max) {
                        return Err(format!(
                            "invalid repetition range(out of order): {min}..{max}"
                        ));
                    }
                    (min, max)
                }
            };
            n = Node::Repeat(Box::new(n), min, max);
        }
        Ok(n)
    }

    /// Digits read as Java's `Integer.parseInt`; `None` if there are none.
    fn number(&mut self) -> Result<Option<usize>, String> {
        let start = self.i;
        while self.peek("0123456789") {
            self.i += 1;
        }
        let s: String = self.s[start..self.i].iter().collect();
        match s.is_empty() {
            true => Ok(None),
            false => (s.parse::<i32>())
                .map(|n| Some(n as usize))
                .map_err(|_| format!("For input string: \"{s}\"")),
        }
    }

    fn complement(&mut self) -> Result<Node, String> {
        if self.eat('~') {
            return Ok(Node::Complement(Box::new(self.complement()?)));
        }
        if !self.eat('[') {
            return self.simple();
        }
        let negated = self.eat('^');
        let mut ranges = vec![];
        loop {
            if self.eat('\\') {
                match self.at_shorthand() {
                    true => ranges.extend(self.shorthand()?),
                    // an escaped character starts no range
                    false => {
                        let c = self.next()?.into();
                        ranges.push((c, c));
                    }
                }
            } else {
                let a = self.char_exp()?;
                let b = if self.eat('-') { self.char_exp()? } else { a };
                if a > b {
                    return Err(format!("invalid range: from ({a}) cannot be > to ({b})"));
                }
                ranges.push((a, b));
            }
            if !self.more() || self.peek("]") {
                break;
            }
        }
        self.expect(']')?;
        Ok(Node::Class(ranges, negated))
    }

    /// The class after a backslash, as Lucene's `expandPreDefined`.
    fn shorthand(&mut self) -> Result<Vec<(u32, u32)>, String> {
        const DIGIT: &[(u32, u32)] = &[('0' as u32, '9' as u32)];
        const SPACE: &[(u32, u32)] = &[('\t' as u32, '\n' as u32), (13, 13), (32, 32)];
        const WORD: &[(u32, u32)] = &[(48, 57), (65, 90), (95, 95), (97, 122)];
        Ok(match self.next()? {
            '\\' => vec![('\\' as u32, '\\' as u32)],
            'd' => DIGIT.to_vec(),
            'D' => gaps(DIGIT),
            's' => SPACE.to_vec(),
            'S' => gaps(SPACE),
            'w' => WORD.to_vec(),
            'W' => gaps(WORD),
            // Lucene appends the code point: `\p` is "invalid character class \112".
            c => return Err(format!("invalid character class \\{}", c as u32)),
        })
    }

    fn simple(&mut self) -> Result<Node, String> {
        let any = || Node::Class(vec![], true);
        Ok(match self.next()? {
            '.' => any(),
            '#' => Node::Union(vec![]),
            '@' => Node::Repeat(Box::new(any()), 0, None),
            '"' => {
                let start = self.i;
                while self.more() && !self.peek("\"") {
                    self.i += 1;
                }
                self.expect('"')?;
                let chars = &self.s[start..self.i - 1];
                Node::Concat(chars.iter().map(|&c| char_node(c.into())).collect())
            }
            '(' if self.eat(')') => Node::Concat(vec![]),
            '(' => {
                let n = self.union()?;
                self.expect(')')?;
                n
            }
            '<' => {
                let start = self.i;
                while self.more() && !self.peek(">") {
                    self.i += 1;
                }
                self.expect('>')?;
                let s: String = self.s[start..self.i - 1].iter().collect();
                angle(&s)
                    .ok_or_else(|| format!("interval syntax error at position {}", self.i - 1))?
            }
            '\\' if self.at_shorthand() => Node::Class(self.shorthand()?, false),
            '\\' => char_node(self.next()?.into()),
            c => char_node(c.into()),
        })
    }

    /// A character, possibly escaped.
    fn char_exp(&mut self) -> Result<u32, String> {
        self.eat('\\');
        Ok(self.next()?.into())
    }
}

#[cfg(test)]
mod tests {
    use super::{compile, parse};

    fn m(p: &str, s: &str) -> bool {
        compile(p).unwrap().matches(s)
    }

    #[test]
    fn matches_whole_strings_like_lucene() {
        assert!(m("a(b|c)*d", "abcbd") && !m("a(b|c)*d", "abcbde"));
        assert!(m("[^a-c]{2,3}", "xyz") && !m("[^a-c]{2,3}", "xa"));
        assert!(m("(a?){3}b", "ab") && m("\"a.b\"@", "a.bc") && !m("\"a.b\"", "axb"));
        assert!(m("\\.\\*", ".*") && !m("#", "") && m("()", "") && m("", ""));
        assert!(m("a{2,}", "aaa") && !m("a{2,}", "a") && m("|a", "|a"));
        for bad in [
            "(a", "a{2", "[ab", "a|", "a{3,2}", "[b-a]", "\\q", "<1x-5>", "a)",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn regexp_automaton_intersects() {
        let a = |p| compile(p).unwrap();
        assert!(a("x.*").intersects(&a(".*y")) && !a("x.*").intersects(&a("y.*")));
        assert!(!a("a~(b.*)").intersects(&a("ab.*")) && a("a~(b.*)").intersects(&a("a.*")));
    }

    #[test]
    fn regexp_automaton_nested_repeats_take_linear_time() {
        let a = "a".repeat(10_000);
        assert!(!m("(a*)*b", &a) && m("(a*)*b", &(a.clone() + "b")));
        assert!(!m("(a|aa)*c", &a) && m("(a+)+", &a));
        assert!(m("a{0,5000}", &a[..5000]) && !m("a{0,5000}", &a[..5001]));
    }

    #[test]
    fn regexp_automaton_complement_intersection_intervals_and_shorthands() {
        // `~` binds tighter than repeats and concatenation, `&` looser than concatenation
        assert!(m("~a", "b") && m("~a", "") && m("~a", "aa") && !m("~a", "a"));
        assert!(m("~ab", "bb") && !m("~ab", "ab") && m("~(ab)", "bb"));
        assert!(m("a.*&.*b", "axb") && !m("a.*&.*b", "ax") && m("@&~(.*z.*)", "abc"));
        assert!(!m("@&~(.*z.*)", "azc"));
        // `<n-m>`: equal-width bounds fix the width, otherwise any leading zeros
        assert!(m("<1-10>", "7") && m("<1-10>", "0010") && !m("<1-10>", "11"));
        assert!(!m("<1-10>", "") && !m("<1-10>", "0") && m("<0-10>", "000"));
        assert!(m("<01-10>", "07") && !m("<01-10>", "7") && !m("<1-5>", "05"));
        assert!(m("x<10-5>", "x07") && !m("<5-10>", "4"));
        assert!(m("\\d\\s\\w", "1 _") && !m("\\d", "a") && m("\\D\\S\\W", "ab-"));
        assert!(!m("[\\.-z]", "a") && m("[\\.-z]", "-"));
        assert!(m("[\\d\\s]+", "1 2") && m("\\\\", "\\") && !m("\\W", "_"));
        assert_eq!(compile("<name>").unwrap_err(), "'name' not found");
        let blowup = compile("~((a|b)*a(a|b){20})").unwrap_err();
        assert!(
            blowup.contains("would require more than 10000 effort"),
            "{blowup}"
        );
    }
}
