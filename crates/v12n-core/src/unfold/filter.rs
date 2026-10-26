// SPDX-License-Identifier: Apache-2.0
//! `FILTER` expressions over the terms a branch binds.

use super::{Branch, Env, Scope, Source};
use crate::Error;
use crate::sparql::Expression;
use crate::sql::{Condition, Filter, FilterTerm, FilterValue, TermMatch};

use super::term::{Equality, TermExpr, TermValue, term_eq};

/// A way an operand is bound: in the rows where the guard holds, or in
/// every row without one.
type Bound<'a> = (Option<Condition>, TermExpr<'a>);

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
                left: value(l),
                right: value(r),
                same_term: same_term(l, r),
            })?,
            Expression::SameTerm(a, b) => self.binary(a, b, |l, r| Filter::SameTerm {
                left: value(l),
                right: value(r),
                same_term: same_term(l, r),
            })?,
            Expression::Bound(variable) => self.bound(variable)?,
            Expression::Test(test, a) => self.unary(a, |t| Filter::Test(*test, value(t)))?,
            // A term where a boolean is expected stands for its effective
            // boolean value (SPARQL 1.1 §17.2.2).
            Expression::Constant(_) | Expression::Variable(_) => {
                self.unary(expression, |t| Filter::Ebv(value(t)))?
            }
        })
    }

    /// The ways `expression` is bound. None for a variable that is never
    /// bound here, so that any function of it is an error (SPARQL 1.1
    /// §17.2: arguments are unbound).
    fn operand(&self, expression: &'a Expression) -> Result<Vec<Bound<'a>>, Error> {
        match expression {
            Expression::Constant(term) => Ok(vec![(None, TermExpr::constant(term))]),
            Expression::Variable(name) => self.operand_of(name),
            _ => Err(Error::UnsupportedFeature(
                "expressions other than variables and constants as arguments in FILTER".to_owned(),
            )),
        }
    }

    fn unary(
        &self,
        a: &'a Expression,
        f: impl Fn(&TermExpr<'a>) -> Filter,
    ) -> Result<Filter, Error> {
        Ok(cases(
            self.operand(a)?
                .into_iter()
                .map(|(guard, term)| (guard.into_iter().collect(), f(&term)))
                .collect(),
        ))
    }

    /// One case per pair of ways the operands are bound, in order, so the
    /// first that holds pairs the first bound source of each.
    fn binary(
        &self,
        a: &'a Expression,
        b: &'a Expression,
        f: impl Fn(&TermExpr<'a>, &TermExpr<'a>) -> Filter,
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

fn same_term(left: &TermExpr<'_>, right: &TermExpr<'_>) -> TermMatch {
    match term_eq(left, right) {
        Equality::Never => TermMatch::Never,
        Equality::When(conditions) => TermMatch::When(conditions),
    }
}
