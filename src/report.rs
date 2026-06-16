//! Compare the CSVs month over month and print a human-readable report.
//!
//! This reuses the inferred [`Schema`] (so it agrees with `analyze` and
//! `load` on column types, the account key, and which columns are
//! enum-like) and then diffs consecutive files purely in memory — no
//! database needed. It answers the questions the README cares about: which
//! accounts came and went, which attributes actually change (down to the
//! literal `old -> new` values), and when each distinct value first
//! appeared. It also prints the per-column value profile inferred upstream —
//! distinct-count cardinality (enum vs. lookup table), numeric/date ranges,
//! a `numeric(p, s)` suggestion, text length, character class, and null
//! counts — so the schema's constraints can be chosen before any load.

use std::{
	collections::{BTreeMap, HashMap},
	fmt::Write as _,
};

use crate::{
	error::{Error, Result},
	infer::{Cardinality, Column, Schema},
	manifest::Entry,
	util::is_null,
};

/// How many per-(attribute, transition) value changes to list before
/// collapsing the rest into `… and N more`.
const SAMPLE_MAX: usize = 10;

/// A single observed change: the account key and its old/new rendered cell.
type Change = (String, String, String);
/// `[attribute][transition]` → the changes seen for that pair.
type Samples = Vec<Vec<Vec<Change>>>;

/// One month's data, keyed by account natural key. Each value vector is
/// aligned to `Schema::attrs` order; a missing/null cell is `None`.
struct Snapshot {
	source: String,
	at: String,
	values: HashMap<String, Vec<Option<String>>>,
}

/// Build the comparison report for `entries`.
pub fn run(
	schema: &Schema,
	entries: &[Entry],
	null_tokens: &[String],
	strict: bool,
) -> Result<String> {
	let snaps = read_snapshots(schema, entries, null_tokens, strict)?;
	let n_attr = schema.attrs.len();
	let n_trans = snaps.len().saturating_sub(1);

	let mut attr_changes = vec![vec![0usize; n_trans]; n_attr];
	// samples[a][t]: the actual (key, old, new) changes for attribute a in
	// transition t, used by the value-transitions section.
	let mut samples: Samples = vec![vec![Vec::new(); n_trans]; n_attr];
	let mut new_counts = vec![0usize; snaps.len()];
	let mut removed_counts = vec![0usize; snaps.len()];
	let mut changed_accounts = vec![0usize; snaps.len()];
	let mut changed_cells = vec![0usize; snaps.len()];
	if let Some(first) = snaps.first() {
		new_counts[0] = first.values.len();
	}

	for (t, pair) in snaps.windows(2).enumerate() {
		let (prev, cur) = (&pair[0].values, &pair[1].values);
		let fi = t + 1;
		new_counts[fi] = cur.keys().filter(|k| !prev.contains_key(*k)).count();
		removed_counts[fi] = prev.keys().filter(|k| !cur.contains_key(*k)).count();
		for (key, currow) in cur {
			let Some(prevrow) = prev.get(key) else {
				continue;
			};
			let mut diffs = 0;
			for (a, (p, c)) in prevrow.iter().zip(currow).enumerate() {
				if p != c {
					attr_changes[a][t] += 1;
					samples[a][t].push((key.clone(), render_cell(p.as_ref()), render_cell(c.as_ref())));
					diffs += 1;
				}
			}
			if diffs > 0 {
				changed_accounts[fi] += 1;
				changed_cells[fi] += diffs;
			}
		}
	}

	let (first_seen, last_seen) = lifecycle(&snaps);

	let mut out = String::new();
	write_header(&mut out, schema, &snaps);
	write_per_file(
		&mut out,
		&snaps,
		&new_counts,
		&removed_counts,
		&changed_accounts,
		&changed_cells,
	);
	write_columns(&mut out, schema);
	write_attr_changes(&mut out, schema, &attr_changes, n_trans);
	write_transitions(&mut out, schema, &mut samples, n_trans);
	write_enums(&mut out, schema, &snaps);
	write_lifecycle(&mut out, &snaps, &first_seen, &last_seen);
	Ok(out)
}

