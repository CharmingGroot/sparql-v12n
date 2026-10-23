// SPDX-License-Identifier: Apache-2.0
//! Unfolding: triple patterns to SQL fragments, shared variables to joins.

mod term;

use std::collections::{BTreeSet, HashMap, HashSet};

use oxrdf::Term;

use crate::Error;
use crate::mapping::{
    LogicalTable, Mapping, MappingIndex, ObjectMap, TemplatePart, TermType, TriplesMap,
};
use crate::sparql::{PatternTerm, RDF_TYPE, SelectQuery, TriplePattern};
use crate::sql::{Condition, ProjectedTerm, ResultTerm, ResultValue, SqlBranch, SqlExpr, SqlPlan};
use term::{Equality, TermExpr, TermValue, is_injective, term_eq};

/// Most UNION branches one query may unfold into. Each pattern with
/// several candidate triples maps multiplies the count.
pub const MAX_BRANCHES: usize = 64;

/// Unfolds a query into a SQL plan over the mapping's logical tables.
pub fn unfold(
    query: &SelectQuery,
    mapping: &Mapping,
    index: &MappingIndex,
) -> Result<SqlPlan, Error> {
    let candidates = query
        .patterns
        .iter()
        .map(|p| candidates(p, mapping, index))
        .collect::<Result<Vec<_>, _>>()?;

    let mut branches = Vec::new();
    for choice in combinations(&candidates)? {
        let chosen: Vec<_> = query
            .patterns
            .iter()
            .zip(&candidates)
            .zip(choice)
            .map(|((pattern, list), c)| (pattern, list[c]))
            .collect();
        if let Some(branch) = Branch::build(&chosen, mapping) {
            branches.push(branch);
        }
    }
    plan(query, branches)
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

/// A branch under construction: one candidate per pattern, joined.
struct Branch<'a> {
    from: Vec<LogicalTable>,
    conditions: Vec<Condition>,
    /// First term each variable is bound to; later occurrences are
    /// compared with it.
    bindings: HashMap<&'a str, TermExpr<'a>>,
    /// Every term the branch produces, in pattern order.
    terms: Vec<TermExpr<'a>>,
    never: bool,
}

impl<'a> Branch<'a> {
    /// `None` when the branch can never produce a row.
    fn build(chosen: &[(&'a TriplePattern, Candidate<'a>)], mapping: &'a Mapping) -> Option<Self> {
        let mut branch = Self {
            from: Vec::new(),
            conditions: Vec::new(),
            bindings: HashMap::new(),
            terms: Vec::new(),
            never: false,
        };
        for (i, &(pattern, candidate)) in chosen.iter().enumerate() {
            if is_implied_class(chosen, i) {
                continue;
            }
            branch.add(pattern, candidate, mapping);
            if branch.never {
                return None;
            }
        }
        branch.add_not_null();
        Some(branch)
    }

    fn add(&mut self, pattern: &'a TriplePattern, candidate: Candidate<'a>, mapping: &'a Mapping) {
        let tm = candidate.triples_map();
        let table = self.push_table(&tm.logical_table);
        let subject = TermExpr::from_term_map(&tm.subject_map.term_map, table);
        let object = match candidate {
            Candidate::Class(_) => None,
            Candidate::Property {
                object_map: ObjectMap::Term(term_map),
                ..
            } => Some(TermExpr::from_term_map(term_map, table)),
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
                    self.conditions.push(Condition::Eq(
                        SqlExpr::Column {
                            table: parent_table,
                            name: join.parent.clone(),
                        },
                        SqlExpr::Column {
                            table,
                            name: join.child.clone(),
                        },
                    ));
                }
                Some(TermExpr::from_term_map(
                    &parent.subject_map.term_map,
                    parent_table,
                ))
            }
        };
        self.bind(&pattern.subject, subject);
        if let Some(object) = object {
            self.bind(&pattern.object, object);
        }
    }

    fn push_table(&mut self, table: &LogicalTable) -> usize {
        self.from.push(table.clone());
        self.from.len() - 1
    }

    fn bind(&mut self, pattern: &'a PatternTerm, term: TermExpr<'a>) {
        self.terms.push(term);
        let other = match pattern {
            PatternTerm::Variable(name) => match self.bindings.get(name.as_str()) {
                Some(first) => *first,
                None => {
                    self.bindings.insert(name, term);
                    return;
                }
            },
            PatternTerm::Constant(constant) => TermExpr::constant(constant),
        };
        match term_eq(&term, &other) {
            Equality::Never => self.never = true,
            Equality::When(conditions) => self.conditions.extend(conditions),
        }
    }

    /// R2RML §11.1: a row whose subject or object columns are NULL yields
    /// no triple. Columns already compared with `=` cannot be NULL in a
    /// matching row, so they need no extra condition.
    fn add_not_null(&mut self) {
        let mut covered = HashSet::new();
        for condition in &self.conditions {
            match condition {
                Condition::Eq(left, right) => {
                    collect_columns(left, &mut covered);
                    collect_columns(right, &mut covered);
                }
                // Every rendering maps NULL to NULL, so `=` is not true.
                Condition::SameTerm(left, right) => {
                    for (table, name) in left.columns().into_iter().chain(right.columns()) {
                        covered.insert((table, name.to_owned()));
                    }
                }
                Condition::IsNotNull(_) => {}
            }
        }
        for term in &self.terms {
            for (table, name) in term.columns() {
                if covered.insert((table, name.to_owned())) {
                    self.conditions.push(Condition::IsNotNull(SqlExpr::Column {
                        table,
                        name: name.to_owned(),
                    }));
                }
            }
        }
    }
}

