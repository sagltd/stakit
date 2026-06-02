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
column tokens + `smallvec` metadata buffers). The final SQL `String` and the
`PgArguments` bind buffer **are heap allocations** — "zero-alloc" applies only to
the column/predicate metadata buffers, never to the produced SQL/args (see §15
for the precise allocation budget). Row *decode* into owned structs copies (sqlx
`PgRow` owns its buffer). **v1 exposes no borrowed/zero-copy row API** — all
terminals return owned values; a borrowing API is out of scope (not claimed,
not benchmarked). sqlx uses the extended query protocol with **binary** result
format by default, which we rely on for fast decode (no text parsing of
ints/uuids/timestamps) — see §16. Accepted tradeoff vs. hand-rolling the pg wire
protocol.

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
- **Column tokens**: associated **consts** in an inherent `impl User` —
  `User::id: Col<User, Uuid>`, `User::email: Col<User, String>` — typed, ZST.
  These live in the path namespace, so they do **not** collide with the struct's
  fields (`instance.id` is field access; `User::id` is the const). The const
  names are lowercase (mirroring columns), which trips clippy's
  `non_upper_case_globals` under our `-D warnings` gate, so the generated `impl`
  carries `#[allow(non_upper_case_globals)]`. (Alternative considered: a
  `user::id` lowercase module of consts — rejected as more verbose; the
  `#[allow]` on generated code is contained and conventional.)
- **Relation tokens**: `User::posts: Relation<User, Post, Many>`.
- sqlx `FromRow` (delegated).

### Compile-time checks (what rustc verifies)

The cross-struct checks work because the derive emits **paths/type-level code**
that rustc resolves *after* expansion — not because the macro reads another
struct's fields (it cannot). Type-equality is enforced with a **`PhantomData`
witness function** (a `const fn assert_same<T>(_: PhantomData<T>, _: PhantomData<T>)`
called with both types), not a value-level `const assert!` (which cannot compare
types).

| Check | When | Mechanism |
|---|---|---|
| `User`/`Post` type exists | compile | path resolution after macro expand |
| `User::id` column exists | compile | `User::id` is a real associated const |
| FK target column exists (`references = User::id`) | compile | path resolves |
| FK type == referenced PK type | compile | `assert_same(PhantomData::<author_id Ty>, PhantomData::<<User as Table>::PkTy>)` |
| `has_many(Post, fk = author_id)` — `Post` + `Post::author_id` exist, fk type matches User PK | compile | emitted code references `Post::author_id` + `assert_same` witness |
| `on_delete = "set null"` requires the FK column be nullable (`Option<_>`) | compile (macro) | macro errors if `set null` on a non-`Option` column |
| `on_delete = "cascade"` is a valid keyword | compile (macro) | match against allowed set (`cascade`/`restrict`/`set null`/`no action`) |
| FK target is actually pk/unique; DB has the constraint | migration gen / apply | snapshot diff / runtime |

`#[column(default = "...")]` and `#[table(name = "...")]`/`#[column(name = "...")]`
take **string literals only** — they are trusted developer input emitted verbatim
into DDL/identifiers, never accept a runtime `String`. Defaults are emitted into
`create table ... default <literal>` as-is (see §5 + §17 for the safety
boundary). Semantic/DB-truth checks that a proc-macro cannot see from another
struct's site are deferred to migration generation, not the build.

## 5. Migrations — generated, sqlx-applied

Schema lives in a parseable file (default `src/schema.rs`). The CLI generates SQL
**without compiling or running the app**.

### Generation approach (Option 3 — syn parse)

**Two-pass resolver** (a path like `references = User::id` cannot be resolved in
one pass):

1. **Pass 1 — collect:** `syn`-parse the schema fileset → a symbol table of every
   `#[derive(Table)]` struct, its `#[table(name)]`, and each column's
   field-name → `#[column(name)]` mapping.
2. **Pass 2 — resolve:** resolve every `references = User::id` / `has_many(Post,
   fk = author_id)` path against the symbol table to a concrete `(table, column)`,
   honoring `#[column(name = "...")]` renames. The last path segment that is not a
   known table/column → hard error (no guessing).
3. Read `migrations/.snapshot.json` → last-known schema (empty on first run).
4. Diff → changes (new table, add column, FK change; alter/drop in v2, see below).
5. Write sqlx-format files into `migrations/` (`<ts>_<name>.up.sql` +
   `.down.sql`), then update `migrations/.snapshot.json`.

