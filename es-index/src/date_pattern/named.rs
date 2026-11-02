//! Elasticsearch's named date formats (`DateFormatters.forPattern`), built from the same
//! `DateTimeFormatterBuilder` calls, and its `Iso8601Parser` for the formats that parse with it.

use super::{
    F::{self, *},
    Pattern, Seq, Sign,
    Sign::*,
    Tok, Week, Zone, append_pattern, days_in_month, instant,
};
use crate::{days_from_civil, tz::TimeZone};

/// One `DateTimeFormatterBuilder` call.
#[derive(Clone, Copy)]
enum B {
    /// `appendValue(field, min, max, sign)`
    V(F, usize, usize, Sign),
    L(&'static str),
    /// `appendFraction(NANO_OF_SECOND, min, max, decimalPoint)`
    Frac(usize, usize, bool),
    /// `appendOffset` by its index in OFFSET_PATTERNS, with `Z` for zero.
    Off(usize),
    /// `appendZoneOrOffsetId` or `appendZoneId`
    Zone,
    /// `appendPattern`, or a whole `ofPattern` formatter
    P(&'static str),
    /// `append(formatter)`
    A(&'static [B]),
    /// `optionalStart()`, these calls, `optionalEnd()`
    Opt(&'static [B]),
}
use B::*;

const NN: Sign = NotNegative;
/// `appendOffset("+HH:MM", "Z")`
const OFFSET: B = Off(4);
/// TIME_ZONE_FORMATTER_NO_COLON: `appendOffset("+HHmm", "Z")`
const OFFSET_NO_COLON: B = Off(1);
/// `appendValue(field)`
const fn v(f: F) -> B {
    V(f, 1, 19, Normal)
}

const ISO_LOCAL_DATE: &[B] = &[
    V(Year, 4, 10, ExceedsPad),
    L("-"),
    V(Month, 2, 2, NN),
    L("-"),
    V(DayOfMonth, 2, 2, NN),
];
const STRICT_YEAR_MONTH_DAY_PRINTER: &[B] = &[
    V(Year, 4, 9, ExceedsPad),
    Opt(&[
        L("-"),
        V(Month, 2, 2, NN),
        Opt(&[L("-"), V(DayOfMonth, 2, 2, NN)]),
    ]),
];
const STRICT_YEAR_MONTH_DAY: &[B] = &[
    V(Year, 4, 4, ExceedsPad),
    Opt(&[
        L("-"),
        V(Month, 2, 2, NN),
        Opt(&[L("-"), V(DayOfMonth, 2, 2, NN)]),
    ]),
];
const STRICT_HOUR_MINUTE_SECOND: &[B] = &[
    V(HourOfDay, 2, 2, NN),
    L(":"),
    V(Minute, 2, 2, NN),
    L(":"),
    V(Second, 2, 2, NN),
];
macro_rules! strict_date_optional_time_printer {
    ($fraction:expr) => {
        &[
            A(STRICT_YEAR_MONTH_DAY_PRINTER),
            L("T"),
            Opt(&[
                V(HourOfDay, 2, 2, NN),
                Opt(&[
                    L(":"),
                    V(Minute, 2, 2, NN),
                    Opt(&[L(":"), V(Second, 2, 2, NN), Opt(&[$fraction])]),
                    Opt(&[OFFSET]),
                ]),
            ]),
        ]
    };
}
const STRICT_DATE_OPTIONAL_TIME_PRINTER: &[B] =
    strict_date_optional_time_printer!(Frac(3, 3, true));
const STRICT_DATE_OPTIONAL_TIME_PRINTER_NANOS: &[B] =
    strict_date_optional_time_printer!(Frac(3, 9, true));
const STRICT_HOUR_MINUTE_SECOND_MILLIS: &[B] = &[A(STRICT_HOUR_MINUTE_SECOND), Frac(1, 9, true)];
const STRICT_HOUR_MINUTE_SECOND_MILLIS_PRINTER: &[B] =
    &[A(STRICT_HOUR_MINUTE_SECOND), Frac(3, 3, true)];
const STRICT_DATE_HOUR_MINUTE_SECOND_MILLIS_PRINTER: &[B] = &[
    A(STRICT_YEAR_MONTH_DAY),
    L("T"),
    A(STRICT_HOUR_MINUTE_SECOND_MILLIS_PRINTER),
];
const STRICT_ORDINAL_DATE_TIME_NO_MILLIS_BASE: &[B] = &[
    V(Year, 4, 4, ExceedsPad),
    L("-"),
    V(DayOfYear, 3, 3, NN),
    L("T"),
    A(STRICT_HOUR_MINUTE_SECOND),
];
macro_rules! strict_ordinal_date_time {
    ($fraction:expr) => {
        &[
            V(Year, 4, 4, ExceedsPad),
            L("-"),
            V(DayOfYear, 3, 3, NN),
            L("T"),
            P("HH:mm"),
            Opt(&[L(":"), V(Second, 2, 2, NN), $fraction]),
        ]
    };
}
const STRICT_ORDINAL_DATE_TIME_PRINTER: &[B] = strict_ordinal_date_time!(Frac(3, 9, true));
const STRICT_ORDINAL_DATE_TIME_BASE: &[B] = strict_ordinal_date_time!(Frac(1, 9, true));
const STRICT_TIME_BASE: &[B] = STRICT_HOUR_MINUTE_SECOND_MILLIS;
const STRICT_TIME_PRINTER: &[B] = STRICT_HOUR_MINUTE_SECOND_MILLIS_PRINTER;
const BASIC_TIME_NO_MILLIS_BASE: &[B] = &[
    V(HourOfDay, 2, 2, NN),
    V(Minute, 2, 2, NN),
    V(Second, 2, 2, NN),
];
const BASIC_TIME: &[B] = &[A(BASIC_TIME_NO_MILLIS_BASE), Frac(1, 9, true)];
const BASIC_TIME_PRINTER: &[B] = &[A(BASIC_TIME_NO_MILLIS_BASE), Frac(3, 3, true)];
const BASIC_T_TIME: &[B] = &[L("T"), A(BASIC_TIME)];
const BASIC_T_TIME_PRINTER: &[B] = &[L("T"), A(BASIC_TIME_PRINTER)];
const BASIC_YEAR_MONTH_DAY_PRINTER: &[B] = &[
    V(Year, 4, 10, Normal),
    V(Month, 2, 2, NN),
    V(DayOfMonth, 2, 2, NN),
];
const BASIC_YEAR_MONTH_DAY: &[B] = &[
    V(Year, 4, 4, Normal),
    V(Month, 2, 2, NN),
    V(DayOfMonth, 2, 2, NN),
];
const DATE: &[B] = &[
    V(Year, 1, 9, Normal),
    Opt(&[
        L("-"),
        V(Month, 1, 2, NN),
        Opt(&[L("-"), V(DayOfMonth, 1, 2, NN)]),
    ]),
];
const HOUR_MINUTE: &[B] = &[V(HourOfDay, 1, 2, NN), L(":"), V(Minute, 1, 2, NN)];
const HOUR_MINUTE_SECOND: &[B] = &[A(HOUR_MINUTE), L(":"), V(Second, 1, 2, NN)];
const HOUR_MINUTE_SECOND_MILLIS: &[B] = &[A(HOUR_MINUTE_SECOND), Frac(1, 3, true)];
const HOUR_MINUTE_SECOND_FRACTION: &[B] = &[A(HOUR_MINUTE_SECOND), Frac(1, 9, true)];
const ORDINAL_DATE: &[B] = &[V(Year, 4, 10, ExceedsPad), L("-"), V(DayOfYear, 1, 3, NN)];
const ORDINAL_DATE_PRINTER: &[B] = &[V(Year, 4, 10, ExceedsPad), L("-"), V(DayOfYear, 3, 3, NN)];
const TIME_NO_MILLIS: &[B] = HOUR_MINUTE_SECOND;
const TIME_PREFIX: &[B] = HOUR_MINUTE_SECOND_FRACTION;
const DATE_TIME: &[B] = &[
    A(DATE),
    L("T"),
    A(HOUR_MINUTE),
    Opt(&[L(":"), V(Second, 1, 2, NN), Frac(1, 9, true)]),
];
const DATE_TIME_PREFIX: &[B] = &[
    A(DATE),
    L("T"),
    A(HOUR_MINUTE),
    Opt(&[L(":"), V(Second, 1, 2, NN)]),
];
const ORDINAL_DATE_TIME_BASE: &[B] = &[
    A(ORDINAL_DATE),
    L("T"),
    A(HOUR_MINUTE),
    Opt(&[L(":"), V(Second, 1, 2, NN), Frac(1, 9, true)]),
];
const ORDINAL_DATE_TIME_NO_MILLIS_BASE: &[B] = &[A(ORDINAL_DATE), L("T"), A(HOUR_MINUTE_SECOND)];

/// `Iso8601Parser`'s fields, by index: year, month, day, hour, minute, second, nano.
const YEAR: usize = 0;
const MONTH: usize = 1;
const DAY: usize = 2;
const HOUR: usize = 3;
const MINUTE: usize = 4;
const SECOND: usize = 5;
const NANO: usize = 6;

/// `TimezonePresence`
#[derive(Clone, Copy, PartialEq, Debug)]
enum Tz {
    Optional,
    Mandatory,
    Forbidden,
}

/// `Iso8601Parser`: `yyyy[-MM[-dd[THH[:mm[:ss[.S+]]]]]]` and a zone, without the
/// `DateTimeFormatter` machinery.
#[derive(Clone, Debug)]
struct Iso {
    /// The fields after the year that must be present, as a bit per field index.
    mandatory: u8,
    /// A date may end at the day though hours are mandatory.
    optional_time: bool,
    /// The last field the value may give.
    max: Option<usize>,
    /// Fractions may follow a `,` as well as a `.`.
    comma: bool,
    tz: Tz,
}

impl Iso {
    /// Fields `from` to `to` (inclusive; none if `to < from`) are mandatory.
    const fn new(from: usize, to: usize, optional_time: bool, max: Option<usize>) -> Iso {
        Iso {
            mandatory: ((1u16 << (to + 1)) - (1 << from)) as u8,
            optional_time,
            max,
            comma: true,
            tz: Tz::Optional,
        }
    }

    const fn dot(mut self) -> Iso {
        self.comma = false;
        self
    }

    const fn tz(mut self, tz: Tz) -> Iso {
        self.tz = tz;
        self
    }

    fn parse(&self, s: &str, tz: &TimeZone, round_up: bool) -> Option<i128> {
        // a negative year parses as a positive one, then is negated
        let (negative, s) = match s.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, s),
        };
        let ([y, m, d, h, mi, sec, nano], zone) = self.fields(s, round_up)?;
        let y = if negative { -y? } else { y? };
        // DateTime's LocalDate.of, then DateFormatters.from
        let days = match (m, d) {
            (Some(m), d) => {
                let d = d.unwrap_or(1);
                ((1..=12).contains(&m) && (1..=days_in_month(y, m)).contains(&d))
                    .then(|| days_from_civil(y, m, d))?
            }
            (None, _) => days_from_civil(y, 1, 1),
        };
        let time = h.map_or(0, |h| {
            ((h * 60 + mi.unwrap_or(0)) * 60 + sec.unwrap_or(0)) * 1000
                + nano.unwrap_or(0) / 1_000_000
        });
        let local = i128::from(days) * 86_400_000 + i128::from(time);
        Some(instant(local, zone.as_ref(), tz))
    }

    /// `Iso8601Parser.parse`: the fields of `s` (a year not negative) and its zone. Fields
    /// left out are `None`, or rounding up, the ends of their ranges; after an hour, minutes
    /// and smaller default to zero.
    fn fields(&self, s: &str, round_up: bool) -> Option<([Option<i64>; 7], Option<Zone>)> {
        const UP: [i64; 7] = [0, 1, 1, 23, 59, 59, 999_999_999];
        let b = s.as_bytes();
        let len = b.len();
        let int = |from: usize, to: usize| -> Option<i64> {
            (b.get(from..to)?.iter()).try_fold(0, |a, &c| {
                c.is_ascii_digit().then(|| a * 10 + i64::from(c - b'0'))
            })
        };
        let optional = |i: usize| self.mandatory & (1 << i) == 0;
        let mut v = [None; 7];
        let done = |mut v: [Option<i64>; 7], from: usize, zero: bool, zone| {
            for (i, x) in v.iter_mut().enumerate().skip(from) {
                *x = match round_up {
                    true => Some(UP[i]),
                    false => zero.then_some(0),
                };
            }
            (v, zone)
        };
        v[0] = Some(int(0, 4)?);
        if len == 4 {
            return optional(MONTH).then(|| done(v, MONTH, false, None));
        }
        if b[4] != b'-' || self.max == Some(YEAR) {
            return None;
        }
        v[MONTH] = Some(int(5, 7).filter(|&m| m <= 12)?);
        if len == 7 {
            return optional(DAY).then(|| done(v, DAY, false, None));
        }
        if b[7] != b'-' || self.max == Some(MONTH) {
            return None;
        }
        v[DAY] = Some(int(8, 10).filter(|&d| d <= 31)?);
        if len == 10 {
            return (self.optional_time || optional(HOUR)).then(|| done(v, HOUR, false, None));
        }
        if b[10] != b'T' || self.max == Some(DAY) {
            return None;
        }
        if len == 11 {
            return optional(HOUR).then(|| done(v, HOUR, false, None));
        }
        v[HOUR] = Some(int(11, 13).filter(|&h| h <= 23)?);
        // a zone or a separator after the hour, minute and second
        for (at, next) in [(13, MINUTE), (16, SECOND), (19, NANO)] {
            if len == at {
                return (optional(next) && self.tz != Tz::Mandatory)
                    .then(|| done(v, next, true, None));
            }
            if is_zone(b[at]) {
                let zone = self.zone(&s[at..])?;
                return (next == NANO || optional(next)).then(|| done(v, next, true, Some(zone)));
            }
            let separator = match next {
                NANO => b[at] == b'.' || self.comma && b[at] == b',',
                _ => b[at] == b':',
            };
            if !separator || self.max == Some(next - 1) {
                return None;
            }
            if next != NANO {
                v[next] = Some(int(at + 1, at + 3).filter(|&x| x <= 59)?);
            }
        }
        let digits = (b[20..].iter().take(9))
            .take_while(|c| c.is_ascii_digit())
            .count();
        if digits == 0 {
            return None;
        }
        v[NANO] = Some(int(20, 20 + digits)? * 10i64.pow(9 - digits as u32));
        let pos = 20 + digits;
        if len == pos {
            return (self.tz != Tz::Mandatory).then_some((v, None));
        }
        if is_zone(b[pos]) {
            return Some((v, Some(self.zone(&s[pos..])?)));
        }
        None
    }

    /// `Iso8601Parser.parseZoneId`: `Z`, `±HH[[:]MM[[:]SS]]` (the same separator throughout)
    /// or a zone ID.
    fn zone(&self, s: &str) -> Option<Zone> {
        if self.tz == Tz::Forbidden {
            return None;
        }
        if s == "Z" {
            return Some(Zone::Offset(0));
        }
        let b = s.as_bytes();
        let sign = match b[0] {
            b'+' => 1,
            b'-' => -1,
            _ => return Some(Zone::Region(TimeZone::parse(s)?)),
        };
        let two = |at: usize| -> Option<i64> {
            let d = b.get(at..at + 2)?;
            d.iter()
                .all(u8::is_ascii_digit)
                .then(|| i64::from((d[0] - b'0') * 10 + d[1] - b'0'))
        };
        let mut parts = [two(1).filter(|&h| h <= 23)?, 0, 0];
        let mut pos = 3;
        let colon = b.get(3) == Some(&b':');
        for part in &mut parts[1..] {
            if pos == b.len() {
                break;
            }
            if (b[pos] == b':') != colon {
                return None;
            }
            pos += usize::from(colon);
            *part = two(pos).filter(|&x| x <= 59)?;
            pos += 2;
        }
        let [h, m, sec] = parts;
        let total = (h * 60 + m) * 60 + sec;
        (pos == b.len() && total <= 18 * 3600).then_some(Zone::Offset(sign * total * 1000))
    }
}

/// `Iso8601Parser.isZoneId`: region IDs start with a letter.
fn is_zone(c: u8) -> bool {
    c == b'+' || c == b'-' || c.is_ascii_alphabetic()
}

/// One of a format's parsers.
#[derive(Clone, Debug)]
enum Parser {
    Java(Pattern),
    Iso(Iso),
}

/// One `||` alternative of an Elasticsearch date format other than an epoch: a named format
/// or a Java pattern, with its printer and the parsers it tries in order.
#[derive(Clone, Debug)]
pub struct DateFormat {
    printer: Pattern,
    parsers: Vec<Parser>,
}

impl DateFormat {
    /// `DateFormatters.forPattern`: a named format, else a pattern; `None` where Elasticsearch
    /// rejects the format or es-mini lacks it (see [`Pattern::new`]).
    // ponytail: no week-based named formats (`week_date`, `weekyear_week_day`, ...)
    pub fn new(format: &str) -> Option<DateFormat> {
        let format = format.trim();
        named(format).or_else(|| {
            let p = Pattern::new(format)?;
            Some(DateFormat {
                printer: p.clone(),
                parsers: vec![Parser::Java(p)],
            })
        })
    }

    /// Epoch millis of `s` by the first parser that reads it (see [`Pattern::parse`]).
    pub fn parse(&self, s: &str, locale: &str, tz: &TimeZone, round_up: bool) -> Option<i64> {
        self.parse_wide(s, locale, tz, round_up)?.try_into().ok()
    }

    /// [`DateFormat::parse`] before the epoch millis are checked to fit in a long.
    pub fn parse_wide(&self, s: &str, locale: &str, tz: &TimeZone, round_up: bool) -> Option<i128> {
        self.parsers.iter().find_map(|p| match p {
            Parser::Java(p) => p.parse_wide(s, locale, tz, round_up),
            Parser::Iso(iso) => iso.parse(s, tz, round_up),
        })
    }

    /// Instant `t` at `offset` (see [`Pattern::format`]).
    pub fn format(&self, t: i64, offset: i64, locale: &str) -> Option<String> {
        self.printer.format(t, offset, locale)
    }
}

/// `DateTimeFormatterBuilder` `calls` onto the open sections in `stack`.
fn append(stack: &mut Vec<Seq>, calls: &[B]) -> Option<()> {
    for &call in calls {
        let seq = stack.last_mut()?;
        match call {
            V(f, min, max, sign) => seq.value(Tok::Num {
                f,
                min,
                max,
                sign,
                subsequent: 0,
                reduced: false,
            }),
            L(l) => seq.push(Tok::Lit(l.into())),
            Frac(min, max, dot) => {
                let t = Tok::Fraction { min, max, dot };
                match min == max && !dot {
                    true => seq.value(t),
                    false => seq.push(t),
                }
            }
            Off(pattern) => seq.push(Tok::Offset(pattern, "Z")),
            Zone => seq.push(Tok::ZoneId),
            P(p) => append_pattern(stack, p)?,
            A(inner) => {
                let toks = build(inner, false)?.toks;
                seq.toks.extend(toks);
                seq.active = None;
            }
            Opt(inner) => {
                stack.push(Seq::default());
                append(stack, inner)?;
                let inner = stack.pop()?.toks;
                stack.last_mut()?.push(Tok::Optional(inner));
            }
        }
    }
    Some(())
}

/// `toFormatter` of `calls`.
fn build(calls: &[B], lenient: bool) -> Option<Pattern> {
    let mut stack = vec![Seq::default()];
    append(&mut stack, calls)?;
    Some(Pattern {
        toks: stack.pop()?.toks,
        lenient,
        week: Some(Week::ISO),
    })
}

/// The named format `name`, as `DateFormatters` defines it.
fn named(name: &str) -> Option<DateFormat> {
    let java = |printer: &[B], parsers: &[&[B]]| -> Option<DateFormat> {
        Some(DateFormat {
            printer: build(printer, false)?,
            parsers: (parsers.iter())
                .map(|p| Some(Parser::Java(build(p, false)?)))
                .collect::<Option<_>>()?,
        })
    };
    let one = |f: &[B]| java(f, &[f]);
    // the printer with an `+HH:MM` offset; parsers with that, a zone or offset ID, or `+HHmm`
    let zoned = |printer: &[B], parser: &[B]| -> Option<DateFormat> {
        let with = |base: &[B], zone: B| build(&[base, &[zone]].concat(), false);
        Some(DateFormat {
            printer: with(printer, OFFSET)?,
            parsers: [OFFSET, Zone, OFFSET_NO_COLON]
                .into_iter()
                .map(|z| Some(Parser::Java(with(parser, z)?)))
                .collect::<Option<_>>()?,
        })
    };
    let iso = |printer: &[B], iso: Iso| -> Option<DateFormat> {
        Some(DateFormat {
            printer: build(printer, false)?,
            parsers: vec![Parser::Iso(iso)],
        })
    };
    match name {
        "iso8601" | "strict_date_optional_time" => iso(
            STRICT_DATE_OPTIONAL_TIME_PRINTER,
            Iso::new(1, 0, false, None),
        ),
        "strict_date_optional_time_nanos" => iso(
            STRICT_DATE_OPTIONAL_TIME_PRINTER_NANOS,
            Iso::new(HOUR, SECOND, true, None),
        ),
        "basic_date" => java(
            BASIC_YEAR_MONTH_DAY_PRINTER,
            &[&[
                V(Year, 1, 4, Normal),
                V(Month, 1, 2, NN),
                V(DayOfMonth, 1, 2, NN),
            ]],
        ),
        "basic_date_time" => zoned(
            &[A(BASIC_YEAR_MONTH_DAY_PRINTER), A(BASIC_T_TIME_PRINTER)],
            &[A(BASIC_YEAR_MONTH_DAY), A(BASIC_T_TIME)],
        ),
        "basic_date_time_no_millis" => zoned(
            &[
                A(BASIC_YEAR_MONTH_DAY_PRINTER),
                L("T"),
                A(BASIC_TIME_NO_MILLIS_BASE),
            ],
            &[
                A(BASIC_YEAR_MONTH_DAY),
                L("T"),
                A(BASIC_TIME_NO_MILLIS_BASE),
            ],
        ),
        "basic_ordinal_date" => one(&[P("uuuuDDD")]),
        "basic_ordinal_date_time" => zoned(
            &[P("yyyyDDD"), A(BASIC_T_TIME_PRINTER)],
            &[P("yyyyDDD"), A(BASIC_T_TIME)],
        ),
        "basic_ordinal_date_time_no_millis" => {
            let base = &[P("uuuuDDD"), L("T"), A(BASIC_TIME_NO_MILLIS_BASE)];
            zoned(base, base)
        }
        "basic_time" => zoned(BASIC_TIME_PRINTER, BASIC_TIME),
        "basic_time_no_millis" => zoned(BASIC_TIME_NO_MILLIS_BASE, BASIC_TIME_NO_MILLIS_BASE),
        "basic_t_time" => zoned(BASIC_T_TIME_PRINTER, BASIC_T_TIME),
        "basic_t_time_no_millis" => {
            let base = &[L("T"), A(BASIC_TIME_NO_MILLIS_BASE)];
            zoned(base, base)
        }
        "date" => java(ISO_LOCAL_DATE, &[DATE]),
        "date_hour" => java(
            &[P("uuuu-MM-dd'T'HH")],
            &[&[A(DATE), L("T"), V(HourOfDay, 1, 2, NN)]],
        ),
        "date_hour_minute" => java(
            &[P("uuuu-MM-dd'T'HH:mm")],
            &[&[A(DATE), L("T"), A(HOUR_MINUTE)]],
        ),
        "date_hour_minute_second" => java(
            &[P("uuuu-MM-dd'T'HH:mm:ss")],
            &[&[A(DATE), L("T"), A(HOUR_MINUTE_SECOND)]],
        ),
        "date_hour_minute_second_fraction" => java(
            STRICT_DATE_HOUR_MINUTE_SECOND_MILLIS_PRINTER,
            &[&[A(DATE), L("T"), A(HOUR_MINUTE_SECOND_FRACTION)]],
        ),
        "date_hour_minute_second_millis" => java(
            STRICT_DATE_HOUR_MINUTE_SECOND_MILLIS_PRINTER,
            &[&[A(DATE), L("T"), A(HOUR_MINUTE_SECOND_MILLIS)]],
        ),
        "date_optional_time" => java(
            STRICT_DATE_OPTIONAL_TIME_PRINTER,
            &[&[
                A(DATE),
                Opt(&[
                    L("T"),
                    Opt(&[
                        V(HourOfDay, 1, 2, NN),
                        Opt(&[
                            L(":"),
                            V(Minute, 1, 2, NN),
                            Opt(&[L(":"), V(Second, 1, 2, NN)]),
                            Opt(&[Frac(1, 9, true)]),
                            Opt(&[L(","), Frac(1, 9, false)]),
                            Opt(&[Zone]),
                            Opt(&[OFFSET_NO_COLON]),
                        ]),
                    ]),
                ]),
            ]],
        ),
        "date_time" => java(
            STRICT_DATE_OPTIONAL_TIME_PRINTER,
            &[&[A(DATE_TIME), OFFSET], &[A(DATE_TIME), OFFSET_NO_COLON]],
        ),
        "date_time_no_millis" => java(
            &[
                A(ISO_LOCAL_DATE),
                L("T"),
                P("HH:mm"),
                L(":"),
                V(Second, 2, 2, NN),
                Zone,
            ],
            &[
                &[A(DATE_TIME_PREFIX), OFFSET],
                &[A(DATE_TIME_PREFIX), OFFSET_NO_COLON],
                &[A(DATE_TIME_PREFIX), Opt(&[Zone])],
                &[A(DATE_TIME_PREFIX), Opt(&[OFFSET_NO_COLON])],
            ],
        ),
        "hour" => java(&[P("HH")], &[&[V(HourOfDay, 1, 2, NN)]]),
        "hour_minute" => java(&[P("HH:mm")], &[HOUR_MINUTE]),
        "hour_minute_second" => java(STRICT_HOUR_MINUTE_SECOND, &[HOUR_MINUTE_SECOND]),
        "hour_minute_second_fraction" => java(
            STRICT_HOUR_MINUTE_SECOND_MILLIS_PRINTER,
            &[HOUR_MINUTE_SECOND_FRACTION],
        ),
        "hour_minute_second_millis" => java(
            STRICT_HOUR_MINUTE_SECOND_MILLIS_PRINTER,
            &[HOUR_MINUTE_SECOND_MILLIS],
        ),
        "ordinal_date" => java(ORDINAL_DATE_PRINTER, &[ORDINAL_DATE]),
        "ordinal_date_time" => zoned(STRICT_ORDINAL_DATE_TIME_PRINTER, ORDINAL_DATE_TIME_BASE),
        "ordinal_date_time_no_millis" => zoned(
            STRICT_ORDINAL_DATE_TIME_NO_MILLIS_BASE,
            ORDINAL_DATE_TIME_NO_MILLIS_BASE,
        ),
        "time" => zoned(STRICT_TIME_PRINTER, TIME_PREFIX),
        "time_no_millis" => zoned(STRICT_HOUR_MINUTE_SECOND, TIME_NO_MILLIS),
        "t_time" => zoned(&[L("T"), A(STRICT_TIME_PRINTER)], &[L("T"), A(TIME_PREFIX)]),
        "t_time_no_millis" => zoned(
            &[L("T"), A(STRICT_HOUR_MINUTE_SECOND)],
            &[L("T"), A(TIME_NO_MILLIS)],
        ),
        "year" => one(&[v(Year)]),
        "year_month" => java(
            &[V(Year, 4, 10, ExceedsPad), L("-"), V(Month, 2, 2, NN)],
            &[&[v(Year), L("-"), v(Month)]],
        ),
        "year_month_day" => java(
            STRICT_YEAR_MONTH_DAY,
            &[&[v(Year), L("-"), v(Month), L("-"), v(DayOfMonth)]],
        ),
        "strict_date" => {
            let p = build(ISO_LOCAL_DATE, true)?;
            Some(DateFormat {
                printer: p.clone(),
                parsers: vec![Parser::Java(p)],
            })
        }
        "strict_date_hour" => one(&[P("uuuu-MM-dd'T'HH")]),
        "strict_date_hour_minute" => one(&[P("uuuu-MM-dd'T'HH:mm")]),
        "strict_date_hour_minute_second" => iso(
            &[P("uuuu-MM-dd'T'HH:mm:ss")],
            Iso::new(MONTH, SECOND, false, Some(SECOND)).tz(Tz::Forbidden),
        ),
        "strict_date_hour_minute_second_fraction" | "strict_date_hour_minute_second_millis" => iso(
            STRICT_DATE_HOUR_MINUTE_SECOND_MILLIS_PRINTER,
            Iso::new(MONTH, NANO, false, None).dot().tz(Tz::Forbidden),
        ),
        "strict_date_time" => iso(
            &[
                A(STRICT_YEAR_MONTH_DAY),
                L("T"),
                A(STRICT_HOUR_MINUTE_SECOND),
                Frac(3, 9, true),
                OFFSET,
            ],
            Iso::new(MONTH, SECOND, false, None).dot().tz(Tz::Mandatory),
        ),
        "strict_date_time_no_millis" => iso(
            &[
                A(STRICT_YEAR_MONTH_DAY),
                L("T"),
                A(STRICT_HOUR_MINUTE_SECOND),
                OFFSET,
            ],
            Iso::new(MONTH, SECOND, false, Some(SECOND)).tz(Tz::Mandatory),
        ),
        "strict_hour" => one(&[P("HH")]),
        "strict_hour_minute" => one(&[P("HH:mm")]),
        "strict_hour_minute_second" => one(STRICT_HOUR_MINUTE_SECOND),
        "strict_hour_minute_second_fraction" | "strict_hour_minute_second_millis" => java(
            STRICT_HOUR_MINUTE_SECOND_MILLIS_PRINTER,
            &[STRICT_HOUR_MINUTE_SECOND_MILLIS],
        ),
        "strict_ordinal_date" => one(&[V(Year, 4, 4, ExceedsPad), L("-"), V(DayOfYear, 3, 3, NN)]),
        "strict_ordinal_date_time" => zoned(
            STRICT_ORDINAL_DATE_TIME_PRINTER,
            STRICT_ORDINAL_DATE_TIME_BASE,
        ),
        "strict_ordinal_date_time_no_millis" => zoned(
            STRICT_ORDINAL_DATE_TIME_NO_MILLIS_BASE,
            STRICT_ORDINAL_DATE_TIME_NO_MILLIS_BASE,
        ),
        "strict_time" => zoned(STRICT_TIME_PRINTER, STRICT_TIME_BASE),
        "strict_time_no_millis" => zoned(STRICT_HOUR_MINUTE_SECOND, STRICT_HOUR_MINUTE_SECOND),
        "strict_t_time" => zoned(
            &[L("T"), A(STRICT_TIME_PRINTER)],
            &[L("T"), A(STRICT_TIME_BASE)],
        ),
        "strict_t_time_no_millis" => {
            let base = &[L("T"), A(STRICT_HOUR_MINUTE_SECOND)];
            zoned(base, base)
        }
        "strict_year" => iso(
            &[V(Year, 4, 4, ExceedsPad)],
            Iso::new(1, 0, false, Some(YEAR)).tz(Tz::Forbidden),
        ),
        "strict_year_month" => iso(
            &[V(Year, 4, 4, ExceedsPad), L("-"), V(Month, 2, 2, NN)],
            Iso::new(MONTH, MONTH, false, Some(MONTH)).tz(Tz::Forbidden),
        ),
        "strict_year_month_day" => one(STRICT_YEAR_MONTH_DAY),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every named format es-mini has prints these instants as Elasticsearch 9.5.5 does.
    #[test]
    fn date_formats_named_formats_print_like_elasticsearch() {
        // 2021-05-01T20:00:00.123Z, 2021-05-01T20:00:00Z, -0001-05-01T00:00:00Z
        let instants = [1_619_899_200_123, 1_619_899_200_000, -62_188_387_200_000];
        let cases = [
            (
                "iso8601",
                [
                    "2021-05-01T20:00:00.123Z",
                    "2021-05-01T20:00:00.000Z",
                    "-0001-05-01T00:00:00.000Z",
                ],
            ),
            (
                "strict_date_optional_time",
                [
                    "2021-05-01T20:00:00.123Z",
                    "2021-05-01T20:00:00.000Z",
                    "-0001-05-01T00:00:00.000Z",
                ],
            ),
            (
                "strict_date_optional_time_nanos",
                [
                    "2021-05-01T20:00:00.123Z",
                    "2021-05-01T20:00:00.000Z",
                    "-0001-05-01T00:00:00.000Z",
                ],
            ),
            ("basic_date", ["20210501", "20210501", "-00010501"]),
            (
                "basic_date_time",
                [
                    "20210501T200000.123Z",
                    "20210501T200000.000Z",
                    "-00010501T000000.000Z",
                ],
            ),
            (
                "basic_date_time_no_millis",
                ["20210501T200000Z", "20210501T200000Z", "-00010501T000000Z"],
            ),
            ("basic_ordinal_date", ["2021121", "2021121", "-0001121"]),
            (
                "basic_ordinal_date_time",
                [
                    "2021121T200000.123Z",
                    "2021121T200000.000Z",
                    "0002121T000000.000Z",
                ],
            ),
            (
                "basic_ordinal_date_time_no_millis",
                ["2021121T200000Z", "2021121T200000Z", "-0001121T000000Z"],
            ),
            ("basic_time", ["200000.123Z", "200000.000Z", "000000.000Z"]),
            ("basic_time_no_millis", ["200000Z", "200000Z", "000000Z"]),
            (
                "basic_t_time",
                ["T200000.123Z", "T200000.000Z", "T000000.000Z"],
            ),
            (
                "basic_t_time_no_millis",
                ["T200000Z", "T200000Z", "T000000Z"],
            ),
            ("date", ["2021-05-01", "2021-05-01", "-0001-05-01"]),
            (
                "date_hour",
                ["2021-05-01T20", "2021-05-01T20", "-0001-05-01T00"],
            ),
            (
                "date_hour_minute",
                ["2021-05-01T20:00", "2021-05-01T20:00", "-0001-05-01T00:00"],
            ),
            (
                "date_hour_minute_second",
                [
                    "2021-05-01T20:00:00",
                    "2021-05-01T20:00:00",
                    "-0001-05-01T00:00:00",
                ],
            ),
            (
                "date_hour_minute_second_fraction",
                [
                    "2021-05-01T20:00:00.123",
                    "2021-05-01T20:00:00.000",
                    "-0001-05-01T00:00:00.000",
                ],
            ),
            (
                "date_hour_minute_second_millis",
                [
                    "2021-05-01T20:00:00.123",
                    "2021-05-01T20:00:00.000",
                    "-0001-05-01T00:00:00.000",
                ],
            ),
            (
                "date_optional_time",
                [
                    "2021-05-01T20:00:00.123Z",
                    "2021-05-01T20:00:00.000Z",
                    "-0001-05-01T00:00:00.000Z",
                ],
            ),
            (
                "date_time",
                [
                    "2021-05-01T20:00:00.123Z",
                    "2021-05-01T20:00:00.000Z",
                    "-0001-05-01T00:00:00.000Z",
                ],
            ),
            (
                "date_time_no_millis",
                [
                    "2021-05-01T20:00:00Z",
                    "2021-05-01T20:00:00Z",
                    "-0001-05-01T00:00:00Z",
                ],
            ),
            ("hour", ["20", "20", "00"]),
            ("hour_minute", ["20:00", "20:00", "00:00"]),
            ("hour_minute_second", ["20:00:00", "20:00:00", "00:00:00"]),
            (
                "hour_minute_second_fraction",
                ["20:00:00.123", "20:00:00.000", "00:00:00.000"],
            ),
            (
                "hour_minute_second_millis",
                ["20:00:00.123", "20:00:00.000", "00:00:00.000"],
            ),
            ("ordinal_date", ["2021-121", "2021-121", "-0001-121"]),
            (
                "ordinal_date_time",
                [
                    "2021-121T20:00:00.123Z",
                    "2021-121T20:00:00.000Z",
                    "-0001-121T00:00:00.000Z",
                ],
            ),
            (
                "ordinal_date_time_no_millis",
                [
                    "2021-121T20:00:00Z",
                    "2021-121T20:00:00Z",
                    "-0001-121T00:00:00Z",
                ],
            ),
            ("time", ["20:00:00.123Z", "20:00:00.000Z", "00:00:00.000Z"]),
            ("time_no_millis", ["20:00:00Z", "20:00:00Z", "00:00:00Z"]),
            (
                "t_time",
                ["T20:00:00.123Z", "T20:00:00.000Z", "T00:00:00.000Z"],
            ),
            (
                "t_time_no_millis",
                ["T20:00:00Z", "T20:00:00Z", "T00:00:00Z"],
            ),
            ("year", ["2021", "2021", "-1"]),
            ("year_month", ["2021-05", "2021-05", "-0001-05"]),
            (
                "year_month_day",
                ["2021-05-01", "2021-05-01", "-0001-05-01"],
            ),
            ("strict_date", ["2021-05-01", "2021-05-01", "-0001-05-01"]),
            (
                "strict_date_hour",
                ["2021-05-01T20", "2021-05-01T20", "-0001-05-01T00"],
            ),
            (
                "strict_date_hour_minute",
                ["2021-05-01T20:00", "2021-05-01T20:00", "-0001-05-01T00:00"],
            ),
            (
                "strict_date_hour_minute_second",
                [
                    "2021-05-01T20:00:00",
                    "2021-05-01T20:00:00",
                    "-0001-05-01T00:00:00",
                ],
            ),
            (
                "strict_date_hour_minute_second_fraction",
                [
                    "2021-05-01T20:00:00.123",
                    "2021-05-01T20:00:00.000",
                    "-0001-05-01T00:00:00.000",
                ],
            ),
            (
                "strict_date_hour_minute_second_millis",
                [
                    "2021-05-01T20:00:00.123",
                    "2021-05-01T20:00:00.000",
                    "-0001-05-01T00:00:00.000",
                ],
            ),
            (
                "strict_date_time",
                [
                    "2021-05-01T20:00:00.123Z",
                    "2021-05-01T20:00:00.000Z",
                    "-0001-05-01T00:00:00.000Z",
                ],
            ),
            (
                "strict_date_time_no_millis",
                [
                    "2021-05-01T20:00:00Z",
                    "2021-05-01T20:00:00Z",
                    "-0001-05-01T00:00:00Z",
                ],
            ),
            ("strict_hour", ["20", "20", "00"]),
            ("strict_hour_minute", ["20:00", "20:00", "00:00"]),
            (
                "strict_hour_minute_second",
                ["20:00:00", "20:00:00", "00:00:00"],
            ),
            (
                "strict_hour_minute_second_fraction",
                ["20:00:00.123", "20:00:00.000", "00:00:00.000"],
            ),
            (
                "strict_hour_minute_second_millis",
                ["20:00:00.123", "20:00:00.000", "00:00:00.000"],
            ),
            ("strict_ordinal_date", ["2021-121", "2021-121", "-0001-121"]),
            (
                "strict_ordinal_date_time",
                [
                    "2021-121T20:00:00.123Z",
                    "2021-121T20:00:00.000Z",
                    "-0001-121T00:00:00.000Z",
                ],
            ),
            (
                "strict_ordinal_date_time_no_millis",
                [
                    "2021-121T20:00:00Z",
                    "2021-121T20:00:00Z",
                    "-0001-121T00:00:00Z",
                ],
            ),
            (
                "strict_time",
                ["20:00:00.123Z", "20:00:00.000Z", "00:00:00.000Z"],
            ),
            (
                "strict_time_no_millis",
                ["20:00:00Z", "20:00:00Z", "00:00:00Z"],
            ),
            (
                "strict_t_time",
                ["T20:00:00.123Z", "T20:00:00.000Z", "T00:00:00.000Z"],
            ),
            (
                "strict_t_time_no_millis",
                ["T20:00:00Z", "T20:00:00Z", "T00:00:00Z"],
            ),
            ("strict_year", ["2021", "2021", "-0001"]),
            ("strict_year_month", ["2021-05", "2021-05", "-0001-05"]),
            (
                "strict_year_month_day",
                ["2021-05-01", "2021-05-01", "-0001-05-01"],
            ),
        ];
        for (name, want) in cases {
            let f = DateFormat::new(name).unwrap();
            let got = instants.map(|t| f.format(t, 0, "").unwrap());
            assert_eq!(got, want, "{name}");
        }
    }

    /// Values Elasticsearch 9.5.5 reads (as epoch millis) or rejects in each named format.
    #[test]
    fn date_formats_named_formats_parse_like_elasticsearch() {
        let cases = [
            (
                "strict_date_optional_time",
                "2021-05-01T20:00:00+0530",
                Some(1619879400000),
            ),
            (
                "strict_date_optional_time",
                "2021-05-01T20:00:00.123456789Z",
                Some(1619899200123),
            ),
            (
                "strict_date_optional_time",
                "2021-05-01T20:00:00.1234567891Z",
                None,
            ),
            ("strict_date_optional_time", "2021-02-30", None),
            (
                "strict_date_optional_time",
                "2021-05-01T",
                Some(1619827200000),
            ),
            (
                "strict_date_optional_time",
                "-0001-05-01",
                Some(-62188387200000),
            ),
            (
                "strict_date_optional_time",
                "2021-05-01T20:00Europe/Paris",
                Some(1619892000000),
            ),
            (
                "strict_date_optional_time",
                "2021-05-01T20:00:00+05",
                Some(1619881200000),
            ),
            (
                "strict_date_optional_time",
                "2021-05-01T20:00:00+05:3000",
                None,
            ),
            (
                "strict_date_optional_time",
                "2021-05-01T20+01:00",
                Some(1619895600000),
            ),
            (
                "strict_date_time",
                "2021-05-01T20:00:00Z",
                Some(1619899200000),
            ),
            (
                "strict_date_time",
                "2021-05-01T20:00:00.5Z",
                Some(1619899200500),
            ),
            ("strict_date_time", "2021-05-01T20:00:00", None),
            ("strict_date_time", "2021-05-01T20:00:00,5Z", None),
            (
                "strict_date_time_no_millis",
                "2021-05-01T20:00:00Z",
                Some(1619899200000),
            ),
            (
                "strict_date_time_no_millis",
                "2021-05-01T20:00:00.000Z",
                None,
            ),
            ("strict_date", "2021-02-30", Some(1614643200000)),
            ("strict_date", "2021-13-01", Some(1640995200000)),
            ("strict_date", "2021-5-01", None),
            ("date", "2021-5-1", Some(1619827200000)),
            (
                "date_optional_time",
                "2021-5-1T2:3:4,5",
                Some(1619834584500),
            ),
            (
                "date_optional_time",
                "2021-05-01T20:00:00.5+0530",
                Some(1619879400500),
            ),
            (
                "date_optional_time",
                "2021-05-01T20:00Z",
                Some(1619899200000),
            ),
            ("basic_date", "20210501", Some(1619827200000)),
            (
                "basic_date_time",
                "20210501T200000.123Z",
                Some(1619899200123),
            ),
            (
                "basic_date_time",
                "20210501T200000.1+0530",
                Some(1619879400100),
            ),
            (
                "basic_date_time",
                "20210501T200000.1+05:30",
                Some(1619879400100),
            ),
            ("basic_ordinal_date", "2021121", Some(1619827200000)),
            ("ordinal_date", "2021-121", Some(1619827200000)),
            ("ordinal_date", "2021-1", Some(1609459200000)),
            ("hour_minute", "8:05", Some(29100000)),
            ("year", "2021", Some(1609459200000)),
            ("year_month", "2021-5", Some(1619827200000)),
            ("strict_year", "2021", Some(1609459200000)),
            ("strict_year", "2021-05", None),
            ("strict_year_month", "2021-05", Some(1619827200000)),
            (
                "date_time_no_millis",
                "2021-05-01T20:00:00Z",
                Some(1619899200000),
            ),
            (
                "date_time_no_millis",
                "2021-05-01T20:00:00",
                Some(1619899200000),
            ),
            (
                "date_time_no_millis",
                "2021-05-01T20:00Z",
                Some(1619899200000),
            ),
            ("time", "20:00:00.5Z", Some(72000500)),
            ("t_time", "T20:00:00.123+01:00", Some(68400123)),
            (
                "strict_ordinal_date_time",
                "2021-121T20:00Z",
                Some(1619899200000),
            ),
            (
                "date_hour_minute_second_fraction",
                "2021-5-1T20:0:0.123456",
                Some(1619899200123),
            ),
            (
                "strict_date_hour_minute_second_millis",
                "2021-05-01T20:00:00.5",
                Some(1619899200500),
            ),
            (
                "strict_date_optional_time_nanos",
                "2021-05-01",
                Some(1619827200000),
            ),
            ("strict_date_optional_time_nanos", "2021-05-01T20", None),
            ("iso8601", "2021-05-01T20:00:00.5", Some(1619899200500)),
            ("strict_date_hour", "2021-05-01T20", Some(1619899200000)),
            (
                "date_time",
                "2021-05-01T20:00:00.1+0100",
                Some(1619895600100),
            ),
            ("basic_t_time_no_millis", "T200000Z", Some(72000000)),
            ("year_month_day", "2021-5-1", Some(1619827200000)),
            (
                "strict_hour_minute_second_fraction",
                "20:00:00.123456",
                Some(72000123),
            ),
        ];
        for (name, s, want) in cases {
            let f = DateFormat::new(name).unwrap();
            assert_eq!(f.parse(s, "", &TimeZone::UTC, false), want, "{name} {s}");
        }
    }
}
