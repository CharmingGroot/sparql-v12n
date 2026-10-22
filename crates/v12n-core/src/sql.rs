// SPDX-License-Identifier: Apache-2.0
//! SQL plan (`SqlPlan`) and PostgreSQL text rendering with bound parameters.

use std::collections::BTreeSet;

use oxrdf::Term;

use crate::mapping::{LogicalTable, TemplatePart, TermType};

/// An unfolded query: one `SELECT` per branch, combined with `UNION ALL`
/// (or `UNION` under `DISTINCT`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlPlan {
    /// Result column aliases, shared by every branch.
    pub columns: Vec<String>,
    /// How to rebuild each projected variable from the result columns.
    pub projection: Vec<ProjectedTerm>,
    /// No branches means the query has no answers.
    pub branches: Vec<SqlBranch>,
    pub distinct: bool,
}

/// One `SELECT ... FROM ... WHERE ...` of the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlBranch {
    /// Logical tables; the one at position `n` is aliased `tn`.
    pub from: Vec<LogicalTable>,
    /// One expression per entry of [`SqlPlan::columns`].
    pub select: Vec<SqlExpr>,
    /// Conjunction of conditions. Rendering puts a condition that links
    /// two tables in the `ON` clause of the later one, the rest in `WHERE`.
    pub conditions: Vec<Condition>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlExpr {
    /// Column `name` of table alias `t{table}`.
    Column { table: usize, name: String },
    /// A constant, always sent as a bound parameter.
    Param(String),
    /// String concatenation of the parts, each cast to text.
    Concat(Vec<SqlExpr>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition {
    Eq(SqlExpr, SqlExpr),
    IsNotNull(SqlExpr),
}

/// A projected variable and how to rebuild its RDF term from a result row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedTerm {
    pub variable: String,
    /// `None` when the variable never occurs in the pattern, so it is
    /// unbound in every solution.
    pub term: Option<ResultTerm>,
}

/// The term map a projected variable takes its values from, with column
/// names replaced by result column aliases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultTerm {
    pub term_type: TermType,
    pub value: ResultValue,
    pub datatype: Option<String>,
    pub language: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResultValue {
    Constant(Term),
    Column(String),
    /// Template parts; [`TemplatePart::Column`] holds a result column alias.
    Template(Vec<TemplatePart>),
}

impl ResultValue {
    /// Result column aliases this value is built from, in order.
    pub fn columns(&self) -> Vec<&str> {
        match self {
            Self::Constant(_) => Vec::new(),
            Self::Column(alias) => vec![alias.as_str()],
            Self::Template(parts) => parts
                .iter()
                .filter_map(|p| match p {
                    TemplatePart::Column(alias) => Some(alias.as_str()),
                    TemplatePart::Literal(_) => None,
                })
                .collect(),
        }
    }
}

/// PostgreSQL query text and its parameters (`$1` is `params[0]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedSql {
    pub sql: String,
    pub params: Vec<String>,
}

impl SqlPlan {
    /// Renders the plan. Parameters are numbered in the order they appear
    /// in the text.
    pub fn to_sql(&self) -> RenderedSql {
        let mut r = Renderer::default();
        if self.branches.is_empty() {
            r.sql.push_str("SELECT 1 WHERE false");
        }
        let single = self.branches.len() == 1;
        for (i, branch) in self.branches.iter().enumerate() {
            if i > 0 {
                r.sql.push_str(if self.distinct {
                    "\nUNION\n"
                } else {
                    "\nUNION ALL\n"
                });
            }
            r.branch(branch, &self.columns, self.distinct && single);
        }
        RenderedSql {
            sql: r.sql,
            params: r.params,
        }
    }
}

#[derive(Default)]
struct Renderer {
    sql: String,
    params: Vec<String>,
}

impl Renderer {
    fn branch(&mut self, branch: &SqlBranch, columns: &[String], distinct: bool) {
        self.sql.push_str("SELECT ");
        if distinct {
            self.sql.push_str("DISTINCT ");
        }
        if branch.select.is_empty() {
            self.sql.push('1');
        }
        for (i, (expr, alias)) in branch.select.iter().zip(columns).enumerate() {
            if i > 0 {
                self.sql.push_str(", ");
            }
            self.expr(expr);
            self.sql.push_str(" AS ");
            self.sql.push_str(&quote_identifier(alias));
        }

        let mut on: Vec<Vec<&Condition>> = vec![Vec::new(); branch.from.len()];
        let mut filters = Vec::new();
        for condition in &branch.conditions {
            let tables = condition_tables(condition);
            match tables.last() {
                Some(&last) if tables.len() > 1 => on[last].push(condition),
                _ => filters.push(condition),
            }
        }
        for (i, table) in branch.from.iter().enumerate() {
            self.sql.push('\n');
            if i > 0 {
                self.sql.push_str(if on[i].is_empty() {
                    "CROSS JOIN "
                } else {
                    "JOIN "
                });
            } else {
                self.sql.push_str("FROM ");
            }
            self.sql.push_str(&logical_table(table));
            self.sql.push_str(&format!(" t{i}"));
            if i > 0 && !on[i].is_empty() {
                self.sql.push_str(" ON ");
                self.conditions(&on[i]);
            }
        }
        if !filters.is_empty() {
            self.sql.push_str("\nWHERE ");
            self.conditions(&filters);
        }
    }

