// SPDX-License-Identifier: Apache-2.0
//! PostgreSQL binary values to [`SqlValue`].

use std::error::Error as StdError;

use tokio_postgres::types::{FromSql, Type};
use v12n_core::solution::SqlValue;
use v12n_core::sql::{ColumnType, Comparison, Lexical, TermKey, TermNormal, quote_identifier};

type BoxError = Box<dyn StdError + Sync + Send>;

/// Days from 1970-01-01 to 2000-01-01, the epoch of PostgreSQL dates and
/// timestamps.
pub(crate) const PG_EPOCH_DAYS: i64 = 10_957;
pub(crate) const MICROS_PER_DAY: i64 = 86_400_000_000;

/// How to read one result column, chosen from its PostgreSQL type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decoder {
    Bool,
    Int2,
    Int4,
    Int8,
    Float4,
    Float8,
    Numeric,
    Text,
    Bytea,
    Date,
    Time,
    Timestamp,
    Timestamptz,
    Uuid,
}

impl Decoder {
    /// `None` for types without a decoder; those are cast to text in SQL.
    pub fn for_type(ty: &Type) -> Option<Self> {
        Some(match *ty {
            Type::BOOL => Self::Bool,
            Type::INT2 => Self::Int2,
            Type::INT4 => Self::Int4,
            Type::INT8 => Self::Int8,
            Type::FLOAT4 => Self::Float4,
            Type::FLOAT8 => Self::Float8,
            Type::NUMERIC => Self::Numeric,
            Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME | Type::UNKNOWN => Self::Text,
            Type::BYTEA => Self::Bytea,
            Type::DATE => Self::Date,
            Type::TIME => Self::Time,
            Type::TIMESTAMP => Self::Timestamp,
            Type::TIMESTAMPTZ => Self::Timestamptz,
            Type::UUID => Self::Uuid,
            _ => return None,
        })
    }

    /// IRI of the natural datatype of the values this decoder produces
    /// (R2RML §10.2); `None` for `xsd:string`.
    fn natural_datatype(self) -> Option<&'static str> {
        Some(match self {
            Self::Bool => "http://www.w3.org/2001/XMLSchema#boolean",
            Self::Int2 | Self::Int4 | Self::Int8 => "http://www.w3.org/2001/XMLSchema#integer",
            Self::Float4 | Self::Float8 => "http://www.w3.org/2001/XMLSchema#double",
            Self::Numeric => "http://www.w3.org/2001/XMLSchema#decimal",
            Self::Bytea => "http://www.w3.org/2001/XMLSchema#hexBinary",
            Self::Date => "http://www.w3.org/2001/XMLSchema#date",
            Self::Time => "http://www.w3.org/2001/XMLSchema#time",
            Self::Timestamp | Self::Timestamptz => "http://www.w3.org/2001/XMLSchema#dateTime",
            Self::Text | Self::Uuid => return None,
        })
    }

    /// Decodes a value in PostgreSQL's binary format.
    pub fn decode(self, raw: Option<&[u8]>) -> Result<SqlValue, String> {
        let Some(raw) = raw else {
            return Ok(SqlValue::Null);
        };
        let wrong_size = || format!("{self:?} value has {} bytes", raw.len());
        Ok(match self {
            Self::Bool => SqlValue::Boolean(*raw.first().ok_or_else(wrong_size)? != 0),
            Self::Int2 => SqlValue::Integer(i16::from_be_bytes(fixed(raw, wrong_size)?).into()),
            Self::Int4 => SqlValue::Integer(i32::from_be_bytes(fixed(raw, wrong_size)?).into()),
            Self::Int8 => SqlValue::Integer(i64::from_be_bytes(fixed(raw, wrong_size)?)),
            Self::Float4 => SqlValue::Double(widen(f32::from_be_bytes(fixed(raw, wrong_size)?))),
            Self::Float8 => SqlValue::Double(f64::from_be_bytes(fixed(raw, wrong_size)?)),
            Self::Numeric => decode_numeric(raw)?,
            Self::Text => SqlValue::String(
                std::str::from_utf8(raw)
                    .map_err(|e| e.to_string())?
                    .to_owned(),
            ),
            Self::Bytea => SqlValue::Binary(raw.to_vec()),
            Self::Date => {
                let days = i32::from_be_bytes(fixed(raw, wrong_size)?);
                match days {
                    i32::MAX => SqlValue::String("infinity".to_owned()),
                    i32::MIN => SqlValue::String("-infinity".to_owned()),
                    _ => SqlValue::Date(format_date(i64::from(days) + PG_EPOCH_DAYS)),
                }
            }
            Self::Time => SqlValue::Time(format_time(i64::from_be_bytes(fixed(raw, wrong_size)?))),
            Self::Timestamp | Self::Timestamptz => {
                let micros = i64::from_be_bytes(fixed(raw, wrong_size)?);
                match micros {
                    i64::MAX => SqlValue::String("infinity".to_owned()),
                    i64::MIN => SqlValue::String("-infinity".to_owned()),
                    _ => {
                        let days = micros.div_euclid(MICROS_PER_DAY);
                        let time = micros.rem_euclid(MICROS_PER_DAY);
                        // A timestamptz is stored in UTC; `Z` is the
                        // canonical xsd:dateTime timezone for it.
                        let zone = if self == Self::Timestamptz { "Z" } else { "" };
                        SqlValue::DateTime(format!(
                            "{}T{}{zone}",
                            format_date(days + PG_EPOCH_DAYS),
                            format_time(time)
                        ))
                    }
                }
            }
            Self::Uuid => {
                let bytes: [u8; 16] = fixed(raw, wrong_size)?;
                let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
                SqlValue::String(format!(
                    "{}-{}-{}-{}-{}",
                    &hex[..8],
                    &hex[8..12],
                    &hex[12..16],
                    &hex[16..20],
                    &hex[20..]
                ))
            }
        })
    }
}

