// SPDX-License-Identifier: Apache-2.0
//! Rendering of a plan as SQL text with bound parameters.

use std::collections::{BTreeSet, HashSet};
use std::ops::Range;

use crate::Error;
use crate::dialect::{Dialect, PostgresDialect, SqlType};
use crate::mapping::{LogicalTable, SCHEME_REGEX, TemplatePart};

use super::identifier::{describe_table, logical_table};
use super::plan::XSD_STRING;
use super::required_columns::arm_has_duplicates;
use super::{
    ColumnType, ColumnTypes, Comparison, Condition, Lexical, OrderColumn, RenderOptions,
    RenderedSql, SqlBranch, SqlExpr, SqlPlan, TermDatatype, TermKey, TermNormal, TermSql,
    TermSqlValue, filter, order, quote_identifier,
};

impl RenderedSql {
    /// `\nORDER BY` over the sort keys as columns of table alias `table`,
    /// or nothing without `ORDER BY`.
    pub fn order_by_clause(&self, dialect: &dyn Dialect, table: &str) -> String {
        order_by_clause(dialect, &self.order_by, table)
    }
}

fn order_by_clause(dialect: &dyn Dialect, columns: &[OrderColumn], table: &str) -> String {
    if columns.is_empty() {
        return String::new();
    }
    let keys: Vec<String> = columns
        .iter()
        .map(|c| {
            let mut key = format!("{table}.{}", quote_identifier(&c.alias));
            if c.collated {
                key.push_str(dialect.code_point_collation());
            }
            if c.descending {
                key.push_str(" DESC");
            }
            key
        })
        .collect();
    format!("\nORDER BY {}", keys.join(", "))
}

impl SqlPlan {
    /// Renders the plan. Parameters are numbered in the order they appear
    /// in the text.
    ///
    /// Every parameter is one side of an equality, so binding it to `NULL`
    /// makes the equality fail. For a [required](RenderedSql::param_required)
    /// one, that empties its branch and leaves the others as they are; in
    /// an `OPTIONAL` part or a disjunction, the part or the alternative
    /// does not match. An alternative of an `IN` list
    /// ([`RenderedSql::param_alternatives`]) only drops itself; binding all
    /// of them to `NULL` fails the `IN`.
    ///
    /// Without column types, a [`Condition::SameTerm`] compares columns
    /// with `=` and template columns by their text output, which is what
    /// the database sees for text and integer columns.
    ///
    /// Rendering fails with [`Error::UnsupportedFeature`] for a plan whose
    /// SQL would not follow SPARQL's semantics, even without column types.
    pub fn to_sql(&self) -> Result<RenderedSql, Error> {
        self.to_sql_with(&PostgresDialect, &RenderOptions::default())
    }

    /// Like [`to_sql`](Self::to_sql), in the SQL of `dialect` and with what
    /// a database adapter knows:
    ///
    /// - The column compared with each parameter in `text_params` is cast
    ///   to text. An adapter uses this for column types whose values it
    ///   does not format itself: the comparison then uses the same text
    ///   the result column gets, instead of the database parsing the
    ///   constant as that type. The cast keeps an index on the column
    ///   from being used.
    /// - With `column_types`, a [`Condition::SameTerm`] is `false` when a
    ///   natural datatype differs from the other side's datatype, compares
    ///   two columns of the same group as [`Comparison`] says, and
    ///   otherwise compares natural lexical forms computed in SQL. A
    ///   column whose lexical form SQL cannot compute is an
    ///   [`Error::UnsupportedFeature`], rather than a comparison that
    ///   fails in the database or gives wrong answers.
    /// - With `keys`, duplicates are not removed when the keys prove that
    ///   there are none (see [`has_duplicates`](Self::has_duplicates)).
    ///
    /// Duplicates are removed with `DISTINCT` (one branch) or `UNION`
    /// over the columns of all variables, in a subquery when some of them
    /// are not selected. This compares column values, so two different
    /// values that a template renders as the same string (see
    /// [`SqlPlan::hidden_columns`]) still count twice.
    ///
    /// `ORDER BY` sorts by keys that put the solutions in SPARQL's order
    /// (see `order.rs`): by expressions over the tables when the plan is
    /// one plain `SELECT`, otherwise by keys every branch selects, outside
    /// a subquery. `LIMIT` and `OFFSET` come last, as parameters.
    pub fn to_sql_with(
        &self,
        dialect: &dyn Dialect,
        options: &RenderOptions<'_>,
    ) -> Result<RenderedSql, Error> {
        let mut r = Renderer::new(dialect, *options);
        if self.branches.is_empty() {
            r.sql.push_str("SELECT 1 WHERE false");
        }
        // Under `DISTINCT`, all branches form one group whose duplicates
        // are removed; otherwise each arm is one, whose duplicates are
        // removed unless keys prove there are none.
        let groups: Vec<(Range<usize>, bool)> = if self.distinct {
            vec![(0..self.branches.len(), true)]
        } else {
            self.arms()
                .into_iter()
                .map(|arm| {
                    let dedup = arm_has_duplicates(&self.branches[arm.clone()], options.keys);
                    (arm, dedup)
                })
                .collect()
        };
        let dedup = !self.distinct && groups.iter().any(|&(_, dedup)| dedup);
        let mut select = Select {
            aliases: self.columns.clone(),
            order: (0..self.columns.len()).collect(),
            typed_text_from: None,
            slots: Vec::new(),
            collate_slots: false,
            term_keys: Vec::new(),
        };
        if dedup {
            select.aliases.extend(self.hidden_columns.iter().cloned());
            if self.distinct_order.is_empty() {
                select.order = (0..select.aliases.len()).collect();
            } else {
                select.order.clone_from(&self.distinct_order);
            }
            if self.branches.len() > 1 {
                select.typed_text_from = Some(self.columns.len());
            }
        }
        if (self.distinct || dedup)
            && let Some(types) = options.column_types
        {
            select.term_keys = self.term_keys(&select, types);
        }
        let subquery = select.aliases.len() > self.columns.len()
            || !select.term_keys.is_empty()
            || select.order.iter().enumerate().any(|(i, &o)| i != o);
        let layouts = if self.order.is_empty() || self.branches.is_empty() {
            Vec::new()
        } else {
            r.order_layouts(self)?
        };
        let ordered = layouts.iter().any(|l| !l.slots.is_empty());
        // One plain `SELECT` sorts by expressions over its tables, which
        // keeps an index on a sorted column usable. Anything else selects
        // the sort keys and sorts the rows of the subquery: `UNION`
        // combines rows of different branches, and `SELECT DISTINCT`
        // cannot sort by expressions it does not select.
        let direct = ordered && self.branches.len() == 1 && !subquery && !groups[0].1;
        let wrapped = subquery || (ordered && !direct);
        if ordered {
            let used: HashSet<&str> = select
                .aliases
                .iter()
                .chain(select.term_keys.iter().map(|(alias, _)| alias))
                .map(String::as_str)
                .collect();
            let mut names = Vec::new();
            for (k, layout) in layouts.iter().enumerate() {
                for &slot in &layout.slots {
                    let mut name = format!("o{}_{}", k + 1, order::slot_name(slot));
                    while used.contains(name.as_str()) {
                        name.push('_');
                    }
                    names.push((name, k, slot));
                }
            }
            select.slots = names;
            select.collate_slots = direct;
        }
        // The sort keys are result columns after the projected ones, so
        // that a query around this one can sort by them again: SQL keeps no
        // order through a subquery.
        let order_by: Vec<OrderColumn> = select
            .slots
            .iter()
            .map(|(alias, k, slot)| OrderColumn {
                alias: alias.clone(),
                collated: slot.collated(),
                descending: self.order[*k].descending,
            })
            .collect();
        if wrapped {
            r.sql.push_str("SELECT ");
            let outer: Vec<String> = self
                .columns
                .iter()
                .chain(order_by.iter().map(|c| &c.alias))
                .map(|c| format!("q.{}", quote_identifier(c)))
                .collect();
            if outer.is_empty() {
                r.sql.push('1');
            }
            r.sql.push_str(&outer.join(", "));
            r.sql.push_str("\nFROM (\n");
        }
        for (g, (range, distinct)) in groups.iter().enumerate() {
            if g > 0 {
                r.sql.push_str("\nUNION ALL\n");
            }
            let single = range.len() == 1;
            // `UNION` and `UNION ALL` apply from left to right, so a group
            // after another is parenthesized.
            let parenthesized = *distinct && !single && groups.len() > 1;
            if parenthesized {
                r.sql.push('(');
            }
            for i in range.clone() {
                if i > range.start {
                    r.sql.push_str(if *distinct {
                        "\nUNION\n"
                    } else {
                        "\nUNION ALL\n"
                    });
                }
                r.branch_index = i;
                r.branch(&self.branches[i], &select, &layouts, *distinct && single)?;
            }
            if parenthesized {
                r.sql.push(')');
            }
        }
        if wrapped {
            r.sql.push_str("\n) q");
        }
        // Sort keys and limits never empty a branch.
        r.required = false;
        if wrapped && ordered {
            r.sql.push_str(&order_by_clause(dialect, &order_by, "q"));
        } else if direct {
            // By the names of the selected keys, which stand for their
            // expressions, so an index on a sorted column serves the sort;
            // the keys are selected with their collation.
            let keys: Vec<String> = order_by
                .iter()
                .map(|c| {
                    let key = quote_identifier(&c.alias);
                    if c.descending {
                        format!("{key} DESC")
                    } else {
                        key
                    }
                })
                .collect();
            r.sql.push_str(&format!("\nORDER BY {}", keys.join(", ")));
        }
        if !self.branches.is_empty() {
            // Constants are bound parameters, here as elsewhere; no result
            // has more rows than `int8` counts.
            let count = |n: usize| i64::try_from(n).unwrap_or(i64::MAX).to_string();
            if let Some(limit) = self.limit {
                r.sql.push_str("\nLIMIT ");
                r.cast_rendered(&SqlType::BigInt, |r| {
                    r.expr(&SqlExpr::Param(count(limit).into()))
                });
            }
            if self.offset > 0 {
                r.sql.push_str("\nOFFSET ");
                r.cast_rendered(&SqlType::BigInt, |r| {
                    r.expr(&SqlExpr::Param(count(self.offset).into()))
                });
            }
        }
        Ok(RenderedSql {
            sql: r.sql,
            params: r.params,
            param_datatypes: r.param_datatypes,
            param_branches: r.param_branches,
            param_required: r.param_required,
            param_alternatives: r.param_alternatives,
            order_by,
        })
    }

