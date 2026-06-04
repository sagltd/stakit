# Plan — stakit-orm schema-emission gaps

Make the ORM toolchain (derive + CLI + runtime) able to emit the schema the
agent/vector workloads need. Three independently testable slices.

## Verified gap evidence (against landed source)

- **Index method/opclass dropped.** `orm-cli/src/parse.rs:261` matches `index` and
  sets a bare `bool`, discarding any value; `index_method` / `opclass` keys are
  *silently swallowed* (the `parse_nested_meta` else-branch returns `Ok(())` with no
  error — `orm-derive/src/lib.rs:743`, `orm-cli/src/parse.rs:256`). The CLI model
  (`orm-cli/src/model.rs:22`) has no method/opclass fields and `diff.rs:180`
  `create_index_sql` always emits a plain (B-tree) `create index`. The runtime
  `Column::create_index_sql` (`orm/src/schema.rs:98`) can emit `using <method>` but
  has no opclass and is unused by the CLI (test-only).
- **Composite PK hard-errors.** `orm-derive/src/lib.rs:560` rejects `>1` `#[column(pk)]`.
  The CLI DDL (`diff.rs:195`) *already* joins all `pk` columns into
  `primary key (a, b)`; only the derive blocks it. Runtime `type Pk` + `pk_filter`
  (`db.rs:547`) assume a single key.
- **FTS recomputes `to_tsvector`.** `expr.rs:297` renders
  `to_tsvector('<cfg>', col) @@ plainto_tsquery('<cfg>', $1)` at query time on the raw
  column — never uses a stored tsvector, never a GIN index. No way to declare a
  `GENERATED … STORED` column.

## Slice 1 — Index method + opclass emission (HNSW / GIN / GiST)

Attribute spellings (derive **and** CLI parser):
- `#[column(index)]` → bare default (B-tree). *(existing)*
- `#[column(index = "hnsw")]` → access method via value form. *(existing in derive; add to CLI)*
- `#[column(index_method = "hnsw")]` → method via keyword. *(new — the spec's syntax)*
- `#[column(opclass = "vector_cosine_ops")]` → operator class on the index column. *(new)*
- Unknown `#[column(..)]` keys now **error** instead of silently dropping (the root cause).

Emission: `create index "idx_<t>_<c>" on "<t>" using <method> ("<c>" <opclass>)`.
- Runtime `stakit_orm::Column`: add `index_opclass: Option<&'static str>`; update
  `create_index_sql`.
- CLI `model::Column`: add `index_method`, `opclass`; update `diff::create_index_sql`.

**Verify:** render/unit tests — `using hnsw ("embedding" vector_cosine_ops)`,
`using gin ("body_tsv")`, bare default unchanged, existing gist test still green.
(HNSW e2e needs pgvector, not bundled → render-only, like the existing pgvector test.)

## Slice 2 — Composite primary keys

Derive: drop the `>1` hard-error. `type Pk` =
- 0 pk → `()` *(unchanged)*
- 1 pk → the field type *(unchanged)*
- ≥2 pk → tuple `(T1, T2, …)`.

Tuples have **no `ToValue` impl** (`value.rs`), so `db.get::<Composite>(…)` is a
*compile error* — the type-state blocks single-key lookups on a composite table with
no runtime footgun. Composite tables are queried with `find().filter(...)`. CLI
already emits `primary key (a, b)`.

**Verify:** derive unit test (2-pk struct compiles, COLUMNS has 2 `is_pk`); CLI diff
test (`primary key ("a", "b")`); embedded-pg e2e (create + insert + `find().filter`
on a 2-column-PK junction table; GIN/PK are core PG, no extension).

## Slice 3 — Stored-tsvector FTS

Attribute `#[column(generated = "<expr>")]` (derive + CLI):
- Derive: a generated column is **excluded from the `…New` insert companion** (DB
  computes it); stays in `COLUMNS`/`all()`.
- CLI `diff::column_ddl`: emit `"<c>" <type> generated always as (<expr>) stored`.
- GIN index via Slice 1 (`index_method = "gin"`).

Query path: `matches_tsv(col, q)` + `matches_tsv_in(col, q, cfg)` →
- Postgres: `"<t>"."<c>" @@ plainto_tsquery('<cfg>', $1)` — queries the **stored**
  tsvector, no `to_tsvector` recompute.
- SQLite/Turso FTS5: `"<c>" MATCH ?` (no stored tsvector there).

**Verify:** CLI diff render test (generated column + `using gin`); expr render test
(`matches_tsv` emits `@@ plainto_tsquery` with **no** `to_tsvector(`); embedded-pg e2e
— `body_tsv tsvector generated always as (to_tsvector('english', body)) stored` + GIN,
`matches_tsv(Article::body_tsv, "term")` returns the row.

## Gate

Per slice: `cargo nextest run -p <crate>` + `cargo clippy -p <crate> --all-targets
--all-features -- -D warnings` + `cargo fmt -p <crate> -- --check`. Full:
`./code-check.sh` (runs `--all-features` → boots embedded Postgres). Then 3 read-only
review agents (security / performance / code-review), master fix loop until clean.
