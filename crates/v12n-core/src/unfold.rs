// SPDX-License-Identifier: Apache-2.0
//! Unfolding: triple patterns to SQL fragments, shared variables to joins,
//! `OPTIONAL` to left joins.

mod filter;
mod term;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Range;

use oxrdf::Term;

use crate::Error;
use crate::mapping::{
    BaseIri, LogicalTable, Mapping, MappingIndex, ObjectMap, TemplatePart, TermType, TriplesMap,
};
use crate::sparql::{Expression, Pattern, PatternTerm, RDF_TYPE, SelectQuery, TriplePattern};
use crate::sql::{
    Condition, LeftJoin, OrderKey, ProjectedTerm, ResultTerm, ResultValue, SqlBranch, SqlExpr,
    SqlPlan,
};
use term::{Equality, LiteralType, TermExpr, TermValue, is_injective, literal_type, term_eq};

/// Most SQL branches one query may unfold into. Each `UNION` adds its
/// arms, and each pattern with several candidate triples maps multiplies
/// the count. The same limit applies to the combinations tried for each
/// `OPTIONAL` part.
pub const MAX_BRANCHES: usize = 64;

/// SQL type of the flag that tells whether an `OPTIONAL` part matched.
const BOOLEAN: &str = "\"pg_catalog\".\"bool\"";

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

/// Unfolds a query into a SQL plan over the mapping's logical tables.
///
/// Each arm of the pattern, a choice of one side of every `UNION`, and
/// every choice of triples maps for the arm's required triple patterns is
/// one SQL branch: Join, Filter and LeftJoin (on its left side) distribute
/// over a union (SPARQL 1.1 §18.5). The right side of a LeftJoin does not,
/// so each `OPTIONAL` part becomes a `LEFT JOIN` inside the branch, and a
/// part that several choices can match splits the branch (see
/// [`Branch::optional`]).
pub fn unfold(
    query: &SelectQuery,
    mapping: &Mapping,
    index: &MappingIndex,
) -> Result<SqlPlan, Error> {
    let context = Context { mapping, index };
    let too_many = || Error::TooManyBranches {
        limit: MAX_BRANCHES,
    };
    let arms = query.pattern.arms(MAX_BRANCHES).ok_or_else(too_many)?;
    let mut total = 0usize;
    let mut branches = Vec::new();
    for (a, arm) in arms.iter().enumerate() {
        let candidates = query
            .pattern
            .required_triple_patterns(arm)
            .into_iter()
            .map(|p| candidates(p, mapping, index))
            .collect::<Result<Vec<_>, _>>()?;
        let choices = combinations(&candidates)?;
        total += choices.len();
        if total > MAX_BRANCHES {
            return Err(too_many());
        }
        for choice in choices {
            let chosen: Vec<_> = candidates
                .iter()
                .zip(choice)
                .map(|(list, c)| list[c])
                .collect();
            branches.extend(Branch::build(&query.pattern, a, arm, &chosen, &context)?);
            if branches.len() > MAX_BRANCHES {
                return Err(too_many());
            }
        }
    }
    plan(query, branches, mapping.base_iri())
}

struct Context<'m> {
    mapping: &'m Mapping,
    index: &'m MappingIndex,
}

/// One way a triple pattern can be produced.
#[derive(Debug, Clone, Copy)]
enum Candidate<'m> {
    /// `rdf:type` triples from the `rr:class` of a subject map.
    Class(&'m TriplesMap),
    /// Triples from one object map of a predicate-object map.
    Property {
        triples_map: &'m TriplesMap,
        object_map: &'m ObjectMap,
    },
}

impl<'m> Candidate<'m> {
    fn triples_map(&self) -> &'m TriplesMap {
        match self {
            Self::Class(tm) => tm,
            Self::Property { triples_map, .. } => triples_map,
        }
    }
}

fn candidates<'m>(
    pattern: &TriplePattern,
    mapping: &'m Mapping,
    index: &MappingIndex,
) -> Result<Vec<Candidate<'m>>, Error> {
    let triples_map = |id| {
        mapping
            .triples_map(id)
            .expect("the index only refers to triples maps of the mapping")
    };
    let mut out = Vec::new();
    if pattern.predicate == RDF_TYPE {
        match &pattern.object {
            PatternTerm::Variable(_) => {
                return Err(Error::UnsupportedFeature(
                    "rdf:type with a variable class".to_owned(),
                ));
            }
            PatternTerm::Constant(Term::NamedNode(class)) => {
                out.extend(
                    index
                        .class(class.as_str())
                        .iter()
                        .map(|id| Candidate::Class(triples_map(id))),
                );
            }
            PatternTerm::Constant(_) => {}
        }
    }
    for candidate in index.property(&pattern.predicate) {
        let tm = triples_map(&candidate.triples_map);
        let pom = &tm.predicate_object_maps[candidate.predicate_object_map];
        out.extend(
            pom.object_maps
                .iter()
                .map(|object_map| Candidate::Property {
                    triples_map: tm,
                    object_map,
                }),
        );
    }
    Ok(out)
}

/// Every choice of one candidate per pattern, in a fixed order: the last
/// pattern's choice varies fastest.
fn combinations<T>(candidates: &[Vec<T>]) -> Result<Vec<Vec<usize>>, Error> {
    let total = candidates
        .iter()
        .try_fold(1usize, |acc, c| acc.checked_mul(c.len()))
        .filter(|&n| n <= MAX_BRANCHES)
        .ok_or(Error::TooManyBranches {
            limit: MAX_BRANCHES,
        })?;
    let mut out = Vec::with_capacity(total);
    let mut choice = vec![0; candidates.len()];
    for _ in 0..total {
        out.push(choice.clone());
        for (slot, c) in choice.iter_mut().zip(candidates).rev() {
            *slot += 1;
            if *slot < c.len() {
                break;
            }
            *slot = 0;
        }
    }
    Ok(out)
}

/// Where a condition applies and which rows bind a term: the whole branch
/// (`None`), or the `OPTIONAL` part at this index of [`Branch::groups`].
type Scope = Option<usize>;

/// A term a variable is bound to.
#[derive(Debug, Clone, Copy)]
struct Source<'a> {
    term: TermExpr<'a>,
    /// The term is unbound exactly in the rows where this scope's
    /// `OPTIONAL` part did not match: a left join leaves all its columns
    /// `NULL`, while a match has none `NULL` (R2RML §11.1, see
    /// [`Branch::add_not_null`]).
    scope: Scope,
}

