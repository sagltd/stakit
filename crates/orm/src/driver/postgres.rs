//! The Postgres backend: [`Row`] for `PgRow`, the [`PostgresDriver`], its
//! transaction handle, and `Value` → `PgArguments` binding.

use super::sqlx_backend::{
    add_null_scalar, add_scalar, geo_from_text, into_decode, read_scalar, sqlx_driver, text_cell,
    vector_from_text,
};
use crate::dialect::PostgresDialect;
use crate::error::{Error, Result};
use crate::value::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc, Uuid, Value, ValueKind};
use sqlx::Row as _;
use sqlx::postgres::{PgArguments, PgRow};
use sqlx::{Arguments, PgPool, Postgres};

fn try_value(row: &PgRow, index: usize, kind: ValueKind) -> Result<Value> {
    match kind {
        // pgvector / PostGIS columns must be selected as text (e.g. `embedding::text`,
        // `ST_AsText(location)`) for these to decode.
        ValueKind::Vector => vector_from_text(text_cell(row, index)?),
        ValueKind::Geo => Ok(geo_from_text(text_cell(row, index)?)),
        ValueKind::Array(_) => read_pg_array(row, index, kind),
        _ => read_scalar!(row, index, kind),
    }
}

sqlx_driver! {
    row: PgRow;
    /// The Postgres [`Driver`](super::Driver), wrapping an sqlx pool.
    pub struct PostgresDriver(pool: PgPool);
    new_doc: "Wrap an existing sqlx pool.";
    pool_doc: "Borrow the underlying sqlx pool (the unaudited raw escape hatch, used by\nthe migrator).";
    /// A Postgres transaction handle.
    struct PgTxConn(database: Postgres);
    dialect: PostgresDialect;
    arguments: PgArguments;
}

#[allow(clippy::needless_pass_by_value)]
fn bind_scalar(args: &mut PgArguments, value: Value) -> Result<()> {
    let result = match value {
        // Bound as the text literal `[..]`; the SQL writer adds the `::vector` cast.
        Value::Vector(x) => args.add(crate::vector::to_literal(&x)),
        // Bound as bare WKT text; the SQL writer adds the `::geometry` cast (and
        // wraps it in `ST_SetSRID(.., srid)` when a SRID is attached).
        Value::Geo { wkt, .. } => args.add(wkt),
        Value::Array(kind, values) => return bind_array(args, kind, values),
        scalar => add_scalar!(args, scalar),
    };
    result.map_err(Error::Encode)
}

fn bind_null(args: &mut PgArguments, kind: ValueKind) -> Result<()> {
    let result = match kind {
        // Vector and geometry bind as text literals, so their nulls are text nulls too.
        ValueKind::Vector | ValueKind::Geo => args.add(None::<String>),
        ValueKind::Array(elem) => return bind_null_array(args, *elem),
        scalar => add_null_scalar!(args, scalar),
    };
    result.map_err(Error::Encode)
}

/// Bind a typed `NULL` array for a nullable native-array column.
fn bind_null_array(args: &mut PgArguments, elem: ValueKind) -> Result<()> {
    let result = match elem {
        ValueKind::I16 => args.add(None::<Vec<i16>>),
        ValueKind::I32 => args.add(None::<Vec<i32>>),
        ValueKind::I64 => args.add(None::<Vec<i64>>),
        ValueKind::F32 => args.add(None::<Vec<f32>>),
        ValueKind::F64 => args.add(None::<Vec<f64>>),
        ValueKind::Bool => args.add(None::<Vec<bool>>),
        ValueKind::Uuid => args.add(None::<Vec<Uuid>>),
        ValueKind::Timestamptz => args.add(None::<Vec<DateTime<Utc>>>),
        ValueKind::NaiveDateTime => args.add(None::<Vec<NaiveDateTime>>),
        ValueKind::Date => args.add(None::<Vec<NaiveDate>>),
        ValueKind::NaiveTime => args.add(None::<Vec<NaiveTime>>),
        ValueKind::Bytes => args.add(None::<Vec<Vec<u8>>>),
        _ => args.add(None::<Vec<String>>),
    };
    result.map_err(Error::Encode)
}

