// SPDX-License-Identifier: Apache-2.0
//! `FILTER` expressions over the terms a branch binds.

use super::{Branch, Env, Scope, Source};
use oxrdf::Term;

use crate::Error;
use crate::sparql::Expression;
use crate::sql::{Condition, Filter, FilterTerm, FilterValue, OrderValue, TermMatch};
use crate::xpath_regex::{Untranslated, translate};

use super::term::{Equality, TermExpr, TermValue, term_eq};

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

/// A way a variable is bound: in the rows where the guard holds, or in
/// every row without one.
type Bound<'a> = (Option<Condition>, TermExpr<'a>);

/// An operand in the rows where its guard holds.
struct Leaf<'a> {
    value: FilterValue,
    /// The term itself, for a variable or a constant, which RDF term
    /// equality can compare through templates and columns.
    term: Option<TermExpr<'a>>,
}

impl<'a> Branch<'a> {
    /// The filter `expression` over the variables of `env`. Sources in
    /// the `certain` scopes are bound wherever the filter is evaluated;
    /// any other source only where its `OPTIONAL` part matched.
    pub(super) fn filter(
        &self,
        expression: &'a Expression,
        env: &Env<'a>,
        certain: &[Scope],
    ) -> Result<Filter, Error> {
        Context {
            branch: self,
            env,
            certain,
        }
        .boolean(expression)
    }

    /// The value of an `ORDER BY` comparator over the branch's solutions,
    /// in which only the sources of the whole branch are always bound.
    pub(super) fn order_value(&self, expression: &'a Expression) -> Result<OrderValue, Error> {
        let context = Context {
            branch: self,
            env: &self.env,
            certain: &[None],
        };
        if !expression.is_term() {
            return Ok(OrderValue::Boolean(Box::new(context.boolean(expression)?)));
        }
        Ok(OrderValue::Term(
            context
                .operand(expression)?
                .into_iter()
                .map(|(guard, leaf)| (guard.into_iter().collect(), leaf.value))
                .collect(),
        ))
    }

    /// A condition that holds exactly when `source` is bound.
    fn bound(&self, source: Source<'a>) -> Result<Condition, Error> {
        let (table, name) = match source.term.columns().first() {
            Some(&column) => column,
            None => self.witness(source.scope)?,
        };
        Ok(Condition::IsNotNull(super::column(table, name)))
    }
}

struct Context<'c, 'a> {
    branch: &'c Branch<'a>,
    env: &'c Env<'a>,
    certain: &'c [Scope],
}