/// The variables a part of the pattern binds, in order of first
/// occurrence, each with the terms it is bound to. Where several of them
/// are bound, they are the same RDF term.
type Env<'a> = Vec<(&'a str, Vec<Source<'a>>)>;

/// The sources to compare with: the first one bound wherever `scope` has
/// rows, or else the first one of every `OPTIONAL` part, since only the
/// rows tell which of those are bound.
fn bound_sources<'a>(sources: &[Source<'a>], scope: Scope) -> Vec<Source<'a>> {
    if let Some(certain) = sources.iter().find(|s| s.scope == scope) {
        return vec![*certain];
    }
    let mut out: Vec<Source<'a>> = Vec::new();
    for source in sources {
        if !out.iter().any(|s| s.scope == source.scope) {
            out.push(*source);
        }
    }
    out
}

/// Drops the sources of variables that `scope` binds in all its rows: a
/// part nested in it binds them only where they are already bound, to
/// the same term.
fn settle<'a>(mut env: Env<'a>, scope: Scope) -> Env<'a> {
    for (_, sources) in &mut env {
        if let Some(certain) = sources.iter().find(|s| s.scope == scope).copied() {
            *sources = vec![certain];
        }
    }
    env
}

/// A branch with the variables of the part of the pattern walked so far.
type Fork<'a> = (Branch<'a>, Env<'a>);

/// The sides of the `Union`s of an arm, in the order they are walked.
type Sides<'s> = std::iter::Copied<std::slice::Iter<'s, bool>>;

/// The candidates of the required triple patterns, in the order they are
/// walked.
type Chosen<'s, 'a> = std::iter::Copied<std::slice::Iter<'s, Candidate<'a>>>;

/// What one choice of arm and triples maps for the right side of a
/// LeftJoin gives.
enum Attempt<'a> {
    /// No triple pattern: the side's one solution binds nothing.
    Empty,
    /// Never compatible with the left side.
    NoMatch,
    /// The branch with the side as `OPTIONAL` part `group`, and the
    /// variables of both sides.
    Matched(Fork<'a>, usize),
}

/// An `OPTIONAL` part of a branch.
#[derive(Debug, Clone)]
struct Group {
    /// The number of its LeftJoin in the pattern (see [`Branch::walk`]),
    /// the same in every branch.
    node: usize,
    parent: Scope,
    tables: Range<usize>,
    conditions: Vec<Condition>,
}

/// A branch under construction: one candidate per required pattern,
/// joined, with its `OPTIONAL` parts left joined.
#[derive(Debug, Clone)]
struct Branch<'a> {
    /// The arm of the pattern (see [`Pattern::arms`]) the branch belongs
    /// to, followed by the arm of the right side of each LeftJoin that
    /// split the branch (see [`Branch::optional`]). Branches with equal
    /// arms remove their duplicates together.
    arm: Vec<usize>,
    from: Vec<LogicalTable>,
    /// Conditions on every row of the branch.
    conditions: Vec<Condition>,
    /// `OPTIONAL` parts that can match, in the order they are written; a
    /// part comes before the parts nested in it.
    groups: Vec<Group>,
    /// Every term the branch produces, in pattern order, with the scope
    /// whose rows produce it.
    terms: Vec<(TermExpr<'a>, Scope)>,
    /// The variables of the whole pattern.
    env: Env<'a>,
}

impl<'a> Branch<'a> {
    /// The branches the pattern gives with these choices; none when they
    /// can never produce a row.
    fn build(
        pattern: &'a Pattern,
        arm_index: usize,
        arm: &[bool],
        chosen: &[Candidate<'a>],
        context: &Context<'a>,
    ) -> Result<Vec<Self>, Error> {
        let branch = Self {
            arm: vec![arm_index],
            from: Vec::new(),
            conditions: Vec::new(),
            groups: Vec::new(),
            terms: Vec::new(),
            env: Vec::new(),
        };
        let mut chosen = chosen.iter().copied();
        let mut sides = arm.iter().copied();
        let forks = branch.walk(pattern, None, 1, &mut sides, &mut chosen, context)?;
        Ok(forks
            .into_iter()
            .map(|(mut branch, env)| {
                branch.env = env;
                branch.add_not_null();
                branch
            })
            .collect())
    }

    /// Adds the tables and conditions of `pattern` to `scope`, taking the
    /// side of each `Union` from `sides` and the candidates of its required
    /// triple patterns from `chosen`. Its LeftJoins are numbered in the
    /// order they are written, starting at `node`; 0 stands for the
    /// required patterns of the query.
    ///
    /// Each part is translated on its own and then joined, as the algebra
    /// evaluates it (SPARQL 1.1 §18.6): the conditions linking two parts go
    /// to the node that joins them, never into a part.
    ///
    /// Returns the branches `pattern` gives, more than one where an
    /// `OPTIONAL` part splits it, each with the variables bound; none when
    /// `pattern` has no solutions in `scope`.
    fn walk(
        self,
        pattern: &'a Pattern,
        scope: Scope,
        node: usize,
        sides: &mut Sides<'_>,
        chosen: &mut Chosen<'_, 'a>,
        context: &Context<'a>,
    ) -> Result<Vec<Fork<'a>>, Error> {
        match pattern {
            Pattern::Bgp(patterns) => {
                let chosen: Vec<_> = patterns
                    .iter()
                    .map(|p| {
                        let candidate = chosen
                            .next()
                            .expect("one candidate per required triple pattern");
                        (p, candidate)
                    })
                    .collect();
                let mut branch = self;
                Ok(branch
                    .bgp(&chosen, scope, context.mapping)
                    .map(|env| (branch, env))
                    .into_iter()
                    .collect())
            }
            Pattern::Join(left, right) => {
                let lefts = self.walk(left, scope, node, sides, chosen, context)?;
                if lefts.is_empty() {
                    skip(right, sides, chosen);
                    return Ok(Vec::new());
                }
                let right_node = node + left.left_joins();
                let (sides_at, chosen_at) = (sides.clone(), chosen.clone());
                let mut out = Vec::new();
                for (branch, l) in lefts {
                    // Every branch walks the right side with the same choices.
                    *sides = sides_at.clone();
                    *chosen = chosen_at.clone();
                    for (mut branch, r) in
                        branch.walk(right, scope, right_node, sides, chosen, context)?
                    {
                        if let Some(env) = branch.join(l.clone(), r, scope, scope, scope)? {
                            out.push((branch, settle(env, scope)));
                        }
                    }
                }
                check_count(out.len())?;
                Ok(out)
            }
            Pattern::LeftJoin(left, right, expression) => {
                let lefts = self.walk(left, scope, node, sides, chosen, context)?;
                let node = node + left.left_joins();
                let mut out = Vec::new();
                for (branch, l) in lefts {
                    out.extend(branch.optional(
                        l,
                        right,
                        expression.as_ref(),
                        scope,
                        node,
                        context,
                    )?);
                }
                check_count(out.len())?;
                Ok(out)
            }
            // Filter(expr, P) keeps the solutions of P where expr holds, so
            // it sees the variables of P only (SPARQL 1.1 §18.2.2.6).
            Pattern::Filter(inner, expression) => {
                let mut out = Vec::new();
                for (mut branch, env) in self.walk(inner, scope, node, sides, chosen, context)? {
                    let filter = branch.filter(expression, &env, &[scope])?;
                    branch.conditions_mut(scope).push(Condition::Filter(filter));
                    out.push((branch, env));
                }
                Ok(out)
            }
            // The branch belongs to one side of the union: the solutions of
            // the other side come from other branches, and the variables
            // only that side binds are unbound here.
            Pattern::Union(left, right) => {
                if sides.next().expect("one side per Union of the arm") {
                    let node = node + left.left_joins();
                    self.walk(right, scope, node, sides, chosen, context)
                } else {
                    self.walk(left, scope, node, sides, chosen, context)
                }
            }
        }
    }

