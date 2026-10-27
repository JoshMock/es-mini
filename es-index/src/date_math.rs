//! Date math, like Elasticsearch's JavaDateMathParser: `now` or a date (`2024-01-01||`), then
//! operations such as `+1d`, `-2h` or `/M` (round to the unit).

use crate::{civil_from_days, date_instant_in, days_from_civil, tz::TimeZone};
use serde_json::Value;

const DAY: i64 = 86_400_000;

/// How the reason of an `illegal_argument_exception` (rather than a `parse_exception`) for a
/// date past the range of a long of epoch millis ends, as ES's DateUtils.toLongMillis words it.
pub const TOO_FAR: &str = "to be represented in a long milliseconds variable";

/// Epoch millis of date math `text` read with a date field's `format` (`||` alternatives) in
/// time zone `tz` (for dates without an offset, and for rounding). With `round_up`, rounding
/// and missing date fields go to the last millisecond of the unit, as for `gt` and `lte`.
/// Errors are Elasticsearch's `parse_exception` reasons.
pub fn parse(
    text: &str,
    format: Option<&str>,
    locale: Option<&str>,
    tz: &TimeZone,
    round_up: bool,
    now: i64,
) -> Result<i64, String> {
    let date = |s: &str, round_up| {
        if s.is_empty() {
            return Err("cannot parse empty datetime".to_owned());
        }
        let Some(t) = date_instant_in(format, locale, &Value::from(s), tz, round_up) else {
            let f = format.unwrap_or("strict_date_optional_time||epoch_millis");
            let e = format!("failed to parse date field [{s}] with format [{f}]");
            return Err(format!("{e}: [{e}]"));
        };
        i64::try_from(t).map_err(|_| {
            let when = if t > 0 { "future" } else { "past" };
            format!(
                "date[{}] is too far in the {when} {TOO_FAR}",
                instant_string(t)
            )
        })
    };
    let (t, math) = match text.strip_prefix("now") {
        Some(math) => (now, math),
        None => match text.split_once("||") {
            Some((anchor, math)) => (date(anchor, false)?, math),
            None => return date(text, round_up),
        },
    };
    apply(math, t, round_up, tz)
}

/// JavaDateMathParser.parseMath: date units add and round in local time, time units add to the
/// instant.
fn apply(math: &str, mut t: i64, round_up: bool, tz: &TimeZone) -> Result<i64, String> {
    let mut rest = math;
    while let Some(op) = rest.chars().next() {
        rest = &rest[op.len_utf8()..];
        let (round, sign) = match op {
            '/' => (true, 1),
            '+' => (false, 1),
            '-' => (false, -1),
            _ => return Err(format!("operator not supported for date math [{math}]")),
        };
        let truncated = || format!("truncated date math [{math}]");
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        let num: i64 = match digits {
            0 => 1,
            _ => rest[..digits].parse().map_err(|_| truncated())?,
        };
        rest = &rest[digits..];
        let unit = rest.chars().next().ok_or_else(truncated)?;
        rest = &rest[unit.len_utf8()..];
        if round && num != 1 {
            return Err(format!(
                "rounding `/` can only be used on single unit types [{math}]"
            ));
        }
        let offset = tz.offset(t);
        // a local date-time change, resolved like java.time keeping the offset where it can
        let local = |f: &dyn Fn(i64) -> i64| tz.at_local(f(t + offset), offset);
        let fixed = match unit {
            'h' | 'H' => 3_600_000,
            'm' => 60_000,
            's' => 1000,
            _ => 0,
        };
        t = match (unit, round) {
            ('y', false) => local(&|l| add_months(l, 12 * sign * num)),
            ('M', false) => local(&|l| add_months(l, sign * num)),
            ('w', false) => local(&|l| l + 7 * DAY * sign * num),
            ('d', false) => local(&|l| l + DAY * sign * num),
            (_, false) if fixed > 0 => t + fixed * sign * num,
            ('y' | 'M' | 'w' | 'd', true) => {
                let days = |l: i64| l.div_euclid(DAY);
                let start = |l: i64| match unit {
                    'y' => days_from_civil(civil_from_days(days(l)).0, 1, 1) * DAY,
                    'M' => (days(l) - civil_from_days(days(l)).2 + 1) * DAY,
                    // 1970-01-01 was a Thursday; weeks start on Monday
                    'w' => (days(l) - (days(l) + 3).rem_euclid(7)) * DAY,
                    _ => days(l) * DAY,
                };
                let next = |l: i64| match unit {
                    'y' => add_months(start(l), 12),
                    'M' => add_months(start(l), 1),
                    'w' => start(l) + 7 * DAY,
                    _ => start(l) + DAY,
                };
                match round_up {
                    true => local(&next) - 1,
                    false => local(&start),
                }
            }
            (_, true) if fixed > 0 => {
                let start = local(&|l| l - l.rem_euclid(fixed));
                if round_up { start + fixed - 1 } else { start }
            }
            _ => {
                return Err(format!(
                    "unit [{unit}] not supported for date math [{math}]"
                ));
            }
        };
    }
    Ok(t)
}

