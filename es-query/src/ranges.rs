//! Term and range queries on ip and range fields, as inclusive intervals of sortable keys: an
//! ip's 16-byte encoding, or a long (or a float's or double's sortable bits) with its sign bit
//! flipped, both read as a big-endian u128 (see [`FieldMapping::range_keys`]).

use crate::{Query, QueryError, Relation, int, text};
use es_index::FieldMapping;
use es_index::mapping::{key_long, long_key, sortable_int, sortable_long};
use serde_json::Value;

fn ip_key(v: &Value) -> Result<u128, QueryError> {
    let ip = es_index::ip_bytes(&Value::String(text(v)));
    ip.map(u128::from_be_bytes)
        .ok_or_else(|| QueryError::illegal(format!("'{}' is not an IP string literal.", text(v))))
}

/// IpFieldMapper.termQuery: an address, or the addresses of a CIDR block (InetAddresses.parseCidr).
pub(crate) fn ip_term(field: String, v: &Value) -> Result<Query, QueryError> {
    let t = text(v);
    let (from, to) = match t.contains('/') {
        true => es_index::mapping::cidr(&t).map_err(QueryError::illegal)?,
        false => (ip_key(v)?, ip_key(v)?),
    };
    Ok(Query::Interval {
        field,
        from,
        to,
        relation: Relation::Intersects,
    })
}

/// A range query bound on a field `m` maps, as a key, like RangeType.parseValue.
fn key(m: &FieldMapping, v: &Value) -> Result<u128, QueryError> {
    let bad = || QueryError::illegal(format!("For input string: \"{}\"", text(v)));
    let num = || es_index::json_num(v).ok_or_else(bad);
    match m {
        FieldMapping::Ip | FieldMapping::IpRange => ip_key(v),
        FieldMapping::FloatRange => Ok(long_key(sortable_int(num()? as f32).into())),
        FieldMapping::DoubleRange => Ok(long_key(sortable_long(num()?))),
        FieldMapping::IntegerRange => {
            let n = int(v).ok_or_else(bad)?;
            match i32::try_from(n) {
                Ok(n) => Ok(long_key(n.into())),
                Err(_) => Err(QueryError::illegal(format!(
                    "Value [{}] is out of range for an integer",
                    text(v)
                ))),
            }
        }
        _ => int(v).map(long_key).ok_or_else(bad),
    }
}

/// A key as Java's `toString` of the value it holds, which range errors show.
fn show(m: &FieldMapping, k: u128) -> String {
    match m {
        FieldMapping::IpRange => {
            let ip = std::net::Ipv6Addr::from(k);
            match ip.to_ipv4_mapped() {
                Some(v4) => format!("/{v4}"),
                // Inet6Address names all eight groups
                None => format!("/{}", ip.segments().map(|g| format!("{g:x}")).join(":")),
            }
        }
        // the float's shortest digits, as Float.toString picks
        FieldMapping::FloatRange => {
            let f = (m.range_number(k) as f32).to_string();
            crate::aggs::java_double(f.parse().unwrap_or(f64::NAN))
        }
        FieldMapping::DoubleRange => crate::aggs::java_double(m.range_number(k)),
        _ => key_long(k).to_string(),
    }
}

/// A range query's `[gt, gte, lt, lte]` on an ip or range field `m` maps, date bounds already
/// epoch millis.
pub(crate) fn range(
    m: &FieldMapping,
    field: String,
    [gt, gte, lt, lte]: [Option<&Value>; 4],
    relation: Relation,
) -> Result<Query, QueryError> {
    let (min, max) = m.range_limits();
    let bound = |inclusive: Option<&Value>, exclusive: Option<&Value>| {
        Ok::<_, QueryError>(match (inclusive, exclusive) {
            (Some(v), _) => Some((key(m, v)?, true)),
            (_, Some(v)) => Some((key(m, v)?, false)),
            _ => None,
        })
    };
    let (lower, upper) = (bound(gte, gt)?, bound(lte, lt)?);
    let (from, to) = (lower.map_or(min, |b| b.0), upper.map_or(max, |b| b.0));
    // RangeType.createQuery; a plain ip field's range does not check
    if from > to && m.is_range() {
        return Err(QueryError::illegal(format!(
            "Range query `from` value ({}) is greater than `to` value ({})",
            show(m, from),
            show(m, to)
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

/// A document value's interval of keys: an ip's single address, or a range field's range.
pub(crate) fn doc_interval(mapping: Option<&FieldMapping>, v: &Value) -> Option<(u128, u128)> {
    match mapping? {
        FieldMapping::Ip => ip_key(v).ok().map(|k| (k, k)),
        m if m.is_range() => m.range_keys(v).flatten(),
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
