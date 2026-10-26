// SPDX-License-Identifier: Apache-2.0
//! The order SPARQL 1.1 §15.1 defines between solutions, for checking
//! results of queries with `ORDER BY`, `LIMIT` or `OFFSET` without
//! requiring an order the specification leaves open.

use std::cmp::Ordering;

use oxrdf::Term;
use spargebra::algebra::{Expression, GraphPattern, OrderExpression};
use spargebra::{Query, SparqlParser};

use crate::compare::Row;

const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

/// What the solution modifiers of a query make of the order of its
/// solutions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Order {
    /// The query has `ORDER BY`.
    pub ordered: bool,
    /// The comparators, each a projected variable and whether it is
    /// descending; `None` when one is anything else, whose values the
    /// results do not show, so that only the exact sequence can be checked.
    pub keys: Option<Vec<(String, bool)>>,
    /// The query has `LIMIT` or `OFFSET`, so that which solutions are
    /// returned depends on the order.
    pub sliced: bool,
}

impl Order {
    /// The order of `query`'s solutions, from its top-level modifiers.
    pub fn of(query: &str) -> Result<Self, String> {
        let query = SparqlParser::new()
            .parse_query(query)
            .map_err(|e| e.to_string())?;
        let Query::Select { pattern, .. } = query else {
            return Ok(Self::default());
        };
        let mut out = Self {
            keys: Some(Vec::new()),
            ..Self::default()
        };
        let mut projected: Option<Vec<String>> = None;
        let mut pattern = &pattern;
        loop {
            pattern = match pattern {
                GraphPattern::Slice { inner, .. } => {
                    out.sliced = true;
                    inner
                }
                GraphPattern::Distinct { inner } | GraphPattern::Reduced { inner } => inner,
                GraphPattern::Project { inner, variables } => {
                    projected = Some(variables.iter().map(|v| v.as_str().to_owned()).collect());
                    inner
                }
                GraphPattern::OrderBy { expression, .. } => {
                    out.ordered = true;
                    out.keys = expression
                        .iter()
                        .map(|e| {
                            let (e, descending) = match e {
                                OrderExpression::Asc(e) => (e, false),
                                OrderExpression::Desc(e) => (e, true),
                            };
                            match e {
                                Expression::Variable(v)
                                    if projected
                                        .as_ref()
                                        .is_some_and(|p| p.iter().any(|p| p == v.as_str())) =>
                                {
                                    Some((v.as_str().to_owned(), descending))
                                }
                                _ => None,
                            }
                        })
                        .collect();
                    break;
                }
                _ => break,
            };
        }
        Ok(out)
    }

    /// How two solutions compare by the comparators: `None` where SPARQL
    /// leaves their order open (§15.1: values that `<` does not order, and
    /// different terms that it finds neither smaller nor larger), and
    /// `Equal` where every comparator gives the same term, which leaves any
    /// order.
    pub fn compare(&self, a: &Row, b: &Row) -> Option<Ordering> {
        let keys = self.keys.as_ref()?;
        for (variable, descending) in keys {
            let order = term_order(a.get(variable), b.get(variable))?;
            match order {
                Ordering::Equal => continue,
                _ if *descending => return Some(order.reverse()),
                _ => return Some(order),
            }
        }
        Some(Ordering::Equal)
    }
}

/// The order of two values of one comparator: unbound first, then blank
/// nodes, IRIs and literals (SPARQL 1.1 §15.1); IRIs by code point, as
/// simple literals; literals by the `<` operator (§17.3) where it orders
/// them.
fn term_order(a: Option<&Term>, b: Option<&Term>) -> Option<Ordering> {
    let rank = |t: Option<&Term>| match t {
        None => 0,
        Some(Term::BlankNode(_)) => 1,
        Some(Term::NamedNode(_)) => 2,
        Some(_) => 3,
    };
    if a == b {
        return Some(Ordering::Equal);
    }
    match rank(a).cmp(&rank(b)) {
        Ordering::Equal => {}
        other => return Some(other),
    }
    match (a?, b?) {
        (Term::NamedNode(x), Term::NamedNode(y)) => Some(x.as_str().cmp(y.as_str())),
        (Term::Literal(x), Term::Literal(y)) => {
            if x.language().is_some() || y.language().is_some() {
                return None;
            }
            let order = value(x.value(), x.datatype().as_str())?
                .partial_cmp(&value(y.value(), y.datatype().as_str())?)?;
            // Different terms that `<` finds neither smaller nor larger,
            // such as 1 and 1.0, have no order.
            (order != Ordering::Equal).then_some(order)
        }
        _ => None,
    }
}

