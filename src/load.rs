//! Process the CSVs in order and store only the deltas.
//!
//! Loading an attribute value is an *upsert in time* (the pattern from the
//! reference `store.rs`): close the current open range at the observation
//! instant only if the value actually changed, then open a fresh
//! `[at, infinity)` range only if none currently covers `at`. A re-seen,
//! unchanged value writes nothing — that is the whole point, since "most
//! data is repeated per month."
//!
//! Every value crosses the wire as text and is cast to the column's
//! inferred type in SQL. The cast is written `$n::text::numeric` rather
//! than `$n::numeric` on purpose: the doubled cast pins the *parameter*
//! type to `text` (so we can always bind a string), while the outer cast
//! still produces the real column type and lets Postgres validate it. A
//! single `$n::numeric` would make Postgres infer the parameter itself as
//! numeric and reject the string bind. This keeps one generic upsert
//! working for every column without per-type plumbing.

use std::{collections::HashSet, fmt::Write as _};

use tokio_postgres::{Client, NoTls, Transaction, types::ToSql};

use crate::{
	error::{Error, Result},
	infer::{Column, Schema},
	manifest::Entry,
	schema,
	util::is_null,
};

/// Tallies returned for the run summary.
pub struct Stats {
	pub files: usize,
	pub rows: usize,
	pub skipped: usize,
}

/// How to load, beyond the connection itself: which cells count as null,
/// whether to validate strictly, which attributes are temporal (`changed`,
/// aligned to `schema.attrs`), and whether `--create-schema` bakes in checks.
pub struct Config<'a> {
	pub null_tokens: &'a [String],
	pub strict: bool,
	pub changed: &'a [bool],
	pub checks: bool,
}

pub async fn run(
	schema: &Schema,
	entries: &[Entry],
	db_url: &str,
	create_schema: bool,
	cfg: &Config<'_>,
) -> Result<Stats> {
	let (mut client, connection) = tokio_postgres::connect(db_url, NoTls).await?;
	tokio::spawn(async move {
		if let Err(e) = connection.await {
			eprintln!("postgres connection error: {e}");
		}
	});

	if create_schema {
		client
			.batch_execute(&schema::render(schema, cfg.changed, cfg.checks))
			.await?;
	}

	let mut stats = Stats {
		files: 0,
		rows: 0,
		skipped: 0,
	};
	for entry in entries {
		load_file(&mut client, schema, entry, cfg, &mut stats).await?;
		stats.files += 1;
	}
	Ok(stats)
}

/// Load one month's CSV inside a single transaction, so a file lands
/// atomically.
async fn load_file(
	client: &mut Client,
	schema: &Schema,
	entry: &Entry,
	cfg: &Config<'_>,
	stats: &mut Stats,
) -> Result<()> {
	let tx = client.transaction().await?;
	let request_id = insert_request(&tx, schema, &entry.source, &entry.at).await?;

	let mut rdr = csv::ReaderBuilder::new()
		.flexible(!cfg.strict)
		.from_path(&entry.path)?;
	let headers: Vec<String> = rdr.headers()?.iter().map(ToOwned::to_owned).collect();

	let key_idx = headers.iter().position(|h| *h == schema.key.header);
	// Split the attributes by whether they change over time: static ones are
	// written inline on the account row, changing ones go through the temporal
	// upsert. Header positions are resolved once, here.
	let mut static_cols: Vec<&Column> = Vec::new();
	let mut static_idx: Vec<Option<usize>> = Vec::new();
	let mut changing: Vec<(&Column, Option<usize>)> = Vec::new();
	for (col, &chg) in schema.attrs.iter().zip(cfg.changed) {
		let idx = headers.iter().position(|h| *h == col.header);
		if chg {
			changing.push((col, idx));
		} else {
			static_cols.push(col);
			static_idx.push(idx);
		}
	}

	let mut seen: HashSet<String> = HashSet::new();
	for (row, record) in rdr.records().enumerate() {
		let record = record?;
		let key_raw = key_idx.and_then(|i| record.get(i)).unwrap_or("");
		if is_null(key_raw, cfg.null_tokens) {
			if cfg.strict {
				return Err(Error::Data {
					source: entry.source.clone(),
					message: format!("row {}: empty key column `{}`", row + 2, schema.key.header),
				});
			}
			eprintln!(
				"warn: {}: skipping row with empty key column `{}`",
				entry.source, schema.key.header
			);
			stats.skipped += 1;
			continue;
		}
		// One row per account per month; reject a repeat key in strict mode
		// rather than silently upserting the same account twice.
		if cfg.strict && !seen.insert(key_raw.to_owned()) {
			return Err(Error::Data {
				source: entry.source.clone(),
				message: format!("row {}: duplicate account key `{key_raw}`", row + 2),
			});
		}
		let cell = |idx: Option<usize>| {
			let raw = idx.and_then(|i| record.get(i)).unwrap_or("");
			if is_null(raw, cfg.null_tokens) {
				None
			} else {
				Some(raw)
			}
		};
		let static_vals: Vec<Option<&str>> = static_idx.iter().map(|idx| cell(*idx)).collect();
		let account_id =
			ensure_account(&tx, schema, key_raw, &static_cols, &static_vals, request_id).await?;
		for &(col, idx) in &changing {
			upsert_attr(
				&tx,
				&schema.name,
				col,
				account_id,
				cell(idx),
				request_id,
				&entry.at,
			)
			.await?;
		}
		stats.rows += 1;
	}

	tx.commit().await?;
	Ok(())
}

