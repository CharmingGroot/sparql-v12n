// SPDX-License-Identifier: Apache-2.0
//! RDF terms produced by term maps of a branch, and SQL conditions for two
//! of them being the same term.

use oxrdf::Term;

use crate::mapping::template::{IriKind, iri_safe, is_iunreserved};
use crate::mapping::{
    BaseIri, Template, TemplatePart, TermMap, TermMapValue, TermType, has_scheme,
};
use crate::sql::{Condition, Param, SqlExpr, TermDatatype, TermSql, TermSqlValue};

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

/// A term map bound to the table alias it reads from, or a constant.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TermExpr<'a> {
    pub value: TermValue<'a>,
    pub term_type: TermType,
    pub datatype: Option<&'a str>,
    pub language: Option<&'a str>,
    /// Set when the term is an IRI whose strings may be relative, which
    /// resolve against this base (R2RML §11.2): a column, or a template
    /// whose strings have a scheme for some values only. Templates whose
    /// strings never have one already start with the base
    /// ([`Mapping::with_base_iri`](crate::mapping::Mapping::with_base_iri)).
    pub base_iri: Option<&'a str>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum TermValue<'a> {
    Constant(&'a Term),
    Column {
        table: usize,
        name: &'a str,
    },
    Template {
        table: usize,
        template: &'a Template,
    },
}

impl<'a> TermExpr<'a> {
    pub fn from_term_map(term_map: &'a TermMap, table: usize, base: Option<&'a BaseIri>) -> Self {
        let value = match &term_map.value {
            TermMapValue::Constant(term) => TermValue::Constant(term),
            TermMapValue::Column(name) => TermValue::Column { table, name },
            TermMapValue::Template(template) => TermValue::Template { table, template },
        };
        let resolved_per_value = match value {
            TermValue::Constant(_) => false,
            TermValue::Column { .. } => true,
            TermValue::Template { template, .. } => template.iri_kind() == IriKind::Mixed,
        };
        Self {
            value,
            term_type: term_map.term_type,
            datatype: term_map.datatype.as_deref(),
            language: term_map.language.as_deref(),
            base_iri: base
                .filter(|_| term_map.term_type == TermType::Iri && resolved_per_value)
                .map(BaseIri::as_str),
        }
    }

    pub fn constant(term: &'a Term) -> Self {
        let term_type = match term {
            Term::NamedNode(_) => TermType::Iri,
            Term::BlankNode(_) => TermType::BlankNode,
            _ => TermType::Literal,
        };
        Self {
            value: TermValue::Constant(term),
            term_type,
            datatype: None,
            language: None,
            base_iri: None,
        }
    }

    /// Columns the term is built from, as `(table, column)`.
    pub fn columns(&self) -> Vec<(usize, &'a str)> {
        match self.value {
            TermValue::Constant(_) => Vec::new(),
            TermValue::Column { table, name } => vec![(table, name)],
            TermValue::Template { table, template } => {
                template.columns().map(|c| (table, c)).collect()
            }
        }
    }

    /// The term as one side of a [`Condition::SameTerm`].
    pub fn to_term_sql(self) -> TermSql {
        let (value, datatype) = match self.value {
            TermValue::Constant(term) => (TermSqlValue::Param(lexical_form(term).into()), None),
            TermValue::Column { table, name } => (
                TermSqlValue::Column {
                    table,
                    name: name.to_owned(),
                },
                Some(TermDatatype::Natural),
            ),
            TermValue::Template { table, template } => (
                TermSqlValue::Template {
                    table,
                    parts: template.parts().to_vec(),
                    iri_safe: self.term_type == TermType::Iri,
                },
                // R2RML §7.4: a template literal is a plain string.
                Some(TermDatatype::Iri(XSD_STRING.to_owned())),
            ),
        };
        let datatype = match (self.term_type, self.language, self.datatype) {
            (TermType::Literal, None, Some(iri)) => Some(TermDatatype::Iri(iri.to_owned())),
            (TermType::Literal, None, None) => datatype,
            _ => None,
        };
        TermSql {
            value,
            datatype,
            base_iri: self.base_iri.map(str::to_owned),
        }
    }
}

/// Template columns compared pairwise: lexical forms only, the datatype
/// of a template is fixed.
fn template_column(table: usize, name: &str) -> TermSql {
    TermSql {
        value: TermSqlValue::Column {
            table,
            name: name.to_owned(),
        },
        datatype: None,
        base_iri: None,
    }
}

/// The column a non-template term reads; a constant here is compared with
/// another constant, which `term_eq` decides without SQL.
fn column_or_constant(term: &TermExpr<'_>) -> SqlExpr {
    match term.value {
        TermValue::Column { table, name } => column(table, name),
        TermValue::Constant(term) => SqlExpr::Param(lexical_form(term).into()),
        TermValue::Template { .. } => unreachable!("templates are matched by decompose_constant"),
    }
}

fn column(table: usize, name: &str) -> SqlExpr {
    SqlExpr::Column {
        table,
        name: name.to_owned(),
    }
}

fn lexical_form(term: &Term) -> &str {
    match term {
        Term::NamedNode(n) => n.as_str(),
        Term::BlankNode(b) => b.as_str(),
        Term::Literal(l) => l.value(),
        // Quoted triples exist only with oxrdf's `rdf-12` feature.
        #[allow(unreachable_patterns)]
        _ => "",
    }
}

/// When two terms are the same RDF term.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Equality {
    /// Never, whatever the rows hold.
    Never,
    /// When all conditions hold; no conditions means always.
    When(Vec<Condition>),
}

