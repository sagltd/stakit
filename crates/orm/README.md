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

Column attributes: `pk`, `unique`, `index`, `nullable`, `default = "<sql>"`,
`name = "<col>"`, `sql_type = "<type>"`, `references = Type::col`,
`on_delete = "cascade|restrict|set null|no action"`. Composite primary keys are
rejected (use exactly one `#[column(pk)]`). `#[column(index)]` makes the CLI emit a
`CREATE INDEX` for that column.

### Foreign keys & `ON DELETE CASCADE`

```rust
#[derive(Table)]
#[table(name = "devices")]
struct Device {
    #[column(pk)]
    id: i64,
    #[column(references = User::id, on_delete = "cascade")] // delete user → delete devices
    user_id: i64,
}
```

The FK type is checked at compile time. `connect_sqlite` and `connect_turso_local`/
`_remote` enable `PRAGMA foreign_keys = ON` on every connection, so cascade is enforced
(SQLite/libSQL leave it off by default); Postgres/MySQL enforce FKs natively.

### Enums — `#[derive(DbEnum)]`

Fieldless enums become column types out of the box, stored as text (default) or int:

```rust
#[derive(DbEnum, Debug, Clone, Copy, PartialEq, Eq)]
enum Status { Active, #[db_enum(rename = "archived_v2")] Archived } // text

#[derive(DbEnum, Debug, Clone, Copy, PartialEq, Eq)]
#[db_enum(int)]
enum Level { Low = 1, Mid = 5, High = 9 } // int (Rust discriminants)
```

Use them with `#[column(sql_type = "text")]` / `"int"`. They work in select, insert,
and filters (`eq(Ticket::status, Status::Active)`). Duplicate labels/values are a
compile error. Stored as portable `text`/`int` columns (not native PG/MySQL enum types).

### Date & time (chrono)

| Rust type (chrono)   | SQL type      | Use for                              |
|----------------------|---------------|--------------------------------------|
| `DateTime<Utc>`      | `timestamptz` | absolute instants, audit logs        |
| `NaiveDateTime`      | `timestamp`   | wall-clock, tz managed externally    |
| `NaiveDate`          | `date`        | birthdays, calendar days             |
| `NaiveTime`          | `time`        | recurring daily times (`08:00:00`)   |

All four bind, read, and filter on every backend (`Option<_>` for nullable). Best
practice: store absolute events as `DateTime<Utc>`, context-free calendar values as
the naive types.

### JSON

`serde_json::Value` is a column type (`json`/`jsonb`; text on SQLite/Turso):

```rust
#[derive(Table)]
#[table(name = "docs")]
struct Doc {
    #[column(pk)]
    id: i64,
    meta: serde_json::Value,                 // jsonb
    #[column(nullable)]
    extra: Option<serde_json::Value>,
}
```

Select/insert work directly; filter on JSON via `raw_pred(...)`.

## Query

```rust
// whole row, output inferred as User
let u = db.get::<User>(1).one().await?;                       // by primary key
let u = db.find::<User>().filter(eq(User::id, 1)).one().await?;

// partial projection -> tuple, inferred as (i64, String)
let pairs = db.select((User::id, User::email)).from::<User>().all().await?;

// filters: eq ne gt lt gte lte like contains is_null and or not any_of matches matches_in
let some = db.find::<User>()
    .filter(and(gt(User::age, 18), not(like(User::email, "%@spam.com"))))
    .order_by(asc(User::age))
    .limit(20)
    .offset(40)
    .all().await?;

// IN membership — one array bind on Postgres, IN (?, …) elsewhere; empty -> no rows
let batch = db.find::<User>().filter(any_of(User::id, &[1_i64, 2, 3])).all().await?;

// literal substring — LIKE-metacharacters in the needle are escaped (no wildcard
// injection); `like(...)` is the raw form when you want to supply your own pattern
let acme = db.find::<User>().filter(contains(User::email, "@acme.")).all().await?;

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

// upsert — overwrite every non-key column on a single-column conflict
db.insert(new_user).on_conflict_do_update(User::id).exec().await?;

db.update::<User>().set(User::name, "Renamed").filter(eq(User::id, 1)).exec().await?;
db.delete::<User>().filter(eq(User::id, 1)).exec().await?;
```