fn read_snapshots(
	schema: &Schema,
	entries: &[Entry],
	null_tokens: &[String],
	strict: bool,
) -> Result<Vec<Snapshot>> {
	let mut snaps = Vec::with_capacity(entries.len());
	for entry in entries {
		let mut rdr = csv::ReaderBuilder::new()
			.flexible(!strict)
			.from_path(&entry.path)?;
		let headers: Vec<String> = rdr.headers()?.iter().map(ToOwned::to_owned).collect();
		let key_idx = headers.iter().position(|h| *h == schema.key.header);
		let attr_idx: Vec<Option<usize>> = schema
			.attrs
			.iter()
			.map(|c| headers.iter().position(|h| *h == c.header))
			.collect();

		let mut values: HashMap<String, Vec<Option<String>>> = HashMap::new();
		for (row, record) in rdr.records().enumerate() {
			let record = record?;
			let key = key_idx.and_then(|i| record.get(i)).unwrap_or("");
			if is_null(key, null_tokens) {
				if strict {
					return Err(Error::Data {
						source: entry.source.clone(),
						message: format!("row {}: empty key column `{}`", row + 2, schema.key.header),
					});
				}
				continue;
			}
			let row: Vec<Option<String>> = attr_idx
				.iter()
				.map(|idx| {
					let raw = idx.and_then(|i| record.get(i)).unwrap_or("");
					if is_null(raw, null_tokens) {
						None
					} else {
						Some(raw.to_owned())
					}
				})
				.collect();
			values.insert(key.to_owned(), row);
		}
		snaps.push(Snapshot {
			source: entry.source.clone(),
			at: entry.at.clone(),
			values,
		});
	}
	Ok(snaps)
}

/// First and last file index in which each account appears.
fn lifecycle(snaps: &[Snapshot]) -> (HashMap<&str, usize>, HashMap<&str, usize>) {
	let mut first: HashMap<&str, usize> = HashMap::new();
	let mut last: HashMap<&str, usize> = HashMap::new();
	for (i, snap) in snaps.iter().enumerate() {
		for key in snap.values.keys() {
			first.entry(key).or_insert(i);
			last.insert(key, i);
		}
	}
	(first, last)
}

fn write_header(out: &mut String, schema: &Schema, snaps: &[Snapshot]) {
	let _ = writeln!(out, "csv-pg-schema comparison report");
	let _ = writeln!(
		out,
		"schema: {}   key: {} ({})   files: {}\n",
		schema.name,
		schema.key.ident,
		schema.key.ty.sql(),
		snaps.len()
	);
	let _ = writeln!(out, "files (mN = column label used below):");
	for (i, snap) in snaps.iter().enumerate() {
		let _ = writeln!(out, "  m{:<3} {}  ({})", i + 1, snap.source, snap.at);
	}
	out.push('\n');
}

fn write_per_file(
	out: &mut String,
	snaps: &[Snapshot],
	new_counts: &[usize],
	removed_counts: &[usize],
	changed_accounts: &[usize],
	changed_cells: &[usize],
) {
	let w = snaps
		.iter()
		.map(|s| s.source.len())
		.max()
		.unwrap_or(4)
		.max(4);
	let _ = writeln!(out, "per-file overview (vs previous month):");
	let _ = writeln!(
		out,
		"  {:<w$}  {:>5}  {:>4}  {:>7}  {:>8}  {:>8}",
		"file", "rows", "new", "removed", "chg-acct", "chg-cell"
	);
	for (i, snap) in snaps.iter().enumerate() {
		let dash = |v: usize, show: bool| if show { v.to_string() } else { "-".to_owned() };
		let first = i == 0;
		let _ = writeln!(
			out,
			"  {:<w$}  {:>5}  {:>4}  {:>7}  {:>8}  {:>8}",
			snap.source,
			snap.values.len(),
			new_counts[i],
			dash(removed_counts[i], !first),
			dash(changed_accounts[i], !first),
			dash(changed_cells[i], !first),
		);
	}
	out.push('\n');
}