async fn insert_request(
	tx: &Transaction<'_>,
	schema: &Schema,
	source: &str,
	at: &str,
) -> Result<i32> {
	let sql = format!(
		"INSERT INTO {s}.import_request (source, at) VALUES ($1, $2::text::timestamptz) RETURNING id",
		s = schema.name,
	);
	let row = tx.query_one(sql.as_str(), &[&source, &at]).await?;
	Ok(row.get(0))
}

/// Insert the account on first sighting, or fetch its surrogate id on a
/// repeat. The no-op `DO UPDATE` (rather than `DO NOTHING`) is what lets
/// `RETURNING` hand back the existing row's id on conflict. The
/// `request_id` and the static attribute values only take effect on the
/// insert path; on conflict the account keeps its original first-seen
/// provenance and (immutable) static values.
async fn ensure_account(
	tx: &Transaction<'_>,
	schema: &Schema,
	key: &str,
	static_cols: &[&Column],
	static_vals: &[Option<&str>],
	request_id: i32,
) -> Result<i32> {
	let kty = schema.key.ty.sql();
	let kcol = &schema.key.ident;
	let mut cols = kcol.clone();
	let mut vals = format!("$1::text::{kty}");
	for (i, col) in static_cols.iter().enumerate() {
		let _ = write!(cols, ", {}", col.ident);
		let _ = write!(vals, ", ${}::text::{}", i + 2, col.ty.sql());
	}
	let _ = write!(cols, ", request_id");
	let _ = write!(vals, ", ${}", static_cols.len() + 2);
	let sql = format!(
		"INSERT INTO {s}.account ({cols}) VALUES ({vals}) \
		 ON CONFLICT ({kcol}) DO UPDATE SET {kcol} = EXCLUDED.{kcol} RETURNING id",
		s = schema.name,
	);
	let mut params: Vec<&(dyn ToSql + Sync)> = Vec::with_capacity(static_cols.len() + 2);
	params.push(&key);
	for v in static_vals {
		params.push(v);
	}
	params.push(&request_id);
	let row = tx.query_one(sql.as_str(), &params).await?;
	Ok(row.get(0))
}

/// Temporally upsert one attribute (see module docs). `value` is bound as
/// text and cast to the column type; `None` becomes a typed SQL `NULL`.
async fn upsert_attr(
	tx: &Transaction<'_>,
	schema: &str,
	col: &Column,
	id: i32,
	value: Option<&str>,
	request_id: i32,
	at: &str,
) -> Result<()> {
	let ty = col.ty.sql();
	let table = format!("{schema}.account_{ident}", ident = col.ident);
	let column = &col.ident;

	let close = format!(
		"UPDATE {table} SET during = tstzrange(lower(during), $3::text::timestamptz) \
		 WHERE id = $1 AND during @> $3::text::timestamptz AND {column} IS DISTINCT FROM $2::text::{ty}"
	);
	tx.execute(close.as_str(), &[&id, &value, &at]).await?;

	let insert = format!(
		"INSERT INTO {table} (id, {column}, request_id, during) \
		 SELECT $1, $2::text::{ty}, $3, tstzrange($4::text::timestamptz, 'infinity') \
		 WHERE NOT EXISTS (SELECT 1 FROM {table} WHERE id = $1 AND during @> $4::text::timestamptz)"
	);
	tx.execute(insert.as_str(), &[&id, &value, &request_id, &at])
		.await?;
	Ok(())
}
