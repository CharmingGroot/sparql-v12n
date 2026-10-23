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
    Condition, LeftJoin, ProjectedTerm, ResultTerm, ResultValue, SqlBranch, SqlExpr, SqlPlan,
};
use term::{Equality, TermExpr, TermValue, is_injective, term_eq};

/// Most UNION branches one query may unfold into. Each pattern with
/// several candidate triples maps multiplies the count. The same limit
/// applies to the combinations tried for each `OPTIONAL` part.
pub const MAX_BRANCHES: usize = 64;

/// SQL type of the flag that tells whether an `OPTIONAL` part matched.
const BOOLEAN: &str = "\"pg_catalog\".\"bool\"";

/// Unfolds a query into a SQL plan over the mapping's logical tables.
///
/// Every choice of triples maps for the required triple patterns is one
/// UNION branch: Join and LeftJoin distribute over a union on their left
/// side (SPARQL 1.1 §18.5). The right side of a LeftJoin does not, so each
/// `OPTIONAL` part becomes a `LEFT JOIN` inside the branch.
pub fn unfold(
    query: &SelectQuery,
    mapping: &Mapping,
    index: &MappingIndex,
) -> Result<SqlPlan, Error> {
    let context = Context { mapping, index };
    let candidates = query
        .pattern
        .required_triple_patterns()
        .into_iter()
        .map(|p| candidates(p, mapping, index))
        .collect::<Result<Vec<_>, _>>()?;

    let mut branches = Vec::new();
    for choice in combinations(&candidates)? {
        let chosen: Vec<_> = candidates
            .iter()
            .zip(choice)
            .map(|(list, c)| list[c])
            .collect();
        if let Some(branch) = Branch::build(&query.pattern, &chosen, &context)? {
            branches.push(branch);
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
    /// `None` when the branch can never produce a row.
    fn build(
        pattern: &'a Pattern,
        chosen: &[Candidate<'a>],
        context: &Context<'a>,
    ) -> Result<Option<Self>, Error> {
        let mut branch = Self {
            from: Vec::new(),
            conditions: Vec::new(),
            groups: Vec::new(),
            terms: Vec::new(),
            env: Vec::new(),
        };
        let mut chosen = chosen.iter().copied();
        let Some(env) = branch.walk(pattern, None, 1, &mut chosen, context)? else {
            return Ok(None);
        };
        branch.env = env;
        branch.add_not_null();
        Ok(Some(branch))
    }

    /// Adds the tables and conditions of `pattern` to `scope`, taking the
    /// candidates of its required triple patterns from `chosen`. Its
    /// LeftJoins are numbered in the order they are written, starting at
    /// `node`; 0 stands for the required patterns of the query.
    ///
    /// Each part is translated on its own and then joined, as the algebra
    /// evaluates it (SPARQL 1.1 §18.6): the conditions linking two parts go
    /// to the node that joins them, never into a part.
    ///
    /// `None` when `pattern` has no solutions in `scope`.
    fn walk(
        &mut self,
        pattern: &'a Pattern,
        scope: Scope,
        node: usize,
        chosen: &mut impl Iterator<Item = Candidate<'a>>,
        context: &Context<'a>,
    ) -> Result<Option<Env<'a>>, Error> {
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
                Ok(self.bgp(&chosen, scope, context.mapping))
            }
            Pattern::Join(left, right) => {
                let Some(l) = self.walk(left, scope, node, chosen, context)? else {
                    return Ok(None);
                };
                let right_node = node + left.left_joins();
                let Some(r) = self.walk(right, scope, right_node, chosen, context)? else {
                    return Ok(None);
                };
                Ok(self
                    .join(l, r, scope, scope, scope)?
                    .map(|env| settle(env, scope)))
            }
            Pattern::LeftJoin(left, right, expression) => {
                let Some(l) = self.walk(left, scope, node, chosen, context)? else {
                    return Ok(None);
                };
                let node = node + left.left_joins();
                self.optional(l, right, expression.as_ref(), scope, node, context)
                    .map(Some)
            }
            // Filter(expr, P) keeps the solutions of P where expr holds, so
            // it sees the variables of P only (SPARQL 1.1 §18.2.2.6).
            Pattern::Filter(inner, expression) => {
                let Some(env) = self.walk(inner, scope, node, chosen, context)? else {
                    return Ok(None);
                };
                let filter = self.filter(expression, &env, &[scope])?;
                self.conditions_mut(scope).push(Condition::Filter(filter));
                Ok(Some(env))
            }
        }
    }

    /// LeftJoin(left, right, expression) (SPARQL 1.1 §18.5): every
    /// solution of the left side, extended by each compatible solution of
    /// `right` for which the expression holds over both, where there are
    /// some. The expression goes into the `ON` clause with the part's
    /// other conditions. The candidates of `right` are chosen here,
    /// because the right side of a LeftJoin does not distribute over a
    /// union: a combination that can never be compatible with the left
    /// side is left out, and more than one that can is not supported.
    fn optional(
        &mut self,
        left: Env<'a>,
        right: &'a Pattern,
        expression: Option<&'a Expression>,
        scope: Scope,
        node: usize,
        context: &Context<'a>,
    ) -> Result<Env<'a>, Error> {
        let candidates = right
            .required_triple_patterns()
            .into_iter()
            .map(|p| candidates(p, context.mapping, context.index))
            .collect::<Result<Vec<_>, _>>()?;
        let mut matched: Option<(Self, Env<'a>)> = None;
        for choice in combinations(&candidates)? {
            let mut attempt = self.clone();
            let group = attempt.groups.len();
            let start = attempt.from.len();
            attempt.groups.push(Group {
                node,
                parent: scope,
                tables: start..start,
                conditions: Vec::new(),
            });
            let mut chosen = candidates.iter().zip(choice).map(|(list, c)| list[c]);
            let Some(inner) = attempt.walk(right, Some(group), node + 1, &mut chosen, context)?
            else {
                continue;
            };
            attempt.groups[group].tables.end = attempt.from.len();
            if attempt.groups[group].tables.is_empty() {
                // No triple pattern left: the part's one solution binds
                // nothing and extends the left side unchanged.
                continue;
            }
            let inner = settle(inner, Some(group));
            let Some(env) = attempt.join(left.clone(), inner, scope, Some(group), Some(group))?
            else {
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
            if matched.is_some() {
                return Err(Error::UnsupportedFeature(
                    "OPTIONAL over triple patterns that more than one combination of triples \
                     maps can match, which needs a UNION inside the OPTIONAL"
                        .to_owned(),
                ));
            }
            matched = Some((attempt, env));
        }
        Ok(match matched {
            Some((attempt, env)) => {
                *self = attempt;
                env
            }
            // Nothing can extend the left side, whose solutions stay as
            // they are.
            None => left,
        })
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
        SqlExpr::Param(_) | SqlExpr::Null(_) | SqlExpr::Unbound | SqlExpr::Matched(_) => {}
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
/// (`node` 0), or from the `OPTIONAL` part of LeftJoin `node`, whose
/// source each branch has unless the part can never match there.
struct Alternative<'a> {
    node: usize,
    sources: Vec<Option<Source<'a>>>,
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

    /// A constant from an `OPTIONAL` part has no column to tell whether
    /// the part matched, so it is selected with a flag that does.
    fn flagged(&self) -> bool {
        self.node != 0 && self.reference().columns().is_empty()
    }

    fn width(&self) -> usize {
        if self.flagged() {
            1
        } else {
            self.reference().columns().len()
        }
    }
}

fn plan(
    query: &SelectQuery,
    branches: Vec<Branch<'_>>,
    base_iri: Option<&BaseIri>,
) -> Result<SqlPlan, Error> {
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

    // Every branch must rebuild a variable the same way, since the result
    // rows do not say which branch they came from. Removing duplicate
    // solutions compares the columns of every variable across branches,
    // which is only sound when they are built the same way too.
    let mut alternatives: Vec<Vec<Alternative<'_>>> = Vec::with_capacity(variables.len());
    for &variable in &variables {
        let per_branch: Vec<Vec<(usize, Source<'_>)>> =
            branches.iter().map(|b| b.alternatives(variable)).collect();
        let mut nodes: Vec<usize> = per_branch.iter().flatten().map(|&(n, _)| n).collect();
        nodes.sort_unstable();
        nodes.dedup();
        let mut list = Vec::with_capacity(nodes.len());
        for node in nodes {
            let alternative = Alternative {
                node,
                sources: per_branch
                    .iter()
                    .map(|a| a.iter().find(|&&(n, _)| n == node).map(|&(_, s)| s))
                    .collect(),
            };
            let reference = shape(alternative.reference());
            if alternative
                .sources
                .iter()
                .flatten()
                .any(|s| shape(&s.term) != reference)
            {
                return Err(Error::UnsupportedFeature(format!(
                    "?{variable} takes values from differently shaped term maps in different \
                     UNION branches"
                )));
            }
            list.push(alternative);
        }
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
            let natural = term.term_type == TermType::Literal
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
                (Some(source), true) => {
                    let (table, name) = branch.witness(source.scope)?;
                    select.push(SqlExpr::Matched(Box::new(column(table, name))));
                }
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
    })
}

/// What `SELECT DISTINCT` cannot compare by column values.
fn check_distinct(variable: &str, alternatives: &[Alternative<'_>]) -> Result<(), Error> {
    // The same term from two `OPTIONAL` parts sits in different columns.
    if alternatives.len() > 1 {
        return Err(Error::UnsupportedFeature(format!(
            "DISTINCT over ?{variable}, which more than one OPTIONAL part binds"
        )));
    }
    for alternative in alternatives {
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