fn write_columns(out: &mut String, schema: &Schema) {
	let w = schema
		.attrs
		.iter()
		.map(|c| c.ident.len())
		.max()
		.unwrap_or(9)
		.max(schema.key.ident.len())
		.max(9);
	let _ = writeln!(
		out,
		"columns & inferred constraints ({} attributes + key):",
		schema.attrs.len()
	);
	write_column_row(out, &schema.key, w, true);
	for col in &schema.attrs {
		write_column_row(out, col, w, false);
	}
	out.push('\n');
}

fn write_column_row(out: &mut String, col: &Column, w: usize, is_key: bool) {
	let null = if is_key {
		"key"
	} else if col.nullable {
		"nullable"
	} else {
		"not null"
	};
	let _ = writeln!(out, "  {:<w$}  {:<12} {}", col.ident, col.ty.sql(), null);
	let detail = constraint_detail(col, is_key);
	if !detail.is_empty() {
		let _ = writeln!(out, "  {:<w$}    {}", "", detail);
	}
}

/// Assemble the indented constraint line for a column: cardinality verdict
/// plus whichever of range / length / character class apply.
fn constraint_detail(col: &Column, is_key: bool) -> String {
	let p = &col.profile;
	let mut parts: Vec<String> = Vec::new();

	let distinct = if p.distinct_overflow {
		format!(">{}", p.enum_cap)
	} else {
		p.distinct.to_string()
	};
	// The natural key is a table by definition, so only attributes get the
	// enum-vs-table recommendation.
	let verdict = if is_key {
		format!("distinct {distinct}")
	} else {
		let tag = match p.cardinality() {
			Cardinality::Enum => "enum candidate (<5)",
			Cardinality::Borderline => "borderline (5-10)",
			Cardinality::Table => "lookup table candidate (>10)",
		};
		format!("distinct {distinct} -> {tag}")
	};
	parts.push(verdict);

	if let Some((lo, hi)) = &p.num_range {
		parts.push(if lo == hi {
			format!("value {lo}")
		} else {
			format!("range {lo} .. {hi}")
		});
	}
	if let Some((precision, scale)) = p.num_scale {
		parts.push(format!("fits numeric({precision},{scale})"));
	}
	if let Some((lo, hi)) = &p.temporal_range {
		parts.push(if lo == hi {
			format!("date {lo}")
		} else {
			format!("range {lo} .. {hi}")
		});
	}
	if col.ty == crate::infer::ColType::Text {
		if let Some((lo, hi)) = p.len_range {
			parts.push(if lo == hi {
				format!("length {lo}")
			} else {
				format!("length {lo}-{hi}")
			});
		}
		if let Some(label) = p.classes.label() {
			parts.push(label.to_owned());
		}
	}

	// Nulls matter for the NOT NULL decision; show the count when present.
	if !is_key && p.null_count > 0 {
		let total = p.non_null + p.null_count;
		parts.push(format!("{}/{} null", p.null_count, total));
	}

	parts.join("  ·  ")
}

/// Render a possibly-null cell for the transitions listing.
fn render_cell(cell: Option<&String>) -> String {
	cell.map_or_else(|| "∅".to_owned(), Clone::clone)
}

