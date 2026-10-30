// SPDX-License-Identifier: Apache-2.0
//! SQL for arithmetic on `xsd:double` values with the IEEE 754 results that
//! XPath requires (F&O 3.1 §4.2). PostgreSQL's `float8` operators raise an
//! error where IEEE 754 overflows to an infinity, rounds a product or a
//! quotient to zero, or divides by zero, so each operator first decides
//! those cases with operations that cannot fail, and is exact in all of
//! them.
//!
//! The SQL of an operator repeats its operands. An operand that is not a
//! column or a parameter is computed once, in a scalar subquery, so that
//! nested arithmetic stays linear in size.

use super::super::{FilterValue, Renderer, SqlExpr};
use super::{Kind, Operand, decimal_guard, is_arith};
use crate::Error;
use crate::dialect::{Dialect, SqlType};
use crate::solution::canonical_double;
use crate::sparql::{ArithOp, CompareOp};
use crate::xsd::{Num, Value};

/// A `float8` value as SQL.
struct Double {
    sql: String,
    /// A column, a parameter, or a cast or negation of one: cheap to
    /// repeat.
    simple: bool,
    /// Whether it can be NaN, which XPath finds neither equal to nor
    /// ordered with anything (op:numeric-equal, op:numeric-less-than),
    /// while PostgreSQL finds `NaN = NaN` and orders NaN last.
    nan: bool,
}

impl Double {
    fn simple(sql: String, nan: bool) -> Self {
        Self {
            sql,
            simple: true,
            nan,
        }
    }

    fn computed(sql: String, nan: bool) -> Self {
        Self {
            sql,
            simple: false,
            nan,
        }
    }
}