### Upsert: composite keys & per-column updates

`on_conflict(key)` takes a single column **or a tuple** for a composite key, then you
pick exactly which columns to refresh — every column you don't list is left untouched:

```rust
db.insert(DeviceNew { user_id, device_id, platform, location: None })
    .on_conflict((Device::user_id, Device::device_id)) // composite key
    .set(Device::platform)            // platform = excluded.platform (overwrite)
    .set_coalesce(Device::location)   // location = coalesce(excluded.location, devices.location)
    .exec().await?;
```

- `.set(col)` → `col = excluded.col` (take the incoming value).
- `.set_coalesce(col)` → `col = coalesce(excluded.col, <table>.col)` — take the incoming
  value, **but keep the stored one when the incoming value is `NULL`**. Ideal for
  best-effort fields (an async-resolved geolocation, a late-arriving attribute) that a
  later write with no value must not erase.
- `.do_update_all()` — overwrite every non-key column (no need to list them).
- `.do_update_all_except(Device::created_at)` — overwrite all but the given column(s).
- `.do_nothing()` — keep the existing row.

This collapses the classic "SELECT, then UPDATE-or-INSERT, plus a unique index and
hand-rolled race handling" into **one atomic statement**. Generated SQL:

```sql
-- Postgres / SQLite / Turso
ON CONFLICT ("user_id", "device_id") DO UPDATE SET
  "platform" = excluded."platform",
  "location" = coalesce(excluded."location", "devices"."location")

-- MySQL (keys implicitly on a unique index; VALUES() = incoming, bare col = stored)
ON DUPLICATE KEY UPDATE
  `platform` = values(`platform`),
  `location` = coalesce(values(`location`), `location`)
```

The conflict key must be backed by a unique/primary constraint: named explicitly in
`ON CONFLICT (...)` on Postgres/`SQLite`/Turso, matched implicitly by `MySQL`. Verified
e2e on real Postgres and SQLite (the "remember this device" login scenario).

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

## Nullable columns

`Option<T>` is the nullable mapping for any supported `T` — `NULL` ⇄ `None`,
automatically, everywhere:

```rust
#[derive(Table)]
#[table(name = "users")]
struct User {
    #[column(pk)]
    id: i64,
    #[column(nullable)]
    bio: Option<String>, // nullability is also inferred from Option<_>
}

// select reads Some/None; insert binds None as SQL NULL
let u = db.get::<User>(1).one().await?;          // u.bio: Option<String>
// filter both sides:
let with_bio = db.find::<User>().filter(eq(User::bio, "hi")).all().await?;   // Some
let no_bio   = db.find::<User>().filter(is_null(User::bio)).all().await?;    // None
```

## Custom column types & extensions (pgvector, PostGIS, …)

Add a brand-new column type by mapping it to an existing `Value` variant
(`Text` / `Bytes` / `I64` / `F64` / `Bool` / `Uuid` / `Timestamptz` / `Date`). Three
small impls — the last is only needed if you filter on the column:

```rust
use stakit_orm::{ToValue, FromValue, Value, ValueKind, Error, Result};
use stakit_orm::expr::{IntoExpr, Operand};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Color { Red, Green }

// (1) bind — how the value goes INTO the database
impl ToValue for Color {
    fn to_value(self) -> Value {
        Value::Text(match self { Self::Red => "red", Self::Green => "green" }.to_owned())
    }
}

// (2) decode — how it comes BACK; KIND picks which cell shape to read
impl FromValue for Color {
    const KIND: ValueKind = ValueKind::Text;
    fn from_value(v: Value) -> Result<Self> {
        match v {
            Value::Text(s) if s == "red"   => Ok(Self::Red),
            Value::Text(s) if s == "green" => Ok(Self::Green),
            other => Err(Error::Decode(format!("bad Color: {other:?}").into())),
        }
    }
}

// (3) filter — lets eq()/ne()/gt()/… accept a Color (optional)
impl IntoExpr<Color> for Color {
    fn into_operand(self) -> Operand { Operand::Value(self.to_value()) }
}

#[derive(Table)]
#[table(name = "items")]
struct Item {
    #[column(pk)]
    id: i64,
    #[column(sql_type = "text")] // the SQL column type for migrations
    color: Color,
    #[column(nullable)]
    note: Option<String>,
}
```