    /// The [`TermKey`] columns to select next to the entries of `select`
    /// whose duplicates are removed: one per entry that some branch reads
    /// from a column whose type has a key.
    fn term_keys(&self, select: &Select, types: &ColumnTypes) -> Vec<(String, usize)> {
        let mut used: HashSet<String> = select.aliases.iter().cloned().collect();
        let mut out = Vec::new();
        for &entry in &select.order {
            let keyed = self.branches.iter().any(|branch| {
                matches!(
                    &branch.select[entry],
                    SqlExpr::Column { table, name }
                        if types
                            .get(&(branch.from[*table].clone(), name.clone()))
                            .is_some_and(|ty| ty.term_key.is_some())
                )
            });
            if keyed {
                let mut alias = format!("{}_key", select.aliases[entry]);
                while used.contains(&alias) {
                    alias.push('_');
                }
                used.insert(alias.clone());
                out.push((alias, entry));
            }
        }
        out
    }
}

/// A table or a nested `OPTIONAL` part of a join tree.
#[derive(Debug, Clone, Copy)]
enum Member {
    Table(usize),
    Part(usize),
}

/// The select list of every branch.
struct Select {
    /// Alias of each entry of [`SqlBranch::select`] that is rendered.
    aliases: Vec<String>,
    /// The entries in the order they are selected.
    order: Vec<usize>,
    /// Entries from here on are only compared, never decoded, and are
    /// selected as their type and text output.
    typed_text_from: Option<usize>,
    /// The sort keys selected after the entries, for sorting outside:
    /// each alias, order comparator and slot.
    slots: Vec<(String, usize, order::Slot)>,
    /// Whether the sort keys compared by code point are selected with the
    /// code point collation, for sorting by their names.
    collate_slots: bool,
    /// Keys selected after the entries where duplicates are removed, each
    /// alias and the entry whose column it tells apart (see [`TermKey`]):
    /// `DISTINCT` and `UNION` compare values with `=`, which would merge
    /// `-0` and `0`, or `char(n)` values of different padding, all
    /// different terms.
    term_keys: Vec<(String, usize)>,
}

pub(super) struct Renderer<'a> {
    pub(super) dialect: &'a dyn Dialect,
    pub(super) sql: String,
    params: Vec<String>,
    param_datatypes: Vec<Option<String>>,
    param_branches: Vec<usize>,
    param_required: Vec<bool>,
    param_alternatives: Vec<Option<usize>>,
    alternatives: usize,
    branch_index: usize,
    /// Whether the conditions being rendered hold for every row of the
    /// branch, rather than in an `OPTIONAL` part or a disjunction.
    required: bool,
    /// Units rendered so far, for their aliases.
    units: usize,
    pub(super) options: RenderOptions<'a>,
    /// Logical tables of the branch being rendered.
    pub(super) from: &'a [LogicalTable],
}

impl<'a> Renderer<'a> {
    fn new(dialect: &'a dyn Dialect, options: RenderOptions<'a>) -> Self {
        Self {
            dialect,
            sql: String::new(),
            params: Vec::new(),
            param_datatypes: Vec::new(),
            param_branches: Vec::new(),
            param_required: Vec::new(),
            param_alternatives: Vec::new(),
            alternatives: 0,
            branch_index: 0,
            required: false,
            units: 0,
            options,
            from: &[],
        }
    }

    /// Runs `render`, then replaces the SQL it wrote with `wrap` of it.
    pub(super) fn wrapped<T>(
        &mut self,
        render: impl FnOnce(&mut Self) -> T,
        wrap: impl FnOnce(&dyn Dialect, &str) -> String,
    ) -> T {
        let start = self.sql.len();
        let out = render(self);
        let inner = self.sql.split_off(start);
        self.sql.push_str(&wrap(self.dialect, &inner));
        out
    }

    /// `NULL` of type `ty`.
    pub(super) fn typed_null(&mut self, ty: &SqlType) {
        self.sql.push_str(&self.dialect.cast("NULL", ty));
    }

