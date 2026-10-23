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
    Param(Param),
    /// String concatenation of the parts, each cast to text.
    Concat(Vec<SqlExpr>),
}

/// A constant of the query and what it has to match.
///
/// A database adapter checks every parameter against the SQL type the
/// database infers for it before binding, because the database parses
/// text with its own input syntax, which differs from RDF lexical forms:
/// `007` and `+7` are the integer 7 there, `7.0` is an error for an
/// integer, and `99999999999` overflows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    /// The lexical form a value must render as to match (R2RML §10.2,
    /// natural RDF lexical form), or plain text for parts of a string.
    pub value: String,
    /// The datatype IRI of a literal constant compared with a column
    /// whose literal datatype is the natural one of its SQL type, which
    /// is only known from the database. The two must then be equal:
    /// `"7"^^xsd:decimal` is not the term an integer column produces.
    pub datatype: Option<String>,
}

impl From<String> for Param {
    fn from(value: String) -> Self {
        Self {
            value,
            datatype: None,
        }
    }
}

impl From<&str> for Param {
    fn from(value: &str) -> Self {
        value.to_owned().into()
    }
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
    /// [`Param::datatype`] of each parameter.
    pub param_datatypes: Vec<Option<String>>,
    /// Index into [`SqlPlan::branches`] of the branch each parameter
    /// belongs to.
    pub param_branches: Vec<usize>,
}

impl SqlPlan {
    /// Renders the plan. Parameters are numbered in the order they appear
    /// in the text.
    ///
    /// Every parameter is one side of an equality in the conjunction of
    /// its branch's `ON` and `WHERE` conditions, so binding it to `NULL`
    /// empties that branch and leaves the others as they are.
    pub fn to_sql(&self) -> RenderedSql {
        self.to_sql_with_text_params(&[])
    }

    /// Like [`to_sql`](Self::to_sql), with the column compared with each
    /// parameter in `text_params` (0-based) cast to text. A database
    /// adapter uses this for column types whose values it does not format
    /// itself: the comparison then uses the same text the result column
    /// gets, instead of the database parsing the constant as that type.
    /// The cast keeps an index on the column from being used.
    pub fn to_sql_with_text_params(&self, text_params: &[usize]) -> RenderedSql {
        let mut r = Renderer {
            text_params,
            ..Renderer::default()
        };
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
            r.branch_index = i;
            r.branch(branch, &self.columns, self.distinct && single);
        }
        RenderedSql {
            sql: r.sql,
            params: r.params,
            param_datatypes: r.param_datatypes,
            param_branches: r.param_branches,
        }
    }
}

#[derive(Default)]
struct Renderer<'a> {
    sql: String,
    params: Vec<String>,
    param_datatypes: Vec<Option<String>>,
    param_branches: Vec<usize>,
    branch_index: usize,
    text_params: &'a [usize],
}

impl Renderer<'_> {
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
                    // A column side holds no parameter, so the parameter
                    // on the other side is the next one either way.
                    let as_text = matches!(
                        (left, right),
                        (SqlExpr::Column { .. }, SqlExpr::Param(_))
                            | (SqlExpr::Param(_), SqlExpr::Column { .. })
                    ) && self.text_params.contains(&self.params.len());
                    self.expr(left);
                    if as_text && matches!(left, SqlExpr::Column { .. }) {
                        self.sql.push_str("::text");
                    }
                    self.sql.push_str(" = ");
                    self.expr(right);
                    if as_text && matches!(right, SqlExpr::Column { .. }) {
                        self.sql.push_str("::text");
                    }
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
            SqlExpr::Param(param) => {
                self.params.push(param.value.clone());
                self.param_datatypes.push(param.datatype.clone());
                self.param_branches.push(self.branch_index);
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
        assert_eq!(rendered.param_branches, [0, 0]);
    }

    #[test]
    fn to_sql_with_text_params_casts_the_compared_column() {
        let branch = |value: &str, datatype: Option<&str>| SqlBranch {
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
        };
        let plan = SqlPlan {
            columns: vec!["x".into()],
            projection: Vec::new(),
            branches: vec![branch("1", None), branch("2", Some("http://ex/dt"))],
            distinct: false,
        };
        let rendered = plan.to_sql_with_text_params(&[3]);
        assert_eq!(
            rendered.sql,
            "SELECT t0.\"id\" AS \"x\"\n\
             FROM \"t\" t0\n\
             WHERE t0.\"a\" = $1 || t0.\"b\"::text AND t0.\"v\" = $2\n\
             UNION ALL\n\
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
            plan.to_sql().sql,
            rendered.sql.replace("::text = $4", " = $4")
        );
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
