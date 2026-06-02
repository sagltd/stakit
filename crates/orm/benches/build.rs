//! Query-build microbenchmarks (divan). Measures the ORM overhead between the
//! user's call and sqlx — pure SQL assembly, no database.

use stakit_orm::Select;
use stakit_orm::prelude::*;
use uuid::Uuid;

#[derive(Table)]
#[table(name = "users")]
#[allow(dead_code)]
struct User {
    #[column(pk)]
    id: Uuid,
    #[column(unique)]
    email: String,
    name: String,
}

fn main() {
    divan::main();
}

/// Build a typical filtered select (projection + 1 predicate).
#[divan::bench]
fn select_build_simple() -> String {
    let select = Select::new(User::all())
        .from::<User>()
        .filter(eq(User::id, Uuid::nil()));
    divan::black_box(select.to_sql().unwrap())
}

/// Generate a default (21-char) nano ID from the OS CSPRNG.
#[divan::bench]
fn nanoid_default() -> String {
    divan::black_box(stakit_orm::nanoid())
}

/// Generate a short nano ID.
#[divan::bench]
fn nanoid_short() -> String {
    divan::black_box(stakit_orm::nanoid_sized(8))
}

/// Build a select with a compound predicate, ordering, and paging.
#[divan::bench]
fn select_build_complex() -> String {
    let select = Select::new((User::id, User::email))
        .from::<User>()
        .filter(and(eq(User::name, "Dan"), eq(User::email, "a@b.com")))
        .order_by(desc(User::name))
        .limit(20)
        .offset(40);
    divan::black_box(select.to_sql().unwrap())
}