/// SQL conditions for `left` and `right` being the same RDF term. `left`
/// goes on the left of each condition.
pub(crate) fn term_eq(left: &TermExpr<'_>, right: &TermExpr<'_>) -> Equality {
    if left.term_type != right.term_type || !literal_types_compatible(left, right) {
        return Equality::Never;
    }
    let iri = left.term_type == TermType::Iri;
    match (left.value, right.value) {
        (TermValue::Constant(a), TermValue::Constant(b)) => {
            if a == b {
                Equality::When(Vec::new())
            } else {
                Equality::Never
            }
        }
        (TermValue::Constant(constant), _) => constant_eq(constant, right, iri),
        (_, TermValue::Constant(constant)) => constant_eq(constant, left, iri),
        (
            TermValue::Template {
                table: lt,
                template: l,
            },
            TermValue::Template {
                table: rt,
                template: r,
            },
        ) if left.base_iri.is_none() && right.base_iri.is_none() => {
            match decompose_template(l, r, left.term_type) {
                Decomposition::Columns(pairs) => Equality::When(
                    pairs
                        .into_iter()
                        .map(|(lc, rc)| {
                            Condition::SameTerm(template_column(lt, lc), template_column(rt, rc))
                        })
                        .collect(),
                ),
                Decomposition::Disjoint => Equality::Never,
                Decomposition::Unknown => string_eq(left, right),
            }
        }
        _ => string_eq(left, right),
    }
}

/// Fallback that compares the rendered strings, IRI-safe encoding
/// included (R2RML §7.3).
fn string_eq(left: &TermExpr<'_>, right: &TermExpr<'_>) -> Equality {
    Equality::When(vec![Condition::SameTerm(
        left.to_term_sql(),
        right.to_term_sql(),
    )])
}

fn constant_eq(constant: &Term, other: &TermExpr<'_>, iri: bool) -> Equality {
    let value = lexical_form(constant);
    match other.value {
        // Values with and without a scheme render the constant through
        // different template text, so compare the generated strings.
        TermValue::Template { .. } if other.base_iri.is_some() => {
            Equality::When(vec![Condition::SameTerm(
                other.to_term_sql(),
                TermExpr::constant(constant).to_term_sql(),
            )])
        }
        TermValue::Template { table, template } => match decompose_constant(value, template, iri) {
            ConstantMatch::Values(values) => Equality::When(
                template
                    .columns()
                    .zip(values)
                    .map(|(name, v)| Condition::Eq(column(table, name), SqlExpr::Param(v.into())))
                    .collect(),
            ),
            ConstantMatch::NoMatch => Equality::Never,
            ConstantMatch::Unknown => Equality::When(vec![Condition::SameTerm(
                other.to_term_sql(),
                TermExpr::constant(constant).to_term_sql(),
            )]),
        },
        TermValue::Column { .. } if other.base_iri.is_some() => {
            let base = other.base_iri.expect("checked by the guard");
            // R2RML §11.2: the column value is the IRI itself, or, when the
            // IRI starts with the base, the relative rest, which resolves to
            // it unless it has a scheme of its own. Two parameters keep the
            // comparison on the column, and so its index.
            let column = column_or_constant(other);
            let iri = SqlExpr::Param(value.into());
            Equality::When(vec![
                match value.strip_prefix(base).filter(|rest| !has_scheme(rest)) {
                    Some(rest) => Condition::EqAny(column, vec![iri, SqlExpr::Param(rest.into())]),
                    None => Condition::Eq(column, iri),
                },
            ])
        }
        _ => {
            let datatype = match (constant, literal_type(other)) {
                (Term::Literal(literal), LiteralType::Natural) => {
                    Some(literal.datatype().as_str().to_owned())
                }
                _ => None,
            };
            Equality::When(vec![Condition::Eq(
                column_or_constant(other),
                SqlExpr::Param(Param {
                    value: value.to_owned(),
                    datatype,
                }),
            )])
        }
    }
}

