// SPDX-License-Identifier: Apache-2.0
//! Constants to bind parameters, checked against the parameter's type
//! before PostgreSQL parses them.

use std::error::Error as StdError;

use bytes::BytesMut;
use tokio_postgres::types::{Format, IsNull, ToSql, Type, to_sql_checked};
use v12n_core::solution::{SqlValue, canonical_decimal};
use v12n_core::sql::TermKey;

use crate::decode::{Decoder, MICROS_PER_DAY, PG_EPOCH_DAYS};

type BoxError = Box<dyn StdError + Sync + Send>;

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

/// PostgreSQL's valid dates, as days from 2000-01-01: from 4714-11-24 BC
/// (Julian day 0) up to, not including, Julian day 2147483494. Binary
/// input outside this range is an error.
const MIN_DATE: i64 = -2_451_545;
const END_DATE: i64 = 2_147_483_494 - 2_451_545;
/// PostgreSQL's valid timestamps, as microseconds from 2000-01-01:
/// 4714-11-24 BC up to, not including, 294277-01-01.
const MIN_TIMESTAMP: i64 = -211_813_488_000_000_000;
const END_TIMESTAMP: i64 = 9_223_371_331_200_000_000;

/// A parameter value ready to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Bound {
    /// Parsed by the server with the input function of the parameter type.
    Text(String),
    /// Already in the type's binary format.
    Binary(Vec<u8>),
    /// No value of the parameter's type can match the constant. `NULL`
    /// makes the equality it is part of unknown, which empties its branch
    /// only, or drops one alternative of an `IN` list (see
    /// `SqlPlan::to_sql`).
    NoMatch,
}

impl Bound {
    pub fn is_no_match(&self) -> bool {
        matches!(self, Self::NoMatch)
    }
}

impl ToSql for Bound {
    fn to_sql(&self, _: &Type, out: &mut BytesMut) -> Result<IsNull, BoxError> {
        match self {
            Self::Text(text) => out.extend_from_slice(text.as_bytes()),
            Self::Binary(bytes) => out.extend_from_slice(bytes),
            Self::NoMatch => return Ok(IsNull::Yes),
        }
        Ok(IsNull::No)
    }

    fn accepts(_: &Type) -> bool {
        true
    }

    fn encode_format(&self, _: &Type) -> Format {
        match self {
            Self::Binary(_) => Format::Binary,
            Self::Text(_) | Self::NoMatch => Format::Text,
        }
    }

    to_sql_checked!();
}

/// Prepares a constant for a parameter the server typed as `ty`.
///
/// RDF terms are compared by lexical form, and a column value takes part
/// through its natural lexical form (R2RML §10.2), so a constant matches
/// only if some value of `ty` renders as exactly that text. The check is
/// a round trip through the decoder and [`SqlValue::natural_lexical_form`]
/// that build result terms: `007`, `+7`, `7.0` and `99999999999` are no
/// `int4` value's form, so they match nothing instead of matching 7 or
/// failing in PostgreSQL's parser. Values are sent in binary where the
/// format is simple, so the server never parses them at all.
///
/// `datatype` is the constant's datatype when it is a literal compared
/// with a column of natural datatype; it must be the datatype `ty` maps
/// to. Types without a decoder must already be compared as text (see
/// `SqlPlan::to_sql_with_text_params`).
pub(crate) fn bind(ty: &Type, value: &str, datatype: Option<&str>) -> Bound {
    let Some(decoder) = Decoder::for_type(ty) else {
        return Bound::Text(value.to_owned());
    };
    // PostgreSQL text cannot hold NUL, so no value renders as one.
    if value.contains('\0') {
        return Bound::NoMatch;
    }
    let (bound, sql_value) = match decoder {
        Decoder::Text => (
            Bound::Text(value.to_owned()),
            SqlValue::String(value.to_owned()),
        ),
        // Canonical decimals are valid numeric input and NUMERIC compares
        // by value, so `1.1` also finds a stored `1.10`, which renders as
        // `1.1` (canonical_decimal) like the constant.
        Decoder::Numeric => {
            let special = matches!(value, "NaN" | "Infinity" | "-Infinity");
            if !special && canonical_decimal(value).is_none() {
                return Bound::NoMatch;
            }
            (
                Bound::Text(value.to_owned()),
                SqlValue::Decimal(value.to_owned()),
            )
        }
        _ => {
            let Some(bytes) = encode(decoder, value) else {
                return Bound::NoMatch;
            };
            let Ok(sql_value) = decoder.decode(Some(&bytes)) else {
                return Bound::NoMatch;
            };
            (Bound::Binary(bytes), sql_value)
        }
    };
    if sql_value.natural_lexical_form().as_deref() != Some(value) {
        return Bound::NoMatch;
    }
    if let Some(datatype) = datatype
        && sql_value.natural_datatype().unwrap_or(XSD_STRING) != datatype
    {
        return Bound::NoMatch;
    }
    bound
}

