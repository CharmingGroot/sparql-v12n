// SPDX-License-Identifier: Apache-2.0
//! What a database adapter looks up before rendering a plan: the types,
//! keys and names of the columns it reads.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Range;

use crate::Error;
use crate::dialect::SqlType;
use crate::mapping::{LogicalTable, TermType};

use super::identifier::describe_table;
use super::plan::{Part, XSD_STRING, visit_cases};
use super::{
    ColumnType, ColumnTypes, Condition, Lexical, ResultValue, SqlBranch, SqlExpr, SqlPlan,
    TableKeys, folded_identifier, identifier_name,
};

impl SqlPlan {
    /// The branches of each arm (see [`SqlBranch::arm`]), in order.
    pub(super) fn arms(&self) -> Vec<Range<usize>> {
        let mut out: Vec<Range<usize>> = Vec::new();
        for (i, branch) in self.branches.iter().enumerate() {
            match out.last_mut() {
                Some(last) if self.branches[last.start].arm == branch.arm => last.end = i + 1,
                _ => out.push(i..i + 1),
            }
        }
        out
    }

    /// Whether two SQL rows may give the same solution of an arm of the
    /// pattern, so that rendering removes duplicates (see
    /// [`SqlBranch::arm`]). Never under `DISTINCT`, which already does.
    /// Otherwise true unless every arm has one branch and `keys` holds,
    /// for every table that branch reads, a unique key among the table's
    /// [`SqlBranch::fixed_columns`]: then each solution fixes one row of
    /// every table, and so one row of the result.
    pub fn has_duplicates(&self, keys: Option<&TableKeys>) -> bool {
        !self.distinct
            && self
                .arms()
                .into_iter()
                .any(|arm| arm_has_duplicates(&self.branches[arm], keys))
    }

    /// The tables whose keys can show that an arm has no duplicates
    /// ([`has_duplicates`](Self::has_duplicates)), each once. Empty when
    /// keys cannot help: under `DISTINCT`, or when no arm has one branch
    /// that reads tables only, without an `rr:sqlQuery`, which has no keys.
    pub fn key_tables(&self) -> Vec<LogicalTable> {
        let mut out: Vec<LogicalTable> = Vec::new();
        if self.distinct {
            return out;
        }
        for arm in self.arms() {
            let [branch] = &self.branches[arm] else {
                continue;
            };
            if branch
                .from
                .iter()
                .any(|t| matches!(t, LogicalTable::SqlQuery(_)))
            {
                continue;
            }
            for table in &branch.from {
                if !out.contains(table) {
                    out.push(table.clone());
                }
            }
        }
        out
    }

