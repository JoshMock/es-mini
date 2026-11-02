//! Time zones as java.time's ZoneId: `Z`, `UTC`, `GMT`, fixed offsets (`+01:00`, `-0730`, `+5`),
//! and region IDs (`Europe/Berlin`) read from the system's TZif files in /usr/share/zoneinfo.

use std::{borrow::Cow, fs};

const HOUR: i64 = 3_600_000;
const DAY: i64 = 24 * HOUR;

/// A time zone's offset history; all times and offsets in millis.
#[derive(Debug, Clone, PartialEq)]
pub struct TimeZone {
    /// The offset before the first transition.
    initial: i64,
    /// `(instant, offset from it on)`, ascending, each changing the offset.
    transitions: Vec<(i64, i64)>,
    /// The daylight saving rule of the TZif footer, for instants past the last transition.
    rule: Option<Rule>,
    /// The region ID (`Europe/Paris`, `UTC`) java.time prints for the zone; `None` for an offset.
    region: Option<Box<str>>,
}

/// A POSIX TZ string's daylight saving rule: standard and daylight offsets, and the dates and
/// local times daylight saving starts (in standard time) and ends (in daylight time).
#[derive(Debug, Clone, PartialEq)]
struct Rule {
    std: i64,
    dst: i64,
    start: (RuleDate, i64),
    end: (RuleDate, i64),
}

/// A POSIX TZ rule's date: `Jn` (Feb 29 never counted), `n` (from 0), or `Mm.w.d` (day `d`,
/// from Sunday = 0, of week `w` of month `m`, week 5 the last).
#[derive(Debug, Clone, PartialEq)]
enum RuleDate {
    Julian(i64),
    Zero(i64),
    Month(i64, i64, i64),
}

impl TimeZone {
    pub const UTC: TimeZone = TimeZone {
        initial: 0,
        transitions: Vec::new(),
        rule: None,
        region: None,
    };

    /// The transitions that matter around instant (or local time) `t`: the listed ones, or
    /// past them, the last listed and those the footer's rule makes in `t`'s year and the
    /// years beside it.
    fn around(&self, t: i64) -> Cow<'_, [(i64, i64)]> {
        let end = self.transitions.last().map_or(i64::MIN, |l| l.0);
        let Some(rule) = self
            .rule
            .as_ref()
            .filter(|_| t.saturating_sub(2 * DAY) > end)
        else {
            return Cow::Borrowed(&self.transitions);
        };
        let y = crate::civil_from_days(t.div_euclid(DAY)).0;
        let mut v: Vec<_> = self.transitions.last().copied().into_iter().collect();
        v.extend(
            (y - 1..=y + 1)
                .flat_map(|y| rule.year(y))
                .filter(|t| t.0 > end),
        );
        Cow::Owned(v)
    }

    /// The zone with ID `id`, if it is a known zone.
    pub fn parse(id: &str) -> Option<TimeZone> {
        match id {
            "Z" => return Some(TimeZone::UTC),
            "UTC" | "GMT" => return Some(TimeZone::UTC.in_region(id)),
            _ => {}
        }
        if let Some(rest) = id.strip_prefix(['+', '-']) {
            return fixed(id.starts_with('-'), rest);
        }
        // region IDs are path segments of letters, digits, `_`, `-` and `+` (no `..`)
        let valid = |seg: &str| {
            seg.starts_with(|c: char| c.is_ascii_alphabetic())
                && seg
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_-+".contains(c))
        };
        if !id.split('/').all(valid) {
            return None;
        }
        Some(tzif(&fs::read(format!("/usr/share/zoneinfo/{id}")).ok()?)?.in_region(id))
    }

    fn in_region(self, id: &str) -> TimeZone {
        TimeZone {
            region: Some(id.into()),
            ..self
        }
    }

    /// The region ID java.time prints for the zone (`VV`), or `None` for a fixed offset.
    pub fn region(&self) -> Option<&str> {
        self.region.as_deref()
    }

    /// The offset at instant `t`.
    pub fn offset(&self, t: i64) -> i64 {
        let transitions = self.around(t);
        match transitions.partition_point(|&(at, _)| at <= t) {
            0 => self.initial,
            i => transitions[i - 1].1,
        }
    }

    /// The last transition strictly before instant `t`.
    pub fn previous_transition(&self, t: i64) -> Option<i64> {
        let transitions = self.around(t);
        let i = transitions.partition_point(|&(at, _)| at < t);
        i.checked_sub(1).map(|i| transitions[i].0)
    }

    /// The instants with local time `local`, ascending: none in a gap, two in an overlap.
    pub fn instants(&self, local: i64) -> Vec<i64> {
        // offsets are under a day, so only transitions within a day of `local` matter
        let transitions = self.around(local);
        let from = transitions.partition_point(|&(at, _)| at < local - DAY);
        let to = transitions.partition_point(|&(at, _)| at <= local + DAY);
        let mut offsets = vec![self.offset(local - DAY)];
        offsets.extend(transitions[from..to].iter().map(|t| t.1));
        let mut out: Vec<i64> = (offsets.into_iter().map(|o| local - o))
            .filter(|&t| local - self.offset(t) == t)
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// The instant of local time `local`, like java.time's `ZonedDateTime.ofLocal`: in an overlap
    /// the one at offset `prefer` if valid, else the earlier; in a gap, later by the gap's length.
    pub fn at_local(&self, local: i64, prefer: i64) -> i64 {
        let instants = self.instants(local);
        match instants.iter().find(|&&t| local - t == prefer) {
            Some(&t) => t,
            None => (instants.first().copied())
                .unwrap_or_else(|| local - self.offset(self.gap_start(local) - 1)),
        }
    }

    /// The transition that skips local time `local` (which [`TimeZone::instants`] has none of).
    pub fn gap_start(&self, local: i64) -> i64 {
        let transitions = self.around(local);
        let i = transitions.partition_point(|&(at, o)| at + o <= local);
        transitions.get(i).map_or(local, |t| t.0)
    }

    /// Instant `t` in ES's `strict_date_optional_time`, at this zone's offset.
    pub fn format(&self, t: i64) -> String {
        let o = self.offset(t);
        let mut s = crate::iso_date(t + o);
        if o != 0 {
            s.pop();
            // `+HH:MM` drops the seconds of an offset
            let (sign, m) = (if o < 0 { '-' } else { '+' }, o.abs() / 60_000);
            s += &format!("{sign}{:02}:{:02}", m / 60, m % 60);
        }
        s
    }
}

