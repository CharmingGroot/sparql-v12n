// SPDX-License-Identifier: Apache-2.0
//! SQL plan (`SqlPlan`) and PostgreSQL text rendering with bound parameters.

use std::collections::{BTreeSet, HashMap};

use oxrdf::Term;

use crate::Error;
use crate::mapping::template::UCSCHAR;
use crate::mapping::{LogicalTable, TemplatePart, TermType};

const XSD_STRING: &str = "http://www.w3.org/2001/XMLSchema#string";

/// An unfolded query: one `SELECT` per branch, combined with `UNION ALL`
/// (or `UNION` under `DISTINCT`, or when duplicate triples are removed).
///
/// An RDF graph is a set of triples, so a basic graph pattern has one
/// solution per distinct mapping of all its variables (SPARQL 1.1 §18.5),
/// while duplicate rows of a logical table, or two term maps producing the
/// same triple, give one SQL row each. Unless the database's keys prove
/// that no two rows give the same solution, the rendered SQL removes
/// duplicates over the columns of every variable of the pattern, and only
/// then projects the selected ones, which keeps the bag semantics of the
/// projection (see [`SqlPlan::to_sql_with`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlPlan {
    /// Result column aliases, shared by every branch.
    pub columns: Vec<String>,
    /// Aliases of the columns of variables that occur in the pattern but
    /// are not selected. They take part in removing duplicates only.
    pub hidden_columns: Vec<String>,
    /// The entries of [`SqlBranch::select`] in the order duplicates are
    /// removed over: by first occurrence of their variable in the pattern,
    /// so that an index on the first subject can deliver the rows sorted
    /// for `DISTINCT` and the first rows arrive without reading them all.
    /// Empty means the order of [`SqlBranch::select`].
    pub distinct_order: Vec<usize>,
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
    /// One expression per entry of [`SqlPlan::columns`], then one per
    /// entry of [`SqlPlan::hidden_columns`].
    pub select: Vec<SqlExpr>,
    /// Conjunction of conditions. Rendering puts a condition that links
    /// two tables in the `ON` clause of the later one, the rest in `WHERE`.
    pub conditions: Vec<Condition>,
    /// For each entry of `from`, the columns whose values a solution
    /// fixes: those of terms that distinct column values always render as
    /// distinct terms. When they include a unique key of the table, the
    /// table contributes at most one row to each solution.
    pub fixed_columns: Vec<BTreeSet<String>>,
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

/// Unique keys of logical tables, each the column names as the database
/// stores them ([`identifier_name`]).
pub type TableKeys = HashMap<LogicalTable, Vec<Vec<String>>>;

