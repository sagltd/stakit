//! Typed predicates and operators: `eq`, `and`, `or`, comparisons, `any_of`,
//! `is_null`, plus `asc`/`desc` ordering.
//!
//! Values reach SQL only as binds (`$N`); identifiers only from [`Col`] tokens.
//! Predicates are an owned enum (no per-leaf closure box) rendered once at
//! terminal assembly.

use crate::ident::IdentError;
use crate::schema::Col;
use crate::sql::SqlWriter;
use crate::value::{ToValue, Value};

/// A column usable on the left of a comparison.
pub trait ColExpr {
    /// The column's Rust type, which the right-hand side must match.
    type Ty;
    /// Owning table name.
    fn table(&self) -> &'static str;
    /// Column name.
    fn name(&self) -> &'static str;
}

impl<T, Ty> ColExpr for Col<T, Ty> {
    type Ty = Ty;
    fn table(&self) -> &'static str {
        self.table
    }
    fn name(&self) -> &'static str {
        self.name
    }
}

/// A comparison operand: another column, or a bound value.
pub enum Operand {
    /// A table-qualified column reference.
    Column {
        /// Table name.
        table: &'static str,
        /// Column name.
        name: &'static str,
    },
    /// A backend-neutral value bound as a placeholder.
    Value(Value),
}

impl Operand {
    pub(crate) fn write_into(self, writer: &mut SqlWriter) -> Result<(), IdentError> {
        match self {
            Self::Column { table, name } => writer.push_qualified(table, name),
            Self::Value(value) => {
                writer.push_bind(value);
                Ok(())
            }
        }
    }
}

/// The right-hand side of a comparison against a column of type `Ty`.
///
/// Implemented for [`Col`] (column = column) and for a curated set of value
/// types (column = value). There is deliberately no reflexive blanket impl, so
/// type mismatches such as `eq(User::id /* Uuid */, 5_i32)` fail to compile.
pub trait IntoExpr<Ty> {
    /// Convert into a comparison operand.
    fn into_operand(self) -> Operand;
}

impl<T, Ty> IntoExpr<Ty> for Col<T, Ty> {
    fn into_operand(self) -> Operand {
        Operand::Column {
            table: self.table,
            name: self.name,
        }
    }
}

/// Implement `IntoExpr<$ty>` for an owned value type (binds itself).
macro_rules! value_into_expr {
    ($($ty:ty),* $(,)?) => {$(
        impl IntoExpr<$ty> for $ty {
            fn into_operand(self) -> Operand {
                Operand::Value(self.to_value())
            }
        }
        impl IntoExpr<Option<$ty>> for $ty {
            fn into_operand(self) -> Operand {
                Operand::Value(self.to_value())
            }
        }
    )*};
}

value_into_expr!(
    i16,
    i32,
    i64,
    f32,
    f64,
    bool,
    String,
    Vec<u8>,
    sqlx::types::Uuid,
    sqlx::types::chrono::DateTime<sqlx::types::chrono::Utc>,
    sqlx::types::chrono::NaiveDate,
);

/// Ergonomic string literals against `text` columns.
impl IntoExpr<String> for &str {
    fn into_operand(self) -> Operand {
        Operand::Value(self.to_value())
    }
}

impl IntoExpr<Option<String>> for &str {
    fn into_operand(self) -> Operand {
        Operand::Value(self.to_value())
    }
}