/// Per-attribute, per-transition list of the actual `key: old -> new`
/// value changes, capped at [`SAMPLE_MAX`] each.
fn write_transitions(out: &mut String, schema: &Schema, samples: &mut Samples, n_trans: usize) {
	if n_trans == 0 || samples.iter().all(|per| per.iter().all(Vec::is_empty)) {
		return;
	}
	let _ = writeln!(
		out,
		"value transitions (up to {SAMPLE_MAX} per attribute per month):"
	);
	for (col, per_trans) in schema.attrs.iter().zip(samples.iter_mut()) {
		if per_trans.iter().all(Vec::is_empty) {
			continue;
		}
		let _ = writeln!(out, "  {}:", col.ident);
		for (t, changes) in per_trans.iter_mut().enumerate() {
			if changes.is_empty() {
				continue;
			}
			changes.sort_by(|a, b| a.0.cmp(&b.0));
			let shown: Vec<String> = changes
				.iter()
				.take(SAMPLE_MAX)
				.map(|(k, old, new)| format!("{k}: {old} -> {new}"))
				.collect();
			let mut line = shown.join(", ");
			if changes.len() > SAMPLE_MAX {
				let _ = write!(line, ", … and {} more", changes.len() - SAMPLE_MAX);
			}
			let _ = writeln!(out, "    m{}->m{}:  {}", t + 1, t + 2, line);
		}
	}
	out.push('\n');
}

fn write_attr_changes(
	out: &mut String,
	schema: &Schema,
	attr_changes: &[Vec<usize>],
	n_trans: usize,
) {
	if n_trans == 0 {
		return;
	}
	let w = schema
		.attrs
		.iter()
		.map(|c| c.ident.len())
		.max()
		.unwrap_or(9)
		.max(9);
	let _ = writeln!(out, "attribute changes (accounts changed, per transition):");
	let _ = write!(out, "  {:<w$}", "attribute");
	for t in 0..n_trans {
		let _ = write!(out, "  {:>7}", format!("m{}->m{}", t + 1, t + 2));
	}
	out.push('\n');
	for (col, changes) in schema.attrs.iter().zip(attr_changes) {
		let total: usize = changes.iter().sum();
		if total == 0 {
			continue;
		}
		let _ = write!(out, "  {:<w$}", col.ident);
		for c in changes {
			let _ = write!(out, "  {c:>7}");
		}
		out.push('\n');
	}
	out.push('\n');
}

fn write_enums(out: &mut String, schema: &Schema, snaps: &[Snapshot]) {
	let has_enum = schema.attrs.iter().any(|c| c.enum_values.is_some());
	if !has_enum {
		return;
	}
	let _ = writeln!(
		out,
		"distinct values & first appearance (mN = month a value first showed up):"
	);
	for (a, col) in schema.attrs.iter().enumerate() {
		if col.enum_values.is_none() {
			continue;
		}
		let mut first: BTreeMap<String, usize> = BTreeMap::new();
		for (i, snap) in snaps.iter().enumerate() {
			for row in snap.values.values() {
				if let Some(v) = &row[a] {
					first.entry(v.clone()).or_insert(i);
				}
			}
		}
		// Sort by first-seen month, then value, so late arrivals stand out.
		let mut items: Vec<(&String, usize)> = first.iter().map(|(v, i)| (v, *i)).collect();
		items.sort_by(|x, y| x.1.cmp(&y.1).then_with(|| x.0.cmp(y.0)));
		let rendered: Vec<String> = items
			.iter()
			.map(|(v, i)| format!("{v} (m{})", i + 1))
			.collect();
		let _ = writeln!(out, "  {}: {}", col.ident, rendered.join(", "));
	}
	out.push('\n');
}

fn write_lifecycle(
	out: &mut String,
	snaps: &[Snapshot],
	first_seen: &HashMap<&str, usize>,
	last_seen: &HashMap<&str, usize>,
) {
	let last_file = snaps.len().saturating_sub(1);
	let total = first_seen.len();

	let mut present_all = 0;
	let mut added: Vec<(&str, usize)> = Vec::new();
	for (key, &f) in first_seen {
		if f == 0 && last_seen.get(key) == Some(&last_file) {
			present_all += 1;
		}
		if f > 0 {
			added.push((*key, f));
		}
	}
	added.sort_unstable();

	let mut removed: Vec<(&str, usize)> = Vec::new();
	for (key, &l) in last_seen {
		if l < last_file {
			removed.push((*key, l));
		}
	}
	removed.sort_unstable();

	let _ = writeln!(out, "account lifecycle:");
	let _ = writeln!(out, "  distinct accounts: {total}");
	let _ = writeln!(out, "  present in every month: {present_all}");
	let _ = writeln!(
		out,
		"  added after first month: {}",
		summarize(&added, |(k, f)| format!("{k} (m{})", f + 1))
	);
	let _ = writeln!(
		out,
		"  gone before last month:  {}",
		summarize(&removed, |(k, l)| format!("{k} (last m{})", l + 1))
	);
}

