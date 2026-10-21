# Guarded upserts

`Upsert::set_expr(column, expression)` accepts the existing typed `IntoExpr<Ty>`
operands, arithmetic, or new `case_when(condition, then_value, else_value)` scalar
expressions. Assignment and CASE branch types must match. Literal values remain
bound parameters. `stored(column)` explicitly selects the table-qualified stored
row; `excluded(column)` selects the incoming row (`values(column)` on MySQL).
Existing `set`, `set_add`, `set_coalesce`, and update-all APIs keep their behavior.

```rust,ignore
let expired = || lte(stored(Attempt::window_started_at), db_now_minus_seconds(300));
db.insert(AttemptNew {
    id,
    identifier,
    failed_count: Some(1),
    window_started_at: None, // column DEFAULT now(), evaluated by the database
})
.on_conflict(Attempt::identifier)
.set_expr(Attempt::failed_count,
    case_when(expired(), 1_i32, add(stored(Attempt::failed_count), 1_i32)))
.set_expr(Attempt::window_started_at,
    case_when(expired(), db_now(), stored(Attempt::window_started_at)))
.where_conflict(or(expired(), lt(stored(Attempt::failed_count), max)))
.exec().await?;
```

`where_conflict` appears after `DO UPDATE SET`, so it never filters initial
inserts or changes the conflict target. PostgreSQL evaluates it against the
conflicting row while holding its row lock. A rejected update affects zero rows;
`returning(...).all()` is empty and `.one()` returns `Error::NotFound`. Repeated
guards combine with AND. Update-all shortcuts retain the guard; explicit
`do_nothing()` and empty assignment lists discard it because no UPDATE occurs.

`db_now()` and `db_now_minus_seconds(i64)` have UTC timestamp type and use
PostgreSQL transaction time, including at exact window boundaries. Seconds are
bound as `bigint`, multiplied by a fixed one-second interval; negative seconds
move the cutoff into the future. Neither application clocks nor formatted
interval strings participate. These helpers return `Error::Unsupported` before
dispatch on SQLite, Turso, and MySQL. Portable CASE and incoming references work
on all supported dialects; guarded UPDATE works on PostgreSQL, SQLite, and Turso.
MySQL lacks the corresponding conflict UPDATE WHERE, so guarded updates fail
with `Error::Unsupported` rather than silently dropping the guard.

Full login-budget and verification-resend shapes are tested in
`crates/orm/tests/sql_building.rs` with SQL and actual ordered bind assertions,
and in `postgres_test.rs` against local embedded PostgreSQL. Behavior includes
fresh insert, active increment, locked rejection, expired reset, exact cutoff,
RETURNING, concurrent budget reservations and concurrent resend counting.

The initial behavioral RED used the legacy unconditional `set_add` builder with
a locked-budget assertion: one executed test failed (affected 1, expected 0).
This demonstrated the missing guarded capability, not an existing API regression.
New API compilation errors and sandbox setup errors are not behavioral RED.
No production databases or providers are used by these tests.
