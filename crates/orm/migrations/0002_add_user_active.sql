-- ALTER TABLE migration: add a column with a default to an existing table.
alter table users add column active boolean not null default true;
