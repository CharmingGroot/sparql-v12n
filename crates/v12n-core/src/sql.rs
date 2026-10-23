// SPDX-License-Identifier: Apache-2.0
//! SQL plan (`SqlPlan`) and PostgreSQL text rendering with bound parameters.

use std::collections::{BTreeSet, HashMap};

use oxrdf::Term;

use crate::Error;
use crate::mapping::template::UCSCHAR;
use crate::mapping::{LogicalTable, TemplatePart, TermType};

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

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
    /// SQL equality: the columns of an `rr:joinCondition` (R2RML §8 joins
    /// with SQL `=`), or a column and a constant, which a database adapter
    /// checks against the column's type before binding (see [`Param`]).
    Eq(SqlExpr, SqlExpr),
    IsNotNull(SqlExpr),
    /// Both sides produce the same RDF term. Column values take part
    /// through their natural lexical form (R2RML §10.2), which SQL `=`
    /// matches only for some pairs of SQL types, so how this renders
    /// depends on the column types (see [`SqlPlan::to_sql_with`]).
    SameTerm(TermSql, TermSql),
}

/// One side of a [`Condition::SameTerm`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TermSql {
    pub value: TermSqlValue,
    /// The datatype of a literal without language tag, when the
    /// comparison may have to check it; `None` when it never needs to,
    /// such as for IRIs and template columns.
    pub datatype: Option<TermDatatype>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermSqlValue {
    /// The natural lexical form of a column of table alias `t{table}`.
    Column { table: usize, name: String },
    /// A template over table alias `t{table}`. With `iri_safe`, column
    /// values are percent-encoded as in an IRI template (R2RML §7.3).
    Template {
        table: usize,
        parts: Vec<TemplatePart>,
        iri_safe: bool,
    },
    /// The lexical form of a constant.
    Param(Param),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermDatatype {
    /// The natural datatype of the column's SQL type (R2RML §10.2), only
    /// known from the database.
    Natural,
    /// A datatype IRI given by the mapping.
    Iri(String),
}

/// What a database adapter knows about the SQL type of a column that a
/// [`Condition::SameTerm`] reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnType {
    /// The SQL type name, for error messages.
    pub name: String,
    /// IRI of the type's natural datatype; `None` for `xsd:string`.
    pub natural_datatype: Option<String>,
    /// Two columns of the same group compare as `comparison` says.
    pub group: String,
    pub comparison: Comparison,
    /// How SQL computes the natural lexical form, when it can.
    pub lexical: Option<Lexical>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Comparison {
    /// `a = b`: within the group, equal values have equal natural lexical
    /// forms and the other way round.
    Value,
    /// `a::text = b::text`: the text output identifies the natural
    /// lexical form within the group, but value equality does not (for
    /// example `-0` and `0`, or `1 day` and `24 hours`).
    Text,
}

/// SQL for a column's natural lexical form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lexical {
    /// The text output, `col::text`.
    Text,
    /// `true` or `false`.
    Boolean,
    /// Upper-case hexadecimal digits.
    Hex,
    /// The canonical `xsd:decimal` form of a PostgreSQL `numeric`,
    /// `trim_scale(col)::text` (PostgreSQL 13 or later).
    Decimal,
}

/// Column types by logical table and column name.
pub type ColumnTypes = HashMap<(LogicalTable, String), ColumnType>;

