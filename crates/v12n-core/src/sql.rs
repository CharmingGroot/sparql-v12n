// SPDX-License-Identifier: Apache-2.0
//! SQL plan (`SqlPlan`) and its rendering as SQL text with bound parameters.

mod catalog_probe;
mod column_names;
mod filter;
mod identifier;
mod order;
mod plan;
mod renderer;
mod required_columns;

pub use catalog_probe::{column_name_probe, column_probe, column_type_probe, table_probe};
pub(crate) use identifier::describe_table;
pub use identifier::{folded_identifier, identifier_name, quote_identifier, quote_table_name};
pub use plan::{
    ColumnType, ColumnTypes, Comparison, Condition, Filter, FilterTerm, FilterValue, LeftJoin,
    Lexical, OrderColumn, OrderKey, OrderValue, Param, ProjectedTerm, RenderOptions, RenderedSql,
    ResultTerm, ResultValue, SqlBranch, SqlExpr, SqlPlan, TableKeys, TermDatatype, TermKey,
    TermMatch, TermNormal, TermSql, TermSqlValue, ValueCases, ValueKind,
};

use crate::Error;
use plan::XSD_STRING;
use renderer::{Renderer, column_sql};
