// SPDX-License-Identifier: Apache-2.0
//! Compares two query results as bags of solutions.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use oxrdf::Term;
use spargebra::algebra::GraphPattern;
use spargebra::{Query, SparqlParser};

/// One solution: the bound variables (without `?`) and their terms.
/// Unbound variables are absent, so a solution with `?x` unbound differs
/// from every solution that binds `?x`.
pub type Row = BTreeMap<String, Term>;

/// The variables and solutions of one result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Results {
    pub variables: Vec<String>,
    pub rows: Vec<Row>,
}

/// How a result differs from the expected one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Diff {
    /// Expected and actual variables, when they differ as sets.
    pub variables: Option<(Vec<String>, Vec<String>)>,
    /// Solutions in the expected result only, once per missing copy.
    pub missing: Vec<Row>,
    /// Solutions in the actual result only, once per extra copy.
    pub extra: Vec<Row>,
    /// Both solution sequences, when the bags are equal but `ORDER BY`
    /// puts them in a different order.
    pub order: Option<(Vec<Row>, Vec<Row>)>,
}

/// Whether the query has a top-level `ORDER BY`, which makes the order of
/// solutions part of the answer (SPARQL 1.1 §15.1).
pub fn has_order_by(query: &str) -> Result<bool, String> {
    let query = SparqlParser::new()
        .parse_query(query)
        .map_err(|e| e.to_string())?;
    let Query::Select { pattern, .. } = query else {
        return Ok(false);
    };
    let mut pattern = &pattern;
    loop {
        pattern = match pattern {
            GraphPattern::OrderBy { .. } => return Ok(true),
            GraphPattern::Project { inner, .. }
            | GraphPattern::Distinct { inner }
            | GraphPattern::Reduced { inner }
            | GraphPattern::Slice { inner, .. } => inner,
            _ => return Ok(false),
        };
    }
}

/// `None` when `actual` has the same variables and the same bag of
/// solutions as `expected`, and, if `ordered`, the same sequence.
///
/// Terms are compared as RDF terms: same kind, lexical form, datatype and
/// language tag. Values are never normalized, so `"4"^^xsd:integer` and
/// `"04"^^xsd:integer` differ. Blank nodes are an error, since their
/// labels are local to each result and comparing them needs graph
/// isomorphism, which no case needs yet.
pub fn compare(
    expected: &Results,
    actual: &Results,
    ordered: bool,
) -> Result<Option<Diff>, String> {
    for (who, results) in [("expected", expected), ("actual", actual)] {
        if results
            .rows
            .iter()
            .flat_map(Row::values)
            .any(Term::is_blank_node)
        {
            return Err(format!(
                "the {who} result has blank nodes, which are not compared"
            ));
        }
    }
    let mut diff = Diff::default();

    let set = |v: &[String]| v.iter().cloned().collect::<BTreeSet<_>>();
    if set(&expected.variables) != set(&actual.variables) {
        diff.variables = Some((expected.variables.clone(), actual.variables.clone()));
    }

    let mut counts: BTreeMap<String, (i64, &Row)> = BTreeMap::new();
    for row in &expected.rows {
        counts.entry(key(row)).or_insert((0, row)).0 += 1;
    }
    for row in &actual.rows {
        counts.entry(key(row)).or_insert((0, row)).0 -= 1;
    }
    for (count, row) in counts.into_values() {
        let copies = usize::try_from(count.unsigned_abs()).unwrap_or(usize::MAX);
        let side = if count > 0 {
            &mut diff.missing
        } else {
            &mut diff.extra
        };
        side.extend(std::iter::repeat_n(row.clone(), copies));
    }

    if ordered && diff.missing.is_empty() && diff.extra.is_empty() {
        let keys = |rows: &[Row]| rows.iter().map(key).collect::<Vec<_>>();
        if keys(&expected.rows) != keys(&actual.rows) {
            diff.order = Some((expected.rows.clone(), actual.rows.clone()));
        }
    }

    Ok((diff != Diff::default()).then_some(diff))
}

/// A row in N-Triples term syntax, which writes each RDF term one way.
fn key(row: &Row) -> String {
    row.iter()
        .map(|(variable, term)| format!("?{variable}={term}"))
        .collect::<Vec<_>>()
        .join("  ")
}

/// The lexical form of a literal or the string of an IRI.
fn lexical(term: &Term) -> &str {
    match term {
        Term::NamedNode(n) => n.as_str(),
        Term::BlankNode(b) => b.as_str(),
        Term::Literal(l) => l.value(),
        #[allow(unreachable_patterns)]
        _ => "",
    }
}

