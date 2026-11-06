// SPDX-License-Identifier: Apache-2.0
//! Values of XML Schema literals that `FILTER` compares (SPARQL 1.1 §17.3),
//! read from their lexical forms (XML Schema 1.1 Part 2).

use crate::dialect::SqlType;
use crate::solution::{canonical_decimal, canonical_double};
use crate::sparql::Cast;

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
    /// The SQL type a value of this class is compared as.
    pub fn sql_type(self) -> SqlType {
        match self {
            Self::Integer => SqlType::BigInt,
            Self::Decimal => SqlType::Numeric,
            Self::Double => SqlType::Double,
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
    let (local, zone) = split_zone(lexical);
    let Some((date, time)) = local.split_once('T') else {
        return Ok(None);
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
    let Some(canonical) = canonical_date_time(micros, offset_minutes.is_some()) else {
        return Err("xsd:dateTime constants outside the years 1 to 9999");
    };
    Ok(Some(DateTime {
        canonical,
        zoned: offset_minutes.is_some(),
    }))
}

/// The lexical form without its time zone, and the time zone as written.
fn split_zone(lexical: &str) -> (&str, Option<&str>) {
    let Some(t) = lexical.find('T') else {
        return (lexical, None);
    };
    match lexical[t..].find(['Z', '+', '-']) {
        Some(at) => (&lexical[..t + at], Some(&lexical[t + at..])),
        None => (lexical, None),
    }
}

/// The form the engine writes for `micros` from 1970-01-01T00:00:00, with
/// `Z` when `zoned`; `None` outside the years 1 to 9999.
fn canonical_date_time(micros: i64, zoned: bool) -> Option<String> {
    let (year, month, day) = civil_from_days(micros.div_euclid(MICROS_PER_DAY));
    if !(1..=9999).contains(&year) {
        return None;
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
    if zoned {
        canonical.push('Z');
    }
    Some(canonical)
}

/// The UTC `xsd:dateTime` `micros` from 1970-01-01T00:00:00Z, in the form
/// the engine writes `timestamptz` values; `None` outside the years 1 to
/// 9999.
pub(crate) fn utc_date_time(micros: i64) -> Option<DateTime> {
    Some(DateTime {
        canonical: canonical_date_time(micros, true)?,
        zoned: true,
    })
}

/// The components of an `xsd:dateTime` in its own time zone, which the
/// functions of SPARQL 1.1 §17.4.5 return (XPath F&O 3.1 §9.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DateTimeParts {
    pub year: i64,
    pub month: i64,
    pub day: i64,
    pub hours: i64,
    pub minutes: i64,
    /// The seconds with their fraction, as a canonical `xsd:decimal`.
    pub seconds: String,
    /// The time zone as written and its offset in minutes.
    pub zone: Option<(String, i64)>,
}

/// The components of the `xsd:dateTime` lexical form `lexical`: `Ok(None)`
/// when it is not one, and an error for a value the engine does not
/// support, as for comparisons. `24:00:00` is the first instant of the
/// next day (XML Schema 1.1 Part 2 §3.3.7).
pub(crate) fn date_time_parts(lexical: &str) -> Result<Option<DateTimeParts>, &'static str> {
    if date_time(lexical)?.is_none() {
        return Ok(None);
    }
    let (local, zone) = split_zone(lexical);
    let local = date_time(local)?.expect("the lexical form without its time zone is valid");
    let c = local.canonical.as_str();
    let field = |range: std::ops::Range<usize>| -> i64 {
        c[range].parse().expect("canonical dateTime digits")
    };
    let fraction = c.get(20..).unwrap_or("");
    let seconds = field(17..19);
    let zone = zone.map(|zone| {
        let minutes = match zone.as_bytes() {
            [sign, h1, h2, b':', m1, m2] => {
                let digit = |d: &u8| i64::from(d - b'0');
                let minutes = (digit(h1) * 10 + digit(h2)) * 60 + digit(m1) * 10 + digit(m2);
                if *sign == b'-' { -minutes } else { minutes }
            }
            _ => 0,
        };
        (zone.to_owned(), minutes)
    });
    Ok(Some(DateTimeParts {
        year: field(0..4),
        month: field(5..7),
        day: field(8..10),
        hours: field(11..13),
        minutes: field(14..16),
        seconds: if fraction.is_empty() {
            seconds.to_string()
        } else {
            format!("{seconds}.{fraction}")
        },
        zone,
    }))
}

/// Why an `xsd:date` or `xsd:time` argument of a function of SPARQL 1.1
/// §17.4.5 is rejected.
pub(crate) const DATE_OR_TIME_ARGUMENT: &str = "YEAR, MONTH, DAY, HOURS, MINUTES, SECONDS, \
     TIMEZONE and TZ of xsd:date and xsd:time values: SPARQL 1.1 defines them only for \
     xsd:dateTime, while engines that extend them differ";

/// Whether `datatype` is `xsd:date` or `xsd:time`.
pub(crate) fn is_date_or_time(datatype: &str) -> bool {
    matches!(datatype.strip_prefix(XSD), Some("date" | "time"))
}

/// The canonical `xsd:dayTimeDuration` of a time zone offset in minutes
/// (the canonical mapping of `duration`, XML Schema 1.1 Part 2), as
/// fn:timezone-from-dateTime returns it.
pub(crate) fn zone_duration(minutes: i64) -> String {
    if minutes == 0 {
        return "PT0S".to_owned();
    }
    let mut out = String::new();
    if minutes < 0 {
        out.push('-');
    }
    out.push_str("PT");
    let (h, m) = (minutes.abs() / 60, minutes.abs() % 60);
    if h > 0 {
        out.push_str(&format!("{h}H"));
    }
    if m > 0 {
        out.push_str(&format!("{m}M"));
    }
    out
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

/// The longest string that is cast to a number. Longer strings are an
/// error, as XPath F&O 1.0 §17.1.3 allows for values beyond the limits of
/// an implementation (err:FOCA0003, err:FOCA0006); within it, PostgreSQL's
/// `numeric` input holds every valid lexical form whose exponent has at
/// most four digits.
pub(crate) const MAX_CAST_LENGTH: usize = 1000;

/// Why casting an `xsd:double` to `xsd:decimal` is rejected.
pub(crate) const DOUBLE_TO_DECIMAL: &str = "xsd:decimal of an xsd:double: XPath F&O 1.0 \
     §17.1.3.3 gives the decimal closest to the double that the implementation can represent, \
     which differs between engines";

/// The characters of the XML Schema `whiteSpace` facet `collapse`, which
/// a cast from a string strips around the lexical form (XPath F&O 1.0
/// §17.1.1).
fn is_xml_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

/// The lexical form and datatype of `cast` applied to a literal with
/// `lexical` and `datatype` (SPARQL 1.1 §17.5, XPath F&O 1.0 §17.1):
/// `Ok(None)` for an error, a cast the table does not allow or a lexical
/// form not valid for the target datatype, and `Err` with the reason for
/// one that is not supported.
pub(crate) fn cast(
    cast: Cast,
    lexical: &str,
    datatype: &str,
) -> Result<Option<(String, &'static str)>, String> {
    let lexical = match value(lexical, datatype) {
        Value::String => cast_string(cast, lexical),
        Value::Numeric(n) => cast_number(cast, &n)?,
        Value::Boolean(b) => Some(match cast {
            Cast::Boolean => b.to_string(),
            Cast::Integer | Cast::Decimal => u8::from(b).to_string(),
            Cast::Double => canonical_double(f64::from(u8::from(b))),
        }),
        Value::Unsupported(reason) => return Err(reason),
        Value::DateTime(_) | Value::Other | Value::IllTyped => None,
    };
    Ok(lexical.map(|lexical| (lexical, cast.datatype())))
}

/// A string cast to `cast`: its lexical form, whitespace stripped, read
/// in the lexical space of the target datatype (XML Schema 1.1 Part 2).
fn cast_string(cast: Cast, lexical: &str) -> Option<String> {
    if cast != Cast::Boolean && lexical.chars().count() > MAX_CAST_LENGTH {
        return None;
    }
    let lexical = lexical.trim_matches(is_xml_space);
    match cast {
        Cast::Boolean => match lexical {
            "true" | "1" => Some("true".to_owned()),
            "false" | "0" => Some("false".to_owned()),
            _ => None,
        },
        Cast::Integer => canonical_integer(lexical),
        Cast::Decimal if is_decimal(lexical) => canonical_decimal(lexical),
        Cast::Decimal => None,
        Cast::Double => double(lexical).map(canonical_double),
    }
}

/// A number cast to `cast`. A decimal or a double becomes an integer by
/// discarding its fraction, and is an error for NaN and the infinities;
/// a number becomes a boolean that is false for zero and NaN (XPath F&O
/// 1.0 §17.1.3, §17.1.6).
fn cast_number(cast: Cast, n: &Number) -> Result<Option<String>, String> {
    let exact = n.decimal.as_deref();
    Ok(match (cast, exact) {
        (Cast::Boolean, Some(decimal)) => Some((decimal != "0").to_string()),
        (Cast::Boolean, None) => Some((n.double != 0.0 && !n.double.is_nan()).to_string()),
        (Cast::Integer, Some(decimal)) => {
            let integer = decimal.split('.').next().unwrap_or(decimal);
            canonical_decimal(integer)
        }
        (Cast::Integer, None) => exact_integer(n.double.trunc()),
        (Cast::Decimal, Some(decimal)) => Some(decimal.to_owned()),
        (Cast::Decimal, None) => return Err(DOUBLE_TO_DECIMAL.to_owned()),
        (Cast::Double, _) => Some(canonical_double(n.double)),
    })
}

/// The decimal digits of an integral double, `None` for NaN and the
/// infinities.
fn exact_integer(x: f64) -> Option<String> {
    if !x.is_finite() {
        return None;
    }
    if x.abs() < 2f64.powi(63) {
        return Some((x as i64).to_string());
    }
    // `x` is the 53-bit significand times 2^exponent, with an exponent of
    // at least 11 here; the digits are in base 10^9, least significant
    // first.
    let bits = x.to_bits();
    let exponent = ((bits >> 52) & 0x7ff) - 1075;
    let significand = (bits & ((1 << 52) - 1)) | (1 << 52);
    let mut limbs = vec![
        (significand % 1_000_000_000) as u32,
        (significand / 1_000_000_000 % 1_000_000_000) as u32,
        (significand / 1_000_000_000_000_000_000) as u32,
    ];
    for _ in 0..exponent {
        let mut carry = 0;
        for limb in &mut limbs {
            let doubled = u64::from(*limb) * 2 + carry;
            *limb = (doubled % 1_000_000_000) as u32;
            carry = doubled / 1_000_000_000;
        }
        if carry > 0 {
            limbs.push(carry as u32);
        }
    }
    while limbs.last() == Some(&0) {
        limbs.pop();
    }
    let mut digits = if x < 0.0 {
        "-".to_owned()
    } else {
        String::new()
    };
    let mut limbs = limbs.iter().rev();
    if let Some(first) = limbs.next() {
        digits.push_str(&first.to_string());
    }
    for limb in limbs {
        digits.push_str(&format!("{limb:09}"));
    }
    Some(digits)
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
    fn date_time_parts_are_local_to_the_time_zone() {
        let p = date_time_parts("2024-03-01T02:05:09.120-05:30")
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                p.year,
                p.month,
                p.day,
                p.hours,
                p.minutes,
                p.seconds.as_str()
            ),
            (2024, 3, 1, 2, 5, "9.12")
        );
        assert_eq!(p.zone, Some(("-05:30".to_owned(), -330)));
        let p = date_time_parts("2023-12-31T24:00:00Z").unwrap().unwrap();
        assert_eq!((p.year, p.month, p.day, p.hours), (2024, 1, 1, 0));
        assert_eq!(
            (p.seconds.as_str(), p.zone),
            ("0", Some(("Z".to_owned(), 0)))
        );
        let p = date_time_parts("0001-01-01T00:00:00.000001")
            .unwrap()
            .unwrap();
        assert_eq!((p.year, p.seconds.as_str(), p.zone), (1, "0.000001", None));
        assert_eq!(
            date_time_parts("2024-01-01T00:00:00+14:00")
                .unwrap()
                .unwrap()
                .zone,
            Some(("+14:00".to_owned(), 840))
        );
        assert_eq!(date_time_parts("2024-02-30T00:00:00"), Ok(None));
        assert_eq!(date_time_parts("2024-01-01"), Ok(None));
        assert!(date_time_parts("10000-01-01T00:00:00").is_err());
        // The instant is in year 10000.
        assert!(date_time_parts("9999-12-31T23:00:00-01:00").is_err());
    }

    #[test]
    fn zone_durations_are_canonical() {
        for (minutes, duration) in [
            (0, "PT0S"),
            (60, "PT1H"),
            (-300, "-PT5H"),
            (330, "PT5H30M"),
            (-45, "-PT45M"),
            (840, "PT14H"),
        ] {
            assert_eq!(zone_duration(minutes), duration);
        }
    }

    #[test]
    fn utc_date_times_are_written_as_timestamptz_values() {
        assert_eq!(utc_date_time(0).unwrap().canonical, "1970-01-01T00:00:00Z");
        assert_eq!(
            utc_date_time(1_700_000_000_123_400).unwrap().canonical,
            "2023-11-14T22:13:20.1234Z"
        );
        assert_eq!(
            date_time_value(&utc_date_time(-1).unwrap().canonical).canonical,
            "1969-12-31T23:59:59.999999Z"
        );
    }

    #[test]
    fn civil_days_round_trip() {
        for days in [-719_162, -1, 0, 10_957, 19_782, 2_932_896] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
    }

    fn cast_of(target: Cast, lexical: &str, datatype: &str) -> Option<String> {
        cast(target, lexical, &xsd(datatype))
            .unwrap()
            .map(|(lexical, datatype)| {
                assert_eq!(datatype, target.datatype());
                lexical
            })
    }

    #[test]
    fn strings_cast_by_the_lexical_space_of_the_target() {
        let integer = |s| cast_of(Cast::Integer, s, "string");
        assert_eq!(integer(" +01\t\n").as_deref(), Some("1"));
        assert_eq!(integer("-0").as_deref(), Some("0"));
        for invalid in ["1.0", "1e0", "", " ", "1 2", "0x1", "+-1", "\u{a0}1", "١"] {
            assert_eq!(integer(invalid), None, "{invalid:?}");
        }
        let decimal = |s| cast_of(Cast::Decimal, s, "string");
        assert_eq!(decimal("1.50").as_deref(), Some("1.5"));
        assert_eq!(decimal("1.").as_deref(), Some("1"));
        assert_eq!(decimal("-.5").as_deref(), Some("-0.5"));
        assert_eq!(decimal("-0.0").as_deref(), Some("0"));
        assert_eq!(decimal("1e3"), None);
        assert_eq!(decimal("INF"), None);
        let double = |s| cast_of(Cast::Double, s, "string");
        assert_eq!(double("1").as_deref(), Some("1.0E0"));
        assert_eq!(double(" 1e3 ").as_deref(), Some("1.0E3"));
        assert_eq!(double(".5E-1").as_deref(), Some("5.0E-2"));
        assert_eq!(double("1.e2").as_deref(), Some("1.0E2"));
        assert_eq!(double("-0").as_deref(), Some("-0.0E0"));
        assert_eq!(double("+INF").as_deref(), Some("INF"));
        assert_eq!(double("-INF").as_deref(), Some("-INF"));
        assert_eq!(double("NaN").as_deref(), Some("NaN"));
        // floatingPointRound: an infinity past the largest double, a zero
        // of the sign below half the smallest subnormal.
        assert_eq!(double("1e99999").as_deref(), Some("INF"));
        assert_eq!(double("1.7976931348623159e308").as_deref(), Some("INF"));
        assert_eq!(
            double("1.7976931348623158e308").as_deref(),
            Some("1.7976931348623157E308")
        );
        assert_eq!(double("-1e-99999").as_deref(), Some("-0.0E0"));
        assert_eq!(
            double("2.4703282292062328e-324").as_deref(),
            Some("5.0E-324")
        );
        assert_eq!(double("2.4703282292062327e-324").as_deref(), Some("0.0E0"));
        for invalid in [
            "inf", "Infinity", "-NaN", "1e", "e1", "1e1.5", "1d0", "0x1p3",
        ] {
            assert_eq!(double(invalid), None, "{invalid:?}");
        }
        let boolean = |s| cast_of(Cast::Boolean, s, "string");
        assert_eq!(boolean(" true ").as_deref(), Some("true"));
        assert_eq!(boolean("1").as_deref(), Some("true"));
        assert_eq!(boolean("0").as_deref(), Some("false"));
        assert_eq!(boolean("TRUE"), None);
        assert_eq!(boolean("01"), None);
        // An implementation limit on the length of strings read as numbers.
        let long = "1".repeat(MAX_CAST_LENGTH);
        assert_eq!(integer(&long).as_deref(), Some(long.as_str()));
        assert_eq!(integer(&format!("{long}1")), None);
        assert_eq!(double(&format!(" {long}")), None);
    }

    #[test]
    fn numbers_and_booleans_cast_by_value() {
        assert_eq!(
            cast_of(Cast::Integer, "-2.75", "decimal").as_deref(),
            Some("-2")
        );
        assert_eq!(
            cast_of(Cast::Integer, "-0.5", "decimal").as_deref(),
            Some("0")
        );
        assert_eq!(
            cast_of(Cast::Integer, "1.9e0", "double").as_deref(),
            Some("1")
        );
        assert_eq!(
            cast_of(Cast::Integer, "-0e0", "double").as_deref(),
            Some("0")
        );
        assert_eq!(
            cast_of(Cast::Integer, "1e21", "double").as_deref(),
            Some("1000000000000000000000")
        );
        assert_eq!(
            cast_of(Cast::Integer, "-1.2345678901234567e18", "double").as_deref(),
            Some("-1234567890123456768")
        );
        assert_eq!(
            cast_of(Cast::Integer, "9.223372036854775807e18", "double").as_deref(),
            Some("9223372036854775808")
        );
        let max = cast_of(Cast::Integer, "1.7976931348623157e308", "double").unwrap();
        assert_eq!(max.len(), 309);
        assert!(
            max.starts_with("17976931348623157081452742373170435679"),
            "{max}"
        );
        assert!(max.ends_with("858368"), "{max}");
        for special in ["NaN", "INF", "-INF"] {
            assert_eq!(cast_of(Cast::Integer, special, "double"), None);
        }
        assert_eq!(
            cast_of(Cast::Decimal, "007", "integer").as_deref(),
            Some("7")
        );
        assert_eq!(
            cast(Cast::Decimal, "1.5e0", &xsd("double")).unwrap_err(),
            DOUBLE_TO_DECIMAL
        );
        assert_eq!(
            cast_of(Cast::Double, "0.1", "decimal").as_deref(),
            Some("1.0E-1")
        );
        assert_eq!(
            cast_of(Cast::Double, "9007199254740993", "integer").as_deref(),
            Some("9.007199254740992E15")
        );
        assert_eq!(
            cast_of(Cast::Boolean, "0.0", "decimal").as_deref(),
            Some("false")
        );
        assert_eq!(
            cast_of(Cast::Boolean, "-0e0", "double").as_deref(),
            Some("false")
        );
        assert_eq!(
            cast_of(Cast::Boolean, "NaN", "double").as_deref(),
            Some("false")
        );
        assert_eq!(
            cast_of(Cast::Boolean, "-INF", "double").as_deref(),
            Some("true")
        );
        assert_eq!(
            cast_of(Cast::Integer, "true", "boolean").as_deref(),
            Some("1")
        );
        assert_eq!(cast_of(Cast::Decimal, "0", "boolean").as_deref(), Some("0"));
        assert_eq!(
            cast_of(Cast::Double, "1", "boolean").as_deref(),
            Some("1.0E0")
        );
        assert_eq!(
            cast_of(Cast::Boolean, "1", "boolean").as_deref(),
            Some("true")
        );
        // Ill-typed literals, dateTimes and other datatypes are errors;
        // derived integer types and xsd:float are not supported.
        assert_eq!(cast_of(Cast::Integer, "abc", "integer"), None);
        assert_eq!(
            cast_of(Cast::Integer, "2024-01-01T00:00:00", "dateTime"),
            None
        );
        assert_eq!(cast_of(Cast::Integer, "2024-01-01", "date"), None);
        assert_eq!(cast_of(Cast::Boolean, "1", "hexBinary"), None);
        assert!(cast(Cast::Integer, "1", &xsd("int")).is_err());
        assert!(cast(Cast::Double, "1", &xsd("float")).is_err());
    }
}