impl Renderer<'_> {
    /// A number as `float8`: arithmetic of class `xsd:double`, or an
    /// integer or decimal promoted to a double.
    pub(super) fn double_value(&mut self, value: &FilterValue) -> Result<(), Error> {
        let double = self.double(value)?;
        self.sql.push_str(&double.sql);
        Ok(())
    }

    /// A comparison of numbers promoted to doubles, false where either is
    /// NaN.
    pub(super) fn double_compare(
        &mut self,
        op: CompareOp,
        l: &Operand<'_>,
        r: &Operand<'_>,
    ) -> Result<(), Error> {
        let (l, r) = (self.double(l.value)?, self.double(r.value)?);
        let nan = self.dialect.cast("'NaN'", &SqlType::Double);
        let sql = bind(&[&l, &r], |o| {
            let mut sql = format!("{} {} {}", o[0], op.sql(), o[1]);
            for (side, double) in o.iter().zip([&l, &r]) {
                if double.nan {
                    sql.push_str(&format!(" AND {side} <> {nan}"));
                }
            }
            format!("({sql})")
        });
        self.sql.push_str(&sql);
        Ok(())
    }

    /// The effective boolean value of a double (SPARQL 1.1 §17.2.2): false
    /// for zero and NaN.
    pub(super) fn double_ebv(&mut self, value: &FilterValue) -> Result<(), Error> {
        let double = self.double(value)?;
        let nan = self.dialect.cast("'NaN'", &SqlType::Double);
        let sql = bind(&[&double], |o| format!("({0} <> 0 AND {0} <> {nan})", o[0]));
        self.sql.push_str(&sql);
        Ok(())
    }

    /// A `numeric` column of decimals promoted to a double, `NULL` for
    /// its NaN and infinities, which are not `xsd:decimal` values.
    pub(super) fn decimal_column_as_double(&self, column: &str) -> String {
        from_numeric(
            self.dialect,
            column,
            Some(&decimal_guard(self.dialect, column)),
        )
    }

    fn double(&mut self, value: &FilterValue) -> Result<Double, Error> {
        if let Some(constant) = self.constant_double(value) {
            let param = self.double_param(constant);
            return Ok(Double::simple(param, constant.is_nan()));
        }
        match (self.operand(value).kind, value) {
            (Kind::Unsupported(reason), _) => Err(super::unsupported(&reason)),
            (Kind::Numeric(Num::Double), FilterValue::Arith(op, a, b)) => {
                self.double_arith(*op, a, b)
            }
            // IEEE 754 negation cannot fail.
            (Kind::Numeric(Num::Double), FilterValue::Negate(a)) => {
                let a = self.double(a)?;
                Ok(Double {
                    sql: format!("(-{})", a.sql),
                    ..a
                })
            }
            (Kind::Numeric(_), _) if is_arith(value) => self.promoted(value),
            _ => Ok(self.leaf_double(value)),
        }
    }

    fn double_arith(
        &mut self,
        op: ArithOp,
        a: &FilterValue,
        b: &FilterValue,
    ) -> Result<Double, Error> {
        let (a, b) = (self.double(a)?, self.double(b)?);
        let dialect = self.dialect;
        let sql = bind(&[&a, &b], |o| match op {
            ArithOp::Add | ArithOp::Subtract => add(dialect, o[0], op.sql(), o[1]),
            ArithOp::Multiply => multiply(dialect, o[0], o[1]),
            ArithOp::Divide => divide(dialect, o[0], o[1]),
        });
        Ok(Double::computed(sql, true))
    }

    /// Arithmetic on integers and decimals, computed exactly in `numeric`,
    /// then promoted to a double.
    fn promoted(&mut self, value: &FilterValue) -> Result<Double, Error> {
        let numeric = self.rendered(|this| this.numeric_value(value))?;
        let dialect = self.dialect;
        let sql = bind(&[&Double::computed(numeric, false)], |o| {
            from_numeric(dialect, o[0], None)
        });
        Ok(Double::computed(sql, false))
    }

    /// A column, or a type error, as a double.
    fn leaf_double(&self, value: &FilterValue) -> Double {
        let operand = self.operand(value);
        let null = || Double::simple(self.dialect.cast("NULL", &SqlType::Double), false);
        let Some((table, name)) = operand.column else {
            return null();
        };
        let column = super::super::column_sql(table, name);
        match operand.kind {
            Kind::Numeric(Num::Double) => Double::simple(column, true),
            Kind::Numeric(Num::Decimal) => {
                Double::computed(self.decimal_column_as_double(&column), false)
            }
            // Integers convert exactly rounded and never overflow; without
            // column types, as if the column held integers.
            Kind::Numeric(Num::Integer) | Kind::Unknown => {
                Double::simple(self.dialect.cast(&column, &SqlType::Double), false)
            }
            _ => null(),
        }
    }

    /// The value of a double computed from constants only, which Rust's
    /// `f64` computes with IEEE 754 results; integer and decimal constants
    /// are promoted to their nearest double (F&O 3.1 §19.1.2.2).
    /// Arithmetic on integers and decimals stays in SQL, which computes it
    /// exactly.
    fn constant_double(&self, value: &FilterValue) -> Option<f64> {
        match value {
            FilterValue::Constant(_) => match &self.operand(value).constant {
                Some(Value::Numeric(n)) => Some(n.double),
                _ => None,
            },
            FilterValue::Arith(op, a, b)
                if self.operand(value).kind == Kind::Numeric(Num::Double) =>
            {
                let (a, b) = (self.constant_double(a)?, self.constant_double(b)?);
                Some(match op {
                    ArithOp::Add => a + b,
                    ArithOp::Subtract => a - b,
                    ArithOp::Multiply => a * b,
                    ArithOp::Divide => a / b,
                })
            }
            FilterValue::Negate(a) if self.operand(value).kind == Kind::Numeric(Num::Double) => {
                Some(-self.constant_double(a)?)
            }
            _ => None,
        }
    }

    fn double_param(&mut self, value: f64) -> String {
        self.rendered(|this| {
            this.cast_rendered(&SqlType::Double, |this| {
                this.expr(&SqlExpr::Param(canonical_double(value).into()))
            });
            Ok(())
        })
        .expect("a parameter renders")
    }

    /// The SQL that `render` writes, taken out of the statement.
    fn rendered(
        &mut self,
        render: impl FnOnce(&mut Self) -> Result<(), Error>,
    ) -> Result<String, Error> {
        let start = self.sql.len();
        render(self)?;
        Ok(self.sql.split_off(start))
    }
}

/// `body` over the operands, where an operand that is not simple is
/// computed once in a scalar subquery and named there.
fn bind(operands: &[&Double], body: impl FnOnce(&[&str]) -> String) -> String {
    const NAMES: [&str; 2] = ["x", "y"];
    let mut computed = Vec::new();
    let names: Vec<&str> = operands
        .iter()
        .zip(NAMES)
        .map(|(operand, name)| {
            if operand.simple {
                operand.sql.as_str()
            } else {
                computed.push(format!("{} AS {name}", operand.sql));
                name
            }
        })
        .collect();
    let body = body(&names);
    if computed.is_empty() {
        body
    } else {
        format!("(SELECT {body} FROM (SELECT {}) AS d)", computed.join(", "))
    }
}

