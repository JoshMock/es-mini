//! Lucene's `RegExp` syntax with `RegExp.ALL | RegExp.DEPRECATED_COMPLEMENT`, as Elasticsearch
//! parses terms include/exclude and ES|QL RLIKE patterns, compiled to an automaton like
//! `RegExp.toAutomaton` and determinized, so matching takes time linear in the string's length.
//! The automata are built state for state as Lucene's `Operations` and `Automata` build them, so
//! determinizing costs the same effort and fails on the same patterns, with the same counts.

use std::collections::HashMap;

const MAX: u32 = char::MAX as u32;
/// Lucene's `Operations.DEFAULT_DETERMINIZE_WORK_LIMIT`.
const WORK_LIMIT: usize = 10_000;

/// A parsed regular expression, shaped like Lucene's `RegExp` tree.
#[derive(Debug)]
pub(crate) enum Node {
    Union(Box<Node>, Box<Node>),
    Concat(Box<Node>, Box<Node>),
    Intersection(Box<Node>, Box<Node>),
    Optional(Box<Node>),
    /// `*`.
    Star(Box<Node>),
    /// `{n,}` and `+`.
    RepeatMin(Box<Node>, usize),
    RepeatMinMax(Box<Node>, usize, usize),
    /// A negated class's complement, determinized without a work limit.
    Complement(Box<Node>),
    /// `~`, determinized within the default work limit.
    DeprecatedComplement(Box<Node>),
    /// Literal code points: one character, or a quoted or concatenated string.
    String(Vec<u32>),
    /// One code point in the inclusive ranges; `.` is all of them, `#` none.
    Class(Vec<(u32, u32)>),
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
        return Ok(Node::String(vec![]));
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
        self.nfa()?.determinize(WORK_LIMIT)
    }

    /// `RegExp.toAutomaton`.
    fn nfa(&self) -> Result<Automaton, String> {
        let leaves = |union| {
            let mut ns = vec![];
            self.leaves(union, &mut ns);
            ns.iter().map(|n| n.nfa()).collect::<Result<Vec<_>, _>>()
        };
        Ok(match self {
            Node::Union(..) => Automaton::union(&leaves(true)?),
            Node::Concat(..) => Automaton::concatenate(&leaves(false)?),
            Node::Intersection(a, b) => a.nfa()?.intersect(&b.nfa()?),
            Node::Optional(n) => n.nfa()?.optional(),
            Node::Star(n) => n.nfa()?.star(),
            &Node::RepeatMin(ref n, min) => n.nfa()?.repeat_min(min),
            &Node::RepeatMinMax(ref n, min, max) => n.nfa()?.repeat(min, max),
            Node::Complement(n) => n.nfa()?.complement(i32::MAX as usize)?,
            Node::DeprecatedComplement(n) => n.nfa()?.complement(WORK_LIMIT)?,
            Node::String(s) => Automaton::string(s),
            Node::Class(ranges) => Automaton::class(ranges),
            &Node::Interval(min, max, digits) => Automaton::interval(min, max, digits),
            Node::Named(name) => return Err(format!("'{name}' not found")),
        })
    }

    /// Lucene's `findLeaves`: the operands of nested unions (or concatenations), in order.
    fn leaves<'a>(&'a self, union: bool, out: &mut Vec<&'a Node>) {
        match (self, union) {
            (Node::Union(a, b), true) | (Node::Concat(a, b), false) => {
                a.leaves(union, out);
                b.leaves(union, out);
            }
            _ => out.push(self),
        }
    }
}

