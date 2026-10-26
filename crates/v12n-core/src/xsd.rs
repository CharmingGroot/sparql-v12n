// SPDX-License-Identifier: Apache-2.0
//! Values of XML Schema literals that `FILTER` compares (SPARQL 1.1 §17.3),
//! read from their lexical forms (XML Schema 1.1 Part 2).

use crate::solution::canonical_decimal;

pub(crate) const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

/// The numeric types of the SPARQL operator mapping that comparisons
/// support, in XPath's order of type promotion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Num {
    Integer,
    Decimal,
    Double,
}

impl Num {
    /// The PostgreSQL type a value of this class is compared as.
    pub fn sql_type(self) -> &'static str {
        match self {
            Self::Integer => "int8",
            Self::Decimal => "numeric",
            Self::Double => "float8",
        }
    }
}

/// A numeric literal's value.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Number {
    pub class: Num,
    /// The canonical `xsd:decimal` form of an integer or decimal.
    pub decimal: Option<String>,
    /// The value as the nearest double, which XPath promotes integers and
    /// decimals to when the other operand is a double.
    pub double: f64,
}

impl Number {
    /// The value as an `int8`, when it is an integer in its range.
    pub fn int8(&self) -> Option<i64> {
        self.decimal
            .as_deref()
            .filter(|d| !d.contains('.'))
            .and_then(|d| d.parse().ok())
    }
}

/// What a typed literal is for the operator mapping.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    /// `xsd:string`.
    String,
    Numeric(Number),
    Boolean(bool),
    DateTime(DateTime),
    /// A datatype outside the operator mapping.
    Other,
    /// A numeric, boolean or dateTime lexical form that is not valid for
    /// its datatype, such as `"abc"^^xsd:integer`.
    IllTyped,
    /// A valid value that comparisons do not support, with the reason.
    Unsupported(String),
}

/// An `xsd:dateTime` as a PostgreSQL `timestamp` (`zoned` false) or
/// `timestamptz` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DateTime {
    /// The form the engine writes for a value of the PostgreSQL type, in
    /// UTC with `Z` when zoned.
    pub canonical: String,
    pub zoned: bool,
}

const DERIVED_NUMERIC: [&str; 13] = [
    "float",
    "nonPositiveInteger",
    "negativeInteger",
    "long",
    "int",
    "short",
    "byte",
    "nonNegativeInteger",
    "unsignedLong",
    "unsignedInt",
    "unsignedShort",
    "unsignedByte",
    "positiveInteger",
];

/// The value of a literal of datatype `datatype` (a full IRI).
pub(crate) fn value(lexical: &str, datatype: &str) -> Value {
    let Some(local) = datatype.strip_prefix(XSD) else {
        return Value::Other;
    };
    match local {
        "string" => Value::String,
        "integer" => match canonical_integer(lexical) {
            Some(decimal) => Value::Numeric(Number {
                class: Num::Integer,
                double: decimal.parse().unwrap_or(f64::NAN),
                decimal: Some(decimal),
            }),
            None => Value::IllTyped,
        },
        "decimal" if is_decimal(lexical) => match canonical_decimal(lexical) {
            Some(decimal) => Value::Numeric(Number {
                class: Num::Decimal,
                double: decimal.parse().unwrap_or(f64::NAN),
                decimal: Some(decimal),
            }),
            None => Value::IllTyped,
        },
        "decimal" => Value::IllTyped,
        "double" => match double(lexical) {
            Some(double) => Value::Numeric(Number {
                class: Num::Double,
                decimal: None,
                double,
            }),
            None => Value::IllTyped,
        },
        "boolean" => match lexical {
            "true" | "1" => Value::Boolean(true),
            "false" | "0" => Value::Boolean(false),
            _ => Value::IllTyped,
        },
        "dateTime" => match date_time(lexical) {
            Ok(Some(value)) => Value::DateTime(value),
            Ok(None) => Value::IllTyped,
            Err(reason) => Value::Unsupported(reason.to_owned()),
        },
        derived if DERIVED_NUMERIC.contains(&derived) => Value::Unsupported(format!(
            "comparing xsd:{derived} literals; only xsd:integer, xsd:decimal and xsd:double \
             numerics are supported"
        )),
        _ => Value::Other,
    }
}

/// XML Schema 1.1 `integer` lexical space, `[+-]?[0-9]+`, to its canonical
/// form.
fn canonical_integer(lexical: &str) -> Option<String> {
    let digits = lexical.strip_prefix(['+', '-']).unwrap_or(lexical);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    canonical_decimal(lexical)
}

