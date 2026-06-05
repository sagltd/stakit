# stakit-orm — Row-Level Security, Roles, Policies, Grants

**Goal:** Make Postgres RLS first-class in stakit-orm, ORM-style and migration-generated.
Declared in the Rust schema; the CLI diffs it into reversible `.up.sql`/`.down.sql`.

## Surface (ORM-style, Drizzle-inspired)

```rust
// A database role -> CREATE ROLE in migrations.
#[derive(Role)]
#[role(name = "app_user", login)]
struct AppUser;            // AppUser::ROLE == "app_user"

#[derive(Table)]
#[table(
    name = "posts",
    rls,                                              // ENABLE ROW LEVEL SECURITY
    grant(app_user(select, insert, update, delete)),
    policy(
        posts_owner(select, to = "app_user",
                    using = "author_id = current_setting('app.user_id')::uuid"),
        posts_insert(insert, to = "app_user",
                     check = "author_id = current_setting('app.user_id')::uuid"),
    ),
)]
struct Post { #[column(pk)] id: Uuid, author_id: Uuid, title: String }
```

> **Note:** grants/policies are nested **inside** `#[table(...)]` as `grant(role(...))`
> and `policy(name(...))` — named sub-lists, not repeated `#[grant]`/`#[policy]`
> attributes — so two policies sharing a role never trip clippy's `duplicated_attributes`.

Generated `up`:
```sql
create role "app_user" login;
create table "posts" ( ... );
grant select, insert, update, delete on "posts" to "app_user";
alter table "posts" enable row level security;
create policy "posts_owner" on "posts" for select to "app_user" using (author_id = current_setting('app.user_id')::uuid);
create policy "posts_insert" on "posts" for insert to "app_user" with check (author_id = current_setting('app.user_id')::uuid);
```
`down` is the exact inverse (drop policy, disable rls, revoke, drop table, drop role).

## Slices (each: tested + green before next)

1. **model** (`orm-cli/model.rs`) — `Role`, `Policy`+`PolicyCommand`, `Grant`+`Privilege`;
   `Schema.roles`; `Table.{rls,force_rls,policies,grants}`. All new fields `#[serde(default)]`
   (old snapshots load). Serde round-trip test.
2. **diff + DDL** (`orm-cli/diff.rs`) — new `Change` variants; shared DDL helpers; a single
   `create_object_sql` (table+indexes+rls+grants+policies) used by `up(CreateTable)` and
   `down(DropTable)` for create/drop symmetry; ordered `diff()`; reversible up/down. Bulk of tests.
3. **CLI parse** (`orm-cli/parse.rs`) — syn-parse `#[derive(Role)]`, `#[table(rls,force_rls)]`,
   repeatable `#[policy]`/`#[grant]`; validation mirrors the derive.
4. **derive** (`orm-derive`) — `#[derive(Role)]`; `#[table(rls,force_rls)]`; register+validate
   `#[policy]`/`#[grant]` on `Table`. Compile-time guardrails.
5. **e2e** (`orm/tests/rls_test.rs`, postgres) — apply generated SQL to embedded pg, prove the
   policy actually enforces (own rows visible, others blocked, WITH CHECK rejects), and `down` reverts.
6. **docs** — README RLS usage section + runtime note (`set_config`/`SET ROLE` via `raw`).
7. **gate + review** — `./code-check.sh` green; adversarial security/perf/code-review agents; fix-loop.

## Rules / invariants

- **Command flag** (bare): one of `all`(default)/`select`/`insert`/`update`/`delete`.
  - `select`/`delete`: `using` required, `check` forbidden.
  - `insert`: `check` required, `using` forbidden.
  - `update`/`all`: at least one of `using`/`check`.
- `policy` and `force_rls` require `rls` on the table; `grant` does not.
- Policy `to = "a, b"` splits to roles; empty `to` = PUBLIC (omit `TO`). Grants are
  `grant(role(privs), …)` — role is the entry head, stored one-role-per-entry (merged on repeat).
- **Safety (§17):** role/policy/table/role-ref names are identifiers → quoted (double `"`),
  validated (≤63 bytes, no NUL, bare identifier). `using`/`check` are verbatim trusted exprs
  (like `default`/`generated`) — review-before-apply gate; non-empty enforced.
- Roles created first / dropped last; revokes before drop-role.