    /// Runs `render`, then casts the SQL it wrote to `ty`.
    pub(super) fn cast_rendered<T>(
        &mut self,
        ty: &SqlType,
        render: impl FnOnce(&mut Self) -> T,
    ) -> T {
        self.wrapped(render, |dialect, expr| dialect.cast(expr, ty))
    }

    fn branch(
        &mut self,
        branch: &'a SqlBranch,
        select: &Select,
        layouts: &[order::Layout],
        distinct: bool,
    ) -> Result<(), Error> {
        self.from = &branch.from;
        // Constants of the select list are strings of terms, never
        // compared, so binding one cannot empty the branch.
        self.required = false;
        self.sql.push_str("SELECT ");
        if distinct {
            self.sql.push_str("DISTINCT ");
        }
        if select.order.is_empty() && select.slots.is_empty() {
            self.sql.push('1');
        }
        let mut selected = 0;
        for (i, &entry) in select.order.iter().enumerate() {
            if i > 0 {
                self.sql.push_str(", ");
            }
            if select.typed_text_from.is_some_and(|from| entry >= from) {
                // UNION matches columns by position, and the same variable
                // may read columns of different types in different
                // branches. Equal type and text output imply the same
                // value, hence the same term; the reverse can fail
                // (`numeric` 1.0 and 1.00, or `varchar` and `text`), which
                // only keeps a duplicate.
                self.wrapped(
                    |this| this.select_expr(branch, &branch.select[entry]),
                    |dialect, column| {
                        format!(
                            "{} || ' ' || {}",
                            dialect.type_of(column),
                            dialect.cast(column, &SqlType::Text)
                        )
                    },
                )?;
            } else {
                self.select_expr(branch, &branch.select[entry])?;
            }
            self.sql.push_str(" AS ");
            self.sql.push_str(&quote_identifier(&select.aliases[entry]));
            selected += 1;
        }
        for (alias, entry) in &select.term_keys {
            if selected > 0 {
                self.sql.push_str(", ");
            }
            // Text in every branch, so that `UNION` combines them.
            match &branch.select[*entry] {
                SqlExpr::Column { table, name } => {
                    let column = column_sql(*table, name);
                    let key = match self.column_type(*table, name).and_then(|ty| ty.term_key) {
                        Some(TermKey::Text) => TermKey::Text.sql(self.dialect, &column),
                        Some(key) => self.dialect.cast(
                            &format!("({})", key.sql(self.dialect, &column)),
                            &SqlType::Text,
                        ),
                        None => self.dialect.cast("NULL", &SqlType::Text),
                    };
                    self.sql.push_str(&key);
                }
                _ => self
                    .sql
                    .push_str(&self.dialect.cast("NULL", &SqlType::Text)),
            }
            self.sql.push_str(" AS ");
            self.sql.push_str(&quote_identifier(alias));
            selected += 1;
        }
        for (alias, key, slot) in &select.slots {
            if selected > 0 {
                self.sql.push_str(", ");
            }
            selected += 1;
            self.order_slot(branch, *key, &layouts[*key], *slot)?;
            if select.collate_slots && slot.collated() {
                self.sql.push_str(self.dialect.code_point_collation());
            }
            self.sql.push_str(" AS ");
            self.sql.push_str(&quote_identifier(alias));
        }

        self.required = true;
        if branch.from.is_empty() {
            // Every `OPTIONAL` part was left out: one row, of unbound values.
            if !branch.conditions.is_empty() {
                self.sql.push_str("\nWHERE ");
                let conditions: Vec<&Condition> = branch.conditions.iter().collect();
                self.conditions(&conditions)?;
            }
            return Ok(());
        }
        self.sql.push_str("\nFROM ");
        let filters = self.join_tree(branch, None)?;
        if !filters.is_empty() {
            self.sql.push_str("\nWHERE ");
            self.conditions(&filters)?;
        }
        Ok(())
    }

    /// The tables of `scope`, the whole branch or one of its `OPTIONAL`
    /// parts, joined: its own tables with `JOIN`, the parts nested in it
    /// with `LEFT JOIN`, in the order they are written. Returns the
    /// conditions of `scope` that are left for its `WHERE` or `ON` clause.
    ///
    /// A condition that links two tables of the scope goes to the `ON`
    /// clause of the later one, as long as that is a table of the scope
    /// rather than a nested part: a condition on a nested part's columns
    /// (its variables being unbound, or compatible) filters the rows of
    /// `scope` after the left join and must not decide whether the part
    /// matches.
    fn join_tree(
        &mut self,
        branch: &'a SqlBranch,
        scope: Option<usize>,
    ) -> Result<Vec<&'a Condition>, Error> {
        let (range, conditions) = match scope {
            None => (0..branch.from.len(), &branch.conditions),
            Some(j) => (
                branch.left_joins[j].tables.clone(),
                &branch.left_joins[j].conditions,
            ),
        };
        // The members of the scope: its own tables and its nested parts.
        let mut members: Vec<Member> = Vec::new();
        let mut member_of = vec![None; branch.from.len()];
        let mut t = range.start;
        while t < range.end {
            let nested =
                branch.left_joins.iter().enumerate().find(|(_, j)| {
                    j.parent == scope && j.tables.start == t && !j.tables.is_empty()
                });
            let end = match nested {
                Some((j, join)) => {
                    members.push(Member::Part(j));
                    join.tables.end
                }
                None => {
                    members.push(Member::Table(t));
                    t + 1
                }
            };
            for slot in &mut member_of[t..end] {
                *slot = Some(members.len() - 1);
            }
            t = end;
        }

        let mut on: Vec<Vec<&Condition>> = vec![Vec::new(); members.len()];
        let mut rest = Vec::new();
        for condition in conditions {
            let tables = condition_tables(condition);
            let owners: Option<BTreeSet<usize>> =
                tables.iter().map(|&table| member_of[table]).collect();
            match owners
                .as_ref()
                .and_then(|o| o.last().map(|&m| (o.len(), m)))
            {
                Some((n, last)) if n > 1 && matches!(members[last], Member::Table(_)) => {
                    on[last].push(condition)
                }
                _ => rest.push(condition),
            }
        }