/// What a database adapter learned before rendering, see
/// [`SqlPlan::to_sql_with`].
#[derive(Debug, Clone, Copy, Default)]
pub struct RenderOptions<'a> {
    /// Parameters (0-based) whose compared column is cast to text.
    pub text_params: &'a [usize],
    /// Types of the columns that [`SqlPlan::term_columns`] lists.
    pub column_types: Option<&'a ColumnTypes>,
    /// Unique keys of the tables that [`SqlPlan::key_tables`] lists.
    pub keys: Option<&'a TableKeys>,
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
    /// - With `keys`, duplicates are not removed when the keys prove that
    ///   there are none (see [`has_duplicates`](Self::has_duplicates)).
    ///
    /// Duplicates are removed with `DISTINCT` (one branch) or `UNION`
    /// over the columns of all variables, in a subquery when some of them
    /// are not selected. This compares column values, so two different
    /// values that a template renders as the same string (see
    /// [`SqlPlan::hidden_columns`]) still count twice.
    pub fn to_sql_with(&self, options: &RenderOptions<'_>) -> Result<RenderedSql, Error> {
        let mut r = Renderer {
            options: *options,
            ..Renderer::default()
        };
        if self.branches.is_empty() {
            r.sql.push_str("SELECT 1 WHERE false");
        }
        let dedup = self.has_duplicates(options.keys);
        let distinct = self.distinct || dedup;
        let single = self.branches.len() == 1;
        let mut select = Select {
            aliases: self.columns.clone(),
            order: (0..self.columns.len()).collect(),
            typed_text_from: None,
        };
        if dedup {
            select.aliases.extend(self.hidden_columns.iter().cloned());
            if self.distinct_order.is_empty() {
                select.order = (0..select.aliases.len()).collect();
            } else {
                select.order.clone_from(&self.distinct_order);
            }
            if !single {
                select.typed_text_from = Some(self.columns.len());
            }
        }
        let subquery = select.aliases.len() > self.columns.len()
            || select.order.iter().enumerate().any(|(i, &o)| i != o);
        if subquery {
            r.sql.push_str("SELECT ");
            if self.columns.is_empty() {
                r.sql.push('1');
            }
            let outer: Vec<String> = self
                .columns
                .iter()
                .map(|c| format!("q.{}", quote_identifier(c)))
                .collect();
            r.sql.push_str(&outer.join(", "));
            r.sql.push_str("\nFROM (\n");
        }
        for (i, branch) in self.branches.iter().enumerate() {
            if i > 0 {
                r.sql.push_str(if distinct {
                    "\nUNION\n"
                } else {
                    "\nUNION ALL\n"
                });
            }
            r.branch_index = i;
            r.branch(branch, &select, distinct && single)?;
        }
        if subquery {
            r.sql.push_str("\n) q");
        }
        Ok(RenderedSql {
            sql: r.sql,
            params: r.params,
            param_datatypes: r.param_datatypes,
            param_branches: r.param_branches,
        })
    }

    /// Whether two SQL rows may give the same solution of the pattern, so
    /// that rendering removes duplicates. Never under `DISTINCT`, which
    /// already does. Otherwise true unless the plan has one branch and
    /// `keys` holds, for every table it reads, a unique key among the
    /// table's [`SqlBranch::fixed_columns`]: then each solution fixes one
    /// row of every table, and so one row of the result.
    pub fn has_duplicates(&self, keys: Option<&TableKeys>) -> bool {
        if self.distinct || self.branches.is_empty() {
            return false;
        }
        let ([branch], Some(keys)) = (self.branches.as_slice(), keys) else {
            return true;
        };
        !branch
            .from
            .iter()
            .zip(&branch.fixed_columns)
            .all(|(table, fixed)| {
                let fixed: BTreeSet<String> = fixed.iter().map(|c| identifier_name(c)).collect();
                keys.get(table).is_some_and(|keys| {
                    keys.iter().any(|key| key.iter().all(|c| fixed.contains(c)))
                })
            })
    }

    /// The tables whose keys can show that the plan has no duplicates
    /// ([`has_duplicates`](Self::has_duplicates)), each once. Empty when
    /// keys cannot help: under `DISTINCT`, with several branches, or when
    /// a branch reads an `rr:sqlQuery`, which has no keys.
    pub fn key_tables(&self) -> Vec<LogicalTable> {
        let [branch] = self.branches.as_slice() else {
            return Vec::new();
        };
        if self.distinct
            || branch
                .from
                .iter()
                .any(|t| matches!(t, LogicalTable::SqlQuery(_)))
        {
            return Vec::new();
        }
        let mut out: Vec<LogicalTable> = Vec::new();
        for table in &branch.from {
            if !out.contains(table) {
                out.push(table.clone());
            }
        }
        out
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
    let mut tables: Vec<&LogicalTable> = Vec::new();
    let select: Vec<String> = columns
        .iter()
        .map(|(table, name)| {
            let position = tables.iter().position(|t| *t == table).unwrap_or_else(|| {
                tables.push(table);
                tables.len() - 1
            });
            format!("p{position}.{}", quote_identifier(name))
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

/// The select list of every branch.
struct Select {
    /// Alias of each entry of [`SqlBranch::select`] that is rendered.
    aliases: Vec<String>,
    /// The entries in the order they are selected.
    order: Vec<usize>,
    /// Entries from here on are only compared, never decoded, and are
    /// selected as their type and text output.
    typed_text_from: Option<usize>,
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
        select: &Select,
        distinct: bool,
    ) -> Result<(), Error> {
        self.from = &branch.from;
        self.sql.push_str("SELECT ");
        if distinct {
            self.sql.push_str("DISTINCT ");
        }
        if select.order.is_empty() {
            self.sql.push('1');
        }
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
                let start = self.sql.len();
                self.expr(&branch.select[entry]);
                let column = self.sql.split_off(start);
                self.sql.push_str(&format!(
                    "pg_typeof({column})::text || ' ' || {column}::text"
                ));
            } else {
                self.expr(&branch.select[entry]);
            }
            self.sql.push_str(" AS ");
            self.sql.push_str(&quote_identifier(&select.aliases[entry]));
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

/// Quotes an identifier from the mapping. An identifier that is already
/// a valid delimited identifier (`"Order Id"`) is kept as written, so
/// R2RML mappings can use either form; anything else is wrapped in double
/// quotes with embedded quotes doubled, which makes it impossible to
/// break out of the identifier.
pub fn quote_identifier(name: &str) -> String {
    if is_delimited(name) {
        name.to_owned()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// The name [`quote_identifier`] refers to, as the database stores it.
pub fn identifier_name(name: &str) -> String {
    if is_delimited(name) {
        name[1..name.len() - 1].replace("\"\"", "\"")
    } else {
        name.to_owned()
    }
}

fn is_delimited(name: &str) -> bool {
    name.len() >= 2
        && name.starts_with('"')
        && name.ends_with('"')
        && !name[1..name.len() - 1].replace("\"\"", "").contains('"')
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
            hidden_columns: Vec::new(),
            distinct_order: Vec::new(),
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
                fixed_columns: Vec::new(),
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
            fixed_columns: Vec::new(),
        };
        let plan = SqlPlan {
            columns: vec!["x".into()],
            hidden_columns: Vec::new(),
            distinct_order: Vec::new(),
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
            hidden_columns: Vec::new(),
            distinct_order: Vec::new(),
            projection: Vec::new(),
            branches: vec![SqlBranch {
                from: vec![l.clone(), r.clone()],
                select: Vec::new(),
                conditions: vec![Condition::SameTerm(left, right)],
                fixed_columns: Vec::new(),
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

    fn keyed_plan(fixed: &[&str], hidden: bool) -> SqlPlan {
        SqlPlan {
            columns: vec!["s".into()],
            hidden_columns: if hidden { vec!["o".into()] } else { Vec::new() },
            distinct_order: Vec::new(),
            projection: Vec::new(),
            branches: vec![SqlBranch {
                from: vec![LogicalTable::TableName("\"T\"".into())],
                select: vec![column(0, "\"Id\""), column(0, "v")],
                conditions: Vec::new(),
                fixed_columns: vec![fixed.iter().map(|c| c.to_string()).collect()],
            }],
            distinct: false,
        }
    }

    #[test]
    fn duplicates_are_removed_over_all_variables_then_projected() {
        assert_eq!(
            keyed_plan(&[], true).to_sql().sql,
            "SELECT q.\"s\"\nFROM (\n\
             SELECT DISTINCT t0.\"Id\" AS \"s\", t0.\"v\" AS \"o\"\n\
             FROM \"T\" t0\n\
             ) q"
        );
        assert_eq!(
            keyed_plan(&[], false).to_sql().sql,
            "SELECT DISTINCT t0.\"Id\" AS \"s\"\nFROM \"T\" t0"
        );
        let mut no_columns = keyed_plan(&[], true);
        no_columns.columns.clear();
        no_columns.branches[0].select.remove(0);
        assert_eq!(
            no_columns.to_sql().sql,
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
            plan.to_sql_with(&options).unwrap().sql,
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
            distinct.to_sql().sql,
            "SELECT DISTINCT t0.\"Id\" AS \"s\"\nFROM \"T\" t0"
        );
    }

    #[test]
    fn identifier_name_unquotes_delimited_names() {
        assert_eq!(identifier_name("id"), "id");
        assert_eq!(identifier_name("\"Id\""), "Id");
        assert_eq!(identifier_name("\"a\"\"b\""), "a\"b");
        assert_eq!(identifier_name("a\"b"), "a\"b");
    }

    #[test]
    fn to_sql_without_branches_returns_no_rows() {
        let plan = SqlPlan {
            columns: Vec::new(),
            hidden_columns: Vec::new(),
            distinct_order: Vec::new(),
            projection: Vec::new(),
            branches: Vec::new(),
            distinct: false,
        };
        assert_eq!(plan.to_sql().sql, "SELECT 1 WHERE false");
    }
}
