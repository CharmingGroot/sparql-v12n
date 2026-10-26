// SPDX-License-Identifier: Apache-2.0
//! SQL for `ORDER BY`: sort keys that put solutions in SPARQL's order
//! (SPARQL 1.1 §15.1) across kinds of terms, datatypes and branches.

use std::collections::BTreeMap;

use super::filter::{Kind, Mode, Operand, Text, unsupported};
use super::{Condition, Error, FilterValue, OrderValue, Renderer, SqlBranch, SqlPlan};
use crate::xsd::{Num, Value};

/// The kinds of values in ascending order. SPARQL 1.1 §15.1 fixes the
/// order of the first four (no value, blank nodes, IRIs, literals); the
/// order of literals of different kinds, for which the `<` operator is not
/// defined, is this engine's choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rank {
    /// No value: unbound, or an error (§15.1: "no value assigned").
    Unbound = 0,
    BlankNode = 1,
    Iri = 2,
    Numeric = 3,
    Boolean = 4,
    DateTime = 5,
    /// Simple literals and `xsd:string`, one kind in RDF 1.1.
    String = 6,
    /// Language-tagged literals: by lexical form, then language tag.
    Language = 7,
    /// Literals of any other datatype: by datatype IRI, then by value
    /// where the SQL type orders them (`date`, `time`), else by lexical
    /// form.
    Other = 8,
}

/// One sort column of an order comparator. A comparator sorts by its
/// slots in this order, each holding a value only in rows of the ranks
/// that use it, so rows of one rank compare by that rank's slots only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Slot {
    Rank,
    /// The datatype IRI of [`Rank::Other`].
    Datatype,
    /// Numbers compared as doubles, when some of them are: XPath promotes
    /// both operands to `xsd:double` then (op:numeric-less-than).
    Double,
    /// Integers and decimals by exact value; after [`Slot::Double`], it
    /// orders integers and decimals that round to the same double.
    Exact,
    Boolean,
    DateTime,
    Date,
    Time,
    /// Strings by code point: IRIs, blank node labels, lexical forms.
    Text,
    Language,
}

/// Where the values of a literal of [`Rank::Other`] go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OtherValue {
    Date,
    Time,
    Text,
}

/// The sort columns of one order comparator, common to all branches.
#[derive(Debug, Clone)]
pub(super) struct Layout {
    /// The slots in sort order.
    pub(super) slots: Vec<Slot>,
    /// The SQL type of [`Slot::Exact`]: `int8` when only integers that
    /// fit it are sorted there, so an integer column sorts as it is.
    exact: Num,
    /// Whether [`Slot::DateTime`] is `timestamptz` rather than `timestamp`.
    zoned: bool,
    /// Whether the infinite values of `date` and `timestamp` columns sort
    /// in the column's own slot rather than among strings.
    infinite_in_place: bool,
}

/// How one case of an order value sorts.
struct Sort<'v> {
    operand: Operand<'v>,
    rank: Rank,
    other: Option<OtherValue>,
    /// A `date` or `timestamp` column whose infinite values are plain
    /// literals, ranked as strings.
    infinite: Option<String>,
}

impl Slot {
    pub(super) fn collated(self) -> bool {
        matches!(self, Self::Datatype | Self::Text | Self::Language)
    }
}

impl<'a> Renderer<'a> {
    /// The sort columns of each order comparator of `plan`, from what the
    /// values of every branch can be.
    pub(super) fn order_layouts(&mut self, plan: &'a SqlPlan) -> Result<Vec<Layout>, Error> {
        let mut layouts = Vec::with_capacity(plan.order.len());
        for k in 0..plan.order.len() {
            let mut varies = false;
            let mut boolean = false;
            let mut sorts: Vec<Sort<'a>> = Vec::new();
            for branch in &plan.branches {
                self.from = &branch.from;
                match &branch.order[k] {
                    // An error has no value, so the rank varies.
                    OrderValue::Boolean(_) => boolean = true,
                    OrderValue::Term(cases) => {
                        if cases.is_empty() || cases.iter().any(|(guards, _)| !guards.is_empty()) {
                            varies = true;
                        }
                        for (_, value) in cases {
                            sorts.push(self.sort(value)?);
                        }
                    }
                }
            }
            layouts.push(self.layout(&sorts, varies, boolean)?);
        }
        Ok(layouts)
    }

