//! Term and range queries on ip and range fields, as inclusive intervals of sortable keys: an
//! ip's 16-byte encoding, or a long with its sign bit flipped, both read as a big-endian u128.

use crate::{Query, QueryError, Relation, int, text};
use es_index::FieldMapping;
use serde_json::Value;

/// A long as a key that orders like it.
fn long_key(n: i64) -> u128 {
    u128::from(n as u64 ^ 1 << 63)
}

fn ip_key(v: &Value) -> Result<u128, QueryError> {
    let ip = es_index::ip_bytes(&Value::String(text(v)));
    ip.map(u128::from_be_bytes)
        .ok_or_else(|| QueryError::illegal(format!("'{}' is not an IP string literal.", text(v))))
}

/// IpFieldMapper.termQuery: an address, or the addresses of a CIDR block (InetAddresses.parseCidr).
pub(crate) fn ip_term(field: String, v: &Value) -> Result<Query, QueryError> {
    let t = text(v);
    let interval = |from, to| Query::Interval {
        field,
        from,
        to,
        relation: Relation::Intersects,
    };
    if !t.contains('/') {
        let k = ip_key(v)?;
        return Ok(interval(k, k));
    }
    let Some((addr, prefix)) = t
        .split_once('/')
        .filter(|(_, p)| !p.is_empty() && !p.contains('/'))
    else {
        return Err(QueryError::illegal(format!(
            "Expected [ip/prefix] but was [{t}]"
        )));
    };
    let k = ip_key(&Value::from(addr))?;
    // Java parses an IPv4-mapped address as IPv4
    let v4 = k >> 32 == 0xffff;
    if v4 && addr.contains(':') {
        return Err(QueryError::illegal(format!(
            "CIDR notation is not allowed with IPv6-mapped IPv4 address [{addr} as it introduces ambiguity as to whether the prefix length should be interpreted as a v4 prefix length or a v6 prefix length"
        )));
    }
    let len: i32 = (prefix.parse())
        .map_err(|_| QueryError::illegal(format!("For input string: \"{prefix}\"")))?;
    let bits = if v4 { 32 } else { 128 };
    if !(0..=bits).contains(&len) {
        return Err(QueryError::illegal(format!(
            "Illegal prefix length [{len}] in [{t}]. Must be 0-32 for IPv4 ranges, 0-128 for IPv6 ranges"
        )));
    }
    // InetAddressPoint.newPrefixQuery: an IPv4 prefix covers the 96 mapping bits too
    let host = u128::MAX
        .checked_shr((128 - bits + len) as u32)
        .unwrap_or(0);
    Ok(interval(k & !host, k | host))
}

/// A range query's `[gt, gte, lt, lte]` on an ip field (`ip`), or on a long or date range field
/// once its bounds are epoch millis.
pub(crate) fn range(
    ip: bool,
    field: String,
    [gt, gte, lt, lte]: [Option<&Value>; 4],
    relation: Relation,
) -> Result<Query, QueryError> {
    let long = |v: &Value| {
        int(v)
            .map(long_key)
            .ok_or_else(|| QueryError::illegal(format!("For input string: \"{}\"", text(v))))
    };
    let key = |v: &Value| if ip { ip_key(v) } else { long(v) };
    let (min, max) = match ip {
        true => (0, u128::MAX),
        false => (long_key(i64::MIN), long_key(i64::MAX)),
    };
    let bound = |inclusive: Option<&Value>, exclusive: Option<&Value>| {
        Ok::<_, QueryError>(match (inclusive, exclusive) {
            (Some(v), _) => Some((key(v)?, true)),
            (_, Some(v)) => Some((key(v)?, false)),
            _ => None,
        })
    };
    let (lower, upper) = (bound(gte, gt)?, bound(lte, lt)?);
    let (from, to) = (lower.map_or(min, |b| b.0), upper.map_or(max, |b| b.0));
    // RangeType.createQuery; a plain ip field's range does not check
    if from > to && !ip {
        let show = |k: u128| (k as u64 ^ 1 << 63) as i64;
        return Err(QueryError::illegal(format!(
            "Range query `from` value ({}) is greater than `to` value ({})",
            show(from),
            show(to)
        )));
    }
    // an exclusive bound past the type's limits leaves an empty interval, which matches nothing
    let from = lower.map_or(Some(min), |(k, inclusive)| {
        k.checked_add(u128::from(!inclusive))
    });
    let to = upper.map_or(Some(max), |(k, inclusive)| {
        k.checked_sub(u128::from(!inclusive))
    });
    let (from, to) = from.zip(to).unwrap_or((1, 0));
    Ok(Query::Interval {
        field,
        from,
        to,
        relation,
    })
}

/// The inclusive `[from, to]` of a range field's value, its bounds parsed by `parse` (rounding
/// up a `gt` or `lte` date when asked) and open ends at the long limits, like RangeFieldMapper.
pub(crate) fn range_ends(
    v: &Value,
    parse: &dyn Fn(&Value, bool) -> Option<i64>,
) -> Option<(i64, i64)> {
    let o = v.as_object()?;
    let end = |k: &str| {
        o.get(k)
            .filter(|v| !v.is_null())
            .map(|v| parse(v, k == "gt" || k == "lte"))
    };
    let from = match (end("gte"), end("gt")) {
        (Some(x), _) => x?,
        (_, Some(x)) => x?.checked_add(1)?,
        _ => i64::MIN,
    };
    let to = match (end("lte"), end("lt")) {
        (Some(x), _) => x?,
        (_, Some(x)) => x?.checked_sub(1)?,
        _ => i64::MAX,
    };
    Some((from, to))
}

/// A document value's interval of keys: an ip's single address, or a range field's range.
pub(crate) fn doc_interval(mapping: Option<&FieldMapping>, v: &Value) -> Option<(u128, u128)> {
    let longs = |(a, b)| (long_key(a), long_key(b));
    match mapping? {
        FieldMapping::Ip => ip_key(v).ok().map(|k| (k, k)),
        FieldMapping::LongRange => range_ends(v, &|v, _| int(v)).map(longs),
        FieldMapping::DateRange { format } => range_ends(v, &|v, round_up| {
            let utc = es_index::tz::TimeZone::UTC;
            es_index::date_millis_in(format.as_deref(), None, v, &utc, round_up)
        })
        .map(longs),
        _ => None,
    }
}

/// Whether a document's `[a, b]` relates to a query's `[c, d]` as `relation` asks.
pub(crate) fn holds(relation: Relation, (a, b): (u128, u128), (c, d): (u128, u128)) -> bool {
    c <= d
        && match relation {
            Relation::Intersects => a <= d && c <= b,
            Relation::Within => c <= a && b <= d,
            Relation::Contains => a <= c && d <= b,
        }
}