/// How terms built from a column of type `ty` compare in SQL.
///
/// Within a group, `=` on values agrees with equality of natural lexical
/// forms, except for floating point (`-0 = 0`) and types without a
/// decoder (`'1 day' = '24 hours'` for `interval`), whose text output is
/// compared instead, and `bpchar`, whose `=` ignores trailing spaces and
/// so also compares the lengths ([`TermKey`]), and `time`, whose
/// `24:00:00` and `00:00:00` are the same term, compared and deduplicated
/// in normal form ([`TermNormal`]). Constants and removing duplicates use
/// the same keys. Across groups only the natural lexical forms can be
/// compared, and SQL computes them only where the text output already is
/// that form, or for booleans, binaries and `numeric` (`trim_scale`).
/// Floating point and date/time types are left out until an SQL
/// expression is shown to match their canonical forms, and `bpchar`
/// because its cast to text drops the padding its values carry.
pub(crate) fn column_type(ty: &Type) -> ColumnType {
    let (group, comparison, lexical) = match (Decoder::for_type(ty), ty) {
        (_, &Type::BPCHAR) => ("bpchar", Comparison::Value, None),
        (Some(Decoder::Int2 | Decoder::Int4 | Decoder::Int8), _) => {
            ("integer", Comparison::Value, Some(Lexical::Text))
        }
        (Some(Decoder::Text), _) => ("text", Comparison::Value, Some(Lexical::Text)),
        (Some(Decoder::Uuid), _) => ("uuid", Comparison::Value, Some(Lexical::Text)),
        (Some(Decoder::Bool), _) => ("boolean", Comparison::Value, Some(Lexical::Boolean)),
        (Some(Decoder::Bytea), _) => ("bytea", Comparison::Value, Some(Lexical::Hex)),
        (Some(Decoder::Float4 | Decoder::Float8), _) => ("double", Comparison::Text, None),
        (Some(Decoder::Numeric), _) => ("numeric", Comparison::Value, Some(Lexical::Decimal)),
        (Some(Decoder::Date), _) => ("date", Comparison::Value, None),
        (Some(Decoder::Time), _) => ("time", Comparison::Value, None),
        (Some(Decoder::Timestamp), _) => ("timestamp", Comparison::Value, None),
        (Some(Decoder::Timestamptz), _) => ("timestamptz", Comparison::Value, None),
        // R2RML §10.2: cast to a string, which is exactly `::text`.
        (None, _) => (ty.name(), Comparison::Text, Some(Lexical::Text)),
    };
    let sql_type = format!(
        "{}.{}",
        quote_identifier(ty.schema()),
        quote_identifier(ty.name())
    );
    // UNION resolves `real` and `double precision` to the latter, which
    // changes the shortest form of a `real` value (0.1 is
    // 0.10000000149011612 as a double). Integer widths and the string
    // types widen without changing any value. A type without a decoder
    // only combines with itself.
    let union_class = match Decoder::for_type(ty) {
        Some(Decoder::Float4 | Decoder::Float8) | None => sql_type.clone(),
        Some(_) => group.to_owned(),
    };
    // Characters of the lexical forms SQL computes: integer output is `-`
    // and digits, uuid output lower-case hexadecimal digits and `-`,
    // booleans `true` or `false`, binaries upper-case hexadecimal digits,
    // and `trim_scale` of a numeric `-`, digits and `.`, or `NaN`,
    // `Infinity` and `-Infinity`. All are in RFC 3987 `iunreserved`.
    let lexical_unreserved = matches!(
        Decoder::for_type(ty),
        Some(
            Decoder::Int2
                | Decoder::Int4
                | Decoder::Int8
                | Decoder::Uuid
                | Decoder::Bool
                | Decoder::Bytea
                | Decoder::Numeric
        )
    );
    let term_key = match Decoder::for_type(ty) {
        Some(Decoder::Float4 | Decoder::Float8) => Some(TermKey::Text),
        _ if *ty == Type::BPCHAR => Some(TermKey::OctetLength),
        _ => None,
    };
    ColumnType {
        name: ty.name().to_owned(),
        sql_type,
        union_class,
        natural_datatype: Decoder::for_type(ty)
            .and_then(Decoder::natural_datatype)
            .map(str::to_owned),
        group: group.to_owned(),
        comparison,
        lexical,
        lexical_unreserved,
        term_key,
        term_normal: (*ty == Type::TIME).then_some(TermNormal::Time),
    }
}