    /// The sort columns for values sorted as `sorts`; `varies` when some
    /// rows have no value, `boolean` for a boolean expression.
    fn layout(&self, sorts: &[Sort<'_>], varies: bool, boolean: bool) -> Result<Layout, Error> {
        let mut layout = Layout {
            slots: Vec::new(),
            exact: Num::Integer,
            zoned: false,
            infinite_in_place: false,
        };
        let mut slots: Vec<Slot> = Vec::new();
        // The ranks the rows can have; a rank column only where they differ.
        let mut ranks: Vec<Rank> = sorts.iter().map(|s| s.rank).collect();
        if varies {
            ranks.push(Rank::Unbound);
        }
        if boolean {
            ranks.extend([Rank::Unbound, Rank::Boolean]);
            slots.push(Slot::Boolean);
        }
        // With no strings to sort among, the infinite values of a column
        // sort with its other values, below and above them: their order
        // against dateTimes and dates is open, and "-infinity" comes before
        // "infinity" as it should. The column then sorts as it is.
        layout.infinite_in_place = !sorts
            .iter()
            .any(|s| matches!(s.rank, Rank::String | Rank::Language));
        let mut numbers: Vec<(Num, bool)> = Vec::new();
        let mut zoned: Vec<bool> = Vec::new();
        let mut others: BTreeMap<String, OtherValue> = BTreeMap::new();
        for sort in sorts {
            if sort.infinite.is_some() && !layout.infinite_in_place {
                ranks.push(Rank::String);
                slots.push(Slot::Text);
            }
            match sort.rank {
                Rank::Unbound => {}
                Rank::BlankNode | Rank::Iri | Rank::String => slots.push(Slot::Text),
                Rank::Numeric => {
                    let Kind::Numeric(class) = sort.operand.kind else {
                        unreachable!("numeric rank for numeric kinds")
                    };
                    let fits = match &sort.operand.constant {
                        Some(Value::Numeric(n)) => n.int8().is_some(),
                        _ => true,
                    };
                    numbers.push((class, fits));
                }
                Rank::Boolean => slots.push(Slot::Boolean),
                Rank::DateTime => {
                    let Kind::DateTime { zoned: z } = sort.operand.kind else {
                        unreachable!("dateTime rank for dateTime kinds")
                    };
                    zoned.push(z);
                    slots.push(Slot::DateTime);
                }
                Rank::Language => slots.extend([Slot::Text, Slot::Language]),
                Rank::Other => {
                    let other = sort.other.expect("other rank has its value slot");
                    slots.push(match other {
                        OtherValue::Date => Slot::Date,
                        OtherValue::Time => Slot::Time,
                        OtherValue::Text => Slot::Text,
                    });
                    let datatype = match self.datatype(&sort.operand) {
                        Text::Constant(d) | Text::NaturalDatatype { datatype: d, .. } => d,
                        Text::Term(_) | Text::Unknown => String::new(),
                    };
                    if others
                        .insert(datatype.clone(), other)
                        .is_some_and(|o| o != other)
                    {
                        return Err(unsupported(&format!(
                            "ORDER BY over literals of datatype <{datatype}> from columns of \
                             different SQL types, which sort them differently"
                        )));
                    }
                }
            }
        }
        if others.len() > 1 {
            slots.push(Slot::Datatype);
        }
        ranks.sort_unstable();
        ranks.dedup();
        if ranks.len() > 1 {
            slots.push(Slot::Rank);
        }
        if !numbers.is_empty() {
            let double = numbers.iter().any(|&(c, _)| c == Num::Double);
            let exact: Vec<&(Num, bool)> =
                numbers.iter().filter(|&&(c, _)| c != Num::Double).collect();
            if double {
                slots.push(Slot::Double);
            }
            if !exact.is_empty() {
                slots.push(Slot::Exact);
                layout.exact = if exact.iter().all(|&&(c, fits)| c == Num::Integer && fits) {
                    Num::Integer
                } else {
                    Num::Decimal
                };
            }
        }
        zoned.sort_unstable();
        zoned.dedup();
        if zoned.len() > 1 {
            // XPath compares them through an implicit time zone that SPARQL
            // leaves to the implementation (XPath F&O §10.4).
            return Err(unsupported(
                "ORDER BY over xsd:dateTime values with and without a time zone, whose order \
                 depends on an implicit time zone",
            ));
        }
        layout.zoned = zoned.first().copied().unwrap_or(false);
        slots.sort_unstable();
        slots.dedup();
        if slots.contains(&Slot::Text) && self.options.non_utf8 {
            return Err(unsupported(
                "ORDER BY over strings or IRIs in a database whose encoding is not UTF-8, where \
                 byte order is not code point order",
            ));
        }
        layout.slots = slots;
        Ok(layout)
    }

    /// How a term sorts.
    fn sort<'v>(&self, value: &'v FilterValue) -> Result<Sort<'v>, Error> {
        let mut operand = self.operand(value);
        if operand.kind == Kind::Unknown {
            // Without column types, as a string, which shows the SQL of
            // the common case.
            operand.kind = Kind::String;
        }
        let mut other = None;
        let rank = match &operand.kind {
            Kind::Error => Rank::Unbound,
            Kind::BlankNode => Rank::BlankNode,
            Kind::Iri => Rank::Iri,
            Kind::Numeric(_) => Rank::Numeric,
            Kind::Boolean => Rank::Boolean,
            Kind::DateTime { .. } => Rank::DateTime,
            Kind::String | Kind::Unknown => Rank::String,
            Kind::Lang(_) => Rank::Language,
            Kind::Other(_) | Kind::IllTyped(_) => {
                // Natural literals of a type whose lexical form SQL does not
                // compute sort by the column's own order.
                let ty = term_column(&operand)
                    .and_then(|(table, name)| self.column_type(table, name))
                    .filter(|ty| ty.natural_datatype.is_some() && ty.lexical.is_none());
                other = Some(match ty.map(|ty| ty.name.as_str()) {
                    Some("date") => OtherValue::Date,
                    Some("time") => OtherValue::Time,
                    _ => OtherValue::Text,
                });
                Rank::Other
            }
            Kind::Unsupported(reason) => return Err(unsupported(reason)),
        };
        // Only a dateTime or a date is infinite; a column read as any other
        // kind fails where its lexical form is needed.
        let infinite = match rank {
            Rank::DateTime | Rank::Other if operand.infinite => {
                term_column(&operand).map(|(table, name)| super::column_sql(table, name))
            }
            _ => None,
        };
        Ok(Sort {
            operand,
            rank,
            other,
            infinite,
        })
    }

    /// The SQL of `slot` of order comparator `key` in the rows of `branch`.
    pub(super) fn order_slot(
        &mut self,
        branch: &'a SqlBranch,
        key: usize,
        layout: &Layout,
        slot: Slot,
    ) -> Result<(), Error> {
        self.from = &branch.from;
        let cases = match &branch.order[key] {
            OrderValue::Boolean(filter) => {
                match slot {
                    Slot::Rank => {
                        self.sql.push_str("CASE WHEN (");
                        self.filter(filter, Mode::Exact)?;
                        self.sql.push_str(&format!(
                            ") IS NULL THEN 0 ELSE {} END",
                            Rank::Boolean as u8
                        ));
                    }
                    Slot::Boolean => {
                        self.sql.push('(');
                        self.filter(filter, Mode::Exact)?;
                        self.sql.push(')');
                    }
                    _ => self.sql.push_str(&null(layout, slot)),
                }
                return Ok(());
            }
            OrderValue::Term(cases) => cases,
        };
        let guarded = cases.len() > 1 || cases.iter().any(|(guards, _)| !guards.is_empty());
        if !guarded {
            return match cases.first() {
                Some((_, value)) => self.slot_value(value, layout, slot),
                None => {
                    self.sql.push_str(&if slot == Slot::Rank {
                        (Rank::Unbound as u8).to_string()
                    } else {
                        null(layout, slot)
                    });
                    Ok(())
                }
            };
        }
        self.sql.push_str("CASE");
        for (guards, value) in cases {
            self.sql.push_str(" WHEN ");
            if guards.is_empty() {
                self.sql.push_str("true");
            } else {
                let guards: Vec<&Condition> = guards.iter().collect();
                self.conditions(&guards)?;
            }
            self.sql.push_str(" THEN ");
            self.slot_value(value, layout, slot)?;
        }
        if slot == Slot::Rank {
            self.sql.push_str(&format!(" ELSE {}", Rank::Unbound as u8));
        }
        self.sql.push_str(" END");
        Ok(())
    }

    /// The SQL of `slot` for a term: its sort value, or `NULL` where its
    /// rank does not use the slot.
    fn slot_value(
        &mut self,
        value: &FilterValue,
        layout: &Layout,
        slot: Slot,
    ) -> Result<(), Error> {
        let mut sort = self.sort(value)?;
        if layout.infinite_in_place {
            sort.infinite = None;
        }
        let operand = &sort.operand;
        // Infinite values of the column are plain literals.
        let (finite, infinite) = match &sort.infinite {
            Some(column) => (
                Some(format!("isfinite({column})")),
                Some(format!("NOT isfinite({column})")),
            ),
            None => (None, None),
        };
        let open = |this: &mut Self, guard: &Option<String>| {
            if let Some(guard) = guard {
                this.sql.push_str(&format!("CASE WHEN {guard} THEN "));
            }
        };
        let close = |this: &mut Self, guard: &Option<String>| {
            if guard.is_some() {
                this.sql.push_str(" END");
            }
        };
        let uses = match slot {
            Slot::Rank => {
                match &finite {
                    Some(finite) => self.sql.push_str(&format!(
                        "CASE WHEN {finite} THEN {} ELSE {} END",
                        sort.rank as u8,
                        Rank::String as u8
                    )),
                    None => self.sql.push_str(&(sort.rank as u8).to_string()),
                }
                return Ok(());
            }
            Slot::Datatype => sort.rank == Rank::Other,
            Slot::Double => matches!(operand.kind, Kind::Numeric(_)),
            Slot::Exact => matches!(operand.kind, Kind::Numeric(c) if c != Num::Double),
            Slot::Boolean => sort.rank == Rank::Boolean,
            Slot::DateTime => sort.rank == Rank::DateTime,
            Slot::Date => sort.other == Some(OtherValue::Date),
            Slot::Time => sort.other == Some(OtherValue::Time),
            Slot::Text => {
                matches!(
                    sort.rank,
                    Rank::BlankNode | Rank::Iri | Rank::String | Rank::Language
                ) || sort.other == Some(OtherValue::Text)
                    || infinite.is_some()
            }
            Slot::Language => sort.rank == Rank::Language,
        };
        if !uses {
            self.sql.push_str(&null(layout, slot));
            return Ok(());
        }
        match slot {
            Slot::Rank => unreachable!("handled above"),
            Slot::Datatype => {
                open(self, &finite);
                match self.datatype(operand) {
                    Text::Constant(datatype) | Text::NaturalDatatype { datatype, .. } => {
                        self.expr(&super::SqlExpr::Param(datatype.as_str().into()));
                        self.sql.push_str("::text");
                    }
                    Text::Term(_) | Text::Unknown => self.sql.push_str("NULL::text"),
                }
                close(self, &finite);
            }
            Slot::Double | Slot::Exact => {
                let Kind::Numeric(class) = operand.kind else {
                    unreachable!("numeric slots for numeric kinds")
                };
                let ty = if slot == Slot::Double {
                    Num::Double
                } else {
                    layout.exact
                };
                let number = match &operand.constant {
                    Some(Value::Numeric(n)) => Some(n.clone()),
                    _ => None,
                };
                self.number(operand, number.as_ref(), class, ty)?;
            }
            Slot::Boolean => self.typed_value(operand, "bool")?,
            Slot::DateTime => {
                open(self, &finite);
                let ty = if layout.zoned {
                    "timestamptz"
                } else {
                    "timestamp"
                };
                self.typed_value(operand, ty)?;
                close(self, &finite);
            }
            Slot::Date | Slot::Time => {
                open(self, &finite);
                let (table, name) = term_column(operand).expect("date and time slots for columns");
                self.sql.push_str(&super::column_sql(table, name));
                close(self, &finite);
            }
            Slot::Text => match (&sort.infinite, &infinite) {
                // The text output of the infinite values is their lexical
                // form (see the decoder).
                (Some(column), Some(infinite)) => self
                    .sql
                    .push_str(&format!("CASE WHEN {infinite} THEN {column}::text END")),
                // Parenthesized where it is an expression, so a `COLLATE`
                // after it applies to the whole of it.
                _ => self.text(operand)?,
            },
            Slot::Language => {
                let Kind::Lang(tag) = &operand.kind else {
                    unreachable!("language slot for language-tagged literals")
                };
                self.expr(&super::SqlExpr::Param(tag.as_str().into()));
                self.sql.push_str("::text");
            }
        }
        Ok(())
    }
}

