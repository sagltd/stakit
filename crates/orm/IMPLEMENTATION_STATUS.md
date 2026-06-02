# stakit-orm — implementation status

Tracks the design spec (`docs/superpowers/specs/2026-06-02-stakit-orm-design.md`)
against what is implemented in code. All implemented code passes `./code-check.sh`
(fmt, clippy pedantic+nursery `-D warnings`, build, nextest, doctests); `unsafe`
forbidden.

## Implemented (v1 core)

- **Schema derive** `#[derive(Table)]` (`crates/orm-derive`): table name, `&[Column]`
  metadata, typed `Col` tokens, `all()` projection, neutral `from_row_at`
  (`&dyn Row` + `FromValue`, positional), compile-time
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

## Multi-backend refactor (in progress — goal: postgres + sqlite + mysql + turso)

The crate is being made backend-neutral. Phases (build stays green each step):

1. **Dialect seam** (`dialect.rs`) — DONE. `Dialect::{Postgres,Sqlite,MySql,Turso}`
   selects bind placeholder syntax (`$N` vs `?N` vs `?`) + `ANY`-vs-`IN`; wired into
   `SqlWriter`. Per-dialect files + `_test.rs`.
2. **Backend-neutral value/row** — DONE. Binds flow as an owned `Value` enum
   (`value.rs`) via `ToValue`; decode goes through the `Row` accessor trait
   (`driver.rs`) + `FromValue` (`driver::decode_cell`). The derive emits
   `from_row_at` over `&dyn Row`; the core no longer names `PgRow`/`Encode`/`Decode`.
   `Value`/`ToValue`/`FromValue` are the public extension point for custom types
   (pgvector etc.).
3. **`Driver` trait** — DONE (all four backends). `driver.rs` defines `Driver`
   (`fetch`/`execute`/`stream`/`begin`/`dialect`) + `TxConn` + `RowSink`; `Db` holds
   `Arc<dyn Driver>`, `Exec` is `Pool(Arc<dyn Driver>)`/`Tx(dialect, SharedTx)`.
   Each backend is self-contained: `driver/postgres.rs`, `driver/sqlite.rs`,
   `driver/mysql.rs` (all sqlx), and `driver/turso.rs` (libSQL — **not** sqlx, the
   proof the abstraction is real). `Db::from_driver(Arc<dyn Driver>)` is the open
   constructor for custom backends.
   **Backends are opt-in cargo features** (`postgres` [default], `sqlite`, `mysql`,
   `turso`): a consumer compiles only the driver(s) they enable — `libsql` and
   `sqlx-mysql`/`sqlx-sqlite` are not pulled in unless requested.
   **Collect path is zero-per-row-alloc**: drivers decode each row inline through a
   borrowed `&dyn Row` + `RowSink` (no `Box<dyn Row>` per row). Only the lazy
   `stream()` path boxes rows (`BoxRow`), since it must yield owned items.
4. **Dialect-correct SQL** — DONE. The builder renders with the live driver's
   dialect: `$N`/`?N`/`?` placeholders, `"`-vs-`` ` ``-quoted identifiers (MySQL),
   and `any_of` → one array bind on Postgres but `IN (?, …)` (empty → `1 = 0`) on
   SQLite/MySQL/Turso. (Per-backend migration type-map is still Postgres-only; CLI
   is Postgres-targeted.)
5. **E2E per backend — DONE and verified LIVE on all four.** Embedded Postgres,
   in-memory SQLite, in-memory Turso/libSQL, and **MySQL run live against a real
   MariaDB** (`tests/{postgres,sqlite,turso,mysql}_test.rs`) — all 121 tests pass with
   the *same* builder. MySQL has no in-process mode, so its suite is gated on
   `MYSQL_URL`; it was verified here by `brew install mariadb`, initializing a throwaway
   datadir, and running with `MYSQL_URL=mysql://root@127.0.0.1:3310/stakit_test`. The
   MySQL tests use disjoint tables so they pass under nextest's default parallelism.

## Extensions / custom column types (pgvector, PostGIS, sqlite-vec)