**Rust type → SQL type is by canonical spelling, and is fragile by nature** — syn
sees only the token text (`Uuid`, `DateTime<Utc>`, `Option<String>`), with no type
resolution. Rules:

- A fixed, **documented allowlist** of canonical spellings maps to SQL types
  (`Uuid → uuid`, `DateTime<Utc> → timestamptz`, `String → text`, …).
- `Vec<u8>` is **special-cased to `bytea` before** the generic `Vec<T> → T[]` rule
  (they otherwise overlap).
- **Type aliases, re-exports, and fully-qualified spellings are unsupported** —
  `type Id = Uuid` or `uuid::Uuid` will not be recognized.
- Any unknown spelling is a **hard error**, never a silent guess. The escape
  hatch is `#[column(sql_type = "...")]` to state the SQL type explicitly (and is
  required for custom/feature-gated types like `Decimal`).

Rationale: no compile, no DB, no app boot; reuses `syn` (already a workspace
dep). Constraint: schema must live in known parseable file(s) in the configured
fileset, not be generated behind other macros. Accepted.

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

### Migration safety + correctness

- **Concurrent boot:** `Migrator::run` takes a Postgres **advisory lock** by
  default, serializing concurrent `run()` across instances. We rely on it and
  never disable locking. Caveat: advisory locks break under PgBouncer
  *transaction* pooling — migrations must run on a direct (session) connection,
  documented for operators.
- **Snapshot integrity:** `.snapshot.json` is the diff source of truth and is a
  repo file (merge-conflict-prone). It MUST be committed atomically with its
  generated migration. `cargo stakit-orm status` cross-checks the snapshot's
  table/migration count against `_sqlx_migrations`; divergence is a hard error,
  not a silent wrong diff.
- **Destructive `down`:** generated `down.sql` for additive diffs contains
  `drop table`/`drop column` — data-destructive. `cargo stakit-orm down` requires
  `--force` (or an interactive confirm) when the down step contains any `drop`.
  The §17 "review before apply" gate covers `gen`; this covers revert.

Diff scope phasing: v1 = create table + add column + new table; alter-type / drop
detection in a later pass (flagged to user, hand-edit allowed meanwhile).

## 6. Query API 1 — SQL builder (Drizzle `db.select()`)

Free-function operators (exactly Drizzle): `eq`, `and`, `or`, `gt`, `lt`, `gte`,
`lte`, `ne`, `like`, `in_`, `is_null`, `asc`, `desc`. `where` is a Rust reserved
keyword → the method is **`.filter()`**.

```rust
use stakit_orm::prelude::*;
use stakit_orm::expr::{eq, and, gt, desc, count, any_of};

let db = Db::new(pool);   // thin wrap over sqlx PgPool

// whole rows, many -> Vec<User>  (inferred, no annotation)
// NOTE: every comparison is type-matched. `created_at` is DateTime<Utc>, so it is
// compared to a DateTime value, never an integer literal.
let users = db.select(User::all())
    .from::<User>()
    .filter(and(eq(User::name, "Dan"), gt(User::created_at, since)))  // since: DateTime<Utc>
    .order_by(desc(User::created_at))
    .limit(10).offset(20)
    .all().await?;

// one whole row -> Option<User>   (uid: Uuid, matches User::id's Uuid)
let u = db.select(User::all()).from::<User>().filter(eq(User::id, uid)).one().await?;

// join -> output type inferred: inner => (Post, Comment), left => (Post, Option<Comment>)
let rows = db.select((Post::all(), Comment::all()))
    .from::<Post>()
    .left_join::<Comment>(eq(Post::id, Comment::post_id))   // col=col, both Uuid
    .filter(eq(Post::id, pid))                              // pid: Uuid
    .all().await?;

// IN list -> rendered as `= ANY($1)` with ONE array bind (see note below)
let some = db.select(User::all()).from::<User>()
    .filter(any_of(User::id, &ids))                         // ids: &[Uuid]
    .all().await?;
```

`eq` (and friends) is generic via an `IntoExpr<Ty>` bound: `fn eq<L: ColExpr, R:
IntoExpr<L::Ty>>(l: L, r: R)`. This makes `eq(Post::id, Comment::post_id)`
(col=col, both `Ty = Uuid`) and `eq(Post::id, pid)` (col=value, `pid: Uuid`) both
type-check, while `eq(User::id, 5)` or `eq(User::id, "str")` **fail to compile**
(`i32`/`&str` do not satisfy `IntoExpr<Uuid>`). Values are bound as positional
`$N` params (injection-safe).