    fn conditions(&mut self, conditions: &[&Condition]) {
        for (i, condition) in conditions.iter().enumerate() {
            if i > 0 {
                self.sql.push_str(" AND ");
            }
            match condition {
                Condition::Eq(left, right) => {
                    self.expr(left);
                    self.sql.push_str(" = ");
                    self.expr(right);
                }
                Condition::IsNotNull(expr) => {
                    self.expr(expr);
                    self.sql.push_str(" IS NOT NULL");
                }
            }
        }
    }

    fn expr(&mut self, expr: &SqlExpr) {
        match expr {
            SqlExpr::Column { table, name } => {
                self.sql
                    .push_str(&format!("t{table}.{}", quote_identifier(name)));
            }
            SqlExpr::Param(value) => {
                self.params.push(value.clone());
                self.sql.push_str(&format!("${}", self.params.len()));
            }
            SqlExpr::Concat(parts) => {
                for (i, part) in parts.iter().enumerate() {
                    if i > 0 {
                        self.sql.push_str(" || ");
                    }
                    self.expr(part);
                    // An untyped parameter next to `||` already resolves to text.
                    if matches!(part, SqlExpr::Column { .. }) {
                        self.sql.push_str("::text");
                    }
                }
            }
        }
    }
}

/// Table aliases a condition refers to, in ascending order.
fn condition_tables(condition: &Condition) -> Vec<usize> {
    fn collect(expr: &SqlExpr, out: &mut BTreeSet<usize>) {
        match expr {
            SqlExpr::Column { table, .. } => {
                out.insert(*table);
            }
            SqlExpr::Param(_) => {}
            SqlExpr::Concat(parts) => parts.iter().for_each(|p| collect(p, out)),
        }
    }
    let mut tables = BTreeSet::new();
    match condition {
        Condition::Eq(left, right) => {
            collect(left, &mut tables);
            collect(right, &mut tables);
        }
        Condition::IsNotNull(expr) => collect(expr, &mut tables),
    }
    tables.into_iter().collect()
}

fn logical_table(table: &LogicalTable) -> String {
    match table {
        LogicalTable::TableName(name) => quote_table_name(name),
        LogicalTable::SqlQuery(query) => {
            format!("({})", query.trim().trim_end_matches(';').trim_end())
        }
    }
}

/// Quotes an identifier from the mapping. An identifier that is already
/// a valid delimited identifier (`"Order Id"`) is kept as written, so
/// R2RML mappings can use either form; anything else is wrapped in double
/// quotes with embedded quotes doubled, which makes it impossible to
/// break out of the identifier.
pub fn quote_identifier(name: &str) -> String {
    let delimited = name.len() >= 2
        && name.starts_with('"')
        && name.ends_with('"')
        && !name[1..name.len() - 1].replace("\"\"", "").contains('"');
    if delimited {
        name.to_owned()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// Quotes each part of a possibly schema-qualified `rr:tableName`.
pub fn quote_table_name(name: &str) -> String {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    for (i, c) in name.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '.' if !quoted => {
                parts.push(&name[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&name[start..]);
    parts
        .into_iter()
        .map(quote_identifier)
        .collect::<Vec<_>>()
        .join(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_identifier_plain_name_is_wrapped() {
        assert_eq!(quote_identifier("customers"), r#""customers""#);
        assert_eq!(quote_identifier("Order Id"), r#""Order Id""#);
    }

    #[test]
    fn quote_identifier_delimited_name_is_kept() {
        assert_eq!(quote_identifier(r#""Order Id""#), r#""Order Id""#);
        assert_eq!(quote_identifier(r#""a""b""#), r#""a""b""#);
    }

    #[test]
    fn quote_identifier_stray_quotes_are_escaped() {
        assert_eq!(quote_identifier(r#"a"b"#), r#""a""b""#);
        assert_eq!(
            quote_identifier(r#""x" ; DROP TABLE t --""#),
            r#""""x"" ; DROP TABLE t --""""#
        );
    }

    #[test]
    fn quote_table_name_quotes_each_part() {
        assert_eq!(quote_table_name("shop.customers"), r#""shop"."customers""#);
        assert_eq!(quote_table_name(r#""my.schema".t"#), r#""my.schema"."t""#);
    }

    fn column(table: usize, name: &str) -> SqlExpr {
        SqlExpr::Column {
            table,
            name: name.to_owned(),
        }
    }

    #[test]
    fn to_sql_places_linking_conditions_in_on_and_numbers_params_in_order() {
        let plan = SqlPlan {
            columns: vec!["x".into()],
            projection: Vec::new(),
            branches: vec![SqlBranch {
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
            }],
            distinct: true,
        };
        let rendered = plan.to_sql();
        assert_eq!(
            rendered.sql,
            "SELECT DISTINCT t0.\"id\" AS \"x\"\n\
             FROM \"a\" t0\n\
             JOIN (SELECT * FROM b) t1 ON t1.\"id\" = $1 || t0.\"id\"::text\n\
             CROSS JOIN \"c\" t2\n\
             WHERE t0.\"k\" = $2 AND t2.\"v\" IS NOT NULL"
        );
        assert_eq!(rendered.params, ["p/", "one"]);
    }

    #[test]
    fn to_sql_without_branches_returns_no_rows() {
        let plan = SqlPlan {
            columns: Vec::new(),
            projection: Vec::new(),
            branches: Vec::new(),
            distinct: false,
        };
        assert_eq!(plan.to_sql().sql, "SELECT 1 WHERE false");
    }
}