/// Lucene's `makeConcatenation`: adjacent literals join into one string.
fn concat_node(a: Node, b: Node) -> Node {
    let lit = |n: &Node| matches!(n, Node::String(_));
    match (a, b) {
        (Node::String(p), Node::String(q)) => Node::String([p, q].concat()),
        (Node::Concat(x, y), b) if lit(&y) && lit(&b) => {
            Node::Concat(x, Box::new(concat_node(*y, b)))
        }
        (a, Node::Concat(x, y)) if lit(&a) && lit(&x) => {
            Node::Concat(Box::new(concat_node(a, *x)), y)
        }
        (a, b) => Node::Concat(Box::new(a), Box::new(b)),
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

/// Lucene's `Automaton` over code points, without epsilon moves, starting at state 0 (if any).
#[derive(Debug, Clone, Default)]
pub(crate) struct Automaton {
    accept: Vec<bool>,
    /// Each state's transitions: an inclusive code point range and the target state.
    edges: Vec<Vec<(u32, u32, usize)>>,
}

impl Automaton {
    fn state(&mut self, accept: bool) -> usize {
        self.accept.push(accept);
        self.edges.push(vec![]);
        self.accept.len() - 1
    }

    /// `Automaton.finishState`: merges each state's overlapping or adjacent ranges to the same
    /// target, sorted by range.
    fn finish(mut self) -> Automaton {
        for es in &mut self.edges {
            es.sort_unstable_by_key(|&(lo, hi, t)| (t, lo, hi));
            let mut out: Vec<(u32, u32, usize)> = vec![];
            for e in es.drain(..) {
                match out.last_mut() {
                    Some(l) if l.2 == e.2 && e.0 <= l.1 + 1 => l.1 = l.1.max(e.1),
                    _ => out.push(e),
                }
            }
            out.sort_unstable();
            *es = out;
        }
        self
    }

    /// `Automaton.isDeterministic`: no state's ranges overlap.
    fn deterministic(&self) -> bool {
        (self.edges.iter()).all(|es| es.windows(2).all(|w| w[1].0 > w[0].1))
    }

    /// `Automaton.copy`: `o`'s states after these; returns where they start.
    fn copy(&mut self, o: &Automaton) -> usize {
        let off = self.accept.len();
        self.accept.extend(&o.accept);
        let shift = |es: &Vec<_>| es.iter().map(|&(lo, hi, t)| (lo, hi, t + off)).collect();
        self.edges.extend(o.edges.iter().map(shift));
        off
    }

    /// `Automaton.addEpsilon`: `s` also does what `d` does.
    fn epsilon(&mut self, s: usize, d: usize) {
        let es = self.edges[d].clone();
        self.edges[s].extend(es);
        self.accept[s] |= self.accept[d];
    }

    /// `Automata.makeCharClass` (and `makeEmpty` when there are no ranges).
    fn class(ranges: &[(u32, u32)]) -> Automaton {
        let mut a = Automaton::default();
        if !ranges.is_empty() {
            a.state(false);
            a.state(true);
            a.edges[0] = ranges.iter().map(|&(lo, hi)| (lo, hi, 1)).collect();
        }
        a.finish()
    }

    /// `Automata.makeString`.
    fn string(s: &[u32]) -> Automaton {
        let mut a = Automaton::default();
        a.state(s.is_empty());
        for (i, &c) in s.iter().enumerate() {
            let t = a.state(i == s.len() - 1);
            a.edges[t - 1].push((c, c, t));
        }
        a
    }

    /// `Operations.union`.
    fn union(alts: &[Automaton]) -> Automaton {
        let mut a = Automaton::default();
        a.state(false);
        for o in alts {
            a.copy(o);
        }
        let mut off = 1;
        for o in alts.iter().filter(|o| !o.accept.is_empty()) {
            a.epsilon(0, off);
            off += o.accept.len();
        }
        a.finish()
            .remove_dead()
            .merge_accept_states_with_no_transition()
    }

    /// `Operations.concatenate`.
    fn concatenate(list: &[Automaton]) -> Automaton {
        if list.iter().any(|a| a.accept.is_empty()) {
            return Automaton::default();
        }
        let n = list.iter().map(|a| a.accept.len()).sum();
        let mut r = Automaton {
            accept: vec![false; n],
            edges: vec![vec![]; n],
        };
        let mut off = 0;
        for (i, a) in list.iter().enumerate() {
            let n = a.accept.len();
            for s in 0..n {
                let shift = |o: usize| move |&(lo, hi, t): &(u32, u32, usize)| (lo, hi, t + o);
                r.edges[off + s].extend(a.edges[s].iter().map(shift(off)));
                if !a.accept[s] {
                    continue;
                }
                // ε-moves into the next automata, through any that accept the empty string
                let mut follow = off + n;
                for f in list[i + 1..].iter().map(Some).chain([None]) {
                    let Some(f) = f else {
                        r.accept[off + s] = true;
                        break;
                    };
                    r.edges[off + s].extend(f.edges[0].iter().map(shift(follow)));
                    if !f.accept[0] {
                        break;
                    }
                    follow += f.accept.len();
                }
            }
            off += n;
        }
        r.finish().remove_dead()
    }

    /// `Operations.optional`.
    fn optional(mut self) -> Automaton {
        if self.accept.first() == Some(&true) {
            return self;
        }
        if !self.edges.iter().flatten().any(|e| e.2 == 0) {
            if self.accept.is_empty() {
                self.state(false);
            }
            self.accept[0] = true;
            return self;
        }
        let mut a = Automaton::default();
        a.state(true);
        a.copy(&self);
        a.epsilon(0, 1);
        a.finish()
    }

    /// `Operations.repeat(a)`: the Kleene star.
    fn star(self) -> Automaton {
        let accepting = self.accept.iter().filter(|&&a| a).count();
        if self.accept.is_empty() || self.accept[0] && accepting == 1 {
            return self;
        }
        let mut b = Automaton::default();
        b.state(true);
        let map: Vec<usize> = (0..self.accept.len())
            .map(|s| match (self.accept[s], self.edges[s].is_empty()) {
                (true, true) => 0,
                (accept, _) => b.state(accept),
            })
            .collect();
        for (s, es) in self.edges.iter().enumerate() {
            b.edges[map[s]].extend(es.iter().map(|&(lo, hi, t)| (lo, hi, map[t])));
        }
        let starts: Vec<_> = self.edges[0]
            .iter()
            .map(|&(lo, hi, t)| (lo, hi, map[t]))
            .collect();
        b.edges[0].extend(&starts);
        for s in (0..self.accept.len()).filter(|&s| self.accept[s] && map[s] != 0) {
            b.edges[map[s]].extend(&starts);
        }
        b.finish().remove_dead()
    }

    /// `Operations.repeat(a, min)`.
    fn repeat_min(self, min: usize) -> Automaton {
        if min == 0 {
            return self.star();
        }
        let mut list = vec![self.clone(); min];
        list.push(self.star());
        Automaton::concatenate(&list)
    }

    /// `Operations.repeat(a, min, max)`.
    fn repeat(&self, min: usize, max: usize) -> Automaton {
        let mut b = match min {
            0 => Automaton::string(&[]),
            1 => self.clone(),
            _ => Automaton::concatenate(&vec![self.clone(); min]),
        };
        let ends = |a: &Automaton, off| -> Vec<usize> {
            (0..a.accept.len())
                .filter(|&s| a.accept[s])
                .map(|s| s + off)
                .collect()
        };
        let mut prev = ends(&b, 0);
        for _ in min..max {
            let off = b.copy(self);
            for &s in &prev {
                b.epsilon(s, off);
            }
            prev = ends(self, off);
        }
        b.finish().remove_dead()
    }

    /// `Operations.complement`: determinized, made total, accepting states flipped.
    fn complement(&self, limit: usize) -> Result<Automaton, String> {
        let mut a = self.determinize(limit)?.totalize();
        a.accept.iter_mut().for_each(|a| *a = !*a);
        Ok(a.remove_dead())
    }

    /// `Operations.totalize`: every state has a transition for every code point.
    fn totalize(&self) -> Automaton {
        let mut a = self.clone();
        let dead = a.state(false);
        a.edges[dead].push((0, MAX, dead));
        for s in 0..dead {
            let ranges: Vec<_> = a.edges[s].iter().map(|e| (e.0, e.1)).collect();
            (a.edges[s]).extend(gaps(&ranges).into_iter().map(|(lo, hi)| (lo, hi, dead)));
        }
        a.finish()
    }

    /// `Operations.intersection`: the product automaton, accepting what both accept.
    fn intersect(&self, o: &Automaton) -> Automaton {
        if self.accept.is_empty() {
            return self.clone();
        }
        if o.accept.is_empty() {
            return o.clone();
        }
        let mut pairs = vec![(0, 0)];
        let mut ids = HashMap::from([((0, 0), 0)]);
        let mut a = Automaton::default();
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
        a.finish().remove_dead()
    }

    /// `Operations.removeDeadStates`: keeps the states reachable from the start that can reach
    /// an accepting state, in order.
    fn remove_dead(self) -> Automaton {
        let n = self.accept.len();
        let reach = |starts: Vec<usize>, next: &Vec<Vec<usize>>| {
            let mut live = vec![false; n];
            let mut work = starts;
            while let Some(s) = work.pop() {
                if !std::mem::replace(&mut live[s], true) {
                    work.extend(&next[s]);
                }
            }
            live
        };
        let fwd: Vec<Vec<usize>> = (self.edges.iter())
            .map(|es| es.iter().map(|e| e.2).collect())
            .collect();
        let mut back = vec![vec![]; n];
        for (s, ts) in fwd.iter().enumerate() {
            ts.iter().for_each(|&t| back[t].push(s));
        }
        let from_start = reach((0..n.min(1)).collect(), &fwd);
        let to_accept = reach((0..n).filter(|&s| self.accept[s]).collect(), &back);
        let live: Vec<bool> = (0..n).map(|s| from_start[s] && to_accept[s]).collect();
        if live.iter().all(|&l| l) {
            return self;
        }
        let mut map = vec![0; n];
        let mut a = Automaton::default();
        for s in (0..n).filter(|&s| live[s]) {
            map[s] = a.state(self.accept[s]);
        }
        for s in (0..n).filter(|&s| live[s]) {
            let es = self.edges[s].iter().filter(|e| live[e.2]);
            a.edges[map[s]] = es.map(|&(lo, hi, t)| (lo, hi, map[t])).collect();
        }
        a.finish()
    }

    /// `Operations.mergeAcceptStatesWithNoTransition`: the accepting states without transitions
    /// become the first of them.
    fn merge_accept_states_with_no_transition(self) -> Automaton {
        let n = self.accept.len();
        let ends: Vec<usize> = (0..n)
            .filter(|&s| self.accept[s] && self.edges[s].is_empty())
            .collect();
        if ends.len() <= 1 {
            return self;
        }
        let remap = |s: usize| match ends.binary_search(&s) {
            Ok(_) => ends[0],
            Err(i) => s - i.saturating_sub(1),
        };
        let mut a = Automaton::default();
        for s in 0..n {
            while a.accept.len() <= remap(s) {
                a.state(false);
            }
            a.accept[remap(s)] |= self.accept[s];
        }
        for (s, es) in self.edges.iter().enumerate() {
            let es: Vec<_> = es.iter().map(|&(lo, hi, t)| (lo, hi, remap(t))).collect();
            a.edges[remap(s)].extend(es);
        }
        a.finish()
    }

    /// Lucene's `Operations.determinize`: the subset construction, failing past `limit` effort
    /// (the subset sizes summed, times ten).
    fn determinize(&self, limit: usize) -> Result<Automaton, String> {
        if self.accept.len() <= 1 || self.deterministic() {
            return Ok(self.clone());
        }
        let mut sets = vec![vec![0]];
        let mut ids = HashMap::from([(vec![0], 0)]);
        let mut d = Automaton::default();
        let mut effort = 0;
        while let Some(set) = sets.get(d.accept.len()).cloned() {
            effort += set.len();
            if effort >= limit * 10 {
                return Err(format!(
                    "Determinizing automaton with {} states and {} transitions would require more than {limit} effort.",
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
                out.push((lo, hi, id));
            }
            d.accept.push(set.iter().any(|&s| self.accept[s]));
            d.edges.push(out);
        }
        Ok(d.finish())
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
        let zeros = digits == 0;
        let mut b = Automaton::default();
        if zeros {
            // the real initial state, which skips leading zeros
            b.state(false);
        }
        let mut initials = vec![];
        between(&mut b, &x, &y, 0, &mut initials, zeros);
        let mut a = b.finish();
        if zeros {
            let zero = u32::from(b'0');
            a.edges[0].push((zero, zero, 0));
            for p in initials {
                a.epsilon(0, p);
            }
            a = a.finish();
        }
        a.remove_dead()
    }

    /// Whether some string matches both.
    pub(crate) fn intersects(&self, o: &Automaton) -> bool {
        self.intersect(o).accept.contains(&true)
    }

    /// Whether the whole of `s` matches; `self` is deterministic.
    pub(crate) fn matches(&self, s: &str) -> bool {
        if self.accept.is_empty() {
            return false;
        }
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

/// `Automata.makeDecimalInterval`'s helpers, which add states for the digits of `x` (and `y`)
/// from position `n` on: any digits, digits at least `x`, at most `x`, and between `x` and `y`.
fn any_of_right_length(b: &mut Automaton, x: &[u8], n: usize) -> usize {
    let s = b.state(x.len() == n);
    if x.len() > n {
        let t = any_of_right_length(b, x, n + 1);
        b.edges[s].push((u32::from(b'0'), u32::from(b'9'), t));
    }
    s
}

fn at_least(
    b: &mut Automaton,
    x: &[u8],
    n: usize,
    initials: &mut Vec<usize>,
    zeros: bool,
) -> usize {
    let s = b.state(x.len() == n);
    if x.len() > n {
        if zeros {
            initials.push(s);
        }
        let c = x[n];
        let t = at_least(b, x, n + 1, initials, zeros && c == b'0');
        b.edges[s].push((c.into(), c.into(), t));
        if c < b'9' {
            let t = any_of_right_length(b, x, n + 1);
            b.edges[s].push((u32::from(c) + 1, u32::from(b'9'), t));
        }
    }
    s
}

fn at_most(b: &mut Automaton, x: &[u8], n: usize) -> usize {
    let s = b.state(x.len() == n);
    if x.len() > n {
        let c = x[n];
        let t = at_most(b, x, n + 1);
        b.edges[s].push((c.into(), c.into(), t));
        if c > b'0' {
            let t = any_of_right_length(b, x, n + 1);
            b.edges[s].push((u32::from(b'0'), u32::from(c) - 1, t));
        }
    }
    s
}

fn between(
    b: &mut Automaton,
    x: &[u8],
    y: &[u8],
    n: usize,
    initials: &mut Vec<usize>,
    zeros: bool,
) -> usize {
    let s = b.state(x.len() == n);
    if x.len() > n {
        if zeros {
            initials.push(s);
        }
        let (cx, cy) = (x[n], y[n]);
        if cx == cy {
            let t = between(b, x, y, n + 1, initials, zeros && cx == b'0');
            b.edges[s].push((cx.into(), cx.into(), t));
        } else {
            let t = at_least(b, x, n + 1, initials, zeros && cx == b'0');
            b.edges[s].push((cx.into(), cx.into(), t));
            let t = at_most(b, y, n + 1);
            b.edges[s].push((cy.into(), cy.into(), t));
            if cx + 1 < cy {
                let t = any_of_right_length(b, x, n + 1);
                b.edges[s].push((u32::from(cx) + 1, u32::from(cy) - 1, t));
            }
        }
    }
    s
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
        let mut n = self.intersection()?;
        while self.eat('|') {
            n = Node::Union(Box::new(n), Box::new(self.intersection()?));
        }
        Ok(n)
    }

    fn intersection(&mut self) -> Result<Node, String> {
        let mut n = self.concat()?;
        while self.eat('&') {
            n = Node::Intersection(Box::new(n), Box::new(self.concat()?));
        }
        Ok(n)
    }

    fn concat(&mut self) -> Result<Node, String> {
        let mut n = self.repeat()?;
        while self.more() && !self.peek(")|&") {
            n = concat_node(n, self.repeat()?);
        }
        Ok(n)
    }

    fn repeat(&mut self) -> Result<Node, String> {
        let mut n = self.complement()?;
        while self.peek("?*+{") {
            n = match self.next()? {
                '?' => Node::Optional(Box::new(n)),
                '*' => Node::Star(Box::new(n)),
                '+' => Node::RepeatMin(Box::new(n), 1),
                _ => {
                    let at = self.i;
                    let min = self.number()?;
                    let min = min.ok_or_else(|| format!("integer expected at position {at}"))?;
                    let max = match self.eat(',') {
                        true => self.number()?,
                        false => Some(min),
                    };
                    self.expect('}')?;
                    match max {
                        Some(max) if min > max => {
                            return Err(format!(
                                "invalid repetition range(out of order): {min}..{max}"
                            ));
                        }
                        Some(max) => Node::RepeatMinMax(Box::new(n), min, max),
                        None => Node::RepeatMin(Box::new(n), min),
                    }
                }
            };
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
            return Ok(Node::DeprecatedComplement(Box::new(self.complement()?)));
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
        // a single character is a literal, which joins adjacent ones into a string
        let mut n = match ranges[..] {
            [(a, b)] if a == b => Node::String(vec![a]),
            _ => Node::Class(ranges),
        };
        if negated {
            let any = Box::new(Node::Class(vec![(0, MAX)]));
            n = Node::Intersection(any, Box::new(Node::Complement(Box::new(n))));
        }
        self.expect(']')?;
        Ok(n)
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
        Ok(match self.next()? {
            '.' => Node::Class(vec![(0, MAX)]),
            '#' => Node::Class(vec![]),
            '@' => Node::Star(Box::new(Node::Class(vec![(0, MAX)]))),
            '"' => {
                let start = self.i;
                while self.more() && !self.peek("\"") {
                    self.i += 1;
                }
                self.expect('"')?;
                Node::String(
                    self.s[start..self.i - 1]
                        .iter()
                        .map(|&c| c.into())
                        .collect(),
                )
            }
            '(' if self.eat(')') => Node::String(vec![]),
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
            // a shorthand outside brackets is a class even when it is one character
            '\\' if self.at_shorthand() => Node::Class(self.shorthand()?),
            '\\' => Node::String(vec![self.next()?.into()]),
            c => Node::String(vec![c.into()]),
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

    #[test]
    fn query_gaps_2_regexp_effort_counts_lucenes_automaton() {
        // Elasticsearch 9.5.5's errors for these terms include patterns
        for (p, states, transitions) in [
            ("~((a|b)*a(a|b){20})", 22, 22),
            ("(a|b)*a(a|b){13}", 15, 15),
            (".*a.{13}", 15, 15),
            ("~(.*ab.*)&.*a.{13}", 29, 83),
        ] {
            assert_eq!(
                compile(p).unwrap_err(),
                format!(
                    "Determinizing automaton with {states} states and {transitions} transitions would require more than 10000 effort."
                )
            );
        }
        // and patterns it accepts, the first of which once counted past the limit
        for p in [
            "~(.*ab.*)&.*a.{12}",
            "(ab|b)*a(ab|b){13}",
            "(a|bc)*a(a|b){13}",
            "[^a]*a\"xyz\"<1-100>.{13}",
        ] {
            assert!(compile(p).is_ok(), "{p}");
        }
    }
}
