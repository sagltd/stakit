# stakit_orm — Design Spec

**Date:** 2026-06-02
**Status:** Draft — pending review
**Crate(s):** `stakit-orm`, `stakit-orm-derive`, `stakit-orm-cli`

## 1. Goal

A high-performance, type-safe Postgres ORM for the stakit workspace, inspired by
Drizzle ORM. Schema is defined once in Rust; everything else (typed queries,
migrations, row decoding) flows from it. Built **on top of sqlx** — we own the
ORM/SQL-building/migration-generation layer; sqlx owns the wire protocol,
connection pooling, query execution, and migration application.

Four pillars, all required:

1. **ORM** — typed, composable query builder (Drizzle-style).
2. **Raw** — full escape hatch to sqlx for any query.
3. **Model derive** — `#[derive(Table)]` defines schema + emits typed tokens.
4. **Migrations** — generated from the Rust schema (no app run), applied by sqlx.

Postgres only for v1. Internals kept generic enough (traits over an executor) to
leave room for other backends later, but no second backend is built now.

## 2. Foundation decision

**sqlx** is the backend (chosen by user). We reuse:

- Connection pool (`PgPool`), execution, binding, `FromRow`.
- Migration **runtime**: file format (`<ts>-<name>.up.sql` / `.down.sql`),
  `sqlx::migrate!`, `_sqlx_migrations` tracking + checksums, apply/revert.

We build on top: typed query builder, derive macros, and the **generation** of
migration SQL from the Rust schema (sqlx does not do schema diffing — confirmed
from sqlx-cli docs; migrations there are hand-written).

Honest limits of "zero-copy" on sqlx: query *building* is allocation-light (ZST
column tokens + `smallvec` buffers, SQL string built once at the terminal). Row
*decode* into owned structs copies (sqlx `PgRow` owns its buffer); borrowed
`&str`/`&[u8]` reads within row lifetime are possible but `FromRow` into owned
types copies. Accepted tradeoff vs. hand-rolling the pg wire protocol.

## 3. Crate layout

```
crates/
  stakit-orm/            # runtime: query builder, executor, traits, errors, prelude
  stakit-orm-derive/     # #[derive(Table)], #[derive(Row)], row!, sql! (syn/quote)
  stakit-orm-cli/        # cargo stakit-orm gen/up/down/status — migration generation
```

Naming note: `stakit-model` already owns the `Model` name (validation + TS
export, unrelated to DB). This crate uses `Table` / `Row`, no clash.

Perf crates: `smallvec` (column/predicate lists — most queries < 8, stack-alloc),
`indexmap` (deterministic column ordering in schema snapshot), `hashbrown`
(lookup maps in migration diff + relation stitching). Follows the workspace
"use latest dep versions" rule.

## 4. Schema definition + derive

```rust
use stakit_orm::prelude::*;

#[derive(Table)]
#[table(name = "users")]
pub struct User {
    #[column(pk, default = "gen_random_uuid()")]
    pub id: Uuid,
    #[column(unique)]
    pub email: String,
    pub name: String,
    #[column(name = "created_at", default = "now()")]
    pub created_at: DateTime<Utc>,

    // relation — virtual, NOT a column (skipped in SQL + migration)
    #[has_many(Post, fk = author_id)]
    pub posts: Rel<Vec<Post>>,
}

#[derive(Table)]
#[table(name = "posts")]
pub struct Post {
    #[column(pk, default = "gen_random_uuid()")]
    pub id: Uuid,
    #[column(references = User::id, on_delete = "cascade")]
    pub author_id: Uuid,
    pub title: String,
    #[column(nullable)]
    pub body: Option<String>,

    #[belongs_to(User, fk = author_id)]
    pub author: Rel<User>,
    #[has_many(Comment, fk = post_id)]
    pub comments: Rel<Vec<Comment>>,
}
```

`#[derive(Table)]` emits (zero runtime cost):

- `impl Table for User` — table name, `&'static [Column]` const (name, sql type,
  pk/unique/nullable/default/fk), associated `PkTy`.
- **Column tokens**: `User::id: Col<User, Uuid>`, `User::email: Col<User, String>`
  — typed, ZST-ish. Used everywhere in queries.
