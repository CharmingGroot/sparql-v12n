// SPDX-License-Identifier: Apache-2.0
//! `FILTER` expressions over the terms a branch binds.

use super::{Branch, Env, Scope, Source};
use oxrdf::{Literal, Term};

use crate::Error;
use crate::sparql::{CompareOp, Expression, StrTest};
use crate::sql::{Condition, Filter, FilterTerm, FilterValue, OrderValue, TermMatch, ValueCases};
use crate::xpath_regex::{Untranslated, translate, translate_replace};
use crate::xsd::{self, Num, Value};

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
                .map(|(guards, leaf)| (guards, leaf.value))
                .collect(),
        ))
    }

    /// The value of a `BIND` or `SELECT` expression over the branch's
    /// solutions, by cases as for a term of `ORDER BY`.
    pub(super) fn value(&self, expression: &'a Expression) -> Result<ValueCases, Error> {
        let context = Context {
            branch: self,
            env: &self.env,
            certain: &[None],
        };
        Ok(context
            .operand(expression)?
            .into_iter()
            .map(|(guards, leaf)| (guards, leaf.value))
            .collect())
    }

    /// A condition that holds exactly when `source` is bound.
    pub(super) fn bound(&self, source: Source<'a>) -> Result<Condition, Error> {
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
            Expression::Compare(op, a, b) => self.compare(*op, a, b)?,
            // An error in one comparison is overruled by a true one, as
            // `||` does (SPARQL 1.1 §17.4.1.9). Each comparison keeps the
            // form of `=`, so a column compared with constants is a
            // disjunction of conditions on the column.
            Expression::In(a, list) => {
                let mut out: Option<Filter> = None;
                for b in list {
                    let equal = self.compare(CompareOp::Equal, a, b)?;
                    out = Some(match out {
                        Some(left) => Filter::Or(Box::new(left), Box::new(equal)),
                        None => equal,
                    });
                }
                out.unwrap_or(Filter::Constant(Some(false)))
            }
            Expression::SameTerm(a, b) => self.binary(a, b, |l, r| Filter::SameTerm {
                left: l.value.clone(),
                right: r.value.clone(),
                same_term: same_term(l, r),
            })?,
            Expression::StrTest(test, a, b) => self.binary(a, b, |l, r| Filter::StrTest {
                test: *test,
                text: l.value.clone(),
                search: r.value.clone(),
            })?,
            Expression::Bound(variable) => self.bound(variable)?,
            Expression::Test(test, a) => {
                self.unary(a, |leaf| Filter::Test(*test, leaf.value.clone()))?
            }
            Expression::Regex(text, pattern, flags) => {
                // The pattern and the flags are simple literals (SPARQL 1.1
                // §17.4.3.14); anything else is a type error.
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

    fn compare(
        &self,
        op: CompareOp,
        a: &'a Expression,
        b: &'a Expression,
    ) -> Result<Filter, Error> {
        self.binary(a, b, |l, r| Filter::Compare {
            op,
            left: l.value.clone(),
            right: r.value.clone(),
            same_term: same_term(l, r),
        })
    }

    /// The ways `expression` is bound. None for a variable that is never
    /// bound here, so that any function of it is an error (SPARQL 1.1
    /// §17.2: arguments are unbound).
    fn operand(
        &self,
        expression: &'a Expression,
    ) -> Result<Vec<(Vec<Condition>, Leaf<'a>)>, Error> {
        let derived = |a: &'a Expression, f: fn(Box<FilterValue>) -> FilterValue| {
            Ok(self
                .operand(a)?
                .into_iter()
                .map(|(guards, leaf)| {
                    let value = f(Box::new(leaf.value));
                    (guards, Leaf { value, term: None })
                })
                .collect())
        };
        match expression {
            Expression::Constant(term) => Ok(vec![(Vec::new(), leaf(TermExpr::constant(term)))]),
            Expression::Variable(name) => Ok(self
                .operand_of(name)?
                .into_iter()
                .map(|(guard, term)| (guard.into_iter().collect(), leaf(term)))
                .collect()),
            Expression::Str(a) => derived(a, FilterValue::Str),
            Expression::Lang(a) => derived(a, FilterValue::Lang),
            Expression::Datatype(a) => derived(a, FilterValue::Datatype),
            Expression::Negate(a) => derived(a, FilterValue::Negate),
            Expression::StrLen(a) => derived(a, FilterValue::StrLen),
            Expression::UCase(a) => derived(a, FilterValue::UCase),
            Expression::LCase(a) => derived(a, FilterValue::LCase),
            Expression::Substr(a, start, length) => {
                let Some((start, length)) = substr_range(start, length.as_ref())? else {
                    return Ok(Vec::new());
                };
                Ok(self
                    .operand(a)?
                    .into_iter()
                    .map(|(guards, leaf)| {
                        let value = FilterValue::Substr {
                            text: Box::new(leaf.value),
                            start,
                            length,
                        };
                        (guards, Leaf { value, term: None })
                    })
                    .collect())
            }
            // One case per combination of the ways the arguments are bound.
            Expression::Concat(args) => {
                let mut out = vec![(Vec::new(), Vec::new())];
                for arg in args {
                    let cases = self.operand(arg)?;
                    let mut next = Vec::with_capacity(out.len() * cases.len());
                    for (guards, values) in &out {
                        for (arg_guards, leaf) in &cases {
                            let mut guards: Vec<Condition> = guards.clone();
                            guards.extend(arg_guards.iter().cloned());
                            let mut values: Vec<FilterValue> = values.clone();
                            values.push(leaf.value.clone());
                            next.push((guards, values));
                        }
                    }
                    out = next;
                }
                Ok(out
                    .into_iter()
                    .map(|(guards, values)| {
                        let value = FilterValue::Concat(values);
                        (guards, Leaf { value, term: None })
                    })
                    .collect())
            }
            Expression::EncodeForUri(a) => derived(a, FilterValue::EncodeForUri),
            Expression::Hash(function, a) => Ok(self
                .operand(a)?
                .into_iter()
                .map(|(guards, leaf)| {
                    let value = FilterValue::Hash(*function, Box::new(leaf.value));
                    (guards, Leaf { value, term: None })
                })
                .collect()),
            Expression::Replace(a, pattern, replacement, flags) => {
                // The pattern, the replacement and the flags are simple
                // literals (SPARQL 1.1 §17.4.3.15); anything else is a type
                // error, and so are an invalid pattern, replacement or flags,
                // and a pattern that matches the empty string (F&O 3.1
                // §5.6.4).
                let flags = match flags {
                    Some(flags) => simple(flags),
                    None => Some(String::new()),
                };
                let (Some(pattern), Some(replacement), Some(flags)) =
                    (simple(pattern), simple(replacement), flags)
                else {
                    return Ok(Vec::new());
                };
                let translated = match translate_replace(&pattern, &flags, &replacement) {
                    Ok(translated) => translated,
                    Err(Untranslated::Invalid) => return Ok(Vec::new()),
                    Err(Untranslated::Unsupported(reason)) => {
                        return Err(Error::UnsupportedFeature(reason));
                    }
                };
                Ok(self
                    .operand(a)?
                    .into_iter()
                    .map(|(guards, leaf)| {
                        let value = FilterValue::Replace {
                            text: Box::new(leaf.value),
                            pattern: translated.pattern.clone(),
                            replacement: translated.replacement.clone(),
                        };
                        (guards, Leaf { value, term: None })
                    })
                    .collect())
            }
            // Without the second string in the first, the result is a
            // simple literal, whatever the language tag of the first
            // (SPARQL 1.1 §17.4.3.9, §17.4.3.10). Where the first can have
            // one, the cases tell the two apart by CONTAINS, which is an
            // error for arguments that are not compatible.
            Expression::StrBefore(a, b) | Expression::StrAfter(a, b) => {
                let right = self.operand(b)?;
                let mut out = Vec::new();
                for (left_guards, left) in self.operand(a)? {
                    for (right_guards, right) in &right {
                        let guards: Vec<Condition> =
                            left_guards.iter().chain(right_guards).cloned().collect();
                        let (text, search) = (left.value.clone(), right.value.clone());
                        let value = match expression {
                            Expression::StrBefore(..) => FilterValue::StrBefore(
                                Box::new(text.clone()),
                                Box::new(search.clone()),
                            ),
                            _ => FilterValue::StrAfter(
                                Box::new(text.clone()),
                                Box::new(search.clone()),
                            ),
                        };
                        if !may_have_language(&text) {
                            out.push((guards, Leaf { value, term: None }));
                            continue;
                        }
                        let found = Filter::StrTest {
                            test: StrTest::Contains,
                            text,
                            search,
                        };
                        let mut found_guards = guards.clone();
                        found_guards.push(Condition::Filter(found.clone()));
                        out.push((found_guards, Leaf { value, term: None }));
                        let mut missing_guards = guards;
                        missing_guards.push(Condition::Filter(Filter::Not(Box::new(found))));
                        let empty = FilterValue::Constant(Literal::new_simple_literal("").into());
                        out.push((
                            missing_guards,
                            Leaf {
                                value: empty,
                                term: None,
                            },
                        ));
                    }
                }
                Ok(out)
            }
            Expression::Numeric(function, a) => Ok(self
                .operand(a)?
                .into_iter()
                .map(|(guards, leaf)| {
                    let value = FilterValue::Numeric(*function, Box::new(leaf.value));
                    (guards, Leaf { value, term: None })
                })
                .collect()),
            // One case per pair of ways the operands are bound, as for a
            // comparison.
            Expression::Arith(op, a, b) => {
                let right = self.operand(b)?;
                let mut out = Vec::new();
                for (left_guards, left) in self.operand(a)? {
                    for (right_guards, right) in &right {
                        let guards = left_guards.iter().chain(right_guards).cloned().collect();
                        let value = FilterValue::Arith(
                            *op,
                            Box::new(left.value.clone()),
                            Box::new(right.value.clone()),
                        );
                        out.push((guards, Leaf { value, term: None }));
                    }
                }
                Ok(out)
            }
            // The cases of the branch that the condition chooses, so where
            // it is an error no case holds (SPARQL 1.1 §17.4.1.2).
            Expression::If(condition, a, b) => {
                let chosen = self.boolean(condition)?;
                match constant(&chosen) {
                    Some(Some(true)) => return self.operand(a),
                    Some(Some(false)) => return self.operand(b),
                    Some(None) => return Ok(Vec::new()),
                    None => {}
                }
                let not_chosen = Filter::Not(Box::new(chosen.clone()));
                let mut out = Vec::new();
                for (guard, branch) in [(chosen, a), (not_chosen, b)] {
                    let guard = Condition::Filter(guard);
                    for (mut guards, leaf) in self.operand(branch)? {
                        guards.insert(0, guard.clone());
                        out.push((guards, leaf));
                    }
                }
                Ok(out)
            }
            // The cases of every argument in order, each also where its
            // value is not an error, so that the first case that holds is
            // the first argument that is not an error (SPARQL 1.1
            // §17.4.1.3). A variable or a constant is never an error where
            // it is bound.
            Expression::Coalesce(args) => {
                let mut out = Vec::new();
                for arg in args {
                    for (mut guards, leaf) in self.operand(arg)? {
                        if !matches!(leaf.value, FilterValue::Term(_) | FilterValue::Constant(_)) {
                            guards.push(Condition::Filter(Filter::NoError(leaf.value.clone())));
                        }
                        out.push((guards, leaf));
                    }
                }
                Ok(out)
            }
            _ => Err(Error::UnsupportedFeature(
                "expressions other than terms as arguments in FILTER".to_owned(),
            )),
        }
    }

    fn unary(&self, a: &'a Expression, f: impl Fn(&Leaf<'a>) -> Filter) -> Result<Filter, Error> {
        Ok(cases(
            self.operand(a)?
                .into_iter()
                .map(|(guards, leaf)| (guards, f(&leaf)))
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

/// The positions that `SUBSTR(source, start, length)` takes, as the start
/// and the length of PostgreSQL's `substr`: fn:substring takes the
/// characters at the positions `p` with `start <= p < start + length`,
/// counted from 1 (SPARQL 1.1 §17.4.3.3), so a start below 1 shortens the
/// result and a negative length gives the empty string. `None` for a type
/// error: an argument that is not an `xsd:integer`.
fn substr_range(start: &Term, length: Option<&Term>) -> Result<Option<(i32, Option<i32>)>, Error> {
    // No string has more characters than an int4 counts, so larger
    // positions all give the same result.
    const LIMIT: i128 = i32::MAX as i128;
    let Some(start) = substr_integer(start)? else {
        return Ok(None);
    };
    let length = match length.map(substr_integer).transpose()? {
        Some(None) => return Ok(None),
        Some(Some(length)) => Some(length),
        None => None,
    };
    let first = start.clamp(1, LIMIT);
    let count = length.map(|length| {
        // Past the end of every string where the sum overflows upwards,
        // before its start where it overflows downwards.
        let end =
            start
                .checked_add(length)
                .unwrap_or(if length > 0 { i128::MAX } else { i128::MIN });
        end.saturating_sub(first).clamp(0, LIMIT - first)
    });
    let int4 = |n: i128| i32::try_from(n).expect("clamped to the int4 range");
    Ok(Some((int4(first), count.map(int4))))
}

/// An `xsd:integer` argument of `SUBSTR`; `None` for a type error.
fn substr_integer(term: &Term) -> Result<Option<i128>, Error> {
    let Term::Literal(literal) = term else {
        return Ok(None);
    };
    match xsd::value(literal.value(), literal.datatype().as_str()) {
        Value::Numeric(n) if n.class == Num::Integer => n
            .decimal
            .expect("integers have a decimal form")
            .parse::<i128>()
            .map(Some)
            .map_err(|_| {
                Error::UnsupportedFeature(
                    "SUBSTR with a starting location or length beyond the range of a 128-bit \
                     integer"
                        .to_owned(),
                )
            }),
        Value::Numeric(_) => Err(Error::UnsupportedFeature(
            "SUBSTR with a starting location or length that is not an xsd:integer: SPARQL 1.1 \
             makes it a type error, while engines that round it as fn:substring does differ"
                .to_owned(),
        )),
        Value::Unsupported(_) => Err(Error::UnsupportedFeature(format!(
            "SUBSTR with a starting location or length of datatype <{}>",
            literal.datatype().as_str()
        ))),
        _ => Ok(None),
    }
}

/// The lexical form of a simple literal; `None` for any other term.
fn simple(term: &Term) -> Option<String> {
    match term {
        Term::Literal(l) if l.datatype().as_str() == XSD_STRING => Some(l.value().to_owned()),
        _ => None,
    }
}

/// Whether `value` can be a literal with a language tag.
fn may_have_language(value: &FilterValue) -> bool {
    match value {
        FilterValue::Term(term) => term.language.is_some(),
        FilterValue::Constant(Term::Literal(literal)) => literal.language().is_some(),
        FilterValue::Substr { text, .. }
        | FilterValue::UCase(text)
        | FilterValue::LCase(text)
        | FilterValue::Replace { text, .. }
        | FilterValue::StrBefore(text, _)
        | FilterValue::StrAfter(text, _) => may_have_language(text),
        FilterValue::Concat(args) => !args.is_empty() && args.iter().all(may_have_language),
        _ => false,
    }
}

/// The value of a filter that does not depend on the row, `None` for one
/// that does.
fn constant(filter: &Filter) -> Option<Option<bool>> {
    match filter {
        Filter::Constant(value) => Some(*value),
        Filter::Not(inner) => constant(inner).map(|value| value.map(|b| !b)),
        _ => None,
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