fn fixed<const N: usize>(raw: &[u8], error: impl Fn() -> String) -> Result<[u8; N], String> {
    raw.try_into().map_err(|_| error())
}

/// The `f64` whose shortest form matches the `f32`'s, so a `REAL` `0.1`
/// prints as `1.0E-1` rather than `1.0000000149011612E-1`.
fn widen(value: f32) -> f64 {
    value.to_string().parse().unwrap_or(f64::from(value))
}

/// NUMERIC binary format: digit count, weight of the first base-10000
/// digit, sign, display scale, then the digits.
fn decode_numeric(raw: &[u8]) -> Result<SqlValue, String> {
    let word = |i: usize| -> Result<u16, String> {
        raw.get(2 * i..2 * i + 2)
            .map(|b| u16::from_be_bytes([b[0], b[1]]))
            .ok_or_else(|| "truncated numeric value".to_owned())
    };
    let ndigits = usize::from(word(0)?);
    let weight = i64::from(word(1)? as i16);
    let sign = word(2)?;
    let scale = usize::from(word(3)?);
    let special = match sign {
        0x0000 | 0x4000 => None,
        0xC000 => Some("NaN"),
        0xD000 => Some("Infinity"),
        0xF000 => Some("-Infinity"),
        _ => return Err(format!("invalid numeric sign {sign:#x}")),
    };
    if let Some(special) = special {
        return Ok(SqlValue::Decimal(special.to_owned()));
    }
    let digits = (0..ndigits)
        .map(|i| word(4 + i))
        .collect::<Result<Vec<_>, _>>()?;
    // Digit `k` has the place value 10000^(weight - k).
    let digit = |power: i64| -> u16 {
        usize::try_from(weight - power)
            .ok()
            .and_then(|k| digits.get(k).copied())
            .unwrap_or(0)
    };
    let mut out = String::new();
    if sign == 0x4000 {
        out.push('-');
    }
    if weight < 0 {
        out.push('0');
    } else {
        out.push_str(&digit(weight).to_string());
        for power in (0..weight).rev() {
            out.push_str(&format!("{:04}", digit(power)));
        }
    }
    if scale > 0 {
        let mut frac = String::new();
        let groups = scale.div_ceil(4) as i64;
        for power in 1..=groups {
            frac.push_str(&format!("{:04}", digit(-power)));
        }
        frac.truncate(scale);
        out.push('.');
        out.push_str(&frac);
    }
    Ok(SqlValue::Decimal(out))
}

/// `YYYY-MM-DD` for a day count from 1970-01-01 in the proleptic Gregorian
/// calendar (H. Hinnant's `civil_from_days`).
fn format_date(days: i64) -> String {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    if year < 0 {
        format!("-{:04}-{month:02}-{day:02}", -year)
    } else {
        format!("{year:04}-{month:02}-{day:02}")
    }
}

/// `hh:mm:ss` with the fraction only when it is not zero, as the canonical
/// `xsd:time` form has it.
fn format_time(micros: i64) -> String {
    // `24:00:00` is the same `xsd:time` value as `00:00:00`, whose
    // canonical form that is (XML Schema 1.1 `timeCanonicalMap`).
    let micros = micros % MICROS_PER_DAY;
    let seconds = micros / 1_000_000;
    let fraction = micros % 1_000_000;
    let mut out = format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    );
    if fraction != 0 {
        out.push_str(format!(".{fraction:06}").trim_end_matches('0'));
    }
    out
}

/// Any column value, still in binary format, so one row can be read before
/// knowing how to decode each column.
pub(crate) struct Raw<'a>(pub Option<&'a [u8]>);

