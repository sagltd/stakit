# stakit-orm — implementation status

Tracks the design spec (`docs/superpowers/specs/2026-06-02-stakit-orm-design.md`)
against what is implemented in code. All implemented code passes `./code-check.sh`
(fmt, clippy pedantic+nursery `-D warnings`, build, nextest, doctests); `unsafe`
forbidden.

## Implemented (v1 core)

- **Schema derive** `#[derive(Table)]` (`crates/orm-derive`): table name, `&[Column]`
  metadata, typed `Col` tokens, `all()` projection, sqlx `FromRow`, compile-time
  FK type-equality witness, compile-time identifier validation (empty / NUL /
  NAMEDATALEN 63-byte), `on_delete` keyword + `set null`-nullable validation,
  canonical Rust→SQL type map with `#[column(sql_type = "...")]` escape,
  `Vec<u8>`→`bytea` special-case, `Option<T>`→nullable.
- **Typed query builder** (`query.rs`): `select` + `from`/`inner_join`/`left_join`/
  `filter`/`order_by`/`limit`/`offset`; terminals `all`/`one`/`exact_one`; all
  values bound `$N`, `LIMIT`/`OFFSET` bound.
- **Projections** (`projection.rs`): `All<T>` whole-row, `All<T, Nullable>` →
  `Option<T>` (PK-null decode), `Col` scalar, `Count`, leaf tuples (positional
  decode); return type inferred from the projection.
- **Operators** (`expr.rs`): `eq/ne/gt/lt/gte/lte/like/and/or/is_null/asc/desc` and
  `any_of` (`= ANY($1)` array bind, no `IN (...)` thrash); `IntoExpr<Ty>` curated
  impls (no reflexive blanket; `&str`→`String`); `like` works on `Option<String>`.
- **Insert** (`insert.rs`): `#[derive(Table)]` generates a `…New` companion
  (defaulted columns are `Option`); `db.insert(new)` / `db.insert_many(rows)` /
  `Tx::insert*`, with `.returning(proj).one()/.all()`. All-`None` defaulted columns
  are **omitted** so the DB default fires; many rows insert in one statement.
- **Mutations** (`mutation.rs`): `update().set().filter().exec()`, `delete().filter().exec()`
  (values bound, `Operand` enum — no per-set closure box).
- **NanoID** (`nanoid.rs`): secure (`getrandom` CSPRNG), collision-resistant
  (uniform sampling, ~126-bit default), `nanoid()`/`nanoid_sized()`/`nanoid_custom()`;
  tested (50k no-collision, custom-alphabet uniformity) + divan bench.
- **Terminals**: `all`/`one`/`one_or_err`/`exact_one`/`count`/`exists` (count/exists
  strip LIMIT/OFFSET/ORDER so they reflect totals).
- **Aggregates + grouping** (`projection.rs`/`query.rs`): `count()`, `count_col`,
  `min`/`max` (→ `Option<Ty>`), `sum`/`avg` (caller-chosen decode type),
  `.group_by()`, `.having()`; `right_join`.
- **`#[derive(Row)]`** (`orm-derive`): named projection — each field `#[from(<expr>)]`
  (column, aggregate, or `sql_expr`); `T::project()` selects + decodes into `T`.
- **`sql_expr::<T>("…")`** (`projection.rs`): raw SQL expression in the select list
  (the `sql!` capability), composes in tuples / `#[derive(Row)]` fields.
- **Whole-row join tuples** (`projection.rs`): `(T::all(), U::all().nullable())` →
  `(T, Option<U>)`, decoded **positionally** (`Table::from_row_at`) so duplicate
  column names across joined tables are unambiguous. Verified against real pg.
- **`.stream()`** (`query.rs`): lazy row stream (pool-only), verified against real pg.
- **CLI `up`/`down`/`status`** (`orm-cli`): apply / revert-latest / report via the
  sqlx `Migrator` against `--url`/`$DATABASE_URL`.
- **Upsert** (`insert.rs`): `.on_conflict_do_nothing(col)` /
  `.on_conflict_do_update(col)` (sets other inserted columns to `excluded`).
- **`DbConfig`** (`db.rs`): pool sizing + timeouts via `Db::connect_with`; `Debug`
  redacts the URL (credentials).
- **`tracing` observability** (`exec.rs`): every query logs SQL at `trace` (never
  bind values) and elapsed-ms + row count at `debug`, under `stakit_orm::query`.
- **Transactions** (`db.rs`/`exec.rs`): `db.transaction(|tx| async { … })` —
  commit on `Ok`, rollback on `Err`; `Tx` hands out the same select/update/
  delete/raw builders via an `Exec` abstraction over pool or transaction.
- **Raw escape hatch** (`raw.rs`): `db.raw(sql).bind(..).all::<T>()/one::<T>()/exec()`.
- **Migration CLI** (`crates/orm-cli`, `stakit-orm` binary): `gen <name>` —
  syn-parses the schema, diffs against `migrations/.snapshot.json`, and writes a
  reversible sqlx migration (`.up.sql`/`.down.sql`). Handles create table, add/
  drop column, alter type, and **rename** — prompting "replace (rename) vs add
  new field" when a change is ambiguous. Pure diff core is unit-tested.
- **Integration test** (`tests/postgres_test.rs`): real embedded Postgres via
  `postgresql_embedded` (no Docker) + sqlx migrations incl. an **ALTER TABLE**
  (`migrations/0002_add_user_active.sql`); exercises select/update/delete,
  unique-violation error mapping, and transaction commit + rollback end to end.
- **Errors** (`error.rs`): SQLSTATE→typed (`Unique`/`ForeignKey`/`NotNull`/`Check`/
  `NotFound`/`TooManyRows`), `Encode`/`Decode` split, reads only
  `code`/`constraint`/`column` (no value leak).
- **Identifier safety** (`ident.rs`): quote + embedded-`"` doubling + NUL reject +
  63-byte limit, at compile time (derive) and render time.
- **Db handle** (`db.rs`): `new`/`connect`/`pool`/`select`/`update`/`delete`.
- **Benchmarks** (`benches/build.rs`, divan): SQL-build microbenchmarks.
- **Tests**: 23 unit/integration SQL-string tests (no DB) + module unit tests.

## Not yet implemented (tracked, with rationale)

- **`copy_into`** bulk path + UNNEST (spec §10) — `insert`/`insert_many`/`returning`/
  upsert are done via multi-row `VALUES`; UNNEST/COPY are perf follow-ups.
- **Relational API 2** `db.query::<T>().with()`, `Loaded<T>`, `#[has_many]`/
  `#[belongs_to]` codegen (spec §8). Attributes are reserved; `Rel<T>` is defaulted.
- **`row!`** inline macro (spec §7) — intentionally skipped: `#[derive(Row)]` gives
  the same named typed projection; `row!` is pure sugar. (`sql!` is covered by
  `sql_expr`.)
- **Savepoints** (nested transactions via `tx.transaction(..)`) (spec §11).
- **aggregate-`HAVING`** (e.g. `count(..) > n`) — `having` currently compares
  grouped columns only.

## Performance

After reworking predicates to an owned enum (no `Box<dyn FnOnce>`), binds to an
inline `SmallVec`, and pre-sizing the SQL `String`, simple `select` build is
**~212 ns median** (was ~380 ns); complex ~620 ns. The remaining floor is the SQL
`String` + one boxed generic bind value + `PgArguments` — inherent to type-erased
generic binds. Measured by `benches/build.rs` (divan).