    /// The columns [`Condition::SameTerm`]s, [`SqlExpr::Term`]s, filters,
    /// order values and computed values read, each once, in order of
    /// appearance. Their types go into
    /// [`RenderOptions::column_types`].
    pub fn term_columns(&self) -> Vec<(LogicalTable, String)> {
        fn collect(
            condition: &Condition,
            from: &[LogicalTable],
            out: &mut Vec<(LogicalTable, String)>,
        ) {
            match condition {
                Condition::SameTerm(left, right) => {
                    for (table, name) in left.columns().into_iter().chain(right.columns()) {
                        let key = (from[table].clone(), name.to_owned());
                        if !out.contains(&key) {
                            out.push(key);
                        }
                    }
                }
                Condition::Or(alternatives) => alternatives
                    .iter()
                    .flatten()
                    .for_each(|c| collect(c, from, out)),
                Condition::Filter(filter) => {
                    let (conditions, terms) = filter.parts();
                    for c in conditions {
                        collect(c, from, out);
                    }
                    for (table, name) in terms.iter().flat_map(|t| t.columns()) {
                        let key = (from[table].clone(), name.to_owned());
                        if !out.contains(&key) {
                            out.push(key);
                        }
                    }
                }
                _ => {}
            }
        }
        let mut out: Vec<(LogicalTable, String)> = Vec::new();
        for branch in &self.branches {
            for expr in &branch.select {
                if let SqlExpr::Term { term, .. } = expr {
                    for (table, name) in term.columns() {
                        let key = (branch.from[table].clone(), name.to_owned());
                        if !out.contains(&key) {
                            out.push(key);
                        }
                    }
                }
            }
            for condition in branch.all_conditions() {
                collect(condition, &branch.from, &mut out);
            }
            // One list of parts per order value and per computed value.
            let mut values: Vec<Vec<Part<'_>>> = Vec::new();
            for value in &branch.order {
                let mut parts = Vec::new();
                value.visit(&mut |part| parts.push(part));
                values.push(parts);
            }
            for cases in &branch.values {
                let mut parts = Vec::new();
                visit_cases(cases, &mut |part| parts.push(part));
                values.push(parts);
            }
            for parts in values {
                let mut conditions = Vec::new();
                let mut terms = Vec::new();
                for part in parts {
                    match part {
                        Part::Condition(c) => conditions.push(c),
                        Part::Term(t) => terms.push(t),
                    }
                }
                for c in conditions {
                    collect(c, &branch.from, &mut out);
                }
                for (table, name) in terms.iter().flat_map(|t| t.columns()) {
                    let key = (branch.from[table].clone(), name.to_owned());
                    if !out.contains(&key) {
                        out.push(key);
                    }
                }
            }
        }
        out
    }

    /// The columns that branches select, each once, in order of
    /// appearance; empty with fewer than two branches. Their types go into
    /// [`with_union_types`](Self::with_union_types).
    pub fn union_columns(&self) -> Vec<(LogicalTable, String)> {
        if self.branches.len() < 2 {
            return Vec::new();
        }
        self.selected_columns()
    }

    /// The columns that branches select, each once, in order of
    /// appearance. Where duplicates are removed (under `DISTINCT`, or when
    /// [`has_duplicates`](Self::has_duplicates)), their types tell which
    /// need a [`TermKey`] next to them.
    pub fn selected_columns(&self) -> Vec<(LogicalTable, String)> {
        let mut out: Vec<(LogicalTable, String)> = Vec::new();
        for branch in &self.branches {
            for expr in &branch.select {
                if let SqlExpr::Column { table, name } = expr {
                    let key = (branch.from[*table].clone(), name.clone());
                    if !out.contains(&key) {
                        out.push(key);
                    }
                }
            }
        }
        out
    }

