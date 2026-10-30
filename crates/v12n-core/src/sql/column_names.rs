// SPDX-License-Identifier: Apache-2.0
//! Column names replaced by the names the database resolved them to, and
//! table aliases renumbered.

use std::collections::HashMap;

use crate::mapping::{LogicalTable, TemplatePart};

use super::plan::{PartMut, visit_cases_mut};
use super::{Condition, SqlBranch, SqlExpr, SqlPlan, TermSql, TermSqlValue, quote_identifier};

impl SqlPlan {
    /// The plan with column names replaced by the names the database
    /// resolved them to, written as delimited identifiers so they are
    /// taken as they are. Names missing from `resolved` are kept.
    pub fn with_resolved_columns(
        &self,
        resolved: &HashMap<(LogicalTable, String), String>,
    ) -> Self {
        let mut plan = self.clone();
        plan.for_each_column(|table, name| {
            if let Some(found) = resolved.get(&(table.clone(), name.clone())) {
                *name = quote_identifier(found);
            }
        });
        plan
    }

    pub(super) fn for_each_column(&mut self, mut f: impl FnMut(&LogicalTable, &mut String)) {
        fn expr(
            e: &mut SqlExpr,
            from: &[LogicalTable],
            f: &mut impl FnMut(&LogicalTable, &mut String),
        ) {
            match e {
                SqlExpr::Column { table, name } | SqlExpr::Lexical { table, name, .. } => {
                    f(&from[*table], name);
                }
                SqlExpr::Param(_)
                | SqlExpr::Null(_)
                | SqlExpr::Unbound
                | SqlExpr::True
                | SqlExpr::Value { .. } => {}
                SqlExpr::Concat(parts) => parts.iter_mut().for_each(|p| expr(p, from, f)),
                SqlExpr::Matched(inner) => expr(inner, from, f),
                SqlExpr::Term { term: t, guard } => {
                    term(t, from, f);
                    if let Some(guard) = guard {
                        expr(guard, from, f);
                    }
                }
            }
        }
        fn term(
            t: &mut TermSql,
            from: &[LogicalTable],
            f: &mut impl FnMut(&LogicalTable, &mut String),
        ) {
            match &mut t.value {
                TermSqlValue::Column { table, name } => f(&from[*table], name),
                TermSqlValue::Template { table, parts, .. } => {
                    for part in parts {
                        if let TemplatePart::Column(name) = part {
                            f(&from[*table], name);
                        }
                    }
                }
                TermSqlValue::Param(_) => {}
            }
        }
        for branch in &mut self.branches {
            let from = &branch.from;
            for e in &mut branch.select {
                expr(e, from, &mut f);
            }
            fn condition(
                c: &mut Condition,
                from: &[LogicalTable],
                f: &mut impl FnMut(&LogicalTable, &mut String),
            ) {
                match c {
                    Condition::Eq(left, right) => {
                        expr(left, from, f);
                        expr(right, from, f);
                    }
                    Condition::EqAny(left, right) => {
                        expr(left, from, f);
                        right.iter_mut().for_each(|e| expr(e, from, f));
                    }
                    Condition::IsNotNull(e) | Condition::IsNull(e) => expr(e, from, f),
                    Condition::Or(alternatives) => alternatives
                        .iter_mut()
                        .flatten()
                        .for_each(|c| condition(c, from, f)),
                    Condition::SameTerm(left, right) => {
                        term(left, from, f);
                        term(right, from, f);
                    }
                    Condition::Filter(filter) => filter.visit_mut(&mut |part| match part {
                        PartMut::Condition(c) => condition(c, from, f),
                        PartMut::Term(t) => term(t, from, f),
                    }),
                }
            }
            let conditions = branch
                .conditions
                .iter_mut()
                .chain(branch.left_joins.iter_mut().flat_map(|j| &mut j.conditions));
            for c in conditions {
                condition(c, from, &mut f);
            }
            for value in &mut branch.order {
                value.visit_mut(&mut |part| match part {
                    PartMut::Condition(c) => condition(c, from, &mut f),
                    PartMut::Term(t) => term(t, from, &mut f),
                });
            }
            for cases in &mut branch.values {
                visit_cases_mut(cases, &mut |part| match part {
                    PartMut::Condition(c) => condition(c, from, &mut f),
                    PartMut::Term(t) => term(t, from, &mut f),
                });
            }
            for (table, fixed) in from.iter().zip(&mut branch.fixed_columns) {
                *fixed = std::mem::take(fixed)
                    .into_iter()
                    .map(|mut name| {
                        f(table, &mut name);
                        name
                    })
                    .collect();
            }
        }
    }
}