/// `x` as a `float8` constant: the shortest decimal that reads back as
/// it, so the database reads exactly `x`.
fn literal(dialect: &dyn Dialect, x: f64) -> String {
    let text = if x.is_nan() {
        "NaN".to_owned()
    } else if x.is_infinite() {
        if x > 0.0 { "Infinity" } else { "-Infinity" }.to_owned()
    } else {
        format!("{x:e}")
    };
    dialect.cast(&format!("'{text}'"), &SqlType::Double)
}

/// `negative ? -x : x` for a constant `x`.
fn signed(dialect: &dyn Dialect, negative: &str, x: f64) -> String {
    format!(
        "CASE WHEN {negative} THEN {} ELSE {} END",
        literal(dialect, -x),
        literal(dialect, x)
    )
}

/// `|x|` between `low` and the largest finite double: false for NaN and
/// the infinities, which PostgreSQL orders above every finite value.
fn magnitude_from(dialect: &dyn Dialect, x: &str, low: f64) -> String {
    format!(
        "abs({x}) BETWEEN {} AND {}",
        literal(dialect, low),
        literal(dialect, f64::MAX)
    )
}

/// op:numeric-add or op:numeric-subtract (`op` `+` or `-`).
///
/// An overflow needs both magnitudes at least 2^970: otherwise the exact
/// result stays below 2^1024 − 2^970, the midpoint between the largest
/// double and 2^1024, and rounds to a finite double. Halving such operands
/// is exact, and their halved sum rounds as the sum does, without
/// overflowing: the sum overflows exactly when the halved sum rounds to
/// 2^1023 or more. Both operands then have the sign of the result.
fn add(dialect: &dyn Dialect, a: &str, op: &str, b: &str) -> String {
    let half = literal(dialect, 0.5);
    let overflow = format!(
        "CASE WHEN {} AND {} THEN abs({a} * {half} {op} {b} * {half}) >= {} END",
        magnitude_from(dialect, a, 2f64.powi(970)),
        magnitude_from(dialect, b, 2f64.powi(970)),
        literal(dialect, 2f64.powi(1023)),
    );
    format!(
        "CASE WHEN {overflow} THEN {} ELSE {a} {op} {b} END",
        signed(dialect, &format!("{a} < 0"), f64::INFINITY)
    )
}

/// op:numeric-multiply.
///
/// An overflow needs both magnitudes above 1, since otherwise `|a × b|`
/// is at most the other one. Scaled by 2^-512 each, which is then exact,
/// the product is a normal double that rounds as `a × b` does, and is 1 or
/// more exactly when `a × b` rounds to 2^1024 or more.
///
/// A product rounds to zero when its magnitude is at most 2^-1075, half
/// the smallest subnormal (a tie rounds to the even zero). That needs both
/// magnitudes below 1 and one below 2^-537, and [`at_most_half_minimum`]
/// decides it exactly.
fn multiply(dialect: &dyn Dialect, a: &str, b: &str) -> String {
    let opposite = format!("({a} < 0) <> ({b} < 0)");
    let scale = literal(dialect, 2f64.powi(-512));
    let overflow = format!(
        "CASE WHEN {} AND {} THEN abs(({a} * {scale}) * ({b} * {scale})) >= 1 END",
        magnitude_from(dialect, a, 1.0),
        magnitude_from(dialect, b, 1.0),
    );
    let small = literal(dialect, 2f64.powi(-537));
    let underflow = format!(
        "CASE WHEN {a} <> 0 AND {b} <> 0 AND abs({a}) < 1 AND abs({b}) < 1 \
         AND (abs({a}) < {small} OR abs({b}) < {small}) THEN {} END",
        at_most_half_minimum(dialect, a, b)
    );
    format!(
        "CASE WHEN {overflow} THEN {} WHEN {underflow} THEN {} ELSE {a} * {b} END",
        signed(dialect, &opposite, f64::INFINITY),
        signed(dialect, &opposite, 0.0)
    )
}