    /// The plan with result columns that `UNION` can combine, given the
    /// types of [`union_columns`](Self::union_columns).
    ///
    /// `UNION` matches columns by position and needs one type per result
    /// column, while a projected column may read columns of different SQL
    /// types in different branches. Where it does, and the types are not
    /// of one [`ColumnType::union_class`], each branch keeps its own type
    /// as long as no two of them can produce the same RDF term:
    ///
    /// - A literal of natural datatype (R2RML §10.2) is split into one
    ///   result column per natural datatype ([`SqlPlan::split_columns`]).
    ///   Terms of different datatypes differ, so removing duplicates stays
    ///   exact, and each value is read with its own type.
    /// - Columns whose terms can be the same, any other term or template
    ///   value, or a natural datatype shared by different classes (`text`
    ///   and `uuid` both give plain literals), are selected as their
    ///   natural lexical form, computed in SQL ([`Lexical`]), so equal
    ///   terms compare equal. A type without such SQL is an
    ///   [`Error::UnsupportedFeature`] instead of a guessed form.
    ///
    /// A branch where an `OPTIONAL` part cannot match selects
    /// [`SqlExpr::Unbound`] for its columns, which becomes a `NULL` of the
    /// type of the other branches' columns.
    ///
    /// Hidden columns are compared, never read, by their type and text
    /// output (see [`to_sql_with`](Self::to_sql_with)), which keeps terms
    /// of different types apart; there the lexical forms only replace
    /// columns whose terms can be the same, where SQL computes them.
    pub fn with_union_types(&self, types: &ColumnTypes) -> Result<Self, Error> {
        if self.branches.len() < 2 {
            return Ok(self.clone());
        }
        let mut owners: HashMap<&str, (&str, bool)> = HashMap::new();
        for projected in &self.projection {
            for term in &projected.terms {
                let natural = term.term_type == TermType::Literal
                    && matches!(term.value, ResultValue::Column(_))
                    && term.datatype.is_none()
                    && term.language.is_none();
                for alias in term.value.columns() {
                    owners.insert(alias, (&projected.variable, natural));
                }
            }
        }
        let mut used: HashSet<String> = self
            .columns
            .iter()
            .chain(&self.hidden_columns)
            .cloned()
            .collect();
        let mut plan = self.clone();
        plan.columns.clear();
        for branch in &mut plan.branches {
            branch.select.clear();
        }
        // Entries of the new select lists for each old one.
        let mut entries: Vec<Vec<usize>> = Vec::new();
        for (i, alias) in self.columns.iter().enumerate() {
            let start = plan.columns.len();
            let owner = owners.get(alias.as_str()).copied();
            for (alias, exprs) in self.union_column(i, alias, owner, false, types, &mut used)? {
                plan.columns.push(alias);
                for (branch, expr) in plan.branches.iter_mut().zip(exprs) {
                    branch.select.push(expr);
                }
            }
            if plan.columns.len() - start > 1 {
                plan.split_columns.push(plan.columns[start..].to_vec());
            }
            entries.push((start..plan.columns.len()).collect());
        }
        let width = plan.columns.len();
        for (h, alias) in self.hidden_columns.iter().enumerate() {
            let natural = !self.lexical_hidden_columns.contains(alias);
            let owner = Some((alias.as_str(), natural));
            let i = self.columns.len() + h;
            for (_, exprs) in self.union_column(i, alias, owner, true, types, &mut used)? {
                for (branch, expr) in plan.branches.iter_mut().zip(exprs) {
                    branch.select.push(expr);
                }
            }
        }
        entries.extend((0..self.hidden_columns.len()).map(|h| vec![width + h]));
        plan.distinct_order = self
            .distinct_order
            .iter()
            .flat_map(|&e| entries[e].iter().copied())
            .collect();
        Ok(plan)
    }

