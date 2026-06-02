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
    pub views: i32,
    #[column(name = "created_at", default = "now()")]
    pub created_at: DateTime<Utc>,
    #[column(nullable)]
    pub body: Option<String>,

    #[belongs_to(User, fk = author_id)]
    pub author: Rel<User>,
    #[has_many(Comment, fk = post_id)]
    pub comments: Rel<Vec<Comment>>,
}

#[derive(Table)]
#[table(name = "comments")]
pub struct Comment {
    #[column(pk, default = "gen_random_uuid()")]
    pub id: Uuid,
    #[column(references = Post::id, on_delete = "cascade")]
    pub post_id: Uuid,
    #[column(references = User::id, on_delete = "cascade")]
    pub author_id: Uuid,
    pub body: String,

    #[belongs_to(Post, fk = post_id)]
    pub post: Rel<Post>,
    #[belongs_to(User, fk = author_id)]
    pub author: Rel<User>,
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
- **Insert companion type** `UserNew` (Drizzle-style): same columns, but any
  column with a `#[column(default = …)]` (or `pk` with a default) is `Option<T>`
  — `None` omits it from the INSERT list so the **DB default fires**; relation
  (`Rel<_>`) fields are absent. `db.insert`/`insert_many` take `UserNew`, so DB
  defaults are reachable (the full `User` struct, with required `id`/`created_at`,
  is the *read* shape). `UserNew::from(User)` exists for convenience.

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

**The FK type-equality witness is compile-time only.** It fires only when the
schema is compiled as part of the normal `cargo build` — so the schema file MUST
be a real build target (not behind a `cfg`, not generator-only). The syn migration
generator (§5) has no type info and **cannot** evaluate `<User as Table>::PkTy`; at
gen time it does a weaker **spelling-level** FK check (FK column canonical spelling
== referenced PK column spelling) as defense-in-depth, and hard-errors on
mismatch. Semantic type-equality is rustc's job; gen-time catches spelling drift.

`#[column(default = "...")]`, `#[column(sql_type = "...")]`, and
`#[table/column(name = "...")]` take **string literals only** — trusted developer
input emitted verbatim into DDL/identifiers, never a runtime `String` (see §5 +
§17 for the safety boundary). Other semantic/DB-truth checks a proc-macro cannot
see from another struct's site are deferred to migration generation.

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
- Identifiers > 63 UTF-8 bytes are a **hard error** (Postgres truncates at
  NAMEDATALEN=63, which would silently collide names; §17).

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

// join -> inner: project T::all(); LEFT/RIGHT outer side MUST be .nullable()
// so unmatched (all-NULL) rows decode to None instead of failing.
let rows = db.select((Post::all(), Comment::all().nullable()))   // (Post, Option<Comment>)
    .from::<Post>()
    .left_join::<Comment>(eq(Post::id, Comment::post_id))        // col=col, both Uuid
    .filter(eq(Post::id, pid))                                   // pid: Uuid
    .all().await?;

// IN list -> rendered as `= ANY($1)` with ONE array bind (see note below)
let some = db.select(User::all()).from::<User>()
    .filter(any_of(User::id, &ids))                         // ids: &[Uuid]
    .all().await?;
```

`eq` (and friends) is generic via an `IntoExpr<Ty>` bound: `fn eq<L: ColExpr, R:
IntoExpr<L::Ty>>(l: L, r: R)`. This makes `eq(Post::id, Comment::post_id)`
(col=col) and `eq(Post::id, pid)` (col=value) both type-check, while
`eq(User::id, 5)` or `eq(User::id, "str")` **fail to compile**.

**The `IntoExpr` impl set is the type-safety boundary and is hand-curated** (no
reflexive `impl<Ty> IntoExpr<Ty> for Ty` — that blanket is a coherence hazard
against the `Col` impl). Specifically:

- `impl<T, Ty> IntoExpr<Ty> for Col<T, Ty>` — column on the RHS (joins).
- One value impl per supported scalar via a tiny wrapper to dodge coherence:
  values are accepted through `impl<Ty: Encode + Type<Postgres>> IntoExpr<Ty> for Val<Ty>`
  plus targeted ergonomic impls (`&str: IntoExpr<String>`, `&str:
  IntoExpr<Option<String>>`, `&[u8]: IntoExpr<Vec<u8>>`, `T: IntoExpr<Option<T>>`).
- Deliberately **absent**: numeric widening (`i32 → i64`), `&str → Uuid`, etc., so
  type mismatches stay compile errors.

Values are bound as positional `$N` params (injection-safe). `LIMIT`/`OFFSET` are
also **bound params** (`limit $n offset $m`), never literals — literal pagination
would mint a new prepared statement per page (§16).

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
| `Insert::returning(proj).one()` | `Output` | non-`Option` — a plain single-row insert always produces exactly one row |
| `Update`/`Delete`/`Insert…on_conflict(do_nothing)` `.returning(proj).one()` | `Option<Output>` | may match/produce **zero** rows |
| `.returning(proj).one_or_err()` / `.exact_one()` | `Output` | erroring forms (`NotFound` / `TooManyRows`) |

Non-`Option` `.one()` is restricted to plain `Insert` (the row is definitely
written). For `Update`/`Delete`/`do_nothing` the `WHERE`/conflict may match zero
rows, so their `.one()` is `Option<Output>` — use `.one_or_err()`/`.exact_one()`
when a row is required.

## 7. Select projections — the `Projection` trait

`select()` takes anything implementing `Projection`. The argument decides the
return type — **no annotation needed**, inference flows from `Projection::Output`
through the terminal (`.all()` → `Vec<Output>`, etc.).

```rust
pub trait Projection {
    type Output;
    /// Append this projection's SQL select-list fragments into a caller-owned
    /// buffer (no large by-value return; see §15 size budget).
    fn write_columns(&self, out: &mut SmallVec<[SqlExpr; 8]>);
    /// Decode one row. `&self` is required: instance-carrying projections (the
    /// `row!` form) hold per-field state. Reads are positional by compile-time
    /// literal ordinal (`row.try_get(0usize)`), not a per-row name lookup —
    /// `PgRow::try_get(usize)` is O(1).
    fn decode(&self, row: &PgRow) -> Result<Self::Output>;
}
```

The trait carries **`decode(&self, …)`** (an earlier draft made it a no-`self`
associated fn — wrong: the `row!` form holds per-field state and the terminal
already owns the projection instance, so `proj.decode(&row)` is what runs). The
static wrappers simply ignore `&self`. Decode strategies are provided by
**distinct, non-overlapping wrapper types** (no coherence conflict):

| `select(...)` argument | wrapper type | `Output` | decode |
|---|---|---|---|
| `User::id` (single `Col`) | `Col<User, Uuid>` | `Uuid` | `try_get(0)` |
| `(User::id, User::name)` | tuple impl (macro, arity 1..=12) | `(Uuid, String)` | positional |
| `count()` / `count(Post::id)` | `Count` | `i64` | `try_get(0)` |
| `sum(Col<_,T>)` / `avg(_)` | `Sum<T>` / `Avg` | `T` / `f64` | `try_get(0)` |
| `User::all()` | `All<User>` | `User` | `User::from_row` (sqlx `FromRow`) |
| `User::all().nullable()` | `All<User, Nullable>` | `Option<User>` | `None` when the row's columns are all NULL (outer-join side), else `User::from_row` |
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
"type annotations needed". The fix drives each field's type from the **column
token's own associated `Expr::Out`** (no extractor closure — closures carry no
info and only host a bound). The macro emits a **complete inline `Projection`
impl** for an anonymous local struct (not a shared library `build` — there is no
fixed arity cap because it is generated per call):

```rust
{
    struct Row { email: String, user_id: Uuid, post_count: i64, year: i32 }
    //          ^ field types are written by the macro from each expr/sql! type:
    //            plain cols -> <Col<_,Ty> as Expr>::Out via `field(expr): Field<X>`
    //            where Field<X> requires X::Out: Decode<Postgres> + Type<Postgres>;
    //            sql! fields -> the explicit 1st-arg type.
    struct __Proj { fields: (Field<Col<User,String>>, Field<Col<User,Uuid>>,
                             Field<Count>, FieldSql<i32>) }
    impl Projection for __Proj {
        type Output = Row;
        fn write_columns(&self, out: &mut SmallVec<[SqlExpr; 8]>) { /* push each */ }
        fn decode(&self, r: &PgRow) -> Result<Row> {
            Ok(Row { email: r.try_get(0)?, user_id: r.try_get(1)?,
                     post_count: r.try_get(2)?, year: r.try_get(3)? })
        }
    }
    __Proj { fields: (field(User::email), field(User::id),
                      field(count(Post::id)), field_sql::<i32>("extract(year from {})", Post::created_at)) }
}
// Output = Row { email: String, user_id: Uuid, post_count: i64, year: i32 }
```

`field<X: Expr>(e: X) -> Field<X>` where `X::Out: Decode + Type<Postgres>`: the
field's decoded type **is** `X::Out`, named in `Field`'s impl, so there is no
"annotations needed" hole and no closure. `decode` reads each field by its
compile-time literal ordinal; the macro maps **struct-field → SELECT-list ordinal**
at expansion time, so a field declared out of SELECT order still gets the correct
index. `sql!` fields supply the type explicitly via `field_sql::<T>`.

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
for u in &users {
    for p in u.posts.get()? { /* `get()` errs if this relation was NOT requested */ }
}

// nested with + filter + order + limit   (v3 target API — see restrictions)
let posts = db.query::<Post>()
    .with(Post::comments, |c| c.with(Comment::author))
    .filter(eq(Post::id, pid))             // pid: Uuid (type-matched, see §6)
    .order_by(asc(Post::id))
    .limit(5)
    .find_many().await?;

// findFirst
let one = db.query::<User>().filter(eq(User::id, uid)).find_first().await?;  // Option<UserWith>

// columns selection (Drizzle `columns: { id: true }`) — v3 (cannot combine with .with in v2)
let slim = db.query::<Post>().columns((Post::id, Post::title)).find_many().await?;
```

**Loading strategy:** batched, not N+1, not one giant join. One query per relation
using **`... where fk = ANY($1)`** (array-bind rule from §6 — stable statement, no
65535 cap on parent-key count), stitched in Rust. **Latency = one round-trip per
relation**, summed across levels; sibling relations at the same level are
**serialized in v2** (so it's per-relation, not per-level). Stitch is O(N+M): one
`hashbrown` map **pre-sized to parent count** (`with_capacity(N)`, no rehash
thrash), O(N) child `Vec`s, **no sort, no dedup** (child order preserved from the
child query's `ORDER BY`).

**The hard part (honest):** API 1 maps cleanly to Rust. API 2's typed result needs
codegen — Drizzle leans on TS structural typing, Rust has none. Result-type
strategy for **v2**:

- The `#[has_many]`/`#[belongs_to]` derive generates **one fixed `UserWith` type
  per table** with a field per declared relation, each wrapped in a **loaded-state
  marker** to avoid the "loaded-but-empty vs not-requested" ambiguity:
  ```rust
  pub enum Loaded<T> { NotLoaded, Loaded(T) }
  // generated:
  struct UserWith { /* User's columns */, posts: Loaded<Vec<Post>>, /* … */ }
  impl Loaded<T> { fn get(&self) -> Result<&T> /* err if NotLoaded */ }
  ```
  A relation not named in `.with(...)` is `NotLoaded` (accessing it errors, not
  silently empty); a requested has-many with zero children is `Loaded(vec![])`;
  a requested belongs-to with a NULL/absent FK is `Loaded(None)`. This single
  fixed type avoids the combinatorial explosion of one type per `.with()` combo.
- **v2 restrictions (stated, not hidden):** `.with()` is **one level deep**;
  nested `.with(rel, |c| c.with(...))` and combining `.columns(...)` *with*
  `.with(...)` are **v3**. The nested/`columns`+`with` examples above are the v3
  target API, shown for direction.

Phasing: **v2** = relational, one fixed `UserWith` with `Loaded<_>` fields, one
level of `.with()`, `= ANY($1)` batched load. **v3** = nested `.with()`,
`columns`+`with` combined, CTEs, subqueries, broader aggregates, optional
concurrent sibling loading (one pooled connection per concurrent sibling —
interacts with `.stream()` pool pressure, §16).

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
// single — UserNew lets defaulted columns (id, created_at) be omitted (None) so
// the DB default fires; required columns are plain fields.
let id: Uuid = db.insert(UserNew { email, name, ..Default::default() })
    .returning(User::id).one().await?;
db.insert(new_user).exec().await?;                   // -> rows affected

// many -> INSERT … SELECT * FROM UNNEST($1::T[], …), one array per column, one statement
let n = db.insert_many(new_users).exec().await?;
let ids: Vec<Uuid> = db.insert_many(new_users).returning(User::id).all().await?;

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
| 2 – ~tens of thousands | `insert_many` | **`INSERT … SELECT * FROM UNNEST($1::T[], $2::U[], …)`** — one array param **per column** |
| huge (10k+) | `copy_into` | `COPY … FROM STDIN` via sqlx `PgCopyIn` — single stream, no per-row parse/plan/bind |

**`insert_many` uses UNNEST, not multi-row VALUES.** `INSERT INTO users (a,b,c)
SELECT * FROM UNNEST($1::int[], $2::text[], $3::uuid[])` binds **one array per
column** (column-count params total, not rows×cols). Consequences:

- **One stable prepared statement for any row count** — no statement-cache thrash,
  no chunk-size bucketing. (Multi-row VALUES was rejected: its text varies with
  row count → a new statement per length, and "padding the statement shape but not
  the data" is impossible — you can't bind fewer rows than the VALUES tuples
  without inserting garbage.)
- **65535 param cap is unreachable** (you'd need 65535 *columns*). The old
  `rows_per_stmt` chunk math is gone; a normal `insert_many` is **one round-trip**.
- Array binds are binary-encoded (extended binary protocol, §16).
- DB defaults: a column left `None` in `UserNew` (§4) is **dropped from the INSERT
  column list** so its DB default applies; UNNEST then supplies arrays only for the
  included columns. (Per-call the included-column set is fixed, so the statement is
  still stable for that set.) Caveat: `on_conflict … do_update` works (EXCLUDED
  mechanics differ slightly from VALUES). For very large arrays the planner can
  misestimate cardinality — that is exactly the point where the docs steer to
  `copy_into`.

**Transaction tradeoff:** the single UNNEST insert is naturally atomic. The only
reason to split is array size; for that, `insert_many` accepts `.commit_per_chunk()`
(splits into N UNNEST inserts, each committed). On a mid-load failure it returns
the **count of rows successfully committed** so the caller can resume/reconcile;
an idempotent key (`on_conflict do_nothing`) is recommended for resumable loads.

**`copy_into` is single-stream** (one logical round-trip). On any error mid-stream
it calls `PgCopyIn::abort` before returning, so the connection is never returned
to the pool stuck in COPY protocol state. **Binary-format is a whole-stream,
all-columns decision** made up front: `COPY … (FORMAT BINARY)` needs a binary
encoder for *every* column, so if **any** column type lacks one the **entire**
COPY uses text format (chosen at build time from the column set, never discovered
mid-stream). The "≥5× `insert_many`" figure (§15) is a *measured* result, not a
guaranteed target — it depends on row width and network.

Recommended defaults: `insert_many` for normal app writes; `copy_into` for
seed/import/ETL.

## 11. Transactions

Real pg transactions via sqlx. The **closure form is the supported path** and
performs an explicit, awaited `ROLLBACK` on the **`Err`-return** path. The manual
form relies on sqlx's drop behavior, which is **best-effort, not a guarantee** (see
caveat below).

**Executor abstraction:** we define our **own** `Executor` trait (not sqlx's
directly), with `async fn run(&mut self, …)`, implemented for `Db` (pool) and
`Tx`. This is required because sqlx's `Executor::fetch*` consumes `self` by value;
for `&mut Transaction` each call must reborrow (`&mut *tx`). Our trait takes
`&mut self` and reborrows internally, so the same `select/insert/update/...` API
works on both a pool and a transaction, and sequential `tx.insert(..); tx.insert(..)`
calls compile.

**Transaction-closure signature.** The bare `for<'c> FnOnce(&'c mut Tx) -> impl
Future + 'c` form is *not* literally expressible (can't put `impl Future` in an
HRTB return), and the naive `F: FnOnce(&mut Tx) -> Fut, Fut: Future` form can't
tie `Fut`'s borrow to the `&mut Tx` (the "closure returning a future that borrows
its argument" hole). v1 uses stable **`AsyncFnOnce`** (edition 2024 / Rust ≥ 1.85,
which the workspace targets):
`fn transaction<T>(&self, f: impl for<'c> AsyncFnOnce(&'c mut Tx) -> Result<T> + Send) -> impl Future<Output = Result<T>> + Send`.
Fallback if `AsyncFnOnce` HRTB ergonomics bite: the boxed form
`for<'c> FnOnce(&'c mut Tx) -> Pin<Box<dyn Future<Output = Result<T>> + Send + 'c>>`
(one alloc per transaction, definitely compiles). Spike this early (§19).

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

- Prefer the **closure form** (`db.transaction(|tx| …)`) — it awaits an explicit
  `ROLLBACK` on the **`Err`-return** path. On **cancellation** (the closure future
  is dropped mid-poll) no async code can run during `Drop`, so it falls back to
  sqlx's best-effort drop-rollback — same as the manual form. "Awaited rollback on
  cancel" is impossible; only `Err`-return can await. (This corrects an over-claim:
  the closure form is *not* a cure for timeout/cancellation rollback.)
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
| `bench_insert_unnest_build` | `insert_many` UNNEST array build for 10k rows (one array per column) | linear in rows; allocs = column-count arrays, reported |
| `bench_projection_row_macro` | `row!` decode path vs `#[derive(Row)]` path | **0 delta** — both lower to `try_get::<X::Out>(literal_ordinal)` per field |
| `bench_error_map` | `sqlx::Error` → `Error` SQLSTATE mapping | < 100 ns |
| `size_of` checks | `size_of::<SqlExpr>()`, builder struct size, inline smallvec footprint | **`SqlExpr` ≤ 32 bytes** (index/enum, no owned `String` inline — fragments reference tokens / write into the shared buffer); guards stack-frame + `write_columns` copy cost |

| Bench (integration, embedded pg, CI) | Measures | Target |
|---|---|---|
| `bench_decode_row` | decode N rows into `User` via sqlx `FromRow` vs raw `query_as` | informational, ±20% (CI noise floor) |
| `bench_decode_row_macro` | `row!` decode (literal ordinals) vs derive | informational, ±20% |
| `bench_relation_stitch` | `with()` stitch, N parents × M children | O(N+M) time; **divan alloc** = ≈ N child-Vecs + 1 pre-sized map, 0 reallocs (not RSS) |
| `bench_with_roundtrips` | 1-level `.with()` vs manual join vs N+1 | `.with()` ≪ N+1 |
| `bench_statement_cache_hitrate` | re-parse rate across (a) varying IN/insert sizes **and** (b) builder-shape cardinality | **near-100% on axis (a)**; (b) characterizes the LRU cliff vs `statement_cache_capacity` |
| `bench_stream_memory` | rows buffered at once + **divan bytes/iter** over a 1M-row `.stream()` | O(row width), **not** O(N) — measured via alloc profiling, not RSS |
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
4. `insert_many` is **one UNNEST statement / one round-trip** for any row count
   (one array param per column), not chunked VALUES (§10).
5. All generated queries go through sqlx's **prepared, extended (binary) protocol**
   path — no simple-protocol fallback. IN lists use `= ANY($1)`, `insert_many` uses
   UNNEST, and `LIMIT`/`OFFSET` are binds, so per-shape statement count is stable
   (`bench_statement_cache_hitrate`); builder-shape cardinality vs the 256-cap LRU
   is the residual axis to watch (§16).
6. No `format!`-per-row in any hot path; SQL assembled into one `String` with
   computed capacity (that String is the 1 unavoidable alloc, per the budget above).

## 16. Production readiness

### Connection pool

`Db::new(pool)` wraps `sqlx::PgPool`. Expose a `DbConfig` builder for production
knobs (all map to sqlx `PoolOptions`):

```rust
let db = Db::connect(DbConfig {
    // accepts a URL or sqlx PgConnectOptions so the password can be supplied
    // out-of-band; the URL field is a redacting wrapper (Debug never prints it).
    conn: ConnSpec::Options(opts),   // or ConnSpec::Url(Secret<String>)
    max_connections: 20,
    min_connections: 2,
    acquire_timeout: Duration::from_secs(5),
    idle_timeout: Some(Duration::from_secs(600)),
    max_lifetime: Some(Duration::from_secs(1800)),
    statement_cache_capacity: 256,
    slow_query_threshold: Some(Duration::from_millis(200)),
}).await?;
```

- **Acquire timeout** is mandatory (no unbounded waits → no hung requests).
- Pool is `Clone` + `Send` + `Sync` (sqlx `PgPool` is `Arc` inside); share across
  tasks freely.
- **Credential hygiene:** the connection string holds the password. `DbConfig`'s
  `Debug` **redacts** it (wrapped in a `Secret`/custom `Debug`), and the URL is
  never placed in spans or error context. Prefer supplying `PgConnectOptions`.

### Protocol

All generated queries use sqlx's **extended query protocol with binary result
format** (sqlx default) — no string parsing of ints/uuids/timestamps on decode,
and prepared statements are reusable. **No code path falls back to the simple
protocol** (which would lose both binary decode and statement reuse); the
`= ANY($1)` rule (§6) is what lets dynamic-length IN lists stay on the prepared
path.

### Prepared-statement cache

sqlx caches prepared statements per connection. A cache hit needs a **stable SQL
string for a query shape**. Two well-known traps are designed out:

- **IN lists:** `in ($1,$2,…)` varies its text with element count → a new
  statement per length → thrash. We render `= ANY($1)` instead (§6) — one stable
  statement for any length.
- **`insert_many`:** uses UNNEST with one array param per column (§10), so a
  **single statement serves any row count** — no bucketing needed.
- **`LIMIT`/`OFFSET`:** bound params (§6), not literals — otherwise every page is
  a distinct statement (pagination would defeat the cache).

What is **not** bounded by these rules: the **combinatorial cardinality of builder
shapes** itself. Each distinct (projection × predicate set × order × join × paging-
present) combination is a distinct prepared statement. A table with 5 optional
filters and 3 orderings is already ~96 shapes; a few such tables exceed
`statement_cache_capacity` (256, **per connection**). sqlx's cache is **LRU**: over
capacity it `DEALLOCATE`s the least-recently-used statement and re-parses on the
next miss — no error, but a silent parse-thrash perf cliff. Guidance: 256 suits
small schemas; high-shape-cardinality apps should raise it. `bench_statement_cache_hitrate`
(§15) exercises **both** axes — IN/insert size **and** builder-shape cardinality.

Values are never embedded in SQL text (they are binds) — a perf invariant and a
security invariant (§17).

### Timeouts & cancellation

- Every terminal accepts an optional per-query timeout; on elapse the future is
  dropped. **Dropping a sqlx query future does not send a Postgres `CancelRequest`**
  — it stops polling, leaving an unread result on the connection, so sqlx may
  **close that connection** on cleanup. This applies to **any** pooled connection,
  not just transaction ones: a timeout storm can churn/deplete the pool and
  cascade into acquire-timeouts (an availability concern). Mitigations: set a
  server-side `statement_timeout` GUC (the real cancellation mechanism), and/or use
  an out-of-band cancel on a separate connection for the timeout path. Documented,
  not hidden.
- Cancelling an in-flight query **on a transaction's connection** may force that
  connection closed rather than rolled back (§11 caveat) — correctness over reuse.

### Observability

- `tracing` spans around each query: fields = operation, table, elapsed, rows.
- **Value-leak boundary (honest):** stakit never interpolates values into SQL, so
  values are not in the SQL text to begin with — that is the real protection.
  stakit additionally configures sqlx's own statement logging **off by default**
  (`.log_statements(Off)`, slow-query logging gated by `slow_query_threshold`), so
  sqlx does not log SQL/values. What stakit **cannot** control: Postgres
  server-side logging (`log_statement`/`log_min_duration_statement`) — operators
  own that. Error `Display` can carry pg messages (§12) — log server-side only.

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
| `Vec<T>` (T ≠ u8) | `T[]` array |
| `#[column(enum)]` Rust enum | pg enum / text |

`Vec<u8>` maps to `bytea` and is **special-cased before** the generic `Vec<T>`
rule (they overlap). The migration generator (§5) recognizes only the canonical
spelling `Vec<u8>`; `&[u8]`/`Bytes` need `#[column(sql_type = "bytea")]`. Optional
types gated behind cargo features (`uuid`, `chrono`, `json`, `decimal`) so a
minimal build stays lean. Newest crate versions pinned at impl (workspace rule).

### `.stream()` semantics

`.stream()` reads rows incrementally from the socket (bounded client memory — it
does not buffer the full result), but it **holds its pooled connection for the
stream's whole lifetime** (pool-pressure cost) and Postgres still computes the
full result server-side unless a cursor is used. Use it for large reads where you
consume-and-drop; for very large server-side result sets, prefer keyset
pagination. `bench_stream_memory` (§15) asserts bounded client memory.

### Concurrency & safety

- All public futures `Send` where the executor is `Send` (matches workspace
  `future_not_send` relaxation but the executor path stays `Send`).
- `unsafe` forbidden workspace-wide; all `unsafe`-free by construction (no
  `GlobalAlloc` for benches — divan's alloc profiling is used instead, §15).
  Allocation-light design is via lifetimes / smallvec, never raw pointers.
- Derives generate only safe code; no `unsafe` in expansions.

### Graceful failure

- Pool exhaustion → `Error::Database` with acquire-timeout context, not a hang.
- Migration checksum drift (edited applied migration) → hard error at startup via
  sqlx, surfaced clearly.
- `insert_many` is one transaction by default (all-or-nothing); `.commit_per_chunk()`
  trades atomicity for shorter locks/WAL on bulk loads (§10).
- `copy_into` is a single atomic COPY (the command is all-or-nothing); on error it
  `abort()`s the COPY stream so no connection is returned mid-protocol (§10).

## 17. Security

- **SQL injection (values):** all user values are bound parameters (`$N`), never
  string-interpolated, on every builder path. `sql!` `{}` accepts only `Col`
  tokens (compile-checked); `?` is a bind only; clause inputs (`order_by`
  direction, nulls placement, `on_conflict` target) are typed tokens/enums, never
  strings. **No `order_by_raw(&str)`-style API exists** — dynamic SQL only via the
  explicit `db.raw` opt-out.
- **SQL injection (identifiers):** table/column names come only from compile-time
  schema tokens, rendered with **correct identifier quoting** — wrap in `"`,
  **double every embedded `"`** (`x"y` → `"x""y"`), reject NUL bytes. (Quoting
  alone is not a sanitizer; embedded-quote doubling is the actual defense and is a
  tested invariant.) Note attribute strings (`#[column(name=…)]`,
  `#[table(name=…)]`) are trusted developer input, but are still quoted correctly.
- **Identifier length (NAMEDATALEN):** Postgres silently truncates identifiers to
  63 bytes, so two names sharing a 63-byte prefix would collide (wrong FK target,
  or a snapshot diff that mis-detects a "new" column). The identifier renderer and
  the migration generator **hard-error on any identifier > 63 UTF-8 bytes** rather
  than truncate.
- **DDL strings (trusted, verbatim, literal-only):** `#[column(default = "…")]`,
  `#[column(sql_type = "…")]`, and `#[table(name=…)]`/`#[column(name=…)]` are
  emitted into DDL from string literals only — never templated from runtime/env.
  The column API accepts no runtime `String` for these. **There is no
  literal-vs-function escaping** of `default`/`sql_type` (a developer writing
  `default = "O'Brien"` produces invalid DDL): the **review-before-apply gate (§5)
  is the only control**, by design. (Future option: split `default_value =
  <typed literal>` (auto-escaped) vs `default_expr = "<sql>"` (verbatim), mirroring
  the value-vs-raw split elsewhere.)
- **Raw escape hatch is unaudited by design:** `db.raw(&str)` and the exposed
  `db.pool()` put SQL-text responsibility on the caller (values still via
  `.bind()`). `&'static str` was rejected as security theater (`Box::leak`
  defeats it; it only blocks legitimate dynamic SQL). The builder APIs are the
  safe default.
- **Value logging:** the real protection is that stakit never interpolates values
  into SQL; additionally sqlx statement logging is configured off by default
  (§16). stakit cannot stop Postgres server-side logging or a caller echoing
  `Error::Display` (which can carry pg messages with values, §12) — both are
  documented operator/caller responsibilities.
- **Schema-info disclosure:** the typed error variants (`Unique { constraint }`,
  `NotNull { column }`, …) place schema identifiers (constraint/column names) into
  `Error::Display`. These names reveal schema structure; like the transparent
  fallback, log them server-side and map to a generic message before returning to
  untrusted clients.
- **`unsafe` forbidden** (workspace lint), no exceptions — including benches
  (divan alloc profiling, not a custom `GlobalAlloc`, §15/§16).
- **Transactions:** drop-rollback is best-effort (§11); the closure form does an
  explicit awaited rollback. Savepoint names are internal counters, never input.
- **Migrations:** advisory-locked against concurrent boot; `.snapshot.json`
  divergence from applied state is a hard error; destructive `gen` diffs are
  reviewed before apply and destructive `down` requires `--force` (§5).
- **Least surprise:** no implicit `*` that leaks new columns into typed results —
  projections are explicit; `T::all()` is generated from the known schema.

## 18. Phasing summary

| Version | Scope |
|---|---|
| **v1** | `#[derive(Table)]` + tokens + compile-checks; migration gen (create/add column) + CLI; query API 1 (select/joins/filter/order/limit, terminals); projections A+B+C (`row!`/`sql!`); insert/insert_many/COPY/upsert; transactions; raw; error mapping |
| **v2** | relational API 2 (one-level `.with()`); migration diff alter/drop |
| **v3** | nested `.with()`; CTEs; subqueries; broader aggregates; possible second backend behind the executor trait |

## 19. Open questions / risks

Highest-risk items to prototype first (de-risk before committing the full v1):

- **`Projection` + `row!` type machinery (§7)** is the core typing bet — the
  `field(expr, extractor)` type-resolution trick and the tuple/`All`/`Col`/`Count`
  wrapper coherence must be proven with a compiling spike before the rest of the
  builder is built on it. Fallback if the inline-`row!` inference proves too
  brittle: ship tuple (A) + `#[derive(Row)]` (B) in v1, move `row!` (C) to v1.1.
- **Unified `Executor` over pool + `&mut Tx` (§11)** — the reborrow + HRTB closure
  signature is fiddly sqlx territory; spike it early.
- **syn migration resolver + type-spelling map (§5)** is inherently fragile
  (no type info); the canonical-spelling allowlist + `#[column(sql_type)]` escape +
  hard-error-on-unknown is the mitigation. Risk: developer friction on
  aliases/re-exports — accepted and documented.
- **Relational result type (§8)** — one fixed `UserWith` per table for v2 to avoid
  combinatorial codegen; nested `.with()` and `columns`+`with` deferred to v3.
- **Statement-cache discipline (§6/§10/§16)** — `= ANY($1)` for IN lists + UNNEST
  for `insert_many` + bound `LIMIT`/`OFFSET`; `bench_statement_cache_hitrate` is the
  guard. The residual axis is builder-shape cardinality vs the 256-entry LRU cap. A
  regression here is a silent perf cliff.
- **Migration diff** for type changes / renames is ambiguous (rename vs drop+add);
  v1 covers additive changes, surfaces the rest for hand-editing.

Resolved during review (now specified, not open):

- *Round 1:* lowercase column-const `#[allow(non_upper_case_globals)]`; FK
  type-equality via `PhantomData` witness (not `const assert!`); `IntoExpr<Ty>`
  for `eq` (curated impl set, no reflexive blanket); terminal split (Select vs
  mutation); honest allocation budget (1 String + 1 args, not "0 allocs"); divan
  alloc profiling (no `unsafe` allocator); identifier-quote doubling; error
  value-leak boundary; credential redaction.
- *Round 2:* `Projection::decode(&self, …)` (instance-carrying `row!`);
  `row!` via `field<X: Expr>` with `X::Out: Decode` (no extractor closure, no
  inference hole) emitting a complete inline impl; **UNNEST** for `insert_many`
  (kills VALUES bucketing, param-cap, and last-chunk statement thrash);
  `returning().one()` non-`Option` restricted to plain `Insert`
  (Update/Delete/`do_nothing` → `Option`); relational `Loaded<T>` (loaded-vs-empty
  vs not-requested); FK type check is compile-time-only (schema must be a build
  target) + gen-time spelling check; transaction closure rollback honest
  (`Err`-return only, not cancel); `AsyncFnOnce`/boxed transaction signature;
  timeout-drop pool-depletion documented; NAMEDATALEN 63-byte hard error;
  `LIMIT`/`OFFSET` bound; builder-shape statement-cache cardinality + LRU cliff;
  `Post::views`/`created_at` added; `sql_type` added to DDL trust inventory;
  `SqlExpr` ≤ 32-byte budget + `write_columns`.

- **Quality gate:** all crates must pass `./code-check.sh` (fmt, clippy
  pedantic+nursery `-D warnings`, build, nextest, doctests); `unsafe` forbidden;
  public items documented.