/// What a literal term's datatype is known to be before reading any row.
enum LiteralType<'a> {
    Language(&'a str),
    Datatype(&'a str),
    /// A column without `rr:datatype` takes its datatype from the SQL
    /// column type (R2RML §10.2), which is not known without the database.
    Natural,
}

fn literal_type<'a>(term: &TermExpr<'a>) -> LiteralType<'a> {
    if let TermValue::Constant(Term::Literal(literal)) = term.value {
        return match literal.language() {
            Some(language) => LiteralType::Language(language),
            None => LiteralType::Datatype(literal.datatype().as_str()),
        };
    }
    match (term.language, term.datatype, term.value) {
        (Some(language), _, _) => LiteralType::Language(language),
        (None, Some(datatype), _) => LiteralType::Datatype(datatype),
        // R2RML §7.4: a template-valued literal without datatype or language is a plain string.
        (None, None, TermValue::Template { .. }) => LiteralType::Datatype(XSD_STRING),
        _ => LiteralType::Natural,
    }
}

/// Literals with different datatypes or language tags are different terms.
/// Non-literals are always compatible here; their term type is compared
/// separately.
fn literal_types_compatible(left: &TermExpr<'_>, right: &TermExpr<'_>) -> bool {
    if left.term_type != TermType::Literal {
        return true;
    }
    match (literal_type(left), literal_type(right)) {
        (LiteralType::Language(a), LiteralType::Language(b)) => a.eq_ignore_ascii_case(b),
        (LiteralType::Language(_), _) | (_, LiteralType::Language(_)) => false,
        (LiteralType::Datatype(a), LiteralType::Datatype(b)) => a == b,
        _ => true,
    }
}

/// How two templates relate when they are compared as strings.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Decomposition<'a> {
    /// The strings are equal exactly when these column pairs are equal.
    Columns(Vec<(&'a str, &'a str)>),
    /// The strings can never be equal: their fixed prefixes or suffixes differ.
    Disjoint,
    /// Neither holds; compare the rendered strings.
    Unknown,
}

/// Template decomposition: two templates of the same shape (same literal
/// parts, same number of column slots) compare column by column, so the
/// database can use indexes instead of comparing concatenated strings.
///
/// This is only sound when the template is injective, i.e. distinct column
/// values always render distinct strings. A single column always is. With
/// several columns, `{a}-{b}` is not (`"x-y","z"` and `"x","y-z"`), but an
/// IRI template whose separators contain a character that IRI-safe
/// encoding always escapes (R2RML §7.3), such as `/`, is: the separator can
/// then be found unambiguously.
pub(crate) fn decompose_template<'a>(
    left: &'a Template,
    right: &'a Template,
    term_type: TermType,
) -> Decomposition<'a> {
    let (lp, rp) = (fixed_prefix(left), fixed_prefix(right));
    let (ls, rs) = (fixed_suffix(left), fixed_suffix(right));
    if !(lp.starts_with(rp) || rp.starts_with(lp)) || !(ls.ends_with(rs) || rs.ends_with(ls)) {
        return Decomposition::Disjoint;
    }
    let same_shape = left.parts().len() == right.parts().len()
        && left
            .parts()
            .iter()
            .zip(right.parts())
            .all(|pair| match pair {
                (TemplatePart::Literal(a), TemplatePart::Literal(b)) => a == b,
                (TemplatePart::Column(_), TemplatePart::Column(_)) => true,
                _ => false,
            });
    if same_shape && is_injective(left, term_type) {
        Decomposition::Columns(left.columns().zip(right.columns()).collect())
    } else {
        Decomposition::Unknown
    }
}