    /// The result columns for entry `i` of the select lists, each with
    /// one expression per branch; one column for a `hidden` entry.
    fn union_column(
        &self,
        i: usize,
        alias: &str,
        owner: Option<(&str, bool)>,
        hidden: bool,
        types: &ColumnTypes,
        used: &mut HashSet<String>,
    ) -> Result<Vec<(String, Vec<SqlExpr>)>, Error> {
        let exprs: Vec<&SqlExpr> = self.branches.iter().map(|b| &b.select[i]).collect();
        let unchanged = || vec![(alias.to_owned(), exprs.iter().map(|&e| e.clone()).collect())];
        let Some((variable, natural)) = owner else {
            return Ok(unchanged());
        };
        // `None` for a branch that selects `Unbound`.
        let mut columns: Vec<Option<(usize, &str, &ColumnType)>> = Vec::new();
        for (branch, expr) in self.branches.iter().zip(&exprs) {
            let (table, name) = match expr {
                SqlExpr::Column { table, name } => (*table, name),
                SqlExpr::Unbound => {
                    columns.push(None);
                    continue;
                }
                _ => return Ok(unchanged()),
            };
            let Some(ty) = types.get(&(branch.from[table].clone(), name.clone())) else {
                return Ok(unchanged());
            };
            columns.push(Some((table, name.as_str(), ty)));
        }
        let column = |b: usize| columns[b].expect("only branches with a column are members");
        let classes = |members: &[usize]| {
            let mut out: Vec<&str> = Vec::new();
            for &b in members {
                let class = column(b).2.union_class.as_str();
                if !out.contains(&class) {
                    out.push(class);
                }
            }
            out
        };
        let present: Vec<usize> = (0..columns.len())
            .filter(|&b| columns[b].is_some())
            .collect();
        let Some(&first) = present.first() else {
            return Ok(unchanged());
        };
        if classes(&present).len() < 2 {
            // `UNION` resolves the `NULL`s of two leading branches to text
            // before it sees a third, so they get the type of the others.
            let null = SqlExpr::Null(SqlType::Named(column(first).2.sql_type.clone()));
            let exprs = exprs
                .iter()
                .map(|&e| match e {
                    SqlExpr::Unbound if !hidden => null.clone(),
                    other => other.clone(),
                })
                .collect();
            return Ok(vec![(alias.to_owned(), exprs)]);
        }

        // Branches whose terms can be the same, in order of first branch.
        let mut buckets: Vec<(Option<&str>, Vec<usize>)> = Vec::new();
        for &b in &present {
            let ty = column(b).2;
            let key = if natural {
                Some(ty.natural_datatype.as_deref().unwrap_or(XSD_STRING))
            } else {
                None
            };
            match buckets.iter_mut().find(|(k, _)| *k == key) {
                Some((_, members)) => members.push(b),
                None => buckets.push((key, vec![b])),
            }
        }
        // A lexical form read back as a string makes a plain literal, which
        // is only right for natural datatype xsd:string.
        let lexical_ok = |key: Option<&str>| key.is_none_or(|datatype| datatype == XSD_STRING);
        if hidden {
            let mut out: Vec<SqlExpr> = exprs.iter().map(|&e| e.clone()).collect();
            for (key, members) in &buckets {
                if classes(members).len() < 2 || !lexical_ok(*key) {
                    continue;
                }
                // Without SQL for one of the forms the columns stay as they
                // are, which can only keep a duplicate.
                let Some(lexical) = members
                    .iter()
                    .map(|&b| column(b).2.lexical)
                    .collect::<Option<Vec<Lexical>>>()
                else {
                    continue;
                };
                for (&b, lexical) in members.iter().zip(lexical) {
                    let (table, name, _) = column(b);
                    out[b] = SqlExpr::Lexical {
                        table,
                        name: name.to_owned(),
                        lexical,
                    };
                }
            }
            return Ok(vec![(alias.to_owned(), out)]);
        }
        let mut out = Vec::new();
        for (k, (key, members)) in buckets.iter().enumerate() {
            let typed = classes(members).len() == 1;
            let lexical_ok = lexical_ok(*key);
            let null = if typed {
                SqlExpr::Null(SqlType::Named(column(members[0]).2.sql_type.clone()))
            } else {
                SqlExpr::Null(SqlType::Text)
            };
            let mut exprs_k = Vec::with_capacity(columns.len());
            for (b, &expr) in exprs.iter().enumerate() {
                exprs_k.push(if !members.contains(&b) {
                    null.clone()
                } else if typed {
                    expr.clone()
                } else {
                    let (table, name, ty) = column(b);
                    match ty.lexical.filter(|_| lexical_ok) {
                        Some(lexical) => SqlExpr::Lexical {
                            table,
                            name: name.to_owned(),
                            lexical,
                        },
                        None => {
                            let names: Vec<&str> =
                                members.iter().map(|&m| column(m).2.name.as_str()).collect();
                            return Err(Error::UnsupportedFeature(format!(
                                "?{variable} reads columns of SQL types {} in different UNION \
                                 branches, whose terms can be the same: comparing them needs the \
                                 natural lexical form of column {name} of {} ({}), which cannot \
                                 be computed in SQL",
                                names.join(", "),
                                describe_table(&self.branches[b].from[table]),
                                ty.name
                            )));
                        }
                    }
                });
            }
            let alias = if k == 0 {
                alias.to_owned()
            } else {
                let fresh = (1..)
                    .map(|n| format!("{alias}_{n}"))
                    .find(|a| !used.contains(a))
                    .expect("an unused alias exists");
                used.insert(fresh.clone());
                fresh
            };
            out.push((alias, exprs_k));
        }
        Ok(out)
    }
}