**IN lists use `= ANY($1)`, never `in ($1,$2,…)`.** A literal IN list makes the
SQL text vary with the element count → a new prepared statement per length →
statement-cache thrash, and it consumes one bind param per element (hitting the
65535 cap). `col = ANY($1::T[])` is a **single** array bind: one stable statement
regardless of length, one param, no cap issue. This rule applies everywhere an IN
list would appear, including relational loading (§8). (Very large arrays should
still be chunked for planner sanity.)

### Terminals

Terminals are split by builder kind — a `Select` builder has no `.exec()`, and
mutation builders have no `.one()/.all()`. `count`/`exists` are **projections**
(§7) consumed by `.one()`, plus the shortcut methods below for the bare case.

**`Select<P>` (query):**

| Method | Returns | Use |
|---|---|---|
| `.all()` | `Vec<P::Output>` | select many |
| `.one()` | `Option<P::Output>` | select one (LIMIT 1) |
| `.one_or_err()` | `P::Output` | one, `Error::NotFound` if absent |
| `.exact_one()` | `P::Output` | err if 0 (`NotFound`) or >1 (`TooManyRows`) |
| `.stream()` | `impl Stream<Item = Result<P::Output>>` | large result, incremental (holds a connection; see §16) |
| `.count()` | `i64` | shortcut: rewrites to `select count(*)` (ignores `P`) |
| `.exists()` | `bool` | shortcut: rewrites to `select exists(...)` (ignores `P`) |

**`Insert`/`Update`/`Delete` (mutation):**

| Method | Returns | Use |
|---|---|---|
| `.exec()` | `u64` | rows affected |
| `.returning(proj).all()` | `Vec<Output>` | `RETURNING` many |
| `.returning(proj).one()` | `Output` | `RETURNING` from a known-single row (non-`Option`; row just written) |

## 7. Select projections — the `Projection` trait

`select()` takes anything implementing `Projection`. The argument decides the
return type — **no annotation needed**, inference flows from `Projection::Output`
through the terminal (`.all()` → `Vec<Output>`, etc.).

```rust
pub trait Projection {
    type Output;
    /// SQL select-list fragments, in order.
    fn columns(&self) -> SmallVec<[SqlExpr; 8]>;
    /// Decode one row. Column ordinals are 0..N in `columns()` order, so this
    /// reads positionally (`row.try_get(0)`, …) — ordinals are fixed at build
    /// time, NOT looked up by name per row.
    fn decode(row: &PgRow) -> Result<Self::Output>;
}
```

The trait carries a **`decode`** method (the earlier draft omitted it — without it
the terminal cannot turn a `PgRow` into `Output`). Three decode strategies are
provided by **distinct, non-overlapping wrapper types** (no coherence conflict):

| `select(...)` argument | wrapper type | `Output` | decode |
|---|---|---|---|
| `User::id` (single `Col`) | `Col<User, Uuid>` | `Uuid` | `try_get(0)` |
| `(User::id, User::name)` | tuple impl (macro, arity 1..=12) | `(Uuid, String)` | positional |
| `count()` / `count(Post::id)` | `Count` | `i64` | `try_get(0)` |
| `sum(Col<_,T>)` / `avg(_)` | `Sum<T>` / `Avg` | `T` / `f64` | `try_get(0)` |
| `User::all()` | `All<User>` | `User` | `User::from_row` (sqlx `FromRow`) |
| `UserStat::project()` (derive Row) | `RowProj<UserStat>` | `UserStat` | derived |
| `row! { .. }` | macro-generated local | anonymous named struct | per-field, see below |

Tuple impls are generated for arities 1..=12; `(User::id,)` and bare `User::id`
decode identically. Each wrapper is a concrete newtype, so the blanket impls
(`Col`, `Count`, `Sum<T>`, `All<T>`, the tuple macro) cannot overlap.

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

**`row!` mechanics.** The proc-macro runs pre-typecheck, so it cannot *name* a
plain column's type (e.g. that `User::email` is `String`). The earlier "generic
struct + `r.get(0)`" sketch does **not** work: `r.get(N)` is generic in its target
type and a bare generic struct field provides nothing to anchor inference →
"type annotations needed". The fix is to drive each field's type from the
**column token's own associated type** (`Col<T, Ty>::Ty`), not from `r.get`:

```rust
{
    // Each field's type comes from its expr's associated Output type, which the
    // EXPR carries (the macro doesn't need to spell it). sql! fields carry an
    // explicit type (1st arg). Result is a local struct, fully concrete.
    struct Row<E, U, P, Y> { email: E, user_id: U, post_count: P, year: Y }
    RowProjection::build(
        // (sql fragment, typed extractor) per field; extractor type fixes the generic
        field(User::email,        |r, i| r.try_get(i)),   // E = <Col as Expr>::Out = String
        field(User::id,           |r, i| r.try_get(i)),   // U = Uuid
        field(count(Post::id),    |r, i| r.try_get(i)),   // P = i64
        field_sql::<i32>("extract(year from {})", Post::created_at),  // Y = i32 (explicit)
    )
}
// Output = Row<String, Uuid, i64, i32>
```

`field(expr, extractor)` ties the extractor's return type to `<expr as Expr>::Out`
via a bound, so the local struct's generics resolve from the **expression types**,
which are known to the type system even though the macro can't write them. `sql!`
fields supply the type explicitly. Ordinals are assigned in declaration order.

**`sql!` rules** (raw SQL fragment, typed):

- 1st arg = output Rust type → field gets that concrete type.
- `{}` placeholder = **column token** (`Col`) → rendered as a properly quoted
  identifier (`"posts"."created_at"`; embedded `"` doubled, see §17);
  compile-checked (column must exist).
- `?` placeholder = **value bind** → a param collected into the query's single
  ordered bind buffer; `$N` is assigned **once at terminal assembly** across the
  whole statement (select-list, then from/join, then where, …), so `sql!` binds in
  the select list never collide with `filter` binds. Never string-spliced.

Tradeoffs: tuple = fully checked at select, positional. Derive Row (B) = named +
checked at select. `row!` (C) = best ergonomics; plain-column field types are
resolved from the column token (so they ARE type-driven, but the field-name↔type
binding is verified at build of the local struct rather than against a declared
struct), and the local struct can't cross a function boundary (fine for
query-and-consume).

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
level using **`... where fk = ANY($1)`** (the array-bind rule from §6 — stable
statement, no 65535 cap on parent-key count), stitched in Rust via `hashbrown`
map (Drizzle's default behavior). **Latency = sum of round-trips** (one per
relation level), issued sequentially; not pipelined in v2.

**The hard part (honest):** API 1 maps cleanly to Rust. API 2's typed result needs
codegen — Drizzle leans on TS structural typing, Rust has none. Result-type
strategy for **v2**:

- The `#[has_many]`/`#[belongs_to]` derive generates **one fixed `UserWith` type
  per table** with a field for **every** declared relation (`Vec<Post>` for
  has-many, `Option<Author>` for belongs-to), populated only for relations named
  in `.with(...)` and empty otherwise. This avoids the combinatorial explosion of
  one type per `.with()` combination.
- **v2 restrictions (stated, not hidden):** `.with()` is **one level deep**;
  nested `.with(rel, |c| c.with(...))` and combining `.columns(...)` *with*
  `.with(...)` are **v3**. The nested/`columns`+`with` examples above are the v3
  target API, shown for direction.

Phasing: **v2** = relational, one fixed `UserWith`, one level of `.with()`,
`= ANY($1)` batched load. **v3** = nested `.with()`, `columns`+`with` combined,
CTEs, subqueries, broader aggregates, optional concurrent per-subtree loading.

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

`db.raw` takes `&str` (not `&'static str` — that adds no safety: `Box::leak` of a
runtime string is `&'static`, and it would block legitimate dynamically-built
SQL). **The raw path and `db.pool()` are an unaudited surface, by design:** the
SQL text is the caller's responsibility; values must still go through `.bind()`/
`$N`, never string interpolation. The builder APIs (§6–§8) are the safe default;
`raw` is the explicit opt-out. This honesty is restated in §17.

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
| 2 – ~tens of thousands | `insert_many` | multi-row `INSERT ... VALUES (...),…`, one round-trip **per chunk** |
| huge (10k+) | `copy_into` | `COPY ... FROM STDIN` via sqlx `PgCopyIn` — single stream, no per-row parse/plan/bind |

**Param-limit + round-trips:** Postgres caps bind params at 65535 per statement.
`insert_many` computes `rows_per_stmt = (65535 − reserved) / column_count`, where
`reserved` accounts for non-VALUES binds (e.g. `on_conflict … do_update set x =
$n`). Round-trips = `ceil(rows / rows_per_stmt)` — **not** "one round-trip" for
large inputs (a 1M-row, 10-col insert ≈ 153 serialized statements). Above ~tens
of thousands, use `copy_into`.