- **Relation tokens**: `User::posts: Relation<User, Post, Many>`.
- sqlx `FromRow` (delegated).

### Compile-time checks (what rustc verifies)

| Check | When | Mechanism |
|---|---|---|
| `User`/`Post` type exists | compile | path resolution after macro expand |
| `User::id` column exists | compile | `User::id` is a real associated const |
| FK target column exists (`references = User::id`) | compile | path resolves |
| FK type == referenced PK type | compile | emitted const assert via `<User as Table>::PkTy` |
| `has_many(Post, fk = author_id)` — `Post` + `Post::author_id` exist, fk type matches User PK | compile | emitted const referencing `Post::author_id` |
| `on_delete = "cascade"` is a valid keyword | compile (macro) | match against allowed set (`cascade`/`restrict`/`set null`/`no action`) |
| FK target is actually pk/unique; DB has the constraint | migration gen / apply | snapshot diff / runtime |

Semantic/DB-truth checks that a proc-macro cannot see from another struct's site
are deferred to migration generation, not the build.

## 5. Migrations — generated, sqlx-applied

Schema lives in a parseable file (default `src/schema.rs`). The CLI generates SQL
**without compiling or running the app**.

### Generation approach (Option 3 — syn parse)

1. `syn`-parse `src/schema.rs` → current schema model (tables, columns, FKs).
2. Read `migrations/.snapshot.json` → last-known schema (empty on first run).
3. Diff → changes (new table, add/drop/alter column, FK change).
4. Write sqlx-format files into `migrations/`:
   - `migrations/<ts>_<name>.up.sql`
   - `migrations/<ts>_<name>.down.sql`
5. Update `migrations/.snapshot.json`.

Rationale: no compile, no DB, no app boot; reuses `syn` (already a workspace
dep). Constraint: schema must live in known parseable file(s), not be generated
behind other macros. Accepted.

(Alternatives considered: `inventory`/`linkme` compile-time registry — needs a
build; proc-macro writing JSON to `OUT_DIR` — macro file IO is smelly/stale-prone.
Option 3 chosen for simplicity + no build/run.)

### Example output

`migrations/20260602120000_init.up.sql`:
```sql
create table users (
    id uuid primary key default gen_random_uuid(),
    email text not null unique,
    name text not null,
    created_at timestamptz not null default now()
);
create table posts (
    id uuid primary key default gen_random_uuid(),
    author_id uuid not null references users(id) on delete cascade,
    title text not null,
    body text
);
```

`...init.down.sql`:
```sql
drop table posts;
drop table users;
```

Generated SQL is reviewed by the user before apply, and may be hand-edited if the
generator is wrong.

### CLI

```bash
cargo stakit-orm gen "init"      # syn-parse schema.rs, diff snapshot, emit .up/.down sql
cargo stakit-orm up              # -> sqlx migrate run   (apply pending)
cargo stakit-orm down            # -> sqlx migrate revert
cargo stakit-orm status          # -> sqlx migrate info
```

In-app apply at boot:
```rust
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");
MIGRATOR.run(&pool).await?;   // sqlx applies pending, tracks _sqlx_migrations
```

Diff scope phasing: v1 = create table + add column + new table; alter-type / drop
detection in a later pass (flagged to user, hand-edit allowed meanwhile).

## 6. Query API 1 — SQL builder (Drizzle `db.select()`)

Free-function operators (exactly Drizzle): `eq`, `and`, `or`, `gt`, `lt`, `gte`,
`lte`, `ne`, `like`, `in_`, `is_null`, `asc`, `desc`. `where` is a Rust reserved
keyword → the method is **`.filter()`**.

```rust
use stakit_orm::prelude::*;
use stakit_orm::expr::{eq, and, gt, desc, count};

let db = Db::new(pool);   // thin wrap over sqlx PgPool

// whole rows, many -> Vec<User>  (inferred, no annotation)
let users = db.select(User::all())
    .from::<User>()
    .filter(and(eq(User::name, "Dan"), gt(User::id, 5)))
    .order_by(desc(User::created_at))
    .limit(10).offset(20)
    .all().await?;

// one whole row -> Option<User>
let u = db.select(User::all()).from::<User>().filter(eq(User::id, uid)).one().await?;

// join -> output type inferred: inner => (Post, Comment), left => (Post, Option<Comment>)
let rows = db.select((Post::all(), Comment::all()))
    .from::<Post>()
    .left_join::<Comment>(eq(Post::id, Comment::post_id))
    .filter(eq(Post::id, 10))
    .all().await?;
```