impl SqlPlan {
    /// The column names that PostgreSQL may resolve differently from how
    /// they are written ([`folded_identifier`]), with their logical tables,
    /// each once, in order of appearance.
    pub fn folded_columns(&self) -> Vec<(LogicalTable, String)> {
        let mut out: Vec<(LogicalTable, String)> = Vec::new();
        let mut plan = self.clone();
        plan.for_each_column(|table, name| {
            if folded_identifier(name) {
                let key = (table.clone(), name.clone());
                if !out.contains(&key) {
                    out.push(key);
                }
            }
        });
        out
    }
}

/// Whether the branches of one arm may give the same solution twice: one
/// branch proves it may not when `keys` holds, for every table it reads, a
/// unique key among its [`SqlBranch::fixed_columns`].
pub(super) fn arm_has_duplicates(branches: &[SqlBranch], keys: Option<&TableKeys>) -> bool {
    let ([branch], Some(keys)) = (branches, keys) else {
        return true;
    };
    !branch
        .from
        .iter()
        .zip(&branch.fixed_columns)
        .all(|(table, fixed)| {
            let fixed: BTreeSet<String> = fixed.iter().map(|c| identifier_name(c)).collect();
            keys.get(table)
                .is_some_and(|keys| keys.iter().any(|key| key.iter().all(|c| fixed.contains(c))))
        })
}

#[cfg(test)]
mod tests {
    use super::super::renderer::tests::{column, unreserved};
    use super::super::{Comparison, ProjectedTerm, RenderOptions, ResultTerm};
    use super::*;
    use crate::dialect::PostgresDialect;
    use crate::mapping::TemplatePart;

    const XSD: &str = "http://www.w3.org/2001/XMLSchema#";

    fn union_type(class: &str, natural: Option<&str>, lexical: Option<Lexical>) -> ColumnType {
        ColumnType {
            name: class.to_owned(),
            sql_type: format!("pg_catalog.{class}"),
            union_class: class.to_owned(),
            natural_datatype: natural.map(|n| format!("{XSD}{n}")),
            group: class.to_owned(),
            comparison: Comparison::Value,
            lexical,
            lexical_unreserved: unreserved(class),
            term_key: None,
            term_normal: None,
        }
    }

    /// `?v` from column `v` of table `a` and of table `b`, as a literal of
    /// natural datatype or in an IRI template, and a hidden `?h`.
    fn union_plan(natural: bool) -> SqlPlan {
        let branch = |t: &str| SqlBranch {
            order: Vec::new(),
            values: Vec::new(),
            arm: 0,
            from: vec![LogicalTable::TableName(t.into())],
            select: vec![column(0, "v"), column(0, "h")],
            conditions: Vec::new(),
            left_joins: Vec::new(),
            fixed_columns: vec![BTreeSet::new()],
        };
        let (term_type, value) = if natural {
            (TermType::Literal, ResultValue::Column("v".into()))
        } else {
            (
                TermType::Iri,
                ResultValue::Template(vec![
                    TemplatePart::Literal("http://ex/".into()),
                    TemplatePart::Column("v".into()),
                ]),
            )
        };
        SqlPlan {
            order: Vec::new(),
            offset: 0,
            limit: None,
            columns: vec!["v".into()],
            hidden_columns: vec!["h".into()],
            distinct_order: vec![1, 0],
            projection: vec![ProjectedTerm {
                variable: "v".into(),
                terms: vec![ResultTerm {
                    term_type,
                    value,
                    datatype: None,
                    language: None,
                    guard: None,
                }],
            }],
            split_columns: Vec::new(),
            lexical_hidden_columns: Default::default(),
            branches: vec![branch("a"), branch("b")],
            distinct: false,
            base_iri: None,
        }
    }

    fn with_union_types(plan: &SqlPlan, a: ColumnType, b: ColumnType) -> Result<SqlPlan, Error> {
        let types: ColumnTypes = [
            ((LogicalTable::TableName("a".into()), "v".to_owned()), a),
            ((LogicalTable::TableName("b".into()), "v".to_owned()), b),
        ]
        .into();
        plan.with_union_types(&types)
    }

