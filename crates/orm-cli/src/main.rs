// Binary crate: module items are internal, so `pub` is intentionally unreachable.
#![allow(unreachable_pub)]

//! `stakit-orm` migration CLI.
//!
//! `stakit-orm gen <name> [--schema <path>] [--migrations <dir>]` diffs the
//! `#[derive(Table)]` structs in the schema file against the snapshot in the
//! migrations directory and writes a reversible sqlx migration (`.up.sql` +
//! `.down.sql`), prompting for replace-vs-add when a change is ambiguous.

mod diff;
mod migrate;
mod model;
mod parse;

use diff::Resolver;
use model::{Column, Schema};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let exit = match args.first().map(String::as_str) {
        Some("gen") => cmd_gen(&args[1..]),
        Some(action @ ("up" | "down" | "status")) => cmd_migrate(action, &args[1..]),
        _ => {
            eprintln!(
                "usage:\n  \
                 stakit-orm gen <name> [--schema <path>] [--migrations <dir>]\n  \
                 stakit-orm up|down|status [--migrations <dir>] [--url <url>]\n\
                 \n  gen: diff #[derive(Table)] structs vs the snapshot and write a\n  \
                 reversible sqlx migration. up/down/status: apply/revert/report\n  \
                 migrations against --url or $DATABASE_URL."
            );
            2
        }
    };
    std::process::exit(exit);
}

struct Options {
    name: String,
    schema: PathBuf,
    migrations: PathBuf,
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut name = None;
    let mut schema = PathBuf::from("src/schema.rs");
    let mut migrations = PathBuf::from("migrations");
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--schema" => schema = iter.next().ok_or("--schema needs a value")?.into(),
            "--migrations" => migrations = iter.next().ok_or("--migrations needs a value")?.into(),
            other if other.starts_with("--") => return Err(format!("unknown flag {other}")),
            other => name = Some(other.to_owned()),
        }
    }
    Ok(Options {
        name: name.ok_or("missing migration name")?,
        schema,
        migrations,
    })
}

fn cmd_migrate(action: &str, args: &[String]) -> i32 {
    let mut migrations = PathBuf::from("migrations");
    let mut url = std::env::var("DATABASE_URL").ok();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--migrations" => {
                if let Some(value) = iter.next() {
                    migrations = value.into();
                } else {
                    eprintln!("error: --migrations needs a value");
                    return 2;
                }
            }
            "--url" => url = iter.next().cloned(),
            other => {
                eprintln!("error: unexpected argument {other}");
                return 2;
            }
        }
    }
    let Some(url) = url else {
        eprintln!("error: set $DATABASE_URL or pass --url <url>");
        return 2;
    };
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("error: {error}");
            return 1;
        }
    };
    match runtime.block_on(migrate::run(action, &migrations, &url)) {
        Ok(message) => {
            println!("{message}");
            0
        }
        Err(error) => {
            eprintln!("error: {error}");
            1
        }
    }
}

fn cmd_gen(args: &[String]) -> i32 {
    let options = match parse_options(args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("error: {error}");
            return 2;
        }
    };
    match run_gen(&options) {
        Ok(message) => {
            println!("{message}");
            0
        }
        Err(error) => {
            eprintln!("error: {error}");
            1
        }
    }
}

fn run_gen(options: &Options) -> Result<String, String> {
    let source = std::fs::read_to_string(&options.schema)
        .map_err(|error| format!("read {}: {error}", options.schema.display()))?;
    let new_schema = parse::parse_schema(&source)?;
    let snapshot_path = options.migrations.join(".snapshot.json");
    let old_schema = load_snapshot(&snapshot_path)?;

    let mut resolver = StdinResolver;
    let changes = diff::diff(&old_schema, &new_schema, &mut resolver);
    if changes.is_empty() {
        return Ok("no schema changes — nothing to generate".to_owned());
    }

    let up = diff::up_sql(&changes);
    let down = diff::down_sql(&changes);
    let version = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_secs();
    let stem = format!("{version}_{}", options.name);

    std::fs::create_dir_all(&options.migrations)
        .map_err(|error| format!("create {}: {error}", options.migrations.display()))?;
    let up_path = options.migrations.join(format!("{stem}.up.sql"));
    let down_path = options.migrations.join(format!("{stem}.down.sql"));
    write_file(&up_path, &format!("{up}\n"))?;
    write_file(&down_path, &format!("{down}\n"))?;
    save_snapshot(&snapshot_path, &new_schema)?;

    Ok(format!(
        "wrote {} and {} ({} change(s)); snapshot updated",
        up_path.display(),
        down_path.display(),
        changes.len()
    ))
}

fn load_snapshot(path: &Path) -> Result<Schema, String> {
    match std::fs::read_to_string(path) {
        Ok(contents) => serde_json::from_str(&contents).map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Schema::default()),
        Err(error) => Err(format!("read {}: {error}", path.display())),
    }
}

fn save_snapshot(path: &Path, schema: &Schema) -> Result<(), String> {
    let json = serde_json::to_string_pretty(schema).map_err(|error| error.to_string())?;
    write_file(path, &format!("{json}\n"))
}

fn write_file(path: &Path, contents: &str) -> Result<(), String> {
    std::fs::write(path, contents).map_err(|error| format!("write {}: {error}", path.display()))
}

/// Asks on stdin whether an added column is a rename of a removed one.
struct StdinResolver;

impl Resolver for StdinResolver {
    fn rename_target(
        &mut self,
        table: &str,
        added: &Column,
        candidates: &[Column],
    ) -> Option<String> {
        let names: Vec<&str> = candidates.iter().map(|c| c.name.as_str()).collect();
        println!(
            "Table '{table}': new column '{}' added while these were removed: {names:?}",
            added.name
        );
        print!(
            "  Rename one of those into '{}' (type its name to REPLACE), or press Enter to ADD new: ",
            added.name
        );
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            return None;
        }
        let answer = line.trim();
        if names.contains(&answer) {
            Some(answer.to_owned())
        } else {
            None
        }
    }
}