impl<'a> Context<'_, 'a> {
    fn boolean(&self, expression: &'a Expression) -> Result<Filter, Error> {
        let boxed = |e| self.boolean(e).map(Box::new);
        Ok(match expression {
            Expression::Or(a, b) => Filter::Or(boxed(a)?, boxed(b)?),
            Expression::And(a, b) => Filter::And(boxed(a)?, boxed(b)?),
            Expression::Not(a) => Filter::Not(boxed(a)?),
            Expression::Compare(op, a, b) => self.binary(a, b, |l, r| Filter::Compare {
                op: *op,
                left: l.value.clone(),
                right: r.value.clone(),
                same_term: same_term(l, r),
            })?,
            Expression::SameTerm(a, b) => self.binary(a, b, |l, r| Filter::SameTerm {
                left: l.value.clone(),
                right: r.value.clone(),
                same_term: same_term(l, r),
            })?,
            Expression::Bound(variable) => self.bound(variable)?,
            Expression::Test(test, a) => {
                self.unary(a, |leaf| Filter::Test(*test, leaf.value.clone()))?
            }
            Expression::Regex(text, pattern, flags) => {
                // The pattern and the flags are simple literals (SPARQL 1.1
                // §17.4.3.14); anything else is a type error.
                let simple = |term: &Term| match term {
                    Term::Literal(l) if l.datatype().as_str() == XSD_STRING => {
                        Some(l.value().to_owned())
                    }
                    _ => None,
                };
                let flags = match flags {
                    Some(flags) => simple(flags),
                    None => Some(String::new()),
                };
                let (Some(pattern), Some(flags)) = (simple(pattern), flags) else {
                    return Ok(Filter::Constant(None));
                };
                match translate(&pattern, &flags) {
                    Ok(pattern) => self.unary(text, |leaf| Filter::Regex {
                        text: leaf.value.clone(),
                        pattern: pattern.clone(),
                    })?,
                    Err(Untranslated::Invalid) => Filter::Constant(None),
                    Err(Untranslated::Unsupported(reason)) => {
                        return Err(Error::UnsupportedFeature(reason));
                    }
                }
            }
            // A term where a boolean is expected stands for its effective
            // boolean value (SPARQL 1.1 §17.2.2).
            _ => self.unary(expression, |leaf| Filter::Ebv(leaf.value.clone()))?,
        })
    }

    /// The ways `expression` is bound. None for a variable that is never
    /// bound here, so that any function of it is an error (SPARQL 1.1
    /// §17.2: arguments are unbound).
    fn operand(
        &self,
        expression: &'a Expression,
    ) -> Result<Vec<(Option<Condition>, Leaf<'a>)>, Error> {
        let derived = |a: &'a Expression, f: fn(Box<FilterValue>) -> FilterValue| {
            Ok(self
                .operand(a)?
                .into_iter()
                .map(|(guard, leaf)| {
                    let value = f(Box::new(leaf.value));
                    (guard, Leaf { value, term: None })
                })
                .collect())
        };
        match expression {
            Expression::Constant(term) => Ok(vec![(None, leaf(TermExpr::constant(term)))]),
            Expression::Variable(name) => Ok(self
                .operand_of(name)?
                .into_iter()
                .map(|(guard, term)| (guard, leaf(term)))
                .collect()),
            Expression::Str(a) => derived(a, FilterValue::Str),
            Expression::Lang(a) => derived(a, FilterValue::Lang),
            Expression::Datatype(a) => derived(a, FilterValue::Datatype),
            _ => Err(Error::UnsupportedFeature(
                "expressions other than terms as arguments in FILTER".to_owned(),
            )),
        }
    }

    fn unary(&self, a: &'a Expression, f: impl Fn(&Leaf<'a>) -> Filter) -> Result<Filter, Error> {
        Ok(cases(
            self.operand(a)?
                .into_iter()
                .map(|(guard, leaf)| (guard.into_iter().collect(), f(&leaf)))
                .collect(),
        ))
    }

    /// One case per pair of ways the operands are bound, in order, so the
    /// first that holds pairs the first bound source of each.
    fn binary(
        &self,
        a: &'a Expression,
        b: &'a Expression,
        f: impl Fn(&Leaf<'a>, &Leaf<'a>) -> Filter,
    ) -> Result<Filter, Error> {
        let right = self.operand(b)?;
        let mut out = Vec::new();
        for (left_guard, left) in self.operand(a)? {
            for (right_guard, right) in &right {
                let guards = left_guard.iter().chain(right_guard).cloned().collect();
                out.push((guards, f(&left, right)));
            }
        }
        Ok(cases(out))
    }

    /// BOUND is never an error (SPARQL 1.1 §17.4.1.1).
    fn bound(&self, variable: &str) -> Result<Filter, Error> {
        let bound = self.operand_of(variable)?;
        Ok(match bound.as_slice() {
            [] => Filter::Constant(Some(false)),
            [(None, _)] => Filter::Constant(Some(true)),
            _ => Filter::Any(bound.into_iter().filter_map(|(guard, _)| guard).collect()),
        })
    }

    /// The ways `variable` is bound: once where a certain source binds it,
    /// or once per `OPTIONAL` part that does, since only the rows tell
    /// which matched. Bound sources of one row are the same term.
    fn operand_of(&self, variable: &str) -> Result<Vec<Bound<'a>>, Error> {
        let Some((_, sources)) = self.env.iter().find(|(v, _)| *v == variable) else {
            return Ok(Vec::new());
        };
        if let Some(source) = sources.iter().find(|s| self.certain.contains(&s.scope)) {
            return Ok(vec![(None, source.term)]);
        }
        let mut scopes = Vec::new();
        let mut out = Vec::new();
        for source in sources {
            if !scopes.contains(&source.scope) {
                scopes.push(source.scope);
                out.push((Some(self.branch.bound(*source)?), source.term));
            }
        }
        Ok(out)
    }
}

fn cases(mut cases: Vec<(Vec<Condition>, Filter)>) -> Filter {
    match cases.as_slice() {
        [] => Filter::Constant(None),
        [(guards, _)] if guards.is_empty() => cases.pop().expect("one case").1,
        _ => Filter::Cases(cases),
    }
}

fn leaf(term: TermExpr<'_>) -> Leaf<'_> {
    Leaf {
        value: value(&term),
        term: Some(term),
    }
}

fn value(term: &TermExpr<'_>) -> FilterValue {
    match term.value {
        TermValue::Constant(constant) => FilterValue::Constant(constant.clone()),
        _ => FilterValue::Term(FilterTerm {
            term_type: term.term_type,
            term: term.to_term_sql(),
            language: term.language.map(str::to_owned),
        }),
    }
}

fn same_term(left: &Leaf<'_>, right: &Leaf<'_>) -> Option<TermMatch> {
    Some(match term_eq(left.term.as_ref()?, right.term.as_ref()?) {
        Equality::Never => TermMatch::Never,
        Equality::When(conditions) => TermMatch::When(conditions),
    })
}