/// The column of a term built from a column alone.
fn term_column<'v>(operand: &Operand<'v>) -> Option<(usize, &'v str)> {
    match &operand.text {
        Text::Term(term) => match &term.value {
            super::TermSqlValue::Column { table, name } => Some((*table, name.as_str())),
            _ => None,
        },
        _ => None,
    }
}

/// A typed `NULL` for a slot a row has no value in, so that `UNION`
/// resolves the slot's type from any branch.
fn null(layout: &Layout, slot: Slot) -> String {
    let ty = match slot {
        Slot::Rank => "int4",
        Slot::Datatype | Slot::Text | Slot::Language => "text",
        Slot::Double => "float8",
        Slot::Exact => layout.exact.sql_type(),
        Slot::Boolean => "bool",
        Slot::DateTime if layout.zoned => "timestamptz",
        Slot::DateTime => "timestamp",
        Slot::Date => "date",
        Slot::Time => "time",
    };
    format!("NULL::{ty}")
}

/// The alias of a slot's result column.
pub(super) fn slot_name(slot: Slot) -> &'static str {
    match slot {
        Slot::Rank => "rank",
        Slot::Datatype => "datatype",
        Slot::Double => "double",
        Slot::Exact => "exact",
        Slot::Boolean => "boolean",
        Slot::DateTime => "datetime",
        Slot::Date => "date",
        Slot::Time => "time",
        Slot::Text => "text",
        Slot::Language => "language",
    }
}