**Mechanism: DONE and e2e-verified.** Any Rust type that implements `ToValue` +
`FromValue` is usable as a column type — `#[derive(Table)]` decodes/binds it through
`from_row_at`/`boxed_bind`. A custom `Tags(Vec<String>)` type round-trips through a
real SQLite column in `tests/sqlite_test.rs::custom_column_type_round_trips`. This is
the single extension point: map the custom type to an existing `Value` variant
(`Text`/`Bytes`/`I64`/`F64`/array…).

**pgvector / PostGIS / sqlite-vec specifically: usable via the mechanism, but NOT
first-class and NOT e2e-verified** (the embedded Postgres / bundled SQLite in the test
env don't ship those extensions, so a live test can't `create extension vector`).
Concretely:
- *Reading* works cleanly: a `vector`/`geometry` column has a text output, so a custom
  type with `FromValue` parsing `Value::Text` decodes it. (pgvector emits `[1,2,3]`;
  PostGIS can emit WKT.)
- *Writing* needs an explicit cast — the typed `insert` builder binds a `$N` text/blob
  param and does **not** add `::vector`/`::geometry`. Use `db.raw("insert … values
  ($1::vector)")` for the cast, or store the canonical text form in a column the
  extension implicitly accepts.
- *Operators* (`<->`/`<=>` KNN, `ST_DWithin`, …) are not modeled by the typed builder;
  use `sql_expr::<T>("…")` in projections and `raw_pred("…")` / `db.raw(…)` in filters.
- Bottom line: custom scalar types are first-class and verified; native vector/geo
  binary protocol + operators are reachable only through the raw/`sql_expr` escape
  hatches today. A `Value::Custom` variant + per-column bind-cast would make them
  first-class — tracked as future work, untestable here without the DB extensions.

## Review loop — round 1 (4 sub-agents: correctness/DX, safety, perf, tests)

Findings fixed:
- **Typed constraint errors now work on every backend.** `error.rs` classifies via
  sqlx's backend-neutral `ErrorKind` (was Postgres-SQLSTATE-only, so `is_unique()`
  etc. silently never fired on SQLite/MySQL). Turso classifies from the `SQLite`
  extended result code in `driver/turso.rs`.
- **Concrete, feature-gated backend errors (not boxed).** `Error::Turso(libsql::Error)`
  under the `turso` feature; sqlx backends keep the concrete `Error::Database(sqlx::Error)`.
  Turso execution errors were previously mislabeled `Error::Decode`.
- **Turso integer truncation fixed (was data corruption).** `i64`→`i16`/`i32` now uses
  a checked `narrow()` (errors on out-of-range) instead of a silent `as` wrap.
- **`MySQL` `RETURNING` guarded.** `Dialect::supports_returning()` (false for MySQL);
  `insert(...).returning(...).one()/.all()` returns `Error::Unsupported("RETURNING")`
  up-front instead of emitting invalid SQL.
- **Perf:** `SqlWriter` caches the dialect's flags (placeholder/quote/numbered/any-array)
  at construction — no vtable dispatch per bind/identifier; `insert` builder uses
  `SmallVec` instead of per-call `Vec` allocations.
- **DX:** added `Db::find::<T>()` / `Tx::find::<T>()` — `SELECT * FROM T` with the
  output inferred as `T` (no `T::all()`/`.from()`/type annotation).
- **Tests:** added real-DB coverage for joins (inner/left/nullable), `group_by` +
  `sum`/`avg`/`min`/`count_col`, `#[derive(Row)]` grouped projection, streaming,
  `ne`/`gt`/`and`/`or`/`like`/`is_null`/`limit`/`offset`, `on_conflict`, and typed
  unique-violation mapping — on `SQLite`; plus joins/grouping/streaming on Turso.
  Total: 113 tests green (all-features), 0 clippy issues.

## Review loop — round 2 (adversarial verification of round-1 fixes + DX)

- Added `not(pred)` (negate any predicate tree), `Db::get::<T>(pk)` / `Tx::get` (fetch
  by primary key), and an offline `MySQL` SQL-rendering test (backtick idents + bare
  `?` placeholders) — live `MySQL` e2e still needs `MYSQL_URL` (no in-process MySQL).
- An adversarial sub-agent re-verified the round-1 fixes against the real sqlx 0.9 /
  libsql 0.9.30 sources: cross-backend `ErrorKind` classification, Turso extended
  result-code mapping, integer narrowing, the `RETURNING` guard, and SqlWriter flag
  caching all **VERIFIED correct**. It found one real defect — `get()` silently
  filtered on only the first key for a composite PK — now fixed: the derive **rejects
  composite primary keys** (compile error), matching the single-column `type Pk`.