/// Render up to a handful of accounts, then `… and N more`.
fn summarize<T>(items: &[T], render: impl Fn(&T) -> String) -> String {
	const MAX: usize = 12;
	if items.is_empty() {
		return "none".to_owned();
	}
	let shown: Vec<String> = items.iter().take(MAX).map(&render).collect();
	let mut s = shown.join(", ");
	if items.len() > MAX {
		let _ = write!(s, ", … and {} more", items.len() - MAX);
	}
	s
}

#[cfg(test)]
mod tests {
	use std::fs;

	use super::run;
	use crate::{infer, manifest};

	/// Build a manifest + two CSVs in a temp dir and run the report end to
	/// end, exercising inference, snapshotting, and every section.
	#[test]
	fn end_to_end_report() {
		let dir = std::env::temp_dir().join(format!("csvpg-report-test-{}", std::process::id()));
		let _ = fs::remove_dir_all(&dir);
		fs::create_dir_all(&dir).unwrap();
		fs::write(
			dir.join("m1.csv"),
			"account_id,status,balance\n1,active,10.00\n2,active,20.00\n",
		)
		.unwrap();
		fs::write(
			dir.join("m2.csv"),
			"account_id,status,balance\n1,closed,10.50\n2,active,20.00\n3,active,5.00\n",
		)
		.unwrap();
		let list = dir.join("list.txt");
		fs::write(
			&list,
			format!(
				"{}\n{}\n",
				dir.join("m1.csv").display(),
				dir.join("m2.csv").display()
			),
		)
		.unwrap();

		let entries = manifest::parse(&list).unwrap();
		let opts = infer::Options {
			schema: "account".to_owned(),
			key: None,
			enum_max: 32,
			null_tokens: Vec::new(),
			strict: true,
		};
		let schema = infer::build_schema(&entries, &opts).unwrap();
		let out = run(&schema, &entries, &[], true).unwrap();

		assert!(out.contains("key: account_id"));
		assert!(out.contains("value transitions"));
		assert!(out.contains("1: active -> closed"));
		assert!(out.contains("fits numeric(4,2)"));
		assert!(out.contains("added after first month: 3"));

		let _ = fs::remove_dir_all(&dir);
	}

	/// A blank account key aborts a strict run but is tolerated otherwise.
	#[test]
	fn strict_rejects_blank_key() {
		let dir = std::env::temp_dir().join(format!("csvpg-report-blank-{}", std::process::id()));
		let _ = fs::remove_dir_all(&dir);
		fs::create_dir_all(&dir).unwrap();
		fs::write(dir.join("m1.csv"), "account_id,status\n1,active\n,active\n").unwrap();
		let list = dir.join("list.txt");
		fs::write(&list, format!("{}\n", dir.join("m1.csv").display())).unwrap();

		let entries = manifest::parse(&list).unwrap();
		let opts = infer::Options {
			schema: "account".to_owned(),
			key: None,
			enum_max: 32,
			null_tokens: Vec::new(),
			strict: true,
		};
		let schema = infer::build_schema(&entries, &opts).unwrap();

		assert!(run(&schema, &entries, &[], true).is_err());
		assert!(run(&schema, &entries, &[], false).is_ok());

		let _ = fs::remove_dir_all(&dir);
	}
}