/// Epoch millis `t` as Java's `Instant.toString`: `+` before years past 9999, a fraction only if
/// there is one.
fn instant_string(t: i128) -> String {
    let (days, ms) = (
        t.div_euclid(DAY.into()) as i64,
        t.rem_euclid(DAY.into()) as i64,
    );
    let (y, m, d) = civil_from_days(days);
    let year = match y {
        10_000.. => format!("+{y}"),
        ..0 => format!("-{:04}", -y),
        _ => format!("{y:04}"),
    };
    let (h, mi, s, milli) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    let fraction = if milli > 0 {
        format!(".{milli:03}")
    } else {
        String::new()
    };
    format!("{year}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}{fraction}Z")
}

/// Local millis `l` plus `n` months, the day clamped to the month's last, like `plusMonths`.
pub fn add_months(l: i64, n: i64) -> i64 {
    let (days, ms) = (l.div_euclid(DAY), l.rem_euclid(DAY));
    let (y, m, d) = civil_from_days(days);
    let m0 = m - 1 + n;
    let (y, m) = (y + m0.div_euclid(12), m0.rem_euclid(12) + 1);
    let last = days_from_civil(y + m / 12, m % 12 + 1, 1) - days_from_civil(y, m, 1);
    days_from_civil(y, m, d.min(last)) * DAY + ms
}

#[cfg(test)]
mod tests {
    use super::parse;
    use crate::{parse_date_millis, tz::TimeZone};
    use serde_json::json;

    fn at(iso: &str) -> i64 {
        parse_date_millis(&json!(iso)).unwrap()
    }

    #[test]
    fn missing_fields_round_up_to_the_end_of_the_day() {
        let p = |s, up| parse(s, None, None, &TimeZone::UTC, up, 0);
        assert_eq!(p("2022-01-01", false), Ok(at("2022-01-01T00:00:00Z")));
        assert_eq!(p("2022-01-01", true), Ok(at("2022-01-01T23:59:59.999Z")));
        assert_eq!(
            p("2022-01-01T12:12", true),
            Ok(at("2022-01-01T12:12:59.999Z"))
        );
        assert_eq!(
            p("2022-01-01T12:12:12.1", true),
            Ok(at("2022-01-01T12:12:12.1Z"))
        );
        // the default format falls back to epoch_millis
        assert_eq!(p("1641000000000", true), Ok(1_641_000_000_000));
        let year = |s, up| parse(s, Some("uuuu"), None, &TimeZone::UTC, up, 0);
        assert_eq!(year("1500", false), Ok(at("1500-01-01T00:00:00Z")));
        assert_eq!(year("1500", true), Ok(at("1500-01-01T23:59:59.999Z")));
        let secs = |s, up| parse(s, Some("epoch_second"), None, &TimeZone::UTC, up, 0);
        assert_eq!(secs("1641000000", true), Ok(1_641_000_000_999));
        assert_eq!(secs("1641000000", false), Ok(1_641_000_000_000));
    }