`eq` is generic on both sides: `eq(Post::id, Comment::post_id)` (col=col, joins)
and `eq(Post::id, 10)` (col=value). Type-checked — `eq(User::id, "str")` fails to
compile (id is `Uuid`). Values are bound as positional `$N` params (injection-safe).

### Terminals

| Method | Returns | Use |
|---|---|---|
| `.all()` | `Vec<Output>` | select many |
| `.one()` | `Option<Output>` | select one (LIMIT 1) |
| `.one_or_err()` | `Output` | one, `Error::NotFound` if absent |
| `.exact_one()` | `Output` | err if 0 (`NotFound`) or >1 (`TooManyRows`) |
| `.stream()` | `impl Stream<Item = Result<Output>>` | large result, unbuffered |
| `.count()` | `i64` | shortcut `select count(*)` |
| `.exists()` | `bool` | shortcut `select exists(...)` |
| `.exec()` | `u64` | insert/update/delete rows affected |

## 7. Select projections — the `Projection` trait

`select()` takes anything implementing `Projection { type Output; }`. The argument
decides the return type — **no annotation needed**, inference flows from
`Projection::Output` through the terminal (`.all()` → `Vec<Output>`, etc.).

```rust
pub trait Projection {
    type Output;
    fn columns(&self) -> SmallVec<[SqlExpr; 8]>;
}
```

| `select(...)` argument | `Output` |
|---|---|
| `User::id` (single `Col`) | `Uuid` |
| `(User::id, User::name)` (tuple) | `(Uuid, String)` |
| `count()` / `count(Post::id)` | `i64` |
| `sum(Col<_,T>)` | `T` ; `avg(_)` → `f64` |
| `User::all()` | `User` |
| `UserStat::project()` (derive Row) | `UserStat` |
| `row! { .. }` | anonymous named struct |

### A) Tuple — quick, positional, inferred

```rust
let rows = db.select((User::email, User::id, count(Post::id)))
    .from::<User>()
    .left_join::<Post>(eq(User::id, Post::author_id))
    .group_by((User::email, User::id))
    .all().await?;   // Vec<(String, Uuid, i64)>
```

### B) `#[derive(Row)]` — named, typed, reusable

```rust
#[derive(Row)]
struct UserStat {
    email: String,
    #[from(User::id)]            // map field -> source expr (optional if names align)
    user_id: Uuid,
    #[from(count(Post::id))]
    post_count: i64,
}

let rows = db.select(UserStat::project())
    .from::<User>()
    .left_join::<Post>(eq(User::id, Post::author_id))
    .group_by((User::email, User::id))
    .all().await?;   // Vec<UserStat>
```

Derive checks each field type == source expr `Output` (mismatch = compile error).

### C) `row! {}` — inline named, Drizzle-closest

```rust
let rows = db.select(row! {
        email:      User::email,                                   // -> String
        user_id:    User::id,                                      // -> Uuid
        post_count: count(Post::id),                               // -> i64
        year:       sql!(i32, "extract(year from {})", Post::created_at),
        upper:      sql!(String, "upper({})", User::name),
        active:     sql!(bool, "{} > ?", Post::views, 100),        // {}=col, ?=bind
    })
    .from::<User>()
    .left_join::<Post>(eq(User::id, Post::author_id))
    .group_by((User::email, User::id))
    .all().await?;

rows[0].post_count;   // i64, named, no predeclare
```

**`row!` mechanics** (proc-macro runs pre-typecheck, so it can't name field types
directly — uses a generic local struct + decode closure; inference fills types):

```rust
{
    struct Row<A, B, C, ..> { email: A, user_id: B, post_count: C, .. }
    Projection::named(
        [expr(User::email), expr(User::id), expr(count(Post::id)), ..],
        |r| Row { email: r.get(0), user_id: r.get(1), post_count: r.get(2), .. },
    )
}
// Output = Row<String, Uuid, i64, ..>
```

**`sql!` rules** (raw SQL fragment, typed):