    #[test]
    fn union_columns_lists_the_columns_of_several_branches() {
        let plan = union_plan(true);
        let column = |t: &str, c: &str| (LogicalTable::TableName(t.into()), c.to_owned());
        assert_eq!(
            plan.union_columns(),
            [
                column("a", "v"),
                column("a", "h"),
                column("b", "v"),
                column("b", "h")
            ]
        );
        let mut single = plan;
        single.branches.pop();
        assert!(single.union_columns().is_empty());
    }

    #[test]
    fn union_of_one_class_keeps_the_columns() {
        let int = || union_type("integer", Some("integer"), Some(Lexical::Text));
        for natural in [true, false] {
            let plan = union_plan(natural);
            assert_eq!(with_union_types(&plan, int(), int()).unwrap(), plan);
        }
    }

    /// Branches where an `OPTIONAL` part cannot match select `Unbound`;
    /// two of them before a typed column would make `UNION` resolve the
    /// column to text first.
    #[test]
    fn union_gives_unbound_columns_the_type_of_the_others() {
        let mut plan = union_plan(true);
        let mut third = plan.branches[0].clone();
        third.from = vec![LogicalTable::TableName("c".into())];
        plan.branches.insert(0, third);
        plan.branches[0].select[0] = SqlExpr::Unbound;
        plan.branches[1].select[0] = SqlExpr::Unbound;
        plan.branches[0].select[1] = SqlExpr::Unbound;
        let int = || union_type("integer", Some("integer"), Some(Lexical::Text));
        let typed = with_union_types(&plan, int(), int()).unwrap();
        let selects: Vec<&[SqlExpr]> = typed.branches.iter().map(|b| &b.select[..]).collect();
        let null = SqlExpr::Null(SqlType::Named("pg_catalog.integer".into()));
        assert_eq!(
            selects,
            [
                &[null.clone(), SqlExpr::Unbound][..],
                &[null, column(0, "h")][..],
                &[column(0, "v"), column(0, "h")][..],
            ]
        );
        // Different classes split as before; a branch without the column
        // leaves every split column NULL.
        plan.branches[1].select[0] = column(0, "v");
        let text = union_type("text", None, Some(Lexical::Text));
        let split = with_union_types(&plan, int(), text).unwrap();
        assert_eq!(split.columns, ["v", "v_1"]);
        assert_eq!(
            split.branches[0].select[..2],
            [
                SqlExpr::Null(SqlType::Named("pg_catalog.integer".into())),
                SqlExpr::Null(SqlType::Named("pg_catalog.text".into()))
            ]
        );
    }

    #[test]
    fn union_splits_literals_of_different_natural_datatypes() {
        let int = union_type("integer", Some("integer"), Some(Lexical::Text));
        let text = union_type("text", None, Some(Lexical::Text));
        let plan = with_union_types(&union_plan(true), int, text).unwrap();
        assert_eq!(plan.columns, ["v", "v_1"]);
        assert_eq!(plan.split_columns, [["v", "v_1"]]);
        assert_eq!(plan.distinct_order, [2, 0, 1]);
        assert_eq!(
            plan.to_sql().unwrap().sql,
            "SELECT q.\"v\", q.\"v_1\"\nFROM (\n\
             SELECT pg_typeof(t0.\"h\")::text || ' ' || t0.\"h\"::text AS \"h\", \
             t0.\"v\" AS \"v\", NULL::pg_catalog.text AS \"v_1\"\n\
             FROM \"a\" t0\n\
             UNION\n\
             SELECT pg_typeof(t0.\"h\")::text || ' ' || t0.\"h\"::text AS \"h\", \
             NULL::pg_catalog.integer AS \"v\", t0.\"v\" AS \"v_1\"\n\
             FROM \"b\" t0\n\
             ) q"
        );
    }