/// XML Schema 1.1 `decimal` lexical space:
/// `[+-]?([0-9]+(\.[0-9]*)?|\.[0-9]+)`.
fn is_decimal(lexical: &str) -> bool {
    let digits = lexical.strip_prefix(['+', '-']).unwrap_or(lexical);
    let (int, frac) = digits.split_once('.').unwrap_or((digits, ""));
    !(int.is_empty() && frac.is_empty())
        && int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit())
}

/// XML Schema 1.1 `double` lexical space, read as the nearest double.
fn double(lexical: &str) -> Option<f64> {
    match lexical {
        "INF" | "+INF" => return Some(f64::INFINITY),
        "-INF" => return Some(f64::NEG_INFINITY),
        "NaN" => return Some(f64::NAN),
        _ => {}
    }
    let (mantissa, exponent) = match lexical.find(['e', 'E']) {
        Some(at) => (&lexical[..at], Some(&lexical[at + 1..])),
        None => (lexical, None),
    };
    if !is_decimal(mantissa) {
        return None;
    }
    if let Some(exponent) = exponent {
        let digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    }
    lexical.parse().ok()
}

const MICROS_PER_DAY: i64 = 86_400_000_000;

/// An `xsd:dateTime` lexical form, `None` when it is not one. Values that
/// PostgreSQL cannot hold exactly are an error: years outside 1 to 9999,
/// and fractions of a second below a microsecond.
fn date_time(lexical: &str) -> Result<Option<DateTime>, &'static str> {
    let Some((date, rest)) = lexical.split_once('T') else {
        return Ok(None);
    };
    let (time, zone) = match rest.find(['Z', '+', '-']) {
        Some(at) => (&rest[..at], Some(&rest[at..])),
        None => (rest, None),
    };
    let mut parts = date.splitn(3, '-');
    let (Some(year), Some(month), Some(day)) = (parts.next(), parts.next(), parts.next()) else {
        return Ok(None);
    };
    if date.starts_with('-') {
        return Err("xsd:dateTime constants before year 1");
    }
    if year.len() > 4 && year.bytes().all(|b| b.is_ascii_digit()) && !year.starts_with('0') {
        return Err("xsd:dateTime constants after year 9999");
    }
    let (Some(year), Some(month), Some(day)) = (number(year, 4), number(month, 2), number(day, 2))
    else {
        return Ok(None);
    };
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) {
        return Ok(None);
    }
    if year == 0 {
        return Err("xsd:dateTime constants before year 1");
    }
    let (clock, fraction) = time.split_once('.').unwrap_or((time, ""));
    if time.contains('.') && fraction.is_empty() {
        return Ok(None);
    }
    let mut clock = clock.split(':');
    let (Some(h), Some(m), Some(s), None) =
        (clock.next(), clock.next(), clock.next(), clock.next())
    else {
        return Ok(None);
    };
    let (Some(h), Some(m), Some(s)) = (number(h, 2), number(m, 2), number(s, 2)) else {
        return Ok(None);
    };
    if !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(None);
    }
    let fraction = fraction.trim_end_matches('0');
    let end_of_day = h == 24 && m == 0 && s == 0 && fraction.is_empty();
    if (h > 23 && !end_of_day) || m > 59 || s > 59 {
        return Ok(None);
    }
    if fraction.len() > 6 {
        return Err("xsd:dateTime constants with fractions of a second below a microsecond");
    }
    let offset_minutes = match zone {
        None => None,
        Some("Z") => Some(0),
        Some(zone) => {
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let Some((zh, zm)) = zone[1..].split_once(':') else {
                return Ok(None);
            };
            let (Some(zh), Some(zm)) = (number(zh, 2), number(zm, 2)) else {
                return Ok(None);
            };
            if zm > 59 || zh > 14 || (zh == 14 && zm > 0) {
                return Ok(None);
            }
            Some(sign * (zh * 60 + zm))
        }
    };
    let micros_of_fraction = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<i64>().unwrap_or(0) * 10i64.pow(6 - fraction.len() as u32)
    };
    let mut micros = days_from_civil(year, month, day) * MICROS_PER_DAY
        + ((h * 60 + m) * 60 + s) * 1_000_000
        + micros_of_fraction;
    if let Some(offset) = offset_minutes {
        micros -= offset * 60_000_000;
    }
    let (year, month, day) = civil_from_days(micros.div_euclid(MICROS_PER_DAY));
    if !(1..=9999).contains(&year) {
        return Err("xsd:dateTime constants outside the years 1 to 9999");
    }
    let time = micros.rem_euclid(MICROS_PER_DAY);
    let seconds = time / 1_000_000;
    let mut canonical = format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    );
    if time % 1_000_000 != 0 {
        canonical.push_str(format!(".{:06}", time % 1_000_000).trim_end_matches('0'));
    }
    if offset_minutes.is_some() {
        canonical.push('Z');
    }
    Ok(Some(DateTime {
        canonical,
        zoned: offset_minutes.is_some(),
    }))
}