- 1st arg = output Rust type → field gets that concrete type (closes the
  inference gap; raw `sql!` fields are explicitly typed).
- `{}` placeholder = **column token** (`Col`) → rendered as quoted identifier
  (`"posts"."created_at"`); compile-checked (column must exist).
- `?` placeholder = **value bind** → positional `$N` param (injection-safe,
  never string-spliced).

Tradeoffs: tuple = fully checked at select but positional. Derive Row = named +
checked at select. `row!` = best ergonomics, but non-`sql!` field types are
checked at decode rather than select, and the local struct can't cross a function
boundary (fine for query-and-consume).

Phasing: **A + B + C all in v1** (user wants C). `sql!` ships with C.

## 8. Query API 2 — relational (Drizzle `db.query.*`)

Mirrors Drizzle's relational builder. Method names match Drizzle: `find_many` /
`find_first`.

```rust
// db.query.users.findMany({ with: { posts: true } })
let users = db.query::<User>().with(User::posts).find_many().await?;   // Vec<UserWith>
for u in &users { for p in &u.posts { /* loaded */ } }

// nested with + filter + order + limit
let posts = db.query::<Post>()
    .with(Post::comments, |c| c.with(Comment::author))
    .filter(eq(Post::id, 10))
    .order_by(asc(Post::id))
    .limit(5)
    .find_many().await?;

// findFirst
let one = db.query::<User>().filter(eq(User::id, 1)).find_first().await?;  // Option<UserWith>

// columns selection (Drizzle `columns: { id: true }`)
let slim = db.query::<Post>().columns((Post::id, Post::title)).find_many().await?;
```

**Loading strategy:** batched, not N+1, not one giant join. One query per relation
level (`... where fk in (...)`), stitched in Rust via `hashbrown` map (Drizzle's
default behavior).

**The hard part (honest):** API 1 maps cleanly to Rust. API 2's nested typed
result needs codegen — Drizzle leans on TS structural typing, Rust has none. The
`#[has_many]`/`#[belongs_to]` derive generates a `UserWith` result type with
loaded relation fields. Arbitrary-depth nesting is real codegen complexity.

Phasing: **v2** = relational, one level of `.with()`. **v3** = nested `.with()`,
CTEs, subqueries, broader aggregates.

## 9. Raw escape hatch

Always available. No lock-in.

```rust
// typed raw -> any FromRow / projection
let users: Vec<User> = db.raw("select * from users where created_at > $1")
    .bind(since)
    .all().await?;

// drop straight to sqlx
let n: i64 = sqlx::query_scalar("select count(*) from users")
    .fetch_one(db.pool()).await?;
```

## 10. Inserts + batching

```rust
// single
let id: Uuid = db.insert(User { .. }).returning(User::id).one().await?;
db.insert(user).exec().await?;                       // -> rows affected

// many -> multi-row VALUES, auto-chunked
let n = db.insert_many(users).exec().await?;
let ids: Vec<Uuid> = db.insert_many(users).returning(User::id).all().await?;

// upsert
db.insert(user).on_conflict(User::email).do_update().exec().await?;
db.insert(user).on_conflict(User::email).do_nothing().exec().await?;

// fastest bulk (10k+ rows) -> COPY binary
let n = db.copy_into::<User>(users).await?;
```

### Speed tiers

| Rows | API | Mechanism |
|---|---|---|
| 1 | `insert` | single `INSERT` |
| 2 – ~1k | `insert_many` | one `INSERT ... VALUES (...),(...),...`, one round-trip |
| huge | `copy_into` | `COPY ... FROM STDIN (FORMAT BINARY)` via sqlx `PgCopyIn` — no per-row parse/plan/bind |

**Param-limit trap:** Postgres caps bind params at 65535 per statement.
`insert_many` computes `rows_per_statement = 65535 / column_count`, chunks
accordingly, and wraps all chunks in one transaction — the caller never hits the
limit. `copy_into` has no param limit and no per-row planning (fastest for big
loads).

Recommended defaults: `insert_many` for normal app writes; `copy_into` for
seed/import/ETL.

## 11. Transactions

Real pg transactions via sqlx. **Auto-rollback** on `Err` return, panic, or drop
without commit. Every query method is generic over the executor, so the same
`db.select/insert/update/...` API works on a pool (`&Db`) or a transaction
(`&mut Tx`).