#[cfg(test)]
mod tests {
    use super::super::filter::tests::{plan, types};
    use super::super::{RenderOptions, TableKeys};
    use crate::Error;
    use crate::mapping::LogicalTable;

    /// The SQL of a query over table `t`, whose key `i4` the subject
    /// template fixes, so one branch needs no `DISTINCT`.
    fn render(query: &str, non_utf8: bool) -> Result<String, Error> {
        let types = types();
        let keys: TableKeys = [(
            LogicalTable::TableName("t".into()),
            vec![vec!["i4".to_owned()]],
        )]
        .into();
        Ok(plan(query)?
            .to_sql_with(&RenderOptions {
                column_types: Some(&types),
                keys: Some(&keys),
                non_utf8,
                ..RenderOptions::default()
            })?
            .sql)
    }

    /// The sort keys `?s ex:<predicate> ?x` ordered by `order` selects
    /// after `?s` and `?x`, and its `ORDER BY` clause, which sorts by them.
    fn order_by(predicate: &str, order: &str) -> String {
        let sql = render(
            &format!("SELECT ?s ?x WHERE {{ ?s ex:{predicate} ?x }} ORDER BY {order}"),
            false,
        )
        .unwrap();
        let select = sql.lines().next().unwrap_or_default();
        let keys = select.split_once(" AS \"x\", ").map_or("", |(_, k)| k);
        let order = sql.split("\nORDER BY ").nth(1).unwrap_or_default();
        format!("{keys} | {order}")
    }