/// What a database adapter learned before rendering, see
/// [`SqlPlan::to_sql_with`].
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderOptions<'a> {
    /// Parameters (0-based) whose compared column is cast to text.
    pub text_params: &'a [usize],
    /// Types of the columns that [`SqlPlan::term_columns`] lists.
    pub column_types: Option<&'a ColumnTypes>,
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
    ///
    /// Without column types, a [`Condition::SameTerm`] compares columns
    /// with `=` and template columns by their text output, which is what
    /// the database sees for text and integer columns.
    pub fn to_sql(&self) -> RenderedSql {
        self.to_sql_with(&RenderOptions::default())
            .expect("rendering without column types does not fail")
    }

    /// Like [`to_sql`](Self::to_sql), with what a database adapter knows:
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
    pub fn to_sql_with(&self, options: &RenderOptions<'_>) -> Result<RenderedSql, Error> {
        let mut r = Renderer {
            options: *options,
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
            r.branch(branch, &self.columns, self.distinct && single)?;
        }
        Ok(RenderedSql {
            sql: r.sql,
            params: r.params,
            param_datatypes: r.param_datatypes,
            param_branches: r.param_branches,
        })
    }

    /// The columns [`Condition::SameTerm`]s read, each once, in order of
    /// appearance. Their types go into [`RenderOptions::column_types`].
    pub fn term_columns(&self) -> Vec<(LogicalTable, String)> {
        let mut out: Vec<(LogicalTable, String)> = Vec::new();
        for branch in &self.branches {
            for condition in &branch.conditions {
                let Condition::SameTerm(left, right) = condition else {
                    continue;
                };
                for (table, name) in left.columns().into_iter().chain(right.columns()) {
                    let key = (branch.from[table].clone(), name.to_owned());
                    if !out.contains(&key) {
                        out.push(key);
                    }
                }
            }
        }
        out
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

    fn for_each_column(&mut self, mut f: impl FnMut(&LogicalTable, &mut String)) {
        fn expr(
            e: &mut SqlExpr,
            from: &[LogicalTable],
            f: &mut impl FnMut(&LogicalTable, &mut String),
        ) {
            match e {
                SqlExpr::Column { table, name } => f(&from[*table], name),
                SqlExpr::Param(_) => {}
                SqlExpr::Concat(parts) => parts.iter_mut().for_each(|p| expr(p, from, f)),
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
            for condition in &mut branch.conditions {
                match condition {
                    Condition::Eq(left, right) => {
                        expr(left, from, &mut f);
                        expr(right, from, &mut f);
                    }
                    Condition::IsNotNull(e) => expr(e, from, &mut f),
                    Condition::SameTerm(left, right) => {
                        term(left, from, &mut f);
                        term(right, from, &mut f);
                    }
                }
            }
        }
    }
}

impl TermSql {
    /// `(table, column)` pairs the side reads.
    pub fn columns(&self) -> Vec<(usize, &str)> {
        match &self.value {
            TermSqlValue::Column { table, name } => vec![(*table, name.as_str())],
            TermSqlValue::Template { table, parts, .. } => parts
                .iter()
                .filter_map(|p| match p {
                    TemplatePart::Column(name) => Some((*table, name.as_str())),
                    TemplatePart::Literal(_) => None,
                })
                .collect(),
            TermSqlValue::Param(_) => Vec::new(),
        }
    }
}

/// A statement whose result columns have the types of `columns`, to
/// prepare (not run) so the database reports them. Each logical table
/// appears once.
pub fn column_type_probe(columns: &[(LogicalTable, String)]) -> String {
    probe(columns, quote_identifier)
}

/// A statement that refers to each of `columns`, all
/// [`folded_identifier`]s, as written, without quotes, to prepare (not
/// run): the database then resolves each one as it resolves a regular
/// identifier and reports the name it found as the result column's name.
/// Each logical table appears once.
pub fn column_name_probe(columns: &[(LogicalTable, String)]) -> String {
    assert!(
        columns.iter().all(|(_, name)| folded_identifier(name)),
        "only regular identifiers may appear unquoted"
    );
    probe(columns, str::to_owned)
}

fn probe(columns: &[(LogicalTable, String)], name_sql: impl Fn(&str) -> String) -> String {
    let mut tables: Vec<&LogicalTable> = Vec::new();
    let select: Vec<String> = columns
        .iter()
        .map(|(table, name)| {
            let position = tables.iter().position(|t| *t == table).unwrap_or_else(|| {
                tables.push(table);
                tables.len() - 1
            });
            format!("p{position}.{}", name_sql(name))
        })
        .collect();
    let from: Vec<String> = tables
        .iter()
        .enumerate()
        .map(|(i, table)| format!("{} p{i}", logical_table(table)))
        .collect();
    format!(
        "SELECT {}\nFROM {}\nLIMIT 0",
        select.join(", "),
        from.join("\nCROSS JOIN ")
    )
}

#[derive(Default)]
struct Renderer<'a> {
    sql: String,
    params: Vec<String>,
    param_datatypes: Vec<Option<String>>,
    param_branches: Vec<usize>,
    branch_index: usize,
    options: RenderOptions<'a>,
    /// Logical tables of the branch being rendered.
    from: &'a [LogicalTable],
}