impl<'a> FromSql<'a> for Raw<'a> {
    fn from_sql(_: &Type, raw: &'a [u8]) -> Result<Self, BoxError> {
        Ok(Self(Some(raw)))
    }

    fn from_sql_null(_: &Type) -> Result<Self, BoxError> {
        Ok(Self(None))
    }

    fn accepts(_: &Type) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numeric(ndigits: u16, weight: i16, sign: u16, scale: u16, digits: &[u16]) -> Vec<u8> {
        let mut out = Vec::new();
        for word in [ndigits, weight as u16, sign, scale].iter().chain(digits) {
            out.extend_from_slice(&word.to_be_bytes());
        }
        out
    }

    fn decimal(raw: &[u8]) -> String {
        match decode_numeric(raw).unwrap() {
            SqlValue::Decimal(d) => d,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn decode_numeric_keeps_scale_and_sign() {
        // 120000.50
        assert_eq!(decimal(&numeric(3, 1, 0, 2, &[12, 0, 5000])), "120000.50");
        // -0.0012
        assert_eq!(decimal(&numeric(1, -1, 0x4000, 4, &[12])), "-0.0012");
        // 10000 with no fraction digits stored
        assert_eq!(decimal(&numeric(1, 1, 0, 0, &[1])), "10000");
        // 0
        assert_eq!(decimal(&numeric(0, 0, 0, 0, &[])), "0");
        assert_eq!(decimal(&numeric(0, 0, 0xC000, 0, &[])), "NaN");
        assert!(decode_numeric(&[0, 1]).is_err());
    }

    #[test]
    fn format_date_handles_leap_years_and_epochs() {
        assert_eq!(format_date(PG_EPOCH_DAYS), "2000-01-01");
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(PG_EPOCH_DAYS + 8825), "2024-02-29");
        assert_eq!(format_date(-719_528), "0000-01-01");
    }

    #[test]
    fn decode_time_writes_midnight_in_canonical_form() {
        let decode = |micros: i64| Decoder::Time.decode(Some(&micros.to_be_bytes())).unwrap();
        assert_eq!(decode(45_000_500_000), SqlValue::Time("12:30:00.5".into()));
        assert_eq!(decode(0), SqlValue::Time("00:00:00".into()));
        assert_eq!(decode(MICROS_PER_DAY), SqlValue::Time("00:00:00".into()));
    }

    #[test]
    fn decode_timestamps_use_xsd_date_time_form() {
        // 2024-01-02 03:04:05.5 is 757 479 845.5 s after 2000-01-01.
        let micros: i64 = 757_479_845_500_000;
        let raw = micros.to_be_bytes();
        assert_eq!(
            Decoder::Timestamp.decode(Some(&raw)).unwrap(),
            SqlValue::DateTime("2024-01-02T03:04:05.5".into())
        );
        assert_eq!(
            Decoder::Timestamptz.decode(Some(&raw)).unwrap(),
            SqlValue::DateTime("2024-01-02T03:04:05.5Z".into())
        );
        let before_epoch = (-1i64).to_be_bytes();
        assert_eq!(
            Decoder::Timestamp.decode(Some(&before_epoch)).unwrap(),
            SqlValue::DateTime("1999-12-31T23:59:59.999999".into())
        );
        assert_eq!(
            Decoder::Date.decode(Some(&i32::MAX.to_be_bytes())).unwrap(),
            SqlValue::String("infinity".into())
        );
    }

    #[test]
    fn decode_scalars_and_null() {
        assert_eq!(Decoder::Int4.decode(None).unwrap(), SqlValue::Null);
        assert_eq!(
            Decoder::Int2.decode(Some(&(-3i16).to_be_bytes())).unwrap(),
            SqlValue::Integer(-3)
        );
        assert_eq!(
            Decoder::Float4.decode(Some(&0.1f32.to_be_bytes())).unwrap(),
            SqlValue::Double(0.1)
        );
        assert_eq!(
            Decoder::Bool.decode(Some(&[1])).unwrap(),
            SqlValue::Boolean(true)
        );
        assert_eq!(
            Decoder::Uuid.decode(Some(&[0xab; 16])).unwrap(),
            SqlValue::String("abababab-abab-abab-abab-abababababab".into())
        );
        assert!(Decoder::Int8.decode(Some(&[0, 1])).is_err());
    }

    /// Only types whose lexical forms never hold a character outside
    /// `iunreserved` skip the IRI-safe encoding in SQL.
    #[test]
    fn lexical_forms_of_numbers_uuids_booleans_and_binaries_are_unreserved() {
        for ty in [
            Type::INT2,
            Type::INT4,
            Type::INT8,
            Type::UUID,
            Type::BOOL,
            Type::BYTEA,
            Type::NUMERIC,
        ] {
            assert!(column_type(&ty).lexical_unreserved, "{ty}");
        }
        for ty in [
            Type::TEXT,
            Type::VARCHAR,
            Type::BPCHAR,
            Type::NAME,
            Type::FLOAT4,
            Type::FLOAT8,
            Type::DATE,
            Type::TIMESTAMP,
            Type::INET,
        ] {
            assert!(!column_type(&ty).lexical_unreserved, "{ty}");
        }
    }
}