**Statement-cache cardinality:** a varying final-chunk size would mint a new
prepared statement per distinct row count. `insert_many` **buckets** chunk sizes
to powers of two (padding the last chunk's statement shape, not the data) so the
number of distinct insert statements stays small and the cache doesn't thrash.

**Transaction tradeoff:** by default all chunks run in one transaction
(all-or-nothing, no partial writes). For very large loads this is one long
transaction holding locks and growing WAL — so `insert_many` accepts a
`.commit_per_chunk()` opt-out for bulk loads that don't need global atomicity, and
the docs steer large loads to `copy_into`.

**`copy_into` is single-stream** (one logical round-trip). On any error mid-stream
it calls `PgCopyIn::abort` before returning, so the connection is never returned
to the pool stuck in COPY protocol state. **Binary-format caveat:** `COPY … (FORMAT
BINARY)` requires a binary encoder for every column type; types in §16 without a
binary COPY encoder fall back to text COPY (still fast, less than binary). The
"≥5× `insert_many`" figure (§15) is a *measured* result, not a guaranteed target —
the multiple depends on row width and network.

Recommended defaults: `insert_many` for normal app writes; `copy_into` for
seed/import/ETL.

## 11. Transactions

Real pg transactions via sqlx. The **closure form is the supported path** and
performs an explicit, awaited `ROLLBACK` in its error/cancel arm. The manual form
relies on sqlx's drop behavior, which is **best-effort, not a guarantee** (see
caveat below).

**Executor abstraction:** we define our **own** `Executor` trait (not sqlx's
directly), with `async fn run(&mut self, …)`, implemented for `Db` (pool) and
`Tx`. This is required because sqlx's `Executor::fetch*` consumes `self` by value;
for `&mut Transaction` each call must reborrow (`&mut *tx`). Our trait takes
`&mut self` and reborrows internally, so the same `select/insert/update/...` API
works on both a pool and a transaction, and sequential `tx.insert(..); tx.insert(..)`
calls compile. The closure-transaction signature uses the standard HRTB form
(`for<'c> FnOnce(&'c mut Tx) -> impl Future + 'c`).

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
transactions map to pg `SAVEPOINT` / `ROLLBACK TO` via `tx.transaction(|sp| ...)`;
**savepoint names are internally generated counters** (`_sp_1`, …), never derived
from any input string.

### Drop-rollback caveat (do not over-trust)

sqlx cannot issue an awaited `ROLLBACK` from `Drop` (Drop is sync). It marks the
connection so rollback happens when the connection is next used / returned to the
pool — **best-effort**: under runtime shutdown, task abort, or a cancelled
in-flight query on the transaction's connection, the rollback may not run before
reuse, and a cancelled mid-query connection may need to be **closed** rather than
rolled back. Therefore:

- Prefer the **closure form** (`db.transaction(|tx| …)`) — it rolls back
  explicitly with `.await` on the error/cancel arm.
- The manual `begin()` + drop form is best-effort; callers SHOULD `commit()` or
  `rollback()` explicitly rather than relying on drop.
- Per-query timeouts (§16) that drop an in-flight tx query may force the
  connection closed; this is acceptable (correctness over connection reuse) and
  documented.

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

**Value-leak boundary:** the mapper populates `constraint`/`column` **only** from
the pg error's `constraint()`/`column()`/`code()` accessors — never from
`message()`/`detail()`, because Postgres's unique-violation `DETAIL` embeds the
offending value (`Key (email)=(a@b.com) already exists`). Caveat: the
`#[error(transparent)]` `Database`/`Decode` fallback wraps the raw sqlx error,
whose `Display` *can* include that pg message (values included). So
`Error::Display` MUST NOT be echoed to untrusted clients; log it server-side only.
This is restated in §17.

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

## 14. Testing strategy

**Postgres has no in-memory mode** (unlike SQLite `:memory:`). Two layers:

### Layer 1 — pure unit tests, no DB (primary)

The builder, `Projection`, migration generator, and error mapper are pure
functions: `input → SQL string` and `sqlx::Error → Error`. Assert the generated
SQL + bind params. No DB, no infra, runs in the default `cargo nextest` loop and
`code-check.sh` everywhere. Covers the bulk of correctness.

```rust
#[test]
fn select_builds_sql() {
    let (sql, _binds) = db.select((User::id, User::email))
        .from::<User>().filter(eq(User::id, uid)).to_sql();
    assert_eq!(sql,
        r#"select "users"."id", "users"."email" from "users" where "users"."id" = $1"#);
}
```

### Layer 2 — integration tests against a real embedded pg

