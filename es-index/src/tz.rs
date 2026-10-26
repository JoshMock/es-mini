//! Time zones as java.time's ZoneId: `Z`, `UTC`, `GMT`, fixed offsets (`+01:00`, `-0730`, `+5`),
//! and region IDs (`Europe/Berlin`) read from the system's TZif files in /usr/share/zoneinfo.

use std::fs;

/// A time zone's offset history; all times and offsets in millis.
#[derive(Debug, Clone, PartialEq)]
pub struct TimeZone {
    /// The offset before the first transition.
    initial: i64,
    /// `(instant, offset from it on)`, ascending, each changing the offset.
    transitions: Vec<(i64, i64)>,
}

impl TimeZone {
    pub const UTC: TimeZone = TimeZone {
        initial: 0,
        transitions: Vec::new(),
    };

    /// The zone with ID `id`, if it is a known zone.
    pub fn parse(id: &str) -> Option<TimeZone> {
        if matches!(id, "Z" | "UTC" | "GMT") {
            return Some(TimeZone::UTC);
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
        tzif(&fs::read(format!("/usr/share/zoneinfo/{id}")).ok()?)
    }

    /// The offset at instant `t`.
    pub fn offset(&self, t: i64) -> i64 {
        match self.transitions.partition_point(|&(at, _)| at <= t) {
            0 => self.initial,
            i => self.transitions[i - 1].1,
        }
    }

    /// The last transition strictly before instant `t`.
    pub fn previous_transition(&self, t: i64) -> Option<i64> {
        let i = self.transitions.partition_point(|&(at, _)| at < t);
        i.checked_sub(1).map(|i| self.transitions[i].0)
    }

    /// The instants with local time `local`, ascending: none in a gap, two in an overlap.
    pub fn instants(&self, local: i64) -> Vec<i64> {
        // offsets are under a day, so only transitions within a day of `local` matter
        const DAY: i64 = 86_400_000;
        let from = self
            .transitions
            .partition_point(|&(at, _)| at < local - DAY);
        let to = self
            .transitions
            .partition_point(|&(at, _)| at <= local + DAY);
        let mut offsets = vec![self.offset(local - DAY)];
        offsets.extend(self.transitions[from..to].iter().map(|t| t.1));
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
        let i = self.transitions.partition_point(|&(at, o)| at + o <= local);
        self.transitions.get(i).map_or(local, |t| t.0)
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
    })
}

/// A TZif file's (RFC 8536) transitions, from its 64-bit block when it has one.
// ponytail: the POSIX TZ footer is ignored, so dates past the last listed transition keep its
// offset; distributions' "fat" files list them to 2037. Parse the footer if slim files matter.
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
    let [_, _, _, time, types, _] = header(b)?;
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
    })
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