/// A boolean SQL predicate (owned tree, rendered once at terminal assembly).
pub enum Predicate {
    /// `"table"."name" <op> <rhs>`.
    Binary {
        /// Left column's table.
        table: &'static str,
        /// Left column's name.
        name: &'static str,
        /// Operator (e.g. ` = `), including surrounding spaces.
        op: &'static str,
        /// Right operand.
        rhs: Operand,
    },
    /// `"table"."name" IS NULL`.
    IsNull {
        /// Column table.
        table: &'static str,
        /// Column name.
        name: &'static str,
    },
    /// `"table"."name" = ANY($1)`.
    AnyOf {
        /// Column table.
        table: &'static str,
        /// Column name.
        name: &'static str,
        /// The bound array value.
        values: Value,
    },
    /// `(left AND right)`.
    And(Box<Self>, Box<Self>),
    /// `(left OR right)`.
    Or(Box<Self>, Box<Self>),
    /// `NOT (inner)`.
    Not(Box<Self>),
    /// A raw SQL fragment (developer-trusted `&'static str`) — e.g. an aggregate
    /// `HAVING` like `count(*) > 5` that the typed builder can't model.
    Raw(&'static str),
}

/// A raw boolean SQL fragment (escape hatch; e.g. aggregate `HAVING`).
#[must_use]
pub const fn raw_pred(fragment: &'static str) -> Predicate {
    Predicate::Raw(fragment)
}

impl Predicate {
    /// `"table"."name" = <value>` from an already-converted [`Value`] (used by
    /// `Db::get` to filter on a table's primary-key column).
    #[must_use]
    pub const fn eq_value(table: &'static str, name: &'static str, value: Value) -> Self {
        Self::Binary {
            table,
            name,
            op: " = ",
            rhs: Operand::Value(value),
        }
    }