Integration tests live in `crates/stakit-orm/tests/` and **always run against a
real Postgres** via `postgresql_embedded` — no gating, no `#[ignore]`, part of the
normal `cargo nextest run --workspace`. Decode, FK cascade, `COPY`, transaction
rollback, and upsert cannot be faked; they must hit real pg.

**SQLite in-memory is rejected**: dialect differs hard (`uuid`,
`gen_random_uuid()`, `timestamptz`, `on conflict`, `COPY BINARY`) — it would test
the wrong SQL.

**Chosen: `postgresql_embedded`** (theseus-rs) — downloads + runs a real pg binary
in a temp dir, no Docker, ephemeral per run. Rationale (crates.io, 2026-06):

| Crate | Latest | Recent dl | Docker | Note |
|---|---|---|---|---|
| **postgresql_embedded** | 0.20.2 | ~971k | no | real pg binary on demand, `forbid(unsafe_code)` (matches workspace lint) |
| pg-embed | 1.0.0 | ~25k | no | ~40× less used |
| testcontainers (+modules) | 0.27.3 | ~9.3M | yes | industry standard, but needs Docker |

`postgresql_embedded` wins: most-used no-Docker option, maintained, real-dialect,
`forbid(unsafe_code)` matches our workspace. (`testcontainers` is an alternative
where Docker is acceptable, but the default here is no-Docker embedded.)

**Performance + isolation:** booting a pg server per test is too slow (seconds
each). Boot **one embedded server per test binary**, shared via a `OnceCell`/
`static`, then isolate each test. A `tests/common/mod.rs` harness exposes the
setup:

```rust
// tests/common/mod.rs — shared embedded server, per-test isolated database
static PG: OnceCell<PostgreSQL> = OnceCell::const_new();

/// Boot the embedded server once, then hand each test its own fresh database
/// (migrations applied) so tests are fully isolated and parallel-safe.
pub async fn test_db() -> Db {
    let pg = PG.get_or_init(|| async { /* PostgreSQL::default().setup().start() */ }).await;
    let name = unique_db_name();          // per-test, unique
    pg.create_database(&name).await;
    let pool = PgPool::connect(&pg.url(&name)).await.unwrap();
    MIGRATOR.run(&pool).await.unwrap();   // apply generated migrations
    Db::new(pool)
}
```

```rust
// tests/select.rs
mod common;

#[tokio::test]
async fn select_one_returns_row() {
    let db = common::test_db().await;
    let id: Uuid = db.insert(User { .. }).returning(User::id).one().await.unwrap();
    let got = db.select(User::all()).from::<User>()
        .filter(eq(User::id, id)).one().await.unwrap();
    assert_eq!(got.unwrap().id, id);
}
```

Isolation options (decide at impl): **per-test fresh database** (clean, fully
parallel) or **per-test transaction rolled back at end** (faster, no create/drop).
Default to per-test database for clarity; revisit if boot/create cost hurts.

First run downloads the pg binary (cached after). Pin newest versions at
implementation time (workspace rule: check crates.io).

## 15. Performance — measured with divan

Performance is a first-class requirement and **must be measured, not asserted**.
The workspace already uses [`divan`](https://crates.io/crates/divan) (in
`[workspace.dependencies]`); each crate gets a `benches/` dir. Benchmarks are part
of the deliverable, not an afterthought — a change that regresses a tracked metric
is a bug.

### What is benchmarked (and why each matters)

The ORM's overhead is **everything between the user's call and sqlx's
`fetch`/`execute`**. That overhead must be near-zero versus hand-written sqlx.

**Allocation budget, stated precisely.** Building a query is **not** zero-alloc —
the SQL `String` and the `PgArguments` bind buffer are necessarily heap. "Zero
alloc" applies only to the **metadata buffers** (columns/predicates) when they fit
the smallvec inline capacity. So the honest budget for a typical select build is
**1 String + 1 args alloc + 0 metadata allocs**. Benches assert exactly that, not
a false "0 allocations".

