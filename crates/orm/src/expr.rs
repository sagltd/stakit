//! Typed predicates and operators: `eq`, `and`, `or`, comparisons, `any_of`,
//! `is_null`, plus `asc`/`desc` ordering.
//!
//! Values reach SQL only as binds (`$N`); identifiers only from [`Col`] tokens.
//! Predicates are an owned enum (no per-leaf closure box) rendered once at
//! terminal assembly.

use crate::ident::IdentError;
use crate::schema::Col;
use crate::sql::{Bind, SqlWriter};

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
    /// A value bound as `$N`.
    Value(Box<dyn Bind>),
}

impl Operand {
    pub(crate) fn write_into(self, writer: &mut SqlWriter) -> Result<(), IdentError> {
        match self {
            Self::Column { table, name } => writer.push_qualified(table, name),
            Self::Value(bind) => {
                writer.push_bind(bind);
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
                Operand::Value(Box::new(self) as Box<dyn Bind>)
            }
        }
        impl IntoExpr<Option<$ty>> for $ty {
            fn into_operand(self) -> Operand {
                Operand::Value(Box::new(self) as Box<dyn Bind>)
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
        Operand::Value(Box::new(self.to_owned()) as Box<dyn Bind>)
    }
}

impl IntoExpr<Option<String>> for &str {
    fn into_operand(self) -> Operand {
        Operand::Value(Box::new(self.to_owned()) as Box<dyn Bind>)
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
        values: Box<dyn Bind>,
    },
    /// `(left AND right)`.
    And(Box<Self>, Box<Self>),
    /// `(left OR right)`.
    Or(Box<Self>, Box<Self>),
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
            } => {
                writer.push_qualified(table, name)?;
                writer.push(" = any(");
                writer.push_bind(values);
                writer.push(")");
                Ok(())
            }
            Self::And(left, right) => combine(*left, " and ", *right, writer),
            Self::Or(left, right) => combine(*left, " or ", *right, writer),
            Self::Raw(fragment) => {
                writer.push(fragment);
                Ok(())
            }
        }
    }
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
    Ty: Clone + Send + 'static,
    Vec<Ty>: for<'q> sqlx::Encode<'q, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    Predicate::AnyOf {
        table: column.table,
        name: column.name,
        values: Box::new(values.to_vec()) as Box<dyn Bind>,
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