    #[test]
    fn time_zones_apply_to_dates_without_an_offset() {
        let plus1 = TimeZone::parse("+01:00").unwrap();
        let p = |s, tz| parse(s, None, None, tz, false, 0);
        assert_eq!(p("2022-01-01", &plus1), Ok(at("2021-12-31T23:00:00Z")));
        assert_eq!(
            p("2022-01-01T12:00:00-05:00", &plus1),
            Ok(at("2022-01-01T17:00:00Z"))
        );
        assert_eq!(p("1641000000000", &plus1), Ok(1_641_000_000_000));
        let ny = TimeZone::parse("America/New_York").unwrap();
        assert_eq!(
            p("2021-12-31T19:00:00", &ny),
            Ok(at("2022-01-01T00:00:00Z"))
        );
        // a local time in a gap moves forward by the gap; one in an overlap takes the earlier
        assert_eq!(
            p("2022-03-13T02:30:00", &ny),
            Ok(at("2022-03-13T07:30:00Z"))
        );
        assert_eq!(
            p("2022-11-06T01:30:00", &ny),
            Ok(at("2022-11-06T05:30:00Z"))
        );
    }

    #[test]
    fn math_adds_and_rounds_in_local_time() {
        let now = at("2022-01-31T10:20:30.400Z");
        let p = |s, up| parse(s, None, None, &TimeZone::UTC, up, now);
        assert_eq!(p("now", false), Ok(now));
        assert_eq!(p("now-1d", false), Ok(at("2022-01-30T10:20:30.400Z")));
        assert_eq!(p("now+1M", false), Ok(at("2022-02-28T10:20:30.400Z")));
        assert_eq!(p("now-2y+3h", false), Ok(at("2020-01-31T13:20:30.400Z")));
        assert_eq!(p("now/d", false), Ok(at("2022-01-31T00:00:00Z")));
        assert_eq!(p("now/d", true), Ok(at("2022-01-31T23:59:59.999Z")));
        assert_eq!(p("now/w", false), Ok(at("2022-01-31T00:00:00Z")));
        assert_eq!(p("now-1d/w", true), Ok(at("2022-01-30T23:59:59.999Z")));
        assert_eq!(p("now/M", true), Ok(at("2022-01-31T23:59:59.999Z")));
        assert_eq!(p("now/y", false), Ok(at("2022-01-01T00:00:00Z")));
        assert_eq!(p("now/H", false), Ok(at("2022-01-31T10:00:00Z")));
        assert_eq!(p("now/m", true), Ok(at("2022-01-31T10:20:59.999Z")));
        assert_eq!(p("now/s", false), Ok(at("2022-01-31T10:20:30Z")));
        assert_eq!(
            p("2022-01-01||+1d/d", true),
            Ok(at("2022-01-02T23:59:59.999Z"))
        );
        // the anchor before `||` never rounds up
        assert_eq!(p("2022-01-01||+0d", true), Ok(at("2022-01-01T00:00:00Z")));
        let berlin = TimeZone::parse("Europe/Berlin").unwrap();
        let local = parse("2022-01-01T00:00:00||+1d", None, None, &berlin, false, 0);
        assert_eq!(local, Ok(at("2022-01-01T23:00:00Z")));
        let day = parse("now/d", None, None, &berlin, false, now);
        assert_eq!(day, Ok(at("2022-01-30T23:00:00Z")));
    }

    #[test]
    fn errors_use_elasticsearch_messages() {
        let p = |s| parse(s, None, None, &TimeZone::UTC, false, 0);
        assert_eq!(
            p("now+1x"),
            Err("unit [x] not supported for date math [+1x]".into())
        );
        assert_eq!(
            p("now*1d"),
            Err("operator not supported for date math [*1d]".into())
        );
        assert_eq!(p("now+1"), Err("truncated date math [+1]".into()));
        assert_eq!(
            p("now/2d"),
            Err("rounding `/` can only be used on single unit types [/2d]".into())
        );
        let f = "strict_date_optional_time||epoch_millis";
        assert_eq!(
            p("bogus"),
            Err(format!(
                "failed to parse date field [bogus] with format [{f}]: [failed to parse date field [bogus] with format [{f}]]"
            ))
        );
        let millis = parse(
            "2022/01/01",
            Some("epoch_millis"),
            None,
            &TimeZone::UTC,
            false,
            0,
        );
        assert!(millis.unwrap_err().contains("with format [epoch_millis]"));
    }
}