- Gate: 115 tests green (all-features), 0 clippy issues, doctest green.

## Review loop — round 3 (universal migrations)

- **Migrations now run out-of-box on any backend.** `Db::migrate(&[Migration])` applies
  pending, versioned migrations through the [`Driver`] (not a backend-specific
  migrator): it creates a `_stakit_migrations` tracking table (portable
  `varchar(255)` PK DDL), runs each pending migration's statements + version record in
  a transaction, and is idempotent. Works on Postgres / `SQLite` / `MySQL` / Turso.
  `Migration { version, statements }` is a plain value (no SQL-file parsing), exported
  in the prelude. E2e verified on `SQLite` and the non-sqlx Turso backend; gated e2e on
  `MySQL`. (Caveat: `MySQL` implicitly commits DDL, so a multi-statement migration that
  fails mid-way is not atomic there — the standard `MySQL` limitation.)
- Gate: 118 tests green (all-features), 0 clippy issues, doctest green.

## Review loop — round 4 (relations)

- **Typed, batched relations (no N+1), backend-agnostic.** `Db::load_has_many(parents,
  child_fk, parent_key, child_key) -> Vec<(P, Vec<C>)>` and
  `Db::load_belongs_to(children, child_key, parent_pk, parent_key) -> Vec<(C, Option<P>)>`
  each issue **one** batched `... WHERE fk IN (keys)` query (via `any_of`, which works on
  every driver) then group in memory. Fully typed — `Col<C, K>` forces the FK type to
  match the parent key. This is Drizzle's relational-load pattern (the efficient
  two-query form, not N+1). E2e verified on **Postgres (live, embedded), `SQLite`, and
  the non-sqlx Turso** backend; gated e2e added for `MySQL`.
- Gate: 121 tests green (all-features), 0 clippy issues, doctest green.

Genuinely remaining (not quick fixes): the CLI `gen` schema-diff DDL generator is still
Postgres-specific (the *runtime* migration apply is universal); a typed-decode fast-path
to skip per-cell `Value` materialization (perf); declarative `#[has_many]`/`#[belongs_to]`
codegen on top of the working `load_*` primitives; and **live `MySQL` e2e**, which is
environment/infra-blocked — there is no in-process MySQL in Rust, and this machine has no
`mysqld`/Docker, so the `MySQL` driver is verified by shared-code-path + offline SQL
rendering + `MYSQL_URL`-gated e2e (runnable in CI with a MySQL service).

## Review loop — round 5 (enums, date/time, JSON, indexes, FK cascade) + audit

New capabilities (all tested across backends):
- **`#[derive(DbEnum)]`** — fieldless enums as columns out of the box: text by default
  (variant name, `#[db_enum(rename="...")]` per variant), or `#[db_enum(int)]` storing
  the discriminant (`= N` / `#[db_enum(value=N)]` / declaration index). Generates
  `ToValue`/`FromValue`/`IntoExpr`. **Duplicate labels/values are a compile error**
  (would otherwise be a silent lossy round-trip). Stored as portable `text`/`int`
  columns — native PG `CREATE TYPE` / MySQL `ENUM` (1–2 byte) optimization is **not**
  generated (documented; reachable via `sql_type` + manual DDL, with a cast caveat on PG).
- **Full chrono temporal**: `Value::{Timestamptz, NaiveDateTime, Date, NaiveTime}` →
  `DateTime<Utc>`/`NaiveDateTime`/`NaiveDate`/`NaiveTime` → `timestamptz`/`timestamp`/
  `date`/`time`. Bind+read on all four drivers; Turso stores temporals as text with a
  fixed `NaiveDateTime` format (Display-vs-FromStr hazard fixed) and accepts
  `CURRENT_TIMESTAMP`-style values for `DateTime<Utc>`.
- **JSON**: `Value::Json` (serde_json::Value) → `json`/`jsonb`; bind+read on all four
  (Turso via text). Select/insert; filter via `raw_pred`.
- **Indexes**: `#[column(index)]` → `Column.is_index`; CLI `gen` emits `CREATE INDEX`
  on table-create and column-add. (Toggling index on an existing column is not yet
  diffed — use a manual migration; composite indexes are future work.)
