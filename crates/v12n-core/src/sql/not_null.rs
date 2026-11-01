// SPDX-License-Identifier: Apache-2.0
//! `IS NOT NULL` conditions that a `NOT NULL` constraint makes true.

use std::collections::HashSet;

use crate::mapping::LogicalTable;

use super::{Condition, SqlBranch, SqlExpr, SqlPlan};

impl SqlPlan {
    /// The plan without the `IS NOT NULL` conditions on `not_null`
    /// columns, which a constraint keeps from being `NULL` in any row of
    /// their table.
    ///
    /// R2RML §11 makes no term from a `NULL`, hence the conditions. Only a
    /// condition of a scope on a table that the scope scans itself is
    /// dropped: a top-level condition on a required table, or a condition
    /// of an `OPTIONAL` part on a table of that part and not of a part
    /// nested in it. A column of a part's table is `NULL` in every row the
    /// part does not extend, so a condition on it from outside the part,
    /// such as the one `BOUND` renders, still tells whether the part
    /// matched. Conditions inside a disjunction, a filter or the cases of
    /// a value are kept. The guard of a part that reads a row
    /// ([`SameRow`](super::SameRow)) drops those on that row.
    pub fn without_implied_not_null(&self, not_null: &HashSet<(LogicalTable, String)>) -> Self {
        let mut plan = self.clone();
        for branch in &mut plan.branches {
            branch.drop_implied_not_null(not_null);
        }
        plan
    }
}

impl SqlBranch {
    fn drop_implied_not_null(&mut self, not_null: &HashSet<(LogicalTable, String)>) {
        let required = self.own_tables(None);
        retain_needed(&mut self.conditions, &required, &self.from, not_null);
        for part in 0..self.left_joins.len() {
            let own = self.own_tables(Some(part));
            let conditions = &mut self.left_joins[part].conditions;
            retain_needed(conditions, &own, &self.from, not_null);
            // Where the row read has no table row, every column read from
            // it is `NULL` whatever the guard says.
            if let Some(same_row) = &mut self.left_joins[part].same_row {
                let row_of = [same_row.row_of];
                retain_needed(&mut same_row.guard, &row_of, &self.from, not_null);
            }
        }
    }

    /// The tables that `part` scans itself: its tables without those of
    /// the parts nested directly in it. With `None`, the required tables.
    fn own_tables(&self, part: Option<usize>) -> Vec<usize> {
        let tables = match part {
            None => 0..self.from.len(),
            Some(part) => self.left_joins[part].tables.clone(),
        };
        let nested: Vec<_> = self
            .left_joins
            .iter()
            .filter(|join| join.parent == part)
            .map(|join| join.tables.clone())
            .collect();
        tables
            .filter(|table| !nested.iter().any(|range| range.contains(table)))
            .collect()
    }
}

fn retain_needed(
    conditions: &mut Vec<Condition>,
    own: &[usize],
    from: &[LogicalTable],
    not_null: &HashSet<(LogicalTable, String)>,
) {
    conditions.retain(|condition| {
        let Condition::IsNotNull(SqlExpr::Column { table, name }) = condition else {
            return true;
        };
        !(own.contains(table) && not_null.contains(&(from[*table].clone(), name.clone())))
    });
}

#[cfg(test)]
mod tests {
    use crate::mapping::{Mapping, MappingIndex};
    use crate::{sparql, unfold};

    use super::*;

    const SHOP: &str = include_str!("../../../../examples/shop/mapping.ttl");

    fn sql(query: &str, not_null: &[(&str, &str)]) -> String {
        let mapping = Mapping::from_turtle(SHOP).unwrap();
        let query = sparql::parse(&format!("PREFIX ex: <http://example.org/>\n{query}")).unwrap();
        let plan = unfold::unfold(&query, &mapping, &MappingIndex::new(&mapping)).unwrap();
        let not_null = not_null
            .iter()
            .map(|(table, name)| (LogicalTable::TableName((*table).into()), (*name).into()))
            .collect();
        plan.without_implied_not_null(&not_null)
            .to_sql()
            .unwrap()
            .sql
    }

    #[test]
    fn required_columns_lose_the_condition_only_with_a_constraint() {
        let query = "SELECT ?c ?n ?t WHERE { ?c ex:name ?n ; ex:tier ?t }";
        let all = sql(query, &[]);
        assert!(all.contains("t0.\"name\" IS NOT NULL"), "{all}");
        let constrained = sql(query, &[("customers", "id"), ("customers", "name")]);
        assert!(
            !constrained.contains("\"name\" IS NOT NULL"),
            "{constrained}"
        );
        assert!(!constrained.contains("\"id\" IS NOT NULL"), "{constrained}");
        assert!(
            constrained.contains("\"tier\" IS NOT NULL"),
            "{constrained}"
        );
    }

    #[test]
    fn an_optional_part_drops_conditions_on_its_own_tables_only() {
        let query = "SELECT ?c ?a WHERE { ?c a ex:Customer \
                     OPTIONAL { ?o ex:placedBy ?c ; ex:amount ?a } FILTER(BOUND(?a)) }";
        let all = sql(query, &[]);
        let constrained = sql(
            query,
            &[
                ("customers", "id"),
                ("orders", "customer_id"),
                ("orders", "amount"),
            ],
        );
        assert_ne!(all, constrained);
        // Inside the part, its own rows are table rows; outside it,
        // `BOUND` reads a column the LEFT JOIN may leave NULL.
        let (inside, outside) = constrained
            .split_once("\nWHERE")
            .map_or((constrained.as_str(), ""), |(on, rest)| (on, rest));
        assert!(!inside.contains("\"amount\" IS NOT NULL"), "{constrained}");
        assert!(outside.contains("IS NOT NULL"), "{constrained}");
        insta::assert_snapshot!(constrained);
    }
}