| Bench (pure, no DB) | Measures | Target |
|---|---|---|
| `bench_select_build` | build SQL+args, typical select (3 cols, 2 preds, 1 order) | < 200 ns; **1 String + 1 args alloc; 0 metadata allocs** |
| `bench_select_build_wide` | `All<User>` on a **12+ column** table (exceeds inline cap) | metadata buffer **spills** — measured + accepted, not claimed zero |
| `bench_select_build_join` | 2-table join | < 400 ns; metadata allocs only if cols > inline cap |
| `bench_projection_tuple` | `Projection::columns()` for a 3-tuple, **derive-generated** types | 0 metadata alloc |
| `bench_bind_encode` | encode N typed values → `PgArguments` | report allocs honestly (args buffer grows) |
| `bench_predicate_compose` | `and(eq, or(gt, lt))` tree build | 0 metadata alloc (within inline cap) |
| `bench_builder_step_alloc` | each chained `.filter()/.order_by()` does **0 allocs** until terminal (guards invariant 1) | 0 alloc per step |
| `bench_insert_chunk_calc` | `insert_many` chunk+bucket planning, 10k rows | < 1 µs; 0 alloc beyond the chunk vec |
| `bench_projection_row_macro` | `row!`-generated decode path vs `#[derive(Row)]` path | **0 delta** (shared impl by construction) |
| `bench_error_map` | `sqlx::Error` → `Error` SQLSTATE mapping | < 100 ns |
| `size_of` checks | `size_of::<SqlExpr>()`, builder struct size, inline smallvec footprint | recorded; guards stack-frame bloat |

| Bench (integration, embedded pg, CI) | Measures | Target |
|---|---|---|
| `bench_decode_row` | decode N rows into `User` via sqlx `FromRow` vs raw `query_as` | informational, ±20% (CI noise floor) |
| `bench_decode_row_macro` | `row!` decode (precomputed ordinals) vs derive | informational, ±20% |
| `bench_relation_stitch` | `with()` hashbrown stitch, N parents × M children: map build + assign, allocs, peak mem | informational; bounded-memory asserted |
| `bench_with_roundtrips` | 1-level `.with()` vs manual join vs N+1 | `.with()` ≪ N+1 |
| `bench_statement_cache_hitrate` | re-parse rate across varying IN/`insert_many` sizes | **near-100% hit** (guards the `= ANY($1)` + bucketing rules) |
| `bench_stream_memory` | peak RSS over a 1M-row `.stream()` | bounded (does not buffer full set) |
| `bench_insert_throughput` | rows/sec: `insert` vs `insert_many` vs `copy_into` at 1/1k/100k | informational; `copy_into` measured multiple reported, not gated |

Pure benches run in the default loop and carry **strict** ns/alloc gates
(reproducible). Integration benches run against the embedded pg from §14 and are
**informational with wide tolerance** — an embedded pg on a contended CI runner
has variance far above a 3% delta, so tight numeric gates there would be flaky.

### Methodology

- Measure **allocations** via **`divan`'s built-in alloc profiling** — *not* a
  custom `GlobalAlloc` (implementing `GlobalAlloc` needs `unsafe`, which the
  workspace forbids; see §16/§17). A "0 allocations" claim that isn't asserted is
  treated as false.
- `std::hint::black_box` all inputs/outputs to defeat const-folding.
- Benches exercise **derive-generated** `Table`/`Row`/`row!` types (not
  hand-rolled stand-ins) so they protect the real codegen; a `cargo expand`
  snapshot test guards against codegen-quality regressions.
- Compile-time cost of macros is tracked separately via `cargo build --timings`
  (divan cannot measure compile time) — the "expansion cost at runtime" idea is
  dropped as incoherent.
- Track regressions: a tracked **pure-bench** metric regressing > 10% is a release
  blocker; integration benches are trend-tracked (median over many runs), not hard
  gates.
- Compare against a **raw-sqlx baseline** in the same bench file so overhead is a
  delta, not a hardware-drifting absolute.

### Performance design invariants (what the benches protect)

1. SQL string built **once** at the terminal, never per builder step
   (`bench_builder_step_alloc`).
2. Metadata buffers are `smallvec`-sized for the common case; **wide projections
   (`All<T>` on >inline-cap columns) spill and that is accepted** — not claimed
   zero (`bench_select_build_wide`). Inline cap chosen against a realistic p95,
   balanced against stack-frame size (`size_of` checks).
3. Column/relation tokens are zero-sized; no per-query allocation for schema info.
4. `insert_many` issues one statement **per chunk**; round-trips grow with size
   (§10) — not "one round-trip" for large inputs.
5. All generated queries go through sqlx's **prepared, extended (binary) protocol**
   path — no simple-protocol fallback. IN lists use `= ANY($1)` and `insert_many`
   buckets chunk sizes so the statement cache stays hot (`bench_statement_cache_hitrate`).
6. No `format!`-per-row in any hot path; SQL assembled into one `String` with
   computed capacity (that String is the 1 unavoidable alloc, per the budget above).

## 16. Production readiness

### Connection pool

`Db::new(pool)` wraps `sqlx::PgPool`. Expose a `DbConfig` builder for production
knobs (all map to sqlx `PoolOptions`):