Now `db.insert`, `db.find::<Item>()`, `db.get`, `#[derive(Row)]`, and
`eq(Item::color, Color::Red)` all work — and `Option<Color>` works automatically (the
blanket `Option<T>` impl). Rule of thumb: impl **(1)+(2)** to store/read it, add **(3)**
to compare against it in `WHERE`.

Geospatial points are already first-class — see [Geospatial](#geospatial--geopoint-works-with-or-without-postgis)
below for the built-in `GeoPoint` type (no custom impls needed). For other PostGIS
geometries (polygons, lines), use the `Geometry`/`Geography` newtypes the same way, or
drop to `db.raw(...)`/`sql_expr` for arbitrary `ST_*` calls.

## Vector search (pgvector / Turso / sqlite-vec)

First-class. Store embeddings in a `Vector` column; `nearest()` renders the right SQL
per backend (`<->`/`<=>`/`<#>` on pgvector, `vector_distance_*` on Turso,
`vec_distance_*` on `sqlite-vec`), and `vector::distance(..)` is a **selectable**
projection that returns the score.

```rust
use stakit_orm::prelude::*;          // Vector, Distance, distance

#[derive(Table)]
#[table(name = "docs")]
struct Doc {
    #[column(pk)] id: i64,
    #[column(sql_type = "vector(3)")] embedding: Vector,   // pg: vector(N); turso: blob; sqlite-vec: vec0
}

let q = [0.1_f32, 0.2, 0.3];
// top-5 nearest by cosine
let hits = db.find::<Doc>().nearest(Doc::embedding, &q, Distance::Cosine).limit(5).all().await?;
// …with the score: Vec<(i64, f64)>
let scored = db.select((Doc::id, distance(Doc::embedding, &q, Distance::Cosine)))
    .from::<Doc>()
    .nearest(Doc::embedding, &q, Distance::Cosine)
    .limit(5)
    .all().await?;
```

`Vector` binds correctly on insert too (pg `$1::vector`, Turso `vector32($1)`). Setup
and caveats per backend:

| Backend     | Column DDL              | ANN index (you create it)                              | Notes |
|-------------|-------------------------|--------------------------------------------------------|-------|
| Postgres    | `vector(N)` (pgvector)  | `CREATE INDEX … USING hnsw (embedding vector_cosine_ops)` | reading the column back needs `embedding::text`; metric must match the index opclass |
| Turso/libSQL| `blob`                  | `libsql_vector_idx(embedding)`                          | works in-process; round-trips as LE-f32 blob |
| sqlite-vec  | `vec0` virtual table    | built into `vec0`                                      | needs the `sqlite-vec` loadable extension |

Without an ANN index `nearest()` is an exact full scan. `Distance` is L2/Cosine/Inner­Product;
for any other metric use `sql_expr`/`raw`. (Verified e2e on Turso; pgvector/sqlite-vec
need their extensions, which aren't bundled.)

## Geospatial — `GeoPoint` (works with or without PostGIS)

`GeoPoint` is a built-in lat/lng column type. It stores as plain **WKT text on every
backend** — so it works as an ordinary column with **zero extensions** — and *also*
binds as a native `geometry` on Postgres when PostGIS is installed (the bind gets a
`::geometry` cast, wrapped in `ST_SetSRID(.., srid)` when a SRID is attached). Same
type, same code, both modes.

```rust
use stakit_orm::prelude::*;     // GeoPoint, DistanceUnit, st_dwithin, …

#[derive(Table)]
#[table(name = "places")]
struct Place {
    #[column(pk)] id: i64,
    // No PostGIS? use `text`. With PostGIS, use `geometry(Point,4326)` (+ a GiST index).
    #[column(sql_type = "text")]
    location: GeoPoint,
}

let here = GeoPoint::new(48.8566, 2.3522);          // (lat, lng)
db.insert(PlaceNew { id: 1, location: here }).exec().await?;   // binds "POINT(2.3522 48.8566)"
let p = db.get::<Place>(1).one().await?;            // reads WKT text back into GeoPoint
let same = db.find::<Place>().filter(eq(Place::location, here)).all().await?;
```

**Constructors & accessors** (note WKT is `POINT(lng lat)` — lng first — but
`new` takes the conventional `lat, lng`):

```rust
GeoPoint::new(lat, lng);              // conventional order
GeoPoint::from_lng_lat(lng, lat);     // GeoJSON / WKT order
GeoPoint::with_srid(lat, lng, 4326);  // tag a SRID (survives the PostGIS bind)
GeoPoint::try_new(lat, lng)?;         // validated: lat ∈ [-90,90], lng ∈ [-180,180]
p.lat(); p.lng(); p.as_lat_lng(); p.as_lng_lat();
p.wkt();   // "POINT(lng lat)"        p.ewkt();  // "SRID=4326;POINT(lng lat)"
```

**Conversions** — GeoJSON and degrees-minutes-seconds, both round-trip:

```rust
let gj = p.to_geojson();              // {"type":"Point","coordinates":[lng,lat]}
let p2 = GeoPoint::from_geojson(&gj)?;
let (lat_dms, lng_dms) = p.to_dms();  // Dms { degrees, minutes, seconds, hemisphere }
let p3 = GeoPoint::from_dms(lat_dms, lng_dms);
```

**Geodesy on the WGS-84 sphere** (no DB round-trip, no extension — pure Rust):

```rust
use stakit_orm::DistanceUnit::{Kilometers, Meters, Miles, NauticalMiles};

let km   = paris.distance(&london, Kilometers);   // also haversine_meters(&other)
let brg  = paris.bearing(&london);                // initial bearing, degrees
let dest = paris.destination(90.0, 10.0, Kilometers); // 10 km due east
let mid  = paris.midpoint(&london);
let near = paris.within(&london, 500.0, Kilometers);  // bool
let (sw, ne) = paris.bounding_box(5.0, Kilometers);   // corners for a coarse pre-filter
```

`DistanceUnit` is `Meters | Kilometers | Miles | NauticalMiles` with
`from_meters` / `to_meters` converters.

### PostGIS spatial queries (Postgres + PostGIS only)

When PostGIS *is* installed, typed predicates render the `ST_*` functions and the
`<->` KNN operator — geometry is always parameter-bound (never interpolated):

```rust
use stakit_orm::{st_dwithin, st_intersects, st_contains, st_within, st_distance};

// rows within 1 km of `here`
let nearby = db.find::<Place>()
    .filter(st_dwithin(Place::location, here, 1_000.0))   // ST_DWithin(col, $1::geometry, $2)
    .all().await?;

// distance as a selectable projection, KNN-ordered nearest-first
let ranked = db.select((Place::id, st_distance(Place::location, here)))
    .from::<Place>()
    .nearest_geo(Place::location, here)                   // ORDER BY col <-> $1
    .limit(10)
    .all().await?;
```

Also `st_intersects` / `st_contains` / `st_within` for polygons (pass a `Geometry`
WKT). These emit PostGIS SQL, so they require the extension; the `GeoPoint` type,
storage, and all the geodesy above need **nothing**. (PostGIS isn't bundled with the
embedded test Postgres, so the spatial-query SQL is render-tested; plain-text
`GeoPoint` round-trips are verified e2e on SQLite and Postgres.)

## Full-text search (Postgres / SQLite FTS5 / Turso FTS5)

`matches(col, query)` renders `to_tsvector @@ plainto_tsquery` on Postgres and FTS5
`MATCH` on SQLite/Turso (create the table `USING fts5(...)` there). The query text is
always a bound parameter.

```rust
let hits = db.find::<Article>().filter(matches(Article::body, "systems")).all().await?;
```

Relevance ranking (`ts_rank` / `bm25`) is not yet a typed projection — order by it via
`raw_pred`/`db.raw` for now. `MySQL` full-text (`MATCH … AGAINST`) is not supported.

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