/// A literal's value where the `<` operator orders it.
#[derive(Debug, Clone, PartialEq)]
enum Value {
    /// An integer or decimal: sign, then the digits without leading zeros
    /// before the point and without trailing zeros after it.
    Exact(bool, String, String),
    Double(f64),
    String(String),
    Boolean(bool),
    /// Seconds from the epoch and the fraction's digits, and whether it
    /// has a time zone.
    DateTime(i64, String, bool),
}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        match (self, other) {
            (Self::Exact(..), Self::Exact(..)) => Some(exact_order(self, other)),
            (Self::Double(_) | Self::Exact(..), Self::Double(_) | Self::Exact(..)) => {
                // XPath promotes both to xsd:double; NaN orders with nothing.
                self.double().partial_cmp(&other.double())
            }
            (Self::String(x), Self::String(y)) => Some(x.cmp(y)),
            (Self::Boolean(x), Self::Boolean(y)) => Some(x.cmp(y)),
            (Self::DateTime(s, f, z), Self::DateTime(t, g, y)) if z == y => {
                Some(s.cmp(t).then_with(|| fraction_order(f, g)))
            }
            _ => None,
        }
    }
}

impl Value {
    fn double(&self) -> f64 {
        match self {
            Self::Double(d) => *d,
            Self::Exact(negative, int, frac) => {
                let sign = if *negative { "-" } else { "" };
                format!("{sign}{int}.{frac}0").parse().unwrap_or(f64::NAN)
            }
            _ => f64::NAN,
        }
    }
}

fn exact_order(a: &Value, b: &Value) -> Ordering {
    let (Value::Exact(an, ai, af), Value::Exact(bn, bi, bf)) = (a, b) else {
        unreachable!("exact values")
    };
    let zero = |i: &str, f: &str| i == "0" && f.is_empty();
    let (an, bn) = (*an && !zero(ai, af), *bn && !zero(bi, bf));
    let magnitude = ai
        .len()
        .cmp(&bi.len())
        .then_with(|| ai.cmp(bi))
        .then_with(|| fraction_order(af, bf));
    match (an, bn) {
        (false, false) => magnitude,
        (true, true) => magnitude.reverse(),
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
    }
}

/// Fractional digits compared as numbers: `5` and `50` are equal.
fn fraction_order(a: &str, b: &str) -> Ordering {
    let (a, b) = (a.trim_end_matches('0'), b.trim_end_matches('0'));
    a.cmp(b)
}

fn value(lexical: &str, datatype: &str) -> Option<Value> {
    let local = datatype.strip_prefix(XSD)?;
    match local {
        "string" => Some(Value::String(lexical.to_owned())),
        "boolean" => match lexical {
            "true" | "1" => Some(Value::Boolean(true)),
            "false" | "0" => Some(Value::Boolean(false)),
            _ => None,
        },
        "double" | "float" => {
            let valid = matches!(lexical, "INF" | "-INF" | "+INF" | "NaN")
                || (!lexical.is_empty()
                    && lexical
                        .bytes()
                        .all(|b| b.is_ascii_digit() || b"+-.eE".contains(&b)));
            valid
                .then(|| lexical.replace("INF", "inf").parse().ok())
                .flatten()
                .map(Value::Double)
        }
        "decimal" | "integer" | "long" | "int" | "short" | "byte" | "nonNegativeInteger"
        | "positiveInteger" | "nonPositiveInteger" | "negativeInteger" | "unsignedLong"
        | "unsignedInt" | "unsignedShort" | "unsignedByte" => {
            let (negative, digits) = match lexical.as_bytes().first() {
                Some(b'-') => (true, &lexical[1..]),
                Some(b'+') => (false, &lexical[1..]),
                _ => (false, lexical),
            };
            let (int, frac) = digits.split_once('.').unwrap_or((digits, ""));
            if (local != "decimal" && digits.contains('.'))
                || (int.is_empty() && frac.is_empty())
                || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit())
            {
                return None;
            }
            let int = match int.trim_start_matches('0') {
                "" => "0",
                int => int,
            };
            Some(Value::Exact(
                negative,
                int.to_owned(),
                frac.trim_end_matches('0').to_owned(),
            ))
        }
        "dateTime" => date_time(lexical),
        _ => None,
    }
}

/// `-?YYYY-MM-DDThh:mm:ss(.s+)?(Z|(+|-)hh:mm)?`, normalized to UTC when it
/// has a time zone.
fn date_time(lexical: &str) -> Option<Value> {
    let (negative, rest) = match lexical.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, lexical),
    };
    let (date, time) = rest.split_once('T')?;
    let mut date = date.splitn(3, '-');
    let year: i64 = date.next()?.parse().ok()?;
    let month: i64 = date.next()?.parse().ok()?;
    let day: i64 = date.next()?.parse().ok()?;
    let year = if negative { -year } else { year };
    let (clock, zone) = match time.find(['Z', '+', '-']) {
        Some(i) => (&time[..i], Some(&time[i..])),
        None => (time, None),
    };
    let (clock, fraction) = clock.split_once('.').unwrap_or((clock, ""));
    let mut clock = clock.splitn(3, ':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    let second: i64 = clock.next()?.parse().ok()?;
    let offset = match zone {
        None | Some("Z") => 0,
        Some(zone) => {
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let (h, m) = zone[1..].split_once(':')?;
            sign * (h.parse::<i64>().ok()? * 60 + m.parse::<i64>().ok()?) * 60
        }
    };
    let seconds =
        days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second - offset;
    Some(Value::DateTime(
        seconds,
        fraction.to_owned(),
        zone.is_some(),
    ))
}