```rust
let db = Db::connect(DbConfig {
    url,
    max_connections: 20,
    min_connections: 2,
    acquire_timeout: Duration::from_secs(5),
    idle_timeout: Some(Duration::from_secs(600)),
    max_lifetime: Some(Duration::from_secs(1800)),
    statement_cache_capacity: 256,
}).await?;
```

- **Acquire timeout** is mandatory (no unbounded waits → no hung requests).
- Pool is `Clone` + `Send` + `Sync` (sqlx `PgPool` is `Arc` inside); share across
  tasks freely.

### Prepared-statement cache

sqlx caches prepared statements per connection by default. The builder produces a
**stable SQL string for a given query shape** (same shape → same string → cache
hit), so repeated queries skip parse/plan. The `row!`/builder must NOT embed
values into the SQL text (they go to binds) — otherwise cache thrashes. This is
both a perf and a security invariant (§17).

### Timeouts & cancellation

- Every terminal accepts an optional per-query timeout; on elapse the future is
  dropped → sqlx cancels. Default inherits `DbConfig`.
- All futures are cancellation-safe (drop = rollback for an open tx, §11).

### Observability

- `tracing` spans around each query: span fields = operation, table, elapsed,
  rows. SQL text only at `DEBUG`; **bind values never logged** (PII/secrets).
- Optional slow-query log threshold in `DbConfig`.
- Errors carry enough context (table, constraint) without leaking values.

### Type mapping (Rust ↔ Postgres)

Delegated to sqlx's `Type`/`Encode`/`Decode`, fixed at v1:

| Rust | Postgres |
|---|---|
| `i16/i32/i64` | `smallint/int/bigint` |
| `f32/f64` | `real/double precision` |
| `bool` | `boolean` |
| `String`/`&str` | `text` |
| `Vec<u8>` | `bytea` |
| `Uuid` (`uuid` feature) | `uuid` |
| `DateTime<Utc>`/`NaiveDate` (`chrono`) | `timestamptz`/`date` |
| `serde_json::Value` (`json` feature) | `jsonb` |
| `Option<T>` | nullable column |
| `Vec<T>` | `T[]` array |
| `#[column(enum)]` Rust enum | pg enum / text |

Optional types gated behind cargo features (`uuid`, `chrono`, `json`, `decimal`)
so a minimal build stays lean. Newest crate versions pinned at impl (workspace
rule).

### Concurrency & safety

- All public futures `Send` where the executor is `Send` (matches workspace
  `future_not_send` relaxation but the executor path stays `Send`).
- `unsafe` forbidden workspace-wide — no exceptions; zero-copy is via lifetimes /
  smallvec, never raw pointers.
- Derives generate only safe code; no `unsafe` in expansions.

### Graceful failure

- Pool exhaustion → `Error::Database` with acquire-timeout context, not a hang.
- Migration checksum drift (edited applied migration) → hard error at startup via
  sqlx, surfaced clearly.
- Partial `insert_many`/`copy_into` failure → whole operation in one transaction,
  rolls back (no partial writes).

## 17. Security

- **SQL injection:** all user values are bound parameters (`$N`), never
  string-interpolated. Identifiers (table/column names) come only from
  compile-time schema tokens and are quoted (`"users"."id"`) — never from runtime
  strings. `sql!` `{}` accepts only `Col` tokens (compile-checked), `?` only
  binds. The raw escape hatch (`db.raw`) takes a `&'static str` SQL with `.bind()`
  params — no interpolation API is exposed.
- **No value logging:** bind values excluded from spans/logs by default (§16).
- **`unsafe` forbidden** (workspace lint); memory-safety by construction.
- **Least surprise:** no implicit `*` that leaks new columns into typed results —
  projections are explicit; `T::all()` is generated from the known schema.
- **Migration safety:** generated SQL is reviewed before apply; destructive diffs
  (drop column/table) are surfaced, never auto-applied silently (§5, v2).

## 18. Phasing summary

| Version | Scope |
|---|---|
| **v1** | `#[derive(Table)]` + tokens + compile-checks; migration gen (create/add column) + CLI; query API 1 (select/joins/filter/order/limit, terminals); projections A+B+C (`row!`/`sql!`); insert/insert_many/COPY/upsert; transactions; raw; error mapping |
| **v2** | relational API 2 (one-level `.with()`); migration diff alter/drop |
| **v3** | nested `.with()`; CTEs; subqueries; broader aggregates; possible second backend behind the executor trait |

## 19. Open questions / risks

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