- **FK `ON DELETE CASCADE` now actually enforced**: `connect_sqlite` sets
  `SqliteConnectOptions::foreign_keys(true)` on every pooled connection, and
  `connect_turso_local`/`_remote` run `PRAGMA foreign_keys = ON` — previously cascade
  silently no-opped on SQLite/Turso (the prior test only passed because it set the
  pragma by hand). Postgres/MySQL enforce FKs natively.

Audit (3 background sub-agents: enums/JSON/custom, date/time, indexes/FK/migrations)
found and fixed: the SQLite/Turso FK-pragma CRITICAL above, the DbEnum duplicate-guard,
and the Turso `CURRENT_TIMESTAMP` decode fallback. Known remaining (documented): CLI
`gen` DDL is Postgres-flavored (runtime `Db::migrate` is universal); native enum types
and JSON-path operators are out of scope; index-toggle diffing and composite indexes
are future work.

Gate: **142 tests pass with all four backends LIVE** (Postgres embedded, SQLite, Turso,
and MySQL/MariaDB via `brew install mariadb`), 0 clippy issues (pedantic+nursery,
all-features), fmt + doctest green.

## Review loop — round 6 (vector search, typed JSON/structs, full-text search)

- **Vector search** (`src/vector.rs`): `Vector(Vec<f32>)` column type + `Value::Vector`;
  dialect-aware bind wrapping (`$N::vector` pgvector, `vector32($N)` Turso, plain
  `sqlite-vec`) so inserts just work; `Select::nearest(col, &q, Distance)` renders the
  per-backend distance ORDER BY (`<->`/`<=>`/`<#>`, `vector_distance_*`,
  `vec_distance_*`); and a **selectable** `vector::distance(col, &q, metric)`
  projection (output `f64`) so you get the score back, not just ordering. E2e on Turso
  (insert via `vector32`, cosine + L2 nearest, score, blob round-trip via LE-f32
  decode); pgvector nearest + score SQL render-tested (all 3 metrics).
- **Typed JSON / structs** (`src/json.rs`): `Json<T>` stores any `serde` struct in a
  `json`/`jsonb`/text column; `Value::Json` (serde_json::Value) works raw too. Tested.
- **Full-text search**: `matches(col, query)` → `to_tsvector @@ plainto_tsquery`
  (Postgres, core/no-extension) or FTS5 `MATCH` (SQLite/Turso). E2e on all three.
- Review (3 sub-agents: safety, idiomatic, perf+coverage). Fixed: added the
  selectable distance-score projection (the unanimous CRITICAL gap), `count`/`exists`
  now clear the vector ORDER BY, `From<[f32;N]>`/`From<&[f32]>` for `Vector`,
  `Vector`/`Distance`/`Json`/`distance` in the prelude, `vector` unit tests
  (literal round-trip/empty/malformed), L2+InnerProduct render tests, README rewritten
  (the stale "not modeled" caveats were wrong — now documents vector/FTS/JSON + ANN
  index DDL + pg `::text` read caveat honestly).
- Gate: 151 tests + 4 doctests pass, 0 clippy (all-features). NOTE: builds now require
  rustc ≥ 1.94 (sqlx 0.9 MSRV); the repo's `rust-toolchain.toml` pins
  nightly-2025-11-20 (1.93) which is **too old** — verified under `stable` (1.96).

Known follow-ups (next loop): FTS relevance ranking (`ts_rank`/`bm25`) as a typed
projection + order-by-rank; `Distance::Raw`/custom-distance escape hatch; binary
vector binds (perf for large embeddings, avoids text formatting + pg `::text` reads);
live pgvector/PostGIS/sqlite-vec e2e (need the extensions); `Json<T>` serialize-error
currently falls back to JSON null (infallible `ToValue`); MySQL `MATCH … AGAINST`.

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

The multi-backend abstraction adds no per-row heap allocation on the collect path:
drivers decode rows inline through a borrowed `&dyn Row`/`RowSink` (one fat-pointer
dispatch per cell, no `Box` per row). The decode closure crosses the driver
`await`, so `all`/`one`/`exact_one` carry `P: Sync, P::Output: Send` bounds (held by
every built-in projection and table struct), which also keep query futures
spawnable.