/// A missing and an extra solution that bind the same variables and share
/// a term or a lexical form are most likely the same solution with a term
/// built differently, so they are shown side by side.
fn similar(a: &Row, b: &Row) -> bool {
    a.keys().eq(b.keys())
        && a.iter()
            .zip(b.values())
            .any(|((_, x), y)| x == y || lexical(x) == lexical(y))
}

impl fmt::Display for Diff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let vars = |v: &[String]| {
            v.iter()
                .map(|v| format!("?{v}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        if let Some((expected, actual)) = &self.variables {
            writeln!(
                f,
                "  variables differ: Ontop [{}], v12n [{}]",
                vars(expected),
                vars(actual)
            )?;
        }
        if !self.missing.is_empty() || !self.extra.is_empty() {
            writeln!(
                f,
                "  {} solution(s) only in Ontop, {} only in v12n",
                self.missing.len(),
                self.extra.len()
            )?;
        }

        let mut extra: Vec<Option<&Row>> = self.extra.iter().map(Some).collect();
        let mut missing = Vec::new();
        let mut pairs = Vec::new();
        for row in &self.missing {
            match extra
                .iter_mut()
                .find(|e| e.is_some_and(|e| similar(row, e)))
            {
                Some(slot) => pairs.push((row, slot.take().expect("matched a present row"))),
                None => missing.push(row),
            }
        }
        if !pairs.is_empty() {
            writeln!(f, "  terms differ:")?;
            for (ontop, v12n) in pairs {
                let same: Vec<String> = ontop
                    .iter()
                    .filter(|(k, v)| v12n.get(*k) == Some(*v))
                    .map(|(k, v)| format!("?{k}={v}"))
                    .collect();
                writeln!(
                    f,
                    "    where {}",
                    if same.is_empty() {
                        "-".to_owned()
                    } else {
                        same.join("  ")
                    }
                )?;
                for (variable, term) in ontop.iter().filter(|(k, v)| v12n.get(*k) != Some(*v)) {
                    writeln!(f, "      ?{variable}  Ontop {term}")?;
                    writeln!(
                        f,
                        "      {:width$}  v12n  {}",
                        "",
                        v12n[variable],
                        width = variable.len() + 1
                    )?;
                }
            }
        }
        let rows = |f: &mut fmt::Formatter<'_>, title: &str, rows: &[&Row]| {
            if rows.is_empty() {
                return Ok(());
            }
            writeln!(f, "  {title}:")?;
            rows.iter()
                .try_for_each(|row| writeln!(f, "    {}", key(row)))
        };
        rows(f, "only in Ontop", &missing)?;
        rows(
            f,
            "only in v12n",
            &extra.into_iter().flatten().collect::<Vec<_>>(),
        )?;

        if let Some((expected, actual)) = &self.order {
            writeln!(
                f,
                "  same solutions in a different order (the query has ORDER BY):"
            )?;
            for (i, (e, a)) in expected.iter().zip(actual).enumerate() {
                let mark = if key(e) == key(a) { ' ' } else { '*' };
                writeln!(f, "   {mark}{i:>3} Ontop {}", key(e))?;
                writeln!(f, "        v12n  {}", key(a))?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use oxrdf::vocab::xsd;
    use oxrdf::{Literal, NamedNode};

    use super::*;

    fn iri(s: &str) -> Term {
        NamedNode::new(format!("http://example.org/{s}"))
            .unwrap()
            .into()
    }

    fn int(s: &str) -> Term {
        Literal::new_typed_literal(s, xsd::INTEGER).into()
    }

    fn row(pairs: &[(&str, Term)]) -> Row {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.clone()))
            .collect()
    }

    fn results(variables: &[&str], rows: Vec<Row>) -> Results {
        Results {
            variables: variables.iter().map(|v| (*v).to_owned()).collect(),
            rows,
        }
    }

    #[test]
    fn compare_ignores_order_without_order_by() {
        let a = row(&[("s", iri("a"))]);
        let b = row(&[("s", iri("b"))]);
        let expected = results(&["s"], vec![a.clone(), b.clone()]);
        let actual = results(&["s"], vec![b, a]);
        assert_eq!(compare(&expected, &actual, false).unwrap(), None);
    }

    #[test]
    fn compare_counts_duplicate_solutions() {
        let a = row(&[("s", iri("a"))]);
        let expected = results(&["s"], vec![a.clone()]);
        let actual = results(&["s"], vec![a.clone(), a.clone()]);
        let diff = compare(&expected, &actual, false).unwrap().unwrap();
        assert_eq!((diff.missing, diff.extra), (vec![], vec![a]));
    }