    /// LeftJoin(left, right, expression) (SPARQL 1.1 §18.5): every
    /// solution of the left side, extended by each compatible solution of
    /// `right` for which the expression holds over both, where there are
    /// some. The expression goes into the `ON` clause with the part's
    /// other conditions.
    ///
    /// The right side of a LeftJoin does not distribute over a union, so
    /// its arms and triples maps are chosen here. A choice that can never
    /// be compatible with the left side is left out. When one is left, the
    /// part is one `LEFT JOIN` in the branch. When several are, `right` is
    /// their union R1 ∪ ... ∪ Rn, and LeftJoin(L, R1 ∪ ... ∪ Rn) is
    /// Join(L, R1) ∪ ... ∪ Join(L, Rn) ∪ Diff(L, R1 ∪ ... ∪ Rn), so the branch
    /// splits: one branch per Ri, where its `LEFT JOIN` matched, and one
    /// with the `LEFT JOIN` of every Ri, where none matched, which keeps
    /// the left side's solutions that nothing extends. Each split branch
    /// keeps the `ON` clauses of its parts, so compatibility and the
    /// expression decide whether a part matches exactly as for one part.
    fn optional(
        self,
        left: Env<'a>,
        right: &'a Pattern,
        expression: Option<&'a Expression>,
        scope: Scope,
        node: usize,
        context: &Context<'a>,
    ) -> Result<Vec<Fork<'a>>, Error> {
        let arms = right.arms(MAX_BRANCHES).ok_or(Error::TooManyBranches {
            limit: MAX_BRANCHES,
        })?;
        // Each choice that matches, with what identifies it among the
        // outcomes of `attempt`, and the arm of an empty one.
        let mut matched: Vec<(Fork<'a>, usize, usize)> = Vec::new();
        let mut choices: Vec<(usize, Vec<Candidate<'a>>, usize)> = Vec::new();
        let mut empty: Option<usize> = None;
        for (a, arm) in arms.iter().enumerate() {
            let candidates = right
                .required_triple_patterns(arm)
                .into_iter()
                .map(|p| candidates(p, context.mapping, context.index))
                .collect::<Result<Vec<_>, _>>()?;
            for choice in combinations(&candidates)? {
                let chosen: Vec<Candidate<'a>> = candidates
                    .iter()
                    .zip(choice)
                    .map(|(list, c)| list[c])
                    .collect();
                let attempts = self
                    .clone()
                    .attempt(&left, right, expression, scope, node, arm, &chosen, context)?;
                for (k, attempt) in attempts.into_iter().enumerate() {
                    match attempt {
                        Attempt::Empty => empty = Some(a),
                        Attempt::NoMatch => {}
                        Attempt::Matched(fork, group) => {
                            matched.push((fork, group, a));
                            choices.push((a, chosen.clone(), k));
                        }
                    }
                }
                check_count(matched.len())?;
            }
        }
        if matched.is_empty() {
            // Nothing extends the left side, or only the empty pattern's
            // one solution does: the left side's solutions stay as they are.
            return Ok(vec![(self, left)]);
        }
        if matched.len() == 1 && empty.is_none() {
            let ((branch, env), _, _) = matched.pop().expect("one choice matches");
            return Ok(vec![(branch, env)]);
        }
        if empty.is_some() && expression.is_some() {
            return Err(Error::UnsupportedFeature(
                "OPTIONAL with a FILTER over a UNION with an empty side".to_owned(),
            ));
        }
        let mut out = Vec::with_capacity(matched.len() + 1);
        for ((mut branch, env), group, a) in matched {
            let (table, name) = branch.witness(Some(group))?;
            branch
                .conditions_mut(scope)
                .push(Condition::IsNotNull(column(table, name)));
            branch.arm.push(a);
            out.push((branch, env));
        }
        match empty {
            // The empty pattern's solution is compatible with every left
            // solution, so none is left over for Diff.
            Some(a) => {
                let mut branch = self;
                branch.arm.push(a);
                out.push((branch, left));
            }
            None => {
                let mut rest = self;
                for (a, chosen, k) in choices {
                    let arm = &arms[a];
                    let attempt = rest
                        .attempt(&left, right, expression, scope, node, arm, &chosen, context)?
                        .into_iter()
                        .nth(k);
                    let Some(Attempt::Matched((branch, _), group)) = attempt else {
                        unreachable!("a choice matches the same way on any branch");
                    };
                    rest = branch;
                    let (table, name) = rest.witness(Some(group))?;
                    rest.conditions_mut(scope)
                        .push(Condition::IsNull(column(table, name)));
                }
                // Diff's solutions are never those of a Join, so they can
                // be compared with those of any arm.
                rest.arm.push(0);
                out.push((rest, left));
            }
        }
        check_count(out.len())?;
        Ok(out)
    }

    /// The right side of a LeftJoin with these choices, as a new `OPTIONAL`
    /// part of the branch: one outcome per branch the side gives.
    #[allow(clippy::too_many_arguments)]
    fn attempt(
        self,
        left: &Env<'a>,
        right: &'a Pattern,
        expression: Option<&'a Expression>,
        scope: Scope,
        node: usize,
        arm: &[bool],
        chosen: &[Candidate<'a>],
        context: &Context<'a>,
    ) -> Result<Vec<Attempt<'a>>, Error> {
        let mut branch = self;
        let group = branch.groups.len();
        let start = branch.from.len();
        branch.groups.push(Group {
            node,
            parent: scope,
            tables: start..start,
            conditions: Vec::new(),
        });
        let mut sides = arm.iter().copied();
        let mut chosen = chosen.iter().copied();
        let forks = branch.walk(
            right,
            Some(group),
            node + 1,
            &mut sides,
            &mut chosen,
            context,
        )?;
        let mut out = Vec::with_capacity(forks.len());
        for (mut attempt, inner) in forks {
            attempt.groups[group].tables.end = attempt.from.len();
            if attempt.groups[group].tables.is_empty() {
                if !attempt.groups[group].conditions.is_empty() {
                    return Err(Error::UnsupportedFeature(
                        "a FILTER in a UNION side without triple patterns inside OPTIONAL"
                            .to_owned(),
                    ));
                }
                out.push(Attempt::Empty);
                continue;
            }
            let inner = settle(inner, Some(group));
            let Some(env) = attempt.join(left.clone(), inner, scope, Some(group), Some(group))?
            else {
                out.push(Attempt::NoMatch);
                continue;
            };
            if let Some(expression) = expression {
                // In the `ON` clause, the left side's row and the part's
                // candidate row both exist.
                let filter = attempt.filter(expression, &env, &[scope, Some(group)])?;
                attempt.groups[group]
                    .conditions
                    .push(Condition::Filter(filter));
            }
            out.push(Attempt::Matched((attempt, env), group));
        }
        Ok(out)
    }

    /// Joins the solutions of two parts evaluated in `left_scope` and
    /// `right_scope`, with the conditions going to `dest`: a variable bound
    /// on both sides must be the same RDF term, unless it is unbound on
    /// one of them (compatible mappings, SPARQL 1.1 §18.3). `None` when no
    /// two solutions can be compatible.
    fn join(
        &mut self,
        mut left: Env<'a>,
        right: Env<'a>,
        left_scope: Scope,
        right_scope: Scope,
        dest: Scope,
    ) -> Result<Option<Env<'a>>, Error> {
        for (variable, sources) in right {
            let Some((_, existing)) = left.iter_mut().find(|(v, _)| *v == variable) else {
                left.push((variable, sources));
                continue;
            };
            for l in bound_sources(existing, left_scope) {
                for r in bound_sources(&sources, right_scope) {
                    let mut alternatives = Vec::new();
                    if l.scope != left_scope {
                        alternatives.push(vec![self.unbound(l)?]);
                    }
                    if r.scope != right_scope {
                        alternatives.push(vec![self.unbound(r)?]);
                    }
                    match term_eq(&r.term, &l.term) {
                        Equality::When(conditions) if conditions.is_empty() => continue,
                        Equality::When(conditions) => alternatives.push(conditions),
                        Equality::Never => {}
                    }
                    let conditions = self.conditions_mut(dest);
                    match alternatives.len() {
                        0 => return Ok(None),
                        1 => conditions.extend(alternatives.pop().unwrap_or_default()),
                        _ => conditions.push(Condition::Or(alternatives)),
                    }
                }
            }
            existing.extend(sources);
        }
        Ok(Some(left))
    }