        for (k, member) in members.iter().enumerate() {
            if k == 0 && matches!(member, Member::Part(_)) {
                // `{ OPTIONAL { ... } }`: the left side is the empty
                // pattern, whose one solution binds nothing.
                self.sql.push_str(&format!("(SELECT) u{}", self.units));
                self.units += 1;
            }
            match *member {
                Member::Table(i) => {
                    if k > 0 {
                        self.sql.push_str(if on[k].is_empty() {
                            "\nCROSS JOIN "
                        } else {
                            "\nJOIN "
                        });
                    }
                    self.sql.push_str(&logical_table(&branch.from[i]));
                    self.sql.push_str(&format!(" t{i}"));
                    if k > 0 && !on[k].is_empty() {
                        self.sql.push_str(" ON ");
                        self.conditions(&on[k])?;
                    }
                }
                Member::Part(part) => {
                    self.sql.push_str("\nLEFT JOIN ");
                    let required = std::mem::replace(&mut self.required, false);
                    let single = branch.left_joins[part].tables.len() == 1
                        && !branch.left_joins.iter().any(|j| j.parent == Some(part));
                    if !single {
                        self.sql.push('(');
                    }
                    let rest = self.join_tree(branch, Some(part))?;
                    if !single {
                        self.sql.push(')');
                    }
                    self.sql.push_str(" ON ");
                    if rest.is_empty() {
                        self.sql.push_str("true");
                    } else {
                        self.conditions(&rest)?;
                    }
                    self.required = required;
                }
            }
        }
        Ok(rest)
    }

    pub(super) fn conditions(&mut self, conditions: &[&Condition]) -> Result<(), Error> {
        for (i, condition) in conditions.iter().enumerate() {
            if i > 0 {
                self.sql.push_str(" AND ");
            }
            match condition {
                Condition::Eq(left, right) => {
                    // A column side holds no parameter, so the parameter
                    // on the other side is the next one either way.
                    let param = self.params.len();
                    let column = match (left, right) {
                        (SqlExpr::Column { table, name }, SqlExpr::Param(_))
                        | (SqlExpr::Param(_), SqlExpr::Column { table, name }) => {
                            Some(column_sql(*table, name))
                        }
                        _ => None,
                    };
                    let as_text = column.is_some() && self.options.text_params.contains(&param);
                    let key = column.as_ref().and_then(|_| self.param_key(param));
                    let normal = column.as_ref().and_then(|_| self.param_normal(param));
                    if key.is_some() || normal.is_some() {
                        self.sql.push('(');
                    }
                    self.side(left, as_text && matches!(left, SqlExpr::Column { .. }));
                    self.sql.push_str(" = ");
                    self.side(right, as_text && matches!(right, SqlExpr::Column { .. }));
                    if let (Some(key), Some(column)) = (key, &column) {
                        // `=` came first, so PostgreSQL has typed the
                        // parameter as the column before the key reads it.
                        self.sql.push_str(&format!(
                            " AND {} = {})",
                            key.sql(self.dialect, column),
                            key.sql(self.dialect, &format!("${}", param + 1))
                        ));
                    }
                    if let (Some(normal), Some(column)) = (normal, column) {
                        // A parameter bound to NULL matches nothing.
                        self.sql.push_str(&format!(
                            " OR (${} IS NOT NULL AND {column} = {}))",
                            param + 1,
                            normal.others()
                        ));
                    }
                }
                Condition::EqAny(left, right) => {
                    let first = self.params.len();
                    let column = match left {
                        SqlExpr::Column { table, name } => Some(column_sql(*table, name)),
                        _ => None,
                    };
                    let as_text = column.is_some() && self.options.text_params.contains(&first);
                    // Every alternative is one parameter of the column's type.
                    let key = column
                        .as_ref()
                        .and_then(|_| (first..first + right.len()).find_map(|p| self.param_key(p)));
                    if key.is_some() {
                        self.sql.push('(');
                    }
                    self.side(left, as_text);
                    self.sql.push_str(" IN (");
                    let group = self.alternatives;
                    self.alternatives += 1;
                    for (i, alternative) in right.iter().enumerate() {
                        if i > 0 {
                            self.sql.push_str(", ");
                        }
                        let before = self.params.len();
                        self.expr(alternative);
                        for slot in &mut self.param_alternatives[before..] {
                            *slot = Some(group);
                        }
                    }
                    self.sql.push(')');
                    if let (Some(key), Some(column)) = (key, column) {
                        let keys: Vec<String> = (first..self.params.len())
                            .map(|p| key.sql(self.dialect, &format!("${}", p + 1)))
                            .collect();
                        self.sql.push_str(&format!(
                            " AND {} IN ({}))",
                            key.sql(self.dialect, &column),
                            keys.join(", ")
                        ));
                    }
                }
                Condition::IsNotNull(expr) => {
                    self.expr(expr);
                    self.sql.push_str(" IS NOT NULL");
                }
                Condition::IsNull(expr) => {
                    self.expr(expr);
                    self.sql.push_str(" IS NULL");
                }
                Condition::Or(alternatives) => {
                    let required = std::mem::replace(&mut self.required, false);
                    self.sql.push('(');
                    for (i, alternative) in alternatives.iter().enumerate() {
                        if i > 0 {
                            self.sql.push_str(" OR ");
                        }
                        let conjunction: Vec<&Condition> = alternative.iter().collect();
                        if conjunction.len() > 1 {
                            self.sql.push('(');
                        }
                        self.conditions(&conjunction)?;
                        if conjunction.len() > 1 {
                            self.sql.push(')');
                        }
                    }
                    self.sql.push(')');
                    self.required = required;
                }
                Condition::SameTerm(left, right) => self.same_term(left, right)?,
                Condition::Filter(filter) => {
                    // A constant of a FILTER binds to NULL only where it is
                    // compared as an RDF term, which fails that comparison
                    // and not the branch.
                    let required = std::mem::replace(&mut self.required, false);
                    self.filter(filter, filter::Mode::Positive)?;
                    self.required = required;
                }
            }
        }
        Ok(())
    }

    /// The key that the comparison with parameter `param` (0-based) also
    /// compares, see [`RenderOptions::keyed_params`].
    fn param_key(&self, param: usize) -> Option<TermKey> {
        self.options
            .keyed_params
            .iter()
            .find(|(p, _)| *p == param)
            .map(|&(_, key)| key)
    }

    /// The normal form whose other values the comparison with parameter
    /// `param` (0-based) also matches, see [`RenderOptions::normal_params`].
    fn param_normal(&self, param: usize) -> Option<TermNormal> {
        self.options
            .normal_params
            .iter()
            .find(|(p, _)| *p == param)
            .map(|&(_, normal)| normal)
    }

    fn same_term(&mut self, left: &TermSql, right: &TermSql) -> Result<(), Error> {
        let Some(types) = self.options.column_types else {
            self.term_side(left, None, false)?;
            self.sql.push_str(" = ");
            return self.term_side(right, None, false);
        };
        if !self.datatypes_agree(left, right, types) {
            self.sql.push_str("false");
            return Ok(());
        }
        if let (
            TermSqlValue::Column {
                table: lt,
                name: ln,
            },
            TermSqlValue::Column {
                table: rt,
                name: rn,
            },
        ) = (&left.value, &right.value)
            && left.base_iri.is_none()
            && right.base_iri.is_none()
            && let (Some(a), Some(b)) = (self.column_type(*lt, ln), self.column_type(*rt, rn))
            && a.group == b.group
        {
            let (l, r) = (column_sql(*lt, ln), column_sql(*rt, rn));
            if let Some(normal) = a.term_normal.or(b.term_normal) {
                self.sql.push_str(&format!(
                    "{} = {}",
                    normal.sql(self.dialect, &l),
                    normal.sql(self.dialect, &r)
                ));
                return Ok(());
            }
            match (a.comparison, a.term_key.or(b.term_key)) {
                (Comparison::Value, Some(key)) => self.sql.push_str(&format!(
                    "({l} = {r} AND {} = {})",
                    key.sql(self.dialect, &l),
                    key.sql(self.dialect, &r)
                )),
                (Comparison::Value, None) => self.sql.push_str(&format!("{l} = {r}")),
                (Comparison::Text, _) => self.sql.push_str(&format!(
                    "{} = {}",
                    self.dialect.cast(&l, &SqlType::Text),
                    self.dialect.cast(&r, &SqlType::Text)
                )),
            }
            return Ok(());
        }
        self.term_side(left, Some(types), false)?;
        self.sql.push_str(" = ");
        self.term_side(right, Some(types), false)
    }

    /// R2RML §10.2: a literal from a column without `rr:datatype` has its
    /// SQL type's natural datatype, so it can only equal a literal with
    /// that datatype.
    fn datatypes_agree(&self, left: &TermSql, right: &TermSql, types: &ColumnTypes) -> bool {
        let datatype = |side: &TermSql| -> Option<String> {
            match (&side.datatype, &side.value) {
                (Some(TermDatatype::Iri(iri)), _) => Some(iri.clone()),
                (Some(TermDatatype::Natural), TermSqlValue::Column { table, name }) => {
                    let ty = types.get(&(self.from[*table].clone(), name.clone()))?;
                    Some(
                        ty.natural_datatype
                            .as_deref()
                            .unwrap_or(XSD_STRING)
                            .to_owned(),
                    )
                }
                _ => None,
            }
        };
        let natural = |side: &TermSql| side.datatype == Some(TermDatatype::Natural);
        if !natural(left) && !natural(right) {
            return true;
        }
        match (datatype(left), datatype(right)) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        }
    }

    pub(super) fn column_type(&self, table: usize, name: &str) -> Option<&'a ColumnType> {
        self.options
            .column_types?
            .get(&(self.from[table].clone(), name.to_owned()))
    }

    /// One side of a term comparison as a string expression. Without
    /// `types`, columns are rendered as they are, or by their text output
    /// with `text`, and template columns by their text output.
    ///
    /// With a base IRI, the side is the IRI R2RML §11.2 generates: `CASE
    /// WHEN s ~ '^scheme:' THEN s ELSE $base || s END`, the same test as
    /// [`has_scheme`](crate::mapping::has_scheme), with `s` rendered three
    /// times, since a column is the common case and costs nothing to repeat.
    pub(super) fn term_side(
        &mut self,
        side: &TermSql,
        types: Option<&ColumnTypes>,
        text: bool,
    ) -> Result<(), Error> {
        let Some(base) = &side.base_iri else {
            return self.term_string(side, types, text);
        };
        self.sql.push_str("CASE WHEN ");
        self.wrapped(
            |this| this.term_string(side, types, true),
            |dialect, s| dialect.regex_match(&format!("({s})"), &format!("'{SCHEME_REGEX}'")),
        )?;
        self.sql.push_str(" THEN (");
        self.term_string(side, types, true)?;
        self.sql.push_str(") ELSE ");
        self.expr(&SqlExpr::Param(base.as_str().into()));
        self.sql.push_str(" || (");
        self.term_string(side, types, true)?;
        self.sql.push_str(") END");
        Ok(())
    }

    /// The string of one side; `text` casts a column without `types` to
    /// text, which comparing it with a pattern needs.
    pub(super) fn term_string(
        &mut self,
        side: &TermSql,
        types: Option<&ColumnTypes>,
        text: bool,
    ) -> Result<(), Error> {
        match &side.value {
            TermSqlValue::Column { table, name } => match types {
                None if text => self
                    .sql
                    .push_str(&self.dialect.cast(&column_sql(*table, name), &SqlType::Text)),
                None => self.sql.push_str(&column_sql(*table, name)),
                Some(_) => {
                    let lexical = self.lexical(*table, name)?;
                    self.sql.push_str(&lexical);
                }
            },
            TermSqlValue::Template {
                table,
                parts,
                iri_safe,
            } => {
                for (i, part) in parts.iter().enumerate() {
                    if i > 0 {
                        self.sql.push_str(" || ");
                    }
                    match part {
                        TemplatePart::Literal(text) => {
                            self.expr(&SqlExpr::Param(text.as_str().into()))
                        }
                        TemplatePart::Column(name) => {
                            let (value, unreserved) = match types {
                                None => (
                                    self.dialect.cast(&column_sql(*table, name), &SqlType::Text),
                                    false,
                                ),
                                Some(_) => (
                                    self.lexical(*table, name)?,
                                    self.column_type(*table, name)
                                        .is_some_and(|ty| ty.lexical_unreserved),
                                ),
                            };
                            // IRI-safe encoding keeps a string of
                            // `iunreserved` characters as it is.
                            self.sql.push_str(&if *iri_safe && !unreserved {
                                self.dialect.iri_safe(&value)
                            } else {
                                value
                            });
                        }
                    }
                }
            }
            TermSqlValue::Param(param) => self.expr(&SqlExpr::Param(param.clone())),
        }
        Ok(())
    }

    /// SQL for the natural lexical form of a column.
    fn lexical(&self, table: usize, name: &str) -> Result<String, Error> {
        let column = column_sql(table, name);
        let Some(ty) = self.column_type(table, name) else {
            return Ok(self.dialect.cast(&column, &SqlType::Text));
        };
        match ty.lexical {
            Some(lexical) => Ok(lexical_sql(self.dialect, &column, lexical)),
            None => Err(Error::UnsupportedFeature(format!(
                "comparing terms built from column {name} of {} ({}) with terms of a \
                 different SQL type or shape: its natural lexical form cannot be computed in SQL",
                describe_table(&self.from[table]),
                ty.name
            ))),
        }
    }

    /// An entry of a select list of `branch`: [`SqlExpr::Term`],
    /// [`SqlExpr::Value`] or any other.
    fn select_expr(&mut self, branch: &SqlBranch, expr: &SqlExpr) -> Result<(), Error> {
        if let SqlExpr::Value { value, kind } = expr {
            return self.value(&branch.values[*value], *kind);
        }
        if let SqlExpr::Column { table, name } = expr
            && let Some(normal) = self.column_type(*table, name).and_then(|ty| ty.term_normal)
        {
            // Values with the same term are one value for `DISTINCT`,
            // `UNION` and the decoder.
            self.sql
                .push_str(&normal.sql(self.dialect, &column_sql(*table, name)));
            return Ok(());
        }
        let SqlExpr::Term { term, guard } = expr else {
            self.expr(expr);
            return Ok(());
        };
        if let Some(guard) = guard {
            self.sql.push_str("CASE WHEN ");
            self.expr(guard);
            self.sql.push_str(" IS NOT NULL THEN ");
        }
        // Text in every branch, so that `UNION` combines them.
        let types = self.options.column_types;
        self.wrapped(
            |this| this.term_side(term, types, true),
            |dialect, term| dialect.cast(&format!("({term})"), &SqlType::Text),
        )?;
        if guard.is_some() {
            self.sql.push_str(" END");
        }
        Ok(())
    }

    /// `expr`, as text with `text`.
    fn side(&mut self, expr: &SqlExpr, text: bool) {
        if text {
            self.cast_rendered(&SqlType::Text, |this| this.expr(expr));
        } else {
            self.expr(expr);
        }
    }

    pub(super) fn expr(&mut self, expr: &SqlExpr) {
        match expr {
            SqlExpr::Column { table, name } => self.sql.push_str(&column_sql(*table, name)),
            SqlExpr::Param(param) => {
                self.params.push(param.value.clone());
                self.param_datatypes.push(param.datatype.clone());
                self.param_branches.push(self.branch_index);
                self.param_required.push(self.required);
                self.param_alternatives.push(None);
                self.sql.push_str(&format!("${}", self.params.len()));
            }
            SqlExpr::Concat(parts) => {
                for (i, part) in parts.iter().enumerate() {
                    if i > 0 {
                        self.sql.push_str(" || ");
                    }
                    // An untyped parameter next to `||` already resolves to text.
                    self.side(part, matches!(part, SqlExpr::Column { .. }));
                }
            }
            SqlExpr::Lexical {
                table,
                name,
                lexical,
            } => self.sql.push_str(&lexical_sql(
                self.dialect,
                &column_sql(*table, name),
                *lexical,
            )),
            // Typed, since `UNION` resolves an untyped NULL of two branches
            // to text before it sees the third.
            SqlExpr::Null(ty) => self.sql.push_str(&self.dialect.cast("NULL", ty)),
            SqlExpr::Unbound => self.sql.push_str("NULL"),
            SqlExpr::Matched(inner) => {
                self.sql.push_str("CASE WHEN ");
                self.expr(inner);
                self.sql.push_str(" IS NOT NULL THEN true END");
            }
            SqlExpr::True => self.sql.push_str("true"),
            SqlExpr::Term { .. } | SqlExpr::Value { .. } => {
                unreachable!("only select lists hold terms and values")
            }
        }
    }
}