    #[test]
    fn compare_tells_unbound_from_bound() {
        let expected = results(&["s", "o"], vec![row(&[("s", iri("a"))])]);
        let actual = results(&["s", "o"], vec![row(&[("s", iri("a")), ("o", int("1"))])]);
        assert!(compare(&expected, &actual, false).unwrap().is_some());
    }

    #[test]
    fn compare_checks_lexical_form_and_datatype_exactly() {
        let expected = results(&["o"], vec![row(&[("o", int("4"))])]);
        for other in [
            int("04"),
            Literal::new_simple_literal("4").into(),
            Literal::new_typed_literal("4", xsd::DECIMAL).into(),
        ] {
            let actual = results(&["o"], vec![row(&[("o", other.clone())])]);
            assert!(
                compare(&expected, &actual, false).unwrap().is_some(),
                "{other}"
            );
        }
        // RDF 1.1: a simple literal is an xsd:string literal.
        let plain = results(
            &["o"],
            vec![row(&[("o", Literal::new_simple_literal("x").into())])],
        );
        let string = results(
            &["o"],
            vec![row(&[(
                "o",
                Literal::new_typed_literal("x", xsd::STRING).into(),
            )])],
        );
        assert_eq!(compare(&plain, &string, false).unwrap(), None);
    }

    #[test]
    fn compare_checks_variables_as_a_set() {
        let expected = results(&["a", "b"], vec![]);
        assert_eq!(
            compare(&expected, &results(&["b", "a"], vec![]), false).unwrap(),
            None
        );
        let diff = compare(&expected, &results(&["a"], vec![]), false)
            .unwrap()
            .unwrap();
        assert!(diff.variables.is_some());
    }

    #[test]
    fn compare_checks_the_sequence_with_order_by() {
        let a = row(&[("s", iri("a"))]);
        let b = row(&[("s", iri("b"))]);
        let expected = results(&["s"], vec![a.clone(), b.clone()]);
        let actual = results(&["s"], vec![b, a]);
        assert_eq!(compare(&expected, &expected, true).unwrap(), None);
        let diff = compare(&expected, &actual, true).unwrap().unwrap();
        assert!(diff.order.is_some() && diff.missing.is_empty());
    }

    #[test]
    fn compare_refuses_blank_nodes() {
        let blank = results(
            &["s"],
            vec![row(&[("s", oxrdf::BlankNode::default().into())])],
        );
        assert!(compare(&blank, &blank, false).is_err());
    }

    #[test]
    fn diff_shows_a_changed_term_next_to_the_expected_one() {
        let expected = results(
            &["f", "s"],
            vec![
                row(&[("f", iri("1")), ("s", int("4"))]),
                row(&[("f", iri("2")), ("s", int("5"))]),
            ],
        );
        let actual = results(
            &["f", "s"],
            vec![
                row(&[
                    ("f", iri("1")),
                    ("s", Literal::new_simple_literal("4").into()),
                ]),
                row(&[("f", iri("3")), ("s", int("6"))]),
            ],
        );
        let text = compare(&expected, &actual, false)
            .unwrap()
            .unwrap()
            .to_string();
        let int = "<http://www.w3.org/2001/XMLSchema#integer>";
        assert_eq!(
            text,
            format!(
                "  2 solution(s) only in Ontop, 2 only in v12n\n\
                 \x20 terms differ:\n\
                 \x20   where ?f=<http://example.org/1>\n\
                 \x20     ?s  Ontop \"4\"^^{int}\n\
                 \x20         v12n  \"4\"\n\
                 \x20 only in Ontop:\n\
                 \x20   ?f=<http://example.org/2>  ?s=\"5\"^^{int}\n\
                 \x20 only in v12n:\n\
                 \x20   ?f=<http://example.org/3>  ?s=\"6\"^^{int}\n"
            )
        );
    }

    #[test]
    fn has_order_by_looks_through_projection_and_slices() {
        let q = |s: &str| has_order_by(&format!("SELECT ?s WHERE {{ ?s ?p ?o }} {s}")).unwrap();
        assert!(!q(""));
        assert!(q("ORDER BY ?s"));
        assert!(q("ORDER BY ?s LIMIT 2"));
        assert!(has_order_by("SELECT DISTINCT ?s WHERE { ?s ?p ?o } ORDER BY ?s").unwrap());
    }
}