/// The key a comparison of a parameter of type `ty` holding `value` with a
/// column also compares (see [`TermKey`]): the sign of a floating point
/// zero, since `-0 = 0` holds while `-0.0E0` and `0.0E0` are different
/// terms, and the length of a `bpchar`, whose `=` ignores trailing spaces.
/// Other floating point values are equal exactly when their terms are, so
/// their comparisons stay as they are.
pub(crate) fn term_key(ty: &Type, value: &str) -> Option<TermKey> {
    match *ty {
        Type::FLOAT4 | Type::FLOAT8 => value
            .parse::<f64>()
            .is_ok_and(|v| v == 0.0)
            .then_some(TermKey::Text),
        Type::BPCHAR => Some(TermKey::OctetLength),
        _ => None,
    }
}

/// Binary format of `value` read as the decoder's type, `None` when it is
/// not a value of that type. Lenient where the round trip in [`bind`]
/// rejects non-canonical forms anyway, strict about ranges the server
/// would refuse.
fn encode(decoder: Decoder, value: &str) -> Option<Vec<u8>> {
    Some(match decoder {
        Decoder::Bool => match value {
            "true" => vec![1],
            "false" => vec![0],
            _ => return None,
        },
        Decoder::Int2 => value.parse::<i16>().ok()?.to_be_bytes().to_vec(),
        Decoder::Int4 => value.parse::<i32>().ok()?.to_be_bytes().to_vec(),
        Decoder::Int8 => value.parse::<i64>().ok()?.to_be_bytes().to_vec(),
        Decoder::Float4 => value.parse::<f32>().ok()?.to_be_bytes().to_vec(),
        Decoder::Float8 => value.parse::<f64>().ok()?.to_be_bytes().to_vec(),
        Decoder::Bytea => parse_hex(value)?,
        Decoder::Uuid => {
            let hex: String = value.split('-').collect();
            let bytes = parse_hex(&hex)?;
            if bytes.len() != 16 {
                return None;
            }
            bytes
        }
        Decoder::Date => match value {
            "infinity" => i32::MAX.to_be_bytes().to_vec(),
            "-infinity" => i32::MIN.to_be_bytes().to_vec(),
            _ => {
                let days = parse_date(value)? - PG_EPOCH_DAYS;
                if !(MIN_DATE..END_DATE).contains(&days) {
                    return None;
                }
                i32::try_from(days).ok()?.to_be_bytes().to_vec()
            }
        },
        Decoder::Time => {
            let micros = parse_time(value)?;
            // 24:00:00 is a valid `time`.
            if micros > MICROS_PER_DAY {
                return None;
            }
            micros.to_be_bytes().to_vec()
        }
        Decoder::Timestamp | Decoder::Timestamptz => {
            let micros = match value {
                "infinity" => i64::MAX,
                "-infinity" => i64::MIN,
                _ => {
                    let value = if decoder == Decoder::Timestamptz {
                        value.strip_suffix('Z')?
                    } else {
                        value
                    };
                    let (date, time) = value.split_once('T')?;
                    let days = parse_date(date)? - PG_EPOCH_DAYS;
                    let micros = days
                        .checked_mul(MICROS_PER_DAY)?
                        .checked_add(parse_time(time)?)?;
                    if !(MIN_TIMESTAMP..END_TIMESTAMP).contains(&micros) {
                        return None;
                    }
                    micros
                }
            };
            micros.to_be_bytes().to_vec()
        }
        Decoder::Text | Decoder::Numeric => value.as_bytes().to_vec(),
    })
}

fn parse_hex(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) || !value.is_ascii() {
        return None;
    }
    (0..value.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&value[i..i + 2], 16).ok())
        .collect()
}