pub(super) fn column_sql(table: usize, name: &str) -> String {
    format!("t{table}.{}", quote_identifier(name))
}

/// SQL for the natural lexical form of `column`.
fn lexical_sql(dialect: &dyn Dialect, column: &str, lexical: Lexical) -> String {
    match lexical {
        Lexical::Text => dialect.cast(column, &SqlType::Text),
        Lexical::Boolean => {
            format!("CASE {column} WHEN true THEN 'true' WHEN false THEN 'false' END")
        }
        Lexical::Hex => dialect.hex_lexical(column),
        Lexical::Decimal => dialect.decimal_lexical(column),
    }
}

/// Table aliases a condition refers to, in ascending order.
fn condition_tables(condition: &Condition) -> Vec<usize> {
    fn collect(expr: &SqlExpr, out: &mut BTreeSet<usize>) {
        match expr {
            SqlExpr::Column { table, .. } | SqlExpr::Lexical { table, .. } => {
                out.insert(*table);
            }
            SqlExpr::Param(_)
            | SqlExpr::Null(_)
            | SqlExpr::Unbound
            | SqlExpr::True
            | SqlExpr::Value { .. } => {}
            SqlExpr::Concat(parts) => parts.iter().for_each(|p| collect(p, out)),
            SqlExpr::Matched(inner) => collect(inner, out),
            SqlExpr::Term { term, guard } => {
                out.extend(term.columns().into_iter().map(|(table, _)| table));
                if let Some(guard) = guard {
                    collect(guard, out);
                }
            }
        }
    }
    fn tables(condition: &Condition, out: &mut BTreeSet<usize>) {
        match condition {
            Condition::Eq(left, right) => {
                collect(left, out);
                collect(right, out);
            }
            Condition::EqAny(left, right) => {
                collect(left, out);
                right.iter().for_each(|e| collect(e, out));
            }
            Condition::IsNotNull(expr) | Condition::IsNull(expr) => collect(expr, out),
            Condition::Or(alternatives) => {
                alternatives.iter().flatten().for_each(|c| tables(c, out))
            }
            Condition::SameTerm(left, right) => {
                for (table, _) in left.columns().into_iter().chain(right.columns()) {
                    out.insert(table);
                }
            }
            Condition::Filter(filter) => {
                let (conditions, terms) = filter.parts();
                for c in conditions {
                    tables(c, out);
                }
                for t in terms {
                    out.extend(t.columns().into_iter().map(|(table, _)| table));
                }
            }
        }
    }
    let mut out = BTreeSet::new();
    tables(condition, &mut out);
    out.into_iter().collect()
}