/// Exactly `len` ASCII digits.
fn number(digits: &str, len: usize) -> Option<i64> {
    (digits.len() == len && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse().ok())
        .flatten()
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days from 1970-01-01 in the proleptic Gregorian calendar (H. Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(month <= 2), month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xsd(local: &str) -> String {
        format!("{XSD}{local}")
    }

    fn number(lexical: &str, datatype: &str) -> Number {
        match value(lexical, &xsd(datatype)) {
            Value::Numeric(n) => n,
            other => panic!("{lexical}: {other:?}"),
        }
    }

    #[test]
    fn numerics_read_their_lexical_spaces() {
        let n = number("+007", "integer");
        assert_eq!(
            (n.class, n.decimal.as_deref(), n.int8()),
            (Num::Integer, Some("7"), Some(7))
        );
        let n = number("7.50", "decimal");
        assert_eq!((n.decimal.as_deref(), n.int8()), (Some("7.5"), None));
        assert_eq!(number("7.0", "decimal").int8(), Some(7));
        assert_eq!(number(".5", "decimal").decimal.as_deref(), Some("0.5"));
        assert_eq!(number("99999999999999999999", "integer").int8(), None);
        assert_eq!(number("1.5e1", "double").double, 15.0);
        assert_eq!(number("-INF", "double").double, f64::NEG_INFINITY);
        assert!(number("NaN", "double").double.is_nan());
        for (lexical, datatype) in [
            ("1.5", "integer"),
            ("", "integer"),
            ("1e3", "decimal"),
            (".", "decimal"),
            ("inf", "double"),
            ("1e", "double"),
            ("0x10", "double"),
            ("yes", "boolean"),
        ] {
            assert_eq!(value(lexical, &xsd(datatype)), Value::IllTyped, "{lexical}");
        }
        assert!(matches!(value("1", &xsd("int")), Value::Unsupported(_)));
        assert_eq!(value("1", &xsd("boolean")), Value::Boolean(true));
        assert_eq!(value("x", "http://example.org/t"), Value::Other);
        assert_eq!(value("2024-01-01", &xsd("date")), Value::Other);
    }

    fn date_time_value(lexical: &str) -> DateTime {
        match value(lexical, &xsd("dateTime")) {
            Value::DateTime(d) => d,
            other => panic!("{lexical}: {other:?}"),
        }
    }

    #[test]
    fn date_times_normalize_to_utc_when_zoned() {
        let d = date_time_value("2024-01-02T03:04:05.500");
        assert_eq!(
            (d.canonical.as_str(), d.zoned),
            ("2024-01-02T03:04:05.5", false)
        );
        let d = date_time_value("2024-01-02T03:04:05+09:00");
        assert_eq!(
            (d.canonical.as_str(), d.zoned),
            ("2024-01-01T18:04:05Z", true)
        );
        assert_eq!(
            date_time_value("2024-12-31T23:30:00-01:00").canonical,
            "2025-01-01T00:30:00Z"
        );
        assert_eq!(
            date_time_value("2024-02-28T24:00:00").canonical,
            "2024-02-29T00:00:00"
        );
        assert_eq!(
            date_time_value("2024-01-01T00:00:00Z").canonical,
            "2024-01-01T00:00:00Z"
        );
        for lexical in [
            "2024-02-30T00:00:00",
            "2023-02-29T00:00:00",
            "2024-01-01",
            "2024-01-01T24:00:01",
            "2024-01-01T10:00",
            "2024-01-01T10:00:00.",
            "2024-01-01T10:00:00+15:00",
            "24-01-01T10:00:00",
        ] {
            assert_eq!(
                value(lexical, &xsd("dateTime")),
                Value::IllTyped,
                "{lexical}"
            );
        }
        for lexical in [
            "2024-01-01T10:00:00.1234567",
            "-0001-01-01T00:00:00",
            "10000-01-01T00:00:00",
            "0001-01-01T00:00:00+01:00",
        ] {
            assert!(
                matches!(value(lexical, &xsd("dateTime")), Value::Unsupported(_)),
                "{lexical}"
            );
        }
    }

    #[test]
    fn civil_days_round_trip() {
        for days in [-719_162, -1, 0, 10_957, 19_782, 2_932_896] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
    }
}