    fn rejected(query: &str) -> String {
        match render(query, false) {
            Err(Error::UnsupportedFeature(reason)) => reason,
            other => panic!("{query}: {other:?}"),
        }
    }

    /// A required column of one kind sorts as it is, so an index on it can
    /// deliver the rows in order: numbers, booleans and dateTimes by their
    /// SQL order, which is SPARQL's.
    #[test]
    fn one_column_of_one_kind_sorts_as_it_is() {
        assert_eq!(
            order_by("i4", "?x"),
            "t0.\"i4\" AS \"o1_exact\" | \"o1_exact\""
        );
        assert_eq!(
            order_by("i8", "DESC(?x)"),
            "t0.\"i8\" AS \"o1_exact\" | \"o1_exact\" DESC"
        );
        assert_eq!(
            order_by("dec", "?x"),
            "t0.\"dec\" AS \"o1_exact\" | \"o1_exact\""
        );
        assert_eq!(
            order_by("f8", "?x"),
            "t0.\"f8\" AS \"o1_double\" | \"o1_double\""
        );
        assert_eq!(
            order_by("b", "?x"),
            "t0.\"b\" AS \"o1_boolean\" | \"o1_boolean\""
        );
        // Infinite timestamps and dates are strings, which no other value
        // here is: they stay at the column's ends.
        assert_eq!(
            order_by("ts", "?x"),
            "t0.\"ts\" AS \"o1_datetime\" | \"o1_datetime\""
        );
        assert_eq!(order_by("d", "?x"), "t0.\"d\" AS \"o1_date\" | \"o1_date\"");
        assert_eq!(
            order_by("txt", "?x"),
            "(t0.\"txt\"::text) COLLATE \"C\" AS \"o1_text\" | \"o1_text\""
        );
        assert_eq!(
            order_by("u", "DESC(?x)"),
            "(t0.\"u\"::text) COLLATE \"C\" AS \"o1_text\" | \"o1_text\" DESC"
        );
        assert_eq!(
            order_by("label", "?x"),
            "(t0.\"txt\"::text) COLLATE \"C\" AS \"o1_text\", $1::text COLLATE \"C\" AS \
             \"o1_language\" | \"o1_text\", \"o1_language\""
        );
        assert!(order_by("link", "?x").starts_with("($1 || CASE WHEN t0.\"i4\"::text IS NULL"));
    }