/// Read a native Postgres array column (`int[]`, `text[]`, …) into a
/// [`Value::Array`]. `kind` is the full `ValueKind::Array(elem)` (used for `NULL`).
fn read_pg_array(row: &PgRow, index: usize, kind: ValueKind) -> Result<Value> {
    let ValueKind::Array(elem_ref) = kind else {
        return Err(Error::Decode("read_pg_array: not an array kind".into()));
    };
    let elem = *elem_ref;
    macro_rules! arr {
        ($ty:ty, $wrap:expr) => {{
            let cell: Option<Vec<$ty>> = row.try_get(index).map_err(into_decode)?;
            match cell {
                None => Ok(Value::Null(kind)),
                Some(items) => Ok(Value::Array(elem, items.into_iter().map($wrap).collect())),
            }
        }};
    }
    match elem {
        ValueKind::I16 => arr!(i16, Value::I16),
        ValueKind::I32 => arr!(i32, Value::I32),
        ValueKind::I64 => arr!(i64, Value::I64),
        ValueKind::F32 => arr!(f32, Value::F32),
        ValueKind::F64 => arr!(f64, Value::F64),
        ValueKind::Bool => arr!(bool, Value::Bool),
        ValueKind::Text => arr!(String, Value::Text),
        ValueKind::Uuid => arr!(Uuid, Value::Uuid),
        ValueKind::Timestamptz => arr!(DateTime<Utc>, Value::Timestamptz),
        ValueKind::NaiveDateTime => arr!(NaiveDateTime, Value::NaiveDateTime),
        ValueKind::Date => arr!(NaiveDate, Value::Date),
        ValueKind::NaiveTime => arr!(NaiveTime, Value::NaiveTime),
        ValueKind::Bytes => arr!(Vec<u8>, Value::Bytes),
        other => Err(Error::Decode(
            format!("unsupported native array element {other:?} (use Json<Vec<_>>)").into(),
        )),
    }
}

/// Collect homogeneous scalar `values` of `kind` into a typed `Vec` and bind it
/// as a Postgres array (the `= ANY($1)` parameter).
fn bind_array(args: &mut PgArguments, kind: ValueKind, values: Vec<Value>) -> Result<()> {
    macro_rules! collect_add {
        ($variant:ident) => {{
            // Error on any element whose variant disagrees with `kind` rather than
            // silently dropping it (which would bind a short array). `any_of` always
            // builds homogeneous arrays, so this only fires on a constructed mismatch.
            let typed: Vec<_> = values
                .into_iter()
                .map(|value| match value {
                    Value::$variant(inner) => Ok(inner),
                    other => Err(Error::Encode(
                        format!(
                            "heterogeneous array bind: expected {} element, got {other:?}",
                            stringify!($variant)
                        )
                        .into(),
                    )),
                })
                .collect::<Result<_>>()?;
            args.add(typed)
        }};
    }
    let result = match kind {
        ValueKind::I16 => collect_add!(I16),
        ValueKind::I32 => collect_add!(I32),
        ValueKind::I64 => collect_add!(I64),
        ValueKind::F32 => collect_add!(F32),
        ValueKind::F64 => collect_add!(F64),
        ValueKind::Bool => collect_add!(Bool),
        ValueKind::Text => collect_add!(Text),
        ValueKind::Bytes => collect_add!(Bytes),
        ValueKind::Uuid => collect_add!(Uuid),
        ValueKind::Timestamptz => collect_add!(Timestamptz),
        ValueKind::NaiveDateTime => collect_add!(NaiveDateTime),
        ValueKind::Date => collect_add!(Date),
        ValueKind::NaiveTime => collect_add!(NaiveTime),
        ValueKind::Json => {
            return Err(Error::Encode("json array binds are not supported".into()));
        }
        ValueKind::Vector => {
            return Err(Error::Encode("vector array binds are not supported".into()));
        }
        ValueKind::Geo => {
            return Err(Error::Encode(
                "geometry array binds are not supported".into(),
            ));
        }
        ValueKind::Array(_) => {
            return Err(Error::Encode(
                "nested arrays are not supported (use Json<Vec<Vec<_>>>)".into(),
            ));
        }
    };
    result.map_err(Error::Encode)
}