/// ZoneOffset.of's `h`, `hh`, `hh:mm`, `hhmm`, `hh:mm:ss` and `hhmmss` (up to 18 hours).
fn fixed(negative: bool, s: &str) -> Option<TimeZone> {
    let digits: String = s.chars().filter(|&c| c != ':').collect();
    let colons = s.len() - digits.len();
    let ok = match digits.len() {
        1 | 2 => colons == 0,
        4 => colons == 0 || s.as_bytes().get(2) == Some(&b':') && colons == 1,
        6 => colons == 0 || colons == 2 && s.as_bytes()[2] == b':' && s.as_bytes()[5] == b':',
        _ => false,
    };
    if !ok || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let part = |i: usize| {
        digits
            .get(i..i + 2)
            .map_or(0, |p| p.parse::<i64>().unwrap_or(0))
    };
    let (h, m, sec) = match digits.len() {
        1 => (digits.parse().ok()?, 0, 0),
        _ => (part(0), part(2), part(4)),
    };
    let ms = ((h * 60 + m) * 60 + sec) * 1000;
    if m > 59 || sec > 59 || ms > 18 * 3_600_000 {
        return None;
    }
    Some(TimeZone {
        initial: if negative { -ms } else { ms },
        transitions: vec![],
        rule: None,
        region: None,
    })
}

/// A TZif file's (RFC 8536) transitions, from its 64-bit block when it has one, and the
/// daylight saving rule of the footer that follows that block.
fn tzif(b: &[u8]) -> Option<TimeZone> {
    let header = |b: &[u8]| -> Option<[usize; 6]> {
        if b.get(..4)? != b"TZif" {
            return None;
        }
        let n = |i: usize| {
            b.get(20 + 4 * i..24 + 4 * i)
                .map(|c| u32::from_be_bytes(c.try_into().unwrap()) as usize)
        };
        Some([n(0)?, n(1)?, n(2)?, n(3)?, n(4)?, n(5)?])
    };
    let [isut, isstd, leap, time, types, chars] = header(b)?;
    let (b, width) = if *b.get(4)? >= b'2' {
        let v1 = 44 + time * 5 + types * 6 + chars + leap * 8 + isstd + isut;
        (b.get(v1..)?, 8)
    } else {
        (b, 4)
    };
    let [isut, isstd, leap, time, types, chars] = header(b)?;
    let footer = match width {
        8 => b.get(44 + time * 9 + types * 6 + chars + leap * 12 + isstd + isut..),
        _ => None,
    };
    let rule = (footer.and_then(|f| std::str::from_utf8(f).ok()))
        .and_then(|f| posix_rule(f.strip_prefix('\n')?.lines().next()?));
    let body = b.get(44..)?;
    let int = |c: &[u8]| match width {
        8 => i64::from_be_bytes(c.try_into().unwrap()),
        _ => i64::from(i32::from_be_bytes(c.try_into().unwrap())),
    };
    let times = body.get(..time * width)?;
    let idx = body.get(time * width..time * (width + 1))?;
    let infos = body.get(time * (width + 1)..time * (width + 1) + types * 6)?;
    let offset = |i: usize| -> Option<i64> {
        let c = infos.get(i * 6..i * 6 + 4)?;
        Some(i64::from(i32::from_be_bytes(c.try_into().unwrap())) * 1000)
    };
    let initial = offset(0)?;
    let mut transitions: Vec<(i64, i64)> = vec![];
    for (t, &i) in times.chunks(width).zip(idx) {
        let o = offset(usize::from(i))?;
        let last = transitions.last().map_or(initial, |t| t.1);
        if o != last {
            transitions.push((int(t).saturating_mul(1000), o));
        }
    }
    Some(TimeZone {
        initial,
        transitions,
        rule,
        region: None,
    })
}