    /// A condition that holds exactly when `source` is unbound.
    fn unbound(&self, source: Source<'a>) -> Result<Condition, Error> {
        let (table, name) = match source.term.columns().first() {
            Some(&column) => column,
            None => self.witness(source.scope)?,
        };
        Ok(Condition::IsNull(column(table, name)))
    }

    /// A column that is never `NULL` where `scope` matched, and always
    /// `NULL` where it did not: one its terms are built from.
    fn witness(&self, scope: Scope) -> Result<(usize, &'a str), Error> {
        self.terms
            .iter()
            .filter(|(_, s)| *s == scope)
            .find_map(|(term, _)| term.columns().first().copied())
            .ok_or_else(|| {
                Error::UnsupportedFeature(
                    "a constant term in an OPTIONAL part whose triples read no column".to_owned(),
                )
            })
    }

    fn conditions_mut(&mut self, scope: Scope) -> &mut Vec<Condition> {
        match scope {
            None => &mut self.conditions,
            Some(group) => &mut self.groups[group].conditions,
        }
    }

    fn conditions(&self, scope: Scope) -> &[Condition] {
        match scope {
            None => &self.conditions,
            Some(group) => &self.groups[group].conditions,
        }
    }

    /// The number of the LeftJoin whose rows bind a term of `scope`, 0 for
    /// the whole branch.
    fn node(&self, scope: Scope) -> usize {
        scope.map_or(0, |group| self.groups[group].node)
    }