#[cfg(test)]
pub(super) mod tests {
    use super::super::Param;
    use super::*;

    fn optional_plan(query: &str) -> SqlPlan {
        let mapping = crate::mapping::Mapping::from_turtle(include_str!(
            "../../../../examples/shop/mapping.ttl"
        ))
        .unwrap();
        let index = crate::mapping::MappingIndex::new(&mapping);
        let query =
            crate::sparql::parse(&format!("PREFIX ex: <http://example.org/>\n{query}")).unwrap();
        crate::unfold::unfold(&query, &mapping, &index).unwrap()
    }

    #[test]
    fn to_sql_nests_optional_parts_and_keeps_their_conditions_in_on() {
        let plan = optional_plan(
            "SELECT ?c ?o ?a WHERE { ?c ex:tier \"VIP\" \
             OPTIONAL { ?o ex:placedBy ?c OPTIONAL { ?o ex:amount ?a } } }",
        );
        let rendered = plan.to_sql().unwrap();
        assert_eq!(
            rendered.sql,
            "SELECT DISTINCT t0.\"id\" AS \"c\", t1.\"id\" AS \"o\", t2.\"amount\" AS \"a\"\n\
             FROM \"customers\" t0\n\
             LEFT JOIN (\"orders\" t1\n\
             LEFT JOIN \"orders\" t2 ON t2.\"id\" = t1.\"id\" AND t2.\"amount\" IS NOT NULL) \
             ON t1.\"customer_id\" = t0.\"id\" AND t1.\"id\" IS NOT NULL\n\
             WHERE t0.\"tier\" = $1 AND t0.\"id\" IS NOT NULL"
        );
        assert_eq!(rendered.param_required, [true]);
    }

    /// A constant that cannot match fails its own condition: in an
    /// `OPTIONAL` part, the part does not match, and the branch stays.
    #[test]
    fn to_sql_marks_parameters_of_optional_parts_as_not_required() {
        let plan = optional_plan(
            "SELECT ?c ?o WHERE { ?c ex:tier \"VIP\" OPTIONAL { ?o ex:placedBy ?c ; ex:amount 100 } }",
        );
        let rendered = plan.to_sql().unwrap();
        assert_eq!(rendered.params, ["100", "VIP"]);
        assert_eq!(rendered.param_required, [false, true]);
    }

    #[test]
    fn to_sql_puts_conditions_on_an_optional_part_after_the_left_join() {
        // ?c may be unbound after the OPTIONAL: the last pattern must not
        // decide whether the part matches.
        let plan = optional_plan(
            "SELECT ?o ?n WHERE { ?o ex:amount ?a OPTIONAL { ?o ex:placedBy ?c } ?c ex:name ?n }",
        );
        assert_eq!(
            plan.to_sql().unwrap().sql,
            "SELECT q.\"o\", q.\"n\"\nFROM (\n\
             SELECT DISTINCT t0.\"id\" AS \"o\", t0.\"amount\" AS \"a\", t2.\"id\" AS \"c\", \
             t2.\"name\" AS \"n\"\n\
             FROM \"orders\" t0\n\
             LEFT JOIN \"orders\" t1 ON t1.\"id\" = t0.\"id\" AND t1.\"customer_id\" IS NOT NULL\n\
             JOIN \"customers\" t2 ON (t1.\"customer_id\" IS NULL OR t2.\"id\" = t1.\"customer_id\")\n\
             WHERE t0.\"id\" IS NOT NULL AND t0.\"amount\" IS NOT NULL AND t2.\"id\" IS NOT NULL \
             AND t2.\"name\" IS NOT NULL\n\
             ) q"
        );
    }