    #[test]
    fn union_compares_terms_that_can_be_equal_by_lexical_form() {
        let int = || union_type("integer", Some("integer"), Some(Lexical::Text));
        let numeric = union_type("numeric", Some("decimal"), Some(Lexical::Decimal));
        // Template values: 7 and 7.00 give the same IRI.
        let plan = with_union_types(&union_plan(false), numeric, int()).unwrap();
        assert!(plan.split_columns.is_empty());
        let sql = plan.to_sql().unwrap().sql;
        assert!(
            sql.contains("trim_scale(t0.\"v\")::text AS \"v\"\nFROM \"a\"")
                && sql.contains("t0.\"v\"::text AS \"v\"\nFROM \"b\""),
            "{sql}"
        );
        // Plain literals from text and uuid columns.
        let text = union_type("text", None, Some(Lexical::Text));
        let uuid = union_type("uuid", None, Some(Lexical::Text));
        let plan = with_union_types(&union_plan(true), text, uuid).unwrap();
        assert_eq!(plan.columns, ["v"]);
        assert!(matches!(
            &plan.branches[1].select[0],
            SqlExpr::Lexical {
                lexical: Lexical::Text,
                ..
            }
        ));
    }

    #[test]
    fn union_compares_hidden_columns_by_lexical_form_where_terms_can_be_equal() {
        let int = || union_type("integer", Some("integer"), Some(Lexical::Text));
        let text = || union_type("text", None, Some(Lexical::Text));
        let double = || union_type("float8", Some("double"), None);
        let a = LogicalTable::TableName("a".into());
        let b = LogicalTable::TableName("b".into());
        let types = |h_a: ColumnType, h_b: ColumnType| -> ColumnTypes {
            [
                ((a.clone(), "v".to_owned()), int()),
                ((b.clone(), "v".to_owned()), int()),
                ((a.clone(), "h".to_owned()), h_a),
                ((b.clone(), "h".to_owned()), h_b),
            ]
            .into()
        };
        let mut plan = union_plan(true);
        // A natural literal: 7 and "7" are different terms, which the type
        // in the compared text keeps apart.
        assert_eq!(plan.with_union_types(&types(int(), text())).unwrap(), plan);
        // A template value: both give the same term.
        plan.lexical_hidden_columns.insert("h".into());
        let sql = plan
            .with_union_types(&types(int(), text()))
            .unwrap()
            .to_sql()
            .unwrap()
            .sql;
        assert_eq!(
            sql.matches("pg_typeof(t0.\"h\"::text)::text || ' ' || t0.\"h\"::text::text AS \"h\"")
                .count(),
            2,
            "{sql}"
        );
        // No SQL for a double's form: kept, at worst a duplicate stays.
        assert_eq!(
            plan.with_union_types(&types(double(), int())).unwrap(),
            plan
        );
    }

    #[test]
    fn union_without_sql_for_the_lexical_form_is_rejected() {
        let int = || union_type("integer", Some("integer"), Some(Lexical::Text));
        let double = union_type("float8", Some("double"), None);
        let err = with_union_types(&union_plan(false), double, int()).unwrap_err();
        assert!(
            matches!(&err, Error::UnsupportedFeature(m)
                if m.contains("?v") && m.contains("column v of table a (float8)")),
            "{err}"
        );
        // Natural literals of different datatypes need no lexical form.
        let double = union_type("float8", Some("double"), None);
        assert!(with_union_types(&union_plan(true), double, int()).is_ok());
        // Two classes of one natural datatype other than xsd:string: a
        // lexical form read back would lose the datatype.
        let a = union_type("numeric", Some("decimal"), Some(Lexical::Decimal));
        let b = union_type("numeric2", Some("decimal"), Some(Lexical::Decimal));
        assert!(matches!(
            with_union_types(&union_plan(true), a, b),
            Err(Error::UnsupportedFeature(_))
        ));
    }