    /// A basic graph pattern: one table per triple pattern. `None` when it
    /// can never match.
    fn bgp(
        &mut self,
        chosen: &[(&'a TriplePattern, Candidate<'a>)],
        scope: Scope,
        mapping: &'a Mapping,
    ) -> Option<Env<'a>> {
        let mut env = Vec::new();
        for (i, &(pattern, candidate)) in chosen.iter().enumerate() {
            if is_implied_class(chosen, i) {
                continue;
            }
            if !self.add(pattern, candidate, scope, mapping, &mut env) {
                return None;
            }
        }
        Some(env)
    }

    /// `false` when the pattern can never match.
    fn add(
        &mut self,
        pattern: &'a TriplePattern,
        candidate: Candidate<'a>,
        scope: Scope,
        mapping: &'a Mapping,
        env: &mut Env<'a>,
    ) -> bool {
        let tm = candidate.triples_map();
        let table = self.push_table(&tm.logical_table);
        let base = mapping.base_iri();
        let subject = TermExpr::from_term_map(&tm.subject_map.term_map, table, base);
        let object = match candidate {
            Candidate::Class(_) => None,
            Candidate::Property {
                object_map: ObjectMap::Term(term_map),
                ..
            } => Some(TermExpr::from_term_map(term_map, table, base)),
            Candidate::Property {
                object_map: ObjectMap::Reference(reference),
                ..
            } => {
                let parent = mapping
                    .triples_map(&reference.parent_triples_map)
                    .expect("parent triples maps are checked when the mapping is parsed");
                // R2RML §8: without join conditions the joint SQL query is
                // the child query alone, so the parent subject comes from the
                // same row. Parsing checks both use the same logical table.
                let parent_table = if reference.join_conditions.is_empty() {
                    table
                } else {
                    self.push_table(&parent.logical_table)
                };
                for join in &reference.join_conditions {
                    self.conditions_mut(scope).push(Condition::Eq(
                        column(parent_table, &join.parent),
                        column(table, &join.child),
                    ));
                }
                Some(TermExpr::from_term_map(
                    &parent.subject_map.term_map,
                    parent_table,
                    base,
                ))
            }
        };
        if !self.bind(&pattern.subject, subject, scope, env) {
            return false;
        }
        match object {
            Some(object) => self.bind(&pattern.object, object, scope, env),
            None => true,
        }
    }

    fn push_table(&mut self, table: &LogicalTable) -> usize {
        self.from.push(table.clone());
        self.from.len() - 1
    }

    /// `false` when the term can never be what `pattern` requires.
    fn bind(
        &mut self,
        pattern: &'a PatternTerm,
        term: TermExpr<'a>,
        scope: Scope,
        env: &mut Env<'a>,
    ) -> bool {
        self.terms.push((term, scope));
        let other = match pattern {
            PatternTerm::Variable(name) => match env.iter().find(|(v, _)| *v == name.as_str()) {
                Some((_, sources)) => sources[0].term,
                None => {
                    env.push((name, vec![Source { term, scope }]));
                    return true;
                }
            },
            PatternTerm::Constant(constant) => TermExpr::constant(constant),
        };
        match term_eq(&term, &other) {
            Equality::Never => false,
            Equality::When(conditions) => {
                self.conditions_mut(scope).extend(conditions);
                true
            }
        }
    }

    /// R2RML §11.1: a row whose subject or object columns are NULL yields
    /// no triple. Columns already compared with `=` cannot be NULL in a
    /// matching row, so they need no extra condition. Each scope checks
    /// its own terms in its own conditions, so a part that does not match
    /// leaves all of its columns `NULL`.
    fn add_not_null(&mut self) {
        let scopes: Vec<Scope> = std::iter::once(None)
            .chain((0..self.groups.len()).map(Some))
            .collect();
        for scope in scopes {
            let mut covered = HashSet::new();
            for condition in self.conditions(scope) {
                match condition {
                    Condition::Eq(left, right) => {
                        collect_columns(left, &mut covered);
                        collect_columns(right, &mut covered);
                    }
                    Condition::EqAny(left, right) => {
                        collect_columns(left, &mut covered);
                        right.iter().for_each(|e| collect_columns(e, &mut covered));
                    }
                    // Every rendering maps NULL to NULL, so `=` is not true.
                    Condition::SameTerm(left, right) => {
                        for (table, name) in left.columns().into_iter().chain(right.columns()) {
                            covered.insert((table, name.to_owned()));
                        }
                    }
                    Condition::IsNotNull(_)
                    | Condition::IsNull(_)
                    | Condition::Or(_)
                    | Condition::Filter(_) => {}
                }
            }
            let mut not_null = Vec::new();
            for (term, _) in self.terms.iter().filter(|(_, s)| *s == scope) {
                for (table, name) in term.columns() {
                    if covered.insert((table, name.to_owned())) {
                        not_null.push(Condition::IsNotNull(column(table, name)));
                    }
                }
            }
            self.conditions_mut(scope).extend(not_null);
        }
    }

    /// [`SqlBranch::fixed_columns`]: every term of the branch is bound to a
    /// variable, equal to one, or equal to a constant, so a solution fixes
    /// its value, and the columns too when the term map is injective. A
    /// term of an `OPTIONAL` part that did not match is unbound, and its
    /// columns are all `NULL`.
    fn fixed_columns(&self) -> Vec<BTreeSet<String>> {
        let mut fixed = vec![BTreeSet::new(); self.from.len()];
        for (term, _) in &self.terms {
            let injective = match term.value {
                TermValue::Constant(_) => continue,
                TermValue::Column { .. } => true,
                TermValue::Template { template, .. } => is_injective(template, term.term_type),
            };
            if injective {
                for (table, name) in term.columns() {
                    fixed[table].insert(name.to_owned());
                }
            }
        }
        fixed
    }

    /// How the results can rebuild `variable`: the source bound in every
    /// row, or else one source per `OPTIONAL` part that binds it, each with
    /// the number of its LeftJoin.
    fn alternatives(&self, variable: &str) -> Vec<(usize, Source<'a>)> {
        let Some((_, sources)) = self.env.iter().find(|(v, _)| *v == variable) else {
            return Vec::new();
        };
        bound_sources(sources, None)
            .into_iter()
            .map(|source| (self.node(source.scope), source))
            .collect()
    }
}

/// Takes the choices of `pattern` from `sides` and `chosen` without
/// walking it, so that every walk takes the same choices whatever a
/// branch gives.
fn skip(pattern: &Pattern, sides: &mut Sides<'_>, chosen: &mut Chosen<'_, '_>) {
    match pattern {
        Pattern::Bgp(patterns) => {
            for _ in patterns {
                chosen.next();
            }
        }
        Pattern::Join(left, right) => {
            skip(left, sides, chosen);
            skip(right, sides, chosen);
        }
        Pattern::LeftJoin(inner, _, _) | Pattern::Filter(inner, _) => skip(inner, sides, chosen),
        Pattern::Union(left, right) => {
            if sides.next().expect("one side per Union of the arm") {
                skip(right, sides, chosen);
            } else {
                skip(left, sides, chosen);
            }
        }
    }
}

fn check_count(count: usize) -> Result<(), Error> {
    if count > MAX_BRANCHES {
        return Err(Error::TooManyBranches {
            limit: MAX_BRANCHES,
        });
    }
    Ok(())
}

fn column(table: usize, name: &str) -> SqlExpr {
    SqlExpr::Column {
        table,
        name: name.to_owned(),
    }
}

fn collect_columns(expr: &SqlExpr, out: &mut HashSet<(usize, String)>) {
    match expr {
        SqlExpr::Column { table, name } | SqlExpr::Lexical { table, name, .. } => {
            out.insert((*table, name.clone()));
        }
        SqlExpr::Param(_)
        | SqlExpr::Null(_)
        | SqlExpr::Unbound
        | SqlExpr::Matched(_)
        | SqlExpr::True
        | SqlExpr::Term { .. } => {}
        SqlExpr::Concat(parts) => parts.iter().for_each(|p| collect_columns(p, out)),
    }
}