impl Rule {
    /// The transitions of year `y`, ascending.
    fn year(&self, y: i64) -> [(i64, i64); 2] {
        let at = |(date, time): &(RuleDate, i64), offset: i64| {
            let jan1 = crate::days_from_civil(y, 1, 1);
            let leap = crate::days_from_civil(y, 3, 1) - jan1 == 60;
            let days = match *date {
                RuleDate::Julian(n) => jan1 + n - 1 + i64::from(leap && n >= 60),
                RuleDate::Zero(n) => jan1 + n,
                RuleDate::Month(m, w, d) => {
                    let first = crate::days_from_civil(y, m, 1);
                    let next = crate::days_from_civil(y + m / 12, m % 12 + 1, 1);
                    // 1970-01-01 was a Thursday
                    let day = first + (d - (first + 4).rem_euclid(7)).rem_euclid(7) + (w - 1) * 7;
                    if day >= next { day - 7 } else { day }
                }
            };
            days.saturating_mul(DAY).saturating_add(time - offset)
        };
        let start = (at(&self.start, self.std), self.dst);
        let end = (at(&self.end, self.dst), self.std);
        if start.0 <= end.0 {
            [start, end]
        } else {
            [end, start]
        }
    }
}

/// The daylight saving rule of a POSIX TZ string (`EST5EDT,M3.2.0,M11.1.0`, with RFC 8536's
/// hours up to 167 and negative in rule times); `None` without one.
fn posix_rule(s: &str) -> Option<Rule> {
    let mut s = s;
    name(&mut s)?;
    let std = -hms(&mut s)?;
    name(&mut s)?;
    let dst = match s.starts_with(',') {
        true => std + HOUR,
        false => -hms(&mut s)?,
    };
    let mut change = || -> Option<(RuleDate, i64)> {
        s = s.strip_prefix(',')?;
        let date = if let Some(rest) = s.strip_prefix('M') {
            s = rest;
            let m = number(&mut s)?;
            s = s.strip_prefix('.')?;
            let w = number(&mut s)?;
            s = s.strip_prefix('.')?;
            let d = number(&mut s)?;
            ((1..=12).contains(&m) && (1..=5).contains(&w) && d <= 6)
                .then_some(RuleDate::Month(m, w, d))?
        } else if let Some(rest) = s.strip_prefix('J') {
            s = rest;
            RuleDate::Julian(number(&mut s).filter(|n| (1..=365).contains(n))?)
        } else {
            RuleDate::Zero(number(&mut s).filter(|&n| n <= 365)?)
        };
        let time = match s.strip_prefix('/') {
            Some(rest) => {
                s = rest;
                hms(&mut s)?
            }
            None => 2 * HOUR,
        };
        Some((date, time))
    };
    let (start, end) = (change()?, change()?);
    s.is_empty().then_some(Rule {
        std,
        dst,
        start,
        end,
    })
}

/// Skips a POSIX TZ zone abbreviation: letters, or anything between `<` and `>`.
fn name(s: &mut &str) -> Option<()> {
    let len = match s.strip_prefix('<') {
        Some(rest) => rest.find('>')? + 2,
        None => s
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(s.len()),
    };
    *s = &s[len..];
    (len >= 3).then_some(())
}