    #[test]
    fn expressions_rank_their_errors_as_no_value() {
        assert_eq!(
            order_by("i4", "(?x > 3)"),
            "CASE WHEN (t0.\"i4\" > $1::int8) IS NULL THEN 0 ELSE 4 END AS \"o1_rank\", \
             (t0.\"i4\" > $2::int8) AS \"o1_boolean\" | \"o1_rank\", \"o1_boolean\""
        );
        assert_eq!(
            order_by("i4", "STR(?s)").matches("COLLATE \"C\"").count(),
            1
        );
        // A variable the pattern does not bind has no value: no sort key.
        assert_eq!(order_by("i4", "?nothing"), " | ");
    }

    /// Numbers of several types sort as doubles where some are doubles,
    /// then exactly, so that integers and decimals keep their order.
    #[test]
    fn numbers_of_several_types_sort_as_doubles_then_exactly() {
        let sql = render(
            "SELECT ?s WHERE { { ?s ex:i4 ?x } UNION { ?s ex:dec ?x } UNION { ?s ex:f8 ?x } } \
             ORDER BY DESC(?x)",
            false,
        )
        .unwrap();
        assert!(
            sql.contains("t0.\"i4\"::float8 AS \"o1_double\", t0.\"i4\"::numeric AS \"o1_exact\""),
            "{sql}"
        );
        assert!(
            sql.contains("t0.\"f8\" AS \"o1_double\", NULL::numeric AS \"o1_exact\""),
            "{sql}"
        );
        assert!(
            sql.ends_with("ORDER BY q.\"o1_double\" DESC, q.\"o1_exact\" DESC"),
            "{sql}"
        );
        // Integers alone keep their type.
        let sql = render(
            "SELECT ?s WHERE { { ?s ex:i4 ?x } UNION { ?s ex:i8 ?x } } ORDER BY ?x",
            false,
        )
        .unwrap();
        assert!(sql.ends_with("ORDER BY q.\"o1_exact\""), "{sql}");
        assert!(sql.contains("t0.\"i4\" AS \"o1_exact\""), "{sql}");
    }

