# stakit-orm

A high-performance, type-safe, **database-agnostic** ORM for Rust — inspired by
[Drizzle](https://orm.drizzle.team), built for speed and ergonomics.

One typed query builder runs on **Postgres, SQLite, MySQL** (via [sqlx](https://github.com/launchbadge/sqlx))
and **Turso / libSQL** (not sqlx) behind a single `Driver` trait. Return types are
inferred from the query — no `let rows: Vec<T>` annotations, no stringly-typed SQL.

```rust
use stakit_orm::prelude::*;

#[derive(Table, Debug)]
#[table(name = "users")]
struct User {
    #[column(pk)]
    id: i64,
    #[column(unique)]
    email: String,
    name: String,
    age: i32,
}

// type inferred as Vec<User> — no annotation
let users = db.find::<User>().filter(eq(User::email, "a@x.com")).all().await?;
```

---

## Why

- **Truly database-agnostic.** The same builder, schema, and types work on four
  backends; switching is one constructor call. The core never names a concrete sqlx
  type — the proof is that **Turso/libSQL (not sqlx at all) runs the identical code**.
- **Fast.** SQL is assembled once at the terminal into a pre-sized `String` with an
  inline bind buffer; the dialect's flags are cached (no vtable dispatch per bind);
  the row-collect path decodes **inline with zero per-row allocation**. Simple
  `SELECT` builds in ~100 ns.
- **Typed end to end.** Column tokens (`User::id`) carry their type, so comparisons,
  joins, and relations are checked at compile time. Foreign keys are type-verified by
  the derive.
- **Easy DX.** `find`, `get`-by-PK, projections that infer their output,
  `#[derive(Row)]` for named result shapes, batched relations, and migrations that
  just run.

## Install

Backends are **opt-in cargo features** — you compile only the driver you use.

```toml
[dependencies]
# Postgres (default)
stakit-orm = "0.1"

# or pick backends explicitly:
stakit-orm = { version = "0.1", default-features = false, features = ["sqlite"] }
stakit-orm = { version = "0.1", default-features = false, features = ["turso"] }
stakit-orm = { version = "0.1", default-features = false, features = ["postgres", "mysql"] }
```

| Feature    | Backend         | Driver            |
|------------|-----------------|-------------------|
| `postgres` | PostgreSQL      | sqlx              |
| `sqlite`   | SQLite          | sqlx              |
| `mysql`    | MySQL / MariaDB | sqlx              |
| `turso`    | Turso / libSQL  | libsql (not sqlx) |

Default = `["postgres"]`. Enabling only `turso` does **not** pull in sqlx-mysql etc.

## Connect

```rust
let db = Db::connect("postgres://user:pass@localhost/app").await?;          // postgres
let db = Db::connect_sqlite("sqlite::memory:").await?;                      // sqlite
let db = Db::connect_mysql("mysql://root@localhost/app").await?;            // mysql
let db = Db::connect_turso_local(":memory:").await?;                        // turso (local / :memory:)
let db = Db::connect_turso_remote("libsql://db.turso.io", "token").await?;  // turso cloud
```

`Db` is cheap to clone (the driver is `Arc`-shared) and `Send + Sync` — share it
across tasks. Wrap an existing pool with `Db::new(pg_pool)` / `Db::sqlite(pool)` /
`Db::mysql(pool)` / `Db::turso(conn)`, or any custom backend with
`Db::from_driver(Arc<dyn Driver>)`.

## Define a schema

```rust
#[derive(Table, Debug)]
#[table(name = "posts")]
struct Post {
    #[column(pk)]
    id: i64,
    #[column(references = User::id, on_delete = "cascade")]
    author_id: i64,
    title: String,
    #[column(nullable)]
    subtitle: Option<String>,
    #[column(default = "0")]
    views: i32, // defaulted -> Option on insert, omitted when None
}
```

`#[derive(Table)]` generates:

- typed column tokens — `Post::id : Col<Post, i64>`
- `Post::all()` whole-row projection
- `PostNew` insert companion (defaulted columns become `Option`, omitted when `None`
  so the DB default fires)
- compile-time **foreign-key type checks** (`author_id` must match `User`'s PK type)
- compile-time identifier validation (empty / NUL / 63-byte limit)

Column attributes: `pk`, `unique`, `nullable`, `default = "<sql>"`, `name = "<col>"`,
`sql_type = "<type>"`, `references = Type::col`,
`on_delete = "cascade|restrict|set null|no action"`. Composite primary keys are
rejected (use exactly one `#[column(pk)]`).

## Query

```rust
// whole row, output inferred as User
let u = db.get::<User>(1).one().await?;                       // by primary key
let u = db.find::<User>().filter(eq(User::id, 1)).one().await?;

// partial projection -> tuple, inferred as (i64, String)
let pairs = db.select((User::id, User::email)).from::<User>().all().await?;

// filters: eq ne gt lt gte lte like is_null and or not any_of
let some = db.find::<User>()
    .filter(and(gt(User::age, 18), not(like(User::email, "%@spam.com"))))
    .order_by(asc(User::age))
    .limit(20)
    .offset(40)
    .all().await?;

// IN membership — one array bind on Postgres, IN (?, …) elsewhere; empty -> no rows
let batch = db.find::<User>().filter(any_of(User::id, &[1_i64, 2, 3])).all().await?;

// terminals: all / one / one_or_err / exact_one / count / exists / stream
let n = db.find::<User>().count().await?;                     // i64
let exists = db.find::<User>().filter(eq(User::id, 1)).exists().await?; // bool
```

### Aggregates & grouping

```rust
let total = db.select(count()).from::<Post>().one().await?;           // Option<i64>
let max_views = db.select(max(Post::views)).from::<Post>().one().await?; // Option<Option<i32>>
let sum = db
    .select(stakit_orm::sum::<Option<i64>, _, _>(Post::views))
    .from::<Post>()
    .group_by(Post::author_id)
    .one().await?;
```

Also: `min`, `count_col`, `avg::<Out>`, `.having(pred)`.

### Joins (typed)

```rust
// whole-row tuples decoded positionally; nullable() for outer-join sides
let rows = db
    .select((Post::all(), User::all().nullable())) // -> Vec<(Post, Option<User>)>
    .from::<Post>()
    .left_join::<User>(eq(Post::author_id, User::id))
    .all().await?;
```

`inner_join`, `left_join`, `right_join`.

### Named result shapes — `#[derive(Row)]`

For ad-hoc projections (incl. aggregates and raw SQL) decoded into a named struct:

```rust
#[derive(stakit_orm::Row, Debug)]
struct AuthorStat {
    #[from(Post::author_id)]
    author_id: i64,
    #[from(stakit_orm::count())]
    posts: i64,
    #[from(stakit_orm::sum::<Option<i64>, _, _>(Post::views))]
    total_views: Option<i64>,
}

let stats = db.select(AuthorStat::project())
    .from::<Post>()
    .group_by(Post::author_id)
    .all().await?; // Vec<AuthorStat>
```

Raw SQL expression in a projection:
`sql_expr::<i32>("extract(year from created_at)")`.

## Insert / update / delete / upsert

```rust
db.insert(UserNew { id: 1, email: "a@x.com".into(), name: "Ann".into(), age: 30 })
    .exec().await?;

// many rows, one statement
db.insert_many(rows).exec().await?;

// RETURNING (Postgres / SQLite / Turso; errors on MySQL with Error::Unsupported)
let id = db.insert(new_user).returning(User::id).one().await?;

// upsert
db.insert(new_user).on_conflict_do_update(User::id).exec().await?;

db.update::<User>().set(User::name, "Renamed").filter(eq(User::id, 1)).exec().await?;
db.delete::<User>().filter(eq(User::id, 1)).exec().await?;
```

## Transactions

Commit on `Ok`, roll back on `Err`. Issue queries on the `Tx` handle sequentially.

```rust
db.transaction(|tx| async move {
    tx.insert(new_user).exec().await?;
    tx.update::<User>().set(User::name, "Ann2").filter(eq(User::id, 1)).exec().await?;
    Ok(())
}).await?;
```

## Relations (batched, no N+1)

Typed `has_many` / `belongs_to` loaders. Each runs **one** batched `WHERE fk IN (…)`
query, then groups in memory — Drizzle's efficient relational pattern, not N+1.

```rust
let authors = db.find::<Author>().all().await?;
let with_posts = db
    .load_has_many::<Author, Post, i64>(authors, Post::author_id, |a| a.id, |p| p.author_id)
    .await?; // Vec<(Author, Vec<Post>)>

let posts = db.find::<Post>().all().await?;
let with_author = db
    .load_belongs_to::<Post, Author, i64>(posts, |p| p.author_id, Author::id, |a| a.id)
    .await?; // Vec<(Post, Option<Author>)>
```

`Col<C, K>` forces the foreign-key type to match the parent key at compile time.
`belongs_to` requires the parent to be `Clone` (a parent may be shared by children).

## Migrations (out-of-box, any backend)

Migrations run through the `Driver`, so they work on every backend with no
backend-specific migrator. Versioned, idempotent, transactional per migration.

```rust
let applied = db.migrate(&[
    Migration {
        version: "0001_init",
        statements: &["create table users (id integer primary key, name text not null)"],
    },
    Migration {
        version: "0002_seed",
        statements: &["insert into users (id, name) values (1, 'Ann')"],
    },
]).await?; // 2 on first run, 0 after (idempotent)
```

Applied versions are tracked in a `_stakit_migrations` table. (MySQL implicitly
commits DDL, so a multi-statement migration that fails mid-way is not atomic there —
the standard MySQL caveat.)

## Raw SQL escape hatch

```rust
// decodes positionally into a Table type, in COLUMNS order
let users = db.raw("select id, name from users where id > ?")
    .bind(10_i64)
    .all::<User>().await?;

db.raw("vacuum").exec().await?;
```

## Errors

`Error` classifies constraint violations across **all** backends (sqlx `ErrorKind`
for pg/sqlite/mysql; SQLite extended result codes for Turso):

```rust
match db.insert(dup).exec().await {
    Err(e) if e.is_unique() => { /* unique violation */ }
    Err(e) if e.is_foreign_key() => { /* FK violation */ }
    other => { other?; }
}
```

Variants include `Unique`, `ForeignKey`, `NotNull`, `Check`, `NotFound`,
`TooManyRows`, `Unsupported`, plus the concrete backend errors
`Database(sqlx::Error)` and (with `turso`) `Turso(libsql::Error)` — not boxed.

## Custom column types & extensions (pgvector, PostGIS, …)

Any type that implements `ToValue` + `FromValue` is usable as a column type — map it
to an existing `Value` variant (`Text`/`Bytes`/`I64`/…):

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
struct Tags(Vec<String>);

impl stakit_orm::ToValue for Tags {
    fn to_value(self) -> stakit_orm::Value {
        stakit_orm::Value::Text(self.0.join(","))
    }
}
impl stakit_orm::FromValue for Tags {
    const KIND: stakit_orm::ValueKind = stakit_orm::ValueKind::Text;
    fn from_value(v: stakit_orm::Value) -> stakit_orm::Result<Self> {
        match v {
            stakit_orm::Value::Text(s) => Ok(Self(s.split(',').map(String::from).collect())),
            other => Err(stakit_orm::Error::Decode(format!("bad Tags: {other:?}").into())),
        }
    }
}

#[derive(Table)]
#[table(name = "docs")]
struct Doc {
    #[column(pk)]
    id: i64,
    #[column(sql_type = "text")]
    tags: Tags,
}
```

This is the extension point for DB extensions like **pgvector** and **PostGIS**:
represent the value as text/bytes and round-trip it. Caveats today:

- **Reading** works directly (`vector`/`geometry` columns have text output → parse in
  `FromValue`).
- **Writing** to a native `vector`/`geometry` column usually needs an explicit cast —
  the typed `insert` binds a plain param and does not add `::vector`. Use
  `db.raw("insert … values ($1::vector)")` for the cast.
- **Operators** (`<->` KNN, `ST_DWithin`, …) aren't modeled by the typed builder — use
  `sql_expr::<T>("…")` in projections and `raw_pred("…")` / `db.raw(…)` in filters.

So custom scalar types are first-class and tested; full native vector/geo support is
reachable via the raw/`sql_expr` escape hatches (first-class binary + operators are
tracked as future work).

## Custom backends

Implement `Driver` (`fetch` via a `RowSink`, `execute`, `stream`, `begin`,
`dialect`) plus `Row` for your result row and a `Dialect`, then
`Db::from_driver(Arc::new(MyDriver))`. The whole query builder, migrations, and
relations work unchanged.

## Performance notes

- SQL assembled once per query into a pre-sized `String`; binds in an inline
  `SmallVec` (no heap for ≤4 binds).
- Dialect flags (placeholder, quote char, numbered, array support) cached in the
  writer — zero vtable dispatch in the assembly loop.
- Collect path (`all`/`one`/…) decodes each row **inline** via a borrowed `&dyn Row`
  — **no `Box` per row**. Only `stream()` boxes (it must yield owned items).
- Build-time work in the derive: column metadata, tokens, arities, FK checks.

## Status

Verified end-to-end on **Postgres, SQLite, Turso/libSQL, and MySQL** (live). See
`IMPLEMENTATION_STATUS.md` for the full feature matrix and review history.

## Testing

```bash
# Postgres (embedded, no Docker) + SQLite + Turso, all in-process:
cargo nextest run -p stakit-orm --all-features

# Live MySQL / MariaDB:
MYSQL_URL=mysql://root@127.0.0.1:3306/test \
  cargo nextest run -p stakit-orm --features mysql
```

## License

See the workspace root.