/// Whether `|a × b| ≤ 2^-1075`, for non-zero `a` and `b` below 1 in
/// magnitude, one of them below 2^-537.
///
/// Scaled exactly to `m = |a| × 2^538` and `n = |b| × 2^537`, the question
/// is whether `m × n ≤ 1`, and `m × n` lies between 2^-1073 and 2^538, so
/// computing it cannot fail. Its rounded value answers unless it is
/// exactly 1; then Dekker's product (Dekker 1971, with Veltkamp's split
/// into halves of 26 bits) gives the exact error `m × n − 1`, whose terms
/// are far from overflow and underflow.
fn at_most_half_minimum(dialect: &dyn Dialect, a: &str, b: &str) -> String {
    let split = literal(dialect, 134_217_729.0);
    format!(
        "(SELECT CASE WHEN m * n < 1 THEN true WHEN m * n > 1 THEN false \
         ELSE ((mh * nh - 1) + mh * nl + ml * nh) + ml * nl <= 0 END \
         FROM (SELECT m, n, mh, m - mh AS ml, nh, n - nh AS nl \
         FROM (SELECT m, n, m * {split} - (m * {split} - m) AS mh, \
         n * {split} - (n * {split} - n) AS nh \
         FROM (SELECT abs({a}) * {} AS m, abs({b}) * {} AS n) AS s) AS h) AS l)",
        literal(dialect, 2f64.powi(538)),
        literal(dialect, 2f64.powi(537)),
    )
}

/// op:numeric-divide.
///
/// A zero divisor gives NaN for a zero or NaN dividend, and otherwise an
/// infinity whose sign is that of the dividend times that of the zero.
///
/// A quotient of finite non-zero operands overflows only if `|b| < 1` and
/// `|a| ≥ |b| × 2^53` (exact, as `|b| × 2^53 < 2^53`). Then `|a| / |b|` lies
/// between 2^53 and 2^2098, and scaled by 2^-1023 (for `|a| < 1`) or
/// 2^-1075 (for `|a| ≥ 1`, through `a × 2^-52`, exact there) it is a normal
/// double that rounds as the quotient does.
///
/// A quotient rounds to zero when `|a| / |b| ≤ 2^-1075`, that is
/// `|a| × 2^1075 ≤ |b|`, which needs `|a| < 2^-51`; then `|a| × 2^1075` is
/// an exact finite double, so the comparison is exact.
fn divide(dialect: &dyn Dialect, a: &str, b: &str) -> String {
    let opposite = format!("({a} < 0) <> ({b} < 0)");
    let max = literal(dialect, f64::MAX);
    let by_zero = format!(
        "CASE WHEN {a} <> 0 AND {a} <= {} THEN {} WHEN {a} IS NOT NULL THEN {} END",
        literal(dialect, f64::INFINITY),
        signed(
            dialect,
            &format!("({a} < 0) <> ({})", dialect.sign_bit(b)),
            f64::INFINITY
        ),
        literal(dialect, f64::NAN),
    );
    let up = literal(dialect, 2f64.powi(1023));
    let overflow = format!(
        "CASE WHEN {a} <> 0 AND abs({a}) <= {max} AND abs({b}) < 1 AND abs({a}) >= abs({b}) * {} \
         THEN CASE WHEN abs({a}) < 1 THEN abs({a} / ({b} * {up})) >= 2 \
         ELSE abs(({a} * {}) / ({b} * {up})) >= {} END END",
        literal(dialect, 2f64.powi(53)),
        literal(dialect, 2f64.powi(-52)),
        literal(dialect, 2f64.powi(-51)),
    );
    let underflow = format!(
        "CASE WHEN {a} <> 0 AND abs({a}) < {} AND abs({b}) <= {max} \
         THEN abs(({a} * {}) * {}) <= abs({b}) END",
        literal(dialect, 2f64.powi(-51)),
        literal(dialect, 2f64.powi(538)),
        literal(dialect, 2f64.powi(537)),
    );
    format!(
        "CASE WHEN {b} = 0 THEN {by_zero} WHEN {overflow} THEN {} WHEN {underflow} THEN {} \
         ELSE {a} / {b} END",
        signed(dialect, &opposite, f64::INFINITY),
        signed(dialect, &opposite, 0.0)
    )
}