/// Whether distinct column values always render distinct strings.
pub(crate) fn is_injective(template: &Template, term_type: TermType) -> bool {
    let parts = template.parts();
    if template.columns().count() <= 1 {
        return true;
    }
    if term_type != TermType::Iri {
        return false;
    }
    parts.windows(3).all(|w| match w {
        [
            TemplatePart::Column(_),
            TemplatePart::Literal(sep),
            TemplatePart::Column(_),
        ] => is_safe_separator(sep),
        _ => true,
    }) && !parts
        .windows(2)
        .any(|w| matches!(w, [TemplatePart::Column(_), TemplatePart::Column(_)]))
}

/// The literal text every rendered string starts with.
fn fixed_prefix(template: &Template) -> &str {
    match template.parts() {
        [TemplatePart::Literal(text), ..] => text,
        _ => "",
    }
}

/// The literal text every rendered string ends with.
fn fixed_suffix(template: &Template) -> &str {
    match template.parts() {
        [.., TemplatePart::Literal(text)] => text,
        _ => "",
    }
}

/// A separator that never occurs inside an IRI-safe encoded value: it
/// contains a character that is neither `iunreserved` (kept as is) nor `%`
/// (used by the encoding).
fn is_safe_separator(separator: &str) -> bool {
    separator.chars().any(|c| !is_iunreserved(c) && c != '%')
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = value.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ConstantMatch {
    /// The template renders the constant exactly for these column values.
    Values(Vec<String>),
    /// The template can never render the constant.
    NoMatch,
    /// The column values cannot be read off unambiguously.
    Unknown,
}

/// Reads the column values off a constant the template would have to
/// render, e.g. `7` for `http://example.org/customer/7` against
/// `http://example.org/customer/{id}`.
pub(crate) fn decompose_constant(value: &str, template: &Template, iri: bool) -> ConstantMatch {
    let parts = template.parts();
    let mut rest = value;
    let mut values = Vec::new();
    let mut i = 0;
    while i < parts.len() {
        let segment = match &parts[i] {
            TemplatePart::Literal(text) => {
                let Some(after) = rest.strip_prefix(text.as_str()) else {
                    return ConstantMatch::NoMatch;
                };
                rest = after;
                i += 1;
                continue;
            }
            TemplatePart::Column(_) => {
                let last_column = !parts[i + 1..]
                    .iter()
                    .any(|p| matches!(p, TemplatePart::Column(_)));
                if last_column {
                    let suffix: String = parts[i + 1..]
                        .iter()
                        .filter_map(|p| match p {
                            TemplatePart::Literal(text) => Some(text.as_str()),
                            TemplatePart::Column(_) => None,
                        })
                        .collect();
                    let Some(segment) = rest.strip_suffix(suffix.as_str()) else {
                        return ConstantMatch::NoMatch;
                    };
                    rest = "";
                    i = parts.len();
                    segment
                } else {
                    let TemplatePart::Literal(separator) = &parts[i + 1] else {
                        return ConstantMatch::Unknown;
                    };
                    if !(iri && is_safe_separator(separator)) {
                        return ConstantMatch::Unknown;
                    }
                    let Some(at) = rest.find(separator.as_str()) else {
                        return ConstantMatch::NoMatch;
                    };
                    let segment = &rest[..at];
                    rest = &rest[at + separator.len()..];
                    i += 2;
                    segment
                }
            }
        };
        if iri {
            // Only the canonical encoding of a value can come out of the template.
            match percent_decode(segment) {
                Some(decoded) if iri_safe(&decoded) == segment => values.push(decoded),
                _ => return ConstantMatch::NoMatch,
            }
        } else {
            values.push(segment.to_owned());
        }
    }
    if rest.is_empty() {
        ConstantMatch::Values(values)
    } else {
        ConstantMatch::NoMatch
    }
}

#[cfg(test)]
mod tests {
    use oxrdf::{Literal, NamedNode};

    use super::*;

    fn template(source: &str) -> Template {
        Template::parse(source).unwrap()
    }

    fn eq(table_l: usize, l: &str, table_r: usize, r: &str) -> Condition {
        Condition::SameTerm(template_column(table_l, l), template_column(table_r, r))
    }

    #[test]
    fn decompose_template_same_shape_compares_columns() {
        let (a, b) = (
            template("http://example.org/customer/{customer_id}"),
            template("http://example.org/customer/{id}"),
        );
        assert_eq!(
            decompose_template(&a, &b, TermType::Iri),
            Decomposition::Columns(vec![("customer_id", "id")])
        );
    }

    #[test]
    fn decompose_template_safe_separator_compares_each_column() {
        let (a, b) = (template("http://ex/{a}/{b}"), template("http://ex/{c}/{d}"));
        assert_eq!(
            decompose_template(&a, &b, TermType::Iri),
            Decomposition::Columns(vec![("a", "c"), ("b", "d")])
        );
    }

    #[test]
    fn decompose_template_ambiguous_separator_falls_back() {
        let (a, b) = (template("http://ex/{a}-{b}"), template("http://ex/{c}-{d}"));
        assert_eq!(
            decompose_template(&a, &b, TermType::Iri),
            Decomposition::Unknown
        );
        let (a, b) = (template("{a}/{b}"), template("{c}/{d}"));
        assert_eq!(
            decompose_template(&a, &b, TermType::Literal),
            Decomposition::Unknown
        );
        let (a, b) = (template("http://ex/{a}{b}"), template("http://ex/{c}{d}"));
        assert_eq!(
            decompose_template(&a, &b, TermType::Iri),
            Decomposition::Unknown
        );
    }

    #[test]
    fn decompose_template_different_fixed_text_is_disjoint() {
        let (a, b) = (
            template("http://ex/order/{id}"),
            template("http://ex/customer/{id}"),
        );
        assert_eq!(
            decompose_template(&a, &b, TermType::Iri),
            Decomposition::Disjoint
        );
        let (a, b) = (
            template("http://ex/{id}.json"),
            template("http://ex/{id}.xml"),
        );
        assert_eq!(
            decompose_template(&a, &b, TermType::Iri),
            Decomposition::Disjoint
        );
    }

    #[test]
    fn decompose_template_overlapping_prefixes_fall_back() {
        let (a, b) = (template("http://ex/c{id}"), template("http://ex/{id}"));
        assert_eq!(
            decompose_template(&a, &b, TermType::Iri),
            Decomposition::Unknown
        );
    }

    #[test]
    fn decompose_constant_reads_column_values() {
        let t = template("http://example.org/customer/{id}");
        assert_eq!(
            decompose_constant("http://example.org/customer/7", &t, true),
            ConstantMatch::Values(vec!["7".into()])
        );
        assert_eq!(
            decompose_constant("http://example.org/order/7", &t, true),
            ConstantMatch::NoMatch
        );
        let t = template("http://ex/{a}/{b}.json");
        assert_eq!(
            decompose_constant("http://ex/x%20y/z.json", &t, true),
            ConstantMatch::Values(vec!["x y".into(), "z".into()])
        );
    }

    #[test]
    fn decompose_constant_non_canonical_encoding_never_matches() {
        let t = template("http://ex/{id}");
        // IRI-safe encoding would have turned `/` into `%2F`.
        assert_eq!(
            decompose_constant("http://ex/a/b", &t, true),
            ConstantMatch::NoMatch
        );
        assert_eq!(
            decompose_constant("http://ex/a%2fb", &t, true),
            ConstantMatch::NoMatch
        );
    }

    #[test]
    fn decompose_constant_ambiguous_template_is_unknown() {
        let t = template("http://ex/{a}-{b}");
        assert_eq!(
            decompose_constant("http://ex/1-2", &t, true),
            ConstantMatch::Unknown
        );
    }

    fn column_map(name: &str, term_type: TermType) -> TermMap {
        TermMap {
            value: TermMapValue::Column(name.to_owned()),
            term_type,
            datatype: None,
            language: None,
        }
    }

    #[test]
    fn term_eq_different_term_types_never_match() {
        let iri = column_map("a", TermType::Iri);
        let literal = column_map("b", TermType::Literal);
        assert_eq!(
            term_eq(
                &TermExpr::from_term_map(&iri, 0, None),
                &TermExpr::from_term_map(&literal, 1, None)
            ),
            Equality::Never
        );
    }

    #[test]
    fn term_eq_literal_datatypes_must_agree() {
        let mut typed = column_map("amount", TermType::Literal);
        typed.datatype = Some("http://www.w3.org/2001/XMLSchema#integer".into());
        let plain: Term = Literal::new_simple_literal("5").into();
        let tagged: Term = Literal::new_language_tagged_literal_unchecked("5", "en").into();
        let typed = TermExpr::from_term_map(&typed, 0, None);
        assert_eq!(
            term_eq(&typed, &TermExpr::constant(&plain)),
            Equality::Never
        );
        assert_eq!(
            term_eq(&typed, &TermExpr::constant(&tagged)),
            Equality::Never
        );

        // The natural datatype depends on the SQL type, so the constant's
        // datatype travels with the parameter for the database adapter.
        let natural = column_map("amount", TermType::Literal);
        assert_eq!(
            term_eq(
                &TermExpr::from_term_map(&natural, 0, None),
                &TermExpr::constant(&plain)
            ),
            Equality::When(vec![Condition::Eq(
                column(0, "amount"),
                SqlExpr::Param(Param {
                    value: "5".into(),
                    datatype: Some(XSD_STRING.into()),
                })
            )])
        );
    }

    #[test]
    fn term_eq_column_and_template_compare_strings() {
        let iri_column = column_map("home", TermType::Iri);
        let t = TermMap {
            value: TermMapValue::Template(template("http://ex/{id}")),
            term_type: TermType::Iri,
            datatype: None,
            language: None,
        };
        assert_eq!(
            term_eq(
                &TermExpr::from_term_map(&iri_column, 1, None),
                &TermExpr::from_term_map(&t, 0, None)
            ),
            Equality::When(vec![Condition::SameTerm(
                TermSql {
                    value: TermSqlValue::Column {
                        table: 1,
                        name: "home".into()
                    },
                    datatype: None,
                    base_iri: None,
                },
                TermSql {
                    value: TermSqlValue::Template {
                        table: 0,
                        parts: template("http://ex/{id}").parts().to_vec(),
                        iri_safe: true,
                    },
                    datatype: None,
                    base_iri: None,
                }
            )])
        );
    }

    #[test]
    fn term_eq_literal_columns_carry_their_datatypes() {
        let natural = column_map("a", TermType::Literal);
        let mut typed = column_map("b", TermType::Literal);
        typed.datatype = Some("http://www.w3.org/2001/XMLSchema#integer".into());
        let literal_template = TermMap {
            value: TermMapValue::Template(template("{x}{y}")),
            term_type: TermType::Literal,
            datatype: None,
            language: None,
        };
        let datatypes = |l: &TermMap, r: &TermMap| match term_eq(
            &TermExpr::from_term_map(l, 0, None),
            &TermExpr::from_term_map(r, 1, None),
        ) {
            Equality::When(c) => match &c[..] {
                [Condition::SameTerm(l, r)] => (l.datatype.clone(), r.datatype.clone()),
                other => panic!("{other:?}"),
            },
            Equality::Never => panic!("never"),
        };
        let integer = TermDatatype::Iri("http://www.w3.org/2001/XMLSchema#integer".into());
        assert_eq!(
            datatypes(&natural, &natural),
            (Some(TermDatatype::Natural), Some(TermDatatype::Natural))
        );
        assert_eq!(
            datatypes(&natural, &typed),
            (Some(TermDatatype::Natural), Some(integer))
        );
        assert_eq!(
            datatypes(&natural, &literal_template),
            (
                Some(TermDatatype::Natural),
                Some(TermDatatype::Iri(XSD_STRING.into()))
            )
        );
    }

    #[test]
    fn term_eq_same_shape_templates_compare_columns() {
        let a = TermMap {
            value: TermMapValue::Template(template("http://ex/{x}")),
            term_type: TermType::Iri,
            datatype: None,
            language: None,
        };
        let b = TermMap {
            value: TermMapValue::Template(template("http://ex/{y}")),
            ..a.clone()
        };
        assert_eq!(
            term_eq(
                &TermExpr::from_term_map(&a, 1, None),
                &TermExpr::from_term_map(&b, 0, None)
            ),
            Equality::When(vec![eq(1, "x", 0, "y")])
        );
        let constant: Term = NamedNode::new_unchecked("http://ex/9").into();
        assert_eq!(
            term_eq(
                &TermExpr::constant(&constant),
                &TermExpr::from_term_map(&a, 0, None)
            ),
            Equality::When(vec![Condition::Eq(
                column(0, "x"),
                SqlExpr::Param("9".into())
            )])
        );
    }
}