impl<'a> Renderer<'a> {
    fn branch(
        &mut self,
        branch: &'a SqlBranch,
        columns: &[String],
        distinct: bool,
    ) -> Result<(), Error> {
        self.from = &branch.from;
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
                self.conditions(&on[i])?;
            }
        }
        if !filters.is_empty() {
            self.sql.push_str("\nWHERE ");
            self.conditions(&filters)?;
        }
        Ok(())
    }

    fn conditions(&mut self, conditions: &[&Condition]) -> Result<(), Error> {
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
                    ) && self.options.text_params.contains(&self.params.len());
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
                Condition::SameTerm(left, right) => self.same_term(left, right)?,
            }
        }
        Ok(())
    }

    fn same_term(&mut self, left: &TermSql, right: &TermSql) -> Result<(), Error> {
        let Some(types) = self.options.column_types else {
            self.term_side(left, None)?;
            self.sql.push_str(" = ");
            return self.term_side(right, None);
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
            && let (Some(a), Some(b)) = (self.column_type(*lt, ln), self.column_type(*rt, rn))
            && a.group == b.group
        {
            let cast = match a.comparison {
                Comparison::Value => "",
                Comparison::Text => "::text",
            };
            self.sql.push_str(&format!(
                "{}{cast} = {}{cast}",
                column_sql(*lt, ln),
                column_sql(*rt, rn)
            ));
            return Ok(());
        }
        self.term_side(left, Some(types))?;
        self.sql.push_str(" = ");
        self.term_side(right, Some(types))
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

    fn column_type(&self, table: usize, name: &str) -> Option<&'a ColumnType> {
        self.options
            .column_types?
            .get(&(self.from[table].clone(), name.to_owned()))
    }

    /// One side of a term comparison as a string expression. Without
    /// `types`, columns are rendered as they are and template columns by
    /// their text output.
    fn term_side(&mut self, side: &TermSql, types: Option<&ColumnTypes>) -> Result<(), Error> {
        match &side.value {
            TermSqlValue::Column { table, name } => match types {
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
                            let value = match types {
                                None => format!("{}::text", column_sql(*table, name)),
                                Some(_) => self.lexical(*table, name)?,
                            };
                            self.sql.push_str(&if *iri_safe {
                                iri_safe_sql(&value)
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
            return Ok(format!("{column}::text"));
        };
        Ok(match ty.lexical {
            Some(Lexical::Text) => format!("{column}::text"),
            Some(Lexical::Boolean) => {
                format!("CASE {column} WHEN true THEN 'true' WHEN false THEN 'false' END")
            }
            Some(Lexical::Hex) => format!("upper(encode({column}, 'hex'))"),
            // `numeric` output never uses an exponent, and trim_scale drops
            // exactly the trailing fractional zeros, which is the XML
            // Schema 1.1 decimalCanonicalMap (`3`, `12.5`); tested against
            // `canonical_decimal` over many values in v12n-postgres.
            Some(Lexical::Decimal) => format!("trim_scale({column})::text"),
            None => {
                let source = match &self.from[table] {
                    LogicalTable::TableName(t) => format!("table {t}"),
                    LogicalTable::SqlQuery(_) => "an rr:sqlQuery".to_owned(),
                };
                return Err(Error::UnsupportedFeature(format!(
                    "comparing terms built from column {name} of {source} ({}) with terms of \
                     a different SQL type or shape: its natural lexical form cannot be \
                     computed in SQL",
                    ty.name
                )));
            }
        })
    }

    fn expr(&mut self, expr: &SqlExpr) {
        match expr {
            SqlExpr::Column { table, name } => self.sql.push_str(&column_sql(*table, name)),
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

fn column_sql(table: usize, name: &str) -> String {
    format!("t{table}.{}", quote_identifier(name))
}

/// R2RML §7.3 IRI-safe version of a text expression: every character
/// outside `iunreserved` becomes `%XX` per UTF-8 byte, upper case, as the
/// engine encodes template values when it builds IRIs. NULL stays NULL.
pub fn iri_safe_sql(text: &str) -> String {
    let mut class = String::from("^[-.0-9A-Z_a-z~");
    for &(from, to) in UCSCHAR {
        let escape = |c: u32| {
            if c <= 0xFFFF {
                format!("\\u{c:04X}")
            } else {
                format!("\\U{c:08X}")
            }
        };
        class.push_str(&format!("{}-{}", escape(from), escape(to)));
    }
    class.push_str("]$");
    format!(
        "CASE WHEN {text} IS NULL THEN NULL ELSE (SELECT coalesce(string_agg(\
         CASE WHEN c ~ '{class}' THEN c \
         ELSE regexp_replace(upper(encode(convert_to(c, 'UTF8'), 'hex')), '(..)', '%\\1', 'g') END, \
         '' ORDER BY n), '') \
         FROM generate_series(1, char_length({text})) AS n, substr({text}, n, 1) AS c) END"
    )
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
        Condition::SameTerm(left, right) => {
            for (table, _) in left.columns().into_iter().chain(right.columns()) {
                tables.insert(table);
            }
        }
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

/// Whether a column name from the mapping is a regular (undelimited)
/// identifier that PostgreSQL may resolve to a different name.
///
/// R2RML §5 makes column names SQL identifiers: delimited ones are taken
/// as written, regular ones are case-insensitive. PostgreSQL folds a
/// regular identifier (PostgreSQL docs §4.1.1: a letter, `_` or a
/// non-ASCII character, then also digits and `$`) to lower case, with
/// encoding-dependent rules for non-ASCII letters, and truncates it to
/// `NAMEDATALEN - 1` bytes. Quoting such a name as written would refer to
/// a different column, so its effective name is asked from the database
/// ([`column_name_probe`]). A name of lower-case ASCII letters, digits,
/// `_` and `$` of at most 63 bytes folds to itself and needs no lookup.
/// Anything else that is not a regular identifier keeps being quoted as
/// written.
pub fn folded_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let regular = (first.is_ascii_alphabetic() || first == '_' || !first.is_ascii())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$') || !c.is_ascii());
    regular
        && (name.len() > 63
            || name
                .chars()
                .any(|c| c.is_ascii_uppercase() || !c.is_ascii()))
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
    fn folded_identifier_is_a_regular_identifier_that_folding_may_change() {
        for name in ["StudentId", "ID", "SPORTCOUNT", "_X", "a$B", "Ä", "nameé"] {
            assert!(folded_identifier(name), "{name}");
        }
        let long = "a".repeat(64);
        assert!(folded_identifier(&long));
        // Folding leaves these as they are, or they are not regular
        // identifiers and keep being quoted as written.
        for name in [
            "studentid",
            "a_b$1",
            &"a".repeat(63),
            "\"Name\"",
            "Order Id",
            "1A",
            "$A",
            "a-B",
            "a\"B",
            "",
        ] {
            assert!(!folded_identifier(name), "{name}");
        }
    }

    #[test]
    fn column_name_probe_leaves_regular_identifiers_unquoted() {
        let view = LogicalTable::SqlQuery("SELECT 1 AS Id".into());
        let table = LogicalTable::TableName("t".into());
        assert_eq!(
            column_name_probe(&[
                (view.clone(), "Id".into()),
                (table, "Name".into()),
                (view, "LABEL".into()),
            ]),
            "SELECT p0.Id, p1.Name, p0.LABEL\nFROM (SELECT 1 AS Id) p0\nCROSS JOIN \"t\" p1\nLIMIT 0"
        );
    }

    #[test]
    #[should_panic(expected = "only regular identifiers")]
    fn column_name_probe_refuses_other_names() {
        column_name_probe(&[(LogicalTable::TableName("t".into()), "a; DROP".into())]);
    }

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
        };
        let plan = SqlPlan {
            columns: vec!["a".into(), "b".into()],
            projection: Vec::new(),
            branches: vec![SqlBranch {
                from: vec![view.clone(), view.clone()],
                select: vec![column(0, "Id"), column(1, "\"V\"")],
                conditions: vec![
                    Condition::SameTerm(template(0), template(1)),
                    Condition::Eq(column(0, "ID"), SqlExpr::Param("1".into())),
                ],
            }],
            distinct: false,
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
            plan.with_resolved_columns(&resolved).to_sql().sql,
            "SELECT t0.\"id\" AS \"a\", t1.\"V\" AS \"b\"\n\
             FROM (SELECT 1 AS Id, 2 AS \"V\") t0\n\
             JOIN (SELECT 1 AS Id, 2 AS \"V\") t1 ON $1 || t0.\"id\"::text = $2 || t1.\"id\"::text\n\
             WHERE t0.\"id\" = $3"
        );
    }

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
        let rendered = plan
            .to_sql_with(&RenderOptions {
                text_params: &[3],
                ..RenderOptions::default()
            })
            .unwrap();
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

    fn ty(group: &str, comparison: Comparison, lexical: Option<Lexical>) -> ColumnType {
        ColumnType {
            name: group.to_owned(),
            natural_datatype: (group == "integer")
                .then(|| "http://www.w3.org/2001/XMLSchema#integer".to_owned()),
            group: group.to_owned(),
            comparison,
            lexical,
        }
    }

    fn side(table: usize, name: &str, datatype: Option<TermDatatype>) -> TermSql {
        TermSql {
            value: TermSqlValue::Column {
                table,
                name: name.to_owned(),
            },
            datatype,
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
            columns: Vec::new(),
            projection: Vec::new(),
            branches: vec![SqlBranch {
                from: vec![l.clone(), r.clone()],
                select: Vec::new(),
                conditions: vec![Condition::SameTerm(left, right)],
            }],
            distinct: false,
        };
        assert_eq!(plan.term_columns().len(), 2);
        let types: ColumnTypes = [((l, "a".to_owned()), a), ((r, "b".to_owned()), b)].into();
        let rendered = plan.to_sql_with(&RenderOptions {
            column_types: Some(&types),
            ..RenderOptions::default()
        })?;
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
    fn iri_safe_sql_keeps_nulls_and_uses_the_iunreserved_ranges() {
        let sql = iri_safe_sql("x");
        assert!(
            sql.starts_with("CASE WHEN x IS NULL THEN NULL ELSE"),
            "{sql}"
        );
        assert!(sql.contains(r"\u00A0-\uD7FF"), "{sql}");
        assert!(sql.contains(r"\U000E1000-\U000EFFFD]$'"), "{sql}");
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