    fn keyed_plan(fixed: &[&str], hidden: bool) -> SqlPlan {
        SqlPlan {
            order: Vec::new(),
            offset: 0,
            limit: None,
            columns: vec!["s".into()],
            hidden_columns: if hidden { vec!["o".into()] } else { Vec::new() },
            distinct_order: Vec::new(),
            split_columns: Vec::new(),
            lexical_hidden_columns: Default::default(),
            projection: Vec::new(),
            branches: vec![SqlBranch {
                order: Vec::new(),
                values: Vec::new(),
                arm: 0,
                from: vec![LogicalTable::TableName("\"T\"".into())],
                select: vec![column(0, "\"Id\""), column(0, "v")],
                conditions: Vec::new(),
                left_joins: Vec::new(),
                fixed_columns: vec![fixed.iter().map(|c| c.to_string()).collect()],
            }],
            distinct: false,
            base_iri: None,
        }
    }

    #[test]
    fn duplicates_are_removed_over_all_variables_then_projected() {
        assert_eq!(
            keyed_plan(&[], true).to_sql().unwrap().sql,
            "SELECT q.\"s\"\nFROM (\n\
             SELECT DISTINCT t0.\"Id\" AS \"s\", t0.\"v\" AS \"o\"\n\
             FROM \"T\" t0\n\
             ) q"
        );
        assert_eq!(
            keyed_plan(&[], false).to_sql().unwrap().sql,
            "SELECT DISTINCT t0.\"Id\" AS \"s\"\nFROM \"T\" t0"
        );
        let mut no_columns = keyed_plan(&[], true);
        no_columns.columns.clear();
        no_columns.branches[0].select.remove(0);
        assert_eq!(
            no_columns.to_sql().unwrap().sql,
            "SELECT 1\nFROM (\n\
             SELECT DISTINCT t0.\"v\" AS \"o\"\n\
             FROM \"T\" t0\n\
             ) q"
        );
    }

    #[test]
    fn a_unique_key_among_the_fixed_columns_keeps_the_rows() {
        let table = LogicalTable::TableName("\"T\"".into());
        let keys: TableKeys = [(table.clone(), vec![vec!["Id".to_owned()]])].into();
        let options = RenderOptions {
            keys: Some(&keys),
            ..RenderOptions::default()
        };
        let plan = keyed_plan(&["\"Id\"", "v"], true);
        assert_eq!(plan.key_tables(), std::slice::from_ref(&table));
        assert!(!plan.has_duplicates(Some(&keys)));
        assert_eq!(
            plan.to_sql_with(&PostgresDialect, &options).unwrap().sql,
            "SELECT t0.\"Id\" AS \"s\"\nFROM \"T\" t0"
        );
        // Not fixed by the solution, unknown keys, or no metadata.
        assert!(keyed_plan(&["v"], true).has_duplicates(Some(&keys)));
        assert!(plan.has_duplicates(Some(&TableKeys::new())));
        assert!(plan.has_duplicates(None));
        // A composite key needs all its columns.
        let composite: TableKeys = [(table, vec![vec!["Id".to_owned(), "w".to_owned()]])].into();
        assert!(plan.has_duplicates(Some(&composite)));
    }

    #[test]
    fn keys_cannot_help_with_several_branches_views_or_distinct() {
        let mut two = keyed_plan(&["\"Id\""], false);
        two.branches.push(two.branches[0].clone());
        assert!(two.key_tables().is_empty());
        assert!(two.has_duplicates(Some(&TableKeys::new())));

        let mut view = keyed_plan(&["\"Id\""], false);
        view.branches[0].from[0] = LogicalTable::SqlQuery("SELECT 1".into());
        assert!(view.key_tables().is_empty());

        let mut distinct = keyed_plan(&[], true);
        distinct.distinct = true;
        assert!(distinct.key_tables().is_empty());
        assert!(!distinct.has_duplicates(None));
        assert_eq!(
            distinct.to_sql().unwrap().sql,
            "SELECT DISTINCT t0.\"Id\" AS \"s\"\nFROM \"T\" t0"
        );
    }
}