/// Days from 1970-01-01 for `[-]YYYY-MM-DD`, year 0 being 1 BC as in the
/// date decoder. An impossible day such as 02-30 yields a different date,
/// which the round trip rejects.
fn parse_date(value: &str) -> Option<i64> {
    let (negative, rest) = match value.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, value),
    };
    let mut parts = rest.split('-');
    let (year, month, day) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || year.len() < 4 || month.len() != 2 || day.len() != 2 {
        return None;
    }
    // Far beyond PostgreSQL's range, and keeps the arithmetic in i64.
    if year.len() > 9 {
        return None;
    }
    let year: i64 = digits(year)?;
    let year = if negative { -year } else { year };
    let (month, day): (i64, i64) = (digits(month)?, digits(day)?);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(days_from_civil(year, month, day))
}

/// Microseconds for `hh:mm:ss` with an optional fraction of at most six
/// digits, PostgreSQL's precision.
fn parse_time(value: &str) -> Option<i64> {
    let (clock, fraction) = match value.split_once('.') {
        Some((clock, fraction)) => (clock, fraction),
        None => (value, ""),
    };
    let mut parts = clock.split(':');
    let (h, m, s) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || [h, m, s].iter().any(|p| p.len() != 2) {
        return None;
    }
    if (value.contains('.') && fraction.is_empty()) || fraction.len() > 6 {
        return None;
    }
    let (h, m, s): (i64, i64, i64) = (digits(h)?, digits(m)?, digits(s)?);
    if h > 24 || m > 59 || s > 59 {
        return None;
    }
    let fraction: i64 = if fraction.is_empty() {
        0
    } else {
        digits::<i64>(fraction)? * 10i64.pow(6 - fraction.len() as u32)
    };
    Some(((h * 60 + m) * 60 + s) * 1_000_000 + fraction)
}

/// Parses ASCII digits only; `str::parse` also takes a sign.
fn digits<T: std::str::FromStr>(value: &str) -> Option<T> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

