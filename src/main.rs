//! csv-pg-schema — infer a temporal Postgres schema from a series of
//! monthly account CSVs and load only the deltas.
//!
//! ```text
//! csv-pg-schema analyze --list FILE [--schema NAME] [--key COL]
//!                       [--enum-max N] [--null-token TOK]... [-o FILE]
//! csv-pg-schema report  --list FILE [--key COL]
//!                       [--enum-max N] [--null-token TOK]... [-o FILE]
//! csv-pg-schema rust    --list FILE [--key COL]
//!                       [--enum-max N] [--null-token TOK]... [-o FILE]
//! csv-pg-schema load    --list FILE  [--schema NAME] [--key COL]
//!                       [--enum-max N] [--null-token TOK]...
//!                       [--database-url URL] [--create-schema]
//! ```
//!
//! `analyze` scans every CSV in the list and prints the `CREATE` script
//! (one temporal `(id, during)` table per attribute). `report` diffs the
//! CSVs month over month (no database needed). `rust` emits an `InputRecord`
//! struct (plus enums for enum-like columns) for the inferred schema. `load`
//! re-infers the same schema and writes each month's rows as deltas, using
//! the `WITHOUT OVERLAPS` upsert so unchanged values cost nothing.
//!
//! Environment: `DATABASE_URL` is the fallback for `--database-url`.

mod codegen;
mod error;
mod infer;
mod load;
mod manifest;
mod report;
mod schema;
mod util;

use std::{path::PathBuf, process::ExitCode};

use crate::{
	error::{Error, Result},
	infer::Options,
};

const USAGE: &str = "usage: csv-pg-schema <analyze|report|rust|load> --list FILE [options]
  analyze   scan the CSVs and print the temporal CREATE script
  report    diff the CSVs month over month (no database needed)
  rust      emit an InputRecord struct (and enums) for the schema
  load      process the CSVs in order, storing only the deltas

options:
  --list FILE        plain-text list of CSV paths, one per line (required)
  --schema NAME      target Postgres schema name (default: account)
  --key COL          account natural-key column (default: auto-detected)
  --enum-max N       max distinct values to treat a column as enum-like (default: 32)
  --null-token TOK   value to treat as NULL besides empty (repeatable)
  --lenient          tolerate ragged rows, blank keys, and header drift
                     instead of erroring (default: strict)
  --with-checks      bake the inferred profile into the DDL as numeric(p,s)
                     types and CHECK constraints (analyze, load)
  -o, --out FILE     write output to a file instead of stdout (analyze, report, rust)
  --database-url URL postgres connection string (load; or env DATABASE_URL)
  --create-schema    run the CREATE script before loading (load)";

#[derive(Clone, Copy)]
enum Command {
	Analyze,
	Report,
	Rust,
	Load,
}

struct Cli {
	command: Command,
	list: PathBuf,
	options: Options,
	out: Option<PathBuf>,
	database_url: Option<String>,
	create_schema: bool,
	with_checks: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
	match run().await {
		Ok(()) => ExitCode::SUCCESS,
		Err(e) => {
			eprintln!("error: {e}");
			ExitCode::FAILURE
		}
	}
}

