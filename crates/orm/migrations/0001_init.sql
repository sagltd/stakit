create table users (
    id uuid primary key,
    email text not null unique,
    name text not null
);

create table posts (
    id uuid primary key,
    author_id uuid not null references users (id) on delete cascade,
    title text not null,
    views int not null
);