impl SqlBranch {
    /// Replaces every table alias the select list, the conditions and the
    /// order values refer to by `f` of it. `from`, `fixed_columns` and
    /// the ranges of `left_joins` are left to the caller.
    pub(crate) fn rename_tables(&mut self, f: &impl Fn(usize) -> usize) {
        fn expr(e: &mut SqlExpr, f: &impl Fn(usize) -> usize) {
            match e {
                SqlExpr::Column { table, .. } | SqlExpr::Lexical { table, .. } => {
                    *table = f(*table)
                }
                SqlExpr::Param(_)
                | SqlExpr::Null(_)
                | SqlExpr::Unbound
                | SqlExpr::True
                | SqlExpr::Value { .. } => {}
                SqlExpr::Concat(parts) => parts.iter_mut().for_each(|p| expr(p, f)),
                SqlExpr::Matched(inner) => expr(inner, f),
                SqlExpr::Term { term: t, guard } => {
                    term(t, f);
                    if let Some(guard) = guard {
                        expr(guard, f);
                    }
                }
            }
        }
        fn term(t: &mut TermSql, f: &impl Fn(usize) -> usize) {
            match &mut t.value {
                TermSqlValue::Column { table, .. } | TermSqlValue::Template { table, .. } => {
                    *table = f(*table);
                }
                TermSqlValue::Param(_) => {}
            }
        }
        fn condition(c: &mut Condition, f: &impl Fn(usize) -> usize) {
            match c {
                Condition::Eq(left, right) => {
                    expr(left, f);
                    expr(right, f);
                }
                Condition::EqAny(left, right) => {
                    expr(left, f);
                    right.iter_mut().for_each(|e| expr(e, f));
                }
                Condition::IsNotNull(e) | Condition::IsNull(e) => expr(e, f),
                Condition::Or(alternatives) => alternatives
                    .iter_mut()
                    .flatten()
                    .for_each(|c| condition(c, f)),
                Condition::SameTerm(left, right) => {
                    term(left, f);
                    term(right, f);
                }
                Condition::Filter(filter) => filter.visit_mut(&mut |part| match part {
                    PartMut::Condition(c) => condition(c, f),
                    PartMut::Term(t) => term(t, f),
                }),
            }
        }
        for e in &mut self.select {
            expr(e, f);
        }
        let conditions = self
            .conditions
            .iter_mut()
            .chain(self.left_joins.iter_mut().flat_map(|j| &mut j.conditions));
        for c in conditions {
            condition(c, f);
        }
        for value in &mut self.order {
            value.visit_mut(&mut |part| match part {
                PartMut::Condition(c) => condition(c, f),
                PartMut::Term(t) => term(t, f),
            });
        }
        for cases in &mut self.values {
            visit_cases_mut(cases, &mut |part| match part {
                PartMut::Condition(c) => condition(c, f),
                PartMut::Term(t) => term(t, f),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::TableKeys;
    use super::super::renderer::tests::column;
    use super::*;

    #[test]
    fn resolved_columns_are_rendered_as_delimited_identifiers() {
        let view = LogicalTable::SqlQuery("SELECT 1 AS Id, 2 AS \"V\"".into());
        let template = |table| TermSql {
            value: TermSqlValue::Template {
                table,
                parts: vec![
                    TemplatePart::Literal("r/".into()),
                    TemplatePart::Column("Id".into()),
                ],
                iri_safe: false,
            },
            datatype: None,
            base_iri: None,
        };
        let plan = SqlPlan {
            order: Vec::new(),
            offset: 0,
            limit: None,
            columns: vec!["a".into(), "b".into()],
            hidden_columns: Vec::new(),
            distinct_order: Vec::new(),
            split_columns: Vec::new(),
            lexical_hidden_columns: Default::default(),
            projection: Vec::new(),
            branches: vec![SqlBranch {
                order: Vec::new(),
                values: Vec::new(),
                arm: 0,
                from: vec![view.clone(), view.clone()],
                select: vec![column(0, "Id"), column(1, "\"V\"")],
                conditions: vec![
                    Condition::SameTerm(template(0), template(1)),
                    Condition::Eq(column(0, "ID"), SqlExpr::Param("1".into())),
                ],
                left_joins: Vec::new(),
                fixed_columns: Vec::new(),
            }],
            distinct: false,
            base_iri: None,
        };
        assert_eq!(
            plan.folded_columns(),
            [
                (view.clone(), "Id".to_owned()),
                (view.clone(), "ID".to_owned())
            ]
        );
        let resolved = HashMap::from([
            ((view.clone(), "Id".to_owned()), "id".to_owned()),
            ((view, "ID".to_owned()), "id".to_owned()),
        ]);
        assert_eq!(
            plan.with_resolved_columns(&resolved).to_sql().unwrap().sql,
            "SELECT DISTINCT t0.\"id\" AS \"a\", t1.\"V\" AS \"b\"\n\
             FROM (SELECT 1 AS Id, 2 AS \"V\") t0\n\
             JOIN (SELECT 1 AS Id, 2 AS \"V\") t1 ON $1 || t0.\"id\"::text = $2 || t1.\"id\"::text\n\
             WHERE t0.\"id\" = $3"
        );
    }

    #[test]
    fn resolved_columns_keep_keys_matching_the_fixed_columns() {
        let table = LogicalTable::TableName("t".into());
        let plan = SqlPlan {
            order: Vec::new(),
            offset: 0,
            limit: None,
            columns: vec!["s".into()],
            hidden_columns: Vec::new(),
            distinct_order: Vec::new(),
            split_columns: Vec::new(),
            lexical_hidden_columns: Default::default(),
            projection: Vec::new(),
            branches: vec![SqlBranch {
                order: Vec::new(),
                values: Vec::new(),
                arm: 0,
                from: vec![table.clone()],
                select: vec![column(0, "ID")],
                conditions: Vec::new(),
                left_joins: Vec::new(),
                fixed_columns: vec![["ID".to_owned()].into()],
            }],
            distinct: false,
            base_iri: None,
        };
        let resolved = HashMap::from([((table.clone(), "ID".to_owned()), "id".to_owned())]);
        let plan = plan.with_resolved_columns(&resolved);
        assert_eq!(
            plan.branches[0].fixed_columns,
            [["\"id\"".to_owned()].into()]
        );
        let keys: TableKeys = [(table, vec![vec!["id".to_owned()]])].into();
        assert!(!plan.has_duplicates(Some(&keys)));
    }
}