/// Inverse of the date decoder's `civil_from_days` (H. Hinnant).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

    fn matches(ty: &Type, value: &str) -> bool {
        !bind(ty, value, None).is_no_match()
    }

    #[test]
    fn integers_need_the_canonical_form_and_the_range() {
        assert_eq!(
            bind(&Type::INT4, "7", None),
            Bound::Binary(vec![0, 0, 0, 7])
        );
        for value in ["007", "+7", "7.0", "abc", "", " 7", "99999999999"] {
            assert!(!matches(&Type::INT4, value), "{value}");
        }
        assert!(!matches(&Type::INT2, "40000"));
        assert!(matches(&Type::INT2, "-32768"));
        assert!(matches(&Type::INT8, "99999999999"));
        assert!(!matches(&Type::INT8, "9223372036854775808"));
    }

    #[test]
    fn literal_datatype_must_be_the_natural_one() {
        let integer = format!("{XSD}integer");
        let decimal = format!("{XSD}decimal");
        let string = format!("{XSD}string");
        assert!(!bind(&Type::INT4, "7", Some(&integer)).is_no_match());
        assert!(bind(&Type::INT4, "7", Some(&decimal)).is_no_match());
        assert!(bind(&Type::INT4, "7", Some(&string)).is_no_match());
        assert!(!bind(&Type::TEXT, "7", Some(&string)).is_no_match());
        assert!(bind(&Type::TEXT, "7", Some(&integer)).is_no_match());
        assert!(!bind(&Type::NUMERIC, "7", Some(&decimal)).is_no_match());
        assert!(bind(&Type::NUMERIC, "7", Some(&integer)).is_no_match());
    }

    #[test]
    fn numeric_uses_the_canonical_decimal_form() {
        assert_eq!(bind(&Type::NUMERIC, "1.1", None), Bound::Text("1.1".into()));
        // XML Schema 1.1 decimalCanonicalMap: integers have no point.
        for value in ["1", "-12", "0", "-0.5", "NaN"] {
            assert!(matches(&Type::NUMERIC, value), "{value}");
        }
        for value in [
            "1.10", "01.1", "1.0", "-0", "0.0", "+1.1", ".5", "1e3", "abc", "nan",
        ] {
            assert!(!matches(&Type::NUMERIC, value), "{value}");
        }
    }

    #[test]
    fn floats_and_booleans_use_canonical_forms() {
        assert!(matches(&Type::FLOAT8, "1.5E0"));
        assert!(matches(&Type::FLOAT8, "INF"));
        for value in ["1.5", "1.50E0", "15E-1", "inf", "abc"] {
            assert!(!matches(&Type::FLOAT8, value), "{value}");
        }
        assert!(matches(&Type::FLOAT4, "1.0E-1"));
        assert!(!matches(&Type::FLOAT4, "1.0E39"));
        assert!(matches(&Type::BOOL, "false"));
        for value in ["t", "1", "TRUE", "yes"] {
            assert!(!matches(&Type::BOOL, value), "{value}");
        }
    }

    #[test]
    fn dates_and_times_round_trip_through_the_decoder() {
        assert!(matches(&Type::DATE, "2024-02-29"));
        assert!(matches(&Type::DATE, "0001-01-01"));
        assert!(matches(&Type::DATE, "-0001-01-01"));
        assert!(matches(&Type::DATE, "infinity"));
        for value in [
            "2024-2-29",
            "2023-02-29",
            "2024-02-30",
            "2024-13-01",
            "24-02-29",
            "2024-02-29Z",
            "-5000-01-01",
            "abc",
        ] {
            assert!(!matches(&Type::DATE, value), "{value}");
        }
        assert!(matches(&Type::TIME, "10:00:00.25"));
        assert!(matches(&Type::TIME, "24:00:00"));
        for value in ["10:00:00.250", "10:00", "10:00:00.", "25:00:00", "10:60:00"] {
            assert!(!matches(&Type::TIME, value), "{value}");
        }
        assert!(matches(&Type::TIMESTAMP, "2024-01-02T03:04:05.5"));
        assert!(matches(&Type::TIMESTAMPTZ, "2024-01-01T18:04:05Z"));
        for value in [
            "2024-01-02 03:04:05.5",
            "2024-01-02T24:00:00",
            "2024-01-02T03:04:05.1234567",
            "2024-01-02T03:04:05+09:00",
        ] {
            assert!(!matches(&Type::TIMESTAMP, value), "{value}");
            assert!(!matches(&Type::TIMESTAMPTZ, value), "{value}");
        }
        assert!(!matches(&Type::TIMESTAMP, "2024-01-01T18:04:05Z"));
    }

    #[test]
    fn binaries_and_uuids_use_the_decoders_case() {
        assert_eq!(
            bind(&Type::BYTEA, "0A0B", None),
            Bound::Binary(vec![10, 11])
        );
        assert!(!matches(&Type::BYTEA, "0a0b"));
        assert!(!matches(&Type::BYTEA, "\\x0a0b"));
        assert!(!matches(&Type::BYTEA, "0A0"));
        let uuid = "00000000-0000-0000-0000-00000000000a";
        assert!(matches(&Type::UUID, uuid));
        assert!(!matches(&Type::UUID, &uuid.to_uppercase()));
        assert!(!matches(&Type::UUID, "000000000000000000000000000000000a"));
        assert!(!matches(&Type::UUID, "abc"));
    }

    #[test]
    fn text_matches_anything_but_nul_and_other_types_go_through() {
        assert_eq!(bind(&Type::TEXT, "007", None), Bound::Text("007".into()));
        assert!(!matches(&Type::VARCHAR, "a\0b"));
        // Compared as text in SQL, so the server never parses it as an interval.
        assert_eq!(
            bind(&Type::INTERVAL, "1 day", None),
            Bound::Text("1 day".into())
        );
    }

    #[test]
    fn zeros_and_padded_strings_compare_a_key_too() {
        for value in ["0.0E0", "-0.0E0"] {
            assert_eq!(term_key(&Type::FLOAT8, value), Some(TermKey::Text));
            assert_eq!(term_key(&Type::FLOAT4, value), Some(TermKey::Text));
        }
        // Other values are equal exactly when their terms are.
        for value in ["1.5E0", "NaN", "INF", "abc"] {
            assert_eq!(term_key(&Type::FLOAT8, value), None, "{value}");
        }
        assert_eq!(term_key(&Type::BPCHAR, "a"), Some(TermKey::OctetLength));
        assert_eq!(term_key(&Type::TEXT, "a"), None);
        assert_eq!(term_key(&Type::INT4, "0"), None);
    }

    #[test]
    fn days_from_civil_inverts_the_decoder() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 1, 1), PG_EPOCH_DAYS);
        assert_eq!(days_from_civil(0, 1, 1), -719_528);
    }
}