    #[test]
    fn to_sql_starts_from_one_row_when_the_left_side_is_empty() {
        let plan = optional_plan("SELECT ?c WHERE { OPTIONAL { ?c ex:tier ?t } }");
        assert!(
            plan.to_sql()
                .unwrap()
                .sql
                .contains("FROM (SELECT) u0\nLEFT JOIN \"customers\" t0 ON "),
            "{}",
            plan.to_sql().unwrap().sql
        );
    }

    pub(in crate::sql) fn column(table: usize, name: &str) -> SqlExpr {
        SqlExpr::Column {
            table,
            name: name.to_owned(),
        }
    }

    #[test]
    fn to_sql_places_linking_conditions_in_on_and_numbers_params_in_order() {
        let plan = SqlPlan {
            order: Vec::new(),
            offset: 0,
            limit: None,
            columns: vec!["x".into()],
            hidden_columns: Vec::new(),
            distinct_order: Vec::new(),
            split_columns: Vec::new(),
            lexical_hidden_columns: Default::default(),
            projection: Vec::new(),
            branches: vec![SqlBranch {
                order: Vec::new(),
                values: Vec::new(),
                arm: 0,
                from: vec![
                    LogicalTable::TableName("a".into()),
                    LogicalTable::SqlQuery("SELECT * FROM b;\n".into()),
                    LogicalTable::TableName("c".into()),
                ],
                select: vec![column(0, "id")],
                conditions: vec![
                    Condition::Eq(column(0, "k"), SqlExpr::Param("one".into())),
                    Condition::Eq(
                        column(1, "id"),
                        SqlExpr::Concat(vec![SqlExpr::Param("p/".into()), column(0, "id")]),
                    ),
                    Condition::IsNotNull(column(2, "v")),
                ],
                left_joins: Vec::new(),
                fixed_columns: Vec::new(),
            }],
            distinct: true,
            base_iri: None,
        };
        let rendered = plan.to_sql().unwrap();
        assert_eq!(
            rendered.sql,
            "SELECT DISTINCT t0.\"id\" AS \"x\"\n\
             FROM \"a\" t0\n\
             JOIN (SELECT * FROM b) t1 ON t1.\"id\" = $1 || t0.\"id\"::text\n\
             CROSS JOIN \"c\" t2\n\
             WHERE t0.\"k\" = $2 AND t2.\"v\" IS NOT NULL"
        );
        assert_eq!(rendered.params, ["p/", "one"]);
        assert_eq!(rendered.param_branches, [0, 0]);
    }

    #[test]
    fn to_sql_with_text_params_casts_the_compared_column() {
        let branch = |value: &str, datatype: Option<&str>| SqlBranch {
            order: Vec::new(),
            values: Vec::new(),
            arm: 0,
            from: vec![LogicalTable::TableName("t".into())],
            select: vec![column(0, "id")],
            conditions: vec![
                Condition::Eq(
                    column(0, "a"),
                    SqlExpr::Concat(vec![SqlExpr::Param("x/".into()), column(0, "b")]),
                ),
                Condition::Eq(
                    column(0, "v"),
                    SqlExpr::Param(Param {
                        value: value.into(),
                        datatype: datatype.map(Into::into),
                    }),
                ),
            ],
            left_joins: Vec::new(),
            fixed_columns: Vec::new(),
        };
        let plan = SqlPlan {
            order: Vec::new(),
            offset: 0,
            limit: None,
            columns: vec!["x".into()],
            hidden_columns: Vec::new(),
            distinct_order: Vec::new(),
            split_columns: Vec::new(),
            lexical_hidden_columns: Default::default(),
            projection: Vec::new(),
            branches: vec![branch("1", None), branch("2", Some("http://ex/dt"))],
            distinct: false,
            base_iri: None,
        };
        let rendered = plan
            .to_sql_with(
                &PostgresDialect,
                &RenderOptions {
                    text_params: &[3],
                    ..RenderOptions::default()
                },
            )
            .unwrap();
        assert_eq!(
            rendered.sql,
            "SELECT t0.\"id\" AS \"x\"\n\
             FROM \"t\" t0\n\
             WHERE t0.\"a\" = $1 || t0.\"b\"::text AND t0.\"v\" = $2\n\
             UNION\n\
             SELECT t0.\"id\" AS \"x\"\n\
             FROM \"t\" t0\n\
             WHERE t0.\"a\" = $3 || t0.\"b\"::text AND t0.\"v\"::text = $4"
        );
        assert_eq!(rendered.params, ["x/", "1", "x/", "2"]);
        assert_eq!(
            rendered.param_datatypes,
            [None, None, None, Some("http://ex/dt".to_owned())]
        );
        assert_eq!(rendered.param_branches, [0, 0, 1, 1]);
        assert_eq!(
            plan.to_sql().unwrap().sql,
            rendered.sql.replace("::text = $4", " = $4")
        );
    }

    #[test]
    fn keyed_params_also_compare_the_key_after_the_value() {
        let branch = SqlBranch {
            order: Vec::new(),
            values: Vec::new(),
            arm: 0,
            from: vec![LogicalTable::TableName("t".into())],
            select: vec![column(0, "id")],
            conditions: vec![
                Condition::Eq(column(0, "f"), SqlExpr::Param("0.0E0".into())),
                Condition::EqAny(
                    column(0, "g"),
                    vec![SqlExpr::Param("a".into()), SqlExpr::Param("b".into())],
                ),
                Condition::Eq(column(0, "h"), SqlExpr::Param("1.5E0".into())),
            ],
            left_joins: Vec::new(),
            fixed_columns: Vec::new(),
        };
        let plan = SqlPlan {
            order: Vec::new(),
            offset: 0,
            limit: None,
            columns: vec!["x".into()],
            hidden_columns: Vec::new(),
            distinct_order: Vec::new(),
            split_columns: Vec::new(),
            lexical_hidden_columns: Default::default(),
            projection: Vec::new(),
            branches: vec![branch],
            distinct: false,
            base_iri: None,
        };
        let rendered = plan
            .to_sql_with(
                &PostgresDialect,
                &RenderOptions {
                    keyed_params: &[(0, TermKey::Text), (2, TermKey::OctetLength)],
                    ..RenderOptions::default()
                },
            )
            .unwrap();
        // The value comparison stays, so an index on the column finds the
        // candidates; the key only filters them.
        assert_eq!(
            rendered.sql,
            "SELECT DISTINCT t0.\"id\" AS \"x\"\n\
             FROM \"t\" t0\n\
             WHERE (t0.\"f\" = $1 AND t0.\"f\"::text = $1::text) \
             AND (t0.\"g\" IN ($2, $3) AND octet_length(t0.\"g\") IN (octet_length($2), octet_length($3))) \
             AND t0.\"h\" = $4"
        );
        assert_eq!(rendered.params, ["0.0E0", "a", "b", "1.5E0"]);
    }