/// The `numeric` decimal `d` promoted to a double: the nearest double,
/// ties to even, an infinity from 2^1024 − 2^970 in magnitude and a zero
/// of the decimal's sign at 2^-1075 or less (F&O 3.1 §19.1.2.2 through the
/// lexical mapping of XML Schema 1.1 §3.3.5.2, `floatingPointRound`).
/// PostgreSQL rounds the rest the same, and raises an error for those.
/// With `decimal`, `NULL` where it does not hold.
fn from_numeric(dialect: &dyn Dialect, d: &str, decimal: Option<&str>) -> String {
    let two = dialect.cast("2", &SqlType::Numeric);
    let ill_typed = decimal.map_or_else(String::new, |guard| {
        format!(
            "WHEN NOT ({guard}) THEN {} ",
            dialect.cast("NULL", &SqlType::Double)
        )
    });
    format!(
        "CASE {ill_typed}WHEN abs({d}) >= {two} ^ 1024 - {two} ^ 970 THEN {} \
         WHEN {d} <> 0 AND abs({d}) * {two} ^ 1075 <= 1 THEN {} ELSE {} END",
        signed(dialect, &format!("{d} < 0"), f64::INFINITY),
        signed(dialect, &format!("{d} < 0"), 0.0),
        dialect.cast(d, &SqlType::Double)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialect::PostgresDialect;

    #[test]
    fn literals_read_back_as_the_same_double() {
        let d = &PostgresDialect;
        assert_eq!(literal(d, 0.5), "'5e-1'::float8");
        assert_eq!(
            literal(d, 2f64.powi(1023)),
            "'8.98846567431158e307'::float8"
        );
        assert_eq!(literal(d, f64::MAX), "'1.7976931348623157e308'::float8");
        assert_eq!(literal(d, -0.0), "'-0e0'::float8");
        assert_eq!(literal(d, f64::NEG_INFINITY), "'-Infinity'::float8");
        for x in [
            2f64.powi(-537),
            2f64.powi(538),
            2f64.powi(-512),
            134_217_729.0,
        ] {
            let text = literal(d, x);
            let digits = text.trim_start_matches('\'').split('\'').next().unwrap();
            assert_eq!(digits.parse::<f64>().unwrap(), x);
        }
    }

    #[test]
    fn simple_operands_are_repeated_and_others_computed_once() {
        let column = Double::simple("t0.\"f\"".to_owned(), true);
        let sum = Double::computed("(t0.\"a\" + 1)".to_owned(), true);
        assert_eq!(
            bind(&[&column, &column], |o| format!("{} * {}", o[0], o[1])),
            "t0.\"f\" * t0.\"f\""
        );
        assert_eq!(
            bind(&[&sum, &column], |o| format!("{} * {}", o[0], o[1])),
            "(SELECT x * t0.\"f\" FROM (SELECT (t0.\"a\" + 1) AS x) AS d)"
        );
    }

    #[test]
    fn addition_checks_for_overflow_before_adding() {
        assert_eq!(
            add(&PostgresDialect, "a", "-", "b"),
            "CASE WHEN CASE WHEN abs(a) BETWEEN '9.9792015476736e291'::float8 AND \
             '1.7976931348623157e308'::float8 AND abs(b) BETWEEN '9.9792015476736e291'::float8 \
             AND '1.7976931348623157e308'::float8 THEN abs(a * '5e-1'::float8 - b * \
             '5e-1'::float8) >= '8.98846567431158e307'::float8 END THEN CASE WHEN a < 0 THEN \
             '-Infinity'::float8 ELSE 'Infinity'::float8 END ELSE a - b END"
        );
    }

    #[test]
    fn division_by_zero_is_signed_by_both_operands() {
        let sql = divide(&PostgresDialect, "a", "b");
        assert!(
            sql.starts_with(
                "CASE WHEN b = 0 THEN CASE WHEN a <> 0 AND a <= 'Infinity'::float8 THEN \
                 CASE WHEN (a < 0) <> (get_byte(float8send(b), 0) >= 128) THEN \
                 '-Infinity'::float8 ELSE 'Infinity'::float8 END \
                 WHEN a IS NOT NULL THEN 'NaN'::float8 END"
            ),
            "{sql}"
        );
        assert!(sql.ends_with("ELSE a / b END"), "{sql}");
    }

    #[test]
    fn decimals_round_to_infinities_and_signed_zeros_outside_the_range() {
        assert_eq!(
            from_numeric(&PostgresDialect, "d", None),
            "CASE WHEN abs(d) >= 2::numeric ^ 1024 - 2::numeric ^ 970 THEN CASE WHEN d < 0 \
             THEN '-Infinity'::float8 ELSE 'Infinity'::float8 END WHEN d <> 0 AND abs(d) * \
             2::numeric ^ 1075 <= 1 THEN CASE WHEN d < 0 THEN '-0e0'::float8 ELSE \
             '0e0'::float8 END ELSE d::float8 END"
        );
    }
}