    /// Unbound sorts first; with strings, infinite timestamps sort among
    /// them as the plain literals they are.
    #[test]
    fn ranks_order_unbound_values_and_kinds() {
        let sql = render(
            "SELECT ?s WHERE { ?s ex:i4 ?i OPTIONAL { ?s ex:b ?x } } ORDER BY ?x",
            false,
        )
        .unwrap();
        assert!(
            sql.contains(
                "CASE WHEN t1.\"b\" IS NOT NULL THEN 4 ELSE 0 END AS \"o1_rank\", \
                 CASE WHEN t1.\"b\" IS NOT NULL THEN t1.\"b\" END AS \"o1_boolean\""
            ),
            "{sql}"
        );
        assert!(
            sql.ends_with("ORDER BY \"o1_rank\", \"o1_boolean\""),
            "{sql}"
        );
        let sql = render(
            "SELECT ?s WHERE { { ?s ex:ts ?x } UNION { ?s ex:txt ?x } } ORDER BY ?x",
            false,
        )
        .unwrap();
        assert!(
            sql.contains(
                "CASE WHEN isfinite(t0.\"ts\") THEN 5 ELSE 6 END AS \"o1_rank\", \
                 CASE WHEN isfinite(t0.\"ts\") THEN t0.\"ts\" END AS \"o1_datetime\", \
                 CASE WHEN NOT isfinite(t0.\"ts\") THEN t0.\"ts\"::text END AS \"o1_text\""
            ),
            "{sql}"
        );
    }

    #[test]
    fn what_sql_cannot_order_exactly_is_rejected() {
        assert!(
            rejected("SELECT ?s WHERE { { ?s ex:ts ?x } UNION { ?s ex:tz ?x } } ORDER BY ?x")
                .contains("with and without a time zone")
        );
        assert!(rejected("SELECT ?s WHERE { ?s ex:f4 ?x } ORDER BY ?x").contains("float4"));
        assert!(
            rejected("SELECT ?s WHERE { ?s ex:intText ?x } ORDER BY ?x").contains("rr:datatype")
        );
        assert!(
            rejected("SELECT DISTINCT ?s WHERE { ?s ex:i4 ?x } ORDER BY ?x")
                .contains("SELECT DISTINCT does not project")
        );
        match render("SELECT ?s WHERE { ?s ex:txt ?x } ORDER BY ?x", true) {
            Err(Error::UnsupportedFeature(reason)) => assert!(reason.contains("UTF-8")),
            other => panic!("{other:?}"),
        }
        // Without strings, the encoding does not matter.
        assert!(render("SELECT ?s WHERE { ?s ex:i4 ?x } ORDER BY ?x", true).is_ok());
    }

    /// `LIMIT` and `OFFSET` are bound parameters after the order; a plan
    /// with `DISTINCT` sorts the selected keys outside it.
    #[test]
    fn limit_and_offset_follow_the_order() {
        let plan =
            plan("SELECT DISTINCT ?x WHERE { ?s ex:i4 ?x } ORDER BY ?x LIMIT 5 OFFSET 2").unwrap();
        let rendered = plan.to_sql();
        assert!(
            rendered.sql.ends_with(
                ") q\nORDER BY q.\"o1_text\" COLLATE \"C\"\nLIMIT $1::int8\nOFFSET $2::int8"
            ),
            "{}",
            rendered.sql
        );
        assert_eq!(rendered.params, ["5", "2"]);
        assert_eq!(rendered.param_required, [false, false]);
    }
}