    #[test]
    fn removing_duplicates_also_compares_the_keys_of_keyed_columns() {
        let table = LogicalTable::TableName("t".into());
        let plan = SqlPlan {
            order: Vec::new(),
            offset: 0,
            limit: None,
            columns: vec!["x".into(), "n".into()],
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
                select: vec![column(0, "f"), column(0, "n")],
                conditions: Vec::new(),
                left_joins: Vec::new(),
                fixed_columns: vec![BTreeSet::new()],
            }],
            distinct: true,
            base_iri: None,
        };
        let double = ColumnType {
            term_key: Some(TermKey::Text),
            term_normal: None,
            ..ty("double", Comparison::Text, None)
        };
        let types: ColumnTypes = [
            ((table.clone(), "f".to_owned()), double),
            (
                (table, "n".to_owned()),
                ty("integer", Comparison::Value, Some(Lexical::Text)),
            ),
        ]
        .into();
        let rendered = plan
            .to_sql_with(
                &PostgresDialect,
                &RenderOptions {
                    column_types: Some(&types),
                    ..RenderOptions::default()
                },
            )
            .unwrap();
        // `DISTINCT` would merge -0 and 0; the key is compared too, and
        // left out of the results.
        assert_eq!(
            rendered.sql,
            "SELECT q.\"x\", q.\"n\"\n\
             FROM (\n\
             SELECT DISTINCT t0.\"f\" AS \"x\", t0.\"n\" AS \"n\", t0.\"f\"::text AS \"x_key\"\n\
             FROM \"t\" t0\n\
             ) q"
        );
        // Without the column types, as `v12n explain` shows it.
        assert_eq!(
            plan.to_sql().unwrap().sql,
            "SELECT DISTINCT t0.\"f\" AS \"x\", t0.\"n\" AS \"n\"\nFROM \"t\" t0"
        );
    }

    fn ty(group: &str, comparison: Comparison, lexical: Option<Lexical>) -> ColumnType {
        ColumnType {
            name: group.to_owned(),
            sql_type: format!("pg_catalog.{group}"),
            union_class: group.to_owned(),
            natural_datatype: (group == "integer")
                .then(|| "http://www.w3.org/2001/XMLSchema#integer".to_owned()),
            group: group.to_owned(),
            comparison,
            lexical,
            lexical_unreserved: unreserved(group),
            term_key: None,
            term_normal: None,
        }
    }

    /// The groups whose lexical forms the adapter marks as unreserved.
    pub(in crate::sql) fn unreserved(group: &str) -> bool {
        matches!(group, "integer" | "numeric" | "boolean" | "uuid" | "bytea")
    }

    fn side(table: usize, name: &str, datatype: Option<TermDatatype>) -> TermSql {
        TermSql {
            value: TermSqlValue::Column {
                table,
                name: name.to_owned(),
            },
            datatype,
            base_iri: None,
        }
    }

    /// `WHERE` of a two-table branch with one `SameTerm`, rendered with
    /// the given column types for `a` (table `l`) and `b` (table `r`).
    fn same_term_sql(
        left: TermSql,
        right: TermSql,
        a: ColumnType,
        b: ColumnType,
    ) -> Result<String, Error> {
        let (l, r) = (
            LogicalTable::TableName("l".into()),
            LogicalTable::TableName("r".into()),
        );
        let plan = SqlPlan {
            order: Vec::new(),
            offset: 0,
            limit: None,
            columns: Vec::new(),
            hidden_columns: Vec::new(),
            distinct_order: Vec::new(),
            split_columns: Vec::new(),
            lexical_hidden_columns: Default::default(),
            projection: Vec::new(),
            branches: vec![SqlBranch {
                order: Vec::new(),
                values: Vec::new(),
                arm: 0,
                from: vec![l.clone(), r.clone()],
                select: Vec::new(),
                conditions: vec![Condition::SameTerm(left, right)],
                left_joins: Vec::new(),
                fixed_columns: Vec::new(),
            }],
            distinct: false,
            base_iri: None,
        };
        assert_eq!(plan.term_columns().len(), 2);
        let types: ColumnTypes = [((l, "a".to_owned()), a), ((r, "b".to_owned()), b)].into();
        let rendered = plan.to_sql_with(
            &PostgresDialect,
            &RenderOptions {
                column_types: Some(&types),
                ..RenderOptions::default()
            },
        )?;
        Ok(rendered.sql.split(" ON ").nth(1).unwrap().to_owned())
    }

    #[test]
    fn same_term_with_column_types_compares_as_the_group_says() {
        let int = || ty("integer", Comparison::Value, Some(Lexical::Text));
        let text = || ty("text", Comparison::Value, Some(Lexical::Text));
        let double = || ty("double", Comparison::Text, None);
        let boolean = || ty("boolean", Comparison::Value, Some(Lexical::Boolean));
        let numeric = || ty("numeric", Comparison::Value, Some(Lexical::Decimal));
        let plain = |t, n| side(t, n, None);
        let natural = |t, n| side(t, n, Some(TermDatatype::Natural));

        // Same group: `=` keeps indexes usable.
        assert_eq!(
            same_term_sql(plain(0, "a"), plain(1, "b"), int(), int()).unwrap(),
            r#"t0."a" = t1."b""#
        );
        assert_eq!(
            same_term_sql(plain(0, "a"), plain(1, "b"), double(), double()).unwrap(),
            r#"t0."a"::text = t1."b"::text"#
        );
        // bpchar's `=` ignores trailing spaces, which its terms keep.
        let bpchar = || ColumnType {
            term_key: Some(TermKey::OctetLength),
            term_normal: None,
            ..ty("bpchar", Comparison::Value, None)
        };
        assert_eq!(
            same_term_sql(plain(0, "a"), plain(1, "b"), bpchar(), bpchar()).unwrap(),
            r#"(t0."a" = t1."b" AND octet_length(t0."a") = octet_length(t1."b"))"#
        );
        // Across groups: natural lexical forms, never an operator error.
        assert_eq!(
            same_term_sql(plain(0, "a"), plain(1, "b"), int(), text()).unwrap(),
            r#"t0."a"::text = t1."b"::text"#
        );
        assert_eq!(
            same_term_sql(plain(0, "a"), plain(1, "b"), boolean(), text()).unwrap(),
            r#"CASE t0."a" WHEN true THEN 'true' WHEN false THEN 'false' END = t1."b"::text"#
        );
        // Natural datatypes that differ never give the same literal.
        assert_eq!(
            same_term_sql(natural(0, "a"), natural(1, "b"), int(), text()).unwrap(),
            "false"
        );
        let integer = TermDatatype::Iri("http://www.w3.org/2001/XMLSchema#integer".into());
        assert_eq!(
            same_term_sql(natural(0, "a"), side(1, "b", Some(integer)), int(), text()).unwrap(),
            r#"t0."a"::text = t1."b"::text"#
        );
        // numeric: the canonical decimal, computed by trim_scale.
        assert_eq!(
            same_term_sql(plain(0, "a"), plain(1, "b"), numeric(), int()).unwrap(),
            r#"trim_scale(t0."a")::text = t1."b"::text"#
        );
        // No SQL for double's canonical form: rejected, not guessed.
        let err = same_term_sql(plain(0, "a"), plain(1, "b"), double(), int()).unwrap_err();
        assert!(
            matches!(&err, Error::UnsupportedFeature(m) if m.contains("column a of table l (double)")),
            "{err}"
        );
    }

    #[test]
    fn to_sql_without_branches_returns_no_rows() {
        let plan = SqlPlan {
            order: Vec::new(),
            offset: 0,
            limit: None,
            columns: Vec::new(),
            hidden_columns: Vec::new(),
            distinct_order: Vec::new(),
            split_columns: Vec::new(),
            lexical_hidden_columns: Default::default(),
            projection: Vec::new(),
            branches: Vec::new(),
            distinct: false,
            base_iri: None,
        };
        assert_eq!(plan.to_sql().unwrap().sql, "SELECT 1 WHERE false");
    }
}
