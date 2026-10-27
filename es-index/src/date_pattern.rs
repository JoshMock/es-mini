//! Java `DateTimeFormatter` patterns as Elasticsearch builds custom date formats:
//! `DateTimeFormatterBuilder.appendPattern` (JDK 24), parsed strictly, resolved with
//! `ResolverStyle.STRICT`, then completed like `DateFormatters.from` (missing fields default to
//! the start of the year, a missing date to 1970-01-01).

use crate::{days_from_civil, tz::TimeZone};

mod named;
pub use named::DateFormat;

/// The `ChronoField`s patterns can name.
#[derive(Clone, Copy, PartialEq, Debug)]
enum F {
    Era,
    YearOfEra,
    Year,
    Quarter,
    Month,
    DayOfYear,
    DayOfMonth,
    DayOfWeek,
    AmPm,
    HourOfDay,
    ClockHourOfDay,
    HourOfAmPm,
    ClockHourOfAmPm,
    Minute,
    Second,
    Nano,
    MilliOfDay,
    NanoOfDay,
}

impl F {
    /// The field's valid range, checked when resolving strictly.
    fn range(self) -> (i64, i64) {
        match self {
            F::Era | F::AmPm => (0, 1),
            F::YearOfEra => (1, 1_000_000_000),
            F::Year => (-999_999_999, 999_999_999),
            F::Quarter => (1, 4),
            F::Month => (1, 12),
            F::DayOfYear => (1, 366),
            F::DayOfMonth => (1, 31),
            F::DayOfWeek => (1, 7),
            F::HourOfDay => (0, 23),
            F::ClockHourOfDay => (1, 24),
            F::HourOfAmPm => (0, 11),
            F::ClockHourOfAmPm => (1, 12),
            F::Minute | F::Second => (0, 59),
            F::Nano => (0, 999_999_999),
            F::MilliOfDay => (0, 86_399_999),
            F::NanoOfDay => (0, 86_399_999_999_999),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Sign {
    Normal,
    ExceedsPad,
    NotNegative,
}

#[derive(Clone, Copy, Debug)]
enum Style {
    Short,
    Full,
    Narrow,
}

#[derive(Clone, Debug)]
enum Tok {
    Lit(String),
    /// NumberPrinterParser; `subsequent` digits are left to the fixed-width fields after it
    /// (adjacent value parsing); `reduced` is `yy`'s two digits based at 2000.
    Num {
        f: F,
        min: usize,
        max: usize,
        sign: Sign,
        subsequent: usize,
        reduced: bool,
    },
    /// FractionPrinterParser: `min` to `max` digits of the nano-of-second, after a `.` with
    /// `dot`.
    Fraction {
        min: usize,
        max: usize,
        dot: bool,
    },
    Text(F, Style),
    /// OffsetIdPrinterParser: an index into its PATTERNS (`+HH`, `+HHmm`, `+HH:mm`, `+HHMM`,
    /// `+HH:MM`, `+HHMMss`, `+HH:MM:ss`, `+HHMMSS`, `+HH:MM:SS`, `+HHmmss`, `+HH:mm:ss`) and
    /// the text of a zero offset.
    Offset(usize, &'static str),
    /// LocalizedOffsetIdPrinterParser (`GMT+08:00`, or short `GMT+8`).
    Gmt {
        full: bool,
    },
    /// ZoneIdPrinterParser (`VV`).
    ZoneId,
    Optional(Vec<Tok>),
}

/// A sequence of tokens and the value parser later fixed-width fields reserve digits from.
#[derive(Default)]
struct Seq {
    toks: Vec<Tok>,
    active: Option<usize>,
}

impl Seq {
    fn push(&mut self, t: Tok) {
        self.toks.push(t);
        self.active = None;
    }

    /// `DateTimeFormatterBuilder.appendValue(NumberPrinterParser)`.
    fn value(&mut self, t: Tok) {
        let width = match t {
            Tok::Num {
                min,
                max,
                sign: Sign::NotNegative,
                ..
            } if min == max => Some(max),
            Tok::Fraction {
                min,
                max,
                dot: false,
            } if min == max => Some(max),
            _ => None,
        };
        match (self.active, width) {
            (Some(i), Some(w)) => {
                if let Tok::Num { subsequent, .. } = &mut self.toks[i] {
                    *subsequent += w;
                }
                self.toks.push(t);
            }
            _ => {
                self.toks.push(t);
                self.active = Some(self.toks.len() - 1);
            }
        }
    }

    /// `DateTimeFormatterBuilder.parseField` and the other letters; `None` for letters Java
    /// rejects or es-mini lacks.
    // ponytail: no week-based fields (`Y w W e c F`), zone names (`z v`), quarter names
    // (`QQQ`), day periods (`B`), padding (`p`) or modified Julian days (`g`); add when needed.
    fn letter(&mut self, c: char, n: usize) -> Option<()> {
        let num = |f, min, max, sign| Tok::Num {
            f,
            min,
            max,
            sign,
            subsequent: 0,
            reduced: false,
        };
        let text = |f, n: usize| match n {
            1..=3 => Some(Tok::Text(f, Style::Short)),
            4 => Some(Tok::Text(f, Style::Full)),
            5 => Some(Tok::Text(f, Style::Narrow)),
            _ => None,
        };
        let one_or_two = |f| match n {
            1 => Some(num(f, 1, 19, Sign::Normal)),
            2 => Some(num(f, 2, 2, Sign::NotNegative)),
            _ => None,
        };
        // `X` and `x` index OFFSET_PATTERNS by `count + (count == 1 ? 0 : 1)`
        let offset = n + usize::from(n != 1);
        match c {
            'y' | 'u' => {
                let f = if c == 'y' { F::YearOfEra } else { F::Year };
                self.value(match n {
                    2 => Tok::Num {
                        f,
                        min: 2,
                        max: 2,
                        sign: Sign::NotNegative,
                        subsequent: 0,
                        reduced: true,
                    },
                    1 | 3 => num(f, n, 19, Sign::Normal),
                    4..=19 => num(f, n, 19, Sign::ExceedsPad),
                    _ => return None,
                });
            }
            'M' | 'L' if n <= 2 => self.value(one_or_two(F::Month)?),
            'M' | 'L' => self.push(text(F::Month, n)?),
            'Q' | 'q' => self.value(one_or_two(F::Quarter)?),
            'E' => self.push(text(F::DayOfWeek, n)?),
            'G' => self.push(text(F::Era, n)?),
            'a' if n == 1 => self.push(Tok::Text(F::AmPm, Style::Short)),
            'S' if n <= 9 => self.value(Tok::Fraction {
                min: n,
                max: n,
                dot: false,
            }),
            'd' => self.value(one_or_two(F::DayOfMonth)?),
            'h' => self.value(one_or_two(F::ClockHourOfAmPm)?),
            'H' => self.value(one_or_two(F::HourOfDay)?),
            'k' => self.value(one_or_two(F::ClockHourOfDay)?),
            'K' => self.value(one_or_two(F::HourOfAmPm)?),
            'm' => self.value(one_or_two(F::Minute)?),
            's' => self.value(one_or_two(F::Second)?),
            'D' => self.value(match n {
                1 => num(F::DayOfYear, 1, 19, Sign::Normal),
                2 | 3 => num(F::DayOfYear, n, 3, Sign::NotNegative),
                _ => return None,
            }),
            'A' | 'n' | 'N' if n <= 19 => {
                let f = match c {
                    'A' => F::MilliOfDay,
                    'n' => F::Nano,
                    _ => F::NanoOfDay,
                };
                self.value(num(f, n, 19, Sign::NotNegative));
            }
            'V' if n == 2 => self.push(Tok::ZoneId),
            'Z' => self.push(match n {
                1..=3 => Tok::Offset(3, "+0000"),
                4 => Tok::Gmt { full: true },
                5 => Tok::Offset(6, "Z"),
                _ => return None,
            }),
            'O' if n == 1 || n == 4 => self.push(Tok::Gmt { full: n == 4 }),
            'X' if n <= 5 => self.push(Tok::Offset(offset, "Z")),
            'x' if n <= 5 => {
                let zero = match n {
                    1 => "+00",
                    _ if n.is_multiple_of(2) => "+0000",
                    _ => "+00:00",
                };
                self.push(Tok::Offset(offset, zero));
            }
            _ => return None,
        }
        Some(())
    }
}

/// A compiled pattern, or a formatter DateFormatters builds; `lenient` resolves out-of-range
/// dates like `ResolverStyle.LENIENT` (only `strict_date` does).
#[derive(Clone, Debug)]
pub struct Pattern {
    toks: Vec<Tok>,
    lenient: bool,
}

/// The zone a parsed value gave.
#[derive(Clone)]
enum Zone {
    Offset(i64),
    Region(TimeZone),
}

/// Parsed field values, by `F as usize`.
#[derive(Clone, Default)]
struct Fields {
    v: [Option<i64>; 18],
    zone: Option<Zone>,
    lenient: bool,
}

impl Fields {
    fn get(&self, f: F) -> Option<i64> {
        self.v[f as usize]
    }

    /// Sets a field in its range (which strict resolving checks); a field set twice must get
    /// the same value (`Parsed.updateCheckConflict`).
    fn set(&mut self, f: F, x: i64) -> Option<()> {
        let (lo, hi) = f.range();
        if !self.lenient && !(lo..=hi).contains(&x) {
            return None;
        }
        match self.v[f as usize].replace(x) {
            Some(old) if old != x => None,
            _ => Some(()),
        }
    }

    fn take(&mut self, f: F) -> Option<i64> {
        self.v[f as usize].take()
    }
}

impl Pattern {
    /// Compiles a pattern like `appendPattern`; `None` where Java rejects it or es-mini lacks a
    /// letter (no week-based fields, zone names, quarter names, day periods or padding).
    pub fn new(pattern: &str) -> Option<Pattern> {
        let mut stack = vec![Seq::default()];
        append_pattern(&mut stack, pattern)?;
        // toFormatter closes open optional sections
        while stack.len() > 1 {
            let inner = stack.pop()?.toks;
            stack.last_mut()?.push(Tok::Optional(inner));
        }
        Some(Pattern {
            toks: stack.pop()?.toks,
            lenient: false,
        })
    }

    /// Epoch millis of `s`, its local time in `tz` unless it gives an offset or zone; with
    /// `round_up`, missing fields default to the end of their range like ES's round-up parser.
    pub fn parse(&self, s: &str, locale: &str, tz: &TimeZone, round_up: bool) -> Option<i64> {
        self.parse_wide(s, locale, tz, round_up)?.try_into().ok()
    }

    /// [`Pattern::parse`] before the epoch millis are checked to fit in a long.
    pub(crate) fn parse_wide(
        &self,
        s: &str,
        locale: &str,
        tz: &TimeZone,
        round_up: bool,
    ) -> Option<i128> {
        let mut p = Fields {
            lenient: self.lenient,
            ..Fields::default()
        };
        if parse_seq(&self.toks, s, 0, &mut p, locale)? != s.len() {
            return None;
        }
        if round_up {
            self.round_up_defaults(&mut p);
        }
        Some(instant(resolve(&mut p)?, p.zone.as_ref(), tz))
    }

    /// `JavaDateFormatter.defaultRoundUp`: `parseDefaulting` for the fields not parsed.
    fn round_up_defaults(&self, p: &mut Fields) {
        let mut has = vec![];
        fields(&self.toks, &mut has);
        let mut default = |f: F, x: i64| {
            p.v[f as usize].get_or_insert(x);
        };
        if has.contains(&F::DayOfYear) {
            default(F::DayOfYear, 1);
        } else {
            default(F::Month, 1);
            default(F::DayOfMonth, 1);
        }
        if has.contains(&F::ClockHourOfAmPm) {
            default(F::ClockHourOfAmPm, 11);
            default(F::AmPm, 1);
        } else if has.contains(&F::HourOfAmPm) {
            default(F::HourOfAmPm, 11);
            default(F::AmPm, 1);
        } else {
            default(F::HourOfDay, 23);
        }
        default(F::Minute, 59);
        default(F::Second, 59);
        default(F::Nano, 999_999_999);
    }

    /// Instant `t` (epoch millis) at `offset` (millis), or `None` for a value too wide for its
    /// field.
    pub fn format(&self, t: i64, offset: i64, locale: &str) -> Option<String> {
        let mut out = String::new();
        format_seq(&self.toks, t + offset, offset, locale, &mut out)?;
        Some(out)
    }
}

/// `DateTimeFormatterBuilder.appendPattern` onto the open sections in `stack`.
fn append_pattern(stack: &mut Vec<Seq>, pattern: &str) -> Option<()> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        i += 1;
        if c.is_ascii_alphabetic() {
            let n = 1 + chars[i..].iter().take_while(|&&x| x == c).count();
            i += n - 1;
            stack.last_mut()?.letter(c, n)?;
            continue;
        }
        match c {
            '\'' => {
                let mut lit = String::new();
                loop {
                    match *chars.get(i)? {
                        '\'' if chars.get(i + 1) == Some(&'\'') => {
                            lit.push('\'');
                            i += 2;
                        }
                        '\'' => break i += 1,
                        ch => {
                            lit.push(ch);
                            i += 1;
                        }
                    }
                }
                if lit.is_empty() {
                    lit.push('\'');
                }
                stack.last_mut()?.push(Tok::Lit(lit));
            }
            '[' => stack.push(Seq::default()),
            ']' if stack.len() > 1 => {
                let inner = stack.pop()?.toks;
                stack.last_mut()?.push(Tok::Optional(inner));
            }
            ']' | '{' | '}' | '#' => return None,
            c => stack.last_mut()?.push(Tok::Lit(c.into())),
        }
    }
    Some(())
}

/// The instant of local epoch millis `local` in the zone a value gave, else in `tz`. Past half
/// a long's range (beyond it, a value only names the instant in an error), a time zone's
/// offset is the one it has at that half.
fn instant(local: i128, zone: Option<&Zone>, tz: &TimeZone) -> i128 {
    let near = local.clamp((i64::MIN / 2).into(), (i64::MAX / 2).into()) as i64;
    local
        - i128::from(match zone {
            Some(Zone::Offset(o)) => *o,
            Some(Zone::Region(z)) => near - z.at_local(near, i64::MIN),
            None => near - tz.at_local(near, 0),
        })
}

/// The fields `toks` parse, as `DateTimeFormatter.toString` names them for `defaultRoundUp`.
fn fields(toks: &[Tok], out: &mut Vec<F>) {
    for t in toks {
        match t {
            Tok::Num { f, .. } | Tok::Text(f, _) => out.push(*f),
            Tok::Optional(inner) => fields(inner, out),
            _ => {}
        }
    }
}

/// Month, day-of-week (Monday first), AM/PM and era (BC first) names, `|`-separated, in the
/// JDK's CLDR data.
fn names(f: F, style: Style, locale: &str) -> Option<Vec<&'static str>> {
    // [short, full, narrow] for months, days, AM/PM and eras
    const EN: [[&str; 3]; 4] = [
        [
            "Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec",
            "January|February|March|April|May|June|July|August|September|October|November|December",
            "J|F|M|A|M|J|J|A|S|O|N|D",
        ],
        [
            "Mon|Tue|Wed|Thu|Fri|Sat|Sun",
            "Monday|Tuesday|Wednesday|Thursday|Friday|Saturday|Sunday",
            "M|T|W|T|F|S|S",
        ],
        ["AM|PM", "AM|PM", "a|p"],
        ["BC|AD", "Before Christ|Anno Domini", "B|A"],
    ];
    const FR: [[&str; 3]; 4] = [
        [
            "janv.|févr.|mars|avr.|mai|juin|juil.|août|sept.|oct.|nov.|déc.",
            "janvier|février|mars|avril|mai|juin|juillet|août|septembre|octobre|novembre|décembre",
            "J|F|M|A|M|J|J|A|S|O|N|D",
        ],
        [
            "lun.|mar.|mer.|jeu.|ven.|sam.|dim.",
            "lundi|mardi|mercredi|jeudi|vendredi|samedi|dimanche",
            "L|M|M|J|V|S|D",
        ],
        ["AM|PM", "AM|PM", "AM|PM"],
        [
            "av. J.-C.|ap. J.-C.",
            "avant Jésus-Christ|après Jésus-Christ",
            "av. J.-C.|ap. J.-C.",
        ],
    ];
    // ponytail: names for the root, English and French locales only; add CLDR tables per locale
    let lang = locale.split(['-', '_']).next()?.to_ascii_lowercase();
    let table = match lang.as_str() {
        "" | "root" | "en" => &EN,
        "fr" => &FR,
        _ => return None,
    };
    let row = match f {
        F::Month => 0,
        F::DayOfWeek => 1,
        F::AmPm => 2,
        _ => 3,
    };
    Some(table[row][style as usize].split('|').collect())
}

/// The first value of a text field.
fn first_value(f: F) -> i64 {
    match f {
        F::Month | F::DayOfWeek => 1,
        _ => 0,
    }
}

fn parse_seq(toks: &[Tok], s: &str, mut pos: usize, p: &mut Fields, locale: &str) -> Option<usize> {
    for t in toks {
        let rest = &s[pos..];
        pos += match t {
            Tok::Lit(l) => rest.starts_with(l.as_str()).then_some(l.len())?,
            &Tok::Num {
                f,
                min,
                max,
                sign,
                subsequent,
                reduced,
            } => {
                let (v, n) = parse_num(rest.as_bytes(), min, max, sign, subsequent)?;
                p.set(f, if reduced { 2000 + v } else { v })?;
                n
            }
            &Tok::Fraction { min, max, dot } => {
                let start = match dot {
                    true if !rest.starts_with('.') => (min == 0).then_some(0)?,
                    true => 1,
                    false => 0,
                };
                let digits = rest[start..]
                    .bytes()
                    .take(max)
                    .take_while(u8::is_ascii_digit)
                    .count();
                if digits < min {
                    return None;
                }
                if digits > 0 || dot && start == 1 {
                    let d = &rest[start..start + digits];
                    p.set(F::Nano, format!("{d:0<9}").parse().unwrap_or(0))?;
                }
                start + digits
            }
            &Tok::Text(f, style) => {
                let names = names(f, style, locale)?;
                let (i, name) = (names.iter().enumerate())
                    .filter(|(_, n)| rest.starts_with(*n))
                    .max_by_key(|(i, n)| (n.len(), std::cmp::Reverse(*i)))?;
                p.set(f, first_value(f) + i as i64)?;
                name.len()
            }
            &Tok::Offset(pattern, zero) => {
                let (o, n) = parse_offset(rest, pattern, zero)?;
                set_zone(p, Zone::Offset(o))?;
                n
            }
            &Tok::Gmt { full } => {
                let (o, n) = parse_gmt(rest, full)?;
                set_zone(p, Zone::Offset(o))?;
                n
            }
            Tok::ZoneId => {
                let ok = |c: char| c.is_ascii_alphanumeric() || "_/+-:".contains(c);
                let end = rest.find(|c| !ok(c)).unwrap_or(rest.len());
                // ZoneIdPrinterParser takes the longest known ID
                let (n, zone) = (1..=end)
                    .rev()
                    .find_map(|n| Some((n, TimeZone::parse(&rest[..n])?)))?;
                set_zone(p, Zone::Region(zone))?;
                n
            }
            Tok::Optional(inner) => {
                let mut q = p.clone();
                match parse_seq(inner, s, pos, &mut q, locale) {
                    Some(end) => {
                        *p = q;
                        end - pos
                    }
                    None => 0,
                }
            }
        };
    }
    Some(pos)
}

fn set_zone(p: &mut Fields, zone: Zone) -> Option<()> {
    match &p.zone {
        Some(z) => match (z, &zone) {
            (Zone::Offset(a), Zone::Offset(b)) if a == b => Some(()),
            (Zone::Region(a), Zone::Region(b)) if a == b => Some(()),
            _ => None,
        },
        None => {
            p.zone = Some(zone);
            Some(())
        }
    }
}

/// `NumberPrinterParser.parse` in strict mode: the value and the bytes it took.
fn parse_num(
    s: &[u8],
    min: usize,
    max: usize,
    sign: Sign,
    subsequent: usize,
) -> Option<(i64, usize)> {
    let (negative, positive) = match s.first()? {
        b'+' if sign == Sign::ExceedsPad => (false, true),
        b'-' if sign != Sign::NotNegative => (true, false),
        b'+' | b'-' => return None,
        _ => (false, false),
    };
    let start = usize::from(negative || positive);
    let digits = s[start..].iter().take_while(|b| b.is_ascii_digit()).count();
    let mut n = digits.min(max + subsequent);
    if n < min {
        return None;
    }
    if subsequent > 0 {
        n = min.max(n.saturating_sub(subsequent));
    }
    let text = std::str::from_utf8(&s[start..start + n]).ok()?;
    // Java parses one digit less when the value would overflow a long
    let (mut v, n) = match text.parse::<i64>() {
        Ok(v) => (v, n),
        Err(_) => (text[..n - 1].parse().ok()?, n - 1),
    };
    if negative {
        if v == 0 {
            return None;
        }
        v = -v;
    } else if sign == Sign::ExceedsPad && positive != (n > min) {
        return None;
    }
    Some((v, start + n))
}

/// `OffsetIdPrinterParser.parse` in strict mode for a padded-hour pattern: millis and bytes taken.
fn parse_offset(s: &str, pattern: usize, zero: &str) -> Option<(i64, usize)> {
    if s.starts_with(zero) {
        return Some((0, zero.len()));
    }
    let sign = match s.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let colon = pattern > 0 && pattern.is_multiple_of(2);
    let b = s.as_bytes();
    let mut pos = 1;
    // two digits under 60, after a colon unless they are the hours
    let mut two = |first: bool| -> Option<i64> {
        let at = pos + usize::from(colon && !first);
        if at > pos && b.get(pos) != Some(&b':') {
            return None;
        }
        let v: i64 = s.get(at..at + 2)?.parse().ok()?;
        if v > 59 || !b[at..at + 2].iter().all(u8::is_ascii_digit) {
            return None;
        }
        pos = at + 2;
        Some(v)
    };
    let h = two(true)?;
    let (mut m, mut sec) = (0, 0);
    match pattern {
        0 => {}
        1 | 2 => m = two(false).unwrap_or(0),
        3 | 4 => m = two(false)?,
        5 | 6 => {
            m = two(false)?;
            sec = two(false).unwrap_or(0);
        }
        7 | 8 => {
            m = two(false)?;
            sec = two(false)?;
        }
        _ => {
            if let Some(x) = two(false) {
                m = x;
                sec = two(false).unwrap_or(0);
            }
        }
    }
    (h <= 23).then_some((sign * ((h * 60 + m) * 60 + sec) * 1000, pos))
}

/// `LocalizedOffsetIdPrinterParser.parse`: `GMT`, then `+HH:MM[:SS]` (full) or `+H[:MM[:SS]]`.
fn parse_gmt(s: &str, full: bool) -> Option<(i64, usize)> {
    let rest = s.strip_prefix("GMT")?;
    let b = rest.as_bytes();
    let sign = match b.first() {
        Some(b'+') => 1,
        Some(b'-') => -1,
        _ => return Some((0, 3)),
    };
    let hd = b[1..]
        .iter()
        .take(2)
        .take_while(|c| c.is_ascii_digit())
        .count();
    if hd == 0 || full && hd < 2 {
        return None;
    }
    let mut parts = [rest[1..1 + hd].parse::<i64>().ok()?, 0, 0];
    let mut pos = 1 + hd;
    for part in &mut parts[1..] {
        match b.get(pos..pos + 3) {
            Some([b':', x, y]) if x.is_ascii_digit() && y.is_ascii_digit() => {
                *part = i64::from((x - b'0') * 10 + y - b'0');
                pos += 3;
            }
            _ if full && pos == 3 => return None,
            _ => break,
        }
    }
    let [h, m, sec] = parts;
    (h <= 23 && m <= 59 && sec <= 59).then_some((sign * ((h * 60 + m) * 60 + sec) * 1000, 3 + pos))
}

fn days_in_month(y: i64, m: i64) -> i64 {
    days_from_civil(y + i64::from(m == 12), m % 12 + 1, 1) - days_from_civil(y, m, 1)
}

/// Days since the epoch of the date `LocalDate.of` (or else `ofYearDay`) makes of the fields:
/// `Some(None)` when they make none, `None` when it throws.
fn date(y: i64, m: Option<i64>, d: Option<i64>, doy: Option<i64>) -> Option<Option<i64>> {
    let jan1 = days_from_civil(y, 1, 1);
    match (m, d, doy) {
        (Some(m), Some(d), _) => (d <= days_in_month(y, m)).then(|| Some(days_from_civil(y, m, d))),
        (_, _, Some(doy)) => {
            (jan1 + doy <= days_from_civil(y + 1, 1, 1)).then_some(Some(jan1 + doy - 1))
        }
        _ => Some(None),
    }
}

/// Local epoch millis of parsed fields: `Parsed.resolve` with ResolverStyle.STRICT, then
/// `DateFormatters.from`; `None` where either throws.
fn resolve(p: &mut Fields) -> Option<i128> {
    // IsoChronology.resolveYMD when lenient: Jan 1, plus months, plus days
    if let (true, Some(y), Some(m), Some(d)) = (
        p.lenient,
        p.get(F::Year),
        p.get(F::Month),
        p.get(F::DayOfMonth),
    ) {
        let months = y.checked_mul(12)?.checked_add(m - 1)?;
        let days = days_from_civil(months.div_euclid(12), months.rem_euclid(12) + 1, 1);
        let (y, m, d) = crate::civil_from_days(days.checked_add(d - 1)?);
        p.v[F::Year as usize] = Some(y);
        p.v[F::Month as usize] = Some(m);
        p.v[F::DayOfMonth as usize] = Some(d);
    }
    // IsoChronology.resolveYearOfEra: no era is invented when strict
    if let Some(yoe) = p.take(F::YearOfEra) {
        match (p.take(F::Era), p.get(F::Year)) {
            (Some(era), _) => p.set(F::Year, if era == 1 { yoe } else { 1 - yoe })?,
            (None, Some(y)) => p.set(F::Year, if y > 0 { yoe } else { 1 - yoe })?,
            (None, None) => p.set(F::YearOfEra, yoe)?,
        }
    }
    p.take(F::Era);
    // the date java.time resolves (with a proleptic year) is cross-checked against other fields
    let (m, d, doy) = (p.get(F::Month), p.get(F::DayOfMonth), p.get(F::DayOfYear));
    let java_date = match p.get(F::Year) {
        Some(y) => date(y, m, d, doy)?,
        None => None,
    };
    if let Some(days) = java_date {
        let (y, cm, cd) = crate::civil_from_days(days);
        let checks = [
            (F::Month, cm),
            (F::DayOfMonth, cd),
            (F::DayOfYear, days - days_from_civil(y, 1, 1) + 1),
            (F::DayOfWeek, (days + 3).rem_euclid(7) + 1),
            (F::Quarter, (cm - 1) / 3 + 1),
        ];
        if checks
            .iter()
            .any(|&(f, x)| p.get(f).is_some_and(|v| v != x))
        {
            return None;
        }
    }
    // Parsed.resolveTimeFields
    if let Some(ch) = p.take(F::ClockHourOfDay) {
        p.set(F::HourOfDay, ch % 24)?;
    }
    if let Some(ch) = p.take(F::ClockHourOfAmPm) {
        p.set(F::HourOfAmPm, ch % 12)?;
    }
    if let (Some(ap), Some(hap)) = (p.get(F::AmPm), p.get(F::HourOfAmPm)) {
        p.take(F::AmPm);
        p.take(F::HourOfAmPm);
        p.set(F::HourOfDay, ap * 12 + hap)?;
    }
    if let Some(nod) = p.take(F::NanoOfDay) {
        p.set(F::HourOfDay, nod / 3_600_000_000_000)?;
        p.set(F::Minute, nod / 60_000_000_000 % 60)?;
        p.set(F::Second, nod / 1_000_000_000 % 60)?;
        p.set(F::Nano, nod % 1_000_000_000)?;
    }
    if let Some(lod) = p.take(F::MilliOfDay) {
        p.set(F::HourOfDay, lod / 3_600_000)?;
        p.set(F::Minute, lod / 60_000 % 60)?;
        p.set(F::Second, lod / 1000 % 60)?;
        let nos = p.get(F::Nano).unwrap_or(0);
        p.set(F::Nano, lod % 1000 * 1_000_000 + nos % 1_000_000)?;
    }
    // Parsed.resolveTimeLenient: only an hour (with leading minutes, seconds) makes a time
    let (mi, sec, nano) = (p.get(F::Minute), p.get(F::Second), p.get(F::Nano));
    let time = p.get(F::HourOfDay).filter(|_| {
        !(mi.is_none() && (sec.is_some() || nano.is_some())
            || mi.is_some() && sec.is_none() && nano.is_some())
    });
    let time = time.map(|h| {
        ((h * 60 + mi.unwrap_or(0)) * 60 + sec.unwrap_or(0)) * 1000 + nano.unwrap_or(0) / 1_000_000
    });
    if let Some(t) = time {
        let h = t / 3_600_000;
        let checks = [(F::AmPm, h / 12), (F::HourOfAmPm, h % 12)];
        if checks
            .iter()
            .any(|&(f, x)| p.get(f).is_some_and(|v| v != x))
        {
            return None;
        }
    }
    // DateFormatters.from, with LOCAL_DATE_QUERY reading a year-of-era as the year
    let year = p.get(F::Year).or(p.get(F::YearOfEra));
    let local_date = match (java_date, year) {
        (Some(days), _) => Some(days),
        (None, Some(y)) => date(y, m, d, doy)?,
        (None, None) => None,
    };
    let days = match (local_date, time, year, m) {
        (Some(days), ..) => days,
        (None, Some(_), y, Some(m)) | (None, None, y @ None, Some(m)) => {
            date(y.unwrap_or(1970), Some(m), Some(d.unwrap_or(1)), None)??
        }
        (None, Some(_), ..) => 0,
        (None, None, Some(y), m) => days_from_civil(y, m.unwrap_or(1), 1),
        (None, None, None, None) => return None,
    };
    Some(i128::from(days) * 86_400_000 + i128::from(time.unwrap_or(0)))
}

/// The value of field `f` at local epoch millis `t`.
fn field_at(f: F, t: i64) -> i64 {
    let (days, ms) = (t.div_euclid(86_400_000), t.rem_euclid(86_400_000));
    let (y, m, d) = crate::civil_from_days(days);
    let h = ms / 3_600_000;
    match f {
        F::Era => i64::from(y > 0),
        F::YearOfEra if y > 0 => y,
        F::YearOfEra => 1 - y,
        F::Year => y,
        F::Quarter => (m - 1) / 3 + 1,
        F::Month => m,
        F::DayOfYear => days - days_from_civil(y, 1, 1) + 1,
        F::DayOfMonth => d,
        F::DayOfWeek => (days + 3).rem_euclid(7) + 1,
        F::AmPm => h / 12,
        F::HourOfDay => h,
        F::ClockHourOfDay if h == 0 => 24,
        F::ClockHourOfDay => h,
        F::HourOfAmPm => h % 12,
        F::ClockHourOfAmPm if h % 12 == 0 => 12,
        F::ClockHourOfAmPm => h % 12,
        F::Minute => ms / 60_000 % 60,
        F::Second => ms / 1000 % 60,
        F::Nano => ms % 1000 * 1_000_000,
        F::MilliOfDay => ms,
        F::NanoOfDay => ms * 1_000_000,
    }
}

fn format_seq(toks: &[Tok], t: i64, offset: i64, locale: &str, out: &mut String) -> Option<()> {
    for tok in toks {
        match tok {
            Tok::Lit(l) => out.push_str(l),
            &Tok::Num {
                f,
                min,
                max,
                sign,
                reduced,
                ..
            } => {
                let v = field_at(f, t);
                let v = if reduced { v.abs() % 100 } else { v };
                let digits = v.unsigned_abs().to_string();
                if digits.len() > max {
                    return None;
                }
                if v < 0 {
                    (sign != Sign::NotNegative).then_some(())?;
                    out.push('-');
                } else if sign == Sign::ExceedsPad && min < 19 && digits.len() > min {
                    out.push('+');
                }
                out.extend(std::iter::repeat_n('0', min.saturating_sub(digits.len())));
                out.push_str(&digits);
            }
            &Tok::Fraction { min, max, dot } => {
                // trailing zeros are dropped down to `min` digits
                let nanos = format!("{:09}", field_at(F::Nano, t));
                let n = nanos.trim_end_matches('0').len().clamp(min, max);
                if n > 0 && dot {
                    out.push('.');
                }
                out.push_str(&nanos[..n]);
            }
            &Tok::Text(f, style) => {
                let i = field_at(f, t) - first_value(f);
                out.push_str(names(f, style, locale)?[i as usize]);
            }
            &Tok::Offset(pattern, zero) => format_offset(offset, pattern, zero, out),
            &Tok::Gmt { full } => {
                out.push_str("GMT");
                if offset != 0 {
                    let a = offset.abs() / 1000;
                    let (h, m, s) = (a / 3600, a / 60 % 60, a % 60);
                    out.push(if offset < 0 { '-' } else { '+' });
                    match full {
                        true => out.push_str(&format!("{h:02}:{m:02}")),
                        false => {
                            out.push_str(&h.to_string());
                            if m != 0 || s != 0 {
                                out.push_str(&format!(":{m:02}"));
                            }
                        }
                    }
                    if s != 0 {
                        out.push_str(&format!(":{s:02}"));
                    }
                }
            }
            // ponytail: a region zone prints as its offset's ID (`+HH:MM:ss`); keep the zone's
            // ID in TimeZone to print `Europe/Paris`
            Tok::ZoneId => format_offset(offset, 6, "Z", out),
            Tok::Optional(inner) => format_seq(inner, t, offset, locale, out)?,
        }
    }
    Some(())
}

/// `OffsetIdPrinterParser.format` for a padded-hour pattern.
fn format_offset(offset: i64, pattern: usize, zero: &str, out: &mut String) {
    let total = offset / 1000;
    let (h, m, s) = (
        (total / 3600 % 100).abs(),
        (total / 60 % 60).abs(),
        (total % 60).abs(),
    );
    let colon = if pattern > 0 && pattern.is_multiple_of(2) {
        ":"
    } else {
        ""
    };
    let mut text = format!("{}{h:02}", if total < 0 { '-' } else { '+' });
    let mut printed = h;
    if (3..=8).contains(&pattern) || pattern >= 9 && s > 0 || pattern >= 1 && m > 0 {
        text += &format!("{colon}{m:02}");
        printed += m;
        if pattern == 7 || pattern == 8 || pattern >= 5 && s > 0 {
            text += &format!("{colon}{s:02}");
            printed += s;
        }
    }
    out.push_str(if printed == 0 { zero } else { &text });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Outputs and values checked against Elasticsearch 9.5.5.
    #[test]
    fn date_formats_patterns_match_elasticsearch() {
        let t = 1_619_899_200_000; // 2021-05-01T20:00:00Z
        let print = |p: &str, zone: &str| {
            let tz = TimeZone::parse(zone).unwrap();
            Pattern::new(p).unwrap().format(t, tz.offset(t), "")
        };
        let cases = [
            (
                "[uuuu]G hh a O|ZZZZ|ZZZZZ|X|x|xxxxx|D|DDD|k|K|A|n|QQ|q",
                "UTC",
                "2021AD 08 PM GMT|GMT|Z|Z|+00|+00:00|121|121|20|8|72000000|0|02|2",
            ),
            (
                "X|XX|XXX|XXXX|O|OOOO|Z|xxxx",
                "Asia/Kolkata",
                "+0530|+0530|+05:30|+0530|GMT+5:30|GMT+05:30|+0530|+0530",
            ),
            (
                "LLLL MMMMM EEEEE GGGG yy y yyyyyy uuu",
                "Europe/Paris",
                "May M S Anno Domini 21 2021 002021 2021",
            ),
        ];
        for (p, zone, want) in cases {
            assert_eq!(print(p, zone).as_deref(), Some(want), "{p}");
        }
        let parse = |p: &str, s: &str| Pattern::new(p)?.parse(s, "", &TimeZone::UTC, false);
        let day = 1_619_827_200_000; // 2021-05-01
        // a year-of-era without an era leaves the date unresolved by java.time, so the day
        // of the week goes unchecked
        assert_eq!(
            parse("E, d MMM yyyy", "Fri, 6 Dec 2000"),
            Some(976_060_800_000)
        );
        // a clock hour without AM/PM makes no time
        assert_eq!(parse("yyyy-MM-dd hh:mm", "2021-05-01 08:30"), Some(day));
        assert_eq!(parse("yyyy-MM-dd HH:mm a", "2021-05-01 20:00 AM"), None);
        assert_eq!(parse("dd MMM yyyy", "01 MAY 2021"), None);
        assert_eq!(parse("MMMM d", "May 3"), Some(10_540_800_000));
    }
}