/// A POSIX TZ `[+-]hh[:mm[:ss]]` in millis.
fn hms(s: &mut &str) -> Option<i64> {
    let sign = if let Some(rest) = s.strip_prefix('-') {
        *s = rest;
        -1
    } else {
        *s = s.strip_prefix('+').unwrap_or(s);
        1
    };
    let mut ms = number(s).filter(|&h| h <= 167)? * HOUR;
    for unit in [60_000, 1000] {
        let Some(rest) = s.strip_prefix(':') else {
            break;
        };
        *s = rest;
        ms += number(s).filter(|&n| n <= 59)? * unit;
    }
    Some(sign * ms)
}

/// Leading decimal digits.
fn number(s: &mut &str) -> Option<i64> {
    let len = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let n = s[..len].parse().ok()?;
    *s = &s[len..];
    Some(n)
}

#[cfg(test)]
mod tests {
    use super::TimeZone;

    #[test]
    fn region_zones_follow_their_transitions() {
        let ny = TimeZone::parse("America/New_York").unwrap();
        // 2020-03-08T07:00:00Z: 02:00 EST becomes 03:00 EDT
        let spring = 1_583_650_800_000;
        assert_eq!(ny.offset(spring - 1), -5 * 3_600_000);
        assert_eq!(ny.offset(spring), -4 * 3_600_000);
        assert_eq!(
            ny.previous_transition(spring),
            ny.previous_transition(spring - 1)
        );
        assert_eq!(ny.previous_transition(spring + 1), Some(spring));
        // local 02:30 does not exist; local 01:30 on 2020-11-01 happens twice
        let local = |t: i64| t - 5 * 3_600_000;
        assert!(ny.instants(local(spring) + 1_800_000).is_empty());
        assert_eq!(ny.gap_start(local(spring) + 1_800_000), spring);
        let fall_local = 1_604_194_200_000; // 2020-11-01T01:30 as if UTC
        let h = 3_600_000;
        assert_eq!(
            ny.instants(fall_local),
            vec![fall_local + 4 * h, fall_local + 5 * h]
        );
        assert_eq!(ny.format(spring), "2020-03-08T03:00:00.000-04:00");
    }

    #[test]
    fn date_formats_zones_follow_the_tzif_footer_rule_past_the_listed_transitions() {
        let h = 3_600_000;
        // 2100's transitions, from Python's zoneinfo on the same TZif files
        for (id, at, before, after) in [
            ("America/New_York", 4_108_690_800_000, -5, -4),
            ("America/New_York", 4_129_250_400_000, -4, -5),
            ("Australia/Sydney", 4_110_451_200_000, 11, 10),
            ("Australia/Sydney", 4_126_176_000_000, 10, 11),
            // M9.1.6/24: hour 24 of a Saturday
            ("America/Santiago", 4_110_490_800_000, -3, -4),
            ("America/Santiago", 4_123_800_000_000, -4, -3),
            // M3.5.0/-1: an hour before a Sunday starts
            ("America/Godthab", 4_109_878_800_000, -2, -1),
            ("America/Godthab", 4_128_627_600_000, -1, -2),
        ] {
            let z = TimeZone::parse(id).unwrap();
            assert_eq!(
                (z.offset(at - 1), z.offset(at)),
                (before * h, after * h),
                "{id} {at}"
            );
            assert_eq!(z.previous_transition(at + 1), Some(at), "{id} {at}");
            // in an overlap, the instant at the preferred offset
            assert_eq!(z.at_local(at + after * h, after * h), at, "{id} {at}");
        }
        // a gap past the listed transitions: 02:30 on 2100-03-14 in New York
        let ny = TimeZone::parse("America/New_York").unwrap();
        assert!(ny.instants(4_108_690_800_000 - 5 * h + h / 2).is_empty());
    }

    #[test]
    fn fixed_offsets_and_bad_ids() {
        let h = 3_600_000;
        assert_eq!(TimeZone::parse("+01:00").unwrap().offset(0), h);
        assert_eq!(TimeZone::parse("-0530").unwrap().offset(0), -5 * h - h / 2);
        assert_eq!(TimeZone::parse("+5").unwrap().offset(0), 5 * h);
        assert_eq!(TimeZone::parse("Z"), Some(TimeZone::UTC));
        assert_eq!(
            TimeZone::parse("UTC").unwrap().format(0),
            "1970-01-01T00:00:00.000Z"
        );
        for bad in [
            "+19:00",
            "+1:00",
            "+01:60",
            "Mars/Olympus",
            "../etc/passwd",
            "",
            "Europe/..",
        ] {
            assert_eq!(TimeZone::parse(bad), None, "{bad}");
        }
    }
}