    /// Render this predicate into the writer.
    ///
    /// # Errors
    /// Returns [`IdentError`] if a column identifier is invalid.
    pub fn write(self, writer: &mut SqlWriter) -> Result<(), IdentError> {
        match self {
            Self::Binary {
                table,
                name,
                op,
                rhs,
            } => {
                writer.push_qualified(table, name)?;
                writer.push(op);
                rhs.write_into(writer)
            }
            Self::IsNull { table, name } => {
                writer.push_qualified(table, name)?;
                writer.push(" is null");
                Ok(())
            }
            Self::AnyOf {
                table,
                name,
                values,
            } => write_any_of(writer, table, name, values),
            Self::And(left, right) => combine(*left, " and ", *right, writer),
            Self::Or(left, right) => combine(*left, " or ", *right, writer),
            Self::Not(inner) => {
                writer.push("not (");
                inner.write(writer)?;
                writer.push(")");
                Ok(())
            }
            Self::Raw(fragment) => {
                writer.push(fragment);
                Ok(())
            }
        }
    }
}

/// Render list membership. On backends with array binds (Postgres) this is one
/// `= ANY($1)` array parameter; otherwise it expands to `IN (?, ?, …)` with one
/// bind per element. An empty list renders as the always-false `1 = 0`.
fn write_any_of(
    writer: &mut SqlWriter,
    table: &'static str,
    name: &'static str,
    values: crate::value::Value,
) -> Result<(), IdentError> {
    if writer.supports_any_array() {
        writer.push_qualified(table, name)?;
        writer.push(" = any(");
        writer.push_bind(values);
        writer.push(")");
        return Ok(());
    }
    let crate::value::Value::Array(_, items) = values else {
        // `any_of` always builds an Array; treat anything else as a single value.
        writer.push_qualified(table, name)?;
        writer.push(" in (");
        writer.push_bind(values);
        writer.push(")");
        return Ok(());
    };
    if items.is_empty() {
        writer.push("1 = 0");
        return Ok(());
    }
    writer.push_qualified(table, name)?;
    writer.push(" in (");
    for (index, item) in items.into_iter().enumerate() {
        if index > 0 {
            writer.push(", ");
        }
        writer.push_bind(item);
    }
    writer.push(")");
    Ok(())
}

fn combine(
    left: Predicate,
    op: &'static str,
    right: Predicate,
    writer: &mut SqlWriter,
) -> Result<(), IdentError> {
    writer.push("(");
    left.write(writer)?;
    writer.push(op);
    right.write(writer)?;
    writer.push(")");
    Ok(())
}

fn binary<L, R>(left: &L, op: &'static str, right: R) -> Predicate
where
    L: ColExpr + Copy,
    R: IntoExpr<L::Ty>,
{
    Predicate::Binary {
        table: left.table(),
        name: left.name(),
        op,
        rhs: right.into_operand(),
    }
}

/// `left = right`.
pub fn eq<L, R>(left: L, right: R) -> Predicate
where
    L: ColExpr + Copy,
    R: IntoExpr<L::Ty>,
{
    binary(&left, " = ", right)
}

/// `left <> right`.
pub fn ne<L, R>(left: L, right: R) -> Predicate
where
    L: ColExpr + Copy,
    R: IntoExpr<L::Ty>,
{
    binary(&left, " <> ", right)
}

/// `left > right`.
pub fn gt<L, R>(left: L, right: R) -> Predicate
where
    L: ColExpr + Copy,
    R: IntoExpr<L::Ty>,
{
    binary(&left, " > ", right)
}

/// `left < right`.
pub fn lt<L, R>(left: L, right: R) -> Predicate
where
    L: ColExpr + Copy,
    R: IntoExpr<L::Ty>,
{
    binary(&left, " < ", right)
}

/// `left >= right`.
pub fn gte<L, R>(left: L, right: R) -> Predicate
where
    L: ColExpr + Copy,
    R: IntoExpr<L::Ty>,
{
    binary(&left, " >= ", right)
}

/// `left <= right`.
pub fn lte<L, R>(left: L, right: R) -> Predicate
where
    L: ColExpr + Copy,
    R: IntoExpr<L::Ty>,
{
    binary(&left, " <= ", right)
}

/// `left LIKE pattern` for a `text` column (`String` or `Option<String>`).
pub fn like<L>(left: L, pattern: &str) -> Predicate
where
    L: ColExpr + Copy,
    String: IntoExpr<L::Ty>,
{
    binary(&left, " like ", pattern.to_owned())
}

/// `column = ANY($1)` — a single array bind for any list length (stable
/// statement, no per-element params; see the spec's statement-cache rules).
pub fn any_of<T, Ty>(column: Col<T, Ty>, values: &[Ty]) -> Predicate
where
    Ty: ToValue + crate::value::FromValue + Clone,
{
    Predicate::AnyOf {
        table: column.table,
        name: column.name,
        values: values.to_vec().to_value(),
    }
}

/// `column IS NULL`.
#[must_use]
pub const fn is_null<T, Ty>(column: Col<T, Ty>) -> Predicate {
    Predicate::IsNull {
        table: column.table,
        name: column.name,
    }
}

/// `(left AND right)`.
#[must_use]
pub fn and(left: Predicate, right: Predicate) -> Predicate {
    Predicate::And(Box::new(left), Box::new(right))
}

/// `(left OR right)`.
#[must_use]
pub fn or(left: Predicate, right: Predicate) -> Predicate {
    Predicate::Or(Box::new(left), Box::new(right))
}

/// `NOT (inner)` — negate any predicate (including compound `and`/`or` trees).
#[must_use]
pub fn not(inner: Predicate) -> Predicate {
    Predicate::Not(Box::new(inner))
}

/// Sort direction.
#[derive(Debug, Clone, Copy)]
pub enum Direction {
    /// Ascending.
    Asc,
    /// Descending.
    Desc,
}

/// An `ORDER BY` term: a column and a direction.
#[derive(Debug, Clone, Copy)]
pub struct Order {
    table: &'static str,
    column: &'static str,
    direction: Direction,
}

impl Order {
    /// Render this term into the writer.
    ///
    /// # Errors
    /// Returns [`IdentError`] if a column identifier is invalid.
    pub fn write(&self, writer: &mut SqlWriter) -> Result<(), IdentError> {
        writer.push_qualified(self.table, self.column)?;
        writer.push(match self.direction {
            Direction::Asc => " asc",
            Direction::Desc => " desc",
        });
        Ok(())
    }
}

/// Ascending order on a column.
#[must_use]
pub const fn asc<T, Ty>(column: Col<T, Ty>) -> Order {
    Order {
        table: column.table,
        column: column.name,
        direction: Direction::Asc,
    }
}

/// Descending order on a column.
#[must_use]
pub const fn desc<T, Ty>(column: Col<T, Ty>) -> Order {
    Order {
        table: column.table,
        column: column.name,
        direction: Direction::Desc,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Predicate, and, any_of, asc, desc, eq, gt, gte, is_null, like, lt, lte, ne, or, raw_pred,
    };
    use crate::schema::Col;
    use crate::sql::SqlWriter;

    /// Render a predicate to its SQL string (binds discarded).
    fn render(predicate: Predicate) -> String {
        let mut writer = SqlWriter::new();
        predicate.write(&mut writer).unwrap();
        writer.sql().to_owned()
    }

    const ID: Col<(), i32> = Col::new("t", "id");
    const NAME: Col<(), String> = Col::new("t", "name");
    const BIO: Col<(), Option<String>> = Col::new("t", "bio");

    #[test]
    fn comparison_operators_render_correct_symbols() {
        assert_eq!(render(eq(ID, 1_i32)), r#""t"."id" = $1"#);
        assert_eq!(render(ne(ID, 1_i32)), r#""t"."id" <> $1"#);
        assert_eq!(render(gt(ID, 1_i32)), r#""t"."id" > $1"#);
        assert_eq!(render(lt(ID, 1_i32)), r#""t"."id" < $1"#);
        assert_eq!(render(gte(ID, 1_i32)), r#""t"."id" >= $1"#);
        assert_eq!(render(lte(ID, 1_i32)), r#""t"."id" <= $1"#);
    }

    #[test]
    fn eq_column_to_column_emits_no_bind() {
        let mut writer = SqlWriter::new();
        let other: Col<(), i32> = Col::new("u", "id");
        eq(ID, other).write(&mut writer).unwrap();
        assert_eq!(writer.sql(), r#""t"."id" = "u"."id""#);
        assert_eq!(writer.bind_count(), 0);
    }

    #[test]
    fn like_on_nullable_renders_like() {
        assert_eq!(render(like(BIO, "%x%")), r#""t"."bio" like $1"#);
    }

    #[test]
    fn like_on_non_nullable_renders_like() {
        assert_eq!(render(like(NAME, "%x%")), r#""t"."name" like $1"#);
    }

    #[test]
    fn is_null_renders_is_null() {
        assert_eq!(render(is_null(BIO)), r#""t"."bio" is null"#);
    }

    #[test]
    fn any_of_multi_is_single_array_bind() {
        let mut writer = SqlWriter::new();
        any_of(ID, &[1_i32, 2, 3]).write(&mut writer).unwrap();
        assert_eq!(writer.sql(), r#""t"."id" = any($1)"#);
        assert_eq!(writer.bind_count(), 1);
    }

    #[test]
    fn any_of_empty_is_single_array_bind() {
        let mut writer = SqlWriter::new();
        let none: [i32; 0] = [];
        any_of(ID, &none).write(&mut writer).unwrap();
        assert_eq!(writer.sql(), r#""t"."id" = any($1)"#);
        assert_eq!(writer.bind_count(), 1);
    }

    #[test]
    fn and_or_nesting_parenthesizes() {
        let predicate = and(
            or(eq(ID, 1_i32), eq(ID, 2_i32)),
            and(eq(ID, 3_i32), eq(ID, 4_i32)),
        );
        assert_eq!(
            render(predicate),
            r#"(("t"."id" = $1 or "t"."id" = $2) and ("t"."id" = $3 and "t"."id" = $4))"#
        );
    }

    #[test]
    fn bind_numbering_follows_predicate_order() {
        let mut writer = SqlWriter::new();
        and(eq(ID, 10_i32), eq(NAME, "x"))
            .write(&mut writer)
            .unwrap();
        assert_eq!(writer.sql(), r#"("t"."id" = $1 and "t"."name" = $2)"#);
        assert_eq!(writer.bind_count(), 2);
    }

    #[test]
    fn raw_pred_renders_verbatim() {
        assert_eq!(render(raw_pred("count(*) > 5")), "count(*) > 5");
    }

    #[test]
    fn order_terms_render_direction() {
        let mut writer = SqlWriter::new();
        asc(ID).write(&mut writer).unwrap();
        assert_eq!(writer.sql(), r#""t"."id" asc"#);

        let mut writer = SqlWriter::new();
        desc(NAME).write(&mut writer).unwrap();
        assert_eq!(writer.sql(), r#""t"."name" desc"#);
    }
}