/// Days from 1970-01-01 in the proleptic Gregorian calendar (H. Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year.rem_euclid(400);
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use oxrdf::vocab::xsd;
    use oxrdf::{BlankNode, Literal, NamedNode};

    use super::*;

    fn typed(lexical: &str, datatype: oxrdf::NamedNodeRef<'_>) -> Term {
        Literal::new_typed_literal(lexical, datatype).into()
    }

    fn iri(s: &str) -> Term {
        NamedNode::new(s).unwrap().into()
    }

    fn order(a: &Term, b: &Term) -> Option<Ordering> {
        term_order(Some(a), Some(b))
    }

    #[test]
    fn kinds_order_unbound_blank_nodes_iris_then_literals() {
        let blank: Term = BlankNode::default().into();
        let literal: Term = Literal::new_simple_literal("a").into();
        assert_eq!(term_order(None, Some(&blank)), Some(Ordering::Less));
        assert_eq!(order(&blank, &iri("http://a")), Some(Ordering::Less));
        assert_eq!(order(&iri("http://z"), &literal), Some(Ordering::Less));
        assert_eq!(term_order(None, None), Some(Ordering::Equal));
    }

    #[test]
    fn iris_and_strings_order_by_code_point() {
        // SPARQL 1.1 §15.1's example: "К" (U+041A) after "L".
        assert_eq!(
            order(
                &iri("http://script.example/Latin"),
                &iri("http://script.example/Кириллица")
            ),
            Some(Ordering::Less)
        );
        let s = |v: &str| -> Term { Literal::new_simple_literal(v).into() };
        assert_eq!(order(&s("Z"), &s("a")), Some(Ordering::Less));
        assert_eq!(order(&s("z"), &s("é")), Some(Ordering::Less));
    }

    #[test]
    fn numbers_order_by_value_across_datatypes() {
        let int = |v| typed(v, xsd::INTEGER);
        let dec = |v| typed(v, xsd::DECIMAL);
        let dbl = |v| typed(v, xsd::DOUBLE);
        assert_eq!(order(&int("9"), &int("10")), Some(Ordering::Less));
        assert_eq!(order(&int("-10"), &int("-9")), Some(Ordering::Less));
        assert_eq!(order(&dec("2.5"), &int("3")), Some(Ordering::Less));
        assert_eq!(order(&dec("0.25"), &dbl("2.0E-1")), Some(Ordering::Greater));
        assert_eq!(
            order(&dec("0.10000000000000000001"), &dec("0.1")),
            Some(Ordering::Greater)
        );
        // Equal values of different terms, and NaN, have no order.
        assert_eq!(order(&int("1"), &dec("1.0")), None);
        assert_eq!(order(&dbl("NaN"), &int("1")), None);
        // A number and a string: `<` is a type error.
        assert_eq!(
            order(&int("1"), &Literal::new_simple_literal("1").into()),
            None
        );
    }

    #[test]
    fn booleans_and_date_times() {
        assert_eq!(
            order(&typed("false", xsd::BOOLEAN), &typed("true", xsd::BOOLEAN)),
            Some(Ordering::Less)
        );
        let dt = |v| typed(v, xsd::DATE_TIME);
        assert_eq!(
            order(
                &dt("2024-01-01T00:00:00Z"),
                &dt("2024-01-01T08:00:00+09:00")
            ),
            Some(Ordering::Greater)
        );
        assert_eq!(
            order(&dt("2024-01-01T00:00:00.5"), &dt("2024-01-01T00:00:00.25")),
            Some(Ordering::Greater)
        );
        // With and without a time zone: depends on the implicit one.
        assert_eq!(
            order(&dt("2024-01-01T00:00:00Z"), &dt("2024-01-02T00:00:00")),
            None
        );
    }

    #[test]
    fn language_tagged_and_other_datatypes_have_no_order() {
        let en = |v| -> Term {
            Literal::new_language_tagged_literal(v, "en")
                .unwrap()
                .into()
        };
        assert_eq!(order(&en("a"), &en("b")), None);
        assert_eq!(
            order(
                &typed("2024-01-01", xsd::DATE),
                &typed("2024-01-02", xsd::DATE)
            ),
            None
        );
    }

    #[test]
    fn order_of_reads_comparators_over_projected_variables() {
        let q = |s: &str| Order::of(&format!("SELECT ?s ?o WHERE {{ ?s ?p ?o }} {s}")).unwrap();
        assert_eq!(
            q(""),
            Order {
                keys: Some(Vec::new()),
                ..Order::default()
            }
        );
        let order = q("ORDER BY DESC(?o) ?s LIMIT 2");
        assert!(order.ordered && order.sliced);
        assert_eq!(
            order.keys,
            Some(vec![("o".to_owned(), true), ("s".to_owned(), false)])
        );
        assert_eq!(q("ORDER BY ?p").keys, None);
        assert_eq!(q("ORDER BY STR(?s)").keys, None);
        assert!(q("OFFSET 1").sliced);
    }
}