/// An RDF graph is a set, so `?s a :C` produced by triples map `M` holds
/// exactly once for every subject another pattern of the same basic graph
/// pattern already reads from `M`: the same row yields both triples.
/// Joining another scan of `M` would instead repeat the solution once per
/// row with that subject. Patterns of other parts do not count, since an
/// `OPTIONAL` part may not match.
fn is_implied_class(chosen: &[(&TriplePattern, Candidate<'_>)], i: usize) -> bool {
    let (pattern, Candidate::Class(tm)) = chosen[i] else {
        return false;
    };
    chosen.iter().enumerate().any(|(j, &(other, candidate))| {
        j != i
            && other.subject == pattern.subject
            && candidate.triples_map().id == tm.id
            && match candidate {
                Candidate::Property { .. } => true,
                Candidate::Class(_) => j < i,
            }
    })
}

/// The comparable shape of a projected term: everything but the columns'
/// table and names.
#[derive(PartialEq, Eq)]
struct Shape<'a> {
    term_type: TermType,
    datatype: Option<&'a str>,
    language: Option<&'a str>,
    value: ShapeValue<'a>,
}

#[derive(PartialEq, Eq)]
enum ShapeValue<'a> {
    Constant(&'a Term),
    Column,
    Template(Vec<Option<&'a str>>),
}

fn shape<'a>(term: &TermExpr<'a>) -> Shape<'a> {
    let value = match term.value {
        TermValue::Constant(c) => ShapeValue::Constant(c),
        TermValue::Column { .. } => ShapeValue::Column,
        TermValue::Template { template, .. } => ShapeValue::Template(
            template
                .parts()
                .iter()
                .map(|p| match p {
                    TemplatePart::Literal(text) => Some(text.as_str()),
                    TemplatePart::Column(_) => None,
                })
                .collect(),
        ),
    };
    Shape {
        term_type: term.term_type,
        datatype: term.datatype,
        language: term.language,
        value,
    }
}

/// Variables of the pattern that are not selected, in order of first
/// occurrence.
fn hidden_variables(query: &SelectQuery) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for pattern in query.pattern.triple_patterns() {
        for term in [&pattern.subject, &pattern.object] {
            if let PatternTerm::Variable(name) = term
                && !query.variables.contains(name)
                && !out.contains(&name.as_str())
            {
                out.push(name);
            }
        }
    }
    out
}

/// One way the results rebuild a variable: from the required patterns
/// (`node` 0), or from the `OPTIONAL` part of LeftJoin `node`, in the
/// branches whose term there has one of the shapes of the alternative.
struct Alternative<'a> {
    node: usize,
    /// The source of each branch, `None` where the branch has no term of
    /// these shapes: the part cannot match there, the branch's `UNION`
    /// side does not bind the variable, or its term has another shape.
    sources: Vec<Option<Source<'a>>>,
    /// Several shapes whose terms can be the same: the one result column
    /// holds the term's string, computed in SQL, so that removing
    /// duplicates compares terms.
    computed: bool,
}

impl<'a> Alternative<'a> {
    fn reference(&self) -> &TermExpr<'a> {
        &self
            .sources
            .iter()
            .flatten()
            .next()
            .expect("an alternative has a source in some branch")
            .term
    }

    /// A constant has no column to tell whether it is bound in a row, so
    /// where some rows do not bind it, it is selected with a flag that
    /// does: in an `OPTIONAL` part, or when some branches have no such
    /// term.
    fn flagged(&self) -> bool {
        !self.computed
            && self.reference().columns().is_empty()
            && (self.node != 0 || self.sources.iter().any(Option::is_none))
    }

    fn width(&self) -> usize {
        if self.flagged() || self.computed {
            1
        } else {
            self.reference().columns().len()
        }
    }
}

/// The alternatives for `variable`, with `per_branch` its sources in each
/// branch, by node and then by shape.
///
/// Each branch rebuilds the variable from its own term map, and terms of
/// different shapes take separate result columns, which the other
/// branches leave `NULL`. Removing duplicates compares columns, which is
/// exact when the shapes compared with each other can never produce the
/// same term. Shapes that can are merged into one alternative whose column
/// holds the term's string: those of branches of one `UNION` arm, whose
/// duplicates are removed (Discussion #41), or of any branch under
/// `DISTINCT`.
fn alternatives_of<'a>(
    variable: &str,
    per_branch: &[Vec<(usize, Source<'a>)>],
    arms: &[usize],
    distinct: bool,
) -> Result<Vec<Alternative<'a>>, Error> {
    let mut nodes: Vec<usize> = per_branch.iter().flatten().map(|&(n, _)| n).collect();
    nodes.sort_unstable();
    nodes.dedup();
    let mut out = Vec::new();
    for node in nodes {
        let sources: Vec<Option<Source<'a>>> = per_branch
            .iter()
            .map(|a| a.iter().find(|&&(n, _)| n == node).map(|&(_, s)| s))
            .collect();
        // Distinct shapes in order of first branch, and each branch's.
        let mut shapes: Vec<TermExpr<'a>> = Vec::new();
        let shape_of: Vec<Option<usize>> = sources
            .iter()
            .map(|source| {
                source.map(|s| {
                    shapes
                        .iter()
                        .position(|t| shape(t) == shape(&s.term))
                        .unwrap_or_else(|| {
                            shapes.push(s.term);
                            shapes.len() - 1
                        })
                })
            })
            .collect();
        let compared = |i: usize, j: usize| {
            distinct
                || (0..sources.len()).any(|a| {
                    shape_of[a] == Some(i)
                        && (0..sources.len()).any(|b| shape_of[b] == Some(j) && arms[a] == arms[b])
                })
        };
        // Classes of shapes that must be merged, as a union-find forest.
        let mut class: Vec<usize> = (0..shapes.len()).collect();
        fn root(class: &mut [usize], mut i: usize) -> usize {
            while class[i] != i {
                i = class[i];
            }
            i
        }
        for i in 0..shapes.len() {
            for j in i + 1..shapes.len() {
                if term_eq(&shapes[i], &shapes[j]) != Equality::Never && compared(i, j) {
                    let (a, b) = (root(&mut class, i), root(&mut class, j));
                    class[a.max(b)] = a.min(b);
                }
            }
        }
        let mut roots: Vec<usize> = Vec::new();
        for i in 0..shapes.len() {
            let r = root(&mut class, i);
            if !roots.contains(&r) {
                roots.push(r);
            }
        }
        for r in roots {
            let members: Vec<usize> = (0..shapes.len())
                .filter(|&i| root(&mut class, i) == r)
                .collect();
            if members.len() > 1 {
                check_computable(variable, members.iter().map(|&i| &shapes[i]))?;
            }
            out.push(Alternative {
                node,
                sources: sources
                    .iter()
                    .zip(&shape_of)
                    .map(|(&s, shape)| s.filter(|_| shape.is_some_and(|i| members.contains(&i))))
                    .collect(),
                computed: members.len() > 1,
            });
        }
    }
    Ok(out)
}