impl Branch<'_> {
    /// [`SqlBranch::fixed_columns`]: every term of the branch is bound to a
    /// variable, equal to one, or equal to a constant, so a solution fixes
    /// its value, and the columns too when the term map is injective.
    fn fixed_columns(&self) -> Vec<BTreeSet<String>> {
        let mut fixed = vec![BTreeSet::new(); self.from.len()];
        for term in &self.terms {
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
}

fn collect_columns(expr: &SqlExpr, out: &mut HashSet<(usize, String)>) {
    match expr {
        SqlExpr::Column { table, name } => {
            out.insert((*table, name.clone()));
        }
        SqlExpr::Param(_) => {}
        SqlExpr::Concat(parts) => parts.iter().for_each(|p| collect_columns(p, out)),
    }
}

/// An RDF graph is a set, so `?s a :C` produced by triples map `M` holds
/// exactly once for every subject another pattern of the branch already
/// reads from `M`: the same row yields both triples. Joining another scan
/// of `M` would instead repeat the solution once per row with that subject.
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
    for pattern in &query.patterns {
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

fn plan(query: &SelectQuery, branches: Vec<Branch<'_>>) -> Result<SqlPlan, Error> {
    let Some(first) = branches.first() else {
        return Ok(SqlPlan {
            columns: Vec::new(),
            hidden_columns: Vec::new(),
            distinct_order: Vec::new(),
            projection: query
                .variables
                .iter()
                .map(|v| ProjectedTerm {
                    variable: v.clone(),
                    term: None,
                })
                .collect(),
            branches: Vec::new(),
            distinct: query.distinct,
        });
    };

    // Every branch must rebuild a variable the same way, since the result
    // rows do not say which branch they came from. Removing duplicate
    // solutions compares the columns of every variable across branches,
    // which is only sound when they are built the same way too.
    let hidden = if query.distinct {
        Vec::new()
    } else {
        hidden_variables(query)
    };
    for variable in query
        .variables
        .iter()
        .map(String::as_str)
        .chain(hidden.iter().copied())
    {
        let reference = first.bindings.get(variable).map(shape);
        for branch in &branches[1..] {
            if branch.bindings.get(variable).map(shape) != reference {
                return Err(Error::UnsupportedFeature(format!(
                    "?{variable} takes values from differently shaped term maps in different \
                     UNION branches"
                )));
            }
        }
        if let Some(TermExpr {
            value: TermValue::Template { template, .. },
            term_type,
            ..
        }) = first.bindings.get(variable)
            && query.distinct
            && !is_injective(template, *term_type)
        {
            return Err(Error::UnsupportedFeature(format!(
                "DISTINCT over ?{variable}, whose template {template} can render the same \
                 string for different column values"
            )));
        }
    }

    let reserved: HashSet<&str> = query
        .variables
        .iter()
        .map(String::as_str)
        .chain(hidden.iter().copied())
        .collect();
    let mut used: HashSet<String> = HashSet::new();
    let mut columns = Vec::new();
    let mut projection = Vec::new();
    for variable in &query.variables {
        let Some(term) = first.bindings.get(variable.as_str()) else {
            projection.push(ProjectedTerm {
                variable: variable.clone(),
                term: None,
            });
            continue;
        };
        let aliases: Vec<String> = (0..term.columns().len())
            .map(|slot| {
                let alias = column_alias(variable, slot, &reserved, &used);
                used.insert(alias.clone());
                alias
            })
            .collect();
        columns.extend(aliases.iter().cloned());
        let mut aliases = aliases.into_iter();
        let value = match term.value {
            TermValue::Constant(c) => ResultValue::Constant(c.clone()),
            TermValue::Column { .. } => ResultValue::Column(aliases.next().unwrap_or_default()),
            TermValue::Template { template, .. } => ResultValue::Template(
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
        };
        projection.push(ProjectedTerm {
            variable: variable.clone(),
            term: Some(ResultTerm {
                term_type: term.term_type,
                value,
                datatype: term.datatype.map(str::to_owned),
                language: term.language.map(str::to_owned),
            }),
        });
    }

    let mut hidden_columns = Vec::new();
    for variable in &hidden {
        let term = first
            .bindings
            .get(variable)
            .expect("every variable of the pattern is bound in every branch");
        for slot in 0..term.columns().len() {
            let alias = column_alias(variable, slot, &reserved, &used);
            used.insert(alias.clone());
            hidden_columns.push(alias);
        }
    }

    // Select entries of each variable, then the variables in order of
    // first occurrence in the pattern.
    let mut entries: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut next = 0;
    for variable in query
        .variables
        .iter()
        .map(String::as_str)
        .chain(hidden.iter().copied())
    {
        let width = first
            .bindings
            .get(variable)
            .map_or(0, |t| t.columns().len());
        entries.insert(variable, (next..next + width).collect());
        next += width;
    }
    let mut distinct_order = Vec::new();
    for pattern in &query.patterns {
        for term in [&pattern.subject, &pattern.object] {
            if let PatternTerm::Variable(name) = term
                && let Some(slots) = entries.remove(name.as_str())
            {
                distinct_order.extend(slots);
            }
        }
    }

    let branches = branches
        .into_iter()
        .map(|branch| {
            let select = query
                .variables
                .iter()
                .map(String::as_str)
                .chain(hidden.iter().copied())
                .filter_map(|v| branch.bindings.get(v))
                .flat_map(|term| term.columns())
                .map(|(table, name)| SqlExpr::Column {
                    table,
                    name: name.to_owned(),
                })
                .collect();
            let fixed_columns = branch.fixed_columns();
            SqlBranch {
                from: branch.from,
                select,
                conditions: branch.conditions,
                fixed_columns,
            }
        })
        .collect();

    Ok(SqlPlan {
        columns,
        hidden_columns,
        distinct_order,
        projection,
        branches,
        distinct: query.distinct,
    })
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