async fn run() -> Result<()> {
	let cli = parse_args()?;
	let entries = manifest::parse(&cli.list)?;
	if cli.options.strict {
		manifest::check_order(&entries)?;
	}
	let schema = infer::build_schema(&entries, &cli.options)?;

	match cli.command {
		Command::Analyze => {
			let changed = report::changing_columns(
				&schema,
				&entries,
				&cli.options.null_tokens,
				cli.options.strict,
			)?;
			let ddl = schema::render(&schema, &changed, cli.with_checks);
			let temporal = changed.iter().filter(|c| **c).count();
			if let Some(path) = &cli.out {
				std::fs::write(path, &ddl)?;
				eprintln!(
					"wrote {} ({} attributes, {} temporal)",
					path.display(),
					schema.attrs.len(),
					temporal
				);
			} else {
				print!("{ddl}");
			}
			eprintln!("key column: {} ({})", schema.key.ident, schema.key.ty.sql());
		}
		Command::Report => {
			let text = report::run(
				&schema,
				&entries,
				&cli.options.null_tokens,
				cli.options.strict,
			)?;
			if let Some(path) = &cli.out {
				std::fs::write(path, &text)?;
				eprintln!("wrote {}", path.display());
			} else {
				print!("{text}");
			}
		}
		Command::Rust => {
			let code = codegen::render(&schema);
			if let Some(path) = &cli.out {
				std::fs::write(path, &code)?;
				eprintln!("wrote {}", path.display());
			} else {
				print!("{code}");
			}
		}
		Command::Load => {
			let url = cli
				.database_url
				.clone()
				.or_else(|| std::env::var("DATABASE_URL").ok())
				.filter(|s| !s.is_empty())
				.ok_or(Error::MissingEnv("DATABASE_URL"))?;
			let changed = report::changing_columns(
				&schema,
				&entries,
				&cli.options.null_tokens,
				cli.options.strict,
			)?;
			let cfg = load::Config {
				null_tokens: &cli.options.null_tokens,
				strict: cli.options.strict,
				changed: &changed,
				checks: cli.with_checks,
			};
			let stats = load::run(&schema, &entries, &url, cli.create_schema, &cfg).await?;
			eprintln!(
				"loaded {} file(s), {} row(s), {} skipped",
				stats.files, stats.rows, stats.skipped
			);
		}
	}
	Ok(())
}

fn parse_args() -> Result<Cli> {
	let mut args = std::env::args().skip(1);
	let command = match args.next().as_deref() {
		Some("analyze") => Command::Analyze,
		Some("report") => Command::Report,
		Some("rust") => Command::Rust,
		Some("load") => Command::Load,
		Some("-h" | "--help" | "help") | None => return Err(Error::Usage(USAGE.to_owned())),
		Some(other) => {
			return Err(Error::Usage(format!(
				"unknown command `{other}`\n\n{USAGE}"
			)));
		}
	};

	let mut list: Option<PathBuf> = None;
	let mut schema = "account".to_owned();
	let mut key: Option<String> = None;
	let mut enum_max = 32usize;
	let mut null_tokens: Vec<String> = Vec::new();
	let mut strict = true;
	let mut out: Option<PathBuf> = None;
	let mut database_url: Option<String> = None;
	let mut create_schema = false;
	let mut with_checks = false;

	while let Some(arg) = args.next() {
		match arg.as_str() {
			"--list" => list = Some(PathBuf::from(value(&mut args, "--list")?)),
			"--schema" => schema = value(&mut args, "--schema")?,
			"--key" => key = Some(value(&mut args, "--key")?),
			"--enum-max" => {
				let raw = value(&mut args, "--enum-max")?;
				enum_max = raw.parse().map_err(|_| {
					Error::Usage(format!("--enum-max expects an integer, got `{raw}`"))
				})?;
			}
			"--null-token" => null_tokens.push(value(&mut args, "--null-token")?),
			"--lenient" => strict = false,
			"-o" | "--out" => out = Some(PathBuf::from(value(&mut args, "--out")?)),
			"--database-url" => database_url = Some(value(&mut args, "--database-url")?),
			"--create-schema" => create_schema = true,
			"--with-checks" => with_checks = true,
			"-h" | "--help" => return Err(Error::Usage(USAGE.to_owned())),
			other => {
				return Err(Error::Usage(format!("unknown option `{other}`\n\n{USAGE}")));
			}
		}
	}

	let list = list.ok_or_else(|| Error::Usage(format!("--list is required\n\n{USAGE}")))?;
	Ok(Cli {
		command,
		list,
		options: Options {
			schema,
			key,
			enum_max,
			null_tokens,
			strict,
		},
		out,
		database_url,
		create_schema,
		with_checks,
	})
}

fn value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String> {
	args.next()
		.ok_or_else(|| Error::Usage(format!("{flag} expects a value")))
}