/// Whether terms of these shapes, which can be the same, can be compared
/// as strings computed in SQL: IRIs and blank nodes are their strings, and
/// literals of one fixed datatype or language tag their lexical forms. A
/// literal of a column's natural datatype (R2RML §10.2) cannot: the
/// datatype depends on the SQL type, so strings alone could merge
/// different terms.
fn check_computable<'t, 'a: 't>(
    variable: &str,
    mut shapes: impl Iterator<Item = &'t TermExpr<'a>>,
) -> Result<(), Error> {
    let first = shapes.next().expect("at least two shapes");
    if first.term_type != TermType::Literal {
        return Ok(());
    }
    let reference = literal_type(first);
    if reference != LiteralType::Natural && shapes.all(|s| literal_type(s) == reference) {
        return Ok(());
    }
    Err(Error::UnsupportedFeature(format!(
        "?{variable} takes literals from differently shaped term maps whose terms can be the \
         same, one of them of a column's natural datatype: removing duplicate solutions would \
         have to compare their datatypes in SQL"
    )))
}

fn plan(
    query: &SelectQuery,
    mut branches: Vec<Branch<'_>>,
    base_iri: Option<&BaseIri>,
) -> Result<SqlPlan, Error> {
    if query.distinct {
        check_distinct_order(query)?;
    }
    // Number the arms in order of first branch, and put the branches of
    // each arm together.
    let mut ids: Vec<Vec<usize>> = Vec::new();
    for branch in &branches {
        if !ids.contains(&branch.arm) {
            ids.push(branch.arm.clone());
        }
    }
    let arm_id = |arm: &Vec<usize>| {
        ids.iter()
            .position(|id| id == arm)
            .expect("every arm is numbered")
    };
    branches.sort_by_key(|b| arm_id(&b.arm));
    let arms: Vec<usize> = branches.iter().map(|b| arm_id(&b.arm)).collect();

    if branches.is_empty() {
        return Ok(SqlPlan {
            columns: Vec::new(),
            hidden_columns: Vec::new(),
            distinct_order: Vec::new(),
            split_columns: Vec::new(),
            lexical_hidden_columns: Default::default(),
            projection: query
                .variables
                .iter()
                .map(|v| ProjectedTerm {
                    variable: v.clone(),
                    terms: Vec::new(),
                })
                .collect(),
            branches: Vec::new(),
            distinct: query.distinct,
            base_iri: base_iri.cloned(),
            order: order_keys(query),
            offset: query.offset,
            limit: query.limit,
        });
    }

    let hidden = if query.distinct {
        Vec::new()
    } else {
        hidden_variables(query)
    };
    let variables: Vec<&str> = query
        .variables
        .iter()
        .map(String::as_str)
        .chain(hidden.iter().copied())
        .collect();

    // The result rows do not say which branch they came from, so each
    // alternative is built the same way in every branch that has it.
    let mut alternatives: Vec<Vec<Alternative<'_>>> = Vec::with_capacity(variables.len());
    for &variable in &variables {
        let per_branch: Vec<Vec<(usize, Source<'_>)>> =
            branches.iter().map(|b| b.alternatives(variable)).collect();
        let list = alternatives_of(variable, &per_branch, &arms, query.distinct)?;
        if query.distinct {
            check_distinct(variable, &list)?;
        }
        alternatives.push(list);
    }

    let reserved: HashSet<&str> = variables.iter().copied().collect();
    let mut used: HashSet<String> = HashSet::new();
    let mut aliases: Vec<Vec<Vec<String>>> = Vec::with_capacity(variables.len());
    for (&variable, list) in variables.iter().zip(&alternatives) {
        let mut slot = 0;
        let mut per_alternative = Vec::with_capacity(list.len());
        for alternative in list {
            let names: Vec<String> = (0..alternative.width())
                .map(|_| {
                    let alias = column_alias(variable, slot, &reserved, &used);
                    used.insert(alias.clone());
                    slot += 1;
                    alias
                })
                .collect();
            per_alternative.push(names);
        }
        aliases.push(per_alternative);
    }

    let projected = query.variables.len();
    let mut columns = Vec::new();
    let mut projection = Vec::with_capacity(projected);
    for ((variable, list), names) in query.variables.iter().zip(&alternatives).zip(&aliases) {
        let mut terms = Vec::with_capacity(list.len());
        for (alternative, names) in list.iter().zip(names) {
            columns.extend(names.iter().cloned());
            terms.push(result_term(alternative, names));
        }
        projection.push(ProjectedTerm {
            variable: variable.clone(),
            terms,
        });
    }

    let mut hidden_columns = Vec::new();
    let mut lexical_hidden_columns = BTreeSet::new();
    for (list, names) in alternatives[projected..].iter().zip(&aliases[projected..]) {
        for (alternative, names) in list.iter().zip(names) {
            let term = alternative.reference();
            let natural = !alternative.computed
                && term.term_type == TermType::Literal
                && matches!(term.value, TermValue::Column { .. })
                && term.datatype.is_none()
                && term.language.is_none();
            for alias in names {
                if !natural {
                    lexical_hidden_columns.insert(alias.clone());
                }
                hidden_columns.push(alias.clone());
            }
        }
    }

    // Select entries of each variable, then the variables in order of
    // first occurrence in the pattern.
    let mut entries: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut next = 0;
    for (&variable, list) in variables.iter().zip(&alternatives) {
        let width: usize = list.iter().map(Alternative::width).sum();
        entries.insert(variable, (next..next + width).collect());
        next += width;
    }
    let mut distinct_order = Vec::new();
    for pattern in query.pattern.triple_patterns() {
        for term in [&pattern.subject, &pattern.object] {
            if let PatternTerm::Variable(name) = term
                && let Some(slots) = entries.remove(name.as_str())
            {
                distinct_order.extend(slots);
            }
        }
    }

    let mut sql_branches = Vec::with_capacity(branches.len());
    for (b, branch) in branches.iter().enumerate() {
        let mut select = Vec::new();
        for alternative in alternatives.iter().flatten() {
            match (alternative.sources[b], alternative.flagged()) {
                (Some(source), _) if alternative.computed => {
                    // A constant is a string in every row, also where its
                    // `OPTIONAL` part did not match.
                    let guard = match (source.term.columns().is_empty(), source.scope) {
                        (true, Some(_)) => {
                            let (table, name) = branch.witness(source.scope)?;
                            Some(Box::new(column(table, name)))
                        }
                        _ => None,
                    };
                    select.push(SqlExpr::Term {
                        term: source.term.to_term_sql(),
                        guard,
                    });
                }
                (Some(source), true) => match source.scope {
                    None => select.push(SqlExpr::True),
                    Some(_) => {
                        let (table, name) = branch.witness(source.scope)?;
                        select.push(SqlExpr::Matched(Box::new(column(table, name))));
                    }
                },
                (Some(source), false) => {
                    select.extend(
                        source
                            .term
                            .columns()
                            .into_iter()
                            .map(|(table, name)| column(table, name)),
                    );
                }
                (None, true) => select.push(SqlExpr::Null(BOOLEAN.to_owned())),
                (None, false) => {
                    select.extend(std::iter::repeat_n(SqlExpr::Unbound, alternative.width()));
                }
            }
        }
        sql_branches.push(SqlBranch {
            arm: arms[b],
            from: branch.from.clone(),
            select,
            conditions: branch.conditions.clone(),
            left_joins: branch
                .groups
                .iter()
                .map(|g| LeftJoin {
                    tables: g.tables.clone(),
                    parent: g.parent,
                    conditions: g.conditions.clone(),
                })
                .collect(),
            fixed_columns: branch.fixed_columns(),
            order: query
                .order_by
                .iter()
                .map(|c| branch.order_value(&c.expression))
                .collect::<Result<_, _>>()?,
        });
    }

    Ok(SqlPlan {
        columns,
        hidden_columns,
        lexical_hidden_columns,
        distinct_order,
        projection,
        split_columns: Vec::new(),
        branches: sql_branches,
        distinct: query.distinct,
        base_iri: base_iri.cloned(),
        order: order_keys(query),
        offset: query.offset,
        limit: query.limit,
    })
}

fn order_keys(query: &SelectQuery) -> Vec<OrderKey> {
    query
        .order_by
        .iter()
        .map(|c| OrderKey {
            descending: c.descending,
        })
        .collect()
}

/// `ORDER BY` applies before projection and `DISTINCT` after it (SPARQL
/// 1.1 §18.2.5), and Distinct must keep the order OrderBy gives (§18.5).
/// A solution of `SELECT DISTINCT` stands for all those it projects, which
/// a comparator over a variable it does not project can put at different
/// places, so such a comparator does not order them.
fn check_distinct_order(query: &SelectQuery) -> Result<(), Error> {
    for condition in &query.order_by {
        if let Some(variable) = condition
            .expression
            .variables()
            .into_iter()
            .find(|v| !query.variables.iter().any(|p| p == v))
        {
            return Err(Error::UnsupportedFeature(format!(
                "ORDER BY over ?{variable}, which SELECT DISTINCT does not project"
            )));
        }
    }
    Ok(())
}

/// What `SELECT DISTINCT` cannot compare by column values.
fn check_distinct(variable: &str, alternatives: &[Alternative<'_>]) -> Result<(), Error> {
    // The same term from two `OPTIONAL` parts sits in different columns.
    if alternatives.iter().any(|a| a.node != alternatives[0].node) {
        return Err(Error::UnsupportedFeature(format!(
            "DISTINCT over ?{variable}, which more than one OPTIONAL part binds"
        )));
    }
    // A computed term is compared as the term's string.
    for alternative in alternatives.iter().filter(|a| !a.computed) {
        let term = alternative.reference();
        if let TermValue::Template { template, .. } = term.value
            && !is_injective(template, term.term_type)
        {
            return Err(Error::UnsupportedFeature(format!(
                "DISTINCT over ?{variable}, whose template {template} can render the same \
                 string for different column values"
            )));
        }
        // Duplicates are removed by column value, and a relative value and
        // the absolute IRI it resolves to are different values of one term.
        if term.base_iri.is_some() {
            return Err(Error::UnsupportedFeature(format!(
                "DISTINCT over ?{variable}, whose IRIs are resolved against the base IRI per \
                 value: a relative value and the absolute IRI it resolves to would count as \
                 two solutions"
            )));
        }
    }
    Ok(())
}

/// How to rebuild the term of `alternative` from its result columns.
fn result_term(alternative: &Alternative<'_>, names: &[String]) -> ResultTerm {
    let term = alternative.reference();
    let mut aliases = names.iter().cloned();
    if alternative.computed {
        // The string of an IRI or blank node, or the lexical form of a
        // literal of the datatype or language tag all its shapes share.
        let (datatype, language) = match literal_type(term) {
            _ if term.term_type != TermType::Literal => (None, None),
            LiteralType::Language(language) => (None, Some(language.to_owned())),
            LiteralType::Datatype(datatype) if datatype != XSD_STRING => {
                (Some(datatype.to_owned()), None)
            }
            _ => (None, None),
        };
        return ResultTerm {
            term_type: term.term_type,
            value: ResultValue::Column(aliases.next().unwrap_or_default()),
            datatype,
            language,
            guard: None,
        };
    }
    let (value, guard) = match term.value {
        TermValue::Constant(c) => (
            ResultValue::Constant(c.clone()),
            alternative
                .flagged()
                .then(|| aliases.next().unwrap_or_default()),
        ),
        TermValue::Column { .. } => (
            ResultValue::Column(aliases.next().unwrap_or_default()),
            None,
        ),
        TermValue::Template { template, .. } => (
            ResultValue::Template(
                template
                    .parts()
                    .iter()
                    .map(|p| match p {
                        TemplatePart::Literal(text) => TemplatePart::Literal(text.clone()),
                        TemplatePart::Column(_) => {
                            TemplatePart::Column(aliases.next().unwrap_or_default())
                        }
                    })
                    .collect(),
            ),
            None,
        ),
    };
    ResultTerm {
        term_type: term.term_type,
        value,
        datatype: term.datatype.map(str::to_owned),
        language: term.language.map(str::to_owned),
        guard,
    }
}

/// The variable name for its first column, `name_1`, `name_2`, ... for the
/// others, skipping names already taken.
fn column_alias(
    variable: &str,
    slot: usize,
    reserved: &HashSet<&str>,
    used: &HashSet<String>,
) -> String {
    if slot == 0 && !used.contains(variable) {
        return variable.to_owned();
    }
    (slot.max(1)..)
        .map(|n| format!("{variable}_{n}"))
        .find(|alias| !reserved.contains(alias.as_str()) && !used.contains(alias))
        .expect("an unused alias exists")
}

#[cfg(test)]
mod tests;