### Closure style (recommended — cannot forget rollback)

```rust
let new_id = db.transaction(|tx| async move {
    let uid: Uuid = tx.insert(User { .. }).returning(User::id).one().await?;
    tx.insert(Post { author_id: uid, .. }).exec().await?;
    tx.update::<User>().set(User::name, "Sam").filter(eq(User::id, uid)).exec().await?;
    Ok(uid)
}).await?;
// Ok  -> COMMIT
// Err -> ROLLBACK (any `?` failure undoes the whole tx; all-or-nothing)
```

### Manual style

```rust
let mut tx = db.begin().await?;
tx.insert(user).exec().await?;
tx.commit().await?;          // explicit; drop/early-return without commit -> ROLLBACK
```

Business-logic rollback (not just DB errors) by returning `Err`. Nested
transactions map to pg `SAVEPOINT` / `ROLLBACK TO` via `tx.transaction(|sp| ...)`.

## 12. Error handling

`thiserror` enum; sqlx errors mapped to typed Postgres variants via SQLSTATE. No
raw sqlx error leakage in the common path.

```rust
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not found")]
    NotFound,
    #[error("unique violation on {constraint}")]
    Unique { constraint: String },          // 23505
    #[error("foreign key violation on {constraint}")]
    ForeignKey { constraint: String },      // 23503
    #[error("not-null violation on {column}")]
    NotNull { column: String },             // 23502
    #[error("check violation on {constraint}")]
    Check { constraint: String },           // 23514
    #[error("too many rows: expected one")]
    TooManyRows,                            // exact_one
    #[error(transparent)]
    Decode(sqlx::error::BoxDynError),
    #[error(transparent)]
    Database(sqlx::Error),                  // unmapped fallback
}
pub type Result<T> = core::result::Result<T, Error>;
```

Ergonomic helpers: `Error::is_unique()`, `is_foreign_key()`, `is_not_found()` plus
the `constraint`/`column` fields for targeted handling.

```rust
match db.insert(user).exec().await {
    Err(Error::Unique { constraint }) if constraint == "users_email_key" => { /* dup */ }
    Ok(_) => {}
    Err(e) => return Err(e),
}
```

## 13. Build-by-unit isolation

- `Col<T, Ty>` / `Relation<..>` — typed schema tokens. Depend on `Table`. Testable
  by checking emitted SQL strings.
- `Projection` — maps select args → `Output` + column list. Independent of the
  executor; unit-testable.
- Query builders (`Select`, `Insert`, `Update`, `Delete`) — accumulate state,
  emit SQL once at terminal. Testable via SQL snapshot tests (no DB).
- `Executor` abstraction — runs SQL on pool or tx. Integration-tested against a
  real pg.
- Migration generator (`stakit-orm-cli`) — `syn` parse + diff + emit. Pure
  function `schema → SQL`; unit-testable with fixtures, no DB.
- Error mapper — `sqlx::Error` → `Error`. Pure; unit-testable per SQLSTATE.

## 14. Phasing summary

| Version | Scope |
|---|---|
| **v1** | `#[derive(Table)]` + tokens + compile-checks; migration gen (create/add column) + CLI; query API 1 (select/joins/filter/order/limit, terminals); projections A+B+C (`row!`/`sql!`); insert/insert_many/COPY/upsert; transactions; raw; error mapping |
| **v2** | relational API 2 (one-level `.with()`); migration diff alter/drop |
| **v3** | nested `.with()`; CTEs; subqueries; broader aggregates; possible second backend behind the executor trait |

## 15. Open questions / risks

- **`row!` field-type checking** happens at decode, not select, for non-`sql!`
  fields (the proc-macro pre-typecheck limitation). Acceptable; derive Row (B)
  gives select-time checking when stronger guarantees are wanted.
- **Relational nested types (v2/v3)** are the highest-risk codegen; one level
  first to de-risk.
- **Migration diff** for type changes / renames is ambiguous (rename vs
  drop+add); v1 covers additive changes, surfaces the rest for hand-editing
  rather than guessing.
- **Quality gate:** all crates must pass `./code-check.sh` (fmt, clippy
  pedantic+nursery `-D warnings`, build, nextest, doctests); `unsafe` forbidden;
  public items documented.
